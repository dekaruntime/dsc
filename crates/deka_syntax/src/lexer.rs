//! DekaScript lexer (Compiler v2).

use crate::ast::{Pos, Span};
use crate::diagnostics::{Diagnostic, Severity};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    // Literals
    Number,
    BigInt,
    String,
    BacktickString,
    RawJs,
    True,
    False,
    None,

    // Identifiers
    Identifier,

    // Keywords
    Const,
    Let,
    Mut,
    Function,
    Fn,
    Struct,
    Enum,
    Interface,
    Type,
    Alias,
    Import,
    Export,
    From,
    As,
    If,
    Else,
    For,
    Of,
    Return,
    Match,
    Unsafe,
    Bridge,
    Await,
    Async,
    Super,
    Pub,
    Break,
    Continue,

    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    Eq,
    EqEq,
    TripleEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Not,
    Ampersand,
    /// `|>`, the pipeline operator.
    Pipe,
    /// A bare `|`. Used to separate or-pattern alternatives (deka#446); it was
    /// a lex error before that, since DekaScript has no bitwise or.
    Bar,
    Caret,
    Shl,
    Shr,

    // Delimiters
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Semicolon,
    Colon,
    DoubleColon,
    Dot,
    Arrow,
    FatArrow,
    Question,
    Spread,

    // JSX
    LtJsx,
    GtJsx,
    SlashJsx,

    // Special
    Newline,
    Comment,
    Eof,
    Error,
}

#[derive(Clone, Debug)]
pub struct Token<'a> {
    pub kind: TokenKind,
    pub text: &'a str,
    pub span: Span,
}

pub struct Lexer<'a> {
    source: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: usize,
    column: usize,
    diagnostics: Vec<Diagnostic>,
    /// Set to true immediately after lexing the `unsafe` keyword so the next
    /// non-whitespace token can switch the lexer into raw-JS mode for the
    /// following `{ ... }` block.
    unsafe_expect_brace: bool,
    /// Byte offset one past the `>` closing an `unsafe<T>` type argument.
    /// While `pos` is below it the pending-brace check is suspended so the
    /// type argument lexes as ordinary tokens (deka#460).
    unsafe_type_end: usize,
    /// >0 while scanning the body of an `unsafe { }` block. The lexer emits a
    /// single `RawJs` token for the body and returns to normal mode at the
    /// matching `}`.
    raw_depth: u32,
    raw_start_pos: Pos,
    raw_start_byte: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            pos: 0,
            line: 1,
            column: 1,
            diagnostics: Vec::new(),
            unsafe_expect_brace: false,
            unsafe_type_end: 0,
            raw_depth: 0,
            raw_start_pos: Pos { line: 1, column: 1 },
            raw_start_byte: 0,
        }
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    fn peek(&self, offset: usize) -> Option<char> {
        self.bytes.get(self.pos + offset).map(|&b| b as char)
    }

    fn current(&self) -> Option<char> {
        self.peek(0)
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.current()?;
        self.pos += 1;
        if ch == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(ch)
    }

    fn pos_at(&self) -> Pos {
        Pos {
            line: self.line,
            column: self.column,
        }
    }

    fn span_from(&self, start: Pos, start_byte: usize) -> Span {
        Span {
            start,
            end: self.pos_at(),
            byte_start: start_byte,
            byte_end: self.pos,
        }
    }

    fn error(&mut self, message: impl Into<String>) -> Token<'a> {
        let start = self.pos_at();
        let start_byte = self.pos;
        let ch = self.advance();
        let text = if ch.is_some() {
            &self.source[start_byte..self.pos]
        } else {
            ""
        };
        self.diagnostics.push(Diagnostic {
            severity: Severity::Error,
            line: start.line,
            column: start.column,
            message: message.into(),
            help_text: None,
            underline_length: 1,
        });
        Token {
            kind: TokenKind::Error,
            text,
            span: self.span_from(start, start_byte),
        }
    }

    fn skip_whitespace(&mut self) {
        while let Some(ch) = self.current() {
            if ch == ' ' || ch == '\t' || ch == '\r' {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn read_string(&mut self) -> Token<'a> {
        let start = self.pos_at();
        let start_byte = self.pos;
        let quote = self.current().unwrap();
        self.advance(); // opening quote
        let start_pos = self.pos;
        loop {
            match self.current() {
                None => {
                    self.diagnostics.push(Diagnostic {
                        severity: Severity::Error,
                        line: start.line,
                        column: start.column,
                        message: "unterminated string literal".into(),
                        help_text: Some("add a closing quote".into()),
                        underline_length: 1,
                    });
                    break;
                }
                Some('\\') => {
                    self.advance();
                    self.advance();
                }
                Some(c) if c == quote => {
                    self.advance();
                    break;
                }
                Some(_) => {
                    self.advance();
                }
            }
        }
        let text = &self.source[start_pos..self.pos - 1];
        Token {
            kind: TokenKind::String,
            text,
            span: self.span_from(start, start_byte),
        }
    }

    /// Read a backtick-delimited raw string. DS does not currently support
    /// template literal interpolation, but backtick strings appear inside
    /// `unsafe { }` blocks as raw JavaScript, so the lexer must consume them
    /// as a single token without emitting an error.
    fn read_backtick_string(&mut self) -> Token<'a> {
        let start = self.pos_at();
        let start_byte = self.pos;
        self.advance(); // opening backtick
        let start_pos = self.pos;
        loop {
            match self.current() {
                None => {
                    self.diagnostics.push(Diagnostic {
                        severity: Severity::Error,
                        line: start.line,
                        column: start.column,
                        message: "unterminated backtick string".into(),
                        help_text: Some("add a closing backtick".into()),
                        underline_length: 1,
                    });
                    break;
                }
                Some('\\') => {
                    self.advance();
                    self.advance();
                }
                Some('`') => {
                    self.advance();
                    break;
                }
                Some(_) => {
                    self.advance();
                }
            }
        }
        let text = &self.source[start_pos..self.pos - 1];
        Token {
            kind: TokenKind::BacktickString,
            text,
            span: self.span_from(start, start_byte),
        }
    }

    fn read_number(&mut self) -> Token<'a> {
        let start = self.pos_at();
        let start_byte = self.pos;
        let start_pos = self.pos;
        let mut saw_dot = false;
        let mut saw_exp = false;
        while let Some(ch) = self.current() {
            match ch {
                '0'..='9' => {
                    self.advance();
                }
                '_' => {
                    self.advance();
                }
                '.' if !saw_dot && !saw_exp && matches!(self.peek(1), Some('0'..='9')) => {
                    saw_dot = true;
                    self.advance();
                }
                'e' | 'E' if !saw_exp => {
                    let next = self.peek(1);
                    let has_digit = matches!(next, Some('0'..='9'));
                    let has_signed_digit = matches!(next, Some('+' | '-'))
                        && matches!(self.peek(2), Some('0'..='9'));
                    if has_digit || has_signed_digit {
                        saw_exp = true;
                        self.advance();
                        if matches!(self.current(), Some('+' | '-')) {
                            self.advance();
                        }
                        while matches!(self.current(), Some('0'..='9' | '_')) {
                            self.advance();
                        }
                    } else {
                        break;
                    }
                }
                _ => break,
            };
        }
        let text = &self.source[start_pos..self.pos];
        // Validate underscores: must sit between two digits.
        let mut prev = '\0';
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '_' {
                if !prev.is_ascii_digit() || !chars.peek().map_or(false, |c| c.is_ascii_digit()) {
                    self.diagnostics.push(Diagnostic::error(
                        start.line,
                        start.column,
                        "invalid placement of underscore in numeric literal",
                    ));
                    break;
                }
            }
            prev = ch;
        }
        let kind = if self.current() == Some('n') {
            self.advance();
            TokenKind::BigInt
        } else {
            TokenKind::Number
        };
        let text = &self.source[start_pos..self.pos];
        // Reject octal-style leading-zero integers like `08` or `00`.
        // Decimal `0` and floats are still allowed.
        if text.len() > 1
            && text.starts_with('0')
            && !saw_dot
            && !matches!(
                text.chars().nth(1),
                Some('x' | 'X' | 'b' | 'B' | 'o' | 'O')
            )
        {
            self.diagnostics.push(Diagnostic::error(
                start.line,
                start.column,
                "leading-zero octal-style integers are not allowed in DekaScript",
            ));
        }
        Token {
            kind,
            text,
            span: self.span_from(start, start_byte),
        }
    }

    fn read_identifier(&mut self) -> Token<'a> {
        let start = self.pos_at();
        let start_byte = self.pos;
        let start_pos = self.pos;
        while let Some(ch) = self.current() {
            if ch.is_alphanumeric() || ch == '_' {
                self.advance();
            } else {
                break;
            }
        }
        let text = &self.source[start_pos..self.pos];
        let kind = match text {
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "None" => TokenKind::None,
            "const" => TokenKind::Const,
            "let" => TokenKind::Let,
            "mut" => TokenKind::Mut,
            "function" => TokenKind::Function,
            "fn" => TokenKind::Fn,
            "struct" => TokenKind::Struct,
            "enum" => TokenKind::Enum,
            "interface" => TokenKind::Interface,
            "type" => TokenKind::Type,
            "alias" => TokenKind::Alias,
            "import" => TokenKind::Import,
            "export" => TokenKind::Export,
            "from" => TokenKind::From,
            "as" => TokenKind::As,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "for" => TokenKind::For,
            "of" => TokenKind::Of,
            "return" => TokenKind::Return,
            "match" => TokenKind::Match,
            "unsafe" => TokenKind::Unsafe,
            "bridge" => TokenKind::Bridge,
            "await" => TokenKind::Await,
            "async" => TokenKind::Async,
            "super" => TokenKind::Super,
            "pub" => TokenKind::Pub,
            "break" => TokenKind::Break,
            "continue" => TokenKind::Continue,
            _ => TokenKind::Identifier,
        };
        if kind == TokenKind::Unsafe {
            self.unsafe_expect_brace = true;
        }
        Token {
            kind,
            text,
            span: self.span_from(start, start_byte),
        }
    }

    /// Scan the body of an `unsafe { ... }` block as raw JavaScript.
    ///
    /// The lexer enters this mode immediately after consuming the opening `{`.
    /// It tracks brace depth while ignoring braces inside JS strings, comments,
    /// regex literals and template literals, then emits a single `RawJs` token
    /// spanning the body (excluding the surrounding braces).
    fn read_raw_js_body(&mut self) -> Token<'a> {
        let start = self.raw_start_pos;
        let start_byte = self.raw_start_byte;
        while let Some(ch) = self.current() {
            match ch {
                '{' => {
                    self.raw_depth += 1;
                    self.advance();
                }
                '}' => {
                    if self.raw_depth == 1 {
                        break;
                    }
                    self.raw_depth -= 1;
                    self.advance();
                }
                '"' | '\'' => {
                    self.skip_string_literal(ch);
                }
                '`' => {
                    self.skip_template_literal();
                }
                '/' => {
                    match self.peek(1) {
                        Some('/') => self.skip_line_comment_raw(),
                        Some('*') => self.skip_block_comment_raw(),
                        _ => {
                            if self.looks_like_regex_start() {
                                self.skip_regex_literal();
                            } else {
                                self.advance();
                            }
                        }
                    }
                }
                _ => {
                    self.advance();
                }
            }
        }
        self.raw_depth = 0;
        let text = &self.source[start_byte..self.pos];
        Token {
            kind: TokenKind::RawJs,
            text,
            span: self.span_from(start, start_byte),
        }
    }

    /// Consume a single- or double-quoted string literal, including escaped
    /// quotes. Used only inside raw JS bodies.
    fn skip_string_literal(&mut self, quote: char) {
        self.advance(); // opening quote
        while let Some(ch) = self.current() {
            match ch {
                '\\' => {
                    self.advance();
                    self.advance();
                }
                c if c == quote => {
                    self.advance();
                    break;
                }
                _ => {
                    self.advance();
                }
            }
        }
    }

    /// Consume a backtick-delimited template literal, including nested
    /// `${...}` interpolations, as raw text.
    fn skip_template_literal(&mut self) {
        self.advance(); // opening backtick
        while let Some(ch) = self.current() {
            match ch {
                '\\' => {
                    self.advance();
                    self.advance();
                }
                '`' => {
                    self.advance();
                    break;
                }
                '$' if self.peek(1) == Some('{') => {
                    // Template interpolation: skip the `${` and scan the
                    // expression as raw JS until the matching `}`. This keeps
                    // brace counting correct for the *outer* unsafe block.
                    self.advance();
                    self.advance();
                    let mut depth = 1u32;
                    while let Some(c) = self.current() {
                        match c {
                            '{' => {
                                depth += 1;
                                self.advance();
                            }
                            '}' => {
                                depth -= 1;
                                self.advance();
                                if depth == 0 {
                                    break;
                                }
                            }
                            '"' | '\'' => self.skip_string_literal(c),
                            '`' => self.skip_template_literal(),
                            '/' => {
                                match self.peek(1) {
                                    Some('/') => self.skip_line_comment_raw(),
                                    Some('*') => self.skip_block_comment_raw(),
                                    _ => {
                                        if self.looks_like_regex_start() {
                                            self.skip_regex_literal();
                                        } else {
                                            self.advance();
                                        }
                                    }
                                }
                            }
                            _ => {
                                self.advance();
                            }
                        }
                    }
                }
                _ => {
                    self.advance();
                }
            }
        }
    }

    /// Heuristic: is the `/` at the current position likely the start of a JS
    /// regex literal rather than a division operator?
    fn looks_like_regex_start(&self) -> bool {
        let prev = self.prev_non_space_char();
        match prev {
            None => true,
            Some(c) => matches!(
                c,
                '(' | ',' | '=' | ':' | '[' | '{' | ';' | '!' | '&' | '|' | '+' | '-' | '*' | '%'
                    | '<' | '>' | '?' | '~' | '^'
            ),
        }
    }

    /// Walk backwards over whitespace to find the character immediately
    /// preceding the current `/` in the source.
    fn prev_non_space_char(&self) -> Option<char> {
        let mut i = self.pos;
        if i == 0 {
            return None;
        }
        loop {
            i -= 1;
            let c = self.source.as_bytes().get(i).copied()? as char;
            if !c.is_whitespace() {
                return Some(c);
            }
            if i == 0 {
                return None;
            }
        }
    }

    /// Consume a `/.../[flags]` regex literal from raw JS. Does not validate
    /// the regex; it only finds the closing `/` while respecting `[...]` classes.
    fn skip_regex_literal(&mut self) {
        self.advance(); // opening /
        let mut in_class = false;
        while let Some(ch) = self.current() {
            match ch {
                '\\' => {
                    self.advance();
                    self.advance();
                }
                '[' if !in_class => {
                    in_class = true;
                    self.advance();
                }
                ']' if in_class => {
                    in_class = false;
                    self.advance();
                }
                '/' if !in_class => {
                    self.advance();
                    break;
                }
                _ => {
                    self.advance();
                }
            }
        }
        // Regex flags.
        while let Some(ch) = self.current() {
            if ch.is_ascii_alphabetic() {
                self.advance();
            } else {
                break;
            }
        }
    }

    /// Consume a `//` line comment inside a raw JS body. Returns at newline or
    /// EOF without emitting diagnostics.
    fn skip_line_comment_raw(&mut self) {
        self.advance(); // first /
        self.advance(); // second /
        while let Some(ch) = self.current() {
            if ch == '\n' {
                break;
            }
            self.advance();
        }
    }

    /// Consume a `/* */` block comment inside a raw JS body. Returns at the
    /// closing `*/` or EOF without emitting diagnostics.
    fn skip_block_comment_raw(&mut self) {
        self.advance(); // /
        self.advance(); // *
        while let Some(ch) = self.current() {
            if ch == '*' && self.peek(1) == Some('/') {
                self.advance();
                self.advance();
                break;
            }
            self.advance();
        }
    }

    fn read_line_comment(&mut self) -> Token<'a> {
        // The caller already consumed the first `/`; the token spans both.
        let mut start = self.pos_at();
        start.column -= 1;
        let start_byte = self.pos - 1;
        let start_pos = self.pos - 1;
        self.advance(); // second /
        while let Some(ch) = self.current() {
            if ch == '\n' {
                break;
            }
            self.advance();
        }
        Token {
            kind: TokenKind::Comment,
            text: &self.source[start_pos..self.pos],
            span: self.span_from(start, start_byte),
        }
    }

    fn read_block_comment(&mut self) -> Token<'a> {
        // The caller already consumed the first `/`; the token spans both.
        let mut start = self.pos_at();
        start.column -= 1;
        let start_byte = self.pos - 1;
        let start_pos = self.pos - 1;
        self.advance(); // *
        let mut terminated = false;
        while let Some(ch) = self.current() {
            if ch == '*' && self.peek(1) == Some('/') {
                self.advance();
                self.advance();
                terminated = true;
                break;
            }
            self.advance();
        }
        if !terminated {
            self.diagnostics.push(Diagnostic {
                severity: Severity::Error,
                line: start.line,
                column: start.column,
                message: "unterminated block comment".into(),
                help_text: Some("add `*/` to close the comment".into()),
                underline_length: 2,
            });
        }
        Token {
            kind: TokenKind::Comment,
            text: &self.source[start_pos..self.pos],
            span: self.span_from(start, start_byte),
        }
    }

    /// Byte offset one past the `>` that closes the angle-bracket group
    /// starting at `self.pos`. Counts characters, so `>>` closes two levels.
    /// Returns the end of input if the group is unterminated -- the parser
    /// reports that as a diagnostic.
    fn matching_angle_end(&self) -> usize {
        let bytes = self.source.as_bytes();
        let mut depth = 0usize;
        let mut i = self.pos;
        while i < bytes.len() {
            match bytes[i] {
                b'<' => depth += 1,
                b'>' => {
                    depth -= 1;
                    if depth == 0 {
                        return i + 1;
                    }
                }
                // A `{` before the group closes means the type argument is
                // unterminated. Stop so the caller still expects a brace.
                b'{' => return i,
                _ => {}
            }
            i += 1;
        }
        bytes.len()
    }

    pub fn next_token(&mut self) -> Token<'a> {
        if self.raw_depth > 0 {
            return self.read_raw_js_body();
        }
        self.skip_whitespace();
        if self.unsafe_expect_brace && self.pos >= self.unsafe_type_end {
            if self.current() == Some('<') {
                // `unsafe<T> { ... }` (deka#460). Find the matching `>` by
                // scanning characters rather than tokens: a nested type ends
                // in `>>`, which the lexer would otherwise emit as a single
                // `Shr`. The pending-brace flag stays set, so the `{` after
                // the type argument still opens the raw-JS body.
                self.unsafe_type_end = self.matching_angle_end();
                // fall through and lex `<` as an ordinary token
            } else {
            self.unsafe_expect_brace = false;
            let start = self.pos_at();
            let start_byte = self.pos;
            if self.current() == Some('{') {
                self.advance();
                self.raw_depth = 1;
                self.raw_start_pos = self.pos_at();
                self.raw_start_byte = self.pos;
                return Token {
                    kind: TokenKind::LBrace,
                    text: "{",
                    span: self.span_from(start, start_byte),
                };
            } else {
                return self.error("expected `{` or `<` after `unsafe`");
            }
            }
        }
        let start = self.pos_at();
        let start_byte = self.pos;
        let ch = match self.current() {
            Some(c) => c,
            None => {
                return Token {
                    kind: TokenKind::Eof,
                    text: "",
                    span: Span {
                        start,
                        end: start,
                        byte_start: start_byte,
                        byte_end: start_byte,
                    },
                }
            }
        };

        match ch {
            '\n' => {
                self.advance();
                Token {
                    kind: TokenKind::Newline,
                    text: "\n",
                    span: self.span_from(start, start_byte),
                }
            }
            '"' | '\'' => self.read_string(),
            '`' => self.read_backtick_string(),
            '0'..='9' => self.read_number(),
            'a'..='z' | 'A'..='Z' | '_' => self.read_identifier(),
            '(' => {
                self.advance();
                Token {
                    kind: TokenKind::LParen,
                    text: "(",
                    span: self.span_from(start, start_byte),
                }
            }
            ')' => {
                self.advance();
                Token {
                    kind: TokenKind::RParen,
                    text: ")",
                    span: self.span_from(start, start_byte),
                }
            }
            '{' => {
                self.advance();
                Token {
                    kind: TokenKind::LBrace,
                    text: "{",
                    span: self.span_from(start, start_byte),
                }
            }
            '}' => {
                self.advance();
                Token {
                    kind: TokenKind::RBrace,
                    text: "}",
                    span: self.span_from(start, start_byte),
                }
            }
            '[' => {
                self.advance();
                Token {
                    kind: TokenKind::LBracket,
                    text: "[",
                    span: self.span_from(start, start_byte),
                }
            }
            ']' => {
                self.advance();
                Token {
                    kind: TokenKind::RBracket,
                    text: "]",
                    span: self.span_from(start, start_byte),
                }
            }
            ',' => {
                self.advance();
                Token {
                    kind: TokenKind::Comma,
                    text: ",",
                    span: self.span_from(start, start_byte),
                }
            }
            ';' => {
                self.advance();
                Token {
                    kind: TokenKind::Semicolon,
                    text: ";",
                    span: self.span_from(start, start_byte),
                }
            }
            ':' => {
                self.advance();
                if self.current() == Some(':') {
                    self.advance();
                    Token {
                        kind: TokenKind::DoubleColon,
                        text: "::",
                        span: self.span_from(start, start_byte),
                    }
                } else {
                    Token {
                        kind: TokenKind::Colon,
                        text: ":",
                        span: self.span_from(start, start_byte),
                    }
                }
            }
            '.' => {
                self.advance();
                if self.current() == Some('.') && self.peek(1) == Some('.') {
                    self.advance();
                    self.advance();
                    Token {
                        kind: TokenKind::Spread,
                        text: "...",
                        span: self.span_from(start, start_byte),
                    }
                } else {
                    Token {
                        kind: TokenKind::Dot,
                        text: ".",
                        span: self.span_from(start, start_byte),
                    }
                }
            }
            '+' => {
                self.advance();
                if self.current() == Some('=') {
                    self.advance();
                    Token {
                        kind: TokenKind::PlusEq,
                        text: "+=",
                        span: self.span_from(start, start_byte),
                    }
                } else {
                    Token {
                        kind: TokenKind::Plus,
                        text: "+",
                        span: self.span_from(start, start_byte),
                    }
                }
            }
            '-' => {
                self.advance();
                if self.current() == Some('>') {
                    self.advance();
                    Token {
                        kind: TokenKind::Arrow,
                        text: "->",
                        span: self.span_from(start, start_byte),
                    }
                } else if self.current() == Some('=') {
                    self.advance();
                    Token {
                        kind: TokenKind::MinusEq,
                        text: "-=",
                        span: self.span_from(start, start_byte),
                    }
                } else {
                    Token {
                        kind: TokenKind::Minus,
                        text: "-",
                        span: self.span_from(start, start_byte),
                    }
                }
            }
            '*' => {
                self.advance();
                if self.current() == Some('=') {
                    self.advance();
                    Token {
                        kind: TokenKind::StarEq,
                        text: "*=",
                        span: self.span_from(start, start_byte),
                    }
                } else {
                    Token {
                        kind: TokenKind::Star,
                        text: "*",
                        span: self.span_from(start, start_byte),
                    }
                }
            }
            '/' => {
                self.advance();
                match self.current() {
                    Some('/') => self.read_line_comment(),
                    Some('*') => self.read_block_comment(),
                    Some('=') => {
                        self.advance();
                        Token {
                            kind: TokenKind::SlashEq,
                            text: "/=",
                            span: self.span_from(start, start_byte),
                        }
                    }
                    _ => Token {
                        kind: TokenKind::Slash,
                        text: "/",
                        span: self.span_from(start, start_byte),
                    },
                }
            }
            '%' => {
                self.advance();
                if self.current() == Some('=') {
                    self.advance();
                    Token {
                        kind: TokenKind::PercentEq,
                        text: "%=",
                        span: self.span_from(start, start_byte),
                    }
                } else {
                    Token {
                        kind: TokenKind::Percent,
                        text: "%",
                        span: self.span_from(start, start_byte),
                    }
                }
            }
            '=' => {
                self.advance();
                if self.current() == Some('=') {
                    self.advance();
                    if self.current() == Some('=') {
                        self.advance();
                        Token {
                            kind: TokenKind::TripleEq,
                            text: "===",
                            span: self.span_from(start, start_byte),
                        }
                    } else {
                        Token {
                            kind: TokenKind::EqEq,
                            text: "==",
                            span: self.span_from(start, start_byte),
                        }
                    }
                } else if self.current() == Some('>') {
                    self.advance();
                    Token {
                        kind: TokenKind::FatArrow,
                        text: "=>",
                        span: self.span_from(start, start_byte),
                    }
                } else {
                    Token {
                        kind: TokenKind::Eq,
                        text: "=",
                        span: self.span_from(start, start_byte),
                    }
                }
            }
            '!' => {
                self.advance();
                if self.current() == Some('=') {
                    self.advance();
                    Token {
                        kind: TokenKind::NotEq,
                        text: "!=",
                        span: self.span_from(start, start_byte),
                    }
                } else {
                    Token {
                        kind: TokenKind::Not,
                        text: "!",
                        span: self.span_from(start, start_byte),
                    }
                }
            }
            '<' => {
                self.advance();
                if self.current() == Some('=') {
                    self.advance();
                    Token {
                        kind: TokenKind::Le,
                        text: "<=",
                        span: self.span_from(start, start_byte),
                    }
                } else if self.current() == Some('<') {
                    self.advance();
                    Token {
                        kind: TokenKind::Shl,
                        text: "<<",
                        span: self.span_from(start, start_byte),
                    }
                } else {
                    Token {
                        kind: TokenKind::Lt,
                        text: "<",
                        span: self.span_from(start, start_byte),
                    }
                }
            }
            '>' => {
                self.advance();
                if self.current() == Some('=') {
                    self.advance();
                    Token {
                        kind: TokenKind::Ge,
                        text: ">=",
                        span: self.span_from(start, start_byte),
                    }
                } else if self.current() == Some('>') {
                    self.advance();
                    Token {
                        kind: TokenKind::Shr,
                        text: ">>",
                        span: self.span_from(start, start_byte),
                    }
                } else {
                    Token {
                        kind: TokenKind::Gt,
                        text: ">",
                        span: self.span_from(start, start_byte),
                    }
                }
            }
            '&' => {
                self.advance();
                if self.current() == Some('&') {
                    self.advance();
                    Token {
                        kind: TokenKind::And,
                        text: "&&",
                        span: self.span_from(start, start_byte),
                    }
                } else {
                    Token {
                        kind: TokenKind::Ampersand,
                        text: "&",
                        span: self.span_from(start, start_byte),
                    }
                }
            }
            '|' => {
                self.advance();
                if self.current() == Some('|') {
                    self.advance();
                    Token {
                        kind: TokenKind::Or,
                        text: "||",
                        span: self.span_from(start, start_byte),
                    }
                } else if self.current() == Some('>') {
                    self.advance();
                    Token {
                        kind: TokenKind::Pipe,
                        text: "|>",
                        span: self.span_from(start, start_byte),
                    }
                } else {
                    Token {
                        kind: TokenKind::Bar,
                        text: "|",
                        span: self.span_from(start, start_byte),
                    }
                }
            }
            '^' => {
                self.advance();
                Token {
                    kind: TokenKind::Caret,
                    text: "^",
                    span: self.span_from(start, start_byte),
                }
            }
            '?' => {
                self.advance();
                Token {
                    kind: TokenKind::Question,
                    text: "?",
                    span: self.span_from(start, start_byte),
                }
            }
            _ => self.error(format!("unexpected character '{}'", ch)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_simple_tokens() {
        let mut lexer = Lexer::new("const x = 42;");
        let tokens = [
            TokenKind::Const,
            TokenKind::Identifier,
            TokenKind::Eq,
            TokenKind::Number,
            TokenKind::Semicolon,
            TokenKind::Eof,
        ];
        for expected in tokens {
            assert_eq!(lexer.next_token().kind, expected);
        }
    }

    #[test]
    fn lexes_string() {
        let mut lexer = Lexer::new("\"hello\"");
        let tok = lexer.next_token();
        assert_eq!(tok.kind, TokenKind::String);
        assert_eq!(tok.text, "hello");
    }

    #[test]
    fn lexes_operators() {
        let mut lexer = Lexer::new("== != <= >= => && ||");
        let kinds = [
            TokenKind::EqEq,
            TokenKind::NotEq,
            TokenKind::Le,
            TokenKind::Ge,
            TokenKind::FatArrow,
            TokenKind::And,
            TokenKind::Or,
            TokenKind::Eof,
        ];
        for expected in kinds {
            assert_eq!(lexer.next_token().kind, expected);
        }
    }

    #[test]
    fn unterminated_block_comment_emits_error() {
        let mut lexer = Lexer::new("/* unterminated");
        let tok = lexer.next_token();
        assert_eq!(tok.kind, TokenKind::Comment);
        assert!(
            lexer.diagnostics().iter().any(|d| d.message.contains("unterminated block comment")),
            "expected unterminated block comment error, got: {:?}",
            lexer.diagnostics()
        );
    }

    #[test]
    fn leading_zero_octal_integer_rejected() {
        let mut lexer = Lexer::new("08");
        let tok = lexer.next_token();
        assert_eq!(tok.kind, TokenKind::Number);
        assert!(
            lexer.diagnostics().iter().any(|d| d.message.contains("leading-zero octal-style")),
            "expected leading-zero octal error, got: {:?}",
            lexer.diagnostics()
        );
    }

    #[test]
    fn zero_and_float_still_allowed() {
        let mut lexer = Lexer::new("0 0.5");
        let kinds = [
            TokenKind::Number,
            TokenKind::Number,
            TokenKind::Eof,
        ];
        for expected in kinds {
            assert_eq!(lexer.next_token().kind, expected);
        }
        assert!(lexer.diagnostics().is_empty(), "{:?}", lexer.diagnostics());
    }

    #[test]
    fn lexes_scientific_notation() {
        for source in ["1e3", "1E3", "1e-3", "1.5e10", "1.5e-10"] {
            let mut lexer = Lexer::new(source);
            let tok = lexer.next_token();
            assert_eq!(tok.kind, TokenKind::Number, "failed for {}", source);
            assert!(lexer.diagnostics().is_empty(), "{:?}", lexer.diagnostics());
        }
    }

    #[test]
    fn lexes_numeric_underscores() {
        let mut lexer = Lexer::new("1_000_000");
        let tok = lexer.next_token();
        assert_eq!(tok.kind, TokenKind::Number);
        assert_eq!(tok.text, "1_000_000");
        assert!(lexer.diagnostics().is_empty(), "{:?}", lexer.diagnostics());
    }

    #[test]
    fn numeric_underscore_before_decimal_is_error() {
        let mut lexer = Lexer::new("1_.5");
        let tok = lexer.next_token();
        assert_eq!(tok.kind, TokenKind::Number);
        assert!(
            lexer.diagnostics().iter().any(|d| d.message.contains("invalid placement")),
            "expected invalid underscore placement error, got: {:?}",
            lexer.diagnostics()
        );
    }
}
