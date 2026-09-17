//! Repo maintenance tasks that do not belong in the published `dsc` binary.
//!
//! `cargo xtask sync-host-decl`      — dsc#272, rfd#27's 2026-09-16 amendment.
//! `cargo xtask check-host-decl-drift` — the same, run in CI on every PR.

mod host_decl;
mod pin;

fn main() {
    let mut args = std::env::args().skip(1);
    let command = args.next();
    let result = match command.as_deref() {
        Some("sync-host-decl") => host_decl::sync(),
        Some("check-host-decl-drift") => host_decl::check_drift(),
        _ => {
            eprintln!("usage: cargo xtask <sync-host-decl|check-host-decl-drift>");
            std::process::exit(2);
        }
    };
    if let Err(message) = result {
        eprintln!("xtask: {message}");
        std::process::exit(1);
    }
}
