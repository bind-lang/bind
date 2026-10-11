//! A reference evaluator over the core IR: it pre-renders static pages at compile time
//! (`backend::js::page::prerender`) and is the reference of the tests. Its semantics are those of
//! the generated Rust and JS: arbitrary-precision `Int`, truncating division with the sign of the
//! remainder following the dividend, `None` for a zero divisor, capture-by-copy closures and a
//! call-depth limit of 10,000. Task 9 compares it with the generated program.
//!
//! `Double` is Rust's `f64` and `Float` Rust's `f32` (IEEE binary64/binary32, as JS numbers and
//! `Math.fround`); a UI tree is a `Value::Ui`, rendered as the page's HTML; `Display*`
//! primitives give the text `Text` shows (`core::display`). `UUID`, `Date`, `DateTime` and
//! `Instant` values (only ever arguments: Bind has no literal of these types) are their
//! canonical wire texts (`Value::Str`), shown as is and ordered by
//! `display::compare_temporal`. Anything else the generated code does not model in memory
//! (database, `Decimal`, durations, environment values, server calls) is
//! `EvalError::Unsupported`.
//!
//! Strings, list items, constructor fields and UI children are shared (`Arc`), so copying a
//! value (reading a local, capturing an environment) costs no more than a few pointers, and a
//! value nested to any depth is dropped iteratively ([`Shared`]).

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use super::bigint::BigInt;

use crate::types::exhaustive::Pat;
use crate::types::ty::Type;

use super::display::{self, UiArgView, UiView};
use super::ir::*;

/// Maximum number of nested function/closure calls; call number 10,001 fails.
pub const DEPTH_LIMIT: usize = 10_000;

/// Stack of a bounded evaluation's thread (the pre-render): a recursive interpreter needs far
/// more than a test thread has. [`MAX_NESTING`] keeps it from overflowing.
const STACK_SIZE: usize = 512 * 1024 * 1024;

/// Stack of an unbounded evaluation's thread (the tests): enough for the deepest programs they
/// run (10,000 nested calls whose bodies nest 20 UI nodes each, in a debug build). Reserved,
/// not committed: only the pages the evaluation touches take memory.
const UNBOUNDED_STACK_SIZE: usize = 8 * 1024 * 1024 * 1024;

/// The deepest nesting of expressions under evaluation (calls included) a bounded evaluation
/// allows: past it, the evaluation stops as past its budget (the page is rendered at run time)
/// instead of overflowing its thread's stack. Calls alone nest at most 10,000 deep; a deeper
/// nesting comes from large expressions (UI trees) in recursive functions.
const MAX_NESTING: usize = 60_000;

#[derive(Clone, Debug)]
pub enum Value {
    Int(BigInt),
    Double(f64),
    Float(f32),
    Bool(bool),
    Str(Arc<str>),
    Unit,
    None,
    Some(Box<Value>),
    /// A record, variant or distinct value. `name` is the record/variant/distinct type name.
    Ctor {
        name: String,
        fields: Shared,
    },
    List(Shared),
    /// Captures its environment by value.
    Closure {
        params: Vec<LocalId>,
        body: Arc<Expr>,
        env: BTreeMap<LocalId, Value>,
    },
    Function(String),
    Builtin(String),
    /// A UI node; `args` are `Str` or `Int` values and `children` are `Ui` values.
    Ui {
        node: UiNode,
        args: Vec<Value>,
        children: Shared,
    },
}

/// The items of a list, the fields of a constructor or the children of a UI node. Shared: a
/// copy of the value shares them (a UI subtree used twice is one subtree, see
/// [`Budget::html_bytes`]). Dropped iteratively: a value nested to any depth (a 200,000-level
/// UI tree) is freed without growing the call stack.
#[derive(Clone, Debug, Default)]
pub struct Shared(Arc<Vec<Value>>);

impl From<Vec<Value>> for Shared {
    fn from(items: Vec<Value>) -> Shared {
        Shared(Arc::new(items))
    }
}

impl std::ops::Deref for Shared {
    type Target = [Value];

    fn deref(&self) -> &[Value] {
        &self.0
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Only the last owner frees the items; their own shared parts are moved to `pending`
        // (when this was their last owner too) before they drop, so no drop recurses.
        let Some(items) = Arc::get_mut(&mut self.0) else {
            return;
        };
        let mut pending: Vec<Value> = std::mem::take(items);
        while let Some(mut value) = pending.pop() {
            match &mut value {
                Value::List(shared) | Value::Ctor { fields: shared, .. } => {
                    if let Some(items) = Arc::get_mut(&mut shared.0) {
                        pending.append(items);
                    }
                }
                Value::Ui { args, children, .. } => {
                    pending.append(args);
                    if let Some(items) = Arc::get_mut(&mut children.0) {
                        pending.append(items);
                    }
                }
                Value::Some(inner) => pending.push(std::mem::replace(&mut **inner, Value::Unit)),
                Value::Closure { env, .. } => pending.extend(std::mem::take(env).into_values()),
                _ => {}
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalError {
    Unsupported(String),
    RecursionLimit,
}

impl Value {
    /// A list of `items`.
    pub fn list(items: Vec<Value>) -> Value {
        Value::List(items.into())
    }

    /// The canonical form compared by the differential test:
    /// - `Int`: decimal digits (`-` prefix when negative);
    /// - `Double`: `display::js_number_to_string`; `Float`: the same on the value as `f64`;
    /// - `Str`: JSON string; escapes `"`, `\`, `\n`, `\r`, `\t`, and other control characters
    ///   below U+0020 as `\u00XX`; everything else (including non-ASCII) is written as is;
    /// - `Bool`: `true` / `false`; `None`: `null`; `Some(v)`: just `v`;
    /// - `Ctor`: `Nome` when it has no fields, otherwise `Nome(a, b)`;
    /// - `List`: `[a, b]`; `Unit`: `()`;
    /// - `Closure`, `Function`, `Builtin`: `<fun>` (not comparable);
    /// - `Ui`: its HTML, `display::render_html_limited` (unlimited).
    pub fn render(&self) -> String {
        match self {
            Value::Int(value) => value.to_string(),
            Value::Double(value) => display::js_number_to_string(*value),
            Value::Float(value) => display::js_number_to_string(f64::from(*value)),
            Value::Bool(value) => value.to_string(),
            Value::Str(text) => json_string(text),
            Value::Unit => "()".into(),
            Value::None => "null".into(),
            Value::Some(value) => value.render(),
            Value::Ctor { name, fields } if fields.is_empty() => name.clone(),
            Value::Ctor { name, fields } => format!("{name}({})", join(fields)),
            Value::List(items) => format!("[{}]", join(items)),
            Value::Closure { .. } | Value::Function(_) | Value::Builtin(_) => "<fun>".into(),
            Value::Ui { .. } => display::render_html_limited(self, usize::MAX).unwrap_or_default(),
        }
    }

    /// The HTML of a UI value when it has at most `limit` bytes (and nodes), else `None`.
    pub fn render_html_within(&self, limit: usize) -> Option<String> {
        match self {
            Value::Ui { .. } => display::render_html_limited(self, limit),
            _ => None,
        }
    }

    /// The kind of value, for error messages (never the value itself, which may be huge).
    fn kind(&self) -> &'static str {
        match self {
            Value::Int(_) => "an Int",
            Value::Double(_) => "a Double",
            Value::Float(_) => "a Float",
            Value::Bool(_) => "a Boolean",
            Value::Str(_) => "a String",
            Value::Unit => "()",
            Value::None => "null",
            Value::Some(_) => "an optional value",
            Value::Ctor { .. } => "a constructor",
            Value::List(_) => "a list",
            Value::Closure { .. } | Value::Function(_) | Value::Builtin(_) => "a function",
            Value::Ui { .. } => "a UI node",
        }
    }
}

/// Evaluation only builds well-formed UI nodes (`Str` or `Int` arguments, `Ui` children); any
/// other value renders as nothing.
impl UiView for Value {
    fn ui_node(&self) -> UiNode {
        match self {
            Value::Ui { node, .. } => *node,
            _ => UiNode::Empty,
        }
    }

    fn ui_arg(&self, index: usize) -> Option<UiArgView<'_>> {
        let Value::Ui { args, .. } = self else {
            return None;
        };
        match args.get(index)? {
            Value::Str(text) => Some(UiArgView::Str(text)),
            Value::Int(value) => Some(UiArgView::Int(Cow::Owned(value.to_string()))),
            _ => None,
        }
    }

    fn ui_children(&self) -> &[Value] {
        match self {
            Value::Ui { children, .. } => children,
            _ => &[],
        }
    }
}

fn join(values: &[Value]) -> String {
    values
        .iter()
        .map(Value::render)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The canonical string escaping. The generated Rust renderer of Task 9 must replicate it
/// exactly; it differs from `serde_json` for U+0008 and U+000C (here `\u0008` and `\u000c`,
/// not `\b` and `\f`).
fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Evaluates the function `name` on `args` in a thread with a large stack, without a budget
/// (the tests' reference; the pre-render uses [`eval_function_within`]).
#[cfg(test)]
pub fn eval_function(
    program: &CoreProgram,
    name: &str,
    args: Vec<Value>,
) -> Result<Value, EvalError> {
    run(program, name, args, None)
}

/// A bound on one evaluation (the compile-time pre-render). `work` counts down one unit per
/// evaluated expression plus the size of what an operation goes through: the limbs of `Int`
/// arithmetic (their product for `*`, `/`, `%` and for the decimal text), a 64th of the bytes of
/// a concatenation, the locals a closure captures, the children a UI node gets and every pair of
/// values `==` / `!=` visits. Every other operation takes constant time (values are shared), so
/// the whole evaluation does at most a constant times `work` steps. `Int`s have at most
/// `int_limbs` 32-bit limbs and strings at most `string_bytes` bytes. Past any of them the
/// evaluation stops with `EvalError::Unsupported("evaluation budget exhausted")`.
/// `html_bytes` bounds the rendering of the result (`Value::render_html_within`): output bytes
/// and visited nodes, since shared subtrees make a tree's HTML exponential in its memory.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub work: u64,
    pub int_limbs: usize,
    pub string_bytes: usize,
    pub html_bytes: usize,
}

/// The budget of the static pre-render: a page that needs more is rendered at run time.
pub const PRERENDER_BUDGET: Budget = Budget {
    work: 2_000_000,
    int_limbs: 1024,
    string_bytes: 4 * 1024 * 1024,
    html_bytes: 1024 * 1024,
};

/// [`eval_function`] within `budget`.
pub fn eval_function_within(
    program: &CoreProgram,
    name: &str,
    args: Vec<Value>,
    budget: Budget,
) -> Result<Value, EvalError> {
    run(program, name, args, Some(budget))
}

fn run(
    program: &CoreProgram,
    name: &str,
    args: Vec<Value>,
    budget: Option<Budget>,
) -> Result<Value, EvalError> {
    std::thread::scope(|scope| {
        let stack = if budget.is_some() {
            STACK_SIZE
        } else {
            UNBOUNDED_STACK_SIZE
        };
        let handle = std::thread::Builder::new()
            .stack_size(stack)
            .spawn_scoped(scope, || {
                let mut interp = Interp {
                    program,
                    depth: 0,
                    nesting: 0,
                    budget,
                };
                interp.call(Value::Function(name.to_string()), args)
            });
        match handle {
            Ok(handle) => handle
                .join()
                .unwrap_or_else(|payload| std::panic::resume_unwind(payload)),
            Err(error) => Err(unsupported(&format!("cannot spawn thread: {error}"))),
        }
    })
}

fn unsupported(what: &str) -> EvalError {
    EvalError::Unsupported(what.to_string())
}

/// Non-local control flow inside the interpreter.
enum Signal {
    Error(EvalError),
    Return(Value),
}

impl From<EvalError> for Signal {
    fn from(error: EvalError) -> Self {
        Signal::Error(error)
    }
}

type Env = BTreeMap<LocalId, Value>;
type Eval = Result<Value, Signal>;

struct Interp<'p> {
    program: &'p CoreProgram,
    depth: usize,
    /// Expressions under evaluation (see [`MAX_NESTING`]).
    nesting: usize,
    /// The remaining budget, if bounded (`work` counts down).
    budget: Option<Budget>,
}

fn exhausted() -> EvalError {
    unsupported("evaluation budget exhausted")
}

/// The value of local `id`.
fn local(env: &Env, id: LocalId) -> Eval {
    match env.get(&id) {
        Some(value) => Ok(value.clone()),
        None => Err(unbound(id).into()),
    }
}

#[cold]
fn unbound(id: LocalId) -> EvalError {
    unsupported(&format!("unbound local {id}"))
}

#[cold]
fn not_modelled(what: &str, detail: &str) -> EvalError {
    unsupported(&format!("{what} {detail}"))
}

/// The limbs of an `Int` value (1 for anything else).
fn limbs(value: &Value) -> u64 {
    match value {
        Value::Int(value) => value.limb_count().max(1) as u64,
        _ => 1,
    }
}

/// The work of `op` on `args` beyond the expression itself (see [`Budget`]).
fn prim_cost(op: Prim, args: &[Value]) -> u64 {
    let (a, b) = (args.first(), args.get(1));
    let size = |value: Option<&Value>| value.map_or(1, limbs);
    match op {
        Prim::IntAdd | Prim::IntSub | Prim::IntNeg => size(a).max(size(b)) / 8,
        Prim::IntMul | Prim::IntDivChecked | Prim::IntRemChecked => size(a) * size(b) / 8,
        Prim::DisplayInt | Prim::DisplayLong => size(a) * size(a) / 8,
        Prim::StringConcat => {
            let bytes = |value: Option<&Value>| match value {
                Some(Value::Str(text)) => text.len() as u64,
                _ => 0,
            };
            (bytes(a) + bytes(b)) / 64
        }
        _ => 0,
    }
}

impl Interp<'_> {
    /// Takes `units` of work from the budget, if bounded.
    fn charge(&mut self, units: u64) -> Result<(), EvalError> {
        if let Some(budget) = &mut self.budget {
            if budget.work < units {
                budget.work = 0;
                return Err(exhausted());
            }
            budget.work -= units;
        }
        Ok(())
    }

    /// Calls a function value; a `Return` inside ends only this call.
    fn call(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, EvalError> {
        // Like the generated `ctx.enter()?`: only Bind functions and closures count.
        let counts = matches!(callee, Value::Function(_) | Value::Closure { .. });
        if !counts {
            return match self.call_inner(callee, args) {
                Ok(value) | Err(Signal::Return(value)) => Ok(value),
                Err(Signal::Error(error)) => Err(error),
            };
        }
        if self.depth >= DEPTH_LIMIT {
            return Err(EvalError::RecursionLimit);
        }
        self.depth += 1;
        let outcome = self.call_inner(callee, args);
        self.depth -= 1;
        match outcome {
            Ok(value) | Err(Signal::Return(value)) => Ok(value),
            Err(Signal::Error(error)) => Err(error),
        }
    }

    fn call_inner(&mut self, callee: Value, args: Vec<Value>) -> Eval {
        match callee {
            Value::Function(name) => {
                let function = self
                    .program
                    .functions
                    .iter()
                    .find(|function| function.name == name)
                    .ok_or_else(|| unsupported(&format!("no function `{name}`")))?;
                let mut env = bind_params(function.params.iter().map(|(id, _, _)| *id), args)?;
                self.eval(&function.body, &mut env)
            }
            Value::Closure {
                params,
                body,
                mut env,
            } => {
                for (id, value) in bind_params(params.into_iter(), args)? {
                    env.insert(id, value);
                }
                self.eval(&body, &mut env)
            }
            Value::Builtin(name) => self.builtin(&name, args),
            other => Err(unsupported(&format!("call of {}", other.kind())).into()),
        }
    }

    fn eval(&mut self, expr: &Expr, env: &mut Env) -> Eval {
        self.charge(1)?;
        if self.budget.is_some() && self.nesting >= MAX_NESTING {
            return Err(exhausted().into());
        }
        self.nesting += 1;
        let value = self.eval_nested(expr, env);
        self.nesting -= 1;
        value
    }

    fn eval_nested(&mut self, expr: &Expr, env: &mut Env) -> Eval {
        match &expr.kind {
            ExprKind::Lit(literal) => literal_value(literal, &expr.ty).map_err(Signal::Error),
            ExprKind::Local(id) => local(env, *id),
            ExprKind::Global { name, .. } => Ok(Value::Function(name.clone())),
            ExprKind::Builtin { name, .. } => Ok(Value::Builtin(name.clone())),
            ExprKind::Environment(name) => Err(not_modelled("environment value", name).into()),
            ExprKind::Ctor { ctor, args, .. } => self.ctor_expr(ctor, args, env),
            ExprKind::None => Ok(Value::None),
            ExprKind::Some(inner) => self.some_expr(inner, env),
            ExprKind::Call { callee, args } => self.call_expr(callee, args, env),
            ExprKind::ServerCall { function, .. } => {
                Err(not_modelled("server call", function).into())
            }
            ExprKind::Lambda { params, body } => self.lambda(params, body, env),
            ExprKind::Let {
                local, value, body, ..
            } => self.let_expr(*local, value, body, env),
            ExprKind::Seq { value, body } => self.seq_expr(value, body, env),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => self.if_expr(condition, then_branch, else_branch, env),
            ExprKind::Match { scrutinee, arms } => self.match_expr(scrutinee, arms, env),
            ExprKind::Field { target, name } => self.field_expr(target, name, env),
            ExprKind::Prim { op, args } => self.prim_expr(*op, args, env),
            ExprKind::Db { .. } => Err(unsupported("database access").into()),
            ExprKind::Return(value) => self.return_expr(value, env),
            ExprKind::Ui {
                node,
                args,
                children,
            } => self.ui_expr(*node, args, children, env),
        }
    }

    #[inline(never)]
    fn ctor_expr(&mut self, ctor: &str, args: &[Expr], env: &mut Env) -> Eval {
        Ok(Value::Ctor {
            name: ctor.to_string(),
            fields: self.eval_all(args, env)?.into(),
        })
    }

    #[inline(never)]
    fn some_expr(&mut self, inner: &Expr, env: &mut Env) -> Eval {
        Ok(Value::Some(Box::new(self.eval(inner, env)?)))
    }

    #[inline(never)]
    fn call_expr(&mut self, callee: &Expr, args: &[Expr], env: &mut Env) -> Eval {
        let callee = self.eval(callee, env)?;
        let args = self.eval_all(args, env)?;
        Ok(self.call(callee, args)?)
    }

    #[inline(never)]
    fn let_expr(&mut self, local: LocalId, value: &Expr, body: &Expr, env: &mut Env) -> Eval {
        let value = self.eval(value, env)?;
        env.insert(local, value);
        self.eval(body, env)
    }

    #[inline(never)]
    fn seq_expr(&mut self, value: &Expr, body: &Expr, env: &mut Env) -> Eval {
        self.eval(value, env)?;
        self.eval(body, env)
    }

    #[inline(never)]
    fn if_expr(
        &mut self,
        condition: &Expr,
        then_branch: &Expr,
        else_branch: &Expr,
        env: &mut Env,
    ) -> Eval {
        match self.eval(condition, env)? {
            Value::Bool(true) => self.eval(then_branch, env),
            Value::Bool(false) => self.eval(else_branch, env),
            other => Err(not_modelled("condition that is", other.kind()).into()),
        }
    }

    #[inline(never)]
    fn field_expr(&mut self, target: &Expr, name: &str, env: &mut Env) -> Eval {
        let target = self.eval(target, env)?;
        Ok(self.field(&target, name)?)
    }

    #[inline(never)]
    fn return_expr(&mut self, value: &Expr, env: &mut Env) -> Eval {
        Err(Signal::Return(self.eval(value, env)?))
    }

    // The arms below are separate functions so that `eval_nested`, which recurses once per
    // nested expression, keeps a small stack frame (debug builds give a function one slot per
    // local of every arm).

    #[inline(never)]
    fn lambda(&mut self, params: &[(LocalId, String, Type)], body: &Expr, env: &mut Env) -> Eval {
        self.charge(env.len() as u64)?;
        Ok(Value::Closure {
            params: params.iter().map(|(id, _, _)| *id).collect(),
            body: Arc::new(body.clone()),
            env: env.clone(),
        })
    }

    #[inline(never)]
    fn match_expr(&mut self, scrutinee: &Expr, arms: &[Arm], env: &mut Env) -> Eval {
        let value = self.eval(scrutinee, env)?;
        for arm in arms {
            if matches(&arm.pattern, &value) {
                for ((id, _, _), path) in arm.bindings.iter().zip(&arm.binding_paths) {
                    env.insert(*id, follow(&value, path)?);
                }
                return self.eval(&arm.body, env);
            }
        }
        Err(unsupported("no `when` arm matches").into())
    }

    #[inline(never)]
    fn prim_expr(&mut self, op: Prim, args: &[Expr], env: &mut Env) -> Eval {
        let temporal = args
            .first()
            .and_then(|arg| scalar(&arg.ty))
            .is_some_and(|name| matches!(name, "Date" | "DateTime" | "Instant"));
        let args = self.eval_all(args, env)?;
        self.charge(prim_cost(op, &args))?;
        let value = match op {
            _ if temporal => temporal_prim(op, args)?,
            Prim::Eq | Prim::NotEq => {
                let [a, b] = <[Value; 2]>::try_from(args)
                    .map_err(|_| unsupported(&format!("bad operands for {op:?}")))?;
                let equal = self.equal(&a, &b)?;
                Value::Bool(equal == (op == Prim::Eq))
            }
            _ => prim(op, args)?,
        };
        if let Some(budget) = &self.budget {
            let too_big = match &value {
                Value::Int(value) => value.limb_count() > budget.int_limbs,
                Value::Str(text) => text.len() > budget.string_bytes,
                _ => false,
            };
            if too_big {
                return Err(exhausted().into());
            }
        }
        Ok(value)
    }

    #[inline(never)]
    fn ui_expr(&mut self, node: UiNode, args: &[Expr], children: &Expr, env: &mut Env) -> Eval {
        let args = self.eval_all(args, env)?;
        if let Some(bad) = args
            .iter()
            .find(|arg| !matches!(arg, Value::Str(_) | Value::Int(_)))
        {
            return Err(not_modelled("UI argument that is", bad.kind()).into());
        }
        let children = match self.eval(children, env)? {
            Value::List(items) => items,
            other => return Err(not_modelled("UI children in", other.kind()).into()),
        };
        self.charge(children.len() as u64)?;
        if let Some(bad) = children
            .iter()
            .find(|child| !matches!(child, Value::Ui { .. }))
        {
            return Err(not_modelled("UI child that is", bad.kind()).into());
        }
        Ok(Value::Ui {
            node,
            args,
            children,
        })
    }

    /// A plain loop (iterator adapters would add stack frames to every nesting level).
    fn eval_all(&mut self, exprs: &[Expr], env: &mut Env) -> Result<Vec<Value>, Signal> {
        let mut values = Vec::with_capacity(exprs.len());
        for expr in exprs {
            values.push(self.eval(expr, env)?);
        }
        Ok(values)
    }

    fn field(&self, target: &Value, name: &str) -> Result<Value, EvalError> {
        let Value::Ctor { name: ctor, fields } = target else {
            return Err(unsupported(&format!("field `{name}` of {}", target.kind())));
        };
        let names: Option<Vec<&String>> = self.program.types.iter().find_map(|ty| match ty {
            CoreType::Record {
                name: record,
                fields,
                ..
            } if record == ctor => Some(fields.iter().map(|(field, _)| field).collect()),
            CoreType::Sum { variants, .. } => variants
                .iter()
                .find(|(variant, _)| variant == ctor)
                .map(|(_, fields)| fields.iter().map(|(field, _)| field).collect()),
            _ => None,
        });
        names
            .and_then(|names| names.iter().position(|field| *field == name))
            .and_then(|index| fields.get(index))
            .cloned()
            .ok_or_else(|| unsupported(&format!("no field `{name}` in `{ctor}`")))
    }

    /// Structural equality, iteratively (any depth), one unit of work per visited pair. IEEE
    /// equality for numbers: `NaN != NaN`, `-0 == 0` (as the generated Rust and JS).
    fn equal(&mut self, a: &Value, b: &Value) -> Result<bool, EvalError> {
        let mut pending = vec![(a, b)];
        while let Some(pair) = pending.pop() {
            self.charge(1)?;
            match pair {
                (Value::Int(a), Value::Int(b)) if a != b => return Ok(false),
                (Value::Double(a), Value::Double(b)) if a != b => return Ok(false),
                (Value::Float(a), Value::Float(b)) if a != b => return Ok(false),
                (Value::Bool(a), Value::Bool(b)) if a != b => return Ok(false),
                (Value::Str(a), Value::Str(b)) if a != b => return Ok(false),
                (Value::Int(_), Value::Int(_))
                | (Value::Double(_), Value::Double(_))
                | (Value::Float(_), Value::Float(_))
                | (Value::Bool(_), Value::Bool(_))
                | (Value::Str(_), Value::Str(_))
                | (Value::Unit, Value::Unit)
                | (Value::None, Value::None) => {}
                (Value::None, Value::Some(_)) | (Value::Some(_), Value::None) => return Ok(false),
                (Value::Some(a), Value::Some(b)) => pending.push((a, b)),
                (
                    Value::Ctor {
                        name: na,
                        fields: fa,
                    },
                    Value::Ctor {
                        name: nb,
                        fields: fb,
                    },
                ) => {
                    if na != nb || fa.len() != fb.len() {
                        return Ok(false);
                    }
                    pending.extend(fa.iter().zip(fb.iter()).rev());
                }
                (Value::List(a), Value::List(b)) => {
                    if a.len() != b.len() {
                        return Ok(false);
                    }
                    pending.extend(a.iter().zip(b.iter()).rev());
                }
                _ => return Err(unsupported("equality on incomparable values")),
            }
        }
        Ok(true)
    }

    fn builtin(&mut self, name: &str, args: Vec<Value>) -> Eval {
        let bad = || unsupported(&format!("bad arguments for builtin `{name}`"));
        let mut args = args.into_iter();
        let value = match name {
            "listOf" => Value::list(args.collect()),
            "emptyList" => Value::list(Vec::new()),
            "length" => match args.next() {
                Some(Value::List(items)) => Value::Int(BigInt::from(items.len())),
                _ => return Err(bad().into()),
            },
            "first" => match args.next() {
                Some(Value::List(items)) => option(items.first().cloned()),
                _ => return Err(bad().into()),
            },
            "get" => match (args.next(), args.next()) {
                (Some(Value::List(items)), Some(Value::Int(index))) => {
                    let item = usize::try_from(&index)
                        .ok()
                        .and_then(|index| items.get(index).cloned());
                    option(item)
                }
                _ => return Err(bad().into()),
            },
            "map" => match (args.next(), args.next()) {
                (Some(Value::List(items)), Some(function)) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items.iter() {
                        out.push(self.call(function.clone(), vec![item.clone()])?);
                    }
                    Value::list(out)
                }
                _ => return Err(bad().into()),
            },
            "fold" => match (args.next(), args.next(), args.next()) {
                (Some(Value::List(items)), Some(mut acc), Some(function)) => {
                    for item in items.iter() {
                        acc = self.call(function.clone(), vec![acc, item.clone()])?;
                    }
                    acc
                }
                _ => return Err(bad().into()),
            },
            other => return Err(unsupported(&format!("builtin `{other}`")).into()),
        };
        Ok(value)
    }
}

fn option(value: Option<Value>) -> Value {
    value.map_or(Value::None, |value| Value::Some(Box::new(value)))
}

fn bind_params(
    ids: impl ExactSizeIterator<Item = LocalId>,
    args: Vec<Value>,
) -> Result<Env, EvalError> {
    if ids.len() != args.len() {
        return Err(unsupported(&format!(
            "arity mismatch: {} parameters, {} arguments",
            ids.len(),
            args.len()
        )));
    }
    Ok(ids.zip(args).collect())
}

/// The value of a literal of type `ty` (an integer literal may be typed `Double` or `Float`).
fn literal_value(literal: &Literal, ty: &Type) -> Result<Value, EvalError> {
    let invalid = |text: &str| unsupported(&format!("invalid number literal `{text}`"));
    match literal {
        Literal::Int(digits) | Literal::Double(digits) if scalar(ty) == Some("Double") => digits
            .parse()
            .map(Value::Double)
            .map_err(|_| invalid(digits)),
        Literal::Int(digits) | Literal::Float(digits) if scalar(ty) == Some("Float") => digits
            .parse()
            .map(Value::Float)
            .map_err(|_| invalid(digits)),
        Literal::Int(digits) => BigInt::parse_bytes(digits.as_bytes(), 10)
            .map(Value::Int)
            .ok_or_else(|| unsupported(&format!("invalid integer literal `{digits}`"))),
        Literal::Str(text) => Ok(Value::Str(text.as_str().into())),
        Literal::Bool(value) => Ok(Value::Bool(*value)),
        Literal::Unit => Ok(Value::Unit),
        Literal::Double(text) | Literal::Float(text) => Err(invalid(text)),
    }
}

/// The name of a type constructor without arguments (`Double`, `Float`, ...).
fn scalar(ty: &Type) -> Option<&str> {
    match ty {
        Type::Con(name, args) if args.is_empty() => Some(name),
        _ => None,
    }
}

/// Whether `value` matches `pattern`. `null`/`Some` patterns address the `Option` layer when
/// the value is one; a user constructor named `Some` is a `Value::Ctor`.
fn matches(pattern: &Pat, value: &Value) -> bool {
    match (pattern, value) {
        (Pat::Wild, _) => true,
        (Pat::Lit(text), Value::Int(value)) => int_pattern(text).as_ref() == Some(value),
        (Pat::Lit(text), Value::Str(value)) => *text == format!("{value:?}"),
        (Pat::Ctor(name, _), Value::Bool(value)) => *name == value.to_string(),
        (Pat::Ctor(name, _), Value::None) => name == "null",
        (Pat::Ctor(name, args), Value::Some(inner)) => {
            name == "Some" && args.len() == 1 && matches(&args[0], inner)
        }
        (Pat::Ctor(name, args), Value::Ctor { name: ctor, fields }) => {
            name == ctor
                && args.len() == fields.len()
                && args
                    .iter()
                    .zip(fields.iter())
                    .all(|(arg, field)| matches(arg, field))
        }
        _ => false,
    }
}

/// The value of an integer literal pattern: optional sign, then decimal, `0x` or `0b` digits
/// (underscores and a trailing `l` are accepted as in the lexer).
fn int_pattern(text: &str) -> Option<BigInt> {
    let cleaned = text.replace('_', "").to_ascii_lowercase();
    let (negative, rest) = match cleaned.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, cleaned.strip_prefix('+').unwrap_or(&cleaned)),
    };
    let (digits, radix) = if let Some(hex) = rest.strip_prefix("0x") {
        (hex, 16)
    } else if let Some(bin) = rest.strip_prefix("0b") {
        (bin, 2)
    } else {
        (rest, 10)
    };
    let value = BigInt::parse_bytes(digits.trim_end_matches('l').as_bytes(), radix)?;
    Some(if negative { -value } else { value })
}

fn follow(value: &Value, path: &[(String, usize)]) -> Result<Value, EvalError> {
    let mut current = value;
    for (ctor, index) in path {
        current = match current {
            Value::Some(inner) if ctor == "Some" && *index == 0 => inner,
            Value::Ctor { name, fields } if name == ctor => fields
                .get(*index)
                .ok_or_else(|| unsupported("pattern path out of range"))?,
            _ => return Err(unsupported("pattern path does not fit the value")),
        };
    }
    Ok(current.clone())
}

fn prim(op: Prim, args: Vec<Value>) -> Result<Value, EvalError> {
    let bad = || unsupported(&format!("bad operands for {op:?}"));
    let mut args = args.into_iter();
    let (a, b) = (args.next(), args.next());
    let zero = BigInt::from(0);
    Ok(match (op, a, b) {
        (Prim::IntAdd, Some(Value::Int(a)), Some(Value::Int(b))) => Value::Int(a + b),
        (Prim::IntSub, Some(Value::Int(a)), Some(Value::Int(b))) => Value::Int(a - b),
        (Prim::IntMul, Some(Value::Int(a)), Some(Value::Int(b))) => Value::Int(a * b),
        (Prim::IntNeg, Some(Value::Int(a)), None) => Value::Int(-a),
        // `BigInt` division truncates toward zero and the remainder follows the dividend.
        (Prim::IntDivChecked, Some(Value::Int(a)), Some(Value::Int(b))) => {
            option((b != zero).then(|| Value::Int(a / b)))
        }
        (Prim::IntRemChecked, Some(Value::Int(a)), Some(Value::Int(b))) => {
            option((b != zero).then(|| Value::Int(a % b)))
        }
        (Prim::DoubleAdd, Some(Value::Double(a)), Some(Value::Double(b))) => Value::Double(a + b),
        (Prim::DoubleSub, Some(Value::Double(a)), Some(Value::Double(b))) => Value::Double(a - b),
        (Prim::DoubleMul, Some(Value::Double(a)), Some(Value::Double(b))) => Value::Double(a * b),
        (Prim::DoubleDiv, Some(Value::Double(a)), Some(Value::Double(b))) => Value::Double(a / b),
        (Prim::DoubleNeg, Some(Value::Double(a)), None) => Value::Double(-a),
        (Prim::FloatAdd, Some(Value::Float(a)), Some(Value::Float(b))) => Value::Float(a + b),
        (Prim::FloatSub, Some(Value::Float(a)), Some(Value::Float(b))) => Value::Float(a - b),
        (Prim::FloatMul, Some(Value::Float(a)), Some(Value::Float(b))) => Value::Float(a * b),
        (Prim::FloatDiv, Some(Value::Float(a)), Some(Value::Float(b))) => Value::Float(a / b),
        (Prim::FloatNeg, Some(Value::Float(a)), None) => Value::Float(-a),
        (Prim::DisplayString, Some(Value::Str(a)), None) => Value::Str(a),
        // `Long` values are `Int`s in the evaluator.
        (Prim::DisplayInt | Prim::DisplayLong, Some(Value::Int(a)), None) => {
            Value::Str(a.to_string().into())
        }
        (Prim::DisplayDouble, Some(a @ Value::Double(_)), None)
        | (Prim::DisplayFloat, Some(a @ Value::Float(_)), None) => Value::Str(a.render().into()),
        (Prim::DisplayBool, Some(Value::Bool(a)), None) => Value::Str(a.to_string().into()),
        // `UUID`, `Date`, `DateTime`, `Instant`: their canonical text, shown as is (the
        // evaluator has no `Duration` values).
        (Prim::DisplayText, Some(Value::Str(a)), None) => Value::Str(a),
        (Prim::StringConcat, Some(Value::Str(a)), Some(Value::Str(b))) => {
            Value::Str(format!("{a}{b}").into())
        }
        (Prim::Not, Some(Value::Bool(a)), None) => Value::Bool(!a),
        // An unordered pair (a NaN operand) makes every comparison false, as in IEEE and JS.
        (Prim::Lt, Some(a), Some(b)) => Value::Bool(compare(&a, &b)? == Some(Ordering::Less)),
        (Prim::LtEq, Some(a), Some(b)) => Value::Bool(matches!(
            compare(&a, &b)?,
            Some(Ordering::Less | Ordering::Equal)
        )),
        (Prim::Gt, Some(a), Some(b)) => Value::Bool(compare(&a, &b)? == Some(Ordering::Greater)),
        (Prim::GtEq, Some(a), Some(b)) => Value::Bool(matches!(
            compare(&a, &b)?,
            Some(Ordering::Greater | Ordering::Equal)
        )),
        (
            Prim::DecimalAdd
            | Prim::DecimalSub
            | Prim::DecimalMul
            | Prim::DecimalDivChecked
            | Prim::DecimalNeg
            | Prim::DurationAdd
            | Prim::DurationSub
            | Prim::DurationNeg
            | Prim::DisplayDecimal
            | Prim::DisplayText,
            _,
            _,
        ) => return Err(unsupported(&format!("{op:?}"))),
        _ => return Err(bad()),
    })
}

/// A comparison of `Date`, `DateTime` or `Instant` values, which the evaluator holds as their
/// canonical texts (`Value::Str`, given as arguments: Bind has no literal of these types):
/// equality is that of the texts (canonical, so unique), order is `display::compare_temporal`
/// (the order of the values, as chrono's in the generated Rust and the JS runtime's), never
/// the text order.
fn temporal_prim(op: Prim, args: Vec<Value>) -> Result<Value, EvalError> {
    let (a, b) = match args.as_slice() {
        [Value::Str(a), Value::Str(b)] => (a, b),
        _ => return prim(op, args),
    };
    let order = display::compare_temporal(a, b);
    Ok(Value::Bool(match op {
        Prim::Eq => a == b,
        Prim::NotEq => a != b,
        Prim::Lt => order == Ordering::Less,
        Prim::LtEq => order != Ordering::Greater,
        Prim::Gt => order == Ordering::Greater,
        Prim::GtEq => order != Ordering::Less,
        _ => return prim(op, args),
    }))
}

/// The order of two values; `None` when they are unordered (a `NaN` operand).
fn compare(a: &Value, b: &Value) -> Result<Option<Ordering>, EvalError> {
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => Ok(Some(a.cmp(b))),
        (Value::Double(a), Value::Double(b)) => Ok(a.partial_cmp(b)),
        (Value::Float(a), Value::Float(b)) => Ok(a.partial_cmp(b)),
        (Value::Str(a), Value::Str(b)) => Ok(Some(a.cmp(b))),
        (Value::Bool(a), Value::Bool(b)) => Ok(Some(a.cmp(b))),
        _ => Err(unsupported("ordering on unsupported values")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::display;
    use crate::core::lower::tests::{CLIENTES, ENTIDADE, compile, compile_with, schema};

    fn int(text: &str) -> Value {
        Value::Int(BigInt::parse_bytes(text.as_bytes(), 10).expect("digits"))
    }

    fn run(source: &str, name: &str, args: Vec<Value>) -> Result<Value, EvalError> {
        eval_function(&compile(source), name, args)
    }

    fn rendered(source: &str, args: Vec<Value>) -> String {
        run(source, "f", args).expect("evaluates").render()
    }

    fn ctor(name: &str, fields: Vec<Value>) -> Value {
        Value::Ctor {
            name: name.into(),
            fields: fields.into(),
        }
    }

    #[test]
    fn integers_are_arbitrary_precision() {
        assert_eq!(
            rendered(
                "fun f(x: Int) = x * x + 1",
                vec![int("12345678901234567890")]
            ),
            "152415787532388367501905199875019052101"
        );
    }

    #[test]
    fn division_truncates_and_zero_divisor_is_none() {
        let div = "fun f(a: Int, b: Int) = (a / b) ?: -1";
        assert_eq!(rendered(div, vec![int("7"), int("2")]), "3");
        assert_eq!(rendered(div, vec![int("-7"), int("2")]), "-3");
        assert_eq!(rendered(div, vec![int("7"), int("0")]), "-1");
        let rem = "fun f(a: Int, b: Int) = (a % b) ?: 99";
        assert_eq!(rendered(rem, vec![int("-7"), int("2")]), "-1");
        assert_eq!(rendered(rem, vec![int("7"), int("0")]), "99");
    }

    #[test]
    fn sum_types_and_when() {
        let source = "sealed class Arvore {\n    data class No(val e: Arvore, val v: Int, val d: Arvore)\n    data object Folha\n}\nfun soma(a: Arvore): Int = when (a) {\n    Folha -> 0\n    No(e, v, d) -> soma(e) + v + soma(d)\n}\nfun f(a: Arvore) = soma(a)\n";
        let folha = || ctor("Folha", vec![]);
        let no = |e, v, d| ctor("No", vec![e, int(v), d]);
        let tree = no(no(folha(), "1", folha()), "2", folha());
        assert_eq!(rendered(source, vec![tree]), "3");
    }

    #[test]
    fn closures_and_builtins() {
        let source = "fun f(xs: List<Int>) = fold(map(xs, { x -> x * 2 }), 0, { s, x -> s + x })";
        let xs = Value::list(vec![int("1"), int("2"), int("3")]);
        assert_eq!(rendered(source, vec![xs]), "12");
    }

    #[test]
    fn list_builtins_never_panic() {
        let source = "fun f(xs: List<Int>, i: Int) = get(xs, i) ?: -1";
        let xs = || Value::list(vec![int("5"), int("6")]);
        assert_eq!(rendered(source, vec![xs(), int("1")]), "6");
        assert_eq!(rendered(source, vec![xs(), int("2")]), "-1");
        assert_eq!(rendered(source, vec![xs(), int("-1")]), "-1");
        assert_eq!(
            rendered("fun f(xs: List<Int>) = first(xs) ?: 0", vec![xs()]),
            "5"
        );
        assert_eq!(
            rendered(
                "fun f(xs: List<Int>) = first(xs) ?: 0",
                vec![Value::list(vec![])]
            ),
            "0"
        );
        assert_eq!(rendered("fun f() = length(listOf(1, 2, 3))", vec![]), "3");
        assert_eq!(
            rendered(
                "fun f(): Int {\n    val e: List<Int> = emptyList()\n    length(e)\n}",
                vec![]
            ),
            "0"
        );
        assert_eq!(rendered("fun f() = listOf(1, 2)", vec![]), "[1, 2]");
    }

    #[test]
    fn option_patterns() {
        let source = "fun f(x: Int?): Int = when (x) {\n    null -> 0\n    n -> n + 1\n}\n";
        assert_eq!(rendered(source, vec![Value::None]), "0");
        assert_eq!(rendered(source, vec![Value::Some(Box::new(int("4")))]), "5");
    }

    #[test]
    fn return_ends_the_current_function() {
        let source = "fun f(x: Int): Int {
    val y = if (x == 0) return 7 else x
    y + 1
}
";
        assert_eq!(rendered(source, vec![int("0")]), "7");
        assert_eq!(rendered(source, vec![int("1")]), "2");
    }

    #[test]
    fn return_as_an_operand_runs_the_earlier_operands_first() {
        // As tests/codegen/returns.bd, whose generated Rust the snapshot crate runs.
        let source = include_str!("../../tests/codegen/returns.bd");
        let call = |name: &str, arg: Value| run(source, name, vec![arg]).map(|v| v.render());
        assert_eq!(call("esquerda", int("4")), Ok("4".into()));
        assert_eq!(call("direita", int("4")), Ok("40".into()));
        assert_eq!(call("nega", int("4")), Ok("4".into()));
        assert_eq!(call("texto", Value::Str("a".into())), Ok("\"a\"".into()));
        assert_eq!(call("escolhe", int("4")), Ok("5".into()));
        assert_eq!(call("ordem", int("1")), Ok("5".into()));
        assert_eq!(call("ordem", int("20000")), Err(EvalError::RecursionLimit));
        assert_eq!(call("condicao", int("4")), Ok("true".into()));
        assert_eq!(call("campo", int("4")), Ok("4".into()));
        assert_eq!(call("aninhado", int("3")), Ok("3".into()));
        assert_eq!(call("ramos", int("4")), Ok("1".into()));
        assert_eq!(call("ramos", int("-4")), Ok("2".into()));
        assert_eq!(call("arms", int("0")), Ok("10".into()));
        assert_eq!(call("arms", int("4")), Ok("20".into()));
        assert_eq!(
            call("aninhado", int("20000")),
            Err(EvalError::RecursionLimit)
        );
    }

    #[test]
    fn depth_limit() {
        let source = "fun loop(n: Int): Int = if (n == 0) 0 else loop(n - 1)";
        assert_eq!(
            run(source, "loop", vec![int("20000")]).map(|v| v.render()),
            Err(EvalError::RecursionLimit)
        );
        // 10,000 calls in all (n = 9999 down to 0 is 10,000 calls) still succeed.
        assert_eq!(
            run(source, "loop", vec![int("9999")]).map(|v| v.render()),
            Ok("0".into())
        );
        assert_eq!(
            run(source, "loop", vec![int("10000")]).map(|v| v.render()),
            Err(EvalError::RecursionLimit)
        );
    }

    #[test]
    fn builtins_do_not_count_toward_the_depth() {
        let source = "fun loop(n: Int): Int = if (n == 0) length(listOf(1)) else loop(n - 1)";
        // loop(9999) is exactly 10,000 function calls; the builtins at the bottom add none.
        assert_eq!(
            run(source, "loop", vec![int("9999")]).map(|v| v.render()),
            Ok("1".into())
        );
        assert_eq!(
            run(source, "loop", vec![int("10000")]).map(|v| v.render()),
            Err(EvalError::RecursionLimit)
        );
    }

    #[test]
    fn closures_count_toward_the_depth() {
        let source =
            "fun g(n: Int): Int = if (n == 0) 0 else fold(listOf(1), 0, { s, x -> g(n - 1) })";
        // g(n) is one call to g plus one to the closure per step: g(n) nests 2n + 1 calls
        // (the last g(0) included), so the limit of 10,000 allows n = 4999 (9,999 calls)
        // and fails at n = 5000 (10,001 calls).
        assert_eq!(
            run(source, "g", vec![int("4999")]).map(|v| v.render()),
            Ok("0".into())
        );
        assert_eq!(
            run(source, "g", vec![int("5000")]).map(|v| v.render()),
            Err(EvalError::RecursionLimit)
        );
    }

    #[test]
    fn integer_patterns_compare_numerically() {
        let source = "fun f(x: Int): Int = when (x) {\n    -0 -> 1\n    0x10 -> 3\n    _ -> 2\n}\n";
        assert_eq!(rendered(source, vec![int("0")]), "1");
        assert_eq!(rendered(source, vec![int("16")]), "3");
        assert_eq!(rendered(source, vec![int("5")]), "2");
    }

    #[test]
    fn database_is_unsupported() {
        let source = format!("{ENTIDADE}fun f(id: ClienteId) = db.clientes.require(id).nome\n");
        let program = compile_with(&source, &schema(CLIENTES));
        let id = ctor("ClienteId", vec![Value::Str("x".into())]);
        assert!(matches!(
            eval_function(&program, "f", vec![id]),
            Err(EvalError::Unsupported(_))
        ));
    }

    #[test]
    fn doubles_are_ieee_binary64() {
        assert_eq!(
            rendered("fun f(x: Double) = x + 1.5", vec![Value::Double(1.0)]),
            "2.5"
        );
        assert_eq!(
            rendered("fun f() = 0.1 + 0.2", vec![]),
            "0.30000000000000004"
        );
        assert_eq!(rendered("fun f(): Double = 1e21 * 10.0", vec![]), "1e+22");
        assert_eq!(rendered("fun f(): Double = 1.0 / 0.0", vec![]), "Infinity");
        assert_eq!(rendered("fun f(): Double = -(0.0 / 0.0)", vec![]), "NaN");
        assert_eq!(rendered("fun f(): Double = 7.5 - 0.25", vec![]), "7.25");
        assert_eq!(rendered("fun f(): Double = -0.0", vec![]), "0");
        let nan = "fun f(): Boolean = 0.0 / 0.0 == 0.0 / 0.0";
        assert_eq!(rendered(nan, vec![]), "false");
        let ne = "fun f(): Boolean = 0.0 / 0.0 != 0.0 / 0.0";
        assert_eq!(rendered(ne, vec![]), "true");
        for op in ["<", "<=", ">", ">="] {
            let source = format!("fun f(): Boolean = 0.0 / 0.0 {op} 1.0");
            assert_eq!(rendered(&source, vec![]), "false", "{op}");
        }
        assert_eq!(rendered("fun f(): Boolean = 1.0 < 2.0", vec![]), "true");
        assert_eq!(rendered("fun f(): Boolean = -0.0 == 0.0", vec![]), "true");
    }

    #[test]
    fn floats_are_ieee_binary32() {
        assert_eq!(
            rendered("fun f(): Float = 0.1f + 0.2f", vec![]),
            display::js_number_to_string((0.1f32 + 0.2f32) as f64)
        );
        assert_eq!(
            rendered("fun f(): Float = 0.1f", vec![]),
            "0.10000000149011612"
        );
        assert_eq!(
            rendered("fun f(x: Float): Float = x * 3.0f", vec![Value::Float(0.1)]),
            display::js_number_to_string((0.1f32 * 3.0f32) as f64)
        );
        assert_eq!(
            rendered("fun f(): Float = 16777216.0f + 1.0f", vec![]),
            "16777216"
        );
        assert_eq!(rendered("fun f(): Boolean = 0.5f < 1.0f", vec![]), "true");
        // A negative zero `Float` is shown as `0`, as `String(-0)` in JS.
        assert_eq!(rendered("fun f(): Float = -0.0f", vec![]), "0");
        assert_eq!(rendered("fun f(): Float = (-1.0f) * 0.0f", vec![]), "0");
        assert_eq!(rendered("fun f(): Boolean = -0.0f == 0.0f", vec![]), "true");
        assert_eq!(rendered("fun f(): Ui = UI { Text(-0.0f) }", vec![]), "0");
        assert_eq!(display::js_number_to_string(f64::from(-0.0f32)), "0");
    }

    #[test]
    fn ui_renders_as_html() {
        assert_eq!(
            rendered("fun f() = UI { Div { Text(1 + 1) } }", vec![]),
            "<div>2</div>"
        );
        // A `UI` from a helper is a fragment: nesting it adds no wrapper.
        let nested = r#"
            fun card(t: String) = UI { Span("card") { Text(t) }; Text("!") }
            fun f() = UI { Div("outer") { card("a"); card("b") }; Rule() }
        "#;
        assert_eq!(
            rendered(nested, vec![]),
            "<div class=\"outer\"><span class=\"card\">a</span>!<span class=\"card\">b</span>!\
             </div><hr>"
        );
        let source = r#"
            fun f(name: String, ok: Boolean, x: Double, y: Float) = UI {
                Section("hero", "top") {
                    val greeting = "Hi " + name
                    Heading(2, "title") { Text(greeting) }
                    Text(ok)
                    Text(x)
                    Text(y)
                    if (ok) Text("<yes>") else Rule("sep")
                    Link("https://x", "c", "_blank") { Text(10000000000000000000000) }
                }
            }
        "#;
        let args = vec![
            Value::Str("A & B".into()),
            Value::Bool(true),
            Value::Double(1e21),
            Value::Float(0.1),
        ];
        assert_eq!(
            rendered(source, args),
            "<section class=\"hero\" id=\"top\">\
             <h2 class=\"title\">Hi A &amp; B</h2>true1e+210.10000000149011612&lt;yes&gt;\
             <a href=\"https://x\" class=\"c\" target=\"_blank\" rel=\"noopener noreferrer\">\
             10000000000000000000000</a></section>"
        );
    }

    #[test]
    fn render_forms() {
        assert_eq!(int("-12").render(), "-12");
        assert_eq!(Value::Bool(true).render(), "true");
        assert_eq!(Value::Bool(false).render(), "false");
        assert_eq!(Value::None.render(), "null");
        assert_eq!(Value::Some(Box::new(int("3"))).render(), "3");
        assert_eq!(Value::Unit.render(), "()");
        assert_eq!(ctor("Folha", vec![]).render(), "Folha");
        assert_eq!(
            ctor("No", vec![int("1"), Value::Str("a".into())]).render(),
            "No(1, \"a\")"
        );
        assert_eq!(
            Value::list(vec![int("1"), Value::None]).render(),
            "[1, null]"
        );
        assert_eq!(Value::list(vec![]).render(), "[]");
    }

    #[test]
    fn strings_render_as_json() {
        assert_eq!(
            Value::Str("a\"b\\c\nd\te\u{1}ção é 😀".into()).render(),
            "\"a\\\"b\\\\c\\nd\\te\\u0001ção é 😀\""
        );
    }
}
