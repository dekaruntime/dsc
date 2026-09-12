use core::{Args, FlagSpec, Registry};

fn security_registry() -> Registry {
    let mut registry = Registry::new();
    registry.add_flag(FlagSpec {
        name: "--allow-read",
        aliases: &[],
        description: "allow filesystem reads",
    });
    registry.add_flag(FlagSpec {
        name: "--deny-read",
        aliases: &[],
        description: "deny filesystem reads",
    });
    registry.add_flag(FlagSpec {
        name: "--allow-net",
        aliases: &[],
        description: "allow network",
    });
    registry.add_flag(FlagSpec {
        name: "--verbose",
        aliases: &[],
        description: "verbose",
    });
    registry
}

#[test]
fn parses_bare_allow_read_flag() {
    let parsed = Args::collect(vec!["--allow-read".to_string()], &security_registry());
    assert!(parsed.errors.is_empty());
    assert_eq!(parsed.args.flags.get("--allow-read"), Some(&true));
    assert!(parsed.args.params.get("--allow-read").is_none());
}

#[test]
fn parses_allow_read_equals_list() {
    let parsed = Args::collect(
        vec!["--allow-read=./src,./data".to_string()],
        &security_registry(),
    );
    assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    assert_eq!(parsed.args.flags.get("--allow-read"), Some(&true));
    assert_eq!(
        parsed.args.params.get("--allow-read").map(String::as_str),
        Some("./src,./data")
    );
}

#[test]
fn parses_deny_read_equals_path() {
    let parsed = Args::collect(vec!["--deny-read=/etc".to_string()], &security_registry());
    assert!(parsed.errors.is_empty());
    assert_eq!(
        parsed.args.params.get("--deny-read").map(String::as_str),
        Some("/etc")
    );
}

#[test]
fn path_tokens_without_command_are_positionals() {
    let parsed = Args::collect(vec!["app/main.ds".to_string()], &security_registry());
    assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    assert_eq!(parsed.args.positionals, vec!["app/main.ds"]);
}

#[test]
fn unknown_words_without_command_still_error() {
    let parsed = Args::collect(vec!["chek".to_string()], &security_registry());
    assert!(!parsed.errors.is_empty());
    assert!(parsed.args.positionals.is_empty());
}
