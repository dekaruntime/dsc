use core::{CommandSpec, Context, Registry};

const COMMAND: CommandSpec = CommandSpec {
    name: "lsp",
    category: "compiler",
    summary: "language server (stdio)",
    aliases: &[],
    subcommands: &[],
    handler: cmd,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(COMMAND);
}

fn cmd(_context: &Context) {
    stdio::error("lsp", "compiler crates not pulled over yet");
    std::process::exit(1);
}
