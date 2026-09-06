use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use swc_common::{FileName, GLOBALS, Globals, Mark, SourceMap, sync::Lrc};
use swc_ecma_ast::{EsVersion, Module, Pass, Program};
use swc_ecma_codegen::{Emitter, text_writer::JsWriter};
use swc_ecma_minifier::optimize;
use swc_ecma_minifier::option::{CompressOptions, MangleOptions, MinifyOptions};
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_ecma_transforms_base::resolver;
use swc_ecma_transforms_react::{Options as JsxOptions, Runtime as JsxRuntime, react};
use swc_ecma_transforms_typescript::strip;
use swc_ecma_visit::VisitWith;

/// Bundle output containing code and optional source map
pub struct BundleOutput {
    pub code: String,
    pub map: Option<String>,
}

/// A module that has been parsed and transformed
#[derive(Clone)]
pub struct ParsedModule {
    pub path: PathBuf,
    pub source: String,
    pub module: Module,
    pub dependencies: Vec<String>,
    pub resolved_dependencies: Vec<PathBuf>, // NEW: Pre-resolved dependency paths
}

/// Message sent to workers containing a path to process
struct WorkMessage {
    path: PathBuf,
}

/// Message sent by workers containing parse results
struct ResultMessage {
    path: PathBuf,
    result: Result<ParsedModule, String>,
}

/// Parallel bundler that processes modules concurrently
pub struct ParallelBundler {
    /// Root directory for resolution
    root: PathBuf,
    /// Number of concurrent workers
    workers: usize,
    /// Whether to bundle node_modules (default: false, mark as external)
    bundle_node_modules: bool,
    /// Whether to generate source maps
    sourcemap: bool,
    /// Whether to minify output
    minify: bool,
}

impl ParallelBundler {
    pub fn new(root: PathBuf) -> Self {
        let workers = num_cpus::get();

        // By default, bundle node_modules (needed for browser apps)
        // Set DEKA_EXTERNAL_NODE_MODULES=1 to skip bundling (for server-side/edge)
        let bundle_node_modules = std::env::var("DEKA_EXTERNAL_NODE_MODULES")
            .map(|v| v != "1" && v != "true")
            .unwrap_or(true);

        // Check for source map generation
        let sourcemap = std::env::var("DEKA_SOURCEMAP")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false);

        // Check for minification
        let minify = std::env::var("DEKA_MINIFY")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false);

        if !bundle_node_modules {
            stdio::debug("parallel", "node_modules marked as external");
        }
        if sourcemap {
            stdio::debug("parallel", "source maps enabled");
        }
        if minify {
            stdio::debug("parallel", "minification enabled");
        }

        Self {
            root,
            workers,
            bundle_node_modules,
            sourcemap,
            minify,
        }
    }

    /// Bundle an entry file by discovering and processing all dependencies in parallel
    pub async fn bundle(&self, entry: &str) -> Result<BundleOutput, String> {
        use std::time::Instant;

        stdio::debug("parallel", &format!("discovering modules from {}", entry));

        // Phase 1: Discover all modules in parallel
        let t1 = Instant::now();
        let modules = self.discover_modules(entry).await?;
        let discovery_time = t1.elapsed();
        stdio::debug(
            "parallel",
            &format!(
                "discovery: {} modules in {}ms",
                modules.len(),
                discovery_time.as_millis()
            ),
        );

        // Phase 2: Sort modules in dependency order
        let t2 = Instant::now();
        let sorted = self.sort_modules(&modules)?;
        let sort_time = t2.elapsed();
        stdio::debug(
            "parallel",
            &format!(
                "sort: {} modules in {}ms",
                sorted.len(),
                sort_time.as_millis()
            ),
        );

        // Phase 3: Concatenate modules
        let t3 = Instant::now();
        let output = self.concatenate_modules(&sorted, &modules)?;
        let concat_time = t3.elapsed();
        stdio::debug(
            "parallel",
            &format!(
                "concatenation: {} bytes in {}ms",
                output.code.len(),
                concat_time.as_millis()
            ),
        );

        Ok(output)
    }

    /// Discover all modules starting from entry, using parallel workers with channels
    async fn discover_modules(
        &self,
        entry: &str,
    ) -> Result<HashMap<PathBuf, ParsedModule>, String> {
        stdio::debug("parallel", "resolving entry path...");
        let entry_path = self.resolve_path(&self.root, entry)?;
        stdio::debug(
            "parallel",
            &format!("entry resolved to: {}", entry_path.display()),
        );

        // Create channels for work distribution
        let (work_tx, work_rx) = mpsc::unbounded_channel::<WorkMessage>();
        let (result_tx, mut result_rx) = mpsc::unbounded_channel::<ResultMessage>();

        // Shared work receiver (workers pull from it) - use tokio::Mutex for async
        let work_rx = Arc::new(tokio::sync::Mutex::new(work_rx));

        // Shared state (only for deduplication)
        let seen: Arc<RwLock<HashSet<PathBuf>>> = Arc::new(RwLock::new(HashSet::new()));

        // Mark entry as seen and send it
        seen.write().insert(entry_path.clone());
        work_tx
            .send(WorkMessage {
                path: entry_path.clone(),
            })
            .map_err(|e| format!("Failed to send entry work: {}", e))?;

        stdio::debug("parallel", &format!("spawning {} workers...", self.workers));

        // Spawn worker tasks
        let mut tasks = JoinSet::new();

        for worker_id in 0..self.workers {
            let work_rx = Arc::clone(&work_rx);
            let result_tx = result_tx.clone();

            tasks.spawn(async move {
                let mut processed_count = 0;

                stdio::debug("worker", &format!("{} started", worker_id));

                loop {
                    // Pull work from shared queue
                    let msg = {
                        let mut rx = work_rx.lock().await;
                        rx.recv().await
                    };

                    let msg = match msg {
                        Some(m) => m,
                        None => break, // Channel closed, shutdown
                    };

                    processed_count += 1;
                    if stdio::is_debug() && processed_count % 100 == 0 {
                        stdio::debug(
                            "worker",
                            &format!("{} processed {} modules", worker_id, processed_count),
                        );
                    }

                    // Parse module (CPU-intensive)
                    let result = Self::parse_module(&msg.path).await;

                    // Send result back
                    let _ = result_tx.send(ResultMessage {
                        path: msg.path.clone(),
                        result,
                    });
                }

                stdio::debug(
                    "worker",
                    &format!(
                        "{} completed ({} modules processed)",
                        worker_id, processed_count
                    ),
                );
                Ok::<(), String>(())
            });
        }

        // Drop our copy of result_tx so channel closes when workers finish
        drop(result_tx);

        // Coordinator: collect results and enqueue new work
        let mut modules: HashMap<PathBuf, ParsedModule> = HashMap::new();
        let mut pending_count = 1; // Started with 1 (entry)

        while let Some(msg) = result_rx.recv().await {
            pending_count -= 1;

            match msg.result {
                Ok(mut parsed) => {
                    // Resolve dependencies and batch them
                    let mut resolved_deps = Vec::new();
                    let mut new_work = Vec::new();

                    for dep in &parsed.dependencies {
                        if let Ok(dep_path) = Self::resolve_dependency(
                            &self.root,
                            &msg.path,
                            dep,
                            self.bundle_node_modules,
                        ) {
                            resolved_deps.push(dep_path.clone());

                            // Check if we've seen this dependency
                            let mut seen_lock = seen.write();
                            if !seen_lock.contains(&dep_path) {
                                seen_lock.insert(dep_path.clone());
                                new_work.push(dep_path);
                            }
                        }
                        // Silently ignore unresolvable dependencies (react, node_modules if external, etc.)
                    }

                    // Store resolved dependencies in the module
                    parsed.resolved_dependencies = resolved_deps;

                    // Batch send new work (reduces contention)
                    for work_path in new_work {
                        work_tx
                            .send(WorkMessage { path: work_path })
                            .map_err(|e| format!("Failed to send work: {}", e))?;
                        pending_count += 1;
                    }

                    // Store parsed module
                    modules.insert(msg.path, parsed);
                }
                Err(e) => {
                    stdio::debug("parse", &format!("failed {}: {}", msg.path.display(), e));
                }
            }

            // Done when no more pending work
            if pending_count == 0 {
                break;
            }
        }

        stdio::debug("parallel", "all workers completed");
        stdio::debug(
            "parallel",
            &format!("extracted {} total modules", modules.len()),
        );

        if stdio::is_debug() {
            // Sample first 10 paths
            for (i, path) in modules.keys().enumerate().take(10) {
                stdio::debug("parallel", &format!("  {}: {}", i, path.display()));
            }
        }

        // Shutdown workers by dropping work_tx (closes channel)
        drop(work_tx);

        // Wait for all workers to finish
        while let Some(result) = tasks.join_next().await {
            result.map_err(|e| format!("Worker task failed: {}", e))??;
        }

        Ok(modules)
    }

    /// Parse a single module (CPU-intensive, runs on tokio worker thread)
    async fn parse_module(path: &Path) -> Result<ParsedModule, String> {
        let path = path.to_path_buf();

        // Run CPU-intensive parsing on blocking thread pool
        tokio::task::spawn_blocking(move || {
            // Read file first
            let source = std::fs::read_to_string(&path)
                .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

            // FAST PATH: Plain .js files in node_modules don't need transformation
            let is_plain_js = matches!(path.extension().and_then(|e| e.to_str()), Some("js"));
            let is_node_module = path.to_string_lossy().contains("node_modules");

            if is_plain_js && is_node_module {
                // Skip SWC transformation entirely - use simple scan for dependencies
                let dependencies = ParallelBundler::extract_dependencies_fast(&source);

                // Debug: Log first few fast-path hits
                if stdio::is_debug() {
                    static COUNTER: std::sync::atomic::AtomicUsize =
                        std::sync::atomic::AtomicUsize::new(0);
                    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if count < 5 {
                        stdio::debug(
                            "fast-path",
                            &format!("skipping SWC for: {}", path.display()),
                        );
                    }
                }

                // Create a minimal ParsedModule without actual AST parsing
                // We'll use the source as-is during concatenation
                return Ok(ParsedModule {
                    path,
                    source,
                    module: Module {
                        span: swc_common::DUMMY_SP,
                        body: vec![],
                        shebang: None,
                    },
                    dependencies,
                    resolved_dependencies: Vec::new(),
                });
            }

            // SLOW PATH: TypeScript/JSX files need full SWC transformation
            let source_map = Lrc::new(SourceMap::default());
            let syntax = ParallelBundler::syntax_for_path(&path);

            // Parse
            let fm =
                source_map.new_source_file(FileName::Real(path.clone()).into(), source.clone());
            let lexer = Lexer::new(syntax, EsVersion::Es2022, StringInput::from(&*fm), None);
            let mut parser = Parser::new_from(lexer);
            let module = parser
                .parse_module()
                .map_err(|e| format!("Parse error: {:?}", e))?;

            // Transform (TypeScript, JSX)
            let module = ParallelBundler::transform_module(module, &path, &source_map)?;

            // Extract dependencies
            let dependencies = ParallelBundler::extract_dependencies(&module);

            Ok(ParsedModule {
                path,
                source,
                module,
                dependencies,
                resolved_dependencies: Vec::new(), // Resolved later in coordinator
            })
        })
        .await
        .map_err(|e| format!("Task join error: {}", e))?
    }

    /// Transform module (strip TypeScript, transform JSX)
    fn transform_module(
        module: Module,
        path: &Path,
        source_map: &Lrc<SourceMap>,
    ) -> Result<Module, String> {
        let globals = Globals::new();
        let is_ts = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("ts") | Some("tsx")
        );
        let is_jsx = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("tsx") | Some("jsx")
        );

        GLOBALS.set(&globals, || {
            let unresolved_mark = Mark::new();
            let top_level_mark = Mark::new();
            let mut program = Program::Module(module);

            // Apply resolver
            let mut pass = resolver(unresolved_mark, top_level_mark, false);
            pass.process(&mut program);

            // Strip TypeScript
            if is_ts {
                let mut pass = strip(unresolved_mark, top_level_mark);
                pass.process(&mut program);
            }

            // Transform JSX
            if is_jsx {
                let mut options = JsxOptions::default();
                options.runtime = Some(JsxRuntime::Automatic);
                options.import_source = Some("react".into());
                let mut pass = react::<Option<swc_common::comments::SingleThreadedComments>>(
                    source_map.clone(),
                    None,
                    options,
                    top_level_mark,
                    unresolved_mark,
                );
                pass.process(&mut program);
            }

            // Apply minification if enabled (must be done within same GLOBALS context)
            let should_minify = std::env::var("DEKA_MINIFY")
                .map(|v| v == "1" || v == "true")
                .unwrap_or(false);

            if should_minify {
                let minify_options = MinifyOptions {
                    compress: Some(CompressOptions::default()),
                    mangle: Some(MangleOptions::default()),
                    ..Default::default()
                };

                program = optimize(
                    program,
                    source_map.clone(),
                    None,
                    None,
                    &minify_options,
                    &swc_ecma_minifier::option::ExtraOptions {
                        unresolved_mark,
                        top_level_mark,
                        mangle_name_cache: Default::default(),
                    },
                );
            }

            match program {
                Program::Module(m) => Ok(m),
                _ => Err("Expected module".to_string()),
            }
        })
    }

    /// Fast dependency extraction by scanning source (for plain JS files)
    fn extract_dependencies_fast(source: &str) -> Vec<String> {
        let mut deps = Vec::new();

        // Simple scan for import/export from statements
        for line in source.lines() {
            let trimmed = line.trim();

            // import ... from "spec" or import ... from 'spec'
            if let Some(from_pos) = trimmed.find(" from ") {
                if trimmed.starts_with("import ") || trimmed.starts_with("export ") {
                    let after_from = &trimmed[from_pos + 6..].trim_start();
                    if let Some(spec) = Self::extract_quoted_string(after_from) {
                        deps.push(spec);
                    }
                }
            }
        }

        deps
    }

    /// Extract string from quotes: "foo" or 'foo' -> foo
    fn extract_quoted_string(s: &str) -> Option<String> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }

        let quote = s.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }

        let end = s[1..].find(quote)?;
        Some(s[1..=end].to_string())
    }

    /// Extract import/export dependencies from a module (for transformed modules)
    fn extract_dependencies(module: &Module) -> Vec<String> {
        use swc_ecma_visit::Visit;

        struct DependencyCollector {
            deps: Vec<String>,
        }

        impl Visit for DependencyCollector {
            fn visit_import_decl(&mut self, n: &swc_ecma_ast::ImportDecl) {
                self.deps
                    .push(String::from_utf8_lossy((&*n.src.value).as_bytes()).into_owned());
            }

            fn visit_export_all(&mut self, n: &swc_ecma_ast::ExportAll) {
                self.deps
                    .push(String::from_utf8_lossy((&*n.src.value).as_bytes()).into_owned());
            }

            fn visit_named_export(&mut self, n: &swc_ecma_ast::NamedExport) {
                if let Some(src) = &n.src {
                    self.deps
                        .push(String::from_utf8_lossy((&*src.value).as_bytes()).into_owned());
                }
            }
        }

        let mut collector = DependencyCollector { deps: Vec::new() };
        module.visit_with(&mut collector);
        collector.deps
    }

    /// Resolve a dependency path
    fn resolve_dependency(
        root: &Path,
        from: &Path,
        specifier: &str,
        bundle_node_modules: bool,
    ) -> Result<PathBuf, String> {
        // Handle special cases (react, etc.)
        if specifier == "react" || specifier == "react-dom/client" || specifier.starts_with("deka/")
        {
            // These are handled by vendor files - skip for now
            return Err("Special module".to_string());
        }

        // Skip node_modules unless explicitly requested
        if !bundle_node_modules
            && !specifier.starts_with("./")
            && !specifier.starts_with("../")
            && !specifier.starts_with("/")
        {
            // This is a bare import (e.g., "lodash", "@iconify-icons/...") - treat as external
            return Err("External module (node_modules)".to_string());
        }

        // Resolve relative imports
        if specifier.starts_with("./") || specifier.starts_with("../") {
            let base = from.parent().unwrap_or(root);
            let path = base.join(specifier);

            // Try with extensions
            let candidates = vec![
                path.clone(),
                path.with_extension("ts"),
                path.with_extension("tsx"),
                path.with_extension("jsx"),
                path.with_extension("js"),
                path.join("index.ts"),
                path.join("index.tsx"),
                path.join("index.jsx"),
                path.join("index.js"),
            ];

            for candidate in candidates {
                if candidate.exists() && candidate.is_file() {
                    // CRITICAL: Canonicalize to prevent duplicate processing
                    return candidate.canonicalize().map_err(|e| {
                        format!("Failed to canonicalize {}: {}", candidate.display(), e)
                    });
                }
            }
        }

        // Node modules
        if !specifier.starts_with(".") && !specifier.starts_with("/") {
            let mut current = from.parent();
            while let Some(dir) = current {
                let node_modules = dir.join("node_modules").join(specifier);
                if node_modules.exists() {
                    // CRITICAL: Canonicalize node_modules paths too!
                    return node_modules.canonicalize().map_err(|e| {
                        format!(
                            "Failed to canonicalize node_modules path {}: {}",
                            node_modules.display(),
                            e
                        )
                    });
                }
                current = dir.parent();
            }
        }

        Err(format!("Could not resolve: {}", specifier))
    }

    /// Sort modules in dependency order using Kahn's algorithm (O(N+E))
    fn sort_modules(
        &self,
        modules: &HashMap<PathBuf, ParsedModule>,
    ) -> Result<Vec<PathBuf>, String> {
        use std::collections::VecDeque;

        // Calculate in-degree for each module
        let mut in_degree: HashMap<PathBuf, usize> = HashMap::new();

        // Initialize all modules with in-degree 0
        for path in modules.keys() {
            in_degree.insert(path.clone(), 0);
        }

        // Count incoming edges (dependencies pointing to this module)
        for module in modules.values() {
            for dep_path in &module.resolved_dependencies {
                if modules.contains_key(dep_path) {
                    *in_degree.entry(dep_path.clone()).or_insert(0) += 1;
                }
            }
        }

        // Start with modules that have no dependencies
        let mut queue = VecDeque::new();
        for (path, &degree) in &in_degree {
            if degree == 0 {
                queue.push_back(path.clone());
            }
        }

        // Process queue
        let mut sorted = Vec::new();
        while let Some(path) = queue.pop_front() {
            sorted.push(path.clone());

            if let Some(module) = modules.get(&path) {
                for dep_path in &module.resolved_dependencies {
                    if let Some(degree) = in_degree.get_mut(dep_path) {
                        *degree -= 1;
                        if *degree == 0 {
                            queue.push_back(dep_path.clone());
                        }
                    }
                }
            }
        }

        // Check for cycles
        if sorted.len() != modules.len() {
            return Err(format!(
                "Circular dependency detected: sorted {} of {} modules",
                sorted.len(),
                modules.len()
            ));
        }

        Ok(sorted)
    }

    /// Concatenate modules into final output
    fn concatenate_modules(
        &self,
        sorted: &[PathBuf],
        modules: &HashMap<PathBuf, ParsedModule>,
    ) -> Result<BundleOutput, String> {
        let mut output = String::new();
        output.push_str("// Parallel bundled output\n\n");

        let source_map = Lrc::new(SourceMap::default());

        for path in sorted {
            if let Some(parsed) = modules.get(path) {
                output.push_str(&format!("// Module: {}\n", path.display()));

                // FAST PATH: If module has empty AST body, it was fast-pathed - use source directly
                if parsed.module.body.is_empty() {
                    output.push_str(&parsed.source);
                    output.push_str("\n\n");
                } else {
                    // SLOW PATH: Emit from AST (transformed TypeScript/JSX, already minified if enabled)
                    let mut buf = vec![];
                    {
                        let mut writer = JsWriter::new(source_map.clone(), "\n", &mut buf, None);
                        let mut emitter = Emitter {
                            cfg: Default::default(),
                            cm: source_map.clone(),
                            comments: None,
                            wr: &mut writer,
                        };

                        emitter
                            .emit_module(&parsed.module)
                            .map_err(|e| format!("Failed to emit module: {:?}", e))?;
                    }

                    let code =
                        String::from_utf8(buf).map_err(|e| format!("Invalid UTF-8: {}", e))?;

                    output.push_str(&code);
                    output.push_str("\n\n");
                }
            }
        }

        // Generate source map if enabled
        let map = if self.sourcemap {
            // Note: Source map generation requires proper file tracking during emit
            // For now, we'll generate a basic source map structure
            // TODO: Implement proper source map generation with file positions
            stdio::debug(
                "sourcemap",
                "source map generation not fully implemented yet",
            );
            None
        } else {
            None
        };

        Ok(BundleOutput { code: output, map })
    }

    fn resolve_path(&self, base: &Path, specifier: &str) -> Result<PathBuf, String> {
        let path = PathBuf::from(specifier);
        let full_path = if path.is_absolute() {
            path
        } else {
            base.join(path)
        };

        // Canonicalize to ensure consistent paths
        full_path
            .canonicalize()
            .map_err(|e| format!("Failed to resolve {}: {}", specifier, e))
    }

    fn syntax_for_path(path: &Path) -> Syntax {
        match path.extension().and_then(|e| e.to_str()) {
            Some("ts") => Syntax::Typescript(TsSyntax {
                tsx: false,
                decorators: false,
                dts: false,
                no_early_errors: true,
                disallow_ambiguous_jsx_like: true,
            }),
            Some("tsx") => Syntax::Typescript(TsSyntax {
                tsx: true,
                decorators: false,
                dts: false,
                no_early_errors: true,
                disallow_ambiguous_jsx_like: true,
            }),
            Some("jsx") => Syntax::Es(EsSyntax {
                jsx: true,
                decorators: false,
                ..Default::default()
            }),
            _ => Syntax::Es(EsSyntax {
                jsx: false,
                decorators: false,
                ..Default::default()
            }),
        }
    }
}
