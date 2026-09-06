use core::{CommandSpec, Context, Registry};

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
}

fn cmd(_context: &Context) {
    stdio::error("transpile", "compiler crates not pulled over yet");
    std::process::exit(1);
}
