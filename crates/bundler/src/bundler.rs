use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use deka_project::module_spec::{ds_source_candidates, module_spec_aliases};
use deka_project::modules::{existing_modules_dirs, read_linked_modules, MODULES_DIR};
use swc_bundler::{BundleKind, Bundler, Config, Hook, Load, ModuleData, ModuleType};
use swc_common::{
    FileName, GLOBALS, Globals, Mark, SourceMap, sync::Lrc,
};
use swc_ecma_ast::{EsVersion, KeyValueProp, Module, Pass, Program};
use swc_ecma_codegen::{Emitter, text_writer::JsWriter};
use swc_ecma_loader::resolve::{Resolution, Resolve};
use swc_ecma_minifier::optimize;
use swc_ecma_minifier::option::{CompressOptions, MangleOptions, MinifyOptions};
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_ecma_transforms_base::helpers::Helpers;
use swc_ecma_transforms_base::resolver;

use swc_ecma_transforms_typescript::strip;

pub use crate::cached::bundle_browser_assets_cached;
use crate::css_bundler::{self, CssAsset};
use crate::optimizer;

const CLIENT_SERVER_IMPORT_ERROR: &str = "client bundle cannot import ui/server";

pub struct JsBundle {
    pub code: String,
    pub css: Option<String>,
    pub assets: Vec<CssAsset>,
}

pub struct BundleOptions {
    pub project_root: PathBuf,
    pub minify: bool,
    pub iife: bool,
    /// When true, a reachable `ui/server` import is a build failure.
    pub client: bool,
    /// Shared runtime prelude synthesized once for the whole program
    /// (deka#595). Module bodies reference its helpers as free identifiers;
    /// the single-file bundle is the one place those resolve. For ES-module
    /// output the prelude is prepended to the bundle text; for IIFE output
    /// it is injected at the head of the wrapper body so the output still
    /// starts with `(async function` (the pool's pre-bundled IIFE
    /// detection depends on it). Minified bundles get the prelude minified
    /// with the same restricted configuration.
    pub prelude: Option<String>,
}

pub type BuildOptions = BundleOptions;


pub trait VirtualSource: Send + Sync {
    fn load_virtual(&self, path: &Path) -> Result<Option<String>, String>;
}

pub fn bundle_virtual_entry(
    entry_path: &Path,
    options: BundleOptions,
    provider: Arc<dyn VirtualSource>,
) -> Result<String, String> {
    let cm: Lrc<SourceMap> = Default::default();
    let globals = Globals::new();
    let loader = VirtualLoader {
        cm: cm.clone(),
        css_collector: Arc::new(Mutex::new(CssCollector::default())),
        provider,
    };
    let resolver = DekaResolver::new(options.project_root.clone(), options.client)?;

    let mut bundler = Bundler::new(
        &globals,
        cm.clone(),
        loader,
        resolver,
        Config {
            require: false,
            disable_inliner: false,
            disable_hygiene: false,
            disable_fixer: false,
            disable_dce: false,
            external_modules: Vec::new(),
            module: if options.iife {
                ModuleType::Iife
            } else {
                ModuleType::Es
            },
        },
        Box::new(NoopHook),
    );

    let mut entries = HashMap::new();
    entries.insert(
        "entry".to_string(),
        FileName::Real(entry_path.to_path_buf()),
    );

    let bundles = GLOBALS
        .set(&globals, || bundler.bundle(entries))
        .map_err(|err| format!("{err:?}"))?;
    let bundle = bundles
        .into_iter()
        .find(|bundle| matches!(bundle.kind, BundleKind::Named { .. }))
        .ok_or_else(|| "Failed to find bundled output".to_string())?;

    let module = if options.minify {
        GLOBALS.set(&globals, || {
            optimizer::minify_module(bundle.module, cm.clone(), false, Mark::new(), Mark::new())
        })
    } else {
        bundle.module
    };

    let mut buf = Vec::new();
    {
        let mut emitter = Emitter {
            cfg: swc_ecma_codegen::Config::default(),
            comments: None,
            cm: cm.clone(),
            wr: JsWriter::new(cm.clone(), "\n", &mut buf, None),
        };
        emitter
            .emit_module(&module)
            .map_err(|err| err.to_string())?;
    }

    let out = String::from_utf8(buf).map_err(|err| err.to_string())?;
    attach_prelude(out, &options)
}

/// Place the program prelude in the final bundle (deka#595): prepended for
/// ES-module output, injected at the head of the IIFE wrapper body for IIFE
/// output so the `(async function` prefix the pool sniffs for stays intact.
fn attach_prelude(out: String, options: &BundleOptions) -> Result<String, String> {
    let Some(prelude) = &options.prelude else {
        return Ok(out);
    };
    let prelude = if options.minify {
        let minified = optimizer::minify_source_text("__deka_prelude__.js", prelude)?;
        format!("{minified}\n")
    } else {
        prelude.clone()
    };
    if !options.iife {
        return Ok(format!("{prelude}{out}"));
    }
    // IIFE bundles must keep starting with `(async function` (the isolate
    // pool detects pre-bundled handlers by that prefix), so the prelude goes
    // INSIDE the wrapper: immediately after its opening brace, which is the
    // first `{` in the output.
    let Some(brace) = out.find('{') else {
        return Err("IIFE bundle has no wrapper body to inject the prelude into".to_string());
    };
    if !out[..brace].contains("function") {
        return Err(format!(
            "IIFE bundle wrapper prefix looks unexpected: {:?}",
            &out[..brace.min(out.len())]
        ));
    }
    // Keep a leading `"use strict";` directive prologue first in the wrapper
    // body: the prelude must not degrade it to a dead string expression.
    let mut insert_at = brace + 1;
    let after_brace = &out[insert_at..];
    let leading_ws = after_brace
        .find(|c: char| !c.is_whitespace())
        .unwrap_or(after_brace.len());
    insert_at += leading_ws;
    const USE_STRICT: &str = "\"use strict\";";
    if out[insert_at..].starts_with(USE_STRICT) {
        insert_at += USE_STRICT.len();
    }
    let mut injected = String::with_capacity(out.len() + prelude.len());
    injected.push_str(&out[..insert_at]);
    if !injected.ends_with('\n') {
        injected.push('\n');
    }
    injected.push_str(&prelude);
    injected.push_str(&out[insert_at..]);
    Ok(injected)
}

pub fn bundle_browser(entry: &str) -> Result<String, String> {
    let entry_path = resolve_entry(entry)?;
    let cm: Lrc<SourceMap> = Default::default();
    let globals = Globals::new();
    let root = std::env::current_dir().map_err(|err| err.to_string())?;
    let loader = FsLoader {
        cm: cm.clone(),
        css_collector: Arc::new(Mutex::new(CssCollector::default())),
        root: root.clone(),
    };
    let resolver = FsResolver::new()?;

    let mut bundler = Bundler::new(
        &globals,
        cm.clone(),
        loader,
        resolver,
        Config {
            require: false,
            disable_inliner: false,
            disable_hygiene: false,
            disable_fixer: false,
            disable_dce: false,
            external_modules: Vec::new(),
            module: ModuleType::Es,
        },
        Box::new(NoopHook),
    );

    let mut entries = HashMap::new();
    entries.insert("entry".to_string(), FileName::Real(entry_path));

    let bundles = GLOBALS
        .set(&globals, || bundler.bundle(entries))
        .map_err(|err| err.to_string())?;
    let bundle = bundles
        .into_iter()
        .find(|bundle| matches!(bundle.kind, BundleKind::Named { .. }))
        .ok_or_else(|| "Failed to find bundled output".to_string())?;

    let mut buf = Vec::new();
    {
        let mut emitter = Emitter {
            cfg: swc_ecma_codegen::Config::default(),
            comments: None,
            cm: cm.clone(),
            wr: JsWriter::new(cm.clone(), "\n", &mut buf, None),
        };
        emitter
            .emit_module(&bundle.module)
            .map_err(|err| err.to_string())?;
    }

    let code = String::from_utf8(buf).map_err(|err| err.to_string())?;
    Ok(code)
}

pub fn bundle_browser_assets(entry: &str) -> Result<JsBundle, String> {
    let entry_path = resolve_entry(entry)?;
    let cm: Lrc<SourceMap> = Default::default();
    let globals = Globals::new();
    let css_collector = Arc::new(Mutex::new(CssCollector::default()));
    let root = std::env::current_dir().map_err(|err| err.to_string())?;
    let loader = FsLoader {
        cm: cm.clone(),
        css_collector: Arc::clone(&css_collector),
        root: root.clone(),
    };
    let resolver = FsResolver::new()?;

    let mut bundler = Bundler::new(
        &globals,
        cm.clone(),
        loader,
        resolver,
        Config {
            require: false,
            disable_inliner: false,
            disable_hygiene: false,
            disable_fixer: false,
            disable_dce: false,
            external_modules: Vec::new(),
            module: ModuleType::Es,
        },
        Box::new(NoopHook),
    );

    let mut entries = HashMap::new();
    entries.insert("entry".to_string(), FileName::Real(entry_path));

    let bundles = GLOBALS
        .set(&globals, || bundler.bundle(entries))
        .map_err(|err| err.to_string())?;
    let bundle = bundles
        .into_iter()
        .find(|bundle| matches!(bundle.kind, BundleKind::Named { .. }))
        .ok_or_else(|| "Failed to find bundled output".to_string())?;

    let mut buf = Vec::new();
    {
        let mut emitter = Emitter {
            cfg: swc_ecma_codegen::Config::default(),
            comments: None,
            cm: cm.clone(),
            wr: JsWriter::new(cm.clone(), "\n", &mut buf, None),
        };
        emitter
            .emit_module(&bundle.module)
            .map_err(|err| err.to_string())?;
    }

    let code = String::from_utf8(buf).map_err(|err| err.to_string())?;
    let collector = css_collector
        .lock()
        .map_err(|_| "CSS collector lock failed".to_string())?;
    let css = collector
        .entries
        .iter()
        .map(|entry| entry.code.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let assets = collector
        .entries
        .iter()
        .flat_map(|entry| entry.assets.clone())
        .collect::<Vec<_>>();

    Ok(JsBundle {
        code,
        css: if css.is_empty() { None } else { Some(css) },
        assets,
    })
}

pub(crate) fn resolve_entry(entry: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(entry);
    if path.is_absolute() {
        return Ok(path);
    }
    let cwd = std::env::current_dir().map_err(|err| err.to_string())?;
    Ok(cwd.join(path))
}

#[derive(Default)]
struct CssCollector {
    seen: HashSet<PathBuf>,
    entries: Vec<CssEntry>,
}

struct CssEntry {
    code: String,
    assets: Vec<CssAsset>,
}

struct FsLoader {
    cm: Lrc<SourceMap>,
    css_collector: Arc<Mutex<CssCollector>>,
    /// Project root — second containment line after DekaResolver.
    /// Even if a future caller bypasses DekaResolver, the loader will not
    /// read files outside this directory.
    root: PathBuf,
}

impl Load for FsLoader {
    fn load(&self, file: &FileName) -> Result<ModuleData, anyhow::Error> {
        let (source, path) = match file {
            FileName::Real(path) => {
                // Defense-in-depth: verify the path stays within the project
                // root before touching the filesystem, even though DekaResolver
                // already enforces this for normal resolution paths.
                if guard_path_traversal(path, &self.root).is_none() {
                    anyhow::bail!(
                        "Path traversal blocked by FsLoader: {} is outside project root",
                        path.display()
                    );
                }
                if path.extension().and_then(|ext| ext.to_str()) == Some("css") {
                    let (source, module_path) = self.load_css_module(path)?;
                    (source, module_path)
                } else {
                    let source = std::fs::read_to_string(path).map_err(|err| {
                        anyhow::Error::msg(format!("Failed to read {}: {}", path.display(), err))
                    })?;
                    (source, Some(path.clone()))
                }
            }
            FileName::Custom(name) => match load_ui_custom(name) {
                Some(source) => (source.to_string(), None),
                None => anyhow::bail!("Unsupported file name: {file:?}"),
            },
            other => anyhow::bail!("Unsupported file name: {other:?}"),
        };

        let file_name = match file {
            FileName::Real(path) => FileName::Real(path.clone()),
            FileName::Custom(name) => FileName::Custom(name.clone()),
            other => other.clone(),
        };
        let fm = self.cm.new_source_file(file_name.into(), source);
        let syntax = match path {
            Some(ref path) => syntax_for_path(path),
            None => Syntax::Es(EsSyntax {
                jsx: false,
                export_default_from: true,
                import_attributes: true,
                ..Default::default()
            }),
        };
        let lexer = Lexer::new(syntax, EsVersion::Es2022, StringInput::from(&*fm), None);
        let mut parser = Parser::new_from(lexer);
        let module = parser
            .parse_module()
            .map_err(|err| anyhow::Error::msg(format!("{:?}", err)))?;

        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        let mut program = Program::Module(module);
        let mut pass = resolver(unresolved_mark, top_level_mark, false);
        pass.process(&mut program);

        if path
            .as_ref()
            .map_or(false, |path| is_typescript(path.as_path()))
        {
            let mut pass = strip(unresolved_mark, top_level_mark);
            pass.process(&mut program);
        }

        let module = match program {
            Program::Module(module) => module,
            Program::Script(_) => anyhow::bail!("Unexpected script output when bundling module"),
        };

        Ok(ModuleData {
            fm,
            module,
            helpers: Helpers::new(false),
        })
    }
}

impl FsLoader {
    fn load_css_module(&self, path: &PathBuf) -> Result<(String, Option<PathBuf>), anyhow::Error> {
        let mut collector = self
            .css_collector
            .lock()
            .map_err(|_| anyhow::Error::msg("CSS collector lock failed"))?;
        let already_seen = collector.seen.contains(path);
        if !already_seen {
            collector.seen.insert(path.clone());
            let css_modules = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.ends_with(".module.css"))
                .unwrap_or(false);
            let bundle = css_bundler::bundle_css(&path.display().to_string(), css_modules, true)
                .map_err(|err| anyhow::Error::msg(err))?;
            collector.entries.push(CssEntry {
                code: bundle.code,
                assets: bundle.assets,
            });

            let module_source = if css_modules {
                let exports = bundle.exports.unwrap_or_default();
                let serialized = serde_json::to_string(&exports)
                    .map_err(|err| anyhow::Error::msg(err.to_string()))?;
                let mut lines = Vec::new();
                lines.push(format!("const styles = {};", serialized));
                lines.push("export default styles;".to_string());
                for key in exports.keys() {
                    append_named_exports(&mut lines, key)?;
                }
                lines.join("\n")
            } else {
                "export default {};".to_string()
            };

            return Ok((module_source, None));
        }

        let css_modules = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.ends_with(".module.css"))
            .unwrap_or(false);
        if css_modules {
            // Rebuild exports for repeated imports to keep module contents stable.
            let bundle = css_bundler::bundle_css(&path.display().to_string(), true, true)
                .map_err(|err| anyhow::Error::msg(err))?;
            let exports = bundle.exports.unwrap_or_default();
            let serialized = serde_json::to_string(&exports)
                .map_err(|err| anyhow::Error::msg(err.to_string()))?;
            let mut lines = Vec::new();
            lines.push(format!("const styles = {};", serialized));
            lines.push("export default styles;".to_string());
            for key in exports.keys() {
                append_named_exports(&mut lines, key)?;
            }
            return Ok((lines.join("\n"), None));
        }

        Ok(("export default {};".to_string(), None))
    }
}

struct VirtualLoader {
    cm: Lrc<SourceMap>,
    css_collector: Arc<Mutex<CssCollector>>,
    provider: Arc<dyn VirtualSource>,
}

impl Load for VirtualLoader {
    fn load(&self, file: &FileName) -> Result<ModuleData, anyhow::Error> {
        let (source, path) = match file {
            FileName::Real(path) => {
                if let Some(source) = self
                    .provider
                    .load_virtual(path)
                    .map_err(|err| anyhow::Error::msg(err))?
                {
                    (source, Some(path.clone()))
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("css") {
                    let (source, module_path) = self.load_css_module(path)?;
                    (source, module_path)
                } else {
                    let source = std::fs::read_to_string(path).map_err(|err| {
                        anyhow::Error::msg(format!("Failed to read {}: {}", path.display(), err))
                    })?;
                    (source, Some(path.clone()))
                }
            }
            FileName::Custom(name) => match load_ui_custom(name) {
                Some(source) => (source.to_string(), None),
                None => anyhow::bail!("Unsupported file name: {file:?}"),
            },
            other => anyhow::bail!("Unsupported file name: {other:?}"),
        };

        let file_name = match file {
            FileName::Real(path) => FileName::Real(path.clone()),
            FileName::Custom(name) => FileName::Custom(name.clone()),
            other => other.clone(),
        };
        let fm = self.cm.new_source_file(file_name.into(), source);
        let syntax = match path {
            Some(ref path) => syntax_for_path(path),
            None => Syntax::Es(EsSyntax {
                jsx: false,
                export_default_from: true,
                import_attributes: true,
                ..Default::default()
            }),
        };

        let lexer = Lexer::new(syntax, EsVersion::Es2022, StringInput::from(&*fm), None);
        let mut parser = Parser::new_from(lexer);
        let module = parser
            .parse_module()
            .map_err(|err| anyhow::Error::msg(format!("{:?}", err)))?;

        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        let mut program = Program::Module(module);
        let mut pass = resolver(unresolved_mark, top_level_mark, false);
        pass.process(&mut program);

        if path
            .as_ref()
            .map_or(false, |path| is_typescript(path.as_path()))
        {
            let mut pass = strip(unresolved_mark, top_level_mark);
            pass.process(&mut program);
        }

        let module = match program {
            Program::Module(module) => module,
            Program::Script(_) => anyhow::bail!("Unexpected script output when bundling module"),
        };

        Ok(ModuleData {
            fm,
            module,
            helpers: Helpers::new(false),
        })
    }
}

impl VirtualLoader {
    fn load_css_module(&self, path: &PathBuf) -> Result<(String, Option<PathBuf>), anyhow::Error> {
        let mut collector = self
            .css_collector
            .lock()
            .map_err(|_| anyhow::Error::msg("CSS collector lock failed"))?;
        let already_seen = collector.seen.contains(path);
        if !already_seen {
            collector.seen.insert(path.clone());
            let css_modules = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.ends_with(".module.css"))
                .unwrap_or(false);
            let bundle = css_bundler::bundle_css(&path.display().to_string(), css_modules, true)
                .map_err(|err| anyhow::Error::msg(err))?;
            collector.entries.push(CssEntry {
                code: bundle.code,
                assets: bundle.assets,
            });

            let module_source = if css_modules {
                let exports = bundle.exports.unwrap_or_default();
                let serialized = serde_json::to_string(&exports)
                    .map_err(|err| anyhow::Error::msg(err.to_string()))?;
                let mut lines = Vec::new();
                lines.push(format!("const styles = {};", serialized));
                lines.push("export default styles;".to_string());
                for key in exports.keys() {
                    append_named_exports(&mut lines, key)?;
                }
                lines.join("\n")
            } else {
                "export default {};".to_string()
            };

            return Ok((module_source, None));
        }

        let css_modules = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.ends_with(".module.css"))
            .unwrap_or(false);
        if css_modules {
            let bundle = css_bundler::bundle_css(&path.display().to_string(), true, true)
                .map_err(|err| anyhow::Error::msg(err))?;
            let exports = bundle.exports.unwrap_or_default();
            let serialized = serde_json::to_string(&exports)
                .map_err(|err| anyhow::Error::msg(err.to_string()))?;
            let mut lines = Vec::new();
            lines.push(format!("const styles = {};", serialized));
            lines.push("export default styles;".to_string());
            for key in exports.keys() {
                append_named_exports(&mut lines, key)?;
            }
            return Ok((lines.join("\n"), None));
        }

        Ok(("export default {};".to_string(), None))
    }
}

struct FsResolver {
    root: PathBuf,
}

impl FsResolver {
    fn new() -> Result<Self, String> {
        let root = std::env::current_dir().map_err(|err| err.to_string())?;
        Ok(Self { root })
    }

    fn resolve_from_node_modules(&self, start_dir: &Path, specifier: &str) -> Option<PathBuf> {
        // Walk up the directory tree looking for node_modules
        let mut current = start_dir;
        loop {
            let node_modules = current.join("node_modules");
            if node_modules.is_dir() {
                // Return the path - let the candidate resolution handle extensions
                return Some(node_modules.join(specifier));
            }

            // Move up to parent directory
            current = current.parent()?;
        }
    }
}

impl Resolve for FsResolver {
    fn resolve(&self, base: &FileName, specifier: &str) -> Result<Resolution, anyhow::Error> {
        if matches!(specifier, "ui/jsx" | "@deka/ui/jsx") {
            anyhow::bail!("ui/jsx is retired; configure jsxRuntime with a React automatic runtime");
        }
        if let Some(filename) = resolve_ui_specifier(base, specifier, false)? {
            return Ok(Resolution {
                filename,
                slug: None,
            });
        }

        let base_path = match base {
            FileName::Real(path) => path.clone(),
            _ => PathBuf::from("."),
        };
        let base_dir = base_path.parent().unwrap_or_else(|| Path::new("."));

        // Check if this is a node_modules import (bare specifier or scoped package)
        let is_node_module = !specifier.starts_with("./")
            && !specifier.starts_with("../")
            && !specifier.starts_with("/")
            && !specifier.starts_with("@/");

        let mut target = if specifier.starts_with("@/") {
            self.root.join(specifier.trim_start_matches("@/"))
        } else if specifier.starts_with('/') {
            PathBuf::from(specifier)
        } else if is_node_module {
            // Try to resolve from node_modules
            if let Some(resolved) = self.resolve_from_node_modules(base_dir, specifier) {
                resolved
            } else {
                // Fallback to treating as relative path
                base_dir.join(specifier)
            }
        } else {
            base_dir.join(specifier)
        };

        let candidates = ds_bundle_candidates(&target);

        for candidate in candidates {
            if candidate.is_file() {
                target = candidate;
                return Ok(Resolution {
                    filename: FileName::Real(target),
                    slug: None,
                });
            }
        }

        anyhow::bail!("Unable to resolve {specifier} from {base:?}")
    }
}

struct NoopHook;

impl Hook for NoopHook {
    fn get_import_meta_props(
        &self,
        _span: swc_common::Span,
        _module_record: &swc_bundler::ModuleRecord,
    ) -> Result<Vec<KeyValueProp>, anyhow::Error> {
        Ok(Vec::new())
    }
}

fn syntax_for_path(path: &Path) -> Syntax {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "ds" => Syntax::Es(EsSyntax {
            jsx: false,
            export_default_from: true,
            import_attributes: true,
            ..Default::default()
        }),
        "ts" => Syntax::Typescript(TsSyntax {
            tsx: false,
            decorators: false,
            dts: false,
            no_early_errors: true,
            disallow_ambiguous_jsx_like: true,
        }),
        "tsx" => Syntax::Typescript(TsSyntax {
            tsx: true,
            decorators: false,
            dts: false,
            no_early_errors: true,
            disallow_ambiguous_jsx_like: true,
        }),
        "jsx" => Syntax::Es(EsSyntax {
            jsx: true,
            export_default_from: true,
            import_attributes: true,
            ..Default::default()
        }),
        _ => Syntax::Es(EsSyntax {
            jsx: false,
            export_default_from: true,
            import_attributes: true,
            ..Default::default()
        }),
    }
}

fn is_typescript(path: &Path) -> bool {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("ds") => false,
        Some("ts") | Some("tsx") => true,
        _ => false,
    }
}



/// Verify that `resolved` stays within `root` after canonicalization.
/// Returns `None` if the path escapes the root (path traversal).
fn guard_path_traversal(resolved: &Path, root: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(resolved).ok()?;
    let canonical_root = std::fs::canonicalize(root).ok()?;
    if canonical.starts_with(&canonical_root) {
        Some(canonical)
    } else {
        None
    }
}

struct DekaResolver {
    root: PathBuf,
    client: bool,
    linked_modules: std::collections::BTreeMap<String, PathBuf>,
}

impl DekaResolver {
    fn new(project_root: PathBuf, client: bool) -> Result<Self, String> {
        let linked_modules = read_linked_modules(&project_root)?;
        Ok(Self {
            root: project_root,
            client,
            linked_modules,
        })
    }

    fn module_roots(&self) -> Vec<PathBuf> {
        let mut dirs = existing_modules_dirs(&self.root);
        if dirs.is_empty() {
            dirs.push(self.root.join(MODULES_DIR));
        }
        dirs
    }

    fn resolve_from_node_modules(&self, start_dir: &Path, specifier: &str) -> Option<PathBuf> {
        let mut current = start_dir;
        loop {
            let node_modules = current.join("node_modules");
            if node_modules.is_dir() {
                return Some(node_modules.join(specifier));
            }
            current = current.parent()?;
        }
    }

    fn resolve_php_module(&self, specifier: &str) -> Result<Option<PathBuf>, String> {
        if let Some(path) = self.resolve_linked_module(specifier)? {
            return Ok(Some(path));
        }
        for modules in self.module_roots() {
            for alias in module_spec_aliases(specifier) {
                let base = if alias.starts_with("@user/") {
                    modules
                        .join("@user")
                        .join(alias.trim_start_matches("@user/"))
                } else {
                    modules.join(&alias)
                };
                if let Some(path) = resolve_with_candidates(&base) {
                    if guard_path_traversal(&path, &modules).is_some() {
                        return Ok(Some(path));
                    }
                }
            }
        }
        Ok(None)
    }

    fn resolve_linked_module(&self, specifier: &str) -> Result<Option<PathBuf>, String> {
        for (package, root) in &self.linked_modules {
            for alias in module_spec_aliases(package) {
                let suffix = if specifier == alias {
                    ""
                } else if let Some(suffix) = specifier.strip_prefix(&(alias + "/")) {
                    suffix
                } else {
                    continue;
                };
                let target = if suffix.is_empty() {
                    root.clone()
                } else {
                    root.join(suffix)
                };
                let candidate = resolve_with_candidates(&target).ok_or_else(|| {
                    format!(
                        "unable to resolve linked module '{specifier}' under {}",
                        root.display()
                    )
                })?;
                if guard_path_traversal(&candidate, root).is_none() {
                    return Err(format!(
                        "linked module '{specifier}' escapes linked package root {}",
                        root.display()
                    ));
                }
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }
}

impl Resolve for DekaResolver {
    fn resolve(&self, base: &FileName, specifier: &str) -> Result<Resolution, anyhow::Error> {
        if matches!(specifier, "ui/jsx" | "@deka/ui/jsx") {
            anyhow::bail!("ui/jsx is retired; configure jsxRuntime with a React automatic runtime");
        }
        if let Some(filename) = resolve_ui_specifier(base, specifier, self.client)? {
            return Ok(Resolution {
                filename,
                slug: None,
            });
        }
        if let Some(name) = specifier.strip_prefix("ui/") {
            let candidate = self
                .root
                .join(".cache")
                .join("dekascript")
                .join("ui")
                .join(format!("{name}.js"));
            if candidate.is_file() {
                return Ok(Resolution {
                    filename: FileName::Real(candidate),
                    slug: None,
                });
            }
        }

        let base_path = match base {
            FileName::Real(path) => path.clone(),
            _ => self.root.clone(),
        };
        let base_dir = base_path.parent().unwrap_or_else(|| self.root.as_path());

        if specifier.starts_with("@/") {
            let target = self.root.join(specifier.trim_start_matches("@/"));
            if let Some(candidate) = resolve_with_candidates(&target) {
                // Guard: resolved @/ path must stay within the project root
                if guard_path_traversal(&candidate, &self.root).is_some() {
                    return Ok(Resolution {
                        filename: FileName::Real(candidate),
                        slug: None,
                    });
                }
            }
        }

        // Local development links intentionally win over installed packages.
        // Check before built-in aliases so a link for @deka/component also
        // wins for the shorthand component/* form.
        if let Some(candidate) = self
            .resolve_linked_module(specifier)
            .map_err(anyhow::Error::msg)?
        {
            return Ok(Resolution {
                filename: FileName::Real(candidate),
                slug: None,
            });
        }

        let is_prefixed_module = specifier.starts_with("component/")
            || specifier.starts_with("encoding/")
            || specifier.starts_with("deka/")
            || specifier.starts_with("db/")
            || specifier.starts_with("core/");
        if is_prefixed_module {
            for modules in self.module_roots() {
                if let Some(candidate) = resolve_with_candidates(&modules.join(specifier)) {
                    if guard_path_traversal(&candidate, &modules).is_some() {
                        return Ok(Resolution {
                            filename: FileName::Real(candidate),
                            slug: None,
                        });
                    }
                }
                let scoped_specifier = format!("@deka/{}", specifier);
                if let Some(candidate) = resolve_with_candidates(&modules.join(&scoped_specifier)) {
                    if guard_path_traversal(&candidate, &modules).is_some() {
                        return Ok(Resolution {
                            filename: FileName::Real(candidate),
                            slug: None,
                        });
                    }
                }
            }
        }

        if let Some(candidate) = self
            .resolve_php_module(specifier)
            .map_err(anyhow::Error::msg)?
        {
            return Ok(Resolution {
                filename: FileName::Real(candidate),
                slug: None,
            });
        }

        let is_node_module = !specifier.starts_with("./")
            && !specifier.starts_with("../")
            && !specifier.starts_with('/')
            && !specifier.starts_with("@/");

        let mut target = if specifier.starts_with('/') {
            PathBuf::from(specifier)
        } else if is_node_module {
            if let Some(resolved) = self.resolve_from_node_modules(base_dir, specifier) {
                resolved
            } else {
                base_dir.join(specifier)
            }
        } else {
            base_dir.join(specifier)
        };

        if let Some(candidate) = resolve_with_candidates(&target) {
            target = candidate;
        }

        if target.is_file() {
            // Guard: resolved relative path must stay within the project root.
            // This closes the path-traversal gap for `../` imports that resolve
            // to a file that actually exists outside the project root.
            if guard_path_traversal(&target, &self.root).is_none() {
                anyhow::bail!(
                    "Path traversal detected: {specifier} from {base:?} resolves outside project root"
                );
            }
            return Ok(Resolution {
                filename: FileName::Real(target),
                slug: None,
            });
        }

        anyhow::bail!("Unable to resolve {specifier} from {base:?}")
    }
}

fn resolve_with_candidates(target: &Path) -> Option<PathBuf> {
    ds_bundle_candidates(target)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

fn load_ui_custom(name: &str) -> Option<&'static str> {
    name.strip_prefix("deka:").and_then(deka_ui::source_for)
}

fn resolve_ui_specifier(
    base: &FileName,
    specifier: &str,
    client: bool,
) -> Result<Option<FileName>, anyhow::Error> {
    let filename = if let Some(source) = deka_ui::source_for(specifier) {
        let _ = source;
        Some(FileName::Custom(format!(
            "deka:{}",
            specifier.trim_end_matches(".js").trim_end_matches(".mjs")
        )))
    } else if let FileName::Custom(name) = base {
        resolve_ui_relative(name, specifier)
    } else {
        None
    };
    let Some(filename) = filename else {
        return Ok(None);
    };
    if client && is_ui_server_filename(&filename) {
        anyhow::bail!("{CLIENT_SERVER_IMPORT_ERROR}");
    }
    Ok(Some(filename))
}

fn resolve_ui_relative(base_name: &str, specifier: &str) -> Option<FileName> {
    let _current = base_name.strip_prefix("deka:ui/")?;
    if !specifier.starts_with("./") && !specifier.starts_with("../") {
        return None;
    }
    let file = specifier
        .rsplit('/')
        .next()?
        .trim_end_matches(".js")
        .trim_end_matches(".mjs");
    let spec = format!("ui/{file}");
    deka_ui::source_for(&spec).map(|_| FileName::Custom(format!("deka:{spec}")))
}

fn is_ui_server_filename(file: &FileName) -> bool {
    match file {
        FileName::Custom(name) => name == "deka:ui/server",
        _ => false,
    }
}

/// DekaScript first (`ds_source_candidates`, deka#241), then JS/TS for mixed
/// graphs. Never `.phpx` / `.php`.
fn ds_bundle_candidates(target: &Path) -> Vec<PathBuf> {
    let mut candidates = ds_source_candidates(target);
    if target.extension().is_none() {
        for ext in ["ts", "tsx", "jsx", "js", "mjs"] {
            candidates.push(target.with_extension(ext));
            candidates.push(target.join(format!("index.{ext}")));
        }
    } else if !matches!(
        target.extension().and_then(|ext| ext.to_str()),
        Some("phpx" | "php")
    ) {
        candidates.push(target.to_path_buf());
    }
    candidates
}

fn append_named_exports(lines: &mut Vec<String>, key: &str) -> Result<(), anyhow::Error> {
    let key_json = serde_json::to_string(key).map_err(|err| anyhow::Error::msg(err.to_string()))?;
    let mut names = Vec::new();
    if is_valid_identifier(key) {
        names.push(key.to_string());
    }
    let camel = to_camel_case(key);
    if !camel.is_empty() && camel != key && is_valid_identifier(&camel) {
        names.push(camel);
    }
    for name in names {
        lines.push(format!("export const {} = styles[{}];", name, key_json));
    }
    Ok(())
}

fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let first = match chars.next() {
        Some(ch) => ch,
        None => return false,
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    for ch in chars {
        if !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '$') {
            return false;
        }
    }
    true
}

fn to_camel_case(input: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for ch in input.chars() {
        if ch == '-' || ch == '_' || ch == ' ' {
            upper = true;
            continue;
        }
        if upper {
            out.extend(ch.to_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod prelude_tests;
#[cfg(test)]
mod tests;
