use std::io::Write;
use std::path::PathBuf;

use core::{CommandSpec, Context, Registry};

use crate::compile_helper::{compile_dev_plan, is_deka_source_path, print_cli_error};

const COMMAND: CommandSpec = CommandSpec {
    name: "plan",
    category: "compiler",
    summary: "print build-only dev materialization plan",
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
        print_cli_error("plan", &err);
        std::process::exit(1);
    }
}

fn run(context: &Context) -> Result<(), String> {
    let [input] = context.args.positionals.as_slice() else {
        return Err(usage().to_string());
    };
    let input = PathBuf::from(input);
    if !is_deka_source_path(&input) {
        return Err(format!(
            "DekaScript uses .ds or .dsx; got '{}'",
            input.display()
        ));
    }
    let input = if input.is_absolute() {
        input
    } else {
        context.env.cwd.join(input)
    };
    let plan = compile_dev_plan(&input, &context.env.cwd)?;
    let output = serde_json::to_string_pretty(&plan)
        .map_err(|err| format!("failed to serialize dev plan: {err}"))?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{output}").map_err(|err| format!("failed to write dev plan: {err}"))?;
    Ok(())
}

fn usage() -> &'static str {
    "usage: dsc plan <file.ds>\n\nPrints the compiler-owned build-only materialization plan. Dsc does not execute plan entries."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_command_has_stable_contract() {
        assert_eq!(COMMAND.name, "plan");
        assert_eq!(COMMAND.category, "compiler");
    }
}
