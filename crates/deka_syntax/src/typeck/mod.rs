//! DekaScript typechecker (Compiler v2).
//!
//! This is a baseline typechecker for the parser's supported subset.  It is
//! intentionally simple: structural checks for the built-in scalar types,
//! generic `Option<T>` (with `none` represented by a dedicated `NoneType`),
//! function types, and local/top-level bindings.
//!
//! Design choice for `none`:
//! `none` is given a fresh built-in type `Type::None` (displayed as `none`).
//! `Type::None` is assignable to any `Option<T>` because it is the empty
//! option payload.

use std::collections::{HashMap, HashSet};

use bumpalo::Bump;

use crate::ast;
use crate::ast::{MethodTarget, Program};
use crate::diagnostics::Diagnostic;

mod ast_type;
mod descriptor;
mod expr;
mod stmt;
mod types;

pub use descriptor::{
    DescriptorField, DescriptorTree, JsonCall, JsonOperation, StaticTypeCall,
};
pub use types::{
    ArrayAccess, NewtypeSide, NumberMath, OperatorRewrite, Type, UnionMemberTest, UnwrapKind,
};

#[derive(Debug)]
pub struct TypeError {
    pub message: String,
}

/// Add the actionable next step when a value's union type is used where one
/// concrete type is required. Keep this at the diagnostic boundary rather
/// than in `is_assignable`, whose recursive calls also check union members.
pub(super) fn with_union_narrowing_hint<'a>(
    message: String,
    expected: &Type<'a>,
    actual: &Type<'a>,
) -> String {
    if matches!(actual, Type::Union { .. }) && !matches!(expected, Type::Union { .. }) {
        format!("{message}; narrow it with a match before use")
    } else {
        message
    }
}

pub struct TypeckResult<'a> {
    pub program: &'a Program<'a>,
    pub errors: Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
    /// Map from primitive extension call expression pointer to the mangled
    /// free-function call that should replace it during emission (deka#527).
    pub method_calls: HashMap<*const ast::Expr<'a>, MethodTarget<'a>>,
    /// Call sites of the builtin `.getType()` method, rewritten to
    /// `__deka_type_of(x)` during emission (rfd#41, deka#529).
    pub type_of_calls: HashSet<*const ast::Expr<'a>>,
    /// `.signature()` call sites and their compile-time declared descriptors.
    pub signature_calls: HashMap<*const ast::Expr<'a>, descriptor::DescriptorTree<'a>>,
    /// Builtin `Name.type()` call sites on `super` declarations (rfd#41,
    /// deka#561 PR B), rewritten to the interned `__deka_super_desc$<Name>`
    /// const during emission.
    pub static_type_calls: HashMap<*const ast::Expr<'a>, descriptor::StaticTypeCall<'a>>,
    /// Descriptor trees for every `super` declaration visible in this module,
    /// keyed by declaration name. The emitter interns one frozen const per
    /// referenced declaration (and its recursive-reference group).
    pub super_trees: HashMap<&'a str, descriptor::DescriptorTree<'a>>,
    /// `.toJSON()` and `.parseJSON<T>()` call sites specialized to a static shape.
    pub json_calls: HashMap<*const ast::Expr<'a>, descriptor::JsonCall<'a>>,
    /// Builtin `Array.first()`/`Array.last()`/`Array.pop()`/`Array.shift()`
    /// call sites, rewritten to an Option-producing expression during
    /// emission (deka#561, deka#566).
    pub array_builtin_calls: HashMap<*const ast::Expr<'a>, types::ArrayAccess>,
    /// Builtin `Math`-backed `number` method call sites, rewritten to a
    /// `Math.*` expression during emission — partial functions wrapped so
    /// `NaN` surfaces as `None` (deka#378 step 2, rfd#40 phase 2).
    pub number_math_calls: HashMap<*const ast::Expr<'a>, types::NumberMath>,
    /// Map from primitive conversion call expression pointer to how it should
    /// be lowered (`parseNumber(x)`, `unboxNumber(x)`, `toNumber(x)`,
    /// `string(x)`).
    pub unwrap_calls: HashMap<*const ast::Expr<'a>, types::UnwrapKind>,
    /// Map from binary/unary operator expression pointer to how a newtype
    /// operation should be lowered.
    pub operator_rewrites: HashMap<*const ast::Expr<'a>, types::OperatorRewrite<'a>>,
    /// How each JSX element's optional props must be materialised.
    ///
    /// A component's props interface is a construction site the compiler owns,
    /// so `?:` props are filled and bare values wrapped there. A plain object
    /// literal is not, which is why omitting one is a type error (deka#416).
    pub jsx_optional_props: HashMap<*const ast::JsxElement<'a>, JsxOptionalProps<'a>>,
    /// Identifier patterns that name a payload-free case of the scrutinee's
    /// enum rather than binding it (deka#450).
    pub enum_case_patterns: HashMap<*const ast::Pattern<'a>, &'a str>,
    /// Constructor patterns that are union member type-patterns (`string(s)`),
    /// mapped to the runtime predicate the emitter must emit (rfd#42).
    pub union_type_patterns: HashMap<*const ast::Pattern<'a>, types::UnionMemberTest<'a>>,
}

/// The `Option` materialisation for one JSX element.
#[derive(Debug, Clone, Default)]
pub struct JsxOptionalProps<'a> {
    /// Optional props with no attribute: emit `Option.None`.
    pub fill_none: Vec<&'a str>,
    /// Attributes whose value must be wrapped in `Option.Some(...)`.
    pub wrap_some: Vec<&'a str>,
}

pub fn check_program<'a>(program: &'a Program<'a>, _source: &str) -> TypeckResult<'a> {
    let imports = HashMap::new();
    check_program_with_imports(program, _source, &imports)
}

/// Collect the declared struct/enum names an export-side type annotation
/// refers to (alias-transparent), for the super-marking closure in
/// `collect_module_exports`.
fn export_type_ast_refs<'a>(
    ty: &ast::Type<'a>,
    aliases: &HashMap<&'a str, ast::Type<'a>>,
    structs: &HashMap<&'a str, StructInfo<'a>>,
    enums: &HashMap<&'a str, EnumInfo<'a>>,
    out: &mut Vec<&'a str>,
    depth: usize,
) {
    if depth > 16 {
        return;
    }
    match ty {
        ast::Type::Named { name, .. } => {
            if structs.contains_key(name) || enums.contains_key(name) {
                out.push(name);
            } else if let Some(target) = aliases.get(name) {
                export_type_ast_refs(target, aliases, structs, enums, out, depth + 1);
            }
        }
        ast::Type::Generic { base, args, .. } => {
            if structs.contains_key(base) || enums.contains_key(base) {
                out.push(base);
            }
            for arg in args.iter() {
                export_type_ast_refs(arg, aliases, structs, enums, out, depth + 1);
            }
        }
        ast::Type::Option { inner, .. } => {
            export_type_ast_refs(inner, aliases, structs, enums, out, depth + 1);
        }
        ast::Type::Union { members, .. } => {
            for member in members.iter() {
                export_type_ast_refs(member, aliases, structs, enums, out, depth + 1);
            }
        }
        _ => {}
    }
}

/// Type information exported by a compiled module, used to seed the
/// typechecker of its importers.
#[derive(Clone, Debug)]
pub struct ModuleExports<'a> {
    pub structs: HashMap<&'a str, StructInfo<'a>>,
    pub enums: HashMap<&'a str, EnumInfo<'a>>,
    pub aliases: HashMap<&'a str, ast::Type<'a>>,
    pub newtypes: HashMap<&'a str, NewtypeInfo>,
    pub receiver_methods: HashMap<(&'a str, &'a str), MethodInfo<'a>>,
    /// Value bindings (functions / constants) exported by the module.
    pub values: HashMap<&'a str, Type<'a>>,
    /// Names exported via `export { name }` that are not locally declared
    /// (i.e. re-exports of imports). These pass through to importers.
    pub re_exports: HashSet<&'a str>,
}

impl<'a> Default for ModuleExports<'a> {
    fn default() -> Self {
        Self {
            structs: HashMap::new(),
            enums: HashMap::new(),
            aliases: HashMap::new(),
            newtypes: HashMap::new(),
            receiver_methods: HashMap::new(),
            values: HashMap::new(),
            re_exports: HashSet::new(),
        }
    }
}

/// Typecheck a program with imported module signatures available.
pub fn check_program_with_imports<'a>(
    program: &'a Program<'a>,
    _source: &str,
    imports: &HashMap<&str, &ModuleExports<'a>>,
) -> TypeckResult<'a> {
    let mut checker = Checker::new(program, imports);
    checker.check_program();

    TypeckResult {
        program,
        errors: checker.errors,
        warnings: checker.warnings,
        method_calls: checker.method_calls,
        type_of_calls: checker.type_of_calls,
        signature_calls: checker.signature_calls,
        static_type_calls: checker.static_type_calls,
        super_trees: checker.super_trees,
        json_calls: checker.json_calls,
        array_builtin_calls: checker.array_builtin_calls,
        number_math_calls: checker.number_math_calls,
        unwrap_calls: checker.unwrap_calls,
        operator_rewrites: checker.operator_rewrites,
        jsx_optional_props: checker.jsx_optional_props,
        enum_case_patterns: checker.enum_case_patterns,
        union_type_patterns: checker.union_type_patterns,
    }
}

/// Infer function signatures for a single module without emitting diagnostics.
///
/// This is used by `collect_module_exports` so that unannotated exported
/// functions (common in the stdlib, e.g. `export fn sha512() { digest(...) }`)
/// still expose a usable return type to importers.
pub fn infer_module_function_signatures<'a>(
    program: &'a Program<'a>,
) -> HashMap<&'a str, Type<'a>> {
    let imports = HashMap::new();
    let mut checker = Checker::new(program, &imports);
    checker.infer_all_function_signatures();
    checker.globals
}

/// Collect the exported type information from a parsed module.
///
/// The returned `ModuleExports` references AST nodes allocated in `arena` (and
/// in the source strings), so `arena` must outlive any importer that consumes
/// these exports.
pub fn collect_module_exports<'a>(program: &'a Program<'a>, _arena: &'a Bump) -> ModuleExports<'a> {
    let mut declared_structs: HashMap<&'a str, StructInfo<'a>> = HashMap::new();
    let mut declared_enums: HashMap<&'a str, EnumInfo<'a>> = HashMap::new();
    let mut declared_aliases: HashMap<&'a str, ast::Type<'a>> = HashMap::new();
    let mut declared_newtypes: HashMap<&'a str, NewtypeInfo> = HashMap::new();
    let mut receiver_methods: HashMap<(&'a str, &'a str), MethodInfo<'a>> = HashMap::new();

    for stmt in program.statements.iter() {
        match stmt {
            ast::Stmt::Struct {
                name, fields, embeds, type_params, is_super, ..
            } => {
                declared_structs.insert(
                    *name,
                    StructInfo {
                        fields: *fields,
                        embeds: *embeds,
                        type_params,
                        is_super: *is_super,
                    },
                );
            }
            ast::Stmt::Enum {
                name,
                cases,
                type_params,
                is_super,
                ..
            } => {
                declared_enums.insert(
                    *name,
                    EnumInfo {
                        cases: *cases,
                        type_params: *type_params,
                        is_super: *is_super,
                    },
                );
            }
            ast::Stmt::TypeAlias { name, value, .. } => {
                declared_aliases.insert(*name, value.clone());
            }
            ast::Stmt::Newtype { name, repr, .. } => {
                declared_newtypes.insert(*name, NewtypeInfo { repr: *repr });
            }
            ast::Stmt::ReceiverMethod {
                receiver_type,
                name,
                params,
                return_type,
                ..
            } => {
                receiver_methods.insert(
                    (*receiver_type, *name),
                    MethodInfo {
                        params: *params,
                        return_type: return_type.clone(),
                        mutable: false,
                        // Filled in the second pass below, once every
                        // declaration in the module is known.
                        param_types: Vec::new(),
                        resolved_return: None,
                    },
                );
            }
            _ => {}
        }
    }

    // Super marking is transitive within a module (deka#561 PR B): a `super`
    // declaration's descriptor embeds the descriptors of the structs/enums
    // it references, so those declarations are marked too. Propagate that
    // marking into the exported infos — otherwise an importer of an
    // auto-marked type would wrongly be told it "does not carry runtime
    // type information".
    {
        let mut marked: HashSet<&'a str> = declared_structs
            .iter()
            .filter(|(_, i)| i.is_super)
            .map(|(n, _)| *n)
            .chain(
                declared_enums
                    .iter()
                    .filter(|(_, i)| i.is_super)
                    .map(|(n, _)| *n),
            )
            .collect();
        let mut worklist: Vec<&'a str> = marked.iter().cloned().collect();
        while let Some(name) = worklist.pop() {
            let member_types: Vec<&'a ast::Type<'a>> = match declared_structs.get(name) {
                Some(info) => info
                    .fields
                    .iter()
                    .map(|f| &f.ty)
                    .collect(),
                None => match declared_enums.get(name) {
                    Some(info) => info
                        .cases
                        .iter()
                        .filter_map(|c| c.payload.as_ref())
                        .collect(),
                    None => continue,
                },
            };
            let mut refs = Vec::new();
            for ty in member_types {
                export_type_ast_refs(ty, &declared_aliases, &declared_structs, &declared_enums, &mut refs, 0);
            }
            for referenced in refs {
                if marked.insert(referenced) {
                    worklist.push(referenced);
                }
            }
        }
        for name in marked {
            if let Some(info) = declared_structs.get_mut(name) {
                info.is_super = true;
            }
            if let Some(info) = declared_enums.get_mut(name) {
                info.is_super = true;
            }
        }
    }

    // Second pass: resolve receiver-method annotations with the module's full
    // declaration tables, so importers (and call sites) reuse one resolution
    // instead of re-resolving per use (deka#494).
    for info in receiver_methods.values_mut() {
        info.param_types = info
            .params
            .iter()
            .map(|p| match &p.ty {
                Some(t) => ast_type_to_export_type(
                    t,
                    &declared_structs,
                    &declared_enums,
                    &declared_aliases,
                    &declared_newtypes,
                    &mut HashSet::new(),
                ),
                None => Type::Error,
            })
            .collect();
        info.resolved_return = info.return_type.as_ref().map(|t| {
            ast_type_to_export_type(
                t,
                &declared_structs,
                &declared_enums,
                &declared_aliases,
                &declared_newtypes,
                &mut HashSet::new(),
            )
        });
    }

    // Helper: convert an AST type annotation into a typechecker type using the
    // declaring module's own struct/enum/newtype/alias bindings.  This lets
    // exported functions and constants carry real signatures across the module
    // graph instead of the previous `Type::Infer` placeholders.
    fn ast_type_to_export_type<'a>(
        ty: &ast::Type<'a>,
        structs: &HashMap<&'a str, StructInfo<'a>>,
        enums: &HashMap<&'a str, EnumInfo<'a>>,
        aliases: &HashMap<&'a str, ast::Type<'a>>,
        newtypes: &HashMap<&'a str, NewtypeInfo>,
        seen: &mut HashSet<&'a str>,
    ) -> Type<'a> {
        match ty {
            ast::Type::Named { name, .. } => match *name {
                "number" | "string" | "boolean" | "never" | "void" | "bytes" | "Component"
                | "JsError" | "Type" => Type::Named { name },
                _ => {
                    if structs.contains_key(name) {
                        Type::Struct { name }
                    } else if enums.contains_key(name) {
                        Type::Named { name }
                    } else if let Some(info) = newtypes.get(name) {
                        Type::Newtype {
                            name,
                            repr: info.repr,
                        }
                    } else if let Some(alias) = aliases.get(name) {
                        if !seen.insert(name) {
                            return Type::Error;
                        }
                        let resolved =
                            ast_type_to_export_type(alias, structs, enums, aliases, newtypes, seen);
                        seen.remove(name);
                        resolved
                    } else {
                        Type::Named { name }
                    }
                }
            },
            ast::Type::Generic { base, args, .. } => {
                if *base == "Option" && args.len() == 1 {
                    Type::Option {
                        inner: Box::new(ast_type_to_export_type(
                            &args[0], structs, enums, aliases, newtypes, seen,
                        )),
                    }
                } else if *base == "Array" && args.len() == 1 {
                    Type::Array {
                        elem: Box::new(ast_type_to_export_type(
                            &args[0], structs, enums, aliases, newtypes, seen,
                        )),
                    }
                } else if (*base == "Result" || *base == "Promise") && args.len() <= 2 {
                    Type::Generic {
                        base,
                        args: args
                            .iter()
                            .map(|arg| {
                                ast_type_to_export_type(
                                    arg, structs, enums, aliases, newtypes, seen,
                                )
                            })
                            .collect(),
                    }
                } else if structs.contains_key(base) || enums.contains_key(base) {
                    Type::Generic {
                        base,
                        args: args
                            .iter()
                            .map(|arg| {
                                ast_type_to_export_type(
                                    arg, structs, enums, aliases, newtypes, seen,
                                )
                            })
                            .collect(),
                    }
                } else {
                    Type::Named { name: base }
                }
            }
            ast::Type::Function { params, ret, .. } => Type::Function {
                params: params
                    .iter()
                    .map(|p| ast_type_to_export_type(p, structs, enums, aliases, newtypes, seen))
                    .collect(),
                ret: Box::new(ast_type_to_export_type(
                    ret, structs, enums, aliases, newtypes, seen,
                )),
                optional: 0,
            },
            ast::Type::Option { inner, .. } => Type::Option {
                inner: Box::new(ast_type_to_export_type(
                    inner, structs, enums, aliases, newtypes, seen,
                )),
            },
            ast::Type::Tuple { .. } | ast::Type::Record { .. } => Type::Error,
            ast::Type::Union { members, .. } => Type::Union {
                members: members
                    .iter()
                    .map(|m| ast_type_to_export_type(m, structs, enums, aliases, newtypes, seen))
                    .collect(),
            },
        }
    }

    // Collect declared value signatures so `export { foo }` can re-export the
    // type of a non-exported `fn foo` or `const foo`. Prefer inferred
    // signatures (which resolve forward references and bridge/unsafe returns)
    // over raw annotations when available.
    let inferred_globals = infer_module_function_signatures(program);
    let mut declared_values: HashMap<&'a str, Type<'a>> = HashMap::new();
    for stmt in program.statements.iter() {
        match stmt {
            ast::Stmt::Function {
                name,
                type_params,
                params,
                return_type,
                ..
            } => {
                if let Some(ty) = inferred_globals.get(name) {
                    // The checker retains `Type::Param` in polymorphic
                    // signatures, so keep that signature at the module
                    // boundary instead of collapsing generic functions to
                    // `Type::Infer` (deka#483).
                    declared_values.insert(*name, ty.clone());
                } else if type_params.is_empty() {
                    let param_types: Vec<Type<'a>> = params
                        .iter()
                        .map(|p| {
                            p.ty.as_ref()
                                .map(|t| {
                                    ast_type_to_export_type(
                                        t,
                                        &declared_structs,
                                        &declared_enums,
                                        &declared_aliases,
                                        &declared_newtypes,
                                        &mut HashSet::new(),
                                    )
                                })
                                .unwrap_or(Type::Infer)
                        })
                        .collect();
                    let ret = return_type
                        .as_ref()
                        .map(|t| {
                            ast_type_to_export_type(
                                t,
                                &declared_structs,
                                &declared_enums,
                                &declared_aliases,
                                &declared_newtypes,
                                &mut HashSet::new(),
                            )
                        })
                        .unwrap_or(Type::Infer);
                    declared_values.insert(
                        *name,
                        Type::Function {
                            params: param_types,
                            ret: Box::new(ret),
                            optional: 0,
                        },
                    );
                } else {
                    // A missing inferred signature is an error-recovery path.
                    // Do not misrepresent an unresolved type parameter as a
                    // concrete named type in the annotation-only fallback.
                    declared_values.insert(*name, Type::Infer);
                }
            }
            ast::Stmt::Const { name, ty, .. } | ast::Stmt::Let { name, ty, .. } => {
                let value_ty = inferred_globals.get(name).cloned().unwrap_or_else(|| {
                    ty.as_ref()
                        .map(|t| {
                            ast_type_to_export_type(
                                t,
                                &declared_structs,
                                &declared_enums,
                                &declared_aliases,
                                &declared_newtypes,
                                &mut HashSet::new(),
                            )
                        })
                        .unwrap_or(Type::Infer)
                });
                declared_values.insert(*name, value_ty);
            }
            _ => {}
        }
    }

    let mut exports = ModuleExports::default();

    for stmt in program.statements.iter() {
        let ast::Stmt::Export { decl, .. } = stmt else {
            continue;
        };
        match decl {
            ast::ExportDecl::Const { name, ty, .. } => {
                let value_ty = inferred_globals.get(name).cloned().unwrap_or_else(|| {
                    ty.as_ref()
                        .map(|t| {
                            ast_type_to_export_type(
                                t,
                                &declared_structs,
                                &declared_enums,
                                &declared_aliases,
                                &declared_newtypes,
                                &mut HashSet::new(),
                            )
                        })
                        .unwrap_or(Type::Infer)
                });
                exports.values.insert(*name, value_ty);
            }
            ast::ExportDecl::Function {
                name,
                type_params,
                params,
                return_type,
                ..
            } => {
                if let Some(ty) = inferred_globals.get(name) {
                    exports.values.insert(*name, ty.clone());
                } else if type_params.is_empty() {
                    let param_types: Vec<Type<'a>> = params
                        .iter()
                        .map(|p| {
                            p.ty.as_ref()
                                .map(|t| {
                                    ast_type_to_export_type(
                                        t,
                                        &declared_structs,
                                        &declared_enums,
                                        &declared_aliases,
                                        &declared_newtypes,
                                        &mut HashSet::new(),
                                    )
                                })
                                .unwrap_or(Type::Infer)
                        })
                        .collect();
                    let ret = return_type
                        .as_ref()
                        .map(|t| {
                            ast_type_to_export_type(
                                t,
                                &declared_structs,
                                &declared_enums,
                                &declared_aliases,
                                &declared_newtypes,
                                &mut HashSet::new(),
                            )
                        })
                        .unwrap_or(Type::Infer);
                    exports.values.insert(
                        *name,
                        Type::Function {
                            params: param_types,
                            ret: Box::new(ret),
                            optional: 0,
                        },
                    );
                } else {
                    exports.values.insert(*name, Type::Infer);
                }
            }
            ast::ExportDecl::NamedGroup { names, .. } => {
                for export_name in names.iter() {
                    let local = export_name.name;
                    let external = export_name.alias.unwrap_or(local);

                    if let Some(info) = declared_structs.get(local) {
                        exports.structs.insert(external, info.clone());
                        // Promote receiver methods declared on the local
                        // struct to the exported name.
                        for ((rt, mn), mi) in receiver_methods.iter() {
                            if *rt == local {
                                exports.receiver_methods.insert((external, *mn), mi.clone());
                            }
                        }
                    }
                    if let Some(info) = declared_enums.get(local) {
                        exports.enums.insert(external, info.clone());
                    }
                    if let Some(ty) = declared_aliases.get(local) {
                        exports.aliases.insert(external, ty.clone());
                    }
                    if let Some(info) = declared_newtypes.get(local) {
                        exports.newtypes.insert(external, info.clone());
                        // Promote receiver methods declared on the local
                        // newtype to the exported name.
                        for ((rt, mn), mi) in receiver_methods.iter() {
                            if *rt == local {
                                exports.receiver_methods.insert((external, *mn), mi.clone());
                            }
                        }
                    }
                    if let Some(ty) = declared_values.get(local).cloned() {
                        exports.values.insert(external, ty);
                    }
                    // If the exported name is not declared in this module, it
                    // must be a re-export of an import (`export { value }`
                    // after `import { value } from "..."`). Record it so
                    // importers can resolve through the chain.
                    if !declared_structs.contains_key(local)
                        && !declared_enums.contains_key(local)
                        && !declared_aliases.contains_key(local)
                        && !declared_newtypes.contains_key(local)
                        && !declared_values.contains_key(local)
                    {
                        exports.re_exports.insert(external);
                    }
                }
            }
        }
    }

    exports
}

/// Information about an enum's cases, collected before typechecking bodies.
#[derive(Clone, Debug)]
pub struct EnumInfo<'a> {
    pub cases: &'a [ast::EnumCase<'a>],
    /// Declared type parameters, e.g. `T` in `enum Box<T>`. Kept so a use site
    /// spelled `Box<number>` can substitute them into case payload types
    /// (deka#372); previously they were parsed and discarded.
    pub type_params: &'a [ast::TypeParam<'a>],
    /// Declared `super enum` (rfd#41, deka#561 PR B): the type's descriptor
    /// survives to runtime and `Name.type()` is legal.
    pub is_super: bool,
}

/// Information about a newtype's primitive representation.
#[derive(Clone, Debug)]
pub struct NewtypeInfo {
    pub repr: ast::NewtypeRepr,
}

/// Information about a struct's fields and embedded structs, collected before
/// typechecking bodies.
#[derive(Clone, Debug)]
pub struct StructInfo<'a> {
    pub fields: &'a [ast::StructField<'a>],
    pub embeds: &'a [ast::Embed<'a>],
    /// Declared type parameters, e.g. `T` in `struct Box<T>`. Kept so a use
    /// site spelled `Box<number>` can substitute them into field types when
    /// building a `super` descriptor tree (kept for super declarations, PR B);
    /// previously they were parsed and discarded.
    pub type_params: &'a [ast::TypeParam<'a>],
    /// Declared `super struct` (rfd#41, deka#561 PR B): the type's descriptor
    /// survives to runtime and `Name.type()` is legal.
    pub is_super: bool,
}

/// Names of the primitive types that support receiver (extension) methods
/// (deka#527). Unlike structs and newtypes, primitives cannot carry methods
/// on a prototype — calls are rewritten to free functions at compile time —
/// and they are immutable values, so `mut` receivers are rejected.
pub(super) fn is_primitive_receiver_name(name: &str) -> bool {
    matches!(name, "string" | "number" | "boolean")
}

/// Information about a receiver method declared on a struct or primitive.
#[derive(Clone, Debug)]
pub struct MethodInfo<'a> {
    pub params: &'a [ast::Param<'a>],
    pub return_type: Option<ast::Type<'a>>,
    pub mutable: bool,
    /// Parameter types resolved once, in the declaring module (deka#494).
    /// Call sites and body checking reuse these instead of re-resolving the
    /// annotations (which would re-report unknown types).
    pub param_types: Vec<Type<'a>>,
    /// The resolved return annotation; `None` when the method has no return
    /// annotation.
    pub resolved_return: Option<Type<'a>>,
}

/// Information about an interface's declared members.
#[derive(Clone, Debug)]
pub struct InterfaceInfo<'a> {
    pub members: &'a [ast::InterfaceMember<'a>],
    pub span: ast::Span,
}

struct Checker<'a> {
    program: &'a ast::Program<'a>,
    errors: Vec<Diagnostic>,
    warnings: Vec<Diagnostic>,
    /// Function and (eventually) global variable types.
    globals: HashMap<&'a str, Type<'a>>,
    /// User-defined type aliases without type parameters.
    aliases: HashMap<&'a str, ast::Type<'a>>,
    /// User-defined enums.
    enums: HashMap<&'a str, EnumInfo<'a>>,
    /// Map from enum case name back to the enum that defines it.
    case_to_enum: HashMap<&'a str, &'a str>,
    /// User-defined structs.
    structs: HashMap<&'a str, StructInfo<'a>>,
    /// User-defined interfaces.
    interfaces: HashMap<&'a str, InterfaceInfo<'a>>,
    /// User-defined newtypes.
    newtypes: HashMap<&'a str, NewtypeInfo>,
    /// Receiver methods keyed by `(receiver_type, method_name)`. Primitive
    /// receivers (`string`, `number`, `boolean`) hold extension methods whose
    /// calls are rewritten to free functions (deka#527).
    receiver_methods: HashMap<(&'a str, &'a str), MethodInfo<'a>>,
    /// Primitive extension call sites to lower to free-function calls,
    /// keyed by call expression pointer.
    /// Lowering collections like this one must also be cleared in
    /// `reset_lowering_state` — the inference pass populates them too.
    method_calls: HashMap<*const ast::Expr<'a>, MethodTarget<'a>>,
    /// Builtin `.getType()` call sites to rewrite to `__deka_type_of(x)`,
    /// keyed by call expression pointer.
    /// Lowering collections like this one must also be cleared in
    /// `reset_lowering_state` — the inference pass populates them too.
    type_of_calls: HashSet<*const ast::Expr<'a>>,
    signature_calls: HashMap<*const ast::Expr<'a>, descriptor::DescriptorTree<'a>>,
    /// Builtin `Name.type()` call sites on `super` declarations (rfd#41,
    /// deka#561 PR B), keyed by call expression pointer. The emitter
    /// rewrites each call to the interned `__deka_super_desc$<Name>` const.
    /// Lowering collections like this one must also be cleared in
    /// `reset_lowering_state` — the inference pass populates them too.
    static_type_calls: HashMap<*const ast::Expr<'a>, descriptor::StaticTypeCall<'a>>,
    /// Descriptor trees for `super` declarations visible in this module
    /// (rfd#41, deka#561 PR B): built during declaration collection, seeded
    /// with local declarations and their transitive closure, extended on
    /// demand for imported super declarations at `Name.type()` call sites.
    /// Passed to the emitter, which interns one frozen const per referenced
    /// declaration. Declaration-derived, not a lowering pass effect — NOT
    /// cleared by `reset_lowering_state`.
    super_trees: HashMap<&'a str, descriptor::DescriptorTree<'a>>,
    json_calls: HashMap<*const ast::Expr<'a>, descriptor::JsonCall<'a>>,
    /// Builtin `Array.first()`/`Array.last()`/`Array.pop()`/`Array.shift()`
    /// call sites to rewrite to an Option-producing expression, keyed by call
    /// expression pointer. Lowering collections like this one must also be
    /// cleared in `reset_lowering_state` — the inference pass populates them
    /// too.
    array_builtin_calls: HashMap<*const ast::Expr<'a>, types::ArrayAccess>,
    /// Builtin `Math`-backed `number` method call sites to rewrite to a
    /// `Math.*` expression, keyed by call expression pointer (deka#378
    /// step 2). Lowering collections like this one must also be cleared in
    /// `reset_lowering_state` — the inference pass populates them too.
    number_math_calls: HashMap<*const ast::Expr<'a>, types::NumberMath>,
    /// Primitive conversion call sites to lower, keyed by call expression pointer.
    /// Cleared between passes by `reset_lowering_state`.
    unwrap_calls: HashMap<*const ast::Expr<'a>, types::UnwrapKind>,
    /// Operator expression sites that need newtype-aware lowering.
    /// Cleared between passes by `reset_lowering_state`.
    operator_rewrites: HashMap<*const ast::Expr<'a>, types::OperatorRewrite<'a>>,
    jsx_optional_props: HashMap<*const ast::JsxElement<'a>, JsxOptionalProps<'a>>,
    enum_case_patterns: HashMap<*const ast::Pattern<'a>, &'a str>,
    /// Union member type-pattern sites to lower, keyed by pattern pointer
    /// (rfd#42, deka#530).
    union_type_patterns: HashMap<*const ast::Pattern<'a>, types::UnionMemberTest<'a>>,
    /// Local scopes. The first scope is the top-level scope.
    scopes: Vec<HashMap<&'a str, Type<'a>>>,
    /// Bindings that were introduced with `let` and may be reassigned.
    /// Each entry mirrors the corresponding scope in `scopes`.
    mutables: Vec<HashSet<&'a str>>,
    /// Module-scope value bindings (`const`/`let`) seeded by
    /// `collect_module_value_bindings` before function bodies are checked
    /// (deka#600), not yet re-declared by `check_binding` in source order.
    /// A pending seed is visible inside function bodies but not to
    /// module-level statements, which keep rejecting forward references.
    pending_module_bindings: HashSet<&'a str>,
    /// Type parameter scopes. Each generic binding introduces a new scope.
    type_scopes: Vec<HashMap<&'a str, Type<'a>>>,
    /// Are we currently inside a function body?
    in_function: bool,
    /// Are we currently inside an async function body?
    in_async_function: bool,
    /// Expected / inferred return type of the current function.
    return_type: Option<Type<'a>>,
    /// How many nested loops currently enclose the checked statement?
    loop_depth: usize,
    /// When true, diagnostics are suppressed. Used during the pre-check
    /// inference pass that resolves forward-referenced function return types.
    infer_only: bool,
}

impl<'a> Checker<'a> {
    fn new(program: &'a ast::Program<'a>, imports: &HashMap<&str, &ModuleExports<'a>>) -> Self {
        let mut this = Self {
            program,
            errors: Vec::new(),
            warnings: Vec::new(),
            globals: HashMap::new(),
            aliases: HashMap::new(),
            enums: HashMap::new(),
            case_to_enum: HashMap::new(),
            structs: HashMap::new(),
            interfaces: HashMap::new(),
            newtypes: HashMap::new(),
            receiver_methods: HashMap::new(),
            method_calls: HashMap::new(),
            type_of_calls: HashSet::new(),
            signature_calls: HashMap::new(),
            static_type_calls: HashMap::new(),
            super_trees: HashMap::new(),
            json_calls: HashMap::new(),
            array_builtin_calls: HashMap::new(),
            number_math_calls: HashMap::new(),
            unwrap_calls: HashMap::new(),
            operator_rewrites: HashMap::new(),
            jsx_optional_props: HashMap::new(),
            enum_case_patterns: HashMap::new(),
            union_type_patterns: HashMap::new(),
            scopes: vec![HashMap::new()],
            mutables: vec![HashSet::new()],
            pending_module_bindings: HashSet::new(),
            type_scopes: Vec::new(),
            in_function: false,
            in_async_function: false,
            return_type: None,
            loop_depth: 0,
            infer_only: false,
        };
        this.seed_imports(imports);
        this.seed_builtins();
        this
    }

    fn seed_builtins(&mut self) {
        // Host-provided JavaScript globals still reachable from plain DekaScript.
        // They are typed opaquely as Infer; field/method access on Infer is
        // allowed and returns Infer, so anything flowing through one of these
        // stops being typechecked (deka#252). Console is excluded per RFD 32.
        //
        // RFD 21 and RFD 13's "imports over ambient globals" corollary say
        // ordinary DekaScript does not reach host globals at all. Removing a
        // name is only possible once a DekaScript replacement exists, so this
        // list shrinks as those land (deka#378):
        //
        //   removed: JSON    -> @deka/json
        //            crypto  -> @deka/crypto
        //            Date    -> @deka/time
        //
        //   remaining: Math      prelude methods on `number` landed (#378
        //                        step 2); the registration is deleted in
        //                        step 5, and `PI` still wants a home
        //              Object    Promise    parseInt    process    undecided
        //              isset     removed by #416
        //
        // `unsafe { }` bodies are raw JavaScript and are not checked against
        // this list, so a removed name is still reachable there — which is the
        // form the stdlib packages already use.
        // `process` is deliberately absent: it is behind the `env` capability
        // and reachable only from `unsafe { }` (deka#378). Naming it in plain
        // DekaScript is now `unknown identifier`, which is the diagnostic RFD
        // 13 P10 asks for.
        // `isset` is gone with deka#416: it existed only to test presence on an
        // interface `?:` field, which is now an `Option` like everywhere else.
        // Only `Math` is left. Its replacement exists as of deka#378 step 2 —
        // `sqrt`/`floor` and friends are prelude methods on `number` — but
        // the ambient registration stays until step 5 deletes it, and `PI`
        // still wants a home.
        //
        // `Object`, `Promise` and `parseInt` are gone. `Promise` stays a
        // *type* -- 63 annotations across the corpora are unaffected, because
        // types resolve through `resolve_ast_type` and never consulted this
        // list. `parseInt`'s replacement is `parseNumber(s)`, which yields
        // `Option<number>` rather than `NaN`, so removing it is a net
        // improvement rather than a subtraction.
        //
        // `deka` is the host-capability global (`deka.ui.State.create`, …),
        // settled by DS decision #6: host capabilities hang off the `deka`
        // global, language and stdlib stay imports. It is declared here so
        // the native and browser (wasm) compilers agree on it (deka#481).
        for name in ["Math", "deka"] {
            self.globals.insert(name, Type::Infer);
        }
    }

    fn seed_imports(&mut self, imports: &HashMap<&str, &ModuleExports<'a>>) {
        for stmt in self.program.statements.iter() {
            let ast::Stmt::Import {
                specifiers, source, ..
            } = stmt
            else {
                continue;
            };
            let Some(exports) = imports.get(source) else {
                continue;
            };
            for spec in specifiers.iter() {
                let imported = spec.imported;
                let local = spec.local;

                if let Some(info) = exports.structs.get(imported) {
                    self.structs.insert(local, info.clone());
                    for ((rt, mn), mi) in exports.receiver_methods.iter() {
                        if *rt == imported {
                            self.receiver_methods.insert((local, *mn), mi.clone());
                        }
                    }
                }

                if let Some(info) = exports.enums.get(imported) {
                    self.enums.insert(local, info.clone());
                    for case in info.cases.iter() {
                        self.case_to_enum.insert(case.name, local);
                    }
                }

                if let Some(ty) = exports.aliases.get(imported) {
                    self.aliases.insert(local, ty.clone());
                }

                if let Some(info) = exports.newtypes.get(imported) {
                    self.newtypes.insert(local, info.clone());
                    for ((rt, mn), mi) in exports.receiver_methods.iter() {
                        if *rt == imported {
                            self.receiver_methods.insert((local, *mn), mi.clone());
                        }
                    }
                }

                if let Some(ty) = exports.values.get(imported) {
                    self.declare_var(local, ty.clone());
                }
            }
        }
    }

    fn error_at_expr(&mut self, expr: &ast::Expr<'a>, message: impl Into<String>) {
        self.error_span(expr.span(), message);
    }

    /// Clear every lowering collection populated while checking.
    ///
    /// The silent inference pass runs `check_function` for real, so these maps
    /// fill up with entries the subsequent check pass must not see. Any new
    /// lowering collection added to `Checker` must be cleared here — missing
    /// one surfaces as a lowering bug far away from this call site (deka#367).
    pub(super) fn reset_lowering_state(&mut self) {
        self.method_calls.clear();
        self.type_of_calls.clear();
        self.signature_calls.clear();
        self.static_type_calls.clear();
        self.json_calls.clear();
        self.array_builtin_calls.clear();
        self.number_math_calls.clear();
        self.unwrap_calls.clear();
        self.operator_rewrites.clear();
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn declare_var(&mut self, name: &'a str, ty: Type<'a>) {
        self.scopes.last_mut().unwrap().insert(name, ty);
    }

    fn declare_mutable_var(&mut self, name: &'a str, ty: Type<'a>) {
        self.scopes.last_mut().unwrap().insert(name, ty);
        self.mutables.last_mut().unwrap().insert(name);
    }

    fn lookup_var(&self, name: &'a str) -> Option<Type<'a>> {
        for (depth, scope) in self.scopes.iter().enumerate().rev() {
            if let Some(ty) = scope.get(name) {
                // A pending module seed (deka#600) is visible only inside
                // function bodies; module-level statements keep rejecting
                // forward references until the declaration is checked.
                if depth == 0 && !self.in_function && self.pending_module_bindings.contains(name) {
                    return None;
                }
                return Some(ty.clone());
            }
        }
        self.globals.get(name).cloned()
    }

    fn is_number(ty: &Type<'_>) -> bool {
        matches!(ty, Type::Named { name: "number" })
    }

    fn is_string(ty: &Type<'_>) -> bool {
        matches!(ty, Type::Named { name: "string" })
    }

    fn is_boolean(ty: &Type<'_>) -> bool {
        matches!(ty, Type::Named { name: "boolean" })
    }

    fn is_promise(ty: &Type<'_>) -> bool {
        matches!(
            ty,
            Type::Generic {
                base: "Promise",
                ..
            }
        )
    }

    fn is_hole_expr(expr: &ast::Expr<'_>) -> bool {
        matches!(expr, ast::Expr::Identifier { name: "_", .. })
    }

    fn expect_number(&mut self, ty: &Type<'a>, span: ast::Span) {
        // `Var` is unconstrained and asserts nothing, so it satisfies any
        // expectation -- the same rule `is_assignable` applies (deka#468).
        if matches!(ty, Type::Var) {
            return;
        }
        if !ty.is_error() && !Self::is_number(ty) {
            let expected = Type::Named { name: "number" };
            self.error_span(
                span,
                with_union_narrowing_hint(
                    format!("expected type `number`, found type `{ty}`"),
                    &expected,
                    ty,
                ),
            );
        }
    }

    fn expect_boolean(&mut self, ty: &Type<'a>, span: ast::Span) {
        if matches!(ty, Type::Var) {
            return;
        }
        if !ty.is_error() && !Self::is_boolean(ty) {
            let expected = Type::Named { name: "boolean" };
            self.error_span(
                span,
                with_union_narrowing_hint(
                    format!("expected type `boolean`, found type `{ty}`"),
                    &expected,
                    ty,
                ),
            );
        }
    }

    /// Assignment / subtyping check. `actual` must be assignable to `expected`.
    fn is_assignable(&mut self, expected: &Type<'a>, actual: &Type<'a>) -> bool {
        if expected.is_error() || actual.is_error() {
            return true;
        }
        // `Var` is an unconstrained type variable: a construct that never named
        // this type, rather than one the checker failed to resolve. It carries
        // no claim, so unifying it with anything is sound and must stay true
        // even after `Infer` is tightened (deka#468).
        if matches!(expected, Type::Var) || matches!(actual, Type::Var) {
            return true;
        }
        // `Infer` is the unknown/externally-provided type. It is compatible with
        // any type until a concrete type is available. This is the deka#252
        // hole and is expected to be removed; `Var` above is not.
        if matches!(expected, Type::Infer) || matches!(actual, Type::Infer) {
            return true;
        }
        if expected == actual {
            return true;
        }
        // `never` is the bottom type: assignable to anything.
        if matches!(actual, Type::Never) {
            return true;
        }
        // Union assignability (rfd#42, deka#530). A union widens: `string` is
        // assignable to `string | number` because it matches SOME member; a
        // union actual is assignable to `expected` only when EVERY member is,
        // so `string | number` is never assignable to `string`. Union-to-union
        // requires every actual member to match some expected member. These
        // arms sit after Var/Infer (a union never silently absorbs or leaks
        // through them) and before the structural arms below.
        if let Type::Union {
            members: expected_members,
        } = expected
        {
            return match actual {
                Type::Union {
                    members: actual_members,
                } => actual_members
                    .iter()
                    .all(|am| expected_members.iter().any(|em| self.is_assignable(em, am))),
                _ => expected_members
                    .iter()
                    .any(|em| self.is_assignable(em, actual)),
            };
        }
        if let Type::Union {
            members: actual_members,
        } = actual
        {
            return actual_members
                .iter()
                .all(|am| self.is_assignable(expected, am));
        }
        // `none` is assignable to any Option<T>.
        if matches!(expected, Type::Option { .. }) && matches!(actual, Type::None) {
            return true;
        }
        // `none` is assignable to `void` (both represent "no useful return value").
        if matches!(expected, Type::Named { name: "void" }) && matches!(actual, Type::None) {
            return true;
        }
        // A concrete `T` is NOT assignable to `Option<T>`. This used to permit it
        // as "sugar for Some(T)", but the emitter never implemented the sugar: the
        // raw value was left in place, so `S { path: "/x" }` on a `path: string?`
        // field typechecked and then threw `non-exhaustive match` at runtime on
        // the first `match`. `Some(x)` must be written explicitly, matching how
        // `Result` already behaves (deka#401).
        // `Option<A>` is assignable to `Option<B>` when `A` is assignable to `B`.
        if let (
            Type::Option {
                inner: expected_inner,
            },
            Type::Option {
                inner: actual_inner,
            },
        ) = (expected, actual)
        {
            if self.is_assignable(expected_inner, actual_inner) {
                return true;
            }
        }
        // Structural subtyping for generic types like Result<T, E>.
        if let (
            Type::Generic {
                base: expected_base,
                args: expected_args,
            },
            Type::Generic {
                base: actual_base,
                args: actual_args,
            },
        ) = (expected, actual)
        {
            if expected_base == actual_base && expected_args.len() == actual_args.len() {
                return expected_args
                    .iter()
                    .zip(actual_args.iter())
                    .all(|(e, a)| self.is_assignable(e, a));
            }
        }
        // Arrays are covariant in their element type.
        if let (
            Type::Array {
                elem: expected_elem,
            },
            Type::Array { elem: actual_elem },
        ) = (expected, actual)
        {
            return self.is_assignable(expected_elem, actual_elem);
        }
        // Object structural subtyping: actual must supply at least the expected fields.
        if let (
            Type::Object {
                fields: expected_fields,
            },
            Type::Object {
                fields: actual_fields,
            },
        ) = (expected, actual)
        {
            return expected_fields.iter().all(|(name, expected_ty)| {
                actual_fields
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, actual_ty)| self.is_assignable(expected_ty, actual_ty))
                    .unwrap_or(false)
            });
        }
        // Function subtyping: parameters are contravariant, return type is covariant.
        if let (
            Type::Function {
                params: expected_params,
                ret: expected_ret,
                optional: expected_optional,
            },
            Type::Function {
                params: actual_params,
                ret: actual_ret,
                optional: actual_optional,
            },
        ) = (expected, actual)
        {
            if expected_params.len() == actual_params.len() && expected_optional == actual_optional
            {
                return actual_params
                    .iter()
                    .zip(expected_params.iter())
                    .all(|(a, e)| self.is_assignable(a, e))
                    && self.is_assignable(expected_ret, actual_ret);
            }
        }
        // Interface satisfaction: structs and objects must supply every
        // required field and method with a compatible type.
        if let Type::Interface { name } = expected {
            let members: Vec<ast::InterfaceMember<'a>> = self
                .interfaces
                .get(name)
                .map(|info| info.members.to_vec())
                .unwrap_or_default();
            if members.is_empty() {
                return true;
            }

            match actual {
                Type::Object {
                    fields: actual_fields,
                } => {
                    for member in members.iter() {
                        match member {
                            ast::InterfaceMember::Field {
                                name: field_name,
                                ty,
                                optional,
                                ..
                            } => {
                                let mut expected_ty = self.resolve_ast_type(ty);
                                if expected_ty.is_error() {
                                    continue;
                                }
                                // `name?: T` is a field of type `Option<T>`,
                                // the same as `name: T?` (deka#416).
                                if *optional {
                                    expected_ty = Type::Option {
                                        inner: Box::new(expected_ty),
                                    };
                                }
                                let Some((_, actual_ty)) =
                                    actual_fields.iter().find(|(n, _)| n == field_name)
                                else {
                                    // Omission is not allowed in a plain object
                                    // literal: there is no construction site
                                    // for the compiler to fill, so the field
                                    // would be JS `undefined` while the type
                                    // says `Option<T>` -- the exact runtime
                                    // hole deka#401 closed for `T?`. JSX is
                                    // different and fills it; see the emitter.
                                    return false;
                                };
                                if !self.is_assignable(&expected_ty, actual_ty) {
                                    return false;
                                }
                            }
                            ast::InterfaceMember::Method {
                                name: method_name,
                                params,
                                return_type,
                                ..
                            } => {
                                let expected_params: Vec<Type<'a>> = params
                                    .iter()
                                    .map(|p| {
                                        p.ty.as_ref()
                                            .map(|t| self.resolve_ast_type(t))
                                            .unwrap_or(Type::Infer)
                                    })
                                    .collect();
                                let expected_ret = return_type
                                    .as_ref()
                                    .map(|t| self.resolve_ast_type(t))
                                    .unwrap_or(Type::Named { name: "void" });
                                let expected_fn = Type::Function {
                                    params: expected_params,
                                    ret: Box::new(expected_ret),
                                    optional: 0,
                                };

                                let Some((_, actual_ty)) =
                                    actual_fields.iter().find(|(n, _)| n == method_name)
                                else {
                                    return false;
                                };
                                if !self.is_assignable(&expected_fn, actual_ty) {
                                    return false;
                                }
                            }
                        }
                    }
                    return true;
                }
                Type::Struct { name: struct_name } => {
                    let struct_fields: Vec<(&'a str, ast::Type<'a>)> = self
                        .structs
                        .get(struct_name)
                        .map(|info| info.fields.iter().map(|f| (f.name, f.ty.clone())).collect())
                        .unwrap_or_default();
                    let struct_methods: Vec<(&'a str, MethodInfo<'a>)> =
                        self.collect_struct_methods(struct_name);

                    for member in members.iter() {
                        match member {
                            ast::InterfaceMember::Field {
                                name: field_name,
                                ty,
                                optional,
                                ..
                            } => {
                                let expected_ty = self.resolve_ast_type(ty);
                                if expected_ty.is_error() {
                                    continue;
                                }
                                let Some((_, actual_ast_ty)) =
                                    struct_fields.iter().find(|(n, _)| n == field_name)
                                else {
                                    if *optional {
                                        continue;
                                    }
                                    return false;
                                };
                                let actual_ty = self.resolve_ast_type(actual_ast_ty);
                                if !self.is_assignable(&expected_ty, &actual_ty) {
                                    return false;
                                }
                            }
                            ast::InterfaceMember::Method {
                                name: method_name,
                                params,
                                return_type,
                                ..
                            } => {
                                let expected_params: Vec<Type<'a>> = params
                                    .iter()
                                    .map(|p| {
                                        p.ty.as_ref()
                                            .map(|t| self.resolve_ast_type(t))
                                            .unwrap_or(Type::Infer)
                                    })
                                    .collect();
                                let expected_ret = return_type
                                    .as_ref()
                                    .map(|t| self.resolve_ast_type(t))
                                    .unwrap_or(Type::Named { name: "void" });
                                let expected_fn = Type::Function {
                                    params: expected_params,
                                    ret: Box::new(expected_ret),
                                    optional: 0,
                                };

                                let Some((_, method_info)) =
                                    struct_methods.iter().find(|(n, _)| n == method_name)
                                else {
                                    return false;
                                };
                                let actual_params: Vec<Type<'a>> = method_info
                                    .params
                                    .iter()
                                    .map(|p| {
                                        p.ty.as_ref()
                                            .map(|t| self.resolve_ast_type(t))
                                            .unwrap_or(Type::Infer)
                                    })
                                    .collect();
                                let actual_ret = method_info
                                    .return_type
                                    .as_ref()
                                    .map(|t| self.resolve_ast_type(t))
                                    .unwrap_or(Type::Named { name: "void" });
                                let actual_fn = Type::Function {
                                    params: actual_params,
                                    ret: Box::new(actual_ret),
                                    optional: 0,
                                };
                                if !self.is_assignable(&expected_fn, &actual_fn) {
                                    return false;
                                }
                            }
                        }
                    }
                    return true;
                }
                _ => return false,
            }
        }
        false
    }

    /// Collect all receiver methods available on a struct, including methods
    /// inherited from embedded structs.
    fn collect_struct_methods(&self, struct_name: &'a str) -> Vec<(&'a str, MethodInfo<'a>)> {
        let mut methods = Vec::new();
        let mut seen = HashSet::new();
        self.collect_struct_methods_rec(struct_name, &mut methods, &mut seen);
        methods
    }

    fn collect_struct_methods_rec(
        &self,
        struct_name: &'a str,
        methods: &mut Vec<(&'a str, MethodInfo<'a>)>,
        seen: &mut HashSet<&'a str>,
    ) {
        if !seen.insert(struct_name) {
            return;
        }
        for ((rt, mn), mi) in self.receiver_methods.iter() {
            if *rt == struct_name && !methods.iter().any(|(n, _)| n == mn) {
                methods.push((*mn, mi.clone()));
            }
        }
        if let Some(info) = self.structs.get(struct_name) {
            for embed in info.embeds.iter() {
                self.collect_struct_methods_rec(embed.name, methods, seen);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bumpalo::Bump;

    use super::*;
    use crate::parse::parse;

    fn typeck(source: &str) -> Vec<Diagnostic> {
        let arena = Bump::new();
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        check_program(&program, source).errors
    }

    #[test]
    fn array_map_solves_callback_return_type() {
        // deka#467: `map` is (T -> U) -> Array<U>; U solves from the
        // callback's return type, so downstream uses see the real element.
        assert!(typeck(
            "const a = [1, 2, 3].map(fn(x: number) string { return \"s\" });\nconst s: string = a[0];"
        )
        .is_empty());
        let errors = typeck(
            "const a = [1, 2, 3].map(fn(x: number) string { return \"s\" });\nconst n: number = a[0];",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn array_map_rejects_wrong_element_callback() {
        let errors = typeck("const a = [1, 2, 3].map(fn(x: string) string { return x });");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("expected argument type"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn array_reduce_solves_accumulator_type() {
        // deka#467: `reduce` is ((A, T) -> A) -> A; A solves from the
        // callback, so the result is the accumulator type, not a wildcard.
        assert!(typeck(
            "const sum = [1, 2, 3].reduce(fn(acc: number, x: number) number { return acc + x });\nconst n: number = sum;"
        )
        .is_empty());
        let errors = typeck(
            "const sum = [1, 2, 3].reduce(fn(acc: number, x: number) number { return acc + x });\nconst s: string = sum;",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn array_map_unknown_callback_stays_unconstrained() {
        // A callback whose type is unknown cannot solve U. The result stays
        // `Array<Var>` — unconstrained, exactly as before `map` gained a type
        // parameter (deka#468 semantics).
        assert!(typeck(
            "const a = [1, 2, 3];\nconst cbs = [];\nconst d = a.map(cbs[0]);\nconst s: string = d[0];"
        )
        .is_empty());
    }

    #[test]
    fn const_number_passes() {
        assert!(typeck("const x: number = 42;").is_empty());
    }

    #[test]
    fn function_add_passes() {
        assert!(typeck("fn add(a: number, b: number) number { return a + b; }").is_empty());
    }

    #[test]
    fn const_string_mismatch_fails() {
        let errors = typeck("const x: string = 42;");
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("number"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn call_wrong_arg_type_fails() {
        let errors =
            typeck("fn add(a: number, b: number) number { return a + b; } add(\"one\", 2);");
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].message.contains("number"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn return_wrong_type_fails() {
        let errors = typeck("fn f() number { return \"x\"; }");
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].message.contains("number"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn recursive_function_passes() {
        assert!(typeck("fn forever(n: number) number { return forever(n); }").is_empty());
    }

    #[test]
    fn match_option_number_passes() {
        assert!(typeck(
            "const o = Some(5); const x: number = match o { Some(n) => n, None => 0 };"
        )
        .is_empty());
    }

    #[test]
    fn match_arm_type_mismatch_fails() {
        let errors = typeck(
            "const o = Some(5); const x: number = match o { Some(n) => n, None => \"oops\" };",
        );
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].message.contains("number"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn option_some_constructor_passes() {
        assert!(typeck("const o: Option<number> = Some(5);").is_empty());
    }

    #[test]
    fn result_ok_constructor_passes() {
        assert!(typeck("const r: Result<number, string> = Ok(5);").is_empty());
    }

    #[test]
    fn panic_is_never_and_assignable() {
        assert!(typeck("const x: number = panic(\"boom\");").is_empty());
        assert!(typeck("const y: string = deka.panic(\"boom\");").is_empty());
    }

    #[test]
    fn panic_expects_one_string() {
        let errors = typeck("const x: number = panic();");
        assert!(
            errors.iter().any(|e| e.message.contains("panic")),
            "{errors:?}"
        );
        let errors = typeck("const x: number = panic(1);");
        assert!(
            errors.iter().any(|e| e.message.contains("string")),
            "{errors:?}"
        );
    }

    #[test]
    fn struct_literal_and_field_access_passes() {
        assert!(typeck("struct Point { x: number; y: number } const p: Point = Point { x: 1, y: 2 }; const x: number = p.x;").is_empty());
    }

    #[test]
    fn user_defined_enum_constructor_passes() {
        assert!(typeck("enum Color { Red, Green, Blue } const c: Color = Color.Red;").is_empty());
    }

    #[test]
    fn user_defined_enum_payload_constructor_passes() {
        assert!(typeck("enum Shape { Circle(number), Label(string) } const s: Shape = Shape.Circle(5); const t: Shape = Shape.Label(\"hello\");").is_empty());
    }

    #[test]
    fn user_defined_enum_wrong_payload_type_fails() {
        let errors =
            typeck("enum Shape { Circle(number) } const s: Shape = Shape.Circle(\"oops\");");
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].message.contains("number"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn user_defined_enum_match_passes() {
        assert!(typeck("enum Color { Red, Green, Blue } const c: Color = Color.Red; const x: number = match c { Red => 1, Green => 2, Blue => 3 };").is_empty());
    }

    #[test]
    fn receiver_method_passes() {
        assert!(typeck(
            "struct Point { x: number; y: number } fn (p Point) distance(other: Point) number { return 0; } const p1: Point = Point { x: 0, y: 0 }; const p2: Point = Point { x: 3, y: 4 }; const d: number = p1.distance(p2);"
        ).is_empty());
    }

    #[test]
    fn primitive_extension_method_passes() {
        assert!(typeck(
            "fn (s string) slugify() string { return s.toLowerCase(); } const title: string = \"Hello World\".slugify();"
        ).is_empty());
        // number and boolean receivers work too.
        assert!(typeck(
            "fn (n number) squared() number { return n * n; } fn (b boolean) flip() boolean { return !b; } const x: number = 3.squared(); const y: boolean = true.flip();"
        ).is_empty());
    }

    #[test]
    fn primitive_extension_call_records_mangled_target() {
        let arena = Bump::new();
        let source =
            "fn (s string) slugify() string { return s; } const title: string = \"Hi\".slugify();";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let typeck = check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        assert_eq!(typeck.method_calls.len(), 1);
        assert!(
            typeck
                .method_calls
                .values()
                .all(|t| t.mangled == "slugify$string"),
            "expected slugify$string, got {:?}",
            typeck
                .method_calls
                .values()
                .map(|t| &t.mangled)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn primitive_extension_wrong_receiver_names_both_types() {
        let errors = typeck(
            "fn (s string) slugify() string { return s; } const n: number = 42; const bad: string = n.slugify();"
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("slugify"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("number"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn primitive_extension_wrong_arg_count_fails() {
        let errors = typeck(
            "fn (s string) wrap(prefix: string) string { return prefix + s; } const w: string = \"x\".wrap();"
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("wrap"), "{}", errors[0].message);
        assert!(
            errors[0].message.contains("1 argument"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn type_toString_is_builtin_method() {
        // `Type` is seeded as a builtin named type; `toString` on it resolves
        // through the primitive member table (rfd#41, deka#529).
        assert!(typeck("fn f(t: Type) string { return t.toString(); }").is_empty());
    }

    // ------------------------------------------------------------------
    // `super` declarations (rfd#41, deka#561 PR B)
    // ------------------------------------------------------------------

    #[test]
    fn super_struct_type_call_passes_and_records() {
        let arena = Bump::new();
        let source = "super struct User { id: number }\nconst t = User.type();";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let typeck = check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        assert_eq!(typeck.static_type_calls.len(), 1);
        assert!(typeck.super_trees.contains_key("User"));
        assert!(
            matches!(typeck.super_trees.get("User"), Some(DescriptorTree::Struct { name, .. }) if *name == "User")
        );
    }

    #[test]
    fn super_type_on_plain_struct_fails_teaching_super() {
        let errors = typeck("struct User { id: number }\nconst t = User.type();");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("User"), "{}", errors[0].message);
        assert!(errors[0].message.contains("super struct"), "{}", errors[0].message);
    }

    #[test]
    fn super_type_on_plain_enum_fails_teaching_super() {
        let errors = typeck("enum Status { Active }\nconst t = Status.type();");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("super enum"), "{}", errors[0].message);
    }

    #[test]
    fn super_type_on_unknown_name_still_unknown_identifier() {
        let errors = typeck("const t = Nope.type();");
        // The unknown name is reported on both the object and callee paths;
        // what matters is it stays an unknown-identifier error, not a
        // super-specific one.
        assert!(errors.iter().any(|e| e.message.contains("unknown identifier")), "{errors:?}");
    }

    #[test]
    fn super_type_on_newtype_explains_the_limit() {
        let errors = typeck("type Cents number\nconst t = Cents.type();");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("only available"), "{}", errors[0].message);
    }

    #[test]
    fn super_type_with_args_fails() {
        let errors = typeck("super struct User { id: number }\nconst t = User.type(1);");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("no arguments"), "{}", errors[0].message);
    }

    #[test]
    fn super_marking_is_transitive() {
        // `Address` is never written `super`, but `super struct User`
        // references it, so its descriptor is built too — composition must
        // not require repeating the mark.
        let arena = Bump::new();
        let source = "struct Address { city: string }\nsuper struct User { id: number; address: Address }\nconst a = Address.type();";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let typeck = check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        assert!(typeck.super_trees.contains_key("Address"));
    }

    #[test]
    fn super_struct_recursive_type_passes() {
        let arena = Bump::new();
        let source = "super struct Node { next: Option<Node> }\nconst t = Node.type();";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let typeck = check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        // The tree holds a Recurse reference, not an infinite expansion.
        let tree = typeck.super_trees.get("Node").expect("Node tree");
        let fields = match tree {
            DescriptorTree::Struct { fields, .. } => fields,
            other => panic!("expected struct tree, got {other:?}"),
        };
        assert!(
            matches!(&fields[0].ty, DescriptorTree::Option { inner } if matches!(&**inner, DescriptorTree::Recurse { name } if *name == "Node")),
            "expected Option<Recurse Node>, got {:?}",
            fields[0].ty
        );
    }

    #[test]
    fn super_struct_undescribable_field_fails_at_declaration() {
        // A function-typed field can never be reflected; the error must name
        // the declaration and the field, even with no `.type()` call.
        let errors = typeck("super struct S { f: fn(number) number }");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("cannot carry runtime type information"), "{}", errors[0].message);
        assert!(errors[0].message.contains("field `f`"), "{}", errors[0].message);
    }

    #[test]
    fn super_type_through_alias_resolves_to_decl() {
        let arena = Bump::new();
        let source = "super struct User { id: number }\nalias Alias = User\nconst t = Alias.type();";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let typeck = check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        assert_eq!(typeck.static_type_calls.len(), 1);
    }

    #[test]
    fn super_enum_type_call_passes() {
        assert!(typeck("super enum Status { Active, Archived }\nconst t = Status.type();").is_empty());
    }

    #[test]
    fn type_declaration_named_type_fails() {
        // `Type` is reserved for the builtin descriptor type.
        for src in [
            "struct Type { x: number }",
            "enum Type { Red }",
            "type Type number",
            "interface Type { fn get() number }",
            "alias Type = number",
        ] {
            let errors = typeck(src);
            assert_eq!(errors.len(), 1, "{src}: {errors:?}");
            assert!(
                errors[0].message.contains("`Type` is a builtin type"),
                "{}",
                errors[0].message
            );
        }
    }

    #[test]
    fn gettype_on_receiver_kinds_returns_type() {
        // Primitives, struct, newtype, enum, array, Option, Result, object.
        assert!(typeck("const t: Type = \"x\".getType();").is_empty());
        assert!(typeck("const n: number = 1; const t: Type = n.getType();").is_empty());
        assert!(typeck("const b: boolean = true; const t: Type = b.getType();").is_empty());
        assert!(typeck("struct Point { x: number } const p: Point = Point { x: 1 }; const t: Type = p.getType();").is_empty());
        assert!(typeck(
            "type Cents number; const c: Cents = Cents(5); const t: Type = c.getType();"
        )
        .is_empty());
        assert!(typeck(
            "enum Color { Red, Green } const c: Color = Color.Red; const t: Type = c.getType();"
        )
        .is_empty());
        assert!(typeck("const t: Type = [1, 2].getType();").is_empty());
        assert!(typeck("const t: Type = Some(1).getType();").is_empty());
        assert!(
            typeck("const r: Result<number, string> = Ok(1); const t: Type = r.getType();")
                .is_empty()
        );
        assert!(typeck("const o = { a: 1 }; const t: Type = o.getType();").is_empty());
    }

    #[test]
    fn gettype_union_receiver_passes() {
        // The rfd#41 headline case: a union value's runtime type.
        assert!(typeck("fn f(v: number | string) Type { return v.getType(); }").is_empty());
    }

    #[test]
    fn signature_records_declared_type_descriptor() {
        let arena = Bump::new();
        let source = "fn f(v: number | string) Type { return v.signature(); }";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let typeck = check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        assert_eq!(typeck.signature_calls.len(), 1);
        assert!(
            matches!(typeck.signature_calls.values().next(), Some(DescriptorTree::Union { members }) if members.len() == 2)
        );
    }

    #[test]
    fn signature_describes_interface_declaration() {
        assert!(typeck(
            "interface Named { name: string } fn f(v: Named) Type { return v.signature(); }"
        )
        .is_empty());
    }

    #[test]
    fn gettype_generic_receiver_passes() {
        // Runtime inspection: `.getType()` works on a builtin-generic
        // receiver with no `super` keyword (user-declared type parameters
        // are banned, deka#561).
        assert!(typeck("fn id(x: Array<number>) Type { return x.getType(); }").is_empty());
    }

    #[test]
    fn gettype_records_type_of_call() {
        let arena = Bump::new();
        let source = "const t: Type = \"hi\".getType();";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let typeck = check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        assert_eq!(typeck.type_of_calls.len(), 1);
        assert!(typeck.method_calls.is_empty());
    }

    #[test]
    fn array_first_last_record_calls_and_have_option_type() {
        let arena = Bump::new();
        let source = "const a: Array<number> = [1, 2, 3];\nlet f = unwrap(a.first()) or { 0 };\nlet l = unwrap(a.last()) or { 0 };";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let checked = check_program(&program, source);
        assert!(checked.errors.is_empty(), "{:?}", checked.errors);
        // Both call sites are recorded for the emitter, with the right kinds.
        assert_eq!(
            checked.array_builtin_calls.len(),
            2,
            "{:?}",
            checked.array_builtin_calls
        );
        assert_eq!(
            checked
                .array_builtin_calls
                .values()
                .filter(|k| matches!(k, ArrayAccess::First))
                .count(),
            1
        );
        assert_eq!(
            checked
                .array_builtin_calls
                .values()
                .filter(|k| matches!(k, ArrayAccess::Last))
                .count(),
            1
        );
        // The result type is Option<number>, not number — the same type
        // pop/shift declare, and all four builtins are now actually emitted
        // that way (deka#561, deka#566).
        let errors = typeck("const a: Array<number> = [1];\nconst bad: string = a.first();");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("Option<number>"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn array_pop_shift_reject_immutable_receiver() {
        // deka#566: const array literals are frozen at emit, so a runtime
        // pop/shift threw `TypeError: Cannot delete property`. The rejection
        // is a compile error naming the immutability, not a runtime throw.
        for method in ["pop", "shift"] {
            let errors = typeck(&format!(
                "const a: Array<number> = [1, 2, 3];\nconst p = a.{method}();"
            ));
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert!(
                errors[0].message.contains("immutable receiver"),
                "{}",
                errors[0].message
            );
            assert!(
                errors[0].message.contains(method),
                "diagnostic should name the method: {}",
                errors[0].message
            );
        }
    }

    #[test]
    fn array_pop_shift_mutable_receiver_records_rewrite() {
        // Mutable receivers typecheck clean and record the call for the
        // emitter's Option-construction rewrite, pop and shift separately.
        let arena = Bump::new();
        let source = "let a: Array<number> = [1, 2, 3];\nconst p = a.pop();\nconst s = a.shift();";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let checked = check_program(&program, source);
        assert!(checked.errors.is_empty(), "{:?}", checked.errors);
        assert_eq!(checked.array_builtin_calls.len(), 2, "{:?}", checked.array_builtin_calls);
        assert_eq!(
            checked.array_builtin_calls.values().filter(|k| matches!(k, ArrayAccess::Pop)).count(),
            1
        );
        assert_eq!(
            checked.array_builtin_calls.values().filter(|k| matches!(k, ArrayAccess::Shift)).count(),
            1
        );
    }

    #[test]
    fn array_pop_result_is_option_not_raw_element() {
        // The declared type must be the runtime type: unwrap(a.pop()) narrows
        // to the element, assigning the raw pop result to string is rejected.
        let errors = typeck("let a: Array<number> = [1];\nconst bad: string = a.pop();");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("Option<number>"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn array_first_last_arity_is_checked() {
        let errors = typeck("const a: Array<number> = [1];\nconst f = a.first(1);");
        assert!(!errors.is_empty());
        assert!(
            errors.iter().any(|e| e.message.contains("argument")),
            "{errors:?}"
        );
    }

    #[test]
    fn array_other_methods_are_not_recorded() {
        let arena = Bump::new();
        let source = "const a: Array<number> = [1];\nconst n: number = a.length;\nconst m: Array<number> = a.slice(0, 1);";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let typeck = check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        assert!(typeck.array_builtin_calls.is_empty());
    }

    #[test]
    fn number_math_records_calls_with_total_and_partial_kinds() {
        let arena = Bump::new();
        let source = "const f: number = (3.7).floor();\nconst m: number = (1).max(2);\nconst s: Option<number> = (4).sqrt();\nconst p: Option<number> = (2).pow(10);";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let checked = check_program(&program, source);
        assert!(checked.errors.is_empty(), "{:?}", checked.errors);
        assert_eq!(
            checked.number_math_calls.len(),
            4,
            "{:?}",
            checked.number_math_calls
        );
        assert_eq!(
            checked
                .number_math_calls
                .values()
                .filter(|k| matches!(k, NumberMath::Total))
                .count(),
            2
        );
        assert_eq!(
            checked
                .number_math_calls
                .values()
                .filter(|k| matches!(k, NumberMath::Partial))
                .count(),
            2
        );
    }

    #[test]
    fn number_math_partial_methods_have_option_type() {
        // The whole point of the wrapper: a partial method cannot hand back
        // a `number` that is not one, so `sqrt` is `Option<number>` and
        // assigning it to a plain `number` is rejected (rfd#13, deka#378
        // step 2).
        let errors = typeck("const bad: number = (4).sqrt();");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("Option<number>"),
            "{}",
            errors[0].message
        );
        assert!(typeck("const good: number = unwrap((4).sqrt()) or { 0 };").is_empty());
    }

    #[test]
    fn number_math_lists_are_disjoint_exhaustive_and_in_sync() {
        // Three artefacts describe the Math-backed method surface on
        // `number`: NUMBER_MATH_TOTAL, NUMBER_MATH_PARTIAL, and the
        // ("number", …) arms of primitive_member. A doc-comment was the only
        // thing keeping them in step — sin/cos/tan landed on the wrong side
        // of the total/partial split because nothing checked (deka#594
        // review). This test is the sync.
        let total = expr::NUMBER_MATH_TOTAL;
        let partial = expr::NUMBER_MATH_PARTIAL;

        // Disjoint: a method on both lists is typed two ways at once.
        for name in total {
            assert!(!partial.contains(name), "{name} is in BOTH lists");
        }

        // The classifier must agree with the list membership.
        for name in total {
            assert_eq!(
                expr::number_math_kind(name),
                Some(NumberMath::Total),
                "{name}: TOTAL list, classifier disagrees"
            );
        }
        for name in partial {
            assert_eq!(
                expr::number_math_kind(name),
                Some(NumberMath::Partial),
                "{name}: PARTIAL list, classifier disagrees"
            );
        }

        // The ("number", …) arms must return what the list promises: plain
        // `number` for total, `Option<number>` for partial. (The member
        // type is a zero-arg function whose return type carries the
        // promise.)
        for name in total {
            match expr::primitive_member("number", name, None) {
                Some(expr::PrimitiveMember::BuiltinMethod(Type::Function { ret, .. })) => assert!(
                    matches!(*ret, Type::Named { name: "number" }),
                    "{name}: total arm must return plain number, got {ret:?}"
                ),
                other => panic!("{name}: primitive_member arm missing or wrong: {other:?}"),
            }
        }
        for name in partial {
            let ret = match expr::primitive_member("number", name, None) {
                Some(expr::PrimitiveMember::BuiltinMethod(Type::Function { ret, .. })) => ret,
                other => panic!("{name}: primitive_member arm missing or wrong: {other:?}"),
            };
            match *ret {
                Type::Option { ref inner } => assert!(
                    matches!(**inner, Type::Named { name: "number" }),
                    "{name}: partial arm must return Option<number>"
                ),
                ref other => panic!("{name}: partial arm must return Option<number>, got {other:?}"),
            }
        }

        // Exhaustive over the exposed surface: TOTAL ∪ PARTIAL ∪
        // {max, min, pow} is exactly this list. Adding a Math method means
        // extending this list AND both tables AND the classifier together.
        let mut surface: Vec<&str> = total.iter().chain(partial.iter()).copied().collect();
        surface.extend(["max", "min", "pow"]);
        surface.sort_unstable();
        assert_eq!(
            surface,
            [
                "abs", "acos", "acosh", "asin", "atan", "atanh", "cbrt", "ceil", "cos", "cosh",
                "exp", "floor", "log", "log10", "log2", "max", "min", "pow", "round", "sign",
                "sin", "sinh", "sqrt", "tan", "tanh", "trunc",
            ]
        );
    }

    #[test]
    fn number_math_args_are_checked() {
        // Unlike the ambient `Math` global — typed `Infer`, so anything went
        // (deka#252) — the replacement checks arity and argument types.
        let errors = typeck("const bad = (3.7).floor(1);");
        assert!(!errors.is_empty(), "arity must be checked");
        let errors = typeck("const bad = (1).max(\"x\");");
        assert!(!errors.is_empty(), "argument types must be checked");
    }

    #[test]
    fn number_math_extension_shadows_builtin() {
        // A user extension named `floor` keeps the deka#527 rewrite; the
        // Math-backed builtin is not recorded (deka#378 step 2).
        let arena = Bump::new();
        let source =
            "fn (n number) floor() string { return \"x\"; } const u: string = (3.7).floor();";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let typeck = check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        assert_eq!(typeck.method_calls.len(), 1);
        assert!(typeck.number_math_calls.is_empty());
    }

    #[test]
    fn gettype_user_extension_shadows_builtin() {
        // A user extension named `getType` keeps the deka#527 rewrite; the
        // builtin `__deka_type_of` rewrite is not recorded.
        let arena = Bump::new();
        let source =
            "fn (s string) getType() string { return s; } const u: string = \"x\".getType();";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let typeck = check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        assert_eq!(typeck.method_calls.len(), 1);
        assert!(
            typeck
                .method_calls
                .values()
                .all(|t| t.mangled == "getType$string"),
            "expected getType$string, got {:?}",
            typeck
                .method_calls
                .values()
                .map(|t| &t.mangled)
                .collect::<Vec<_>>()
        );
        assert!(typeck.type_of_calls.is_empty());
    }

    #[test]
    fn gettype_receiver_method_shadows_builtin() {
        // A struct-declared `getType` wins over the builtin fallback.
        assert!(typeck(
            "struct A { x: number } fn (a A) getType() string { return \"A\" } const s: string = A { x: 1 }.getType();"
        ).is_empty());
    }

    #[test]
    fn gettype_interface_member_shadows_builtin() {
        // An interface declaring `getType` dispatches dynamically, as with
        // any declared member.
        assert!(typeck(
            "interface Has { fn getType() string } fn f(v: Has) string { return v.getType(); }"
        )
        .is_empty());
    }

    #[test]
    fn gettype_with_arguments_fails() {
        let errors = typeck("const t: Type = \"x\".getType(1);");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("`getType` expects no arguments"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn gettype_toString_chaining_passes() {
        assert!(typeck("const s: string = \"x\".getType().toString();").is_empty());
    }

    #[test]
    fn primitive_extension_wrong_arg_type_fails() {
        let errors = typeck(
            "fn (s string) repeat(n: number) string { return s; } const r: string = \"x\".repeat(\"three\");"
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("number"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn primitive_extension_mut_receiver_fails() {
        let errors = typeck("fn (s mut string) broken() string { return s; }");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("mutable receiver"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn primitive_extension_duplicate_fails() {
        let errors = typeck(
            "fn (s string) slugify() string { return s; } fn (s string) slugify() string { return s; }"
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("duplicate receiver method"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn primitive_extension_property_read_fails() {
        let errors =
            typeck("fn (s string) slugify() string { return s; } const f = \"x\".slugify;");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("slugify"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn primitive_extension_shadows_builtin_call_only() {
        // Call-shaped access routes to the extension...
        assert!(typeck(
            "fn (s string) toUpperCase() string { return s; } const u: string = \"x\".toUpperCase();"
        ).is_empty());
        // ...while the builtin property `length` is untouched.
        assert!(typeck(
            "fn (s string) slugify() string { return s; } const n: number = \"abc\".length;"
        )
        .is_empty());
    }

    #[test]
    fn primitive_extension_builtin_property_collision_fails() {
        // `length` stays property-shaped even for call-shaped access, so an
        // extension of the same name would give one name two silent meanings.
        let errors = typeck("fn (s string) length() number { return 0; }");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("builtin property"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("length"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
        // Builtin methods stay shadowable.
        assert!(typeck(
            "fn (s string) toUpperCase() string { return s; } const u: string = \"x\".toUpperCase();"
        ).is_empty());
    }

    #[test]
    fn collection_element_preserves_unconstrained_var() {
        assert_eq!(
            Type::Array {
                elem: Box::new(Type::Var)
            }
            .collection_element(),
            Type::Var
        );
        assert_eq!(
            Type::Named { name: "string" }.collection_element(),
            Type::Named { name: "string" }
        );
    }

    #[test]
    fn array_and_string_index_types_are_checked_at_use_sites() {
        let errors = typeck(
            "fn takes_number(x: number) {} fn takes_string(x: string) {}\
             takes_number([1, 2][0]); takes_string(\"ab\"[0]); takes_string([1, 2][0]);",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("number"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn indexed_array_mutation_and_function_elements_are_typed() {
        let errors = typeck(
            "fn apply(f: fn(number) number) number { return f(1); }\
             let numbers = [1]; numbers[0] = \"bad\";\
             const funcs = [fn (x: number) number { return x }];\
             apply(funcs[0]);",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("number"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn for_of_binds_array_element_type_in_sync_and_async_functions() {
        let errors = typeck(
            "fn takes_string(x: string) {}\
             fn sync() { for (const item of [1, 2]) { takes_string(item); } }\
             async fn double(x: number) Promise<number> { return x * 2; }\
             async fn async_loop() { for (const item of [1, 2]) { await double(item); } }",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("number"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn generic_indexing_propagates_across_module_boundary() {
        let arena = Bump::new();
        let lib_source = "export fn first(values: Array<string>) { return values[0]; }";
        let lib_result = parse(lib_source, &arena);
        assert!(lib_result.errors.is_empty(), "{:?}", lib_result.errors);
        let lib_program = lib_result
            .program
            .expect("library parse produced no program");
        let exports = collect_module_exports(&lib_program, &arena);

        let main_source = "import { first } from \"./lib.ds\";\
             fn takes_string(x: string) {}\
             takes_string(first([\"ok\"]));\
             const wrong: number = first([\"bad\"]);";
        let main_result = parse(main_source, &arena);
        assert!(main_result.errors.is_empty(), "{:?}", main_result.errors);
        let main_program = main_result.program.expect("main parse produced no program");
        let mut imports = HashMap::new();
        imports.insert("./lib.ds", &exports);
        let errors = check_program_with_imports(&main_program, main_source, &imports).errors;

        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("number"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn fn_expression_literal_passes() {
        assert!(typeck(
            "const double = fn (x: number) number { return x * 2 }; const y: number = double(5);"
        )
        .is_empty());
    }

    #[test]
    fn fn_expression_return_type_mismatch_fails() {
        let errors = typeck("const double = fn (x: number) number { return \"oops\" };");
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].message.contains("number"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
    }

    // deka#476: a declared return type must be honoured on every path.

    #[test]
    fn declared_return_with_empty_body_fails() {
        let errors = typeck("fn no_return() string { }");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("does not return"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn declared_return_with_fallthrough_body_fails() {
        let errors = typeck("fn wrong_tail() string { const x: number = 1 }");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn return_on_one_branch_only_fails() {
        // The shape most likely to produce a false positive if the analysis is
        // naive: an `if` with no `else` always leaves a falling-through path.
        let errors = typeck("fn maybe(x: number) string { if (x > 0) { return \"y\" } }");
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn return_on_both_branches_passes() {
        assert!(typeck(
            "fn both(x: number) string { if (x > 0) { return \"y\" } else { return \"n\" } }"
        )
        .is_empty());
    }

    #[test]
    fn return_after_if_passes() {
        assert!(
            typeck("fn after(x: number) string { if (x > 0) { return \"y\" } return \"n\" }")
                .is_empty()
        );
    }

    #[test]
    fn nested_if_else_returns_pass() {
        assert!(typeck(
            "fn nested(x: number) string { if (x > 0) { if (x > 1) { return \"a\" } else { return \"b\" } } else { return \"c\" } }"
        )
        .is_empty());
    }

    #[test]
    fn void_return_without_value_passes() {
        assert!(typeck("fn nothing() void { const x: number = 1 }").is_empty());
    }

    #[test]
    fn unannotated_return_without_value_passes() {
        assert!(typeck("fn nothing() { const x: number = 1 }").is_empty());
    }

    #[test]
    fn async_promise_void_without_return_passes() {
        assert!(typeck("async fn nothing() Promise<void> { const x: number = 1 }").is_empty());
    }

    #[test]
    fn async_promise_value_without_return_fails() {
        let errors = typeck("async fn thing() Promise<string> { const x: number = 1 }");
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn missing_return_does_not_stack_on_bad_annotation() {
        // An unresolvable return type is already an error; it must not also
        // produce a missing-return diagnostic.
        let errors = typeck("fn bad() NotAType { }");
        assert!(
            errors
                .iter()
                .all(|e| !e.message.contains("does not return")),
            "{errors:?}"
        );
    }

    #[test]
    fn unknown_return_type_reported_once() {
        // deka#494: the return annotation used to pass through
        // resolve_ast_type twice (signature collection + check_function),
        // producing two identical diagnostics.
        let errors = typeck("fn bad() NotAType { }");
        let unknown: Vec<_> = errors
            .iter()
            .filter(|e| e.message == "unknown type `NotAType`")
            .collect();
        assert_eq!(unknown.len(), 1, "{errors:?}");
    }

    #[test]
    fn unknown_param_type_reported_once() {
        // deka#494 companion check: parameter annotations are resolved during
        // signature collection and reused by check_function, so they must
        // report exactly once as well.
        let errors = typeck("fn bad(x: NotAType) void { }");
        let unknown: Vec<_> = errors
            .iter()
            .filter(|e| e.message == "unknown type `NotAType`")
            .collect();
        assert_eq!(unknown.len(), 1, "{errors:?}");
    }

    #[test]
    fn unknown_receiver_method_return_type_reported_once() {
        // deka#494 companion check: receiver method annotations are resolved
        // in check_receiver_method and again at every call site; with no call
        // site there must be exactly one report.
        let errors = typeck("struct S { x: number } fn (s S) bad() NotAType { }");
        let unknown: Vec<_> = errors
            .iter()
            .filter(|e| e.message == "unknown type `NotAType`")
            .collect();
        assert_eq!(unknown.len(), 1, "{errors:?}");
    }

    #[test]
    fn unknown_receiver_method_return_type_not_rereported_at_call_site() {
        // deka#494 companion check: a call site must not re-report the
        // method's unresolvable return type.
        let errors = typeck(
            "struct S { x: number } fn (s S) bad() NotAType { } const s = S { x: 1 }; s.bad();",
        );
        let unknown: Vec<_> = errors
            .iter()
            .filter(|e| e.message == "unknown type `NotAType`")
            .collect();
        assert_eq!(unknown.len(), 1, "{errors:?}");
    }

    #[test]
    fn for_loop_break_continue_passes() {
        assert!(typeck(
            "for (let i = 0; i < 10; i = i + 1) { if (i == 5) { break } else { continue } }"
        )
        .is_empty());
    }

    #[test]
    fn break_outside_loop_fails() {
        let errors = typeck("break;");
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].message.contains("outside of loop"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn struct_embed_field_access_passes() {
        assert!(typeck(
            "struct Label { name: string } struct Person { Label } const p: Person = Person { Label: Label { name: \"Ada\" } }; const n: string = p.name;"
        ).is_empty());
    }

    #[test]
    fn struct_embed_method_call_passes() {
        assert!(typeck(
            "struct Legs {} fn (l Legs) move() string { return \"walk\" } struct Robot { Legs } const r: Robot = Robot { Legs: Legs {} }; const m: string = r.move();"
        ).is_empty());
    }

    #[test]
    fn struct_embed_promoted_field_literal_passes() {
        assert!(typeck(
            "struct Person { name: string } struct Employee { Person } const e = Employee { name: \"Bob\" }; const n: string = e.name;"
        ).is_empty());
    }

    #[test]
    fn struct_embed_promoted_field_and_method_passes() {
        // deka#496: the wasm fixture constructs an embedder with promoted
        // fields and calls a promoted method on it.
        assert!(typeck(
            "struct Person { name: string } fn (p Person) greet() string { return \"hello\" } struct Employee { Person } const e = Employee { name: \"Bob\" }; const g: string = e.greet();"
        ).is_empty());
    }

    #[test]
    fn struct_embed_promoted_field_wrong_type_fails() {
        let errors = typeck(
            "struct Person { name: string } struct Employee { Person } const e = Employee { name: 1 };",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("field `name` expected type")),
            "{errors:?}"
        );
    }

    #[test]
    fn struct_embed_missing_promoted_field_fails() {
        let errors = typeck(
            "struct Person { name: string } struct Employee { Person } const e = Employee {};",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("missing embedded struct `Person`")),
            "{errors:?}"
        );
    }

    #[test]
    fn struct_embed_direct_and_promoted_conflict_fails() {
        let errors = typeck(
            "struct Person { name: string } struct Employee { Person } const e = Employee { Person: Person { name: \"A\" }, name: \"B\" };",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("both directly and via promoted field")),
            "{errors:?}"
        );
    }

    #[test]
    fn struct_embed_nested_promoted_field_literal_passes() {
        assert!(typeck(
            "struct Legs { count: number } struct Robot { Legs } struct Cyborg { Robot } const c = Cyborg { count: 4 }; const n: number = c.count;"
        ).is_empty());
    }

    #[test]
    fn async_function_passes() {
        assert!(typeck(
            "async fn value() Promise<number> { return 1 } const p: Promise<number> = value();"
        )
        .is_empty());
    }

    #[test]
    fn top_level_await_passes() {
        assert!(typeck(
            "async fn main() Promise<number> { return 1 } const n: number = await main();"
        )
        .is_empty());
    }

    #[test]
    fn await_in_sync_function_fails() {
        let errors = typeck("fn f() { await 1 }");
        assert!(!errors.is_empty());
        assert!(
            errors.iter().any(|e| e.message.contains("await")),
            "{:?}",
            errors
        );
    }

    #[test]
    fn unsafe_block_match_result_passes() {
        let errors =
            typeck("const r = match (unsafe { console.log(1) }) { Ok(v) => v, Err(e) => e };");
        assert!(errors.is_empty(), "{:?}", errors);
    }

    // Union types (rfd#42, deka#530). The positive cases pass trivially if
    // unions degrade to Infer, which is assignable to everything — every
    // negative here is what proves the checker keeps unions real.

    #[test]
    fn union_widening_assign_passes() {
        // `string` widens into `string | number`: assignable to SOME member.
        assert!(typeck("const x: string | number = \"a\";").is_empty());
        assert!(typeck("const x: string | number = 1;").is_empty());
    }

    #[test]
    fn union_narrows_and_binds_in_match() {
        let errors = typeck(
            "fn f(v: string | number) string { return match (v) { string(s) => s, number(n) => string(n) }; }",
        );
        assert!(errors.is_empty(), "{:?}", errors);
    }

    #[test]
    fn union_match_with_catch_all_passes() {
        let errors = typeck(
            "fn f(v: string | number) string { return match (v) { string(s) => s, _ => \"other\" }; }",
        );
        assert!(errors.is_empty(), "{:?}", errors);
    }

    #[test]
    fn union_match_or_pattern_is_exhaustive() {
        let errors = typeck(
            "fn f(v: string | number) string { return match (v) { string(s) | number(s) => string(s) }; }",
        );
        // Alternatives cannot bind (deka#446) — the or-pattern must not
        // silently become exhaustive by binding; it errors instead.
        assert!(!errors.is_empty());
    }

    #[test]
    fn union_match_rebinds_operand_in_arm() {
        // Inside the arm, `v` is shadowed with `string`, so `v.length`
        // typechecks; outside it the union still rejects member access.
        let errors = typeck(
            "fn f(v: string | number) number { return match (v) { string(s) => v.length, number(n) => n }; }",
        );
        assert!(errors.is_empty(), "{:?}", errors);
    }

    #[test]
    fn union_assigned_to_member_fails() {
        // `string | number` is NOT assignable to `string`: EVERY member must
        // be assignable to the expected type.
        let errors = typeck("const x: string | number = 1; const y: string = x;");
        assert_eq!(errors.len(), 1, "{:?}", errors);
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn union_return_diagnostic_teaches_narrowing() {
        let errors = typeck("fn f(v: string | number) string { return v; }");
        assert_eq!(errors.len(), 1, "{:?}", errors);
        assert_eq!(
            errors[0].message,
            "expected return type `string`, found type `number | string`; narrow it with a match before use"
        );
    }

    #[test]
    fn union_argument_diagnostic_teaches_narrowing() {
        let errors = typeck(
            "fn takes(v: string) string { return v; } fn f(v: string | number) string { return takes(v); }",
        );
        assert_eq!(errors.len(), 1, "{:?}", errors);
        assert_eq!(
            errors[0].message,
            "expected argument type `string`, found type `number | string`; narrow it with a match before use"
        );
    }

    #[test]
    fn union_to_union_diagnostic_has_no_narrowing_hint() {
        let errors = typeck(
            "const x: string | number = 1; const y: string | boolean = x;",
        );
        assert_eq!(errors.len(), 1, "{:?}", errors);
        assert!(!errors[0].message.contains("narrow it with a match"));
    }

    #[test]
    fn union_member_access_without_narrowing_fails() {
        let errors = typeck("const v: string | number = \"a\"; const n = v.length;");
        assert_eq!(errors.len(), 1, "{:?}", errors);
        assert!(
            errors[0].message.contains("narrow") && errors[0].message.contains("match"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn union_match_missing_arm_fails() {
        let errors =
            typeck("fn f(v: string | number) string { return match (v) { string(s) => s }; }");
        assert_eq!(errors.len(), 1, "{:?}", errors);
        assert!(
            errors[0].message.contains("non-exhaustive") && errors[0].message.contains("number"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn union_duplicate_members_fail() {
        let errors = typeck("const v: string | string = \"a\";");
        assert_eq!(errors.len(), 1, "{:?}", errors);
        assert!(
            errors[0].message.contains("overlap"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn union_overlapping_struct_interface_members_fail() {
        // A struct that satisfies an interface would match both predicates,
        // so narrowing would be ambiguous.
        let errors = typeck(
            "struct User { name: string } interface Named { name: string } const v: User | Named = User { name: \"a\" };",
        );
        assert_eq!(errors.len(), 1, "{:?}", errors);
        assert!(
            errors[0].message.contains("User") && errors[0].message.contains("Named"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn union_distinct_primitives_do_not_overlap() {
        assert!(typeck("const v: string | number | boolean = true;").is_empty());
    }

    #[test]
    fn union_function_member_fails() {
        let errors = typeck("const f: (fn(number) number) | string = \"a\";");
        assert_eq!(errors.len(), 1, "{:?}", errors);
        assert!(
            errors[0].message.contains("decidable runtime predicate"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn union_option_member_fails() {
        // Option has no runtime predicate today (deka#401 shape requires
        // explicit Some/None construction), so it cannot join a union.
        let errors = typeck("const v: string | Option<number> = \"a\";");
        assert_eq!(errors.len(), 1, "{:?}", errors);
        assert!(
            errors[0].message.contains("decidable runtime predicate"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn union_type_pattern_requires_binding() {
        let errors = typeck(
            "fn f(v: string | number) string { return match (v) { string => \"a\", number(n) => string(n) }; }",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("requires a binding")),
            "{:?}",
            errors
        );
    }

    #[test]
    fn union_type_pattern_unknown_member_fails() {
        let errors = typeck(
            "fn f(v: string | number) string { return match (v) { boolean(b) => string(b), string(s) => s, number(n) => string(n) }; }",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("not a member of union")),
            "{:?}",
            errors
        );
    }

    #[test]
    fn union_newtype_member_fails() {
        // v1 rejects newtypes: the __deka_newtype tag predicate is not wired
        // into match emission yet.
        let errors = typeck("type Meters number\nconst v: Meters | string = \"a\";");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("decidable runtime predicate")),
            "{:?}",
            errors
        );
    }

    #[test]
    fn bridge_async_op_awaits_to_result() {
        // deka#578: async catalog ops (fs.*) have type Promise<Result<T, E>>,
        // so `await bridge fs.read_file(path)` typechecks as rfd#27
        // describes.
        assert!(
            typeck("const r = await bridge fs.read_file(\"a.txt\");").is_empty(),
            "await on an async bridge op must typecheck"
        );
    }

    #[test]
    fn bridge_sync_op_is_plain_result_without_await() {
        // rfd#27: sync ops (crypto.*) stay plain Results and are used without
        // await.
        assert!(
            typeck("const r = bridge crypto.random_bytes(16);").is_empty(),
            "sync bridge op must typecheck as a plain Result"
        );
    }

    #[test]
    fn bridge_sync_op_rejects_await() {
        // Awaiting a sync op is a type error, same as awaiting any other
        // non-Promise value.
        let errors = typeck("const r = await bridge crypto.random_bytes(16);");
        assert_eq!(errors.len(), 1, "{:?}", errors);
        assert!(
            errors[0].message.contains("Promise"),
            "{}",
            errors[0].message
        );
    }
}
