//! Stateful JavaScript emitter for DekaScript compiler v2.
//!
//! Emits real runtime factories for structs (`__deka_struct`) and frozen case
//! objects for enums.  Receiver methods are registered on the struct factory
//! prototype so `p.greet()` works, including methods promoted from embedded
//! structs via the `__deka_struct` helper's embed map.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use deka_syntax::{
    BinOp, ExportDecl, Expr, ForInit, NewtypeRepr, Pattern, Program, Span, Stmt, Type,
};

use crate::util::{bin_op_str, escape_string, is_primitive_receiver, un_op_str, write_indent};

fn is_panic_callee(callee: &Expr<'_>) -> bool {
    match callee {
        Expr::Identifier { name: "panic", .. } => true,
        Expr::FieldAccess {
            object,
            field: "panic",
            ..
        } => matches!(object, Expr::Identifier { name: "deka", .. }),
        _ => false,
    }
}

/// Stable virtual-module key for one build-only binding.
///
/// It is intentionally derived only from source identity and location. Deka
/// owns the materialized value behind this key; Dsc never evaluates it.
pub fn dev_slot_id(file_path: &str, binding: &str, span: Span) -> String {
    let identity = format!(
        "{file_path}\0{binding}\0{}\0{}",
        span.byte_start, span.byte_end
    );
    let hash = identity.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    format!("{hash:016x}")
}

/// Hash identity path for one source file. When `module_root` is set and the
/// file lives under it, the hashed path is root-relative so build slot ids
/// are stable across machine and checkout locations (dsc#61). Otherwise the
/// path is returned unchanged, preserving the historical absolute-path hash
/// for hosts with no known project root (playground, wasm).
pub fn dev_slot_source_path(file_path: &str, module_root: Option<&Path>) -> String {
    let Some(root) = module_root else {
        return file_path.to_string();
    };
    match Path::new(file_path).strip_prefix(root) {
        Ok(relative) if !relative.as_os_str().is_empty() => {
            relative.to_string_lossy().replace('\\', "/")
        }
        _ => file_path.to_string(),
    }
}

fn dev_binding<'a>(stmt: &'a Stmt<'a>) -> Option<(&'a str, &'a Expr<'a>, bool)> {
    match stmt {
        Stmt::Const {
            name,
            value: value @ Expr::Build { .. },
            ..
        } => Some((name, value, false)),
        Stmt::Export {
            decl:
                ExportDecl::Const {
                    name,
                    value: value @ Expr::Build { .. },
                    ..
                },
            ..
        } => Some((name, value, true)),
        _ => None,
    }
}

/// Collect every named struct/newtype/enum factory a descriptor tree needs
/// for build hydration. Shared with module-graph compilation, which derives
/// a module's factory closure from its exported fragments (dsc#52).
pub fn build_factory_names(
    tree: &deka_syntax::typeck::DescriptorTree<'_>,
    names: &mut HashSet<String>,
) {
    use deka_syntax::typeck::DescriptorTree;

    match tree {
        DescriptorTree::Struct { name, fields } => {
            names.insert((*name).to_string());
            for field in fields {
                build_factory_names(&field.ty, names);
            }
        }
        DescriptorTree::Newtype { name, repr } => {
            names.insert((*name).to_string());
            build_factory_names(repr, names);
        }
        DescriptorTree::Enum { name, cases } => {
            names.insert((*name).to_string());
            for (_, payload) in cases {
                if let Some(payload) = payload {
                    build_factory_names(payload, names);
                }
            }
        }
        DescriptorTree::Array { elem } | DescriptorTree::Option { inner: elem } => {
            build_factory_names(elem, names);
        }
        DescriptorTree::Tuple { elements: members } | DescriptorTree::Union { members } => {
            for member in members {
                build_factory_names(member, names);
            }
        }
        DescriptorTree::Leaf { .. }
        | DescriptorTree::Recurse { .. }
        | DescriptorTree::Interface { .. } => {}
    }
}

fn declared_name<'a>(stmt: &'a Stmt<'a>) -> Option<&'a str> {
    match stmt {
        Stmt::Const { name, .. }
        | Stmt::Let { name, .. }
        | Stmt::Function { name, .. }
        | Stmt::Struct { name, .. }
        | Stmt::Enum { name, .. }
        | Stmt::TypeAlias { name, .. }
        | Stmt::Newtype { name, .. }
        | Stmt::Interface { name, .. } => Some(name),
        Stmt::Export {
            decl: ExportDecl::Const { name, .. } | ExportDecl::Function { name, .. },
            ..
        } => Some(name),
        _ => None,
    }
}

/// Every identifier-shaped token in a chunk of raw JavaScript.
///
/// Deliberately not a lexer: this feeds dead-code elimination, where a false
/// positive keeps a binding alive and a false negative breaks the program.
/// Shared with the module-graph shaker (deka_compile::shake), which solved
/// the same problem for the runtime graph (deka#437).
pub fn collect_js_identifier_tokens(source: &str, out: &mut HashSet<String>) {
    let mut current = String::new();
    for ch in source.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            current.push(ch);
        } else if !current.is_empty() {
            push_identifier(std::mem::take(&mut current), out);
        }
    }
    if !current.is_empty() {
        push_identifier(current, out);
    }
}

fn push_identifier(token: String, out: &mut HashSet<String>) {
    // A leading digit means it was a number, not a name.
    if token.starts_with(|ch: char| ch.is_ascii_digit()) {
        return;
    }
    out.insert(token);
}

/// Whether a raw `unsafe` body mentions `target`. The body is spliced
/// verbatim into the output, so ordinary expression walking sees nothing
/// inside it and every name it uses must be found by token scan instead.
fn js_mentions_name(source: &str, target: &str) -> bool {
    let mut tokens = HashSet::new();
    collect_js_identifier_tokens(source, &mut tokens);
    tokens.contains(target)
}

fn collect_dev_stmt_names(stmt: &Stmt<'_>, out: &mut HashSet<String>) {
    visit_stmt_exprs(stmt, &mut |expr| match expr {
        Expr::Identifier { name, .. } => {
            out.insert((*name).to_string());
        }
        Expr::StructLiteral { name, .. }
        | Expr::EnumConstructor {
            enum_name: name, ..
        } => {
            out.insert((*name).to_string());
        }
        Expr::JsxElement { element, .. }
            if element
                .tag
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_uppercase()) =>
        {
            out.insert(element.tag.to_string());
        }
        // An `unsafe` body is raw JS text, invisible to the walker; a helper
        // referenced only inside it must still be retained for the dev entry
        // (dsc#59). Over-approximation is the safe direction here.
        Expr::Unsafe { source, .. } => collect_js_identifier_tokens(source, out),
        _ => {}
    });
}

fn runtime_uses_name(program: &Program<'_>, target: &str) -> bool {
    let mut used = false;
    for stmt in program.statements.iter() {
        if dev_binding(stmt).is_some() {
            continue;
        }
        visit_stmt_exprs(stmt, &mut |expr| {
            if matches!(expr, Expr::Identifier { name, .. } if *name == target)
                || matches!(expr, Expr::JsxElement { element, .. } if element.tag == target)
                || matches!(expr, Expr::Unsafe { source, .. } if js_mentions_name(source, target))
            {
                used = true;
            }
        });
        if used {
            break;
        }
    }
    used
}

/// Whether a binding is referenced from a build-only `build` body.
/// Module-graph compilation uses this to build a separate dev reachability
/// graph without treating those imports as runtime dependencies.
pub fn dev_uses_name(program: &Program<'_>, target: &str) -> bool {
    let mut used = false;
    for stmt in program.statements.iter() {
        let Some((_, Expr::Build { body, .. }, _)) = dev_binding(stmt) else {
            continue;
        };
        if build_body_uses_name(body, target) {
            used = true;
            break;
        }
    }
    used
}

/// Like [`dev_uses_name`], but only counts build bindings that survived
/// graph shaking. Module-graph compilation uses this to decide whether a
/// dependency must emit its compiler-private factory closure (dsc#52): a
/// shaken build binding must not retain the closure. A binding's declared
/// type counts as usage: the build materializes that type even when the
/// body never names it.
pub fn live_dev_uses_name(
    program: &Program<'_>,
    live: Option<&HashSet<String>>,
    target: &str,
) -> bool {
    for stmt in program.statements.iter() {
        let Some((name, Expr::Build { body, .. }, _)) = dev_binding(stmt) else {
            continue;
        };
        if live.is_some_and(|live| !live.contains(name)) {
            continue;
        }
        if build_body_uses_name(body, target) || build_declares_name(stmt, target) {
            return true;
        }
    }
    false
}

/// Whether a build binding's declared type annotation names `target`.
fn build_declares_name(stmt: &Stmt<'_>, target: &str) -> bool {
    let deka_syntax::Stmt::Const {
        ty: Some(ty),
        value: Expr::Build { .. },
        ..
    } = stmt
    else {
        return false;
    };
    let mut found = false;
    fn visit(ty: &deka_syntax::Type<'_>, target: &str, found: &mut bool) {
        match ty {
            deka_syntax::Type::Named { name, .. } => {
                if *name == target {
                    *found = true;
                }
            }
            deka_syntax::Type::Generic { base, args, .. } => {
                if *base == target {
                    *found = true;
                }
                for arg in args.iter() {
                    visit(arg, target, found);
                }
            }
            deka_syntax::Type::Function { params, ret, .. } => {
                for param in params.iter() {
                    visit(param, target, found);
                }
                visit(ret, target, found);
            }
            deka_syntax::Type::Option { inner, .. } => visit(inner, target, found),
            deka_syntax::Type::Union { members, .. } => {
                for member in members.iter() {
                    visit(member, target, found);
                }
            }
            _ => {}
        }
    }
    visit(ty, target, &mut found);
    found
}

fn build_body_uses_name(body: &[Stmt<'_>], target: &str) -> bool {
    let mut used = false;
    for stmt in body {
        visit_stmt_exprs(stmt, &mut |expr| {
            if matches!(expr, Expr::Identifier { name, .. } if *name == target)
                || matches!(expr, Expr::StructLiteral { name, .. } if *name == target)
                || matches!(expr, Expr::EnumConstructor { enum_name: name, .. } if *name == target)
                || matches!(expr, Expr::JsxElement { element, .. } if element.tag == target)
                // Raw `unsafe` text must count too: an import used only
                // inside it still has to be retained for the dev entry
                // (dsc#59).
                || matches!(expr, Expr::Unsafe { source, .. } if js_mentions_name(source, target))
            {
                used = true;
            }
        });
    }
    used
}

/// Emit JavaScript for a parsed and type-checked program.
pub fn emit_js(program: &Program, _source: &str) -> Result<String, String> {
    emit_js_with_options(
        program,
        _source,
        &HashMap::new(),
        None,
        &Default::default(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashSet::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashSet::new(),
        "module.ds",
        None,
    )
}

/// Emit JavaScript with imported module metadata available.
///
/// Imported structs and enums are seeded into the emitter so that struct
/// literals and enum constructors defined in other modules can be emitted
/// correctly in the current file. `unwrap_calls` maps primitive conversion
/// call sites (`parseNumber(x)`, `unboxNumber(x)`, `toNumber(x)`,
/// `string(x)`) to their lowering kind.
pub fn emit_js_with_imports<'a>(
    program: &'a Program<'a>,
    _source: &str,
    imports: &HashMap<&str, &deka_syntax::ModuleExports<'a>>,
    exception_forms: &deka_syntax::typeck::ExceptionLowering<'a>,
    unwrap_calls: &HashMap<*const Expr<'a>, deka_syntax::typeck::UnwrapKind>,
    operator_rewrites: &HashMap<*const Expr<'a>, deka_syntax::typeck::OperatorRewrite<'a>>,
    method_calls: &HashMap<*const Expr<'a>, deka_syntax::MethodTarget<'a>>,
) -> Result<String, String> {
    emit_js_with_options(
        program,
        _source,
        imports,
        None,
        exception_forms,
        unwrap_calls,
        operator_rewrites,
        method_calls,
        &HashSet::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashSet::new(),
        "module.ds",
        None,
    )
}

/// Emit JavaScript with imported module metadata and a base URL for bare
/// module specifiers.
///
/// When `module_base` is provided, bare import specifiers (those not starting
/// with `.`, `/`, or a URL scheme) are rewritten to
/// `<module_base>/<spec>.mjs`. This lets a host serve stdlib modules as real
/// ESM files instead of string-rewriting compiled output.
pub fn emit_js_with_options<'a>(
    program: &'a Program<'a>,
    source: &str,
    imports: &HashMap<&str, &deka_syntax::ModuleExports<'a>>,
    module_base: Option<String>,
    exception_forms: &deka_syntax::typeck::ExceptionLowering<'a>,
    unwrap_calls: &HashMap<*const Expr<'a>, deka_syntax::typeck::UnwrapKind>,
    operator_rewrites: &HashMap<*const Expr<'a>, deka_syntax::typeck::OperatorRewrite<'a>>,
    // Primitive extension call sites (`s.slugify()`) to rewrite to
    // free-function calls (`slugify$string(s)`), lowered by the typechecker
    // (deka#527).
    method_calls: &HashMap<*const Expr<'a>, deka_syntax::MethodTarget<'a>>,
    // Builtin `.getType()` call sites to rewrite to `__deka_type_of(x)`,
    // lowered by the typechecker (rfd#41, deka#529).
    type_of_calls: &HashSet<*const Expr<'a>>,
    // `.signature()` call sites lowered to declared-type descriptor literals.
    signature_calls: &HashMap<*const Expr<'a>, deka_syntax::typeck::DescriptorTree<'a>>,
    json_calls: &HashMap<*const Expr<'a>, deka_syntax::typeck::JsonCall<'a>>,
    // Builtin `first`/`last`/`pop`/`shift` array method call sites, lowered
    // by the typechecker (deka#561, deka#566): the emitter rewrites them to
    // an Option-producing expression since JS has no `first`/`last` and its
    // `pop`/`shift` return raw values, not the declared Option<T>.
    array_builtin_calls: &HashMap<*const Expr<'a>, deka_syntax::typeck::ArrayAccess>,
    // Builtin Math-backed `number` method call sites, lowered by the
    // typechecker (deka#378 step 2, rfd#40 phase 2): the emitter rewrites
    // them to `Math.*` expressions since JS numbers have no such methods.
    number_math_calls: &HashMap<*const Expr<'a>, deka_syntax::typeck::NumberMath>,
    // Builtin `.type()` call sites inside `super` functions, lowered by the
    // typechecker to the hidden descriptor parameter or a static tree const
    // (deka#529, rfd#41). Kept for super declarations (PR B): this is the
    // feed point PR B re-populates.
    static_type_calls: &HashMap<*const Expr<'a>, deka_syntax::typeck::StaticTypeCall<'a>>,
    // Descriptor trees for every `super` declaration visible to the
    // typechecker, keyed by declaration name. The emitter interns one
    // frozen const per declaration referenced (directly or through a
    // recursive group) by `static_type_calls`.
    super_decl_trees: &std::collections::HashMap<&'a str, deka_syntax::typeck::DescriptorTree<'a>>,
    enum_case_patterns: &HashMap<*const deka_syntax::Pattern<'a>, &'a str>,
    // Union member type-patterns (`string(s)`) and the runtime predicate
    // each one compiles to (rfd#42, deka#530).
    union_type_patterns: &HashMap<
        *const deka_syntax::Pattern<'a>,
        deka_syntax::typeck::UnionMemberTest<'a>,
    >,
    // Factories captured by this module's compiler-private
    // `__deka_factories` closure for consumer build hydration (dsc#52).
    build_closure_names: &HashSet<String>,
    file_path: &str,
    live_names: Option<&HashSet<String>>,
) -> Result<String, String> {
    Ok(emit_js_module_with_options(
        program,
        source,
        imports,
        module_base,
        exception_forms,
        unwrap_calls,
        operator_rewrites,
        method_calls,
        type_of_calls,
        signature_calls,
        json_calls,
        array_builtin_calls,
        number_math_calls,
        static_type_calls,
        super_decl_trees,
        enum_case_patterns,
        union_type_patterns,
        &HashMap::new(),
        build_closure_names,
        file_path,
        None,
        live_names,
        false,
        None,
    )?
    .js)
}

/// The emitted module plus its demand for shared runtime helpers.
///
/// `js` always contains the module body. When `detached` is false (the
/// default for single-module compilation) it also inlines the shared prelude
/// synthesized from [`PreludeDemand`], exactly as before. When `detached` is
/// true (module-graph emission) the shared prelude is left out of `js`; the
/// caller unions every module's `demand` and synthesizes the program prelude
/// once (deka#595).
#[derive(Debug)]
pub struct ModuleEmit {
    pub js: String,
    pub demand: crate::prelude::PreludeDemand,
}

/// Like [`emit_js_with_options`], with control over prelude placement.
#[allow(clippy::too_many_arguments)]
pub fn emit_js_module_with_options<'a>(
    program: &'a Program<'a>,
    source: &str,
    imports: &HashMap<&str, &deka_syntax::ModuleExports<'a>>,
    module_base: Option<String>,
    exception_forms: &deka_syntax::typeck::ExceptionLowering<'a>,
    unwrap_calls: &HashMap<*const Expr<'a>, deka_syntax::typeck::UnwrapKind>,
    operator_rewrites: &HashMap<*const Expr<'a>, deka_syntax::typeck::OperatorRewrite<'a>>,
    method_calls: &HashMap<*const Expr<'a>, deka_syntax::MethodTarget<'a>>,
    type_of_calls: &HashSet<*const Expr<'a>>,
    signature_calls: &HashMap<*const Expr<'a>, deka_syntax::typeck::DescriptorTree<'a>>,
    json_calls: &HashMap<*const Expr<'a>, deka_syntax::typeck::JsonCall<'a>>,
    array_builtin_calls: &HashMap<*const Expr<'a>, deka_syntax::typeck::ArrayAccess>,
    number_math_calls: &HashMap<*const Expr<'a>, deka_syntax::typeck::NumberMath>,
    static_type_calls: &HashMap<*const Expr<'a>, deka_syntax::typeck::StaticTypeCall<'a>>,
    super_decl_trees: &std::collections::HashMap<&'a str, deka_syntax::typeck::DescriptorTree<'a>>,
    enum_case_patterns: &HashMap<*const deka_syntax::Pattern<'a>, &'a str>,
    union_type_patterns: &HashMap<
        *const deka_syntax::Pattern<'a>,
        deka_syntax::typeck::UnionMemberTest<'a>,
    >,
    build_blocks: &HashMap<*const Expr<'a>, deka_syntax::typeck::DevBlock<'a>>,
    // Factories this module's compiler-private `__deka_factories` closure
    // must capture for consumer build hydration (dsc#52). Empty disables the
    // closure export entirely.
    build_closure_names: &HashSet<String>,
    file_path: &str,
    // Project root that build slot ids are hashed relative to (dsc#61).
    // None preserves the historical absolute-path identity.
    module_root: Option<PathBuf>,
    live_names: Option<&HashSet<String>>,
    detached: bool,
    jsx_runtime: Option<String>,
) -> Result<ModuleEmit, String> {
    let mut emitter = Emitter::new(program);
    emitter.configure_jsx_names(source);
    emitter.module_base = module_base;
    emitter.source_path = file_path.to_string();
    emitter.file_stem = file_stem_from_path(file_path);
    emitter.module_root = module_root;
    emitter.seed_imports(imports);
    emitter.exception_forms = exception_forms.clone();
    emitter.unwrap_calls = unwrap_calls.clone();
    emitter.operator_rewrites = operator_rewrites.clone();
    emitter.method_calls = method_calls.clone();
    emitter.type_of_calls = type_of_calls.clone();
    emitter.signature_calls = signature_calls.clone();
    emitter.json_calls = json_calls.clone();
    emitter.array_builtin_calls = array_builtin_calls.clone();
    emitter.number_math_calls = number_math_calls.clone();
    emitter.static_type_calls = static_type_calls.clone();
    emitter.super_decl_trees = super_decl_trees.clone();
    emitter.enum_case_patterns = enum_case_patterns.clone();
    emitter.union_type_patterns = union_type_patterns.clone();
    emitter.build_blocks = build_blocks.clone();
    for block in build_blocks.values() {
        build_factory_names(&block.descriptor, &mut emitter.build_factory_names);
    }
    emitter.build_closure_names = build_closure_names.clone();
    emitter.live_names = live_names.cloned();
    emitter.detached = detached;
    if let Some(runtime) = jsx_runtime { emitter.jsx_runtime = runtime; }
    let js = emitter.emit()?;
    Ok(ModuleEmit {
        js,
        demand: emitter.demand,
    })
}

/// Emit one dev-only entry. Its caller is responsible for publishing the
/// returned module in the compiler plan; this function never executes it.
#[allow(clippy::too_many_arguments)]
pub fn emit_dev_entry<'a>(
    program: &'a Program<'a>,
    source: &str,
    imports: &HashMap<&str, &deka_syntax::ModuleExports<'a>>,
    module_base: Option<String>,
    typeck: &deka_syntax::typeck::TypeckResult<'a>,
    body: &'a [Stmt<'a>],
    slot: &str,
    file_path: &str,
    module_root: Option<PathBuf>,
    jsx_runtime: Option<String>,
) -> Result<String, String> {
    let mut emitter = Emitter::new(program);
    if let Some(runtime) = jsx_runtime { emitter.jsx_runtime = runtime; }
    emitter.configure_jsx_names(source);
    emitter.module_base = module_base;
    emitter.source_path = file_path.to_string();
    emitter.file_stem = file_stem_from_path(file_path);
    emitter.module_root = module_root;
    emitter.seed_imports(imports);
    emitter.exception_forms = typeck.exception_forms.clone();
    emitter.unwrap_calls = typeck.unwrap_calls.clone();
    emitter.operator_rewrites = typeck.operator_rewrites.clone();
    emitter.method_calls = typeck.method_calls.clone();
    emitter.type_of_calls = typeck.type_of_calls.clone();
    emitter.signature_calls = typeck.signature_calls.clone();
    emitter.json_calls = typeck.json_calls.clone();
    emitter.array_builtin_calls = typeck.array_builtin_calls.clone();
    emitter.number_math_calls = typeck.number_math_calls.clone();
    emitter.static_type_calls = typeck.static_type_calls.clone();
    emitter.super_decl_trees = typeck.super_trees.clone();
    emitter.enum_case_patterns = typeck.enum_case_patterns.clone();
    emitter.union_type_patterns = typeck.union_type_patterns.clone();
    emitter.emit_dev_entry(slot, body)
}

fn file_stem_from_path(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("module")
        .to_string()
}

/// The display name of a descriptor tree node: the name scalars, structs,
/// newtypes and enums carry, `Array<…>`/`Option<…>` for the collection
/// nodes, and `… | …` for unions. Kept for super declarations (PR B).
fn descriptor_tree_name(tree: &deka_syntax::typeck::DescriptorTree) -> String {
    use deka_syntax::typeck::DescriptorTree as T;
    match tree {
        T::Leaf { name, .. } => name.clone(),
        T::Recurse { name } => name.to_string(),
        T::Struct { name, .. }
        | T::Interface { name }
        | T::Newtype { name, .. }
        | T::Enum { name, .. } => name.to_string(),
        T::Tuple { elements } => format!(
            "[{}]",
            elements
                .iter()
                .map(descriptor_tree_name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        T::Array { elem } => format!("Array<{}>", descriptor_tree_name(elem)),
        T::Option { inner } => format!("Option<{}>", descriptor_tree_name(inner)),
        T::Union { members } => members
            .iter()
            .map(descriptor_tree_name)
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

/// Serialize a static descriptor tree to its frozen JavaScript literal
/// (deka#529, rfd#41). Kept for super declarations (PR B).
///
/// Every node carries `{kind, name, toString}` so static trees stay
/// interchangeable with the runtime descriptors `__deka_type_of` returns
/// (the shape rule #550 applied to the `{kind, name, toString}` triple);
/// composites additionally expose `fields`/`cases`/`elem`/`inner`/
/// `members`/`repr` so the schema endgame is not precluded.
fn emit_descriptor_tree(tree: &deka_syntax::typeck::DescriptorTree) -> Result<String, String> {
    use deka_syntax::typeck::DescriptorTree as T;
    let mut out = String::new();
    let header = |out: &mut String, kind: &str, name: &str| {
        out.push_str("Object.freeze({ kind: \"");
        out.push_str(kind);
        out.push_str("\", name: \"");
        out.push_str(&escape_string(name));
        out.push_str("\", toString() { return this.name; }");
    };
    match tree {
        T::Leaf { kind, name } => {
            header(&mut out, kind, name);
            out.push_str(" })");
        }
        T::Struct { name, fields } => {
            header(&mut out, "struct", name);
            out.push_str(", fields: Object.freeze([");
            for (i, field) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str("{ name: \"");
                out.push_str(&escape_string(field.name));
                out.push_str("\", optional: ");
                out.push_str(if field.optional { "true" } else { "false" });
                out.push_str(", type: ");
                out.push_str(&emit_descriptor_tree(&field.ty)?);
                out.push_str(" }");
            }
            out.push_str("]) })");
        }
        T::Interface { name } => {
            header(&mut out, "interface", name);
            out.push_str(" })");
        }
        T::Newtype { name, repr } => {
            header(&mut out, "newtype", name);
            out.push_str(", repr: ");
            out.push_str(&emit_descriptor_tree(repr)?);
            out.push_str(" })");
        }
        T::Enum { name, cases } => {
            header(&mut out, "enum", name);
            out.push_str(", cases: Object.freeze([");
            for (i, (case_name, payload)) in cases.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str("{ name: \"");
                out.push_str(&escape_string(case_name));
                out.push_str("\", payload: ");
                match payload {
                    Some(payload_tree) => {
                        out.push_str(&emit_descriptor_tree(payload_tree)?);
                    }
                    None => out.push_str("null"),
                }
                out.push_str(" }");
            }
            out.push_str("]) })");
        }
        T::Tuple { elements } => {
            header(&mut out, "tuple", &descriptor_tree_name(tree));
            out.push_str(", get elements() { return Object.freeze([");
            for (i, element) in elements.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&emit_descriptor_tree(element)?);
            }
            out.push_str("]); } })");
        }
        T::Array { elem } => {
            header(&mut out, "array", "Array");
            out.push_str(", elem: ");
            out.push_str(&emit_descriptor_tree(elem)?);
            out.push_str(" })");
        }
        T::Option { inner } => {
            header(&mut out, "option", "Option");
            out.push_str(", inner: ");
            out.push_str(&emit_descriptor_tree(inner)?);
            out.push_str(" })");
        }
        T::Union { members } => {
            let name = descriptor_tree_name(tree);
            header(&mut out, "union", &name);
            out.push_str(", members: Object.freeze([");
            for (i, member) in members.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&emit_descriptor_tree(member)?);
            }
            out.push_str("]) })");
        }
        // `Recurse` nodes are only produced by the super-declaration path
        // (which emits through `emit_super_tree`, where they become const
        // references). Reaching one here means a recursive tree leaked into
        // an eager context (`.signature()`), which the typechecker
        // guarantees cannot happen; fail loudly rather than emit a
        // dangling reference.
        T::Recurse { name } => {
            return Err(format!(
                "internal: recursive descriptor reference `{name}` in eager emission"
            ));
        }
    }
    Ok(out)
}

/// The interned const name for a `super` declaration's descriptor.
fn super_const_name(name: &str) -> String {
    format!("__deka_super_desc${name}")
}

/// The declaration name a static-type-call tree describes: the name of its
/// top struct/enum node. Non-declaration trees (impossible from the
/// typechecker) return None.
fn super_decl_top_name<'a>(tree: &deka_syntax::typeck::DescriptorTree<'a>) -> Option<&'a str> {
    use deka_syntax::typeck::DescriptorTree as T;
    match tree {
        T::Struct { name, .. } | T::Enum { name, .. } => Some(name),
        _ => None,
    }
}

/// Does this tree contain a recursive-reference node anywhere below (or at)
/// its root?
fn tree_contains_recurse(tree: &deka_syntax::typeck::DescriptorTree) -> bool {
    use deka_syntax::typeck::DescriptorTree as T;
    match tree {
        T::Recurse { .. } => true,
        T::Struct { fields, .. } => fields.iter().any(|f| tree_contains_recurse(&f.ty)),
        T::Enum { cases, .. } => cases
            .iter()
            .any(|(_, payload)| payload.as_ref().is_some_and(tree_contains_recurse)),
        T::Newtype { repr, .. } => tree_contains_recurse(repr),
        T::Array { elem } | T::Option { inner: elem } => tree_contains_recurse(elem),
        T::Tuple { elements: members } | T::Union { members } => {
            members.iter().any(tree_contains_recurse)
        }
        T::Leaf { .. } | T::Interface { .. } => false,
    }
}

/// Collect the names of every `Recurse` node in a tree.
fn collect_recurse_refs<'a>(
    tree: &deka_syntax::typeck::DescriptorTree<'a>,
    out: &mut Vec<&'a str>,
) {
    use deka_syntax::typeck::DescriptorTree as T;
    match tree {
        T::Recurse { name } => out.push(name),
        T::Struct { fields, .. } => {
            for field in fields.iter() {
                collect_recurse_refs(&field.ty, out);
            }
        }
        T::Enum { cases, .. } => {
            for (_, payload) in cases.iter() {
                if let Some(payload) = payload {
                    collect_recurse_refs(payload, out);
                }
            }
        }
        T::Newtype { repr, .. } => collect_recurse_refs(repr, out),
        T::Array { elem } | T::Option { inner: elem } => collect_recurse_refs(elem, out),
        T::Tuple { elements: members } | T::Union { members } => {
            for member in members.iter() {
                collect_recurse_refs(member, out);
            }
        }
        T::Leaf { .. } | T::Interface { .. } => {}
    }
}

/// Expand a referenced `super` declaration into its full const group: the
/// declaration itself plus, transitively, every declaration its tree
/// references recursively. `emitted` guards the walk; `group` accumulates
/// first-visit order (emission order is unconstrained — recursive consts use
/// lazy getters — but deterministic output is nicer to read and diff).
fn collect_super_group<'a>(
    name: &'a str,
    decl_trees: &std::collections::HashMap<&'a str, deka_syntax::typeck::DescriptorTree<'a>>,
    emitted: &mut HashSet<&'a str>,
    group: &mut Vec<&'a str>,
) {
    if !emitted.insert(name) {
        return;
    }
    group.push(name);
    let Some(tree) = decl_trees.get(name) else {
        // The typechecker records a call site only for declarations whose
        // tree it built; a miss means the maps came from different passes.
        // Emitting a dangling const reference would be a silent miscompile,
        // so refuse (the caller turns this into a compile error).
        return;
    };
    let mut refs = Vec::new();
    collect_recurse_refs(tree, &mut refs);
    for referenced in refs {
        collect_super_group(referenced, decl_trees, emitted, group);
    }
}

/// Serialize a `super` declaration's descriptor tree as its interned const
/// body. Trees without recursion are plain eager frozen literals (the
/// `emit_descriptor_tree` shape `.signature()` also uses). Trees with
/// recursion emit their composite properties as lazy getters so a const may
/// reference itself (or a cycle partner) without an initialization-order
/// constraint; the `Recurse` nodes become references to the sibling consts.
fn emit_super_tree(tree: &deka_syntax::typeck::DescriptorTree) -> Result<String, String> {
    use deka_syntax::typeck::DescriptorTree as T;
    if !tree_contains_recurse(tree) {
        return emit_descriptor_tree(tree);
    }
    let mut out = String::new();
    let header = |out: &mut String, kind: &str, name: &str| {
        out.push_str("Object.freeze({ kind: \"");
        out.push_str(kind);
        out.push_str("\", name: \"");
        out.push_str(&escape_string(name));
        out.push_str("\", toString() { return this.name; }");
    };
    match tree {
        T::Recurse { name } => out.push_str(&super_const_name(name)),
        T::Struct { name, fields } => {
            header(&mut out, "struct", name);
            out.push_str(", get fields() { return Object.freeze([");
            for (i, field) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str("{ name: \"");
                out.push_str(&escape_string(field.name));
                out.push_str("\", optional: ");
                out.push_str(if field.optional { "true" } else { "false" });
                out.push_str(", type: ");
                out.push_str(&emit_super_tree(&field.ty)?);
                out.push_str(" }");
            }
            out.push_str("]); } })");
        }
        T::Enum { name, cases } => {
            header(&mut out, "enum", name);
            out.push_str(", get cases() { return Object.freeze([");
            for (i, (case_name, payload)) in cases.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str("{ name: \"");
                out.push_str(&escape_string(case_name));
                out.push_str("\", payload: ");
                match payload {
                    Some(payload_tree) => {
                        out.push_str(&emit_super_tree(payload_tree)?);
                    }
                    None => out.push_str("null"),
                }
                out.push_str(" }");
            }
            out.push_str("]); } })");
        }
        T::Tuple { elements } => {
            header(&mut out, "tuple", &descriptor_tree_name(tree));
            out.push_str(", get elements() { return Object.freeze([");
            for (i, element) in elements.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&emit_super_tree(element)?);
            }
            out.push_str("]); } })");
        }
        T::Array { elem } => {
            header(&mut out, "array", "Array");
            out.push_str(", get elem() { return ");
            out.push_str(&emit_super_tree(elem)?);
            out.push_str("; } })");
        }
        T::Option { inner } => {
            header(&mut out, "option", "Option");
            out.push_str(", get inner() { return ");
            out.push_str(&emit_super_tree(inner)?);
            out.push_str("; } })");
        }
        T::Union { members } => {
            let name = descriptor_tree_name(tree);
            header(&mut out, "union", &name);
            out.push_str(", get members() { return Object.freeze([");
            for (i, member) in members.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&emit_super_tree(member)?);
            }
            out.push_str("]); } })");
        }
        // Leaves, interfaces and newtypes have no recursion below them
        // (tree_contains_recurse gated above): the eager literal is right.
        _ => return emit_descriptor_tree(tree),
    }
    Ok(out)
}

fn json_shape_name(tree: &deka_syntax::typeck::DescriptorTree) -> String {
    descriptor_tree_name(tree)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn json_encode(tree: &deka_syntax::typeck::DescriptorTree, value: &str) -> String {
    use deka_syntax::typeck::DescriptorTree as T;
    match tree {
        // Unreachable by construction: JSON walks the type with
        // `allow_recurse: false`, so the typechecker errors on a cycle
        // before emission and `json_shape_error` rejects any `Recurse`
        // that somehow survives. Panic rather than fabricate output --
        // a wrong serializer is a silent wrong answer, which is worse
        // than a loud compiler bug.
        T::Recurse { name } => unreachable!(
            "JSON emission reached a recursive descriptor node for `{name}`; \
             the typechecker should have rejected it"
        ),
        T::Leaf { .. } => value.to_string(),
        T::Newtype { .. } => format!("{value}[__p]"),
        T::Tuple { elements } => format!("[{}]", elements.iter().enumerate().map(|(i, t)| json_encode(t, &format!("{value}[{i}]"))).collect::<Vec<_>>().join(", ")),
        T::Array { elem } => format!("{value}.map((v) => {})", json_encode(elem, "v")),
        T::Option { inner } => format!(
            "(() => {{ const __option = {value}; return __option !== undefined ? {{ Option: {{ case: \"Some\", values: [{}] }} }} : {{ Option: {{ case: \"None\" }} }}; }})()",
            json_encode(inner, "__option")
        ),
        T::Struct { name, fields } => {
            let entries = fields
                .iter()
                .map(|field| {
                    format!(
                        "{}: {}",
                        json_string(field.name),
                        json_encode(&field.ty, &format!("{value}[{}]", json_string(field.name)))
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{ {}: {{ {} }} }}", json_string(name), entries)
        }
        T::Enum { name, cases } => {
            let arms = cases
                .iter()
                .map(|(case, payload)| {
                    let body = match payload {
                        Some(payload) => format!(
                            "{{ {}: {{ case: {}, values: [{}] }} }}",
                            json_string(name),
                            json_string(case),
                            json_encode(payload, &format!("{value}.{}", if *name == "Result" && *case == "Err" { "error" } else { "value" }))
                        ),
                        None => format!(
                            "{{ {}: {{ case: {} }} }}",
                            json_string(name),
                            json_string(case)
                        ),
                    };
                    format!("case {}: return {};", json_string(case), body)
                })
                .collect::<Vec<_>>()
                .join(" ");
            let discriminant = if *name == "Result" { format!("{value}.ok ? \"Ok\" : \"Err\"") } else { format!("{value}.__case") };
            format!("(() => {{ switch ({discriminant}) {{ {arms} default: return undefined; }} }})()")
        }
        T::Union { members } => {
            let arms = members
                .iter()
                .map(|member| {
                    format!(
                        "if ({}) return {};",
                        json_predicate(member, value),
                        json_encode(member, value)
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            format!("(() => {{ {} return undefined; }})()", arms)
        }
        T::Interface { .. } => "undefined".to_string(),
    }
}

fn json_predicate(tree: &deka_syntax::typeck::DescriptorTree, value: &str) -> String {
    use deka_syntax::typeck::DescriptorTree as T;
    match tree {
        // Unreachable by construction: JSON walks the type with
        // `allow_recurse: false`, so the typechecker errors on a cycle
        // before emission and `json_shape_error` rejects any `Recurse`
        // that somehow survives. Panic rather than fabricate output --
        // a wrong serializer is a silent wrong answer, which is worse
        // than a loud compiler bug.
        T::Recurse { name } => unreachable!(
            "JSON emission reached a recursive descriptor node for `{name}`; \
             the typechecker should have rejected it"
        ),
        T::Leaf { kind, .. } => match *kind {
            "number" | "string" | "boolean" => format!("typeof {value} === {}", json_string(kind)),
            "bytes" => format!("{value} instanceof Uint8Array"),
            _ => "true".to_string(),
        },
        T::Struct { name, .. } => format!("{value}?.__deka_struct === {}", json_string(name)),
        T::Enum { name: "Result", .. } => format!("typeof {value}?.ok === \"boolean\""),
        T::Enum { name, .. } => format!("{value}?.__enum === {}", json_string(name)),
        T::Newtype { name, .. } => format!("{value}?.__deka_newtype === {}", json_string(name)),
        T::Option { inner } => format!(
            "({value} === undefined || ({}))",
            json_predicate(inner, value)
        ),
        T::Tuple { elements } => format!(
            "(Array.isArray({value}) && {value}.length === {})",
            elements.len()
        ),
        T::Array { .. } => format!("Array.isArray({value})"),
        T::Union { .. } => "true".to_string(),
        T::Interface { .. } => "true".to_string(),
    }
}

fn json_decode(tree: &deka_syntax::typeck::DescriptorTree, value: &str) -> String {
    use deka_syntax::typeck::DescriptorTree as T;
    let invalid = "__invalid";
    match tree {
        // Unreachable by construction -- see json_encode.
        T::Recurse { name } => unreachable!(
            "JSON emission reached a recursive descriptor node for `{name}`; \
             the typechecker should have rejected it"
        ),
        T::Leaf { kind, .. } => match *kind {
            "number" | "string" | "boolean" => format!(
                "typeof {value} === {} ? {value} : {invalid}",
                json_string(kind)
            ),
            "bytes" => format!("{value} instanceof Uint8Array ? {value} : {invalid}"),
            _ => value.to_string(),
        },
        T::Newtype { name, repr } => {
            let inner = json_decode(repr, value);
            format!(
                "(() => {{ const x = {inner}; return x === __invalid ? __invalid : {name}(x); }})()"
            )
        }
        T::Tuple { elements } => {
            let mut checks = String::new();
            for (i, t) in elements.iter().enumerate() {
                checks.push_str(&format!("const v{i} = {}; if (v{i} === __invalid) return __invalid;", json_decode(t, &format!("{value}[{i}]"))));
            }
            let values = (0..elements.len()).map(|i| format!("v{i}")).collect::<Vec<_>>().join(", ");
            format!("(Array.isArray({value}) && {value}.length === {}) ? (() => {{ {checks} return [{values}]; }})() : __invalid", elements.len())
        }
        T::Array { elem } => format!(
            "Array.isArray({value}) ? (() => {{ const a = []; for (const x of {value}) {{ const y = {}; if (y === __invalid) return __invalid; a.push(y); }} return a; }})() : __invalid",
            json_decode(elem, "x")
        ),
        T::Option { inner } => format!(
            "{value} && typeof {value} === \"object\" && {value}.Option ? ({value}.Option.case === \"None\" ? undefined : {value}.Option.case === \"Some\" ? ({}) : __invalid) : __invalid",
            json_decode(inner, &format!("{value}.Option.values?.[0]"))
        ),
        T::Struct { name, fields } => {
            let mut checks = vec![format!(
                "{value} && typeof {value} === \"object\" && {value}[{}]",
                json_string(name)
            )];
            let mut assignments = Vec::new();
            for field in fields {
                let source = format!(
                    "{value}[{}][{}]",
                    json_string(name),
                    json_string(field.name)
                );
                let decoded = json_decode(&field.ty, &source);
                let local = format!("__{}", field.name);
                checks.push(format!("(() => {{ const {local} = {decoded}; if ({local} === __invalid) return false; return true; }})()"));
                assignments.push(format!("{}: {}", json_string(field.name), decoded));
            }
            format!(
                "({}) ? {}({{ {} }}) : __invalid",
                checks.join(" && "),
                name,
                assignments.join(", ")
            )
        }
        T::Enum { name, cases } => {
            let mut arms = Vec::new();
            for (case, payload) in cases {
                let body = match payload {
                    Some(payload) => {
                        let source = format!("{value}[{}].values?.[0]", json_string(name));
                        let decoded = json_decode(payload, &source);
                        format!(
                            "(() => {{ const x = {}; return x === __invalid ? __invalid : {}.{}(x); }})()",
                            decoded, name, case
                        )
                    }
                    None => format!("{}.{}", name, case),
                };
                arms.push(format!(
                    "{} === {} ? {}",
                    format!("{value}[{}].case", json_string(name)),
                    json_string(case),
                    body
                ));
            }
            format!(
                "{value} && typeof {value} === \"object\" && {value}[{}] ? ({} : __invalid) : __invalid",
                json_string(name),
                arms.join(" : ")
            )
        }
        T::Union { members } => {
            let mut expression = "__invalid".to_string();
            for member in members.iter().rev() {
                let decoded = json_decode(member, value);
                expression = format!(
                    "(() => {{ const x = {}; return x === __invalid ? {} : x; }})()",
                    decoded, expression
                );
            }
            expression
        }
        T::Interface { .. } => invalid.to_string(),
    }
}

fn emit_json_functions(
    calls: &HashMap<*const Expr<'_>, deka_syntax::typeck::JsonCall<'_>>,
) -> String {
    let mut functions = HashMap::<
        String,
        (
            deka_syntax::typeck::JsonOperation,
            deka_syntax::typeck::DescriptorTree<'_>,
        ),
    >::new();
    for call in calls.values() {
        let name = format!(
            "{}${}",
            match call.operation {
                deka_syntax::typeck::JsonOperation::ToJson => "toJSON",
                deka_syntax::typeck::JsonOperation::ParseJson => "parseJSON",
            },
            json_shape_name(&call.shape)
        );
        functions
            .entry(name)
            .or_insert((call.operation, call.shape.clone()));
    }
    let mut out = String::new();
    let mut names = functions.keys().cloned().collect::<Vec<_>>();
    names.sort();
    for name in names {
        let (operation, shape) = functions.remove(&name).unwrap();
        match operation {
            deka_syntax::typeck::JsonOperation::ToJson => {
                out.push_str("function ");
                out.push_str(&name);
                out.push_str("(v) { return JSON.stringify(");
                out.push_str(&json_encode(&shape, "v"));
                out.push_str("); }\n");
            }
            deka_syntax::typeck::JsonOperation::ParseJson => {
                out.push_str("function ");
                out.push_str(&name);
                out.push_str("(s) { const __invalid = Symbol(); try { const v = JSON.parse(s); const x = ");
                out.push_str(&json_decode(&shape, "v"));
                out.push_str("; return x === __invalid ? Err(\"invalid JSON value\") : Ok(x); } catch (_) { return Err(\"invalid JSON\"); } }\n");
            }
        }
    }
    out
}

#[derive(Default, Clone)]
struct StructMeta {
    fields: HashSet<String>,
    embeds: Vec<String>,
    /// Optional fields: `None` means auto-fill with `Option.None`;
    /// `Some(expr)` means auto-fill with the pre-emitted default value.
    optional: HashMap<String, Option<String>>,
    empty_embeds: HashSet<String>,
    /// The brand the runtime factory carries: the declared name for local
    /// structs, the exported name when an import renames the binding
    /// (`import { User as Person }`). Brand comparisons (union type-patterns)
    /// must test this, never the local spelling (dsc#51).
    brand: String,
}

#[derive(Default, Clone)]
struct EnumMeta {
    cases: Vec<String>,
    payload_cases: HashSet<String>,
}

#[derive(Clone)]
struct ReceiverMethod<'a> {
    name: String,
    receiver_name: String,
    receiver_mutable: bool,
    params: Vec<String>,
    body: &'a [deka_syntax::Stmt<'a>],
    is_async: bool,
}

struct Emitter<'a> {
    program: &'a Program<'a>,
    out: String,
    uses_struct: bool,
    /// A promoted literal constructs a private embed through the exported
    /// root factory, so the shared struct helper must expose its internal
    /// factory lookup (dsc#86).
    uses_promotion_factory: bool,
    uses_newtype: bool,
    uses_prelude_enums: bool,
    struct_order: Vec<String>,
    structs: HashMap<String, StructMeta>,
    /// Private transitive embed metadata for imported structs. Kept outside
    /// `structs` so it can guide promoted-literal emission without making an
    /// embedded declaration a normal, user-addressable factory (dsc#86).
    promotion_structs: HashMap<String, HashMap<String, StructMeta>>,
    enums: HashMap<String, EnumMeta>,
    opaques: HashSet<String>,
    interfaces: HashSet<String>,
    erased_type_exports: HashMap<String, HashSet<String>>,
    newtypes: HashMap<String, NewtypeRepr>,
    receiver_methods: HashMap<String, Vec<ReceiverMethod<'a>>>,
    /// Base URL for rewriting bare import specifiers.
    module_base: Option<String>,
    /// Primitive conversion calls lowered by the typechecker.
    exception_forms: deka_syntax::typeck::ExceptionLowering<'a>,
    unwrap_calls: HashMap<*const Expr<'a>, deka_syntax::typeck::UnwrapKind>,
    enum_case_patterns: HashMap<*const deka_syntax::Pattern<'a>, &'a str>,
    /// Union member type-patterns and their runtime predicates, lowered by
    /// the typechecker (rfd#42, deka#530).
    union_type_patterns:
        HashMap<*const deka_syntax::Pattern<'a>, deka_syntax::typeck::UnionMemberTest<'a>>,
    /// Compiler descriptors for build-only bindings. These are used only to
    /// retain the real factories needed by cache-only virtual modules.
    build_blocks: HashMap<*const Expr<'a>, deka_syntax::typeck::DevBlock<'a>>,
    build_factory_names: HashSet<String>,
    /// Factories this module's compiler-private `__deka_factories` closure
    /// must provide to consumer modules' build hydration (dsc#52): every
    /// descriptor-reachable factory of this module's exported types,
    /// including types private to this module. Empty when module-graph
    /// compilation determined no consumer build needs the closure.
    build_closure_names: HashSet<String>,
    /// Per-source factory closures available to this module's build
    /// hydration, derived from imported modules' descriptor fragments. A
    /// descriptor-reachable factory with no local binding is obtained by
    /// spreading the defining module's closure (dsc#52).
    closure_sources: HashMap<String, HashSet<String>>,
    unwrap_id: usize,
    match_id: usize,
    lifted_values: HashMap<*const Expr<'a>, String>,
    melted_results: HashMap<*const Expr<'a>, (String, String, String)>,
    /// Newtype operator rewrites lowered by the typechecker.
    operator_rewrites: HashMap<*const Expr<'a>, deka_syntax::typeck::OperatorRewrite<'a>>,
    /// Primitive extension call sites lowered by the typechecker to
    /// free-function calls (`slugify$string(s)`) (deka#527).
    method_calls: HashMap<*const Expr<'a>, deka_syntax::MethodTarget<'a>>,
    /// Builtin `.getType()` call sites lowered by the typechecker to
    /// `__deka_type_of(x)` (rfd#41, deka#529).
    type_of_calls: HashSet<*const Expr<'a>>,
    /// `.signature()` call sites lowered to static descriptor literals.
    signature_calls: HashMap<*const Expr<'a>, deka_syntax::typeck::DescriptorTree<'a>>,
    json_calls: HashMap<*const Expr<'a>, deka_syntax::typeck::JsonCall<'a>>,
    /// Builtin `first`/`last`/`pop`/`shift` array method call sites lowered by
    /// the typechecker (deka#561, deka#566): the emitter rewrites the call
    /// to an Option-producing expression, since JS has no `first`/`last` and
    /// its `pop`/`shift` return raw values rather than the declared Option.
    array_builtin_calls: HashMap<*const Expr<'a>, deka_syntax::typeck::ArrayAccess>,
    /// Builtin Math-backed `number` method call sites lowered by the
    /// typechecker (deka#378 step 2, rfd#40 phase 2): JS numbers have no
    /// such methods, so the emitter rewrites the call to a `Math.*`
    /// expression, wrapping partial functions so `NaN` surfaces as `None`.
    number_math_calls: HashMap<*const Expr<'a>, deka_syntax::typeck::NumberMath>,
    /// Builtin `Name.type()` call sites on `super` declarations, lowered by
    /// the typechecker (rfd#41, deka#561 PR B): the emitter rewrites each
    /// call to the interned `__deka_super_desc$<Name>` const.
    static_type_calls: HashMap<*const Expr<'a>, deka_syntax::typeck::StaticTypeCall<'a>>,
    /// Descriptor trees for every `super` declaration visible to the
    /// typechecker, keyed by declaration name. Read by `emit_prelude` to
    /// intern one frozen const per referenced declaration.
    super_decl_trees: std::collections::HashMap<&'a str, deka_syntax::typeck::DescriptorTree<'a>>,
    source_path: String,
    file_stem: String,
    /// Project root that build slot ids are relativized against before
    /// hashing (dsc#61). None keeps the historical absolute-path identity.
    module_root: Option<PathBuf>,
    /// Dev entries are JS modules written next to normal emitted modules.
    dev_entry: bool,
    fn_scope: String,
    jsx_path: Vec<usize>,
    jsx_siblings: Vec<usize>,
    jsx_roots: usize,
    /// When set, only these top-level names are emitted (graph shaking).
    live_names: Option<HashSet<String>>,
    jsx_runtime: String,
    jsx_helpers: [String; 3],
    /// When true (module-graph emission), the shared runtime prelude is NOT
    /// inlined into this module's output. The module graph synthesizes it
    /// once per program from the union of per-module [`Self::demand`]
    /// (deka#595); module bodies then reference the helpers as free
    /// identifiers resolved by the single program prelude.
    detached: bool,
    /// This module's demand for shared runtime helpers, computed during
    /// `emit_prelude` from the same scans that gate emission.
    demand: crate::prelude::PreludeDemand,
}

impl<'a> Emitter<'a> {
    fn new(program: &'a Program<'a>) -> Self {
        let mut emitter = Self {
            program,
            out: String::new(),
            uses_struct: false,
            uses_promotion_factory: false,
            uses_newtype: false,
            uses_prelude_enums: false,
            struct_order: Vec::new(),
            structs: HashMap::new(),
            promotion_structs: HashMap::new(),
            enums: HashMap::new(),
            opaques: HashSet::new(),
            interfaces: HashSet::new(),
            erased_type_exports: HashMap::new(),
            newtypes: HashMap::new(),
            receiver_methods: HashMap::new(),
            module_base: None,
            exception_forms: Default::default(),
            unwrap_calls: HashMap::new(),
            enum_case_patterns: HashMap::new(),
            union_type_patterns: HashMap::new(),
            build_blocks: HashMap::new(),
            build_factory_names: HashSet::new(),
            build_closure_names: HashSet::new(),
            closure_sources: HashMap::new(),
            unwrap_id: 0,
            match_id: 0,
            lifted_values: HashMap::new(),
            melted_results: HashMap::new(),
            operator_rewrites: HashMap::new(),
            method_calls: HashMap::new(),
            type_of_calls: HashSet::new(),
            signature_calls: HashMap::new(),
            json_calls: HashMap::new(),
            array_builtin_calls: HashMap::new(),
            number_math_calls: HashMap::new(),
            static_type_calls: HashMap::new(),
            super_decl_trees: std::collections::HashMap::new(),
            source_path: "module.ds".to_string(),
            file_stem: "module".to_string(),
            module_root: None,
            dev_entry: false,
            fn_scope: "_".to_string(),
            jsx_path: Vec::new(),
            jsx_siblings: Vec::new(),
            jsx_roots: 0,
            live_names: None,
            jsx_runtime: "@js/react/jsx-runtime".to_string(),
            jsx_helpers: ["jsx".into(), "jsxs".into(), "Fragment".into()],
            detached: false,
            demand: crate::prelude::PreludeDemand::default(),
        };
        emitter.prepass();
        emitter
    }

    fn emit(&mut self) -> Result<String, String> {
        // "use strict" belongs in the directive prologue, before even the
        // hoisted imports: after an import it degrades to a dead expression
        // statement, and the kit's RAW display strips it only when it is the
        // first line. (Redundant but legal in ES modules, which are always
        // strict.)
        self.out.push_str("\"use strict\";\n");
        // Imports must precede other statements. Hoist user imports, then the
        // jsx runtime import when this file contains JSX.
        let mut first = true;
        for stmt in self.program.statements.iter() {
            if matches!(stmt, Stmt::Import { .. })
                && self.should_emit_stmt(stmt)
                && self.should_emit_runtime_import(stmt)
            {
                if !first {
                    self.out.push('\n');
                }
                first = false;
                self.emit_stmt(stmt)?;
            }
        }
        // Runtime code never contains a dev body. The host resolves this
        // opaque value import from the compiler plan after executing the
        // matching dev-only entry.
        let mut closure_sources: Vec<String> = Vec::new();
        for stmt in self.program.statements.iter() {
            let Some((name, value, _)) = dev_binding(stmt) else {
                continue;
            };
            if !self.is_live(name) {
                continue;
            }
            let slot = dev_slot_id(
                &dev_slot_source_path(&self.source_path, self.module_root.as_deref()),
                name,
                value.span(),
            );
            if !first {
                self.out.push('\n');
            }
            first = false;
            self.out.push_str("import { hydrate as __deka_build_");
            self.out.push_str(&slot);
            self.out.push_str(" } from \"deka:dev/");
            self.out.push_str(&slot);
            self.out.push_str("\";");
            // Build descriptors can reach factories private to an imported
            // module; hydration obtains those through the module's
            // compiler-private closure (dsc#52). Collect the needed sources
            // here so the synthesized imports stay hoisted with the others.
            for source in self.build_binding_spreads(name, value)? {
                if !closure_sources.contains(&source) {
                    closure_sources.push(source);
                }
            }
        }
        for (index, source) in closure_sources.iter().enumerate() {
            if !first {
                self.out.push('\n');
            }
            first = false;
            self.out.push_str("import { __deka_factories as __deka_factories_");
            self.out.push_str(&index.to_string());
            self.out.push_str(" } from \"");
            self.out.push_str(&self.resolve_module_source(source));
            self.out.push_str("\";");
        }
        if self.needs_jsx_helper() {
            if !first { self.out.push('\n'); }
            first = false;
            let helpers: Vec<String> = ["jsx", "jsxs", "Fragment"].iter().zip(&self.jsx_helpers)
                .map(|(name, local)| if *name == local { local.clone() } else { format!("{name} as {local}") }).collect();
            self.out.push_str("import { ");
            self.out.push_str(&helpers.join(", "));
            self.out.push_str(" } from ");
            self.out.push_str(&json_string(&self.jsx_runtime));
            self.out.push(';');
        }

        // Imports end without a trailing newline; separate them from the
        // prelude so each `import` stays on its own line. Hosts transform
        // static imports line-by-line, so `import ...;"use strict";` on one
        // line would survive into non-module execution.
        if !first {
            self.out.push('\n');
        }

        self.emit_prelude()?;

        // First pass: emit struct/enum/function declarations so that all
        // factories exist before receiver methods are registered.
        for stmt in self.program.statements.iter() {
            if matches!(stmt, Stmt::Import { .. }) || dev_binding(stmt).is_some() {
                continue;
            }
            if !Self::is_runtime_statement(stmt) && self.should_emit_stmt(stmt) {
                if !first {
                    self.out.push('\n');
                }
                first = false;
                self.emit_stmt(stmt)?;
            }
        }

        // Register receiver methods after all struct factories are declared.
        self.emit_method_registrations()?;

        // Compiler-private factory closure for cross-module build hydration
        // (dsc#52). It captures every descriptor-reachable factory of this
        // module's exported types — including types private to this module —
        // so a consumer's hydration call can obtain factories it has no
        // lexical binding for. Module-graph compilation only asks for this
        // export when a live consumer build reaches these factories; the
        // export is never part of DekaScript module metadata, and an unused
        // one is dropped by ordinary bundler tree-shaking.
        if !self.build_closure_names.is_empty() {
            if !first {
                self.out.push('\n');
            }
            first = false;
            let mut closure: Vec<(String, String)> = self
                .build_closure_names
                .iter()
                .filter_map(|name| {
                    self.build_factory_binding(name)
                        .map(|binding| (name.clone(), binding.to_string()))
                })
                .collect();
            closure.sort();
            self.out.push_str("const __deka_factories = () => ({");
            for (index, (_, binding)) in closure.iter().enumerate() {
                if index > 0 {
                    self.out.push_str(", ");
                }
                self.out.push_str(binding);
            }
            self.out.push_str("});\nexport { __deka_factories };");
        }

        // The host publishes only JSON-compatible build data. Materialize it
        // through this module's declared factories after receiver methods are
        // installed, preserving the same prototype identity as a source
        // literal without exposing descriptors at runtime.
        for stmt in self.program.statements.iter() {
            let Some((name, value, exported)) = dev_binding(stmt) else {
                continue;
            };
            if !self.is_live(name) {
                continue;
            }
            let slot = dev_slot_id(
                &dev_slot_source_path(&self.source_path, self.module_root.as_deref()),
                name,
                value.span(),
            );
            if !first {
                self.out.push('\n');
            }
            first = false;
            self.out.push_str("const ");
            self.out.push_str(name);
            self.out.push_str(" = __deka_build_");
            self.out.push_str(&slot);
            self.out.push_str("({");
            let mut names = HashSet::new();
            if let Some(block) = self.build_blocks.get(&(value as *const Expr<'a>)) {
                build_factory_names(&block.descriptor, &mut names);
            }
            // Local bindings cover factories this module can name; anything
            // else must come from an imported module's closure spread. A
            // factory in neither place is a build error — hydration would
            // silently downgrade identity otherwise (dsc#52).
            let spreads = self.build_binding_spreads(name, value)?;
            let mut factories: Vec<_> = names
                .iter()
                .filter_map(|name| {
                    self.build_factory_binding(&name)
                        .map(|binding| ((*name).to_string(), binding.to_string()))
                })
                .collect();
            factories.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            let mut entries = String::new();
            for source in &spreads {
                let alias = format!(
                    "__deka_factories_{}",
                    closure_sources.iter().position(|s| s == source).unwrap()
                );
                if !entries.is_empty() {
                    entries.push_str(", ");
                }
                entries.push_str("...");
                entries.push_str(&alias);
                entries.push_str("()");
            }
            for (factory, binding) in &factories {
                if !entries.is_empty() {
                    entries.push_str(", ");
                }
                if factory == binding {
                    entries.push_str(binding);
                } else {
                    entries.push_str(&json_string(factory));
                    entries.push_str(": ");
                    entries.push_str(binding);
                }
            }
            self.out.push_str(&entries);
            self.out.push_str("});");
            if exported {
                self.out.push('\n');
                self.out.push_str("export { ");
                self.out.push_str(name);
                self.out.push_str(" };");
            }
        }

        // Second pass: emit executable top-level statements (const/let/expr).
        for stmt in self.program.statements.iter() {
            if Self::is_runtime_statement(stmt)
                && dev_binding(stmt).is_none()
                && self.should_emit_stmt(stmt)
            {
                if !first {
                    self.out.push('\n');
                }
                first = false;
                self.emit_stmt(stmt)?;
            }
        }

        Ok(std::mem::take(&mut self.out))
    }

    fn emit_dev_entry(&mut self, slot: &str, body: &'a [Stmt<'a>]) -> Result<String, String> {
        self.dev_entry = true;
        self.live_names = Some(self.dev_live_names(body));
        self.out.push_str("\"use strict\";\n");

        for stmt in self.program.statements.iter() {
            if matches!(stmt, Stmt::Import { .. }) && self.should_emit_stmt(stmt) {
                self.emit_stmt(stmt)?;
                self.out.push('\n');
            }
        }
        self.emit_prelude()?;

        // Keep only declarations and top-level values reachable from this
        // dev body. Runtime expressions and other dev slots stay out.
        for stmt in self.program.statements.iter() {
            if matches!(stmt, Stmt::Import { .. }) || dev_binding(stmt).is_some() {
                continue;
            }
            if !Self::is_runtime_statement(stmt) && self.should_emit_stmt(stmt) {
                self.emit_stmt(stmt)?;
                self.out.push('\n');
            }
        }
        self.emit_method_registrations()?;
        for stmt in self.program.statements.iter() {
            if Self::is_runtime_statement(stmt)
                && self.should_emit_stmt(stmt)
                && dev_binding(stmt).is_none()
                && !matches!(stmt, Stmt::Expr { .. })
            {
                self.emit_stmt(stmt)?;
                self.out.push('\n');
            }
        }

        self.out
            .push_str("export default async function __deka_dev_");
        self.out.push_str(slot);
        self.out.push_str("() {\n");
        for stmt in body {
            self.emit_stmt(stmt)?;
            self.out.push('\n');
        }
        self.out.push_str("}\n");
        Ok(std::mem::take(&mut self.out))
    }

    fn dev_live_names(&self, body: &[Stmt<'a>]) -> HashSet<String> {
        let mut live = HashSet::new();
        for stmt in body {
            collect_dev_stmt_names(stmt, &mut live);
        }

        let mut changed = true;
        while changed {
            changed = false;
            for stmt in self.program.statements.iter() {
                let Some(name) = declared_name(stmt) else {
                    continue;
                };
                if !live.contains(name) || dev_binding(stmt).is_some() {
                    continue;
                }
                let before = live.len();
                collect_dev_stmt_names(stmt, &mut live);
                changed |= live.len() > before;
            }
        }
        live
    }

    fn is_erased_binding(&self, name: &str) -> bool {
        self.opaques.contains(name) || self.interfaces.contains(name)
    }

    fn is_erased_export(&self, name: &str, source: Option<&str>) -> bool {
        match source {
            Some(source) => self
                .erased_type_exports
                .get(source)
                .is_some_and(|names| names.contains(name)),
            None => self.is_erased_binding(name),
        }
    }

    fn is_live(&self, name: &str) -> bool {
        self.live_names
            .as_ref()
            .map_or(true, |live| live.contains(name))
    }

    fn should_emit_runtime_import(&self, stmt: &Stmt<'a>) -> bool {
        // Graph shaking already computes runtime liveness. Single-file
        // compilation keeps every ordinary import by default, so it needs the
        // same distinction here: references nested in `build` do not make a
        // binding part of the runtime graph.
        let Stmt::Import { specifiers, .. } = stmt else {
            return true;
        };
        if specifiers.is_empty() {
            return true;
        }

        if self.live_names.is_some() {
            return specifiers.iter().any(|specifier| {
                self.is_live(specifier.local) || self.is_build_factory_import(specifier)
            });
        }

        if specifiers
            .iter()
            .any(|specifier| self.is_build_factory_import(specifier))
        {
            return true;
        }

        // Retain normal imports, including apparently-unused package imports:
        // their module initialization may be observable. Only an import whose
        // every binding belongs solely to a dev entry can leave runtime JS.
        !specifiers.iter().all(|specifier| {
            dev_uses_name(self.program, specifier.local)
                && !runtime_uses_name(self.program, specifier.local)
        })
    }

    fn should_emit_stmt(&self, stmt: &Stmt<'_>) -> bool {
        if self.live_names.is_none() {
            return true;
        }
        match stmt {
            // Same rule as : kept when the bound name is live.
            Stmt::TupleBinding { .. } => true,
            Stmt::UnwrapLet { name, .. } => self.is_live(name),
            Stmt::Import { specifiers, .. } => {
                specifiers.is_empty()
                    || specifiers.iter().any(|spec| {
                        !self.is_erased_binding(spec.local)
                            && (self.is_live(spec.local) || self.is_build_factory_import(spec))
                    })
            }
            Stmt::Export { decl, .. } => match decl {
                ExportDecl::Const { name, .. } | ExportDecl::Function { name, .. } => {
                    self.is_live(name)
                }
                ExportDecl::NamedGroup { names, .. } => names.iter().any(|n| {
                    !self.is_erased_binding(n.name)
                        && (self.is_live(n.alias.unwrap_or(n.name)) || self.is_live(n.name))
                }),
            },
            Stmt::Const { name, .. }
            | Stmt::Let { name, .. }
            | Stmt::Function { name, .. }
            | Stmt::TypeAlias { name, .. }
            | Stmt::Interface { name, .. } => self.is_live(name),
            Stmt::Struct { name, .. } | Stmt::Enum { name, .. } | Stmt::Newtype { name, .. } => {
                self.is_live(name) || self.is_build_retained(name)
            }
            Stmt::ReceiverMethod {
                receiver_type,
                name,
                ..
            } => {
                // Primitive extensions are emitted as free functions named
                // `method$receiver`; struct and newtype methods ride the
                // receiver type's liveness (deka#527).
                if is_primitive_receiver(receiver_type) {
                    self.is_live(&format!("{name}${receiver_type}"))
                } else {
                    self.is_live(receiver_type) || self.is_build_retained(receiver_type)
                }
            }
            Stmt::Expr { .. }
            | Stmt::If { .. }
            | Stmt::Block { .. }
            | Stmt::For { .. }
            | Stmt::ForOf { .. }
            | Stmt::Return { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. }
            | Stmt::Try { .. }
            | Stmt::Opaque { .. }
            | Stmt::Summon { .. }
            | Stmt::Empty { .. } => true,
        }
    }

    /// Returns true for statements whose initializers run at module load time.
    fn is_runtime_statement(stmt: &Stmt<'_>) -> bool {
        matches!(
            stmt,
            Stmt::TupleBinding { .. } | Stmt::Const { .. }
                | Stmt::Let { .. }
                // Its initializer runs at load time like any other binding.
                // Omitting it here classified the statement as a declaration,
                // so it was hoisted above the runtime statements and read a
                // binding that had not been initialised yet (deka#445).
                | Stmt::UnwrapLet { .. }
                | Stmt::Expr { .. }
                | Stmt::Return { .. }
                | Stmt::If { .. }
                | Stmt::Try { .. }
                | Stmt::Block { .. }
                | Stmt::For { .. }
                | Stmt::ForOf { .. }
                | Stmt::Break { .. }
                | Stmt::Continue { .. }
        )
    }

    /// Return the runtime binding that implements a descriptor factory in
    /// this module. Imports may rename it, and the descriptor then carries
    /// the local name, so match either side of the specifier; hydration uses
    /// the descriptor name as its object key rather than relying on
    /// JavaScript shorthand.
    fn build_factory_binding(&self, factory: &str) -> Option<&str> {
        for stmt in self.program.statements.iter() {
            match stmt {
                Stmt::Struct { name, .. }
                | Stmt::Enum { name, .. }
                | Stmt::Newtype { name, .. }
                    if *name == factory =>
                {
                    return Some(name);
                }
                Stmt::Import { specifiers, .. } => {
                    if let Some(specifier) = specifiers.iter().find(|specifier| {
                        specifier.local == factory || specifier.imported == factory
                    }) {
                        return Some(specifier.local);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn is_build_factory_import(&self, specifier: &deka_syntax::ImportSpec<'a>) -> bool {
        self.build_factory_names.contains(specifier.imported)
            || self.build_factory_names.contains(specifier.local)
            || self.build_closure_names.contains(specifier.imported)
            || self.build_closure_names.contains(specifier.local)
    }

    /// Whether a type declaration is retained for build materialization:
    /// either its own module's build bindings need it, or this module's
    /// compiler-private factory closure must provide it to consumers (dsc#52).
    fn is_build_retained(&self, name: &str) -> bool {
        self.build_factory_names.contains(name) || self.build_closure_names.contains(name)
    }

    /// Import sources whose compiler-private factory closure a build
    /// binding's hydration call must spread (dsc#52). Factories with a local
    /// binding need no closure; a factory with neither a local binding nor a
    /// covering closure is a build error — hydration must never fall back to
    /// a lookalike object.
    fn build_binding_spreads(
        &self,
        binding: &str,
        value: &Expr<'a>,
    ) -> Result<Vec<String>, String> {
        let mut names = HashSet::new();
        if let Some(block) = self.build_blocks.get(&(value as *const Expr<'a>)) {
            build_factory_names(&block.descriptor, &mut names);
        }
        let missing: Vec<&String> = names
            .iter()
            .filter(|name| self.build_factory_binding(name).is_none())
            .collect();
        if missing.is_empty() {
            return Ok(Vec::new());
        }
        let mut sources: Vec<String> = Vec::new();
        for stmt in self.program.statements.iter() {
            let Stmt::Import { source, .. } = stmt else {
                continue;
            };
            if sources.iter().any(|seen| seen.as_str() == *source) {
                continue;
            }
            let Some(closure) = self.closure_sources.get(*source) else {
                continue;
            };
            if missing.iter().any(|name| closure.contains(*name)) {
                sources.push((*source).to_string());
            }
        }
        for name in missing {
            let covered = sources.iter().any(|source| {
                self.closure_sources
                    .get(source)
                    .is_some_and(|closure| closure.contains(name.as_str()))
            });
            if !covered {
                return Err(format!(
                    "build binding `{binding}` cannot obtain the factory `{name}` required by its declared type"
                ));
            }
        }
        Ok(sources)
    }

    // ------------------------------------------------------------------
    // Pre-pass: collect struct/enum metadata and receiver methods.
    // ------------------------------------------------------------------
    fn prepass(&mut self) {
        for stmt in self.program.statements.iter() {
            match stmt {
                Stmt::Struct {
                    name,
                    fields,
                    embeds,
                    ..
                } => {
                    let mut meta = StructMeta::default();
                    meta.brand = name.to_string();
                    for field in fields.iter() {
                        meta.fields.insert(field.name.to_string());
                        if field.default_value.is_some()
                            || field.optional
                            || is_optional_type(&field.ty)
                        {
                            let default = field.default_value.as_ref().map(|v| {
                                // If a default value cannot be pre-emitted, fall back to null.
                                self.emit_expr_to_string(v)
                                    .unwrap_or_else(|_| "null".to_string())
                            });
                            if default.is_none() {
                                // Omitted optional fields auto-fill to Option.None, so the
                                // prelude enum helpers are required even if the source never
                                // mentions Some/None explicitly.
                                self.uses_prelude_enums = true;
                            }
                            meta.optional.insert(field.name.to_string(), default);
                        }
                    }
                    for embed in embeds.iter() {
                        meta.embeds.push(embed.name.to_string());
                    }
                    self.structs.insert(name.to_string(), meta);
                    self.struct_order.push(name.to_string());
                }
                Stmt::Enum { name, cases, .. } => {
                    let mut meta = EnumMeta::default();
                    for case in cases.iter() {
                        meta.cases.push(case.name.to_string());
                        if case.payload.is_some() {
                            meta.payload_cases.insert(case.name.to_string());
                        }
                    }
                    self.enums.insert(name.to_string(), meta);
                }
                Stmt::ReceiverMethod {
                    receiver_type,
                    receiver_name,
                    receiver_mutable,
                    name,
                    params,
                    body,
                    is_async,
                    ..
                } => {
                    self.receiver_methods
                        .entry(receiver_type.to_string())
                        .or_default()
                        .push(ReceiverMethod {
                            name: name.to_string(),
                            receiver_name: receiver_name.to_string(),
                            receiver_mutable: *receiver_mutable,
                            params: params.iter().map(|p| p.name.to_string()).collect(),
                            body: *body,
                            is_async: *is_async,
                        });
                }
                Stmt::Opaque { name, .. } => {
                    self.opaques.insert(name.to_string());
                }
                Stmt::Interface { name, .. } => {
                    self.interfaces.insert(name.to_string());
                }
                Stmt::Newtype { name, repr, .. } => {
                    self.newtypes.insert(name.to_string(), *repr);
                    self.uses_newtype = true;
                }
                _ => {}
            }
        }
        self.compute_empty_embeds();
    }

    /// Rewrite a bare module specifier to a resolvable URL when `module_base`
    /// is configured. Bare specifiers are those that do not start with `.`,
    /// `/`, or a URL scheme. The `@deka/` prefix is stripped so both `io` and
    /// `@deka/io` map to `<base>/io.mjs`.
    ///
    /// Note: unknown bare specifiers are rejected at compile time before emit
    /// when module_base is set (deka#497), so this path only sees stdlib names.
    fn resolve_module_source(&self, source: &str) -> String {
        if self.dev_entry
            && (source.starts_with("./") || source.starts_with("../"))
            && (source.ends_with(".ds") || source.ends_with(".dsx"))
        {
            let ext_len = if source.ends_with(".dsx") { 4 } else { 3 };
            return format!("{}.js", &source[..source.len() - ext_len]);
        }
        let Some(base) = &self.module_base else {
            return source.to_string();
        };
        if source.starts_with('.') || source.starts_with('/') {
            return source.to_string();
        }
        if source.contains(':') {
            // URL scheme (e.g. https://, data:)
            return source.to_string();
        }
        let name = source.strip_prefix("@deka/").unwrap_or(source);
        let base = base.trim_end_matches('/');
        format!("{}/{}.mjs", base, name)
    }

    fn seed_imports(&mut self, imports: &HashMap<&str, &deka_syntax::ModuleExports<'a>>) {
        for (source, exports) in imports.iter() {
            self.erased_type_exports.insert(
                (*source).to_string(),
                exports
                    .opaques
                    .keys()
                    .chain(exports.interfaces.keys())
                    .map(|name| name.to_string())
                    .collect(),
            );
            // A dependency's descriptor fragments name factories in the
            // dependency's own namespace; their union is the closure its
            // compiler-private `__deka_factories` export provides (dsc#52).
            let mut names = HashSet::new();
            for tree in exports.build_fragments.values() {
                build_factory_names(tree, &mut names);
            }
            if !names.is_empty() {
                self.closure_sources
                    .insert((*source).to_string(), names);
            }
        }
        // Import specifiers can rename their binding (`import { User as
        // Person }`). The typechecker binds imported metadata under the local
        // name, and source literals spell the local name, so aliased factories
        // must be seeded under the alias as well (dsc#51).
        let mut renamed: Vec<(&'a str, &'a str, &deka_syntax::ModuleExports<'a>)> = Vec::new();
        let mut promotion_closures: Vec<(&'a str, &'a str, &deka_syntax::ModuleExports<'a>)> = Vec::new();
        for stmt in self.program.statements.iter() {
            let Stmt::Import {
                specifiers, source, ..
            } = stmt
            else {
                continue;
            };
            let Some(exports) = imports.get(source) else {
                continue;
            };
            for spec in specifiers.iter() {
                promotion_closures.push((spec.local, spec.imported, *exports));
                if spec.imported != spec.local {
                    renamed.push((spec.local, spec.imported, *exports));
                }
            }
        }
        for exports in imports.values() {
            for (name, info) in exports.structs.iter() {
                if !self.structs.contains_key(*name) {
                    self.seed_struct_export(name, name, info);
                }
            }
            for (name, info) in exports.enums.iter() {
                if !self.enums.contains_key(*name) {
                    self.seed_enum_export(name, info);
                }
            }
            for (name, info) in exports.newtypes.iter() {
                if !self.newtypes.contains_key(*name) {
                    self.seed_newtype_export(name, info);
                }
            }
        }
        for (local, imported, exports) in renamed {
            if let Some(info) = exports.structs.get(imported) {
                if !self.structs.contains_key(local) {
                    self.seed_struct_export(local, imported, info);
                }
            }
            if let Some(info) = exports.enums.get(imported) {
                if !self.enums.contains_key(local) {
                    self.seed_enum_export(local, info);
                }
            }
            if exports.opaques.contains_key(imported) {
                self.opaques.insert(local.to_string());
            }
            if exports.interfaces.contains_key(imported) {
                self.interfaces.insert(local.to_string());
            }
            if let Some(info) = exports.newtypes.get(imported) {
                if !self.newtypes.contains_key(local) {
                    self.seed_newtype_export(local, info);
                }
            }
        }
        for (local, imported, exports) in promotion_closures {
            let Some(closure) = exports.promotion_structs.get(imported) else {
                continue;
            };
            let mut metas = HashMap::new();
            for (name, info) in closure {
                metas.insert((*name).to_string(), self.struct_meta(name, info));
            }
            self.promotion_structs.insert(local.to_string(), metas);
        }
        self.compute_empty_embeds();
    }

    fn seed_struct_export(&mut self, name: &str, brand: &str, info: &deka_syntax::StructInfo<'a>) {
        let meta = self.struct_meta(brand, info);
        self.structs.insert(name.to_string(), meta);
    }

    fn struct_meta(&mut self, brand: &str, info: &deka_syntax::StructInfo<'a>) -> StructMeta {
        let mut meta = StructMeta::default();
        meta.brand = brand.to_string();
        for field in info.fields.iter() {
            meta.fields.insert(field.name.to_string());
            if field.default_value.is_some() || field.optional || is_optional_type(&field.ty) {
                // Imported struct defaults are not pre-emitted here;
                // omitting the field produces Option.None for Option-typed fields.
                if field.default_value.is_none() {
                    self.uses_prelude_enums = true;
                }
                meta.optional.insert(field.name.to_string(), None);
            }
        }
        for embed in info.embeds.iter() {
            meta.embeds.push(embed.name.to_string());
        }
        meta
    }

    fn seed_enum_export(&mut self, name: &str, info: &deka_syntax::EnumInfo<'a>) {
        let mut meta = EnumMeta::default();
        for case in info.cases.iter() {
            meta.cases.push(case.name.to_string());
            if case.payload.is_some() {
                meta.payload_cases.insert(case.name.to_string());
            }
        }
        self.enums.insert(name.to_string(), meta);
    }

    fn seed_newtype_export(&mut self, name: &str, info: &deka_syntax::typeck::NewtypeInfo) {
        self.newtypes.insert(name.to_string(), info.repr);
        self.uses_newtype = true;
    }

    fn compute_empty_embeds(&mut self) {
        let names: Vec<String> = self.structs.keys().cloned().collect();
        let mut emptiness: HashMap<String, bool> = HashMap::new();
        for name in &names {
            emptiness.insert(name.clone(), self.is_empty_embed_struct(name));
        }
        for name in &names {
            let embeds = self.structs[name].embeds.clone();
            for embed in embeds {
                if *emptiness.get(&embed).unwrap_or(&false) {
                    self.structs
                        .get_mut(name)
                        .unwrap()
                        .empty_embeds
                        .insert(embed);
                }
            }
        }
    }

    fn is_empty_embed_struct(&self, name: &str) -> bool {
        let meta = match self.structs.get(name) {
            Some(m) => m,
            None => return false,
        };
        // A struct is an "empty embed" if it has no non-embed fields and all
        // of its embedded structs are also empty embeds.
        for field in &meta.fields {
            if !meta.embeds.contains(field) {
                return false;
            }
        }
        for embed in &meta.embeds {
            if !self.is_empty_embed_struct(embed) {
                return false;
            }
        }
        true
    }

    // ------------------------------------------------------------------
    // Prelude
    // ------------------------------------------------------------------
    fn emit_prelude(&mut self) -> Result<(), String> {
        // Determine which helpers are needed by scanning the AST.
        self.uses_struct = self.needs_struct_helper();
        self.uses_prelude_enums = self.uses_prelude_enums || self.needs_prelude_enums();
        // `type_of_calls` is fully populated by the typechecker before
        // emission, so unlike the emission-time flags above it can gate the
        // helper directly — no AST scan needed. A call in shaken code still
        // forces the ~4-line helper; harmless bloat, never incorrectness.
        let uses_typeof = !self.type_of_calls.is_empty();
        let uses_json = !self.json_calls.is_empty();
        if uses_json {
            self.uses_prelude_enums = true;
        }
        // Record this module's demand for the shared helpers at member
        // granularity; the module graph unions these sets across the whole
        // program and synthesizes the prelude once (deka#595).
        self.demand = self.prelude_demand(uses_typeof);

        if !self.detached {
            // Module-local synthesis goes through the same single builder the
            // program-level prelude uses — one construction site, one spelling
            // (deka#582, deka#622). The module graph instead detaches this and
            // prepends the unioned program prelude to the bundle.
            let shared = crate::prelude::shared_prelude(&self.demand);
            self.out.push_str(&shared);
        }

        // Static descriptor consts for `super` declarations (rfd#41,
        // deka#561 PR B): one frozen, name-keyed module-local const per
        // declaration actually referenced by a recorded `Name.type()` call —
        // `User.type()` rewrites to `__deka_super_desc$User`. Emission is
        // driven by use, so an unused `super` marking costs nothing, and a
        // marking whose only calls sit in shaken code forces only its own
        // const (harmless bloat, never incorrectness — same guarantee the
        // type_of_calls gate documents above). Recursive groups expand: a
        // declaration whose tree holds `Recurse` nodes pulls the consts of
        // every declaration it references, via the typechecker's
        // `super_decl_trees` map. Trees with recursion are emitted with lazy
        // composite getters (`get fields() { ... }`), so const
        // initialization order is unconstrained — a recursive const may be
        // referenced before it is declared and is only dereferenced on
        // access, after module init.
        let mut emitted: HashSet<&'a str> = HashSet::new();
        let mut group: Vec<&'a str> = Vec::new();
        for call in self.static_type_calls.values() {
            if let Some(tree) = call.tree.as_ref() {
                if let Some(name) = super_decl_top_name(tree) {
                    collect_super_group(name, &self.super_decl_trees, &mut emitted, &mut group);
                }
            }
        }
        for name in group {
            let tree = self
                .super_decl_trees
                .get(name)
                .ok_or_else(|| format!("missing descriptor tree for super declaration `{name}`"))?;
            self.out.push_str(&format!(
                "const {} = {};\n",
                super_const_name(name),
                emit_super_tree(tree)?
            ));
        }

        if uses_json {
            self.out.push_str(&emit_json_functions(&self.json_calls));
        }

        Ok(())
    }

    /// This module's demand for the shared runtime helpers, at member
    /// granularity. The scans mirror exactly what will be emitted: receiver
    /// methods gate `impl`/`implMut` the same way `emit_method_registrations`
    /// decides which `.impl(`/`.implMut(` calls exist, and embeds are
    /// demanded by any live struct whose factory is constructed with an
    /// embeds map.
    fn prelude_demand(&self, uses_typeof: bool) -> crate::prelude::PreludeDemand {
        let mut demand = crate::prelude::PreludeDemand::default();
        if self.uses_struct {
            let mut parts = crate::prelude::StructDemand::default();
            for struct_name in &self.struct_order {
                if !self.is_live(struct_name) && !self.is_build_retained(struct_name) {
                    continue;
                }
                let methods = self.collect_methods_for_struct(struct_name, &mut HashSet::new());
                for method in &methods {
                    if method.receiver_mutable {
                        parts.impl_mut = true;
                    } else {
                        parts.impl_methods = true;
                    }
                }
                if let Some(meta) = self.structs.get(struct_name) {
                    if !meta.embeds.is_empty() {
                        parts.embeds = true;
                    }
                }
            }
            parts.embeds |= self.uses_promotion_factory;
            demand.structs = Some(parts);
        }
        demand.newtype = self.uses_newtype;
        demand.type_of = uses_typeof;
        demand.enums = self.uses_prelude_enums;
        for stmt in self.program.statements {
            if self.should_emit_stmt(stmt) {
                visit_stmt_exprs(stmt, &mut |expr| {
                    if matches!(expr, Expr::Bridge { .. }) {
                        demand.host_result = true;
                    }
                });
            }
        }
        // Brand-tag branches of `__deka_type_of` are gated by what the
        // program can produce, not by what this module imports: values cross
        // module boundaries without their type binding (function returns), so
        // only declarations are a sound approximation of "tag can exist".
        demand.declares_structs = !self.struct_order.is_empty();
        demand.declares_enums = self
            .program
            .statements
            .iter()
            .any(|stmt| matches!(stmt, Stmt::Enum { .. }));
        demand.declares_newtypes = self
            .program
            .statements
            .iter()
            .any(|stmt| matches!(stmt, Stmt::Newtype { .. }));
        demand
    }

    fn enter_jsx_node(&mut self) -> usize {
        let index = if let Some(next) = self.jsx_siblings.last_mut() {
            let i = *next;
            *next += 1;
            i
        } else {
            let i = self.jsx_roots;
            self.jsx_roots += 1;
            i
        };
        self.jsx_path.push(index);
        self.jsx_siblings.push(0);
        index
    }

    fn exit_jsx_node(&mut self) {
        self.jsx_siblings.pop();
        self.jsx_path.pop();
    }

    fn current_deka_id(&self) -> String {
        let path = self
            .jsx_path
            .iter()
            .map(|i| format!("i{i}"))
            .collect::<Vec<_>>()
            .join("/");
        format!("{}:{}/{}", self.file_stem, self.fn_scope, path)
    }

    fn with_fn_scope<T>(
        &mut self,
        name: &str,
        f: impl FnOnce(&mut Self) -> Result<T, String>,
    ) -> Result<T, String> {
        let previous = std::mem::replace(&mut self.fn_scope, name.to_string());
        let prev_roots = self.jsx_roots;
        let prev_path = std::mem::take(&mut self.jsx_path);
        let prev_sibs = std::mem::take(&mut self.jsx_siblings);
        self.jsx_roots = 0;
        let result = f(self);
        self.fn_scope = previous;
        self.jsx_roots = prev_roots;
        self.jsx_path = prev_path;
        self.jsx_siblings = prev_sibs;
        result
    }

    fn needs_struct_helper(&self) -> bool {
        // Receiver methods for struct receivers install on the factory
        // prototype via `.impl`, so they need the factory emitted; primitive
        // extensions are plain free functions and must not force it
        // (deka#527). Struct type-patterns read the `__deka_struct` brand tag
        // directly and need nothing (deka#551).
        !self.structs.is_empty()
            || self
                .receiver_methods
                .keys()
                .any(|name| !is_primitive_receiver(name))
    }

    fn needs_prelude_enums(&self) -> bool {
        if self
            .exception_forms
            .values()
            .any(|form| *form == deka_syntax::typeck::ExceptionEmit::ToResult)
        {
            return true;
        }
        self.program.statements.iter().any(|stmt| {
            let mut found = false;
            deka_syntax::visit::walk_stmt(stmt, &mut |expr| {
                if let Expr::EnumConstructor { enum_name, .. } = expr {
                    if *enum_name == "Result"
                        && !matches!(self.exception_forms.get(&(expr as *const _)), Some(deka_syntax::typeck::ExceptionEmit::Ok | deka_syntax::typeck::ExceptionEmit::Throw)) {
                        found = true;
                    }
                }
                // Safe catalog calls and unsafe bridges still need Result constructors.
                if matches!(expr, Expr::Safe { .. })
                    || matches!(expr, Expr::Unsafe { source, .. } if source.contains("deka") || source.contains("\\u")) {
                    found = true;
                }
            });
            found
        })
    }

    fn needs_jsx_helper(&self) -> bool {
        self.program.statements.iter().any(|stmt| {
            if !self.should_emit_stmt(stmt) {
                return false;
            }
            let mut found = false;
            visit_stmt_exprs(stmt, &mut |expr| {
                if matches!(expr, Expr::JsxElement { .. } | Expr::JsxFragment { .. }) {
                    found = true;
                }
            });
            found
        })
    }

    fn configure_jsx_names(&mut self, source: &str) {
        let mut names = HashSet::new();
        collect_js_identifier_tokens(source, &mut names);
        for name in &mut self.jsx_helpers {
            if names.contains(name) {
                *name = format!("__deka_{name}");
                while names.contains(name) {
                    name.push('_');
                }
            }
            names.insert(name.clone());
        }
    }

    // ------------------------------------------------------------------
    // Statements
    // ------------------------------------------------------------------
    fn emit_has_temporaries(&mut self, expressions: Vec<(usize, (String, String))>) {
        let mut names: Vec<_> = expressions
            .into_iter()
            .filter_map(|(ptr, names)| {
                self.array_builtin_calls
                    .iter()
                    .any(|(p, k)| *p as usize == ptr && *k == deka_syntax::typeck::ArrayAccess::Has)
                    .then_some(names)
            })
            .collect();
        names.sort();
        names.dedup();
        for (receiver, index) in names {
            self.out.push_str(&format!("let {receiver}, {index};\n"));
        }
    }

    fn emit_array_has(&mut self, expr: &Expr<'a>, wrap: bool) -> Result<(), String> {
        let Expr::Call { callee, args, .. } = expr else {
            unreachable!()
        };
        let Expr::FieldAccess { object, .. } = &**callee else {
            unreachable!()
        };
        let complex = has_needs_temporaries(expr);
        if wrap || complex {
            self.out.push('(');
        }
        let (receiver, index) = has_temporary_names(expr);
        if complex {
            self.out.push_str(&format!("{receiver} = "));
            self.emit_expr(object)?;
            self.out.push_str(&format!(", {index} = "));
            self.emit_expr(&args[0])?;
            self.out.push_str(", ");
        }
        self.out.push_str("Number.isInteger(");
        if complex {
            self.out.push_str(&index);
        } else {
            self.emit_expr(&args[0])?;
        }
        self.out.push_str(") && ");
        if complex {
            self.out.push_str(&index);
        } else {
            self.emit_expr(&args[0])?;
        }
        self.out.push_str(" >= 0 && ");
        if complex {
            self.out.push_str(&index);
        } else {
            self.emit_expr(&args[0])?;
        }
        self.out.push_str(" < ");
        if complex {
            self.out.push_str(&receiver);
        } else {
            self.emit_expr(object)?;
        }
        self.out.push_str(".length");
        if wrap || complex {
            self.out.push(')');
        }
        Ok(())
    }

    fn emit_stmt(&mut self, stmt: &Stmt<'a>) -> Result<(), String> {
        // Complex predicates evaluate receiver and index exactly once, in source
        // order. Locals plus a comma expression preserve lazy expression contexts
        // without an IIFE or runtime helper. `$` cannot collide with DS bindings.
        let mut expressions = Vec::new();
        deka_syntax::visit::walk_stmt(stmt, &mut |expr| {
            if has_needs_temporaries(expr) {
                expressions.push((expr as *const _ as usize, has_temporary_names(expr)));
            }
        });
        self.emit_has_temporaries(expressions);
        let saved = self.lifted_values.clone();
        let operand = match stmt {
            Stmt::TupleBinding { value, .. }
            | Stmt::Const { value, .. }
            | Stmt::Let { value, .. }
            | Stmt::Expr { expr: value, .. } => Some(value),
            Stmt::Return { value, .. } => value.as_ref(),
            Stmt::UnwrapLet { scrutinee, .. } => Some(scrutinee),
            Stmt::Export {
                decl: ExportDecl::Const { value, .. },
                ..
            } => Some(value),
            Stmt::If { condition, .. } => Some(condition),
            Stmt::ForOf { iterable, .. } => Some(iterable),
            _ => None,
        };
        if let Some(value) = operand {
            let handles_match = matches!(
                stmt,
                Stmt::Const { .. } | Stmt::Let { .. } | Stmt::Expr { .. } | Stmt::Return { .. }
            );
            if (!handles_match || !matches!(peel_exception_parens(value), Expr::Match { .. }))
                && self.exception_form(value) != Some(deka_syntax::typeck::ExceptionEmit::Throw)
                && needs_lifting(value)
            {
                let emitted = self.lift_value(value)?;
                self.lifted_values.insert(value as *const _, emitted);
            }
        }
        let result = self.emit_stmt_inner(stmt);
        self.lifted_values = saved;
        result
    }

    fn emit_stmt_inner(&mut self, stmt: &Stmt<'a>) -> Result<(), String> {
        match stmt {
            Stmt::Try {
                body,
                catch_name,
                catch_type,
                catch_body,
                ..
            } => {
                self.out.push_str("try {\n");
                for stmt in *body {
                    self.emit_stmt(stmt)?;
                    self.out.push('\n');
                }
                self.out.push_str(&format!("}} catch ({catch_name}) {{\n"));
                if let Some(annotation) = catch_type {
                    let constructor = self
                        .exception_forms
                        .catches
                        .get(&(annotation as *const _))
                        .copied()
                        .ok_or(
                            "typed catch emission requires checker-resolved constructor metadata",
                        )?;
                    self.out.push_str(&format!(
                        "if (!({catch_name} instanceof {constructor})) {{ throw {catch_name}; }}\n"
                    ));
                }
                for stmt in *catch_body {
                    self.emit_stmt(stmt)?;
                    self.out.push('\n');
                }
                self.out.push('}');
            }

            Stmt::TupleBinding {
                names,
                value,
                is_const,
                ..
            } => {
                self.out
                    .push_str(if *is_const { "const [" } else { "let [" });
                self.out.push_str(&names.join(", "));
                self.out.push_str("] = ");
                self.emit_expr(value)?;
                self.out.push(';');
            }
            Stmt::Const { name, value, .. } => {
                if let Expr::Match {
                    scrutinee, arms, ..
                } = peel_exception_parens(value)
                {
                    let result = self.emit_match_value_statements(scrutinee, arms)?;
                    write_indent(&mut self.out, 0);
                    self.out.push_str("const ");
                    self.out.push_str(name);
                    self.out.push_str(" = ");
                    self.out.push_str(&result);
                    self.out.push(';');
                    return Ok(());
                }
                write_indent(&mut self.out, 0);
                self.out.push_str("const ");
                self.out.push_str(name);
                self.out.push_str(" = ");
                // Const value immutability is carried by the checker, not the
                // runtime: typeck rejects index assignment and mutating
                // builtins on a const-bound array/object (deka#590, step 1 in
                // deka#591), so no Object.freeze is needed here. Freezing
                // moved arrays off the fast element path (3-8.7x slower).
                self.emit_expr(value)?;
                self.out.push_str(";");
            }
            Stmt::Let { name, value, .. } => {
                if let Expr::Match {
                    scrutinee, arms, ..
                } = peel_exception_parens(value)
                {
                    let result = self.emit_match_value_statements(scrutinee, arms)?;
                    write_indent(&mut self.out, 0);
                    self.out.push_str("let ");
                    self.out.push_str(name);
                    self.out.push_str(" = ");
                    self.out.push_str(&result);
                    self.out.push(';');
                    return Ok(());
                }
                write_indent(&mut self.out, 0);
                self.out.push_str("let ");
                self.out.push_str(name);
                self.out.push_str(" = ");
                self.emit_expr(value)?;
                self.out.push_str(";");
            }
            Stmt::UnwrapLet {
                name,
                is_const,
                scrutinee,
                alternative,
                ..
            } => {
                // Lowered to statements, not an IIFE. That is the whole reason
                // this is a binding form: `return` in the alternative has to
                // leave the enclosing function, and a `return` inside an IIFE
                // returns from the IIFE (deka#445).
                //
                //   let name;
                //   { const __u = scrutinee;
                //     if (__u !== undefined) { name = __u; }
                //     else { …alternative… } }
                //
                // `let` even for `const`, because the assignment happens in a
                // branch. Reassignment is rejected by typeck, not by JS.
                let _ = is_const;
                let temp = format!("__deka_unwrap_{}", self.next_unwrap_id());
                write_indent(&mut self.out, 0);
                self.out.push_str("let ");
                self.out.push_str(name);
                self.out.push_str(";\n");
                write_indent(&mut self.out, 0);
                self.out.push_str("{\n");
                self.out.push_str(&format!("const {temp} = "));
                self.emit_expr(scrutinee)?;
                self.out.push_str(";\n");
                if self.exception_forms.option_values.contains(&(scrutinee as *const _)) {
                    self.out.push_str(&format!("if ({temp} !== undefined) {{ {name} = {temp}; }} else {{\n"));
                } else {
                    self.out.push_str(&format!("if ({temp}.ok === true) {{ {name} = {temp}.value; }} else {{\n"));
                }
                match alternative {
                    deka_syntax::UnwrapAlternative::Block(stmts) => {
                        for inner in stmts.iter() {
                            self.emit_stmt_in_unwrap(inner, name)?;
                        }
                    }
                    deka_syntax::UnwrapAlternative::Match(arms) => {
                        // The arms match the original value, so their tests and
                        // bindings are the ordinary ones against the temporary.
                        // The success arm is already handled above.
                        for (index, arm) in arms.iter().enumerate() {
                            let condition = self.match_condition(&arm.pattern, &temp);
                            if index > 0 {
                                self.out.push_str("else ");
                            }
                            self.out.push_str(&format!("if ({condition}) {{\n"));
                            self.emit_pattern_bindings(&arm.pattern, &temp, 0)?;
                            self.emit_handling_arm(arm, Some(name), Some(&temp))?;
                            self.out.push_str("}\n");
                        }
                        if !arms.is_empty() {
                            self.out
                                .push_str("else { throw new Error(\"non-exhaustive match\"); }\n");
                        }
                    }
                }
                self.out.push_str("}\n}");
            }
            Stmt::Function {
                name,
                params,
                body,
                is_async,
                ..
            } => {
                write_indent(&mut self.out, 0);
                if *is_async {
                    self.out.push_str("async function ");
                } else {
                    self.out.push_str("function ");
                }
                self.out.push_str(name);
                self.out.push('(');
                for (i, param) in params.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.emit_param(
                        param,
                        !is_async && !params.iter().any(default_needs_lifting),
                    )?;
                }
                self.out.push_str(") {\n");
                if *is_async || params.iter().any(default_needs_lifting) {
                    self.emit_default_param_assignments(params, 1)?;
                }
                self.with_fn_scope(name, |s| {
                    s.emit_body(body)?;
                    Ok(())
                })?;
                write_indent(&mut self.out, 0);
                self.out.push('}');
            }
            Stmt::Expr { expr, .. } => {
                if self.exception_form(expr) == Some(deka_syntax::typeck::ExceptionEmit::Throw) {
                    return self.emit_raise(expr);
                }
                write_indent(&mut self.out, 0);
                if let Expr::Match {
                    scrutinee, arms, ..
                } = peel_exception_parens(expr)
                {
                    self.emit_match_statements(scrutinee, arms, None)?;
                } else {
                    self.emit_expr(expr)?;
                    self.out.push_str(";");
                }
            }
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    if self.exception_form(value) == Some(deka_syntax::typeck::ExceptionEmit::Throw)
                    {
                        self.emit_raise(value)?;
                        return Ok(());
                    }
                }
                if let Some(Expr::Match {
                    scrutinee, arms, ..
                }) = value.as_ref().map(peel_exception_parens)
                {
                    let result = self.emit_match_value_statements(scrutinee, arms)?;
                    write_indent(&mut self.out, 0);
                    self.out.push_str("return ");
                    self.out.push_str(&result);
                    self.out.push(';');
                    return Ok(());
                }
                write_indent(&mut self.out, 0);
                self.out.push_str("return");
                if let Some(value) = value {
                    self.out.push(' ');
                    self.emit_expr(value)?;
                }
                self.out.push_str(";");
            }
            Stmt::Export { decl, .. } => {
                if let ExportDecl::NamedGroup { names, source } = decl {
                    if names.iter().all(|n| self.is_erased_export(n.name, *source)) {
                        return Ok(());
                    }
                }
                write_indent(&mut self.out, 0);
                self.out.push_str("export ");
                match decl {
                    ExportDecl::Const { name, value, .. } => {
                        self.out.push_str("const ");
                        self.out.push_str(name);
                        self.out.push_str(" = ");
                        self.emit_expr(value)?;
                        self.out.push_str(";");
                    }
                    ExportDecl::Function {
                        name,
                        params,
                        body,
                        is_async,
                        ..
                    } => {
                        if *is_async {
                            self.out.push_str("async function ");
                        } else {
                            self.out.push_str("function ");
                        }
                        self.out.push_str(name);
                        self.out.push('(');
                        for (i, param) in params.iter().enumerate() {
                            if i > 0 {
                                self.out.push_str(", ");
                            }
                            self.emit_param(
                                param,
                                !is_async && !params.iter().any(default_needs_lifting),
                            )?;
                        }
                        self.out.push_str(") {\n");
                        if *is_async || params.iter().any(default_needs_lifting) {
                            self.emit_default_param_assignments(params, 1)?;
                        }
                        self.with_fn_scope(name, |s| {
                            s.emit_body(body)?;
                            Ok(())
                        })?;
                        write_indent(&mut self.out, 0);
                        self.out.push('}');
                    }
                    ExportDecl::NamedGroup { names, source } => {
                        let kept: Vec<_> = names
                            .iter()
                            .filter(|n| {
                                !self.is_erased_export(n.name, *source)
                                    && (self.is_live(n.alias.unwrap_or(n.name))
                                        || self.is_live(n.name))
                            })
                            .collect();
                        if kept.is_empty() {
                            return Ok(());
                        }
                        self.out.push_str("{ ");
                        for (i, name) in kept.iter().enumerate() {
                            if i > 0 {
                                self.out.push_str(", ");
                            }
                            self.out.push_str(name.name);
                            if let Some(alias) = name.alias {
                                self.out.push_str(" as ");
                                self.out.push_str(alias);
                            }
                        }
                        self.out.push_str(" }");
                        if let Some(source) = source {
                            self.out.push_str(" from \"");
                            self.out.push_str(&self.resolve_module_source(source));
                            self.out.push('"');
                        }
                        self.out.push(';');
                    }
                }
            }
            Stmt::Import {
                specifiers, source, ..
            } => {
                if source_is_css(source) && specifiers.is_empty() {
                    // Side-effect `import "./x.css"` is collected per-route as <link>.
                    // Keep specifier imports (CSS modules) so the bundler can resolve them.
                    return Ok(());
                }
                // `math` is a compiler-owned, closed stdlib module. Its
                // bindings lower locally, keeping `Math` out of DekaScript's
                // value namespace while still using the JavaScript constant
                // as the implementation detail.
                if source_is_math(source) {
                    let kept: Vec<_> = specifiers
                        .iter()
                        .filter(|spec| {
                            !self.is_erased_binding(spec.local)
                                && (self.is_live(spec.local) || self.is_build_factory_import(spec))
                        })
                        .collect();
                    for (index, spec) in kept.iter().enumerate() {
                        if index > 0 {
                            self.out.push('\n');
                        }
                        write_indent(&mut self.out, 0);
                        self.out.push_str("const ");
                        self.out.push_str(spec.local);
                        self.out.push_str(" = Math.PI;");
                    }
                    return Ok(());
                }
                write_indent(&mut self.out, 0);
                let resolved_source = self.resolve_module_source(source);
                if specifiers.is_empty() {
                    self.out.push_str("import \"");
                    self.out.push_str(&resolved_source);
                    self.out.push_str("\";");
                } else {
                    let kept: Vec<_> = specifiers
                        .iter()
                        .filter(|spec| {
                            !self.is_erased_binding(spec.local)
                                && (self.is_live(spec.local) || self.is_build_factory_import(spec))
                        })
                        .collect();
                    if kept.is_empty() {
                        return Ok(());
                    }
                    self.out.push_str("import { ");
                    for (i, spec) in kept.iter().enumerate() {
                        if i > 0 {
                            self.out.push_str(", ");
                        }
                        if spec.imported == spec.local {
                            self.out.push_str(spec.imported);
                        } else {
                            self.out.push_str(spec.imported);
                            self.out.push_str(" as ");
                            self.out.push_str(spec.local);
                        }
                    }
                    self.out.push_str(" } from \"");
                    self.out.push_str(&resolved_source);
                    self.out.push_str("\";");
                }
            }
            Stmt::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                write_indent(&mut self.out, 0);
                self.out.push_str("if (");
                self.emit_expr(condition)?;
                self.out.push_str(") {\n");
                for stmt in then_body.iter() {
                    self.emit_stmt(stmt)?;
                    self.out.push('\n');
                }
                write_indent(&mut self.out, 0);
                self.out.push('}');
                if !else_body.is_empty() {
                    self.out.push_str(" else {\n");
                    for stmt in else_body.iter() {
                        self.emit_stmt(stmt)?;
                        self.out.push('\n');
                    }
                    write_indent(&mut self.out, 0);
                    self.out.push('}');
                }
            }
            Stmt::Block { body, .. } => {
                write_indent(&mut self.out, 0);
                self.out.push_str("{\n");
                self.emit_body(body)?;
                write_indent(&mut self.out, 0);
                self.out.push('}');
            }
            Stmt::For {
                init,
                condition,
                step,
                body,
                ..
            } => {
                if condition.as_ref().is_some_and(needs_lifting)
                    || step.as_ref().is_some_and(needs_lifting)
                    || init.as_ref().is_some_and(|init| match init {
                        ForInit::Const { value, .. }
                        | ForInit::Let { value, .. }
                        | ForInit::Expr(value) => needs_lifting(value),
                    })
                {
                    return self.emit_lifted_for(
                        init.as_ref(),
                        condition.as_ref(),
                        step.as_ref(),
                        body,
                    );
                }
                write_indent(&mut self.out, 0);
                self.out.push_str("for (");
                if let Some(init) = init {
                    self.emit_for_init(init)?;
                }
                self.out.push_str("; ");
                if let Some(condition) = condition {
                    self.emit_expr(condition)?;
                }
                self.out.push_str("; ");
                if let Some(step) = step {
                    self.emit_expr(step)?;
                }
                self.out.push_str(") {\n");
                self.emit_body(body)?;
                write_indent(&mut self.out, 0);
                self.out.push('}');
            }
            Stmt::ForOf {
                name,
                is_const,
                iterable,
                body,
                ..
            } => {
                write_indent(&mut self.out, 0);
                self.out.push_str("for (");
                if *is_const {
                    self.out.push_str("const ");
                } else {
                    self.out.push_str("let ");
                }
                self.out.push_str(name);
                self.out.push_str(" of ");
                self.emit_expr(iterable)?;
                self.out.push_str(") {\n");
                self.emit_body(body)?;
                write_indent(&mut self.out, 0);
                self.out.push('}');
            }
            Stmt::Struct { name, embeds, .. } => {
                write_indent(&mut self.out, 0);
                self.out.push_str("const ");
                self.out.push_str(name);
                self.out.push_str(" = __deka_struct(");
                self.out.push_str(&json_string(name));
                if !embeds.is_empty() {
                    self.out.push_str(", { ");
                    for (i, embed) in embeds.iter().enumerate() {
                        if i > 0 {
                            self.out.push_str(", ");
                        }
                        self.out.push_str(embed.name);
                        self.out.push_str(": ");
                        self.out.push_str(embed.name);
                    }
                    self.out.push_str(" }");
                }
                self.out.push_str(");");
            }
            Stmt::Enum { name, cases, .. } => {
                self.emit_enum_object(name, cases)?;
            }
            Stmt::Summon {
                functions, source, ..
            } => {
                self.out.push_str("import { ");
                for (i, f) in functions.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(f.name);
                }
                self.out.push_str(" } from ");
                self.out.push_str(&json_string(source));
                self.out.push(';');
            }
            Stmt::Opaque { .. } | Stmt::TypeAlias { .. } => {
                // Erased at runtime.
            }
            Stmt::Newtype { name, repr, .. } => {
                self.emit_newtype_factory(name, *repr)?;
            }
            Stmt::Interface { .. } => {
                // Erased at runtime.
            }
            Stmt::Break { .. } => {
                write_indent(&mut self.out, 0);
                self.out.push_str("break;");
            }
            Stmt::Continue { .. } => {
                write_indent(&mut self.out, 0);
                self.out.push_str("continue;");
            }
            Stmt::ReceiverMethod { .. } => {
                // Collected in the pre-pass and emitted after all struct
                // factories have been declared.
            }
            Stmt::Empty { .. } => {
                // No output.
            }
        }
        Ok(())
    }

    fn emit_enum_object(
        &mut self,
        name: &str,
        cases: &[deka_syntax::EnumCase<'a>],
    ) -> Result<(), String> {
        write_indent(&mut self.out, 0);
        self.out.push_str("const ");
        self.out.push_str(name);
        self.out.push_str(" = Object.freeze({\n");
        for (i, case) in cases.iter().enumerate() {
            write_indent(&mut self.out, 2);
            self.out.push_str(&case.name);
            if let Some(payload_ty) = &case.payload {
                self.out.push_str("(value) { return Object.freeze({ ");
                self.out.push_str("__enum: ");
                self.out.push_str(&json_string(name));
                self.out.push_str(", __case: ");
                self.out.push_str(&json_string(&case.name));
                self.out.push_str(", name: ");
                self.out.push_str(&json_string(&case.name));
                self.out.push_str(", value }); }");
                let _ = payload_ty; // type-only, no runtime effect
            } else {
                self.out.push_str(": Object.freeze({ ");
                self.out.push_str("__enum: ");
                self.out.push_str(&json_string(name));
                self.out.push_str(", __case: ");
                self.out.push_str(&json_string(&case.name));
                self.out.push_str(", name: ");
                self.out.push_str(&json_string(&case.name));
                self.out.push_str(" })");
            }
            if i + 1 < cases.len() {
                self.out.push(',');
            }
            self.out.push('\n');
        }
        write_indent(&mut self.out, 0);
        self.out.push_str("});");
        Ok(())
    }

    fn emit_newtype_factory(&mut self, name: &str, _repr: NewtypeRepr) -> Result<(), String> {
        write_indent(&mut self.out, 0);
        self.out.push_str("const ");
        self.out.push_str(name);
        self.out.push_str("$values = new WeakMap();\n");
        write_indent(&mut self.out, 0);
        self.out.push_str("const ");
        self.out.push_str(name);
        self.out.push_str("$proto = Object.create(null);\n");
        write_indent(&mut self.out, 0);
        self.out.push_str("Object.defineProperty(");
        self.out.push_str(name);
        self.out.push_str("$proto, '__deka_newtype', { value: ");
        self.out.push_str(&json_string(name));
        self.out
            .push_str(", enumerable: false, writable: false, configurable: false });\n");
        write_indent(&mut self.out, 0);
        self.out.push_str("Object.defineProperty(");
        self.out.push_str(name);
        self.out.push_str("$proto, __p, { get() { return ");
        self.out.push_str(name);
        self.out
            .push_str("$values.get(this); }, enumerable: false, configurable: false });\n");
        write_indent(&mut self.out, 0);
        self.out.push_str(name);
        self.out
            .push_str("$proto.toJSON = function () { return this[__p]; };\n");
        write_indent(&mut self.out, 0);
        self.out.push_str("function ");
        self.out.push_str(name);
        self.out.push_str("(v) { const o = Object.create(");
        self.out.push_str(name);
        self.out.push_str("$proto); ");
        self.out.push_str(name);
        self.out.push_str("$values.set(o, v); return o; }");
        Ok(())
    }

    fn collect_methods_for_struct(
        &self,
        struct_name: &str,
        visited: &mut HashSet<String>,
    ) -> Vec<ReceiverMethod<'a>> {
        if !visited.insert(struct_name.to_string()) {
            return Vec::new();
        }
        let mut methods = Vec::new();
        if let Some(meta) = self.structs.get(struct_name) {
            let embeds = meta.embeds.clone();
            for embed in embeds {
                methods.extend(self.collect_methods_for_struct(&embed, visited));
            }
        }
        if let Some(own) = self.receiver_methods.get(struct_name) {
            methods.extend(own.iter().cloned());
        }
        methods
    }

    fn emit_method_registrations(&mut self) -> Result<(), String> {
        let order = self.struct_order.clone();
        for struct_name in order {
            if !self.is_live(&struct_name) && !self.is_build_retained(&struct_name) {
                continue;
            }
            let methods = self.collect_methods_for_struct(&struct_name, &mut HashSet::new());
            for method in methods {
                write_indent(&mut self.out, 0);
                self.out.push_str(&struct_name);
                self.out.push_str(if method.receiver_mutable {
                    ".implMut("
                } else {
                    ".impl("
                });
                self.out.push_str(&json_string(&method.name));
                self.out.push_str(", ");
                if method.is_async {
                    self.out.push_str("async ");
                }
                self.out.push_str("function(");
                for (i, param) in method.params.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(param);
                }
                self.out.push_str(") {\n");
                write_indent(&mut self.out, 2);
                self.out.push_str("const ");
                self.out.push_str(&method.receiver_name);
                self.out.push_str(" = this;\n");
                for stmt in method.body.iter() {
                    self.emit_stmt(stmt)?;
                    self.out.push('\n');
                }
                write_indent(&mut self.out, 0);
                self.out.push_str("});\n");
            }
        }

        // Newtype receiver methods are installed directly on the newtype
        // factory's prototype object.
        let newtype_methods: Vec<(String, Vec<ReceiverMethod<'a>>)> = self
            .newtypes
            .keys()
            .filter_map(|name| {
                if !self.is_live(name) && !self.is_build_retained(name) {
                    return None;
                }
                self.receiver_methods
                    .get(name)
                    .map(|methods| (name.clone(), methods.clone()))
            })
            .collect();
        for (newtype_name, methods) in newtype_methods {
            for method in methods {
                write_indent(&mut self.out, 0);
                self.out.push_str(&newtype_name);
                self.out.push_str("$proto.");
                self.out.push_str(&method.name);
                self.out.push_str(" = ");
                if method.is_async {
                    self.out.push_str("async ");
                }
                self.out.push_str("function(");
                for (i, param) in method.params.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(param);
                }
                self.out.push_str(") {\n");
                write_indent(&mut self.out, 2);
                self.out.push_str("const ");
                self.out.push_str(&method.receiver_name);
                self.out.push_str(" = this;\n");
                for stmt in method.body.iter() {
                    self.emit_stmt(stmt)?;
                    self.out.push('\n');
                }
                write_indent(&mut self.out, 0);
                self.out.push_str("};\n");
            }
        }

        // Primitive receiver methods cannot hang off a prototype (primitives
        // cannot be branded), so they are emitted as module-local free
        // functions named `method$receiver`; call sites are rewritten to
        // match (deka#527). Unused extensions are dropped by the same
        // liveness gate as everything else.
        let primitive_methods: Vec<(String, Vec<ReceiverMethod<'a>>)> = self
            .receiver_methods
            .iter()
            .filter(|(receiver_type, _)| {
                is_primitive_receiver(receiver_type) || self.opaques.contains(*receiver_type)
            })
            .map(|(receiver_type, methods)| (receiver_type.clone(), methods.clone()))
            .collect();
        for (receiver_type, methods) in primitive_methods {
            for method in methods {
                let mangled = format!("{}${}", method.name, receiver_type);
                if !self.is_live(&mangled) && !self.opaques.contains(&receiver_type) {
                    continue;
                }
                write_indent(&mut self.out, 0);
                if method.is_async {
                    self.out.push_str("async ");
                }
                self.out.push_str("function ");
                self.out.push_str(&mangled);
                self.out.push('(');
                self.out.push_str(&method.receiver_name);
                for param in method.params.iter() {
                    self.out.push_str(", ");
                    self.out.push_str(param);
                }
                self.out.push_str(") {\n");
                for stmt in method.body.iter() {
                    self.emit_stmt(stmt)?;
                    self.out.push('\n');
                }
                write_indent(&mut self.out, 0);
                self.out.push_str("}\n");
            }
        }
        Ok(())
    }

    fn emit_newtype_binary(
        &mut self,
        rewrite: &deka_syntax::typeck::OperatorRewrite<'a>,
        op: BinOp,
        left: &Expr<'a>,
        right: &Expr<'a>,
    ) -> Result<(), String> {
        use deka_syntax::typeck::{NewtypeSide, OperatorRewrite};
        match rewrite {
            OperatorRewrite::NewtypeBinary { name } => {
                self.out.push_str(name);
                self.out.push_str("(");
                self.out.push_str("(");
                self.emit_expr(left)?;
                self.out.push_str("[__p]");
                self.out.push(' ');
                self.out.push_str(bin_op_str(op));
                self.out.push(' ');
                self.emit_expr(right)?;
                self.out.push_str("[__p]");
                self.out.push_str("))");
            }
            OperatorRewrite::NewtypeDiv => {
                self.out.push_str("(");
                self.emit_expr(left)?;
                self.out.push_str("[__p]");
                self.out.push(' ');
                self.out.push_str(bin_op_str(op));
                self.out.push(' ');
                self.emit_expr(right)?;
                self.out.push_str("[__p])");
            }
            OperatorRewrite::NewtypeScalar { name, side } => {
                self.out.push_str(name);
                self.out.push_str("(");
                self.out.push_str("(");
                match side {
                    NewtypeSide::Left => {
                        self.emit_expr(left)?;
                        self.out.push_str("[__p]");
                        self.out.push(' ');
                        self.out.push_str(bin_op_str(op));
                        self.out.push(' ');
                        self.emit_expr(right)?;
                    }
                    NewtypeSide::Right => {
                        self.emit_expr(left)?;
                        self.out.push(' ');
                        self.out.push_str(bin_op_str(op));
                        self.out.push(' ');
                        self.emit_expr(right)?;
                        self.out.push_str("[__p]");
                    }
                }
                self.out.push_str("))");
            }
            OperatorRewrite::NewtypeCompare => {
                self.out.push_str("(");
                self.emit_expr(left)?;
                self.out.push_str("[__p]");
                self.out.push(' ');
                self.out.push_str(bin_op_str(op));
                self.out.push(' ');
                self.emit_expr(right)?;
                self.out.push_str("[__p])");
            }
            _ => {
                return Err(format!("unexpected unary rewrite for binary expression"));
            }
        }
        Ok(())
    }

    fn emit_newtype_unary(
        &mut self,
        rewrite: &deka_syntax::typeck::OperatorRewrite<'a>,
        op: deka_syntax::UnOp,
        operand: &Expr<'a>,
    ) -> Result<(), String> {
        use deka_syntax::typeck::OperatorRewrite;
        match rewrite {
            OperatorRewrite::NewtypeUnary { name } => {
                self.out.push_str(name);
                self.out.push_str("((");
                self.out.push_str(un_op_str(op));
                self.emit_expr(operand)?;
                self.out.push_str("[__p]))");
            }
            _ => {
                return Err(format!("unexpected binary rewrite for unary expression"));
            }
        }
        Ok(())
    }

    fn emit_for_init(&mut self, init: &ForInit<'a>) -> Result<(), String> {
        match init {
            ForInit::Const { name, value } => {
                self.out.push_str("const ");
                self.out.push_str(name);
                self.out.push_str(" = ");
                self.emit_expr(value)?;
            }
            ForInit::Let { name, value } => {
                self.out.push_str("let ");
                self.out.push_str(name);
                self.out.push_str(" = ");
                self.emit_expr(value)?;
            }
            ForInit::Expr(expr) => {
                self.emit_expr(expr)?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Expressions
    // ------------------------------------------------------------------
    fn emit_expr_to_string(&mut self, expr: &Expr<'a>) -> Result<String, String> {
        let mut tmp = String::new();
        std::mem::swap(&mut self.out, &mut tmp);
        let res = self.emit_expr(expr);
        std::mem::swap(&mut self.out, &mut tmp);
        res?;
        Ok(tmp)
    }

    /// A counter so nested unwraps do not collide on the temporary name.
    fn next_unwrap_id(&mut self) -> usize {
        self.unwrap_id += 1;
        self.unwrap_id
    }

    fn next_match_id(&mut self) -> usize {
        self.match_id += 1;
        self.match_id
    }

    /// Emit one statement of an `or { … }` block.
    ///
    /// A trailing expression statement is the block's value and is assigned to
    /// the binding; everything else is emitted as-is, so a `return` in there
    /// leaves the enclosing function.
    fn emit_stmt_in_unwrap(&mut self, stmt: &'a Stmt<'a>, binding: &str) -> Result<(), String> {
        if let Stmt::Expr { expr, .. } = stmt {
            let value = self.lift_value(expr)?;
            self.out.push_str(&format!("{binding} = {value}"));
            self.out.push_str(";\n");
            return Ok(());
        }
        self.emit_stmt(stmt)?;
        self.out.push('\n');
        Ok(())
    }

    fn emit_expr(&mut self, expr: &Expr<'a>) -> Result<(), String> {
        if let Some(value) = self.lifted_values.get(&(expr as *const _)) {
            self.out.push_str(value);
            return Ok(());
        }
        use deka_syntax::typeck::ExceptionEmit;
        match self.exception_forms.get(&(expr as *const _)).copied() {
            Some(ExceptionEmit::Ok) => {
                if let Expr::EnumConstructor {
                    payload: Some(payload),
                    ..
                } = expr
                {
                    return self.emit_expr(payload);
                }
            }
            Some(ExceptionEmit::Throw) => {
                self.out.push_str("(() => { ");
                self.emit_raise(expr)?;
                self.out.push_str(" })()");
                return Ok(());
            }
            Some(ExceptionEmit::ToResult) => {
                if let Expr::Call {
                    callee: Expr::FieldAccess { object, .. },
                    ..
                } = expr
                {
                    let asynchronous = expr_contains_await(object);
                    self.out.push_str(if asynchronous {
                        "(await (async () => { try { return Ok("
                    } else {
                        "(() => { try { return Ok("
                    });
                    self.emit_expr(object)?;
                    self.out.push_str("); } catch (e) { if (e?.[Symbol.for(\"deka.BoundaryError\")]) throw e; return Err(e); } })()");
                    if asynchronous {
                        self.out.push(')');
                    }
                    return Ok(());
                }
            }
            Some(ExceptionEmit::FromResult) => {
                if let Expr::Call { args, .. } = expr {
                    self.out.push_str(
                        "((r) => { if (r.ok === false) { throw r.error; } return r.value; })(",
                    );
                    self.emit_expr(&args[0])?;
                    self.out.push(')');
                    return Ok(());
                }
            }
            _ => {}
        }
        match expr {
            Expr::Number { value, .. } => {
                if value.is_nan() {
                    self.out.push_str("NaN");
                } else if value.is_infinite() {
                    if value.is_sign_negative() {
                        self.out.push_str("-Infinity");
                    } else {
                        self.out.push_str("Infinity");
                    }
                } else if *value == 0.0 && value.is_sign_negative() {
                    self.out.push_str("-0");
                } else {
                    self.out.push_str(&format!("{}", value));
                }
            }
            Expr::BigInt { value, .. } => {
                self.out.push_str(value);
                self.out.push('n');
            }
            Expr::String { value, .. } => {
                self.out.push('"');
                self.out.push_str(&escape_string(value));
                self.out.push('"');
            }
            Expr::Boolean { value, .. } => {
                self.out.push_str(if *value { "true" } else { "false" });
            }
            Expr::None { .. } => {
                // Undefined is the reserved empty Option representation (rfd#62).
                self.out.push_str("undefined");
            }
            Expr::Identifier { name, .. } => {
                self.out.push_str(name);
            }
            Expr::Binary {
                op, left, right, ..
            } => {
                let expr_ptr = expr as *const Expr<'a>;

                let rewrite = self.operator_rewrites.get(&expr_ptr).copied();
                if let Some(rewrite) = rewrite {
                    self.emit_newtype_binary(&rewrite, *op, left, right)?;
                    return Ok(());
                }
                if *op == BinOp::Pipe {
                    // Desugar pipe into a call. The left-hand value becomes
                    // argument 0 unless the right-hand call contains a hole.
                    match right {
                        Expr::Call { callee, args, .. }
                            if !args.iter().any(|a| is_hole_expr(a)) =>
                        {
                            self.emit_expr(callee)?;
                            self.out.push('(');
                            self.emit_expr(left)?;
                            for arg in args.iter() {
                                self.out.push_str(", ");
                                self.emit_expr(arg)?;
                            }
                            self.out.push(')');
                        }
                        _ => {
                            self.out.push('(');
                            self.emit_expr(right)?;
                            self.out.push_str(")(");
                            self.emit_expr(left)?;
                            self.out.push(')');
                        }
                    }
                } else {
                    self.emit_expr(left)?;
                    self.out.push(' ');
                    self.out.push_str(bin_op_str(*op));
                    self.out.push(' ');
                    self.emit_expr(right)?;
                }
            }
            Expr::Unary { op, operand, .. } => {
                let expr_ptr = expr as *const Expr<'a>;
                let rewrite = self.operator_rewrites.get(&expr_ptr).copied();
                if let Some(rewrite) = rewrite {
                    self.emit_newtype_unary(&rewrite, *op, operand)?;
                    return Ok(());
                }
                self.out.push_str(un_op_str(*op));
                self.emit_expr(operand)?;
            }
            Expr::Call {
                callee, args, span, ..
            } => {
                let expr_ptr = expr as *const Expr<'a>;
                if let Some((_params, ret, asynchronous)) =
                    self.exception_forms.summons.get(&expr_ptr).cloned()
                {
                    use deka_syntax::typeck::Type;
                    let void = matches!(ret, Type::Named { name: "void" });
                    let option = matches!(ret, Type::Option { .. });
                    if !void {
                        self.out.push_str(if asynchronous { "(async ($__deka_input) => { const $__deka_foreign = await $__deka_input; " } else { "(($__deka_foreign) => { " });
                        if option {
                            // Foreign nullish values enter the reserved None representation.
                            self.out.push_str("return $__deka_foreign == null ? undefined : $__deka_foreign; })(");
                        } else {
                            self.out.push_str("if ($__deka_foreign == null) { const e = new Error(\"summoned function returned null or undefined\"); e.name = \"BoundaryError\"; e[Symbol.for(\"deka.BoundaryError\")] = true; throw e; } return $__deka_foreign; })(");
                        }
                    }
                    // Evaluate the direct call in the authored frame. In particular,
                    // await inside an argument must not move into a sync IIFE.
                    self.emit_expr(callee)?;
                    self.out.push('(');
                    for (i, arg) in args.iter().enumerate() {
                        if i > 0 {
                            self.out.push_str(", ");
                        }
                        self.emit_expr(arg)?;
                    }
                    self.out.push(')');
                    if !void {
                        self.out.push(')');
                    }
                    return Ok(());
                }
                if let Some(kind) = self.unwrap_calls.get(&expr_ptr) {
                    if let Some(arg) = args.first() {
                        match kind {
                            deka_syntax::typeck::UnwrapKind::Isset => {
                                self.out.push('(');
                                self.emit_expr(arg)?;
                                self.out.push_str(" !== undefined)");
                            }
                            deka_syntax::typeck::UnwrapKind::Identity => {
                                self.emit_expr(arg)?;
                            }
                            deka_syntax::typeck::UnwrapKind::Payload => {
                                self.emit_expr(arg)?;
                                self.out.push_str("[__p]");
                            }
                            deka_syntax::typeck::UnwrapKind::WidenToString => {
                                self.out.push_str("String(");
                                self.emit_expr(arg)?;
                                self.out.push(')');
                            }
                            deka_syntax::typeck::UnwrapKind::WidenToNumber => {
                                self.out.push_str("Number(");
                                self.emit_expr(arg)?;
                                self.out.push(')');
                            }
                            deka_syntax::typeck::UnwrapKind::StringToOptionNumber => {
                                // `parseNumber(s)` on a string can produce
                                // NaN; surface it as Option<number>.
                                self.out.push_str("(() => { const __n = Number(");
                                self.emit_expr(arg)?;
                                self.out.push_str("); return isNaN(__n) ? undefined : __n; })()");
                            }
                        }
                    }
                    return Ok(());
                }

                // Builtin `.getType()`: rewrite `obj.getType()` to the
                // module-local free function `__deka_type_of(obj)` — static
                // dispatch, no prototype mutation, no globalThis (rfd#41,
                // deka#529).
                if self.type_of_calls.contains(&expr_ptr) {
                    self.out.push_str("__deka_type_of(");
                    if let Expr::FieldAccess { object, .. } = &**callee {
                        self.emit_expr(object)?;
                        if self
                            .exception_forms
                            .result_values
                            .contains(&(*object as *const _))
                        {
                            self.out.push_str(", true");
                        } else if self.exception_forms.option_values.contains(&(*object as *const _)) {
                            self.out.push_str(", false, true");
                        }
                    }
                    self.out.push(')');
                    return Ok(());
                }

                if let Some(tree) = self.signature_calls.get(&expr_ptr).cloned() {
                    self.out.push_str(&emit_descriptor_tree(&tree)?);
                    return Ok(());
                }

                // Builtin `Name.type()` on a `super` declaration (rfd#41,
                // deka#561 PR B): rewrite to the interned frozen descriptor
                // const emitted in the prelude — a module-local reference,
                // no prototype mutation, no globalThis.
                if let Some(site) = self.static_type_calls.get(&expr_ptr) {
                    if let Some(tree) = site.tree.as_ref() {
                        if let Some(name) = super_decl_top_name(tree) {
                            self.out.push_str(&super_const_name(name));
                            return Ok(());
                        }
                    }
                }

                if let Some(call) = self.json_calls.get(&expr_ptr).cloned() {
                    let prefix = match call.operation {
                        deka_syntax::typeck::JsonOperation::ToJson => "toJSON$",
                        deka_syntax::typeck::JsonOperation::ParseJson => "parseJSON$",
                    };
                    self.out.push_str(prefix);
                    self.out.push_str(&json_shape_name(&call.shape));
                    self.out.push('(');
                    if let Expr::FieldAccess { object, .. } = &**callee {
                        self.emit_expr(object)?;
                    }
                    self.out.push(')');
                    return Ok(());
                }

                // Builtin `first`/`last`/`pop`/`shift`: emit a real Option
                // construction at the site — `Some`/`None` from the prelude —
                // never a raw JS passthrough. A bare `v.pop()` returns the raw
                // element with no `__case` tag, so unwrap read a present value
                // as absent, and threw `TypeError: Cannot delete property` on
                // frozen (const) arrays (deka#566). The arrow IIFE evaluates
                // the receiver once, is expression-position safe, and the
                // length guard turns empty-array pop/shift into `None`
                // instead of `Some(undefined)`.
                if let Some(kind) = self.array_builtin_calls.get(&expr_ptr) {
                    if *kind == deka_syntax::typeck::ArrayAccess::Has {
                        self.emit_array_has(expr, true)?;
                        return Ok(());
                    }
                    let produce = match kind {
                        deka_syntax::typeck::ArrayAccess::Has => unreachable!(),
                        deka_syntax::typeck::ArrayAccess::First => "v[0]",
                        deka_syntax::typeck::ArrayAccess::Last => "v[v.length - 1]",
                        deka_syntax::typeck::ArrayAccess::Pop => "v.pop()",
                        deka_syntax::typeck::ArrayAccess::Shift => "v.shift()",
                    };
                    self.out.push_str("((v) => v.length > 0 ? ");
                    self.out.push_str(produce);
                    self.out.push_str(" : undefined)(");
                    if let Expr::FieldAccess { object, .. } = &**callee {
                        self.emit_expr(object)?;
                    }
                    self.out.push(')');
                    return Ok(());
                }

                // Builtin Math-backed methods on `number` (deka#378 step 2,
                // rfd#40 phase 2): JS numbers have no such methods, so
                // verbatim passthrough would be a runtime lie — rewrite to
                // `Math.<name>(recv, args)`. The method names in the
                // typechecker's table are exactly the `Math` member names.
                // Partial functions answer `None` exactly where JS would
                // hand back `NaN` — a value of type `number` that is not a
                // number (rfd#13). `Infinity` is a legitimate IEEE-754
                // value and passes through as `Some`. The arrow IIFE
                // evaluates the receiver and arguments exactly once.
                if let Some(kind) = self.number_math_calls.get(&expr_ptr) {
                    if let Expr::FieldAccess { object, field, .. } = &**callee {
                        let partial = matches!(kind, deka_syntax::typeck::NumberMath::Partial);
                        if partial {
                            self.out.push_str("((v) => isNaN(v) ? undefined : v)(");
                        }
                        self.out.push_str("Math.");
                        self.out.push_str(field);
                        self.out.push('(');
                        self.emit_expr(object)?;
                        for arg in args.iter() {
                            self.out.push_str(", ");
                            self.emit_expr(arg)?;
                        }
                        self.out.push(')');
                        if partial {
                            self.out.push(')');
                        }
                    }
                    return Ok(());
                }

                // Primitive extension call: rewrite `obj.method(args)` to the
                // module-local free function `method$receiver(obj, args)`.
                // Primitives cannot be branded with a prototype, so static
                // dispatch is the only option (deka#527).
                if let Some(target) = self.method_calls.get(&expr_ptr) {
                    self.out.push_str(&target.mangled);
                    self.out.push('(');
                    let mut first = true;
                    if let Expr::FieldAccess { object, .. } = &**callee {
                        self.emit_expr(object)?;
                        first = false;
                    }
                    for arg in args.iter() {
                        if !first {
                            self.out.push_str(", ");
                        }
                        first = false;
                        self.emit_expr(arg)?;
                    }
                    self.out.push(')');
                    return Ok(());
                }

                if is_panic_callee(callee) {
                    self.out.push_str("(() => { throw new Error(String(");
                    if let Some(arg) = args.first() {
                        self.emit_expr(arg)?;
                    } else {
                        self.out.push_str("\"panic\"");
                    }
                    self.out.push_str(")); })()");
                    return Ok(());
                }

                if let Expr::Identifier { name, .. } = callee {
                    if self.newtypes.contains_key(*name) {
                        self.out.push_str(name);
                        self.out.push('(');
                        for (i, arg) in args.iter().enumerate() {
                            if i > 0 {
                                self.out.push_str(", ");
                            }
                            self.emit_expr(arg)?;
                        }
                        self.out.push(')');
                        return Ok(());
                    }
                }

                let hole_count = args.iter().filter(|a| is_hole_expr(a)).count();
                if hole_count > 0 {
                    // Partial application: emit a wrapper function.
                    self.out.push('(');
                    for i in 0..hole_count {
                        if i > 0 {
                            self.out.push_str(", ");
                        }
                        self.out.push_str("__deka_hole_");
                        self.out.push_str(&i.to_string());
                    }
                    self.out.push_str(") => ");
                    self.emit_expr(callee)?;
                    self.out.push('(');
                    let mut hole_idx = 0;
                    for (i, arg) in args.iter().enumerate() {
                        if i > 0 {
                            self.out.push_str(", ");
                        }
                        if is_hole_expr(arg) {
                            self.out.push_str("__deka_hole_");
                            self.out.push_str(&hole_idx.to_string());
                            hole_idx += 1;
                        } else {
                            self.emit_expr(arg)?;
                        }
                    }
                    self.out.push(')');
                } else {
                    self.emit_expr(callee)?;
                    self.out.push('(');
                    for (i, arg) in args.iter().enumerate() {
                        if i > 0 {
                            self.out.push_str(", ");
                        }
                        self.emit_expr(arg)?;
                    }
                    self.out.push(')');
                }
                let _ = span;
            }
            Expr::FieldAccess { object, field, .. } => {
                self.emit_expr(object)?;
                self.out.push('.');
                self.out.push_str(field);
            }
            Expr::StructLiteral { name, fields, .. } => {
                self.emit_struct_literal(name, fields)?;
            }
            Expr::IndexAccess { object, index, .. } => {
                self.emit_expr(object)?;
                self.out.push('[');
                self.emit_expr(index)?;
                self.out.push(']');
            }
            Expr::Safe { expr, .. } | Expr::Paren { expr, .. } => {
                self.out.push('(');
                self.emit_expr(expr)?;
                self.out.push(')');
            }
            Expr::Array { elements, .. } => {
                self.out.push('[');
                for (i, element) in elements.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.emit_expr(element)?;
                }
                self.out.push(']');
            }
            Expr::Object { fields, .. } => {
                self.out.push_str("{");
                for (i, field) in fields.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    if field.key.is_empty() {
                        self.out.push_str("...");
                        self.emit_expr(&field.value)?;
                    } else {
                        let key = if is_js_identifier(field.key) {
                            field.key.to_string()
                        } else {
                            json_string(field.key)
                        };
                        self.out.push_str(&key);
                        self.out.push_str(": ");
                        self.emit_expr(&field.value)?;
                    }
                }
                self.out.push_str("}");
            }
            Expr::Spread { expr, .. } => {
                self.out.push_str("...");
                self.emit_expr(expr)?;
            }
            Expr::Unsafe {
                source, result_type, ..
            } => {
                self.emit_unsafe(source, result_type.is_none())?;
            }
            // Valid dev blocks are replaced at their enclosing top-level
            // binding. This fallback prevents an invalid nested form from
            // leaking build-only code into the runtime module.
            Expr::Build { .. } => self.out.push_str("undefined"),
            Expr::Bridge {
                kind, action, args, ..
            } => {
                self.emit_bridge(kind, action, args)?;
            }
            Expr::Ternary {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                if self.array_builtin_calls.get(&(*condition as *const _))
                    == Some(&deka_syntax::typeck::ArrayAccess::Has)
                {
                    self.emit_array_has(condition, false)?;
                } else {
                    self.emit_expr(condition)?;
                }
                self.out.push_str(" ? ");
                self.emit_expr(then_branch)?;
                self.out.push_str(" : ");
                self.emit_expr(else_branch)?;
            }
            Expr::EnumConstructor {
                enum_name,
                case_name,
                payload,
                ..
            } => {
                if *enum_name == "Option" {
                    if let Some(payload) = payload {
                        self.out.push('(');
                        self.emit_expr(payload)?;
                        self.out.push(')');
                    } else {
                        self.out.push_str("undefined");
                    }
                    return Ok(());
                }
                if *enum_name == "Result" { self.uses_prelude_enums = true; }
                self.out.push_str(enum_name);
                self.out.push('.');
                self.out.push_str(case_name);
                if let Some(payload) = payload {
                    self.out.push('(');
                    self.emit_expr(payload)?;
                    self.out.push(')');
                }
            }
            Expr::Match {
                scrutinee, arms, ..
            } => {
                self.emit_match(scrutinee, arms)?;
            }
            Expr::Await { expr, .. } => {
                self.out.push_str("await ");
                self.emit_expr(expr)?;
            }
            Expr::JsxElement { element, .. } => {
                self.emit_jsx_element(element)?;
            }
            Expr::JsxFragment { children, .. } => {
                self.emit_jsx_fragment(children)?;
            }
            Expr::JsxText { value, .. } => {
                self.out.push('"');
                self.out.push_str(&escape_string(value));
                self.out.push('"');
            }
            Expr::TemplateLiteral { parts, .. } => {
                self.out.push('`');
                for part in parts.iter() {
                    match part {
                        deka_syntax::TemplatePart::Text(text) => self.out.push_str(text),
                        deka_syntax::TemplatePart::Expr(expr) => {
                            self.out.push_str("${");
                            self.emit_expr(expr)?;
                            self.out.push('}');
                        }
                    }
                }
                self.out.push('`');
            }
            Expr::Function {
                params,
                body,
                is_async,
                ..
            } => {
                if *is_async {
                    self.out.push_str("async function(");
                } else {
                    self.out.push_str("function(");
                }
                for (i, param) in params.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.emit_param(
                        param,
                        !is_async && !params.iter().any(default_needs_lifting),
                    )?;
                }
                self.out.push_str(") {\n");
                if *is_async || params.iter().any(default_needs_lifting) {
                    self.emit_default_param_assignments(params, 1)?;
                }
                self.with_fn_scope("fn", |s| {
                    s.emit_body(body)?;
                    Ok(())
                })?;
                self.out.push('}');
            }
        }
        Ok(())
    }

    fn emit_param(
        &mut self,
        param: &deka_syntax::Param<'a>,
        emit_default: bool,
    ) -> Result<(), String> {
        self.out.push_str(param.name);
        if emit_default {
            if let Some(default) = &param.default_value {
                self.out.push_str(" = ");
                self.emit_expr(default)?;
            }
        }
        Ok(())
    }

    fn emit_default_param_assignments(
        &mut self,
        params: &[deka_syntax::Param<'a>],
        indent: usize,
    ) -> Result<(), String> {
        for param in params.iter() {
            if param.default_value.is_some() {
                write_indent(&mut self.out, indent);
                self.out.push_str("if (");
                self.out.push_str(param.name);
                self.out.push_str(" === undefined) { ");
                let mut expressions = Vec::new();
                deka_syntax::visit::walk_expr(param.default_value.as_ref().unwrap(), &mut |expr| {
                    if has_needs_temporaries(expr) {
                        expressions.push((expr as *const _ as usize, has_temporary_names(expr)));
                    }
                });
                self.emit_has_temporaries(expressions);
                let value = self.lift_value(param.default_value.as_ref().unwrap())?;
                self.out.push_str(param.name);
                self.out.push_str(" = ");
                self.out.push_str(&value);
                self.out.push_str("; }\n");
            }
        }
        Ok(())
    }

    fn emit_struct_literal(
        &mut self,
        name: &str,
        fields: &[deka_syntax::StructLiteralField<'a>],
    ) -> Result<(), String> {
        let meta = self
            .structs
            .get(name)
            .cloned()
            .ok_or_else(|| format!("unknown struct `{}` in emitter", name))?;

        self.uses_struct = true;
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        // Fields promoted from embedded structs, recorded as (embed path from
        // this struct to the owner, field name, emitted value).
        let mut promoted: Vec<(Vec<String>, String, String)> = Vec::new();

        for field in fields.iter() {
            seen.insert(field.name.to_string());
            let mut value_buf = String::new();
            std::mem::swap(&mut self.out, &mut value_buf);
            self.emit_expr(&field.value)?;
            std::mem::swap(&mut self.out, &mut value_buf);
            if meta.fields.contains(field.name)
                || meta.embeds.iter().any(|e| e.as_str() == field.name)
            {
                let key = if is_js_identifier(field.name) {
                    field.name.to_string()
                } else {
                    json_string(field.name)
                };
                entries.push(format!("{}: {}", key, value_buf));
            } else {
                // Promoted field: route it into the embedded struct that owns
                // it, e.g. `Employee { name: ... }` becomes
                // `Employee({ Person: Person({ name: ... }) })`.
                let mut path = Vec::new();
                if !self.find_promoted_field_path(name, field.name, &mut path) {
                    return Err(format!(
                        "struct `{}` has no field or embed `{}` in emitter",
                        name, field.name
                    ));
                }
                promoted.push((path, field.name.to_string(), value_buf));
            }
        }

        // Auto-fill embedded structs: assemble them from promoted fields when
        // supplied piecemeal, or default-construct them when entirely empty.
        for embed in &meta.embeds {
            if seen.contains(embed) {
                continue;
            }
            let group: Vec<(Vec<String>, String, String)> = promoted
                .iter()
                .filter(|(path, _, _)| path.first() == Some(embed))
                .map(|(path, name, value)| (path[1..].to_vec(), name.clone(), value.clone()))
                .collect();
            if group.is_empty() {
                if meta.empty_embeds.contains(embed)
                    || self.is_empty_embed_struct_for_promotion(name, embed)
                {
                    if self.promotion_structs.contains_key(name) {
                        self.uses_promotion_factory = true;
                    }
                    let path = vec![embed.clone()];
                    let factory = self.embed_factory_expr(name, &path);
                    entries.push(format!("{}: {}({{}})", embed, factory));
                }
                continue;
            }
            let path = vec![embed.clone()];
            let body = self.emit_promoted_embed_body(name, embed, &path, &group)?;
            if self.promotion_structs.contains_key(name) {
                self.uses_promotion_factory = true;
            }
            let factory = self.embed_factory_expr(name, &path);
            entries.push(format!("{}: {}({{ {} }})", embed, factory, body));
        }

        // Auto-fill omitted optional fields.
        for (opt, default) in &meta.optional {
            if !seen.contains(opt) {
                let value = match default {
                    Some(expr) => expr.clone(),
                    None => {
                        self.uses_prelude_enums = true;
                        "undefined".to_string()
                    }
                };
                entries.push(format!("{}: {}", opt, value));
            }
        }

        self.out.push_str(name);
        self.out.push_str("({ ");
        self.out.push_str(&entries.join(", "));
        self.out.push_str(" })");
        Ok(())
    }

    /// Find the chain of embedded structs leading from `struct_name` to the
    /// struct that declares `field`. Depth-first, mirroring the typechecker's
    /// promoted-field resolution.
    fn find_promoted_field_path(
        &self,
        struct_name: &str,
        field: &str,
        path: &mut Vec<String>,
    ) -> bool {
        self.find_promoted_field_path_from(struct_name, struct_name, field, path)
    }

    fn find_promoted_field_path_from(
        &self,
        root: &str,
        struct_name: &str,
        field: &str,
        path: &mut Vec<String>,
    ) -> bool {
        let meta = match self.struct_meta_for_promotion(root, struct_name) {
            Some(m) => m,
            None => return false,
        };
        for embed in &meta.embeds {
            path.push(embed.clone());
            let declares = self
                .struct_meta_for_promotion(root, embed)
                .map(|m| m.fields.contains(field))
                .unwrap_or(false);
            if declares || self.find_promoted_field_path_from(root, embed, field, path) {
                return true;
            }
            path.pop();
        }
        false
    }

    fn struct_meta_for_promotion(&self, root: &str, name: &str) -> Option<&StructMeta> {
        self.promotion_structs
            .get(root)
            .and_then(|closure| closure.get(name))
            .or_else(|| self.structs.get(name))
    }

    fn is_empty_embed_struct_for_promotion(&self, root: &str, name: &str) -> bool {
        let Some(meta) = self.struct_meta_for_promotion(root, name) else {
            return false;
        };
        meta.fields.is_empty()
            && meta.embeds.iter().all(|embed| self.is_empty_embed_struct_for_promotion(root, embed))
    }

    /// The factory for a private embed remains encapsulated by the exported
    /// root factory. The generated helper reaches it without importing or
    /// naming the private struct in the consumer module (dsc#86).
    fn embed_factory_expr(&self, root: &str, path: &[String]) -> String {
        if self.promotion_structs.contains_key(root) {
            let path = path.iter().map(|name| json_string(name)).collect::<Vec<_>>().join(", ");
            format!("__deka_embed_factory({}, [{}])", root, path)
        } else {
            path.last().cloned().unwrap_or_else(|| root.to_string())
        }
    }

    /// Emit the object-literal body for an embedded struct assembled from
    /// promoted fields. Each entry carries the remaining embed path below
    /// this struct, the field name, and the already-emitted value.
    fn emit_promoted_embed_body(
        &mut self,
        root: &str,
        struct_name: &str,
        path: &[String],
        fields: &[(Vec<String>, String, String)],
    ) -> Result<String, String> {
        let meta = self
            .struct_meta_for_promotion(root, struct_name)
            .cloned()
            .ok_or_else(|| format!("unknown struct `{}` in emitter", struct_name))?;
        let mut entries = Vec::new();
        let mut supplied = HashSet::new();
        for (path, name, value) in fields {
            if path.is_empty() {
                supplied.insert(name.clone());
                let key = if is_js_identifier(name) {
                    name.clone()
                } else {
                    json_string(name)
                };
                entries.push(format!("{}: {}", key, value));
            }
        }
        // Recurse into sub-embeds that own any of the remaining fields.
        for embed in &meta.embeds {
            let group: Vec<(Vec<String>, String, String)> = fields
                .iter()
                .filter(|(path, _, _)| path.first() == Some(embed))
                .map(|(path, name, value)| (path[1..].to_vec(), name.clone(), value.clone()))
                .collect();
            if group.is_empty() {
                if meta.empty_embeds.contains(embed)
                    || self.is_empty_embed_struct_for_promotion(root, embed)
                {
                    let mut embed_path = path.to_vec();
                    embed_path.push(embed.clone());
                    let factory = self.embed_factory_expr(root, &embed_path);
                    entries.push(format!("{}: {}({{}})", embed, factory));
                }
                continue;
            }
            let mut embed_path = path.to_vec();
            embed_path.push(embed.clone());
            let body = self.emit_promoted_embed_body(root, embed, &embed_path, &group)?;
            let factory = self.embed_factory_expr(root, &embed_path);
            entries.push(format!("{}: {}({{ {} }})", embed, factory, body));
        }
        // Auto-fill omitted optional fields, mirroring emit_struct_literal.
        for (opt, default) in &meta.optional {
            if !supplied.contains(opt) {
                let value = match default {
                    Some(expr) => expr.clone(),
                    None => {
                        self.uses_prelude_enums = true;
                        "undefined".to_string()
                    }
                };
                entries.push(format!("{}: {}", opt, value));
            }
        }
        Ok(entries.join(", "))
    }

    fn emit_unsafe(&mut self, source: &str, bare: bool) -> Result<(), String> {
        // All construction sites use the shared Result representation,
        // including raw-JS boundaries and explicit exception conversion.
        //
        // dsc#60: the Err payload expression depends on the form. The
        // annotated form `unsafe<T> { }` types as `Result<T, JsError>`
        // (deka#460), so its payload stays an Error object — thrown Errors
        // pass through, anything else is wrapped — which is exactly what the
        // `JsError` member table (.message/.name) promises. The bare legacy
        // form yields `Result<Infer, string>` (deka#252, dsc#103): its Err
        // side is the thrown value's string representation, so an Error
        // object leaking out would be silently accepted as any type. Until
        // the bare form is migrated to mandatory annotations, its Err payload
        // is normalized to the thrown value's string representation here, at
        // the boundary, so errors-as-values (`Err` carries diagnostic text)
        // holds on every path.
        let err_payload = unsafe_error_payload(bare);
        let (invocation, is_async) = self.unsafe_invocation(source);
        let fn_kw = if is_async {
            "async function"
        } else {
            "function"
        };
        self.out.push('(');
        self.out.push_str(fn_kw);
        self.out.push_str("() { try { return (");
        self.out.push_str(crate::prelude::RESULT_OK);
        self.out.push_str(")(");
        self.out.push_str(&invocation);
        self.out.push_str("); } catch (err) { return (");
        self.out.push_str(crate::prelude::RESULT_ERR);
        self.out.push_str(")(");
        self.out.push_str(err_payload);
        self.out.push_str("); } })()");

        Ok(())
    }

    fn unsafe_invocation(&self, source: &str) -> (String, bool) {
        let trimmed = source.trim();
        if trimmed.is_empty() {
            return ("undefined".into(), false);
        }

        // A DekaScript struct literal spliced into the raw JavaScript does not
        // parse (`User { ... }` is a syntax error in expression position), so
        // rewrite known-struct spellings to the factory call the syntax
        // denotes before choosing the wrapper shape (dsc#58).
        let rewritten;
        let trimmed = if self.structs.is_empty() || !trimmed.contains('{') {
            trimmed
        } else {
            rewritten =
                rewrite_struct_literals_in_js(trimmed, 0, &|name| self.structs.contains_key(name));
            rewritten.as_str()
        };

        let is_async = js_has_top_level_await(trimmed);
        let is_statement_block = raw_js_looks_like_statements(trimmed);

        let fn_kw = if is_async {
            "async function"
        } else {
            "function"
        };
        // The body is raw JavaScript spliced verbatim, so the delimiters that
        // follow it must start on their own line. Without the newlines a body
        // whose last line is a `//` comment swallows the closing `}` and `)()`
        // and the module fails to parse with `Unexpected end of input`
        // (deka#378-adjacent codegen-as-text class; see also #356, #371).
        let inner = if is_statement_block {
            format!("({fn_kw}() {{\n{trimmed}\n}})()")
        } else {
            format!("({fn_kw}() {{ return (\n{trimmed}\n); }})()")
        };

        let invocation = if is_async {
            format!("await {inner}")
        } else {
            inner
        };

        (invocation, is_async)
    }

    fn emit_bridge(&mut self, kind: &str, action: &str, args: &[Expr<'a>]) -> Result<(), String> {
        // The catalog owns sync/async dispatch. Normalize with a compiler-
        // owned helper so older installed runtimes cannot reintroduce the
        // former tagged Result representation.
        let is_async = deka_syntax::bridge_op_is_async(kind, action);
        if !is_async {
            self.out.push_str("__deka_result_from_host(");
        }
        self.out.push_str("__deka_host(");
        self.out.push_str(&json_string(kind));
        self.out.push_str(", ");
        self.out.push_str(&json_string(action));
        self.out.push_str(", [");
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.emit_expr(arg)?;
        }
        self.out.push_str("])");
        if is_async {
            self.out.push_str(".then(__deka_result_from_host)");
        } else {
            self.out.push(')');
        }
        Ok(())
    }

    fn exception_form(&self, expr: &Expr<'a>) -> Option<deka_syntax::typeck::ExceptionEmit> {
        match expr {
            Expr::Paren { expr, .. } | Expr::Safe { expr, .. } => self.exception_form(expr),
            _ => self.exception_forms.get(&(expr as *const _)).copied(),
        }
    }

    fn emit_raise(&mut self, expr: &Expr<'a>) -> Result<(), String> {
        match expr {
            Expr::Paren { expr, .. } | Expr::Safe { expr, .. } => self.emit_raise(expr),
            Expr::EnumConstructor {
                payload: Some(payload),
                ..
            } => {
                let payload = self.lift_value(payload)?;
                self.out.push_str("throw ");
                self.out.push_str(&payload);
                self.out.push_str(";\n");
                Ok(())
            }
            _ => Err("invalid checked Throw lowering".into()),
        }
    }

    fn emit_passthrough_binding(&mut self, arm: &deka_syntax::MatchArm<'a>, payload: &str) {
        if arm.bodyless {
            if let Expr::EnumConstructor { payload: Some(Expr::Identifier { name, .. }), .. } = &arm.body {
                if name.starts_with("$__deka_passthrough_") { self.out.push_str(&format!("const {name} = {payload};\n")); }
            }
        }
    }

    fn emit_handling_arm(
        &mut self,
        arm: &deka_syntax::MatchArm<'a>,
        result: Option<&str>,
        passthrough: Option<&str>,
    ) -> Result<(), String> {
        if self.exception_form(&arm.body) == Some(deka_syntax::typeck::ExceptionEmit::Throw) {
            return self.emit_raise(&arm.body);
        }
        if arm.bodyless && self.exception_form(&arm.body).is_none() {
            if let Some(scrutinee) = passthrough {
                self.out.push_str(&format!("return {scrutinee};\n"));
                return Ok(());
            }
        }
        let value = self.lift_value(&arm.body)?;
        if arm.bodyless {
            self.out.push_str("return ");
        } else if let Some(result) = result {
            self.out.push_str(result);
            self.out.push_str(" = ");
        }
        self.out.push_str(&value);
        self.out.push_str(";\n");
        Ok(())
    }

    fn emit_exception_match(
        &mut self,
        scrutinee: &Expr<'a>,
        arms: &[deka_syntax::MatchArm<'a>],
        result: Option<&str>,
    ) -> Result<(), String> {
        let id = self.next_match_id();
        let label = format!("__deka_exception_{id}");
        let value = format!("__deka_ok_{id}");
        let error = format!("__deka_throw_{id}");
        // The protected region contains only the invocation. A handler that
        // throws is observed by its caller, never by its sibling handler.
        self.out
            .push_str(&format!("{label}: {{\nlet {value};\ntry {{\n"));
        let scrutinee_value = self.lift_value(scrutinee)?;
        self.out.push_str(&format!("{value} = {scrutinee_value}"));
        self.out.push_str(&format!("; }} catch ({error}) {{\nif ({error}?.[Symbol.for(\"deka.BoundaryError\")]) throw {error};\n"));
        self.emit_exception_arms(arms, "Throw", &error, &label, result)?;
        self.out.push_str(&format!("throw {error};\n}}\n"));
        self.emit_exception_arms(arms, "Ok", &value, &label, result)?;
        self.out.push_str("}\n");
        Ok(())
    }

    fn emit_exception_arms(
        &mut self,
        arms: &[deka_syntax::MatchArm<'a>],
        case: &str,
        value: &str,
        label: &str,
        result: Option<&str>,
    ) -> Result<(), String> {
        for arm in arms {
            let payload = match &arm.pattern {
                Pattern::Constructor {
                    name,
                    payload: Some(payload),
                    ..
                } if *name == case => *payload,
                Pattern::Constructor { .. } => continue,
                other => other,
            };
            let condition = self.match_condition(payload, value);
            self.out.push_str(&format!("if ({condition}) {{\n"));
            self.emit_pattern_bindings(payload, value, 0)?;
            self.emit_passthrough_binding(arm, value);
            self.emit_handling_arm(arm, result, None)?;
            if !arm.bodyless
                && self.exception_form(&arm.body) != Some(deka_syntax::typeck::ExceptionEmit::Throw)
            {
                self.out.push_str(&format!("break {label};\n"));
            }
            self.out.push_str("}\n");
        }
        Ok(())
    }

    /// Emit a match in statement position. A match expression is only
    /// statement-shaped at its enclosing statement boundary; lowering it
    /// here avoids allocating a closure for the common case.
    fn emit_match_statements(
        &mut self,
        scrutinee: &Expr<'a>,
        arms: &[deka_syntax::MatchArm<'a>],
        result: Option<&str>,
    ) -> Result<(), String> {
        if matches!(
            self.exception_form(scrutinee),
            Some(
                deka_syntax::typeck::ExceptionEmit::Match
                    | deka_syntax::typeck::ExceptionEmit::FromResult
            )
        ) {
            return self.emit_exception_match(scrutinee, arms, result);
        }
        if self.emit_melted_match(scrutinee, arms, result)? {
            return Ok(());
        }
        let scrutinee_value = self.lift_value(scrutinee)?;
        let id = self.next_match_id();
        let scrutinee_var = format!("__deka_match_scrutinee_{id}");
        write_indent(&mut self.out, 0);
        self.out.push_str("const ");
        self.out.push_str(&scrutinee_var);
        self.out.push_str(" = ");
        self.out.push_str(&scrutinee_value);
        self.out.push_str(";\n");

        let option_pair = arms.len() == 2 && arms.iter().all(|arm| {
            self.exception_forms.option_patterns.contains(&(&arm.pattern as *const _))
                && matches!(arm.pattern, Pattern::Constructor { payload: None, .. }
                    | Pattern::Constructor { payload: Some(Pattern::Identifier { .. } | Pattern::Wildcard { .. }), .. }
                    | Pattern::Identifier { .. })
                && arm.guard.is_none()
        });
        for (i, arm) in arms.iter().enumerate() {
            let condition = if option_pair && i == 1 { "true".into() } else { self.match_condition(&arm.pattern, &scrutinee_var) };
            write_indent(&mut self.out, 0);
            if i > 0 {
                self.out.push_str("else ");
            }
            if condition == "true" {
                self.out.push_str("{\n");
                self.emit_pattern_bindings(&arm.pattern, &scrutinee_var, 1)?;
                let field = if matches!(arm.pattern, Pattern::Constructor { name: "Err", .. }) { "error" } else { "value" };
                self.emit_passthrough_binding(arm, &format!("{scrutinee_var}.{field}"));
                write_indent(&mut self.out, 1);
                self.emit_handling_arm(arm, result, Some(&scrutinee_var))?;
                write_indent(&mut self.out, 0);
                self.out.push_str("}\n");
                continue;
            }
            self.out.push_str("if (");
            self.out.push_str(&condition);
            self.out.push_str(") {\n");
            self.emit_pattern_bindings(&arm.pattern, &scrutinee_var, 1)?;
                let field = if matches!(arm.pattern, Pattern::Constructor { name: "Err", .. }) { "error" } else { "value" };
                self.emit_passthrough_binding(arm, &format!("{scrutinee_var}.{field}"));
            write_indent(&mut self.out, 1);
            self.emit_handling_arm(arm, result, Some(&scrutinee_var))?;
            write_indent(&mut self.out, 0);
            self.out.push_str("}\n");
        }
        if !option_pair && arms
            .last()
            .map(|arm| self.match_condition(&arm.pattern, &scrutinee_var) != "true")
            .unwrap_or(true)
        {
            write_indent(&mut self.out, 0);
            self.out
                .push_str("else { throw new Error(\"non-exhaustive match\"); }\n");
        }
        Ok(())
    }

    fn emit_match_value_statements(
        &mut self,
        scrutinee: &Expr<'a>,
        arms: &[deka_syntax::MatchArm<'a>],
    ) -> Result<String, String> {
        let result = format!("__deka_match_result_{}", self.next_unwrap_id());
        write_indent(&mut self.out, 0);
        self.out.push_str("let ");
        self.out.push_str(&result);
        self.out.push_str(";\n");
        self.emit_match_statements(scrutinee, arms, Some(&result))?;
        Ok(result)
    }

    fn emit_match(
        &mut self,
        scrutinee: &Expr<'a>,
        arms: &[deka_syntax::MatchArm<'a>],
    ) -> Result<(), String> {
        if arms.iter().any(|arm| arm.bodyless) {
            return Err("internal error: bodyless match reached expression emission without statement lifting".into());
        }
        if matches!(
            self.exception_form(scrutinee),
            Some(
                deka_syntax::typeck::ExceptionEmit::Match
                    | deka_syntax::typeck::ExceptionEmit::FromResult
            )
        ) {
            let asynchronous = expr_contains_await(scrutinee) || arms.iter().any(|arm| expr_contains_await(&arm.body));
            self.out.push_str(if asynchronous {
                "(await (async () => {\n"
            } else {
                "(() => {\n"
            });
            let result = self.emit_match_value_statements(scrutinee, arms)?;
            self.out.push_str(&format!("return {result};\n}})()"));
            if asynchronous {
                self.out.push(')');
            }
            return Ok(());
        }
        let scrutinee_var = "__deka_scrutinee";
        self.out.push_str("((");
        self.out.push_str(scrutinee_var);
        self.out.push_str(") => {\n");

        for (i, arm) in arms.iter().enumerate() {
            let is_last = i == arms.len() - 1;
            self.emit_match_arm(arm, scrutinee_var, is_last)?;
        }

        self.out
            .push_str("  throw new Error(\"non-exhaustive match\");\n");
        self.out.push_str("})(");
        self.emit_expr(scrutinee)?;
        self.out.push_str(")");
        Ok(())
    }

    fn emit_match_arm(
        &mut self,
        arm: &deka_syntax::MatchArm<'a>,
        scrutinee_var: &str,
        is_last: bool,
    ) -> Result<(), String> {
        let condition = self.match_condition(&arm.pattern, scrutinee_var);

        if condition == "true" && is_last {
            self.emit_pattern_bindings(&arm.pattern, scrutinee_var, 2)?;
            write_indent(&mut self.out, 2);
            self.out.push_str("return ");
            self.emit_expr(&arm.body)?;
            self.out.push_str(";\n");
            return Ok(());
        }

        write_indent(&mut self.out, 2);
        self.out.push_str("if (");
        self.out.push_str(&condition);
        self.out.push_str(") {\n");
        self.emit_pattern_bindings(&arm.pattern, scrutinee_var, 4)?;
        write_indent(&mut self.out, 4);
        self.out.push_str("return ");
        self.emit_expr(&arm.body)?;
        self.out.push_str(";\n");
        write_indent(&mut self.out, 2);
        self.out.push_str("}\n");
        Ok(())
    }

    fn match_condition(&mut self, pattern: &Pattern<'a>, scrutinee_var: &str) -> String {
        match pattern {
            Pattern::Wildcard { .. } => "true".to_string(),
            // A bare name that resolved to a payload-free case is a case test,
            // not a binding. Emitting `true` for it is what made every arm
            // after the first one dead (deka#450).
            Pattern::Identifier { .. } => {
                match self
                    .enum_case_patterns
                    .get(&(pattern as *const Pattern<'a>))
                {
                    Some(&"None") if self.exception_forms.option_patterns.contains(&(pattern as *const _)) => format!("{scrutinee_var} === undefined"),
                    Some(case) => format!("{}.__case === \"{}\"", scrutinee_var, case),
                    None => "true".to_string(),
                }
            }
            Pattern::Literal { expr, .. } => {
                let mut literal = String::new();
                // Literal patterns are always simple literals; reuse expr emission.
                let mut tmp = Emitter {
                    program: self.program,
                    out: literal,
                    uses_struct: false,
                    uses_promotion_factory: false,
                    uses_newtype: false,
                    uses_prelude_enums: false,
                    struct_order: Vec::new(),
                    structs: HashMap::new(),
                    promotion_structs: HashMap::new(),
                    enums: HashMap::new(),
                    opaques: HashSet::new(),
                    interfaces: HashSet::new(),
                    erased_type_exports: HashMap::new(),
                    newtypes: HashMap::new(),
                    receiver_methods: HashMap::new(),
                    module_base: self.module_base.clone(),
                    module_root: self.module_root.clone(),
                    exception_forms: Default::default(),
                    unwrap_calls: HashMap::new(),
                    enum_case_patterns: HashMap::new(),
                    union_type_patterns: HashMap::new(),
                    build_blocks: HashMap::new(),
                    build_factory_names: HashSet::new(),
                    build_closure_names: HashSet::new(),
                    closure_sources: HashMap::new(),
                    unwrap_id: 0,
                    match_id: 0,
                    lifted_values: HashMap::new(),
            melted_results: HashMap::new(),
                    operator_rewrites: HashMap::new(),
                    method_calls: HashMap::new(),
                    type_of_calls: HashSet::new(),
                    signature_calls: HashMap::new(),
                    json_calls: HashMap::new(),
                    array_builtin_calls: HashMap::new(),
                    number_math_calls: HashMap::new(),
                    static_type_calls: HashMap::new(),
                    super_decl_trees: std::collections::HashMap::new(),
                    source_path: self.source_path.clone(),
                    file_stem: self.file_stem.clone(),
                    dev_entry: self.dev_entry,
                    fn_scope: self.fn_scope.clone(),
                    jsx_path: Vec::new(),
                    jsx_siblings: Vec::new(),
                    jsx_roots: 0,
                    live_names: None,
                    jsx_runtime: self.jsx_runtime.clone(),
                    jsx_helpers: self.jsx_helpers.clone(),
                    detached: false,
                    demand: crate::prelude::PreludeDemand::default(),
                };
                tmp.emit_expr(expr).expect("literal emission");
                literal = tmp.out;
                format!("{} === {}", scrutinee_var, literal)
            }
            Pattern::Constructor { name, payload, .. } => {
                // Union member type-patterns take priority: `string(s)` tests
                // `typeof`, not a `__case` that primitives do not have
                // (rfd#42, deka#530).
                if let Some(test) = self
                    .union_type_patterns
                    .get(&(pattern as *const Pattern<'a>))
                    .copied()
                {
                    if let deka_syntax::typeck::UnionMemberTest::EnumCase(enum_name) = test {
                        if enum_name == "Result" {
                            let mut condition = format!("typeof {scrutinee_var}?.ok === \"boolean\" && {scrutinee_var}.ok === {}", *name == "Ok");
                            if let Some(payload) = payload {
                                let field = if *name == "Err" { "error" } else { "value" };
                                condition.push_str(&format!(
                                    " && {}",
                                    self.match_condition(
                                        payload,
                                        &format!("{scrutinee_var}.{field}")
                                    )
                                ));
                            }
                            return condition;
                        }
                        return format!(
                            "{} && {}.__case === \"{}\"",
                            self.union_member_condition(&deka_syntax::typeck::UnionMemberTest::Enum(
                                enum_name
                            ), scrutinee_var),
                            scrutinee_var,
                            name
                        );
                    }
                    return self.union_member_condition(&test, scrutinee_var);
                }
                let result_case = self
                    .exception_forms
                    .result_patterns
                    .contains(&(pattern as *const _));
                let option_case = self.exception_forms.option_patterns.contains(&(pattern as *const _));
                let mut conditions = vec![if option_case {
                    format!("{scrutinee_var} {} undefined", if *name == "Some" { "!==" } else { "===" })
                } else if result_case {
                    format!("{scrutinee_var}.ok === {}", *name == "Ok")
                } else {
                    format!("{}.__case === \"{}\"", scrutinee_var, name)
                }];
                if let Some(payload) = payload {
                    let payload_access = if option_case {
                        scrutinee_var.to_string()
                    } else if *name == "Err" {
                        format!("{}.error", scrutinee_var)
                    } else {
                        format!("{}.value", scrutinee_var)
                    };
                    let payload_condition = self.match_condition(payload, &payload_access);
                    if payload_condition != "true" {
                        conditions.push(payload_condition);
                    }
                }
                conditions.join(" && ")
            }
            // Alternatives cannot bind (deka#446), so the test is a plain
            // disjunction and there is nothing to destructure.
            Pattern::Or { alternatives, .. } => {
                let tests: Vec<String> = alternatives
                    .iter()
                    .map(|alternative| self.match_condition(alternative, scrutinee_var))
                    .collect();
                format!("({})", tests.join(" || "))
            }
            Pattern::Struct { name, fields, .. } => {
                let brand = self.struct_brand(name);
                let mut conditions = vec![format!(
                    "{}?.__deka_struct === \"{}\"",
                    scrutinee_var, brand
                )];
                for field in fields.iter() {
                    conditions.push(self.match_condition(
                        &field.pattern,
                        &format!("{}.{}", scrutinee_var, field.name),
                    ));
                }
                conditions.join(" && ")
            }
            Pattern::Tuple { elements, .. } => {
                let mut conditions = vec![
                    format!("Array.isArray({})", scrutinee_var),
                    format!("{}.length === {}", scrutinee_var, elements.len()),
                ];
                for (index, element) in elements.iter().enumerate() {
                    conditions.push(
                        self.match_condition(element, &format!("{}[{}]", scrutinee_var, index)),
                    );
                }
                conditions.join(" && ")
            }
        }
    }

    /// The factory brand a local struct name resolves to. For an aliased
    /// import the brand is the exported name the declaring module
    /// instantiated the factory with, not the local spelling (dsc#51).
    fn struct_brand(&self, name: &str) -> String {
        self.structs
            .get(name)
            .map(|meta| {
                if meta.brand.is_empty() {
                    name.to_string()
                } else {
                    meta.brand.clone()
                }
            })
            .unwrap_or_else(|| name.to_string())
    }

    /// The runtime predicate for a union member type-pattern (rfd#42,
    /// deka#530). Structs read the `__deka_struct` brand tag directly — no
    /// factory, no helper, nothing to force (deka#551).
    fn union_member_condition(
        &mut self,
        test: &deka_syntax::typeck::UnionMemberTest<'a>,
        scrutinee_var: &str,
    ) -> String {
        match test {
            deka_syntax::typeck::UnionMemberTest::Primitive(name) => {
                let js_type = if *name == "void" { "undefined" } else { name };
                format!("typeof {} === \"{}\"", scrutinee_var, js_type)
            }
            deka_syntax::typeck::UnionMemberTest::ErrorClass(name) => {
                format!("{scrutinee_var} instanceof {name}")
            }
            deka_syntax::typeck::UnionMemberTest::Bytes => {
                format!("{} instanceof Uint8Array", scrutinee_var)
            }
            deka_syntax::typeck::UnionMemberTest::Struct(name) => {
                // Read the brand tag directly, the same way
                // `__deka_type_of` does — no prelude helper needed (deka#551).
                // The brand is the declared name, though: through a renamed
                // import (`import { User as Person }`) the local spelling
                // never matches the factory tag (dsc#51).
                let brand = self.struct_brand(name);
                format!("{}?.__deka_struct === \"{}\"", scrutinee_var, brand)
            }
            deka_syntax::typeck::UnionMemberTest::Enum("Result")
            | deka_syntax::typeck::UnionMemberTest::EnumCase("Result") => {
                format!("typeof {scrutinee_var}?.ok === \"boolean\"")
            }
            deka_syntax::typeck::UnionMemberTest::Enum(name) => {
                format!("{}.__enum === \"{}\"", scrutinee_var, name)
            }
            deka_syntax::typeck::UnionMemberTest::EnumCase(name) => {
                format!("{}.__enum === \"{}\"", scrutinee_var, name)
            }
        }
    }

    fn emit_pattern_bindings(
        &mut self,
        pattern: &Pattern<'a>,
        scrutinee_var: &str,
        indent: usize,
    ) -> Result<(), String> {
        match pattern {
            Pattern::Wildcard { .. } => {}
            Pattern::Identifier { name, .. } => {
                write_indent(&mut self.out, indent);
                self.out.push_str("const ");
                self.out.push_str(name);
                self.out.push_str(" = ");
                self.out.push_str(scrutinee_var);
                self.out.push_str(";\n");
            }
            Pattern::Literal { .. } => {}
            // Alternatives cannot bind (deka#446), so there is nothing to
            // destructure here.
            Pattern::Or { .. } => {}
            Pattern::Constructor { name, payload, .. } => {
                // Union type-patterns: the payload IS the scrutinee, so the
                // binding is `const s = <scrutinee>;` — the Identifier arm
                // below, reached by passing the scrutinee through unchanged
                // (rfd#42, deka#530).
                if self
                    .union_type_patterns
                    .get(&(pattern as *const Pattern<'a>))
                    .is_some_and(|test| {
                        !matches!(test, deka_syntax::typeck::UnionMemberTest::EnumCase(_))
                    })
                {
                    if let Some(payload) = payload {
                        self.emit_pattern_bindings(payload, scrutinee_var, indent)?;
                    }
                    return Ok(());
                }
                if let Some(payload) = payload {
                    let payload_access = if self.exception_forms.option_patterns.contains(&(pattern as *const _)) {
                        scrutinee_var.to_string()
                    } else if *name == "Err" {
                        format!("{}.error", scrutinee_var)
                    } else {
                        format!("{}.value", scrutinee_var)
                    };
                    self.emit_pattern_bindings(payload, &payload_access, indent)?;
                }
            }
            Pattern::Struct { fields, .. } => {
                for field in fields.iter() {
                    self.emit_pattern_bindings(
                        &field.pattern,
                        &format!("{}.{}", scrutinee_var, field.name),
                        indent,
                    )?;
                }
            }
            Pattern::Tuple { elements, .. } => {
                for (index, element) in elements.iter().enumerate() {
                    self.emit_pattern_bindings(
                        element,
                        &format!("{}[{}]", scrutinee_var, index),
                        indent,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn emit_jsx_element(&mut self, element: &deka_syntax::JsxElement<'a>) -> Result<(), String> {
        self.enter_jsx_node();
        let is_component = element
            .tag
            .chars()
            .next()
            .map(|c| c.is_ascii_uppercase())
            .unwrap_or(false);
        let tag_expr = if is_component {
            element.tag.to_string()
        } else {
            format!("\"{}\"", escape_string(element.tag))
        };

        let mut props = Vec::new();
        let mut key = None;
        if !is_component {
            props.push(format!(
                "\"data-deka-id\": {}",
                json_string(&self.current_deka_id())
            ));
        }
        for attr in element.attributes.iter() {
            if attr.name.is_empty() {
                return Err("JSX spread attributes are not supported".into());
            } else {
                let value = match &attr.value {
                    Some(v) => {
                        let mut buf = String::new();
                        std::mem::swap(&mut self.out, &mut buf);
                        self.emit_expr(v)?;
                        std::mem::swap(&mut self.out, &mut buf);
                        buf
                    }
                    None => "true".to_string(),
                };
                // Optional props erase to the caller's value, just like Some(v).
                if attr.name == "key" { key = Some(value); } else {
                    props.push(format!("\"{}\": {}", escape_string(attr.name), value));
                }
            }
        }

        let mut child_values = Vec::new();
        for child in element.children.iter() {
            child_values.push(self.emit_jsx_child(child)?);
        }

        if child_values.len() == 1 {
            props.push(format!("\"children\": {}", child_values[0]));
        } else if !child_values.is_empty() {
            props.push(format!("\"children\": [{}]", child_values.join(", ")));
        }
        let helper = usize::from(child_values.len() > 1);
        self.out.push_str(&self.jsx_helpers[helper]);
        self.out.push('(');
        self.out.push_str(&tag_expr);
        self.out.push_str(", {");
        self.out.push_str(&props.join(", "));
        self.out.push('}');
        if let Some(key) = key {
            self.out.push_str(", ");
            self.out.push_str(&key);
        }
        self.out.push(')');
        self.exit_jsx_node();
        Ok(())
    }

    fn emit_jsx_child(&mut self, child: &Expr<'a>) -> Result<String, String> {
        let mut buf = String::new();
        std::mem::swap(&mut self.out, &mut buf);
        self.emit_expr(child)?;
        std::mem::swap(&mut self.out, &mut buf);
        Ok(buf)
    }

    fn emit_jsx_fragment(&mut self, children: &[Expr<'a>]) -> Result<(), String> {
        self.enter_jsx_node();
        let mut child_values = Vec::new();
        for child in children.iter() {
            child_values.push(self.emit_jsx_child(child)?);
        }

        let helper = usize::from(child_values.len() > 1);
        self.out.push_str(&self.jsx_helpers[helper]);
        self.out.push('(');
        self.out.push_str(&self.jsx_helpers[2]);
        self.out.push_str(", {");
        if child_values.len() == 1 {
            self.out.push_str("\"children\": ");
            self.out.push_str(&child_values[0]);
        } else if !child_values.is_empty() {
            self.out.push_str("\"children\": [");
            self.out.push_str(&child_values.join(", "));
            self.out.push(']');
        }
        self.out.push_str("})");
        self.exit_jsx_node();
        Ok(())
    }
}

fn is_optional_type(ty: &Type) -> bool {
    matches!(ty, Type::Option { .. })
}

fn json_string(s: &str) -> String {
    format!("\"{}\"", escape_string(s))
}

fn is_js_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !first.is_ascii_alphabetic() && first != '_' && first != '$' {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn is_hole_expr(expr: &Expr) -> bool {
    matches!(expr, Expr::Identifier { name: "_", .. })
}

#[derive(Default)]
struct RawJsScan<'a> {
    first_word: Option<&'a str>,
    has_top_level_semicolon: bool,
    has_await: bool,
    saw_significant: bool,
    previous_can_end_expression: bool,
}

#[derive(Clone, Copy)]
enum RawJsTokenClass {
    Word,
    Number,
    String,
    Template,
    Punctuation(u8),
}

/// Scan the parts of an unsafe body that affect wrapper selection.
///
/// This is deliberately a small lexical scanner rather than a JavaScript
/// parser. The browser compiler cannot afford to link SWC's parser, but the
/// old string searches were not safe: punctuation and keywords inside
/// literals, comments, and regexes changed the emitted wrapper. Keeping this
/// scanner lexical also means native and WASM compilers make the same choice.
/// Rewrite DekaScript struct literals spliced into a raw `unsafe` body into
/// factory calls. `User { name: "Ada" }` denotes the factory construction
/// `User({ name: "Ada" })`, but the body is spliced verbatim, and
/// `Identifier {` is a syntax error in JavaScript expression position —
/// arrow bodies in particular (`() => User { ... }`) took the whole emitted
/// module down with them (dsc#58).
///
/// Only names this module knows as structs are rewritten, and only where
/// `Identifier {` cannot already be valid JavaScript: never after `.`
/// (member access), never where a class name or `extends` clause could sit,
/// and never across a line break (ASI may already split the two tokens).
/// Everything else — strings, templates, comments, regexes, foreign names —
/// passes through byte-for-byte.
fn rewrite_struct_literals_in_js(
    raw: &str,
    depth: usize,
    is_struct: &impl Fn(&str) -> bool,
) -> String {
    const MAX_DEPTH: usize = 64;
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    // Start of the not-yet-copied span.
    let mut copy_from = 0usize;
    let mut i = 0usize;
    // Class of the previous significant token.
    let mut prev_dot = false;
    let mut prev_guard_word = false;
    let mut previous_allows_regex = true;
    let mut changed = false;

    while i < bytes.len() {
        let start = i;
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'\'' | b'"' => {
                i = skip_js_quoted(raw, i);
                previous_allows_regex = false;
            }
            b'`' => {
                i = skip_js_template(raw, i);
                previous_allows_regex = false;
            }
            b'/' if previous_allows_regex => {
                i = skip_js_regex(raw, i);
                previous_allows_regex = false;
            }
            b'0'..=b'9' => {
                i = skip_js_number(bytes, i);
                previous_allows_regex = false;
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$' => {
                i += 1;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
                {
                    i += 1;
                }
                let word = &raw[start..i];
                let (next, crossed_newline) = skip_js_ws_and_comments(raw, i);
                let rewritable = is_struct(word)
                    && !prev_dot
                    && !prev_guard_word
                    && !crossed_newline
                    && next < bytes.len()
                    && bytes[next] == b'{'
                    && depth < MAX_DEPTH
                    && js_matching_brace(raw, next).is_some();
                if rewritable {
                    let after = js_matching_brace(raw, next).unwrap();
                    out.push_str(&raw[copy_from..start]);
                    out.push_str(word);
                    out.push_str("({");
                    let inner = rewrite_struct_literals_in_js(
                        &raw[next + 1..after - 1],
                        depth + 1,
                        is_struct,
                    );
                    out.push_str(&inner);
                    out.push_str("})");
                    i = after;
                    copy_from = after;
                    changed = true;
                    previous_allows_regex = false;
                } else {
                    previous_allows_regex = word_allows_regex_after(word);
                }
                prev_dot = false;
                prev_guard_word = matches!(
                    word,
                    "class" | "extends" | "new" | "typeof" | "instanceof" | "in" | "of"
                        | "delete" | "void" | "case"
                );
            }
            punct => {
                i += 1;
                if i < bytes.len() && is_two_byte_js_punctuation(punct, bytes[i]) {
                    i += 1;
                }
                prev_dot = punct == b'.';
                prev_guard_word = false;
                previous_allows_regex = punctuation_allows_regex_after(punct);
            }
        }
    }
    if changed {
        out.push_str(&raw[copy_from..]);
        out
    } else {
        raw.to_string()
    }
}

/// Whitespace and comments starting at `i`; reports whether a line break was
/// crossed (which lets JavaScript ASI split an identifier from a block).
fn skip_js_ws_and_comments(raw: &str, mut i: usize) -> (usize, bool) {
    let bytes = raw.as_bytes();
    let mut saw_newline = false;
    loop {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\r') {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'\n' {
            saw_newline = true;
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            if raw[start..i].contains('\n') {
                saw_newline = true;
            }
            continue;
        }
        return (i, saw_newline);
    }
}

/// The position just past the `}` matching the `{` at `open`, skipping
/// strings, templates, comments, and (best-effort) regex literals so braces
/// inside them do not skew the count. `None` when unbalanced.
fn js_matching_brace(raw: &str, open: usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut depth = 1usize;
    let mut i = open + 1;
    let mut previous_allows_regex = true;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                depth += 1;
                i += 1;
                previous_allows_regex = true;
            }
            b'}' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return Some(i);
                }
                previous_allows_regex = false;
            }
            b'\'' | b'"' => {
                i = skip_js_quoted(raw, i);
                previous_allows_regex = false;
            }
            b'`' => {
                i = skip_js_template(raw, i);
                previous_allows_regex = false;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'/' if previous_allows_regex => {
                i = skip_js_regex(raw, i);
                previous_allows_regex = false;
            }
            b'0'..=b'9' => {
                i = skip_js_number(bytes, i);
                previous_allows_regex = false;
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$' => {
                let word_start = i;
                i += 1;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
                {
                    i += 1;
                }
                previous_allows_regex = word_allows_regex_after(&raw[word_start..i]);
            }
            punct => {
                i += 1;
                previous_allows_regex = punctuation_allows_regex_after(punct);
            }
        }
    }
    None
}

fn scan_raw_js(raw: &str) -> RawJsScan<'_> {
    let bytes = raw.as_bytes();
    let mut scan = RawJsScan::default();
    let mut i = 0;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut line_break_since_token = false;
    let mut previous_allows_regex = true;

    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => {
                if bytes[i] == b'\n' {
                    line_break_since_token = true;
                }
                i += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                line_break_since_token = true;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                let start = i;
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                if i + 1 < bytes.len() {
                    i += 2;
                }
                if raw[start..i.min(raw.len())].contains('\n') {
                    line_break_since_token = true;
                }
            }
            b'\'' | b'"' => {
                let start = i;
                i = skip_js_quoted(raw, i);
                scan_token(
                    &mut scan,
                    &raw[start..i],
                    RawJsTokenClass::String,
                    line_break_since_token,
                    paren_depth,
                    bracket_depth,
                    brace_depth,
                );
                line_break_since_token = false;
                previous_allows_regex = false;
            }
            b'`' => {
                let start = i;
                i = skip_js_template(raw, i);
                scan_token(
                    &mut scan,
                    &raw[start..i],
                    RawJsTokenClass::Template,
                    line_break_since_token,
                    paren_depth,
                    bracket_depth,
                    brace_depth,
                );
                line_break_since_token = false;
                previous_allows_regex = false;
            }
            b'/' if previous_allows_regex => {
                i = skip_js_regex(raw, i);
                scan_token(
                    &mut scan,
                    "/regex/",
                    RawJsTokenClass::String,
                    line_break_since_token,
                    paren_depth,
                    bracket_depth,
                    brace_depth,
                );
                line_break_since_token = false;
                previous_allows_regex = false;
            }
            b'0'..=b'9' => {
                let start = i;
                i = skip_js_number(bytes, i);
                scan_token(
                    &mut scan,
                    &raw[start..i],
                    RawJsTokenClass::Number,
                    line_break_since_token,
                    paren_depth,
                    bracket_depth,
                    brace_depth,
                );
                line_break_since_token = false;
                previous_allows_regex = false;
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$' => {
                let start = i;
                i += 1;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
                {
                    i += 1;
                }
                let word = &raw[start..i];
                scan_token(
                    &mut scan,
                    word,
                    RawJsTokenClass::Word,
                    line_break_since_token,
                    paren_depth,
                    bracket_depth,
                    brace_depth,
                );
                line_break_since_token = false;
                previous_allows_regex = word_allows_regex_after(word);
            }
            punct => {
                let start = i;
                let class = RawJsTokenClass::Punctuation(punct);
                i += 1;
                if i < bytes.len() && is_two_byte_js_punctuation(punct, bytes[i]) {
                    i += 1;
                }
                scan_token(
                    &mut scan,
                    &raw[start..i],
                    class,
                    line_break_since_token,
                    paren_depth,
                    bracket_depth,
                    brace_depth,
                );
                line_break_since_token = false;
                match punct {
                    b'(' => paren_depth += 1,
                    b')' => paren_depth = paren_depth.saturating_sub(1),
                    b'[' => bracket_depth += 1,
                    b']' => bracket_depth = bracket_depth.saturating_sub(1),
                    b'{' => brace_depth += 1,
                    b'}' => brace_depth = brace_depth.saturating_sub(1),
                    _ => {}
                }
                previous_allows_regex = punctuation_allows_regex_after(punct);
            }
        }
    }

    scan
}

fn scan_token<'a>(
    scan: &mut RawJsScan<'a>,
    text: &'a str,
    class: RawJsTokenClass,
    line_break_before: bool,
    paren_depth: usize,
    bracket_depth: usize,
    brace_depth: usize,
) {
    let at_top_level = paren_depth == 0 && bracket_depth == 0 && brace_depth == 0;
    let can_start_expression = matches!(
        class,
        RawJsTokenClass::Word
            | RawJsTokenClass::Number
            | RawJsTokenClass::String
            | RawJsTokenClass::Template
    );
    if line_break_before && at_top_level && scan.previous_can_end_expression && can_start_expression
    {
        scan.has_top_level_semicolon = true;
    }

    if !scan.saw_significant {
        if let RawJsTokenClass::Word = class {
            scan.first_word = Some(text);
        }
        scan.saw_significant = true;
    }
    if text == "await" {
        scan.has_await = true;
    }
    if let RawJsTokenClass::Punctuation(punct) = class {
        if punct == b';' && at_top_level {
            scan.has_top_level_semicolon = true;
        }
    }
    scan.previous_can_end_expression = matches!(
        class,
        RawJsTokenClass::Word
            | RawJsTokenClass::Number
            | RawJsTokenClass::String
            | RawJsTokenClass::Template
    );
    if let RawJsTokenClass::Punctuation(punct) = class {
        scan.previous_can_end_expression |= matches!(punct, b')' | b']' | b'}');
    }
}

fn skip_js_quoted(raw: &str, mut i: usize) -> usize {
    let quote = raw.as_bytes()[i];
    i += 1;
    let bytes = raw.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = (i + 2).min(bytes.len());
        } else if bytes[i] == quote {
            return i + 1;
        } else {
            i += 1;
        }
    }
    i
}

fn skip_js_template(raw: &str, mut i: usize) -> usize {
    let bytes = raw.as_bytes();
    i += 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = (i + 2).min(bytes.len());
        } else if bytes[i] == b'`' {
            return i + 1;
        } else {
            i += 1;
        }
    }
    i
}

fn skip_js_regex(raw: &str, mut i: usize) -> usize {
    let bytes = raw.as_bytes();
    i += 1;
    let mut in_class = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i = (i + 2).min(bytes.len()),
            b'[' => {
                in_class = true;
                i += 1;
            }
            b']' => {
                in_class = false;
                i += 1;
            }
            b'/' if !in_class => {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                return i;
            }
            _ => i += 1,
        }
    }
    i
}

fn skip_js_number(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'.' | b'_')) {
        i += 1;
    }
    i
}

fn word_allows_regex_after(word: &str) -> bool {
    matches!(
        word,
        "return"
            | "throw"
            | "case"
            | "delete"
            | "void"
            | "typeof"
            | "instanceof"
            | "in"
            | "of"
            | "yield"
            | "await"
    )
}

fn punctuation_allows_regex_after(punct: u8) -> bool {
    matches!(
        punct,
        b'(' | b'['
            | b'{'
            | b'='
            | b':'
            | b','
            | b';'
            | b'!'
            | b'?'
            | b'&'
            | b'|'
            | b'+'
            | b'-'
            | b'*'
            | b'%'
            | b'^'
            | b'~'
            | b'<'
            | b'>'
    )
}

fn is_two_byte_js_punctuation(first: u8, second: u8) -> bool {
    matches!(
        (first, second),
        (b'=', b'=')
            | (b'!', b'=')
            | (b'&', b'&')
            | (b'|', b'|')
            | (b'=', b'>')
            | (b'+', b'+')
            | (b'-', b'-')
            | (b'<', b'=')
            | (b'>', b'=')
    )
}

fn raw_js_looks_like_statements(raw: &str) -> bool {
    let scan = scan_raw_js(raw);
    scan.has_top_level_semicolon
        || matches!(
            scan.first_word,
            Some(
                "const"
                    | "let"
                    | "var"
                    | "function"
                    | "class"
                    | "if"
                    | "for"
                    | "while"
                    | "do"
                    | "try"
                    | "switch"
                    | "return"
                    | "throw"
                    | "break"
                    | "continue"
                    | "with"
                    | "debugger"
                    | "import"
                    | "export"
                    | "async"
            )
        )
}

fn js_has_top_level_await(raw: &str) -> bool {
    scan_raw_js(raw).has_await
}

fn source_is_css(source: &str) -> bool {
    let trimmed = source.trim().trim_matches('"').trim_matches('\'');
    let lower = trimmed.to_ascii_lowercase();
    lower.ends_with(".css")
}

fn source_is_math(source: &str) -> bool {
    let trimmed = source.trim().trim_matches('"').trim_matches('\'');
    let bare = trimmed.strip_prefix("@deka/").unwrap_or(trimmed);
    bare == "math"
}

fn visit_stmt_exprs(stmt: &Stmt, visitor: &mut dyn FnMut(&Expr)) {
    match stmt {
        Stmt::TupleBinding { value, .. }
        | Stmt::Const { value, .. }
        | Stmt::Let { value, .. }
        | Stmt::Expr { expr: value, .. }
        | Stmt::Return {
            value: Some(value), ..
        } => visit_expr(value, visitor),
        Stmt::Export { decl, .. } => match decl {
            ExportDecl::Const { value, .. } => visit_expr(value, visitor),
            ExportDecl::Function { body, .. } => {
                for s in body.iter() {
                    visit_stmt_exprs(s, visitor);
                }
            }
            _ => {}
        },
        Stmt::Function { body, .. }
        | Stmt::ReceiverMethod { body, .. }
        | Stmt::For { body, .. }
        | Stmt::ForOf { body, .. } => {
            for s in body.iter() {
                visit_stmt_exprs(s, visitor);
            }
        }
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            for s in then_body.iter() {
                visit_stmt_exprs(s, visitor);
            }
            for s in else_body.iter() {
                visit_stmt_exprs(s, visitor);
            }
        }
        Stmt::Try {
            body, catch_body, ..
        } => {
            for s in body.iter().chain(catch_body.iter()) {
                visit_stmt_exprs(s, visitor);
            }
        }
        Stmt::Block { body, .. } => {
            for s in body.iter() {
                visit_stmt_exprs(s, visitor);
            }
        }
        _ => {}
    }
}

fn visit_expr(expr: &Expr, visitor: &mut dyn FnMut(&Expr)) {
    visitor(expr);
    match expr {
        Expr::Binary { left, right, .. } => {
            visit_expr(left, visitor);
            visit_expr(right, visitor);
        }
        Expr::Unary { operand, .. } => visit_expr(operand, visitor),
        Expr::Call { callee, args, .. } => {
            visit_expr(callee, visitor);
            for a in args.iter() {
                visit_expr(a, visitor);
            }
        }
        Expr::FieldAccess { object, .. }
        | Expr::IndexAccess { object, .. }
        | Expr::Await { expr: object, .. }
        | Expr::Safe { expr: object, .. }
        | Expr::Paren { expr: object, .. }
        | Expr::Spread { expr: object, .. } => visit_expr(object, visitor),
        Expr::StructLiteral { fields, .. } => {
            for f in fields.iter() {
                visit_expr(&f.value, visitor);
            }
        }
        Expr::EnumConstructor { payload, .. } => {
            if let Some(p) = payload {
                visit_expr(p, visitor);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            visit_expr(scrutinee, visitor);
            for arm in arms.iter() {
                if let Some(guard) = &arm.guard {
                    visit_expr(guard, visitor);
                }
                visit_expr(&arm.body, visitor);
            }
        }
        Expr::Array { elements, .. } => {
            for e in elements.iter() {
                visit_expr(e, visitor);
            }
        }
        Expr::Object { fields, .. } => {
            for f in fields.iter() {
                visit_expr(&f.value, visitor);
            }
        }
        Expr::TemplateLiteral { parts, .. } => {
            for part in parts.iter() {
                if let deka_syntax::TemplatePart::Expr(e) = part {
                    visit_expr(e, visitor);
                }
            }
        }
        Expr::Function { body, .. } => {
            for s in body.iter() {
                visit_stmt_exprs(s, visitor);
            }
        }
        // Build-only bodies have a separate import graph and cannot retain a
        // runtime import merely because they reference it.
        Expr::Build { .. } => {}
        Expr::JsxElement { element, .. } => {
            for attr in element.attributes.iter() {
                if let Some(v) = &attr.value {
                    visit_expr(v, visitor);
                }
            }
            for child in element.children.iter() {
                visit_expr(child, visitor);
            }
        }
        Expr::JsxFragment { children, .. } => {
            for child in children.iter() {
                visit_expr(child, visitor);
            }
        }
        _ => {}
    }
}

fn expr_contains_await(expr: &Expr<'_>) -> bool {
    deka_syntax::parse::expr_has_top_level_await(expr)
}

fn peel_exception_parens<'e, 'a>(expr: &'e Expr<'a>) -> &'e Expr<'a> {
    match expr {
        Expr::Paren { expr, .. } | Expr::Safe { expr, .. } => peel_exception_parens(expr),
        other => other,
    }
}

include!("lifting.rs");

// Only bindings and numeric literals are safe to repeat in the inline predicate.
fn has_stable_operand(expr: &Expr<'_>) -> bool {
    match expr {
        Expr::Identifier { .. } | Expr::Number { .. } => true,
        Expr::Paren { expr, .. } => has_stable_operand(expr),
        _ => false,
    }
}
fn has_needs_temporaries(expr: &Expr<'_>) -> bool {
    matches!(expr, Expr::Call { callee, args, .. }
        if matches!(&**callee, Expr::FieldAccess { object, .. }
            if !has_stable_operand(object) || args.iter().any(|arg| !has_stable_operand(arg))))
}
fn has_temporary_names(expr: &Expr<'_>) -> (String, String) {
    let span = expr.span();
    let stem = format!("__deka_index${}_{}", span.byte_start, span.byte_end);
    (format!("{stem}_array"), format!("{stem}_index"))
}
