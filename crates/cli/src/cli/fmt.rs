use core::{CommandSpec, Context, Registry};

const COMMAND: CommandSpec = CommandSpec {
    name: "fmt",
    category: "compiler",
    summary: "format DekaScript",
    aliases: &[],
    subcommands: &[],
    handler: cmd,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(COMMAND);
}

fn cmd(_context: &Context) {
    stdio::error("fmt", "compiler crates not pulled over yet");
    std::process::exit(1);
}
