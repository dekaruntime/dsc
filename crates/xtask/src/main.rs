//! Repo maintenance tasks that do not belong in the published `dsc` binary.
//!
//! `cargo xtask sync-host-decl`      — dsc#272, rfd#27's 2026-09-16 amendment.
//! `cargo xtask check-host-decl-drift` — the same, run in CI on every PR.
//! `cargo xtask check-lockstep`      — dsc#293, rfd#68's 2026-09-17 amendment
//!                                     to rfd#59; run in CI on every PR.

mod host_decl;
mod lockstep;
mod pin;

fn main() {
    let mut args = std::env::args().skip(1);
    let command = args.next();
    let result = match command.as_deref() {
        Some("sync-host-decl") => host_decl::sync(),
        Some("check-host-decl-drift") => host_decl::check_drift(),
        Some("check-lockstep") => lockstep::check(),
        _ => {
            eprintln!("usage: cargo xtask <sync-host-decl|check-host-decl-drift|check-lockstep>");
            std::process::exit(2);
        }
    };
    if let Err(message) = result {
        eprintln!("xtask: {message}");
        std::process::exit(1);
    }
}
