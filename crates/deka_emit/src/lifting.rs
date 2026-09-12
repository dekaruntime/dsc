// Statement lifting keeps bodyless propagation in its owning function. Only
// expressions containing a match need this path; other output stays compact.
fn expression_children<'e, 'a>(expr: &'e Expr<'a>) -> Vec<&'e Expr<'a>> {
    match expr {
        Expr::Binary { left, right, .. } => vec![left, right],
        Expr::Unary { operand, .. } => vec![operand],
        Expr::Call { callee, args, .. } => std::iter::once(*callee).chain(args.iter()).collect(),
        Expr::FieldAccess { object, .. }
        | Expr::Await { expr: object, .. }
        | Expr::Safe { expr: object, .. }
        | Expr::Paren { expr: object, .. }
        | Expr::Spread { expr: object, .. } => vec![object],
        Expr::IndexAccess { object, index, .. } => vec![object, index],
        Expr::StructLiteral { fields, .. } => fields.iter().map(|f| &f.value).collect(),
        Expr::EnumConstructor { payload, .. } => payload.iter().copied().collect(),
        Expr::Match {
            scrutinee, arms, ..
        } => std::iter::once(*scrutinee)
            .chain(arms.iter().map(|a| &a.body))
            .collect(),
        Expr::Ternary {
            condition,
            then_branch,
            else_branch,
            ..
        } => vec![condition, then_branch, else_branch],
        Expr::Bridge { args: elements, .. } | Expr::Array { elements, .. } => {
            elements.iter().collect()
        }
        Expr::Object { fields, .. } => fields.iter().map(|f| &f.value).collect(),
        Expr::TemplateLiteral { parts, .. } => parts
            .iter()
            .filter_map(|p| match p {
                deka_syntax::TemplatePart::Expr(e) => Some(*e),
                _ => None,
            })
            .collect(),
        Expr::JsxElement { element, .. } => element
            .attributes
            .iter()
            .filter_map(|a| a.value.as_ref())
            .chain(element.children.iter())
            .collect(),
        Expr::JsxFragment { children, .. } => children.iter().collect(),
        // Function/default-parameter and build bodies own their own scopes.
        _ => Vec::new(),
    }
}

fn needs_lifting(expr: &Expr<'_>) -> bool {
    matches!(expr, Expr::Match { .. }) || expression_children(expr).into_iter().any(needs_lifting)
}

impl<'a> Emitter<'a> {
    fn lift_value(&mut self, expr: &Expr<'a>) -> Result<String, String> {
        if let Some(value) = self.lifted_values.get(&(expr as *const _)) {
            return Ok(value.clone());
        }
        if !needs_lifting(expr) {
            return self.emit_expr_to_string(expr);
        }
        if let Expr::Call { callee, args, .. } = expr {
            if args.iter().any(is_hole_expr) {
                // Captures are functions: fixed operands are evaluated when
                // invoked, and a bodyless arm returns from the capture.
                let outer = std::mem::take(&mut self.out);
                let saved = self.lifted_values.clone();
                self.out.push_str("(__deka_hole_0) => {\n");
                let (callee, receiver) = if let Expr::FieldAccess { object, .. } = callee {
                    let receiver = self.lift_temp(object)?;
                    (format!("{}.call", self.lift_temp(callee)?), Some(receiver))
                } else {
                    (self.lift_temp(callee)?, None)
                };
                let mut values: Vec<String> = receiver.into_iter().collect();
                for arg in *args {
                    values.push(if is_hole_expr(arg) {
                        "__deka_hole_0".into()
                    } else {
                        self.lift_temp(arg)?
                    });
                }
                self.out
                    .push_str(&format!("return {callee}({});\n}}", values.join(", ")));
                let function = std::mem::replace(&mut self.out, outer);
                self.lifted_values = saved;
                return Ok(function);
            }
        }
        if self.exception_form(expr) == Some(deka_syntax::typeck::ExceptionEmit::ToResult) {
            if let Expr::Call {
                callee: Expr::FieldAccess { object, .. },
                ..
            } = peel_exception_parens(expr)
            {
                let temp = format!("__deka_conversion_{}", self.next_match_id());
                self.out.push_str(&format!("let {temp};\ntry {{\n"));
                let value = self.lift_value(object)?;
                self.out.push_str(&format!("{temp} = Result.Ok({value});\n}} catch (__deka_error) {{\n{temp} = Result.Err(__deka_error);\n}}\n"));
                return Ok(temp);
            }
        }
        if let Expr::Match {
            scrutinee, arms, ..
        } = expr
        {
            return self.emit_match_value_statements(scrutinee, arms);
        }
        if let Expr::Paren { expr, .. } | Expr::Safe { expr, .. } = expr {
            return self.lift_value(expr);
        }
        let saved = self.lifted_values.clone();
        let result = self.lift_operands(expr);
        self.lifted_values = saved;
        result
    }

    fn lift_temp(&mut self, expr: &Expr<'a>) -> Result<String, String> {
        if let Expr::Spread { expr: inner, .. } = expr {
            let value = self.lift_value(inner)?;
            let temp = format!("__deka_spread_{}", self.next_match_id());
            self.out
                .push_str(&format!("const {temp} = [...{value}];\n"));
            let spread = format!("...{temp}");
            self.lifted_values.insert(expr as *const _, spread.clone());
            return Ok(spread);
        }
        let value = self.lift_value(expr)?;
        let temp = format!("__deka_operand_{}", self.next_match_id());
        self.out.push_str(&format!("const {temp} = {value};\n"));
        self.lifted_values.insert(expr as *const _, temp.clone());
        Ok(temp)
    }

    fn lift_operands(&mut self, expr: &Expr<'a>) -> Result<String, String> {
        // Lazy operands are emitted in their authored branch, never hoisted.
        if let Expr::Ternary {
            condition,
            then_branch,
            else_branch,
            ..
        } = expr
        {
            let condition = self.lift_value(condition)?;
            let temp = format!("__deka_choice_{}", self.next_match_id());
            self.out
                .push_str(&format!("let {temp};\nif ({condition}) {{\n"));
            let value = self.lift_value(then_branch)?;
            self.out
                .push_str(&format!("{temp} = {value};\n}} else {{\n"));
            let value = self.lift_value(else_branch)?;
            self.out.push_str(&format!("{temp} = {value};\n}}\n"));
            return Ok(temp);
        }
        if let Expr::Binary {
            op: op @ (BinOp::And | BinOp::Or),
            left,
            right,
            ..
        } = expr
        {
            let left = self.lift_value(left)?;
            let temp = format!("__deka_choice_{}", self.next_match_id());
            let test = if *op == BinOp::And {
                temp.clone()
            } else {
                format!("!{temp}")
            };
            self.out
                .push_str(&format!("let {temp} = {left};\nif ({test}) {{\n"));
            let right = self.lift_value(right)?;
            self.out.push_str(&format!("{temp} = {right};\n}}\n"));
            return Ok(temp);
        }
        if let Expr::Binary {
            op:
                op @ (BinOp::Assign
                | BinOp::AddAssign
                | BinOp::SubAssign
                | BinOp::MulAssign
                | BinOp::DivAssign
                | BinOp::ModAssign),
            left,
            right,
            ..
        } = expr
        {
            // Stabilize the reference, not the assigned value.
            for child in expression_children(left) {
                self.lift_temp(child)?;
            }
            let reference = self.emit_expr_to_string(left)?;
            let old = if *op != BinOp::Assign {
                let temp = format!("__deka_previous_{}", self.next_match_id());
                self.out.push_str(&format!("const {temp} = {reference};\n"));
                Some(temp)
            } else {
                None
            };
            let value = self.lift_value(right)?;
            return Ok(if let Some(old) = old {
                let operator = bin_op_str(*op).trim_end_matches('=');
                format!("({reference} = {old} {operator} ({value}))")
            } else {
                format!("({reference} = {value})")
            });
        }
        if let Expr::Binary {
            op: BinOp::Pipe,
            left,
            right,
            ..
        } = expr
        {
            if let Expr::Call { callee, args, .. } = right {
                if !args.iter().any(is_hole_expr) {
                    let (callee, receiver) = if let Expr::FieldAccess { object, .. } = callee {
                        let receiver = self.lift_temp(object)?;
                        (format!("{}.call", self.lift_temp(callee)?), Some(receiver))
                    } else {
                        (self.lift_temp(callee)?, None)
                    };
                    let mut values: Vec<String> = receiver.into_iter().collect();
                    values.push(self.lift_temp(left)?);
                    for arg in *args {
                        values.push(self.lift_temp(arg)?);
                    }
                    return Ok(format!("{callee}({})", values.join(", ")));
                }
            }
            let callee = self.lift_temp(right)?;
            let arg = self.lift_temp(left)?;
            return Ok(format!("{callee}({arg})"));
        }
        if let Expr::JsxElement { element, .. } = expr {
            for attr in element.attributes {
                if let Some(value) = &attr.value {
                    if attr.name.is_empty() {
                        let emitted = self.lift_value(value)?;
                        let temp = format!("__deka_props_spread_{}", self.next_match_id());
                        self.out
                            .push_str(&format!("const {temp} = {{...{emitted}}};\n"));
                        self.lifted_values.insert(value as *const _, temp);
                    } else {
                        self.lift_temp(value)?;
                    }
                }
            }
            for child in element.children {
                self.lift_temp(child)?;
            }
            return self.emit_expr_to_string(expr);
        }
        if let Expr::Object { fields, .. } = expr {
            for field in *fields {
                if field.key.is_empty() {
                    // Object spread copies properties at its source position,
                    // before a later operand can mutate the original object.
                    let value = self.lift_value(&field.value)?;
                    let temp = format!("__deka_object_spread_{}", self.next_match_id());
                    self.out
                        .push_str(&format!("const {temp} = {{...{value}}};\n"));
                    self.lifted_values.insert(&field.value as *const _, temp);
                } else {
                    self.lift_temp(&field.value)?;
                }
            }
            return self.emit_expr_to_string(expr);
        }
        if let Expr::Call { callee, args, .. } = expr {
            let ptr = expr as *const _;
            let rewritten = self.method_calls.contains_key(&ptr)
                || self.unwrap_calls.contains_key(&ptr)
                || self.type_of_calls.contains(&ptr)
                || self.signature_calls.contains_key(&ptr)
                || self.json_calls.contains_key(&ptr)
                || self.array_builtin_calls.contains_key(&ptr)
                || self.number_math_calls.contains_key(&ptr)
                || self.static_type_calls.contains_key(&ptr)
                || self.exception_form(expr).is_some();
            let mut native_method = None;
            // Keep compiler-resolved method/primitive call rewrites attached to
            // the original AST, and stabilize their receiver before arguments.
            if let Expr::FieldAccess { object, .. } = callee {
                if !matches!(
                    object,
                    Expr::Identifier {
                        name: "Exception" | "Result" | "Option",
                        ..
                    }
                ) {
                    let receiver = self.lift_temp(object)?;
                    if !rewritten {
                        let method = self.lift_temp(callee)?;
                        native_method = Some((method, receiver));
                    }
                }
            } else if !is_panic_callee(callee)
                && !self.unwrap_calls.contains_key(&(expr as *const _))
            {
                self.lift_temp(callee)?;
            }
            let mut values = Vec::new();
            for arg in *args {
                values.push(self.lift_temp(arg)?);
            }
            if let Some((method, receiver)) = native_method {
                values.insert(0, receiver);
                return Ok(format!("{method}.call({})", values.join(", ")));
            }
        } else {
            for child in expression_children(expr) {
                self.lift_temp(child)?;
            }
        }
        self.emit_expr_to_string(expr)
    }
}

impl<'a> Emitter<'a> {
    fn meltable_result(&self, expr: &Expr<'a>) -> bool {
        match peel_exception_parens(expr) {
            Expr::EnumConstructor {
                enum_name: "Result",
                ..
            } => self.exception_form(expr).is_none(),
            Expr::Unsafe { source, .. } => !js_has_top_level_await(source),
            Expr::Await { expr, .. }
                if matches!(peel_exception_parens(expr), Expr::Unsafe { .. }) =>
            {
                true
            }
            Expr::Ternary {
                then_branch,
                else_branch,
                ..
            } => self.meltable_result(then_branch) && self.meltable_result(else_branch),
            _ => self.exception_form(expr) == Some(deka_syntax::typeck::ExceptionEmit::ToResult),
        }
    }

    fn emit_result_scalars(
        &mut self,
        expr: &Expr<'a>,
        ok: &str,
        value: &str,
        error: &str,
    ) -> Result<(), String> {
        let unsafe_expr = match peel_exception_parens(expr) {
            Expr::Await { expr, .. } => peel_exception_parens(expr),
            expr => expr,
        };
        if let Expr::Unsafe {
            source,
            result_type,
            ..
        } = unsafe_expr
        {
            let (invocation, _) = self.unsafe_invocation(source);
            let payload = unsafe_error_payload(result_type.is_none());
            self.out.push_str(&format!("try {{\n{value} = {invocation};\n{ok} = true;\n}} catch (err) {{\n{error} = {payload};\n{ok} = false;\n}}\n"));
            return Ok(());
        }
        match peel_exception_parens(expr) {
            Expr::EnumConstructor {
                case_name,
                payload: Some(payload),
                ..
            } => {
                let payload = self.lift_value(payload)?;
                let field = if *case_name == "Ok" { value } else { error };
                self.out.push_str(&format!(
                    "{field} = {payload};\n{ok} = {};\n",
                    *case_name == "Ok"
                ));
            }
            Expr::Ternary {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                let condition = self.lift_value(condition)?;
                self.out.push_str(&format!("if ({condition}) {{\n"));
                self.emit_result_scalars(then_branch, ok, value, error)?;
                self.out.push_str("} else {\n");
                self.emit_result_scalars(else_branch, ok, value, error)?;
                self.out.push_str("}\n");
            }
            Expr::Call {
                callee: Expr::FieldAccess { object, .. },
                ..
            } => {
                // Conversion to locally consumed data needs scalar channels,
                // but still protects only the authored exception invocation.
                self.out.push_str("try {\n");
                let invocation = self.lift_value(object)?;
                self.out.push_str(&format!("{value} = {invocation};\n{ok} = true;\n}} catch (__deka_error) {{\n{error} = __deka_error;\n{ok} = false;\n}}\n"));
            }
            _ => return Err("invalid scalar Result producer".into()),
        }
        Ok(())
    }

    fn emit_melted_match(
        &mut self,
        expr: &Expr<'a>,
        arms: &[deka_syntax::MatchArm<'a>],
        result: Option<&str>,
    ) -> Result<bool, String> {
        let stored = self.melted_results.get(&(expr as *const _)).cloned();
        if (stored.is_none() && !self.meltable_result(expr)) || !payload_only_arms(arms) {
            return Ok(false);
        }
        let (ok, value, error) = if let Some(stored) = stored {
            stored
        } else {
            self.melt_result(expr)?
        };
        for (i, arm) in arms.iter().enumerate() {
            let (condition, payload, source) = match &arm.pattern {
                Pattern::Constructor { name, payload, .. } => {
                    let source = if *name == "Ok" { &value } else { &error };
                    let mut condition = format!("{ok} === {}", *name == "Ok");
                    if let Some(payload) = payload {
                        condition
                            .push_str(&format!(" && ({})", self.match_condition(payload, source)));
                    }
                    (condition, *payload, source)
                }
                _ => ("true".into(), None, &value),
            };
            if i > 0 {
                self.out.push_str("else ");
            }
            self.out.push_str(&format!("if ({condition}) {{\n"));
            if let Some(payload) = payload {
                self.emit_pattern_bindings(payload, source, 0)?;
            }
            self.emit_passthrough_binding(arm, source);
            self.emit_handling_arm(arm, result, None)?;
            self.out.push_str("}\n");
        }
        self.out
            .push_str("else { throw new Error(\"non-exhaustive match\"); }\n");
        Ok(true)
    }
}

fn default_needs_lifting(param: &deka_syntax::Param<'_>) -> bool {
    param.default_value.as_ref().is_some_and(needs_lifting)
}

impl<'a> Emitter<'a> {
    fn emit_lifted_for(
        &mut self,
        init: Option<&ForInit<'a>>,
        condition: Option<&Expr<'a>>,
        step: Option<&Expr<'a>>,
        body: &[Stmt<'a>],
    ) -> Result<(), String> {
        self.out.push_str("{\n");
        let initializer = if let Some(init) = init {
            let expr = match init {
                ForInit::Const { value, .. }
                | ForInit::Let { value, .. }
                | ForInit::Expr(value) => value,
            };
            let value = self.lift_value(expr)?;
            match init {
                ForInit::Const { name, .. } => format!("const {name} = {value}"),
                ForInit::Let { name, .. } => format!("let {name} = {value}"),
                _ => value,
            }
        } else {
            String::new()
        };
        let first = format!("__deka_first_{}", self.next_match_id());
        self.out.push_str(&format!(
            "let {first} = true;\nfor ({initializer};;) {{\nif (!{first}) {{\n"
        ));
        if let Some(step) = step {
            let step = self.lift_value(step)?;
            self.out.push_str(&format!("{step};\n"));
        }
        self.out.push_str(&format!("}}\n{first} = false;\n"));
        if let Some(condition) = condition {
            let condition = self.lift_value(condition)?;
            self.out
                .push_str(&format!("if (!({condition})) {{ break; }}\n"));
        }
        for stmt in body {
            self.emit_stmt(stmt)?;
            self.out.push('\n');
        }
        self.out.push_str("}\n}\n");
        Ok(())
    }
}

impl<'a> Emitter<'a> {
    fn melt_result(&mut self, value: &Expr<'a>) -> Result<(String, String, String), String> {
        let id = self.next_match_id();
        let ok = format!("__deka_result_ok_{id}");
        let payload = format!("__deka_result_value_{id}");
        let error = format!("__deka_result_error_{id}");
        self.out
            .push_str(&format!("let {ok}, {payload}, {error};\n"));
        self.emit_result_scalars(value, &ok, &payload, &error)?;
        Ok((ok, payload, error))
    }

    fn emit_body(&mut self, body: &[Stmt<'a>]) -> Result<(), String> {
        let saved = self.melted_results.clone();
        for (index, stmt) in body.iter().enumerate() {
            if let Stmt::Const { name, value, .. } = stmt {
                if self.meltable_result(value) {
                    let mut uses = 0;
                    let mut opaque = false;
                    for later in body {
                        deka_syntax::visit::walk_stmt(later, &mut |expr| match expr {
                            Expr::Identifier { name: used, .. } if used == name => uses += 1,
                            Expr::Unsafe { .. } => opaque = true,
                            _ => {}
                        });
                    }
                    // Restrict the consumer to this block and payload patterns:
                    // closures or whole-value bindings can observe identity.
                    let consumer = body[index + 1..].iter().find_map(|later| {
                        let expr = match later {
                            Stmt::Const { value, .. } | Stmt::Let { value, .. } => value,
                            Stmt::Return { value: Some(value), .. } => value,
                            Stmt::Expr { expr, .. } => expr,
                            _ => return None,
                        };
                        match peel_exception_parens(expr) {
                            Expr::Match { scrutinee, arms, .. } if payload_only_arms(arms)
                                && matches!(peel_exception_parens(scrutinee), Expr::Identifier { name: used, .. } if used == name) => Some(*scrutinee),
                            _ => None,
                        }
                    });
                    if uses == 1 && !opaque {
                        if let Some(consumer) = consumer {
                            let scalars = self.melt_result(value)?;
                            self.melted_results.insert(consumer as *const _, scalars);
                            continue;
                        }
                    }
                }
            }
            self.emit_stmt(stmt)?;
            self.out.push('\n');
        }
        self.melted_results = saved;
        Ok(())
    }
}

fn payload_only_arms(arms: &[deka_syntax::MatchArm<'_>]) -> bool {
    arms.iter().all(|arm| {
        matches!(
            arm.pattern,
            Pattern::Constructor {
                name: "Ok" | "Err",
                ..
            } | Pattern::Wildcard { .. }
        )
    })
}

fn unsafe_error_payload(bare: bool) -> &'static str {
    if bare {
        "(err instanceof Error ? (err.message || String(err)) : String(err))"
    } else {
        "(err instanceof Error ? err : new Error(String(err)))"
    }
}
