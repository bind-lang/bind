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
//! `display::compare_temporal`. `Decimal` and `Money` values (also only arguments) are
//! `Value::Decimal`, their canonical text, compared by value; their arithmetic is not
//! modelled. `Set` and `Map` values are kept in the canonical key order ([`KeyKind`]) without
//! duplicates. Anything else the generated code does not model in memory (database, `Decimal`
//! arithmetic, durations, environment values, server calls) is `EvalError::Unsupported`.
//!
//! Strings, list items, constructor fields, UI children and a closure's parts (parameters,
//! body, captured locals) are shared (`Arc`), so copying a value costs a few pointers (an
//! `Int` copies its limbs, which the budget charges), and a value nested to any depth is
//! dropped iteratively ([`Shared`]). A closure captures only the locals its body reads: an
//! environment is never copied whole, so closures that capture closures stay linear in size.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::Arc;

use super::bigint::{BigInt, MAX_INT_LIMBS};

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
    /// A `Decimal` or `Money` value: its canonical text (`rust_decimal`'s `Display`), kept as
    /// given. Compared by value ([`decimal_order`]): `1.0` and `1.00` are equal, and a set or
    /// map keeps the text of the element or key it holds first. Bind has no `Decimal`
    /// literal: such values are only ever arguments (built by the tests and the harnesses).
    #[cfg_attr(not(test), allow(dead_code))]
    Decimal(Arc<str>),
    /// A `Set`: its elements in canonical order ([`Interp::key_order`]), without duplicates.
    Set(Shared),
    /// A `Map`: its keys in canonical order, without duplicates, and `values[i]` the value of
    /// `keys[i]` (always the same length).
    Map {
        keys: Shared,
        values: Shared,
    },
    /// Captures, by value, the locals its body uses. Every part is shared: copying a closure
    /// copies three pointers (a closure inside `env` is not copied again).
    Closure {
        params: Arc<[LocalId]>,
        body: Arc<Expr>,
        env: Arc<Env>,
    },
    Function(String),
    /// A builtin function; `key` is its first instance type (the element or key type of the
    /// `Set`/`Map` builtins, which orders their values).
    Builtin {
        name: String,
        key: Option<Type>,
    },
    /// A UI node; `args` are its `String` and `Int` arguments (an `Int` as its decimal text,
    /// computed once) and `children` are `Ui` values.
    Ui {
        node: UiNode,
        args: Vec<UiArgValue>,
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
                Value::List(shared) | Value::Set(shared) | Value::Ctor { fields: shared, .. } => {
                    if let Some(items) = Arc::get_mut(&mut shared.0) {
                        pending.append(items);
                    }
                }
                Value::Map { keys, values } => {
                    for shared in [keys, values] {
                        if let Some(items) = Arc::get_mut(&mut shared.0) {
                            pending.append(items);
                        }
                    }
                }
                Value::Ui { children, .. } => {
                    if let Some(items) = Arc::get_mut(&mut children.0) {
                        pending.append(items);
                    }
                }
                Value::Some(inner) => pending.push(std::mem::replace(&mut **inner, Value::Unit)),
                Value::Closure { env, .. } => {
                    if let Some(env) = Arc::get_mut(env) {
                        pending.extend(std::mem::take(env).into_values());
                    }
                }
                _ => {}
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalError {
    Unsupported(String),
    RecursionLimit,
    /// An `Int` sum, difference or product beyond `bigint::MAX_INT_LIMBS` (the runtimes'
    /// `AppError::Overflow`).
    Overflow,
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
    /// - `Decimal` (also `Money`): its text as held, unquoted (`1.50`, `-0.5`);
    /// - `Set`: `Set[` the elements in canonical order joined by `, ` `]`: `Set[1, 3]`,
    ///   `Set[]`;
    /// - `Map`: `Map[` the entries `key: value` in canonical key order joined by `, ` `]`:
    ///   `Map[1: "a", 2: "b"]`, `Map[]`;
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
            Value::Decimal(text) => text.to_string(),
            Value::Set(items) => format!("Set[{}]", join(items)),
            Value::Map { keys, values } => {
                let entries: Vec<String> = keys
                    .iter()
                    .zip(values.iter())
                    .map(|(key, value)| format!("{}: {}", key.render(), value.render()))
                    .collect();
                format!("Map[{}]", entries.join(", "))
            }
            Value::Closure { .. } | Value::Function(_) | Value::Builtin { .. } => "<fun>".into(),
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
            Value::Decimal(_) => "a Decimal",
            Value::Set(_) => "a set",
            Value::Map { .. } => "a map",
            Value::Closure { .. } | Value::Function(_) | Value::Builtin { .. } => "a function",
            Value::Ui { .. } => "a UI node",
        }
    }
}

/// An argument of a [`Value::Ui`]: a `String`, or an `Int` as its decimal digits (converted
/// once, when the node is built: a subtree shared many times is rendered many times).
#[derive(Clone, Debug)]
pub enum UiArgValue {
    Str(Arc<str>),
    Int(Arc<str>),
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
        Some(match args.get(index)? {
            UiArgValue::Str(text) => UiArgView::Str(text),
            UiArgValue::Int(digits) => UiArgView::Int(Cow::Borrowed(digits)),
        })
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
/// a concatenation, the locals a closure captures (and copies again at each call), a 64th of
/// the limbs of an `Int` local read, the decimal text of a UI node's `Int` argument (computed
/// once, however often a shared subtree renders it), the children a UI node gets and every pair
/// of values `==` / `!=` visits. Every other operation takes constant time (values are shared), so
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
    int_limbs: MAX_INT_LIMBS,
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
                    functions: program
                        .functions
                        .iter()
                        .map(|function| (function.name.as_str(), function))
                        .collect(),
                    lambdas: HashMap::new(),
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
    /// The program's functions by name.
    functions: HashMap<&'p str, &'p CoreFunction>,
    /// Each lambda of the program evaluated so far (keyed by its body's address in `program`),
    /// prepared once: its parameters, its shared body and the locals the body uses.
    lambdas: HashMap<*const Expr, Lambda>,
    depth: usize,
    /// Expressions under evaluation (see [`MAX_NESTING`]).
    nesting: usize,
    /// The remaining budget, if bounded (`work` counts down).
    budget: Option<Budget>,
}

/// A lambda of the program, prepared for building closures (see [`Interp::lambdas`]).
#[derive(Clone)]
struct Lambda {
    params: Arc<[LocalId]>,
    body: Arc<Expr>,
    /// Every local the body reads, sorted, without repetitions: what a closure captures
    /// (those bound inside the body are never in scope where the lambda is evaluated).
    uses: Arc<[LocalId]>,
}

/// The locals `expr` reads (`Local` nodes), sorted, without repetitions.
fn used_locals(expr: &Expr) -> Vec<LocalId> {
    let mut output = Vec::new();
    let mut pending = vec![expr];
    while let Some(expr) = pending.pop() {
        if let ExprKind::Local(id) = expr.kind {
            output.push(id);
        }
        pending.extend(super::specialize::children(expr));
    }
    output.sort_unstable();
    output.dedup();
    output
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
                let function = *self
                    .functions
                    .get(name.as_str())
                    .ok_or_else(|| unsupported(&format!("no function `{name}`")))?;
                let mut env = bind_params(function.params.iter().map(|(id, _, _)| *id), args)?;
                self.eval(&function.body, &mut env)
            }
            Value::Closure { params, body, env } => {
                self.charge(env.len() as u64)?;
                let mut env = Arc::unwrap_or_clone(env);
                for (id, value) in bind_params(params.iter().copied(), args)? {
                    env.insert(id, value);
                }
                self.eval(&body, &mut env)
            }
            Value::Builtin { name, key } => self.builtin(&name, key.as_ref(), args),
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
            ExprKind::Local(id) => {
                // Copying an `Int` copies its limbs (everything else is shared).
                if let Some(Value::Int(value)) = env.get(id) {
                    self.charge(value.limb_count() as u64 / 64)?;
                }
                local(env, *id)
            }
            ExprKind::Global { name, .. } => Ok(Value::Function(name.clone())),
            ExprKind::Builtin { name, instance } => Ok(Value::Builtin {
                name: name.clone(),
                key: instance.first().cloned(),
            }),
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
        let lambda = match self.lambdas.get(&std::ptr::from_ref(body)) {
            Some(lambda) => lambda.clone(),
            None => {
                // Once per lambda of the program: bounded by the program's size.
                let lambda = Lambda {
                    params: params.iter().map(|(id, _, _)| *id).collect(),
                    body: Arc::new(body.clone()),
                    uses: used_locals(body).into(),
                };
                self.lambdas
                    .insert(std::ptr::from_ref(body), lambda.clone());
                lambda
            }
        };
        self.charge(lambda.uses.len() as u64)?;
        let captured: Env = lambda
            .uses
            .iter()
            .filter_map(|id| env.get(id).map(|value| (*id, value.clone())))
            .collect();
        Ok(Value::Closure {
            params: lambda.params,
            body: lambda.body,
            env: Arc::new(captured),
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
        let mut texts = Vec::with_capacity(args.len());
        for arg in args {
            texts.push(match arg {
                Value::Int(value) => {
                    // The cost of `DisplayInt`, paid once for every render of the node.
                    let size = value.limb_count().max(1) as u64;
                    self.charge(size * size / 8)?;
                    UiArgValue::Int(value.to_string().into())
                }
                Value::Str(text) => UiArgValue::Str(text),
                _ => unreachable!("checked above"),
            });
        }
        let args = texts;
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
                (Value::List(a), Value::List(b)) | (Value::Set(a), Value::Set(b)) => {
                    if a.len() != b.len() {
                        return Ok(false);
                    }
                    pending.extend(a.iter().zip(b.iter()).rev());
                }
                (
                    Value::Map {
                        keys: ka,
                        values: va,
                    },
                    Value::Map {
                        keys: kb,
                        values: vb,
                    },
                ) => {
                    if ka.len() != kb.len() {
                        return Ok(false);
                    }
                    pending.extend(va.iter().zip(vb.iter()).rev());
                    pending.extend(ka.iter().zip(kb.iter()).rev());
                }
                (Value::Decimal(a), Value::Decimal(b)) => {
                    if decimal_order(a, b)? != Ordering::Equal {
                        return Ok(false);
                    }
                }
                _ => return Err(unsupported("equality on incomparable values")),
            }
        }
        Ok(true)
    }

    fn builtin(&mut self, name: &str, key: Option<&Type>, args: Vec<Value>) -> Eval {
        if is_collection_builtin(name) {
            let key =
                key.ok_or_else(|| unsupported(&format!("builtin `{name}` without a key type")))?;
            let order = self.key_kind(key)?;
            return Ok(self.collection_builtin(name, &order, args)?);
        }
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

/// The `Set`/`Map` builtins (`setOf` included), whose first instance type is the key type.
fn is_collection_builtin(name: &str) -> bool {
    matches!(
        name,
        "setOf"
            | "emptySet"
            | "insert"
            | "remove"
            | "contains"
            | "setSize"
            | "toList"
            | "emptyMap"
            | "put"
            | "lookup"
            | "removeKey"
            | "keys"
            | "values"
            | "mapSize"
    )
}

/// How the values of a `Key` type are ordered (the canonical order of sets and map keys,
/// the same in the three runtimes).
enum KeyKind {
    /// `Int`: numeric.
    Int,
    /// `Boolean`: `false < true`.
    Bool,
    /// `Decimal`, `Money`: numeric, by value ([`decimal_order`]).
    Decimal,
    /// `Date`, `DateTime`, `Instant`: `display::compare_temporal` on the canonical texts.
    Temporal,
    /// `String`, `Email`, `Url`, `UUID`: by Unicode code point (the order of the UTF-8 bytes;
    /// a `UUID` is its canonical lowercase text, so this is also the order of its 128 bits).
    Text,
    /// An `enum class` or a `sealed class` of `data object`s: the declaration order of its
    /// variants.
    Enum(Vec<String>),
    /// A distinct type: the order of the value it wraps.
    Distinct(Box<KeyKind>),
}

impl Interp<'_> {
    /// The order of the values of the `Key` type `ty`.
    fn key_kind(&self, ty: &Type) -> Result<KeyKind, EvalError> {
        let mut current = ty;
        let mut wrappers = 0;
        let base = loop {
            let name = scalar(current).ok_or_else(|| unsupported("a key of a non-key type"))?;
            let kind = match name {
                "Int" => KeyKind::Int,
                "Boolean" => KeyKind::Bool,
                "Decimal" | "Money" => KeyKind::Decimal,
                "Date" | "DateTime" | "Instant" => KeyKind::Temporal,
                "String" | "Email" | "Url" | "UUID" => KeyKind::Text,
                _ => match self.program.types.iter().find(|ty| type_name(ty) == name) {
                    Some(CoreType::Sum { variants, .. }) => {
                        KeyKind::Enum(variants.iter().map(|(name, _)| name.clone()).collect())
                    }
                    // Bounded: a cycle of distinct types is never a `Key` type.
                    Some(CoreType::Distinct { underlying, .. }) if wrappers < 64 => {
                        wrappers += 1;
                        current = underlying;
                        continue;
                    }
                    _ => return Err(unsupported(&format!("a key of type `{name}`"))),
                },
            };
            break kind;
        };
        Ok((0..wrappers).fold(base, |kind, _| KeyKind::Distinct(Box::new(kind))))
    }

    /// The canonical order of two keys (one unit of work).
    fn key_order(&mut self, kind: &KeyKind, a: &Value, b: &Value) -> Result<Ordering, EvalError> {
        self.charge(1)?;
        key_order(kind, a, b)
    }

    /// The position of `key` in the canonically ordered `items`: `Ok` where an equal key is,
    /// `Err` where it would be inserted.
    fn search(
        &mut self,
        kind: &KeyKind,
        items: &[Value],
        key: &Value,
    ) -> Result<Result<usize, usize>, EvalError> {
        let (mut low, mut high) = (0, items.len());
        while low < high {
            let middle = low + (high - low) / 2;
            match self.key_order(kind, &items[middle], key)? {
                Ordering::Less => low = middle + 1,
                Ordering::Greater => high = middle,
                Ordering::Equal => return Ok(Ok(middle)),
            }
        }
        Ok(Err(low))
    }

    /// `s` with `x` added; `s` itself when it has an equal element (which it keeps).
    fn insert(&mut self, kind: &KeyKind, set: Shared, item: Value) -> Result<Shared, EvalError> {
        match self.search(kind, &set, &item)? {
            Ok(_) => Ok(set),
            Err(index) => {
                self.charge(set.len() as u64)?;
                let mut items = set.to_vec();
                items.insert(index, item);
                Ok(items.into())
            }
        }
    }

    /// The `Set`/`Map` builtins. Collections are immutable: an operation that changes nothing
    /// (inserting an element already there, removing an absent one) returns the same value.
    fn collection_builtin(
        &mut self,
        name: &str,
        kind: &KeyKind,
        args: Vec<Value>,
    ) -> Result<Value, EvalError> {
        let bad = || unsupported(&format!("bad arguments for builtin `{name}`"));
        let mut args = args.into_iter();
        let mut next = || args.next();
        Ok(match (name, next(), next(), next()) {
            // Inserted left to right: of equal elements, the first is kept.
            ("setOf", first, second, third) => {
                let mut set = Shared::default();
                for item in [first, second, third].into_iter().flatten().chain(args) {
                    set = self.insert(kind, set, item)?;
                }
                Value::Set(set)
            }
            ("emptySet", None, None, None) => Value::Set(Shared::default()),
            ("insert", Some(Value::Set(set)), Some(item), None) => {
                Value::Set(self.insert(kind, set, item)?)
            }
            ("remove", Some(Value::Set(set)), Some(item), None) => {
                match self.search(kind, &set, &item)? {
                    Ok(index) => {
                        self.charge(set.len() as u64)?;
                        let mut items = set.to_vec();
                        items.remove(index);
                        Value::Set(items.into())
                    }
                    Err(_) => Value::Set(set),
                }
            }
            ("contains", Some(Value::Set(set)), Some(item), None) => {
                Value::Bool(self.search(kind, &set, &item)?.is_ok())
            }
            ("setSize", Some(Value::Set(set)), None, None) => Value::Int(BigInt::from(set.len())),
            ("toList", Some(Value::Set(set)), None, None) => Value::List(set),
            ("emptyMap", None, None, None) => Value::Map {
                keys: Shared::default(),
                values: Shared::default(),
            },
            // An equal key already there keeps its text; only the value is replaced.
            ("put", Some(Value::Map { keys, values }), Some(key), Some(value)) => {
                self.charge(keys.len() as u64)?;
                match self.search(kind, &keys, &key)? {
                    Ok(index) => {
                        let mut items = values.to_vec();
                        items[index] = value;
                        Value::Map {
                            keys,
                            values: items.into(),
                        }
                    }
                    Err(index) => {
                        let (mut new_keys, mut new_values) = (keys.to_vec(), values.to_vec());
                        new_keys.insert(index, key);
                        new_values.insert(index, value);
                        Value::Map {
                            keys: new_keys.into(),
                            values: new_values.into(),
                        }
                    }
                }
            }
            ("lookup", Some(Value::Map { keys, values }), Some(key), None) => {
                match self.search(kind, &keys, &key)? {
                    Ok(index) => option(values.get(index).cloned()),
                    Err(_) => Value::None,
                }
            }
            ("removeKey", Some(Value::Map { keys, values }), Some(key), None) => {
                match self.search(kind, &keys, &key)? {
                    Ok(index) => {
                        self.charge(keys.len() as u64)?;
                        let (mut new_keys, mut new_values) = (keys.to_vec(), values.to_vec());
                        new_keys.remove(index);
                        new_values.remove(index);
                        Value::Map {
                            keys: new_keys.into(),
                            values: new_values.into(),
                        }
                    }
                    Err(_) => Value::Map { keys, values },
                }
            }
            ("keys", Some(Value::Map { keys, .. }), None, None) => Value::List(keys),
            ("values", Some(Value::Map { values, .. }), None, None) => Value::List(values),
            ("mapSize", Some(Value::Map { keys, .. }), None, None) => {
                Value::Int(BigInt::from(keys.len()))
            }
            _ => return Err(bad()),
        })
    }
}

fn type_name(ty: &CoreType) -> &str {
    match ty {
        CoreType::Record { name, .. }
        | CoreType::Sum { name, .. }
        | CoreType::Distinct { name, .. } => name,
    }
}

/// The canonical order of two keys of the kind `kind` (see [`KeyKind`]).
fn key_order(kind: &KeyKind, a: &Value, b: &Value) -> Result<Ordering, EvalError> {
    match (kind, a, b) {
        (KeyKind::Int, Value::Int(a), Value::Int(b)) => Ok(a.cmp(b)),
        (KeyKind::Bool, Value::Bool(a), Value::Bool(b)) => Ok(a.cmp(b)),
        (KeyKind::Decimal, Value::Decimal(a), Value::Decimal(b)) => decimal_order(a, b),
        (KeyKind::Temporal, Value::Str(a), Value::Str(b)) => Ok(display::compare_temporal(a, b)),
        (KeyKind::Text, Value::Str(a), Value::Str(b)) => Ok(a.as_bytes().cmp(b.as_bytes())),
        (KeyKind::Enum(variants), Value::Ctor { name: a, .. }, Value::Ctor { name: b, .. }) => {
            let index = |name: &String| {
                variants
                    .iter()
                    .position(|variant| variant == name)
                    .ok_or_else(|| {
                        unsupported(&format!("`{name}` is not a variant of the key type"))
                    })
            };
            Ok(index(a)?.cmp(&index(b)?))
        }
        (
            KeyKind::Distinct(inner),
            Value::Ctor { fields: a, .. },
            Value::Ctor { fields: b, .. },
        ) => match (a.first(), b.first()) {
            (Some(a), Some(b)) => key_order(inner, a, b),
            _ => Err(unsupported("a distinct key without its value")),
        },
        _ => Err(unsupported(&format!(
            "ordering {} and {} as keys",
            a.kind(),
            b.kind()
        ))),
    }
}

/// The value of a canonical `Decimal` text (`-?digits(.digits)?`, as `rust_decimal` prints it)
/// as `mantissa / 10^scale`; `None` for any other text.
fn parse_decimal(text: &str) -> Option<(BigInt, usize)> {
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty()
        || !digits(whole)
        || !digits(fraction)
        || (unsigned.contains('.') && fraction.is_empty())
    {
        return None;
    }
    let magnitude = BigInt::parse_bytes(format!("{whole}{fraction}").as_bytes(), 10)?;
    Some((
        if negative { -magnitude } else { magnitude },
        fraction.len(),
    ))
}

/// The order of two `Decimal` (or `Money`) texts by value: `1.0` equals `1.00` and `-0`
/// equals `0`, as `rust_decimal` compares them.
pub(crate) fn decimal_order(a: &str, b: &str) -> Result<Ordering, EvalError> {
    let invalid = |text: &str| unsupported(&format!("invalid Decimal `{text}`"));
    let (mut x, x_scale) = parse_decimal(a).ok_or_else(|| invalid(a))?;
    let (mut y, y_scale) = parse_decimal(b).ok_or_else(|| invalid(b))?;
    let ten = || BigInt::from(10usize);
    for _ in x_scale..y_scale {
        x = x * ten();
    }
    for _ in y_scale..x_scale {
        y = y * ten();
    }
    Ok(x.cmp(&y))
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

/// An `Int` result of arithmetic, or `Overflow` beyond `MAX_INT_LIMBS` (as in the runtimes).
fn bounded(value: BigInt) -> Result<Value, EvalError> {
    if value.exceeds_int_bound() {
        Err(EvalError::Overflow)
    } else {
        Ok(Value::Int(value))
    }
}

fn prim(op: Prim, args: Vec<Value>) -> Result<Value, EvalError> {
    let bad = || unsupported(&format!("bad operands for {op:?}"));
    let mut args = args.into_iter();
    let (a, b) = (args.next(), args.next());
    let zero = BigInt::from(0);
    Ok(match (op, a, b) {
        (Prim::IntAdd, Some(Value::Int(a)), Some(Value::Int(b))) => bounded(a + b)?,
        (Prim::IntSub, Some(Value::Int(a)), Some(Value::Int(b))) => bounded(a - b)?,
        (Prim::IntMul, Some(Value::Int(a)), Some(Value::Int(b))) => {
            // The product has at least `limbs(a) + limbs(b) - 1` limbs: refused unbuilt.
            let (x, y) = (a.limb_count(), b.limb_count());
            if x > 0 && y > 0 && x + y - 1 > MAX_INT_LIMBS {
                return Err(EvalError::Overflow);
            }
            bounded(a * b)?
        }
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
        (Value::Decimal(a), Value::Decimal(b)) => decimal_order(a, b).map(Some),
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

    /// `n` lambdas in one scope (`fK = { x -> x + K }`), then `f{n-1}(1)`. Each closure once
    /// captured the whole environment by deep copy, so the last one held 2^n copies: 22 took
    /// 4 GB, 40 never finished.
    fn many_lambdas(n: usize) -> String {
        let mut source = String::from("fun f(): Int {\n");
        for k in 0..n {
            source.push_str(&format!(
                "    val f{k}: (Int) -> Int = {{ x -> x + {k} }}\n"
            ));
        }
        source.push_str(&format!("    return f{}(1)\n}}\n", n - 1));
        source
    }

    #[test]
    fn closures_capture_only_what_they_use_and_stay_linear() {
        let started = std::time::Instant::now();
        let program = compile(&many_lambdas(60));
        let value = eval_function_within(&program, "f", Vec::new(), PRERENDER_BUDGET);
        assert_eq!(value.map(|value| value.render()), Ok("60".into()));
        assert_eq!(rendered(&many_lambdas(60), Vec::new()), "60");
        // Closures that capture closures, and a closure built in one call and run in another.
        let nested = "fun twice(g: (Int) -> Int): (Int) -> Int = { x -> g(g(x)) }\nfun f(n: Int): Int {\n    val a: (Int) -> Int = { x -> x + n }\n    val b = twice(a)\n    val c = twice(b)\n    val d: (Int) -> Int = { x -> c(x) * b(x) }\n    return fold(map(listOf(1, 2), d), 0, { s, x -> s + x })\n}";
        // d(1) = (1 + 4n) * (1 + 2n), d(2) = (2 + 4n) * (2 + 2n); n = 1: 15 + 24.
        assert_eq!(rendered(nested, vec![int("1")]), "39");
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(10), "{elapsed:?}");
    }

    #[test]
    fn int_arguments_of_shared_subtrees_are_converted_once() {
        // A 1024-limb `Int` as a heading level, in a subtree shared 2^17 times: rendering
        // converted it to decimal at every visit (over a minute in a release build).
        let mut source = String::from("fun f(): Ui {\n    val x0 = 2\n");
        for k in 0..14 {
            source.push_str(&format!("    val x{} = x{k} * x{k}\n", k + 1));
        }
        source.push_str("    val y = x14 * (x14 - 1)\n    val t0 = UI { Heading(y) }\n");
        for k in 0..17 {
            source.push_str(&format!(
                "    val t{} = UI {{ Div {{ t{k}; t{k} }} }}\n",
                k + 1
            ));
        }
        source.push_str("    return t17\n}\n");
        let started = std::time::Instant::now();
        let value = eval_function_within(&compile(&source), "f", Vec::new(), PRERENDER_BUDGET)
            .expect("within the budget");
        assert_eq!(value.render_html_within(PRERENDER_BUDGET.html_bytes), None);
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(10), "{elapsed:?}");
        // The converted text keeps the argument an `Int`: a level, not a class.
        let small = "fun f(): Ui {\n    val t = UI { Heading(1 + 2, \"c\") { Text(10 * 10) } }\n    return UI { Div { t; t } }\n}";
        assert_eq!(
            rendered(small, Vec::new()),
            "<div><h3 class=\"c\">100</h3><h3 class=\"c\">100</h3></div>"
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

    fn decimal(text: &str) -> Value {
        Value::Decimal(text.into())
    }

    fn text(value: &str) -> Value {
        Value::Str(value.into())
    }

    #[test]
    fn sets_are_canonical_and_without_duplicates() {
        assert_eq!(
            rendered("fun f() = toList(setOf(3, 1, 3))", vec![]),
            "[1, 3]"
        );
        assert_eq!(rendered("fun f() = setOf(3, 1, 3)", vec![]), "Set[1, 3]");
        assert_eq!(rendered("fun f(): Set<Int> = emptySet()", vec![]), "Set[]");
        assert_eq!(
            rendered("fun f(): Map<Int, String> = emptyMap()", vec![]),
            "Map[]"
        );
        assert_eq!(
            rendered(
                "fun f() = setOf(\"b\", \"a\", \"B\", \"é\", \"z\", \"\")",
                vec![]
            ),
            "Set[\"\", \"B\", \"a\", \"b\", \"z\", \"é\"]"
        );
        assert_eq!(
            rendered("fun f() = setOf(true, false, true)", vec![]),
            "Set[false, true]"
        );
        assert_eq!(
            rendered("fun f() = setOf(10, -2, 0, 7, -30)", vec![]),
            "Set[-30, -2, 0, 7, 10]"
        );
        let ops = "fun f(s: Set<Int>): List<Int> = listOf(setSize(s), setSize(insert(s, 2)), setSize(insert(s, 9)), setSize(remove(s, 2)), if (contains(s, 2)) 1 else 0, if (contains(s, 3)) 1 else 0)";
        assert_eq!(
            run(ops, "f", vec![Value::Set(vec![int("1"), int("2")].into())])
                .expect("evaluates")
                .render(),
            "[2, 2, 3, 1, 1, 0]"
        );
    }

    /// The fixture types of `runtime/vectors/collections.json` (see its README).
    const COLLECTION_FIXTURES: &str = "enum class Cor { Z, A, M }\ndistinct type Nota = Decimal\n";

    /// A `bind-json` value of the vectors as an evaluator value of type `ty`.
    fn from_wire(ty: &str, json: &display::tests::Json) -> Value {
        use display::tests::Json;
        match (ty, json) {
            ("Int", Json::Str(text)) => int(text),
            ("Decimal", Json::Str(text)) => decimal(text),
            ("Nota", _) => ctor("Nota", vec![from_wire("Decimal", json)]),
            ("Cor", Json::Str(name)) => ctor(&name.to_uppercase(), vec![]),
            ("Boolean", Json::Bool(value)) => Value::Bool(*value),
            ("String" | "UUID" | "Date" | "DateTime" | "Instant", Json::Str(value)) => text(value),
            other => panic!("unexpected vector value {other:?}"),
        }
    }

    /// The `bind-json` form of an evaluator value of a fixture or key type.
    fn to_wire(value: &Value) -> display::tests::Json {
        use display::tests::Json;
        match value {
            Value::Int(_) => Json::Str(value.render()),
            Value::Decimal(text) | Value::Str(text) => Json::Str(text.to_string()),
            Value::Bool(value) => Json::Bool(*value),
            Value::None => Json::Null,
            Value::Some(inner) => to_wire(inner),
            Value::Ctor { name, fields } if name == "Nota" => to_wire(&fields.0[0]),
            Value::Ctor { name, fields } if fields.0.is_empty() => Json::Str(name.to_lowercase()),
            Value::List(items) | Value::Set(items) => {
                Json::Array(items.0.iter().map(to_wire).collect())
            }
            Value::Map { keys, values } => Json::Array(
                keys.0
                    .iter()
                    .zip(values.0.iter())
                    .map(|(key, value)| Json::Array(vec![to_wire(key), to_wire(value)]))
                    .collect(),
            ),
            other => panic!("unexpected value {other:?}"),
        }
    }

    /// `runtime/vectors/collections.json` agrees with the evaluator, the oracle of the Rust and
    /// JS runtimes: each case becomes a Bind expression over parameters (`Decimal` has no
    /// literal), evaluated with the case's values.
    #[test]
    fn collection_vectors_agree_with_the_evaluator() {
        use display::tests::{Json, parse_json, vector_path};
        let text = std::fs::read_to_string(vector_path("collections.json")).expect("vectors");
        let cases = parse_json(&text);
        assert!(cases.array().len() >= 20);
        for case in cases.array() {
            let set = case.get("kind").str() == "set";
            let ty = if set {
                case.get("element").str()
            } else {
                case.get("key").str()
            };
            // Parameters `p<i>` (with their values) and the collection expression.
            let mut params: Vec<(String, &str)> = Vec::new();
            let mut args = Vec::new();
            let mut param = |param_ty: &'static str, json: &Json, args: &mut Vec<Value>| {
                let name = format!("p{}", params.len());
                params.push((format!("{name}: {param_ty}"), param_ty));
                args.push(from_wire(param_ty, json));
                name
            };
            let key_ty: &'static str = match ty {
                "Int" => "Int",
                "Decimal" => "Decimal",
                "Nota" => "Nota",
                "Cor" => "Cor",
                "Boolean" => "Boolean",
                "String" => "String",
                "UUID" => "UUID",
                "Date" => "Date",
                "DateTime" => "DateTime",
                "Instant" => "Instant",
                other => panic!("unknown type {other}"),
            };
            let mut collection = if set {
                let items: Vec<String> = case
                    .get("setOf")
                    .array()
                    .iter()
                    .map(|item| param(key_ty, item, &mut args))
                    .collect();
                if items.is_empty() {
                    "emptySet()".to_string()
                } else {
                    format!("setOf({})", items.join(", "))
                }
            } else {
                "emptyMap()".to_string()
            };
            for step in case.get("steps").array() {
                let step = step.array();
                let key = param(key_ty, &step[1], &mut args);
                collection = match step[0].str() {
                    "put" => {
                        let value = param("String", &step[2], &mut args);
                        format!("put({collection}, {key}, {value})")
                    }
                    op => format!("{op}({collection}, {key})"),
                };
            }
            let checks: Vec<String> = case
                .get(if set { "contains" } else { "lookup" })
                .array()
                .iter()
                .map(|check| {
                    let item = param(key_ty, &check.array()[0], &mut args);
                    if set {
                        format!("contains(c, {item})")
                    } else {
                        format!("lookup(c, {item})")
                    }
                })
                .collect();
            let collection_ty = if set {
                format!("Set<{key_ty}>")
            } else {
                format!("Map<{key_ty}, String>")
            };
            let signature: Vec<&str> = params.iter().map(|(text, _)| text.as_str()).collect();
            let source = format!(
                "{COLLECTION_FIXTURES}fun c0({}): {collection_ty} = {collection}\nfun f({}): List<{}> {{\n    val c = c0({})\n    return listOf({})\n}}\nfun g({}): {collection_ty} = c0({})\n",
                signature.join(", "),
                signature.join(", "),
                if set { "Boolean" } else { "String?" },
                (0..params.len())
                    .map(|i| format!("p{i}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                checks.join(", "),
                signature.join(", "),
                (0..params.len())
                    .map(|i| format!("p{i}"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            let program = compile(&source);
            let collection = eval_function(&program, "g", args.clone()).expect("evaluates");
            let results = eval_function(&program, "f", args).expect("evaluates");
            let wire = to_wire(&collection);
            let expected_checks: Vec<&Json> = case
                .get(if set { "contains" } else { "lookup" })
                .array()
                .iter()
                .map(|check| &check.array()[1])
                .collect();
            let Json::Array(found_checks) = to_wire(&results) else {
                panic!("not a list");
            };
            assert_eq!(
                found_checks.iter().collect::<Vec<_>>(),
                expected_checks,
                "{source}"
            );
            if set {
                assert_eq!(&wire, case.get("toList"), "{source}");
            } else {
                assert_eq!(&wire, case.get("entries"), "{source}");
            }
        }
    }

    /// Review Focus 3: variants are ordered by declaration, not by name.
    #[test]
    fn enum_keys_follow_declaration_order() {
        let source = "enum class Cor { Z, A, M }\nfun f() = toList(setOf(M, Z, A))\nfun g() = keys(put(put(put(emptyMap(), Cor.A, 1), Cor.M, 2), Cor.Z, 3))";
        assert_eq!(rendered(source, vec![]), "[Z, A, M]");
        assert_eq!(
            run(source, "g", vec![]).expect("evaluates").render(),
            "[Z, A, M]"
        );
        // A sealed class made only of `data object`s orders the same way, also under a distinct.
        let sealed = "sealed class Fase {\n    data object Fim\n    data object Inicio\n}\ndistinct type Etapa = Fase\nfun f() = toList(setOf(Inicio, Fim, Inicio))\nfun g() = setOf(Etapa(Inicio), Etapa(Fim))";
        assert_eq!(rendered(sealed, vec![]), "[Fim, Inicio]");
        assert_eq!(
            run(sealed, "g", vec![]).expect("evaluates").render(),
            "Set[Etapa(Fim), Etapa(Inicio)]"
        );
    }

    /// Review Focus 1: `Decimal` keys are compared by value; the stored key keeps its text.
    #[test]
    fn decimal_keys_compare_by_value() {
        let lookup =
            "fun f(a: Decimal, b: Decimal): String? = lookup(put(emptyMap(), a, \"a\"), b)";
        assert_eq!(
            rendered(lookup, vec![decimal("1.0"), decimal("1.00")]),
            "\"a\""
        );
        assert_eq!(
            rendered(lookup, vec![decimal("1.0"), decimal("1.01")]),
            "null"
        );
        // Controller Ruling A: `put` keeps the existing key, `insert` the existing element.
        let put = "fun f(a: Decimal, b: Decimal) = put(put(emptyMap(), a, \"x\"), b, \"y\")";
        assert_eq!(
            rendered(put, vec![decimal("1.0"), decimal("1.00")]),
            "Map[1.0: \"y\"]"
        );
        let insert = "fun f(a: Decimal, b: Decimal) = insert(setOf(a), b)";
        assert_eq!(
            rendered(insert, vec![decimal("1.0"), decimal("1.00")]),
            "Set[1.0]"
        );
        let first = "fun f(a: Decimal, b: Decimal) = setOf(b, a)";
        assert_eq!(
            rendered(first, vec![decimal("1.0"), decimal("1.00")]),
            "Set[1.00]"
        );
        let order = "fun f(a: Decimal, b: Decimal, c: Decimal, d: Decimal, e: Decimal) = setOf(a, b, c, d, e)";
        assert_eq!(
            rendered(
                order,
                vec![
                    decimal("10"),
                    decimal("9.5"),
                    decimal("-1"),
                    decimal("0.001"),
                    decimal("-0.50")
                ]
            ),
            "Set[-1, -0.50, 0.001, 9.5, 10]"
        );
        assert_eq!(
            rendered(
                "fun f(a: Money, b: Money) = setOf(a, b)",
                vec![decimal("0.00"), decimal("-0")]
            ),
            "Set[0.00]"
        );
        let equal = "fun f(a: Decimal, b: Decimal) = listOf(setOf(a) == setOf(b), a == b, a < b)";
        assert_eq!(
            rendered(equal, vec![decimal("2.50"), decimal("2.5")]),
            "[true, true, false]"
        );
    }

    #[test]
    fn temporal_and_distinct_keys_use_their_typed_order() {
        let dates = "fun f(a: Date, b: Date, c: Date) = toList(setOf(a, b, c))";
        assert_eq!(
            rendered(
                dates,
                vec![
                    text("+10000-01-01"),
                    text("2026-01-02"),
                    text("-0001-01-01")
                ]
            ),
            "[\"-0001-01-01\", \"2026-01-02\", \"+10000-01-01\"]"
        );
        let instants = "fun f(a: Instant, b: Instant) = setOf(a, b)";
        assert_eq!(
            rendered(
                instants,
                vec![
                    text("2026-01-01T10:00:00.500Z"),
                    text("2026-01-01T10:00:00Z")
                ]
            ),
            "Set[\"2026-01-01T10:00:00Z\", \"2026-01-01T10:00:00.500Z\"]"
        );
        let distinct = "distinct type Dia = Date\nfun f(a: Date, b: Date) = setOf(Dia(a), Dia(b))";
        assert_eq!(
            rendered(distinct, vec![text("+10000-01-01"), text("2026-01-01")]),
            "Set[Dia(\"2026-01-01\"), Dia(\"+10000-01-01\")]"
        );
    }

    #[test]
    fn maps_keep_values_in_key_order() {
        let build =
            "fun m(): Map<Int, String> = put(put(put(emptyMap(), 3, \"c\"), 1, \"a\"), 2, \"b\")\n";
        let eval = |body: &str| {
            run(&format!("{build}fun f() = {body}"), "f", vec![])
                .expect("evaluates")
                .render()
        };
        assert_eq!(eval("m()"), "Map[1: \"a\", 2: \"b\", 3: \"c\"]");
        assert_eq!(eval("keys(m())"), "[1, 2, 3]");
        assert_eq!(eval("values(m())"), "[\"a\", \"b\", \"c\"]");
        assert_eq!(eval("mapSize(m())"), "3");
        assert_eq!(eval("lookup(m(), 2)"), "\"b\"");
        assert_eq!(eval("lookup(m(), 4)"), "null");
        assert_eq!(eval("removeKey(m(), 1)"), "Map[2: \"b\", 3: \"c\"]");
        assert_eq!(
            eval("put(m(), 2, \"z\")"),
            "Map[1: \"a\", 2: \"z\", 3: \"c\"]"
        );
        assert_eq!(eval("put(emptyMap(), 1, setOf(2))"), "Map[1: Set[2]]");
        assert_eq!(
            eval("lookup(put(emptyMap(), 1, first(emptyList())), 1)"),
            "null"
        );
    }

    #[test]
    fn removing_an_absent_element_returns_the_same_collection() {
        let set: Value = Value::Set(vec![int("1"), int("2")].into());
        let Value::Set(same) =
            run("fun f(s: Set<Int>) = remove(s, 5)", "f", vec![set.clone()]).expect("evaluates")
        else {
            panic!("not a set")
        };
        let Value::Set(original) = &set else { panic!() };
        assert!(Arc::ptr_eq(&same.0, &original.0));
        let map = Value::Map {
            keys: vec![int("1")].into(),
            values: vec![text("a")].into(),
        };
        let removed = run(
            "fun f(m: Map<Int, String>) = removeKey(m, 5)",
            "f",
            vec![map.clone()],
        )
        .expect("evaluates");
        let (Value::Map { keys: a, .. }, Value::Map { keys: b, .. }) = (&removed, &map) else {
            panic!("not maps")
        };
        assert!(Arc::ptr_eq(&a.0, &b.0));
        assert_eq!(removed.render(), "Map[1: \"a\"]");
    }
}
