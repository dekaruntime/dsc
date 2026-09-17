//! DekaScript syntax crate (Compiler v2).
//!
//! Contains the DS-only lexer, parser, AST, and typechecker. No PHPX.

pub mod ast;
pub mod bridge;
pub mod canonicalize;
pub mod diagnostics;
pub mod lexer;
pub mod parse;
pub mod scope;
pub mod typeck;

pub use ast::*;
pub use bridge::{BridgeSignature, HOST_DECL_SOURCE, bridge_op_is_async, format_signature};
pub use canonicalize::{lower_method_calls, resolve_imported_enum_constructors};
pub use diagnostics::{Diagnostic, Severity};
pub use lexer::Lexer;
pub use parse::{ParseResult, parse, parse_recovering};
pub use scope::{
    ScopeDeclaration, ScopeItem, ScopeItemKind, declarations_in_scope_at_offset,
    jsx_tag_prefix_at, names_in_scope_at_offset,
};
pub use typeck::{
    build_module_build_fragments, check_program, check_program_with_imports,
    collect_exported_interactive_components, collect_interactive_components, collect_module_exports,
    program_has_client_directive, program_needs_hydration_ids, refresh_module_export_values,
    AMBIENT_REACT_BUILTINS, EnumInfo, MethodInfo, ModuleExports, StructInfo, TypeError,
};

pub mod deka_catalog;
pub mod visit;
