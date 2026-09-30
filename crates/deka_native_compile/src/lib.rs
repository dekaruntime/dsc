//! Isolated native target. The existing DS parser/checker remains authoritative.
mod emit;
use deka_native_ir::{
    Condition, FORMAT_VERSION, Number, Program, State, Style, StyleWhen, Template, Text, Update,
};
use deka_syntax::{Diagnostic, Severity, ast::*};
pub use emit::emit_rust;
use std::collections::HashMap;

type Result<T> = std::result::Result<T, String>;

pub fn compile(source: &str) -> Result<Program> {
    let arena = bumpalo::Bump::new();
    let parsed = deka_syntax::parse(source, &arena);
    diagnostics(&parsed.errors)?;
    let ast = parsed.program.ok_or("parser produced no program")?;
    let checked = deka_syntax::check_program(&ast, source);
    diagnostics(&checked.errors)?;
    let [
        Stmt::Export {
            decl:
                ExportDecl::Function {
                    name,
                    params,
                    type_params,
                    body,
                    is_async: false,
                    ..
                },
            ..
        },
    ] = ast.statements
    else {
        return Err(
            "native slice requires exactly one exported, synchronous component function".into(),
        );
    };
    if !params.is_empty() || !type_params.is_empty() {
        return Err("native slice does not yet support component props or type parameters".into());
    }
    let mut lower = Lower {
        states: vec![],
        bindings: HashMap::new(),
        setters: HashMap::new(),
        mutable: std::collections::HashSet::new(),
        handlers: vec![],
    };
    let mut root = None;
    for statement in *body {
        if root.is_some() {
            return Err("native component must end at its return".into());
        }
        match statement {
            Stmt::Let {
                name,
                value: Expr::Number { value, .. },
                ..
            } if value.is_finite() => {
                if lower.bindings.contains_key(*name) || lower.setters.contains_key(*name) {
                    return Err(format!("duplicate native state binding: {name}"));
                }
                let index = lower.states.len();
                lower.states.push(State {
                    name: (*name).into(),
                    initial: *value,
                });
                lower.bindings.insert((*name).into(), index);
                lower.mutable.insert((*name).into());
            }
            Stmt::TupleBinding {
                names,
                value: Expr::Call { callee, args, .. },
                is_const: true,
                ..
            } if ident(callee) == Some("useState") && names.len() == 2 => {
                let [Expr::Number { value, .. }] = *args else {
                    return Err("native state requires a numeric literal initializer".into());
                };
                let index = lower.states.len();
                if !value.is_finite() {
                    return Err("state initializer must be finite".into());
                }
                lower.states.push(State {
                    name: names[0].into(),
                    initial: *value,
                });
                lower.bindings.insert(names[0].into(), index);
                lower.setters.insert(names[1].into(), index);
            }
            Stmt::Return {
                value: Some(expr), ..
            } => root = Some(lower.node(expr, "root".into())?),
            _ => {
                return Err(
                    "native slice supports numeric let state (or legacy useState) followed by a JSX return".into(),
                );
            }
        }
    }
    Ok(Program {
        format: FORMAT_VERSION,
        component: (*name).into(),
        states: lower.states,
        root: root.ok_or("component requires a JSX return")?,
        handlers: lower.handlers,
    })
}
fn diagnostics(items: &[Diagnostic]) -> Result<()> {
    let errors: Vec<_> = items
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| format!("{}:{}: {}", d.line, d.column, d.message))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}
fn ident<'a>(expr: &Expr<'a>) -> Option<&'a str> {
    if let Expr::Identifier { name, .. } = expr {
        Some(name)
    } else {
        None
    }
}
struct Lower {
    states: Vec<State>,
    bindings: HashMap<String, usize>,
    setters: HashMap<String, usize>,
    handlers: Vec<Update>,
    mutable: std::collections::HashSet<String>,
}
impl Lower {
    fn update(&self, expr: &Expr<'_>) -> Result<Update> {
        match expr {
            Expr::Paren { expr, .. } => self.update(expr),
            Expr::Binary {
                left, op, right, ..
            } => {
                let name = ident(left).ok_or("native assignment needs a local state name")?;
                if !self.mutable.contains(name) {
                    return Err(format!(
                        "native assignment requires mutable let state: {name}"
                    ));
                }
                let state = self.bindings[name];
                let value = self.number(right)?;
                let current = Box::new(Number::State(state));
                let value = match op {
                    BinOp::Assign => value,
                    BinOp::AddAssign => Number::Add(current, Box::new(value)),
                    BinOp::SubAssign => Number::Sub(current, Box::new(value)),
                    BinOp::MulAssign => Number::Mul(current, Box::new(value)),
                    _ => return Err("native assignment supports =, +=, -= and *=".into()),
                };
                Ok(Update { state, value })
            }
            Expr::Call { callee, args, .. } => {
                let state = ident(callee)
                    .and_then(|name| self.setters.get(name))
                    .copied()
                    .ok_or("native handler must assign local state or call a legacy setter")?;
                let [value] = *args else {
                    return Err("native setter requires one numeric expression".into());
                };
                Ok(Update {
                    state,
                    value: self.number(value)?,
                })
            }
            _ => Err("native handler requires a state assignment".into()),
        }
    }
    fn number(&self, expr: &Expr<'_>) -> Result<Number> {
        match expr {
            Expr::Number { value, .. } if value.is_finite() => Ok(Number::Literal(*value)),
            Expr::Identifier { name, .. } => self
                .bindings
                .get(*name)
                .copied()
                .map(Number::State)
                .ok_or_else(|| format!("unsupported native numeric binding: {name}")),
            Expr::Paren { expr, .. } => self.number(expr),
            Expr::Unary {
                op: UnOp::Neg,
                operand,
                ..
            } => Ok(Number::Sub(
                Box::new(Number::Literal(0.)),
                Box::new(self.number(operand)?),
            )),
            Expr::Binary {
                op, left, right, ..
            } => {
                let a = Box::new(self.number(left)?);
                let b = Box::new(self.number(right)?);
                match op {
                    BinOp::Add => Ok(Number::Add(a, b)),
                    BinOp::Sub => Ok(Number::Sub(a, b)),
                    BinOp::Mul => Ok(Number::Mul(a, b)),
                    _ => Err("native numeric operators currently support +, - and *".into()),
                }
            }
            _ => Err("unsupported native numeric expression".into()),
        }
    }
    fn node(&mut self, expr: &Expr<'_>, id: String) -> Result<Template> {
        let mut node = Template {
            id: id.clone(),
            style: Style::default(),
            style_when: None,
            visible_when: None,
            text: None,
            on_click: None,
            children: vec![],
        };
        match expr {
            Expr::Paren { expr, .. } => return self.node(expr, id),
            Expr::Ternary {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                if id == "root" {
                    return Err("conditional native content needs a container root".into());
                }
                if !matches!(else_branch, Expr::None { .. }) {
                    return Err(
                        "native conditional children require None as the absent branch".into(),
                    );
                }
                let mut child = self.node(then_branch, id)?;
                child.visible_when = Some(match condition {
                    Expr::Binary {
                        left,
                        op: BinOp::Eq,
                        right,
                        ..
                    } => Condition::Equal(self.number(left)?, self.number(right)?),
                    _ => return Err("native presence requires numeric equality".into()),
                });
                return Ok(child);
            }
            Expr::JsxElement { element, .. } => {
                if element.tag == "view" && id != "root" {
                    return Err("this native slice supports view only as the component root".into());
                }
                node.style = deka_native_ir::element_style(element.tag)?;
                for attr in element.attributes {
                    match (attr.name, &attr.value) {
                        ("className", Some(Expr::String { value, .. })) => {
                            deka_native_ir::apply_classes(&mut node.style, value)?
                        }
                        (
                            "className",
                            Some(Expr::Ternary {
                                condition,
                                then_branch,
                                else_branch,
                                ..
                            }),
                        ) => {
                            let (Expr::String { value: yes, .. }, Expr::String { value: no, .. }) =
                                (then_branch, else_branch)
                            else {
                                return Err(
                                    "native conditional classes require two string literals".into(),
                                );
                            };
                            let mut then_style = node.style.clone();
                            let mut else_style = node.style.clone();
                            deka_native_ir::apply_classes(&mut then_style, yes)?;
                            deka_native_ir::apply_classes(&mut else_style, no)?;
                            node.style_when = Some(StyleWhen { condition: match condition {
                                Expr::Binary { left, op: BinOp::Eq, right, .. } => Condition::Equal(self.number(left)?, self.number(right)?),
                                _ => return Err("native conditional classes require a numeric equality, such as open == 1".into()),
                            }, then_style, else_style });
                        }
                        ("onClick", Some(handler)) if element.tag == "button" => {
                            let Expr::Function {
                                params,
                                body,
                                is_async: false,
                                ..
                            } = handler
                            else {
                                return Err(
                                    "native onClick requires an inline synchronous function".into(),
                                );
                            };
                            if !params.is_empty() {
                                return Err(
                                    "native click event arguments are not supported yet".into()
                                );
                            }
                            let expression = match *body {
                                [Stmt::Expr { expr, .. }] | [Stmt::Return { value: Some(expr), .. }] => expr,
                                _ => return Err("native handler requires one state assignment or legacy setter call".into()),
                            };
                            let update = self.update(expression)?;
                            node.on_click = Some(self.handlers.len());
                            self.handlers.push(update);
                        }
                        _ => {
                            return Err(format!(
                                "unsupported native attribute or attribute value: {}",
                                attr.name
                            ));
                        }
                    }
                }
                for (i, child) in element.children.iter().enumerate() {
                    let child = self.node(child, format!("{id}/{i}"))?;
                    if matches!(&child.text, Some(Text::Literal(text)) if text.is_empty()) {
                        continue;
                    }
                    node.children.push(child);
                }
            }
            Expr::String { value, .. } => node.text = Some(Text::Literal((*value).into())),
            Expr::JsxText { value, .. } => node.text = Some(Text::Literal(jsx_text(value))),
            Expr::Call { callee, args, .. } if ident(callee) == Some("string") => {
                let [arg] = *args else {
                    return Err("string requires one argument".into());
                };
                node.text = Some(Text::Number(self.number(arg)?));
            }
            Expr::Identifier { .. }
            | Expr::Number { .. }
            | Expr::Binary { .. }
            | Expr::Unary { .. } => node.text = Some(Text::Number(self.number(expr)?)),
            _ => {
                return Err(
                    "native children support elements, literal text and numeric bindings".into(),
                );
            }
        }
        Ok(node)
    }
}

// Preserve deliberate spaces beside expressions while discarding source indentation.
fn jsx_text(value: &str) -> String {
    let lines: Vec<_> = value.split('\n').collect();
    let last = lines.len() - 1;
    let mut text = String::new();
    for (i, line) in lines.into_iter().enumerate() {
        let line = if i > 0 { line.trim_start() } else { line };
        let line = if i < last { line.trim_end() } else { line };
        if line.is_empty() {
            continue;
        }
        if !text.is_empty() && !text.ends_with(' ') {
            text.push(' ');
        }
        for c in line.chars() {
            if c.is_whitespace() {
                if !text.ends_with(' ') {
                    text.push(' ');
                }
            } else {
                text.push(c);
            }
        }
    }
    text
}
