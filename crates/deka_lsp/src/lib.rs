#![allow(clippy::all)]

#[cfg(feature = "native")]
use std::collections::HashMap;
#[cfg(feature = "native")]
use std::fs;
#[cfg(feature = "native")]
use std::path::{Path, PathBuf};
#[cfg(feature = "native")]
use std::sync::Arc;
#[cfg(feature = "native")]
use tokio::sync::RwLock;
#[cfg(feature = "native")]
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionOptions, CompletionParams, CompletionResponse,
    Diagnostic, DiagnosticOptions, DiagnosticServerCapabilities, DiagnosticSeverity,
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, DocumentDiagnosticParams,
    DocumentDiagnosticReport, DocumentDiagnosticReportResult, Documentation,
    FullDocumentDiagnosticReport, Hover, HoverContents, InitializeParams, InitializeResult,
    InitializedParams, InsertTextFormat, Location, MarkupContent, MarkupKind, MessageType, OneOf,
    Position, Range, ReferenceParams, RelatedFullDocumentDiagnosticReport, RenameParams,
    ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Url,
    WorkspaceEdit,
};
#[cfg(feature = "native")]
use tower_lsp::{Client, LanguageServer, LspService, Server};

mod analysis;
#[cfg(feature = "native")]
mod completion;
#[cfg(feature = "native")]
mod diagnostics;
#[cfg(feature = "native")]
mod documents;
#[cfg(feature = "native")]
mod handlers;
#[cfg(feature = "native")]
mod symbols;
pub use analysis::{
    AnalysisContext, AnalysisDiagnostic, AnalysisPosition, AnalysisRange, AnalysisSeverity,
    analyze, is_dekascript_context,
};
#[cfg(feature = "native")]
pub use handlers::run_stdio;

#[cfg(feature = "native")]
pub(crate) use completion::*;
#[cfg(feature = "native")]
pub(crate) use diagnostics::*;
#[cfg(feature = "native")]
pub(crate) use documents::*;
#[cfg(feature = "native")]
pub(crate) use handlers::TargetMode;
#[cfg(feature = "native")]
pub(crate) use symbols::*;

#[cfg(feature = "native")]
pub(crate) const LANGUAGE_ID: &str = "dekascript";

#[cfg(feature = "native")]
pub(crate) fn is_dekascript_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ds"))
}

#[cfg(feature = "native")]
pub(crate) fn is_dekascript_uri(uri: &Url) -> bool {
    uri.to_file_path()
        .is_ok_and(|path| is_dekascript_path(&path))
}

#[cfg(all(test, feature = "native"))]
mod tests;
