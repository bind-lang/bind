//! The core IR validator: an independent check of the invariants code generators rely on.
//! A violation is a compiler bug; `compiler::verify` reports it as E9001.
//!
//! Rules:
//! 1. No node, binding, instance or signature has `Type::Error` or a type variable outside the
//!    function's `generics` (lowering defaults unconstrained variables of monomorphic code to
//!    `Unit`, which is valid).
//! 2. A `Local` appears only inside the scope that declares it, at the type of its binding.
//! 3. `Global`/`ServerCall` name a function of the program, `Builtin` one of the seven builtins,
//!    `Ctor` a constructor of the named type with one argument per field; each at its type.
//! 4. Types agree between each node and its children (`If`, `Prim`, `Call`, `Let`, `Seq`,
//!    `Match`, `Lambda`, `Some`, `Field`, `Return`, and the function body against `result`).
//! 5. Every `Match` is exhaustive.
//! 6. A `Db` outside lambdas needs its label in the function's `effects`; inside a lambda, in
//!    the row of the innermost lambda's function type.
//!
//! Types are compared ignoring effect rows (rule 6 is the only one that reads rows): rows of
//! function types are the checker's and are not specialized by lowering.
//!
//! Divergence: a `Return` leaves the function, so its own `ty` means nothing. An expression that
//! certainly diverges (see `diverges`) is accepted wherever any type is expected; its parts are
//! still checked (the returned value against the function's `result`).

use std::collections::BTreeMap;

use crate::types::env::{BUILTINS, builtin_scheme};
use crate::types::exhaustive::{CtorSig, Signatures, check_match, render};
use crate::types::ty::{Type, display, display_all};

use super::ir::*;
use super::lower::replace_markers;

pub fn validate(program: &CoreProgram) -> Vec<String> {
    let mut validator = Validator {
        functions: program
            .functions
            .iter()
            .map(|function| (function.name.as_str(), function))
            .collect(),
        ctors: constructors(&program.types),
        signatures: CoreSignatures {
            types: program.types.iter().map(|ty| (type_name(ty), ty)).collect(),
        },
        problems: Vec::new(),
        context: String::new(),
        generics: &[],
        result: Type::Error,
    };
    for ty in &program.types {
        validator.declared_type(ty);
    }
    for function in &program.functions {
        validator.function(function);
    }
    validator.problems
}

fn type_name(ty: &CoreType) -> &str {
    match ty {
        CoreType::Record { name, .. }
        | CoreType::Sum { name, .. }
        | CoreType::Distinct { name, .. } => name,
    }
}

/// A constructor: the type it builds, that type's parameters and its fields (with markers).
struct CtorDef<'p> {
    type_name: &'p str,
    params: &'p [String],
    fields: Vec<&'p Type>,
}

impl CtorDef<'_> {
    /// Field types at `args`, the type arguments of the constructed type.
    fn field_types(&self, args: &[Type]) -> Vec<Type> {
        let markers: BTreeMap<String, Type> = self
            .params
            .iter()
            .zip(args)
            .map(|(param, arg)| (format!("'{param}"), arg.clone()))
            .collect();
        self.fields
            .iter()
            .map(|field| replace_markers(field, &markers))
            .collect()
    }
}

fn constructors(types: &[CoreType]) -> BTreeMap<&str, CtorDef<'_>> {
    let mut ctors = BTreeMap::new();
    for ty in types {
        match ty {
            CoreType::Record {
                name,
                params,
                fields,
            } => {
                ctors.insert(
                    name.as_str(),
                    CtorDef {
                        type_name: name,
                        params,
                        fields: fields.iter().map(|(_, ty)| ty).collect(),
                    },
                );
            }
            CoreType::Sum {
                name,
                params,
                variants,
            } => {
                for (variant, fields) in variants {
                    ctors.insert(
                        variant.as_str(),
                        CtorDef {
                            type_name: name,
                            params,
                            fields: fields.iter().map(|(_, ty)| ty).collect(),
                        },
                    );
                }
            }
            CoreType::Distinct { name, underlying } => {
                ctors.insert(
                    name.as_str(),
                    CtorDef {
                        type_name: name,
                        params: &[],
                        fields: vec![underlying],
                    },
                );
            }
        }
    }
    ctors
}

/// Constructor signatures read from `CoreProgram.types`, as the checker's `Env` gives them.
struct CoreSignatures<'p> {
    types: BTreeMap<&'p str, &'p CoreType>,
}

impl Signatures for CoreSignatures<'_> {
    fn constructors(&self, ty: &Type) -> Option<Vec<CtorSig>> {
        let Type::Con(name, args) = ty else {
            return None;
        };
        let at = |params: &[String], fields: &[(String, Type)]| -> Vec<Type> {
            let markers: BTreeMap<String, Type> = params
                .iter()
                .zip(args)
                .map(|(param, arg)| (format!("'{param}"), arg.clone()))
                .collect();
            fields
                .iter()
                .map(|(_, field)| replace_markers(field, &markers))
                .collect()
        };
        match name.as_str() {
            "Boolean" => Some(vec![CtorSig::nullary("true"), CtorSig::nullary("false")]),
            "Option" => Some(vec![
                CtorSig::nullary("null"),
                CtorSig {
                    name: "Some".into(),
                    args: vec![args.first().cloned().unwrap_or(Type::Error)],
                },
            ]),
            _ => match self.types.get(name.as_str())? {
                CoreType::Record {
                    name,
                    params,
                    fields,
                } => Some(vec![CtorSig {
                    name: name.clone(),
                    args: at(params, fields),
                }]),
                CoreType::Sum {
                    params, variants, ..
                } => Some(
                    variants
                        .iter()
                        .map(|(variant, fields)| CtorSig {
                            name: variant.clone(),
                            args: at(params, fields),
                        })
                        .collect(),
                ),
                CoreType::Distinct { name, underlying } => Some(vec![CtorSig {
                    name: name.clone(),
                    args: vec![underlying.clone()],
                }]),
            },
        }
    }
}

/// Structural equality ignoring effect rows. `Type::Error` equals anything: rule 1 reports it
/// once, without cascades.
fn same(a: &Type, b: &Type) -> bool {
    match (a, b) {
        (Type::Error, _) | (_, Type::Error) => true,
        (Type::Var(x), Type::Var(y)) => x == y,
        (Type::Con(x, xs), Type::Con(y, ys)) => {
            x == y && xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| same(x, y))
        }
        (Type::Fun(xs, x, _), Type::Fun(ys, y, _)) => {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| same(x, y)) && same(x, y)
        }
        _ => false,
    }
}

/// Whether evaluating `expr` certainly leaves the function through a `Return`.
fn diverges(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Return(_) => true,
        ExprKind::Let { value, body, .. } | ExprKind::Seq { value, body } => {
            diverges(value) || diverges(body)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => diverges(condition) || (diverges(then_branch) && diverges(else_branch)),
        ExprKind::Match { scrutinee, arms } => {
            diverges(scrutinee) || (!arms.is_empty() && arms.iter().all(|arm| diverges(&arm.body)))
        }
        _ => false,
    }
}

fn option_inner(ty: &Type) -> Option<&Type> {
    match ty {
        Type::Con(name, args) if name == "Option" && args.len() == 1 => args.first(),
        _ => None,
    }
}

/// Replaces `Var(vars[i])` by `instance[i]`.
fn instantiate(ty: &Type, vars: &[usize], instance: &[Type]) -> Type {
    match ty {
        Type::Var(id) => vars
            .iter()
            .position(|var| var == id)
            .and_then(|index| instance.get(index).cloned())
            .unwrap_or(Type::Var(*id)),
        Type::Con(name, args) => Type::Con(
            name.clone(),
            args.iter()
                .map(|arg| instantiate(arg, vars, instance))
                .collect(),
        ),
        Type::Fun(params, result, row) => Type::Fun(
            params
                .iter()
                .map(|param| instantiate(param, vars, instance))
                .collect(),
            Box::new(instantiate(result, vars, instance)),
            row.clone(),
        ),
        Type::Error => Type::Error,
    }
}

fn function_type(function: &CoreFunction) -> Type {
    Type::Fun(
        function
            .params
            .iter()
            .map(|(_, _, ty)| ty.clone())
            .collect(),
        Box::new(function.result.clone()),
        crate::types::effects::Row::empty(),
    )
}

/// How a primitive types its operands and result.
enum Operands {
    /// Every operand has this exact type.
    Exactly(&'static str),
    /// Every operand has the same type, one of these.
    OneOf(&'static [&'static str]),
    /// Every operand has the same type, any type.
    Any,
}

enum Outcome {
    /// The operand type.
    Same,
    /// The operand type wrapped in `Option` (checked division).
    Checked,
    Boolean,
}

fn prim_signature(op: Prim) -> (usize, Operands, Outcome) {
    use Operands::*;
    use Outcome::*;
    use Prim::*;
    const DECIMAL: &[&str] = &["Decimal", "Money"];
    match op {
        IntAdd | IntSub | IntMul => (2, Exactly("Int"), Same),
        IntNeg => (1, Exactly("Int"), Same),
        IntDivChecked | IntRemChecked => (2, Exactly("Int"), Checked),
        DoubleAdd | DoubleSub | DoubleMul | DoubleDiv => (2, Exactly("Double"), Same),
        DoubleNeg => (1, Exactly("Double"), Same),
        FloatAdd | FloatSub | FloatMul | FloatDiv => (2, Exactly("Float"), Same),
        FloatNeg => (1, Exactly("Float"), Same),
        DecimalAdd | DecimalSub | DecimalMul => (2, OneOf(DECIMAL), Same),
        DecimalDivChecked => (2, OneOf(DECIMAL), Checked),
        DecimalNeg => (1, OneOf(DECIMAL), Same),
        DurationAdd | DurationSub => (2, Exactly("Duration"), Same),
        DurationNeg => (1, Exactly("Duration"), Same),
        StringConcat => (2, Exactly("String"), Same),
        Eq | NotEq | Lt | LtEq | Gt | GtEq => (2, Any, Boolean),
        Not => (1, Exactly("Boolean"), Same),
    }
}

/// A local in scope: its id and the type of its binding.
type Scope = Vec<(LocalId, Type)>;

struct Validator<'p> {
    functions: BTreeMap<&'p str, &'p CoreFunction>,
    ctors: BTreeMap<&'p str, CtorDef<'p>>,
    signatures: CoreSignatures<'p>,
    problems: Vec<String>,
    /// `in function `f``, prefixed to every problem.
    context: String,
    generics: &'p [usize],
    result: Type,
}

impl<'p> Validator<'p> {
    fn problem(&mut self, message: String) {
        self.problems.push(format!("{}: {message}", self.context));
    }

    fn mismatch(&mut self, what: &str, expected: &Type, found: &Type) {
        let shown = display_all(&[expected, found]);
        self.problem(format!(
            "type mismatch in {what}: expected {}, found {}",
            shown[0], shown[1]
        ));
    }

    /// `found` must be `expected`, unless the expression diverges.
    fn expect(&mut self, what: &str, expr: &Expr, expected: &Type) {
        if !same(&expr.ty, expected) && !diverges(expr) {
            self.mismatch(what, expected, &expr.ty);
        }
    }

    fn declared_type(&mut self, ty: &CoreType) {
        self.context = format!("in type `{}`", type_name(ty));
        let error = match ty {
            CoreType::Record { fields, .. } => fields.iter().any(|(_, ty)| ty.contains_error()),
            CoreType::Sum { variants, .. } => variants
                .iter()
                .flat_map(|(_, fields)| fields)
                .any(|(_, ty)| ty.contains_error()),
            CoreType::Distinct { underlying, .. } => underlying.contains_error(),
        };
        if error {
            self.problem("a field has an error type".into());
        }
    }

    /// Rule 1 for one type.
    fn closed(&mut self, what: &str, ty: &Type) {
        if ty.contains_error() {
            self.problem(format!("{what} has an error type"));
            return;
        }
        let mut vars = Vec::new();
        super::lower::type_vars(ty, &mut vars);
        if let Some(var) = vars.iter().find(|var| !self.generics.contains(var)) {
            self.problem(format!(
                "{what} has a free type variable (#{var}) outside the function's generics: {}",
                display(ty)
            ));
        }
    }

    fn function(&mut self, function: &'p CoreFunction) {
        self.context = format!("in function `{}`", function.name);
        self.generics = &function.generics;
        self.result = function.result.clone();
        let mut scope = Scope::new();
        for (local, name, ty) in &function.params {
            self.closed(&format!("parameter `{name}`"), ty);
            scope.push((*local, ty.clone()));
        }
        self.closed("the result", &function.result);
        self.expr(&function.body, &mut scope, &mut Vec::new(), function);
        self.expect("the body", &function.body, &function.result);
    }

    fn bind(&mut self, scope: &mut Scope, bindings: &[(LocalId, String, Type)]) {
        for (local, name, ty) in bindings {
            self.closed(&format!("binding `{name}`"), ty);
            scope.push((*local, ty.clone()));
        }
    }

    /// `lambdas` holds the types of the enclosing lambdas, innermost last.
    fn expr(
        &mut self,
        expr: &'p Expr,
        scope: &mut Scope,
        lambdas: &mut Vec<&'p Type>,
        function: &'p CoreFunction,
    ) {
        self.closed(&format!("a `{}` node", kind_name(&expr.kind)), &expr.ty);
        match &expr.kind {
            ExprKind::Lit(literal) => {
                let expected = match literal {
                    Literal::Bool(_) => Some("Boolean"),
                    Literal::Str(_) => Some("String"),
                    Literal::Unit => Some("Unit"),
                    _ => None,
                };
                if let Some(name) = expected
                    && !same(&expr.ty, &Type::con(name))
                {
                    self.mismatch("a literal", &Type::con(name), &expr.ty);
                }
            }
            ExprKind::Local(local) => match scope.iter().rev().find(|(other, _)| other == local) {
                None => self.problem(format!(
                    "local {local} is used outside the scope that declares it"
                )),
                Some((_, bound)) => {
                    if !same(bound, &expr.ty) {
                        let bound = bound.clone();
                        self.mismatch(&format!("a use of local {local}"), &bound, &expr.ty);
                    }
                }
            },
            ExprKind::Global { name, instance } => {
                self.instances(instance);
                match self.functions.get(name.as_str()).copied() {
                    None => self.problem(format!("unknown function `{name}`")),
                    Some(callee) => {
                        if instance.len() != callee.generics.len() {
                            self.problem(format!(
                                "`{name}` has {} generics but is instantiated with {} types",
                                callee.generics.len(),
                                instance.len()
                            ));
                        } else {
                            let expected =
                                instantiate(&function_type(callee), &callee.generics, instance);
                            if !same(&expected, &expr.ty) {
                                self.mismatch(
                                    &format!("a reference to `{name}`"),
                                    &expected,
                                    &expr.ty,
                                );
                            }
                        }
                    }
                }
            }
            ExprKind::Builtin { name, instance } => {
                self.instances(instance);
                self.builtin(name, instance, &expr.ty);
            }
            ExprKind::Environment(_) | ExprKind::Ui => {}
            ExprKind::Ctor {
                type_name,
                ctor,
                args,
            } => {
                for arg in args {
                    self.expr(arg, scope, lambdas, function);
                }
                self.ctor(type_name, ctor, args, &expr.ty);
            }
            ExprKind::None => {
                if option_inner(&expr.ty).is_none() && expr.ty != Type::Error {
                    self.problem(format!(
                        "`null` has the non-optional type {}",
                        display(&expr.ty)
                    ));
                }
            }
            ExprKind::Some(inner) => {
                self.expr(inner, scope, lambdas, function);
                match option_inner(&expr.ty) {
                    Some(element) => {
                        let element = element.clone();
                        self.expect("a `Some` value", inner, &element);
                    }
                    None => self.problem(format!(
                        "`Some` has the non-optional type {}",
                        display(&expr.ty)
                    )),
                }
            }
            ExprKind::Call { callee, args } => {
                self.expr(callee, scope, lambdas, function);
                for arg in args {
                    self.expr(arg, scope, lambdas, function);
                }
                match &callee.ty {
                    Type::Fun(params, result, _) => {
                        self.arguments("a call", params, args);
                        if !same(result, &expr.ty) {
                            self.mismatch("the result of a call", result, &expr.ty);
                        }
                    }
                    Type::Error => {}
                    other => {
                        self.problem(format!("callee is not a function: {}", display(other)));
                    }
                }
            }
            ExprKind::ServerCall {
                function: name,
                args,
            } => {
                for arg in args {
                    self.expr(arg, scope, lambdas, function);
                }
                match self.functions.get(name.as_str()).copied() {
                    None => self.problem(format!("unknown function `{name}` in a server call")),
                    Some(callee) if callee.generics.is_empty() => {
                        let params: Vec<Type> =
                            callee.params.iter().map(|(_, _, ty)| ty.clone()).collect();
                        self.arguments(&format!("a server call to `{name}`"), &params, args);
                        if !same(&callee.result, &expr.ty) {
                            self.mismatch("the result of a server call", &callee.result, &expr.ty);
                        }
                    }
                    Some(_) => {}
                }
            }
            ExprKind::Lambda { params, body } => {
                let mark = scope.len();
                self.bind(scope, params);
                lambdas.push(&expr.ty);
                self.expr(body, scope, lambdas, function);
                lambdas.pop();
                scope.truncate(mark);
                match &expr.ty {
                    Type::Fun(types, result, _) if types.len() == params.len() => {
                        for ((_, name, param), ty) in params.iter().zip(types) {
                            if !same(param, ty) {
                                self.mismatch(&format!("lambda parameter `{name}`"), ty, param);
                            }
                        }
                        self.expect("a lambda body", body, result);
                    }
                    Type::Error => {}
                    other => self.problem(format!(
                        "a lambda with {} parameters has the type {}",
                        params.len(),
                        display(other)
                    )),
                }
            }
            ExprKind::Let {
                local,
                name,
                value,
                body,
            } => {
                self.expr(value, scope, lambdas, function);
                let mark = scope.len();
                self.bind(scope, &[(*local, name.clone(), value.ty.clone())]);
                self.expr(body, scope, lambdas, function);
                scope.truncate(mark);
                self.expect("a `let` body", body, &expr.ty);
            }
            ExprKind::Seq { value, body } => {
                self.expr(value, scope, lambdas, function);
                self.expr(body, scope, lambdas, function);
                self.expect("a sequence body", body, &expr.ty);
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.expr(condition, scope, lambdas, function);
                self.expr(then_branch, scope, lambdas, function);
                self.expr(else_branch, scope, lambdas, function);
                self.expect("an `if` condition", condition, &Type::con("Boolean"));
                self.expect("the `then` branch", then_branch, &expr.ty);
                self.expect("the `else` branch", else_branch, &expr.ty);
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee, scope, lambdas, function);
                for arm in arms {
                    self.binding_paths(arm, &scrutinee.ty);
                    let mark = scope.len();
                    self.bind(scope, &arm.bindings);
                    self.expr(&arm.body, scope, lambdas, function);
                    scope.truncate(mark);
                    self.expect("a `match` arm", &arm.body, &expr.ty);
                }
                if !scrutinee.ty.contains_error() {
                    let patterns: Vec<_> = arms.iter().map(|arm| arm.pattern.clone()).collect();
                    let report = check_match(&self.signatures, &scrutinee.ty, &patterns);
                    if let Some(missing) = report.missing {
                        self.problem(format!(
                            "a `match` on {} is not exhaustive (missing {})",
                            display(&scrutinee.ty),
                            render(&missing)
                        ));
                    }
                }
            }
            ExprKind::Field { target, name } => {
                self.expr(target, scope, lambdas, function);
                self.field(target, name, &expr.ty);
            }
            ExprKind::Prim { op, args } => {
                for arg in args {
                    self.expr(arg, scope, lambdas, function);
                }
                self.prim(*op, args, &expr.ty);
            }
            ExprKind::Db { op, table, args } => {
                for arg in args {
                    self.expr(arg, scope, lambdas, function);
                }
                let label = match op {
                    DbOp::Require | DbOp::Find => "db.read",
                    DbOp::Insert { .. } => "db.write",
                };
                match lambdas.last() {
                    Some(Type::Fun(_, _, row)) => {
                        if !row.labels.iter().any(|other| other.to_string() == label) {
                            self.problem(format!(
                                "the effect `{label}` of an operation on `{table}` is not in the row of the enclosing lambda"
                            ));
                        }
                    }
                    Some(_) => self.problem(format!(
                        "the effect `{label}` of an operation on `{table}` sits in a lambda without a function type"
                    )),
                    None => {
                        if !function.effects.iter().any(|other| other == label) {
                            self.problem(format!(
                                "the effect `{label}` of an operation on `{table}` is not among the function's effects"
                            ));
                        }
                    }
                }
            }
            ExprKind::Return(value) => {
                self.expr(value, scope, lambdas, function);
                if !lambdas.is_empty() {
                    self.problem("a `return` inside a lambda".into());
                }
                let result = self.result.clone();
                self.expect("a returned value", value, &result);
            }
        }
    }

    fn instances(&mut self, instance: &[Type]) {
        for ty in instance {
            self.closed("an instance type", ty);
        }
    }

    fn arguments(&mut self, what: &str, params: &[Type], args: &[Expr]) {
        if params.len() != args.len() {
            self.problem(format!(
                "{what} passes {} arguments to a function of arity {}",
                args.len(),
                params.len()
            ));
            return;
        }
        for (index, (param, arg)) in params.iter().zip(args).enumerate() {
            self.expect(&format!("argument {index} of {what}"), arg, param);
        }
    }

    fn builtin(&mut self, name: &str, instance: &[Type], ty: &Type) {
        if name == "listOf" {
            let [element] = instance else {
                self.problem(format!(
                    "`listOf` is instantiated with {} types",
                    instance.len()
                ));
                return;
            };
            let list = Type::list(element.clone());
            let fits = matches!(ty, Type::Fun(params, result, _)
                if params.iter().all(|param| same(param, element)) && same(result, &list));
            if !fits {
                self.problem(format!(
                    "`listOf` at element type {} has the type {}",
                    display(element),
                    display(ty)
                ));
            }
            return;
        }
        let Some(scheme) = BUILTINS
            .contains(&name)
            .then(|| builtin_scheme(name))
            .flatten()
        else {
            self.problem(format!("unknown builtin `{name}`"));
            return;
        };
        if instance.len() != scheme.vars {
            self.problem(format!(
                "builtin `{name}` has {} type variables but is instantiated with {} types",
                scheme.vars,
                instance.len()
            ));
            return;
        }
        let vars: Vec<usize> = (0..scheme.vars).collect();
        let expected = instantiate(&scheme.ty, &vars, instance);
        if !same(&expected, ty) {
            self.mismatch(&format!("a reference to builtin `{name}`"), &expected, ty);
        }
    }

    fn ctor(&mut self, type_name: &str, ctor: &str, args: &[Expr], ty: &Type) {
        let Some(def) = self
            .ctors
            .get(ctor)
            .filter(|def| def.type_name == type_name)
        else {
            self.problem(format!(
                "unknown constructor `{ctor}` of type `{type_name}`"
            ));
            return;
        };
        if def.fields.len() != args.len() {
            self.problem(format!(
                "constructor `{ctor}` takes {} arguments, got {}",
                def.fields.len(),
                args.len()
            ));
            return;
        }
        let type_args = match ty {
            Type::Con(name, type_args)
                if name == type_name && type_args.len() == def.params.len() =>
            {
                type_args
            }
            Type::Error => return,
            _ => {
                self.problem(format!(
                    "constructor `{ctor}` of `{type_name}` builds a value of type {}",
                    display(ty)
                ));
                return;
            }
        };
        let fields = def.field_types(type_args);
        for (index, (field, arg)) in fields.iter().zip(args).enumerate() {
            self.expect(
                &format!("argument {index} of constructor `{ctor}`"),
                arg,
                field,
            );
        }
    }

    /// The field types of `ctor` at `ty`, when `ty` is the constructed type.
    fn ctor_fields(&self, ctor: &str, ty: &Type) -> Option<Vec<Type>> {
        let def = self.ctors.get(ctor)?;
        match ty {
            Type::Con(name, args) if name == def.type_name && args.len() == def.params.len() => {
                Some(def.field_types(args))
            }
            _ => None,
        }
    }

    /// Each binding's type is the type reached by following its path from the scrutinee.
    fn binding_paths(&mut self, arm: &Arm, scrutinee: &Type) {
        if arm.bindings.len() != arm.binding_paths.len() {
            self.problem(format!(
                "a `match` arm has {} bindings but {} binding paths",
                arm.bindings.len(),
                arm.binding_paths.len()
            ));
            return;
        }
        for ((_, name, ty), path) in arm.bindings.iter().zip(&arm.binding_paths) {
            let mut current = scrutinee.clone();
            for (ctor, index) in path {
                let next = match option_inner(&current) {
                    Some(inner) if ctor == "Some" && *index == 0 => Some(inner.clone()),
                    _ => self
                        .ctor_fields(ctor, &current)
                        .and_then(|fields| fields.get(*index).cloned()),
                };
                match next {
                    Some(next) => current = next,
                    None => {
                        self.problem(format!(
                            "the path of binding `{name}` does not fit {}",
                            display(scrutinee)
                        ));
                        return;
                    }
                }
            }
            if !same(&current, ty) {
                self.mismatch(&format!("binding `{name}`"), &current, ty);
            }
        }
    }

    /// A field of a record, at the record's type arguments. Fields of other types are not
    /// checked here.
    fn field(&mut self, target: &Expr, name: &str, ty: &Type) {
        let Type::Con(type_name, args) = &target.ty else {
            return;
        };
        let Some(CoreType::Record { params, fields, .. }) =
            self.signatures.types.get(type_name.as_str()).copied()
        else {
            return;
        };
        let markers: BTreeMap<String, Type> = params
            .iter()
            .zip(args)
            .map(|(param, arg)| (format!("'{param}"), arg.clone()))
            .collect();
        match fields.iter().find(|(field, _)| field == name) {
            None => self.problem(format!("unknown field `{name}` of `{type_name}`")),
            Some((_, field)) => {
                let expected = replace_markers(field, &markers);
                if !same(&expected, ty) {
                    self.mismatch(&format!("field `{name}`"), &expected, ty);
                }
            }
        }
    }

    fn prim(&mut self, op: Prim, args: &[Expr], ty: &Type) {
        let (arity, operands, outcome) = prim_signature(op);
        if args.len() != arity {
            self.problem(format!(
                "`{op:?}` takes {arity} operands, got {}",
                args.len()
            ));
            return;
        }
        // The operand type: fixed by the operator, or the first operand that does not diverge.
        let operand = match operands {
            Operands::Exactly(name) => Some(Type::con(name)),
            Operands::OneOf(_) | Operands::Any => args
                .iter()
                .find(|arg| !diverges(arg))
                .map(|arg| arg.ty.clone()),
        };
        let Some(operand) = operand else {
            return;
        };
        if let Operands::OneOf(allowed) = operands
            && !matches!(&operand, Type::Con(name, args) if args.is_empty() && allowed.contains(&name.as_str()))
            && operand != Type::Error
        {
            self.problem(format!(
                "`{op:?}` has an operand of type {}",
                display(&operand)
            ));
            return;
        }
        for (index, arg) in args.iter().enumerate() {
            self.expect(&format!("operand {index} of `{op:?}`"), arg, &operand);
        }
        let expected = match outcome {
            Outcome::Same => operand,
            Outcome::Checked => Type::option(operand),
            Outcome::Boolean => Type::con("Boolean"),
        };
        if !same(&expected, ty) {
            self.mismatch(&format!("the result of `{op:?}`"), &expected, ty);
        }
    }
}

fn kind_name(kind: &ExprKind) -> &'static str {
    match kind {
        ExprKind::Lit(_) => "Lit",
        ExprKind::Local(_) => "Local",
        ExprKind::Global { .. } => "Global",
        ExprKind::Builtin { .. } => "Builtin",
        ExprKind::Environment(_) => "Environment",
        ExprKind::Ctor { .. } => "Ctor",
        ExprKind::None => "None",
        ExprKind::Some(_) => "Some",
        ExprKind::Call { .. } => "Call",
        ExprKind::ServerCall { .. } => "ServerCall",
        ExprKind::Lambda { .. } => "Lambda",
        ExprKind::Let { .. } => "Let",
        ExprKind::Seq { .. } => "Seq",
        ExprKind::If { .. } => "If",
        ExprKind::Match { .. } => "Match",
        ExprKind::Field { .. } => "Field",
        ExprKind::Prim { .. } => "Prim",
        ExprKind::Db { .. } => "Db",
        ExprKind::Return(_) => "Return",
        ExprKind::Ui => "Ui",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::has_errors;
    use crate::schema::DatabaseSchema;
    use crate::types::effects::{Label, Row};
    use crate::types::exhaustive::Pat;
    use crate::types::ty::Type;

    fn core_with(source: &str, schema: &DatabaseSchema) -> CoreProgram {
        let (tokens, mut diagnostics) = crate::lexer::lex("test.bd", source);
        let (program, mut parse_diagnostics) = crate::parser::parse(tokens);
        diagnostics.append(&mut parse_diagnostics);
        assert!(!has_errors(&diagnostics), "{diagnostics:?}");
        let (typed, diagnostics) = crate::types::check(&program, schema);
        assert!(!has_errors(&diagnostics), "{diagnostics:?}");
        super::super::lower::lower(&program, &typed, schema)
    }

    fn core(source: &str) -> CoreProgram {
        core_with(source, &DatabaseSchema::default())
    }

    fn t(name: &str) -> Type {
        Type::con(name)
    }

    fn e(ty: Type, kind: ExprKind) -> Expr {
        Expr { ty, kind }
    }

    fn int(value: &str) -> Expr {
        e(t("Int"), ExprKind::Lit(Literal::Int(value.into())))
    }

    fn local(id: LocalId, ty: Type) -> Expr {
        e(ty, ExprKind::Local(id))
    }

    fn function(
        name: &str,
        params: Vec<(LocalId, &str, Type)>,
        result: Type,
        body: Expr,
    ) -> CoreFunction {
        CoreFunction {
            name: name.into(),
            domain: Domain::Inferred,
            endpoint: false,
            generics: Vec::new(),
            params: params
                .into_iter()
                .map(|(id, name, ty)| (id, name.to_string(), ty))
                .collect(),
            result,
            effects: Vec::new(),
            body,
        }
    }

    fn program(functions: Vec<CoreFunction>) -> CoreProgram {
        CoreProgram {
            types: vec![CoreType::Record {
                name: "P".into(),
                params: Vec::new(),
                fields: vec![("n".into(), t("Int"))],
            }],
            functions,
        }
    }

    /// Validates a single function and asserts one problem mentions `keyword`.
    fn rejects(function: CoreFunction, keyword: &str) {
        let problems = validate(&program(vec![function]));
        assert!(
            problems.iter().any(|problem| problem.contains(keyword)),
            "expected a problem mentioning {keyword:?}, got {problems:?}"
        );
    }

    #[test]
    fn a_lowered_program_is_valid() {
        let source = "sealed class C {\n data object A\n data class B(val n: Int)\n}\nfun f(c: C) = when (c) {\n A -> 0\n B(n) -> n\n}";
        assert_eq!(validate(&core(source)), Vec::<String>::new());
    }

    #[test]
    fn every_ok_golden_case_is_valid() {
        let directory =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/typecheck");
        let mut checked = 0;
        for entry in std::fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|extension| extension != "bd") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            if !source.starts_with("// expect: ok") {
                continue;
            }
            let schema_path = path.with_extension("csv");
            let schema = if schema_path.exists() {
                DatabaseSchema::load(&schema_path).unwrap()
            } else {
                DatabaseSchema::default()
            };
            let problems = validate(&core_with(&source, &schema));
            assert!(problems.is_empty(), "{}: {problems:?}", path.display());
            checked += 1;
        }
        assert!(checked >= 7, "only {checked} golden cases checked");
    }

    #[test]
    fn programs_with_returns_and_lambdas_are_valid() {
        let sources = [
            "fun f(): Int {\n val a = 1\n return a\n}",
            "fun f(x: Int?): Int {\n val y = when (x) {\n null -> return 0\n n -> n\n }\n y + 1\n}",
            "fun f(b: Boolean): Int {\n if (b) return 1 else return 2\n}",
            "fun f(xs: List<Int>) = map(xs, { x -> x * 2 })",
            "fun f(): Int {\n val id = { x -> x }\n val s = id(\"a\")\n id(1)\n}",
            "fun <T> ident(x: T): T = x\nfun f() = ident(1)",
            "fun f(x: Int?) = x ?: 0",
            "data class P(val n: String)\nfun f(p: P?) = p?.n",
            "fun f(r: Result<Int, String>) = when (r) {\n Ok(v) -> v\n Err(e) -> 0\n}",
            "fun f(): Result<Int, String> = Ok(1)",
            "fun f() = listOf(1, 2)",
            "fun h(): Int {\n val e = emptyList()\n 1\n}",
            "fun even(n: Int): Boolean = n == 0 || odd(n - 1)\nfun odd(n: Int): Boolean = n != 0 && even(n - 1)",
            "distinct type Id = UUID\nfun f(i: Id): Id = i",
        ];
        for source in sources {
            assert_eq!(validate(&core(source)), Vec::<String>::new(), "{source}");
        }
    }

    #[test]
    fn rejects_a_non_boolean_condition() {
        let body = e(
            t("Int"),
            ExprKind::If {
                condition: Box::new(int("1")),
                then_branch: Box::new(int("2")),
                else_branch: Box::new(int("3")),
            },
        );
        rejects(function("f", Vec::new(), t("Int"), body), "condition");
    }

    #[test]
    fn rejects_a_local_out_of_scope() {
        rejects(
            function("f", Vec::new(), t("Int"), local(99, t("Int"))),
            "scope",
        );
        // A `let` binding is not visible in its own value.
        let body = e(
            t("Int"),
            ExprKind::Let {
                local: 1,
                name: "x".into(),
                value: Box::new(local(1, t("Int"))),
                body: Box::new(local(1, t("Int"))),
            },
        );
        rejects(function("f", Vec::new(), t("Int"), body), "scope");
    }

    #[test]
    fn rejects_an_unknown_function() {
        let body = e(
            Type::Fun(Vec::new(), Box::new(t("Int")), Row::empty()),
            ExprKind::Global {
                name: "nada".into(),
                instance: Vec::new(),
            },
        );
        let result = Type::Fun(Vec::new(), Box::new(t("Int")), Row::empty());
        rejects(function("f", Vec::new(), result, body), "unknown function");
    }

    #[test]
    fn rejects_a_mistyped_operand() {
        let body = e(
            t("Int"),
            ExprKind::Prim {
                op: Prim::IntAdd,
                args: vec![
                    int("1"),
                    e(t("String"), ExprKind::Lit(Literal::Str("a".into()))),
                ],
            },
        );
        rejects(function("f", Vec::new(), t("Int"), body), "operand");
    }

    #[test]
    fn rejects_a_match_without_its_null_arm() {
        let body = e(
            t("Int"),
            ExprKind::Match {
                scrutinee: Box::new(local(0, Type::option(t("Int")))),
                arms: vec![Arm {
                    pattern: Pat::Ctor("Some".into(), vec![Pat::Wild]),
                    bindings: vec![(1, "n".into(), t("Int"))],
                    binding_paths: vec![vec![("Some".into(), 0)]],
                    body: local(1, t("Int")),
                }],
            },
        );
        let params = vec![(0, "x", Type::option(t("Int")))];
        rejects(function("f", params, t("Int"), body), "exhaustive");
    }

    fn insert() -> Expr {
        e(
            t("P"),
            ExprKind::Db {
                op: DbOp::Insert {
                    columns: vec!["n".into()],
                },
                table: "ps".into(),
                args: vec![int("1")],
            },
        )
    }

    #[test]
    fn rejects_a_database_effect_the_function_lacks() {
        rejects(function("f", Vec::new(), t("P"), insert()), "effect");
        let mut allowed = function("f", Vec::new(), t("P"), insert());
        allowed.effects = vec!["db.write".into()];
        assert_eq!(validate(&program(vec![allowed])), Vec::<String>::new());
    }

    #[test]
    fn effects_inside_a_lambda_follow_the_lambda_row() {
        let lambda = |row: Row| {
            e(
                Type::Fun(Vec::new(), Box::new(t("P")), row),
                ExprKind::Lambda {
                    params: Vec::new(),
                    body: Box::new(insert()),
                },
            )
        };
        let fun_ty = |row: Row| Type::Fun(Vec::new(), Box::new(t("P")), row);
        let write = || Row::closed([Label::new("db", "write")]);
        // Never called: the function itself needs no effect.
        let uncalled = function("f", Vec::new(), fun_ty(write()), lambda(write()));
        assert_eq!(validate(&program(vec![uncalled])), Vec::<String>::new());
        let mut missing = function("f", Vec::new(), fun_ty(Row::empty()), lambda(Row::empty()));
        missing.effects = vec!["db.write".into()];
        rejects(missing, "effect");
    }

    #[test]
    fn rejects_error_types_and_free_variables() {
        rejects(
            function(
                "f",
                Vec::new(),
                t("Unit"),
                e(Type::Error, ExprKind::Lit(Literal::Unit)),
            ),
            "error type",
        );
        rejects(
            function(
                "f",
                vec![(0, "x", Type::Var(7))],
                Type::Var(7),
                local(0, Type::Var(7)),
            ),
            "free type variable",
        );
        let mut generic = function(
            "f",
            vec![(0, "x", Type::Var(7))],
            Type::Var(7),
            local(0, Type::Var(7)),
        );
        generic.generics = vec![7];
        assert_eq!(validate(&program(vec![generic])), Vec::<String>::new());
    }

    #[test]
    fn rejects_mismatched_local_and_call_types() {
        let body = local(0, t("String"));
        rejects(
            function("f", vec![(0, "x", t("Int"))], t("String"), body),
            "local 0",
        );
        let g = function("g", vec![(0, "x", t("Int"))], t("Int"), local(0, t("Int")));
        let g_ty = Type::Fun(vec![t("Int")], Box::new(t("Int")), Row::empty());
        let call = e(
            t("Int"),
            ExprKind::Call {
                callee: Box::new(e(
                    g_ty,
                    ExprKind::Global {
                        name: "g".into(),
                        instance: Vec::new(),
                    },
                )),
                args: vec![e(t("String"), ExprKind::Lit(Literal::Str("a".into())))],
            },
        );
        let f = function("f", Vec::new(), t("Int"), call);
        let problems = validate(&program(vec![g, f]));
        assert!(
            problems.iter().any(|problem| problem.contains("argument")),
            "{problems:?}"
        );
    }

    #[test]
    fn rejects_unknown_constructors_and_wrong_arity() {
        let ctor = |name: &str, args: Vec<Expr>| {
            e(
                t("P"),
                ExprKind::Ctor {
                    type_name: "P".into(),
                    ctor: name.into(),
                    args,
                },
            )
        };
        rejects(
            function("f", Vec::new(), t("P"), ctor("Q", vec![int("1")])),
            "constructor",
        );
        rejects(
            function("f", Vec::new(), t("P"), ctor("P", Vec::new())),
            "argument",
        );
        let ok = function("f", Vec::new(), t("P"), ctor("P", vec![int("1")]));
        assert_eq!(validate(&program(vec![ok])), Vec::<String>::new());
    }

    #[test]
    fn a_return_is_accepted_wherever_a_type_is_expected() {
        // `if (b) return 1 else 2` where the `Return` node kept a defaulted type.
        let body = e(
            t("Int"),
            ExprKind::If {
                condition: Box::new(local(0, t("Boolean"))),
                then_branch: Box::new(e(t("Unit"), ExprKind::Return(Box::new(int("1"))))),
                else_branch: Box::new(int("2")),
            },
        );
        let f = function("f", vec![(0, "b", t("Boolean"))], t("Int"), body);
        assert_eq!(validate(&program(vec![f])), Vec::<String>::new());
        // But the returned value must have the function's result type.
        let body = e(
            t("Unit"),
            ExprKind::Return(Box::new(e(
                t("String"),
                ExprKind::Lit(Literal::Str("a".into())),
            ))),
        );
        rejects(function("f", Vec::new(), t("Int"), body), "return");
    }
}
