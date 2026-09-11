use std::path::PathBuf;

use core::{CommandSpec, Context, Registry};

use crate::cli::transpile;
use crate::compile_helper::{is_deka_source_path, print_cli_error};

const COMMAND: CommandSpec = CommandSpec {
    name: "bundle",
    category: "compiler",
    summary: "compile and bundle a module graph into a single JavaScript file",
    aliases: &[],
    subcommands: &[],
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

    if let Err(err) = run(context) {
        print_cli_error("bundle", &err);
        std::process::exit(1);
    }
}

fn run(context: &Context) -> Result<(), String> {
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

    let output = context
        .args
        .params
        .get("--out")
        .map(PathBuf::from)
        .ok_or_else(|| {
            "bundle requires --out <file.js> so the bundle target is unambiguous".to_string()
        })?;
    if output.is_dir() || output.extension().and_then(|ext| ext.to_str()) != Some("js") {
        return Err("bundle --out must name a .js file".to_string());
    }
    let treeshake = context
        .args
        .flags
        .get("--treeshake")
        .copied()
        .unwrap_or(false);
    let client = context.args.flags.get("--client").copied().unwrap_or(false);

    let entry = match (input.is_file(), input.is_dir()) {
        (true, _) => {
            if !is_deka_source_path(&input) {
                return Err(format!(
                    "DekaScript uses .ds or .dsx; cannot transpile {}",
                    input.display()
                ));
            }
            input.clone()
        }
        (_, true) => {
            let sources = transpile::collect_ds_sources(&input)?;
            if sources.is_empty() {
                return Err(format!("no .ds files found under {}", input.display()));
            }
            transpile::directory_entry(&input, &sources)?
        }
        _ => return Err(format!("input path does not exist: {}", input.display())),
    };

    let js = transpile::build_bundle(&entry, &context.env.cwd, treeshake, client)?;
    transpile::write_generated_js(&output, &js)?;
    stdio::success(&format!(
        "bundled {} -> {}",
        input.display(),
        output.display()
    ));
    Ok(())
}

fn usage() -> &'static str {
    "usage: dsc bundle <file-or-directory> --out <file.js> [--treeshake] [--client]\n\nCompiles the input, bundles the resolved module graph into a single\nJavaScript file, and optimizes it.\n  --out <file.js>: required output file\n  --treeshake: minify the bundled JavaScript\n  --client: fail the build if the graph can reach ui/server\n\nExamples:\n  dsc bundle app/main.ds --out dist/app.js\n  dsc bundle app --out dist/app.js --treeshake"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_command_has_stable_contract() {
        assert_eq!(COMMAND.name, "bundle");
        assert_eq!(COMMAND.category, "compiler");
    }
}
