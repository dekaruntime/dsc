use core::{CommandSpec, Context, ParamSpec, Registry};
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;

const COMMAND: CommandSpec = CommandSpec {
    name: "fmt",
    owner: "legacy",
    category: "compiler",
    summary: "format DekaScript source or emitted JavaScript",
    aliases: &[],
    subcommands: &[],
    handler: cmd,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(COMMAND);
    registry.add_param(ParamSpec {
        name: "--lang",
        description: "language to format: ds or js (default: ds)",
    });
    registry.add_flag(core::FlagSpec {
        name: "--check",
        aliases: &[],
        description: "exit non-zero if files would change",
    });
    registry.add_flag(core::FlagSpec {
        name: "--stdin",
        aliases: &[],
        description: "read source from stdin instead of a file",
    });
}

pub fn cmd(context: &Context) {
    if context.args.flags.get("--help").copied().unwrap_or(false)
        || context.args.flags.get("-H").copied().unwrap_or(false)
        || context.args.flags.get("help").copied().unwrap_or(false)
    {
        stdio::raw(usage());
        return;
    }
    if let Err(err) = run(context) {
        stdio::error("fmt", &err);
        std::process::exit(1);
    }
}

fn usage() -> &'static str {
    "usage: dsc fmt [file-or-directory] [--lang ds|js] [--check] [--stdin]\n\n\
     Format DekaScript source or JavaScript output.\n\
     Without --stdin, reads from the given path (file or directory).\n\
     With --stdin, reads from standard input and writes to standard output.\n\n\
     Options:\n\
       --lang ds|js   language to format (default: ds)\n\
       --check        exit with code 1 if any file would be reformatted\n\
       --stdin        read source from stdin and write formatted output to stdout\n\n\
     Examples:\n\
       dsc fmt app/main.ds\n\
       dsc fmt --lang=js dist/app.js\n\
       echo 'const x=1' | dsc fmt --lang=js --stdin"
}

fn run(context: &Context) -> Result<(), String> {
    let lang = context
        .args
        .params
        .get("--lang")
        .map(|s| s.as_str())
        .unwrap_or("ds");
    let check = context
        .args
        .flags
        .get("--check")
        .copied()
        .unwrap_or(false);
    let stdin = context
        .args
        .flags
        .get("--stdin")
        .copied()
        .unwrap_or(false);

    match lang {
        "js" => run_js(context, stdin, check),
        "ds" => run_ds(context, stdin, check),
        _ => Err(format!("unsupported language: {} (expected ds or js)", lang)),
    }
}

fn run_js(context: &Context, stdin: bool, check: bool) -> Result<(), String> {
    if stdin {
        let mut source = String::new();
        io::stdin()
            .read_to_string(&mut source)
            .map_err(|err| format!("failed to read stdin: {err}"))?;
        let formatted = deka_fmt::format_js(&source)?;
        if check {
            if formatted != source {
                std::process::exit(1);
            }
            return Ok(());
        }
        io::stdout()
            .write_all(formatted.as_bytes())
            .map_err(|err| format!("failed to write stdout: {err}"))?;
        return Ok(());
    }

    let input = context
        .args
        .positionals
        .first()
        .ok_or_else(|| usage().to_string())?;
    let path = PathBuf::from(input);
    if path.is_file() {
        format_js_file(&path, check)
    } else if path.is_dir() {
        format_js_directory(&path, check)
    } else {
        Err(format!("input path does not exist: {}", path.display()))
    }
}

fn format_js_file(path: &std::path::Path, check: bool) -> Result<(), String> {
    let source = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let formatted = deka_fmt::format_js(&source)?;
    if check {
        if formatted != source {
            stdio::error("fmt", &format!("{} would be reformatted", path.display()));
            std::process::exit(1);
        }
        return Ok(());
    }
    fs::write(path, formatted)
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    stdio::success(&format!("formatted {}", path.display()));
    Ok(())
}

fn format_js_directory(root: &std::path::Path, check: bool) -> Result<(), String> {
    let mut changed = false;
    for entry in walkdir(root, &["js"])? {
        let source = fs::read_to_string(&entry)
            .map_err(|err| format!("failed to read {}: {err}", entry.display()))?;
        let formatted = deka_fmt::format_js(&source)?;
        if formatted != source {
            if check {
                changed = true;
                stdio::error("fmt", &format!("{} would be reformatted", entry.display()));
                continue;
            }
            fs::write(&entry, formatted)
                .map_err(|err| format!("failed to write {}: {err}", entry.display()))?;
            stdio::success(&format!("formatted {}", entry.display()));
        }
    }
    if check && changed {
        std::process::exit(1);
    }
    Ok(())
}

fn run_ds(context: &Context, stdin: bool, check: bool) -> Result<(), String> {
    if stdin {
        let mut source = String::new();
        io::stdin()
            .read_to_string(&mut source)
            .map_err(|err| format!("failed to read stdin: {err}"))?;
        let formatted = deka_fmt::format_ds(&source)?;
        if check {
            if formatted != source {
                std::process::exit(1);
            }
            return Ok(());
        }
        io::stdout()
            .write_all(formatted.as_bytes())
            .map_err(|err| format!("failed to write stdout: {err}"))?;
        return Ok(());
    }

    let input = context
        .args
        .positionals
        .first()
        .ok_or_else(|| usage().to_string())?;
    let path = PathBuf::from(input);
    if path.is_file() {
        format_ds_file(&path, check)
    } else if path.is_dir() {
        format_ds_directory(&path, check)
    } else {
        Err(format!("input path does not exist: {}", path.display()))
    }
}

fn format_ds_file(path: &std::path::Path, check: bool) -> Result<(), String> {
    let source = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let formatted = deka_fmt::format_ds(&source)?;
    if check {
        if formatted != source {
            stdio::error("fmt", &format!("{} would be reformatted", path.display()));
            std::process::exit(1);
        }
        return Ok(());
    }
    fs::write(path, formatted)
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    stdio::success(&format!("formatted {}", path.display()));
    Ok(())
}

fn format_ds_directory(root: &std::path::Path, check: bool) -> Result<(), String> {
    let mut changed = false;
    let mut reformatted = 0usize;
    let entries = walkdir(root, &["ds", "dsx"])?;
    for entry in &entries {
        let source = fs::read_to_string(entry)
            .map_err(|err| format!("failed to read {}: {err}", entry.display()))?;
        let formatted = deka_fmt::format_ds(&source)?;
        if formatted != source {
            if check {
                changed = true;
                stdio::error("fmt", &format!("{} would be reformatted", entry.display()));
                continue;
            }
            fs::write(entry, formatted)
                .map_err(|err| format!("failed to write {}: {err}", entry.display()))?;
            reformatted += 1;
            stdio::success(&format!("formatted {}", entry.display()));
        }
    }
    // Report what was actually visited: the formatter is mandatory for the
    // corpus, so a directory run must state how many files it covered rather
    // than silently succeeding. Both .ds and .dsx are DekaScript source.
    stdio::success(&format!(
        "visited {} DekaScript file(s) under {} ({} reformatted)",
        entries.len(),
        root.display(),
        reformatted
    ));
    if check && changed {
        std::process::exit(1);
    }
    Ok(())
}

fn walkdir(root: &std::path::Path, exts: &[&str]) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|err| format!("failed to read entry in {}: {err}", dir.display()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| exts.contains(&e))
            {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}
