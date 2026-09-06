use core::{CommandSpec, Context, ParamSpec, Registry};
use std::fs;
use std::path::{Path, PathBuf};

use crate::compile_helper::compile_js_or_report;

const COMMAND: CommandSpec = CommandSpec {
    name: "transpile",
    category: "compiler",
    summary: "emit JavaScript",
    aliases: &["emit"],
    subcommands: &[],
    handler: cmd,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(COMMAND);
    registry.add_param(ParamSpec {
        name: "--out",
        description: "output .js file",
    });
}

fn cmd(context: &Context) {
    if let Err(err) = run(context) {
        stdio::error("transpile", &err);
        std::process::exit(1);
    }
}

fn run(context: &Context) -> Result<(), String> {
    let input = context
        .args
        .positionals
        .first()
        .ok_or_else(|| "usage: dsc transpile <file.ds> [--out file.js]".to_string())?;
    let path = Path::new(input);
    if !matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("ds" | "dsx")
    ) {
        return Err(format!("DekaScript uses .ds or .dsx; got '{input}'"));
    }
    let source = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let js = compile_js_or_report(&source, input)?;
    if let Some(out) = context.args.params.get("--out") {
        let out = PathBuf::from(out);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        fs::write(&out, js).map_err(|err| format!("failed to write {}: {err}", out.display()))?;
        stdio::success(&format!("wrote {}", out.display()));
    } else {
        print!("{js}");
    }
    Ok(())
}
