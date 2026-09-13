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
mod exceptions;
mod expr;
mod indexing;
mod stmt;
#[cfg(test)]
mod tuples_tests;
mod types;

pub use descriptor::{DescriptorField, DescriptorTree, JsonCall, JsonOperation, StaticTypeCall};
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
    if matches!(expected, Type::Named { name: "Component" } | Type::Generic { base: "Component", .. }) {
        return format!("{message}; Component is a props interface or struct → ReactNode function; use ReactNode for a JSX value or return type");
    }

    if matches!(actual, Type::Union { .. }) && !matches!(expected, Type::Union { .. }) {
        format!("{message}; narrow it with a match before use")
    } else {
        message
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExceptionEmit {
    Ok,
    Throw,
    Match,
    ToResult,
    FromResult,
}

/// Checker-owned erasure decisions, including resolved typed-catch aliases.
#[derive(Clone, Default)]
pub struct ExceptionLowering<'a> {
    pub forms: HashMap<*const ast::Expr<'a>, ExceptionEmit>,
    /// Distinguish checked metadata from the legacy parser-only emit API.
    pub checked: bool,
    /// Result-typed values whose reflection must survive representation erasure.
    pub result_values: HashSet<*const ast::Expr<'a>>,
    /// Resolved Result patterns; spelling alone cannot identify a builtin enum.
    pub result_patterns: HashSet<*const ast::Pattern<'a>>,
    pub option_values: HashSet<*const ast::Expr<'a>>,
    pub option_patterns: HashSet<*const ast::Pattern<'a>>,
    pub catches: HashMap<*const ast::Type<'a>, &'a str>,
    pub summons: HashMap<*const ast::Expr<'a>, (Vec<Type<'a>>, Type<'a>, bool)>,
}
impl<'a> std::ops::Deref for ExceptionLowering<'a> {
    type Target = HashMap<*const ast::Expr<'a>, ExceptionEmit>;
    fn deref(&self) -> &Self::Target {
        &self.forms
    }
}
impl<'a> std::ops::DerefMut for ExceptionLowering<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.forms
    }
}

pub struct TypeckResult<'a> {
    pub exception_forms: ExceptionLowering<'a>,
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
    /// Builtin array call sites: accessors produce Option (deka#561,
    /// deka#566); `has` emits an inline predicate and carries a bounds fact
    /// into a guarded branch (rfd#65).
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
    /// Identifier patterns that name a payload-free case of the scrutinee's
    /// enum rather than binding it (deka#450).
    pub enum_case_patterns: HashMap<*const ast::Pattern<'a>, &'a str>,
    /// Constructor patterns that are union member type-patterns (`string(s)`),
    /// mapped to the runtime predicate the emitter must emit (rfd#42).
    pub union_type_patterns: HashMap<*const ast::Pattern<'a>, types::UnionMemberTest<'a>>,
    /// Typed build-only expressions consumed by the compiler into a dev plan.
    pub dev_blocks: HashMap<*const ast::Expr<'a>, DevBlock<'a>>,
}

/// Compiler-owned information for one `build { ... }` initializer.
#[derive(Debug, Clone)]
pub struct DevBlock<'a> {
    pub body: &'a [ast::Stmt<'a>],
    pub descriptor: descriptor::DescriptorTree<'a>,
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
    /// Explicitly exported, erased interface type bindings.
    pub interfaces: HashMap<&'a str, InterfaceInfo<'a>>,
    /// Members resolved in the declaring module, keyed by declaration identity.
    /// Private signature payloads are reachable here but are not importable.
    pub interface_members: HashMap<usize, ExportedInterface<'a>>,
    pub structs: HashMap<&'a str, StructInfo<'a>>,
    /// Compiler-private transitive struct metadata. The outer key is an
    /// exported struct name or a private struct named by an exported value;
    /// the inner map contains that struct and its embeds as needed. Importers
    /// seed this separately from `structs` and consult it only while resolving
    /// fields and promoted literals (dsc#86, dsc#119). In particular, these
    /// entries are not importable or nameable as ordinary types.
    pub promotion_structs: HashMap<&'a str, HashMap<&'a str, StructInfo<'a>>>,
    pub enums: HashMap<&'a str, EnumInfo<'a>>,
    pub aliases: HashMap<&'a str, ast::Type<'a>>,
    pub opaques: HashMap<&'a str, Type<'a>>,
    pub newtypes: HashMap<&'a str, NewtypeInfo>,
    pub receiver_methods: HashMap<(&'a str, &'a str), MethodInfo<'a>>,
    /// Value bindings (functions / constants) exported by the module.
    pub values: HashMap<&'a str, Type<'a>>,
    /// Names exported via `export { name }` that are not locally declared
    /// (i.e. re-exports of imports). These pass through to importers.
    pub re_exports: HashSet<&'a str>,
    /// Compiler-private descriptor fragments for exported struct/enum/newtype
    /// factories, keyed by exported name and computed in the declaring
    /// module's own namespace (so private nested types are visible). Build
    /// hydration in importing modules splices these instead of re-walking the
    /// type through local bindings (dsc#52). Never consulted for ordinary
    /// import validation, so it is invisible in DekaScript module metadata.
    pub build_fragments: HashMap<&'a str, DescriptorTree<'a>>,
    /// Compiler-private receiver methods declared on this module's types,
    /// including types private to the module. Keyed by the declaring-module
    /// receiver name so an importer can typecheck method calls on private
    /// nested values it can name only through a fragment (dsc#52). Like
    /// `build_fragments`, invisible in DekaScript module metadata.
    pub build_receiver_methods: HashMap<(&'a str, &'a str), MethodInfo<'a>>,
    /// Exported components whose rendered subtree uses client-only APIs.
    /// This is checker metadata, not a language-level export surface: importers
    /// use it to diagnose an unhydrated JSX usage at its own tag (dsc#65).
    pub interactive_components: HashSet<&'a str>,
}

impl<'a> Default for ModuleExports<'a> {
    fn default() -> Self {
        Self {
            interfaces: HashMap::new(),
            interface_members: HashMap::new(),
            structs: HashMap::new(),
            promotion_structs: HashMap::new(),
            enums: HashMap::new(),
            aliases: HashMap::new(),
            opaques: HashMap::new(),
            newtypes: HashMap::new(),
            receiver_methods: HashMap::new(),
            values: HashMap::new(),
            re_exports: HashSet::new(),
            build_fragments: HashMap::new(),
            build_receiver_methods: HashMap::new(),
            interactive_components: HashSet::new(),
        }
    }
}

/// Compute every local or imported component binding that requires hydration.
///
/// A component is direct-interactive when it binds an `on*={...}` JSX handler. The result then
/// closes over ordinary (unhydrated) uppercase JSX references, which makes the
/// property transitive through statically resolved components. A child below a
/// `client:*` island is deliberately excluded: its island root hydrates the
/// whole subtree.
pub fn collect_interactive_components<'a>(
    program: &'a Program<'a>,
    imports: &HashMap<&str, &ModuleExports<'a>>,
) -> HashSet<&'a str> {
    let mut interactive = HashSet::new();
    for stmt in program.statements.iter() {
        match stmt {
            ast::Stmt::Import {
                specifiers, source, ..
            } => {
                let Some(exports) = imports.get(source) else {
                    continue;
                };
                for specifier in specifiers.iter() {
                    if exports.interactive_components.contains(specifier.imported) {
                        interactive.insert(specifier.local);
                    }
                }
            }
            ast::Stmt::Export {
                decl:
                    ast::ExportDecl::NamedGroup {
                        names,
                        source: Some(source),
                    },
                ..
            } => {
                let Some(exports) = imports.get(source) else {
                    continue;
                };
                for name in names.iter() {
                    if exports.interactive_components.contains(name.name) {
                        interactive.insert(name.name);
                    }
                }
            }
            _ => {}
        }
    }

    let mut summaries: HashMap<&str, (bool, HashSet<&str>)> = HashMap::new();
    for stmt in program.statements.iter() {
        let Some((name, body)) = component_body(stmt) else {
            continue;
        };
        let mut direct = false;
        let mut references = HashSet::new();
        summarize_statements(body, &mut direct, &mut references, false);
        if direct {
            interactive.insert(name);
        }
        summaries.insert(name, (direct, references));
    }

    loop {
        let mut changed = false;
        for (name, (direct, references)) in &summaries {
            if (*direct
                || references
                    .iter()
                    .any(|reference| interactive.contains(reference)))
                && interactive.insert(name)
            {
                changed = true;
            }
        }
        if !changed {
            return interactive;
        }
    }
}

/// Project a module's local/imported analysis onto the names it exports.
pub fn collect_exported_interactive_components<'a>(
    program: &'a Program<'a>,
    interactive: &HashSet<&'a str>,
) -> HashSet<&'a str> {
    let mut exported = HashSet::new();
    for stmt in program.statements.iter() {
        match stmt {
            ast::Stmt::Export {
                decl: ast::ExportDecl::Function { name, .. },
                ..
            } if interactive.contains(name) => {
                exported.insert(*name);
            }
            ast::Stmt::Export {
                decl: ast::ExportDecl::Const { name, .. },
                ..
            } if interactive.contains(name) => {
                exported.insert(*name);
            }
            ast::Stmt::Export {
                decl: ast::ExportDecl::NamedGroup { names, .. },
                ..
            } => {
                for name in names.iter() {
                    if interactive.contains(name.name) {
                        exported.insert(name.alias.unwrap_or(name.name));
                    }
                }
            }
            _ => {}
        }
    }
    exported
}

fn component_body<'a>(stmt: &'a ast::Stmt<'a>) -> Option<(&'a str, &'a [ast::Stmt<'a>])> {
    match stmt {
        ast::Stmt::Function { name, body, .. }
        | ast::Stmt::Export {
            decl: ast::ExportDecl::Function { name, body, .. },
            ..
        } => Some((*name, *body)),
        ast::Stmt::Const {
            name,
            value: ast::Expr::Function { body, .. },
            ..
        }
        | ast::Stmt::Let {
            name,
            value: ast::Expr::Function { body, .. },
            ..
        }
        | ast::Stmt::Export {
            decl:
                ast::ExportDecl::Const {
                    name,
                    value: ast::Expr::Function { body, .. },
                    ..
                },
            ..
        } => Some((*name, *body)),
        _ => None,
    }
}

fn summarize_statements<'a>(
    statements: &'a [ast::Stmt<'a>],
    direct: &mut bool,
    references: &mut HashSet<&'a str>,
    inside_island: bool,
) {
    for stmt in statements {
        summarize_stmt(stmt, direct, references, inside_island);
    }
}

fn summarize_stmt<'a>(
    stmt: &'a ast::Stmt<'a>,
    direct: &mut bool,
    references: &mut HashSet<&'a str>,
    inside_island: bool,
) {
    match stmt {
        ast::Stmt::Try {
            body, catch_body, ..
        } => {
            summarize_statements(body, direct, references, inside_island);
            summarize_statements(catch_body, direct, references, inside_island);
        }
        ast::Stmt::TupleBinding { value, .. }
        | ast::Stmt::Const { value, .. }
        | ast::Stmt::Let { value, .. } => summarize_expr(value, direct, references, inside_island),
        ast::Stmt::UnwrapLet {
            scrutinee,
            alternative,
            ..
        } => {
            summarize_expr(scrutinee, direct, references, inside_island);
            match alternative {
                ast::UnwrapAlternative::Block(body) => {
                    summarize_statements(body, direct, references, inside_island)
                }
                ast::UnwrapAlternative::Match(arms) => {
                    for arm in arms.iter() {
                        if let Some(guard) = &arm.guard {
                            summarize_expr(guard, direct, references, inside_island);
                        }
                        summarize_expr(&arm.body, direct, references, inside_island);
                    }
                }
            }
        }
        ast::Stmt::Function { body, .. }
        | ast::Stmt::ReceiverMethod { body, .. }
        | ast::Stmt::Block { body, .. } => {
            summarize_statements(body, direct, references, inside_island)
        }
        ast::Stmt::Export {
            decl: ast::ExportDecl::Const { value, .. },
            ..
        } => summarize_expr(value, direct, references, inside_island),
        ast::Stmt::Export {
            decl: ast::ExportDecl::Function { body, .. },
            ..
        } => summarize_statements(body, direct, references, inside_island),
        ast::Stmt::Expr { expr, .. } => summarize_expr(expr, direct, references, inside_island),
        ast::Stmt::Return { value, .. } => {
            if let Some(value) = value {
                summarize_expr(value, direct, references, inside_island);
            }
        }
        ast::Stmt::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            summarize_expr(condition, direct, references, inside_island);
            summarize_statements(then_body, direct, references, inside_island);
            summarize_statements(else_body, direct, references, inside_island);
        }
        ast::Stmt::For {
            init,
            condition,
            step,
            body,
            ..
        } => {
            if let Some(init) = init {
                match init {
                    ast::ForInit::Const { value, .. } | ast::ForInit::Let { value, .. } => {
                        summarize_expr(value, direct, references, inside_island)
                    }
                    ast::ForInit::Expr(expr) => {
                        summarize_expr(expr, direct, references, inside_island)
                    }
                }
            }
            if let Some(condition) = condition {
                summarize_expr(condition, direct, references, inside_island);
            }
            if let Some(step) = step {
                summarize_expr(step, direct, references, inside_island);
            }
            summarize_statements(body, direct, references, inside_island);
        }
        ast::Stmt::ForOf { iterable, body, .. } => {
            summarize_expr(iterable, direct, references, inside_island);
            summarize_statements(body, direct, references, inside_island);
        }
        ast::Stmt::Import { .. }
        | ast::Stmt::Struct { .. }
        | ast::Stmt::Enum { .. }
        | ast::Stmt::TypeAlias { .. }
        | ast::Stmt::Opaque { .. }
        | ast::Stmt::Summon { .. }
        | ast::Stmt::Newtype { .. }
        | ast::Stmt::Interface { .. }
        | ast::Stmt::Break { .. }
        | ast::Stmt::Continue { .. }
        | ast::Stmt::Empty { .. }
        | ast::Stmt::Export {
            decl: ast::ExportDecl::NamedGroup { .. },
            ..
        } => {}
    }
}

fn summarize_expr<'a>(
    expr: &'a ast::Expr<'a>,
    direct: &mut bool,
    references: &mut HashSet<&'a str>,
    inside_island: bool,
) {
    match expr {
        ast::Expr::Call { callee, args, .. } => {
            summarize_expr(callee, direct, references, inside_island);
            for arg in args.iter() {
                summarize_expr(arg, direct, references, inside_island);
            }
        }
        ast::Expr::Binary { left, right, .. } => {
            summarize_expr(left, direct, references, inside_island);
            summarize_expr(right, direct, references, inside_island);
        }
        ast::Expr::Unary { operand, .. }
        | ast::Expr::Await { expr: operand, .. }
        | ast::Expr::Spread { expr: operand, .. }
        | ast::Expr::Safe { expr: operand, .. }
        | ast::Expr::Paren { expr: operand, .. } => {
            summarize_expr(operand, direct, references, inside_island)
        }
        ast::Expr::FieldAccess { object, .. } => {
            summarize_expr(object, direct, references, inside_island)
        }
        ast::Expr::IndexAccess { object, index, .. } => {
            summarize_expr(object, direct, references, inside_island);
            summarize_expr(index, direct, references, inside_island);
        }
        ast::Expr::StructLiteral { fields, .. } => {
            for field in fields.iter() {
                summarize_expr(&field.value, direct, references, inside_island);
            }
        }
        ast::Expr::EnumConstructor { payload, .. } => {
            if let Some(payload) = payload {
                summarize_expr(payload, direct, references, inside_island);
            }
        }
        ast::Expr::Match {
            scrutinee, arms, ..
        } => {
            summarize_expr(scrutinee, direct, references, inside_island);
            for arm in arms.iter() {
                if let Some(guard) = &arm.guard {
                    summarize_expr(guard, direct, references, inside_island);
                }
                summarize_expr(&arm.body, direct, references, inside_island);
            }
        }
        ast::Expr::Build { body, .. } | ast::Expr::Function { body, .. } => {
            summarize_statements(body, direct, references, inside_island)
        }
        ast::Expr::Bridge { args, .. } => {
            for arg in args.iter() {
                summarize_expr(arg, direct, references, inside_island);
            }
        }
        ast::Expr::Ternary {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            summarize_expr(condition, direct, references, inside_island);
            summarize_expr(then_branch, direct, references, inside_island);
            summarize_expr(else_branch, direct, references, inside_island);
        }
        ast::Expr::JsxElement { element, .. } => {
            let has_client_directive = element
                .attributes
                .iter()
                .any(|attribute| attribute.name.starts_with("client:"));
            let in_hydrated_subtree = inside_island || has_client_directive;
            if !in_hydrated_subtree && element.tag.chars().next().is_some_and(char::is_uppercase) {
                references.insert(element.tag);
            }
            for attribute in element.attributes.iter() {
                if is_event_handler(attribute) {
                    *direct = true;
                }
                if let Some(value) = &attribute.value {
                    summarize_expr(value, direct, references, in_hydrated_subtree);
                }
            }
            for child in element.children.iter() {
                summarize_expr(child, direct, references, in_hydrated_subtree);
            }
        }
        ast::Expr::JsxFragment { children, .. }
        | ast::Expr::Array {
            elements: children, ..
        } => {
            for child in children.iter() {
                summarize_expr(child, direct, references, inside_island);
            }
        }
        ast::Expr::Object { fields, .. } => {
            for field in fields.iter() {
                summarize_expr(&field.value, direct, references, inside_island);
            }
        }
        ast::Expr::TemplateLiteral { parts, .. } => {
            for part in parts.iter() {
                if let ast::TemplatePart::Expr(expr) = part {
                    summarize_expr(expr, direct, references, inside_island);
                }
            }
        }
        ast::Expr::Number { .. }
        | ast::Expr::BigInt { .. }
        | ast::Expr::String { .. }
        | ast::Expr::Boolean { .. }
        | ast::Expr::None { .. }
        | ast::Expr::Identifier { .. }
        | ast::Expr::Unsafe { .. }
        | ast::Expr::JsxText { .. } => {}
    }
}

fn is_event_handler(attribute: &ast::JsxAttribute<'_>) -> bool {
    attribute.value.is_some()
        && attribute
            .name
            .strip_prefix("on")
            .is_some_and(|suffix| suffix.chars().next().is_some_and(char::is_alphabetic))
}

/// True when any JSX element in this program carries a `client:*` island
/// directive. `ui/client` walk-and-attach hydrate is the only consumer of
/// the `data-deka-id` host-element marker, and it only runs for islands.
pub fn program_has_client_directive(program: &Program<'_>) -> bool {
    let mut found = false;
    for stmt in program.statements.iter() {
        crate::visit::walk_stmt(stmt, &mut |expr| {
            if let ast::Expr::JsxElement { element, .. } = expr {
                if element
                    .attributes
                    .iter()
                    .any(|attribute| attribute.name.starts_with("client:"))
                {
                    found = true;
                }
            }
        });
        if found {
            break;
        }
    }
    found
}

/// True when this module needs hydration markers on host JSX: a `client:*`
/// directive is present, or interactive-component analysis identified a
/// component that requires a hydrated island (including through imports).
pub fn program_needs_hydration_ids<'a>(
    program: &'a Program<'a>,
    imports: &HashMap<&str, &ModuleExports<'a>>,
) -> bool {
    program_has_client_directive(program)
        || !collect_interactive_components(program, imports).is_empty()
}

/// The kind of a named factory an importer can reach only through a
/// descriptor fragment (dsc#52). Lets the consumer's checker resolve field
/// and receiver types that are private to the declaring module.
#[derive(Clone, Copy, Debug)]
enum BuildFactoryKind {
    Struct,
    Enum,
    Newtype(crate::ast::NewtypeRepr),
}

impl BuildFactoryKind {
    fn to_type<'a>(self, name: &'a str) -> Type<'a> {
        match self {
            BuildFactoryKind::Struct => Type::Struct { name },
            BuildFactoryKind::Enum => Type::Named { name },
            BuildFactoryKind::Newtype(repr) => Type::Newtype { name, repr },
        }
    }
}

/// Walk a descriptor fragment recording the kind of every named factory.
fn collect_fragment_factory_kinds<'a>(
    tree: &DescriptorTree<'a>,
    out: &mut HashMap<&'a str, BuildFactoryKind>,
) {
    match tree {
        DescriptorTree::Struct { name, fields } => {
            out.entry(name).or_insert(BuildFactoryKind::Struct);
            for field in fields {
                collect_fragment_factory_kinds(&field.ty, out);
            }
        }
        DescriptorTree::Newtype { name, repr } => {
            let repr_kind = match **repr {
                DescriptorTree::Leaf { kind, .. } => match kind {
                    "number" => crate::ast::NewtypeRepr::Number,
                    "boolean" => crate::ast::NewtypeRepr::Bool,
                    _ => crate::ast::NewtypeRepr::String,
                },
                _ => crate::ast::NewtypeRepr::Number,
            };
            out.entry(name)
                .or_insert(BuildFactoryKind::Newtype(repr_kind));
            collect_fragment_factory_kinds(repr, out);
        }
        DescriptorTree::Enum { name, cases } => {
            out.entry(name).or_insert(BuildFactoryKind::Enum);
            for (_, payload) in cases {
                if let Some(payload) = payload {
                    collect_fragment_factory_kinds(payload, out);
                }
            }
        }
        DescriptorTree::Array { elem } | DescriptorTree::Option { inner: elem } => {
            collect_fragment_factory_kinds(elem, out);
        }
        DescriptorTree::Tuple { elements: members } | DescriptorTree::Union { members } => {
            for member in members {
                collect_fragment_factory_kinds(member, out);
            }
        }
        DescriptorTree::Leaf { .. }
        | DescriptorTree::Recurse { .. }
        | DescriptorTree::Interface { .. } => {}
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
        exception_forms: checker.exception_forms,
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
        enum_case_patterns: checker.enum_case_patterns,
        union_type_patterns: checker.union_type_patterns,
        dev_blocks: checker.dev_blocks,
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

/// Infer the concrete types of this module's top-level value bindings without
/// emitting diagnostics.
///
/// Export collection uses this for constants as well as the existing function
/// signature pass. It checks source-ordered initializers after function
/// signatures are known, so local functions and earlier constants participate.
/// The module graph supplies resolved imports on its later refresh pass.
pub fn infer_module_value_types<'a>(
    program: &'a Program<'a>,
    imports: &HashMap<&str, &ModuleExports<'a>>,
) -> HashMap<&'a str, Type<'a>> {
    let mut checker = Checker::new(program, imports);
    checker.infer_only = true;
    checker.check_program();
    checker.scopes.first().cloned().unwrap_or_default()
}

/// A module boundary must carry a concrete type. An unresolved member inside
/// a container is as contagious as a top-level `Infer`.
pub(crate) fn is_concrete_export_type(ty: &Type<'_>) -> bool {
    match ty {
        Type::Infer | Type::Var | Type::Error => false,
        Type::Option { inner } | Type::Array { elem: inner } => is_concrete_export_type(inner),
        Type::Function { params, ret, .. } => {
            params.iter().all(is_concrete_export_type) && is_concrete_export_type(ret)
        }
        Type::Generic { args, .. }
        | Type::Tuple { elements: args }
        | Type::Union { members: args } => args.iter().all(is_concrete_export_type),
        Type::Object { fields } => fields
            .iter()
            .all(|(_, field_type)| is_concrete_export_type(field_type)),
        _ => true,
    }
}

fn collect_struct_type_names<'a>(ty: &Type<'a>, names: &mut HashSet<&'a str>) {
    match ty {
        Type::Struct { name } => {
            names.insert(*name);
        }
        Type::Option { inner } | Type::Array { elem: inner } => {
            collect_struct_type_names(inner, names)
        }
        Type::Function { params, ret, .. } => {
            for param in params {
                collect_struct_type_names(param, names);
            }
            collect_struct_type_names(ret, names);
        }
        Type::Generic { base, args } => {
            // The same representation is used for `Array<T>` and a generic
            // struct; callers retain only names present in their struct map.
            names.insert(*base);
            for arg in args {
                collect_struct_type_names(arg, names);
            }
        }
        Type::Tuple { elements: args } | Type::Union { members: args } => {
            for arg in args {
                collect_struct_type_names(arg, names);
            }
        }
        Type::Object { fields } => {
            for (_, field_type) in fields {
                collect_struct_type_names(field_type, names);
            }
        }
        _ => {}
    }
}

/// Refresh exported constants after dependency exports are available.
///
/// Initial collection has no import map while the graph is still discovered.
/// The graph updates the ordinary `ModuleExports::values` map in dependency
/// order, rather than introducing a second export representation.
pub fn refresh_module_export_values<'a>(
    program: &'a Program<'a>,
    imports: &HashMap<&str, &ModuleExports<'a>>,
    exports: &mut ModuleExports<'a>,
) {
    let mut checker = Checker::new(program, imports);
    checker.infer_only = true;
    checker.check_program();
    let mut inferred_values = checker.globals.clone();
    inferred_values.extend(checker.scopes.first().cloned().unwrap_or_default());
    // Resolve members before leaving the defining namespace. This preserves
    // aliases, nested/recursive interfaces, and same-spelled foreign types.
    let declarations: Vec<_> = checker
        .interfaces
        .iter()
        .map(|(name, info)| (*name, info.clone()))
        .collect();
    for (name, info) in declarations {
        let identity = info.members.as_ptr() as usize;
        if checker.interface_members.contains_key(&identity) {
            continue;
        }
        let fields = info
            .members
            .iter()
            .filter_map(|member| {
                let field = match member {
                    ast::InterfaceMember::Field { name, .. }
                    | ast::InterfaceMember::Method { name, .. } => *name,
                };
                checker
                    .resolve_interface_field(name, field)
                    .map(|ty| (field, ty))
            })
            .collect();
        checker.interface_members.insert(
            identity,
            ExportedInterface {
                info,
                members: fields,
            },
        );
    }
    exports
        .interface_members
        .extend(checker.interface_members.clone());
    // Refresh functions too: their inferred returns may depend on imports.
    for stmt in program.statements.iter() {
        let names: Vec<_> = match stmt {
            ast::Stmt::Export {
                decl: ast::ExportDecl::Function { name, .. },
                ..
            } => vec![(*name, *name)],
            ast::Stmt::Export {
                decl:
                    ast::ExportDecl::NamedGroup {
                        names,
                        source: None,
                    },
                ..
            } => names
                .iter()
                .map(|n| (n.name, n.alias.unwrap_or(n.name)))
                .collect(),
            _ => Vec::new(),
        };
        for (local, external) in names {
            if let Some(inferred @ Type::Function { .. }) = inferred_values.get(local) {
                exports.values.insert(external, inferred.clone());
            }
        }
    }

    let declared_structs: HashMap<&str, StructInfo<'a>> = program
        .statements
        .iter()
        .filter_map(|stmt| match stmt {
            ast::Stmt::Struct {
                name,
                fields,
                embeds,
                type_params,
                is_super,
                ..
            } => Some((
                *name,
                StructInfo {
                    fields: *fields,
                    embeds: *embeds,
                    type_params,
                    is_super: *is_super,
                },
            )),
            _ => None,
        })
        .collect();
    fn collect_struct_closure<'a>(
        info: &StructInfo<'a>,
        declared: &HashMap<&'a str, StructInfo<'a>>,
        closure: &mut HashMap<&'a str, StructInfo<'a>>,
        seen: &mut HashSet<&'a str>,
    ) {
        for embed in info.embeds {
            if !seen.insert(embed.name) {
                continue;
            }
            let Some(embed_info) = declared.get(embed.name) else {
                continue;
            };
            closure.insert(embed.name, embed_info.clone());
            collect_struct_closure(embed_info, declared, closure, seen);
        }
    }
    let local_constants: HashSet<&str> = program
        .statements
        .iter()
        .flat_map(|stmt| match stmt {
            ast::Stmt::Const { name, .. } | ast::Stmt::Let { name, .. } => vec![*name],
            ast::Stmt::Export {
                decl: ast::ExportDecl::Const { name, .. },
                ..
            } => vec![*name],
            ast::Stmt::TupleBinding { names, .. } => names.to_vec(),
            _ => Vec::new(),
        })
        .collect();
    let exported_constants: Vec<(&str, &str)> = program
        .statements
        .iter()
        .flat_map(|stmt| match stmt {
            ast::Stmt::Export {
                decl: ast::ExportDecl::Const { name, .. },
                ..
            } => vec![(*name, *name)],
            ast::Stmt::Export {
                decl:
                    ast::ExportDecl::NamedGroup {
                        names,
                        source: None,
                    },
                ..
            } => names
                .iter()
                .filter(|name| local_constants.contains(name.name))
                .map(|name| (name.name, name.alias.unwrap_or(name.name)))
                .collect(),
            _ => Vec::new(),
        })
        .collect();
    for (local, exported) in exported_constants {
        let ty = inferred_values.get(local).cloned().unwrap_or(Type::Error);
        let concrete = is_concrete_export_type(&ty);
        exports
            .values
            .insert(exported, if concrete { ty.clone() } else { Type::Error });
        if concrete {
            let mut names = HashSet::new();
            collect_struct_type_names(&ty, &mut names);
            for name in names {
                let Some(info) = declared_structs.get(name) else {
                    continue;
                };
                let mut closure = HashMap::new();
                closure.insert(name, info.clone());
                collect_struct_closure(info, &declared_structs, &mut closure, &mut HashSet::new());
                exports.promotion_structs.insert(name, closure);
            }
        }
    }
}

/// Build compiler-private descriptor fragments for a module's declared
/// factories, in the declaring module's own namespace.
///
/// A fragment is the descriptor tree of one declared struct/enum/newtype.
/// Because it is computed here — where every type the declaration references
/// is in scope, including types private to this module — a fragment can name
/// factories that importers have no lexical binding for. Module-graph
/// compilation stores these on [`ModuleExports::build_fragments`] under the
/// exported name; an importer's build hydration splices them in place of a
/// local re-walk (dsc#52). Generic and undescribable declarations are skipped:
/// importers fall back to their local walk, which preserves today's behavior.
pub fn build_module_build_fragments<'a>(
    program: &'a Program<'a>,
    imports: &HashMap<&str, &ModuleExports<'a>>,
) -> HashMap<&'a str, DescriptorTree<'a>> {
    let mut checker = Checker::new(program, imports);
    checker.build_declared_build_fragments()
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
                name,
                fields,
                embeds,
                type_params,
                is_super,
                ..
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
                receiver_type_args,
                name,
                type_params,
                params,
                return_type,
                ..
            } => {
                receiver_methods.insert(
                    (*receiver_type, *name),
                    MethodInfo {
                        params: *params,
                        receiver_type_args: *receiver_type_args,
                        type_params: *type_params,
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
                Some(info) => info.fields.iter().map(|f| &f.ty).collect(),
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
                export_type_ast_refs(
                    ty,
                    &declared_aliases,
                    &declared_structs,
                    &declared_enums,
                    &mut refs,
                    0,
                );
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
                "ReactNode" => Type::react_node(),
                "number" | "string" | "boolean" | "never" | "void" | "bytes" | "Component"
                | "JsError" | "SyntaxError" | "TypeError" | "RangeError" | "Error" | "Type" => {
                    Type::Named { name }
                }
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
                } else if (*base == "Result" || *base == "Exception" || *base == "Promise")
                    && args.len() <= 2
                {
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
            ast::Type::Tuple { elements, .. } => Type::Tuple {
                elements: elements
                    .iter()
                    .map(|t| ast_type_to_export_type(t, structs, enums, aliases, newtypes, seen))
                    .collect(),
            },
            ast::Type::Record { .. } => Type::Error,
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
            ast::Stmt::TupleBinding { names, .. } => {
                for name in *names {
                    declared_values.insert(
                        *name,
                        inferred_globals.get(name).cloned().unwrap_or(Type::Infer),
                    );
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

    // Keep the representation of public `StructInfo` unchanged. An exported
    // struct can nevertheless promote fields of a private embed, so retain a
    // private, transitive lookup closure alongside that exported struct. It is
    // deliberately not added to `exports.structs`: doing so would make the
    // embedded declarations importable from consumers.
    fn collect_promotion_structs<'a>(
        info: &StructInfo<'a>,
        declared_structs: &HashMap<&'a str, StructInfo<'a>>,
        out: &mut HashMap<&'a str, StructInfo<'a>>,
        seen: &mut HashSet<&'a str>,
    ) {
        for embed in info.embeds {
            if !seen.insert(embed.name) {
                continue;
            }
            let Some(embed_info) = declared_structs.get(embed.name) else {
                continue;
            };
            out.insert(embed.name, embed_info.clone());
            collect_promotion_structs(embed_info, declared_structs, out, seen);
        }
    }

    // Compiler-private receiver methods for every declared receiver type,
    // including private ones (dsc#52): an importer can name a private nested
    // type only through a descriptor fragment, and it needs these entries to
    // typecheck method calls on such values. Keyed by declared name; the
    // importer's checker re-keys promoted entries under the local binding.
    for ((receiver, method), info) in receiver_methods.iter() {
        exports
            .build_receiver_methods
            .insert((*receiver, *method), info.clone());
    }

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

                    if let Some((members, type_params, span)) =
                        program.statements.iter().find_map(|stmt| match stmt {
                            ast::Stmt::Interface {
                                name,
                                members,
                                type_params,
                                span,
                                ..
                            } if *name == local => Some((*members, *type_params, *span)),
                            _ => None,
                        })
                    {
                        exports.interfaces.insert(
                            external,
                            InterfaceInfo {
                                members,
                                type_params,
                                span,
                            },
                        );
                    }
                    if let Some(info) = declared_structs.get(local) {
                        exports.structs.insert(external, info.clone());
                        let mut closure = HashMap::new();
                        collect_promotion_structs(
                            info,
                            &declared_structs,
                            &mut closure,
                            &mut HashSet::new(),
                        );
                        if !closure.is_empty() {
                            exports.promotion_structs.insert(external, closure);
                        }
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
                    if let Some(stmt) = program.statements.iter().find(
                        |stmt| matches!(stmt, ast::Stmt::Opaque { name, .. } if *name == local),
                    ) {
                        exports.opaques.insert(
                            external,
                            Type::Opaque {
                                name: local,
                                identity: stmt as *const _ as usize,
                            },
                        );
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
                    if !exports.interfaces.contains_key(external)
                        && !declared_structs.contains_key(local)
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

    // Direct callers can infer literals and local expressions. Module graph
    // compilation refreshes these same values with dependency imports.
    refresh_module_export_values(program, &HashMap::new(), &mut exports);
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
    /// Type parameters bound by the receiver type, e.g. `T` in
    /// `fn (s Signal<T>) get() T` (rfd#56, dsc#101). Introduced by the
    /// receiver and bound from the receiver value's type arguments at each
    /// call site. Empty when the receiver is not generic.
    pub receiver_type_args: &'a [ast::TypeParam<'a>],
    /// Declared type parameters, e.g. `T` in `fn (s Signal) set<T>(next: T)`.
    /// On a generic struct receiver the parameters bind positionally to the
    /// receiver value's type arguments at each call site (rfd#56 phase 1).
    /// Superseded by `receiver_type_args`: declaring both is an error, since
    /// the parameter is bound by the receiver, not the method.
    pub type_params: &'a [ast::TypeParam<'a>],
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

/// Compiler-private interface definition with types resolved at its origin.
#[derive(Clone, Debug)]
pub struct ExportedInterface<'a> {
    pub info: InterfaceInfo<'a>,
    pub members: Vec<(&'a str, Type<'a>)>,
}

/// Information about an interface's declared members.
#[derive(Clone, Debug)]
pub struct InterfaceInfo<'a> {
    pub members: &'a [ast::InterfaceMember<'a>],
    /// Declared type parameters, e.g. `T` in `interface Container<T>`. Generic
    /// interface *use* (`Container<number>`) is not instantiated in phase 1
    /// (rfd#56); the parameters are kept so member validation can resolve
    /// them instead of reporting `unknown type T`.
    pub type_params: &'a [ast::TypeParam<'a>],
    pub span: ast::Span,
}

struct Checker<'a> {
    index_flow: indexing::IndexFlow,
    program: &'a ast::Program<'a>,
    errors: Vec<Diagnostic>,
    warnings: Vec<Diagnostic>,
    /// Component bindings known to require a hydrated client island. Includes
    /// imported bindings whose exporting module computed the same fact.
    interactive_components: HashSet<&'a str>,
    /// A `client:*` root hydrates all JSX below it, so descendant component
    /// tags must not be diagnosed again.
    jsx_island_depth: usize,
    /// An interactive component's own child tags are covered by hydration at
    /// its eventual usage site. Diagnose that outer usage instead of requiring
    /// every descendant tag to repeat the same directive.
    interactive_component_depth: usize,
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
    /// Transitive private embeds carried by an imported exported struct. This
    /// stays separate from `structs` so a consumer cannot name a private embed
    /// in an annotation, import, or direct struct literal (dsc#86).
    promotion_structs: HashMap<&'a str, HashMap<&'a str, StructInfo<'a>>>,
    /// User-defined interfaces.
    interfaces: HashMap<&'a str, InterfaceInfo<'a>>,
    interface_members: HashMap<usize, ExportedInterface<'a>>,
    interface_comparisons: HashSet<(usize, usize)>,
    /// User-defined newtypes.
    opaques: HashMap<&'a str, Type<'a>>,
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
    enum_case_patterns: HashMap<*const ast::Pattern<'a>, &'a str>,
    /// Union member type-pattern sites to lower, keyed by pattern pointer
    /// (rfd#42, deka#530).
    union_type_patterns: HashMap<*const ast::Pattern<'a>, types::UnionMemberTest<'a>>,
    /// `build { ... }` bodies and type descriptors keyed by expression pointer.
    dev_blocks: HashMap<*const ast::Expr<'a>, DevBlock<'a>>,
    /// Descriptor fragments for imported factories, keyed by the local import
    /// binding. Spliced wholesale when a build descriptor (or any descriptor
    /// walk) reaches an imported type, so private nested types of the
    /// declaring module keep their real descriptor nodes (dsc#52).
    build_fragments: HashMap<&'a str, DescriptorTree<'a>>,
    /// Kinds of factories reachable only through imported descriptor
    /// fragments, keyed by the declaring-module name (dsc#52).
    build_factory_kinds: HashMap<&'a str, BuildFactoryKind>,
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
    /// Declared bounds of the type parameters in scope, parallel to
    /// `type_scopes` (rfd#56 phase 2). A param with no bound has no entry.
    /// Empty scopes are pushed for parameterless declarations too, so this
    /// stays index-parallel with `type_scopes`.
    param_bounds: Vec<HashMap<&'a str, Type<'a>>>,
    /// Resolved type-parameter bounds, cached per bound AST node so the
    /// several passes that push the same declaration report an unresolvable
    /// bound exactly once. `Error` results from the silent inference pass
    /// are deliberately not cached (they would mask the real diagnostic).
    bound_cache: HashMap<*const ast::Type<'a>, Type<'a>>,
    /// Declared bounds of top-level generic functions, keyed by function
    /// name, for the call-site check that runs after inference solves the
    /// type arguments (rfd#56 phase 2).
    fn_param_bounds: HashMap<&'a str, Vec<(&'a str, Type<'a>)>>,
    /// Are we currently inside a function body?
    in_function: bool,
    /// Are we currently inside an async function body?
    in_async_function: bool,
    /// Expected / inferred return type of the current function.
    exception_forms: ExceptionLowering<'a>,
    exception_use: exceptions::Use,
    exception_expected: Option<Type<'a>>,
    exception_catches: Vec<Vec<Type<'a>>>,
    return_type: Option<Type<'a>>,
    /// How many nested loops currently enclose the checked statement?
    loop_depth: usize,
    /// When true, diagnostics are suppressed. Used during the pre-check
    /// inference pass that resolves forward-referenced function return types.
    infer_only: bool,
}

impl<'a> Checker<'a> {
    fn new(program: &'a ast::Program<'a>, imports: &HashMap<&str, &ModuleExports<'a>>) -> Self {
        let interactive_components = collect_interactive_components(program, imports);
        let mut this = Self {
            program,
            errors: Vec::new(),
            warnings: Vec::new(),
            interactive_components,
            jsx_island_depth: 0,
            interactive_component_depth: 0,
            globals: HashMap::new(),
            aliases: HashMap::new(),
            enums: HashMap::new(),
            case_to_enum: HashMap::new(),
            structs: HashMap::new(),
            promotion_structs: HashMap::new(),
            interfaces: HashMap::new(),
            interface_members: HashMap::new(),
            interface_comparisons: HashSet::new(),
            opaques: HashMap::new(),
            newtypes: HashMap::new(),
            receiver_methods: HashMap::new(),
            method_calls: HashMap::new(),
            type_of_calls: HashSet::new(),
            signature_calls: HashMap::new(),
            static_type_calls: HashMap::new(),
            super_trees: HashMap::new(),
            json_calls: HashMap::new(),
            index_flow: indexing::IndexFlow::default(),
            array_builtin_calls: HashMap::new(),
            number_math_calls: HashMap::new(),
            unwrap_calls: HashMap::new(),
            operator_rewrites: HashMap::new(),
            enum_case_patterns: HashMap::new(),
            union_type_patterns: HashMap::new(),
            dev_blocks: HashMap::new(),
            build_fragments: HashMap::new(),
            build_factory_kinds: HashMap::new(),
            scopes: vec![HashMap::new()],
            mutables: vec![HashSet::new()],
            pending_module_bindings: HashSet::new(),
            type_scopes: Vec::new(),
            param_bounds: Vec::new(),
            bound_cache: HashMap::new(),
            fn_param_bounds: HashMap::new(),
            in_function: false,
            in_async_function: false,
            exception_forms: ExceptionLowering {
                checked: true,
                ..Default::default()
            },
            exception_use: exceptions::Use::Value,
            exception_expected: None,
            exception_catches: Vec::new(),
            return_type: None,
            loop_depth: 0,
            infer_only: false,
        };
        this.seed_imports(imports);
        this.seed_builtins();
        this
    }

    fn seed_builtins(&mut self) {
        // `deka` is the host-capability global (`deka.ui.State.create`, …),
        // settled by DS decision #6: host capabilities hang off the `deka`
        // global, language and stdlib stay imports. It is declared here so
        // the native and browser (wasm) compilers agree on it (deka#481).
        self.globals.insert("deka", Type::Infer);
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
                for spec in specifiers.iter() {
                    self.error_span(
                        spec.span,
                        format!(
                            "cannot resolve imported name `{}` from `{}`",
                            spec.imported, source
                        ),
                    );
                    // Continue from an error boundary so the unresolved name
                    // cannot create an assignable `Infer` region.
                    self.declare_var(spec.local, Type::Error);
                }
                continue;
            };
            self.interface_members
                .extend(exports.interface_members.clone());
            // Private factories named by the dependency's fragments are
            // type-visible here for build hydration purposes (dsc#52), and so
            // are the receiver methods declared on them.
            let mut kinds = HashMap::new();
            for tree in exports.build_fragments.values() {
                collect_fragment_factory_kinds(tree, &mut kinds);
            }
            for (name, kind) in kinds {
                self.build_factory_kinds.entry(name).or_insert(kind);
            }
            for ((receiver, method), info) in exports.build_receiver_methods.iter() {
                self.receiver_methods
                    .entry((*receiver, *method))
                    .or_insert_with(|| info.clone());
                if let Some(local) = specifiers
                    .iter()
                    .find(|spec| spec.imported == *receiver)
                    .map(|spec| spec.local)
                {
                    self.receiver_methods.insert((local, *method), info.clone());
                }
            }
            for spec in specifiers.iter() {
                let imported = spec.imported;
                let local = spec.local;

                if let Some(info) = exports.interfaces.get(imported) {
                    self.interfaces.insert(local, info.clone());
                }
                if let Some(info) = exports.structs.get(imported) {
                    self.structs.insert(local, info.clone());
                    if let Some(closure) = exports.promotion_structs.get(imported) {
                        self.promotion_structs.insert(local, closure.clone());
                    }
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

                if let Some(ty) = exports.opaques.get(imported) {
                    self.opaques.insert(local, ty.clone());
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
                    self.declare_var(local, exceptions::localize_export(ty, specifiers, exports));
                    // A value can carry a private struct type (for example,
                    // `export const origin = Point { ... }`). Reuse the
                    // compiler-private closure from dsc#86 for field lookup
                    // without adding the struct to `self.structs`, which
                    // would make it nameable or constructible by importers.
                    let mut names = HashSet::new();
                    collect_struct_type_names(ty, &mut names);
                    for name in names {
                        if let Some(closure) = exports.promotion_structs.get(name) {
                            self.promotion_structs.insert(name, closure.clone());
                        }
                    }
                }

                if let Some(tree) = exports.build_fragments.get(imported) {
                    // A renamed import binds the fragment under the local
                    // name, so the spliced descriptor must root under the
                    // local name too (dsc#51). Nested factory references keep
                    // the declaring module's names — hydration obtains those
                    // through the module's factory closure (dsc#52).
                    let mut tree = tree.clone();
                    if local != imported {
                        match &mut tree {
                            DescriptorTree::Struct { name, .. }
                            | DescriptorTree::Enum { name, .. }
                            | DescriptorTree::Newtype { name, .. } => *name = local,
                            _ => {}
                        }
                    }
                    self.build_fragments.insert(local, tree);
                }

                let known = exports.interfaces.contains_key(imported)
                    || exports.values.contains_key(imported)
                    || exports.structs.contains_key(imported)
                    || exports.enums.contains_key(imported)
                    || exports.aliases.contains_key(imported)
                    || exports.opaques.contains_key(imported)
                    || exports.newtypes.contains_key(imported);
                if !known {
                    self.error_span(
                        spec.span,
                        format!(
                            "cannot resolve imported name `{}` from `{}`",
                            imported, source
                        ),
                    );
                    self.declare_var(local, Type::Error);
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
        self.exception_forms.clear();
        self.exception_forms.catches.clear();
        self.exception_forms.result_values.clear();
        self.exception_forms.result_patterns.clear();
        self.exception_forms.option_values.clear();
        self.exception_forms.option_patterns.clear();
        self.method_calls.clear();
        self.type_of_calls.clear();
        self.signature_calls.clear();
        self.static_type_calls.clear();
        self.json_calls.clear();
        self.array_builtin_calls.clear();
        self.index_flow = indexing::IndexFlow::default();
        self.number_math_calls.clear();
        self.unwrap_calls.clear();
        self.operator_rewrites.clear();
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn declare_var(&mut self, name: &'a str, ty: Type<'a>) {
        self.index_flow.shadow(name);
        if self.program.statements.iter().any(|stmt| matches!(stmt, ast::Stmt::Summon { functions, .. } if functions.iter().any(|f| f.name == name))) {
            self.error_span(ast::Span::dummy(), format!("cannot shadow summoned binding `{name}`"));
        }
        self.scopes.last_mut().unwrap().insert(name, ty);
    }

    fn declare_mutable_var(&mut self, name: &'a str, ty: Type<'a>) {
        self.index_flow.shadow(name);
        if self.program.statements.iter().any(|stmt| matches!(stmt, ast::Stmt::Summon { functions, .. } if functions.iter().any(|f| f.name == name))) {
            self.error_span(ast::Span::dummy(), format!("cannot shadow summoned binding `{name}`"));
        }
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

    /// Match and conditional arms share the first returning arm's type.
    /// A diverging arm contributes no type, regardless of its position.
    fn unify_arm_types(&mut self, first: &Type<'a>, next: &Type<'a>) -> Option<Type<'a>> {
        if matches!(first, Type::Never) {
            Some(next.clone())
        } else if self.is_assignable(first, next) {
            Some(first.clone())
        } else {
            None
        }
    }

    fn unify_ternary_arms(
        &mut self,
        first: Type<'a>,
        next: Type<'a>,
        span: ast::Span,
    ) -> Type<'a> {
        self.unify_arm_types(&first, &next).unwrap_or_else(|| {
            self.error_span(
                span,
                format!("ternary arm has type `{next}`, expected type `{first}`"),
            );
            Type::Error
        })
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
        if matches!(expected, Type::Named { name: "Component" }) {
            return match actual.function_contract() {
                Type::Function { params, ret, .. } => params.len() == 1
                    && matches!(params[0], Type::Interface { .. } | Type::Struct { .. })
                    && self.is_assignable(&Type::react_node(), &ret),
                Type::Named { name: "Component" } => true,
                _ => false,
            };
        }
        if matches!(expected, Type::Generic { base: "Component", .. }) || matches!(actual, Type::Generic { base: "Component", .. }) {
            return self.is_assignable(&expected.function_contract(), &actual.function_contract());
        }
        if expected.is_react_node() {
            if actual.is_react_node() { return true; }
            return match actual {
                Type::Named { name: "string" | "number" | "boolean" | "void" } | Type::None | Type::Never => true,
                Type::Array { elem } => matches!(**elem, Type::Var) || self.is_assignable(expected, elem),
                Type::Union { members } => members.iter().all(|m| self.is_assignable(expected, m)),
                _ => false,
            };
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
        // rfd#56 phase 1: one unbounded type parameter is assignable to
        // another unbounded slot ("passing to another unbounded slot"). No
        // operations exist on either side, so nothing a bound could guarantee
        // is violated. Phase 2 (bounds) must refine this arm to check the
        // bound before widening it.
        if matches!(expected, Type::Param { .. }) && matches!(actual, Type::Param { .. }) {
            return true;
        }
        // rfd#56 phase 2, union bounds: a value whose type is a concrete
        // member of T's union bound is assignable to T. This is the
        // identity-preservation rule — inside a match arm `item: T` has
        // narrowed to the member, and `return item` against a declared
        // return type `T` must still hold. (Members of a union bound are
        // never themselves type parameters, so this cannot recurse back
        // into the parameter arms.)
        if let Type::Param { name } = expected {
            if let Some(Type::Union { members }) = self.lookup_param_bound(name) {
                if members.iter().any(|m| self.is_assignable(m, actual)) {
                    return true;
                }
            }
        }
        if let (Type::Interface { identity: a, .. }, Type::Interface { identity: b, .. }) =
            (expected, actual)
        {
            if a == b {
                return true;
            }
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
        if let (Type::Tuple { elements: e }, Type::Tuple { elements: a }) = (expected, actual) {
            return e.len() == a.len() && e.iter().zip(a).all(|(e, a)| self.is_assignable(e, a));
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
        // Imported members are already resolved in their defining namespace.
        // Never re-resolve their AST against a consumer's aliases/interfaces.
        if let Type::Interface { identity, .. } = expected {
            if let Some(members) = self.interface_members.get(identity).cloned() {
                // Recursive interfaces are compared coinductively. Keep this
                // guard only for the active comparison, not as a result cache.
                let pair = if let Type::Interface {
                    identity: actual_id,
                    ..
                } = actual
                {
                    let pair = (*identity, *actual_id);
                    if !self.interface_comparisons.insert(pair) {
                        return true;
                    }
                    Some(pair)
                } else {
                    None
                };
                let compatible = members.members.iter().all(|(field, expected_ty)| {
                    let actual_ty = match actual {
                        Type::Object { fields } => fields
                            .iter()
                            .find(|(n, _)| n == field)
                            .map(|(_, ty)| ty.clone()),
                        Type::Interface { name, identity } => {
                            if let Some(fields) = self.interface_members.get(identity) {
                                fields
                                    .members
                                    .iter()
                                    .find(|(n, _)| n == field)
                                    .map(|(_, ty)| ty.clone())
                            } else {
                                self.resolve_interface_field(name, field)
                            }
                        }
                        Type::Struct { name } => {
                            self.resolve_field_type(name, field).or_else(|| {
                                self.collect_struct_methods(name)
                                    .into_iter()
                                    .find(|(method, _)| method == field)
                                    .map(|(_, info)| Type::Function {
                                        params: info.param_types,
                                        ret: Box::new(
                                            info.resolved_return
                                                .unwrap_or(Type::Named { name: "void" }),
                                        ),
                                        optional: 0,
                                    })
                            })
                        }
                        _ => return false,
                    };
                    match actual_ty {
                        Some(actual_ty) => self.is_assignable(expected_ty, &actual_ty),
                        None => matches!(expected_ty, Type::Option { .. }),
                    }
                });
                if let Some(pair) = pair {
                    self.interface_comparisons.remove(&pair);
                }
                return compatible;
            }
        }
        // Interface satisfaction: structs and objects must supply every
        // required field and method with a compatible type.
        if let Type::Interface { name, .. } = expected {
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
                                    // An absent property reads as undefined, the erased None.
                                    if matches!(expected_ty, Type::Option { .. }) {
                                        continue;
                                    }
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

    pub(super) fn typeck(source: &str) -> Vec<Diagnostic> {
        let arena = Bump::new();
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        check_program(&program, source).errors
    }

    #[test]
    fn component_is_a_checked_props_to_react_node_function() {
        for (declaration, props) in [
            ("interface Props { title: string }", "{ title: \"ok\" }"),
            ("struct Props { title: string }", "Props { title: \"ok\" }"),
        ] {
            let source = format!(
                "{declaration} fn Card(props: Props) ReactNode {{ return <p>{{props.title}}</p>; }} const component: Component<Props> = Card; const inferred: Component = Card; const node: ReactNode = component({props});"
            );
            let errors = typeck(&source);
            assert!(errors.is_empty(), "{source}: {errors:?}");
        }
        assert!(typeck("interface Props { title: string } fn Card(props: Props) string { return props.title; } const C: Component<Props> = Card; const node: ReactNode = <C title=\"ok\" />;").is_empty());
    }

    #[test]
    fn component_wrong_shapes_teach_the_function_contract() {
        for source in [
            "const C: Component = <p />;",
            "fn C() Component { return <p />; }",
            "fn C(props: number) ReactNode { return <p />; } const c: Component = C;",
            "interface Props { title: string } fn C(props: Props) Props { return props; } const c: Component = C;",
            "const c: Component<number> = 1;",
        ] {
            let errors = typeck(source);
            assert!(
                errors.iter().any(|e| e.message.contains("Component")
                    && (e.message.contains("ReactNode") || e.message.contains("props"))),
                "{source}: {errors:?}"
            );
        }
        for source in [
            "struct Props { title: string } fn Card(props: Props) ReactNode { return <p />; } const x = <Card title={42} />;",
            "interface Props { title: string } fn Card(props: Props) ReactNode { return <p />; } const x = <Card />;",
            "fn Card(props: number) ReactNode { return <p />; } const x = <Card />;",
            "interface Props { children: number } fn Card(props: Props) ReactNode { return <p />; } const x = <Card>text</Card>;",
            "const node = <p>{{title: \"object\"}}</p>;",
            "const node = <p />; const field = node.type;",
        ] {
            assert!(!typeck(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn signal_name_alone_no_longer_requires_hydration() {
        let source = "fn signal() number { return 1; } fn Card() ReactNode { const n = signal(); return <p>{n}</p>; } const node = <Card />;";
        assert!(typeck(source).is_empty(), "{:?}", typeck(source));
    }

    #[test]
    fn ternary_checks_precedence_nesting_and_arm_types() {
        for source in [
            "const x: number = true || false ? 1 : 2;",
            "const x: number = true && false || true ? 1 : 2;",
            "const x: number = true ? false ? 1 : 2 : 3;",
            "const x: number = false ? 1 : true ? 2 : 3;",
            "const x: boolean = true ? false || true : true && false;",
            "fn f(b: boolean) number { return b ? 1 : 2; }",
            "const x: string = true ? panic(\"stop\") : \"ok\";",
            "const x: string = true ? \"ok\" : panic(\"stop\");",
            "fn f(b: boolean) string { return b ? panic(\"stop\") : \"ok\"; }",
        ] {
            assert!(typeck(source).is_empty(), "{source}: {:?}", typeck(source));
        }
    }

    #[test]
    fn ternary_condition_diagnostic_teaches_the_actual_type() {
        for source in [
            "const x = 1 ? 2 : 3;",
            "fn f() number { return 1 ? 2 : 3; }",
            "const x = [1 ? 2 : 3];",
        ] {
            let errors = typeck(source);
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert_eq!(
                errors[0].message,
                "expected type `boolean`, found type `number`"
            );
        }
    }

    #[test]
    fn ternary_arm_mismatch_matches_match_unification() {
        for expression in ["true ? 1 : \"no\"", "true ? \"no\" : 1"] {
            for source in [
                format!("const x = {expression};"),
                format!("fn f() {{ const x = [{expression}]; }}"),
            ] {
                let errors = typeck(&source);
                assert_eq!(errors.len(), 1, "{errors:?}");
                assert!(
                    errors[0].message.contains("ternary arm has type"),
                    "{errors:?}"
                );
            }
        }
        // Match selects the first non-never arm, including a declared union.
        for expression in ["b ? x : 1", "match b { true => x, false => 1 }"] {
            let source = format!(
                "fn f(b: boolean, x: number | string) number | string {{ return {expression}; }}"
            );
            assert!(
                typeck(&source).is_empty(),
                "{source}: {:?}",
                typeck(&source)
            );
        }
        for expression in ["b ? 1 : x", "match b { true => 1, false => x }"] {
            let source = format!(
                "fn f(b: boolean, x: number | string) number | string {{ return {expression}; }}"
            );
            assert!(!typeck(&source).is_empty(), "{source}");
        }
    }

    #[test]
    fn type_param_interface_bound_unlocks_declared_members() {
        // rfd#56 phase 2: `<T: Named>` permits the phase-1 list plus the
        // members `Named` declares.
        assert!(typeck(
            "interface Named { name: string }\n\
             struct User { name: string }\n\
             fn greet<T: Named>(x: T) string { return x.name }\n\
             const u = User { name: \"Ada\" }\n\
             const s = greet(u);"
        )
        .is_empty());

        // A member the bound does not declare is a check-time error.
        let errors = typeck(
            "interface Named { name: string }\n\
             struct User { name: string; age: number }\n\
             fn greet<T: Named>(x: T) number { return x.age }",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("no field `age`"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn type_param_interface_method_call_dispatches_on_bound() {
        assert!(typeck(
            "interface Named { fn describe() string }\n\
             struct User { name: string }\n\
             fn (u User) describe() string { return u.name }\n\
             fn greet<T: Named>(x: T) string { return x.describe() }\n\
             const u = User { name: \"Ada\" }\n\
             const s = greet(u);"
        )
        .is_empty());

        // Calling a method the bound does not declare fails.
        let errors = typeck(
            "interface Named { fn describe() string }\n\
             fn greet<T: Named>(x: T) string { return x.shout() }",
        );
        assert!(!errors.is_empty(), "undeclared method must fail");
        assert!(
            errors.iter().any(|e| e.message.contains("shout")),
            "{errors:?}"
        );
    }

    #[test]
    fn type_param_concrete_bound_unlocks_bound_operations() {
        // `<T: number>` arithmetic is number arithmetic.
        assert!(typeck(
            "fn double<T: number>(x: T) number { return x * 2 }\nconst y = double(21);"
        )
        .is_empty());
    }

    #[test]
    fn type_param_assignment_stays_param_to_param() {
        // Assignment is a phase-1 permitted operation and stays T-to-T even
        // with a bound: the slot may hold any member, not every value the
        // bound names.
        assert!(typeck(
            "struct Product { price: number }\n\
             struct Bundle { price: number }\n\
             fn carry<T: Product | Bundle>(item: T) T {\n\
             \x20 let slot = item\n\
             \x20 slot = item\n\
             \x20 return slot\n\
             }"
        )
        .is_empty());
    }

    #[test]
    fn type_param_unknown_bound_is_an_error() {
        // A bound that does not resolve is a check-time error, reported once
        // even though signature collection and body checking both push the
        // parameter scope.
        let errors = typeck("fn f<T: Nope>(x: T) T { return x }");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("unknown type `Nope`"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn generic_receiver_method_binds_type_param() {
        // dsc#101: `T` is bound by the receiver (`fn (s Signal<T>) get() T`),
        // so the body and signature resolve it at each call site.
        assert!(typeck(
            "struct Signal<T> { value: T }\n\
             fn signal<T>(initial: T) Signal<T> { return Signal { value: initial } }\n\
             fn (s Signal<T>) get() T { return s.value }\n\
             fn (s mut Signal<T>) set(next: T) void { s.value = next }\n\
             let count = signal(0)\n\
             const a = count.get()\n\
             count.set(41)\n\
             const b = count.get()\n\
             let label = signal(\"active\")\n\
             const c = label.get()"
        )
        .is_empty());

        // The call site solves T from the receiver's type argument:
        // `signal(42).get()` yields number ...
        let errors = typeck(
            "struct Signal<T> { value: T }\n\
             fn signal<T>(initial: T) Signal<T> { return Signal { value: initial } }\n\
             fn (s Signal<T>) get() T { return s.value }\n\
             const s: string = signal(42).get()",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0]
                .message
                .contains("expected type `string`, found type `number`"),
            "{}",
            errors[0].message
        );

        // ... and `signal("x").get()` yields string.
        let errors = typeck(
            "struct Signal<T> { value: T }\n\
             fn signal<T>(initial: T) Signal<T> { return Signal { value: initial } }\n\
             fn (s Signal<T>) get() T { return s.value }\n\
             const n: number = signal(\"x\").get()",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0]
                .message
                .contains("expected type `number`, found type `string`"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn generic_receiver_method_wrong_argument_type_fails() {
        // `signal(0).set("nope")`: T solves to number from the receiver, so
        // the string argument is a check-time error with a span.
        let errors = typeck(
            "struct Signal<T> { value: T }\n\
             fn signal<T>(initial: T) Signal<T> { return Signal { value: initial } }\n\
             fn (s mut Signal<T>) set(next: T) void { s.value = next }\n\
             let count = signal(0)\n\
             count.set(\"nope\")",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0]
                .message
                .contains("expected argument type `number`, found type `string`"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn generic_receiver_method_unbounded_member_call_fails() {
        // rfd#56 phase 1 capability rule, inside a receiver method: an
        // unbounded `T` permits no member calls, and no bound is declared
        // here that could permit one.
        let errors = typeck(
            "struct Signal<T> { value: T }\n\
             fn (s Signal<T>) bad() void { s.value.whatever() }",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains(
                "cannot call method `whatever` on a value of unbounded type parameter `T`"
            ),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn generic_receiver_method_bounded_receiver() {
        // rfd#56 phase 2: a receiver whose parameter carries an interface
        // bound unlocks the bound's members in the body, and the call site
        // verifies the receiver's type argument against the bound.
        assert!(typeck(
            "interface Named { name: string }\n\
             struct Holder<T> { value: T }\n\
             fn (x Holder<T: Named>) name() string { return x.value.name }\n\
             struct User { name: string }\n\
             let h = Holder { value: User { name: \"Ada\" } }\n\
             const n = h.name()"
        )
        .is_empty());

        // A receiver value whose type argument is outside the bound fails at
        // the call site — construction of the struct itself is unbounded.
        let errors = typeck(
            "interface Named { name: string }\n\
             struct Holder<T> { value: T }\n\
             fn (x Holder<T: Named>) name() string { return x.value.name }\n\
             struct NoName { age: number }\n\
             let bad = Holder { value: NoName { age: 1 } }\n\
             const n = bad.name()",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0]
                .message
                .contains("does not satisfy the bound `Named`"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn generic_receiver_method_union_bound_narrows_in_body() {
        // A union-bound receiver parameter narrows to the concrete member
        // inside each match arm, the same as a union-bound function
        // parameter (rfd#56 phase 2).
        assert!(typeck(
            "struct Product { sku: string }\n\
             struct Bundle { id: number }\n\
             struct Holder<T> { value: T }\n\
             fn (x Holder<T: Product | Bundle>) describe() string {\n\
             \x20 return match x.value {\n\
             \x20   Product(p) => p.sku\n\
             \x20   Bundle(b) => string(b.id)\n\
             \x20 }\n\
             }\n\
             let h = Holder { value: Product { sku: \"A-1\" } }\n\
             const d = h.describe()"
        )
        .is_empty());

        // Non-exhaustive match over a union-bound receiver parameter fails.
        let errors = typeck(
            "struct Product { sku: string }\n\
             struct Bundle { id: number }\n\
             struct Holder<T> { value: T }\n\
             fn (x Holder<T: Product | Bundle>) describe() string {\n\
             \x20 return match x.value {\n\
             \x20   Product(p) => p.sku\n\
             \x20 }\n\
             }",
        );
        assert!(!errors.is_empty(), "non-exhaustive match must fail");
        assert!(
            errors.iter().all(|e| e.message.contains("match")),
            "{errors:?}"
        );
    }

    #[test]
    fn generic_receiver_method_declaration_errors() {
        // Type parameters are bound by the receiver, not declared on the
        // method: declaring both is an error (rfd#56, dsc#101).
        let errors = typeck(
            "struct Signal<T> { value: T }\n\
             fn (s Signal<T>) get<U>() T { return s.value }",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("bound by the receiver")),
            "{errors:?}"
        );

        // The receiver's parameter arity must match the struct declaration.
        let errors = typeck(
            "struct Signal<T> { value: T }\n\
             fn (s Signal<T, U>) get() T { return s.value }",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("declares 1 type parameter")),
            "{errors:?}"
        );

        // A receiver with no type parameters binds none.
        let errors = typeck(
            "struct Point { x: number }\n\
             fn (p Point<T>) dist() number { return p.x }",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("has no type parameters to bind")),
            "{errors:?}"
        );
    }

    #[test]
    fn generic_receiver_method_struct_bound_is_inherited() {
        // A bound declared on the struct's parameter applies inside a
        // receiver method that leaves the parameter unbounded: the bound is
        // a property of the parameter, not of one spelling of it.
        assert!(typeck(
            "interface Named { name: string }\n\
             struct Holder<T: Named> { value: T }\n\
             fn (x Holder<T>) name() string { return x.value.name }\n\
             struct User { name: string }\n\
             let h = Holder { value: User { name: \"Ada\" } }\n\
             const n = h.name()"
        )
        .is_empty());
    }

    #[test]
    fn struct_literal_type_args_rejected_at_parse() {
        // dsc#101: `Signal<T> { ... }` puts explicit type arguments at a
        // construction site; the design infers them. The parse rejects the
        // spelling with a dedicated diagnostic instead of the old
        // comparison-operator misparse (`unknown identifier T`).
        let arena = Bump::new();
        let result = crate::parse::parse(
            "struct Signal<T> { value: T }\nconst s = Signal<T> { value: 1 }",
            &arena,
        );
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("infers its type arguments from the field values")),
            "{:?}",
            result.errors
        );

        // Concrete type arguments get the same diagnostic.
        let arena = Bump::new();
        let result = crate::parse::parse(
            "struct Signal<T> { value: T }\nconst s = Signal<number> { value: 1 }",
            &arena,
        );
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("infers its type arguments from the field values")),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn type_param_union_bound_narrows_per_arm_and_preserves_identity() {
        // rfd#56 phase 2: the load-bearing case. Matching narrows T to the
        // concrete member inside each arm, the declared return type T still
        // holds (a narrowed member is assignable to T), and the caller
        // receives the member it passed without narrowing again.
        let source = "struct Product { price: number }\n\
             struct Bundle { price: number }\n\
             fn render<T: Product | Bundle>(item: T) T {\n\
             \x20 return match item {\n\
             \x20   Product(p) => p\n\
             \x20   Bundle(b) => b\n\
             \x20 }\n\
             }\n\
             const widget = Product { price: 5 }\n\
             const back = render(widget)\n\
             const n: number = back.price;";
        assert!(typeck(source).is_empty(), "{:?}", typeck(source));

        // Using a member outside the bound is not narrowed to it: matching a
        // name that is not a bound member is an error.
        let errors = typeck(
            "struct Product { price: number }\n\
             struct Bundle { price: number }\n\
             struct GiftCard { credit: number }\n\
             fn render<T: Product | Bundle>(item: T) T {\n\
             \x20 return match item {\n\
             \x20   Product(p) => p\n\
             \x20   Bundle(b) => b\n\
             \x20   GiftCard(g) => g\n\
             \x20 }\n\
             }",
        );
        assert!(!errors.is_empty(), "non-member pattern must fail");
        assert!(
            errors.iter().any(|e| e.message.contains("GiftCard")),
            "{errors:?}"
        );
    }

    #[test]
    fn type_param_union_bound_requires_exhaustiveness() {
        let errors = typeck(
            "struct Product { price: number }\n\
             struct Bundle { price: number }\n\
             struct GiftCard { credit: number }\n\
             fn render<T: Product | Bundle | GiftCard>(item: T) number {\n\
             \x20 return match item {\n\
             \x20   Product(p) => p.price\n\
             \x20   Bundle(b) => b.price\n\
             \x20 }\n\
             }",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("missing GiftCard"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn type_param_narrowing_reverts_after_the_match() {
        // Inside the arm `item` is the member; after the match it is T
        // again, and a member-only field access is an error.
        let errors = typeck(
            "struct Product { price: number }\n\
             struct Bundle { price: number }\n\
             fn f<T: Product | Bundle>(item: T) number {\n\
             \x20 const n = match item {\n\
             \x20   Product(p) => p.price\n\
             \x20   Bundle(b) => b.price\n\
             \x20 }\n\
             \x20 return item.price\n\
             }",
        );
        assert!(!errors.is_empty(), "narrowed member must not leak");
    }

    #[test]
    fn type_param_bound_checked_at_call_site_after_inference() {
        // The union bound: an argument outside the union names the bound.
        let errors = typeck(
            "struct Product { price: number }\n\
             struct Bundle { price: number }\n\
             struct Widget { size: number }\n\
             fn render<T: Product | Bundle>(item: T) T { return item }\n\
             const w = Widget { size: 1 }\n\
             const r = render(w);",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("does not satisfy the bound `Bundle | Product`"),
            "{}",
            errors[0].message
        );

        // The interface bound: a type that does not satisfy the interface
        // names the interface.
        let errors = typeck(
            "interface Named { name: string }\n\
             struct Point { x: number }\n\
             fn greet<T: Named>(x: T) T { return x }\n\
             const p = Point { x: 1 }\n\
             const r = greet(p);",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("does not satisfy the bound `Named`"),
            "{}",
            errors[0].message
        );

        // Inference failure must not crash the bound check: an unannotated
        // empty array leaves T unsolved (Var) and only the argument error
        // is reported.
        let errors = typeck(
            "struct Product { price: number }\n\
             fn render<T: Product | Bundle>(item: T) T { return item }\n\
             const xs = []\n\
             const r = render(xs.first());",
        );
        assert!(!errors.is_empty());
        assert!(
            errors
                .iter()
                .all(|e| !e.message.contains("does not satisfy")),
            "unsolved T must not produce a bound error: {errors:?}"
        );
    }

    #[test]
    fn type_param_get_type_is_not_a_bound_operation() {
        // Runtime type interrogation on a type parameter is deferred by
        // rfd#56; neither an unbounded nor a bounded T may call getType.
        let errors = typeck(
            "interface Named { name: string }\n\
             fn f<T: Named>(x: T) {\n\
             \x20 const t = x.getType()\n\
             }",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("cannot call `getType` on"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn template_interpolation_is_checked() {
        // dsc#89: a name that does not exist must be a check-time error with
        // a span pointing at the interpolation, not a runtime surprise from
        // emitted JavaScript.
        let errors = typeck("const s = `hello ${undefinedName}`;");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("undefinedName"),
            "{}",
            errors[0].message
        );
        assert_eq!((errors[0].line, errors[0].column), (1, 20));

        // Type errors inside the interpolation are caught like anywhere else.
        let errors = typeck("const s = `${\"a\" - 1}`;");
        assert!(!errors.is_empty(), "expected a type error");
    }

    #[test]
    fn array_map_solves_callback_return_type() {
        // deka#467: `map` is (T -> U) -> Array<U>; U solves from the
        // callback's return type, so downstream uses see the real element.
        assert!(typeck(
            "const a = [1, 2, 3].map(fn(x: number) string { return \"s\" });\nconst s: string = a.has(0) ? a[0] : \"\";"
        )
        .is_empty());
        let errors = typeck(
            "const a = [1, 2, 3].map(fn(x: number) string { return \"s\" });\nconst n: number = a.has(0) ? a[0] : \"\";",
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
            "const a = [1, 2, 3];\nconst cbs = [];\nif (cbs.has(0)) { const d = a.map(cbs[0]); if (d.has(0)) { const s: string = d[0]; } }"
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
        assert!(
            typeck("const o = Some(5); const x: number = match o { Some(n) => n, None => 0 };")
                .is_empty()
        );
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
    fn option_binding_payload_mismatch_fails() {
        let errors = typeck("const o: Option<number> = Some(\"text\");");
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
    fn option_binding_none_passes() {
        assert!(typeck("const o: Option<number> = None;").is_empty());
    }

    #[test]
    fn option_struct_defaults_and_partial_literals() {
        for field in ["path?: string", "path: string?", "path: Option<string>", "path: Maybe"] {
            let source = format!(r#"
                alias Maybe = Option<string>;
                interface Options {{ {field}; secure?: boolean }}
                fn path(options: Options = {{}}) string {{
                    return match options.path {{ Some(v) => v, None => "/" }};
                }}
                const omitted: Options = {{}};
                const partial: Options = {{ path: Some("/app") }};
                const a = path();
                const b = path({{}});
                const c = path({{ secure: Some(false) }});
                fn defaults() Options {{ return {{}}; }}
                interface Request {{ options: Options; child?: Options }}
                const nested: Request = {{ options: {{}} }};
                const partial_nested: Request = {{ options: {{ secure: Some(false) }}, child: Some({{}}) }};
                struct Named {{ {field} }}
                const named = Named {{}};
            "#);
            let errors = typeck(&source);
            assert!(errors.is_empty(), "{field}: {errors:?}");
        }
        let errors = typeck("struct Inner { value: Option<number> } struct Outer { inner: Inner; extra: Option<Inner> } const nested = Outer { inner: Inner {} };");
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn option_struct_defaults_preserve_required_field_diagnostics() {
        for (source, expected) in [
            ("interface Options { required: string; path?: string } const o: Options = {};",
             "expected type `Options`, found type `{}`"),
            ("interface Options { required: string; path?: string } fn f(o: Options = {}) void {}",
             "expected default type `Options`, found type `{}`"),
            ("interface Options { required: string; path?: string } fn f(o: Options) void {} f({});",
             "expected argument type `Options`, found type `{}`"),
            ("interface Inner { required: string; path?: string } interface Outer { inner: Inner } const o: Outer = { inner: {} };",
             "expected type `Outer`, found type `{inner: {}}`"),
            ("struct Options { required: string; path: Option<string> } const o = Options {};",
             "missing required field `required` in struct literal for `Options`"),
            ("interface Options { path?: string } const o: Options = { path: 42 };",
             "expected type `Options`, found type `{path: number}`"),
        ] {
            let errors = typeck(source);
            assert_eq!(errors.len(), 1, "{source}: {errors:?}");
            assert_eq!(errors[0].message, expected, "{source}");
        }
    }

    #[test]
    fn option_erasure_rejects_ambiguous_payloads() {
        for source in [
            "const o: Option<void> = None;",
            "fn noop() void {} const o = Some(noop());",
            "alias Unit = void; const o: Option<Unit> = None;",
            "fn wrap<T>(v: T) Option<T> { return Some(v); } fn noop() void {} const o = wrap(noop());",
            "fn absent<T>(v: T) Option<T> { return None; } fn noop() void {} const o = absent(noop());",
            "fn empty<T>(v: T) Array<Option<T>> { return []; } fn noop() void {} const o = empty(noop());",
            "fn absent<T>(v: T) Result<Option<T>, string> { return Ok(None); } fn noop() void {} const o = absent(noop());",
            "const o: Option<Option<number>> = None;",
            "const o = Some(Some(1));",
            "const o = Some(None);",
            "alias Maybe = Option<number>; const o: Option<Maybe> = None;",
            "fn wrap<T>(v: T) Option<T> { return Some(v); } const o = wrap(Some(1));",
        ] {
            let errors = typeck(source);
            assert!(errors.iter().any(|e| e.message.contains("under erasure") && e.message.contains("purpose-built enum")), "{source}: {errors:?}");
        }
        assert!(typeck("const a: Option<number> = Some(0); const b: Option<boolean> = Some(false); const c: Option<string> = Some(\"\");").is_empty());
        let arena = bumpalo::Bump::new();
        let parsed = crate::parse("fn f(v: Option<number>) Option<number> { return match v { Some(x) => Some(x), None }; }", &arena);
        assert!(parsed.errors.iter().any(|e| e.message.contains("bodyless arm requires a Result or Exception")));
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
    fn match_struct_pattern_binds_named_fields_and_ignores_others() {
        assert!(typeck(
            "struct Point { x: number; y: number }\n\
             fn x_of(point: Point) number {\n\
               return match (point) { Point { x } => x };\n\
             }"
        )
        .is_empty());
    }

    #[test]
    fn match_tuple_pattern_binds_array_elements() {
        assert!(typeck(
            "fn first(pair: Array<number>) number {\n\
               return match (pair) { (first, second) => first, _ => 0 };\n\
             }"
        )
        .is_empty());
    }

    #[test]
    fn match_pattern_recurses_from_enum_to_struct() {
        assert!(typeck(
            "struct Point { x: number; y: number }\n\
             enum Message { Move(Point), Stop }\n\
             fn x_of(message: Message) number {\n\
               return match (message) {\n\
                 Move(Point { x }) => x,\n\
                 Stop => 0,\n\
               };\n\
             }"
        )
        .is_empty());
    }

    #[test]
    fn match_struct_pattern_wrong_scrutinee_type_fails() {
        let errors = typeck(
            "struct Point { x: number }\n\
             const value: string = \"nope\";\n\
             const answer = match (value) { Point { x } => x, _ => 0 };",
        );
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("struct pattern `Point` does not match scrutinee type `string`")),
            "{errors:?}"
        );
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
            "fn (s string) slugify() string { return s; } const n: number = 42; const bad: string = n.slugify();",
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
            "fn (s string) wrap(prefix: string) string { return prefix + s; } const w: string = \"x\".wrap();",
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
        assert!(
            errors[0].message.contains("super struct"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn super_type_on_plain_enum_fails_teaching_super() {
        let errors = typeck("enum Status { Active }\nconst t = Status.type();");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("super enum"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn super_type_on_unknown_name_still_unknown_identifier() {
        let errors = typeck("const t = Nope.type();");
        // The unknown name is reported on both the object and callee paths;
        // what matters is it stays an unknown-identifier error, not a
        // super-specific one.
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("unknown identifier")),
            "{errors:?}"
        );
    }

    #[test]
    fn super_type_on_newtype_explains_the_limit() {
        let errors = typeck("type Cents number\nconst t = Cents.type();");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("only available"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn super_type_with_args_fails() {
        let errors = typeck("super struct User { id: number }\nconst t = User.type(1);");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("no arguments"),
            "{}",
            errors[0].message
        );
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
        assert!(
            errors[0]
                .message
                .contains("cannot carry runtime type information"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("field `f`"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn super_type_through_alias_resolves_to_decl() {
        let arena = Bump::new();
        let source =
            "super struct User { id: number }\nalias Alias = User\nconst t = Alias.type();";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let typeck = check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        assert_eq!(typeck.static_type_calls.len(), 1);
    }

    #[test]
    fn super_enum_type_call_passes() {
        assert!(
            typeck("super enum Status { Active, Archived }\nconst t = Status.type();").is_empty()
        );
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
        assert!(
            typeck("type Cents number; const c: Cents = Cents(5); const t: Type = c.getType();")
                .is_empty()
        );
        assert!(
            typeck(
                "enum Color { Red, Green } const c: Color = Color.Red; const t: Type = c.getType();"
            )
            .is_empty()
        );
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
        assert!(
            typeck(
                "interface Named { name: string } fn f(v: Named) Type { return v.signature(); }"
            )
            .is_empty()
        );
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
                .filter(|k| matches!(k, ArrayAccess::Pop))
                .count(),
            1
        );
        assert_eq!(
            checked
                .array_builtin_calls
                .values()
                .filter(|k| matches!(k, ArrayAccess::Shift))
                .count(),
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
    fn bytes_length_is_a_number_property() {
        // dsc#92: `bytes` gets a member catalog starting at `.length`
        // (RFD 15's `len` operation, surfaced with the same spelling
        // `string`/`array` use). Indexing (dsc#88) is unaffected.
        assert!(typeck(
            "fn f(b: bytes) number { return b.length }\nfn g(b: bytes) number { return b[0] }"
        )
        .is_empty());

        // The property is `number`: flowing it elsewhere fails.
        let errors = typeck("fn f(b: bytes) string { return b.length }");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("number"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn bytes_catalog_is_closed() {
        // A member that does not exist on `bytes` is a check-time error with
        // a span, and the diagnostic names the one member that does exist.
        let errors = typeck("fn f(b: bytes) number {\n  return b.slice\n}");
        assert_eq!(errors.len(), 1, "{errors:?}");
        let error = &errors[0];
        assert!(
            error
                .message
                .contains("`bytes` has no field `slice` (available: `length`)"),
            "{}",
            error.message
        );
        // The diagnostic points at the field access itself (plain
        // `error_span` underlines a single character, as with the other
        // "has no field" diagnostics).
        assert_eq!(
            (error.line, error.column, error.underline_length),
            (2, 10, 1)
        );
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
                ref other => {
                    panic!("{name}: partial arm must return Option<number>, got {other:?}")
                }
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
        assert!(
            typeck(
                "interface Has { fn getType() string } fn f(v: Has) string { return v.getType(); }"
            )
            .is_empty()
        );
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
            "fn (s string) repeat(n: number) string { return s; } const r: string = \"x\".repeat(\"three\");",
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
            "fn (s string) slugify() string { return s; } fn (s string) slugify() string { return s; }",
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
        assert!(
            typeck(
                "fn (s string) slugify() string { return s; } const n: number = \"abc\".length;"
            )
            .is_empty()
        );
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
             const items = [1, 2]; takes_number(items.has(0) ? items[0] : 0); takes_string(\"ab\"[0]); takes_string(items.has(0) ? items[0] : 0);",
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
             let numbers = [1]; if (numbers.has(0)) { numbers[0] = \"bad\"; }\
             const funcs = [fn (x: number) number { return x }];\
             if (funcs.has(0)) { apply(funcs[0]); }",
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
    fn mixed_array_literal_is_a_check_time_error() {
        // dsc#88: `[1, "x"]` used to model as `Array<number>` — the string
        // reached user code typed as `number`.
        let errors = typeck("const a = [1, \"x\"];");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("mixed element types"),
            "{}",
            errors[0].message
        );
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

        // Homogeneous literals, and literals mixing only along an
        // assignable direction (`None` into `Option<T>`), still pass.
        assert!(typeck("const a = [1, 2, 3];").is_empty());
        let errors = typeck("const a = [Some(1), None];");
        assert!(errors.is_empty(), "{errors:?}");
        let errors = typeck("const a = [None, Some(1)];");
        assert!(errors.is_empty(), "{errors:?}");

        // Mixed named types are rejected too.
        let errors = typeck(
            "struct Product { price: number }\n\
             struct Bundle { price: number }\n\
             const a = [Product { price: 1 }, Bundle { price: 2 }];",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("mixed element types"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn spread_preserves_known_element_type() {
        // dsc#88: spread used to return `Infer` unconditionally, erasing the
        // element type. Now a spread element contributes the source's
        // element type, so a following element of a different type is a
        // mixed-literal error instead of silently unchecked.
        let errors = typeck(
            "const xs = [\"a\", \"b\"]\n\
             const ys = [...xs, 1];",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("mixed element types"),
            "{}",
            errors[0].message
        );

        // The erased-`Infer` hole, pinned directly: indexing the spread
        // array used to yield `Infer`, assignable to anything.
        let errors = typeck(
            "const xs = [\"a\", \"b\"]\n\
             const items = [...xs]; if (items.has(0)) { const n: number = items[0]; }",
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

        // A homogeneous spread still builds the array it always did.
        assert!(typeck("const xs = [\"a\"]\nconst ys = [...xs, \"b\"];").is_empty());
    }

    #[test]
    fn non_numeric_index_is_a_check_time_error() {
        // dsc#88: `items["nope"]` used to type as the element type while
        // JavaScript answers `undefined`.
        let errors = typeck("const a = [1, 2, 3]\nconst x = a[\"nope\"];");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("index must be a number"),
            "{}",
            errors[0].message
        );

        // Same rule for string indexing.
        let errors = typeck("const s = \"abc\"\nconst c = s[\"x\"];");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("index must be a number"),
            "{}",
            errors[0].message
        );

        // Numeric indices require an explicit bounds proof (rfd#65).
        assert!(typeck("const a = [1, 2, 3]\nconst x = a.has(999) ? a[999] : 0;").is_empty());
    }

    #[test]
    fn indexing_non_collection_is_a_check_time_error() {
        // dsc#88: indexing a non-collection used to fall through to
        // `Infer`, silently unchecked.
        let errors = typeck("const o = { a: 1 }\nconst x = o[0];");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("cannot index into"),
            "{}",
            errors[0].message
        );

        let errors = typeck(
            "struct Point { x: number }\n\
             const p = Point { x: 1 }\n\
             const y = p[0];",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("cannot index into"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn bytes_indexing_yields_number() {
        // dsc#88: `bytes` is a Uint8Array view; integer indexing reads a
        // byte. Previously this fell through collection_element to Infer.
        assert!(typeck("fn first_byte(b: bytes) number { return b[0]; }").is_empty());

        // The element type is real: it does not flow into an unrelated type.
        let errors = typeck("fn f(b: bytes) string { return b[0]; }");
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
    fn duplicate_object_literal_keys_are_rejected() {
        // dsc#88: `{ a: 1, a: "x" }` typed the field by the FIRST write
        // while JavaScript keeps the LAST write. Rejected now.
        let errors = typeck("const o = { a: 1, a: \"x\" };");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("duplicate key `a`"),
            "{}",
            errors[0].message
        );

        // A duplicate anywhere in the literal is rejected.
        let errors = typeck("const o = { a: 1, b: 2, a: 3 };");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("duplicate key `a`"),
            "{}",
            errors[0].message
        );

        // Distinct keys are unaffected.
        assert!(typeck("const o = { a: 1, b: \"x\" };").is_empty());
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
        assert!(
            typeck(
                "fn both(x: number) string { if (x > 0) { return \"y\" } else { return \"n\" } }"
            )
            .is_empty()
        );
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
        assert!(
            typeck(
                "for (let i = 0; i < 10; i = i + 1) { if (i == 5) { break } else { continue } }"
            )
            .is_empty()
        );
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
        assert!(
            typeck(
                "async fn value() Promise<number> { return 1 } const p: Promise<number> = value();"
            )
            .is_empty()
        );
    }

    #[test]
    fn top_level_await_passes() {
        assert!(
            typeck("async fn main() Promise<number> { return 1 } const n: number = await main();")
                .is_empty()
        );
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

    #[test]
    fn bare_unsafe_err_side_is_string() {
        // dsc#103: the bare form's Err payload is the thrown value's string
        // representation (dsc#60), so the Err side types as `string`. Member
        // access on it is a check-time error with a span instead of a runtime
        // `undefined`. The Ok side stays `Infer` until deka#252/#460.
        let errors = typeck(
            "const r = match (unsafe { JSON.parse(1) }) { Ok(v) => v, Err(e) => e.message };",
        );
        assert!(
            errors.iter().any(|e| e.message.contains("`string` has no field `message`")),
            "{:?}",
            errors
        );
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
    fn union_enum_case_pattern_binds_payload_and_covers_every_case() {
        let errors = typeck(
            "enum Shape { Empty, Rect(number) }\n\
             fn describe(s: Shape | number) string {\n\
               return match (s) {\n\
                 Shape.Empty => \"empty\",\n\
                 Shape.Rect(n) => \"rect \" + string(n),\n\
                 number(n) => string(n),\n\
               }\n\
             }",
        );
        assert!(errors.is_empty(), "{:?}", errors);
    }

    #[test]
    fn union_enum_case_pattern_does_not_cover_other_enum_cases() {
        let errors = typeck(
            "enum Shape { Empty, Rect(number) }\n\
             fn describe(s: Shape | number) string {\n\
               return match (s) {\n\
                 Shape.Rect(n) => \"rect \" + string(n),\n\
                 number(n) => string(n),\n\
               }\n\
             }",
        );
        assert_eq!(errors.len(), 1, "{:?}", errors);
        assert!(
            errors[0].message.contains("non-exhaustive")
                && errors[0].message.contains("Shape::Empty"),
            "{}",
            errors[0].message
        );
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
    fn math_global_diagnostic_teaches_the_module_import() {
        let errors = typeck("const circumference: number = Math.PI * 2;");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(
            errors[0].message,
            "`Math` is not available in DekaScript; import { PI } from \"math\" instead for PI, or use number methods such as `x.sqrt()`"
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
        let errors = typeck("const x: string | number = 1; const y: string | boolean = x;");
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

    #[test]
    fn interactive_component_without_client_directive_errors_at_its_tag() {
        let errors = typeck(
            "fn Counter() ReactNode {\n\
               return <button onClick={clicked}>0</button>\n\
             }\n\
             fn clicked() {}\n\
             const page = <Counter />\n",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        let error = &errors[0];
        assert_eq!(
            (error.line, error.column, error.underline_length),
            (5, 15, 7)
        );
        assert!(
            error.message.contains("uses interactive APIs")
                && error.message.contains("will not render")
                && error.message.contains("client:load"),
            "{}",
            error.message
        );
    }

    #[test]
    fn client_directive_allows_interactive_component() {
        let errors = typeck(
            "fn Counter() ReactNode {\n\
               return <button onClick={clicked}>0</button>\n\
             }\n\
             fn clicked() {}\n\
             const page = <Counter client:load />\n",
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn hydration_ids_follow_client_directives_and_interactive_analysis() {
        let arena = bumpalo::Bump::new();
        let empty = std::collections::HashMap::new();
        let plain = parse("fn Card() ReactNode { return <p>hi</p>; }", &arena)
            .program
            .unwrap();
        assert!(!program_has_client_directive(&plain));
        assert!(!program_needs_hydration_ids(&plain, &empty));
        let island = parse("const page = <Card client:load />;", &arena)
            .program
            .unwrap();
        assert!(program_has_client_directive(&island));
        assert!(program_needs_hydration_ids(&island, &empty));
        let interactive = parse(
            "fn Counter() ReactNode { return <button onClick={clicked}>0</button>; } fn clicked() {}",
            &arena,
        )
        .program
        .unwrap();
        assert!(!program_has_client_directive(&interactive));
        assert!(program_needs_hydration_ids(&interactive, &empty));
    }

    #[test]
    fn generic_function_declares_and_infers() {
        // rfd#56 phase 1: `Signal<T>`'s supporting shapes parse, check, and
        // infer at call sites.
        assert!(
            typeck(
                "struct Signal<T> { value: T }\n\
                 fn signal<T>(initial: T) Signal<T> { return Signal { value: initial } }\n\
                 fn first<T>(items: Array<T>) Option<T> { return items.first() }\n\
                 const count = signal(0);\n\
                 const n: number = count.value;\n\
                 const one: Option<number> = first([1, 2, 3]);"
            )
            .is_empty()
        );

        // The concrete type is known at the call site: `signal("s")` gives
        // `T = string`, so `.value` is not `number`.
        let errors = typeck(
            "struct Signal<T> { value: T }\n\
             fn signal<T>(initial: T) Signal<T> { return Signal { value: initial } }\n\
             const count = signal(\"s\");\n\
             const n: number = count.value;",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("string"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn generic_receiver_method_binds_type_argument_at_call_site() {
        // `count.set("nope")` where `count: Signal<number>` is a check-time
        // error with a span on the argument.
        let source = "struct Signal<T> { value: T }\n\
                      fn (s mut Signal) set<T>(next: T) { s.value = next }\n\
                      fn signal<T>(initial: T) Signal<T> { return Signal { value: initial } }\n\
                      fn main() {\n\
                        let count = signal(0)\n\
                        count.set(\"nope\")\n\
                      }";
        let errors = typeck(source);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("expected argument type `number`"),
            "{}",
            errors[0].message
        );
        assert_eq!((errors[0].line, errors[0].column), (6, 11));

        // The well-typed call passes and the return type substitutes too.
        assert!(
            typeck(
                "struct Signal<T> { value: T }\n\
                 fn (s Signal) get<T>() T { return s.value }\n\
                 fn signal<T>(initial: T) Signal<T> { return Signal { value: initial } }\n\
                 fn main() {\n\
                   let count = signal(0)\n\
                   const n: number = count.get()\n\
                 }"
            )
            .is_empty()
        );
    }

    #[test]
    fn generic_struct_literal_infers_type_arguments() {
        // Two parameters, inferred from two fields.
        assert!(
            typeck(
                "struct Pair<A, B> { first: A\n second: B }\n\
                 const p = Pair { first: 1, second: \"s\" };\n\
                 const n: number = p.first;\n\
                 const s: string = p.second;"
            )
            .is_empty()
        );
        // A field value that contradicts the annotation is a check-time error.
        let errors = typeck(
            "struct Pair<A, B> { first: A\n second: B }\n\
             const p: Pair<string, number> = Pair { first: 1, second: \"s\" };",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn generic_enum_substitutes_case_payload_types() {
        // The deka#372 path, now reachable from user code again (rfd#56).
        assert!(
            typeck(
                "enum Box<T> { Empty, Full(T) }\n\
                 fn unwrap(b: Box<number>) number {\n\
                   return match (b) {\n\
                     Full(value) => value,\n\
                     Empty => 0,\n\
                   }\n\
                 }\n\
                 const x = unwrap(Box.Full(5))"
            )
            .is_empty()
        );
    }

    #[test]
    fn unbounded_type_parameter_passes_to_another_unbounded_slot() {
        // "Passing to another unbounded slot" is on the rfd#56 operation list.
        assert!(
            typeck(
                "fn sink<U>(y: U) {}\n\
                 fn f<T>(x: T) { sink(x) }\n\
                 fn g<T>(x: T) T {\n\
                   let y = x\n\
                   return y\n\
                 }"
            )
            .is_empty()
        );
    }

    #[test]
    fn unbounded_type_parameter_capability_rule() {
        // The rfd#56 negative matrix: each illegal operation fails with the
        // normative diagnostic, not an emergent one.
        let cases: &[(&str, &str)] = &[
            (
                "fn f<T>(x: T) { x.whatever() }",
                "cannot call method `whatever` on a value of unbounded type parameter `T`",
            ),
            (
                "fn f<T>(x: T) { const y = x.field }",
                "cannot access field `field` on a value of unbounded type parameter `T`",
            ),
            (
                "fn f<T>(x: T, y: T) { const b = x == y }",
                "cannot apply `==` to a value of unbounded type parameter `T`",
            ),
            (
                "fn f<T>(x: T) { const n = x + 1 }",
                "cannot apply `+` to a value of unbounded type parameter `T`",
            ),
            (
                "fn f<T>(x: T) { const e = x[0] }",
                "cannot index into a value of unbounded type parameter `T`",
            ),
            (
                "fn f<T>(x: T) { const n = -x }",
                "cannot negate a value of unbounded type parameter `T`",
            ),
            (
                "fn f<T>(xs: T) { for (const x of xs) { } }",
                "cannot iterate over a value of unbounded type parameter `T`",
            ),
        ];
        for (source, expected) in cases {
            let errors = typeck(source);
            assert_eq!(
                errors.len(),
                1,
                "{source:?} expected exactly one error, got: {errors:?}"
            );
            assert!(
                errors[0].message.contains(expected),
                "{source:?}\nexpected: {expected}\ngot: {}",
                errors[0].message
            );
        }

        // A type parameter of one name is not a concrete type either: `T`
        // does not satisfy a `number` slot.
        let errors = typeck("fn f<T>(x: T) { const n: number = x }");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("found type `T`"),
            "{}",
            errors[0].message
        );
    }
}
