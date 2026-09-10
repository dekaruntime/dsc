//! Cross-file module graph compilation for DekaScript compiler v2.
//!
//! This module discovers all reachable `.ds` files from an entry point,
//! resolves relative and bare stdlib specifiers, and compiles each module
//! through the v2 pipeline.  Exported structs, enums, type aliases, and
//! receiver methods are propagated through the module graph so importers can
//! construct imported structs and match imported enums with full typechecking.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use bumpalo::Bump;
use deka_syntax::Diagnostic;

use crate::shake::{self, ShakeModule, ShakePlan};
use crate::{CompileOptions, compile_to_js_with_imports_and_options, parse_source_module_meta};

/// Compiler-provided JS runtime (`ui/jsx`, `ui/form`, …). These are not
/// DekaScript modules: hosts materialize the files, and the graph leaves the
/// import specifier intact.
pub(crate) fn is_compiler_ui_spec(spec: &str) -> bool {
    let bare = spec.trim().strip_prefix("@deka/").unwrap_or(spec.trim());
    bare == "ui" || bare.starts_with("ui/")
}

/// A module loader supplies source text and resolves specifiers for the
/// graph compiler.
///
/// Callers provide the loader so the compiler can run against a filesystem,
/// an in-memory test fixture set, or a virtual project layout.
pub trait ModuleLoader {
    /// Resolve a module specifier relative to the importing file.
    ///
    /// Returns the absolute path to the DekaScript source file that should
    /// be loaded.
    fn resolve(&self, specifier: &str, referrer: &Path) -> Result<PathBuf, String>;

    /// Read the source text for a resolved module path.
    fn load(&self, path: &Path) -> Result<String, String>;
}

/// Filesystem loader that mirrors the runtime's module resolution rules.
///
/// Resolution order (kept in sync with `deka_project::module_spec`):
///
/// 1. `@/path` → project root.
/// 2. `/abs/path` → absolute path (must still lie inside the project root).
/// 3. `./path` or `../path` → relative to the importing file's directory.
/// 4. Bare specifier (e.g. `json`, `@deka/crypto`) → a project-local link
///    from `.deka/links.json`, then `ds_modules/` (with `@deka/` aliases),
///    falling back to `module_root` when provided.
pub struct FsModuleLoader {
    project_root: PathBuf,
    module_root: Option<PathBuf>,
    linked_modules: BTreeMap<String, PathBuf>,
    link_error: Option<String>,
}

impl FsModuleLoader {
    pub fn new(project_root: PathBuf) -> Self {
        let (linked_modules, link_error) = load_linked_modules(&project_root);
        Self {
            project_root,
            module_root: None,
            linked_modules,
            link_error,
        }
    }

    /// Create a loader with an explicit module root for resolving bare stdlib
    /// imports. When a bare specifier cannot be found under the project's
    /// `ds_modules/`, the loader tries `<module_root>/ds_modules/` before
    /// giving up.
    pub fn with_module_root(project_root: PathBuf, module_root: PathBuf) -> Self {
        let (linked_modules, link_error) = load_linked_modules(&project_root);
        Self {
            project_root,
            module_root: Some(module_root),
            linked_modules,
            link_error,
        }
    }

    fn resolve_ds_file(&self, base: &Path) -> Option<PathBuf> {
        deka_project::module_spec::resolve_ds_source_file(base)
    }

    fn guard_project_root(&self, path: &Path) -> Result<PathBuf, String> {
        let canon_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let canon_root =
            std::fs::canonicalize(&self.project_root).unwrap_or_else(|_| self.project_root.clone());
        if canon_path.starts_with(&canon_root) {
            return Ok(canon_path);
        }
        // Files inside a linked package root are part of the project even
        // though they live outside the project directory on disk.
        for root in self.linked_modules.values() {
            let canon_link = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            if canon_path.starts_with(&canon_link) {
                return Ok(canon_path);
            }
        }
        Err(format!(
            "import escapes project root: {} is outside {}",
            canon_path.display(),
            canon_root.display()
        ))
    }

    fn resolve_linked_module(&self, specifier: &str) -> Result<Option<PathBuf>, String> {
        for (package, root) in &self.linked_modules {
            for alias in deka_project::module_spec::module_spec_aliases(package) {
                let suffix = if specifier == alias {
                    ""
                } else if let Some(suffix) = specifier.strip_prefix(&(alias + "/")) {
                    suffix
                } else {
                    continue;
                };
                let base = if suffix.is_empty() {
                    root.clone()
                } else {
                    root.join(suffix)
                };
                let resolved = self.resolve_ds_file(&base).ok_or_else(|| {
                    format!(
                        "unable to resolve linked module '{specifier}' under {}",
                        root.display()
                    )
                })?;
                let canonical = std::fs::canonicalize(&resolved).map_err(|err| {
                    format!(
                        "unable to canonicalize linked module '{specifier}' at {}: {err}",
                        resolved.display()
                    )
                })?;
                let canonical_root = std::fs::canonicalize(root).map_err(|err| {
                    format!(
                        "unable to canonicalize linked package root {}: {err}",
                        root.display()
                    )
                })?;
                if canonical.starts_with(canonical_root) {
                    return Ok(Some(canonical));
                }
                return Err(format!(
                    "linked module '{specifier}' escapes linked package root {}",
                    root.display()
                ));
            }
        }
        Ok(None)
    }
}

fn load_linked_modules(project_root: &Path) -> (BTreeMap<String, PathBuf>, Option<String>) {
    match deka_project::modules::read_linked_modules(project_root) {
        Ok(links) => (links, None),
        Err(error) => (BTreeMap::new(), Some(error)),
    }
}

impl ModuleLoader for FsModuleLoader {
    fn resolve(&self, specifier: &str, referrer: &Path) -> Result<PathBuf, String> {
        if let Some(error) = &self.link_error {
            return Err(error.clone());
        }
        let trimmed = specifier.trim();

        if trimmed.starts_with("http://")
            || trimmed.starts_with("https://")
            || trimmed.starts_with("file://")
        {
            return Err(format!(
                "unsupported module specifier '{}' (only relative and bare stdlib imports are supported)",
                trimmed
            ));
        }

        // Project-root alias.
        if let Some(rel) = trimmed.strip_prefix("@/") {
            if rel.split('/').any(|seg| seg == ".." || seg == ".") {
                return Err(format!(
                    "invalid project alias '{}': path cannot contain '.' or '..'",
                    trimmed
                ));
            }
            let base = self.project_root.join(rel);
            let resolved = self
                .resolve_ds_file(&base)
                .ok_or_else(|| format!("cannot resolve project alias '{}'", trimmed))?;
            return self.guard_project_root(&resolved);
        }

        // Absolute path.
        if trimmed.starts_with('/') {
            let base = PathBuf::from(trimmed);
            let resolved = self
                .resolve_ds_file(&base)
                .ok_or_else(|| format!("cannot resolve absolute import '{}'", trimmed))?;
            return self.guard_project_root(&resolved);
        }

        // Relative path.
        if trimmed.starts_with("./") || trimmed.starts_with("../") {
            let base = referrer
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(trimmed);
            let resolved = self
                .resolve_ds_file(&base)
                .ok_or_else(|| format!("cannot resolve relative import '{}'", trimmed))?;
            return self.guard_project_root(&resolved);
        }

        if let Some(resolved) = self.resolve_linked_module(trimmed)? {
            return Ok(resolved);
        }

        // Bare / stdlib specifier.
        let modules_dir = deka_project::modules::resolve_modules_dir(&self.project_root);
        let mut aliases = deka_project::module_spec::module_spec_aliases(trimmed);
        if trimmed.contains('/') && !trimmed.starts_with('@') {
            aliases.push(format!("@deka/{}", trimmed));
        }
        for alias in &aliases {
            let base = modules_dir.join(alias);
            if let Some(resolved) = self.resolve_ds_file(&base) {
                return self.guard_project_root(&resolved);
            }
        }

        // Explicit module_root fallback for stdlib-only tenants.
        if let Some(module_root) = &self.module_root {
            let modules_dir = deka_project::modules::resolve_modules_dir(module_root);
            for alias in &aliases {
                let base = modules_dir.join(alias);
                if let Some(resolved) = self.resolve_ds_file(&base) {
                    return self.guard_project_root(&resolved);
                }
            }
        }

        Err(format!(
            "cannot resolve module specifier '{}' (tried {:?})",
            trimmed, aliases
        ))
    }

    fn load(&self, path: &Path) -> Result<String, String> {
        std::fs::read_to_string(path)
            .map_err(|err| format!("failed to read {}: {}", path.display(), err))
    }
}

/// Options for compiling a module graph.
#[derive(Debug, Default, Clone)]
pub struct GraphCompileOptions {
    /// When true, any import-graph path to `ui/server` is a compile error.
    pub client: bool,
    /// Base URL for bare stdlib module specifiers. When set, known stdlib
    /// imports (`io`, `json`, …) are not resolved to `.ds` files; the emitter
    /// rewrites them to `<module_base>/<spec>.mjs` and the host serves those
    /// URLs. This mirrors the single-file compiler's `module_base` option for
    /// browser hosts that have no stdlib filesystem (deka#497).
    pub module_base: Option<String>,
    /// Project root that build slot ids are hashed relative to (dsc#61).
    /// Forwarded to [`CompileOptions::module_root`] for every module so plan
    /// and graph emission agree on `deka:dev/<id>` identities.
    pub module_root: Option<PathBuf>,
}

/// A discovered module and its outgoing dependencies.
#[derive(Debug)]
struct GraphModule {
    path: PathBuf,
    source: String,
    /// Resolved dependency path for each import specifier in this module.
    dependencies: HashMap<String, PathBuf>,
    /// Compiler-provided specifiers (`ui/jsx`, …) that are not `.ds` files.
    virtual_imports: Vec<String>,
}

/// Result of compiling a module graph.
#[derive(Debug)]
pub struct ModuleGraphResult {
    /// Absolute path of the entry module.
    pub entry: PathBuf,
    /// Map from absolute module path to emitted JavaScript. The shared
    /// runtime prelude is NOT inlined (see [`Self::prelude`]); module bodies
    /// reference its helpers as free identifiers.
    pub modules: HashMap<PathBuf, String>,
    /// The shared runtime prelude synthesized ONCE for the whole program
    /// from the union of every module's demand (deka#595): a helper appears
    /// iff some call site in the program needs it, and each decomposable
    /// member (`impl` / `implMut` / embeds, `__deka_type_of` branches)
    /// appears iff some module asked for it. Bundle consumers prepend this
    /// to the single output scope.
    pub prelude: String,
    /// Per-module shared prelude, synthesized from each module's own demand
    /// with the program-level `declares_*` bits overlaid — a consuming
    /// module's `__deka_type_of` must brand-check kinds it never declares,
    /// because values cross module boundaries without their type binding
    /// being imported (function returns). Hosts that serve modules as
    /// separate files (separate scopes) prepend the module's prelude to its
    /// body — see [`Self::self_contained_modules`].
    pub module_preludes: HashMap<PathBuf, String>,
    /// Every import specifier written anywhere in the graph, as written.
    ///
    /// Callers that gate on dependencies need the transitive set, not the
    /// entry file's imports: a module the entry never mentions can pull in a
    /// stdlib package the project does not declare.  gated on
    /// entry-only imports and missed exactly that (deka#430).
    pub imports: BTreeSet<String>,
    /// Build-only slots from every kept module. The graph compiler aggregates
    /// these without evaluating them; Deka consumes the plan later.
    pub dev_plan: crate::DevPlan,
}

impl ModuleGraphResult {
    /// Self-contained per-module JS (own prelude + body) for hosts that serve
    /// modules as separate files and cannot share the program prelude.
    pub fn self_contained_modules(&self) -> HashMap<PathBuf, String> {
        self.modules
            .iter()
            .map(|(path, js)| {
                let mut full = self.module_preludes.get(path).cloned().unwrap_or_default();
                full.push_str(js);
                (path.clone(), full)
            })
            .collect()
    }
}

/// Compile every reachable `.ds` module from `entry` and return the emitted
/// JavaScript for each file.
///
/// The graph is discovered via the supplied loader, cycles are rejected with
/// diagnostics, and each module is compiled through the full v2 pipeline
/// (parse → typecheck → emit).  Type information is propagated from
/// dependencies to importers in topological order so cross-module structs,
/// enums, and receiver methods resolve correctly.
pub fn compile_module_graph(
    entry: &Path,
    loader: &dyn ModuleLoader,
) -> Result<ModuleGraphResult, Vec<Diagnostic>> {
    compile_module_graph_with_options(entry, loader, GraphCompileOptions::default())
}

/// Compile every reachable `.ds` module from `entry` with shaking options.
pub fn compile_module_graph_with_options(
    entry: &Path,
    loader: &dyn ModuleLoader,
    options: GraphCompileOptions,
) -> Result<ModuleGraphResult, Vec<Diagnostic>> {
    let entry = std::fs::canonicalize(entry).unwrap_or_else(|_| entry.to_path_buf());
    let arena = Bump::new();

    let mut modules: HashMap<PathBuf, GraphModule> = HashMap::new();
    let mut errors: Vec<Diagnostic> = Vec::new();
    let mut all_imports: BTreeSet<String> = BTreeSet::new();

    // ------------------------------------------------------------------
    // Discovery: BFS from the entry, resolving every import specifier.
    // ------------------------------------------------------------------
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(entry.clone());

    while let Some(path) = queue.pop_front() {
        if modules.contains_key(&path) {
            continue;
        }

        let source = match loader.load(&path) {
            Ok(src) => src,
            Err(msg) => {
                errors.push(diag(0, 0, format!("{}: {}", path.display(), msg)));
                continue;
            }
        };

        let meta = parse_source_module_meta(&source);
        let mut dependencies = HashMap::with_capacity(meta.imports.len());
        let mut virtual_imports = Vec::new();
        for import in &meta.imports {
            // Recorded before resolution so the set is complete even for
            // specifiers this loader cannot resolve.
            all_imports.insert(import.path.trim().to_string());
            if let Some(ui) = shake::normalize_ui_specifier(&import.path) {
                virtual_imports.push(ui);
                continue;
            }
            if is_compiler_ui_spec(&import.path) {
                continue;
            }
            // Side-effect CSS imports (`import "./x.css"`) are not JS modules:
            // emit drops them and the per-route CSS collector rewrites their
            // selectors with the component's scope stamp (RFD 24 §10.6).
            // Treat them as virtual so component modules can author CSS.
            if import.specs.is_empty() && import.path.trim().to_ascii_lowercase().ends_with(".css")
            {
                continue;
            }
            // With a module base configured (browser/WASM hosts), known stdlib
            // bare specifiers are normally served by the host as `<base>/<spec>.mjs`
            // and left virtual. If the loader can resolve the specifier (e.g. a
            // type stub exists in the project), use that resolution for
            // typechecking and still emit the original bare specifier (deka#497).
            let is_stdlib =
                options.module_base.is_some() && crate::is_stdlib_module_spec(&import.path);
            if is_stdlib {
                if let Ok(dep) = loader.resolve(&import.path, &path) {
                    dependencies.insert(import.path.clone(), dep.clone());
                    if !modules.contains_key(&dep) {
                        queue.push_back(dep);
                    }
                    continue;
                }
                continue;
            }
            match loader.resolve(&import.path, &path) {
                Ok(dep) => {
                    let from_ds = path.extension().and_then(|e| e.to_str()) == Some("ds");
                    let to_dsx = dep.extension().and_then(|e| e.to_str()) == Some("dsx");
                    if from_ds && to_dsx {
                        errors.push(diag(
                            0,
                            0,
                            format!(
                                "{}: `.ds` files cannot import `.dsx` modules (`{}`)",
                                path.display(),
                                import.path
                            ),
                        ));
                    }
                    dependencies.insert(import.path.clone(), dep.clone());
                    if !modules.contains_key(&dep) {
                        queue.push_back(dep);
                    }
                }
                Err(msg) => {
                    errors.push(diag(
                        0,
                        0,
                        format!(
                            "{}: cannot resolve '{}': {}",
                            path.display(),
                            import.path,
                            msg
                        ),
                    ));
                }
            }
        }

        modules.insert(
            path.clone(),
            GraphModule {
                path,
                source,
                dependencies,
                virtual_imports,
            },
        );
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    // ------------------------------------------------------------------
    // Topological order (Kahn).  Cycles produce a diagnostic.
    // ------------------------------------------------------------------
    let order = topological_order(&modules).map_err(|cycle| {
        vec![diag(
            0,
            0,
            format!(
                "module import cycle detected: {}",
                cycle
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" -> ")
            ),
        )]
    })?;

    // ------------------------------------------------------------------
    // Collect exported type information for every module first.  We keep the
    // parsed programs alive alongside the arena so importers can reference
    // dependency AST nodes safely.
    // ------------------------------------------------------------------
    let mut programs: HashMap<PathBuf, deka_syntax::Program> = HashMap::new();
    let mut exports: HashMap<PathBuf, deka_syntax::ModuleExports> =
        HashMap::with_capacity(modules.len());
    for module in modules.values() {
        let parse_result = deka_syntax::parse(&module.source, &arena);
        // A module that fails to parse must fail the whole graph here, with
        // its own diagnostics and its path — not vanish from `programs` and
        // let every importer bind its imported names to the Infer sentinel,
        // which surfaces downstream as bare `<infer>` errors that name
        // neither the module nor the cause (deka#567). This is the gate every
        // multi-module path (transpile, build, run/serve, check --as-package,
        // wasm project mode) funnels through.
        if !parse_result.errors.is_empty() {
            for d in &parse_result.errors {
                errors.push(diag(
                    d.line,
                    d.column,
                    format!("{}: {}", module.path.display(), d.message),
                ));
            }
            continue;
        }
        if let Some(program) = parse_result.program {
            programs.insert(module.path.clone(), program);
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    for (path, program) in programs.iter() {
        exports.insert(
            path.clone(),
            deka_syntax::collect_module_exports(program, &arena),
        );
    }

    // ------------------------------------------------------------------
    // Compiler-private build fragments (dsc#52). Each module's exported
    // factories get a descriptor computed in the declaring module's own
    // namespace — the only place private nested types are visible — so a
    // consumer's build hydration can reach factories it has no lexical
    // binding for. The per-module closure set (every factory named by those
    // fragments) drives the private `__deka_factories` export decision.
    // ------------------------------------------------------------------
    let mut build_closures: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    for (path, program) in programs.iter() {
        let module = modules.get(path).expect("module in graph");
        let inferred = crate::infer_stdlib_imports_for_source(&module.source, &arena);
        let mut combined: HashMap<&str, &deka_syntax::ModuleExports> = HashMap::new();
        for (spec, dep_exports) in &inferred {
            combined.insert(*spec, dep_exports);
        }
        for (spec, dep) in module.dependencies.iter() {
            if let Some(dep_exports) = exports.get(dep) {
                combined.insert(spec.as_str(), dep_exports);
            }
        }
        let fragments = deka_syntax::build_module_build_fragments(program, &combined);
        if fragments.is_empty() {
            continue;
        }
        // Map declared names to the names this module exports them under
        // (`export { User as Person }` stores the fragment under `Person`).
        let mut export_names: HashMap<&str, &str> = HashMap::new();
        for stmt in program.statements.iter() {
            if let deka_syntax::Stmt::Export {
                decl: deka_syntax::ExportDecl::NamedGroup { names, .. },
                ..
            } = stmt
            {
                for name in names.iter() {
                    export_names.insert(name.name, name.alias.unwrap_or(name.name));
                }
            }
        }
        let mut closure_names = HashSet::new();
        let module_exports = exports.get_mut(path).expect("module exports collected");
        for (declared, tree) in fragments {
            let Some(external) = export_names.get(declared) else {
                continue;
            };
            if module_exports.structs.contains_key(external)
                || module_exports.enums.contains_key(external)
                || module_exports.newtypes.contains_key(external)
            {
                deka_emit::build_factory_names(&tree, &mut closure_names);
                module_exports.build_fragments.insert(external, tree);
            }
        }
        if !closure_names.is_empty() {
            build_closures.insert(path.clone(), closure_names);
        }
    }

    // `collect_module_exports` can identify a re-export name, but the graph
    // must resolve that name through the barrel's import edge so downstream
    // modules receive the actual signature and runtime export metadata.
    for _ in 0..modules.len() {
        let mut changed = false;
        for module in modules.values() {
            let Some(program) = programs.get(&module.path) else {
                continue;
            };
            let mut imports_by_local: HashMap<&str, (&str, &PathBuf)> = HashMap::new();
            for stmt in program.statements.iter() {
                if let deka_syntax::Stmt::Import {
                    specifiers, source, ..
                } = stmt
                {
                    if let Some(dep) = module.dependencies.get(*source) {
                        for spec in specifiers.iter() {
                            imports_by_local.insert(spec.local, (spec.imported, dep));
                        }
                    }
                }
            }
            let export_specs: Vec<(&str, &str, Option<&str>)> = program
                .statements
                .iter()
                .filter_map(|stmt| match stmt {
                    deka_syntax::Stmt::Export {
                        decl: deka_syntax::ExportDecl::NamedGroup { names, source },
                        ..
                    } => Some(
                        names
                            .iter()
                            .map(move |n| (n.name, n.alias.unwrap_or(n.name), *source))
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                })
                .flatten()
                .collect();
            for (local, external, explicit_source) in export_specs {
                let (imported, dep) = if let Some(source) = explicit_source {
                    let Some(dep) = module.dependencies.get(source) else {
                        continue;
                    };
                    (local, dep)
                } else {
                    let Some((imported, dep)) = imports_by_local.get(local).copied() else {
                        continue;
                    };
                    (imported, dep)
                };
                let Some(dep_exports) = exports.get(dep).cloned() else {
                    continue;
                };
                changed |=
                    copy_export(&mut exports, &module.path, &dep_exports, imported, external);
            }
        }
        if !changed {
            break;
        }
    }

    // Hydration is a property of a component's rendered subtree, not merely
    // of the file that happens to import it. Compute that property from leaves
    // to entry so an importer sees it through ordinary module exports and can
    // point the diagnostic at its own JSX tag (dsc#65). `order` is
    // entry-to-dependencies, hence the reverse traversal here.
    for path in order.iter().rev() {
        let Some(program) = programs.get(path) else {
            continue;
        };
        let module = modules.get(path).expect("module in graph");
        let mut combined: HashMap<&str, &deka_syntax::ModuleExports> = HashMap::new();
        for (specifier, dependency) in module.dependencies.iter() {
            if let Some(dependency_exports) = exports.get(dependency) {
                combined.insert(specifier.as_str(), dependency_exports);
            }
        }
        let interactive = deka_syntax::collect_interactive_components(program, &combined);
        let exported = deka_syntax::collect_exported_interactive_components(program, &interactive);
        exports
            .get_mut(path)
            .expect("module exports collected")
            .interactive_components = exported;
    }

    // Reject imports of names the dependency does not export. The typechecker
    // binds imported names loosely, so without this check a bad import only
    // surfaced as a runtime link error (deka#198), and browser project mode
    // could not report it at all (deka#497). Mirrors the native module
    // validator's diagnostic.
    for module in modules.values() {
        let Some(program) = programs.get(&module.path) else {
            continue;
        };
        for stmt in program.statements.iter() {
            let deka_syntax::Stmt::Import {
                specifiers, source, ..
            } = stmt
            else {
                continue;
            };
            let Some(dep) = module.dependencies.get(*source) else {
                continue;
            };
            let Some(dep_exports) = exports.get(dep) else {
                continue;
            };
            for spec in specifiers.iter() {
                let known = dep_exports.values.contains_key(spec.imported)
                    || dep_exports.structs.contains_key(spec.imported)
                    || dep_exports.enums.contains_key(spec.imported)
                    || dep_exports.aliases.contains_key(spec.imported)
                    || dep_exports.newtypes.contains_key(spec.imported)
                    || dep_exports.re_exports.contains(spec.imported);
                if !known {
                    errors.push(diag(
                        spec.span.start.line,
                        spec.span.start.column,
                        format!(
                            "Missing export '{}' in '{}' (imported by '{}').",
                            spec.imported,
                            source,
                            module.path.display()
                        ),
                    ));
                }
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    // Build per-module import maps pointing to dependency exports.
    let mut imports: HashMap<PathBuf, HashMap<&str, &deka_syntax::ModuleExports>> =
        HashMap::with_capacity(modules.len());
    for module in modules.values() {
        let mut module_imports = HashMap::new();
        for (spec, dep) in module.dependencies.iter() {
            if let Some(dep_exports) = exports.get(dep) {
                module_imports.insert(spec.as_str(), dep_exports);
            }
        }
        imports.insert(module.path.clone(), module_imports);
    }

    // ------------------------------------------------------------------
    // Graph shaking: drop unused exports of pure modules, then unused modules.
    // Client entries that can reach ui/server fail the build.
    // ------------------------------------------------------------------
    let shake_modules: HashMap<PathBuf, ShakeModule> = modules
        .iter()
        .map(|(path, module)| {
            (
                path.clone(),
                ShakeModule {
                    dependencies: module.dependencies.clone(),
                    virtual_imports: module.virtual_imports.clone(),
                },
            )
        })
        .collect();
    let mut plan: ShakePlan = shake::shake_graph(&entry, &shake_modules, &programs);
    if options.client && plan.reaches_ui_server {
        errors.push(diag(
            0,
            0,
            format!("{}: client bundle cannot import ui/server", entry.display()),
        ));
        return Err(errors);
    }

    // A build entry can construct an imported struct/enum/newtype solely for
    // its declared-type materializer. The runtime graph still needs that
    // exported factory even though ordinary expression liveness sees it only
    // inside `build { ... }`; retain the binding and its defining module.
    let mut closure_needed: HashSet<PathBuf> = HashSet::new();
    let mut factory_queue: VecDeque<PathBuf> = plan.keep.iter().cloned().collect();
    while let Some(path) = factory_queue.pop_front() {
        let Some(program) = programs.get(&path) else {
            continue;
        };
        let Some(module) = modules.get(&path) else {
            continue;
        };
        for stmt in program.statements.iter() {
            let deka_syntax::Stmt::Import {
                specifiers, source, ..
            } = stmt
            else {
                continue;
            };
            let Some(dep) = module.dependencies.get(*source) else {
                continue;
            };
            let Some(dep_exports) = exports.get(dep) else {
                continue;
            };
            for specifier in specifiers.iter() {
                let is_factory = dep_exports.structs.contains_key(specifier.imported)
                    || dep_exports.enums.contains_key(specifier.imported)
                    || dep_exports.newtypes.contains_key(specifier.imported);
                // A build entry can reference a factory from its body or only
                // from its declared type — the latter is how private nested
                // types reach hydration at all (dsc#52).
                if !is_factory
                    || !(deka_emit::dev_uses_name(program, specifier.local)
                        || deka_emit::live_dev_uses_name(program, None, specifier.local))
                {
                    continue;
                }
                match plan
                    .live
                    .entry(path.clone())
                    .or_insert_with(|| Some(HashSet::new()))
                {
                    Some(live) => {
                        live.insert(specifier.local.to_string());
                    }
                    None => {}
                }
                match plan
                    .live
                    .entry(dep.clone())
                    .or_insert_with(|| Some(HashSet::new()))
                {
                    Some(live) => {
                        live.insert(specifier.imported.to_string());
                    }
                    None => {}
                }
                if plan.keep.insert(dep.clone()) {
                    factory_queue.push_back(dep.clone());
                }
                // Only a build binding that survived shaking may retain the
                // dependency's compiler-private factory closure (dsc#52).
                if deka_emit::live_dev_uses_name(
                    program,
                    plan.live.get(&path).cloned().flatten().as_ref(),
                    specifier.local,
                ) {
                    closure_needed.insert(dep.clone());
                }
            }
        }
    }

    // A module that emits the factory closure must keep every import the
    // closure captures — e.g. a factory declared in a third module that an
    // exported type references by import. Follow those edges and retain the
    // defining modules; their own closures are not needed, only the bindings.
    let mut closure_queue: VecDeque<PathBuf> = closure_needed.iter().cloned().collect();
    while let Some(path) = closure_queue.pop_front() {
        let Some(program) = programs.get(&path) else {
            continue;
        };
        let Some(module) = modules.get(&path) else {
            continue;
        };
        let Some(names) = build_closures.get(&path) else {
            continue;
        };
        for stmt in program.statements.iter() {
            let deka_syntax::Stmt::Import {
                specifiers, source, ..
            } = stmt
            else {
                continue;
            };
            let Some(dep) = module.dependencies.get(*source) else {
                continue;
            };
            for specifier in specifiers.iter() {
                if !names.contains(specifier.local) {
                    continue;
                }
                match plan
                    .live
                    .entry(path.clone())
                    .or_insert_with(|| Some(HashSet::new()))
                {
                    Some(live) => {
                        live.insert(specifier.local.to_string());
                    }
                    None => {}
                }
                match plan
                    .live
                    .entry(dep.clone())
                    .or_insert_with(|| Some(HashSet::new()))
                {
                    Some(live) => {
                        live.insert(specifier.imported.to_string());
                    }
                    None => {}
                }
                if plan.keep.insert(dep.clone()) {
                    closure_queue.push_back(dep.clone());
                }
            }
        }
    }

    // Runtime shaking must not make a dev-only dependency disappear. Start
    // with kept modules and follow only imports referenced by a `build` body;
    // this is deliberately separate from the runtime graph.
    let mut dev_keep = std::collections::HashSet::new();
    let mut dev_queue: VecDeque<PathBuf> = plan.keep.iter().cloned().collect();
    while let Some(path) = dev_queue.pop_front() {
        if !dev_keep.insert(path.clone()) {
            continue;
        }
        let Some(program) = programs.get(&path) else {
            continue;
        };
        let Some(module) = modules.get(&path) else {
            continue;
        };
        for stmt in program.statements.iter() {
            let deka_syntax::Stmt::Import {
                specifiers, source, ..
            } = stmt
            else {
                continue;
            };
            if specifiers
                .iter()
                .any(|specifier| deka_emit::dev_uses_name(program, specifier.local))
            {
                if let Some(dep) = module.dependencies.get(*source) {
                    dev_queue.push_back(dep.clone());
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Emit each kept module.  We compile in dependency order so imported
    // structs, enums, and receiver methods are known to the typechecker.
    // The shared runtime prelude is detached from every module: each module
    // records its demand, and the program prelude below is synthesized once
    // from their union (deka#595).
    // ------------------------------------------------------------------
    let mut emitted: HashMap<PathBuf, String> = HashMap::with_capacity(plan.keep.len());
    let mut demands: HashMap<PathBuf, deka_emit::prelude::PreludeDemand> =
        HashMap::with_capacity(plan.keep.len());
    let mut dev_slots = Vec::new();
    // Route disposition is a fact about the entry module only; imported
    // modules may export their own `prerender` without it meaning anything
    // for this plan (dsc#54).
    let mut entry_prerender: Option<bool> = None;
    for path in order {
        if !plan.keep.contains(&path) && !dev_keep.contains(&path) {
            continue;
        }
        let module = modules.get(&path).expect("module in graph");
        let input = path.to_string_lossy();
        let inferred = crate::infer_stdlib_imports_for_source(&module.source, &arena);
        let mut combined: HashMap<&str, &deka_syntax::ModuleExports> = HashMap::new();
        for (spec, exports) in &inferred {
            combined.insert(*spec, exports);
        }
        if let Some(graph_imports) = imports.get(&path) {
            for (spec, exports) in graph_imports {
                combined.insert(*spec, *exports);
            }
        }
        let compile_options = CompileOptions {
            used_exports: plan.live.get(&path).cloned().flatten(),
            client: options.client,
            module_base: options.module_base.clone(),
            module_root: options.module_root.clone(),
            detached_prelude: true,
            // The private factory closure is emitted only when a live consumer
            // build reaches this module's factories (dsc#52).
            build_closure_names: if closure_needed.contains(&path) {
                build_closures.get(&path).cloned().unwrap_or_default()
            } else {
                HashSet::new()
            },
            ..Default::default()
        };
        match compile_to_js_with_imports_and_options(
            &module.source,
            &input,
            &arena,
            &combined,
            compile_options,
        ) {
            Ok(result) => {
                if plan.keep.contains(&path) {
                    demands.insert(path.clone(), result.demand);
                    emitted.insert(path.clone(), result.js);
                }
                dev_slots.extend(result.dev_plan.slots);
                if path == entry {
                    entry_prerender = result.dev_plan.prerender;
                }
            }
            Err(diagnostics) => {
                for mut diagnostic in diagnostics {
                    diagnostic.message = format!("{}: {}", path.display(), diagnostic.message);
                    errors.push(diagnostic);
                }
            }
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    // Synthesize the shared runtime prelude once for the whole program from
    // the union of per-module demand: each helper iff some call site needs
    // it, each member iff some module asked for it (deka#595). Per-module
    // preludes serve hosts that keep modules in separate scopes.
    let mut program_demand = deka_emit::prelude::PreludeDemand::default();
    for demand in demands.values() {
        program_demand.union(demand);
    }
    let mut module_preludes: HashMap<PathBuf, String> = HashMap::with_capacity(demands.len());
    for (path, demand) in &demands {
        // The `__deka_type_of` brand branches must reflect the whole program,
        // not the module's own declarations: values cross module boundaries
        // without their type binding being imported (function returns), so a
        // consuming module's prelude needs the struct/enum/newtype check even
        // when it declares nothing itself. Overlay the program-level bits.
        let mut module_demand = demand.clone();
        module_demand.declares_structs = program_demand.declares_structs;
        module_demand.declares_enums = program_demand.declares_enums;
        module_demand.declares_newtypes = program_demand.declares_newtypes;
        module_preludes.insert(
            path.clone(),
            deka_emit::prelude::shared_prelude(&module_demand),
        );
    }
    let prelude = deka_emit::prelude::shared_prelude(&program_demand);

    Ok(ModuleGraphResult {
        entry,
        modules: emitted,
        prelude,
        module_preludes,
        imports: all_imports,
        dev_plan: crate::DevPlan {
            version: 2,
            prerender: entry_prerender,
            slots: dev_slots,
        },
    })
}

fn copy_export<'a>(
    exports: &mut HashMap<PathBuf, deka_syntax::ModuleExports<'a>>,
    target: &Path,
    source: &deka_syntax::ModuleExports<'a>,
    imported: &'a str,
    external: &'a str,
) -> bool {
    let Some(dest) = exports.get_mut(target) else {
        return false;
    };
    let mut changed = false;
    if let Some(value) = source.values.get(imported) {
        changed |= dest.values.insert(external, value.clone()) != Some(value.clone());
    }
    if let Some(info) = source.structs.get(imported) {
        changed |= dest.structs.insert(external, info.clone()).is_none();
    }
    if let Some(info) = source.enums.get(imported) {
        changed |= dest.enums.insert(external, info.clone()).is_none();
    }
    if let Some(info) = source.aliases.get(imported) {
        changed |= dest.aliases.insert(external, info.clone()).is_none();
    }
    if let Some(info) = source.newtypes.get(imported) {
        changed |= dest.newtypes.insert(external, info.clone()).is_none();
    }
    if let Some(tree) = source.build_fragments.get(imported) {
        // Compiler-private descriptor fragments ride re-export chains so a
        // barrel's consumers splice the declaring module's own namespace
        // (dsc#52). Invisible to ordinary import validation.
        changed |= dest.build_fragments.insert(external, tree.clone()).is_none();
    }
    for ((receiver, method), info) in &source.receiver_methods {
        if *receiver == imported {
            changed |= dest
                .receiver_methods
                .insert((external, *method), info.clone())
                .is_none();
        }
    }
    changed
}

fn diag(line: usize, column: usize, message: String) -> Diagnostic {
    deka_syntax::Diagnostic::error(line, column, message)
}

/// Compute a topological ordering of the discovered modules.  Returns the
/// first cycle found if the graph is not a DAG.
fn topological_order(
    modules: &HashMap<PathBuf, GraphModule>,
) -> Result<Vec<PathBuf>, Vec<PathBuf>> {
    let mut in_degree: HashMap<PathBuf, usize> = modules.keys().map(|p| (p.clone(), 0)).collect();
    let mut adj: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

    for (path, module) in modules.iter() {
        for dep in module.dependencies.values() {
            if let Some(dep_module) = modules.get(dep) {
                // Only count edges to modules that were successfully loaded.
                *in_degree.entry(dep_module.path.clone()).or_insert(0) += 1;
                adj.entry(path.clone()).or_default().push(dep.clone());
            }
        }
    }

    let mut queue: VecDeque<PathBuf> = in_degree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(p, _)| p.clone())
        .collect();
    let mut order: Vec<PathBuf> = Vec::with_capacity(modules.len());

    while let Some(path) = queue.pop_front() {
        order.push(path.clone());
        for dep in adj.get(&path).into_iter().flatten() {
            let deg = in_degree.get_mut(dep).expect("in-degree entry");
            *deg -= 1;
            if *deg == 0 {
                queue.push_back(dep.clone());
            }
        }
    }

    if order.len() != modules.len() {
        // Return one cycle using DFS.
        return Err(find_cycle(modules));
    }

    Ok(order)
}

fn find_cycle(modules: &HashMap<PathBuf, GraphModule>) -> Vec<PathBuf> {
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut stack: Vec<PathBuf> = Vec::new();
    let mut on_stack: HashSet<PathBuf> = HashSet::new();

    for start in modules.keys() {
        if visited.contains(start) {
            continue;
        }
        if let Some(cycle) = dfs_cycle(start, modules, &mut visited, &mut stack, &mut on_stack) {
            return cycle;
        }
    }

    Vec::new()
}

fn dfs_cycle(
    node: &PathBuf,
    modules: &HashMap<PathBuf, GraphModule>,
    visited: &mut HashSet<PathBuf>,
    stack: &mut Vec<PathBuf>,
    on_stack: &mut HashSet<PathBuf>,
) -> Option<Vec<PathBuf>> {
    visited.insert(node.clone());
    stack.push(node.clone());
    on_stack.insert(node.clone());

    for dep in modules.get(node)?.dependencies.values() {
        if !modules.contains_key(dep) {
            continue;
        }
        if !visited.contains(dep) {
            if let Some(cycle) = dfs_cycle(dep, modules, visited, stack, on_stack) {
                return Some(cycle);
            }
        } else if on_stack.contains(dep) {
            // Found cycle; slice from dep to end of stack.
            let idx = stack.iter().position(|p| p == dep).unwrap_or(0);
            let mut cycle = stack[idx..].to_vec();
            cycle.push(dep.clone());
            return Some(cycle);
        }
    }

    stack.pop();
    on_stack.remove(node);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct InMemoryLoader {
        files: HashMap<PathBuf, String>,
        aliases: HashMap<(PathBuf, String), PathBuf>,
    }

    impl ModuleLoader for InMemoryLoader {
        fn resolve(&self, specifier: &str, referrer: &Path) -> Result<PathBuf, String> {
            let key = (referrer.to_path_buf(), specifier.to_string());
            self.aliases.get(&key).cloned().ok_or_else(|| {
                format!(
                    "unmapped specifier {} from {}",
                    specifier,
                    referrer.display()
                )
            })
        }

        fn load(&self, path: &Path) -> Result<String, String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| format!("missing in-memory file {}", path.display()))
        }
    }

    #[test]
    fn graph_compiles_relative_imports() {
        let root = PathBuf::from("/project");
        let math = root.join("math.ds");
        let main = root.join("main.ds");

        let mut files = HashMap::new();
        files.insert(
            math.clone(),
            "export fn add(a: number, b: number) number { return a + b; }".to_string(),
        );
        files.insert(
            main.clone(),
            "import { add } from \"./math.ds\";\nconst r: number = add(1, 2);".to_string(),
        );

        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./math.ds".to_string()), math.clone());

        let loader = InMemoryLoader { files, aliases };
        let result = compile_module_graph(&main, &loader).expect("compile graph");
        assert_eq!(result.modules.len(), 2);
        assert!(result.modules[&main].contains("add(1, 2)"));
        assert!(result.modules[&math].contains("function add"));
    }

    #[test]
    fn graph_resolves_imported_symbol_through_barrel() {
        let root = PathBuf::from("/project");
        let a = root.join("a.ds");
        let b = root.join("b.ds");
        let c = root.join("c.ds");
        let mut files = HashMap::new();
        files.insert(
            a.clone(),
            "export fn validate(json: string) string { return json; }".to_string(),
        );
        files.insert(
            b.clone(),
            "import { validate } from \"./a.ds\"; export { validate };".to_string(),
        );
        files.insert(
            c.clone(),
            "import { validate } from \"./b.ds\"; const result: string = validate(\"{}\");"
                .to_string(),
        );
        let mut aliases = HashMap::new();
        aliases.insert((c.clone(), "./b.ds".to_string()), b.clone());
        aliases.insert((b.clone(), "./a.ds".to_string()), a.clone());
        let result =
            compile_module_graph(&c, &InMemoryLoader { files, aliases }).expect("barrel compiles");
        assert_eq!(result.modules.len(), 3);
    }

    /// Regression for the testsuite fixture `cross-module-struct-identity-001`:
    /// a module that calls `.getType()` on a value RETURNED from another
    /// module declares no struct itself, but its per-module prelude must
    /// still carry the struct brand branch of `__deka_type_of` — values cross
    /// module boundaries without their type binding being imported, so the
    /// program-level `declares_*` bits are overlaid onto every module prelude
    /// (deka#595). Without the overlay the consumer's `__deka_type_of` falls
    /// through to "object" and cross-module reflection silently lies.
    #[test]
    fn module_prelude_brand_branches_follow_program_declarations() {
        let root = PathBuf::from("/project");
        let types = root.join("types.ds");
        let factory = root.join("factory.ds");
        let main = root.join("main.ds");
        let mut files = HashMap::new();
        files.insert(
            types.clone(),
            "struct Person { name: string }\nexport { Person }".to_string(),
        );
        files.insert(
            factory.clone(),
            "import { Person } from \"./types.ds\";\nfn make_person(name: string) Person { return Person { name: name }; }\nexport { make_person }".to_string(),
        );
        files.insert(
            main.clone(),
            "import { Person } from \"./types.ds\";\nimport { make_person } from \"./factory.ds\";\nconst p: Person = make_person(\"Deka\");\nconst t = p.getType().toString();".to_string(),
        );
        let mut aliases = HashMap::new();
        aliases.insert((factory.clone(), "./types.ds".to_string()), types.clone());
        aliases.insert((main.clone(), "./factory.ds".to_string()), factory.clone());
        aliases.insert((main.clone(), "./types.ds".to_string()), types.clone());
        let result =
            compile_module_graph(&main, &InMemoryLoader { files, aliases }).expect("compile graph");

        // The call site must rewrite through the typechecker's type_of_calls
        // set — an unannotated binding does not record it, so this assertion
        // pins the fixture's exact shape (annotated import) against drift.
        assert!(
            result.modules[&main].contains("__deka_type_of(p)"),
            "getType() call site was not rewritten:\n{}",
            result.modules[&main]
        );
        // The consuming module declares no struct, yet its own prelude must
        // brand-check structs: make_person's return value crosses the
        // boundary with only the brand to identify it.
        let main_prelude = &result.module_preludes[&main];
        assert!(
            main_prelude.contains("v.__deka_struct"),
            "consuming module's prelude is missing the struct brand branch:\n{main_prelude}"
        );
        // Sanity: main's prelude carries `__deka_type_of` at all (it has the
        // call site) and the program prelude agrees.
        assert!(main_prelude.contains("function __deka_type_of"));
        assert!(result.prelude.contains("v.__deka_struct"));

        // A program that declares no structs anywhere keeps the branch out of
        // every module prelude — the overlay must not become unconditional.
        let math = root.join("math.ds");
        let app = root.join("app.ds");
        let mut files = HashMap::new();
        files.insert(
            math.clone(),
            "export fn double(n: number) number { return n + n; }".to_string(),
        );
        files.insert(
            app.clone(),
            "import { double } from \"./math.ds\";\nconst t = double(2).getType().toString();"
                .to_string(),
        );
        let mut aliases = HashMap::new();
        aliases.insert((app.clone(), "./math.ds".to_string()), math.clone());
        let result =
            compile_module_graph(&app, &InMemoryLoader { files, aliases }).expect("compile graph");
        assert!(
            !result.module_preludes[&app].contains("v.__deka_struct"),
            "struct-free program emitted the struct brand branch:\n{}",
            result.module_preludes[&app]
        );
    }

    #[test]
    fn graph_resolves_direct_reexport_through_barrel() {
        let root = PathBuf::from("/project");
        let a = root.join("a.ds");
        let b = root.join("b.ds");
        let c = root.join("c.ds");
        let mut files = HashMap::new();
        files.insert(
            a.clone(),
            "export fn validate(json: string) string { return json; }".to_string(),
        );
        files.insert(
            b.clone(),
            "export { validate } from \"./a.ds\";".to_string(),
        );
        files.insert(
            c.clone(),
            "import { validate } from \"./b.ds\"; const result: string = validate(\"{}\");"
                .to_string(),
        );
        let mut aliases = HashMap::new();
        aliases.insert((c.clone(), "./b.ds".to_string()), b.clone());
        aliases.insert((b.clone(), "./a.ds".to_string()), a.clone());
        let result = compile_module_graph(&c, &InMemoryLoader { files, aliases })
            .expect("direct reexport compiles");
        assert_eq!(result.modules.len(), 3);
    }

    #[test]
    fn graph_fails_naming_module_that_fails_to_parse() {
        // deka#567: a module that fails to parse must fail the whole graph
        // with its own path-prefixed diagnostic, and the importer must not
        // emit its downstream `<infer>` cascade — the pre-fix output was
        // three errors about the importer's own code ("`Ok` is not a case
        // of type `<infer>`", "unknown identifier `v`", ...) before the one
        // line that named the real cause.
        let root = PathBuf::from("/project");
        let broken = root.join("broken.ds");
        let main = root.join("main.ds");

        let mut files = HashMap::new();
        files.insert(
            broken.clone(),
            "export fn helper() string { return \"x\" }\nconst = 5".to_string(),
        );
        files.insert(
            main.clone(),
            "import { helper } from \"./broken.ds\";\nconst r = match (helper()) { Ok(v) => v, Err(e) => \"\" };"
                .to_string(),
        );

        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./broken.ds".to_string()), broken.clone());

        let err = compile_module_graph(&main, &InMemoryLoader { files, aliases })
            .expect_err("graph must fail when an imported module fails to parse");
        let messages: Vec<String> = err.iter().map(|d| d.message.clone()).collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("broken.ds") && m.contains("expected identifier")),
            "diagnostics must name the broken module and carry its parse error: {messages:?}"
        );
        assert!(
            !messages.iter().any(|m| m.contains("<infer>")),
            "downstream <infer> cascade must be suppressed, not reported alongside: {messages:?}"
        );
        assert!(
            !messages.iter().any(|m| m.contains("main.ds")),
            "no diagnostics may blame the importer for the module's parse failure: {messages:?}"
        );
    }

    #[test]
    fn graph_with_module_base_leaves_documented_stdlib_imports_virtual() {
        let root = PathBuf::from("/project");
        let math = root.join("math.ds");
        let main = root.join("main.ds");

        let mut files = HashMap::new();
        files.insert(
            math.clone(),
            "export fn add(a: number, b: number) number { return a + b; }".to_string(),
        );
        files.insert(
            main.clone(),
            "import { echo } from \"io\";\nimport { add } from \"./math.ds\";\necho(add(1, 2));"
                .to_string(),
        );

        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./math.ds".to_string()), math.clone());

        let loader = InMemoryLoader { files, aliases };
        // Without a module base, the loader is asked to resolve `io`.
        let err = compile_module_graph(&main, &loader).expect_err("io is not a .ds module");
        assert!(
            err.iter()
                .any(|d| d.message.contains("cannot resolve 'io'")),
            "got: {:?}",
            err
        );

        let loader = InMemoryLoader {
            files: loader.files,
            aliases: loader.aliases,
        };
        let result = compile_module_graph_with_options(
            &main,
            &loader,
            GraphCompileOptions {
                client: false,
                module_base: Some("https://hats.dump.invalid/modules".to_string()),
                module_root: None,
            },
        )
        .expect("virtual stdlib imports remain available pending dsc#129");
        let main_js = &result.modules[&main];
        assert!(
            main_js.contains("import { echo } from \"https://hats.dump.invalid/modules/io.mjs\";"),
            "got: {}",
            main_js
        );
        assert!(main_js.contains("import { add } from \"./math.ds\";"), "got: {}", main_js);
    }

    #[test]
    fn graph_reports_missing_export() {
        let root = PathBuf::from("/project");
        let math = root.join("math.ds");
        let main = root.join("main.ds");

        let mut files = HashMap::new();
        files.insert(
            math.clone(),
            "export fn add(a: number, b: number) number { return a + b; }".to_string(),
        );
        files.insert(
            main.clone(),
            "import { subtract } from \"./math.ds\";\nconst r: number = subtract(1, 2);"
                .to_string(),
        );

        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./math.ds".to_string()), math.clone());

        let loader = InMemoryLoader { files, aliases };
        let err = compile_module_graph(&main, &loader).expect_err("missing export should fail");
        assert!(
            err.iter().any(|d| d
                .message
                .contains("Missing export 'subtract' in './math.ds'")),
            "got: {:?}",
            err
        );
    }

    #[test]
    fn graph_rejects_cycles() {
        let root = PathBuf::from("/project");
        let a = root.join("a.ds");
        let b = root.join("b.ds");

        let mut files = HashMap::new();
        files.insert(a.clone(), "import { x } from \"./b.ds\";".to_string());
        files.insert(b.clone(), "import { x } from \"./a.ds\";".to_string());

        let mut aliases = HashMap::new();
        aliases.insert((a.clone(), "./b.ds".to_string()), b.clone());
        aliases.insert((b.clone(), "./a.ds".to_string()), a.clone());

        let loader = InMemoryLoader { files, aliases };
        let err = compile_module_graph(&a, &loader).expect_err("cycle should fail");
        assert!(err.iter().any(|d| d.message.contains("cycle")));
    }

    #[test]
    fn graph_compiles_cross_module_struct() {
        let root = PathBuf::from("/project");
        let person = root.join("person.ds");
        let main = root.join("main.ds");

        let mut files = HashMap::new();
        files.insert(
            person.clone(),
            "struct Person { name: string }\nfn (p Person) greet() string { return p.name }\nexport { Person }".to_string(),
        );
        files.insert(
            main.clone(),
            "import { Person } from \"./person.ds\";\nconst p = Person { name: \"Deka\" };\nconst g: string = p.greet();".to_string(),
        );

        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./person.ds".to_string()), person.clone());

        let loader = InMemoryLoader { files, aliases };
        let result = compile_module_graph(&main, &loader).expect("compile graph");
        assert_eq!(result.modules.len(), 2);
        let person_js = &result.modules[&person];
        let main_js = &result.modules[&main];
        assert!(
            person_js.contains("const Person = __deka_struct(\"Person\")"),
            "got: {}",
            person_js
        );
        assert!(
            person_js.contains("Person.impl(\"greet\""),
            "got: {}",
            person_js
        );
        assert!(
            person_js.contains("export { Person };"),
            "got: {}",
            person_js
        );
        assert!(
            main_js.contains("import { Person } from \"./person.ds\";"),
            "got: {}",
            main_js
        );
        assert!(
            main_js.contains("Person({ name: \"Deka\" })"),
            "got: {}",
            main_js
        );
        assert!(main_js.contains("p.greet()"), "got: {}", main_js);
    }

    /// deka#595: the graph synthesizes the shared prelude ONCE for the whole
    /// program, from the union of per-module demand at member granularity.
    /// Module bodies are emitted without it (bundle consumers prepend the
    /// program prelude); separate-file hosts use `self_contained_modules`.
    #[test]
    fn graph_prelude_synthesized_once_from_union_demand() {
        let root = PathBuf::from("/project");
        let point = root.join("point.ds");
        let size = root.join("size.ds");
        let main = root.join("main.ds");

        let mut files = HashMap::new();
        files.insert(
            point.clone(),
            "struct Point { x: number }\nexport { Point }".to_string(),
        );
        files.insert(
            size.clone(),
            "struct Size { w: number }\nexport { Size }".to_string(),
        );
        files.insert(
            main.clone(),
            "import { Point } from \"./point.ds\";\nimport { Size } from \"./size.ds\";\nconst p = Point { x: 1 };\nconst s = Size { w: 2 };\nconst t = p.x + s.w;".to_string(),
        );

        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./point.ds".to_string()), point.clone());
        aliases.insert((main.clone(), "./size.ds".to_string()), size.clone());

        let loader = InMemoryLoader { files, aliases };
        let result = compile_module_graph(&main, &loader).expect("compile graph");

        // One program prelude: one factory helper for both modules' structs,
        // and no impl/implMut/embeds machinery — nobody declared any.
        assert_eq!(
            result.prelude.matches("function __deka_struct").count(),
            1,
            "program prelude must contain exactly one factory helper:\n{}",
            result.prelude
        );
        assert!(
            !result.prelude.contains("implMut"),
            "program prelude has implMut:\n{}",
            result.prelude
        );
        assert!(
            !result.prelude.contains("f.impl="),
            "program prelude has impl:\n{}",
            result.prelude
        );
        assert!(
            !result.prelude.contains("Object.entries(embeds)"),
            "program prelude has the embeds loop:\n{}",
            result.prelude
        );
        // Neither module body carries its own copy.
        for path in [&point, &size, &main] {
            assert!(
                !result.modules[path].contains("function __deka_struct"),
                "{} still inlines the helper:\n{}",
                path.display(),
                result.modules[path]
            );
        }
        // Separate-file hosts get self-contained modules again.
        let self_contained = result.self_contained_modules();
        assert!(
            self_contained[&point].contains("function __deka_struct"),
            "self-contained point module lost the helper:\n{}",
            self_contained[&point]
        );
        assert!(
            self_contained[&main].contains("const p = Point({ x: 1 })"),
            "self-contained main module lost its body:\n{}",
            self_contained[&main]
        );
    }

    /// deka#595 member granularity across the union: a mutable method in one
    /// module forces `implMut` + `MutationError` into the program prelude for
    /// everyone, exactly once.
    #[test]
    fn graph_prelude_union_includes_demanded_members() {
        let root = PathBuf::from("/project");
        let counter = root.join("counter.ds");
        let main = root.join("main.ds");

        let mut files = HashMap::new();
        files.insert(
            counter.clone(),
            "struct Counter { n: number }\nfn (c mut Counter) bump() number { return c.n; }\nexport { Counter }".to_string(),
        );
        files.insert(
            main.clone(),
            "import { Counter } from \"./counter.ds\";\nlet c = Counter { n: 1 };\nc.bump();"
                .to_string(),
        );

        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./counter.ds".to_string()), counter.clone());

        let loader = InMemoryLoader { files, aliases };
        let result = compile_module_graph(&main, &loader).expect("compile graph");

        assert_eq!(
            result.prelude.matches("function __deka_struct").count(),
            1,
            "got:\n{}",
            result.prelude
        );
        assert!(
            result.prelude.contains("f.implMut="),
            "unioned demand lost implMut:\n{}",
            result.prelude
        );
        assert!(
            result.prelude.contains("MutationError"),
            "implMut without its MutationError class:\n{}",
            result.prelude
        );
    }

    #[test]
    fn graph_compiles_cross_module_enum() {
        let root = PathBuf::from("/project");
        let color = root.join("color.ds");
        let main = root.join("main.ds");

        let mut files = HashMap::new();
        files.insert(
            color.clone(),
            "enum Color { Red, Green, Blue }\nexport { Color }".to_string(),
        );
        files.insert(
            main.clone(),
            "import { Color } from \"./color.ds\";\nconst c: Color = Color.Red;\nconst label: string = match c { Red => \"red\", _ => \"other\" };".to_string(),
        );

        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./color.ds".to_string()), color.clone());

        let loader = InMemoryLoader { files, aliases };
        let result = compile_module_graph(&main, &loader).expect("compile graph");
        assert_eq!(result.modules.len(), 2);
        let color_js = &result.modules[&color];
        let main_js = &result.modules[&main];
        assert!(
            color_js.contains("const Color = Object.freeze"),
            "got: {}",
            color_js
        );
        assert!(color_js.contains("export { Color };"), "got: {}", color_js);
        assert!(
            main_js.contains("import { Color } from \"./color.ds\";"),
            "got: {}",
            main_js
        );
        assert!(main_js.contains("Color.Red"), "got: {}", main_js);
        assert!(
            main_js.contains("__deka_match_scrutinee_"),
            "got: {}",
            main_js
        );
    }

    #[test]
    fn graph_compiles_cross_module_newtype_method() {
        let root = PathBuf::from("/project");
        let money = root.join("money.ds");
        let main = root.join("main.ds");

        let mut files = HashMap::new();
        files.insert(
            money.clone(),
            "type Cents number\nfn (c Cents) toDollars() number { return unboxNumber(c) / 100 }\nexport { Cents }".to_string(),
        );
        files.insert(
            main.clone(),
            "import { Cents } from \"./money.ds\";\nconst c: Cents = Cents(500);\nconst d: number = c.toDollars();".to_string(),
        );

        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./money.ds".to_string()), money.clone());

        let loader = InMemoryLoader { files, aliases };
        let result = compile_module_graph(&main, &loader).expect("compile graph");
        assert_eq!(result.modules.len(), 2);
        let money_js = &result.modules[&money];
        let main_js = &result.modules[&main];
        assert!(money_js.contains("const Cents$proto"), "got: {}", money_js);
        assert!(
            money_js.contains("Cents$proto.toDollars = function()"),
            "got: {}",
            money_js
        );
        assert!(money_js.contains("export { Cents };"), "got: {}", money_js);
        assert!(
            main_js.contains("import { Cents } from \"./money.ds\";"),
            "got: {}",
            main_js
        );
        assert!(main_js.contains("Cents(500)"), "got: {}", main_js);
        assert!(main_js.contains("c.toDollars()"), "got: {}", main_js);
    }

    #[test]
    fn graph_drops_unused_export_from_pure_module() {
        let root = PathBuf::from("/project");
        let lib = root.join("lib.ds");
        let main = root.join("main.ds");

        let mut files = HashMap::new();
        files.insert(
            lib.clone(),
            "export fn keep() { return \"KEEP_ME\"; }\nexport fn drop() { return \"DROP_ME_UNIQUE\"; }".to_string(),
        );
        files.insert(
            main.clone(),
            "import { keep } from \"./lib.ds\";\nconst x: string = keep();".to_string(),
        );

        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./lib.ds".to_string()), lib.clone());

        let loader = InMemoryLoader { files, aliases };
        let result = compile_module_graph(&main, &loader).expect("compile graph");
        assert_eq!(result.modules.len(), 2);
        let lib_js = &result.modules[&lib];
        assert!(lib_js.contains("KEEP_ME"), "got: {}", lib_js);
        assert!(
            !lib_js.contains("DROP_ME_UNIQUE"),
            "unused export should be shaken: {}",
            lib_js
        );
    }

    #[test]
    fn graph_keeps_impure_module_side_effect() {
        let root = PathBuf::from("/project");
        let logger = root.join("logger.ds");
        let main = root.join("main.ds");

        let mut files = HashMap::new();
        files.insert(
            logger.clone(),
            "const marker: string = \"SIDE_EFFECT_UNIQUE\";".to_string(),
        );
        files.insert(
            main.clone(),
            "import \"./logger.ds\";\nconst x: number = 1;".to_string(),
        );

        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./logger.ds".to_string()), logger.clone());

        let loader = InMemoryLoader { files, aliases };
        let result = compile_module_graph(&main, &loader).expect("compile graph");
        assert_eq!(result.modules.len(), 2);
        let logger_js = &result.modules[&logger];
        assert!(
            logger_js.contains("SIDE_EFFECT_UNIQUE"),
            "impure module must be kept: {}",
            logger_js
        );
    }

    #[test]
    fn graph_treats_side_effect_css_imports_as_virtual() {
        // `import "./x.css"` authors component CSS: emit drops it and the
        // per-route CSS collector rewrites its selectors (RFD 24 §10.6), so
        // resolution must not demand a loadable JS module for it.
        let root = PathBuf::from("/project");
        let main = root.join("page.dsx");

        let mut files = HashMap::new();
        files.insert(
            main.clone(),
            "import \"./page.css\";\nconst x: number = 1;".to_string(),
        );

        let loader = InMemoryLoader {
            files,
            aliases: HashMap::new(),
        };
        let result = compile_module_graph(&main, &loader).expect("compile graph");
        assert_eq!(result.modules.len(), 1);
        assert!(
            !result.modules[&main].contains("page.css"),
            "css imports must not reach the JS output: {}",
            result.modules[&main]
        );
    }

    #[test]
    fn graph_aggregates_dev_plan_without_runtime_dev_imports() {
        let main = PathBuf::from("/project/main.ds");
        let dev_data = PathBuf::from("/project/dev-data.ds");
        let mut files = HashMap::new();
        files.insert(
            main.clone(),
            "import { load } from \"./dev-data.ds\";\nconst labels: Array<string> = build { return Ok(load()) }"
                .to_string(),
        );
        files.insert(
            dev_data.clone(),
            "export fn load() Array<string> { return [\"Ada\"] }".to_string(),
        );
        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./dev-data.ds".to_string()), dev_data);
        let result = compile_module_graph(&main, &InMemoryLoader { files, aliases })
            .expect("graph compiles");
        assert_eq!(result.dev_plan.version, 2);
        assert_eq!(result.dev_plan.prerender, None);
        assert_eq!(result.dev_plan.slots.len(), 1);
        assert!(
            !result.modules[&main].contains("./dev-data.ds"),
            "{}",
            result.modules[&main]
        );
        assert!(
            result.dev_plan.slots[0].entry.contains("./dev-data.js"),
            "{}",
            result.dev_plan.slots[0].entry
        );
    }

    #[test]
    fn graph_reports_entry_prerender_only() {
        let main = PathBuf::from("/project/page.ds");
        let helper = PathBuf::from("/project/helper.ds");
        let mut files = HashMap::new();
        files.insert(
            main.clone(),
            "import { help } from \"./helper.ds\"\n\
             export const prerender = false\n\
             export fn Page() string { return help() }"
                .to_string(),
        );
        // An imported module may export its own `prerender`; it is not a
        // route fact for the entry and must not leak into the plan.
        files.insert(
            helper.clone(),
            "export const prerender = true\nexport fn help() string { return \"hi\" }"
                .to_string(),
        );
        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./helper.ds".to_string()), helper);
        let result = compile_module_graph(&main, &InMemoryLoader { files, aliases })
            .expect("graph compiles");
        assert_eq!(result.dev_plan.version, 2);
        assert_eq!(result.dev_plan.prerender, Some(false));
    }

    #[test]
    fn graph_keeps_build_factories_and_receiver_methods_live() {
        let main = PathBuf::from("/project/page.ds");
        let mut files = HashMap::new();
        files.insert(
            main.clone(),
            "struct User { name: string }\n\
             fn (user User) greet() string { return \"Hello \" + user.name }\n\
             const user: User = build { return Ok(User { name: \"Ada\" }) }\n\
             export fn Page() string { return user.greet() }"
                .to_string(),
        );
        let result = compile_module_graph(
            &main,
            &InMemoryLoader {
                files,
                aliases: HashMap::new(),
            },
        )
        .expect("graph compiles");
        let emitted = &result.modules[&main];
        assert!(emitted.contains("User.impl(\"greet\""), "{emitted}");
        assert!(emitted.contains("const user = __deka_build_"), "{emitted}");
        assert!(emitted.contains("({User});"), "{emitted}");
        assert!(
            emitted
                .find("User.impl(\"greet\"")
                .zip(emitted.find("const user = __deka_build_"))
                .is_some_and(|(method, binding)| method < binding),
            "factory hydration must follow receiver-method registration:\n{emitted}"
        );
        assert!(
            result.prelude.contains("f.impl=(a,b)=>"),
            "the detached prelude must include struct method support:\n{}",
            result.prelude
        );
    }

    #[test]
    fn graph_keeps_imported_factory_used_only_by_build_hydration() {
        let types = PathBuf::from("/project/types.ds");
        let page = PathBuf::from("/project/page.ds");
        let mut files = HashMap::new();
        files.insert(
            types.clone(),
            "struct User { name: string }\n\
             fn (user User) greet() string { return \"Hello \" + user.name }\n\
             export { User }"
                .to_string(),
        );
        files.insert(
            page.clone(),
            "import { User } from \"./types.ds\";\n\
             const user: User = build { return Ok(User { name: \"Ada\" }) }\n\
             export fn Page() string { return user.greet() }"
                .to_string(),
        );
        let mut aliases = HashMap::new();
        aliases.insert((page.clone(), "./types.ds".to_string()), types.clone());
        let result = compile_module_graph(&page, &InMemoryLoader { files, aliases })
            .expect("graph compiles");
        let page_js = &result.modules[&page];
        assert!(
            page_js.contains("import { User } from \"./types.ds\";"),
            "factory import must stay live for build hydration:\n{page_js}"
        );
        assert!(page_js.contains("({User});"), "{page_js}");
        assert!(
            result.modules[&types].contains("User.impl(\"greet\""),
            "exported factory must retain receiver methods:\n{}",
            result.modules[&types]
        );
    }

    #[test]
    fn graph_build_closure_exposes_private_nested_struct_with_methods() {
        let types = PathBuf::from("/project/types.ds");
        let page = PathBuf::from("/project/page.ds");
        let mut files = HashMap::new();
        files.insert(
            types.clone(),
            "struct Profile { bio: string }\n\
             fn (profile Profile) greet() string { return \"Hello \" + profile.bio }\n\
             struct User { name: string; profile: Profile }\n\
             export { User }\n\
             export fn ada() User { return User { name: \"Ada\", profile: Profile { bio: \"engineer\" } } }"
                .to_string(),
        );
        files.insert(
            page.clone(),
            "import { User, ada } from \"./types.ds\";\n\
             const user: User = build { return Ok(ada()) }\n\
             export fn Page() string { return user.profile.greet() }"
                .to_string(),
        );
        let mut aliases = HashMap::new();
        aliases.insert((page.clone(), "./types.ds".to_string()), types.clone());
        let result = compile_module_graph(&page, &InMemoryLoader { files, aliases })
            .expect("graph compiles");
        let types_js = &result.modules[&types];
        let page_js = &result.modules[&page];
        assert!(
            types_js.contains("const __deka_factories = () => ({Profile, User});"),
            "the declaring module must close over its private factories:\n{types_js}"
        );
        assert!(
            !types_js.contains("export { Profile"),
            "the private struct must not leak into module exports:\n{types_js}"
        );
        assert!(
            types_js
                .find("Profile.impl(\"greet\"")
                .zip(types_js.find("const __deka_factories"))
                .is_some_and(|(method, closure)| method < closure),
            "receiver methods must register before the closure captures the factory:\n{types_js}"
        );
        assert!(
            page_js.contains(
                "import { __deka_factories as __deka_factories_0 } from \"./types.ds\";"
            ),
            "the consumer must import the declaring module's factory closure:\n{page_js}"
        );
        assert!(
            page_js.contains("{...__deka_factories_0(), User})"),
            "build hydration must splice the closure fragments:\n{page_js}"
        );
        assert!(page_js.contains("user.profile.greet()"), "{page_js}");
        assert!(
            result.dev_plan.slots[0]
                .descriptor
                .to_string()
                .contains("\"name\":\"Profile\",\"node\":\"struct\""),
            "the build descriptor must carry the private struct node:\n{}",
            result.dev_plan.slots[0].descriptor
        );
    }

    #[test]
    fn graph_build_closure_exposes_private_newtype_and_enum() {
        let types = PathBuf::from("/project/types.ds");
        let page = PathBuf::from("/project/page.ds");
        let mut files = HashMap::new();
        files.insert(
            types.clone(),
            "type Cents number\n\
             fn (c Cents) tag() string { return \"cents\" }\n\
             enum Status { Active, Inactive }\n\
             struct Invoice { total: Cents; status: Status }\n\
             export { Invoice }\n\
             export fn inv() Invoice { return Invoice { total: Cents(5), status: Status.Active } }"
                .to_string(),
        );
        files.insert(
            page.clone(),
            "import { Invoice, inv } from \"./types.ds\";\n\
             const invoice: Invoice = build { return Ok(inv()) }\n\
             export fn Page() string { return invoice.total.tag() }"
                .to_string(),
        );
        let mut aliases = HashMap::new();
        aliases.insert((page.clone(), "./types.ds".to_string()), types.clone());
        let result = compile_module_graph(&page, &InMemoryLoader { files, aliases })
            .expect("graph compiles");
        let types_js = &result.modules[&types];
        let page_js = &result.modules[&page];
        let closure_names = types_js
            .split("const __deka_factories = () => (")
            .nth(1)
            .and_then(|rest| rest.split("})").next())
            .expect("factory closure must be emitted");
        for name in ["Cents", "Invoice", "Status"] {
            assert!(
                closure_names.contains(name),
                "factory closure must capture {name}:\n{types_js}"
            );
        }
        assert!(
            types_js.contains("Cents$proto.tag = function("),
            "the private newtype must retain receiver methods:\n{types_js}"
        );
        assert!(
            page_js.contains("{...__deka_factories_0(), Invoice})"),
            "build hydration must splice the closure fragments:\n{page_js}"
        );
        let descriptor = result.dev_plan.slots[0].descriptor.to_string();
        assert!(
            descriptor.contains("\"name\":\"Cents\",\"node\":\"newtype\""),
            "the build descriptor must carry the private newtype node:\n{descriptor}"
        );
        assert!(
            descriptor.contains("\"name\":\"Status\",\"node\":\"enum\""),
            "the build descriptor must carry the private enum node:\n{descriptor}"
        );
    }

    #[test]
    fn graph_shakes_build_closure_when_build_binding_is_dead() {
        // The build binding lives in a non-entry module: entry modules seed
        // every top-level statement as live (shake.rs), so only a dead
        // binding in a dependency actually shakes away.
        let types = PathBuf::from("/project/types.ds");
        let page = PathBuf::from("/project/page.ds");
        let main = PathBuf::from("/project/main.ds");
        let mut files = HashMap::new();
        files.insert(
            types.clone(),
            "struct Profile { bio: string }\n\
             struct User { name: string; profile: Profile }\n\
             export { User }\n\
             export fn ada() User { return User { name: \"Ada\", profile: Profile { bio: \"engineer\" } } }"
                .to_string(),
        );
        files.insert(
            page.clone(),
            "import { User, ada } from \"./types.ds\";\n\
             const user: User = build { return Ok(ada()) }\n\
             export fn Page() string { return ada().name }"
                .to_string(),
        );
        files.insert(
            main.clone(),
            "import { Page } from \"./page.ds\";\n\
             export fn Main() string { return Page() }"
                .to_string(),
        );
        let mut aliases = HashMap::new();
        aliases.insert((page.clone(), "./types.ds".to_string()), types.clone());
        aliases.insert((main.clone(), "./page.ds".to_string()), page.clone());
        let result = compile_module_graph(&main, &InMemoryLoader { files, aliases })
            .expect("graph compiles");
        let types_js = &result.modules[&types];
        let page_js = &result.modules[&page];
        assert!(
            !page_js.contains("__deka_build"),
            "a dead build binding must not hydrate:\n{page_js}"
        );
        assert!(
            !page_js.contains("__deka_factories"),
            "a dead build binding must not import the closure:\n{page_js}"
        );
        assert!(
            !types_js.contains("__deka_factories"),
            "no live consumer build means no factory closure:\n{types_js}"
        );
    }

    #[test]
    fn graph_emits_aliased_imported_struct_literal() {
        let types = PathBuf::from("/project/types.ds");
        let page = PathBuf::from("/project/page.ds");
        let mut files = HashMap::new();
        files.insert(
            types.clone(),
            "struct User { name: string }\n\
             fn (user User) greet() string { return \"Hello \" + user.name }\n\
             export { User }"
                .to_string(),
        );
        files.insert(
            page.clone(),
            "import { User as Person } from \"./types.ds\";\n\
             const person: Person = build { return Ok(Person { name: \"Ada\" }) }\n\
             export fn Page() string { return person.greet() }"
                .to_string(),
        );
        let mut aliases = HashMap::new();
        aliases.insert((page.clone(), "./types.ds".to_string()), types.clone());
        let result = compile_module_graph(&page, &InMemoryLoader { files, aliases })
            .expect("graph compiles");
        let page_js = &result.modules[&page];
        assert!(
            page_js.contains("import { User as Person } from \"./types.ds\";"),
            "aliased factory import must stay live:\n{page_js}"
        );
        assert!(
            page_js.contains("({Person});"),
            "hydration map must bind the descriptor name through the alias:\n{page_js}"
        );
        assert!(
            page_js.contains("person.greet()"),
            "receiver call must emit through the alias:\n{page_js}"
        );
        let types_js = &result.modules[&types];
        assert!(
            types_js.contains("User.impl(\"greet\""),
            "receiver methods stay registered on the exported factory:\n{types_js}"
        );
        let slot = result
            .dev_plan
            .slots
            .iter()
            .find(|slot| slot.binding == "person")
            .expect("one build slot for person");
        assert!(
            slot.entry.contains("Person({"),
            "aliased struct literal must emit through the local binding:\n{}",
            slot.entry
        );
    }



    #[test]
    fn graph_keeps_dev_slots_reachable_only_through_a_dev_body() {
        let main = PathBuf::from("/project/main.ds");
        let dev_data = PathBuf::from("/project/dev-data.ds");
        let mut files = HashMap::new();
        files.insert(
            main.clone(),
            "import { labels } from \"./dev-data.ds\";\nconst page_labels: Array<string> = build { return Ok(labels) }"
                .to_string(),
        );
        files.insert(
            dev_data.clone(),
            "const labels: Array<string> = build { return Ok([\"Ada\"]) };\nexport { labels };"
                .to_string(),
        );
        let mut aliases = HashMap::new();
        aliases.insert(
            (main.clone(), "./dev-data.ds".to_string()),
            dev_data.clone(),
        );
        let result = compile_module_graph(&main, &InMemoryLoader { files, aliases })
            .expect("graph compiles");
        assert_eq!(result.dev_plan.slots.len(), 2);
        assert!(
            !result.modules.contains_key(&dev_data),
            "dev-only dependency leaked into runtime modules"
        );
    }

    #[test]
    fn graph_dev_entry_emits_helper_referenced_only_inside_unsafe() {
        // dsc#59: a helper reachable only from inside an `unsafe` arrow body
        // in a build body was dropped from the dev plan entry, so executing
        // the entry failed with an unknown-identifier error even though the
        // source typechecks. The dev-entry liveness scans raw `unsafe` text
        // for identifier tokens, matching what the runtime shaker has done
        // since deka#437. The unrelated helper pins the over-approximation
        // at the import-retention sites: it must stay out of the entry.
        let main = PathBuf::from("/project/main.ds");
        let helper = PathBuf::from("/project/helper.ds");
        let mut files = HashMap::new();
        files.insert(
            main.clone(),
            "import { greet } from \"./helper.ds\";\n\
             fn local_helper() string { return \"local\" }\n\
             fn unrelated_helper() string { return \"unrelated\" }\n\
             const greeting: string = build {\n\
             \x20 const run = unsafe { () => greet() + \" \" + local_helper() }\n\
             \x20 return match (run) { Ok(f) => f(), Err(_) => \"err\" }\n\
             }"
                .to_string(),
        );
        files.insert(
            helper.clone(),
            "export fn greet() string { return \"imported\" }".to_string(),
        );
        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./helper.ds".to_string()), helper.clone());
        let result = compile_module_graph(&main, &InMemoryLoader { files, aliases })
            .expect("graph compiles");
        let slot = result
            .dev_plan
            .slots
            .iter()
            .find(|slot| slot.binding == "greeting")
            .expect("one build slot for greeting");
        assert!(
            slot.entry.contains("function local_helper"),
            "module-local helper must be emitted in the dev entry:\n{}",
            slot.entry
        );
        assert!(
            slot.entry.contains("import { greet } from \"./helper.js\";"),
            "imported helper must be retained in the dev entry:\n{}",
            slot.entry
        );
        assert!(
            !slot.entry.contains("unrelated_helper"),
            "the token scan may over-approximate liveness but must not emit unrelated code:\n{}",
            slot.entry
        );
    }

    #[test]
    fn client_graph_rejects_ui_server() {
        let root = PathBuf::from("/project");
        let main = root.join("main.dsx");

        let mut files = HashMap::new();
        files.insert(
            main.clone(),
            "import { renderToString } from \"ui/server\";\nexport const x = renderToString;"
                .to_string(),
        );

        let loader = InMemoryLoader {
            files,
            aliases: HashMap::new(),
        };
        let err = compile_module_graph_with_options(
            &main,
            &loader,
            GraphCompileOptions {
                client: true,
                ..Default::default()
            },
        )
        .expect_err("ui/server on a client entry");
        assert!(
            err.iter().any(|d| d.message.contains("ui/server")),
            "{:?}",
            err
        );
    }

    #[test]
    fn graph_dev_slot_ids_are_project_relative_and_match_single_file_plan() {
        // The host correlates `dsc plan <file>` slot ids with the ids embedded
        // in the graph-emitted runtime modules, so both paths must relativize
        // identically when module_root is set (dsc#61).
        let source = "const labels: Array<string> = build { return Ok([\"Ada\"]) }";
        let compile_graph_at = |root: &str| {
            let page = PathBuf::from(format!("{root}/app/page.ds"));
            let files = HashMap::from([(page.clone(), source.to_string())]);
            compile_module_graph_with_options(
                &page,
                &InMemoryLoader {
                    files,
                    aliases: HashMap::new(),
                },
                GraphCompileOptions {
                    module_root: Some(PathBuf::from(root)),
                    ..Default::default()
                },
            )
            .expect("graph compiles")
        };
        let graph_a = compile_graph_at("/a/proj");
        let graph_b = compile_graph_at("/b/proj");
        assert_eq!(graph_a.dev_plan.slots.len(), 1);
        assert_eq!(
            graph_a.dev_plan.slots[0].id, graph_b.dev_plan.slots[0].id,
            "slot ids must be stable across absolute checkout roots"
        );

        let page_a = PathBuf::from("/a/proj/app/page.ds");
        let plan_a = crate::compile_to_js_with_options(
            source,
            &page_a.to_string_lossy(),
            crate::CompileOptions {
                module_root: Some(PathBuf::from("/a/proj")),
                ..Default::default()
            },
        )
        .expect("single-file plan compiles");
        assert_eq!(
            graph_a.dev_plan.slots[0].id,
            plan_a.dev_plan.slots[0].id,
            "graph and single-file plan must agree on the slot id"
        );
        let import = format!("deka:dev/{}", graph_a.dev_plan.slots[0].id);
        assert!(
            graph_a.modules[&page_a].contains(&import),
            "emitted module must import the plan slot id:\n{}",
            graph_a.modules[&page_a]
        );
    }

    #[test]
    fn graph_compiles_cross_module_struct_embed() {
        let root = PathBuf::from("/project");
        let legs = root.join("legs.ds");
        let robot = root.join("robot.ds");
        let main = root.join("main.ds");

        let mut files = HashMap::new();
        files.insert(
            legs.clone(),
            "struct Legs {}\nfn (l Legs) move() string { return \"walk\" }\nexport { Legs }"
                .to_string(),
        );
        files.insert(
            robot.clone(),
            "import { Legs } from \"./legs.ds\";\nstruct Robot { Legs }\nexport { Robot }"
                .to_string(),
        );
        files.insert(
            main.clone(),
            "import { Robot } from \"./robot.ds\";\nimport { Legs } from \"./legs.ds\";\nconst r = Robot { Legs: Legs {} };\nconst m: string = r.move();".to_string(),
        );

        let mut aliases = HashMap::new();
        aliases.insert((robot.clone(), "./legs.ds".to_string()), legs.clone());
        aliases.insert((main.clone(), "./robot.ds".to_string()), robot.clone());
        aliases.insert((main.clone(), "./legs.ds".to_string()), legs.clone());

        let loader = InMemoryLoader { files, aliases };
        let result = compile_module_graph(&main, &loader).expect("compile graph");
        assert_eq!(result.modules.len(), 3);
        let robot_js = &result.modules[&robot];
        let main_js = &result.modules[&main];
        assert!(
            robot_js.contains("Robot = __deka_struct(\"Robot\"")
                && robot_js.contains("{ Legs: Legs }"),
            "got: {}",
            robot_js
        );
        assert!(main_js.contains("Robot({ Legs: Legs({"), "got: {}", main_js);
        assert!(main_js.contains("r.move()"), "got: {}", main_js);
    }

    #[test]
    fn graph_propagates_result_type_for_imported_function() {
        let root = PathBuf::from("/project");
        let crypto = root.join("crypto.ds");
        let main = root.join("main.ds");

        let mut files = HashMap::new();
        files.insert(
            crypto.clone(),
            "export fn random_bytes(n: number) Result<string, string> {\n  return unsafe { String(n) }\n}".to_string(),
        );
        files.insert(
            main.clone(),
            "import { random_bytes } from \"./crypto.ds\";\nconst r = match (random_bytes(32)) { Ok(v) => v, Err(e) => \"\" };".to_string(),
        );

        let mut aliases = HashMap::new();
        aliases.insert((main.clone(), "./crypto.ds".to_string()), crypto.clone());

        let loader = InMemoryLoader { files, aliases };
        let result = compile_module_graph(&main, &loader).expect("compile graph");
        assert_eq!(result.modules.len(), 2);
        let main_js = &result.modules[&main];
        assert!(main_js.contains("random_bytes(32)"), "got: {}", main_js);
        assert!(main_js.contains("__case"), "got: {}", main_js);
    }

    #[test]
    fn fs_loader_resolves_relative_and_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(src.join("lib")).unwrap();
        std::fs::write(src.join("math.ds"), "export fn add() {}").unwrap();
        std::fs::write(src.join("lib").join("index.ds"), "export const x = 1;").unwrap();

        let loader = FsModuleLoader::new(root.clone());
        let referrer = src.join("main.ds");
        assert_eq!(
            loader.resolve("./math.ds", &referrer).unwrap(),
            std::fs::canonicalize(src.join("math.ds")).unwrap()
        );
        assert_eq!(
            loader.resolve("./lib", &referrer).unwrap(),
            std::fs::canonicalize(src.join("lib").join("index.ds")).unwrap()
        );
    }

    #[test]
    fn fs_loader_resolves_bare_stdlib_in_ds_modules() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let ds_modules = root.join("ds_modules");
        std::fs::create_dir_all(ds_modules.join("json")).unwrap();
        std::fs::write(
            ds_modules.join("json").join("index.ds"),
            "export fn parse() {}",
        )
        .unwrap();

        let loader = FsModuleLoader::new(root.clone());
        let referrer = root.join("main.ds");
        let resolved = loader.resolve("json", &referrer).unwrap();
        assert_eq!(
            std::fs::canonicalize(&resolved).unwrap(),
            std::fs::canonicalize(ds_modules.join("json").join("index.ds")).unwrap()
        );
    }

    #[test]
    fn fs_loader_prefers_local_link_over_installed_package() {
        let project = tempfile::tempdir().expect("project");
        let package = tempfile::tempdir().expect("package");
        let installed = project.path().join("ds_modules/@deka/example");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(installed.join("index.ds"), "export fn source() {}\n").unwrap();
        std::fs::write(
            package.path().join("deka.json"),
            r#"{"name":"@deka/example","version":"0.1.0"}"#,
        )
        .unwrap();
        std::fs::write(package.path().join("index.ds"), "export fn source() {}\n").unwrap();
        deka_project::modules::write_links_at(
            project.path(),
            &deka_project::modules::LinkManifest {
                version: deka_project::modules::LINKS_VERSION,
                packages: std::collections::BTreeMap::from([(
                    "@deka/example".to_string(),
                    deka_project::modules::LinkEntry {
                        path: package.path().canonicalize().unwrap(),
                    },
                )]),
            },
        )
        .unwrap();

        let loader = FsModuleLoader::new(project.path().to_path_buf());
        let resolved = loader
            .resolve("example", &project.path().join("main.ds"))
            .unwrap();
        assert!(resolved.starts_with(package.path().canonicalize().unwrap()));
    }

    #[test]
    fn fs_loader_does_not_fall_back_when_linked_subpath_is_missing() {
        let project = tempfile::tempdir().expect("project");
        let package = tempfile::tempdir().expect("package");
        let installed = project.path().join("ds_modules/@deka/example");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(installed.join("missing.ds"), "export fn source() {}\n").unwrap();
        std::fs::write(
            package.path().join("deka.json"),
            r#"{"name":"@deka/example","version":"0.1.0"}"#,
        )
        .unwrap();
        std::fs::write(package.path().join("index.ds"), "export fn source() {}\n").unwrap();
        deka_project::modules::write_links_at(
            project.path(),
            &deka_project::modules::LinkManifest {
                version: deka_project::modules::LINKS_VERSION,
                packages: std::collections::BTreeMap::from([(
                    "@deka/example".to_string(),
                    deka_project::modules::LinkEntry {
                        path: package.path().canonicalize().unwrap(),
                    },
                )]),
            },
        )
        .unwrap();

        let loader = FsModuleLoader::new(project.path().to_path_buf());
        let error = loader
            .resolve("@deka/example/missing", &project.path().join("main.ds"))
            .expect_err("missing linked subpath must not use installed bytes");
        assert!(error.contains("unable to resolve linked module"), "{error}");
    }

    #[test]
    fn graph_treats_ui_runtime_as_documented_virtual_stdlib() {
        let main = PathBuf::from("/project/page.dsx");
        let mut files = HashMap::new();
        files.insert(
            main.clone(),
            "import { Form } from \"ui/form\";\nconst el = <Form action=\"/api/x\" method=\"post\">Go</Form>;\n"
                .to_string(),
        );
        let loader = InMemoryLoader {
            files,
            aliases: HashMap::new(),
        };
        let result = compile_module_graph(&main, &loader)
            .expect("ui/form should remain virtual pending dsc#129");
        let js = &result.modules[&main];
        assert!(js.contains("ui/form"), "got: {js}");
        assert!(js.contains("Form"), "got: {js}");
    }

    #[test]
    fn graph_requires_client_directive_for_transitively_interactive_component() {
        let child = PathBuf::from("/project/counter.dsx");
        let wrapper = PathBuf::from("/project/wrapper.dsx");
        let page = PathBuf::from("/project/page.dsx");
        let mut files = HashMap::new();
        files.insert(
            child.clone(),
            "export fn Counter() Component {\n  return <button onClick={clicked}>0</button>\n}\nfn clicked() {}\n"
                .to_string(),
        );
        files.insert(
            wrapper.clone(),
            "import { Counter } from \"./counter.dsx\"\nexport fn Wrapper() Component { return <Counter /> }\n"
                .to_string(),
        );
        files.insert(
            page.clone(),
            "import { Wrapper } from \"./wrapper.dsx\"\nconst page = <Wrapper />\n".to_string(),
        );
        let aliases = HashMap::from([
            (
                (wrapper.clone(), "./counter.dsx".to_string()),
                child.clone(),
            ),
            ((page.clone(), "./wrapper.dsx".to_string()), wrapper.clone()),
        ]);

        let errors = compile_module_graph(&page, &InMemoryLoader { files, aliases })
            .expect_err("the page must hydrate Wrapper before its interactive child can render");
        assert_eq!(errors.len(), 1, "{errors:?}");
        let error = &errors[0];
        assert!(
            error.message.contains("page.dsx")
                && error.message.contains("Wrapper")
                && error.message.contains("uses interactive APIs")
                && error.message.contains("will not render"),
            "{}",
            error.message
        );
        assert_eq!(
            (error.line, error.column, error.underline_length),
            (2, 15, 7),
            "{error:?}"
        );
    }

    #[test]
    fn graph_client_directive_keeps_transitive_interactive_component_as_an_island() {
        let child = PathBuf::from("/project/counter.dsx");
        let wrapper = PathBuf::from("/project/wrapper.dsx");
        let page = PathBuf::from("/project/page.dsx");
        let mut files = HashMap::new();
        files.insert(
            child.clone(),
            "export fn Counter() Component {\n  return <button onClick={clicked}>0</button>\n}\nfn clicked() {}\n"
                .to_string(),
        );
        files.insert(
            wrapper.clone(),
            "import { Counter } from \"./counter.dsx\"\nexport fn Wrapper() Component { return <Counter /> }\n"
                .to_string(),
        );
        files.insert(
            page.clone(),
            "import { Wrapper } from \"./wrapper.dsx\"\nconst page = <Wrapper client:load />\n"
                .to_string(),
        );
        let aliases = HashMap::from([
            (
                (wrapper.clone(), "./counter.dsx".to_string()),
                child.clone(),
            ),
            ((page.clone(), "./wrapper.dsx".to_string()), wrapper.clone()),
        ]);

        let result = compile_module_graph(&page, &InMemoryLoader { files, aliases })
            .expect("client:load is the island root for the whole component subtree");
        assert!(
            result.modules[&page].contains("\"client:load\": true"),
            "client directive must reach emitted island code:\n{}",
            result.modules[&page]
        );
    }
}
