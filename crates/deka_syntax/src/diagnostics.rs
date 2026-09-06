//! Diagnostics produced by the DekaScript compiler v2.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub line: usize,
    pub column: usize,
    pub message: String,
    pub help_text: Option<String>,
    pub underline_length: usize,
}

impl Diagnostic {
    pub fn error(line: usize, column: usize, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            line,
            column,
            message: message.into(),
            help_text: None,
            underline_length: 1,
        }
    }

    pub fn warning(line: usize, column: usize, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            line,
            column,
            message: message.into(),
            help_text: None,
            underline_length: 1,
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help_text = Some(help.into());
        self
    }

    pub fn with_underline(mut self, len: usize) -> Self {
        self.underline_length = len;
        self
    }
}
