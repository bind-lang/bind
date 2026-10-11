//! Translation of core IR functions and expressions to Rust.
//!
//! Calling convention: every Bind function is
//! `pub fn bind_<name><T…>(ctx: Ctx, p…) -> BoxFuture<R>`, whose future enters one level of
//! call depth (`ctx.enter()?`) and evaluates the body. Lambdas are
//! `Arc<dyn Fn(Ctx, A…) -> BoxFuture<R> + Send + Sync>` whose futures also enter one level;
//! builtins and the wrappers that turn functions and builtins into values do not count, as in
//! the reference evaluator (`core::eval`).
//!
//! Every expression becomes a Rust expression of the Bind type's representation (errors are
//! propagated with `?`, calls awaited with `.await?`). Locals are `v<id>_<name>`; reading one
//! clones it (`Copy` representations are copied). Places (locals and their non-boxed fields)
//! are borrowed instead of cloned where the operation only needs a reference.

use std::collections::{BTreeMap, BTreeSet};

use crate::core::ir::{Arm, CoreFunction, DbOp, Expr, ExprKind, Literal, LocalId, Prim};
use crate::types::exhaustive::Pat;
use crate::types::ty::Type;

use super::db::{DbFunction, DbKey};
use super::types::{
    ARC, BOX, CtorDef, CtorKind, DISTINCT_FIELD, Layout, OPTION, PHANTOM_FIELD, PHANTOM_VARIANT,
    RESULT, RUNTIME, STRING, field_ident,
};

/// One function in the uniform calling convention.
#[cfg(test)]
pub fn function_item(function: &CoreFunction, program: &crate::core::ir::CoreProgram) -> String {
    super::indent(&function_with(function, &Layout::new(program)))
}

#[cfg(test)]
pub(super) fn function_with(function: &CoreFunction, layout: &Layout) -> String {
    static NO_DATABASE: BTreeMap<DbKey, DbFunction> = BTreeMap::new();
    function_with_db(function, layout, &NO_DATABASE)
}

/// [`function_with`] for server code: database operations call the functions of `db`.
pub(super) fn function_with_db(
    function: &CoreFunction,
    layout: &Layout,
    db: &BTreeMap<DbKey, DbFunction>,
) -> String {
    let prefix = generic_prefix(layout);
    let generics: BTreeMap<usize, String> = function
        .generics
        .iter()
        .enumerate()
        .map(|(index, var)| (*var, format!("{prefix}{index}")))
        .collect();
    let mut names = BTreeMap::new();
    for (local, name, _) in &function.params {
        names.insert(*local, local_name(*local, name));
    }
    collect_names(&function.body, &mut names);
    let mut generator = Generator {
        layout,
        db,
        generics,
        names,
        next_temp: 0,
    };
    let bounds = if function.generics.is_empty() {
        String::new()
    } else {
        let list: Vec<String> = (0..function.generics.len())
            .map(|index| {
                format!(
                    "{prefix}{index}: ::std::clone::Clone + ::std::marker::Send + ::std::marker::Sync + 'static"
                )
            })
            .collect();
        format!("<{}>", list.join(", "))
    };
    let mut params = vec![format!("ctx: {RUNTIME}::Ctx")];
    for (local, _, ty) in &function.params {
        params.push(format!("{}: {}", generator.local(*local), generator.ty(ty)));
    }
    let body = generator.future_body(&function.body);
    format!(
        "pub fn bind_{}{bounds}({}) -> {RUNTIME}::BoxFuture<{}> {{\n{BOX}::pin(async move {{\n{body}\n}})\n}}\n",
        function.name,
        params.join(", "),
        generator.ty(&function.result),
    )
}

/// A prefix for generic parameters (`T0`, `T1`, …) that no user type name can collide with.
fn generic_prefix(layout: &Layout) -> String {
    let mut prefix = String::from("T");
    let collides = |prefix: &str| {
        layout.program.types.iter().any(|ty| {
            let name = match ty {
                crate::core::ir::CoreType::Record { name, .. }
                | crate::core::ir::CoreType::Sum { name, .. }
                | crate::core::ir::CoreType::Distinct { name, .. } => name,
            };
            name.strip_prefix(prefix)
                .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        })
    };
    while collides(&prefix) {
        prefix.push('T');
    }
    prefix
}

fn local_name(local: LocalId, name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("v{local}_{clean}")
}

/// Names every local bound inside `expr` (lets, pattern variables, lambda parameters).
fn collect_names(expr: &Expr, names: &mut BTreeMap<LocalId, String>) {
    let (_, defs) = locals(expr);
    for (local, name) in defs {
        names.insert(local, local_name(local, &name));
    }
}

/// The locals used in `expr` and the locals it binds (with their names).
fn locals(expr: &Expr) -> (BTreeSet<LocalId>, BTreeMap<LocalId, String>) {
    fn walk(expr: &Expr, uses: &mut BTreeSet<LocalId>, defs: &mut BTreeMap<LocalId, String>) {
        let all = |exprs: &[Expr], uses: &mut BTreeSet<LocalId>, defs: &mut _| {
            for item in exprs {
                walk(item, uses, defs);
            }
        };
        match &expr.kind {
            ExprKind::Local(local) => {
                uses.insert(*local);
            }
            ExprKind::Lit(_)
            | ExprKind::Global { .. }
            | ExprKind::Builtin { .. }
            | ExprKind::Environment(_)
            | ExprKind::None => {}
            ExprKind::Ctor { args, .. }
            | ExprKind::ServerCall { args, .. }
            | ExprKind::Prim { args, .. }
            | ExprKind::Db { args, .. } => all(args, uses, defs),
            ExprKind::Ui { args, children, .. } => {
                all(args, uses, defs);
                walk(children, uses, defs);
            }
            ExprKind::Some(inner) | ExprKind::Return(inner) => walk(inner, uses, defs),
            ExprKind::Field { target, .. } => walk(target, uses, defs),
            ExprKind::Call { callee, args } => {
                walk(callee, uses, defs);
                all(args, uses, defs);
            }
            ExprKind::Lambda { params, body } => {
                for (local, name, _) in params {
                    defs.insert(*local, name.clone());
                }
                walk(body, uses, defs);
            }
            ExprKind::Let {
                local,
                name,
                value,
                body,
            } => {
                defs.insert(*local, name.clone());
                walk(value, uses, defs);
                walk(body, uses, defs);
            }
            ExprKind::Seq { value, body } => {
                walk(value, uses, defs);
                walk(body, uses, defs);
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                walk(condition, uses, defs);
                walk(then_branch, uses, defs);
                walk(else_branch, uses, defs);
            }
            ExprKind::Match { scrutinee, arms } => {
                walk(scrutinee, uses, defs);
                for arm in arms {
                    for (local, name, _) in &arm.bindings {
                        defs.insert(*local, name.clone());
                    }
                    walk(&arm.body, uses, defs);
                }
            }
        }
    }
    let mut uses = BTreeSet::new();
    let mut defs = BTreeMap::new();
    walk(expr, &mut uses, &mut defs);
    (uses, defs)
}

/// The type of `local` at a use inside `expr`.
fn local_type(expr: &Expr, local: LocalId) -> Option<Type> {
    if expr.kind == ExprKind::Local(local) {
        return Some(expr.ty.clone());
    }
    let children: Vec<&Expr> = match &expr.kind {
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
    };
    children
        .into_iter()
        .find_map(|child| local_type(child, local))
}

/// Whether a single-line rendered expression has a `{` outside every bracket and string
/// literal: in generated code that is a struct or variant literal (blocks, `if` and `match`
/// always span lines, and closures sit inside call parentheses).
fn has_exterior_struct_literal(code: &str) -> bool {
    let mut depth = 0usize;
    let mut chars = code.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => {
                            chars.next();
                        }
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            '{' if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

/// Rendered Rust expression; `atomic` when it can be a method receiver or operand as is.
struct Code {
    text: String,
    atomic: bool,
}

fn atom(text: impl Into<String>) -> Code {
    Code {
        text: text.into(),
        atomic: true,
    }
}

fn complex(text: impl Into<String>) -> Code {
    Code {
        text: text.into(),
        atomic: false,
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
}

/// How a matched value (or part of it) is reached.
#[derive(Clone)]
struct Access {
    /// An expression of type `&T`.
    reference: String,
    /// A place expression of type `T` (read without moving: compared or copied).
    place: String,
    /// The expression fields are read from (auto-dereferenced).
    base: String,
    /// A `T`-typed expression that clones (or copies) the value.
    owned: String,
}

impl Access {
    /// A place of type `T`: a local, a scrutinee temporary or a field path.
    fn place(place: &str, copy: bool) -> Access {
        Access {
            reference: format!("&{place}"),
            place: place.to_string(),
            base: place.to_string(),
            owned: if copy {
                place.to_string()
            } else {
                format!("{place}.clone()")
            },
        }
    }

    /// A pattern binder: `&T`, or `&Box<T>` when `boxed`.
    fn bound(name: &str, boxed: bool, copy: bool) -> Access {
        if boxed {
            Access {
                reference: format!("&**{name}"),
                place: format!("**{name}"),
                base: name.to_string(),
                owned: if copy {
                    format!("**{name}")
                } else {
                    format!("(**{name}).clone()")
                },
            }
        } else {
            Access {
                reference: name.to_string(),
                place: format!("*{name}"),
                base: name.to_string(),
                owned: if copy {
                    format!("*{name}")
                } else {
                    format!("{name}.clone()")
                },
            }
        }
    }

    /// The field `field` of this value, stored boxed when `boxed`.
    fn project(&self, field: &str, boxed: bool, copy: bool) -> Access {
        let path = format!("{}.{field}", self.base);
        if boxed {
            Access {
                reference: format!("&*{path}"),
                place: format!("*{path}"),
                base: path.clone(),
                owned: if copy {
                    format!("*{path}")
                } else {
                    format!("(*{path}).clone()")
                },
            }
        } else {
            Access::place(&path, copy)
        }
    }
}

type Path = Vec<(String, usize)>;

/// The tests and binders of one `when` arm.
#[derive(Default)]
struct ArmPlan {
    conditions: Vec<String>,
    /// Indices in `conditions` of the `let`s that cannot fail (single-variant sum types).
    irrefutable: BTreeSet<usize>,
    accesses: BTreeMap<Path, Access>,
}

struct Generator<'a, 'p> {
    layout: &'a Layout<'p>,
    db: &'a BTreeMap<DbKey, DbFunction>,
    generics: BTreeMap<usize, String>,
    names: BTreeMap<LocalId, String>,
    next_temp: usize,
}

impl Generator<'_, '_> {
    fn ty(&self, ty: &Type) -> String {
        self.layout.ty(ty, &self.generics)
    }

    fn local(&self, local: LocalId) -> String {
        self.names
            .get(&local)
            .cloned()
            .unwrap_or_else(|| format!("v{local}"))
    }

    fn temp(&mut self, prefix: &str) -> String {
        let name = format!("{prefix}{}", self.next_temp);
        self.next_temp += 1;
        name
    }

    /// The statements of a future: `let ctx = ctx.enter()?;`, the body, `Ok(result)`.
    fn future_body(&mut self, body: &Expr) -> String {
        let (mut statements, tail) = self.statements(body);
        statements.insert(0, "let ctx = ctx.enter()?;".to_string());
        statements.push(match &tail {
            Tail::Return(code) | Tail::Value(code) => ok(code),
        });
        statements.join("\n")
    }

    /// Flattens `Let`/`Seq` chains into statements and a final expression.
    fn statements(&mut self, expr: &Expr) -> (Vec<String>, Tail) {
        let mut statements = Vec::new();
        let mut current = expr;
        loop {
            match &current.kind {
                ExprKind::Let {
                    local, value, body, ..
                } => {
                    // Annotated, so a diverging value (`return`) still gives the local a type.
                    let ty = match value.kind {
                        ExprKind::Return(_) => local_type(body, *local),
                        _ => Some(value.ty.clone()),
                    };
                    let annotation =
                        ty.map_or_else(String::new, |ty| format!(": {}", self.ty(&ty)));
                    let value = self.expr(value);
                    statements.push(format!(
                        "let {}{annotation} = {};",
                        self.local(*local),
                        value.text
                    ));
                    current = body;
                }
                ExprKind::Seq { value, body } => {
                    let value = self.expr(value);
                    statements.push(format!("let _ = {};", value.text));
                    current = body;
                }
                ExprKind::Return(value) => {
                    let value = self.expr(value);
                    return (statements, Tail::Return(value));
                }
                _ => {
                    let tail = self.expr(current);
                    return (statements, Tail::Value(tail));
                }
            }
        }
    }

    /// `expr` as the contents of a `{ … }` block (without the braces).
    fn block_contents(&mut self, expr: &Expr, prefix: Vec<String>) -> String {
        let (statements, tail) = self.statements(expr);
        let mut lines = prefix;
        lines.extend(statements);
        lines.push(tail.into_text());
        lines.join("\n")
    }

    /// `expr` as a Rust expression; a block when it has statements.
    fn block_expr(&mut self, expr: &Expr) -> Code {
        let (statements, tail) = self.statements(expr);
        if statements.is_empty() {
            return match tail {
                Tail::Return(_) => complex(tail.into_text()),
                Tail::Value(code) => code,
            };
        }
        let mut lines = statements;
        lines.push(tail.into_text());
        complex(format!("{{\n{}\n}}", lines.join("\n")))
    }

    /// A place expression for locals and their non-boxed record fields.
    fn place(&self, expr: &Expr) -> Option<String> {
        match &expr.kind {
            ExprKind::Local(local) => Some(self.local(*local)),
            ExprKind::Field { target, name } => {
                let (def, index) = self.record_field(&target.ty, name)?;
                if self.layout.is_boxed(def.name, index) {
                    return None;
                }
                Some(format!("{}.{}", self.place(target)?, field_ident(name)))
            }
            _ => None,
        }
    }

    fn record_field(&self, record: &Type, name: &str) -> Option<(CtorDef<'_>, usize)> {
        let Type::Con(type_name, _) = record else {
            return None;
        };
        let def = self.layout.ctor(type_name, type_name)?;
        let index = def.fields.iter().position(|(field, _)| *field == name)?;
        Some((def, index))
    }

    /// `&value`, borrowing places instead of cloning them.
    fn borrowed(&mut self, expr: &Expr) -> String {
        match self.place(expr) {
            Some(place) => format!("&{place}"),
            None => format!("&{}", self.expr(expr).wrapped()),
        }
    }

    /// The value as a method receiver or comparison operand (places are not cloned).
    fn operand(&mut self, expr: &Expr) -> String {
        match self.place(expr) {
            Some(place) => place,
            None => self.expr(expr).wrapped(),
        }
    }

    fn owned_args(&mut self, args: &[Expr]) -> Vec<String> {
        args.iter().map(|arg| self.expr(arg).text).collect()
    }

    fn expr(&mut self, expr: &Expr) -> Code {
        match &expr.kind {
            ExprKind::Lit(literal) => atom(self.literal(literal)),
            ExprKind::Local(local) => {
                let name = self.local(*local);
                if self.layout.is_copy(&expr.ty) {
                    atom(name)
                } else {
                    atom(format!("{name}.clone()"))
                }
            }
            ExprKind::Global { name, instance } => complex(format!(
                "{ARC}::new(bind_{name}{}) as {}",
                self.turbofish(instance),
                self.ty(&expr.ty)
            )),
            ExprKind::Builtin { name, instance } => self.builtin_value(name, instance, &expr.ty),
            ExprKind::Db { op, table, args } => self.db(op, table, args),
            ExprKind::Environment(_) | ExprKind::ServerCall { .. } => atom(
                "::std::compile_error!(\"bindc: environment values and server calls are not server code\")",
            ),
            // Rejected by `server_problems` (E8310) before generation; never panics.
            ExprKind::Ui { .. } => {
                atom("::std::compile_error!(\"bindc: UI trees are not server code\")")
            }
            ExprKind::Ctor {
                type_name,
                ctor,
                args,
            } => self.ctor(type_name, ctor, args, &expr.ty),
            ExprKind::None => atom(format!("{OPTION}::<{}>::None", self.option_inner(&expr.ty))),
            ExprKind::Some(inner) => {
                let inner = self.expr(inner);
                atom(format!("{OPTION}::Some({})", inner.text))
            }
            ExprKind::Call { callee, args } => self.call(callee, args),
            ExprKind::Lambda { params, body } => self.lambda(params, body, &expr.ty),
            ExprKind::Let { .. } | ExprKind::Seq { .. } => self.block_expr(expr),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => self.if_expr(condition, then_branch, else_branch),
            ExprKind::Match { scrutinee, arms } => self.match_expr(scrutinee, arms),
            ExprKind::Field { target, name } => self.field(target, name, &expr.ty),
            ExprKind::Prim { op, args } => self.prim(*op, args),
            ExprKind::Return(value) => {
                let value = self.expr(value);
                complex(format!("return {}", ok(&value)))
            }
        }
    }

    /// `if`; the lowering's `&&` (`if a then b else false`) and `||` (`if a then true else b`)
    /// become Rust's short-circuit operators again.
    fn if_expr(&mut self, condition: &Expr, then_branch: &Expr, else_branch: &Expr) -> Code {
        let boolean = |expr: &Expr| match expr.kind {
            ExprKind::Lit(Literal::Bool(value)) => Some(value),
            _ => None,
        };
        match (boolean(then_branch), boolean(else_branch)) {
            (_, Some(false)) => {
                let a = self.expr(condition).wrapped();
                let b = self.expr(then_branch).wrapped();
                return complex(format!("{a} && {b}"));
            }
            (Some(true), _) => {
                let a = self.expr(condition).wrapped();
                let b = self.expr(else_branch).wrapped();
                return complex(format!("{a} || {b}"));
            }
            _ => {}
        }
        let condition = self.expr(condition);
        let then_branch = self.block_contents(then_branch, Vec::new());
        let else_branch = match &else_branch.kind {
            ExprKind::If { .. } => {
                let code = self.expr(else_branch);
                if code.text.starts_with("if ") {
                    // `else if`: the nested `if` is the whole else branch.
                    code.text
                } else {
                    format!("{{\n{}\n}}", code.text)
                }
            }
            _ => format!("{{\n{}\n}}", self.block_contents(else_branch, Vec::new())),
        };
        if condition.text.contains('\n') {
            // A multi-line condition (a block, `match` or `if`) is evaluated first.
            let name = self.temp("c");
            return complex(format!(
                "{{\nlet {name} = {};\nif {name} {{\n{then_branch}\n}} else {else_branch}\n}}",
                condition.text
            ));
        }
        // Rust reads a `{` after `if Name` as the block: a struct literal in the condition
        // (`Par { … }.primeiro > x`) needs parentheses there.
        let condition = if has_exterior_struct_literal(&condition.text) {
            format!("({})", condition.text)
        } else {
            condition.text
        };
        complex(format!(
            "if {condition} {{\n{then_branch}\n}} else {else_branch}"
        ))
    }

    fn option_inner(&self, ty: &Type) -> String {
        match ty {
            Type::Con(name, args) if name == "Option" => match args.as_slice() {
                [inner] => self.ty(inner),
                _ => self.ty(&Type::Error),
            },
            _ => self.ty(&Type::Error),
        }
    }

    fn turbofish(&self, instance: &[Type]) -> String {
        if instance.is_empty() {
            String::new()
        } else {
            let types: Vec<String> = instance.iter().map(|ty| self.ty(ty)).collect();
            format!("::<{}>", types.join(", "))
        }
    }

    fn literal(&self, literal: &Literal) -> String {
        match literal {
            Literal::Int(digits) => match limbs(digits, 10) {
                Some(limbs) => int_literal(false, &limbs),
                None => "::std::compile_error!(\"bindc: invalid integer literal\")".into(),
            },
            Literal::Double(text) => format!("{text}_f64"),
            Literal::Float(text) => format!("{text}_f32"),
            Literal::Str(text) => format!("{STRING}::from({text:?})"),
            Literal::Bool(value) => value.to_string(),
            Literal::Unit => "()".into(),
        }
    }

    fn field(&mut self, target: &Expr, name: &str, ty: &Type) -> Code {
        let ident = field_ident(name);
        let copy = self.layout.is_copy(ty);
        let boxed = self
            .record_field(&target.ty, name)
            .is_some_and(|(def, index)| self.layout.is_boxed(def.name, index));
        let base = self.operand(target);
        let path = format!("{base}.{ident}");
        atom(match (boxed, copy) {
            (false, true) => path,
            (false, false) => format!("{path}.clone()"),
            (true, true) => format!("(*{path})"),
            (true, false) => format!("(*{path}).clone()"),
        })
    }

    fn ctor(&mut self, type_name: &str, ctor: &str, args: &[Expr], ty: &Type) -> Code {
        let Some(def) = self.layout.ctor(type_name, ctor) else {
            return atom("::std::compile_error!(\"bindc: unknown constructor\")");
        };
        let values = self.owned_args(args);
        let type_args = match ty {
            Type::Con(_, args) if !args.is_empty() => self.turbofish(args),
            _ => String::new(),
        };
        let fields = || -> String {
            def.fields
                .iter()
                .zip(&values)
                .enumerate()
                .map(|(index, ((field, _), value))| {
                    let value = if self.layout.is_boxed(def.name, index) {
                        format!("{BOX}::new({value})")
                    } else {
                        value.clone()
                    };
                    format!("{}: {value}", field_ident(field))
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let path = self.layout.type_path(type_name);
        let variant = self.layout.ident(ctor);
        atom(match def.kind {
            CtorKind::Record => {
                let mut fields = fields();
                if self.layout.phantom(type_name).is_some() {
                    if !fields.is_empty() {
                        fields.push_str(", ");
                    }
                    let phantom = self.layout.phantom_name(type_name, PHANTOM_FIELD);
                    fields.push_str(&format!("{phantom}: ::std::marker::PhantomData"));
                }
                format!("{path}{type_args} {{ {fields} }}")
            }
            CtorKind::Variant if def.fields.is_empty() => format!("{path}{type_args}::{variant}"),
            CtorKind::Variant => format!("{path}{type_args}::{variant} {{ {} }}", fields()),
            CtorKind::Distinct => format!("{path} {{ {DISTINCT_FIELD}: {} }}", values.join(", ")),
            CtorKind::BindResult => {
                format!(
                    "{RUNTIME}::BindResult{type_args}::{ctor}({})",
                    values.join(", ")
                )
            }
        })
    }

    fn call(&mut self, callee: &Expr, args: &[Expr]) -> Code {
        match &callee.kind {
            ExprKind::Global { name, instance } => {
                let mut values = vec!["ctx.clone()".to_string()];
                values.extend(self.owned_args(args));
                atom(format!(
                    "bind_{name}{}({}).await?",
                    self.turbofish(instance),
                    values.join(", ")
                ))
            }
            ExprKind::Builtin { name, instance } => self.builtin_call(name, instance, args),
            _ => {
                let function = match self.place(callee) {
                    Some(place) => format!("({place})"),
                    None => self.expr(callee).wrapped(),
                };
                let mut values = vec!["ctx.clone()".to_string()];
                values.extend(self.owned_args(args));
                atom(format!("{function}({}).await?", values.join(", ")))
            }
        }
    }

    /// A database operation: a call to its function in the server's database layer, with the
    /// arguments evaluated first (the function takes the transaction only after that).
    fn db(&mut self, op: &DbOp, table: &str, args: &[Expr]) -> Code {
        let db = self.db;
        let Some(function) = db.get(&DbKey::of(op, table)) else {
            return atom("::std::compile_error!(\"bindc: database operation outside the server\")");
        };
        let mut values = vec!["ctx.clone()".to_string()];
        for (arg, param) in args.iter().zip(&function.params) {
            let value = self.expr(arg).text;
            // A non-optional value assigned to an optional column.
            let lifted = matches!(param, Type::Con(name, inner) if name == "Option" && inner.first() == Some(&arg.ty));
            values.push(if lifted {
                format!("{OPTION}::Some({value})")
            } else {
                value
            });
        }
        // The database layer is at the crate root, outside the program module.
        atom(format!(
            "crate::{}({}).await?",
            function.name,
            values.join(", ")
        ))
    }

    fn builtin_call(&mut self, name: &str, instance: &[Type], args: &[Expr]) -> Code {
        let element = instance
            .first()
            .map_or_else(|| self.ty(&Type::Error), |ty| self.ty(ty));
        match (name, args) {
            ("first", [list]) => atom(format!("{RUNTIME}::first({})", self.borrowed(list))),
            ("get", [list, index]) => {
                let list = self.borrowed(list);
                let index = self.borrowed(index);
                atom(format!("{RUNTIME}::get({list}, {index})"))
            }
            ("length", [list]) => atom(format!("{RUNTIME}::length({})", self.borrowed(list))),
            ("emptyList", []) => atom(format!("{RUNTIME}::empty_list::<{element}>()")),
            ("listOf", items) => {
                let items = self.owned_args(items);
                atom(format!(
                    "{RUNTIME}::list_of::<{element}>(::std::vec![{}])",
                    items.join(", ")
                ))
            }
            ("map" | "fold", items) => {
                let mut values = vec!["ctx.clone()".to_string()];
                values.extend(self.owned_args(items));
                atom(format!("{RUNTIME}::{name}({}).await?", values.join(", ")))
            }
            _ => atom("::std::compile_error!(\"bindc: unknown builtin call\")"),
        }
    }

    /// A builtin used as a value: a closure that applies it (without entering a call level).
    fn builtin_value(&mut self, name: &str, instance: &[Type], ty: &Type) -> Code {
        let Type::Fun(params, result, _) = ty else {
            return atom("::std::compile_error!(\"bindc: builtin value without a function type\")");
        };
        let names: Vec<String> = (0..params.len()).map(|index| format!("a{index}")).collect();
        let element = instance
            .first()
            .map_or_else(|| self.ty(&Type::Error), |ty| self.ty(ty));
        let ready = |value: String| format!("{BOX}::pin(async move {{ {RESULT}::Ok({value}) }})");
        let body = match (name, names.as_slice()) {
            ("first", [list]) => ready(format!("{RUNTIME}::first(&{list})")),
            ("get", [list, index]) => ready(format!("{RUNTIME}::get(&{list}, &{index})")),
            ("length", [list]) => ready(format!("{RUNTIME}::length(&{list})")),
            ("emptyList", []) => ready(format!("{RUNTIME}::empty_list::<{element}>()")),
            ("listOf", items) => ready(format!(
                "{RUNTIME}::list_of::<{element}>(::std::vec![{}])",
                items.join(", ")
            )),
            ("map" | "fold", items) => {
                format!("{BOX}::pin({RUNTIME}::{name}(ctx, {}))", items.join(", "))
            }
            _ => "::std::compile_error!(\"bindc: unknown builtin\")".to_string(),
        };
        let mut inputs = vec![format!("ctx: {RUNTIME}::Ctx")];
        for (name, param) in names.iter().zip(params) {
            inputs.push(format!("{name}: {}", self.ty(param)));
        }
        complex(format!(
            "{ARC}::new(move |{}| -> {RUNTIME}::BoxFuture<{}> {{\n{body}\n}}) as {}",
            inputs.join(", "),
            self.ty(result),
            self.ty(ty)
        ))
    }

    fn lambda(&mut self, params: &[(LocalId, String, Type)], body: &Expr, ty: &Type) -> Code {
        let result = match ty {
            Type::Fun(_, result, _) => self.ty(result),
            _ => self.ty(&Type::Error),
        };
        let (uses, defs) = locals(body);
        let captures: Vec<String> = uses
            .iter()
            .filter(|local| !defs.contains_key(local))
            .filter(|local| !params.iter().any(|(param, _, _)| param == *local))
            // `Copy` values are copied into the closure and its future by `move` already.
            .filter(|local| local_type(body, **local).is_none_or(|ty| !self.layout.is_copy(&ty)))
            .map(|local| self.local(*local))
            .collect();
        let clones: Vec<String> = captures
            .iter()
            .map(|name| format!("let {name} = {name}.clone();"))
            .collect();
        let mut inputs = vec![format!("ctx: {RUNTIME}::Ctx")];
        for (local, _, param) in params {
            inputs.push(format!("{}: {}", self.local(*local), self.ty(param)));
        }
        let future = self.future_body(body);
        let mut inner = clones.clone();
        inner.push(format!("{BOX}::pin(async move {{\n{future}\n}})"));
        let closure = format!(
            "{ARC}::new(move |{}| -> {RUNTIME}::BoxFuture<{result}> {{\n{}\n}}) as {}",
            inputs.join(", "),
            inner.join("\n"),
            self.ty(ty)
        );
        if clones.is_empty() {
            complex(closure)
        } else {
            complex(format!("{{\n{}\n{closure}\n}}", clones.join("\n")))
        }
    }

    fn prim(&mut self, op: Prim, args: &[Expr]) -> Code {
        use Prim::*;
        match (op, args) {
            (IntAdd | IntSub | IntMul | IntDivChecked | IntRemChecked, [a, b]) => {
                let method = match op {
                    IntAdd => "add",
                    IntSub => "sub",
                    IntMul => "mul",
                    IntDivChecked => "div_checked",
                    _ => "rem_checked",
                };
                let a = self.operand(a);
                let b = self.borrowed(b);
                atom(format!("{a}.{method}({b})"))
            }
            (IntNeg, [a]) => atom(format!("{}.neg()", self.operand(a))),
            (
                DoubleAdd | DoubleSub | DoubleMul | DoubleDiv | FloatAdd | FloatSub | FloatMul
                | FloatDiv,
                [a, b],
            ) => {
                let symbol = match op {
                    DoubleAdd | FloatAdd => "+",
                    DoubleSub | FloatSub => "-",
                    DoubleMul | FloatMul => "*",
                    _ => "/",
                };
                let a = self.operand(a);
                let b = self.operand(b);
                complex(format!("{a} {symbol} {b}"))
            }
            (DoubleNeg | FloatNeg, [a]) => complex(format!("-{}", self.operand(a))),
            (DecimalAdd | DecimalSub | DecimalMul | DecimalDivChecked, [a, b]) => {
                let function = match op {
                    DecimalAdd => "decimal_add",
                    DecimalSub => "decimal_sub",
                    DecimalMul => "decimal_mul",
                    _ => "decimal_div_checked",
                };
                let a = self.borrowed(a);
                let b = self.borrowed(b);
                atom(format!("{RUNTIME}::{function}({a}, {b})?"))
            }
            (DecimalNeg, [a]) => atom(format!("{RUNTIME}::decimal_neg({})?", self.borrowed(a))),
            (DurationAdd | DurationSub, [a, b]) => {
                let function = if op == DurationAdd {
                    "duration_add"
                } else {
                    "duration_sub"
                };
                let a = self.borrowed(a);
                let b = self.borrowed(b);
                atom(format!("{RUNTIME}::{function}({a}, {b})?"))
            }
            (DurationNeg, [a]) => atom(format!("{RUNTIME}::duration_neg({})?", self.borrowed(a))),
            (StringConcat, [a, b]) => {
                let a = self.expr(a);
                // A literal right operand is appended as a `&str`, without allocating.
                let b = match &b.kind {
                    ExprKind::Lit(Literal::Str(text)) => format!("{text:?}"),
                    _ => self.borrowed(b),
                };
                complex(format!("{} + {b}", a.wrapped()))
            }
            (Eq | NotEq | Lt | LtEq | Gt | GtEq, [a, b]) => {
                if is_unit(&a.ty) {
                    // `()` has a single value; operands are still evaluated for their effects.
                    let a = self.expr(a);
                    let b = self.expr(b);
                    let value = matches!(op, Eq | LtEq | GtEq);
                    return complex(format!(
                        "{{\nlet _ = {};\nlet _ = {};\n{value}\n}}",
                        a.text, b.text
                    ));
                }
                if matches!(op, Eq | NotEq) {
                    // A comparison with `null` asks whether the other operand is absent.
                    let method = if op == Eq { "is_none" } else { "is_some" };
                    match (&a.kind, &b.kind) {
                        (_, ExprKind::None) => {
                            return atom(format!("{}.{method}()", self.operand(a)));
                        }
                        (ExprKind::None, _) => {
                            return atom(format!("{}.{method}()", self.operand(b)));
                        }
                        _ => {}
                    }
                }
                let symbol = match op {
                    Eq => "==",
                    NotEq => "!=",
                    Lt => "<",
                    LtEq => "<=",
                    Gt => ">",
                    _ => ">=",
                };
                // A string literal operand is compared as a `&str`, without allocating.
                let literal = |expr: &Expr| match &expr.kind {
                    ExprKind::Lit(Literal::Str(text)) => Some(format!("{text:?}")),
                    _ => None,
                };
                match (literal(a), literal(b)) {
                    (Some(a), Some(b)) => return complex(format!("{a} {symbol} {b}")),
                    (None, Some(b)) => {
                        let a = self.operand(a);
                        return complex(format!("{a}.as_str() {symbol} {b}"));
                    }
                    (Some(a), None) => {
                        let b = self.operand(b);
                        return complex(format!("{a} {symbol} {b}.as_str()"));
                    }
                    _ => {}
                }
                let a = self.operand(a);
                let b = self.operand(b);
                complex(format!("{a} {symbol} {b}"))
            }
            (Not, [a]) => {
                let a = self.expr(a);
                complex(format!("!{}", a.wrapped()))
            }
            // Only inside `Text` nodes, which `server_problems` rejects (E8310).
            (
                DisplayString | DisplayInt | DisplayLong | DisplayDouble | DisplayFloat
                | DisplayBool | DisplayDecimal | DisplayText,
                _,
            ) => atom("::std::compile_error!(\"bindc: display primitives are not server code\")"),
            _ => atom("::std::compile_error!(\"bindc: malformed primitive\")"),
        }
    }

    // ---- when ----------------------------------------------------------------------------

    fn match_expr(&mut self, scrutinee: &Expr, arms: &[Arm]) -> Code {
        // Arms after an irrefutable `_` can never run (the checker rejects them anyway).
        let end = arms
            .iter()
            .position(|arm| arm.pattern == Pat::Wild)
            .map_or(arms.len(), |index| index + 1);
        let arms = arms.get(..end).unwrap_or(arms);
        let copy = self.layout.is_copy(&scrutinee.ty);
        let (setup, access) = match self.place(scrutinee) {
            Some(place) => (None, Access::place(&place, copy)),
            None => {
                let name = self.temp("m");
                let value = self.expr(scrutinee);
                (
                    Some(format!("let {name} = {};", value.text)),
                    Access::place(&name, copy),
                )
            }
        };
        let body = if self.is_simple(&scrutinee.ty, arms) {
            self.simple_match(&scrutinee.ty, arms, &access)
        } else {
            self.ladder(&scrutinee.ty, arms, &access)
        };
        match setup {
            Some(setup) => complex(format!("{{\n{setup}\n{body}\n}}")),
            None => complex(body),
        }
    }

    /// Whether every arm is `_` or one constructor of the scrutinee's type applied to `_`s:
    /// then a Rust `match` expresses it directly, with the same exhaustiveness.
    fn is_simple(&self, ty: &Type, arms: &[Arm]) -> bool {
        arms.iter().all(|arm| match &arm.pattern {
            Pat::Wild => true,
            Pat::Lit(_) => false,
            Pat::Ctor(name, subs) => {
                subs.iter().all(|sub| *sub == Pat::Wild)
                    && match ty {
                        Type::Con(type_name, _) if type_name == "Option" => {
                            name == "null" || name == "Some"
                        }
                        Type::Con(type_name, _) if type_name == "Boolean" => {
                            name == "true" || name == "false"
                        }
                        Type::Con(type_name, _) => self.layout.ctor(type_name, name).is_some(),
                        _ => false,
                    }
            }
        })
    }

    fn simple_match(&mut self, ty: &Type, arms: &[Arm], access: &Access) -> String {
        let mut output = format!("match {} {{\n", access.reference);
        for arm in arms {
            let mut binders: BTreeMap<usize, String> = BTreeMap::new();
            let mut lets = Vec::new();
            for ((local, _, binding_ty), path) in arm.bindings.iter().zip(&arm.binding_paths) {
                let copy = self.layout.is_copy(binding_ty);
                let value = match path.as_slice() {
                    [] => access.owned.clone(),
                    [(ctor, index)] => {
                        let name = match binders.get(index) {
                            Some(name) => name.clone(),
                            None => {
                                let name = self.temp("p");
                                binders.insert(*index, name.clone());
                                name
                            }
                        };
                        let boxed = self.is_boxed_in(ty, ctor, *index);
                        Access::bound(&name, boxed, copy).owned
                    }
                    _ => "::std::compile_error!(\"bindc: malformed binding path\")".to_string(),
                };
                lets.push(format!("let {} = {value};", self.local(*local)));
            }
            let pattern = match &arm.pattern {
                Pat::Ctor(name, _) => self.ctor_pattern(ty, name, &binders),
                _ => "_".to_string(),
            };
            let body = self.block_contents(&arm.body, lets);
            output.push_str(&format!("{pattern} => {{\n{body}\n}}\n"));
        }
        if let Type::Con(type_name, _) = ty
            && self.layout.phantom(type_name).is_some()
            && !self
                .layout
                .ctors_of(type_name)
                .iter()
                .any(|def| def.kind == CtorKind::Record)
            && !arms.iter().any(|arm| arm.pattern == Pat::Wild)
        {
            // The hidden variant is uninhabited: the empty `match` needs no arm and no panic.
            output.push_str(&format!(
                "{}::{}(_, never) => match *never {{}},\n",
                self.layout.type_path(type_name),
                self.layout.phantom_name(type_name, PHANTOM_VARIANT)
            ));
        }
        output.push('}');
        output
    }

    fn is_boxed_in(&self, ty: &Type, ctor: &str, index: usize) -> bool {
        match ty {
            Type::Con(type_name, _) => self
                .layout
                .ctor(type_name, ctor)
                .is_some_and(|def| self.layout.is_boxed(def.name, index)),
            _ => false,
        }
    }

    /// A top-level Rust pattern for constructor `name` of `ty`, binding the listed fields.
    fn ctor_pattern(&self, ty: &Type, name: &str, binders: &BTreeMap<usize, String>) -> String {
        let single = || binders.get(&0).cloned().unwrap_or_else(|| "_".to_string());
        let type_name = match ty {
            Type::Con(type_name, _) => type_name.as_str(),
            _ => "",
        };
        match (type_name, name) {
            ("Option", "null") => format!("{OPTION}::None"),
            ("Option", "Some") => format!("{OPTION}::Some({})", single()),
            ("Boolean", value) => value.to_string(),
            _ => match self.layout.ctor(type_name, name) {
                Some(def) => match def.kind {
                    CtorKind::BindResult => format!("{RUNTIME}::BindResult::{name}({})", single()),
                    CtorKind::Distinct => format!(
                        "{} {{ {DISTINCT_FIELD}: {} }}",
                        self.layout.type_path(type_name),
                        single()
                    ),
                    CtorKind::Variant if def.fields.is_empty() => format!(
                        "{}::{}",
                        self.layout.type_path(type_name),
                        self.layout.ident(name)
                    ),
                    CtorKind::Variant | CtorKind::Record => {
                        let prefix = if def.kind == CtorKind::Variant {
                            format!(
                                "{}::{}",
                                self.layout.type_path(type_name),
                                self.layout.ident(name)
                            )
                        } else {
                            self.layout.type_path(type_name)
                        };
                        let mut fields: Vec<String> = binders
                            .iter()
                            .filter_map(|(index, binder)| {
                                def.fields
                                    .get(*index)
                                    .map(|(field, _)| format!("{}: {binder}", field_ident(field)))
                            })
                            .collect();
                        if fields.len() < def.fields.len()
                            || (def.kind == CtorKind::Record
                                && self.layout.phantom(type_name).is_some())
                        {
                            fields.push("..".to_string());
                        }
                        format!("{prefix} {{ {} }}", fields.join(", "))
                    }
                },
                None => "_".to_string(),
            },
        }
    }

    /// A `when` as an `if let … && …` ladder: needed for literals and nested patterns.
    fn ladder(&mut self, ty: &Type, arms: &[Arm], access: &Access) -> String {
        let mut branches: Vec<(Vec<String>, String)> = Vec::new();
        let mut fallback = None;
        for arm in arms {
            let mut needed = BTreeSet::new();
            for path in &arm.binding_paths {
                for end in 0..=path.len() {
                    needed.insert(path.get(..end).unwrap_or_default().to_vec());
                }
            }
            let mut plan = ArmPlan::default();
            self.plan(
                &arm.pattern,
                access.clone(),
                ty,
                Vec::new(),
                &needed,
                &mut plan,
            );
            let lets: Vec<String> = arm
                .bindings
                .iter()
                .zip(&arm.binding_paths)
                .map(|((local, _, _), path)| {
                    let value = plan.accesses.get(path).map_or_else(
                        || "::std::compile_error!(\"bindc: malformed binding path\")".to_string(),
                        |access| access.owned.clone(),
                    );
                    format!("let {} = {value};", self.local(*local))
                })
                .collect();
            if plan.conditions.len() == plan.irrefutable.len() {
                // Only destructuring that cannot fail: the arm always matches.
                let mut statements: Vec<String> = plan
                    .conditions
                    .iter()
                    .map(|condition| format!("{condition};"))
                    .collect();
                statements.extend(lets);
                fallback = Some(self.block_contents(&arm.body, statements));
                break;
            }
            let body = self.block_contents(&arm.body, lets);
            branches.push((plan.conditions, body));
        }
        if branches.is_empty() {
            return format!("{{\n{}\n}}", fallback.unwrap_or_default());
        }
        let fallback = fallback.unwrap_or_else(|| {
            format!(
                "return {RESULT}::Err({RUNTIME}::AppError::Internal({STRING}::from(\"no `when` arm matched\")))"
            )
        });
        let mut output = String::new();
        for (index, (conditions, body)) in branches.iter().enumerate() {
            let keyword = if index == 0 { "if" } else { " else if" };
            output.push_str(&format!(
                "{keyword} {} {{\n{body}\n}}",
                conditions.join(" && ")
            ));
        }
        output.push_str(&format!(" else {{\n{fallback}\n}}"));
        output
    }

    /// Adds the tests of `pattern` against the value at `access` (of type `ty`, at `path`).
    fn plan(
        &mut self,
        pattern: &Pat,
        access: Access,
        ty: &Type,
        path: Path,
        needed: &BTreeSet<Path>,
        plan: &mut ArmPlan,
    ) {
        plan.accesses.insert(path.clone(), access.clone());
        let invalid = || "::std::compile_error!(\"bindc: malformed pattern\")".to_string();
        let type_name = match ty {
            Type::Con(name, _) => name.as_str(),
            _ => "",
        };
        let (name, subs) = match pattern {
            Pat::Wild => return,
            Pat::Lit(text) => {
                let condition = match type_name {
                    "Int" => match int_pattern(text) {
                        Some((negative, limbs)) => {
                            format!("{} == {}", access.place, int_literal(negative, &limbs))
                        }
                        None => invalid(),
                    },
                    // The checker records string patterns as Rust-style debug literals.
                    "String" => format!("{} == {text}", access.place),
                    _ => invalid(),
                };
                plan.conditions.push(condition);
                return;
            }
            Pat::Ctor(name, subs) => (name.as_str(), subs.as_slice()),
        };
        let child = |index: usize| -> Path {
            let mut child = path.clone();
            child.push((name.to_string(), index));
            child
        };
        let is_needed = |sub: &Pat, child: &Path| *sub != Pat::Wild || needed.contains(child);
        match (type_name, name, subs) {
            ("Boolean", "true", []) => plan.conditions.push(access.place.clone()),
            ("Boolean", "false", []) => plan.conditions.push(format!("!{}", access.place)),
            ("Option", "null", []) => plan.conditions.push(format!("{}.is_none()", access.base)),
            ("Option", "Some", [sub]) => {
                let inner = option_arg(ty);
                if !is_needed(sub, &child(0)) {
                    plan.conditions.push(format!("{}.is_some()", access.base));
                    return;
                }
                let binder = self.temp("p");
                plan.conditions.push(format!(
                    "let {OPTION}::Some({binder}) = {}",
                    access.reference
                ));
                let copy = self.layout.is_copy(&inner);
                let next = Access::bound(&binder, false, copy);
                self.plan(sub, next, &inner, child(0), needed, plan);
            }
            _ => {
                let Some(def) = self.layout.ctor(type_name, name) else {
                    plan.conditions.push(invalid());
                    return;
                };
                let field_types = def.field_types(ty);
                match def.kind {
                    CtorKind::Record | CtorKind::Distinct => {
                        for (index, sub) in subs.iter().enumerate() {
                            let child_path = child(index);
                            if !is_needed(sub, &child_path) {
                                continue;
                            }
                            let field_ty = field_types.get(index).cloned().unwrap_or(Type::Error);
                            let field = match def.kind {
                                CtorKind::Distinct => DISTINCT_FIELD.to_string(),
                                _ => def
                                    .fields
                                    .get(index)
                                    .map_or_else(invalid, |(field, _)| field_ident(field)),
                            };
                            let copy = self.layout.is_copy(&field_ty);
                            let boxed = self.layout.is_boxed(def.name, index);
                            let next = access.project(&field, boxed, copy);
                            self.plan(sub, next, &field_ty, child_path, needed, plan);
                        }
                    }
                    CtorKind::Variant | CtorKind::BindResult => {
                        let mut binders = BTreeMap::new();
                        let mut nested = Vec::new();
                        for (index, sub) in subs.iter().enumerate() {
                            let child_path = child(index);
                            if !is_needed(sub, &child_path) {
                                continue;
                            }
                            let binder = self.temp("p");
                            binders.insert(index, binder.clone());
                            nested.push((index, sub, binder, child_path));
                        }
                        let pattern = self.ctor_pattern(ty, name, &binders);
                        let single = self.layout.ctors_of(type_name).len() == 1
                            && self.layout.phantom(type_name).is_none();
                        if binders.is_empty() && single {
                            // Always matches: nothing to test.
                        } else if binders.is_empty() {
                            let pattern = if def.kind == CtorKind::Variant && !def.fields.is_empty()
                            {
                                format!(
                                    "{}::{} {{ .. }}",
                                    self.layout.type_path(type_name),
                                    self.layout.ident(name)
                                )
                            } else {
                                pattern
                            };
                            plan.conditions
                                .push(format!("::std::matches!({}, {pattern})", access.reference));
                        } else {
                            if single {
                                plan.irrefutable.insert(plan.conditions.len());
                            }
                            plan.conditions
                                .push(format!("let {pattern} = {}", access.reference));
                        }
                        for (index, sub, binder, child_path) in nested {
                            let field_ty = field_types.get(index).cloned().unwrap_or(Type::Error);
                            let copy = self.layout.is_copy(&field_ty);
                            let boxed = self.layout.is_boxed(def.name, index);
                            let next = Access::bound(&binder, boxed, copy);
                            self.plan(sub, next, &field_ty, child_path, needed, plan);
                        }
                    }
                }
            }
        }
    }
}

enum Tail {
    Value(Code),
    /// The value of a trailing `return`, emitted as the result itself where possible.
    Return(Code),
}

impl Tail {
    fn into_text(self) -> String {
        match self {
            Tail::Value(code) => code.text,
            Tail::Return(code) => format!("return {}", ok(&code)),
        }
    }
}

/// `Ok(value)`; a value ending in `?` (always a `Result<_, AppError>`) is returned as is.
fn ok(code: &Code) -> String {
    match code.text.strip_suffix('?') {
        Some(result) if code.atomic => result.to_string(),
        _ => format!("{RESULT}::Ok({})", code.text),
    }
}

fn is_unit(ty: &Type) -> bool {
    matches!(ty, Type::Con(name, args) if name == "Unit" && args.is_empty())
}

fn option_arg(ty: &Type) -> Type {
    match ty {
        Type::Con(name, args) if name == "Option" => args.first().cloned().unwrap_or(Type::Error),
        _ => Type::Error,
    }
}

// ---- integer literals --------------------------------------------------------------------

/// Base-2^32 digits (least significant first, no high zeros) of the digits of `text` in
/// `radix`; `None` if a character is not a digit.
pub fn limbs(text: &str, radix: u32) -> Option<Vec<u32>> {
    if text.is_empty() {
        return None;
    }
    let mut limbs: Vec<u32> = Vec::new();
    for ch in text.chars() {
        let mut carry = u64::from(ch.to_digit(radix)?);
        for limb in &mut limbs {
            let value = u64::from(*limb) * u64::from(radix) + carry;
            *limb = (value & 0xFFFF_FFFF) as u32;
            carry = value >> 32;
        }
        if carry > 0 {
            limbs.push(carry as u32);
        }
    }
    while limbs.last() == Some(&0) {
        limbs.pop();
    }
    Some(limbs)
}

/// `Int::from_i64(n)` when the value fits in an `i64`, else `Int::from_parts(neg, &[…])`.
pub fn int_literal(negative: bool, limbs: &[u32]) -> String {
    let small = match limbs {
        [] => Some(0u64),
        [low] => Some(u64::from(*low)),
        [low, high] => Some((u64::from(*high) << 32) | u64::from(*low)),
        _ => None,
    };
    if let Some(magnitude) = small.filter(|value| *value <= i64::MAX as u64) {
        let sign = if negative && magnitude != 0 { "-" } else { "" };
        return format!("{RUNTIME}::Int::from_i64({sign}{magnitude})");
    }
    let digits: Vec<String> = limbs.iter().map(u32::to_string).collect();
    format!(
        "{RUNTIME}::Int::from_parts({negative}, &[{}])",
        digits.join(", ")
    )
}

/// An integer literal pattern as written: optional sign, then decimal, `0x` or `0b` digits,
/// with `_` separators and an optional `l` suffix (as in the reference evaluator).
fn int_pattern(text: &str) -> Option<(bool, Vec<u32>)> {
    let cleaned = text.replace('_', "").to_ascii_lowercase();
    let (negative, rest) = match cleaned.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, cleaned.strip_prefix('+').unwrap_or(&cleaned)),
    };
    let rest = rest.trim_end_matches('l');
    let (digits, radix) = if let Some(hex) = rest.strip_prefix("0x") {
        (hex, 16)
    } else if let Some(bin) = rest.strip_prefix("0b") {
        (bin, 2)
    } else {
        (rest, 10)
    };
    Some((negative, limbs(digits, radix)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_bigint::BigInt;

    fn from_limbs(limbs: &[u32]) -> BigInt {
        limbs.iter().rev().fold(BigInt::from(0), |acc, limb| {
            (acc << 32) + BigInt::from(*limb)
        })
    }

    #[test]
    fn exterior_struct_literals() {
        assert!(has_exterior_struct_literal("P { a: 1 }.a > x"));
        assert!(has_exterior_struct_literal("x == E::V { a: f(\"(\") }"));
        assert!(!has_exterior_struct_literal("f(P { a: 1 }) > x"));
        assert!(!has_exterior_struct_literal(
            "x == ::std::string::String::from(\"{\")"
        ));
        assert!(!has_exterior_struct_literal("v[0] && g(\"\\\"{\")"));
    }

    #[test]
    fn decimal_to_base_2_32() {
        for text in [
            "0",
            "1",
            "4294967295",
            "4294967296",
            "9223372036854775807",
            "9223372036854775808",
            "12345678901234567890123",
            "340282366920938463463374607431768211457",
        ] {
            let limbs = limbs(text, 10).expect("digits");
            assert_eq!(from_limbs(&limbs).to_string(), text);
            assert_ne!(limbs.last(), Some(&0));
        }
        assert_eq!(limbs("12a", 10), None);
        assert_eq!(limbs("", 10), None);
    }

    #[test]
    fn int_literals() {
        let lit = |text: &str| int_literal(false, &limbs(text, 10).expect("digits"));
        assert_eq!(lit("0"), "crate::bind_runtime::Int::from_i64(0)");
        assert_eq!(
            lit("9223372036854775807"),
            "crate::bind_runtime::Int::from_i64(9223372036854775807)"
        );
        assert_eq!(
            lit("9223372036854775808"),
            "crate::bind_runtime::Int::from_parts(false, &[0, 2147483648])"
        );
        assert_eq!(
            lit("12345678901234567890123"),
            "crate::bind_runtime::Int::from_parts(false, &[1900168395, 1119243894, 669])"
        );
    }

    #[test]
    fn int_patterns() {
        let pattern = |text: &str| {
            let (negative, limbs) = int_pattern(text).expect("pattern");
            int_literal(negative, &limbs)
        };
        assert_eq!(pattern("-0"), "crate::bind_runtime::Int::from_i64(0)");
        assert_eq!(pattern("0x10"), "crate::bind_runtime::Int::from_i64(16)");
        assert_eq!(
            pattern("-1_000L"),
            "crate::bind_runtime::Int::from_i64(-1000)"
        );
        assert_eq!(pattern("0b101"), "crate::bind_runtime::Int::from_i64(5)");
        assert_eq!(int_pattern("0xZZ"), None);
    }
}
