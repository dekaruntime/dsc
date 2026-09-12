use core::{CommandSpec, Context, FlagSpec, Registry};
use std::fs;
use std::path::Path;

use crate::compile_helper::{compile_or_report, find_project_root, is_deka_source_path};
use deka_project::modules::{LinkEntry, LinkManifest};

const COMMAND: CommandSpec = CommandSpec {
    name: "check",
    owner: "legacy",
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
    if context.args.flags.get("--help").copied().unwrap_or(false)
        || context.args.flags.get("-H").copied().unwrap_or(false)
        || context.args.flags.get("help").copied().unwrap_or(false)
    {
        stdio::raw(usage());
        return;
    }
    if let Err(err) = run(context) {
        stdio::error("check", &err);
        std::process::exit(1);
    }
}

fn usage() -> &'static str {
    "usage: dsc check <file.ds>\n       dsc check --as-package <package-directory>\n       dsc check --single-file <file.ds>"
}

fn run(context: &Context) -> Result<(), String> {
    if context
        .args
        .flags
        .get("--as-package")
        .copied()
        .unwrap_or(false)
    {
        let dir = context
            .args
            .positionals
            .first()
            .ok_or_else(|| "usage: dsc check --as-package <package-directory>".to_string())?;
        return check_as_package(Path::new(dir));
    }

    let input = context
        .args
        .positionals
        .first()
        .ok_or_else(|| "usage: dsc check <file.ds>".to_string())?;
    let path = Path::new(input);
    if !is_deka_source_path(path) {
        return Err(format!("DekaScript uses .ds or .dsx; got '{input}'"));
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

/// Typecheck a package the way a consumer would: scratch project + local link
/// + module graph. Registry install stays on the host (`deka`).
fn check_as_package(package_dir: &Path) -> Result<(), String> {
    let package_dir = fs::canonicalize(package_dir).map_err(|err| {
        format!(
            "package directory does not exist: {} ({err})",
            package_dir.display()
        )
    })?;

    let manifest_raw = fs::read_to_string(package_dir.join("deka.json"))
        .map_err(|err| format!("cannot read package deka.json: {err}"))?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_raw)
        .map_err(|err| format!("package deka.json is invalid: {err}"))?;
    let name = manifest
        .get("name")
        .and_then(|value| value.as_str())
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "package deka.json must contain a non-empty `name`".to_string())?;
    let version = manifest
        .get("version")
        .and_then(|value| value.as_str())
        .unwrap_or("0.0.0");
    let main = manifest
        .get("main")
        .and_then(|value| value.as_str())
        .unwrap_or("index.ds");

    if manifest
        .get("dependencies")
        .and_then(|deps| deps.as_object())
        .is_some_and(|deps| !deps.is_empty())
    {
        return Err(
            "`dsc check --as-package` does not install registry packages; the host (`deka`) owns that. Check packages with dependencies from a consumer project after `deka install`."
                .to_string(),
        );
    }

    let entry_path = package_dir.join(main);
    let entry_source = fs::read_to_string(&entry_path)
        .map_err(|err| format!("cannot read package entry {}: {err}", entry_path.display()))?;
    let surface = collect_export_surface(&entry_source, main)?;
    if surface.is_empty() {
        return Err(format!(
            "package entry {main} exports nothing; there is no consumer surface to verify"
        ));
    }

    let scratch = tempfile::tempdir()
        .map_err(|err| format!("failed to create scratch consumer project: {err}"))?;
    let project = scratch.path();

    let consumer_manifest = serde_json::json!({
        "name": "dsc-check-as-package",
        "version": "0.0.0",
        "main": "main.ds",
        "dependencies": { name: version },
    });
    fs::write(
        project.join("deka.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&consumer_manifest).unwrap()
        ),
    )
    .map_err(|err| format!("failed to write scratch deka.json: {err}"))?;
    fs::write(
        project.join("deka.lock"),
        "{\n  \"version\": 1,\n  \"packages\": {}\n}\n",
    )
    .map_err(|err| format!("failed to write scratch deka.lock: {err}"))?;

    let mut consumer_main = format!(
        "import {{ {} }} from \"{name}\"\n",
        surface.all_names().join(", ")
    );
    for (index, value_name) in surface.value_names.iter().enumerate() {
        consumer_main.push_str(&format!("const __check_value_{index} = {value_name}\n"));
    }
    for (index, type_name) in surface.type_names.iter().enumerate() {
        consumer_main.push_str(&format!(
            "fn __check_type_{index}(x: {type_name}) {type_name} {{\n  return x\n}}\n"
        ));
    }
    fs::write(project.join("main.ds"), consumer_main)
        .map_err(|err| format!("failed to write scratch main.ds: {err}"))?;

    let mut links = LinkManifest::default();
    links.packages.insert(
        name.to_string(),
        LinkEntry {
            path: package_dir.clone(),
        },
    );
    deka_project::modules::write_links_at(project, &links)?;

    let entry = project.join("main.ds");
    let loader = deka_compile::module_graph::FsModuleLoader::new(project.to_path_buf());
    match deka_compile::module_graph::compile_module_graph(&entry, &loader) {
        Ok(_) => {
            stdio::success(&format!("checked {name} as an installed consumer"));
            Ok(())
        }
        Err(diagnostics) => {
            eprintln!("{}", deka_compile::format_diagnostics(&diagnostics));
            Err(format!(
                "{name} does not typecheck as an installed consumer ({} diagnostic(s))",
                diagnostics.len()
            ))
        }
    }
}

struct ExportSurface {
    value_names: Vec<String>,
    type_names: Vec<String>,
}

impl ExportSurface {
    fn is_empty(&self) -> bool {
        self.value_names.is_empty() && self.type_names.is_empty()
    }

    fn all_names(&self) -> Vec<String> {
        self.value_names
            .iter()
            .chain(self.type_names.iter())
            .cloned()
            .collect()
    }
}

fn collect_export_surface(source: &str, entry_name: &str) -> Result<ExportSurface, String> {
    let arena = bumpalo::Bump::new();
    let parsed = deka_syntax::parse(source, &arena);
    if let Some(error) = parsed
        .errors
        .iter()
        .find(|diagnostic| matches!(diagnostic.severity, deka_syntax::Severity::Error))
    {
        return Err(format!(
            "package entry {entry_name} does not parse: {}:{}: {}",
            error.line, error.column, error.message
        ));
    }
    let program = parsed
        .program
        .ok_or_else(|| format!("package entry {entry_name} does not parse"))?;

    let exports = deka_syntax::collect_module_exports(&program, &arena);
    let value_names = exports
        .values
        .keys()
        .map(|name| (*name).to_string())
        .collect();
    let type_names = exports
        .structs
        .keys()
        .chain(exports.enums.keys())
        .chain(exports.aliases.keys())
        .chain(exports.newtypes.keys())
        .map(|name| (*name).to_string())
        .collect();
    Ok(ExportSurface {
        value_names,
        type_names,
    })
}
