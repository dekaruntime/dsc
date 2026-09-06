//! # dsc-stdio
//!
//! Terminal output utilities for dsc.
//! Same format as deka: `[action] message`.
//!
//! ## Format
//!
//! ```text
//! [action] message
//! ```
//!
//! ## Usage
//!
//! ```rust
//! use stdio::{log, error, warn, success, fail};
//!
//! log("build", "compiling contract...");
//! success("build complete");
//! error("build", "compilation failed");
//! ```
//!
//! ## Log Levels
//!
//! Control output with `LOG_LEVEL` environment variable:
//! - `error` - Errors only
//! - `info` - Default (startup + important messages)
//! - `debug` - Verbose output

use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::env;
#[cfg(target_arch = "wasm32")]
use std::sync::Mutex;
use std::sync::OnceLock;

mod terrace_font;

const BRAND_BLUE: &str = "\x1b[38;5;39m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// Log level for tana services
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum LogLevel {
    Error = 0,
    Info = 1,
    Debug = 2,
}

impl LogLevel {
    #[cfg(not(target_arch = "wasm32"))]
    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "error" => LogLevel::Error,
            "debug" => LogLevel::Debug,
            _ => LogLevel::Info,
        }
    }
}

static LOG_LEVEL: OnceLock<LogLevel> = OnceLock::new();
#[cfg(target_arch = "wasm32")]
static CAPTURED_OUTPUT: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn emit_line(line: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(lock) = CAPTURED_OUTPUT.get() {
            if let Ok(mut guard) = lock.lock() {
                if let Some(buf) = guard.as_mut() {
                    buf.push_str(line);
                    buf.push('\n');
                    return;
                }
            }
        }
    }
    eprintln!("{}", line);
}

#[cfg(target_arch = "wasm32")]
pub fn begin_capture() {
    let lock = CAPTURED_OUTPUT.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = lock.lock() {
        *guard = Some(String::new());
    }
}

#[cfg(target_arch = "wasm32")]
pub fn end_capture() -> String {
    let lock = CAPTURED_OUTPUT.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = lock.lock() {
        return guard.take().unwrap_or_default();
    }
    String::new()
}

/// Get the current log level (cached from LOG_LEVEL env var)
pub fn log_level() -> LogLevel {
    *LOG_LEVEL.get_or_init(|| {
        #[cfg(target_arch = "wasm32")]
        {
            return LogLevel::Info;
        }
        #[cfg(not(target_arch = "wasm32"))]
        env::var("LOG_LEVEL")
            .map(|s| LogLevel::from_str(&s))
            .unwrap_or(LogLevel::Info)
    })
}

/// Check if debug logging is enabled
pub fn is_debug() -> bool {
    log_level() >= LogLevel::Debug
}

/// Check if info logging is enabled
pub fn is_info() -> bool {
    log_level() >= LogLevel::Info
}

// ============================================================
// Core logging functions (match @tananetwork/stdio API)
// ============================================================

/// Log an action with a message
/// Format: `[action] message`
///
/// # Example
/// ```
/// stdio::log("build", "compiling contract...");
/// // Output: [build] compiling contract...
/// ```
pub fn log(action: &str, message: &str) {
    if log_level() >= LogLevel::Info {
        emit_line(&format!("[{}] {}", action, message));
    }
}

/// Log an error
/// Format: `[action] message`
///
/// # Example
/// ```
/// stdio::error("build", "compilation failed");
/// // Output: [build] compilation failed
/// ```
pub fn error(action: &str, message: &str) {
    emit_line(&format!("[{}] {}", action, message));
}

/// Log a warning
/// Format: `[warn] message` or `[name] message`
///
/// # Example
/// ```
/// stdio::warn("cache", "stale entries detected");
/// // Output: [warn] [cache] stale entries detected
/// ```
pub fn warn(name: &str, message: &str) {
    emit_line(&format!("[warn] [{}] {}", name, message));
}

/// Log a simple warning without component name
/// Format: `[warn] message`
pub fn warn_simple(message: &str) {
    emit_line(&format!("[warn] {}", message));
}

/// Log a status line with success/failure indicator
/// Format: `[ok] message` or `[fail] message`
///
/// # Example
/// ```
/// stdio::status("database", "connected", true);
/// // Output: [ok] [database] connected
/// ```
pub fn status(name: &str, message: &str, ok: bool) {
    if ok {
        emit_line(&format!("[ok] [{}] {}", name, message));
    } else {
        emit_line(&format!("[fail] [{}] {}", name, message));
    }
}

/// Print a section header
///
/// # Example
/// ```
/// stdio::header("configuration");
/// // Output:
/// //
/// // configuration
/// // ----------------------------------------
/// ```
pub fn header(title: &str) {
    emit_line("");
    emit_line(title);
    emit_line(&"-".repeat(40));
}

/// Print a blank line
pub fn blank() {
    emit_line("");
}

/// Success message
/// Format: `[ok] message`
///
/// # Example
/// ```
/// stdio::success("build complete");
/// // Output: [ok] build complete
/// ```
pub fn success(message: &str) {
    emit_line(&format!("[ok] {}", message));
}

/// Failure message
/// Format: `[fail] message`
///
/// # Example
/// ```
/// stdio::fail("build failed");
/// // Output: [fail] build failed
/// ```
pub fn fail(message: &str) {
    emit_line(&format!("[fail] {}", message));
}

/// Info line with label
/// Format: `  label     value`
///
/// # Example
/// ```
/// stdio::info("port", "8506");
/// // Output:   port       8506
/// ```
pub fn info(label: &str, value: &str) {
    emit_line(&format!("  {:<10} {}", label, value));
}

/// Hint in subdued format
/// Format: `  message`
pub fn hint(message: &str) {
    emit_line(&format!("  {}", message));
}

/// Detail line with arrow
/// Format: `    -> message`
pub fn detail(message: &str) {
    emit_line(&format!("    -> {}", message));
}

/// Suggest a next step
/// Format: `  -> description: command`
///
/// # Example
/// ```
/// stdio::next_step("start the server", "npm run dev");
/// // Output:   -> start the server: npm run dev
/// ```
pub fn next_step(description: &str, command: &str) {
    emit_line(&format!("  -> {}: {}", description, command));
}

/// Diagnostic warning
/// Format: `[warn] [component] message`
pub fn diagnostic(component: &str, message: &str) {
    emit_line(&format!("[warn] [{}] {}", component, message));
}

// ============================================================
// Debug-level logging (only shown when LOG_LEVEL=debug)
// ============================================================

/// Debug log (only shown when LOG_LEVEL=debug)
///
/// # Example
/// ```
/// stdio::debug("cache", "hit for key: user_123");
/// // Output (only if LOG_LEVEL=debug): [cache] hit for key: user_123
/// ```
pub fn debug(action: &str, message: &str) {
    if log_level() >= LogLevel::Debug {
        emit_line(&format!("[{}] {}", action, message));
    }
}

/// Print a raw line (no formatting).
pub fn raw(message: &str) {
    emit_line(message);
}

/// Generate ASCII art banner in Tana brand style using the Terrace font.
pub fn ascii(text: &str) -> String {
    let font = FigFont::parse(terrace_font::TERRACE_FONT);
    let art = font
        .render(text)
        .unwrap_or_else(|| text.to_string())
        .trim_end()
        .to_string();
    format!("{BRAND_BLUE}{BOLD}{art}{RESET}")
}

// ============================================================
// Macros for convenient formatting
// ============================================================

/// Log with format string support
///
/// # Example
/// ```
/// stdio::logf!("build", "compiled {} files in {}ms", 42, 150);
/// ```
#[macro_export]
macro_rules! logf {
    ($action:expr, $($arg:tt)*) => {
        if $crate::log_level() >= $crate::LogLevel::Info {
            $crate::raw(&format!(concat!("[", $action, "] {}"), format!($($arg)*)));
        }
    };
}

/// Error with format string support
#[macro_export]
macro_rules! errorf {
    ($action:expr, $($arg:tt)*) => {
        $crate::raw(&format!(concat!("[", $action, "] {}"), format!($($arg)*)));
    };
}

/// Debug with format string support (only shown when LOG_LEVEL=debug)
#[macro_export]
macro_rules! debugf {
    ($action:expr, $($arg:tt)*) => {
        if $crate::log_level() >= $crate::LogLevel::Debug {
            $crate::raw(&format!(concat!("[", $action, "] {}"), format!($($arg)*)));
        }
    };
}

struct FigFont {
    height: usize,
    glyphs: HashMap<char, Vec<String>>,
}

impl FigFont {
    fn parse(source: &str) -> Self {
        let mut lines = source.lines();
        let header = lines.next().unwrap_or_default();
        let mut header_parts = header.split_whitespace();
        let signature = header_parts.next().unwrap_or_default();
        let hardblank = signature.chars().last().unwrap_or('$');
        let height = header_parts
            .next()
            .and_then(|part| part.parse::<usize>().ok())
            .unwrap_or(1);
        let comment_lines = header_parts
            .nth(3)
            .and_then(|part| part.parse::<usize>().ok())
            .unwrap_or(0);

        for _ in 0..comment_lines {
            lines.next();
        }

        let mut glyphs = HashMap::new();
        let mut endmark = '@';
        for codepoint in 32u8..=126u8 {
            let mut glyph = Vec::with_capacity(height);
            for i in 0..height {
                if let Some(line) = lines.next() {
                    let mut line = line.trim_end_matches('\r').to_string();
                    if i == 0 && !line.is_empty() {
                        if let Some(last) = line.chars().last() {
                            endmark = last;
                        }
                    }
                    while line.ends_with(endmark) {
                        line.pop();
                    }
                    if hardblank != ' ' {
                        line = line.replace(hardblank, " ");
                    }
                    glyph.push(line);
                } else {
                    glyph.push(String::new());
                }
            }
            glyphs.insert(codepoint as char, glyph);
        }

        Self { height, glyphs }
    }

    fn render(&self, text: &str) -> Option<String> {
        let mut lines = vec![String::new(); self.height];
        for ch in text.chars() {
            let glyph = self.glyphs.get(&ch).or_else(|| self.glyphs.get(&'?'));
            let glyph = glyph?;
            for (idx, line) in lines.iter_mut().enumerate() {
                if let Some(part) = glyph.get(idx) {
                    line.push_str(part);
                }
            }
        }
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_level_parsing() {
        assert_eq!(LogLevel::from_str("error"), LogLevel::Error);
        assert_eq!(LogLevel::from_str("info"), LogLevel::Info);
        assert_eq!(LogLevel::from_str("debug"), LogLevel::Debug);
        assert_eq!(LogLevel::from_str("INFO"), LogLevel::Info);
        assert_eq!(LogLevel::from_str("unknown"), LogLevel::Info);
    }

    #[test]
    fn test_log_level_ordering() {
        assert!(LogLevel::Error < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Debug);
    }
}
