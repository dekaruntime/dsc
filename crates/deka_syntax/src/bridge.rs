//! Host bridge declaration catalog (rfd#27, 2026-09-16 amendment; grammar
//! from rfd#39's declaration-files amendment).
//!
//! Every `bridge kind.action(args)` call is typed from `deka-host.d.ds`, the
//! declaration file deka generates from `HOST_CATALOG` (the table the
//! runtime actually executes) and publishes with every release. dsc embeds a
//! checked-in copy at compile time — synced by `cargo xtask sync-host-decl`
//! and pinned to `scripts/deka-runtime-version` — and parses it once, with
//! its own parser, into the same `Stmt::BridgeDecl` / `BridgeAction` AST that
//! parsing a user's `bridge <kind> { ... }` block would produce. There is no
//! separate hand-kept table to drift from the grammar: this *is* the
//! grammar, applied to the one file allowed to use it.
//!
//! Before this (dsc#223, dsc#272), the catalog was a hand-written
//! `BRIDGE_OPS` list of names and async flags, and every bridge call typed as
//! `Result<Infer, Infer>` regardless of the op's real signature. That list is
//! gone; the async flag now comes from whether the declared action is
//! `async fn`.

use std::sync::OnceLock;

use crate::ast::{self, Program};
use bumpalo::Bump;

/// The checked-in copy of deka's generated host declaration file. Do not
/// hand-edit: `cargo xtask sync-host-decl` overwrites it from the release
/// pinned in `scripts/deka-runtime-version`, and CI's drift check fails a PR
/// that edited this file (or bumped the pin) without also syncing it.
pub const HOST_DECL_SOURCE: &str = include_str!("../host-decl/deka-host.d.ds");

/// A resolved entry from the host catalog: `kind.action`'s declared
/// parameter and return types, and whether the call is async.
#[derive(Clone, Copy)]
pub struct BridgeSignature {
    pub params: &'static [ast::Param<'static>],
    pub return_type: &'static ast::Type<'static>,
    pub is_async: bool,
    /// Byte span of this action's declaration within [`HOST_DECL_SOURCE`] —
    /// used by the LSP to point go-to-definition at the right line (dsc#272
    /// item 7), not just open the file.
    pub span: ast::Span,
}

struct HostCatalog {
    program: &'static Program<'static>,
}

static CATALOG: OnceLock<HostCatalog> = OnceLock::new();

fn catalog() -> &'static HostCatalog {
    CATALOG.get_or_init(|| {
        // Leaked once for the process lifetime: the embedded catalog is
        // parsed a single time and consulted by every subsequent compile,
        // the same way `lib.*.d.ts` is loaded once inside `tsc`.
        let arena: &'static Bump = Box::leak(Box::new(Bump::new()));
        let result = crate::parse::parse(HOST_DECL_SOURCE, arena);
        let program = result.program.unwrap_or_else(|| {
            panic!(
                "deka-host.d.ds embedded in dsc failed to parse (this is a dsc bug, not a \
                 user error — the checked-in file is out of sync or corrupt): {:?}",
                result.errors
            )
        });
        for stmt in program.statements {
            if !matches!(
                stmt,
                ast::Stmt::BridgeDecl { .. }
                    | ast::Stmt::Struct { .. }
                    | ast::Stmt::Enum { .. }
                    | ast::Stmt::Interface { .. }
            ) {
                panic!(
                    "deka-host.d.ds contains a top-level statement that is neither `bridge`, \
                     `struct`, `enum` nor `interface` — the embedded catalog only declares \
                     `bridge <kind> {{ ... }}` blocks and the types (dsc#288) their signatures use"
                );
            }
        }
        let program: &'static Program<'static> = Box::leak(Box::new(program));
        HostCatalog { program }
    })
}

/// Look up `kind.action` in the embedded host catalog.
pub fn find(kind: &str, action: &str) -> Option<BridgeSignature> {
    for stmt in catalog().program.statements {
        let ast::Stmt::BridgeDecl {
            kind: decl_kind,
            actions,
            ..
        } = stmt
        else {
            continue;
        };
        if *decl_kind != kind {
            continue;
        }
        for entry in actions.iter() {
            if entry.name == action {
                return Some(BridgeSignature {
                    params: entry.params,
                    return_type: &entry.return_type,
                    is_async: entry.is_async,
                    span: entry.span,
                });
            }
        }
    }
    None
}

/// True when `kind` is a declared bridge kind at all, regardless of action —
/// used to tell "unknown kind" from "unknown action on a known kind" at a
/// bad call site.
pub fn kind_exists(kind: &str) -> bool {
    catalog()
        .program
        .statements
        .iter()
        .any(|stmt| matches!(stmt, ast::Stmt::BridgeDecl { kind: k, .. } if *k == kind))
}

/// A struct, enum, or interface declared in the host file itself, alongside
/// its `bridge` blocks (dsc#288, deka#1146): the runtime builds some values
/// itself (`read_dir` entries, fs errors) and the host file names their shape
/// so a bridge signature can use them, e.g. `Result<bytes, FsError>`.
pub enum HostTypeDecl {
    Struct(&'static ast::Stmt<'static>),
    Enum(&'static ast::Stmt<'static>),
    Interface(&'static ast::Stmt<'static>),
}

/// Look up a struct/enum/interface declared in the host file by name. This is
/// host-file only — application code still cannot declare one of these
/// ambiently — and it is not the `from "host"` package import (rfd#27
/// decision 7, dsc#288 item 4): the typechecker consults this only while
/// resolving a type a bridge signature already names (`resolve_ast_type`),
/// never as a standalone import surface.
pub fn host_type(name: &str) -> Option<HostTypeDecl> {
    catalog().program.statements.iter().find_map(|stmt| match stmt {
        ast::Stmt::Struct { name: n, .. } if *n == name => Some(HostTypeDecl::Struct(stmt)),
        ast::Stmt::Enum { name: n, .. } if *n == name => Some(HostTypeDecl::Enum(stmt)),
        ast::Stmt::Interface { name: n, .. } if *n == name => Some(HostTypeDecl::Interface(stmt)),
        _ => None,
    })
}

/// Whether the host dispatches `kind.action` asynchronously, i.e. the bridge
/// call evaluates to a `Promise` rather than a plain value. Unknown ops are
/// not async; the typechecker reports them as unknown separately.
pub fn bridge_op_is_async(kind: &str, action: &str) -> bool {
    find(kind, action).is_some_and(|sig| sig.is_async)
}

/// Render a declared type back into DekaScript source syntax, for
/// diagnostics and LSP hover. The catalog only ever contains primitive and
/// generic types (no user types, no type parameters), so this need not
/// handle every `ast::Type` shape as richly as a full pretty-printer would.
pub fn format_type(ty: &ast::Type<'_>) -> String {
    match ty {
        ast::Type::Named { name, .. } => (*name).to_string(),
        ast::Type::Generic { base, args, .. } => {
            let rendered: Vec<String> = args.iter().map(format_type).collect();
            format!("{base}<{}>", rendered.join(", "))
        }
        ast::Type::Function { params, ret, .. } => {
            let rendered: Vec<String> = params.iter().map(format_type).collect();
            format!("fn({}) {}", rendered.join(", "), format_type(ret))
        }
        ast::Type::Option { inner, .. } => format!("Option<{}>", format_type(inner)),
        ast::Type::Tuple { elements, .. } => {
            let rendered: Vec<String> = elements.iter().map(format_type).collect();
            format!("({})", rendered.join(", "))
        }
        ast::Type::Record { .. } => "{ .. }".to_string(),
        ast::Type::Union { members, .. } => {
            let rendered: Vec<String> = members.iter().map(format_type).collect();
            rendered.join(" | ")
        }
    }
}

/// Render `kind.action`'s declared signature as DekaScript source, for
/// hover text. `None` for an unknown op.
pub fn format_signature(kind: &str, action: &str) -> Option<String> {
    let sig = find(kind, action)?;
    let params: Vec<String> = sig
        .params
        .iter()
        .map(|p| {
            let name = p.binding.identifier().unwrap_or("_");
            match &p.ty {
                Some(ty) => format!("{name}: {}", format_type(ty)),
                None => name.to_string(),
            }
        })
        .collect();
    let ret = format_type(sig.return_type);
    let prefix = if sig.is_async { "async fn" } else { "fn" };
    Some(format!("{prefix} {action}({}) {ret}", params.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_parses() {
        // Forces the lazy parse; panics (failing the test) if the checked-in
        // file is malformed, or contains a top-level statement that is
        // neither `bridge`, `struct`, `enum` nor `interface` (dsc#288).
        assert!(kind_exists("crypto"));
    }

    /// dsc#288 / deka#1146: the host file declares `FsError`/`FsPermission`/
    /// `DirEntry` alongside its `bridge` blocks, and `host_type` finds each
    /// by name and kind.
    #[test]
    fn host_type_finds_declared_struct_enum_and_interface() {
        assert!(matches!(host_type("FsError"), Some(HostTypeDecl::Enum(_))));
        assert!(matches!(host_type("FsPermission"), Some(HostTypeDecl::Struct(_))));
        assert!(matches!(host_type("DirEntry"), Some(HostTypeDecl::Interface(_))));
        assert!(host_type("NotAHostType").is_none());
    }

    #[test]
    fn known_sync_op_resolves() {
        let sig = find("crypto", "random_bytes").expect("crypto.random_bytes is in the catalog");
        assert!(!sig.is_async);
        assert_eq!(sig.params.len(), 1);
    }

    #[test]
    fn known_async_op_resolves() {
        let sig = find("fs", "read_file").expect("fs.read_file is in the catalog");
        assert!(sig.is_async);
    }

    #[test]
    fn unknown_action_is_none() {
        assert!(find("crypto", "not_a_real_action").is_none());
    }

    #[test]
    fn unknown_kind_is_none_and_not_a_known_kind() {
        assert!(find("not_a_real_kind", "anything").is_none());
        assert!(!kind_exists("not_a_real_kind"));
    }

    #[test]
    fn bridge_op_is_async_matches_declared_async_flag() {
        assert!(bridge_op_is_async("fs", "read_file"));
        assert!(!bridge_op_is_async("crypto", "random_bytes"));
        assert!(!bridge_op_is_async("nope", "nope"));
    }
}
