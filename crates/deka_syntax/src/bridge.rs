//! Host bridge op catalog (rfd#27).
//!
//! The bridge itself does not know sync from async — the catalog does
//! (rfd#27 decision 2). Each `bridge kind.action(args)` call maps to one host
//! op, declared here as synchronous or asynchronous. The typechecker uses this
//! to give sync ops the type `Result<T, E>` and async ops
//! `Promise<Result<T, E>>`, so `await bridge fs.read_file(path)` typechecks
//! while `bridge crypto.random_bytes(n)` stays a plain `Result`.
//!
//! This table is the compiler-side mirror of the host op declarations in
//! `deka_host` (`#[op2]` vs `#[op2(async)]`) and the runtime allowlist
//! (`DS_HOST_CATALOG` in `crates/pool/src/isolate_pool/worker_execution.rs`).
//! All three must agree: an op marked async here must be dispatched through
//! an async op host-side, or the emitted `.then(__deka_to_result)` would be a
//! no-op on a plain value and the isolate would keep blocking.

/// One entry in the bridge op catalog.
pub struct BridgeOp {
    pub kind: &'static str,
    pub action: &'static str,
    /// True when the host op is declared `#[op2(async)]` and the bridge
    /// dispatch returns a Promise.
    pub is_async: bool,
}

/// The full catalog of host ops reachable through `bridge kind.action(..)`.
/// Mirrors `DS_HOST_CATALOG`; unknown kinds/actions are rejected by the
/// runtime allowlist, not here.
pub const BRIDGE_OPS: &[BridgeOp] = &[
    // crypto — rfd#27's canonical sync op.
    BridgeOp { kind: "crypto", action: "random_bytes", is_async: false },
    BridgeOp { kind: "crypto", action: "digest", is_async: false },
    BridgeOp { kind: "crypto", action: "hmac", is_async: false },
    BridgeOp { kind: "crypto", action: "secure_compare", is_async: false },
    BridgeOp { kind: "crypto", action: "aes_256_gcm_encrypt", is_async: false },
    BridgeOp { kind: "crypto", action: "aes_256_gcm_decrypt", is_async: false },
    BridgeOp { kind: "crypto", action: "bcrypt_verify", is_async: false },
    // fs — blocking std::fs IO; dispatched through the async op so a read or
    // write does not stall the isolate (deka#578).
    BridgeOp { kind: "fs", action: "read_file", is_async: true },
    BridgeOp { kind: "fs", action: "write_file", is_async: true },
    BridgeOp { kind: "fs", action: "read_dir", is_async: true },
    BridgeOp { kind: "fs", action: "mkdirs", is_async: true },
    // net / tls — synchronous std::net dispatch today.
    BridgeOp { kind: "net", action: "connect", is_async: false },
    BridgeOp { kind: "net", action: "listen", is_async: false },
    BridgeOp { kind: "net", action: "accept", is_async: false },
    BridgeOp { kind: "net", action: "read", is_async: false },
    BridgeOp { kind: "net", action: "write", is_async: false },
    BridgeOp { kind: "net", action: "close", is_async: false },
    BridgeOp { kind: "net", action: "set_deadline", is_async: false },
    BridgeOp { kind: "tls", action: "upgrade", is_async: false },
    // time
    BridgeOp { kind: "time", action: "sleep_ms", is_async: false },
];

/// Whether the host dispatches `kind.action` asynchronously, i.e. the bridge
/// call evaluates to a `Promise` rather than a plain value.
pub fn bridge_op_is_async(kind: &str, action: &str) -> bool {
    BRIDGE_OPS
        .iter()
        .any(|op| op.kind == kind && op.action == action && op.is_async)
}
