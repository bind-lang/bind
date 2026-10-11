//! A reference evaluator over the core IR, for tests only. Its semantics are those of the
//! generated Rust (Task 6): arbitrary-precision `Int`, truncating division with the sign of the
//! remainder following the dividend, `None` for a zero divisor, capture-by-copy closures and a
//! call-depth limit of 10,000. Task 9 compares it with the generated program.
//!
//! `Double` is Rust's `f64` and `Float` Rust's `f32` (IEEE binary64/binary32, as JS numbers and
//! `Math.fround`); a UI tree is a `Value::Ui`, rendered as the page's HTML; `Display*`
//! primitives give the text `Text` shows (`core::display`). Anything else the generated code
//! does not model in memory (database, `Decimal`, durations, dates, environment values, server
//! calls) is `EvalError::Unsupported`.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use num_bigint::BigInt;

use crate::types::exhaustive::Pat;
use crate::types::ty::Type;

use super::display::{self, UiArg, UiTree};
use super::ir::*;

/// Maximum number of nested function/closure calls; call number 10,001 fails.
pub const DEPTH_LIMIT: usize = 10_000;

/// Stack of the evaluation thread: a recursive interpreter needs far more than a test thread has.
const STACK_SIZE: usize = 512 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum Value {
    Int(BigInt),
    Double(f64),
    Float(f32),
    Bool(bool),
    Str(String),
    Unit,
    None,
    Some(Box<Value>),
    /// A record, variant or distinct value. `name` is the record/variant/distinct type name.
    Ctor {
        name: String,
        fields: Vec<Value>,
    },
    List(Vec<Value>),
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
        children: Vec<Value>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalError {
    Unsupported(String),
    RecursionLimit,
}

impl Value {
    /// The canonical form compared by the differential test:
    /// - `Int`: decimal digits (`-` prefix when negative);
    /// - `Double`: `display::js_number_to_string`; `Float`: the same on the value as `f64`;
    /// - `Str`: JSON string; escapes `"`, `\`, `\n`, `\r`, `\t`, and other control characters
    ///   below U+0020 as `\u00XX`; everything else (including non-ASCII) is written as is;
    /// - `Bool`: `true` / `false`; `None`: `null`; `Some(v)`: just `v`;
    /// - `Ctor`: `Nome` when it has no fields, otherwise `Nome(a, b)`;
    /// - `List`: `[a, b]`; `Unit`: `()`;
    /// - `Closure`, `Function`, `Builtin`: `<fun>` (not comparable);
    /// - `Ui`: its HTML, `display::render_html_value`.
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
            Value::Ui { .. } => display::render_html_value(&self.ui_tree()),
        }
    }

    /// The tree `render_html_value` takes. Evaluation only builds well-formed nodes; anything
    /// else is shown as its canonical form (an argument) or as nothing (a child).
    fn ui_tree(&self) -> UiTree {
        let Value::Ui {
            node,
            args,
            children,
        } = self
        else {
            return UiTree {
                node: UiNode::Empty,
                args: Vec::new(),
                children: Vec::new(),
            };
        };
        UiTree {
            node: *node,
            args: args
                .iter()
                .map(|arg| match arg {
                    Value::Str(text) => UiArg::Str(text.clone()),
                    Value::Int(value) => UiArg::Int(value.to_string()),
                    other => UiArg::Str(other.render()),
                })
                .collect(),
            children: children.iter().map(Value::ui_tree).collect(),
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

/// Evaluates the function `name` on `args` in a thread with a large stack.
pub fn eval_function(
    program: &CoreProgram,
    name: &str,
    args: Vec<Value>,
) -> Result<Value, EvalError> {
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .stack_size(STACK_SIZE)
            .spawn_scoped(scope, || {
                let mut interp = Interp { program, depth: 0 };
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
}

impl Interp<'_> {
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
            other => Err(unsupported(&format!("call of non-function {}", other.render())).into()),
        }
    }

    fn eval(&mut self, expr: &Expr, env: &mut Env) -> Eval {
        match &expr.kind {
            ExprKind::Lit(literal) => Ok(literal_value(literal, &expr.ty)?),
            ExprKind::Local(id) => env
                .get(id)
                .cloned()
                .ok_or_else(|| unsupported(&format!("unbound local {id}")).into()),
            ExprKind::Global { name, .. } => Ok(Value::Function(name.clone())),
            ExprKind::Builtin { name, .. } => Ok(Value::Builtin(name.clone())),
            ExprKind::Environment(name) => {
                Err(unsupported(&format!("environment value `{name}`")).into())
            }
            ExprKind::Ctor { ctor, args, .. } => Ok(Value::Ctor {
                name: ctor.clone(),
                fields: self.eval_all(args, env)?,
            }),
            ExprKind::None => Ok(Value::None),
            ExprKind::Some(inner) => Ok(Value::Some(Box::new(self.eval(inner, env)?))),
            ExprKind::Call { callee, args } => {
                let callee = self.eval(callee, env)?;
                let args = self.eval_all(args, env)?;
                Ok(self.call(callee, args)?)
            }
            ExprKind::ServerCall { function, .. } => {
                Err(unsupported(&format!("server call `{function}`")).into())
            }
            ExprKind::Lambda { params, body } => Ok(Value::Closure {
                params: params.iter().map(|(id, _, _)| *id).collect(),
                body: Arc::new((**body).clone()),
                env: env.clone(),
            }),
            ExprKind::Let {
                local, value, body, ..
            } => {
                let value = self.eval(value, env)?;
                env.insert(*local, value);
                self.eval(body, env)
            }
            ExprKind::Seq { value, body } => {
                self.eval(value, env)?;
                self.eval(body, env)
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => match self.eval(condition, env)? {
                Value::Bool(true) => self.eval(then_branch, env),
                Value::Bool(false) => self.eval(else_branch, env),
                other => {
                    Err(unsupported(&format!("non-Boolean condition {}", other.render())).into())
                }
            },
            ExprKind::Match { scrutinee, arms } => {
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
            ExprKind::Field { target, name } => {
                let target = self.eval(target, env)?;
                Ok(self.field(&target, name)?)
            }
            ExprKind::Prim { op, args } => {
                let args = self.eval_all(args, env)?;
                Ok(prim(*op, args)?)
            }
            ExprKind::Db { .. } => Err(unsupported("database access").into()),
            ExprKind::Return(value) => Err(Signal::Return(self.eval(value, env)?)),
            ExprKind::Ui {
                node,
                args,
                children,
            } => {
                let args = self.eval_all(args, env)?;
                if let Some(bad) = args
                    .iter()
                    .find(|arg| !matches!(arg, Value::Str(_) | Value::Int(_)))
                {
                    return Err(unsupported(&format!("UI argument {}", bad.render())).into());
                }
                let children = match self.eval(children, env)? {
                    Value::List(items)
                        if items.iter().all(|item| matches!(item, Value::Ui { .. })) =>
                    {
                        items
                    }
                    other => {
                        return Err(unsupported(&format!("UI children {}", other.render())).into());
                    }
                };
                Ok(Value::Ui {
                    node: *node,
                    args,
                    children,
                })
            }
        }
    }

    fn eval_all(&mut self, exprs: &[Expr], env: &mut Env) -> Result<Vec<Value>, Signal> {
        exprs.iter().map(|expr| self.eval(expr, env)).collect()
    }

    fn field(&self, target: &Value, name: &str) -> Result<Value, EvalError> {
        let Value::Ctor { name: ctor, fields } = target else {
            return Err(unsupported(&format!(
                "field `{name}` of {}",
                target.render()
            )));
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

    fn builtin(&mut self, name: &str, args: Vec<Value>) -> Eval {
        let bad = || unsupported(&format!("bad arguments for builtin `{name}`"));
        let mut args = args.into_iter();
        let value = match name {
            "listOf" => Value::List(args.collect()),
            "emptyList" => Value::List(Vec::new()),
            "length" => match args.next() {
                Some(Value::List(items)) => Value::Int(BigInt::from(items.len())),
                _ => return Err(bad().into()),
            },
            "first" => match args.next() {
                Some(Value::List(items)) => option(items.into_iter().next()),
                _ => return Err(bad().into()),
            },
            "get" => match (args.next(), args.next()) {
                (Some(Value::List(items)), Some(Value::Int(index))) => {
                    let item = usize::try_from(&index)
                        .ok()
                        .and_then(|index| items.into_iter().nth(index));
                    option(item)
                }
                _ => return Err(bad().into()),
            },
            "map" => match (args.next(), args.next()) {
                (Some(Value::List(items)), Some(function)) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        out.push(self.call(function.clone(), vec![item])?);
                    }
                    Value::List(out)
                }
                _ => return Err(bad().into()),
            },
            "fold" => match (args.next(), args.next(), args.next()) {
                (Some(Value::List(items)), Some(mut acc), Some(function)) => {
                    for item in items {
                        acc = self.call(function.clone(), vec![acc, item])?;
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
        Literal::Str(text) => Ok(Value::Str(text.clone())),
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
                    .zip(fields)
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
            Value::Str(a.to_string())
        }
        (Prim::DisplayDouble, Some(a @ Value::Double(_)), None)
        | (Prim::DisplayFloat, Some(a @ Value::Float(_)), None) => Value::Str(a.render()),
        (Prim::DisplayBool, Some(Value::Bool(a)), None) => Value::Str(a.to_string()),
        (Prim::StringConcat, Some(Value::Str(a)), Some(Value::Str(b))) => Value::Str(a + &b),
        (Prim::Not, Some(Value::Bool(a)), None) => Value::Bool(!a),
        (Prim::Eq, Some(a), Some(b)) => Value::Bool(equal(&a, &b)?),
        (Prim::NotEq, Some(a), Some(b)) => Value::Bool(!equal(&a, &b)?),
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

fn equal(a: &Value, b: &Value) -> Result<bool, EvalError> {
    Ok(match (a, b) {
        (Value::Int(a), Value::Int(b)) => a == b,
        // IEEE equality: `NaN != NaN`, `-0 == 0` (as the generated Rust and JS).
        (Value::Double(a), Value::Double(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Unit, Value::Unit) | (Value::None, Value::None) => true,
        (Value::None, Value::Some(_)) | (Value::Some(_), Value::None) => false,
        (Value::Some(a), Value::Some(b)) => equal(a, b)?,
        (
            Value::Ctor {
                name: na,
                fields: fa,
            },
            Value::Ctor {
                name: nb,
                fields: fb,
            },
        ) => na == nb && all_equal(fa, fb)?,
        (Value::List(a), Value::List(b)) => all_equal(a, b)?,
        _ => return Err(unsupported("equality on incomparable values")),
    })
}

fn all_equal(a: &[Value], b: &[Value]) -> Result<bool, EvalError> {
    if a.len() != b.len() {
        return Ok(false);
    }
    for (a, b) in a.iter().zip(b) {
        if !equal(a, b)? {
            return Ok(false);
        }
    }
    Ok(true)
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
            fields,
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
        let xs = Value::List(vec![int("1"), int("2"), int("3")]);
        assert_eq!(rendered(source, vec![xs]), "12");
    }

    #[test]
    fn list_builtins_never_panic() {
        let source = "fun f(xs: List<Int>, i: Int) = get(xs, i) ?: -1";
        let xs = || Value::List(vec![int("5"), int("6")]);
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
                vec![Value::List(vec![])]
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
    }

    #[test]
    fn ui_renders_as_html() {
        assert_eq!(
            rendered("fun f() = UI { Div { Text(1 + 1) } }", vec![]),
            "<div class=\"bind-site\"><div>2</div></div>"
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
            "<div class=\"bind-site\"><section class=\"hero\" id=\"top\">\
             <h2 class=\"title\">Hi A &amp; B</h2>true1e+210.10000000149011612&lt;yes&gt;\
             <a href=\"https://x\" class=\"c\" target=\"_blank\" rel=\"noopener noreferrer\">\
             10000000000000000000000</a></section></div>"
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
            Value::List(vec![int("1"), Value::None]).render(),
            "[1, null]"
        );
        assert_eq!(Value::List(vec![]).render(), "[]");
    }

    #[test]
    fn strings_render_as_json() {
        assert_eq!(
            Value::Str("a\"b\\c\nd\te\u{1}ção é 😀".into()).render(),
            "\"a\\\"b\\\\c\\nd\\te\\u0001ção é 😀\""
        );
    }
}
