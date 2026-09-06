use std::collections::{HashMap, HashSet};

use crate::registry::Registry;

#[derive(Debug, Clone)]
pub struct Args {
    pub flags: HashMap<String, bool>,
    pub params: HashMap<String, String>,
    pub commands: Vec<String>,
    pub positionals: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ParseOutcome {
    pub args: Args,
    pub errors: Vec<ParseError>,
}

#[derive(Debug, Clone)]
pub struct ParseError {
    pub token: String,
    pub kind: ParseErrorKind,
    pub suggestions: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum ParseErrorKind {
    UnknownToken,
    MissingParamValue { param: String },
}

impl ParseError {
    fn unknown(token: String, suggestions: Vec<String>) -> Self {
        Self {
            token,
            kind: ParseErrorKind::UnknownToken,
            suggestions,
        }
    }

    fn missing_param(param: String) -> Self {
        Self {
            token: param.clone(),
            kind: ParseErrorKind::MissingParamValue { param },
            suggestions: Vec::new(),
        }
    }
}

impl Args {
    pub fn collect(args: Vec<String>, registry: &Registry) -> ParseOutcome {
        let mut flags = HashMap::new();
        let mut params = HashMap::new();
        let mut commands = Vec::new();
        let mut positionals = Vec::new();
        let mut errors = Vec::new();
        let mut flag_tokens: HashSet<&'static str> = HashSet::new();
        let mut param_tokens: HashSet<&'static str> = HashSet::new();
        let suggestion_tokens = registry.suggestion_tokens();

        for flag in registry.flags() {
            flag_tokens.insert(flag.name);
            for alias in flag.aliases {
                flag_tokens.insert(alias);
            }
        }

        for param in registry.params() {
            param_tokens.insert(param.name);
        }

        let mut current_command = None;
        let mut iter = args.iter().enumerate();
        while let Some((_i, arg)) = iter.next() {
            let arg_str = arg.as_str();
            if let Some((name, value)) = arg_str.split_once('=') {
                if flag_tokens.contains(name) {
                    flags.insert(name.to_string(), true);
                    params.insert(name.to_string(), value.to_string());
                    continue;
                }
            }
            if flag_tokens.contains(arg_str) {
                flags.insert(arg.clone(), true);
                continue;
            }

            if param_tokens.contains(arg_str) {
                if let Some(value) = iter.next().map(|(_, v)| v) {
                    params.insert(arg.clone(), value.clone());
                } else {
                    errors.push(ParseError::missing_param(arg.clone()));
                }
                continue;
            }

            if let Some(command) = current_command {
                if let Some(subcommand) = registry.subcommand_for(command, arg_str) {
                    commands.push(subcommand.name.to_string());
                    continue;
                }

                if arg_str.starts_with('-') {
                    let suggestions = suggest(arg_str, &suggestion_tokens);
                    errors.push(ParseError::unknown(arg.clone(), suggestions));
                    continue;
                }

                positionals.push(arg.clone());
                continue;
            }

            if let Some(command) = registry.command_for(arg_str) {
                commands.push(command.name.to_string());
                current_command = Some(command);
                continue;
            }

            if arg_str.starts_with('-') {
                let suggestions = suggest(arg_str, &suggestion_tokens);
                errors.push(ParseError::unknown(arg.clone(), suggestions));
                continue;
            }

            let suggestions = suggest(arg_str, &suggestion_tokens);
            errors.push(ParseError::unknown(arg.clone(), suggestions));
        }

        ParseOutcome {
            args: Args {
                flags,
                params,
                commands,
                positionals,
            },
            errors,
        }
    }
}

pub fn parse_env(registry: &Registry) -> ParseOutcome {
    #[cfg(target_arch = "wasm32")]
    let args: Vec<String> = Vec::new();
    #[cfg(not(target_arch = "wasm32"))]
    let args: Vec<String> = std::env::args().skip(1).collect();
    Args::collect(args, registry)
}

fn suggest(token: &str, candidates: &[String]) -> Vec<String> {
    let threshold = if token.len() <= 4 {
        1
    } else if token.len() <= 7 {
        2
    } else {
        3
    };

    let mut scored: Vec<(usize, &String)> = candidates
        .iter()
        .map(|candidate| (levenshtein(token, candidate), candidate))
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));

    let mut matches = Vec::new();
    for (distance, candidate) in scored {
        if distance <= threshold {
            matches.push(candidate.clone());
        }
        if matches.len() >= 3 {
            break;
        }
    }

    matches
}

fn levenshtein(a: &str, b: &str) -> usize {
    if a.is_empty() {
        return b.chars().count();
    }
    if b.is_empty() {
        return a.chars().count();
    }

    let b_len = b.chars().count();
    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0; b_len + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] =
                std::cmp::min(std::cmp::min(curr[j] + 1, prev[j + 1] + 1), prev[j] + cost);
        }
        prev.clone_from_slice(&curr);
    }

    prev[b_len]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{FlagSpec, Registry};

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
        let parsed = Args::collect(
            vec!["--allow-read".to_string()],
            &security_registry(),
        );
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
        let parsed = Args::collect(
            vec!["--deny-read=/etc".to_string()],
            &security_registry(),
        );
        assert!(parsed.errors.is_empty());
        assert_eq!(
            parsed.args.params.get("--deny-read").map(String::as_str),
            Some("/etc")
        );
    }
}
