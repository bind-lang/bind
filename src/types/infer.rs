//! Hindley-Milner inference with level-based generalization (Rémy), as in OCaml's compiler.
//!
//! Bind is stricter than OCaml: operator constraints must resolve to a concrete type,
//! discarded non-`Unit` values are errors and `when` must be exhaustive.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{Function, Program, TypeKind, TypeRef};
use crate::diagnostic::{Diagnostic, Span};
use crate::parser::expr::parse_type_text;
use crate::schema::{Column, DatabaseSchema};
use crate::syntax::expr::*;

use super::env::{Env, UI_NODES, rigid_vars};
use super::exhaustive::{self, Pat};
use super::ty::*;
use super::unify::{UnifyError, constrain, unify};

/// Level of quantified variables in schemes; never adjusted by unification.
const GENERIC_LEVEL: u32 = u32::MAX;

#[allow(dead_code)] // Consumed by subprojects 2 and 3 (typed IR for verification and code generation).
#[derive(Clone, Debug, Default)]
pub struct TypedProgram {
    /// Rendered type scheme of each function, e.g. `('a) -> 'a`.
    pub schemes: BTreeMap<String, String>,
    /// Resolved type of each function.
    pub signatures: BTreeMap<String, Type>,
    /// Resolved type of each expression and block, keyed by function and node.
    pub node_types: BTreeMap<(String, NodeId), Type>,
}

pub fn check_program(program: &Program, env: &Env, schema: &DatabaseSchema) -> (TypedProgram, Vec<Diagnostic>) {
    let mut checker = Checker::new(env, schema);
    checker.run(program);
    let typed = checker.finish(program);
    (typed, checker.diagnostics)
}

struct Checker<'a> {
    env: &'a Env,
    schema: &'a DatabaseSchema,
    table: TypeTable,
    level: u32,
    globals: BTreeMap<String, Scheme>,
    function_spans: BTreeMap<String, Span>,
    locals: Vec<(String, Scheme)>,
    /// Type parameters in scope: rigid in annotated functions, flexible in inferred ones.
    rigid: BTreeMap<String, Type>,
    function: String,
    return_type: Option<Type>,
    lambda_depth: usize,
    nodes: BTreeMap<(String, NodeId), (Type, Span)>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Checker<'a> {
    fn new(env: &'a Env, schema: &'a DatabaseSchema) -> Self {
        let mut table = TypeTable::default();
        table.set_non_equatable(env.non_equatable.clone());
        Self {
            env,
            schema,
            table,
            level: 0,
            globals: BTreeMap::new(),
            function_spans: BTreeMap::new(),
            locals: Vec::new(),
            rigid: BTreeMap::new(),
            function: String::new(),
            return_type: None,
            lambda_depth: 0,
            nodes: BTreeMap::new(),
            diagnostics: Vec::new(),
        }
    }

    // ---- program level -------------------------------------------------------------

    fn run(&mut self, program: &Program) {
        self.install_builtins();
        self.install_environment(program);
        for function in &program.functions {
            self.function_spans.insert(function.name.clone(), function.span.clone());
        }
        let (annotated, inferred): (Vec<&Function>, Vec<&Function>) =
            program.functions.iter().partition(|function| is_fully_annotated(function));
        for function in &annotated {
            let scheme = self.annotated_scheme(function);
            self.globals.insert(function.name.clone(), scheme);
        }
        for group in components(&inferred) {
            let members: Vec<&Function> = group.iter().map(|&index| inferred[index]).collect();
            self.infer_group(&members);
        }
        for function in annotated {
            self.check_annotated(function);
        }
    }

    fn install_environment(&mut self, program: &Program) {
        for declaration in &program.environment {
            if program.functions.iter().any(|function| function.name == declaration.name) {
                self.error(
                    "E7110",
                    format!("`{}` is declared both as an environment value and as a function.", declaration.name),
                    &declaration.span,
                );
            }
            let base = self.lower_ref(&declaration.type_ref, &declaration.span, &BTreeMap::new(), true);
            let ty = if declaration.optional { Type::option(base) } else { base };
            self.globals.insert(declaration.name.clone(), Scheme::mono(ty));
        }
    }

    fn install_builtins(&mut self) {
        let table = &mut self.table;
        let list = |ty: &Type| Type::list(ty.clone());
        let builtins = [
            ("first", builtin(table, 1, |v| Type::Fun(vec![list(&v[0])], Box::new(Type::option(v[0].clone()))))),
            (
                "get",
                builtin(table, 1, |v| {
                    Type::Fun(vec![list(&v[0]), Type::con("Int")], Box::new(Type::option(v[0].clone())))
                }),
            ),
            ("length", builtin(table, 1, |v| Type::Fun(vec![list(&v[0])], Box::new(Type::con("Int"))))),
            (
                "map",
                builtin(table, 2, |v| {
                    let mapper = Type::Fun(vec![v[0].clone()], Box::new(v[1].clone()));
                    Type::Fun(vec![list(&v[0]), mapper], Box::new(list(&v[1])))
                }),
            ),
            (
                "fold",
                builtin(table, 2, |v| {
                    let step = Type::Fun(vec![v[1].clone(), v[0].clone()], Box::new(v[1].clone()));
                    Type::Fun(vec![list(&v[0]), v[1].clone(), step], Box::new(v[1].clone()))
                }),
            ),
            ("emptyList", builtin(table, 1, |v| Type::Fun(Vec::new(), Box::new(list(&v[0]))))),
        ];
        for (name, scheme) in builtins {
            self.globals.insert(name.into(), scheme);
        }
    }

    fn annotated_scheme(&mut self, function: &Function) -> Scheme {
        let mut vars = BTreeMap::new();
        let mut ids = Vec::new();
        for name in &function.type_params {
            let var = self.table.fresh(GENERIC_LEVEL);
            if let Type::Var(id) = var {
                ids.push(id);
            }
            vars.insert(name.clone(), var);
        }
        let ty = self.signature(function, &vars, true);
        Scheme { vars: ids, ty }
    }

    /// Function type from the declaration; missing annotations become fresh variables.
    fn signature(&mut self, function: &Function, vars: &BTreeMap<String, Type>, report: bool) -> Type {
        let mut params = Vec::new();
        for parameter in &function.parameters {
            let ty = match &parameter.type_ref {
                Some(type_ref) => self.lower_ref(type_ref, &function.span, vars, report),
                None => self.fresh(),
            };
            params.push(ty);
        }
        let result = if function.component {
            Type::con("Ui")
        } else {
            match &function.return_type {
                Some(type_ref) => self.lower_ref(type_ref, &function.span, vars, report),
                None => self.fresh(),
            }
        };
        Type::Fun(params, Box::new(result))
    }

    fn lower_ref(&mut self, type_ref: &TypeRef, span: &Span, vars: &BTreeMap<String, Type>, report: bool) -> Type {
        let mut sink = Vec::new();
        let diagnostics = if report { &mut self.diagnostics } else { &mut sink };
        match parse_type_text(&type_ref.display(), span) {
            Ok(te) => self.env.lower(&te, vars, diagnostics),
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                Type::Error
            }
        }
    }

    fn infer_group(&mut self, group: &[&Function]) {
        self.level += 1;
        let mut types = Vec::new();
        let mut scopes = Vec::new();
        for function in group {
            let mut scope = BTreeMap::new();
            for name in &function.type_params {
                scope.insert(name.clone(), self.fresh());
            }
            let ty = self.signature(function, &scope, true);
            self.globals.insert(function.name.clone(), Scheme::mono(ty.clone()));
            types.push(ty);
            scopes.push(scope);
        }
        for ((function, ty), scope) in group.iter().zip(&types).zip(&scopes) {
            self.rigid = scope.clone();
            self.check_body(function, ty);
        }
        self.rigid.clear();
        for (function, scope) in group.iter().zip(&scopes) {
            self.check_declared_generics(function, scope);
        }
        for function in group {
            self.report_ambiguous(&function.name);
        }
        self.level -= 1;
        for (function, ty) in group.iter().zip(&types) {
            let (scheme, ambiguous) = generalize(&self.table, self.level, ty);
            self.report_constraints(ambiguous, &function.span);
            self.globals.insert(function.name.clone(), scheme);
        }
    }

    /// Declared type parameters of a partially annotated function are flexible during
    /// inference, but the body must not specialize them or make two of them equal.
    fn check_declared_generics(&mut self, function: &Function, scope: &BTreeMap<String, Type>) {
        let mut seen: BTreeMap<TypeVarId, &str> = BTreeMap::new();
        for name in &function.type_params {
            let Some(var) = scope.get(name) else { continue };
            let requirement = match self.table.shallow(var) {
                Type::Error => continue,
                Type::Var(id) => match seen.get(&id) {
                    Some(other) => format!("it to equal `{other}`"),
                    None => {
                        seen.insert(id, name);
                        continue;
                    }
                },
                other => format!("`{}`", self.shown(&other)),
            };
            self.error(
                "E7101",
                format!(
                    "Type parameter `{name}` of `{}` is declared generic, but the body requires {requirement}.",
                    function.name
                ),
                &function.span,
            );
        }
    }

    fn check_annotated(&mut self, function: &Function) {
        self.level += 1;
        self.rigid = rigid_vars(&function.type_params);
        let rigid = self.rigid.clone();
        let ty = self.signature(function, &rigid, false);
        self.check_body(function, &ty);
        self.report_ambiguous(&function.name);
        self.rigid.clear();
        self.level -= 1;
    }

    fn check_body(&mut self, function: &Function, ty: &Type) {
        let Type::Fun(params, result) = ty else { return };
        let mut names = BTreeSet::new();
        for parameter in &function.parameters {
            if !names.insert(parameter.name.as_str()) {
                self.error(
                    "E7113",
                    format!("`{}` is declared more than once in the parameters of `{}`.", parameter.name, function.name),
                    &function.span,
                );
            }
        }
        let Some(block) = &function.expr else { return };
        self.function = function.name.clone();
        self.return_type = Some((**result).clone());
        self.locals.clear();
        for (parameter, param) in function.parameters.iter().zip(params) {
            self.locals.push((parameter.name.clone(), Scheme::mono(param.clone())));
        }
        let body = self.infer_block(block);
        let reason = if function.component {
            "a component body, which must end with a UI tree".to_string()
        } else {
            format!("the declared result of `{}`", function.name)
        };
        self.unify_at(result, &body, &result_span(block), Some(reason));
        self.locals.clear();
        self.return_type = None;
    }

    fn finish(&self, program: &Program) -> TypedProgram {
        let mut typed = TypedProgram::default();
        for function in &program.functions {
            if let Some(scheme) = self.globals.get(&function.name) {
                let ty = self.table.resolve(&scheme.ty);
                typed.schemes.insert(function.name.clone(), display(&ty));
                typed.signatures.insert(function.name.clone(), ty);
            }
        }
        typed.node_types = self.nodes.iter().map(|(key, (ty, _))| (key.clone(), self.table.resolve(ty))).collect();
        typed
    }

    // ---- ambiguity -----------------------------------------------------------------

    /// Constrained variables still unbound at the end of a function have no concrete type.
    fn report_ambiguous(&mut self, function: &str) {
        let key = function.to_string();
        let entries: Vec<(Type, Span)> =
            self.nodes.range((key.clone(), 0)..=(key, NodeId::MAX)).map(|(_, entry)| entry.clone()).collect();
        let mut seen = BTreeSet::new();
        for (ty, span) in entries {
            for id in free_vars(&self.table, &ty) {
                if let VarState::Unbound { constraint: Some(constraint), .. } = self.table.state(id).clone()
                    && seen.insert(id)
                {
                    self.ambiguity(constraint, &span);
                    self.table.set(id, VarState::Link(Type::Error));
                }
            }
        }
    }

    fn report_constraints(&mut self, ambiguous: Vec<(TypeVarId, Constraint)>, span: &Span) {
        for (id, constraint) in ambiguous {
            self.ambiguity(constraint, span);
            self.table.set(id, VarState::Link(Type::Error));
        }
    }

    fn ambiguity(&mut self, constraint: Constraint, span: &Span) {
        self.diagnostics.push(
            Diagnostic::error(
                "E7302",
                format!("Ambiguous type: a value used with {} never receives a concrete type.", constraint.describe()),
                span.clone(),
            )
            .with_hint("Annotate a parameter or value, e.g. `a: Int` or `a: Double`."),
        );
    }

    // ---- helpers -------------------------------------------------------------------

    fn fresh(&mut self) -> Type {
        self.table.fresh(self.level)
    }

    fn record(&mut self, id: NodeId, ty: &Type, span: &Span) {
        self.nodes.insert((self.function.clone(), id), (ty.clone(), span.clone()));
    }

    fn error(&mut self, code: &'static str, message: impl Into<String>, span: &Span) {
        self.diagnostics.push(Diagnostic::error(code, message, span.clone()));
    }

    fn shown(&self, ty: &Type) -> String {
        display(&self.table.resolve(ty))
    }

    fn is_local(&self, name: &str) -> bool {
        self.locals.iter().any(|(local, _)| local == name)
    }

    fn is_shadowed(&self, name: &str) -> bool {
        self.is_local(name) || self.globals.contains_key(name)
    }

    fn unify_at(&mut self, expected: &Type, found: &Type, span: &Span, reason: Option<String>) {
        let Err(error) = unify(&mut self.table, expected, found) else { return };
        let diagnostic = match error {
            UnifyError::Mismatch { expected, found } => {
                let names = display_all(&[&expected, &found]);
                let diagnostic = Diagnostic::error(
                    "E7101",
                    format!("Type mismatch: expected `{}`, found `{}`.", names[0], names[1]),
                    span.clone(),
                );
                match reason {
                    Some(reason) => diagnostic.with_hint(format!("`{}` is required by {reason}.", names[0])),
                    None => diagnostic,
                }
            }
            UnifyError::Infinite { var, ty } => {
                let names = display_all(&[&Type::Var(var), &ty]);
                Diagnostic::error(
                    "E7107",
                    format!("Infinite type: `{}` would have to equal `{}`.", names[0], names[1]),
                    span.clone(),
                )
            }
            UnifyError::Constraint { constraint, ty } => self.constraint_error(constraint, &ty, span),
        };
        self.diagnostics.push(diagnostic);
    }

    fn require(&mut self, constraint: Constraint, ty: &Type, span: &Span) {
        if let Err(UnifyError::Constraint { constraint, ty }) = constrain(&mut self.table, constraint, ty) {
            let diagnostic = self.constraint_error(constraint, &ty, span);
            self.diagnostics.push(diagnostic);
        }
    }

    fn constraint_error(&self, constraint: Constraint, ty: &Type, span: &Span) -> Diagnostic {
        let diagnostic =
            Diagnostic::error("E7301", format!("`{}` does not support {}.", display(ty), constraint.describe()), span.clone());
        match ty {
            Type::Con(name, _) if constraint == Constraint::Eq && name != "Ui" && self.table.is_non_equatable(name) => {
                diagnostic.with_hint(format!("`{name}` has a field that contains a function or a UI tree."))
            }
            _ => diagnostic,
        }
    }

    // ---- blocks and statements -----------------------------------------------------

    fn infer_block(&mut self, block: &Block) -> Type {
        let mark = self.locals.len();
        let mut value = Type::con("Unit");
        for (index, stmt) in block.stmts.iter().enumerate() {
            let last = index + 1 == block.stmts.len();
            match stmt {
                Stmt::Val { name, annotation, value: expr, span } => self.bind_val(name, annotation.as_ref(), expr, span),
                Stmt::Expr(expr) => {
                    let ty = self.infer(expr);
                    if last {
                        value = ty;
                    } else {
                        self.discard(&ty, expr);
                    }
                }
            }
        }
        self.locals.truncate(mark);
        self.record(block.id, &value, &block.span);
        value
    }

    fn bind_val(&mut self, name: &str, annotation: Option<&TypeExpr>, value: &Expr, span: &Span) {
        self.level += 1;
        let mut ty = self.infer(value);
        if let Some(annotation) = annotation {
            let declared = self.env.lower(annotation, &self.rigid, &mut self.diagnostics);
            self.unify_at(&declared, &ty, &value.span, Some(format!("the annotation on `{name}`")));
            ty = declared;
        }
        self.level -= 1;
        let (scheme, ambiguous) = generalize(&self.table, self.level, &ty);
        self.report_constraints(ambiguous, span);
        if name != "_" {
            self.locals.push((name.to_string(), scheme));
        }
    }

    fn discard(&mut self, ty: &Type, expr: &Expr) {
        if unify(&mut self.table, &Type::con("Unit"), ty).is_ok() {
            return;
        }
        let shown = self.shown(ty);
        self.diagnostics.push(
            Diagnostic::error(
                "E7108",
                format!(
                    "The value of `{}` (type `{shown}`) is computed and then discarded.",
                    abbreviate(&render_expr(expr))
                ),
                expr.span.clone(),
            )
            .with_hint("Use the value, or write `val _ = ...` to discard it explicitly."),
        );
    }

    // ---- expressions ---------------------------------------------------------------

    fn infer(&mut self, expr: &Expr) -> Type {
        let ty = match &expr.kind {
            ExprKind::Int(_) => Type::con("Int"),
            ExprKind::Double(_) => Type::con("Double"),
            ExprKind::Float(_) => Type::con("Float"),
            ExprKind::Str(_) => Type::con("String"),
            ExprKind::Bool(_) => Type::con("Boolean"),
            ExprKind::Null => {
                let inner = self.fresh();
                Type::option(inner)
            }
            ExprKind::Name(name) => self.infer_name(name, &expr.span),
            ExprKind::Call { callee, args, children } => self.infer_call(callee, args, children.as_ref(), &expr.span),
            ExprKind::Field { target, name } => self.infer_field(target, name, &expr.span),
            ExprKind::SafeField { target, name } => {
                let receiver = self.infer(target);
                let inner = self.fresh();
                let reported = self.diagnostics.len();
                self.unify_at(&Type::option(inner.clone()), &receiver, &target.span, Some("the receiver of `?.`".into()));
                if self.diagnostics.len() > reported {
                    self.record(expr.id, &Type::Error, &expr.span);
                    return Type::Error;
                }
                let field = self.field_of(&inner, name, &expr.span);
                match self.table.shallow(&field) {
                    Type::Con(option, _) if option == "Option" => field,
                    _ => Type::option(field),
                }
            }
            ExprKind::Unary { op: UnaryOp::Neg, operand } => {
                let ty = self.infer(operand);
                self.require(Constraint::Sub, &ty, &operand.span);
                ty
            }
            ExprKind::Unary { op: UnaryOp::Not, operand } => {
                let ty = self.infer(operand);
                self.unify_at(&Type::con("Boolean"), &ty, &operand.span, Some("the `!` operator".into()));
                Type::con("Boolean")
            }
            ExprKind::Binary { op, left, right } => self.infer_binary(*op, left, right, &expr.span),
            ExprKind::Elvis { value, fallback } => {
                let ty = self.infer(value);
                let inner = self.fresh();
                self.unify_at(&Type::option(inner.clone()), &ty, &value.span, Some("the left side of `?:`".into()));
                let fallback_ty = self.infer(fallback);
                self.unify_at(&inner, &fallback_ty, &fallback.span, Some("the left side of `?:`".into()));
                inner
            }
            ExprKind::If { condition, then_branch, else_branch } => {
                let condition_ty = self.infer(condition);
                self.unify_at(&Type::con("Boolean"), &condition_ty, &condition.span, Some("an `if` condition".into()));
                let then_ty = self.infer(then_branch);
                let else_ty = self.infer(else_branch);
                self.unify_at(&then_ty, &else_ty, &else_branch.span, Some("the first `if` branch".into()));
                then_ty
            }
            ExprKind::When { scrutinee, arms } => self.infer_when(scrutinee, arms, &expr.span),
            ExprKind::Lambda { params, body } => self.infer_lambda(params, body, None),
            ExprKind::Block(block) => self.infer_block(block),
            ExprKind::Return(value) => {
                let found = self.infer(value);
                if self.lambda_depth > 0 {
                    self.error(
                        "E7111",
                        "`return` is not allowed inside a lambda; the lambda's value is its last expression.",
                        &expr.span,
                    );
                } else if let Some(expected) = self.return_type.clone() {
                    let reason = format!("the return type of `{}`", self.function);
                    self.unify_at(&expected, &found, &value.span, Some(reason));
                }
                self.fresh()
            }
            ExprKind::Builder { target, fields } => self.infer_builder(target, fields, &expr.span),
        };
        self.record(expr.id, &ty, &expr.span);
        ty
    }

    fn infer_name(&mut self, name: &str, span: &Span) -> Type {
        if let Some(scheme) = self.locals.iter().rev().find(|(local, _)| local == name).map(|(_, scheme)| scheme.clone()) {
            return instantiate(&mut self.table, &scheme, self.level);
        }
        if let Some(scheme) = self.globals.get(name).cloned() {
            return instantiate(&mut self.table, &scheme, self.level);
        }
        if self.env.ctors.contains_key(name) {
            return self.constructor_value(name);
        }
        if UI_NODES.contains(&name) {
            self.error("E7004", format!("UI node `{name}` must be called, e.g. `{name}(...)`."), span);
            return Type::Error;
        }
        self.error("E7103", format!("Unknown name `{name}`."), span);
        Type::Error
    }

    fn constructor_value(&mut self, name: &str) -> Type {
        let (fields, result) = self.env.instantiate_ctor(name, &mut self.table, self.level);
        if fields.is_empty() { result } else { Type::Fun(fields, Box::new(result)) }
    }

    fn infer_call(&mut self, callee: &Expr, args: &[Expr], children: Option<&Block>, span: &Span) -> Type {
        if let ExprKind::Name(name) = &callee.kind
            && !self.is_shadowed(name)
            && !self.env.ctors.contains_key(name)
        {
            if UI_NODES.contains(&name.as_str()) {
                return self.infer_ui(name, args, children, span);
            }
            if name == "listOf" {
                return self.infer_list_of(args, children, span);
            }
        }
        if let Some((table, method)) = db_method(callee)
            && !self.is_shadowed("db")
        {
            return self.infer_db_call(table, method, args, span);
        }
        if let Some(block) = children {
            self.error("E7004", "Only UI nodes accept a child block.", &block.span);
        }
        let callee_ty = self.infer(callee);
        let shallow = self.table.shallow(&callee_ty);
        // Arguments are checked left to right against the parameters, so a lambda argument
        // sees the parameter types fixed by the arguments before it (as in `map(xs, { p -> p.x })`).
        if let Type::Fun(params, result) = &shallow
            && params.len() == args.len()
        {
            let reason = self.parameter_reason(callee);
            for (param, arg) in params.iter().zip(args) {
                let arg_ty = self.infer_argument(arg, param);
                self.unify_at(param, &arg_ty, &arg.span, reason.clone());
            }
            return (**result).clone();
        }
        let arg_types: Vec<Type> = args.iter().map(|arg| self.infer(arg)).collect();
        match shallow {
            Type::Fun(params, result) => {
                self.error(
                    "E7102",
                    format!("This function expects {} argument(s), but {} were given.", params.len(), args.len()),
                    span,
                );
                *result
            }
            Type::Error => Type::Error,
            Type::Var(_) => {
                let result = self.fresh();
                let function = Type::Fun(arg_types, Box::new(result.clone()));
                self.unify_at(&callee_ty, &function, &callee.span, None);
                result
            }
            other => {
                let shown = self.shown(&other);
                self.error("E7101", format!("A value of type `{shown}` is not a function."), &callee.span);
                Type::Error
            }
        }
    }

    /// A lambda passed where a function of the same arity is expected takes its parameter
    /// types from that function; any other argument is inferred on its own.
    fn infer_argument(&mut self, arg: &Expr, expected: &Type) -> Type {
        if let ExprKind::Lambda { params, body } = &arg.kind
            && let Type::Fun(seed, _) = self.table.shallow(expected)
            && seed.len() == params.len()
        {
            let ty = self.infer_lambda(params, body, Some(&seed));
            self.record(arg.id, &ty, &arg.span);
            return ty;
        }
        self.infer(arg)
    }

    fn infer_lambda(&mut self, params: &[(String, Span)], body: &Block, seed: Option<&[Type]>) -> Type {
        let mark = self.locals.len();
        let mut names = BTreeSet::new();
        let mut param_types = Vec::new();
        for (index, (name, span)) in params.iter().enumerate() {
            if !names.insert(name.as_str()) {
                self.error("E7113", format!("`{name}` is bound more than once in this lambda."), span);
            }
            let ty = match seed.and_then(|types| types.get(index)) {
                Some(ty) => ty.clone(),
                None => self.fresh(),
            };
            param_types.push(ty.clone());
            self.locals.push((name.clone(), Scheme::mono(ty)));
        }
        self.lambda_depth += 1;
        let result = self.infer_block(body);
        self.lambda_depth -= 1;
        self.locals.truncate(mark);
        Type::Fun(param_types, Box::new(result))
    }

    fn parameter_reason(&self, callee: &Expr) -> Option<String> {
        let ExprKind::Name(name) = &callee.kind else { return None };
        match self.function_spans.get(name) {
            Some(span) if !self.is_local(name) => Some(format!(
                "a parameter of `{name}` (declared at {}:{}:{})",
                span.file, span.line, span.column
            )),
            _ => Some(format!("a parameter of `{name}`")),
        }
    }

    fn infer_list_of(&mut self, args: &[Expr], children: Option<&Block>, span: &Span) -> Type {
        if children.is_some() {
            self.error("E7004", "`listOf` does not accept a child block.", span);
        }
        let element = self.fresh();
        for arg in args {
            let ty = self.infer(arg);
            self.unify_at(&element, &ty, &arg.span, Some("the first element of `listOf`".into()));
        }
        Type::list(element)
    }

    fn infer_field(&mut self, target: &Expr, name: &str, span: &Span) -> Type {
        if let ExprKind::Name(base) = &target.kind
            && !self.is_shadowed(base)
        {
            if base == "server" {
                let declared = self.function_spans.contains_key(name);
                return match self.globals.get(name).cloned().filter(|_| declared) {
                    Some(scheme) => instantiate(&mut self.table, &scheme, self.level),
                    None => {
                        self.error("E7103", format!("`server.{name}` does not name a function."), span);
                        Type::Error
                    }
                };
            }
            if base == "db" {
                self.error(
                    "E7004",
                    "Database tables are used through `db.<table>.require(...)`, `find(...)` or `insert { ... }`.",
                    span,
                );
                return Type::Error;
            }
        }
        if let ExprKind::Name(base) = &target.kind
            && !self.is_shadowed(base)
            && let Some(info) = self.env.types.get(base)
            && matches!(info.kind, TypeKind::Enum | TypeKind::Sealed)
        {
            if info.ctors.iter().any(|ctor| ctor == name) {
                return self.constructor_value(name);
            }
            self.error("E7104", format!("`{base}` has no variant `{name}`."), span);
            return Type::Error;
        }
        let target_ty = self.infer(target);
        self.field_of(&target_ty, name, span)
    }

    fn field_of(&mut self, target: &Type, name: &str, span: &Span) -> Type {
        match self.table.shallow(target) {
            Type::Error => Type::Error,
            Type::Con(type_name, args) => {
                if type_name == "Option" {
                    let shown = self.shown(target);
                    self.diagnostics.push(
                        Diagnostic::error(
                            "E7101",
                            format!("Cannot access `{name}` on a nullable value of type `{shown}`."),
                            span.clone(),
                        )
                        .with_hint("Use `?.`, `?:` or a `when` that handles `null`."),
                    );
                    return Type::Error;
                }
                match self.env.field_type(&type_name, &args, name) {
                    Some(ty) => ty,
                    None => {
                        let shown = self.shown(target);
                        self.error("E7104", format!("`{shown}` has no field `{name}`."), span);
                        Type::Error
                    }
                }
            }
            Type::Var(_) => {
                let owners = self.env.records_with_field(name);
                match owners.as_slice() {
                    [owner] => {
                        let record = self.env.instantiate_record(owner, &mut self.table, self.level);
                        self.unify_at(&record, target, span, None);
                        let Type::Con(record_name, args) = &record else { return Type::Error };
                        self.env.field_type(record_name, args, name).unwrap_or(Type::Error)
                    }
                    [] => {
                        self.error("E7104", format!("No record type has a field `{name}`."), span);
                        Type::Error
                    }
                    _ => {
                        self.diagnostics.push(
                            Diagnostic::error(
                                "E7106",
                                format!("The field `{name}` exists in several types ({}).", owners.join(", ")),
                                span.clone(),
                            )
                            .with_hint("Annotate the value's type."),
                        );
                        Type::Error
                    }
                }
            }
            Type::Fun(..) => {
                self.error("E7104", format!("A function has no field `{name}`."), span);
                Type::Error
            }
        }
    }

    fn infer_binary(&mut self, op: BinaryOp, left: &Expr, right: &Expr, span: &Span) -> Type {
        let left_ty = self.infer(left);
        let right_ty = self.infer(right);
        let boolean = Type::con("Boolean");
        match op {
            BinaryOp::And | BinaryOp::Or => {
                let reason = Some(format!("the `{}` operator", op.symbol()));
                self.unify_at(&boolean, &left_ty, &left.span, reason.clone());
                self.unify_at(&boolean, &right_ty, &right.span, reason);
                boolean
            }
            BinaryOp::Rem => {
                let int = Type::con("Int");
                self.unify_at(&int, &left_ty, &left.span, Some("the `%` operator".into()));
                self.unify_at(&int, &right_ty, &right.span, Some("the `%` operator".into()));
                Type::option(int)
            }
            _ => {
                let constraint = match op {
                    BinaryOp::Add => Constraint::Add,
                    BinaryOp::Sub => Constraint::Sub,
                    BinaryOp::Mul | BinaryOp::Div => Constraint::Num,
                    BinaryOp::Eq | BinaryOp::NotEq => Constraint::Eq,
                    _ => Constraint::Ord,
                };
                self.unify_at(&left_ty, &right_ty, &right.span, Some(format!("the left operand of `{}`", op.symbol())));
                self.require(constraint, &left_ty, span);
                match op {
                    BinaryOp::Div => self.division_result(&left_ty, span),
                    BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => left_ty,
                    _ => boolean,
                }
            }
        }
    }

    /// `Int` and `Decimal` division can divide by zero, so it is nullable; IEEE floats are not.
    fn division_result(&mut self, operand: &Type, span: &Span) -> Type {
        match self.table.shallow(operand) {
            Type::Con(name, _) if name == "Int" || name == "Decimal" => Type::option(operand.clone()),
            Type::Var(id) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        "E7302",
                        "Ambiguous type: the result of `/` depends on the operand type, which is not known yet.",
                        span.clone(),
                    )
                    .with_hint("Annotate a parameter or value, e.g. `a: Int` or `a: Double`."),
                );
                self.table.set(id, VarState::Link(Type::Error));
                Type::Error
            }
            _ => operand.clone(),
        }
    }

    fn infer_builder(&mut self, target: &Expr, fields: &[FieldInit], span: &Span) -> Type {
        let insert = db_method(target).filter(|(_, method)| *method == "insert" && !self.is_shadowed("db"));
        let Some((table, _)) = insert else {
            for field in fields {
                self.infer(&field.value);
            }
            self.error("E7004", "`{ field = value }` blocks are only supported on `db.<table>.insert`.", span);
            return Type::Error;
        };
        let schema = self.schema;
        let Some((_, model)) = self.db_table(table, span) else {
            for field in fields {
                self.infer(&field.value);
            }
            return Type::Error;
        };
        let columns = &schema.tables[table].columns;
        let mut seen = BTreeSet::new();
        for field in fields {
            let found = self.infer(&field.value);
            if !seen.insert(field.name.as_str()) {
                self.error("E7110", format!("Column `{}` is assigned more than once.", field.name), &field.span);
                continue;
            }
            match columns.get(&field.name) {
                Some(column) => {
                    let expected = self.column_type(column, &field.span);
                    let reason = Some(format!("column `{table}.{}`", field.name));
                    self.unify_at(&expected, &found, &field.value.span, reason);
                }
                None => self.error("E7104", format!("Table `{table}` has no column `{}`.", field.name), &field.span),
            }
        }
        model
    }

    fn infer_db_call(&mut self, table: &str, method: &str, args: &[Expr], span: &Span) -> Type {
        let Some((key, model)) = self.db_table(table, span) else {
            for arg in args {
                self.infer(arg);
            }
            return Type::Error;
        };
        if !matches!(method, "require" | "find") {
            for arg in args {
                self.infer(arg);
            }
            self.error(
                "E7004",
                format!("Unsupported table operation `{method}`; use `require`, `find` or `insert {{ ... }}`."),
                span,
            );
            return Type::Error;
        }
        if args.len() != 1 {
            for arg in args {
                self.infer(arg);
            }
            self.error(
                "E7102",
                format!("`db.{table}.{method}` expects 1 argument (the primary key), but {} were given.", args.len()),
                span,
            );
            return Type::Error;
        }
        let found = self.infer(&args[0]);
        self.unify_at(&key, &found, &args[0].span, Some(format!("the primary key of table `{table}`")));
        if method == "find" { Type::option(model) } else { model }
    }

    /// The primary-key type and model type of a table, which must map to a declared entity.
    fn db_table(&mut self, table: &str, span: &Span) -> Option<(Type, Type)> {
        let schema = self.schema;
        let Some(definition) = schema.tables.get(table) else {
            self.error("E7104", format!("Unknown database table `{table}`."), span);
            return None;
        };
        let is_entity = self.env.types.get(&definition.model).is_some_and(|info| info.kind == TypeKind::Entity);
        if !is_entity {
            self.error(
                "E7103",
                format!("Table `{table}` maps to `{}`, which is not a declared entity.", definition.model),
                span,
            );
            return None;
        }
        let key = match definition.columns.get(&definition.primary_key) {
            Some(column) => self.column_type(column, span),
            None => Type::Error,
        };
        Some((key, Type::con(&definition.model)))
    }

    fn column_type(&mut self, column: &Column, span: &Span) -> Type {
        let base = match parse_type_text(&column.type_name, span) {
            Ok(te) => self.env.lower(&te, &BTreeMap::new(), &mut self.diagnostics),
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                Type::Error
            }
        };
        if column.nullable { Type::option(base) } else { base }
    }

    fn infer_ui(&mut self, name: &str, args: &[Expr], children: Option<&Block>, span: &Span) -> Type {
        if name == "Text" {
            if args.len() != 1 {
                self.error("E7102", format!("`Text` expects 1 argument, but {} were given.", args.len()), span);
            }
            for arg in args {
                let ty = self.infer(arg);
                let resolved = self.table.resolve(&ty);
                if !self.env.is_displayable(&resolved) {
                    let hint = if matches!(resolved, Type::Var(_)) {
                        "Annotate the parameter's type, e.g. `label: String`."
                    } else {
                        "Display a String, number, Boolean, domain scalar or a distinct type over one of them."
                    };
                    self.diagnostics.push(
                        Diagnostic::error(
                            "E7105",
                            format!("`Text` cannot display a value of type `{}`.", display(&resolved)),
                            arg.span.clone(),
                        )
                        .with_hint(hint),
                    );
                }
            }
            if let Some(block) = children {
                self.error("E7004", "`Text` cannot have children.", &block.span);
            }
            return Type::con("Ui");
        }
        for arg in args {
            let ty = self.infer(arg);
            match self.table.shallow(&ty) {
                Type::Error => {}
                Type::Con(type_name, type_args) if type_args.is_empty() && (type_name == "String" || type_name == "Int") => {}
                other => {
                    let shown = self.shown(&other);
                    let mut diagnostic = Diagnostic::error(
                        "E7101",
                        format!("UI node arguments must be `String` or `Int`, found `{shown}`."),
                        arg.span.clone(),
                    );
                    if matches!(other, Type::Var(_)) {
                        diagnostic = diagnostic.with_hint("Annotate the parameter's type, e.g. `label: String`.");
                    }
                    self.diagnostics.push(diagnostic);
                }
            }
        }
        if let Some(block) = children {
            let mark = self.locals.len();
            for stmt in &block.stmts {
                match stmt {
                    Stmt::Val { name, annotation, value, span } => self.bind_val(name, annotation.as_ref(), value, span),
                    Stmt::Expr(expr) => {
                        let ty = self.infer(expr);
                        self.unify_at(&Type::con("Ui"), &ty, &expr.span, Some("a UI child block".into()));
                    }
                }
            }
            self.locals.truncate(mark);
            self.record(block.id, &Type::con("Ui"), &block.span);
        }
        Type::con("Ui")
    }

    // ---- when and patterns ---------------------------------------------------------

    fn infer_when(&mut self, scrutinee: &Expr, arms: &[Arm], span: &Span) -> Type {
        let subject = self.infer(scrutinee);
        if arms.iter().any(|arm| matches!(arm.pattern, Pattern::Null(_))) {
            let inner = self.fresh();
            self.unify_at(&Type::option(inner), &subject, &scrutinee.span, Some("a `null` pattern".into()));
        }
        let result = self.fresh();
        let mut patterns = Vec::new();
        let mut invalid_pattern = false;
        for arm in arms {
            let mark = self.locals.len();
            let mut bound = BTreeSet::new();
            let reported = self.diagnostics.len();
            let pattern = self.check_pattern(&arm.pattern, &subject, &mut bound);
            invalid_pattern |= self.diagnostics.len() > reported;
            let body = self.infer(&arm.body);
            self.unify_at(&result, &body, &arm.body.span, Some("the first `when` branch".into()));
            self.locals.truncate(mark);
            patterns.push(pattern);
        }
        // An invalid pattern stands in as `_`, which would make later arms look redundant.
        let subject_ty = self.table.resolve(&subject);
        if !invalid_pattern && !subject_ty.contains_error() {
            let report = exhaustive::check_match(self.env, &subject_ty, &patterns);
            for index in report.redundant {
                self.error("E7202", "This `when` branch can never be reached.", &arms[index].span);
            }
            if let Some(missing) = report.missing {
                self.error(
                    "E7201",
                    format!("`when` is not exhaustive; missing case: {}.", exhaustive::render(&missing)),
                    span,
                );
            }
        }
        result
    }

    /// Checks `pattern` against `ty`. At a type already known to be `T?`, every pattern other
    /// than `_` and `null` describes a non-null value and is checked against `T`.
    fn check_pattern(&mut self, pattern: &Pattern, ty: &Type, bound: &mut BTreeSet<String>) -> Pat {
        if !matches!(pattern, Pattern::Wildcard | Pattern::Null(_))
            && let Type::Con(name, args) = self.table.shallow(ty)
            && name == "Option"
            && args.len() == 1
        {
            let inner = self.check_pattern(pattern, &args[0], bound);
            return Pat::Ctor("Some".into(), vec![inner]);
        }
        match pattern {
            Pattern::Wildcard => Pat::Wild,
            Pattern::Null(span) => {
                let inner = self.fresh();
                self.unify_at(&Type::option(inner), ty, span, Some("a `null` pattern".into()));
                Pat::Ctor("null".into(), Vec::new())
            }
            Pattern::Bool(value, span) => {
                self.unify_at(ty, &Type::con("Boolean"), span, None);
                Pat::Ctor(value.to_string(), Vec::new())
            }
            Pattern::Int(text, span) => {
                self.unify_at(ty, &Type::con("Int"), span, None);
                Pat::Lit(text.clone())
            }
            Pattern::Str(text, span) => {
                self.unify_at(ty, &Type::con("String"), span, None);
                Pat::Lit(format!("{text:?}"))
            }
            Pattern::Double(text, span) => {
                self.error("E7109", format!("The floating-point literal `{text}` cannot be used as a pattern."), span);
                Pat::Wild
            }
            Pattern::Name(name, span) => {
                if let Some(ctor) = self.env.ctors.get(name) {
                    if !ctor.fields.is_empty() {
                        self.error(
                            "E7102",
                            format!("Constructor `{name}` has {} field(s); write `{name}(...)`.", ctor.fields.len()),
                            span,
                        );
                        return Pat::Wild;
                    }
                    let (_, result) = self.env.instantiate_ctor(name, &mut self.table, self.level);
                    self.unify_at(ty, &result, span, None);
                    return Pat::Ctor(name.clone(), Vec::new());
                }
                if name.chars().next().is_some_and(char::is_uppercase) {
                    self.error("E7103", format!("Unknown constructor `{name}`."), span);
                    return Pat::Wild;
                }
                if !bound.insert(name.clone()) {
                    self.error("E7113", format!("`{name}` is bound more than once in this pattern."), span);
                }
                self.locals.push((name.clone(), Scheme::mono(ty.clone())));
                Pat::Wild
            }
            Pattern::Constructor { name, args, span } => {
                if !self.env.ctors.contains_key(name) {
                    self.error("E7103", format!("Unknown constructor `{name}`."), span);
                    return Pat::Wild;
                }
                let (fields, result) = self.env.instantiate_ctor(name, &mut self.table, self.level);
                self.unify_at(ty, &result, span, None);
                if fields.len() != args.len() {
                    self.error(
                        "E7102",
                        format!("Constructor `{name}` has {} field(s), but the pattern lists {}.", fields.len(), args.len()),
                        span,
                    );
                    return Pat::Wild;
                }
                let patterns = args.iter().zip(&fields).map(|(arg, field)| self.check_pattern(arg, field, bound)).collect();
                Pat::Ctor(name.clone(), patterns)
            }
        }
    }
}

/// `db.<table>.<method>` as written, before any shadowing check.
fn db_method(expr: &Expr) -> Option<(&str, &str)> {
    let ExprKind::Field { target, name: method } = &expr.kind else { return None };
    let ExprKind::Field { target: root, name: table } = &target.kind else { return None };
    match &root.kind {
        ExprKind::Name(db) if db == "db" => Some((table.as_str(), method.as_str())),
        _ => None,
    }
}

fn is_fully_annotated(function: &Function) -> bool {
    function.parameters.iter().all(|parameter| parameter.type_ref.is_some())
        && (function.component || function.return_type.is_some())
}

fn builtin(table: &mut TypeTable, arity: usize, build: impl Fn(&[Type]) -> Type) -> Scheme {
    let vars: Vec<Type> = (0..arity).map(|_| table.fresh(GENERIC_LEVEL)).collect();
    let ids = vars.iter().filter_map(|var| if let Type::Var(id) = var { Some(*id) } else { None }).collect();
    Scheme { vars: ids, ty: build(&vars) }
}

fn result_span(block: &Block) -> Span {
    match block.stmts.last() {
        Some(Stmt::Expr(expr)) => expr.span.clone(),
        Some(Stmt::Val { span, .. }) => span.clone(),
        None => block.span.clone(),
    }
}

fn abbreviate(text: &str) -> String {
    if text.chars().count() <= 60 {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(59).collect::<String>())
    }
}

/// Strongly connected components of the call graph among `functions`, dependencies first
/// (Tarjan). Each component is inferred together, which supports mutual recursion.
fn components(functions: &[&Function]) -> Vec<Vec<usize>> {
    let index: BTreeMap<&str, usize> =
        functions.iter().enumerate().map(|(position, function)| (function.name.as_str(), position)).collect();
    let edges: Vec<Vec<usize>> = functions
        .iter()
        .map(|function| {
            let mut names = BTreeSet::new();
            if let Some(block) = &function.expr {
                collect_block(block, &mut names);
            }
            names.iter().filter_map(|name| index.get(name.as_str()).copied()).collect()
        })
        .collect();
    let mut tarjan = Tarjan {
        edges: &edges,
        next: 0,
        indices: vec![None; functions.len()],
        low: vec![0; functions.len()],
        stack: Vec::new(),
        on_stack: vec![false; functions.len()],
        output: Vec::new(),
    };
    for vertex in 0..functions.len() {
        if tarjan.indices[vertex].is_none() {
            tarjan.visit(vertex);
        }
    }
    tarjan.output
}

struct Tarjan<'a> {
    edges: &'a [Vec<usize>],
    next: usize,
    indices: Vec<Option<usize>>,
    low: Vec<usize>,
    stack: Vec<usize>,
    on_stack: Vec<bool>,
    output: Vec<Vec<usize>>,
}

impl Tarjan<'_> {
    fn visit(&mut self, vertex: usize) {
        self.indices[vertex] = Some(self.next);
        self.low[vertex] = self.next;
        self.next += 1;
        self.stack.push(vertex);
        self.on_stack[vertex] = true;
        let edges = self.edges;
        for &other in &edges[vertex] {
            match self.indices[other] {
                None => {
                    self.visit(other);
                    self.low[vertex] = self.low[vertex].min(self.low[other]);
                }
                Some(index) if self.on_stack[other] => self.low[vertex] = self.low[vertex].min(index),
                Some(_) => {}
            }
        }
        if Some(self.low[vertex]) == self.indices[vertex] {
            let mut component = Vec::new();
            while let Some(member) = self.stack.pop() {
                self.on_stack[member] = false;
                component.push(member);
                if member == vertex {
                    break;
                }
            }
            component.sort_unstable();
            self.output.push(component);
        }
    }
}

/// Names referenced by a body (over-approximated: shadowing is ignored).
fn collect_block(block: &Block, names: &mut BTreeSet<String>) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Val { value, .. } | Stmt::Expr(value) => collect_expr(value, names),
        }
    }
}

fn collect_expr(expr: &Expr, names: &mut BTreeSet<String>) {
    match &expr.kind {
        ExprKind::Name(name) => {
            names.insert(name.clone());
        }
        ExprKind::Field { target, name } => {
            if let ExprKind::Name(base) = &target.kind
                && base == "server"
            {
                names.insert(name.clone());
            }
            collect_expr(target, names);
        }
        ExprKind::SafeField { target, .. } => collect_expr(target, names),
        ExprKind::Call { callee, args, children } => {
            collect_expr(callee, names);
            args.iter().for_each(|arg| collect_expr(arg, names));
            if let Some(block) = children {
                collect_block(block, names);
            }
        }
        ExprKind::Unary { operand, .. } => collect_expr(operand, names),
        ExprKind::Binary { left, right, .. } => {
            collect_expr(left, names);
            collect_expr(right, names);
        }
        ExprKind::Elvis { value, fallback } => {
            collect_expr(value, names);
            collect_expr(fallback, names);
        }
        ExprKind::If { condition, then_branch, else_branch } => {
            collect_expr(condition, names);
            collect_expr(then_branch, names);
            collect_expr(else_branch, names);
        }
        ExprKind::When { scrutinee, arms } => {
            collect_expr(scrutinee, names);
            arms.iter().for_each(|arm| collect_expr(&arm.body, names));
        }
        ExprKind::Lambda { body, .. } | ExprKind::Block(body) => collect_block(body, names),
        ExprKind::Return(value) => collect_expr(value, names),
        ExprKind::Builder { target, fields } => {
            collect_expr(target, names);
            fields.iter().for_each(|field| collect_expr(&field.value, names));
        }
        ExprKind::Int(_) | ExprKind::Double(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Bool(_) | ExprKind::Null => {}
    }
}

#[cfg(test)]
mod tests {
    use crate::types::testing::{errors, scheme};

    const PEDIDO: &str = "sealed class Pedido {\n    data class Pendente(val itens: Int)\n    data class Pago(val valor: Money)\n    data object Cancelado\n}\n";

    #[test]
    fn nested_optional_annotation_is_supported() {
        assert_eq!(scheme("fun f(a: Int??): Int? = a ?: null", "f"), "(Int??) -> Int?");
    }

    #[test]
    fn infers_principal_types() {
        assert_eq!(scheme("fun ident(x) = x", "ident"), "('a) -> 'a");
        assert_eq!(scheme("fun constante(x, y) = x", "constante"), "('a, 'b) -> 'a");
        assert_eq!(scheme("fun compose(f, g, x) = f(g(x))", "compose"), "(('a) -> 'b, ('c) -> 'a, 'c) -> 'b");
        assert_eq!(scheme("fun aplica(f, x) = f(x)", "aplica"), "(('a) -> 'b, 'a) -> 'b");
    }

    #[test]
    fn literals_resolve_operator_constraints() {
        assert_eq!(scheme("fun dobro(x) = x * 2", "dobro"), "(Int) -> Int");
        assert_eq!(scheme("fun concat(a, b) = a + b + \"\"", "concat"), "(String, String) -> String");
        assert_eq!(scheme("fun media(a: Double, b: Double) = (a + b) / 2.0", "media"), "(Double, Double) -> Double");
        assert_eq!(scheme("fun menor(a: Instant, b: Instant) = a < b", "menor"), "(Instant, Instant) -> Boolean");
    }

    #[test]
    fn integer_division_and_remainder_are_nullable() {
        assert_eq!(scheme("fun metade(x: Int) = x / 2", "metade"), "(Int) -> Int?");
        assert_eq!(scheme("fun resto(x: Int) = (x % 2) ?: 0", "resto"), "(Int) -> Int");
        assert_eq!(errors("fun f(x: Int): Int = x / 2"), vec!["E7101"]);
    }

    #[test]
    fn rejects_ambiguous_and_unsupported_operators() {
        assert_eq!(errors("fun soma(a, b) = a + b"), vec!["E7302"]);
        assert_eq!(errors("fun divide(a, b) = a / b"), vec!["E7302"]);
        assert_eq!(errors("fun f(a: Boolean, b: Boolean) = a + b"), vec!["E7301"]);
        assert_eq!(errors("fun f(a: String, b: String) = a - b"), vec!["E7301"]);
        assert_eq!(errors("fun g(x: Int): Int = x\nfun f(): Boolean = g == g"), vec!["E7301"]);
        assert_eq!(errors("fun <T> igual(a: T, b: T): Boolean = a == b"), vec!["E7301"]);
    }

    #[test]
    fn equality_rejects_types_that_contain_functions() {
        let holder = "data class H(val run: (Int) -> Int)\n";
        assert_eq!(errors(&format!("{holder}fun f(a: H, b: H): Boolean = a == b")), vec!["E7301"]);
        let wrapper = format!("{holder}data class W(val h: H)\nfun f(a: W, b: W): Boolean = a == b");
        assert_eq!(errors(&wrapper), vec!["E7301"]);
        let sealed = "sealed class S {\n    data class C(val k: () -> Int)\n    data object D\n}\n";
        assert_eq!(errors(&format!("{sealed}fun f(a: S, b: S): Boolean = a == b")), vec!["E7301"]);
        assert_eq!(errors("fun f(a: Ui, b: Ui): Boolean = a == b"), vec!["E7301"]);
        let plain = "data class P(val x: Int)\nfun f(a: P, b: P): Boolean = a == b";
        assert_eq!(scheme(plain, "f"), "(P, P) -> Boolean");
        let boxed = "data class Box<T>(val v: T)\n";
        assert_eq!(scheme(&format!("{boxed}fun f(a: Box<Int>, b: Box<Int>): Boolean = a == b"), "f"), "(Box<Int>, Box<Int>) -> Boolean");
        let functions = format!("{boxed}fun f(a: Box<(Int) -> Int>, b: Box<(Int) -> Int>): Boolean = a == b");
        assert_eq!(errors(&functions), vec!["E7301"]);
        let local = format!("{holder}fun f(x: H): Boolean {{\n    val y = x\n    y == x\n}}");
        assert_eq!(errors(&local), vec!["E7301"]);
    }

    #[test]
    fn generalizes_local_values() {
        let source = "fun f(): Boolean {\n    val id = { x -> x }\n    val n = id(1)\n    id(n == 1)\n}";
        assert_eq!(scheme(source, "f"), "() -> Boolean");
    }

    #[test]
    fn annotated_type_parameters_are_rigid() {
        assert_eq!(scheme("fun <T> primeiro(xs: List<T>): T? = first(xs)", "primeiro"), "(List<'a>) -> 'a?");
        assert_eq!(errors("fun <T> errado(x: T): Int = x"), vec!["E7101"]);
        assert_eq!(
            scheme("fun <A, B> mapear(xs: List<A>, f: (A) -> B): List<B> = map(xs, f)", "mapear"),
            "(List<'a>, ('a) -> 'b) -> List<'b>"
        );
    }

    #[test]
    fn infers_mutual_recursion() {
        let source = "fun par(n) = if (n == 0) true else impar(n - 1)\nfun impar(n) = if (n == 0) false else par(n - 1)";
        assert_eq!(scheme(source, "par"), "(Int) -> Boolean");
        assert_eq!(scheme(source, "impar"), "(Int) -> Boolean");
        assert_eq!(scheme("fun fatorial(n) = if (n <= 1) 1 else n * fatorial(n - 1)", "fatorial"), "(Int) -> Int");
    }

    #[test]
    fn rejects_infinite_types() {
        assert_eq!(errors("fun auto(x) = x(x)"), vec!["E7107"]);
    }

    #[test]
    fn discarded_values_are_errors() {
        assert_eq!(errors("fun f(): Int {\n    1 + 1\n    2\n}"), vec!["E7108"]);
        assert_eq!(scheme("fun f(): Int {\n    val _ = 1 + 1\n    2\n}", "f"), "() -> Int");
        let operator_on_next_line = "fun f(a: Int, b: Int): Int {\n    val y = a\n        - b\n    y\n}";
        assert_eq!(errors(operator_on_next_line), vec!["E7108"]);
    }

    #[test]
    fn returns_and_lambdas() {
        assert_eq!(scheme("fun f(x: Int): Int {\n    return x\n}", "f"), "(Int) -> Int");
        assert_eq!(errors("fun f(): Int {\n    val g = { x -> return x }\n    g(1)\n}"), vec!["E7111"]);
        assert_eq!(errors("fun f(): Int {\n    return \"a\"\n}"), vec!["E7101"]);
        assert_eq!(errors("fun f(c: Boolean) = if (c) 1 else \"a\""), vec!["E7101"]);
    }

    #[test]
    fn when_must_be_exhaustive_and_free_of_redundancy() {
        let complete = format!("{PEDIDO}fun f(p: Pedido): Int = when (p) {{\n    Pendente(n) -> n\n    Pago(_) -> 1\n    Cancelado -> 0\n}}");
        assert_eq!(scheme(&complete, "f"), "(Pedido) -> Int");
        let missing = format!("{PEDIDO}fun f(p: Pedido): Int = when (p) {{\n    Pendente(n) -> n\n    Cancelado -> 0\n}}");
        assert_eq!(errors(&missing), vec!["E7201"]);
        let redundant = format!("{PEDIDO}fun f(p: Pedido): Int = when (p) {{\n    _ -> 0\n    Cancelado -> 1\n}}");
        assert_eq!(errors(&redundant), vec!["E7202"]);
        assert_eq!(errors(&format!("{PEDIDO}fun f(p: Pedido): Int = when (p) {{ Pendente(a, b) -> 1; _ -> 0 }}")), vec!["E7102"]);
    }

    #[test]
    fn nullable_patterns() {
        assert_eq!(scheme("fun f(x: Int?): Int = when (x) {\n    null -> 0\n    n -> n\n}", "f"), "(Int?) -> Int");
        assert_eq!(errors("fun f(x: Int?): Int = when (x) { n -> n }"), vec!["E7201"]);
        assert_eq!(scheme("fun f(x) = when (x) {\n    null -> 0\n    n -> n + 1\n}", "f"), "(Int?) -> Int");
    }

    #[test]
    fn literal_patterns() {
        let source = "fun f(x: Int): String = when (x) {\n    0 -> \"zero\"\n    _ -> \"outro\"\n}";
        assert_eq!(scheme(source, "f"), "(Int) -> String");
        assert_eq!(errors("fun f(x: Int): String = when (x) { 0 -> \"zero\" }"), vec!["E7201"]);
        assert_eq!(errors("fun f(x: Double): Int = when (x) { 1.5 -> 1; _ -> 0 }"), vec!["E7109"]);
        assert_eq!(errors("fun f(b: Boolean): Int = when (b) { true -> 1; true -> 2; false -> 3 }"), vec!["E7202"]);
    }

    #[test]
    fn records_and_field_access() {
        let source = "data class P(val x: Int, val nome: String?)\nfun getX(p) = p.x\nfun nome(p: P?) = p?.nome ?: \"\"";
        assert_eq!(scheme(source, "getX"), "(P) -> Int");
        assert_eq!(scheme(source, "nome"), "(P?) -> String");
        assert_eq!(errors("data class A(val x: Int)\ndata class B(val x: Int)\nfun f(v) = v.x"), vec!["E7106"]);
        assert_eq!(errors("data class A(val x: Int)\nfun f(a: A) = a.y"), vec!["E7104"]);
        assert_eq!(errors("data class A(val x: Int)\nfun f(a: A?) = a.x"), vec!["E7101"]);
    }

    #[test]
    fn constructors_results_and_enums() {
        assert_eq!(scheme("fun ok() = Ok(1)", "ok"), "() -> Result<Int, 'a>");
        assert_eq!(scheme("fun erro(s: String): Result<Int, String> = Err(s)", "erro"), "(String) -> Result<Int, String>");
        assert_eq!(scheme("enum class Cor { Azul, Verde }\nfun azul() = Cor.Azul", "azul"), "() -> Cor");
        assert_eq!(scheme("data class P(val x: Int)\nfun p() = P(1)", "p"), "() -> P");
        assert_eq!(errors("data class P(val x: Int)\nfun p() = P(1, 2)"), vec!["E7102"]);
        assert_eq!(scheme("fun xs() = listOf(1, 2, 3)", "xs"), "() -> List<Int>");
        assert_eq!(errors("fun xs() = listOf(1, \"a\")"), vec!["E7101"]);
    }

    #[test]
    fn unknown_names_do_not_cascade() {
        assert_eq!(errors("fun f() = nada"), vec!["E7103"]);
        assert_eq!(errors("fun f(x: Nada) = x"), vec!["E7103"]);
        assert_eq!(errors("fun f() = naoExiste.campo + 1"), vec!["E7103"]);
    }

    #[test]
    fn declared_type_parameters_stay_generic_in_inferred_functions() {
        assert_eq!(scheme("fun <T> ident(x: T) = x", "ident"), "('a) -> 'a");
        assert_eq!(errors("fun <T> f(x: T) = x + 1"), vec!["E7101"]);
        assert_eq!(errors("fun <A, B> f(x: A, y: B) = if (true) x else y"), vec!["E7101"]);
    }

    #[test]
    fn duplicate_parameters_are_errors() {
        assert_eq!(errors("fun f(x, x) = x"), vec!["E7113"]);
    }

    #[test]
    fn lambda_arguments_see_the_expected_parameter_types() {
        let records = "data class A(val valor: Int)\ndata class B(val valor: Int)\n";
        let mapped = format!("{records}fun f(xs: List<A>): List<Int> = map(xs, {{ p -> p.valor }})");
        assert_eq!(scheme(&mapped, "f"), "(List<A>) -> List<Int>");
        let folded = "fun f(xs: List<Double>): Double = fold(xs, 1.0, { acc, x -> acc / x })";
        assert_eq!(scheme(folded, "f"), "(List<Double>) -> Double");
    }

    #[test]
    fn unknown_uppercase_pattern_names_are_not_bindings() {
        let sealed = "sealed class P {\n    data object Um\n    data object Dois\n}\n";
        assert_eq!(errors(&format!("{sealed}fun f(p: P): Int = when (p) {{ Um -> 1; Doiss -> 2 }}")), vec!["E7103"]);
        assert_eq!(scheme(&format!("{sealed}fun f(p: P): Int = when (p) {{ Um -> 1; outro -> 2 }}"), "f"), "(P) -> Int");
    }

    #[test]
    fn safe_field_on_non_nullable_does_not_cascade() {
        assert_eq!(errors("fun f(p: Int) = p?.x"), vec!["E7101"]);
    }

    #[test]
    fn boundaries_require_annotations() {
        assert_eq!(errors("server fun f(x) = x"), vec!["E7003"]);
        assert_eq!(errors("external fun g(x: Int)"), vec!["E7003"]);
    }

    use crate::schema::DatabaseSchema;
    use crate::types::testing::infer_with_schema;

    const CLIENTES: &str = "table,model,primary_key,column,type,nullable,required,has_default,engine\nclientes,Cliente,id,id,ClienteId,false,true,true,postgresql\nclientes,Cliente,id,nome,String,false,true,false,postgresql\nclientes,Cliente,id,apelido,String,true,false,false,postgresql\n";

    const ENTIDADE: &str = "distinct type ClienteId = UUID\nentity Cliente(val id: ClienteId, val nome: String, val apelido: String?)\n";

    fn schema(csv: &str) -> DatabaseSchema {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("bind-types-{}-{id}.csv", std::process::id()));
        std::fs::write(&path, csv).unwrap();
        let schema = DatabaseSchema::load(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        schema
    }

    fn with_db(source: &str) -> Result<Vec<(String, String)>, Vec<String>> {
        infer_with_schema(&format!("{ENTIDADE}{source}"), &schema(CLIENTES))
    }

    fn db_scheme(source: &str, name: &str) -> String {
        let schemes = with_db(source).unwrap_or_else(|codes| panic!("{codes:?}"));
        schemes.into_iter().find(|(other, _)| other == name).unwrap().1
    }

    #[test]
    fn database_operations() {
        assert_eq!(
            db_scheme("server fun buscar(id: ClienteId): Cliente {\n    return db.clientes.require(id)\n}", "buscar"),
            "(ClienteId) -> Cliente"
        );
        assert_eq!(db_scheme("server fun talvez(id: ClienteId): Cliente? = db.clientes.find(id)", "talvez"), "(ClienteId) -> Cliente?");
        assert_eq!(
            db_scheme("server fun criar(nome: String): Cliente {\n    return db.clientes.insert {\n        nome = nome\n        apelido = null\n    }\n}", "criar"),
            "(String) -> Cliente"
        );
    }

    #[test]
    fn database_errors() {
        assert_eq!(with_db("server fun f(id: ClienteId): Cliente = db.pedidos.require(id)"), Err(vec!["E7104".into()]));
        assert_eq!(with_db("server fun f(id: String): Cliente = db.clientes.require(id)"), Err(vec!["E7101".into()]));
        assert_eq!(with_db("server fun f(n: Int): Cliente = db.clientes.insert { nome = n }"), Err(vec!["E7101".into()]));
        assert_eq!(with_db("server fun f(n: String): Cliente = db.clientes.insert { idade = n }"), Err(vec!["E7104".into()]));
        assert_eq!(with_db("server fun f(id: ClienteId): Cliente = db.clientes.delete(id)"), Err(vec!["E7004".into()]));
        assert_eq!(with_db("fun f(c: Cliente) = c.nome { x = 1 }"), Err(vec!["E7004".into()]));
    }

    #[test]
    fn server_calls_and_environment_values() {
        let source = "environment development {\n    secret val token: String = env(\"T\", default = \"x\")\n    runtime val porta: Int = envOrNull(\"PORT\")\n}\nserver fun eco(x: Int): Int = x\nclient fun usa(): Int = server.eco(1)\nclient fun segredo(): String = token\nfun porta2() = porta ?: 80\n";
        assert_eq!(scheme(source, "usa"), "() -> Int");
        assert_eq!(scheme(source, "segredo"), "() -> String");
        assert_eq!(scheme(source, "porta2"), "() -> Int");
        assert_eq!(errors("client fun f(): Int = server.nada(1)"), vec!["E7103"]);
    }

    #[test]
    fn ui_trees() {
        let page = "data class Item(val nome: String, val preco: Money)\nclient component Pagina(item: Item) {\n    UI {\n        Section(\"hero\", \"top\") {\n            Heading(1, \"titulo\") { Text(item.nome) }\n            Text(item.preco)\n        }\n    }\n}\n";
        assert_eq!(scheme(page, "Pagina"), "(Item) -> Ui");
        assert_eq!(errors("client component P(xs: List<String>) {\n    UI { Text(xs) }\n}"), vec!["E7105"]);
        assert_eq!(errors("client component P() {\n    UI { Div(true) { Text(\"a\") } }\n}"), vec!["E7101"]);
        assert_eq!(errors("client component P() {\n    1\n}"), vec!["E7101"]);
        assert_eq!(errors("client component P() {\n    UI { 1 }\n}"), vec!["E7101"]);
        assert_eq!(errors("fun f(x: Int) = f(x) { Text(\"a\") }"), vec!["E7004"]);
    }

    #[test]
    fn server_only_names_program_functions_and_names_do_not_collide() {
        assert_eq!(errors("fun f() = server.first"), vec!["E7103"]);
        let env = "environment development {\n    secret val token: String = env(\"T\", default = \"x\")\n}\n";
        assert_eq!(errors(&format!("{env}fun f() = server.token")), vec!["E7103"]);
        assert_eq!(errors(&format!("{env}fun token(): Int = 1")), vec!["E7110"]);
    }

    #[test]
    fn unannotated_ui_arguments_get_a_hint() {
        assert_eq!(errors("client component P(x) {\n    UI { Text(x) }\n}"), vec!["E7003", "E7105"]);
    }

    #[test]
    fn user_names_shadow_bind_forms() {
        assert_eq!(scheme("fun Text(x: Int): Int = x\nfun f() = Text(1)", "f"), "() -> Int");
        assert_eq!(scheme("data class D(val clientes: Int)\nfun f(db: D) = db.clientes", "f"), "(D) -> Int");
        assert_eq!(scheme("data class Item(val n: Int)\nfun f() = Item(1)", "f"), "() -> Item");
        assert_eq!(scheme("data class Link(val u: Url)\nfun f(u: Url) = Link(u)", "f"), "(Url) -> Link");
        assert_eq!(scheme("data class S(val eco: Int)\nfun f(server: S) = server.eco", "f"), "(S) -> Int");
    }
}
