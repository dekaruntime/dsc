use core::{CommandSpec, Context, FlagSpec, Registry};

const COMMAND: CommandSpec = CommandSpec {
    name: "lsp",
    owner: "legacy",
    category: "compiler",
    summary: "language server (stdio)",
    aliases: &[],
    subcommands: &[],
    handler: cmd,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(COMMAND);
    registry.add_flag(FlagSpec {
        name: "--stdio",
        aliases: &[],
        description: "run the language server over stdio",
    });
}

fn cmd(_context: &Context) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            stdio::error("lsp", &format!("failed to initialize runtime: {err}"));
            std::process::exit(1);
        }
    };

    if let Err(err) = runtime.block_on(async { dekascript_lsp::run_stdio().await }) {
        stdio::error("lsp", &format!("failed to start language server: {err}"));
        std::process::exit(1);
    }
}
