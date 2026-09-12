use std::collections::BTreeMap;

use core::{Context, FlagSpec, ParamSpec, ParseError, ParseErrorKind, Registry};
use stdio::{ascii, error as stdio_error, raw};

pub mod bundle;
pub mod check;
pub mod emit;
pub mod fmt;
pub mod lsp;
pub mod plan;
pub mod summon;
pub mod transpile;

pub fn register_global_flags(registry: &mut Registry) {
    registry.add_flag(FlagSpec {
        name: "--help",
        aliases: &["-H", "help"],
        description: "show help",
    });
    registry.add_flag(FlagSpec {
        name: "--version",
        aliases: &["-V", "version"],
        description: "show version",
    });
    registry.add_flag(FlagSpec {
        name: "--verbose",
        aliases: &[],
        description: "show detailed metadata where supported",
    });
}

pub fn register_global_params(registry: &mut Registry) {
    registry.add_param(ParamSpec {
        name: "--outdir",
        description: "emit output directory",
    });
    registry.add_param(ParamSpec {
        name: "-o",
        description: "emit output directory",
    });
}

pub fn help(registry: &Registry) {
    raw(&ascii("dsc"));
    raw("");
    raw("Usage: dsc [options] [command]");
    raw(&format!(
        "dsc v{} — DekaScript compiler. Emits JavaScript. Does not run it.",
        env!("CARGO_PKG_VERSION")
    ));
    raw("With no command, emit app/, api/, and src/ to dist/.");
    raw("");

    let dim = "\x1b[2m";
    let reset = "\x1b[0m";

    let mut grouped: BTreeMap<&str, Vec<&core::CommandSpec>> = BTreeMap::new();
    for command in registry.commands() {
        grouped.entry(command.category).or_default().push(command);
    }

    for (category, commands) in grouped {
        raw(&format!("{dim}{category}{reset}"));
        for command in commands {
            raw(&format!("  {}\t\t{}", command.name, command.summary));
            if !command.subcommands.is_empty() {
                for subcommand in command.subcommands {
                    raw(&format!(
                        "  {} {}\t{}",
                        command.name, subcommand.name, subcommand.summary
                    ));
                }
            }
        }
        raw("");
    }

    if !registry.flags().is_empty() {
        raw(&format!("{dim}flags{reset}"));
        for flag in registry.flags() {
            raw(&format!("  {}\t\t{}", flag.name, flag.description));
        }
        raw("");
    }
}

pub fn version(verbose: bool) {
    let version = env!("CARGO_PKG_VERSION");
    raw(&format!("dsc [version {}]", version));
    if verbose {
        let git_sha = option_env!("DSC_GIT_SHA").unwrap_or("unknown");
        let build_unix = option_env!("DSC_BUILD_UNIX").unwrap_or("unknown");
        let target = option_env!("DSC_TARGET").unwrap_or("unknown");
        raw(&format!("git_sha: {}", git_sha));
        raw(&format!("build_unix: {}", build_unix));
        raw(&format!("target: {}", target));
    }
    raw("");
}

pub fn error(msg: Option<&str>) {
    stdio_error(
        "cli",
        msg.unwrap_or("instructions unclear. try '--help' for guidance"),
    );
}

/// Dispatches argv. Returns the process exit code: 0 on success, 2 on any
/// usage error (unknown argument/flag, missing param value, unknown command),
/// following the GNU exit-code convention. Command handlers keep owning
/// runtime failures (they exit 1 themselves).
pub fn execute(registry: &Registry) -> i32 {
    let parsed = core::parse_env(registry);
    if !parsed.errors.is_empty() {
        let message = format_parse_errors(&parsed.errors);
        error(Some(message.as_str()));
        return 2;
    }

    let args = &parsed.args;
    if args.commands.is_empty() {
        if args.flags.contains_key("--version")
            || args.flags.contains_key("-V")
            || args.flags.contains_key("version")
        {
            let verbose = args.flags.contains_key("--verbose");
            version(verbose);
            return 0;
        }
        if args.flags.contains_key("--help")
            || args.flags.contains_key("-H")
            || args.flags.contains_key("help")
        {
            help(registry);
            return 0;
        }
        let context = match Context::from_env(registry) {
            Ok(context) => context,
            Err(core::ContextError::Parse(errors)) => {
                let message = format_parse_errors(&errors);
                error(Some(message.as_str()));
                return 2;
            }
        };
        emit::cmd(&context);
        return 0;
    }

    let context = match Context::from_env(registry) {
        Ok(context) => context,
        Err(core::ContextError::Parse(errors)) => {
            let message = format_parse_errors(&errors);
            error(Some(message.as_str()));
            return 2;
        }
    };
    let cmd = &context.args;

    if cmd.commands.len() > 2 {
        error(None);
        return 2;
    }

    let cmd_name = &cmd.commands[0];
    let Some(command) = registry.command_named(cmd_name) else {
        error(None);
        return 2;
    };

    if cmd.commands.len() == 1 {
        (command.handler)(&context);
        return 0;
    }

    let sub_name = &cmd.commands[1];
    let Some(subcommand) = registry.subcommand_named(command, sub_name) else {
        error(None);
        return 2;
    };

    (subcommand.handler)(&context);
    0
}

pub fn format_parse_errors(errors: &[ParseError]) -> String {
    let mut output = String::new();
    for error in errors {
        match &error.kind {
            ParseErrorKind::UnknownToken => {
                output.push_str(&format!("unknown argument '{}'", error.token));
                if !error.suggestions.is_empty() {
                    output.push_str(". did you mean ");
                    output.push_str(&format_suggestions(&error.suggestions));
                    output.push('?');
                }
                output.push('\n');
            }
            ParseErrorKind::MissingParamValue { param } => {
                output.push_str(&format!("missing value for '{}'\n", param));
            }
        }
    }
    output
}

fn format_suggestions(suggestions: &[String]) -> String {
    suggestions
        .iter()
        .map(|suggestion| format!("'{}'", suggestion))
        .collect::<Vec<String>>()
        .join(", ")
}
