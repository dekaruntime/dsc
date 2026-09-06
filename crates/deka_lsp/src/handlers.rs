use super::*;

pub(crate) struct Backend {
    _client: Client,
    documents: Arc<RwLock<HashMap<Url, String>>>,
    workspace_roots: Arc<RwLock<Vec<PathBuf>>>,
    target_mode: Arc<RwLock<TargetMode>>,
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
        let core_diagnostics = analyze(text, &AnalysisContext::new(file_path));
        let workspace_roots = self.workspace_roots.read().await.clone();
        let target_mode = *self.target_mode.read().await;
        let unresolved_imports = unresolved_import_diagnostics(text, file_path, &workspace_roots);
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

    pub(crate) async fn validate_document(&self, uri: Url, text: &str) {
        if !is_dekascript_uri(&uri) {
            return;
        }
        let file_path = uri
            .to_file_path()
            .ok()
            .and_then(|path| path.to_str().map(|path| path.to_string()))
            .unwrap_or_else(|| uri.to_string());
        let diagnostics = self.diagnostics_for_text(text, &file_path).await;

        self._client
            .publish_diagnostics(uri, diagnostics, None)
            .await;
    }

    pub(crate) async fn get_document(&self, uri: &Url) -> Option<String> {
        let docs = self.documents.read().await;
        docs.get(uri).cloned()
    }
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
        self._client
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
        self._client
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
        self.documents
            .write()
            .await
            .insert(uri.clone(), text.clone());
        self.validate_document(uri, &text).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self._client
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
        self.validate_document(uri, &text).await;
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

        let line_index = LineIndex::new(&text);
        let offset = match line_index.position_to_offset(position) {
            Some(offset) => offset,
            None => return Ok(None),
        };

        let hover_text = hover_for_annotation(&text, offset)
            .or_else(|| hover_from_import(&text, offset));

        let Some(value) = hover_text else {
            return Ok(None);
        };

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
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

        let workspace_roots = self.workspace_roots.read().await.clone();
        if let Some(items) = completion_for_import(&text, &file_path, offset, &workspace_roots) {
            return Ok(Some(CompletionResponse::Array(items)));
        }
        if let Some(items) = completion_for_annotation(&text, offset) {
            return Ok(Some(CompletionResponse::Array(items)));
        }

        let mut items = builtin_completion_items();
        items.extend(stdlib_completion_items());
        items.extend(snippet_completion_items());
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
        _client: client,
        documents: Arc::new(RwLock::new(HashMap::new())),
        workspace_roots: Arc::new(RwLock::new(Vec::new())),
        target_mode: Arc::new(RwLock::new(TargetMode::default())),
    });
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
    Ok(())
}
