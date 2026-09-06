use core::{CommandSpec, Context, FlagSpec, Registry};
use std::fs;
use std::path::{Path, PathBuf};

use crate::compile_helper::compile_or_report;

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

fn cmd(context: &Context) {
    if let Err(err) = run(context) {
        stdio::error("check", &err);
        std::process::exit(1);
    }
}

fn run(context: &Context) -> Result<(), String> {
    if context
        .args
        .flags
        .get("--as-package")
        .copied()
        .unwrap_or(false)
    {
        return Err(
            "`dsc check --as-package` needs the package linker; not pulled over yet".to_string(),
        );
    }

    let input = context
        .args
        .positionals
        .first()
        .ok_or_else(|| "usage: dsc check <file.ds>".to_string())?;
    let path = Path::new(input);
    if !is_deka_source_path(path) {
        return Err(format!(
            "DekaScript uses .ds or .dsx; got '{}'",
            input
        ));
    }

    let source = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {}", path.display(), err))?;
    let single_file = context
        .args
        .flags
        .get("--single-file")
        .copied()
        .unwrap_or(false);
    let report = if single_file {
        compile_or_report(&source, input)?
    } else if let Some(project_root) = find_project_root(&context.env.cwd, path) {
        check_project_file(path, &project_root, &context.env.cwd)?;
        crate::compile_helper::CompileReport {
            js: String::new(),
            warnings: Vec::new(),
        }
    } else {
        compile_or_report(&source, input)?
    };

    for warning in &report.warnings {
        eprintln!("{}", warning);
    }

    stdio::success(&format!("checked {}", path.display()));
    Ok(())
}

fn find_project_root(cwd: &Path, input: &Path) -> Option<PathBuf> {
    let absolute_input = if input.is_absolute() {
        input.to_path_buf()
    } else {
        cwd.join(input)
    };
    let start = if absolute_input.is_dir() {
        absolute_input
    } else {
        absolute_input.parent()?.to_path_buf()
    };

    for dir in start.ancestors() {
        if dir.join("deka.json").is_file() || dir.join("deka.lock").is_file() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

fn check_project_file(path: &Path, project_root: &Path, cwd: &Path) -> Result<(), String> {
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let loader = deka_compile::module_graph::FsModuleLoader::new(project_root.to_path_buf());
    deka_compile::module_graph::compile_module_graph(&absolute_path, &loader)
        .map_err(|diagnostics| deka_compile::format_diagnostics(&diagnostics))?;
    Ok(())
}

fn is_deka_source_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("ds" | "dsx")
    )
}
