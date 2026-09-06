use std::fs;
use std::path::{Path, PathBuf};

use core::Context;

use crate::cli::transpile::{build_module, transpile_entry_file, write_generated_js};
use crate::compile_helper::{is_deka_source_path, print_cli_error, project_root_from_cwd};

const PROJECT_TREES: [&str; 3] = ["app", "api", "src"];
const SKIP_DIRS: &[&str] = &[
    "ds_modules",
    "php_modules",
    ".git",
    "target",
    "dist",
    "node_modules",
    ".deka",
];

pub fn cmd(context: &Context) {
    if let Err(err) = run(context) {
        print_cli_error("emit", &err);
        std::process::exit(1);
    }
}

fn run(context: &Context) -> Result<(), String> {
    let cwd = &context.env.cwd;
    if context.args.positionals.len() > 1 {
        return Err("expected at most one file argument".to_string());
    }
    if let Some(file) = context.args.positionals.first() {
        return emit_file_arg(cwd, file);
    }
    emit_project(context)
}

fn emit_file_arg(cwd: &Path, file: &str) -> Result<(), String> {
    let input = PathBuf::from(file);
    let input = if input.is_absolute() {
        input
    } else {
        cwd.join(input)
    };
    if input.is_dir() {
        return Err(format!(
            "{} is a directory; run dsc with no arguments to emit app/, api/, and src/, or use dsc transpile {}",
            input.display(),
            input.display()
        ));
    }
    transpile_entry_file(&input, None, cwd)
}

fn emit_project(context: &Context) -> Result<(), String> {
    let cwd = &context.env.cwd;
    let project_root = project_root_from_cwd(cwd);
    let trees: Vec<&str> = PROJECT_TREES
        .into_iter()
        .filter(|name| project_root.join(name).is_dir())
        .collect();
    if trees.is_empty() {
        return Err(format!(
            "looked for app/, api/, src/ under {}; set a file arg or create one of those folders",
            project_root.display()
        ));
    }

    let outdir = resolve_outdir(context, &project_root);
    for name in &trees {
        let input = project_root.join(name);
        let output = outdir.join(name);
        if *name == "src" {
            emit_src_tree(&input, &output, cwd)?;
        } else {
            emit_ds_tree(&input, &output, cwd)?;
        }
    }

    stdio::success(&format!(
        "emitted {} -> {}",
        trees
            .iter()
            .map(|name| format!("{name}/"))
            .collect::<Vec<_>>()
            .join(" "),
        outdir.display()
    ));
    Ok(())
}

fn resolve_outdir(context: &Context, project_root: &Path) -> PathBuf {
    let raw = context
        .args
        .params
        .get("--outdir")
        .or_else(|| context.args.params.get("-o"));
    match raw {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                project_root.join(path)
            }
        }
        None => project_root.join("dist"),
    }
}

fn emit_ds_tree(input: &Path, output_root: &Path, cwd: &Path) -> Result<(), String> {
    for source in collect_files(input)? {
        if !is_deka_source_path(&source) {
            continue;
        }
        emit_ds_file(&source, input, output_root, cwd)?;
    }
    Ok(())
}

fn emit_src_tree(input: &Path, output_root: &Path, cwd: &Path) -> Result<(), String> {
    let files = collect_files(input)?;
    for source in &files {
        if is_deka_source_path(source) {
            continue;
        }
        let rel = source
            .strip_prefix(input)
            .map_err(|_| "failed to preserve source tree".to_string())?;
        copy_verbatim(source, &output_root.join(rel))?;
    }
    for source in &files {
        if !is_deka_source_path(source) {
            continue;
        }
        emit_ds_file(source, input, output_root, cwd)?;
    }
    Ok(())
}

fn emit_ds_file(
    source: &Path,
    input_root: &Path,
    output_root: &Path,
    cwd: &Path,
) -> Result<(), String> {
    let rel = source
        .strip_prefix(input_root)
        .map_err(|_| "failed to preserve source tree".to_string())?;
    let output = output_root.join(rel).with_extension("js");
    let js = build_module(source, cwd, false, false, false)?;
    write_generated_js(&output, &js)
}

fn copy_verbatim(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    fs::copy(from, to).map_err(|err| {
        format!(
            "failed to copy {} -> {}: {err}",
            from.display(),
            to.display()
        )
    })?;
    Ok(())
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    collect_files_inner(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_files_inner(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let mut entries = fs::read_dir(dir)
        .map_err(|err| format!("failed to read {}: {err}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if SKIP_DIRS.iter().any(|skip| *skip == name.as_ref()) {
                continue;
            }
            collect_files_inner(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}
