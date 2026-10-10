//! Hindley-Milner inference with level-based generalization (Rémy), as in OCaml's compiler.
//!
//! Bind is stricter than OCaml: operator constraints must resolve to a concrete type,
//! discarded non-`Unit` values are errors and `when` must be exhaustive.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{Function, Program, TypeKind, TypeRef};
use crate::diagnostic::{Diagnostic, Span};
use crate::parser::expr::parse_type_text;
use crate::syntax::expr::*;

use super::env::{Env, rigid_vars};
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

pub fn check_program(program: &Program, env: &Env) -> (TypedProgram, Vec<Diagnostic>) {
    let mut checker = Checker::new(env);
    checker.run(program);
    let typed = checker.finish(program);
    (typed, checker.diagnostics)
}

struct Checker<'a> {
    env: &'a Env,
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
    fn new(env: &'a Env) -> Self {
        Self {
            env,
            table: TypeTable::default(),
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
        for ((function, ty), scope) in group.iter().zip(&types).zip(scopes) {
            self.rigid = scope;
            self.check_body(function, ty);
        }
        self.rigid.clear();
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
            UnifyError::Constraint { constraint, ty } => Diagnostic::error(
                "E7301",
                format!("`{}` does not support {}.", display(&ty), constraint.describe()),
                span.clone(),
            ),
        };
        self.diagnostics.push(diagnostic);
    }

    fn require(&mut self, constraint: Constraint, ty: &Type, span: &Span) {
        if let Err(UnifyError::Constraint { constraint, ty }) = constrain(&mut self.table, constraint, ty) {
            self.error("E7301", format!("`{}` does not support {}.", display(&ty), constraint.describe()), span);
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
                self.unify_at(&Type::option(inner.clone()), &receiver, &target.span, Some("the receiver of `?.`".into()));
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
            ExprKind::Lambda { params, body } => {
                let mark = self.locals.len();
                let mut names = BTreeSet::new();
                let mut param_types = Vec::new();
                for (name, span) in params {
                    if !names.insert(name.as_str()) {
                        self.error("E7113", format!("`{name}` is bound more than once in this lambda."), span);
                    }
                    let ty = self.fresh();
                    param_types.push(ty.clone());
                    self.locals.push((name.clone(), Scheme::mono(ty)));
                }
                self.lambda_depth += 1;
                let result = self.infer_block(body);
                self.lambda_depth -= 1;
                self.locals.truncate(mark);
                Type::Fun(param_types, Box::new(result))
            }
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
            ExprKind::Builder { fields, .. } => self.infer_builder(fields, &expr.span),
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
        self.error("E7103", format!("Unknown name `{name}`."), span);
        Type::Error
    }

    fn constructor_value(&mut self, name: &str) -> Type {
        let (fields, result) = self.env.instantiate_ctor(name, &mut self.table, self.level);
        if fields.is_empty() { result } else { Type::Fun(fields, Box::new(result)) }
    }

    fn infer_call(&mut self, callee: &Expr, args: &[Expr], children: Option<&Block>, span: &Span) -> Type {
        if let ExprKind::Name(name) = &callee.kind
            && name == "listOf"
            && !self.is_shadowed(name)
        {
            return self.infer_list_of(args, children, span);
        }
        if let Some(block) = children {
            self.error("E7004", "Only UI nodes accept a child block.", &block.span);
        }
        let callee_ty = self.infer(callee);
        let arg_types: Vec<Type> = args.iter().map(|arg| self.infer(arg)).collect();
        match self.table.shallow(&callee_ty) {
            Type::Fun(params, result) => {
                if params.len() != args.len() {
                    self.error(
                        "E7102",
                        format!("This function expects {} argument(s), but {} were given.", params.len(), args.len()),
                        span,
                    );
                    return *result;
                }
                let reason = self.parameter_reason(callee);
                for ((param, arg_ty), arg) in params.iter().zip(&arg_types).zip(args) {
                    self.unify_at(param, arg_ty, &arg.span, reason.clone());
                }
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

    fn infer_builder(&mut self, fields: &[FieldInit], span: &Span) -> Type {
        for field in fields {
            self.infer(&field.value);
        }
        self.error("E7004", "`{ field = value }` blocks are only supported on `db.<table>.insert`.", span);
        Type::Error
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
    fn boundaries_require_annotations() {
        assert_eq!(errors("server fun f(x) = x"), vec!["E7003"]);
        assert_eq!(errors("external fun g(x: Int)"), vec!["E7003"]);
    }
}
