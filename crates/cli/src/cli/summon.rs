use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use core::{CommandSpec, Context, Registry, SubcommandSpec};

use crate::compile_helper::print_cli_error;

const INFER: SubcommandSpec = SubcommandSpec {
    name: "infer",
    summary: "scaffold a DRAFT summon declaration from a vendored .mjs module",
    aliases: &[],
    handler: infer_cmd,
};

const COMMAND: CommandSpec = CommandSpec {
    name: "summon",
    owner: "legacy",
    category: "compiler",
    summary: "foreign-module declaration tools (rfd#39)",
    aliases: &[],
    subcommands: &[INFER],
    handler: cmd,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(COMMAND);
}

fn cmd(context: &Context) {
    if context.args.flags.get("--help").copied().unwrap_or(false)
        || context.args.flags.get("-H").copied().unwrap_or(false)
        || context.args.flags.get("help").copied().unwrap_or(false)
    {
        stdio::raw(usage());
        return;
    }
    stdio::error("summon", usage());
    std::process::exit(1);
}

fn infer_cmd(context: &Context) {
    if context.args.flags.get("--help").copied().unwrap_or(false)
        || context.args.flags.get("-H").copied().unwrap_or(false)
        || context.args.flags.get("help").copied().unwrap_or(false)
    {
        stdio::raw(usage());
        return;
    }
    if let Err(err) = run_infer(context) {
        print_cli_error("summon", &err);
        std::process::exit(1);
    }
}

fn run_infer(context: &Context) -> Result<(), String> {
    let input = context
        .args
        .positionals
        .first()
        .ok_or_else(|| usage().to_string())?;
    if context.args.positionals.len() != 1 {
        return Err(usage().to_string());
    }
    let input = PathBuf::from(input);
    let input = if input.is_absolute() {
        input
    } else {
        context.env.cwd.join(input)
    };
    if input.extension().and_then(|ext| ext.to_str()) != Some("mjs") {
        return Err(format!(
            "summon infer requires a vendored .mjs module, got {}",
            input.display()
        ));
    }
    let source = fs::read_to_string(&input)
        .map_err(|err| format!("cannot read {}: {err}", input.display()))?;
    let out = context.args.params.get("--out").map(PathBuf::from);
    let spec = specifier_for(&input, out.as_deref(), &context.env.cwd)?;
    let draft = deka_compile::summon::infer_draft(&source, &spec)?;
    if let Some(out) = out {
        if let Some(parent) = out.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
            }
        }
        fs::write(&out, &draft)
            .map_err(|err| format!("failed to write {}: {err}", out.display()))?;
        stdio::success(&format!("wrote {}", out.display()));
        return Ok(());
    }
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(draft.as_bytes())
        .map_err(|err| format!("failed to write stdout: {err}"))?;
    Ok(())
}

fn specifier_for(module: &Path, out: Option<&Path>, cwd: &Path) -> Result<String, String> {
    let from_dir = match out.and_then(Path::parent) {
        Some(parent) if !parent.as_os_str().is_empty() => {
            if parent.is_absolute() {
                parent.to_path_buf()
            } else {
                cwd.join(parent)
            }
        }
        _ => cwd.to_path_buf(),
    };
    let module = if module.is_absolute() {
        module.to_path_buf()
    } else {
        cwd.join(module)
    };
    let from_dir = fs::canonicalize(&from_dir).unwrap_or(from_dir);
    let module = fs::canonicalize(&module).unwrap_or(module);
    let spec = match module.strip_prefix(&from_dir) {
        Ok(rel) => {
            let rel = rel.to_string_lossy().replace('\\', "/");
            format!("./{rel}")
        }
        Err(_) => {
            let name = module
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| format!("module path is not valid UTF-8: {}", module.display()))?;
            format!("./{name}")
        }
    };
    deka_compile::summon::module_spec(&spec)
}

fn usage() -> &'static str {
    "usage: dsc summon infer <module.mjs> [--out <file.d.ds>]\n\n\
     Scaffold a DRAFT summon block from a vendored .mjs module (rfd#39).\n\
     Writes to stdout unless --out names a file. Review the draft before committing:\n\
     `total` is emitted only where visible analysis proves there are no throw sites."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_command_has_infer_subcommand() {
        assert_eq!(COMMAND.name, "summon");
        assert_eq!(COMMAND.subcommands.len(), 1);
        assert_eq!(COMMAND.subcommands[0].name, "infer");
    }
}
