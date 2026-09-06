//! Format → parse → format round-trip over the conformance corpus
//! (deka#477).
//!
//! Two invariants, one class of bug:
//!
//! 1. **Output must parse.** A formatter that emits something the parser
//!    rejects is performing a semantic rewrite, not formatting (#453:
//!    `None` → `none`; deka#479: `fn increment() mut void;`). Running this
//!    over the whole corpus closes the class instead of each instance.
//! 2. **Formatting must be idempotent.** The second format is byte-identical
//!    to the first. Non-idempotent output means the "canonical" form is not
//!    a fixed point, so two hosts (or two runs) can disagree forever.
//!
//! The dual-host half of the invariant — native and wasm producing identical
//! bytes for the same input — is asserted by the dump harness
//! (`tests/dump`, `fmtHostsAgree`), which runs this comparison on both
//! hosts per fixture.

use std::fs;
use std::path::{Path, PathBuf};

/// Collect every corpus source file: testsuite pass fixtures and tour
/// lessons. Fail fixtures are excluded — they intentionally do not parse,
/// and the formatter contract for unparseable input is "returned unchanged".
/// Both .ds and .dsx pass fixtures are collected: matching "pass.ds" alone
/// misses every "pass.dsx" (deka#493 — the same .ds-only assumption that made
/// `cli fmt <dir>` silently skip .dsx).
fn corpus_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
    let mut files = Vec::new();
    collect(&root.join("testsuite"), "pass.ds", &mut files);
    collect(&root.join("testsuite"), "pass.dsx", &mut files);
    collect(&root.join("tour"), "ds", &mut files);
    collect(&root.join("tour"), "dsx", &mut files);
    files.sort();
    files
}

fn collect(dir: &Path, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, suffix, out);
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(suffix))
        {
            out.push(path);
        }
    }
}

#[test]
fn fmt_output_parses_and_is_idempotent_across_corpus() {
    let files = corpus_files();
    if files.is_empty() {
        panic!("tests/testsuite and tests/tour are missing");
    }
    assert!(
        files.len() > 100,
        "corpus lookup is broken: only {} files found",
        files.len()
    );

    let mut unparseable = Vec::new();
    let mut unstable = Vec::new();
    let mut comments_lost = Vec::new();

    for file in &files {
        let source = fs::read_to_string(file)
            .unwrap_or_else(|err| panic!("cannot read {}: {err}", file.display()));

        // The property only governs valid programs: if the input itself does
        // not parse (diagnostic lessons, fail fixtures), the formatter's
        // contract is "returned unchanged", which is not a round-trip bug.
        let input_arena = bumpalo::Bump::new();
        let input_parsed = deka_syntax::parse::parse(&source, &input_arena);
        if !input_parsed.errors.is_empty() || input_parsed.program.is_none() {
            continue;
        }

        let once = deka_fmt::format_ds(&source)
            .unwrap_or_else(|err| panic!("fmt failed on {}: {err}", file.display()));

        let arena = bumpalo::Bump::new();
        let reparsed = deka_syntax::parse::parse(&once, &arena);
        if !reparsed.errors.is_empty() || reparsed.program.is_none() {
            unparseable.push(format!(
                "{}: {}",
                file.display(),
                reparsed
                    .errors
                    .first()
                    .map(|d| d.message.clone())
                    .unwrap_or_default()
            ));
            continue;
        }

        let twice = deka_fmt::format_ds(&once)
            .unwrap_or_else(|err| panic!("re-fmt failed on {}: {err}", file.display()));
        if once != twice {
            unstable.push(file.display().to_string());
        }

        // deka#484: every `//` line comment in the input must survive.
        // Counted via the lexer so strings containing "//" do not confuse
        // the assertion; comments inside unsafe {} bodies are raw JS
        // passthrough and preserved by construction.
        let before = count_line_comments(&source);
        let after = count_line_comments(&once);
        if before != after {
            comments_lost.push(format!(
                "{}: {} comment(s) in, {} out",
                file.display(),
                before,
                after
            ));
        }
    }

    assert!(
        unparseable.is_empty(),
        "fmt output does not parse for {} file(s):\n{}",
        unparseable.len(),
        unparseable.join("\n")
    );
    assert!(
        unstable.is_empty(),
        "fmt is not idempotent for {} file(s):\n{}",
        unstable.len(),
        unstable.join("\n")
    );
    assert!(
        comments_lost.is_empty(),
        "fmt dropped or duplicated // comments in {} file(s):\n{}",
        comments_lost.len(),
        comments_lost.join("\n")
    );
}

fn count_line_comments(source: &str) -> usize {
    let mut lexer = deka_syntax::Lexer::new(source);
    let mut count = 0;
    loop {
        let token = lexer.next_token();
        if token.kind == deka_syntax::lexer::TokenKind::Comment && token.text.starts_with("//") {
            count += 1;
        }
        if token.kind == deka_syntax::lexer::TokenKind::Eof {
            break;
        }
    }
    count
}
