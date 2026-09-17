//! Shared hash-verification code for dsc's pinned, checked-in artifacts.
//!
//! dsc pins two kinds of external artifact by SHA-256: the testsuite corpus
//! (`scripts/testsuite-corpus-version`, verified today by
//! `scripts/ci-fetch-testsuite-corpus.sh`) and, as of rfd#27's 2026-09-16
//! amendment, the generated host declaration file (`scripts/deka-runtime-version`,
//! verified here). Both follow the same shape: download bytes, hash them,
//! compare against a hash obtained from a trusted manifest, hard-fail on any
//! mismatch. This module is that one comparison, so a future third pin (or a
//! Rust port of the corpus fetch) has one function to call instead of a new
//! copy of the `sha256sum | expected` dance.

use sha2::{Digest, Sha256};

/// Hex-encoded SHA-256 of `bytes`, lowercase, matching `sha256sum`'s output.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Verify `bytes` hashes to `expected_hex` (case-insensitively). Mirrors the
/// `[[ "$actual" == "$expected" ]]` gate in `ci-fetch-testsuite-corpus.sh` and
/// `ci-install-deka-runtime.sh`, as one Rust function instead of three shell
/// copies of the same comparison.
pub fn verify_sha256(bytes: &[u8], expected_hex: &str) -> Result<(), String> {
    let actual = sha256_hex(bytes);
    let expected = expected_hex.trim().to_ascii_lowercase();
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "checksum mismatch: expected {expected}, got {actual}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_matches_known_vector() {
        // echo -n "abc" | sha256sum
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verify_accepts_matching_hash_case_insensitively() {
        let hash = sha256_hex(b"hello");
        assert!(verify_sha256(b"hello", &hash.to_ascii_uppercase()).is_ok());
    }

    #[test]
    fn verify_rejects_mismatch() {
        let err = verify_sha256(b"hello", &sha256_hex(b"world")).unwrap_err();
        assert!(err.contains("checksum mismatch"));
    }
}
