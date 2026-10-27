//! Translation of core IR functions and expressions to JavaScript.
//!
//! Calling convention: every Bind function is `async function u_f(ctx, …)` whose body starts
//! with `ctx = await rt.enter(ctx);` (one level of call depth, `RecursionLimit` past 10,000);
//! lambdas are `async (ctx, …) => { ctx = await rt.enter(ctx); … }`. Builtins and the wrappers
//! that turn builtins into values do not enter, as in `core::eval`.
//!
//! Expressions become statements plus a final JS expression ([`Code`]): `Let` is `const`,
//! `If`/`Match` with statements in a branch become `if` chains assigning a temporary, a
//! `Return` is `return` and ends the statement list (the rest is never emitted). Operands are
//! evaluated left to right: an operand that may have effects is bound to a temporary before a
//! later operand's statements run.
//!
//! Values (see the runtime's header): `Int`/`Long` are BigInts (`123n`); `Double`/`Float` are
//! numbers, `Float` results rounded with `rt.f32`; `Unit` is `undefined`; `T?` is `null`
//! (`None`) or `rt.some(v)` (`Some(v)`), uniformly for every `T`; records are `rt.record`, variants
//! `rt.variant(tag, …)` and `Result` `rt.variant("Ok" | "Err", [["value", v]])`, fields under
//! their escaped names (`names::js_value`); a distinct value is its underlying value; a
//! `Set` is a frozen array in canonical order and a `Map` a frozen array of frozen `[k, v]`
//! pairs in canonical key order, operated with the runtime's collection builtins and the key
//! comparator of the element / key type (`Codecs::key_order`); UI nodes are `rt.node` /
//! `rt.text` / `rt.EMPTY`.

use std::collections::BTreeMap;

use crate::core::display::js_number_to_string;
use crate::core::ir::{Arm, CoreFunction, CoreProgram, CoreType, Expr, ExprKind, Literal, Prim};
use crate::core::ir::{LocalId, UiNode};
use crate::types::exhaustive::Pat;
use crate::types::ty::Type;

use super::codecs::{Codecs, substitute};
use super::names::{js_local, js_string, js_value};

/// Longest `listOf` passed as arguments; longer literals use `rt.listFrom([...])`.
const MAX_SPREAD: usize = 1000;

/// One function, with the codec helpers its server calls need placed before it.
#[cfg(test)]
pub fn function(function: &CoreFunction, program: &CoreProgram) -> String {
    let mut codecs = Codecs::new(program);
    let text = function_with(function, program, &mut codecs);
    let definitions = codecs.definitions();
    if definitions.is_empty() {
        text
    } else {
        format!("{definitions}\n{text}")
    }
}

/// One function; codec helpers are added to `codecs`.
pub(crate) fn function_with<'p>(
    function: &CoreFunction,
    program: &'p CoreProgram,
    codecs: &mut Codecs<'p>,
) -> String {
    let mut generator = Generator {
        program,
        codecs,
        names: BTreeMap::new(),
        next_temp: 0,
    };
    let mut params = vec!["ctx".to_string()];
    for (local, name, _) in &function.params {
        generator.names.insert(*local, js_local(name, *local));
        params.push(js_local(name, *local));
    }
    let mut twice = Vec::new();
    collect_names(&function.body, &mut generator.names, &mut twice);
    let body = match twice.first() {
        // An internal error, in release builds too (never a duplicate declaration).
        Some(local) => format!(
            "throw new Error({});",
            js_string(&format!(
                "bindc: internal error: local {local} is bound twice in function {}",
                function.name
            ))
        ),
        None => generator.body(&function.body),
    };
    format!(
        "async function {}({}) {{\n{body}\n}}\n",
        js_value(&function.name),
        params.join(", ")
    )
}

/// Names every local bound inside `expr` (lets, pattern variables, lambda parameters) and
/// returns the locals bound more than once.
///
/// Every local becomes a `const` (or parameter) named after its id, so a `LocalId` bound
/// twice in one function would be declared twice in one JS scope (a SyntaxError) or shadow
/// itself. The IR binds each id once per function, which `core::validate` checks (`verify`
/// stops with E9001 otherwise); should a faulty IR still get here, the function is emitted as
/// one that throws instead of as invalid JS (`function_with`). Every `tests/codegen` case is
/// translated by `backend::js::tests::every_codegen_case_translates`.
fn collect_names(expr: &Expr, names: &mut BTreeMap<LocalId, String>, twice: &mut Vec<LocalId>) {
    let mut bind = |local: LocalId, name: &str| {
        if names.insert(local, js_local(name, local)).is_some() {
            twice.push(local);
        }
    };
    match &expr.kind {
        ExprKind::Lambda { params, .. } => {
            for (local, name, _) in params {
                bind(*local, name);
            }
        }
        ExprKind::Let { local, name, .. } => bind(*local, name),
        ExprKind::Match { arms, .. } => {
            for arm in arms {
                for (local, name, _) in &arm.bindings {
                    bind(*local, name);
                }
            }
        }
        _ => {}
    }
    for child in children(expr) {
        collect_names(child, names, twice);
    }
}

fn children(expr: &Expr) -> Vec<&Expr> {
    match &expr.kind {
        ExprKind::Lit(_)
        | ExprKind::Local(_)
        | ExprKind::Global { .. }
        | ExprKind::Builtin { .. }
        | ExprKind::Environment(_)
        | ExprKind::None => Vec::new(),
        ExprKind::Ctor { args, .. }
        | ExprKind::ServerCall { args, .. }
        | ExprKind::Prim { args, .. }
        | ExprKind::Db { args, .. } => args.iter().collect(),
        ExprKind::Ui { args, children, .. } => {
            args.iter().chain(std::iter::once(&**children)).collect()
        }
        ExprKind::Some(inner) | ExprKind::Return(inner) => vec![inner],
        ExprKind::Field { target, .. } => vec![target],
        ExprKind::Call { callee, args } => std::iter::once(&**callee).chain(args).collect(),
        ExprKind::Lambda { body, .. } => vec![body],
        ExprKind::Let { value, body, .. } | ExprKind::Seq { value, body } => vec![value, body],
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => vec![condition, then_branch, else_branch],
        ExprKind::Match { scrutinee, arms } => std::iter::once(&**scrutinee)
            .chain(arms.iter().map(|arm| &arm.body))
            .collect(),
    }
}

/// A rendered JS expression.
#[derive(Clone)]
struct Code {
    text: String,
    /// Usable as an operand or member-access target without parentheses.
    atomic: bool,
    /// Evaluating it has no effect and cannot throw (so it may move past other code).
    pure: bool,
}

fn atom(text: impl Into<String>) -> Code {
    Code {
        text: text.into(),
        atomic: true,
        pure: true,
    }
}

impl Code {
    fn wrapped(&self) -> String {
        if self.atomic {
            self.text.clone()
        } else {
            format!("({})", self.text)
        }
    }

    fn complex(text: impl Into<String>, pure: bool) -> Code {
        Code {
            text: text.into(),
            atomic: false,
            pure,
        }
    }

    fn call(text: impl Into<String>, pure: bool) -> Code {
        Code {
            text: text.into(),
            atomic: true,
            pure,
        }
    }
}

/// The value of an expression: `None` when it diverges (its statements end in `return` or
/// `throw`).
type Lowered = Option<Code>;

/// The tests and binders of one `when` arm.
#[derive(Default)]
struct ArmPlan {
    conditions: Vec<String>,
    accesses: BTreeMap<Vec<(String, usize)>, String>,
}

struct Generator<'a, 'p> {
    program: &'p CoreProgram,
    codecs: &'a mut Codecs<'p>,
    names: BTreeMap<LocalId, String>,
    next_temp: usize,
}

impl Generator<'_, '_> {
    fn temp(&mut self) -> String {
        let name = format!("t{}", self.next_temp);
        self.next_temp += 1;
        name
    }

    fn local(&self, local: LocalId) -> String {
        self.names
            .get(&local)
            .cloned()
            .unwrap_or_else(|| format!("u_{local}"))
    }

    /// The statements of a function or lambda body: enter, the body, `return`.
    fn body(&mut self, body: &Expr) -> String {
        let mut out = vec!["ctx = await rt.enter(ctx);".to_string()];
        if let Some(code) = self.expr(body, &mut out) {
            out.push(format!("return {};", code.text));
        }
        out.join("\n")
    }

    /// Binds `code` to a temporary unless it is a plain name or literal already.
    fn named(&mut self, code: Code, out: &mut Vec<String>) -> String {
        if code.atomic && code.pure && is_simple(&code.text) {
            return code.text;
        }
        let name = self.temp();
        out.push(format!("const {name} = {};", code.text));
        name
    }

    /// Lowers `args` left to right. An earlier operand that may have effects is bound to a
    /// temporary before a later operand's statements; `None` when an operand diverges.
    fn operands(&mut self, args: &[&Expr], out: &mut Vec<String>) -> Option<Vec<Code>> {
        let mut codes: Vec<Code> = Vec::new();
        for arg in args {
            let mut statements = Vec::new();
            let code = self.expr(arg, &mut statements);
            if !statements.is_empty() || code.is_none() {
                for earlier in &mut codes {
                    if !earlier.pure {
                        let name = self.temp();
                        out.push(format!("const {name} = {};", earlier.text));
                        *earlier = atom(name);
                    }
                }
            }
            out.extend(statements);
            codes.push(code?);
        }
        Some(codes)
    }

    fn all(&mut self, args: &[Expr], out: &mut Vec<String>) -> Option<Vec<Code>> {
        let refs: Vec<&Expr> = args.iter().collect();
        self.operands(&refs, out)
    }

    /// A statement that fails at run time: the IR node is not client code (the validator
    /// keeps it out; this only keeps the module valid).
    fn unsupported(&self, what: &str, out: &mut Vec<String>) -> Lowered {
        out.push(format!(
            "throw new Error({});",
            js_string(&format!("bindc: {what} is not client code"))
        ));
        None
    }

    fn expr(&mut self, expr: &Expr, out: &mut Vec<String>) -> Lowered {
        match &expr.kind {
            ExprKind::Lit(literal) => Some(literal_code(literal, &expr.ty)),
            ExprKind::Local(local) => Some(atom(self.local(*local))),
            ExprKind::Global { name, .. } => Some(atom(js_value(name))),
            ExprKind::Builtin { name, instance } => {
                let order = self.key_order(name, instance);
                Some(builtin_value(name, &order, &expr.ty))
            }
            ExprKind::Environment(name) => {
                self.unsupported(&format!("the environment value `{name}`"), out)
            }
            ExprKind::Db { .. } => self.unsupported("database access", out),
            ExprKind::Ctor {
                type_name,
                ctor,
                args,
            } => {
                let values = self.all(args, out)?;
                Some(self.ctor(type_name, ctor, values))
            }
            ExprKind::None => Some(atom("null")),
            ExprKind::Some(inner) => {
                // Uniform options: `Some(v)` is always `{some: v}`, whatever `v`'s type (generic
                // code is emitted once, so the representation cannot depend on it).
                let value = self.expr(inner, out)?;
                Some(Code::call(format!("rt.some({})", value.text), value.pure))
            }
            ExprKind::Call { callee, args } => self.call(callee, args, out),
            ExprKind::ServerCall { function, args } => {
                self.server_call(function, args, &expr.ty, out)
            }
            ExprKind::Lambda { params, body } => {
                let mut inputs = vec!["ctx".to_string()];
                inputs.extend(params.iter().map(|(local, _, _)| self.local(*local)));
                let body = self.body(body);
                Some(Code::complex(
                    format!("async ({}) => {{\n{body}\n}}", inputs.join(", ")),
                    true,
                ))
            }
            ExprKind::Let {
                local, value, body, ..
            } => {
                let value = self.expr(value, out)?;
                out.push(format!("const {} = {};", self.local(*local), value.text));
                self.expr(body, out)
            }
            ExprKind::Seq { value, body } => {
                let value = self.expr(value, out)?;
                if !value.pure {
                    out.push(format!("{};", value.text));
                }
                self.expr(body, out)
            }
            ExprKind::Return(value) => {
                let value = self.expr(value, out)?;
                out.push(format!("return {};", value.text));
                None
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => self.if_expr(condition, then_branch, else_branch, out),
            ExprKind::Match { scrutinee, arms } => self.match_expr(scrutinee, arms, out),
            ExprKind::Field { target, name } => {
                let target = self.expr(target, out)?;
                Some(Code::call(
                    format!("{}.{}", target.wrapped(), js_value(name)),
                    target.pure,
                ))
            }
            ExprKind::Prim { op, args } => self.prim(*op, args, out),
            ExprKind::Ui {
                node,
                args,
                children,
            } => {
                let mut operands: Vec<&Expr> = args.iter().collect();
                operands.push(children);
                let mut values = self.operands(&operands, out)?;
                let pure = values.iter().all(|value| value.pure);
                let children = values.pop().map_or_else(|| "[]".to_string(), |c| c.text);
                let args: Vec<String> = values.into_iter().map(|value| value.text).collect();
                Some(match node {
                    UiNode::Empty => atom("rt.EMPTY"),
                    UiNode::Text => Code::call(
                        format!(
                            "rt.text({})",
                            args.first().map_or("\"\"", |arg| arg.as_str())
                        ),
                        pure,
                    ),
                    _ => Code::call(
                        format!(
                            "rt.node({}, [{}], {children})",
                            js_string(node.name()),
                            args.join(", ")
                        ),
                        pure,
                    ),
                })
            }
        }
    }

    fn ctor(&self, type_name: &str, ctor: &str, values: Vec<Code>) -> Code {
        let pure = values.iter().all(|value| value.pure);
        let declaration = self.program.types.iter().find(|ty| match ty {
            CoreType::Record { name, .. }
            | CoreType::Sum { name, .. }
            | CoreType::Distinct { name, .. } => name == type_name,
        });
        let entries = |fields: &[(String, Type)], values: Vec<Code>| -> String {
            let entries: Vec<String> = fields
                .iter()
                .zip(values)
                .map(|((field, _), value)| {
                    format!("[{}, {}]", js_string(&js_value(field)), value.text)
                })
                .collect();
            format!("[{}]", entries.join(", "))
        };
        match declaration {
            Some(CoreType::Distinct { .. }) => values
                .into_iter()
                .next()
                .unwrap_or_else(|| atom("undefined")),
            Some(CoreType::Sum { name, .. }) if name == "Result" => {
                let value = values
                    .into_iter()
                    .next()
                    .map_or_else(|| "undefined".to_string(), |code| code.text);
                Code::call(
                    format!("rt.variant({}, [[\"value\", {value}]])", js_string(ctor)),
                    pure,
                )
            }
            Some(CoreType::Sum { variants, .. }) => {
                let fields = variants
                    .iter()
                    .find(|(variant, _)| variant == ctor)
                    .map(|(_, fields)| fields.as_slice())
                    .unwrap_or_default();
                Code::call(
                    format!(
                        "rt.variant({}, {})",
                        js_string(ctor),
                        entries(fields, values)
                    ),
                    pure,
                )
            }
            Some(CoreType::Record { fields, .. }) => {
                Code::call(format!("rt.record({})", entries(fields, values)), pure)
            }
            None => Code::call(
                format!(
                    "(() => {{ throw new Error({}); }})()",
                    js_string("bindc: unknown constructor")
                ),
                false,
            ),
        }
    }

    fn call(&mut self, callee: &Expr, args: &[Expr], out: &mut Vec<String>) -> Lowered {
        match &callee.kind {
            ExprKind::Global { name, .. } => {
                let values = self.all(args, out)?;
                let mut inputs = vec!["ctx".to_string()];
                inputs.extend(values.into_iter().map(|value| value.text));
                Some(Code::complex(
                    format!("await {}({})", js_value(name), inputs.join(", ")),
                    false,
                ))
            }
            ExprKind::Builtin { name, instance } => {
                let values = self.all(args, out)?;
                let order = self.key_order(name, instance);
                Some(builtin_call(name, &order, values))
            }
            _ => {
                let mut operands = vec![callee];
                operands.extend(args.iter());
                let mut values = self.operands(&operands, out)?.into_iter();
                let function = values
                    .next()
                    .map_or_else(|| "undefined".to_string(), |code| code.wrapped());
                let mut inputs = vec!["ctx".to_string()];
                inputs.extend(values.map(|value| value.text));
                Some(Code::complex(
                    format!("await {function}({})", inputs.join(", ")),
                    false,
                ))
            }
        }
    }

    /// `server.f(args)`: a POST of the encoded arguments to the endpoint's route, decoded with
    /// the codec of its result type.
    fn server_call(
        &mut self,
        function: &str,
        args: &[Expr],
        result: &Type,
        out: &mut Vec<String>,
    ) -> Lowered {
        let Some(endpoint) = self
            .program
            .functions
            .iter()
            .find(|item| item.name == function)
        else {
            return self.unsupported(&format!("the unknown server function `{function}`"), out);
        };
        let values = self.all(args, out)?;
        let mut entries = Vec::new();
        for ((_, name, ty), value) in endpoint.params.iter().zip(values) {
            let value = self.named(value, out);
            let encoded = self.codecs.encode(ty, &value);
            entries.push(format!("[{}, {encoded}]", js_string(name)));
        }
        let decoder = self.codecs.decoder(result);
        Some(Code::complex(
            format!(
                "await rt.rpc(ctx, API_URL + {}, rt.record([{}]), {decoder})",
                js_string(&format!("/{function}")),
                entries.join(", ")
            ),
            false,
        ))
    }

    /// `if`: a conditional expression (or `&&` / `||`, the lowering of those operators) when
    /// neither branch has statements, otherwise an `if` statement assigning a temporary.
    fn if_expr(
        &mut self,
        condition: &Expr,
        then_branch: &Expr,
        else_branch: &Expr,
        out: &mut Vec<String>,
    ) -> Lowered {
        let condition = self.expr(condition, out)?;
        let mut then_statements = Vec::new();
        let then_code = self.expr(then_branch, &mut then_statements);
        let mut else_statements = Vec::new();
        let else_code = self.expr(else_branch, &mut else_statements);
        if then_statements.is_empty()
            && else_statements.is_empty()
            && let (Some(a), Some(b)) = (&then_code, &else_code)
        {
            let pure = condition.pure && a.pure && b.pure;
            let boolean = |expr: &Expr| match expr.kind {
                ExprKind::Lit(Literal::Bool(value)) => Some(value),
                _ => None,
            };
            let text = match (boolean(then_branch), boolean(else_branch)) {
                (_, Some(false)) => format!("{} && {}", condition.wrapped(), a.wrapped()),
                (Some(true), _) => format!("{} || {}", condition.wrapped(), b.wrapped()),
                _ => format!(
                    "{} ? {} : {}",
                    condition.wrapped(),
                    a.wrapped(),
                    b.wrapped()
                ),
            };
            return Some(Code::complex(text, pure));
        }
        if then_code.is_none() && else_code.is_none() {
            out.push(format!(
                "if ({}) {{\n{}\n}} else {{\n{}\n}}",
                condition.text,
                then_statements.join("\n"),
                else_statements.join("\n")
            ));
            return None;
        }
        let result = self.temp();
        let branch = |mut statements: Vec<String>, code: Lowered| {
            if let Some(code) = code {
                statements.push(format!("{result} = {};", code.text));
            }
            statements.join("\n")
        };
        let then_text = branch(then_statements, then_code);
        let else_text = branch(else_statements, else_code);
        out.push(format!("let {result};"));
        out.push(format!(
            "if ({}) {{\n{then_text}\n}} else {{\n{else_text}\n}}",
            condition.text
        ));
        Some(atom(result))
    }

    // ---- when ----------------------------------------------------------------------------

    /// A `when` as an `if` / `else if` chain over the scrutinee (bound to a name first).
    fn match_expr(&mut self, scrutinee: &Expr, arms: &[Arm], out: &mut Vec<String>) -> Lowered {
        let value = self.expr(scrutinee, out)?;
        let subject = self.named(value, out);
        let mut branches: Vec<(Vec<String>, Vec<String>, Lowered)> = Vec::new();
        let mut fallback: Option<(Vec<String>, Lowered)> = None;
        for arm in arms {
            let mut plan = ArmPlan::default();
            self.plan(&arm.pattern, &subject, &scrutinee.ty, Vec::new(), &mut plan);
            let mut statements: Vec<String> = arm
                .bindings
                .iter()
                .zip(&arm.binding_paths)
                .map(|((local, _, _), path)| {
                    let access = plan
                        .accesses
                        .get(path)
                        .cloned()
                        .unwrap_or_else(|| "undefined".to_string());
                    format!("const {} = {access};", self.local(*local))
                })
                .collect();
            let code = self.expr(&arm.body, &mut statements);
            if plan.conditions.is_empty() {
                // Always matches: later arms never run.
                fallback = Some((statements, code));
                break;
            }
            branches.push((plan.conditions, statements, code));
        }
        if branches.is_empty() {
            let (statements, code) = fallback?;
            out.extend(statements);
            return code;
        }
        let diverges = branches.iter().all(|(_, _, code)| code.is_none())
            && fallback.as_ref().is_none_or(|(_, code)| code.is_none());
        let result = if diverges { None } else { Some(self.temp()) };
        let assign = |mut statements: Vec<String>, code: Lowered| -> String {
            if let (Some(result), Some(code)) = (&result, code) {
                statements.push(format!("{result} = {};", code.text));
            }
            statements.join("\n")
        };
        let mut text = String::new();
        for (index, (conditions, statements, code)) in branches.into_iter().enumerate() {
            let keyword = if index == 0 { "if" } else { " else if" };
            text.push_str(&format!(
                "{keyword} ({}) {{\n{}\n}}",
                conditions.join(" && "),
                assign(statements, code)
            ));
        }
        let otherwise = match fallback {
            Some((statements, code)) => assign(statements, code),
            None => format!(
                "throw new Error({});",
                js_string("bindc: no `when` arm matched")
            ),
        };
        text.push_str(&format!(" else {{\n{otherwise}\n}}"));
        if let Some(result) = &result {
            out.push(format!("let {result};"));
        }
        out.push(text);
        result.map(atom)
    }

    /// Adds the tests of `pattern` against the value at `access` (of type `ty`, at `path`).
    fn plan(
        &self,
        pattern: &Pat,
        access: &str,
        ty: &Type,
        path: Vec<(String, usize)>,
        plan: &mut ArmPlan,
    ) {
        plan.accesses.insert(path.clone(), access.to_string());
        let (type_name, type_args) = match ty {
            Type::Con(name, args) => (name.as_str(), args.as_slice()),
            _ => ("", &[][..]),
        };
        let (name, subs) = match pattern {
            Pat::Wild => return,
            Pat::Lit(text) => {
                let literal = match type_name {
                    "Int" | "Long" => int_pattern(text),
                    // The checker records string patterns as Rust debug literals.
                    "String" => rust_debug_string(text).map(|value| js_string(&value)),
                    _ => None,
                };
                plan.conditions.push(match literal {
                    Some(literal) => format!("{access} === {literal}"),
                    None => "false".to_string(),
                });
                return;
            }
            Pat::Ctor(name, subs) => (name.as_str(), subs.as_slice()),
        };
        let child = |index: usize| -> Vec<(String, usize)> {
            let mut child = path.clone();
            child.push((name.to_string(), index));
            child
        };
        match (type_name, name, subs) {
            ("Boolean", "true", []) => plan.conditions.push(access.to_string()),
            ("Boolean", "false", []) => plan.conditions.push(format!("!{access}")),
            ("Option", "null", []) => plan.conditions.push(format!("{access} === null")),
            ("Option", "Some", [sub]) => {
                plan.conditions.push(format!("{access} !== null"));
                let inner = type_args.first().cloned().unwrap_or(Type::Error);
                self.plan(sub, &format!("{access}.some"), &inner, child(0), plan);
            }
            _ => {
                let declaration = self.program.types.iter().find(|item| match item {
                    CoreType::Record { name, .. }
                    | CoreType::Sum { name, .. }
                    | CoreType::Distinct { name, .. } => name == type_name,
                });
                let markers = |params: &[String]| -> BTreeMap<String, Type> {
                    params
                        .iter()
                        .zip(type_args)
                        .map(|(param, arg)| (format!("'{param}"), arg.clone()))
                        .collect()
                };
                match declaration {
                    Some(CoreType::Distinct { underlying, .. }) => {
                        if let [sub] = subs {
                            self.plan(sub, access, underlying, child(0), plan);
                        }
                    }
                    Some(CoreType::Record { params, fields, .. }) => {
                        let markers = markers(params);
                        for (index, (sub, (field, field_ty))) in subs.iter().zip(fields).enumerate()
                        {
                            let next = format!("{access}.{}", js_value(field));
                            let field_ty = substitute(field_ty, &markers);
                            self.plan(sub, &next, &field_ty, child(index), plan);
                        }
                    }
                    Some(CoreType::Sum {
                        name: sum,
                        params,
                        variants,
                    }) => {
                        let Some((_, fields)) =
                            variants.iter().find(|(variant, _)| variant == name)
                        else {
                            plan.conditions.push("false".to_string());
                            return;
                        };
                        if variants.len() > 1 {
                            plan.conditions
                                .push(format!("{access}.tag === {}", js_string(name)));
                        }
                        let markers = markers(params);
                        for (index, (sub, (field, field_ty))) in subs.iter().zip(fields).enumerate()
                        {
                            let next = if sum == "Result" {
                                format!("{access}.value")
                            } else {
                                format!("{access}.{}", js_value(field))
                            };
                            let field_ty = substitute(field_ty, &markers);
                            self.plan(sub, &next, &field_ty, child(index), plan);
                        }
                    }
                    None => plan.conditions.push("false".to_string()),
                }
            }
        }
    }

    // ---- primitives ----------------------------------------------------------------------

    fn prim(&mut self, op: Prim, args: &[Expr], out: &mut Vec<String>) -> Lowered {
        use Prim::*;
        if matches!(op, Eq | NotEq) {
            // A comparison with `null` asks whether the other operand is absent.
            let other = match args {
                [a, b] if b.kind == ExprKind::None => Some(a),
                [a, b] if a.kind == ExprKind::None => Some(b),
                _ => None,
            };
            if let Some(other) = other {
                let value = self.expr(other, out)?;
                let symbol = if op == Eq { "===" } else { "!==" };
                return Some(Code::complex(
                    format!("{} {symbol} null", value.wrapped()),
                    value.pure,
                ));
            }
        }
        let operand_type = args
            .first()
            .map(|arg| arg.ty.clone())
            .unwrap_or(Type::Error);
        let values = self.all(args, out)?;
        let pure = values.iter().all(|value| value.pure);
        let unary = |f: &dyn Fn(&Code) -> Code| values.first().map(f);
        let binary = |f: &dyn Fn(&Code, &Code) -> Code| match values.as_slice() {
            [a, b] => Some(f(a, b)),
            _ => None,
        };
        let infix = |symbol: &str| {
            binary(&|a, b| Code::complex(format!("{} {symbol} {}", a.wrapped(), b.wrapped()), pure))
        };
        let float = |symbol: &str| {
            binary(&|a, b| {
                Code::call(
                    format!("rt.f32({} {symbol} {})", a.wrapped(), b.wrapped()),
                    pure,
                )
            })
        };
        let runtime = |function: &str, may_throw: bool| {
            let texts: Vec<String> = values.iter().map(|value| value.text.clone()).collect();
            Code::call(
                format!("rt.{function}({})", texts.join(", ")),
                pure && !may_throw,
            )
        };
        let code = match op {
            // `Int` results above the runtime's bound throw Overflow, as in Rust and the evaluator.
            IntAdd => Some(runtime("intAdd", true)),
            IntSub => Some(runtime("intSub", true)),
            IntMul => Some(runtime("intMul", true)),
            DoubleAdd | StringConcat => infix("+"),
            DoubleSub => infix("-"),
            DoubleMul => infix("*"),
            DoubleDiv => infix("/"),
            IntNeg | DoubleNeg => unary(&|a| Code::complex(format!("-{}", a.wrapped()), pure)),
            IntDivChecked => Some(runtime("intDiv", false)),
            IntRemChecked => Some(runtime("intRem", false)),
            FloatAdd => float("+"),
            FloatSub => float("-"),
            FloatMul => float("*"),
            FloatDiv => float("/"),
            FloatNeg => unary(&|a| Code::call(format!("rt.f32(-{})", a.wrapped()), pure)),
            DecimalAdd => Some(runtime("decimalAdd", true)),
            DecimalSub => Some(runtime("decimalSub", true)),
            DecimalNeg => Some(runtime("decimalNeg", true)),
            // E8311 rejects `*` and `/` on decimals in client code before generation.
            DecimalMul | DecimalDivChecked => {
                return self.unsupported("decimal multiplication and division", out);
            }
            DurationAdd | DurationSub | DurationNeg => {
                return self.unsupported("duration arithmetic", out);
            }
            Not => unary(&|a| Code::complex(format!("!{}", a.wrapped()), pure)),
            Eq | NotEq | Lt | LtEq | Gt | GtEq => {
                return Some(self.comparison(op, &operand_type, values, out));
            }
            DisplayString | DisplayText => values.first().cloned(),
            DisplayInt | DisplayLong => Some(runtime("intToString", false)),
            DisplayDouble | DisplayFloat => Some(runtime("jsNumberToString", false)),
            DisplayDecimal => Some(runtime("displayDecimal", false)),
            DisplayBool => {
                unary(&|a| Code::complex(format!("{} ? \"true\" : \"false\"", a.wrapped()), pure))
            }
        };
        match code {
            Some(code) => Some(code),
            None => self.unsupported("a malformed primitive", out),
        }
    }

    /// The key comparator a `Set`/`Map` builtin takes: the canonical order of its element / key
    /// type (instance 0), or `""` for the builtins that need none.
    fn key_order(&mut self, name: &str, instance: &[Type]) -> String {
        match (name, instance.first()) {
            (
                "setOf" | "insert" | "remove" | "contains" | "put" | "lookup" | "removeKey",
                Some(key),
            ) => self.codecs.key_order(key),
            _ => String::new(),
        }
    }

    /// `==`, `!=`, `<`, … on operands of type `ty`: JS operators on BigInts, numbers, booleans
    /// and strings (equality only: strings are ordered by code point, `rt.compare`; `Date`,
    /// `DateTime` and `Instant` texts by `rt.compareDate` / `rt.compareDateTime` /
    /// `rt.compareInstant`, their canonical texts being unique so `===` is their equality),
    /// the runtime's structural `equals` / `compare` otherwise.
    fn comparison(
        &mut self,
        op: Prim,
        ty: &Type,
        values: Vec<Code>,
        out: &mut Vec<String>,
    ) -> Code {
        use Prim::*;
        let pure = values.iter().all(|value| value.pure);
        let scalar = match ty {
            Type::Con(name, args) if args.is_empty() => name.as_str(),
            _ => "",
        };
        let (a, b) = match values.as_slice() {
            [a, b] => (a.clone(), b.clone()),
            _ => return atom("false"),
        };
        if scalar == "Unit" {
            // `()` has a single value; the operands still run for their effects.
            for value in [a, b] {
                if !value.pure {
                    out.push(format!("{};", value.text));
                }
            }
            return atom(if matches!(op, Eq | LtEq | GtEq) {
                "true"
            } else {
                "false"
            });
        }
        let symbol = match op {
            Eq => "===",
            NotEq => "!==",
            Lt => "<",
            LtEq => "<=",
            Gt => ">",
            _ => ">=",
        };
        let numeric = matches!(scalar, "Int" | "Long" | "Double" | "Float" | "Port");
        let primitive = numeric
            || scalar == "Boolean"
            || matches!(
                scalar,
                "String" | "Email" | "Url" | "UUID" | "Date" | "DateTime" | "Instant"
            );
        let infix = |a: &Code, b: &Code, symbol: &str| {
            Code::complex(format!("{} {symbol} {}", a.wrapped(), b.wrapped()), pure)
        };
        if matches!(op, Eq | NotEq) {
            if primitive {
                return infix(&a, &b, symbol);
            }
            let negation = if op == NotEq { "!" } else { "" };
            return Code::complex(format!("{negation}rt.equals({}, {})", a.text, b.text), pure);
        }
        if numeric {
            return infix(&a, &b, symbol);
        }
        let function = match scalar {
            "Decimal" | "Money" => "decimalCompare",
            // Ordered as chrono orders the values, not by text (fractions, extended years).
            "Date" => "compareDate",
            "DateTime" => "compareDateTime",
            "Instant" => "compareInstant",
            _ => "compare",
        };
        Code::complex(
            format!("rt.{function}({}, {}) {symbol} 0", a.text, b.text),
            pure,
        )
    }
}

/// Whether `text` is a plain identifier or literal (safe to repeat).
fn is_simple(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

fn scalar_name(ty: &Type) -> Option<&str> {
    match ty {
        Type::Con(name, args) if args.is_empty() => Some(name),
        _ => None,
    }
}

/// A literal of type `ty` (an integer literal may be typed `Double` or `Float`). `Float`
/// literals are rounded to f32 here and written with their exact JS text.
fn literal_code(literal: &Literal, ty: &Type) -> Code {
    let number = |value: Option<f64>| match value {
        Some(value) => {
            let text = js_number_to_string(value);
            if text.starts_with('-') {
                Code::complex(text, true)
            } else {
                atom(text)
            }
        }
        None => invalid_literal(),
    };
    match literal {
        Literal::Int(digits) | Literal::Double(digits) if scalar_name(ty) == Some("Double") => {
            number(digits.parse::<f64>().ok())
        }
        Literal::Int(digits) | Literal::Float(digits) if scalar_name(ty) == Some("Float") => {
            number(digits.parse::<f32>().ok().map(f64::from))
        }
        Literal::Int(digits) => {
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return invalid_literal();
            }
            // A BigInt literal cannot have leading zeros.
            let trimmed = digits.trim_start_matches('0');
            atom(format!(
                "{}n",
                if trimmed.is_empty() { "0" } else { trimmed }
            ))
        }
        Literal::Str(text) => atom(js_string(text)),
        Literal::Bool(value) => atom(value.to_string()),
        Literal::Unit => atom("undefined"),
        Literal::Double(_) | Literal::Float(_) => invalid_literal(),
    }
}

fn invalid_literal() -> Code {
    Code::call(
        format!(
            "(() => {{ throw new Error({}); }})()",
            js_string("bindc: invalid number literal")
        ),
        false,
    )
}

/// A BigInt literal for an integer literal pattern as written: optional sign, then decimal,
/// `0x` or `0b` digits, with `_` separators and an optional `l` suffix (as `core::eval`).
fn int_pattern(text: &str) -> Option<String> {
    let cleaned = text.replace('_', "").to_ascii_lowercase();
    let (negative, rest) = match cleaned.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, cleaned.strip_prefix('+').unwrap_or(&cleaned)),
    };
    let rest = rest.trim_end_matches('l');
    let (prefix, digits, radix) = if let Some(hex) = rest.strip_prefix("0x") {
        ("0x", hex, 16)
    } else if let Some(bin) = rest.strip_prefix("0b") {
        ("0b", bin, 2)
    } else {
        ("", rest, 10)
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    let digits = if radix == 10 {
        let trimmed = digits.trim_start_matches('0');
        if trimmed.is_empty() { "0" } else { trimmed }
    } else {
        digits
    };
    let sign = if negative { "-" } else { "" };
    Some(format!("{sign}{prefix}{digits}n"))
}

/// The string a Rust `{:?}` string literal stands for (`"a\"b"` → `a"b`).
fn rust_debug_string(text: &str) -> Option<String> {
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    let mut output = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            output.push(c);
            continue;
        }
        match chars.next()? {
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            '0' => output.push('\0'),
            '\\' => output.push('\\'),
            '"' => output.push('"'),
            '\'' => output.push('\''),
            'u' => {
                if chars.next()? != '{' {
                    return None;
                }
                let mut hex = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    hex.push(c);
                }
                output.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(output)
}

/// A builtin applied to `values`; `order` is the key comparator of the `Set`/`Map` builtins
/// that take one (`Generator::key_order`). Collections are immutable frozen arrays, so every
/// builtin but `map`/`fold` is a pure call when its arguments are.
fn builtin_call(name: &str, order: &str, values: Vec<Code>) -> Code {
    let texts: Vec<String> = values.iter().map(|value| value.text.clone()).collect();
    let pure = values.iter().all(|value| value.pure);
    let runtime = |function: &str, with_order: bool| {
        let mut arguments = Vec::new();
        if with_order {
            arguments.push(order.to_string());
        }
        arguments.extend(texts.iter().cloned());
        Code::call(format!("rt.{function}({})", arguments.join(", ")), pure)
    };
    match (name, texts.as_slice()) {
        ("setOf", items) if items.len() > MAX_SPREAD => {
            Code::call(format!("rt.setFrom({order}, [{}])", items.join(", ")), pure)
        }
        ("setOf", _) => runtime("setOf", true),
        ("emptySet", []) => atom("rt.emptySet()"),
        ("insert", [_, _]) => runtime("setInsert", true),
        ("remove", [_, _]) => runtime("setRemove", true),
        ("contains", [_, _]) => runtime("setContains", true),
        ("setSize", [_]) => runtime("setSize", false),
        ("toList", [_]) => runtime("setToList", false),
        ("emptyMap", []) => atom("rt.emptyMap()"),
        ("put", [_, _, _]) => runtime("mapPut", true),
        ("lookup", [_, _]) => runtime("mapLookup", true),
        ("removeKey", [_, _]) => runtime("mapRemoveKey", true),
        ("keys", [_]) => runtime("mapKeys", false),
        ("values", [_]) => runtime("mapValues", false),
        ("mapSize", [_]) => runtime("mapSize", false),
        ("listOf", items) if items.len() > MAX_SPREAD => {
            Code::call(format!("rt.listFrom([{}])", items.join(", ")), pure)
        }
        ("listOf", items) => Code::call(format!("rt.listOf({})", items.join(", ")), pure),
        ("emptyList", []) => atom("rt.emptyList()"),
        ("length", [list]) => Code::call(format!("rt.length({list})"), pure),
        ("first", [list]) => Code::call(format!("rt.first({list})"), pure),
        ("get", [list, index]) => Code::call(format!("rt.get({list}, {index})"), pure),
        ("map" | "fold", items) => {
            Code::complex(format!("await rt.{name}(ctx, {})", items.join(", ")), false)
        }
        _ => Code::call(
            format!(
                "(() => {{ throw new Error({}); }})()",
                js_string(&format!("bindc: unknown builtin `{name}`"))
            ),
            false,
        ),
    }
}

/// A builtin used as a value: an async function that applies it (without entering a level).
fn builtin_value(name: &str, order: &str, ty: &Type) -> Code {
    let Type::Fun(params, _, _) = ty else {
        return builtin_call(name, order, Vec::new());
    };
    let names: Vec<String> = (0..params.len()).map(|index| format!("a{index}")).collect();
    let values: Vec<Code> = names.iter().map(atom).collect();
    let call = builtin_call(name, order, values);
    let call = call
        .text
        .strip_prefix("await ")
        .unwrap_or(&call.text)
        .to_string();
    let mut inputs = vec!["ctx".to_string()];
    inputs.extend(names);
    Code::complex(format!("async ({}) => {call}", inputs.join(", ")), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_bound_twice_is_an_internal_error() {
        let program =
            crate::core::lower::tests::compile("fun f(x: Int): Int {\n    val y = x\n    y\n}\n");
        let mut function = program.functions.first().expect("function").clone();
        // Rebind the parameter's id with a `Let`, as a faulty lowering could.
        let (param, _, ty) = function.params.first().cloned().expect("param");
        function.body = Expr {
            ty: ty.clone(),
            kind: ExprKind::Let {
                local: param,
                name: "x".into(),
                value: Box::new(Expr {
                    ty: ty.clone(),
                    kind: ExprKind::Local(param),
                }),
                body: Box::new(function.body.clone()),
            },
        };
        let text = super::function(&function, &program);
        assert!(
            text.contains(&format!(
                "throw new Error(\"bindc: internal error: local {param} is bound twice in function f\");"
            )),
            "{text}"
        );
        assert!(!text.contains("const "), "{text}");
        // `verify` never gets this far: the IR validator reports the second binding (E9001).
        let mut faulty = program.clone();
        faulty.functions[0] = function;
        let problems = crate::core::validate::validate(&faulty);
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("is bound twice")),
            "{problems:?}"
        );
    }

    #[test]
    fn temporal_values_are_ordered_by_the_typed_comparators() {
        for (ty, function) in [
            ("Date", "compareDate"),
            ("DateTime", "compareDateTime"),
            ("Instant", "compareInstant"),
        ] {
            let program = crate::core::lower::tests::compile(&format!(
                "fun f(a: {ty}, b: {ty}): List<Boolean> = listOf(a < b, a >= b, a == b)\n"
            ));
            let text = super::function(&program.functions[0], &program);
            assert!(text.contains(&format!("rt.{function}(")), "{text}");
            assert!(text.contains(" === "), "{text}");
            assert!(!text.contains("rt.compare("), "{text}");
        }
    }

    #[test]
    fn collection_builtins_take_the_key_order() {
        let program = crate::core::lower::tests::compile(
            "enum class Cor { Z, A, M }\nfun aplica(f: (Set<Cor>, Cor) -> Set<Cor>): Set<Cor> = f(setOf(M), Z)\nfun f(): Set<Cor> = aplica(insert)\nfun g(m: Map<Decimal, Int>, k: Decimal): Int? = lookup(put(m, k, mapSize(m)), k)\n",
        );
        let function = |name: &str| {
            let function = program
                .functions
                .iter()
                .find(|function| function.name == name)
                .expect("function");
            super::function(function, &program)
        };
        let f = function("f");
        assert!(
            f.contains("async (ctx, a0, a1) => rt.setInsert(ord0, a0, a1)"),
            "{f}"
        );
        assert!(
            f.contains("const ord0 = rt.variantOrder([\"Z\", \"A\", \"M\"]);"),
            "{f}"
        );
        let aplica = function("aplica");
        assert!(
            aplica.contains("rt.setOf(ord0, rt.variant(\"M\", []))"),
            "{aplica}"
        );
        let g = function("g");
        assert!(
            g.contains("rt.mapLookup(rt.decimalCompare, rt.mapPut(rt.decimalCompare, u_m_0, u_k_1, rt.mapSize(u_m_0)), u_k_1)"),
            "{g}"
        );
    }

    #[test]
    fn integer_patterns() {
        assert_eq!(int_pattern("-0").as_deref(), Some("-0n"));
        assert_eq!(int_pattern("007").as_deref(), Some("7n"));
        assert_eq!(int_pattern("0x1F").as_deref(), Some("0x1fn"));
        assert_eq!(int_pattern("-1_000L").as_deref(), Some("-1000n"));
        assert_eq!(int_pattern("0b101").as_deref(), Some("0b101n"));
        assert_eq!(int_pattern("0xZZ"), None);
        assert_eq!(int_pattern(""), None);
    }

    #[test]
    fn debug_strings() {
        let text = "a\"b\\c\nd\u{1}é\u{0}";
        assert_eq!(
            rust_debug_string(&format!("{text:?}")).as_deref(),
            Some(text)
        );
        assert_eq!(rust_debug_string("\"x"), None);
    }

    #[test]
    fn literals() {
        let ty = |name: &str| Type::con(name);
        let text = |literal: Literal, name: &str| literal_code(&literal, &ty(name)).text;
        assert_eq!(text(Literal::Int("007".into()), "Int"), "7n");
        assert_eq!(text(Literal::Int("0".into()), "Int"), "0n");
        assert_eq!(text(Literal::Int("3".into()), "Double"), "3");
        assert_eq!(
            text(Literal::Float("0.1".into()), "Float"),
            "0.10000000149011612"
        );
        assert_eq!(text(Literal::Double("1e21".into()), "Double"), "1e+21");
        assert_eq!(text(Literal::Double("0.1".into()), "Double"), "0.1");
        assert_eq!(text(Literal::Unit, "Unit"), "undefined");
    }

    #[test]
    fn functions_enter_and_return() {
        let program = crate::core::lower::tests::compile("fun f(x: Int) = x * x + 1\n");
        let function = program.functions.first().expect("function");
        assert_eq!(
            super::function(function, &program),
            "async function u_f(ctx, u_x_0) {\nctx = await rt.enter(ctx);\nreturn rt.intAdd(rt.intMul(u_x_0, u_x_0), 1n);\n}\n"
        );
    }

    #[test]
    fn effects_keep_their_order_before_statements() {
        // `g(x)` runs before the `if`'s statements, as in `core::eval`.
        let program = crate::core::lower::tests::compile(
            "fun g(x: Int) = x\nfun f(x: Int): Int = g(x) + (if (x == 0) { val y = g(1)\n y } else 2)\n",
        );
        let function = program
            .functions
            .iter()
            .find(|function| function.name == "f")
            .expect("f");
        let text = super::function(function, &program);
        let call = text.find("= await u_g(ctx, u_x_0);").expect(&text);
        let branch = text.find("if (u_x_0 === 0n)").expect(&text);
        assert!(call < branch, "{text}");
    }
}
