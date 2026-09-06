use core::{CommandSpec, Context, FlagSpec, Registry};

const COMMAND: CommandSpec = CommandSpec {
    name: "check",
    category: "compiler",
    summary: "typecheck DekaScript",
    aliases: &[],
    subcommands: &[],
    handler: cmd,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(COMMAND);
    registry.add_flag(FlagSpec {
        name: "--as-package",
        aliases: &[],
        description: "typecheck a package the way an installed consumer would resolve it",
    });
    registry.add_flag(FlagSpec {
        name: "--single-file",
        aliases: &[],
        description: "typecheck only the requested file without project imports",
    });
}

fn cmd(_context: &Context) {
    stdio::error("check", "compiler crates not pulled over yet");
    std::process::exit(1);
}
