//! Bounds facts are deliberately local and conservative. Calls may mutate aliases;
//! writes, suspension and opaque JS kill every fact. Branch joins intersect, never
//! restore a snapshot (which could resurrect a proof killed in either branch).
use super::{Checker, types::ArrayAccess};
use crate::ast::{BinOp, Expr, ForInit, Span};
use std::collections::HashSet;

#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct IndexFlow {
    bounds: HashSet<(String, String)>,
    integers: HashSet<String>,
}
impl IndexFlow {
    pub(super) fn proves(&self, object: &Expr<'_>, index: &Expr<'_>) -> bool {
        array(object)
            .zip(key(index))
            .is_some_and(|pair| self.bounds.contains(&pair))
    }
    pub(super) fn kill(&mut self) {
        self.bounds.clear();
        self.integers.clear();
    }
    pub(super) fn remember_integer(&mut self, name: &str, value: &Expr<'_>) {
        if integer(value, self) {
            self.integers.insert(name.into());
        }
    }
    pub(super) fn shadow(&mut self, name: &str) {
        self.bounds.retain(|(a, i)| a != name && i != name);
        self.integers.remove(name);
    }
    pub(super) fn restrict_to(&mut self, before: &Self) {
        self.bounds.retain(|p| before.bounds.contains(p));
        self.integers.retain(|p| before.integers.contains(p));
    }
}
fn plain<'e, 'a>(e: &'e Expr<'a>) -> &'e Expr<'a> {
    match e {
        Expr::Paren { expr, .. } => plain(expr),
        _ => e,
    }
}
fn key(e: &Expr<'_>) -> Option<String> {
    match plain(e) {
        Expr::Identifier { name, .. } => Some((*name).into()),
        Expr::Number { value, .. } if value.is_finite() && value.fract() == 0.0 => {
            Some(value.to_string())
        }
        _ => None,
    }
}
fn array(e: &Expr<'_>) -> Option<String> {
    match plain(e) {
        Expr::Identifier { name, .. } => Some((*name).into()),
        _ => None,
    }
}
fn pretty_index_expr(e: &Expr<'_>) -> Option<String> {
    match plain(e) {
        Expr::Identifier { name, .. } => Some((*name).into()),
        Expr::Number { value, .. } if value.is_finite() && value.fract() == 0.0 => {
            Some(value.to_string())
        }
        Expr::FieldAccess { object, field, .. } => {
            Some(format!("{}.{}", pretty_index_expr(object)?, field))
        }
        Expr::IndexAccess { object, index, .. } => Some(format!(
            "{}[{}]",
            pretty_index_expr(object)?,
            pretty_index_expr(index)?
        )),
        _ => None,
    }
}
/// Non-bare receivers cannot carry bounds facts. Teach the hoist-to-local
/// pattern instead of the `has()` recipe, which would still not prove.
fn hoist_receiver(object: &Expr<'_>) -> Option<(String, String)> {
    match plain(object) {
        Expr::Identifier { .. } => None,
        Expr::FieldAccess { object, field, .. } => {
            let rhs = pretty_index_expr(object)
                .map(|base| format!("{base}.{field}"))
                .unwrap_or_else(|| format!("cart.{field}"));
            Some(((*field).into(), rhs))
        }
        Expr::IndexAccess { object, index, .. } => {
            let rhs = match (pretty_index_expr(object), pretty_index_expr(index)) {
                (Some(base), Some(index)) => format!("{base}[{index}]"),
                _ => "cart.items".into(),
            };
            Some(("items".into(), rhs))
        }
        _ => Some((
            "items".into(),
            pretty_index_expr(object).unwrap_or_else(|| "cart.items".into()),
        )),
    }
}
fn length(e: &Expr<'_>) -> Option<String> {
    match plain(e) {
        Expr::FieldAccess {
            object,
            field: "length",
            ..
        } => array(object),
        _ => None,
    }
}
fn zero(e: &Expr<'_>) -> bool {
    matches!(plain(e), Expr::Number { value: 0.0, .. })
}
fn integer(e: &Expr<'_>, flow: &IndexFlow) -> bool {
    matches!(plain(e), Expr::Number { value, .. } if value.is_finite() && value.fract() == 0.0)
        || key(e).is_some_and(|k| flow.integers.contains(&k))
}
fn terms<'e, 'a>(e: &'e Expr<'a>, out: &mut Vec<&'e Expr<'a>>) {
    if let Expr::Binary {
        op: BinOp::And,
        left,
        right,
        ..
    } = plain(e)
    {
        terms(left, out);
        terms(right, out);
    } else {
        out.push(plain(e));
    }
}
impl<'a> Checker<'a> {
    pub(super) fn pop_value_scope(&mut self) {
        if let Some(scope) = self.scopes.pop() {
            for name in scope.keys() {
                self.index_flow.shadow(name);
            }
        }
        self.capture_scopes.pop();
        self.context_scopes.pop();
    }

    /// Fourth proof source (rfd#66): tuple length is fixed by its type.
    /// Unlike flow facts, this proof survives calls and alias writes because
    /// tuple operations cannot change the length or a position's type.
    pub(super) fn tuple_index_type(
        &mut self,
        elements: &[super::Type<'a>],
        index: &Expr<'a>,
        span: Span,
    ) -> super::Type<'a> {
        self.check_expr(index);
        if let Expr::Number { value, .. } = plain(index) {
            if *value >= 0.0 && value.fract() == 0.0 && *value < elements.len() as f64 {
                return elements[*value as usize].clone();
            }
            self.error_span(
                span,
                format!(
                    "tuple index {value} is out of range for {} positions",
                    elements.len()
                ),
            );
        } else {
            self.error_span(span, "tuple indexing requires an in-range integer literal; use destructuring: const [a, b] = pair");
        }
        super::Type::Error
    }

    pub(super) fn require_index_proof(&mut self, object: &Expr<'a>, index: &Expr<'a>, span: Span) {
        if self.index_flow.proves(object, index) {
            return;
        }
        let i = key(index);
        let i = i.as_deref().unwrap_or("i");
        let message = match hoist_receiver(object) {
            Some((local, rhs)) => format!(
                "index not proven in bounds — proofs track local names; hoist it first: const {local} = {rhs}, then {local}.has({i}) ? ..."
            ),
            None => {
                let a = array(object).unwrap_or_else(|| "items".into());
                format!(
                    "index not proven in bounds — test it first: `{a}.has({i}) ? {a}[{i}] : fallback`"
                )
            }
        };
        self.error_span(span, message);
    }
    pub(super) fn assume_index_condition(&mut self, e: &Expr<'a>) {
        let mut conditions = Vec::new();
        terms(e, &mut conditions);
        // Never reconstruct facts across an effect in the condition itself.
        let mut effect = false;
        let pure_calls: HashSet<usize> = self
            .array_builtin_calls
            .iter()
            .filter(|(_, kind)| **kind == ArrayAccess::Has)
            .map(|(ptr, _)| *ptr as usize)
            .collect();
        crate::visit::walk_expr(e, &mut |node| match node {
            Expr::Call { .. } if !pure_calls.contains(&(node as *const _ as usize)) => {
                effect = true
            }
            Expr::Binary {
                op:
                    BinOp::Assign
                    | BinOp::AddAssign
                    | BinOp::SubAssign
                    | BinOp::MulAssign
                    | BinOp::DivAssign
                    | BinOp::ModAssign
                    | BinOp::Pipe,
                ..
            }
            | Expr::Unsafe { .. }
            | Expr::Await { .. }
            | Expr::Function { .. } => effect = true,
            _ => {}
        });
        if effect {
            return;
        }
        for term in &conditions {
            if let Expr::Call { callee, args, .. } = term {
                if self.array_builtin_calls.get(&(*term as *const _)) == Some(&ArrayAccess::Has)
                    && args.len() == 1
                {
                    if let Expr::FieldAccess { object, .. } = plain(callee) {
                        if let (Some(a), Some(i)) = (array(object), key(&args[0])) {
                            self.index_flow.integers.insert(i.clone());
                            self.index_flow.bounds.insert((a, i));
                        }
                    }
                }
            }
        }
        for term in &conditions {
            if let Expr::Binary {
                op: BinOp::Lt,
                left,
                right,
                ..
            } = term
            {
                if let (Some(i), Some(a)) = (key(left), length(right)) {
                    let lower = conditions.iter().any(|t| matches!(t, Expr::Binary { op: BinOp::Ge, left, right, .. } if key(left) == Some(i.clone()) && zero(right)));
                    if lower && integer(left, &self.index_flow) {
                        self.index_flow.bounds.insert((a, i));
                    }
                }
            }
        }
    }
    fn index_effect(
        e: &Expr<'a>,
        calls: &std::collections::HashMap<*const Expr<'a>, ArrayAccess>,
    ) -> bool {
        match e {
            Expr::Call { .. } => calls.get(&(e as *const _)) != Some(&ArrayAccess::Has),
            Expr::Binary { op, .. } => matches!(
                op,
                BinOp::Assign
                    | BinOp::AddAssign
                    | BinOp::SubAssign
                    | BinOp::MulAssign
                    | BinOp::DivAssign
                    | BinOp::ModAssign
                    | BinOp::Pipe
            ),
            Expr::Unsafe { .. } | Expr::Await { .. } | Expr::Function { .. } => true,
            _ => false,
        }
    }
    pub(super) fn apply_index_effect(&mut self, e: &Expr<'a>) {
        if Self::index_effect(e, &self.array_builtin_calls) {
            self.index_flow.kill();
        }
    }
    pub(super) fn assume_index_loop(
        &mut self,
        init: Option<&ForInit<'a>>,
        condition: Option<&Expr<'a>>,
        step: Option<&Expr<'a>>,
        body: &[crate::ast::Stmt<'a>],
    ) {
        let Some(ForInit::Let { name, value }) = init else {
            return;
        };
        if !zero(value) {
            return;
        }
        let mut unstable = false;
        for stmt in body {
            crate::visit::walk_stmt(stmt, &mut |e| {
                if matches!(e, Expr::Binary { op: BinOp::Assign | BinOp::AddAssign | BinOp::SubAssign | BinOp::MulAssign | BinOp::DivAssign | BinOp::ModAssign, left, .. } if key(left).as_deref() == Some(name))
                    || matches!(e, Expr::Unsafe { .. })
                {
                    unstable = true;
                }
            });
        }
        if unstable {
            return;
        }
        // The increment must preserve nonnegative integerness on every backedge.
        let Some(Expr::Binary {
            op: BinOp::Assign,
            left,
            right,
            ..
        }) = step.map(plain)
        else {
            return;
        };
        if key(left).as_deref() != Some(name) {
            return;
        }
        let Expr::Binary {
            op: BinOp::Add,
            left,
            right,
            ..
        } = plain(right)
        else {
            return;
        };
        if key(left).as_deref() != Some(name)
            || !matches!(plain(right), Expr::Number { value: 1.0, .. })
        {
            return;
        }
        self.index_flow.integers.insert((*name).into());
        if let Some(Expr::Binary {
            op: BinOp::Lt,
            left,
            right,
            ..
        }) = condition.map(plain)
        {
            if key(left).as_deref() == Some(name) {
                if let Some(a) = length(right) {
                    self.index_flow.bounds.insert((a, (*name).into()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::typeck;
    #[test]
    fn indexing_proof_forms_admit_reads_and_writes() {
        for source in [
            "const a = [1]; for i in 0..a.length { const x: number = a[i]; }",
            "const a = [1]; for (let i = 0; i < a.length; i = i + 1) { const x = a[i]; }",
            "let a = [1]; let i = 0; if (a.has(i)) { a[i] = 2; }",
            "fn f(a: Array<number>, i: number) number { return a.has(i) ? a[i] : 0; }",
            "const a = [1]; const x = 0 >= 0 && 0 < a.length ? a[0] : 0;",
            "const a = [1]; for i in 0..a.length { if (i >= 0 && i < a.length) { const x = a[i]; } }",
            "const a = [1]; let i = 0; const x = a.has(i) && a[i] > 0;",
            "fn f<T: Array<number>>(a: T, i: number) number { return a.has(i) ? a[i] : 0; }",
            "const a = [1]; for i in 0..3 { if (i >= 0 && i < a.length) { const x = a[i]; } }",
            "fn f(a: Array<number>, x: number = a.has(0) ? a[0] : 0) number { return x; }",
            "const a = [1]; const i = 0; const x = i >= 0 && i < a.length ? a[i] : 0;",
        ] {
            assert!(typeck(source).is_empty(), "{source}: {:?}", typeck(source));
        }
    }
    #[test]
    fn indexing_rejects_missing_stale_or_wrong_branch_facts() {
        for source in [
            "const a = [1]; const x = a[0];",
            "let a = [1]; a[0] = 2;",
            "let a = [1]; a[0] += 2;",
            "let a = [1]; let i = 0; if (a.has(i)) { i = 9; const x = a[i]; }",
            "let a = [1]; let i = 0; if (a.has(i)) { a.push(2); const x = a[i]; }",
            "let a = [1]; let i = 0; if (a.has(i)) { a = []; const x = a[i]; }",
            "let a = [1]; let i = 0; if (a.has(i)) { a[i] = 2; const x = a[i]; }",
            "const a = [1]; let i = 0; const x = a.has(i) ? 0 : a[i];",
            "const a = [1]; let i = 0; if (a.has(i)) {} else { const x = a[i]; }",
            "const a = [1]; let i = 0; const x = true ? (a.has(i) ? a[i] : 0) : a[i];",
            "const a = [1]; let i = 0; if (a.has(i)) {} const x = a[i];",
            "const a = [1]; let i = 0; if (a.has(i)) { const i = 99; const x = a[i]; }",
            "const a = [1]; let i = 0; if (a.has(i)) { const f = fn() number { return a[i]; }; }",
            "const a = [1]; let i = 0.5; const x = i >= 0 && i < a.length ? a[i] : 0;",
            "const a = [1]; const b = [2]; let i = 0; const x = a.has(i) ? b[i] : 0;",
            "const a = [1]; let i = 0; const x = a.has(i) || a[i] > 0;",
            "let a = [1]; for i in 0..a.length { const x = a[i]; i = -2; }",
            "let a = [1]; fn change() number { a = []; return 2; } if (a.has(0)) { a[0] = change(); }",
            "let a = [1]; const other = a; if (a.has(0)) { other.first(); const x = a[0]; }",
            "let a = [1]; let i = 0; if (a.has(i)) { if (true) { i = 9; } const x = a[i]; }",
            "let a = [1]; let i = 0; const ok = a.has(i); if (ok) { const x = a[i]; }",
            "fn f(a: Array<number>, x: number = a[0]) number { return x; }",
            "const a = [1]; let i = 0.5; { const i = 0; } const x = i >= 0 && i < a.length ? a[i] : 0;",
            "const a = [1]; let i = 0.5; for (const x of a) { const i = 0; } const x = i >= 0 && i < a.length ? a[i] : 0;",
        ] {
            assert!(
                typeck(source)
                    .iter()
                    .any(|e| e.message.contains("index not proven in bounds")),
                "{source}: {:?}",
                typeck(source)
            );
        }
    }
    #[test]
    fn indexing_await_invalidates_established_proof() {
        // Await a parameter so no call or assignment can invalidate the proof.
        let source = "async fn f(a: Array<number>, i: number, pending: Promise<number>) Promise<number> { if (a.has(i)) { const before = a[i]; await pending; return a[i]; } return 0; }";
        assert!(typeck(&source.replace("await pending;", "")).is_empty());
        let errors = typeck(source);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(
            errors[0].message,
            "index not proven in bounds — test it first: `a.has(i) ? a[i] : fallback`"
        );
        let refreshed = source.replace("return a[i];", "return a.has(i) ? a[i] : 0;");
        assert!(typeck(&refreshed).is_empty(), "{:?}", typeck(&refreshed));
    }

    #[test]
    fn indexing_has_signature_is_checked() {
        for source in [
            "const a = [1]; const x = a.has();",
            "const a = [1]; const x = a.has(0, 1);",
            "const a = [1]; const x = a.has(\"zero\");",
            "const a = [1]; const x: number = a.has(0);",
        ] {
            assert!(!typeck(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn indexing_diagnostic_teaches_has() {
        let errors = typeck("const scores = [1]; const round = 0; const x = scores[round];");
        assert_eq!(
            errors[0].message,
            "index not proven in bounds — test it first: `scores.has(round) ? scores[round] : fallback`"
        );
    }

    #[test]
    fn indexing_diagnostic_teaches_hoist_for_field_access() {
        let errors = typeck(
            "struct Cart { items: Array<number> } const cart = Cart { items: [1] }; const i = 0; const x = cart.items[i];",
        );
        assert_eq!(
            errors[0].message,
            "index not proven in bounds — proofs track local names; hoist it first: const items = cart.items, then items.has(i) ? ..."
        );
    }

    #[test]
    fn indexing_diagnostic_teaches_hoist_for_nested_index() {
        let errors = typeck(
            "const matrix = [[1]]; const i = 0; const j = 0; const x = matrix.has(i) ? matrix[i][j] : 0;",
        );
        assert_eq!(
            errors[0].message,
            "index not proven in bounds — proofs track local names; hoist it first: const items = matrix[i], then items.has(j) ? ..."
        );
    }
}
