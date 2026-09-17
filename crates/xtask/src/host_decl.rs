//! Sync and drift-check for the embedded host declaration file
//! (`deka-host.d.ds`), rfd#27's 2026-09-16 amendment.
//!
//! deka publishes `deka-host.d.ds` beside its release binaries on
//! `releases.deka.gg`, with its SHA-256 listed under `host_decl.sha256` in
//! that release's `release.json` (the same manifest
//! `scripts/ci-install-deka-runtime.sh` reads for the CLI binaries' hashes).
//! dsc checks in a copy, moved only in the same commit as a
//! `scripts/deka-runtime-version` bump, so the diff shows exactly which ops
//! changed and dsc builds (including wasm) stay offline and hermetic.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::pin;

/// Path (relative to the repo root) of the checked-in copy embedded by
/// `deka_syntax::bridge` via `include_str!`.
pub const CHECKED_IN_PATH: &str = "crates/deka_syntax/host-decl/deka-host.d.ds";

const VERSION_PIN_PATH: &str = "scripts/deka-runtime-version";

fn repo_root() -> Result<PathBuf, String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("cannot find repo root above {manifest_dir}"))
}

fn releases_base_url() -> String {
    std::env::var("DEKA_RELEASES_BASE_URL").unwrap_or_else(|_| "https://releases.deka.gg".into())
}

fn read_pinned_version(root: &Path) -> Result<String, String> {
    let path = root.join(VERSION_PIN_PATH);
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("reading {}: {e}", path.display()))?;
    let version = raw.trim();
    if version.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    Ok(version.to_string())
}

/// Download `url` with `curl`, the same tool `ci-fetch-testsuite-corpus.sh`
/// and `ci-install-deka-runtime.sh` already require to be on PATH. A Rust
/// HTTP client is deliberately not pulled into the workspace for one dev-only
/// sync command.
fn curl_bytes(url: &str) -> Result<Vec<u8>, String> {
    let output = Command::new("curl")
        .args(["-fsSL", url])
        .output()
        .map_err(|e| format!("running curl for {url}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "curl {url} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(output.stdout)
}

struct Release {
    host_decl_bytes: Vec<u8>,
    host_decl_sha256: String,
}

fn fetch_release(version: &str) -> Result<Release, String> {
    let base = releases_base_url();
    let manifest_bytes = curl_bytes(&format!("{base}/{version}/release.json"))?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| format!("parsing release.json for {version}: {e}"))?;
    let expected = manifest
        .get("host_decl")
        .and_then(|h| h.get("sha256"))
        .and_then(|s| s.as_str())
        .ok_or_else(|| {
            format!("release.json for {version} has no host_decl.sha256 (deka has not published the host declaration file for this release yet)")
        })?
        .to_string();
    let file_name = manifest
        .get("host_decl")
        .and_then(|h| h.get("file"))
        .and_then(|s| s.as_str())
        .unwrap_or("deka-host.d.ds");
    let bytes = curl_bytes(&format!("{base}/{version}/{file_name}"))?;
    pin::verify_sha256(&bytes, &expected)
        .map_err(|e| format!("deka-host.d.ds for {version}: {e}"))?;
    Ok(Release {
        host_decl_bytes: bytes,
        host_decl_sha256: expected,
    })
}

/// `cargo xtask sync-host-decl`: download the pinned version's
/// `deka-host.d.ds`, verify it against that release's manifest, and
/// overwrite the checked-in copy. Bump `scripts/deka-runtime-version` first,
/// then run this, then commit both together.
pub fn sync() -> Result<(), String> {
    let root = repo_root()?;
    let version = read_pinned_version(&root)?;
    let release = fetch_release(&version)?;
    let dest = root.join(CHECKED_IN_PATH);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(&dest, &release.host_decl_bytes)
        .map_err(|e| format!("writing {}: {e}", dest.display()))?;
    println!(
        "synced {} from deka {version} (sha256 {})",
        dest.display(),
        release.host_decl_sha256
    );
    Ok(())
}

/// Pure byte-for-byte comparison, pulled out of [`check_drift`] so the "one
/// changed byte is a hard failure" behavior is unit-testable without a
/// network call. `label` names the upstream version in the error, so the CLI
/// wrapper does not need to duplicate the message.
fn detect_drift(checked_in: &[u8], upstream: &[u8], label: &str) -> Result<(), String> {
    if checked_in == upstream {
        Ok(())
    } else {
        Err(format!(
            "{} is out of sync with {label}'s deka-host.d.ds; run `cargo run -p xtask -- sync-host-decl` and commit the result",
            CHECKED_IN_PATH
        ))
    }
}

/// `cargo xtask check-host-decl-drift`: CI's per-PR gate. Downloads the file
/// for the pinned version, verifies it against that release's manifest, and
/// hard-fails if it differs from the checked-in copy byte-for-byte — a hand
/// edit, or a `deka-runtime-version` bump without a `sync-host-decl`, fails
/// this the same way.
pub fn check_drift() -> Result<(), String> {
    let root = repo_root()?;
    let version = read_pinned_version(&root)?;
    let release = fetch_release(&version)?;
    let dest = root.join(CHECKED_IN_PATH);
    let checked_in = std::fs::read(&dest).map_err(|e| {
        format!(
            "reading checked-in {} (run `cargo run -p xtask -- sync-host-decl`): {e}",
            dest.display()
        )
    })?;
    detect_drift(&checked_in, &release.host_decl_bytes, &format!("deka {version}"))?;
    println!(
        "{} matches deka {version} (sha256 {})",
        dest.display(),
        release.host_decl_sha256
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CI drift check (dsc#272 item 4): a single changed byte between the
    /// checked-in copy and what the pinned release actually publishes is a
    /// hard failure, not a warning.
    #[test]
    fn one_byte_edit_is_detected_as_drift() {
        let upstream: &[u8] =
            b"bridge crypto {\n  fn random_bytes(len: number) Result<bytes, string>\n}\n";
        let mut edited = upstream.to_vec();
        // Flip one byte in the checked-in copy, as a hand edit would. XOR
        // guarantees a different byte regardless of its original value.
        let i = edited.len() / 2;
        edited[i] ^= 0xFF;
        assert_ne!(edited.as_slice(), upstream);
        let err = detect_drift(&edited, upstream, "deka 0.0.0-test").unwrap_err();
        assert!(err.contains("out of sync"), "{err}");
    }

    #[test]
    fn identical_bytes_are_not_drift() {
        let bytes = b"bridge crypto {\n  fn random_bytes(len: number) Result<bytes, string>\n}\n";
        assert!(detect_drift(bytes, bytes, "deka 0.0.0-test").is_ok());
    }
}
