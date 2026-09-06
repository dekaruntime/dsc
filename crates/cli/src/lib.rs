use core::Registry;

pub mod cli;
pub mod compile_helper;

pub fn build_registry() -> Registry {
    let mut registry = Registry::new();
    cli::register_global_flags(&mut registry);
    cli::register_global_params(&mut registry);
    cli::check::register(&mut registry);
    cli::fmt::register(&mut registry);
    cli::transpile::register(&mut registry);
    cli::lsp::register(&mut registry);
    registry
}

pub fn run() {
    let registry = build_registry();
    cli::execute(&registry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_compiler_commands() {
        let registry = build_registry();
        for name in ["check", "fmt", "transpile", "lsp"] {
            assert!(
                registry.command_named(name).is_some(),
                "missing command {name}"
            );
        }
    }
}
