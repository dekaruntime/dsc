//! Deka Validation Library
//!
//! Shared validation and error formatting logic for Deka runtimes.
//! Supports both native Rust and WebAssembly compilation.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

/// Format a validation error with beautiful Rust/Gleam-style output
///
/// This function creates consistent error messages across all Deka systems:
/// - deka-runtime (native Rust)
/// - deka-edge (native Rust)
/// - playground (WASM in browser)
/// - CLI tools (WASM in Bun/Node)
///
/// # Arguments
///
/// * `code` - The source code containing the error
/// * `file_path` - Path to the file (e.g., "handler.ts")
/// * `error_kind` - Category of error (e.g., "Invalid Import", "Type Error")
/// * `line_num` - Line number (1-indexed)
/// * `col_num` - Column number (1-indexed)
/// * `message` - Error message
/// * `help` - Help text explaining how to fix
/// * `underline_length` - Number of characters to underline (for ^^^)
///
/// # Example
///
/// ```rust
/// use deka_validation::format_validation_error;
///
/// let error = format_validation_error(
///     "import { serve } from 'deka/invalid';",
///     "handler.ts",
///     "Invalid Import",
///     1,
///     26,
///     "Module 'deka/invalid' not found",
///     "Available modules: deka, deka/router, deka/sqlite",
///     12
/// );
///
/// // Produces (every row hangs off one gutter, so the caret sits under the
/// // column the header names):
/// // Validation Error
/// // ❌ Invalid Import
/// //
/// //     ┌─ handler.ts:1:26
/// //     │
/// //   1 │ import { serve } from 'deka/invalid';
/// //     │                          ^^^^^^^^^^^^ Module 'deka/invalid' not found
/// //     │
/// //     = help: Available modules: deka, deka/router, deka/sqlite
/// //     │
/// //     └─
/// ```
// Pre-existing WASM API; refactoring is out of scope for #231.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(feature = "wasm", wasm_bindgen)]
pub fn format_validation_error(
    code: &str,
    file_path: &str,
    error_kind: &str,
    line_num: usize,
    col_num: usize,
    message: &str,
    help: &str,
    underline_length: usize,
) -> String {
    format_error_impl(
        code,
        file_path,
        error_kind,
        line_num,
        col_num,
        message,
        help,
        underline_length,
        None,
    )
}

// Pre-existing WASM API; refactoring is out of scope for #231.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(feature = "wasm", wasm_bindgen)]
pub fn format_validation_error_extended(
    code: &str,
    file_path: &str,
    error_kind: &str,
    line_num: usize,
    col_num: usize,
    message: &str,
    help: &str,
    underline_length: usize,
    severity: &str,
    docs_link: Option<String>,
) -> String {
    format_error_impl(
        code,
        file_path,
        error_kind,
        line_num,
        col_num,
        message,
        help,
        underline_length,
        Some(ExtraFormatInfo {
            severity: severity.to_string(),
            docs_link,
            suggestion: None,
        }),
    )
}

// Pre-existing WASM API; refactoring is out of scope for #231.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(feature = "wasm", wasm_bindgen)]
pub fn format_validation_error_with_suggestion(
    code: &str,
    file_path: &str,
    error_kind: &str,
    line_num: usize,
    col_num: usize,
    message: &str,
    help: &str,
    underline_length: usize,
    severity: &str,
    docs_link: Option<String>,
    suggestion: Option<String>,
) -> String {
    format_error_impl(
        code,
        file_path,
        error_kind,
        line_num,
        col_num,
        message,
        help,
        underline_length,
        Some(ExtraFormatInfo {
            severity: severity.to_string(),
            docs_link,
            suggestion,
        }),
    )
}

#[derive(Debug, Clone)]
struct ExtraFormatInfo {
    severity: String,
    docs_link: Option<String>,
    suggestion: Option<String>,
}

// Pre-existing formatter signature; refactoring is out of scope for #231.
#[allow(clippy::too_many_arguments)]
fn format_error_impl(
    code: &str,
    file_path: &str,
    error_kind: &str,
    line_num: usize,
    col_num: usize,
    message: &str,
    help: &str,
    underline_length: usize,
    extra: Option<ExtraFormatInfo>,
) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let error_line = if line_num > 0 && line_num <= lines.len() {
        lines[line_num - 1]
    } else {
        ""
    };

    let underline_length = underline_length.max(1);

    let severity = extra
        .as_ref()
        .map(|extra| extra.severity.as_str())
        .unwrap_or("error");
    let (icon, label) = match severity {
        "warning" | "warn" => ("⚠️", "Validation Warning"),
        "info" => ("ℹ️", "Validation Info"),
        _ => ("❌", "Validation Error"),
    };

    let use_color = use_color_output();
    let severity_color = match severity {
        "warning" | "warn" => "\x1b[33m",
        "info" => "\x1b[34m",
        _ => "\x1b[31m",
    };
    let kind_color = color_for_kind(error_kind).unwrap_or(severity_color);
    let icon = colorize(icon, severity_color, use_color);
    let label = colorize(label, severity_color, use_color);
    let kind_label = colorize(error_kind, kind_color, use_color);

    // Every line of the frame carries the gutter explicitly, and the frame is
    // assembled line by line rather than as one `\`-continued literal.
    //
    // It used to be a single `format!` whose lines ended in `\`. A `\` at
    // end-of-line strips the newline *and all leading whitespace on the next
    // line*, so the four spaces written in front of the caret row were deleted
    // at compile time: the source row got a six-character prefix (`  2 │ `)
    // and the caret row got two (`│ `), putting every caret four columns left
    // of what it pointed at (deka#441). Indentation a `\` can silently eat is
    // not a safe way to align anything, so there is none here to eat.
    //
    // The width tracks the line number so the frame does not drift on files
    // with four-digit lines -- the same bug waiting to happen again.
    let number = line_num.to_string();
    let gutter_width = number.len().max(3);
    let gutter = " ".repeat(gutter_width);

    let caret_pad = " ".repeat(col_num.saturating_sub(1));
    let carets = "^".repeat(underline_length);

    let mut out = String::new();
    out.push('\n');
    out.push_str(&format!("{label}\n"));
    out.push_str(&format!("{icon} {kind_label}\n"));
    out.push('\n');
    out.push_str(&format!("{gutter} ┌─ {file_path}:{line_num}:{col_num}\n"));
    out.push_str(&format!("{gutter} │\n"));
    out.push_str(&format!("{number:>gutter_width$} │ {error_line}\n"));
    out.push_str(&format!("{gutter} │ {caret_pad}{carets} {message}\n"));
    out.push_str(&format!("{gutter} │\n"));

    let help_trimmed = help.trim();
    let suggestion = extra.as_ref().and_then(|extra| {
        extra
            .suggestion
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string())
    });
    let mut show_help = !help_trimmed.is_empty();
    if let Some(suggestion_value) = &suggestion {
        if suggestion_value == help_trimmed {
            show_help = false;
        }
    }
    if show_help {
        out.push_str(&format!("{gutter} = help: {}\n", help));
    }
    if let Some(suggestion_value) = suggestion {
        let suggestion_label = colorize("suggestion", "\x1b[36m", use_color);
        out.push_str(&format!("{gutter} = {}: {}\n", suggestion_label, suggestion_value));
    }
    if let Some(link) = extra.as_ref().and_then(|extra| extra.docs_link.clone()) {
        let docs_label = colorize("docs", "\x1b[36m", use_color);
        out.push_str(&format!("{gutter} = {}: {}\n", docs_label, link));
    }
    out.push_str(&format!("{gutter} │\n{gutter} └─\n"));
    out
}

fn use_color_output() -> bool {
    if cfg!(test) {
        return false;
    }
    if std::env::var("NO_COLOR").is_ok() || std::env::var("DEKA_NO_COLOR").is_ok() {
        return false;
    }
    if let Ok(term) = std::env::var("TERM") {
        if term == "dumb" {
            return false;
        }
    }
    true
}

fn colorize(text: &str, color: &str, enabled: bool) -> String {
    if !enabled {
        return text.to_string();
    }
    format!("{color}{text}\x1b[0m")
}

fn color_for_kind(kind: &str) -> Option<&'static str> {
    let lower = kind.to_ascii_lowercase();
    if lower.contains("syntax") || lower.contains("token") {
        return Some("\x1b[31m");
    }
    if lower.contains("type") {
        return Some("\x1b[35m");
    }
    if lower.contains("import") || lower.contains("export") || lower.contains("module") {
        return Some("\x1b[36m");
    }
    if lower.contains("wasm") {
        return Some("\x1b[33m");
    }
    if lower.contains("jsx") {
        return Some("\x1b[32m");
    }
    if lower.contains("struct") || lower.contains("enum") || lower.contains("pattern") {
        return Some("\x1b[34m");
    }
    if lower.contains("null") || lower.contains("exception") || lower.contains("namespace") {
        return Some("\x1b[33m");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Column index of the first `needle`, counted in characters.
    ///
    /// `str::find` returns a byte index and `│` is three bytes in UTF-8, so
    /// byte offsets are not columns.
    fn char_col(line: &str, needle: char) -> usize {
        line.chars()
            .position(|c| c == needle)
            .unwrap_or_else(|| panic!("no `{needle}` in {line:?}"))
    }

    /// deka#441: the caret row was four columns left of what it pointed at, for
    /// weeks, because the tests only asserted `contains("^")`. These assert the
    /// geometry instead -- that the gutters line up and that the character
    /// under the caret is the one the column names.
    #[test]
    fn caret_points_at_the_column_it_names() {
        let code = "interface Logger {\n  fn log(...msgs: string[]) void\n}\n";
        let error = format_validation_error(
            code,
            "rest.ds",
            "Syntax Error",
            2,
            10,
            "expected identifier",
            "",
            3,
        );

        let lines: Vec<&str> = error.lines().collect();
        let source_row = lines
            .iter()
            .find(|line| line.contains("fn log("))
            .expect("source row");
        let caret_row = lines
            .iter()
            .find(|line| line.contains('^'))
            .expect("caret row");

        let source_bar = char_col(source_row, '│');
        let caret_bar = char_col(caret_row, '│');
        assert_eq!(
            source_bar, caret_bar,
            "gutter bars must align:\n{source_row}\n{caret_row}"
        );

        // Text begins two characters past the bar: `│` then one space.
        let source_text: String = source_row.chars().skip(source_bar + 2).collect();
        let caret_offset = char_col(caret_row, '^') - (caret_bar + 2);
        assert_eq!(
            caret_offset,
            10 - 1,
            "caret must sit at column 10:\n{source_row}\n{caret_row}"
        );
        assert_eq!(
            source_text.chars().nth(caret_offset),
            Some('.'),
            "column 10 of that line is the first `.` of `...`"
        );
    }

    #[test]
    fn the_frame_holds_for_four_digit_line_numbers() {
        let mut code = String::new();
        for _ in 0..1233 {
            code.push_str("let filler = 1\n");
        }
        code.push_str("  let x = 2\n");
        let error = format_validation_error(
            &code,
            "big.ds",
            "Type Error",
            1234,
            7,
            "nope",
            "",
            1,
        );

        let lines: Vec<&str> = error.lines().collect();
        let source_row = lines
            .iter()
            .find(|line| line.contains("let x = 2"))
            .expect("source row");
        let caret_row = lines
            .iter()
            .find(|line| line.contains('^'))
            .expect("caret row");

        assert_eq!(
            char_col(source_row, '│'),
            char_col(caret_row, '│'),
            "a wider line number must widen the whole frame:\n{source_row}\n{caret_row}"
        );
        let bar = char_col(caret_row, '│');
        let source_text: String = source_row.chars().skip(bar + 2).collect();
        let caret_offset = char_col(caret_row, '^') - (bar + 2);
        assert_eq!(source_text.chars().nth(caret_offset), Some('x'));
    }

    #[test]
    fn every_framed_row_shares_one_gutter() {
        let error = format_validation_error(
            "const a = 1\n",
            "f.ds",
            "Type Error",
            1,
            7,
            "message",
            "a help line",
            1,
        );

        let bars: Vec<usize> = error
            .lines()
            .filter(|line| line.contains('│'))
            .map(|line| char_col(line, '│'))
            .collect();
        assert!(bars.len() >= 3, "expected several framed rows: {error}");
        assert!(
            bars.windows(2).all(|w| w[0] == w[1]),
            "gutter bars drift: {bars:?}\n{error}"
        );

        let corners: Vec<usize> = error
            .lines()
            .filter(|line| line.contains('┌') || line.contains('└'))
            .map(|line| char_col(line, if line.contains('┌') { '┌' } else { '└' }))
            .collect();
        assert_eq!(corners.len(), 2, "expected both corners: {error}");
        assert!(
            corners.iter().all(|corner| *corner == bars[0]),
            "corners must sit on the gutter: {corners:?} vs {}\n{error}",
            bars[0]
        );
    }

    #[test]
    fn test_basic_error_formatting() {
        let code = "import { serve } from 'deka/invalid';";
        let error = format_validation_error(
            code,
            "test.ts",
            "Invalid Import",
            1,
            26,
            "Module 'deka/invalid' not found",
            "Available modules: deka, deka/router",
            12,
        );

        assert!(error.contains("❌ Invalid Import"));
        assert!(error.contains("test.ts:1:26"));
        assert!(error.contains("deka/invalid"));
        assert!(error.contains("^^^^^^^^^^^^"));
        assert!(error.contains("= help: Available modules"));
    }

    #[test]
    fn test_multiline_code() {
        let code = "line 1\nline 2 with error\nline 3";
        let error = format_validation_error(
            code,
            "multi.ts",
            "Type Error",
            2,
            7,
            "Something wrong here",
            "Fix it like this",
            4,
        );

        assert!(error.contains("❌ Type Error"));
        assert!(error.contains("multi.ts:2:7"));
        assert!(error.contains("line 2 with error"));
        assert!(error.contains("^^^^"));
    }

    #[test]
    fn test_underline_length_minimum() {
        let error = format_validation_error("test", "test.ts", "Error", 1, 1, "msg", "help", 0);

        assert!(error.contains("^"));
    }

    #[test]
    fn test_out_of_bounds_line() {
        let error = format_validation_error(
            "only one line",
            "test.ts",
            "Error",
            999,
            1,
            "msg",
            "help",
            5,
        );

        assert!(error.contains("❌ Error"));
        assert!(error.contains("999 │"));
    }
}
