use super::*;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// didChange validations are debounced so a keystroke burst runs the project
/// graph check once, not per change notification. didOpen validates
/// immediately — the first paint of a file should already carry squiggles.
const VALIDATION_DEBOUNCE: Duration = Duration::from_millis(300);

pub(crate) struct Backend {
    client: Client,
    documents: Arc<RwLock<HashMap<Url, String>>>,
    workspace_roots: Arc<RwLock<Vec<PathBuf>>>,
    target_mode: Arc<RwLock<TargetMode>>,
    /// Monotonic counter bumped on every change; a debounced validation runs
    /// only if it is still the latest.
    validation_seq: Arc<AtomicU64>,
    /// URIs the project-aware path last published diagnostics for, so files
    /// that go clean get an empty publish instead of keeping stale squiggles.
    project_published: Arc<RwLock<HashSet<Url>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub(crate) enum TargetMode {
    #[default]
    Server,
    Adwa,
}

impl TargetMode {
    pub(crate) fn from_initialize_params(params: &InitializeParams) -> Self {
        let Some(options) = params.initialization_options.as_ref() else {
            return Self::Server;
        };
        let Some(root) = options.as_object() else {
            return Self::Server;
        };
        let Some(dekascript) = root.get(LANGUAGE_ID).and_then(|value| value.as_object()) else {
            return Self::Server;
        };
        let Some(target) = dekascript.get("target").and_then(|value| value.as_str()) else {
            return Self::Server;
        };
        if target.eq_ignore_ascii_case("adwa") {
            Self::Adwa
        } else {
            Self::Server
        }
    }
}

impl Backend {
    pub(crate) async fn diagnostics_for_text(
        &self,
        text: &str,
        file_path: &str,
    ) -> Vec<Diagnostic> {
        let documents = self.documents.read().await.clone();
        let workspace_roots = self.workspace_roots.read().await.clone();
        let target_mode = *self.target_mode.read().await;
        entry_diagnostics(&documents, &workspace_roots, target_mode, text, file_path)
    }

    pub(crate) async fn get_document(&self, uri: &Url) -> Option<String> {
        let docs = self.documents.read().await;
        docs.get(uri).cloned()
    }

    #[cfg(test)]
    pub(crate) fn for_test(client: Client, workspace_root: PathBuf) -> Self {
        Self {
            client,
            documents: Arc::new(RwLock::new(HashMap::new())),
            workspace_roots: Arc::new(RwLock::new(vec![workspace_root])),
            target_mode: Arc::new(RwLock::new(TargetMode::default())),
            validation_seq: Arc::new(AtomicU64::new(0)),
            project_published: Arc::new(RwLock::new(HashSet::new())),
        }
    }
}

/// Diagnostics for one document. Inside a project (a `deka.json`/`deka.lock`
/// marker up the tree) this is the project-aware graph check shared with
/// `dsc check`; outside a project it is the legacy single-file analysis.
pub(crate) fn entry_diagnostics(
    documents: &HashMap<Url, String>,
    workspace_roots: &[PathBuf],
    target_mode: TargetMode,
    text: &str,
    file_path: &str,
) -> Vec<Diagnostic> {
    let path = Path::new(file_path);
    if path.is_absolute() {
        let open_documents = open_document_paths(documents);
        if let Some(files) = project_file_diagnostics(path, &open_documents) {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let mut diagnostics = files
                .into_iter()
                .find(|(file, _)| *file == canonical)
                .map(|(_, diagnostics)| diagnostics)
                .unwrap_or_default();
            diagnostics.extend(target_capability_diagnostics(text, target_mode));
            return diagnostics;
        }
    }
    legacy_diagnostics(text, file_path, workspace_roots, target_mode)
}

/// The pre-project single-file path: compiler front end on the one buffer,
/// the hand-rolled import/export checker, and target capability gating.
fn legacy_diagnostics(
    text: &str,
    file_path: &str,
    workspace_roots: &[PathBuf],
    target_mode: TargetMode,
) -> Vec<Diagnostic> {
    let core_diagnostics = analyze(text, &AnalysisContext::new(file_path));
    let unresolved_imports = unresolved_import_diagnostics(text, file_path, workspace_roots);
    let unresolved_ranges: std::collections::HashSet<(u32, u32, u32, u32)> = unresolved_imports
        .iter()
        .map(|diag| {
            (
                diag.range.start.line,
                diag.range.start.character,
                diag.range.end.line,
                diag.range.end.character,
            )
        })
        .collect();

    let mut diagnostics = Vec::new();
    for diagnostic in core_diagnostics {
        let diagnostic = diagnostic_from_analysis(diagnostic);
        if should_skip_unused_import_warning(&diagnostic, &unresolved_ranges) {
            continue;
        }
        diagnostics.push(diagnostic);
    }
    diagnostics.extend(unresolved_imports);
    diagnostics.extend(target_capability_diagnostics(text, target_mode));
    diagnostics
}

/// Validate `uri` and publish. Inside a project, every file the graph check
/// reports on gets its own publish (cross-file errors surface on the file
/// that owns them), and previously published files that went clean get an
/// empty publish. Outside a project only `uri` is published.
pub(crate) async fn validate_document(
    client: &Client,
    documents: &Arc<RwLock<HashMap<Url, String>>>,
    workspace_roots: &Arc<RwLock<Vec<PathBuf>>>,
    target_mode: &Arc<RwLock<TargetMode>>,
    project_published: &Arc<RwLock<HashSet<Url>>>,
    uri: Url,
) {
    if !is_dekascript_uri(&uri) {
        return;
    }
    let docs = documents.read().await.clone();
    let Some(text) = docs.get(&uri).cloned() else {
        return;
    };
    let Ok(file_path) = uri.to_file_path() else {
        return;
    };
    let mode = *target_mode.read().await;

    let open_documents = open_document_paths(&docs);
    if let Some(files) = project_file_diagnostics(&file_path, &open_documents) {
        let canonical_entry = std::fs::canonicalize(&file_path).unwrap_or(file_path);
        let mut current: HashSet<Url> = HashSet::new();
        let mut entry_published = false;
        for (path, mut file_diagnostics) in files {
            if path == canonical_entry {
                file_diagnostics.extend(target_capability_diagnostics(&text, mode));
            }
            let Ok(file_uri) = Url::from_file_path(&path) else {
                continue;
            };
            entry_published |= file_uri == uri;
            current.insert(file_uri.clone());
            client
                .publish_diagnostics(file_uri, file_diagnostics, None)
                .await;
        }
        if !entry_published {
            // The entry is clean: publish (possibly only target-capability
            // diagnostics, empty otherwise) so stale squiggles clear.
            let diagnostics = target_capability_diagnostics(&text, mode);
            client.publish_diagnostics(uri.clone(), diagnostics, None).await;
            current.insert(uri.clone());
        }
        let mut published = project_published.write().await;
        for stale in published.difference(&current) {
            client
                .publish_diagnostics(stale.clone(), Vec::new(), None)
                .await;
        }
        *published = current;
        return;
    }

    let roots = workspace_roots.read().await.clone();
    let file_path_str = file_path
        .to_str()
        .map(str::to_string)
        .unwrap_or_else(|| uri.to_string());
    let diagnostics = legacy_diagnostics(&text, &file_path_str, &roots, mode);
    client.publish_diagnostics(uri, diagnostics, None).await;
}

/// Completion items for one document. Import-clause and module-path contexts
/// come first (the imported module's exports, or stdlib items for bare stdlib
/// specifiers), then `@` annotations, then the scope-aware path: component
/// names in JSX tag position, otherwise everything in scope at the cursor —
/// locals/params from the enclosing blocks, top-level items, imports — plus
/// the static builtin/stdlib/snippet lists, all filtered by the identifier
/// prefix before the cursor.
pub(crate) fn entry_completions(
    documents: &HashMap<Url, String>,
    workspace_roots: &[PathBuf],
    text: &str,
    file_path: &str,
    offset: usize,
) -> Vec<CompletionItem> {
    let open_documents = open_document_paths(documents);
    if let Some(items) =
        completion_for_import(text, file_path, offset, workspace_roots, &open_documents)
    {
        return items;
    }
    if let Some(items) = completion_for_annotation(text, offset) {
        return items;
    }

    let arena = bumpalo::Bump::new();
    let program = deka_syntax::parse_recovering(text, &arena).program;
    if let Some(items) = component_completion_items(program.as_ref(), text, offset) {
        return items;
    }

    let prefix = identifier_prefix_at(text, offset);
    let mut items = match &program {
        Some(program) => scope_completion_items(program, offset),
        None => Vec::new(),
    };
    let is_dsx = Path::new(file_path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("dsx"));
    if is_dsx {
        items.extend(ambient_hook_completion_items());
    }
    items.extend(builtin_completion_items());
    items.extend(stdlib_completion_items());
    items.extend(snippet_completion_items());
    filter_by_prefix(items, prefix)
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> tower_lsp::jsonrpc::Result<InitializeResult> {
        let target_mode = TargetMode::from_initialize_params(&params);
        *self.target_mode.write().await = target_mode;
        let mut roots = Vec::new();
        if let Some(folders) = params.workspace_folders {
            for folder in folders {
                if let Ok(path) = folder.uri.to_file_path() {
                    roots.push(path);
                }
            }
        } else if let Some(root_uri) = params.root_uri {
            if let Ok(path) = root_uri.to_file_path() {
                roots.push(path);
            }
        }
        if roots.is_empty() {
            if let Ok(current) = std::env::current_dir() {
                roots.push(current);
            }
        }
        *self.workspace_roots.write().await = roots;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(true.into()),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "'".to_string(),
                        "\"".to_string(),
                        "@".to_string(),
                    ]),
                    ..CompletionOptions::default()
                }),
                diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                    DiagnosticOptions {
                        identifier: Some(LANGUAGE_ID.to_string()),
                        inter_file_dependencies: true,
                        workspace_diagnostics: false,
                        work_done_progress_options: Default::default(),
                    },
                )),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Left(true)),
                ..ServerCapabilities::default()
            },
            server_info: None,
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "DekaScript LSP initialized")
            .await;
    }

    async fn shutdown(&self) -> tower_lsp::jsonrpc::Result<()> {
        Ok(())
    }

    async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> tower_lsp::jsonrpc::Result<DocumentDiagnosticReportResult> {
        let uri = params.text_document.uri;
        if !is_dekascript_uri(&uri) {
            return Ok(empty_diagnostic_report());
        }
        let text = if let Some(in_memory) = self.get_document(&uri).await {
            in_memory
        } else if let Ok(path) = uri.to_file_path() {
            fs::read_to_string(path).unwrap_or_default()
        } else {
            String::new()
        };
        let file_path = uri
            .to_file_path()
            .ok()
            .and_then(|path| path.to_str().map(|path| path.to_string()))
            .unwrap_or_else(|| uri.to_string());
        let diagnostics = self.diagnostics_for_text(&text, &file_path).await;
        Ok(DocumentDiagnosticReportResult::Report(
            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                related_documents: None,
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: None,
                    items: diagnostics,
                },
            }),
        ))
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        if params.text_document.language_id != LANGUAGE_ID {
            return;
        }
        self.client
            .log_message(
                MessageType::INFO,
                format!("Opened {}", params.text_document.uri),
            )
            .await;

        let uri = params.text_document.uri;
        if !is_dekascript_uri(&uri) {
            return;
        }
        let text = params.text_document.text;
        self.documents.write().await.insert(uri.clone(), text);
        validate_document(
            &self.client,
            &self.documents,
            &self.workspace_roots,
            &self.target_mode,
            &self.project_published,
            uri,
        )
        .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.client
            .log_message(
                MessageType::INFO,
                format!("Changed {}", params.text_document.uri),
            )
            .await;

        let uri = params.text_document.uri;
        if !is_dekascript_uri(&uri) {
            return;
        }
        let text = params
            .content_changes
            .last()
            .map(|change| change.text.as_str())
            .unwrap_or("")
            .to_string();
        self.documents
            .write()
            .await
            .insert(uri.clone(), text.clone());

        let seq = self.validation_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let client = self.client.clone();
        let documents = self.documents.clone();
        let workspace_roots = self.workspace_roots.clone();
        let target_mode = self.target_mode.clone();
        let project_published = self.project_published.clone();
        let validation_seq = self.validation_seq.clone();
        tokio::spawn(async move {
            tokio::time::sleep(VALIDATION_DEBOUNCE).await;
            if validation_seq.load(Ordering::SeqCst) != seq {
                return;
            }
            validate_document(
                &client,
                &documents,
                &workspace_roots,
                &target_mode,
                &project_published,
                uri,
            )
            .await;
        });
    }

    async fn hover(
        &self,
        params: tower_lsp::lsp_types::HoverParams,
    ) -> tower_lsp::jsonrpc::Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        if !is_dekascript_uri(&uri) {
            return Ok(None);
        }
        let position = params.text_document_position_params.position;
        let Some(text) = self.get_document(&uri).await else {
            return Ok(None);
        };
        let file_path = uri
            .to_file_path()
            .ok()
            .and_then(|path| path.to_str().map(|path| path.to_string()))
            .unwrap_or_else(|| uri.to_string());

        let line_index = LineIndex::new(&text);
        let offset = match line_index.position_to_offset(position) {
            Some(offset) => offset,
            None => return Ok(None),
        };

        let documents = self.documents.read().await.clone();
        let hover_text = entry_hover(&documents, &text, &file_path, offset);

        let Some(value) = hover_text else {
            return Ok(None);
        };

        let range = word_span_at_offset(text.as_bytes(), offset)
            .map(|span| span_to_range(span, &line_index));
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range,
        }))
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> tower_lsp::jsonrpc::Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        if !is_dekascript_uri(&uri) {
            return Ok(None);
        }
        let position = params.text_document_position.position;
        let Some(text) = self.get_document(&uri).await else {
            return Ok(None);
        };
        let file_path = uri
            .to_file_path()
            .ok()
            .and_then(|path| path.to_str().map(|path| path.to_string()))
            .unwrap_or_else(|| uri.to_string());

        let line_index = LineIndex::new(&text);
        let offset = match line_index.position_to_offset(position) {
            Some(offset) => offset,
            None => return Ok(None),
        };

        let documents = self.documents.read().await.clone();
        let workspace_roots = self.workspace_roots.read().await.clone();
        let items = entry_completions(&documents, &workspace_roots, &text, &file_path, offset);
        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn references(
        &self,
        params: ReferenceParams,
    ) -> tower_lsp::jsonrpc::Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        if !is_dekascript_uri(&uri) {
            return Ok(None);
        }
        let position = params.text_document_position.position;
        let mut text = self.get_document(&uri).await;
        if text.is_none() {
            if let Ok(path) = uri.to_file_path() {
                text = fs::read_to_string(path).ok();
            }
        }
        let Some(text) = text else {
            return Ok(None);
        };
        let line_index = LineIndex::new(&text);
        let offset = match line_index.position_to_offset(position) {
            Some(offset) => offset,
            None => return Ok(None),
        };

        let mut roots = self.workspace_roots.read().await.clone();
        if roots.is_empty() {
            if let Ok(path) = uri.to_file_path() {
                if let Some(parent) = path.parent() {
                    roots.push(parent.to_path_buf());
                }
            }
        }

        let Some(word) = word_at_offset(text.as_bytes(), offset) else {
            return Ok(None);
        };

        let locations = collect_reference_locations(&roots, &uri, &text, &word);

        Ok(Some(locations))
    }

    async fn rename(
        &self,
        params: RenameParams,
    ) -> tower_lsp::jsonrpc::Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri;
        if !is_dekascript_uri(&uri) {
            return Ok(None);
        }
        let position = params.text_document_position.position;
        let new_name = params.new_name;
        let mut text = self.get_document(&uri).await;
        if text.is_none() {
            if let Ok(path) = uri.to_file_path() {
                text = fs::read_to_string(path).ok();
            }
        }
        let Some(text) = text else {
            return Ok(None);
        };
        let line_index = LineIndex::new(&text);
        let offset = match line_index.position_to_offset(position) {
            Some(offset) => offset,
            None => return Ok(None),
        };

        let mut roots = self.workspace_roots.read().await.clone();
        if roots.is_empty() {
            if let Ok(path) = uri.to_file_path() {
                if let Some(parent) = path.parent() {
                    roots.push(parent.to_path_buf());
                }
            }
        }

        if let Some(module_spec) = import_module_at_offset(&text, offset) {
            let changes = collect_module_rename_edits(&roots, &uri, &text, &module_spec, &new_name);
            if !changes.is_empty() {
                return Ok(Some(WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                }));
            }
        }

        let Some(word) = word_at_offset(text.as_bytes(), offset) else {
            return Ok(None);
        };

        let changes = collect_symbol_rename_edits(&roots, &uri, &text, &word, &new_name);

        if changes.is_empty() {
            return Ok(None);
        }

        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }))
    }
}

fn empty_diagnostic_report() -> DocumentDiagnosticReportResult {
    DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(
        RelatedFullDocumentDiagnosticReport {
            related_documents: None,
            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                result_id: None,
                items: Vec::new(),
            },
        },
    ))
}

pub async fn run_stdio() -> anyhow::Result<()> {
    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: Arc::new(RwLock::new(HashMap::new())),
        workspace_roots: Arc::new(RwLock::new(Vec::new())),
        target_mode: Arc::new(RwLock::new(TargetMode::default())),
        validation_seq: Arc::new(AtomicU64::new(0)),
        project_published: Arc::new(RwLock::new(HashSet::new())),
    });
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
    Ok(())
}
