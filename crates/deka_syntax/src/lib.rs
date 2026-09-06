//! DekaScript syntax crate (Compiler v2).
//!
//! Contains the DS-only lexer, parser, AST, and typechecker. No PHPX.

pub mod ast;
pub mod bridge;
pub mod canonicalize;
pub mod diagnostics;
pub mod lexer;
pub mod parse;
pub mod typeck;

pub use ast::*;
pub use bridge::{BRIDGE_OPS, BridgeOp, bridge_op_is_async};
pub use canonicalize::{lower_method_calls, resolve_imported_enum_constructors};
pub use diagnostics::{Diagnostic, Severity};
pub use lexer::Lexer;
pub use parse::{parse, ParseResult};
pub use typeck::{
    check_program, check_program_with_imports, collect_module_exports, EnumInfo, MethodInfo,
    ModuleExports, StructInfo, TypeError,
};
