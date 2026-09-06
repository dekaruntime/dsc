//! DekaScript source formatter.
//!
//! v2 is AST-aware: it parses the source with the `deka_syntax` compiler and
//! pretty-prints a canonical layout. If the source cannot be parsed, it is
//! returned unchanged so the formatter is safe to run on incomplete code.

use deka_syntax::ast::{
    UnwrapAlternative,
    BinOp, Embed, EnumCase, ExportDecl, ExportName, Expr, ForInit, ImportSpec,
    InterfaceMember, JsxElement, MatchArm, ObjectField, Param, Pattern, Program, Span,
    Stmt, StructField, TemplatePart, Type, TypeParam, UnOp,
};
use deka_syntax::parse;

/// Format a DekaScript source string.
///
/// Parsed programs are pretty-printed with 2-space indentation and an
/// 80-character soft line-width target. If parsing fails, the original source
/// is returned unchanged.
pub fn format_ds(source: &str) -> Result<String, String> {
    let arena = bumpalo::Bump::new();
    let result = parse::parse(source, &arena);
    if !result.errors.is_empty() || result.program.is_none() {
        // Formatter is conservative: don't try to repair broken code.
        return Ok(source.to_string());
    }
    let mut fmt = Formatter::new(source);
    fmt.fmt_program(result.program.unwrap());
    fmt.finish()
}

struct Formatter<'src> {
    source: &'src str,
    out: String,
    indent: usize,
    /// Tracks whether the last character written was a newline so we can emit
    /// indentation before the next non-whitespace token.
    at_line_start: bool,
    /// `//` line comments as (line, text), sorted by source line. The parser
    /// discards comment tokens, so the formatter re-lexes the source and
    /// reattaches comments positionally (deka#484).
    comments: Vec<(usize, String)>,
    comment_cursor: usize,
}

impl<'src> Formatter<'src> {
    fn new(source: &'src str) -> Self {
        Self {
            source,
            out: String::new(),
            indent: 0,
            at_line_start: true,
            comments: collect_line_comments(source),
            comment_cursor: 0,
        }
    }

    /// Emit every not-yet-emitted comment from a source line before `line`,
    /// each on its own line at the current indent. Statements are formatted
    /// in source order, so a single forward cursor stays consistent.
    fn emit_comments_before(&mut self, line: usize) {
        while self.comment_cursor < self.comments.len()
            && self.comments[self.comment_cursor].0 < line
        {
            let text = self.comments[self.comment_cursor].1.clone();
            self.write(&text);
            self.newline();
            self.comment_cursor += 1;
        }
    }

    /// Line of the first pending comment before `line`, if any. The
    /// blank-line separator measures the gap to this rather than to the next
    /// statement, because a statement span swallows trailing comments and
    /// would otherwise hide the gap (deka#484).
    fn pending_comment_line(&self, before_line: usize) -> Option<usize> {
        self.comments
            .get(self.comment_cursor)
            .map(|(line, _)| *line)
            .filter(|line| *line < before_line)
    }

    fn finish(mut self) -> Result<String, String> {
        // Trim trailing blank lines, then ensure exactly one trailing newline.
        while self.out.ends_with("\n\n") {
            self.out.pop();
        }
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        Ok(self.out)
    }

    // --- output helpers ----------------------------------------------------

    fn write(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if self.at_line_start && s != "\n" {
            for _ in 0..self.indent {
                self.out.push_str("  ");
            }
            self.at_line_start = false;
        }
        self.out.push_str(s);
        if s.ends_with('\n') {
            self.at_line_start = true;
        }
    }

    fn newline(&mut self) {
        self.out.push('\n');
        self.at_line_start = true;
    }

    fn indented<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        self.indent += 1;
        let result = f(self);
        self.indent -= 1;
        result
    }

    fn span_start_line(&self, span: Span) -> usize {
        span.start.line
    }

    fn stmt_start_line(&self, stmt: &Stmt<'_>) -> usize {
        self.span_start_line(stmt_span(stmt))
    }

    /// The line a statement's content actually ends on. Statement spans
    /// extend through trailing blank lines (the parser consumes them before
    /// closing the span) and through trailing standalone comments, so
    /// `span.end.line` can point at the next statement's line — walk back
    /// over trailing whitespace and whole-line comments instead.
    fn stmt_end_line(&self, stmt: &Stmt<'_>) -> usize {
        let span = stmt_span(stmt);
        let bytes = self.source.as_bytes();
        let mut end = span.byte_end.min(bytes.len());
        loop {
            while end > span.byte_start && matches!(bytes[end - 1], b' ' | b'\t' | b'\r' | b'\n') {
                end -= 1;
            }
            // A trailing line whose content is entirely a `//` comment is not
            // part of the statement either; skip it and keep walking.
            let line_start = self.source[..end].rfind('\n').map(|i| i + 1).unwrap_or(0);
            if self.source[line_start..end].trim_start().starts_with("//") {
                end = line_start.saturating_sub(1);
                continue;
            }
            break;
        }
        self.source[..end].matches('\n').count() + 1
    }

    /// Preserve a blank line between statements when the source had one.
    /// `prev_end_line` is the line the previous statement ENDS on, not where
    /// it starts: comparing start lines treats every multi-line statement as
    /// if a blank line followed it, so each reformat inserted another blank
    /// line and the formatter never reached a fixed point (deka#477).
    fn emit_stmt_separator(&mut self, prev_end_line: usize, next_start_line: usize) {
        if next_start_line > prev_end_line + 1 {
            self.write("\n\n");
        } else {
            self.newline();
        }
    }

    // --- program & statements ----------------------------------------------

    fn fmt_program(&mut self, program: Program<'_>) {
        let mut first = true;
        let mut prev_end_line: Option<usize> = None;
        for stmt in program.statements {
            if matches!(stmt, Stmt::Empty { .. }) {
                continue;
            }
            let next_line = self.stmt_start_line(stmt);
            if !first {
                let gap_to = self.pending_comment_line(next_line).unwrap_or(next_line);
                self.emit_stmt_separator(prev_end_line.unwrap_or(0), gap_to);
            }
            self.emit_comments_before(next_line);
            first = false;
            self.fmt_stmt(stmt);
            prev_end_line = Some(self.stmt_end_line(stmt));
        }
        // Comments after the last statement still belong to the file.
        self.emit_comments_before(usize::MAX);
    }

    fn fmt_stmt(&mut self, stmt: &Stmt<'_>) {
        let stmt_end_line = self.stmt_end_line(stmt);
        match stmt {
            Stmt::Export { decl, .. } => self.fmt_export_decl(decl, stmt_end_line),
            Stmt::Import {
                specifiers,
                source,
                ..
            } => {
                self.write("import ");
                if specifiers.is_empty() {
                    self.write("\"");
                    self.write(source);
                    self.write("\"");
                } else {
                    self.write("{ ");
                    let parts: Vec<String> = specifiers
                        .iter()
                        .map(|s| import_spec_to_string(s))
                        .collect();
                    self.write(&parts.join(", "));
                    self.write(" } from \"");
                    self.write(source);
                    self.write("\"");
                }
            }
            Stmt::Const { name, ty, value, .. } => {
                self.write("const ");
                self.write(name);
                if let Some(ty) = ty {
                    self.write(": ");
                    self.fmt_type(ty);
                }
                self.write(" = ");
                self.fmt_expr(value);
            }
            Stmt::Let { name, ty, value, .. } => {
                self.write("let ");
                self.write(name);
                if let Some(ty) = ty {
                    self.write(": ");
                    self.fmt_type(ty);
                }
                self.write(" = ");
                self.fmt_expr(value);
            }
            // `deka fmt` is compile (RFD 28), so a construct the formatter
            // cannot print is a construct nobody can use.
            Stmt::UnwrapLet {
                name,
                ty,
                is_const,
                scrutinee,
                alternative,
                ..
            } => {
                self.write(if *is_const { "const " } else { "let " });
                self.write(name);
                if let Some(ty) = ty {
                    self.write(": ");
                    self.fmt_type(ty);
                }
                self.write(" = unwrap(");
                self.fmt_expr(scrutinee);
                match alternative {
                    UnwrapAlternative::Block(stmts) => {
                        self.write(") or {");
                        self.newline();
                        self.indent += 1;
                        for inner in stmts.iter() {
                            self.emit_comments_before(self.stmt_start_line(inner));
                            self.fmt_stmt(inner);
                            self.newline();
                        }
                        self.emit_comments_before(stmt_end_line);
                        self.indent -= 1;
                        self.write("}");
                    }
                    UnwrapAlternative::Match(arms) => {
                        self.write(") or match {");
                        self.newline();
                        self.indent += 1;
                        for arm in arms.iter() {
                            self.write(&pattern_to_string(&arm.pattern));
                            self.write(" => ");
                            self.fmt_expr(&arm.body);
                            self.write(",");
                            self.newline();
                        }
                        self.indent -= 1;
                        self.write("}");
                    }
                }
            }
            Stmt::Function {
                name,
                type_params,
                params,
                return_type,
                body,
                is_async,
                ..
            } => {
                self.fmt_fn_sig(*is_async, Some(name), type_params, params, return_type.as_ref());
                self.write(" ");
                self.fmt_block(body, stmt_end_line);
            }
            Stmt::ReceiverMethod {
                receiver_type,
                receiver_name,
                receiver_mutable,
                name,
                type_params,
                params,
                return_type,
                body,
                is_async,
                ..
            } => {
                self.fmt_receiver_method_sig(
                    *is_async,
                    name,
                    receiver_name,
                    *receiver_mutable,
                    receiver_type,
                    type_params,
                    params,
                    return_type.as_ref(),
                );
                self.write(" ");
                self.fmt_block(body, stmt_end_line);
            }
            Stmt::Struct {
                name,
                type_params,
                fields,
                embeds,
                is_super,
                ..
            } => {
                if *is_super {
                    self.write("super ");
                }
                self.write("struct ");
                self.write(name);
                if !type_params.is_empty() {
                    self.write("<");
                    self.fmt_type_param_list(type_params);
                    self.write(">");
                }
                self.write(" {");
                if !fields.is_empty() || !embeds.is_empty() {
                    self.newline();
                    self.indented(|this| {
                        this.fmt_struct_body(embeds, fields);
                        this.newline();
                    });
                    self.write("}");
                } else {
                    self.write("}");
                }
            }
            Stmt::Enum { name, type_params, cases, is_super, .. } => {
                if *is_super {
                    self.write("super ");
                }
                self.write("enum ");
                self.write(name);
                if !type_params.is_empty() {
                    self.write("<");
                    self.fmt_type_param_list(type_params);
                    self.write(">");
                }
                self.write(" {");
                if !cases.is_empty() {
                    self.newline();
                    self.indented(|this| {
                        this.fmt_enum_cases(cases);
                        this.newline();
                    });
                    self.write("}");
                } else {
                    self.write("}");
                }
            }
            Stmt::TypeAlias {
                name,
                type_params,
                value,
                ..
            } => {
                self.write("alias ");
                self.write(name);
                if !type_params.is_empty() {
                    self.write("<");
                    self.fmt_type_param_list(type_params);
                    self.write(">");
                }
                self.write(" = ");
                self.fmt_type(value);
            }
            Stmt::Newtype { name, repr, .. } => {
                self.write("type ");
                self.write(name);
                self.write(" ");
                self.write(match repr {
                    deka_syntax::NewtypeRepr::Number => "number",
                    deka_syntax::NewtypeRepr::String => "string",
                    deka_syntax::NewtypeRepr::Bool => "bool",
                });
            }
            Stmt::Interface {
                name,
                type_params,
                members,
                ..
            } => {
                self.write("interface ");
                self.write(name);
                if !type_params.is_empty() {
                    self.write("<");
                    self.fmt_type_param_list(type_params);
                    self.write(">");
                }
                self.write(" {");
                if !members.is_empty() {
                    self.newline();
                    self.indented(|this| {
                        this.fmt_interface_members(members);
                        this.newline();
                    });
                    self.write("}");
                } else {
                    self.write("}");
                }
            }
            Stmt::Expr { expr, .. } => {
                self.fmt_expr(expr);
            }
            Stmt::Return { value: None, .. } => self.write("return"),
            Stmt::Return { value: Some(value), .. } => {
                self.write("return ");
                self.fmt_expr(value);
            }
            Stmt::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.write("if (");
                self.fmt_expr(condition);
                self.write(") ");
                self.fmt_block(then_body, stmt_end_line);
                if !else_body.is_empty() {
                    self.write(" else ");
                    if else_body.len() == 1 && matches!(else_body[0], Stmt::If { .. }) {
                        self.fmt_stmt(&else_body[0]);
                    } else {
                        self.fmt_block(else_body, stmt_end_line);
                    }
                }
            }
            Stmt::Block { body, .. } => self.fmt_block(body, stmt_end_line),
            Stmt::For {
                init,
                condition,
                step,
                body,
                ..
            } => {
                self.write("for (");
                if let Some(init) = init {
                    self.fmt_for_init(init);
                }
                self.write("; ");
                if let Some(condition) = condition {
                    self.fmt_expr(condition);
                }
                self.write("; ");
                if let Some(step) = step {
                    self.fmt_expr(step);
                }
                self.write(") ");
                self.fmt_block(body, stmt_end_line);
            }
            Stmt::ForOf {
                name,
                is_const,
                iterable,
                body,
                ..
            } => {
                self.write("for (");
                if *is_const {
                    self.write("const ");
                } else {
                    self.write("let ");
                }
                self.write(name);
                self.write(" of ");
                self.fmt_expr(iterable);
                self.write(") ");
                self.fmt_block(body, stmt_end_line);
            }
            Stmt::Break { .. } => self.write("break"),
            Stmt::Continue { .. } => self.write("continue"),
            Stmt::Empty { .. } => {}
        }
    }

    fn fmt_block(&mut self, stmts: &[Stmt<'_>], end_line: usize) {
        self.write("{");
        if stmts.is_empty() {
            self.write("}");
            return;
        }
        self.newline();
        self.indented(|this| {
            let mut first = true;
            let mut prev_end_line: Option<usize> = None;
            for stmt in stmts {
                if matches!(stmt, Stmt::Empty { .. }) {
                    continue;
                }
                let next_line = this.stmt_start_line(stmt);
                if !first {
                    let gap_to = this.pending_comment_line(next_line).unwrap_or(next_line);
                    this.emit_stmt_separator(prev_end_line.unwrap_or(0), gap_to);
                }
                this.emit_comments_before(next_line);
                first = false;
                this.fmt_stmt(stmt);
                prev_end_line = Some(this.stmt_end_line(stmt));
            }
            // Comments between the last statement and the closing brace.
            this.emit_comments_before(end_line);
        });
        self.newline();
        self.write("}");
    }

    fn fmt_for_init(&mut self, init: &ForInit<'_>) {
        match init {
            ForInit::Const { name, value } => {
                self.write("const ");
                self.write(name);
                self.write(" = ");
                self.fmt_expr(value);
            }
            ForInit::Let { name, value } => {
                self.write("let ");
                self.write(name);
                self.write(" = ");
                self.fmt_expr(value);
            }
            ForInit::Expr(expr) => self.fmt_expr(expr),
        }
    }

    fn fmt_export_decl(&mut self, decl: &ExportDecl<'_>, stmt_end_line: usize) {
        match decl {
            ExportDecl::Const { name, ty, value } => {
                self.write("export const ");
                self.write(name);
                if let Some(ty) = ty {
                    self.write(": ");
                    self.fmt_type(ty);
                }
                self.write(" = ");
                self.fmt_expr(value);
            }
            ExportDecl::Function {
                name,
                type_params,
                params,
                return_type,
                body,
                is_async,
            } => {
                self.write("export ");
                self.fmt_fn_sig(*is_async, Some(name), type_params, params, return_type.as_ref());
                self.write(" ");
                self.fmt_block(body, stmt_end_line);
            }
            ExportDecl::NamedGroup { names, source } => {
                self.write("export { ");
                let parts: Vec<String> = names.iter().map(export_name_to_string).collect();
                self.write(&parts.join(", "));
                self.write(" }");
                if let Some(source) = source {
                    self.write(" from ");
                    self.write("\"");
                    self.write(source);
                    self.write("\"");
                }
            }
        }
    }

    // --- struct / enum / interface bodies ----------------------------------

    fn fmt_struct_body(&mut self, embeds: &[Embed<'_>], fields: &[StructField<'_>]) {
        let mut first = true;
        for embed in embeds {
            if !first {
                self.newline();
            }
            first = false;
            self.write(&embed.name);
            self.write(";");
        }
        for field in fields {
            if !first {
                self.newline();
            }
            first = false;
            self.fmt_struct_field(field);
        }
    }

    fn fmt_struct_field(&mut self, field: &StructField<'_>) {
        self.write(field.name);
        if let Type::Option { inner, .. } = &field.ty {
            self.write("?: ");
            self.fmt_type(inner);
        } else {
            self.write(": ");
            self.fmt_type(&field.ty);
        }
        if let Some(default) = &field.default_value {
            self.write(" = ");
            self.fmt_expr(default);
        }
        self.write(";");
    }

    fn fmt_enum_cases(&mut self, cases: &[EnumCase<'_>]) {
        let mut first = true;
        for case in cases {
            if !first {
                self.newline();
            }
            first = false;
            self.write(case.name);
            if let Some(payload) = &case.payload {
                self.write("(");
                self.fmt_type(payload);
                self.write(")");
            }
            self.write(",");
        }
    }

    fn fmt_interface_members(&mut self, members: &[InterfaceMember<'_>]) {
        let mut first = true;
        for member in members {
            if !first {
                self.newline();
            }
            first = false;
            self.fmt_interface_member(member);
        }
    }

    fn fmt_interface_member(&mut self, member: &InterfaceMember<'_>) {
        match member {
            InterfaceMember::Field {
                name,
                ty,
                mutable,
                optional,
                ..
            } => {
                if *mutable {
                    self.write("mut ");
                }
                self.write(name);
                if *optional {
                    self.write("?");
                }
                self.write(": ");
                self.fmt_type(ty);
                self.write(";");
            }
            InterfaceMember::Method {
                name,
                params,
                return_type,
                mutable,
                ..
            } => {
                // `mut fn name(params) Ret;` — mut leads the signature in the
                // grammar; emitting it before the return type produced
                // output that does not parse (deka#479).
                if *mutable {
                    self.write("mut ");
                }
                self.write("fn ");
                self.write(name);
                self.write("(");
                self.fmt_param_list(params);
                self.write(")");
                if let Some(ty) = return_type {
                    self.write(" ");
                    self.fmt_type(ty);
                }
                self.write(";");
            }
        }
    }

    // --- function signatures -----------------------------------------------

    fn fmt_fn_sig(
        &mut self,
        is_async: bool,
        name: Option<&str>,
        type_params: &[TypeParam<'_>],
        params: &[Param<'_>],
        return_type: Option<&Type<'_>>,
    ) {
        if is_async {
            self.write("async ");
        }
        self.write("fn");
        if let Some(name) = name {
            self.write(" ");
            self.write(name);
        }
        if !type_params.is_empty() {
            self.write("<");
            self.fmt_type_param_list(type_params);
            self.write(">");
        }
        self.write("(");
        self.fmt_param_list(params);
        self.write(")");
        if let Some(ty) = return_type {
            self.write(" ");
            self.fmt_type(ty);
        }
    }

    fn fmt_receiver_method_sig(
        &mut self,
        is_async: bool,
        name: &str,
        receiver_name: &str,
        receiver_mutable: bool,
        receiver_type: &str,
        type_params: &[TypeParam<'_>],
        params: &[Param<'_>],
        return_type: Option<&Type<'_>>,
    ) {
        if is_async {
            self.write("async ");
        }
        self.write("fn (");
        self.write(receiver_name);
        if receiver_mutable {
            self.write(" mut");
        }
        self.write(" ");
        self.write(receiver_type);
        self.write(") ");
        self.write(name);
        if !type_params.is_empty() {
            self.write("<");
            self.fmt_type_param_list(type_params);
            self.write(">");
        }
        self.write("(");
        self.fmt_param_list(params);
        self.write(")");
        if let Some(ty) = return_type {
            self.write(" ");
            self.fmt_type(ty);
        }
    }

    fn fmt_param_list(&mut self, params: &[Param<'_>]) {
        let parts: Vec<String> = params.iter().map(|p| param_to_string(p)).collect();
        self.write(&parts.join(", "));
    }

    fn fmt_type_param_list(&mut self, type_params: &[TypeParam<'_>]) {
        let parts: Vec<String> = type_params.iter().map(|tp| tp.name.to_string()).collect();
        self.write(&parts.join(", "));
    }

    // --- types -------------------------------------------------------------

    fn fmt_type(&mut self, ty: &Type<'_>) {
        self.write(&type_to_string(ty));
    }

    // --- expressions -------------------------------------------------------

    fn fmt_expr(&mut self, expr: &Expr<'_>) {
        self.write(&self.expr_to_string(expr));
    }

    fn expr_to_string(&self, expr: &Expr<'_>) -> String {
        self.expr_to_string_with_prec(expr, Prec::Min)
    }

    fn expr_to_string_with_prec(&self, expr: &Expr<'_>, min_prec: Prec) -> String {
        let s = self.expr_inner_to_string(expr);
        if self.expr_prec(expr) < min_prec {
            format!("({})", s)
        } else {
            s
        }
    }

    fn expr_inner_to_string(&self, expr: &Expr<'_>) -> String {
        match expr {
            Expr::Number { value, .. } => format_number(*value),
            Expr::BigInt { value, .. } => value.to_string(),
            Expr::String { value, .. } => format!("\"{}\"", escape_string(value)),
            Expr::Boolean { value: true, .. } => "true".to_string(),
            Expr::Boolean { value: false, .. } => "false".to_string(),
            // `None`, not `none`. The lexer maps only the capitalised spelling
            // (lexer.rs: `"None" => TokenKind::None`), so emitting the lower
            // one produced a file that no longer compiled -- and `deka fmt` is
            // compile, so running it destroyed working code (deka#453).
            Expr::None { .. } => "None".to_string(),
            Expr::Identifier { name, .. } => name.to_string(),
            Expr::Binary { op, left, right, .. } => {
                if *op == BinOp::Pipe {
                    return self.pipe_chain_to_string(expr);
                }
                let (op_prec, assoc, op_str) = binary_op_info(op);
                let (left_min, right_min) = match assoc {
                    Assoc::Left => (op_prec, op_prec.next()),
                    Assoc::Right => (op_prec.next(), op_prec),
                    Assoc::NonAssoc => (op_prec.next(), op_prec.next()),
                };
                format!(
                    "{} {} {}",
                    self.expr_to_string_with_prec(left, left_min),
                    op_str,
                    self.expr_to_string_with_prec(right, right_min)
                )
            }
            Expr::Unary { op, operand, .. } => {
                let (prefix, postfix) = unary_op_str(op);
                if !prefix.is_empty() {
                    format!("{}{}", prefix, self.expr_to_string_with_prec(operand, Prec::Unary))
                } else {
                    format!("{}{}", self.expr_to_string_with_prec(operand, Prec::Postfix), postfix)
                }
            }
            Expr::Call {
                callee,
                type_args,
                args,
                ..
            } => {
                let mut s = self.expr_to_string(callee);
                if !type_args.is_empty() {
                    s.push('<');
                    s.push_str(&type_args.iter().map(type_to_string).collect::<Vec<_>>().join(", "));
                    s.push('>');
                }
                s.push('(');
                s.push_str(&args.iter().map(|a| self.expr_to_string(a)).collect::<Vec<_>>().join(", "));
                s.push(')');
                s
            }
            Expr::FieldAccess { object, field, .. } => {
                format!(
                    "{}.{}",
                    self.expr_to_string_with_prec(object, Prec::Postfix),
                    field
                )
            }
            Expr::IndexAccess { object, index, .. } => {
                format!(
                    "{}[{}]",
                    self.expr_to_string_with_prec(object, Prec::Postfix),
                    self.expr_to_string(index)
                )
            }
            Expr::StructLiteral { name, fields, .. } => {
                let mut s = name.to_string();
                s.push_str(" { ");
                let parts: Vec<String> = fields
                    .iter()
                    .map(|f| format!("{}: {}", f.name, self.expr_to_string(&f.value)))
                    .collect();
                s.push_str(&parts.join(", "));
                s.push_str(" }");
                s
            }
            Expr::EnumConstructor {
                enum_name,
                case_name,
                payload,
                ..
            } => {
                let mut s = format!("{}.{}", enum_name, case_name);
                if let Some(payload) = payload {
                    s.push('(');
                    s.push_str(&self.expr_to_string(payload));
                    s.push(')');
                }
                s
            }
            Expr::Match {
                scrutinee,
                arms,
                span,
            } => self.match_to_string(scrutinee, arms, *span),
            Expr::Unsafe {
                source,
                result_type,
                span,
            } => self.unsafe_to_string(source, result_type.as_ref(), *span),
            Expr::Bridge {
                kind,
                action,
                args,
                ..
            } => {
                let mut s = format!("bridge {}.{action}(", kind);
                s.push_str(&args.iter().map(|a| self.expr_to_string(a)).collect::<Vec<_>>().join(", "));
                s.push(')');
                s
            }
            Expr::Ternary {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                let mut s = self.expr_to_string_with_prec(condition, Prec::Ternary);
                s.push_str(" ? ");
                s.push_str(&self.expr_to_string(then_branch));
                s.push_str(" : ");
                s.push_str(&self.expr_to_string_with_prec(else_branch, Prec::Ternary));
                s
            }
            Expr::Await { expr, .. } => {
                format!("await {}", self.expr_to_string_with_prec(expr, Prec::Unary))
            }
            Expr::JsxElement { element, .. } => self.jsx_element_to_string(element),
            Expr::JsxFragment { children, .. } => self.jsx_fragment_to_string(children),
            Expr::JsxText { value, .. } => value.to_string(),
            Expr::Array { elements, .. } => {
                let mut s = "[".to_string();
                s.push_str(&elements.iter().map(|e| self.expr_to_string(e)).collect::<Vec<_>>().join(", "));
                s.push(']');
                s
            }
            Expr::Object { fields, .. } => {
                let mut s = "{".to_string();
                if fields.is_empty() {
                    s.push('}');
                } else {
                    let parts: Vec<String> = fields
                        .iter()
                        .map(|f| object_field_to_string(f, self))
                        .collect();
                    s.push_str(&parts.join(", "));
                    s.push('}');
                }
                s
            }
            Expr::Spread { expr, .. } => {
                format!("...{}", self.expr_to_string(expr))
            }
            Expr::Paren { expr, .. } => {
                format!("({})", self.expr_to_string(expr))
            }
            Expr::TemplateLiteral { parts, .. } => {
                let mut s = "`".to_string();
                for part in *parts {
                    match part {
                        TemplatePart::Text(text) => s.push_str(text),
                        TemplatePart::Expr(expr) => {
                            s.push_str("${");
                            s.push_str(&self.expr_to_string(expr));
                            s.push('}');
                        }
                    }
                }
                s.push('`');
                s
            }
            Expr::Function {
                params,
                return_type,
                body,
                is_async,
                ..
            } => {
                let mut s = String::new();
                if *is_async {
                    s.push_str("async ");
                }
                s.push_str("fn(");
                s.push_str(&params.iter().map(|p| param_to_string(p)).collect::<Vec<_>>().join(", "));
                s.push(')');
                if let Some(ty) = return_type {
                    s.push_str(" ");
                    s.push_str(&type_to_string(ty));
                }
                s.push_str(" { ");
                s.push_str(&stmt_list_to_string(body, " ", self));
                s.push_str(" }");
                s
            }
        }
    }

    fn expr_prec(&self, expr: &Expr<'_>) -> Prec {
        match expr {
            Expr::Ternary { .. } => Prec::Ternary,
            Expr::Binary { op, .. } => binary_op_info(op).0,
            Expr::Unary { .. } => Prec::Unary,
            _ => Prec::Max,
        }
    }

    fn pipe_chain_to_string(&self, expr: &Expr<'_>) -> String {
        let mut chain: Vec<&Expr<'_>> = Vec::new();
        let mut current = expr;
        loop {
            match current {
                Expr::Binary {
                    op: BinOp::Pipe,
                    left,
                    right,
                    ..
                } => {
                    chain.push(*right);
                    current = *left;
                }
                _ => {
                    chain.push(current);
                    break;
                }
            }
        }
        chain.reverse();

        let source_has_newline = chain.windows(2).any(|w| {
            let prev = w[0].span();
            let next = w[1].span();
            if prev.byte_start >= next.byte_start {
                return false;
            }
            self.source[prev.byte_end..next.byte_start]
                .chars()
                .any(|c| c == '\n')
        });

        let single_line = chain
            .iter()
            .map(|e| self.expr_to_string(e))
            .collect::<Vec<_>>()
            .join(" |> ");

        if !source_has_newline && single_line.len() <= 80 {
            return single_line;
        }

        let mut s = self.expr_to_string(chain[0]);
        let indent = "  ".repeat(self.indent + 1);
        for segment in &chain[1..] {
            s.push('\n');
            s.push_str(&indent);
            s.push_str("|> ");
            s.push_str(&self.expr_to_string(segment));
        }
        s
    }

    fn match_to_string(&self, scrutinee: &Expr<'_>, arms: &[MatchArm<'_>], span: Span) -> String {
        let scrutinee_str = match scrutinee {
            Expr::Paren { expr, .. } => self.expr_to_string(expr),
            _ => self.expr_to_string(scrutinee),
        };
        let mut s = "match (".to_string();
        s.push_str(&scrutinee_str);
        s.push_str(") {");
        let arm_strs: Vec<String> = arms.iter().map(|a| self.match_arm_to_string(a)).collect();
        if arm_strs.is_empty() {
            s.push('}');
            return s;
        }

        let source_has_newline = arms.windows(2).any(|w| {
            let prev = w[0].span;
            let next = w[1].span;
            if prev.byte_end >= next.byte_start {
                return false;
            }
            self.source[prev.byte_end..next.byte_start]
                .chars()
                .any(|c| c == '\n')
        });
        let single_line = arm_strs.join(", ");
        let use_multiline =
            arms.len() >= 2 || source_has_newline || single_line.len() > 80 || span.start.line != span.end.line;

        if !use_multiline {
            s.push(' ');
            s.push_str(&single_line);
            s.push(' ');
            s.push('}');
        } else {
            let arm_indent = "  ".repeat(self.indent + 1);
            for arm in arm_strs {
                s.push('\n');
                s.push_str(&arm_indent);
                s.push_str(&arm);
                s.push(',');
            }
            s.push('\n');
            s.push_str(&"  ".repeat(self.indent));
            s.push('}');
        }
        s
    }

    fn match_arm_to_string(&self, arm: &MatchArm<'_>) -> String {
        let mut s = String::new();
        s.push_str(&pattern_to_string(&arm.pattern));
        if let Some(guard) = &arm.guard {
            s.push_str(" if ");
            s.push_str(&self.expr_to_string(guard));
        }
        s.push_str(" => ");
        s.push_str(&self.expr_to_string(&arm.body));
        s
    }

    fn unsafe_to_string(
        &self,
        source: &str,
        result_type: Option<&Type<'_>>,
        _span: Span,
    ) -> String {
        let head = match result_type {
            Some(ty) => format!("unsafe<{}>", type_to_string(ty)),
            None => "unsafe".to_string(),
        };
        // Preserve raw JavaScript inside `unsafe { ... }` verbatim. We
        // only normalize the surrounding whitespace, not the body.
        let inner = source;
        let has_surrounding_ws = inner.starts_with(' ')
            || inner.starts_with('\t')
            || inner.starts_with('\n')
            || inner.ends_with(' ')
            || inner.ends_with('\t')
            || inner.ends_with('\n')
            || inner.is_empty();
        if has_surrounding_ws {
            format!("{head} {{{inner}}}")
        } else {
            format!("{head} {{ {inner} }}")
        }
    }

    fn jsx_element_to_string(&self, element: &JsxElement<'_>) -> String {
        let mut s = String::new();
        s.push('<');
        s.push_str(element.tag);
        for attr in element.attributes {
            s.push(' ');
            s.push_str(attr.name);
            if let Some(value) = &attr.value {
                s.push_str("={");
                s.push_str(&self.expr_to_string(value));
                s.push('}');
            }
        }
        if element.children.is_empty() {
            s.push_str(" />");
        } else {
            s.push('>');
            for child in element.children {
                s.push_str(&self.jsx_child_to_string(child));
            }
            s.push_str("</");
            s.push_str(element.tag);
            s.push('>');
        }
        s
    }

    fn jsx_fragment_to_string(&self, children: &[Expr<'_>]) -> String {
        let mut s = String::new();
        s.push_str("<>");
        for child in children {
            s.push_str(&self.jsx_child_to_string(child));
        }
        s.push_str("</>");
        s
    }

    fn jsx_child_to_string(&self, child: &Expr<'_>) -> String {
        match child {
            Expr::JsxText { value, .. } => value.to_string(),
            Expr::JsxElement { element, .. } => self.jsx_element_to_string(element),
            Expr::JsxFragment { children, .. } => self.jsx_fragment_to_string(children),
            other => {
                let mut s = "{".to_string();
                s.push_str(&self.expr_to_string(other));
                s.push('}');
                s
            }
        }
    }
}

// --- helpers that don't need Formatter state -----------------------------

fn import_spec_to_string(spec: &ImportSpec<'_>) -> String {
    if spec.imported == spec.local {
        spec.imported.to_string()
    } else {
        format!("{} as {}", spec.imported, spec.local)
    }
}

fn export_name_to_string(name: &ExportName<'_>) -> String {
    if let Some(alias) = name.alias {
        format!("{} as {}", name.name, alias)
    } else {
        name.name.to_string()
    }
}

fn param_to_string(param: &Param<'_>) -> String {
    let mut s = param.name.to_string();
    if let Some(ty) = &param.ty {
        s.push_str(": ");
        s.push_str(&type_to_string(ty));
    }
    if let Some(default) = &param.default_value {
        s.push_str(" = ");
        s.push_str(&expr_to_string_in_param(default));
    }
    s
}

fn expr_to_string_in_param(expr: &Expr<'_>) -> String {
    // Parameters can contain function literals; use the same formatter with
    // default indentation. This is a convenience for nested fn expressions.
    let fmt = Formatter::new("");
    fmt.expr_to_string(expr)
}

fn object_field_to_string(field: &ObjectField<'_>, fmt: &Formatter<'_>) -> String {
    if field.key.is_empty() {
        // Spread field encoded as empty key.
        format!("...{}", fmt.expr_to_string(&field.value))
    } else {
        format!(
            "{}: {}",
            object_key_to_string(field.key),
            fmt.expr_to_string(&field.value)
        )
    }
}

/// Object keys arrive unquoted from the parser. Bare-emit valid identifiers;
/// anything else (dashes, spaces, leading digits) must keep its quotes or the
/// output no longer parses (deka#477).
fn object_key_to_string(key: &str) -> String {
    let mut chars = key.chars();
    let bare = matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
    if bare {
        key.to_string()
    } else {
        format!("\"{}\"", escape_string(key))
    }
}

fn stmt_list_to_string(stmts: &[Stmt<'_>], sep: &str, fmt: &Formatter<'_>) -> String {
    stmts
        .iter()
        .filter(|s| !matches!(s, Stmt::Empty { .. }))
        .map(|s| stmt_to_string(s, fmt))
        .collect::<Vec<_>>()
        .join(sep)
}

fn stmt_to_string(stmt: &Stmt<'_>, fmt: &Formatter<'_>) -> String {
    match stmt {
        Stmt::Return { value: Some(value), .. } => format!("return {}", fmt.expr_to_string(value)),
        Stmt::Return { value: None, .. } => "return".to_string(),
        Stmt::Expr { expr, .. } => fmt.expr_to_string(expr),
        Stmt::Const { name, ty, value, .. } => {
            let mut s = format!("const {}", name);
            if let Some(ty) = ty {
                s.push_str(": ");
                s.push_str(&type_to_string(ty));
            }
            s.push_str(" = ");
            s.push_str(&fmt.expr_to_string(value));
            s
        }
        Stmt::Let { name, ty, value, .. } => {
            let mut s = format!("let {}", name);
            if let Some(ty) = ty {
                s.push_str(": ");
                s.push_str(&type_to_string(ty));
            }
            s.push_str(" = ");
            s.push_str(&fmt.expr_to_string(value));
            s
        }
        _ => "/* stmt */".to_string(),
    }
}

fn pattern_to_string(pattern: &Pattern<'_>) -> String {
    match pattern {
        Pattern::Wildcard { .. } => "_".to_string(),
        Pattern::Identifier { name, .. } => name.to_string(),
        Pattern::Literal { expr, .. } => {
            let fmt = Formatter::new("");
            fmt.expr_to_string(expr)
        }
        Pattern::Constructor { name, payload, .. } => {
            let mut s = name.to_string();
            if let Some(payload) = payload {
                s.push('(');
                s.push_str(&pattern_to_string(payload));
                s.push(')');
            }
            s
        }
        //  is compile (RFD 28), so a pattern the formatter cannot
        // render is a pattern nobody can use.
        Pattern::Or { alternatives, .. } => alternatives
            .iter()
            .map(pattern_to_string)
            .collect::<Vec<_>>()
            .join(" | "),
        Pattern::Struct { name, fields, .. } => {
            let mut s = name.to_string();
            s.push_str(" { ");
            let parts: Vec<String> = fields
                .iter()
                .map(|f| format!("{}: {}", f.name, pattern_to_string(&f.pattern)))
                .collect();
            s.push_str(&parts.join(", "));
            s.push_str(" }");
            s
        }
        Pattern::Tuple { elements, .. } => {
            let parts: Vec<String> = elements.iter().map(pattern_to_string).collect();
            format!("({})", parts.join(", "))
        }
    }
}

fn type_to_string(ty: &Type<'_>) -> String {
    match ty {
        Type::Named { name, .. } => name.to_string(),
        Type::Generic { base, args, .. } => {
            let mut s = base.to_string();
            s.push('<');
            s.push_str(&args.iter().map(type_to_string).collect::<Vec<_>>().join(", "));
            s.push('>');
            s
        }
        Type::Function { params, ret, .. } => {
            let mut s = "fn(".to_string();
            s.push_str(&params.iter().map(type_to_string).collect::<Vec<_>>().join(", "));
            s.push_str(") ");
            s.push_str(&type_to_string(ret));
            s
        }
        Type::Option { inner, .. } => {
            // A union inner needs parens: `(A | B)?` re-parsed from
            // `A | B?` would bind the `?` to the LAST member (rfd#42).
            let inner_s = match &**inner {
                Type::Union { .. } => format!("({})", type_to_string(inner)),
                _ => type_to_string(inner),
            };
            format!("{}?", inner_s)
        }
        Type::Tuple { elements, .. } => {
            let parts: Vec<String> = elements.iter().map(type_to_string).collect();
            format!("({})", parts.join(", "))
        }
        Type::Record { fields, .. } => {
            let parts: Vec<String> = fields
                .iter()
                .map(|f| format!("{}: {}", f.name, type_to_string(&f.ty)))
                .collect();
            format!("{{ {} }}", parts.join("; "))
        }
        Type::Union { members, .. } => {
            // Function and nested-union members need parens: without them
            // `(fn(A) B) | C` would re-parse as a function returning a
            // union (rfd#42, deka#530).
            let parts: Vec<String> = members
                .iter()
                .map(|m| match m {
                    Type::Function { .. } | Type::Union { .. } => {
                        format!("({})", type_to_string(m))
                    }
                    _ => type_to_string(m),
                })
                .collect();
            parts.join(" | ")
        }
    }
}

fn format_number(value: f64) -> String {
    if value.is_infinite() {
        if value.is_sign_negative() {
            return "-Infinity".to_string();
        }
        return "Infinity".to_string();
    }
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value == (value as i64) as f64 {
        format!("{:.0}", value)
    } else {
        format!("{}", value)
    }
}

/// Re-lex the source and collect `//` line comments as (line, text) pairs.
/// Comments inside `unsafe { }` bodies are part of the raw JS passthrough
/// and never surface as Comment tokens, so they are untouched by design.
/// Block comments (`/* */`) are not DekaScript and are not preserved.
fn collect_line_comments(source: &str) -> Vec<(usize, String)> {
    let mut lexer = deka_syntax::Lexer::new(source);
    let mut comments = Vec::new();
    loop {
        let token = lexer.next_token();
        if token.kind == deka_syntax::lexer::TokenKind::Comment && token.text.starts_with("//") {
            comments.push((token.span.start.line, token.text.to_string()));
        }
        if token.kind == deka_syntax::lexer::TokenKind::Eof {
            break;
        }
    }
    comments
}

fn escape_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn stmt_span(stmt: &Stmt<'_>) -> Span {
    match stmt {
        Stmt::Export { span, .. } => *span,
        Stmt::Import { span, .. } => *span,
        Stmt::Const { span, .. } => *span,
        Stmt::Let { span, .. } => *span,
        Stmt::UnwrapLet { span, .. } => *span,
        Stmt::Function { span, .. } => *span,
        Stmt::ReceiverMethod { span, .. } => *span,
        Stmt::Struct { span, .. } => *span,
        Stmt::Enum { span, .. } => *span,
        Stmt::TypeAlias { span, .. } => *span,
        Stmt::Newtype { span, .. } => *span,
        Stmt::Interface { span, .. } => *span,
        Stmt::Expr { span, .. } => *span,
        Stmt::Return { span, .. } => *span,
        Stmt::If { span, .. } => *span,
        Stmt::Block { span, .. } => *span,
        Stmt::For { span, .. } => *span,
        Stmt::ForOf { span, .. } => *span,
        Stmt::Break { span, .. } => *span,
        Stmt::Continue { span, .. } => *span,
        Stmt::Empty { span, .. } => *span,
    }
}

// --- operator tables -------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Prec {
    Min = 0,
    Ternary = 1,
    Or = 2,
    And = 3,
    Equality = 4,
    Relational = 5,
    Add = 6,
    Mul = 7,
    Unary = 8,
    Postfix = 9,
    Max = 10,
}

impl Prec {
    fn next(self) -> Self {
        match self {
            Prec::Min => Prec::Ternary,
            Prec::Ternary => Prec::Or,
            Prec::Or => Prec::And,
            Prec::And => Prec::Equality,
            Prec::Equality => Prec::Relational,
            Prec::Relational => Prec::Add,
            Prec::Add => Prec::Mul,
            Prec::Mul => Prec::Unary,
            Prec::Unary => Prec::Postfix,
            Prec::Postfix => Prec::Max,
            Prec::Max => Prec::Max,
        }
    }
}

#[derive(Clone, Copy)]
enum Assoc {
    Left,
    Right,
    NonAssoc,
}

fn binary_op_info(op: &BinOp) -> (Prec, Assoc, &'static str) {
    match op {
        BinOp::Or => (Prec::Or, Assoc::Left, "||"),
        BinOp::And => (Prec::And, Assoc::Left, "&&"),
        BinOp::Pipe => (Prec::And, Assoc::Left, "|>"),
        BinOp::Eq => (Prec::Equality, Assoc::NonAssoc, "=="),
        BinOp::Ne => (Prec::Equality, Assoc::NonAssoc, "!="),
        BinOp::Lt => (Prec::Relational, Assoc::NonAssoc, "<"),
        BinOp::Le => (Prec::Relational, Assoc::NonAssoc, "<="),
        BinOp::Gt => (Prec::Relational, Assoc::NonAssoc, ">"),
        BinOp::Ge => (Prec::Relational, Assoc::NonAssoc, ">="),
        BinOp::Add => (Prec::Add, Assoc::Left, "+"),
        BinOp::Sub => (Prec::Add, Assoc::Left, "-"),
        BinOp::Mul => (Prec::Mul, Assoc::Left, "*"),
        BinOp::Div => (Prec::Mul, Assoc::Left, "/"),
        BinOp::Mod => (Prec::Mul, Assoc::Left, "%"),
        BinOp::Assign => (Prec::Min, Assoc::Right, "="),
        BinOp::AddAssign => (Prec::Min, Assoc::Right, "+="),
        BinOp::SubAssign => (Prec::Min, Assoc::Right, "-="),
        BinOp::MulAssign => (Prec::Min, Assoc::Right, "*="),
        BinOp::DivAssign => (Prec::Min, Assoc::Right, "/="),
        BinOp::ModAssign => (Prec::Min, Assoc::Right, "%="),
        BinOp::BitAnd => (Prec::Mul, Assoc::Left, "&"),
        BinOp::BitOr => (Prec::Mul, Assoc::Left, "|"),
        BinOp::BitXor => (Prec::Mul, Assoc::Left, "^"),
        BinOp::Shl => (Prec::Mul, Assoc::Left, "<<"),
        BinOp::Shr => (Prec::Mul, Assoc::Left, ">>"),
    }
}

fn unary_op_str(op: &UnOp) -> (&'static str, &'static str) {
    match op {
        UnOp::Plus => ("+", ""),
        UnOp::Neg => ("-", ""),
        UnOp::Not => ("!", ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ds(source: &str) {
        let arena = bumpalo::Bump::new();
        let result = parse::parse(source, &arena);
        assert!(
            result.errors.is_empty(),
            "parse errors: {:?}",
            result.errors
        );
        assert!(result.program.is_some(), "parser returned no program");
    }

    /// Formatting must produce something that still *compiles*.
    ///
    /// deka#453: the formatter emitted `none` for `Expr::None` while the lexer
    /// only accepts `None`, so `deka fmt` turned a compiling file into one that
    /// did not. Every test here asserted on formatted *text*, so a spelling the
    /// language does not have could not fail them.
    ///
    /// Re-parsing is not enough either -- `none` parses perfectly well as an
    /// identifier and only fails in the typechecker. A round-trip check that
    /// stops at the parser passes against the broken formatter, which is how
    /// the first version of this test was itself vacuous.
    fn round_trips(source: &str) {
        let formatted = format_ds(source).expect("formats");

        let arena = bumpalo::Bump::new();
        let parsed = parse::parse(&formatted, &arena);
        assert!(
            parsed.errors.is_empty(),
            "formatted output does not parse: {:?}\n{formatted}",
            parsed.errors
        );
        let program = parsed.program.expect("parser returned no program");
        let checked = deka_syntax::typeck::check_program(&program, &formatted);
        assert!(
            checked.errors.is_empty(),
            "formatted output does not typecheck: {:?}\n{formatted}",
            checked.errors
        );

        let again = format_ds(&formatted).expect("formats twice");
        assert_eq!(formatted, again, "formatting is not idempotent");
    }

    #[test]
    fn prelude_constructors_round_trip() {
        round_trips("const a = None\n");
        round_trips("const a = Some(\"x\")\n");
        round_trips("const a = Ok(1)\n");
        round_trips("const a = Err(\"e\")\n");
    }

    #[test]
    fn option_round_trips_through_a_match() {
        round_trips(
            "fn f(o: Option<string>) string {\n  return match (o) { Some(v) => v, None => \"n\" }\n}\n",
        );
    }

    #[test]
    fn literals_and_patterns_round_trip() {
        round_trips("const a = true\nconst b = false\n");
        round_trips("fn f(n: number) number {\n  return match (n) { 1 => 1, _ => 0 }\n}\n");
    }

    #[test]
    fn super_declarations_round_trip() {
        // The `super` prefix must survive formatting on both struct and enum
        // declarations; dropping it would silently demote a reflected type
        // to a plain one without any formatter error.
        round_trips("super struct User {\n  id: number\n  name: string\n}\n");
        round_trips("super enum Status {\n  Active\n  Archived\n}\n");
        round_trips("super struct Node {\n  next: Option<Node>\n}\nconst t = Node.type()\n");
    }

    #[test]
    fn normalizes_trailing_whitespace_and_eof() {
        let input = "fn add() int {\n  return 1;  \n}\n\n";
        let output = format_ds(input).unwrap();
        assert_eq!(output, "fn add() int {\n  return 1\n}\n");
    }

    #[test]
    fn adds_trailing_newline_when_missing() {
        let input = "const x = 1;";
        let output = format_ds(input).unwrap();
        assert_eq!(output, "const x = 1\n");
    }

    #[test]
    fn empty_source_stays_empty() {
        assert_eq!(format_ds("").unwrap(), "");
    }

    #[test]
    fn formats_function() {
        let input = "fn add(  a:int,b :  string   ) int{ return a+b; }";
        let output = format_ds(input).unwrap();
        assert!(output.contains("fn add(a: int, b: string) int {"), "got: {}", output);
        assert!(output.contains("  return a + b"), "got: {}", output);
    }

    #[test]
    fn formats_function_literal() {
        let input = "const f=fn(x:int) int{return x*x;};";
        let output = format_ds(input).unwrap();
        assert!(
            output.contains("const f = fn(x: int) int { return x * x }"),
            "got: {}",
            output
        );
    }

    #[test]
    fn formats_struct() {
        let input = "struct Point{x:int;y:int}";
        let output = format_ds(input).unwrap();
        assert!(output.contains("struct Point {"), "got: {}", output);
        assert!(output.contains("  x: int;"), "got: {}", output);
        assert!(output.contains("  y: int;"), "got: {}", output);
    }

    #[test]
    fn formats_struct_literal() {
        let input = "fn origin() Point { return Point { x: 0, y: 0 }; }";
        let output = format_ds(input).unwrap();
        assert!(output.contains("Point { x: 0, y: 0 }"), "got: {}", output);
    }

    #[test]
    fn formats_receiver_method() {
        let input = "fn (p mut Person) setName(name: string) { p.name = name; }";
        let output = format_ds(input).unwrap();
        assert!(
            output.contains("fn (p mut Person) setName(name: string) {"),
            "got: {}",
            output
        );
    }

    #[test]
    fn formats_match_expression() {
        let input = r#"
            enum Status { Loading, Ready, Failed }
            fn f(s: Status) int {
                return match (s) {
                    Status.Loading => 0,
                    Status.Ready => 1,
                    Status.Failed => 2,
                };
            }
        "#;
        let output = format_ds(input).unwrap();
        assert!(output.contains("match (s) {"), "got: {}", output);
        assert!(output.contains("Loading => 0"), "got: {}", output);
    }

    #[test]
    fn formats_nested_match_patterns() {
        let input = r#"
            fn f(r: Result<Option<number>, string>) number {
                return match (r) {
                    Err(e) => 0,
                    Ok(None) => 1,
                    Ok(Some(v)) => v,
                }
            }
        "#;
        let output = format_ds(input).unwrap();
        assert!(output.contains("Ok(None) => 1"), "got: {}", output);
        assert!(output.contains("Ok(Some(v)) => v"), "got: {}", output);
    }

    #[test]
    fn formats_jsx_element() {
        let input = "fn View() Object { return <div class=\"test\">hello {name}</div>; }";
        let output = format_ds(input).unwrap();
        assert!(output.contains("<div class={\"test\"}>"), "got: {}", output);
        assert!(output.contains("hello{name}"), "got: {}", output);
        assert!(output.contains("</div>"), "got: {}", output);
    }

    #[test]
    fn formats_pipe_expression() {
        let input = "fn inc(x: int) int { return x + 1; }\nconst y = 5 |> inc;";
        let output = format_ds(input).unwrap();
        assert!(output.contains("5 |> inc"), "got: {}", output);
    }

    #[test]
    fn formats_type_alias() {
        let input = "alias UserId=string;";
        let output = format_ds(input).unwrap();
        assert_eq!(output, "alias UserId = string\n");
    }

    #[test]
    fn formats_let_binding() {
        let input = "fn f(){let x=1; x+=2;}";
        let output = format_ds(input).unwrap();
        assert!(output.contains("let x = 1"), "got: {}", output);
        assert!(output.contains("x += 2"), "got: {}", output);
    }

    #[test]
    fn leaves_invalid_source_unchanged() {
        let input = "fn f( { return 1;";
        let output = format_ds(input).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn formats_async_await() {
        let input = "async fn fetch() Promise<int> { return await 1; }";
        let output = format_ds(input).unwrap();
        assert!(output.contains("async fn fetch() Promise<int> {"), "got: {}", output);
        assert!(output.contains("return await 1"), "got: {}", output);
    }

    #[test]
    fn formats_interface() {
        let input = "interface Named { name: string; }";
        let output = format_ds(input).unwrap();
        assert!(output.contains("interface Named {"), "got: {}", output);
        assert!(output.contains("  name: string;"), "got: {}", output);
    }

    #[test]
    fn formats_enum() {
        // User code cannot declare type parameters (deka#561); a payload-typed
        // case covers the same formatting surface without generics.
        let input = "enum Status { Loading(number), Ready, Failed }";
        parse_ds(input);
        let output = format_ds(input).unwrap();
        assert!(output.contains("enum Status {"), "got: {}", output);
        assert!(output.contains("  Loading(number),"), "got: {}", output);
        assert!(output.contains("  Ready,"), "got: {}", output);
        assert!(output.contains("  Failed,"), "got: {}", output);
    }

    #[test]
    fn preserves_blank_line_between_top_level_statements() {
        let input = "fn a() {}\n\nfn b() {}";
        let output = format_ds(input).unwrap();
        assert_eq!(output, "fn a() {}\n\nfn b() {}\n");
    }

    #[test]
    fn interface_mut_method_keeps_mut_before_fn() {
        // deka#479: the formatter emitted `fn increment() mut void;`, which
        // does not parse. mut leads the signature.
        let input = "interface Counter {\n  mut fn increment() void\n}";
        let output = format_ds(input).unwrap();
        assert_eq!(
            output,
            "interface Counter {\n  mut fn increment() void;\n}\n"
        );
        // And the output itself must parse.
        let arena = bumpalo::Bump::new();
        let reparsed = deka_syntax::parse::parse(&output, &arena);
        assert!(reparsed.errors.is_empty(), "{:?}", reparsed.errors);
    }

    #[test]
    fn quoted_object_keys_keep_their_quotes() {
        // deka#477: keys arrive unquoted from the parser; emitting them bare
        // broke on dashes (`{ "X-A": "1" }` became `{ X-A: "1" }`).
        let input = "const headers = { \"X-A\": \"1\", plain: 2 }";
        let output = format_ds(input).unwrap();
        assert_eq!(output, "const headers = {\"X-A\": \"1\", plain: 2}\n");
    }

    #[test]
    fn reformat_does_not_insert_blank_line_after_multiline_stmt() {
        // deka#477: the statement separator compared START lines, so any
        // multi-line statement looked like it was followed by a blank line
        // and every reformat inserted another one.
        let input = "let parsed = unwrap(number(\"42\")) or { 0 }\necho(string(parsed))";
        let once = format_ds(input).unwrap();
        let twice = format_ds(&once).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn preserves_line_comments() {
        // deka#484: the parser discards comment tokens; the formatter
        // re-lexes and reattaches them by position.
        let input = "// file header\nlet x = 1\n\n// doc for f\nfn f() {\n  // inside\n  return x\n}\n";
        let output = format_ds(input).unwrap();
        assert_eq!(
            output,
            "// file header\nlet x = 1\n\n// doc for f\nfn f() {\n  // inside\n  return x\n}\n"
        );
    }

    #[test]
    fn preserved_comments_are_idempotent() {
        let input = "// header\n\nfn f() {\n  // note\n  let x = 1\n}\n";
        let once = format_ds(input).unwrap();
        let twice = format_ds(&once).unwrap();
        assert_eq!(once, twice);
        assert!(once.contains("// header"));
        assert!(once.contains("// note"));
    }

    #[test]
    fn comment_tokens_include_both_slashes() {
        // The lexer caller consumed the first `/` before the comment reader
        // ran, so Comment token text started with a single slash — writing
        // it back produced `/ comment`, which does not lex as a comment.
        let input = "// hello\nlet x = 1\n";
        let output = format_ds(input).unwrap();
        assert!(output.starts_with("// hello\n"), "got: {output:?}");
    }

    #[test]
    fn preserves_blank_line_inside_block() {
        let input = "fn f() {\n  let x = 1\n\n  let y = 2\n}";
        let output = format_ds(input).unwrap();
        assert_eq!(
            output,
            "fn f() {\n  let x = 1\n\n  let y = 2\n}\n"
        );
    }

    #[test]
    fn collapses_multiple_blank_lines_to_one() {
        let input = "fn a() {}\n\n\n\nfn b() {}";
        let output = format_ds(input).unwrap();
        assert_eq!(output, "fn a() {}\n\nfn b() {}\n");
    }

    #[test]
    fn keeps_pipe_chain_on_one_line_when_short() {
        let input = "const y = 5 |> inc";
        let output = format_ds(input).unwrap();
        assert_eq!(output, "const y = 5 |> inc\n");
    }

    #[test]
    fn keeps_pipe_chain_across_multiple_lines() {
        let input = "const x = 5\n  |> add(1)\n  |> console.log";
        let output = format_ds(input).unwrap();
        assert_eq!(
            output,
            "const x = 5\n  |> add(1)\n  |> console.log\n"
        );
    }

    #[test]
    fn breaks_long_pipe_chain_to_multiple_lines() {
        let input = "const result = initialValue |> transformWithLongName |> anotherVeryLongTransformationName |> finalTransform";
        let output = format_ds(input).unwrap();
        assert!(output.contains("\n  |> "), "expected multiline pipe, got: {}", output);
    }

    #[test]
    fn does_not_add_semicolons_to_statements() {
        let input = "const x = 1\nconst y = 2\nconsole.log(x + y)";
        let output = format_ds(input).unwrap();
        assert_eq!(
            output,
            "const x = 1\nconst y = 2\nconsole.log(x + y)\n"
        );
        parse_ds(&output);
    }

    #[test]
    fn struct_embed_does_not_spam_embed() {
        let input = r#"struct Person {}
struct Employee {
  Person
  name: string
}"#;
        let output = format_ds(input).unwrap();
        assert!(
            output.matches("embed").count() <= 1,
            "formatter spammed 'embed' tokens: {}",
            output
        );
        assert!(
            output.contains("struct Employee {"),
            "expected Employee struct to be preserved: {}",
            output
        );
        parse_ds(&output);
    }

    #[test]
    fn import_span_does_not_leak_into_next_statement() {
        let input = "import { add } from \"./missing.ds\";\nconsole.log(add(1, 2))";
        let output = format_ds(input).unwrap();
        assert!(
            !output.contains("console\nconsole"),
            "formatter duplicated the next statement due to an overlapping import span: {}",
            output
        );
        assert!(
            output.contains("console.log(add(1, 2))"),
            "expected console.log statement to be preserved: {}",
            output
        );
        parse_ds(&output);
    }

    #[test]
    fn export_named_span_does_not_leak_into_next_statement() {
        let input = "export { answer };\nconsole.log(answer)";
        let output = format_ds(input).unwrap();
        assert!(
            !output.contains("console\nconsole"),
            "formatter duplicated the next statement due to an overlapping export span: {}",
            output
        );
        assert!(
            output.contains("console.log(answer)"),
            "expected console.log statement to be preserved: {}",
            output
        );
        parse_ds(&output);
    }

    #[test]
    fn formatted_output_re_parses_without_errors() {
        let input = r#"fn inc(x: int) int {
  return x + 1
}

const y = 5
  |> add(1)
  |> console.log

const a = 1
const b = 2
print(a + b)
"#;
        let output = format_ds(input).unwrap();
        parse_ds(&output);
    }

    #[test]
    fn closing_brace_on_own_line_for_struct() {
        let input = "struct User { name: string; email: string? }";
        let output = format_ds(input).unwrap();
        assert!(
            output.contains("  email?: string;\n}\n"),
            "expected struct closing brace on own line, got: {}",
            output
        );
        parse_ds(&output);
    }

    #[test]
    fn closing_brace_on_own_line_for_enum() {
        let input = "enum Status { Loading, Ready, Failed }";
        let output = format_ds(input).unwrap();
        assert!(
            output.contains("  Failed,\n}\n"),
            "expected enum closing brace on own line, got: {}",
            output
        );
        parse_ds(&output);
    }

    #[test]
    fn optional_field_does_not_double_question_mark() {
        let input = "struct User { name: string; email: string? }";
        let output = format_ds(input).unwrap();
        assert!(
            output.contains("email?: string"),
            "expected canonical optional field, got: {}",
            output
        );
        assert!(
            !output.contains("email?: string?"),
            "double ? in optional field, got: {}",
            output
        );
        parse_ds(&output);
    }

    #[test]
    fn match_expression_breaks_arms_to_multiple_lines() {
        let input = r#"const label = match (current) {
  Loading => "Loading...",
  Ready => "Ready",
  Failed => "Failed",
  _ => "Unknown"
}"#;
        let output = format_ds(input).unwrap();
        assert!(
            output.contains("  Loading => \"Loading...\","),
            "expected multiline match arm, got: {}",
            output
        );
        assert!(
            output.contains("}\n"),
            "expected closing brace on own line, got: {}",
            output
        );
        parse_ds(&output);
        assert!(
            output.contains("_ =>"),
            "catch-all `_` must round-trip (deka#281), got: {}",
            output
        );
        assert!(
            !output.contains(" | "),
            "match arm conditions must use commas, not `|` (deka#281), got: {}",
            output
        );
    }

    #[test]
    fn match_preserves_default_catch_all() {
        let input = r#"fn label(n: number) string {
  return match (n) {
    1 => "one",
    default => "other",
  }
}"#;
        let output = format_ds(input).unwrap();
        assert!(
            output.contains("default =>"),
            "default catch-all must round-trip (deka#281), got: {}",
            output
        );
        parse_ds(&output);
    }

    #[test]
    fn unsafe_block_does_not_grow_whitespace_on_reformat() {
        let input = r#"const r = unsafe { 1 + 2 }
console.log(r)"#;
        let once = format_ds(input).unwrap();
        let twice = format_ds(&once).unwrap();
        assert_eq!(
            once, twice,
            "formatter should be idempotent for unsafe blocks, got:\n{}",
            twice
        );
        assert!(
            once.contains("unsafe { 1 + 2 }"),
            "expected single spaces inside unsafe block, got: {}",
            once
        );
        parse_ds(&once);
    }

    #[test]
    fn unsafe_block_preserves_multiline_body() {
        let input = r#"const r = unsafe {
  const x = 1
  x + 2
}
console.log(r)"#;
        let output = format_ds(input).unwrap();
        assert!(
            output.contains("unsafe {\n  const x = 1\n  x + 2\n}"),
            "expected multiline unsafe body to be preserved, got: {}",
            output
        );
        let reformatted = format_ds(&output).unwrap();
        assert_eq!(
            output, reformatted,
            "formatter should be idempotent for multiline unsafe blocks"
        );
    }
}
