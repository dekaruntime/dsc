//! Lockstep pin check — rfd#68's 2026-09-17 amendment to rfd#59: deka, dsc,
//! testsuite and tour share ONE version, on both channels. This enforces it
//! in dsc: dsc's own crate version (the workspace `Cargo.toml`) is the
//! source of truth, and `scripts/deka-runtime-version` /
//! `scripts/testsuite-corpus-version` must each carry that version — or the
//! immediately previous released version, while a bump is mid-flight (deka
//! and dsc cannot both publish the new number in the same instant). Any
//! other value fails. See dekaruntime/rfd#68 and dsc#293.
//!
//! `cargo xtask check-lockstep` runs this against the real repo state (git
//! tags, the checked-in pin files). The unit tests below exercise the pure
//! decision logic (`check_pin`, `parse_pin`) with fixture strings only — no
//! git, no filesystem, no network.

use std::path::{Path, PathBuf};
use std::process::Command;

const DEKA_RUNTIME_VERSION_PATH: &str = "scripts/deka-runtime-version";
const TESTSUITE_CORPUS_VERSION_PATH: &str = "scripts/testsuite-corpus-version";

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
}

impl SemVer {
    fn parse(s: &str) -> Result<SemVer, String> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 3 {
            return Err(format!("expected X.Y.Z, found \"{s}\""));
        }
        let mut nums = [0u64; 3];
        for (i, p) in parts.iter().enumerate() {
            nums[i] = p
                .parse::<u64>()
                .map_err(|_| format!("expected X.Y.Z, found \"{s}\""))?;
        }
        Ok(SemVer {
            major: nums[0],
            minor: nums[1],
            patch: nums[2],
        })
    }
}

impl std::fmt::Display for SemVer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

struct Pin {
    base: SemVer,
}

/// Parse a pin's line 1, given the literal prefix it must start with ("" for
/// a bare version pin such as `scripts/deka-runtime-version`, `"corpus-v"`
/// for the corpus pin). Accepts an optional `-canary-<sha>` suffix with a
/// non-empty sha.
fn parse_pin(raw: &str, prefix: &str) -> Result<Pin, String> {
    let trimmed = raw.trim();
    let rest = trimmed
        .strip_prefix(prefix)
        .ok_or_else(|| format!("expected prefix \"{prefix}\""))?;

    let base_str = match rest.split_once("-canary-") {
        Some((base, sha)) => {
            if sha.is_empty() {
                return Err("empty canary sha".to_string());
            }
            base
        }
        None => rest,
    };

    let base = SemVer::parse(base_str)?;
    Ok(Pin { base })
}

/// Core decision: does `raw_pin` (a pin file's line 1) carry `current` or
/// `previous` (the immediately previous released set version, if any) as
/// its base version? `label` and `prefix` identify which pin this is, for
/// the error message. Ok(()) on pass; Err(message) states what was expected
/// and what was found — bad shape, wrong prefix, or a base version that is
/// neither `current` nor `previous`.
fn check_pin(
    label: &str,
    prefix: &str,
    raw_pin: &str,
    current: SemVer,
    previous: Option<SemVer>,
) -> Result<(), String> {
    let expected = match previous {
        Some(p) => format!("{current} or {p}"),
        None => current.to_string(),
    };

    let pin = parse_pin(raw_pin, prefix).map_err(|e| {
        format!(
            "{label}: expected \"{prefix}X.Y.Z\" or \"{prefix}X.Y.Z-canary-<sha>\" with X.Y.Z = {expected}, found \"{}\" ({e})",
            raw_pin.trim()
        )
    })?;

    if pin.base == current || Some(pin.base) == previous {
        return Ok(());
    }

    Err(format!(
        "{label}: expected base version {expected}, found {} in \"{}\"",
        pin.base,
        raw_pin.trim()
    ))
}

fn repo_root() -> Result<PathBuf, String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("cannot find repo root above {manifest_dir}"))
}

fn read_own_version(root: &Path) -> Result<SemVer, String> {
    let path = root.join("Cargo.toml");
    let raw =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    for line in raw.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("version")
            && let Some(rest) = rest.trim_start().strip_prefix('=')
        {
            let value = rest.trim().trim_matches('"');
            return SemVer::parse(value)
                .map_err(|e| format!("parsing version in {}: {e}", path.display()));
        }
    }
    Err(format!("no version line found in {}", path.display()))
}

fn read_pin_file(root: &Path, rel: &str) -> Result<String, String> {
    let path = root.join(rel);
    let raw =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    raw.lines()
        .next()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .ok_or_else(|| format!("{} has no line 1", path.display()))
}

/// The highest `vX.Y.Z` tag (canary tags excluded) strictly below `current`,
/// via `git tag --list 'v*' --sort=-v:refname` run in `root`. `None` if no
/// such tag exists.
fn previous_released_version(root: &Path, current: SemVer) -> Result<Option<SemVer>, String> {
    let output = Command::new("git")
        .args(["tag", "--list", "v*", "--sort=-v:refname"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("running git tag: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git tag failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let Some(rest) = line.trim().strip_prefix('v') else {
            continue;
        };
        // Stable tags only: exactly "X.Y.Z" — a canary tag's "-canary-<sha>"
        // tail makes SemVer::parse reject it, so it is skipped here.
        let Ok(v) = SemVer::parse(rest) else {
            continue;
        };
        if v < current {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// `cargo xtask check-lockstep`: verify dsc's own pins against dsc's crate
/// version. Prints one result line per pin and returns an error (nonzero
/// exit via `main`) if either pin fails.
pub fn check() -> Result<(), String> {
    let root = repo_root()?;
    let current = read_own_version(&root)?;
    let previous = previous_released_version(&root, current)?;

    let runtime_pin = read_pin_file(&root, DEKA_RUNTIME_VERSION_PATH)?;
    let corpus_pin = read_pin_file(&root, TESTSUITE_CORPUS_VERSION_PATH)?;

    let results = [
        check_pin(
            DEKA_RUNTIME_VERSION_PATH,
            "",
            &runtime_pin,
            current,
            previous,
        ),
        check_pin(
            TESTSUITE_CORPUS_VERSION_PATH,
            "corpus-v",
            &corpus_pin,
            current,
            previous,
        ),
    ];

    let mut failed = false;
    for result in &results {
        match result {
            Ok(()) => {}
            Err(message) => {
                eprintln!("FAIL {message}");
                failed = true;
            }
        }
    }

    if failed {
        return Err("lockstep check failed (dekaruntime/rfd#68 amendment to rfd#59)".to_string());
    }

    println!(
        "lockstep ok: dsc {current}; pins carry {current}{}",
        previous
            .map(|p| format!(" or the previous set version {p}"))
            .unwrap_or_default()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> SemVer {
        SemVer::parse(s).unwrap()
    }

    #[test]
    fn pin_matching_current_version_passes() {
        assert!(
            check_pin(
                "deka-runtime-version",
                "",
                "0.53.7",
                v("0.53.7"),
                Some(v("0.53.6"))
            )
            .is_ok()
        );
    }

    #[test]
    fn pin_with_canary_suffix_at_current_version_passes() {
        assert!(
            check_pin(
                "deka-runtime-version",
                "",
                "0.53.7-canary-b5228e0",
                v("0.53.7"),
                Some(v("0.53.6"))
            )
            .is_ok()
        );
    }

    #[test]
    fn corpus_pin_matching_current_version_passes() {
        assert!(
            check_pin(
                "testsuite-corpus-version",
                "corpus-v",
                "corpus-v0.53.7",
                v("0.53.7"),
                Some(v("0.53.6"))
            )
            .is_ok()
        );
    }

    #[test]
    fn pin_lagging_by_one_set_version_passes() {
        // dsc main today: dsc is 0.53.6, deka-runtime-version pins
        // 0.53.7-canary-... which is *ahead*, not lagging — that fails (see
        // pin_ahead_of_current_fails). This is the legitimate lag case: the
        // repo is already 0.53.7 and the pin still carries the previous set
        // version while the other side of the bump lands.
        assert!(
            check_pin(
                "deka-runtime-version",
                "",
                "0.53.6-canary-5d18cc5",
                v("0.53.7"),
                Some(v("0.53.6"))
            )
            .is_ok()
        );
    }

    #[test]
    fn pin_two_versions_behind_fails() {
        let err = check_pin(
            "testsuite-corpus-version",
            "corpus-v",
            "corpus-v0.53.4",
            v("0.53.7"),
            Some(v("0.53.6")),
        )
        .unwrap_err();
        assert!(
            err.contains("expected base version 0.53.7 or 0.53.6"),
            "{err}"
        );
        assert!(err.contains("found 0.53.4"), "{err}");
    }

    #[test]
    fn pin_ahead_of_current_fails() {
        // dsc main today (dsc#293's own motivating state): dsc is 0.53.6,
        // deka-runtime-version pins 0.53.7-canary-b5228e0 — ahead of both the
        // current and previous set version. Never allowed.
        let err = check_pin(
            "deka-runtime-version",
            "",
            "0.53.7-canary-b5228e0",
            v("0.53.6"),
            Some(v("0.53.5")),
        )
        .unwrap_err();
        assert!(
            err.contains("expected base version 0.53.6 or 0.53.5"),
            "{err}"
        );
        assert!(err.contains("found 0.53.7"), "{err}");
    }

    #[test]
    fn mismatched_corpus_tag_prefix_fails() {
        let err = check_pin(
            "testsuite-corpus-version",
            "corpus-v",
            "v0.53.7",
            v("0.53.7"),
            Some(v("0.53.6")),
        )
        .unwrap_err();
        assert!(err.contains("expected prefix"), "{err}");
    }

    #[test]
    fn empty_canary_sha_fails() {
        assert!(parse_pin("0.53.7-canary-", "").is_err());
    }

    #[test]
    fn malformed_version_fails() {
        assert!(parse_pin("0.53", "").is_err());
    }

    #[test]
    fn no_previous_version_message_names_current_only() {
        let err = check_pin("deka-runtime-version", "", "0.40.0", v("0.53.7"), None).unwrap_err();
        assert!(err.contains("expected base version 0.53.7,"), "{err}");
        assert!(!err.contains(" or "), "{err}");
    }
}
