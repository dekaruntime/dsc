use deka_compile::summon::infer_draft;

fn draft(fixture: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/summon/{fixture}",
        env!("CARGO_MANIFEST_DIR")
    );
    let source = std::fs::read_to_string(&path).unwrap();
    infer_draft(&source).unwrap()
}

fn signature<'a>(draft: &'a str, name: &str) -> &'a str {
    draft
        .lines()
        .map(str::trim)
        .find(|line| {
            line.strip_prefix("export total fn ")
                .or_else(|| line.strip_prefix("export fn "))
                .is_some_and(|rest| rest.starts_with(&format!("{name}(")))
        })
        .unwrap_or_else(|| panic!("draft is missing export `{name}`:\n{draft}"))
}

fn is_total(draft: &str, name: &str) -> bool {
    signature(draft, name).starts_with("export total ")
}

#[test]
fn basic_fixture_matches_golden_draft() {
    let got = draft("infer_basic.mjs");
    let expected = include_str!("fixtures/summon/infer_basic.draft.ds");
    assert_eq!(got, expected);
}

#[test]
fn throwing_fixture_matches_golden_draft() {
    let got = draft("infer_throw.mjs");
    let expected = include_str!("fixtures/summon/infer_throw.draft.ds");
    assert_eq!(got, expected);
}

#[test]
fn throwing_fixture_never_drafts_total() {
    let got = draft("infer_throw.mjs");
    assert!(got.contains("DRAFT — review before committing"), "{got}");
    for name in ["boom", "parses", "callsUnknown", "usesHelper"] {
        assert!(
            !is_total(&got, name),
            "{name} must not be drafted total:\n{}",
            signature(&got, name)
        );
        assert!(
            signature(&got, name).contains("Exception<"),
            "{name} must use the pessimistic Exception default:\n{}",
            signature(&got, name)
        );
    }
    assert!(is_total(&got, "ok"), "{}", signature(&got, "ok"));
    assert!(is_total(&got, "caught"), "{}", signature(&got, "caught"));
}

#[test]
fn infer_draft_from_path_reads_the_vendored_module() {
    let path = format!(
        "{}/tests/fixtures/summon/infer_throw.mjs",
        env!("CARGO_MANIFEST_DIR")
    );
    let got = deka_compile::summon::infer_draft_from_path(std::path::Path::new(&path)).unwrap();
    assert!(!is_total(&got, "boom"), "{}", signature(&got, "boom"));
}

#[test]
fn rejects_unparseable_javascript() {
    let err = infer_draft("export function broken( {").unwrap_err();
    assert!(!err.is_empty(), "{err}");
}

#[test]
fn inferred_signatures_are_colon_free_and_parseable() {
    for fixture in ["infer_basic.mjs", "infer_throw.mjs"] {
        let got = draft(fixture);
        let arena = bumpalo::Bump::new();
        let parsed = deka_syntax::parse::parse(&got, &arena);
        assert!(parsed.errors.is_empty(), "{got}\n{:?}", parsed.errors);
        assert!(parsed.program.is_some());
        for line in got
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
        {
            assert!(!line.contains("):"), "{line}");
        }
    }
}
