//! Hindley-Milner inference with level-based generalization (Rémy), as in OCaml's compiler.
//!
//! Bind is stricter than OCaml: operator constraints must resolve to a concrete type,
//! discarded non-`Unit` values are errors and `when` must be exhaustive.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{Domain, Function, Program, TypeKind, TypeRef};
use crate::diagnostic::{Diagnostic, Span};
use crate::parser::expr::parse_type_text;
use crate::schema::{Column, DatabaseSchema};
use crate::syntax::expr::*;

use super::effects::{DB_READ, DB_WRITE, Label, Row, RowState, RowVarId, SERVER_CALL};
use super::env::{Env, UI_NODES, rigid_vars};
use super::exhaustive::{self, Pat};
use super::ty::*;
use super::unify::{UnifyError, constrain, unify, unify_rows};

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
    /// Elaborated patterns of each `when`, keyed by function and the `when` node, with the
    /// optional layers made explicit (`Some(...)`), so later passes need not re-derive them.
    pub patterns: BTreeMap<(String, NodeId), Vec<Pat>>,
    /// Resolved effect labels of each function (e.g. `["db.read"]`), sorted.
    pub effects: BTreeMap<String, Vec<String>>,
}

pub fn check_program(
    program: &Program,
    env: &Env,
    schema: &DatabaseSchema,
) -> (TypedProgram, Vec<Diagnostic>) {
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
    patterns: BTreeMap<(String, NodeId), Vec<Pat>>,
    diagnostics: Vec<Diagnostic>,
    /// Effects performed so far by the body being checked (the function's or a lambda's).
    context: Row,
    /// Where each effect of each function first comes from.
    effect_origins: BTreeMap<String, BTreeMap<Label, Origin>>,
    /// Origins and calls of the bodies being checked: the function's, then one per enclosing
    /// lambda. A lambda's record is kept apart until it is known to run (see `settle_origins`).
    records: Vec<EffectRecord>,
    /// Records of the lambdas of the current function, with their rows.
    lambda_records: Vec<(Row, EffectRecord)>,
    /// Row variables made by instantiating a local's scheme, mapped to the quantified variable
    /// they stand for, so a `val` lambda that is called is recognized.
    row_instances: Vec<(RowVarId, RowVarId)>,
    /// Orders origins and calls across records, so merging keeps the first one.
    sequence: usize,
    /// Effect variables of the signature whose body is being checked.
    effect_scope: BTreeMap<String, Row>,
    /// Domain of the body being checked (`Inferred` outside bodies).
    domain: Domain,
    /// Declared domain of each function, to recognize server endpoints used by clients.
    function_domains: BTreeMap<String, Domain>,
    /// `server fun`s that are not components: the only targets of `server.f`.
    endpoints: BTreeSet<String>,
    /// Calls each function makes to program functions, in order, for effect paths.
    calls: BTreeMap<String, Vec<(String, Span)>>,
}

/// One entry of the path from a function down to the operation that produces an effect.
struct Step {
    name: String,
    span: Span,
    operation: bool,
}

impl Step {
    fn render(&self) -> String {
        format!(
            "{} ({}:{}:{})",
            self.name, self.span.file, self.span.line, self.span.column
        )
    }
}

/// Origins and program calls of one body, each numbered in order of appearance.
#[derive(Default)]
struct EffectRecord {
    origins: BTreeMap<Label, (usize, Origin)>,
    calls: Vec<(usize, String, Span)>,
}

impl EffectRecord {
    /// Adds `other`, keeping the earliest origin of each label and the calls in order.
    fn merge(&mut self, other: EffectRecord) {
        for (label, (order, origin)) in other.origins {
            match self.origins.get(&label) {
                Some((existing, _)) if *existing < order => {}
                _ => {
                    self.origins.insert(label, (order, origin));
                }
            }
        }
        self.calls.extend(other.calls);
        self.calls.sort_by_key(|(order, _, _)| *order);
    }
}

/// Where a function's effect first comes from, for diagnostics that show the path.
#[derive(Clone, Debug)]
enum Origin {
    Operation { text: String, span: Span },
    Call { callee: String, span: Span },
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
            patterns: BTreeMap::new(),
            diagnostics: Vec::new(),
            context: Row::empty(),
            effect_origins: BTreeMap::new(),
            records: Vec::new(),
            lambda_records: Vec::new(),
            row_instances: Vec::new(),
            sequence: 0,
            effect_scope: BTreeMap::new(),
            domain: Domain::Inferred,
            function_domains: BTreeMap::new(),
            endpoints: BTreeSet::new(),
            calls: BTreeMap::new(),
        }
    }

    // ---- program level -------------------------------------------------------------

    fn run(&mut self, program: &Program) {
        self.install_builtins();
        self.install_environment(program);
        for function in &program.functions {
            self.function_spans
                .insert(function.name.clone(), function.span.clone());
            self.function_domains
                .insert(function.name.clone(), function.domain.clone());
            if function.domain == Domain::Server && !function.component {
                self.endpoints.insert(function.name.clone());
            }
        }
        let (annotated, inferred): (Vec<&Function>, Vec<&Function>) = program
            .functions
            .iter()
            .partition(|function| is_fully_annotated(function));
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
            if program
                .functions
                .iter()
                .any(|function| function.name == declaration.name)
            {
                self.error(
                    "E7110",
                    format!(
                        "`{}` is declared both as an environment value and as a function.",
                        declaration.name
                    ),
                    &declaration.span,
                );
            }
            let base = self.lower_ref(
                &declaration.type_ref,
                &declaration.span,
                &BTreeMap::new(),
                &BTreeMap::new(),
                true,
            );
            let ty = if declaration.optional {
                Type::option(base)
            } else {
                base
            };
            self.globals
                .insert(declaration.name.clone(), Scheme::mono(ty));
        }
    }

    fn install_builtins(&mut self) {
        let table = &mut self.table;
        let list = |ty: &Type| Type::list(ty.clone());
        let builtins = [
            (
                "first",
                builtin(table, 1, 0, |v, _| {
                    Type::Fun(
                        vec![list(&v[0])],
                        Box::new(Type::option(v[0].clone())),
                        Row::empty(),
                    )
                }),
            ),
            (
                "get",
                builtin(table, 1, 0, |v, _| {
                    Type::Fun(
                        vec![list(&v[0]), Type::con("Int")],
                        Box::new(Type::option(v[0].clone())),
                        Row::empty(),
                    )
                }),
            ),
            (
                "length",
                builtin(table, 1, 0, |v, _| {
                    Type::Fun(vec![list(&v[0])], Box::new(Type::con("Int")), Row::empty())
                }),
            ),
            (
                "map",
                builtin(table, 2, 1, |v, r| {
                    let mapper = Type::Fun(
                        vec![v[0].clone()],
                        Box::new(v[1].clone()),
                        Row::with_tail([], r[0]),
                    );
                    Type::Fun(
                        vec![list(&v[0]), mapper],
                        Box::new(list(&v[1])),
                        Row::with_tail([], r[0]),
                    )
                }),
            ),
            (
                "fold",
                builtin(table, 2, 1, |v, r| {
                    let step = Type::Fun(
                        vec![v[1].clone(), v[0].clone()],
                        Box::new(v[1].clone()),
                        Row::with_tail([], r[0]),
                    );
                    Type::Fun(
                        vec![list(&v[0]), v[1].clone(), step],
                        Box::new(v[1].clone()),
                        Row::with_tail([], r[0]),
                    )
                }),
            ),
            (
                "emptyList",
                builtin(table, 1, 0, |v, _| {
                    Type::Fun(Vec::new(), Box::new(list(&v[0])), Row::empty())
                }),
            ),
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
        let mut effects = BTreeMap::new();
        let mut row_vars = Vec::new();
        for name in effect_variable_names(function) {
            let var = self.table.fresh_row_var(GENERIC_LEVEL);
            row_vars.push(var);
            effects.insert(name, Row::with_tail([], var));
        }
        let ty = self.signature(function, &vars, &effects, true);
        Scheme {
            vars: ids,
            row_vars,
            ty,
        }
    }

    /// Function type from the declaration; missing annotations become fresh variables.
    fn signature(
        &mut self,
        function: &Function,
        vars: &BTreeMap<String, Type>,
        effects: &BTreeMap<String, Row>,
        report: bool,
    ) -> Type {
        let mut params = Vec::new();
        for parameter in &function.parameters {
            let ty = match &parameter.type_ref {
                Some(type_ref) => self.lower_ref(type_ref, &function.span, vars, effects, report),
                None => self.fresh(),
            };
            params.push(ty);
        }
        let result = if function.component {
            Type::con("Ui")
        } else {
            match &function.return_type {
                Some(type_ref) => self.lower_ref(type_ref, &function.span, vars, effects, report),
                None => self.fresh(),
            }
        };
        let row = if effects_known(function) {
            let mut sink = Vec::new();
            let diagnostics = if report {
                &mut self.diagnostics
            } else {
                &mut sink
            };
            // Without an `effects [...]` clause, the effect variables of the parameters are part
            // of the row: effect variables are polymorphic in every domain, and the caller is
            // charged for whatever it instantiates them with.
            let mut names = function.effects.clone();
            if !function.effects_declared {
                names.extend(parameter_effect_variables(function));
            }
            let mut row = self
                .env
                .lower_row(&names, effects, &function.span, diagnostics);
            if let Some(ceiling) = ceiling(&function.domain) {
                let outside: Vec<Label> = row
                    .labels
                    .iter()
                    .filter(|label| !label.is_rigid() && !ceiling.contains(label))
                    .cloned()
                    .collect();
                for label in outside {
                    if report {
                        let what = domain_description(&function.domain);
                        self.error(
                            "E7401",
                            format!(
                                "`{}` is {what} and cannot declare `{label}`.",
                                function.name
                            ),
                            &function.span,
                        );
                    }
                    // Dropped, so the body is not also told it never performs it (E7403).
                    row.labels.remove(&label);
                }
            }
            row
        } else {
            self.table.fresh_row(self.level)
        };
        Type::Fun(params, Box::new(result), row)
    }

    fn lower_ref(
        &mut self,
        type_ref: &TypeRef,
        span: &Span,
        vars: &BTreeMap<String, Type>,
        effects: &BTreeMap<String, Row>,
        report: bool,
    ) -> Type {
        let mut sink = Vec::new();
        let diagnostics = if report {
            &mut self.diagnostics
        } else {
            &mut sink
        };
        match parse_type_text(&type_ref.display(), span) {
            Ok(te) => self.env.lower_with_effects(&te, vars, effects, diagnostics),
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
        let mut effect_scopes = Vec::new();
        let mut rigid_names = BTreeSet::new();
        for function in group {
            let mut scope = BTreeMap::new();
            for name in &function.type_params {
                scope.insert(name.clone(), self.fresh());
            }
            // Effect variables are flexible, like type parameters, unless the function's effects
            // are declared: then, as in annotated bodies, each one only equals itself inside the
            // body (so the body cannot add concrete labels to it), and it is released into a
            // generic row variable once the group is checked.
            let rigid = effects_known(function);
            let effects: BTreeMap<String, Row> = effect_variable_names(function)
                .into_iter()
                .map(|name| {
                    let row = if rigid {
                        rigid_names.insert(name.clone());
                        Row::closed([Label::rigid(&name)])
                    } else {
                        self.table.fresh_row(self.level)
                    };
                    (name, row)
                })
                .collect();
            let ty = self.signature(function, &scope, &effects, true);
            self.globals
                .insert(function.name.clone(), Scheme::mono(ty.clone()));
            types.push(ty);
            scopes.push(scope);
            effect_scopes.push(effects);
        }
        // Declared effects are compared only after every body of the group is checked: the
        // members share rows through their calls, and closing one member's row earlier would
        // reject a sibling whose effects are not inferred yet.
        let mut produced = Vec::new();
        for (((function, ty), scope), effects) in
            group.iter().zip(&types).zip(&scopes).zip(effect_scopes)
        {
            self.rigid = scope.clone();
            self.effect_scope = effects;
            produced.push(self.check_body(function, ty));
        }
        for ((function, ty), context) in group.iter().zip(&types).zip(produced) {
            if let (Some(context), Type::Fun(_, _, declared)) = (context, ty) {
                self.check_effects(function, declared, &context);
            }
        }
        self.rigid.clear();
        for (function, scope) in group.iter().zip(&scopes) {
            self.check_declared_generics(function, scope);
        }
        for function in group {
            self.report_ambiguous(&function.name);
        }
        let generic: BTreeMap<String, RowVarId> = rigid_names
            .into_iter()
            .map(|name| (name, self.table.fresh_row_var(self.level)))
            .collect();
        let types: Vec<Type> = if generic.is_empty() {
            types
        } else {
            types
                .iter()
                .map(|ty| self.release_rigid(ty, &generic))
                .collect()
        };
        self.level -= 1;
        self.close_top_rows(&types);
        for (function, ty) in group.iter().zip(&types) {
            let (scheme, ambiguous) = generalize(&self.table, self.level, ty);
            self.report_constraints(ambiguous, &function.span);
            self.globals.insert(function.name.clone(), scheme);
        }
    }

    /// Replaces each rigid effect variable of a checked group by its generic row variable, so
    /// the published scheme is polymorphic in it. A row that ends up naming several variables
    /// (or a variable and another tail) joins them, which is less general but sound.
    fn release_rigid(&mut self, ty: &Type, generic: &BTreeMap<String, RowVarId>) -> Type {
        match self.table.resolve(ty) {
            Type::Fun(params, result, row) => {
                let params = params
                    .iter()
                    .map(|param| self.release_rigid(param, generic))
                    .collect();
                let result = self.release_rigid(&result, generic);
                let row = self.release_row(&row, generic);
                Type::Fun(params, Box::new(result), row)
            }
            Type::Con(name, args) => Type::Con(
                name,
                args.iter()
                    .map(|arg| self.release_rigid(arg, generic))
                    .collect(),
            ),
            other => other,
        }
    }

    fn release_row(&mut self, row: &Row, generic: &BTreeMap<String, RowVarId>) -> Row {
        let row = self.table.resolve_row(row);
        let mut labels = BTreeSet::new();
        let mut tails = Vec::new();
        for label in row.labels {
            match label.rigid_name().and_then(|name| generic.get(name)) {
                Some(var) => tails.push(*var),
                None => {
                    labels.insert(label);
                }
            }
        }
        let Some(&first) = tails.first() else {
            return Row {
                labels,
                tail: row.tail,
            };
        };
        tails.extend(row.tail);
        for other in &tails[1..] {
            // Two open rows without labels always unify.
            let _ = unify_rows(
                &mut self.table,
                &Row::with_tail([], first),
                &Row::with_tail([], *other),
            );
        }
        self.table.resolve_row(&Row::with_tail(labels, first))
    }

    /// Declared type parameters of a partially annotated function are flexible during
    /// inference, but the body must not specialize them or make two of them equal.
    fn check_declared_generics(&mut self, function: &Function, scope: &BTreeMap<String, Type>) {
        let mut seen: BTreeMap<TypeVarId, &str> = BTreeMap::new();
        for name in &function.type_params {
            let Some(var) = scope.get(name) else { continue };
            let shallow = self.table.shallow(var);
            // An operator constraint left on the variable specializes the parameter just as a
            // concrete type would (e.g. `a == b` on a declared `T`).
            if let Type::Var(id) = shallow
                && let VarState::Unbound {
                    constraint: Some(constraint),
                    ..
                } = self.table.state(id).clone()
            {
                self.error(
                    "E7301",
                    format!(
                        "Type parameter `{name}` of `{}` is declared generic, but the body requires {}.",
                        function.name,
                        constraint.describe()
                    ),
                    &function.span,
                );
                self.table.set(id, VarState::Link(Type::Error));
                continue;
            }
            let requirement = match shallow {
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
        // Inside the body, an effect variable only equals itself (like a rigid type parameter).
        let effects: BTreeMap<String, Row> = effect_variable_names(function)
            .into_iter()
            .map(|name| {
                let row = Row::closed([Label::rigid(&name)]);
                (name, row)
            })
            .collect();
        let ty = self.signature(function, &rigid, &effects, false);
        self.effect_scope = effects;
        if let (Some(context), Type::Fun(_, _, declared)) = (self.check_body(function, &ty), &ty) {
            self.check_effects(function, declared, &context);
        }
        self.report_ambiguous(&function.name);
        self.rigid.clear();
        self.level -= 1;
    }

    /// Checks a body and returns the row it produced (the function's own row when its effects
    /// are inferred), to be checked against the domain and the declaration by the caller.
    fn check_body(&mut self, function: &Function, ty: &Type) -> Option<Row> {
        let Type::Fun(params, result, row) = ty else {
            return None;
        };
        let mut names = BTreeSet::new();
        for parameter in &function.parameters {
            if !names.insert(parameter.name.as_str()) {
                self.error(
                    "E7113",
                    format!(
                        "`{}` is declared more than once in the parameters of `{}`.",
                        parameter.name, function.name
                    ),
                    &function.span,
                );
            }
        }
        let block = function.expr.as_ref()?;
        self.function = function.name.clone();
        self.domain = function.domain.clone();
        self.return_type = Some((**result).clone());
        self.locals.clear();
        for (parameter, param) in function.parameters.iter().zip(params) {
            self.locals
                .push((parameter.name.clone(), Scheme::mono(param.clone())));
        }
        let declared = effects_known(function);
        self.context = if declared {
            self.table.fresh_row(self.level)
        } else {
            row.clone()
        };
        self.records = vec![EffectRecord::default()];
        self.lambda_records.clear();
        self.row_instances.clear();
        let body = self.infer_block(block);
        let reason = if function.component {
            "a component body, which must end with a UI tree".to_string()
        } else {
            format!("the declared result of `{}`", function.name)
        };
        self.unify_at(result, &body, &result_span(block), Some(reason));
        self.locals.clear();
        self.return_type = None;
        let context = std::mem::replace(&mut self.context, Row::empty());
        self.settle_origins(&context);
        self.effect_scope.clear();
        self.domain = Domain::Inferred;
        Some(context)
    }

    fn finish(&self, program: &Program) -> TypedProgram {
        let mut typed = TypedProgram::default();
        for function in &program.functions {
            if let Some(scheme) = self.globals.get(&function.name) {
                let ty = self.table.resolve(&scheme.ty);
                typed.schemes.insert(function.name.clone(), display(&ty));
                let labels = match &ty {
                    Type::Fun(_, _, row) => row
                        .labels
                        .iter()
                        .filter(|label| !label.is_rigid())
                        .map(Label::to_string)
                        .collect(),
                    _ => Vec::new(),
                };
                typed.effects.insert(function.name.clone(), labels);
                typed.signatures.insert(function.name.clone(), ty);
            }
        }
        typed.node_types = self
            .nodes
            .iter()
            .map(|(key, (ty, _))| (key.clone(), self.table.resolve(ty)))
            .collect();
        typed.patterns = self.patterns.clone();
        typed
    }

    // ---- effects ---------------------------------------------------------------------

    /// An operation's effect joins the current context row.
    fn perform(&mut self, pair: (&str, &str), text: String, span: &Span) {
        let label = Label::of(pair);
        let tail = self.table.fresh_row_var(self.level);
        let produced = Row::with_tail([label.clone()], tail);
        let context = self.context.clone();
        match unify_rows(&mut self.table, &context, &produced) {
            Ok(()) => self.note_origin(
                label,
                Origin::Operation {
                    text,
                    span: span.clone(),
                },
            ),
            Err(_) => self.effect_mismatch(span),
        }
    }

    /// Calling a function brings its row into the current context. A closed row is opened
    /// first, so a pure callee never closes the caller's row. `origin` (a call to a program
    /// function, or a `server.f` call) is recorded for the labels the row already has, so
    /// diagnostics can show the path to an effect.
    fn absorb(&mut self, row: &Row, origin: Option<Origin>, span: &Span) {
        let resolved = self.table.resolve_row(row);
        let opened = match resolved.tail {
            Some(_) => resolved.clone(),
            None => {
                let tail = self.table.fresh_row_var(self.level);
                Row::with_tail(resolved.labels.iter().cloned(), tail)
            }
        };
        let context = self.context.clone();
        match unify_rows(&mut self.table, &context, &opened) {
            Ok(()) => {
                if let Some(origin) = &origin {
                    for label in &resolved.labels {
                        self.note_origin(label.clone(), origin.clone());
                    }
                }
                if let Some(Origin::Call { callee, span }) = origin {
                    self.sequence += 1;
                    let order = self.sequence;
                    if let Some(record) = self.records.last_mut() {
                        record.calls.push((order, callee, span));
                    }
                }
            }
            Err(_) => self.effect_mismatch(span),
        }
    }

    /// Context rows are always open and carry no ceiling, so joining effects cannot fail;
    /// this only guards against a broken invariant.
    fn effect_mismatch(&mut self, span: &Span) {
        self.error(
            "E7101",
            format!("Effect mismatch in `{}`.", self.function),
            span,
        );
    }

    fn note_origin(&mut self, label: Label, origin: Origin) {
        self.sequence += 1;
        let order = self.sequence;
        if let Some(record) = self.records.last_mut() {
            record.origins.entry(label).or_insert((order, origin));
        }
    }

    /// At the end of a body: the records of the lambdas whose rows flowed into the body's row
    /// (they were called, directly or through the functions they were passed to) join the
    /// function's record; the others never run and leave no origin behind.
    fn settle_origins(&mut self, context: &Row) {
        let mut record = self.records.pop().unwrap_or_default();
        let target = self.table.resolve_row(context).tail;
        for (row, lambda) in std::mem::take(&mut self.lambda_records) {
            if target.is_some() && self.reaches(&row, target) {
                record.merge(lambda);
            }
        }
        self.row_instances.clear();
        self.records.clear();
        self.effect_origins.insert(
            self.function.clone(),
            record
                .origins
                .into_iter()
                .map(|(label, (_, origin))| (label, origin))
                .collect(),
        );
        self.calls.insert(
            self.function.clone(),
            record
                .calls
                .into_iter()
                .map(|(_, callee, span)| (callee, span))
                .collect(),
        );
    }

    /// Whether `row` (or an instance of it made by using a `val`) now ends in `target`.
    fn reaches(&self, row: &Row, target: Option<RowVarId>) -> bool {
        let Some(start) = self.table.resolve_row(row).tail else {
            return false;
        };
        let mut seen = BTreeSet::from([start]);
        let mut frontier = vec![start];
        while let Some(current) = frontier.pop() {
            if self.table.resolve_row(&Row::with_tail([], current)).tail == target {
                return true;
            }
            for (instance, quantified) in &self.row_instances {
                let quantified = self
                    .table
                    .resolve_row(&Row::with_tail([], *quantified))
                    .tail;
                if quantified == Some(current) && seen.insert(*instance) {
                    frontier.push(*instance);
                }
            }
        }
        false
    }

    /// The origin to record for a call: program functions (non-local names) as calls,
    /// `server.f` as an operation; builtins and locals are transparent.
    fn call_origin(&self, callee: &Expr, span: &Span) -> Option<Origin> {
        match &callee.kind {
            ExprKind::Name(name)
                if !self.is_local(name) && self.function_spans.contains_key(name) =>
            {
                Some(Origin::Call {
                    callee: name.clone(),
                    span: span.clone(),
                })
            }
            ExprKind::Field { target, name }
                if matches!(&target.kind, ExprKind::Name(base) if base == "server" && !self.is_shadowed(base))
                    && self.function_spans.contains_key(name) =>
            {
                Some(Origin::Operation {
                    text: format!("server.{name}"),
                    span: span.clone(),
                })
            }
            _ => None,
        }
    }

    /// After a body: every concrete label it produced must fit its domain's ceiling (effect
    /// variables are exempt; the caller is charged when it instantiates them). Then a
    /// function whose effects are known is compared with its declaration.
    fn check_effects(&mut self, function: &Function, declared: &Row, produced: &Row) {
        if let Some(ceiling) = ceiling(&function.domain) {
            let resolved = self.table.resolve_row(produced);
            for label in &resolved.labels {
                if !label.is_rigid() && !ceiling.contains(label) {
                    self.ceiling_error(function, label);
                }
            }
        }
        if effects_known(function) {
            self.check_declared_effects(function, declared, produced);
        }
    }

    /// E7401: `function` produced `label`, outside its domain's ceiling.
    fn ceiling_error(&mut self, function: &Function, label: &Label) {
        let what = domain_description(&function.domain);
        let steps = self.effect_trace(&function.name, label);
        let mut path = vec![function.name.clone()];
        path.extend(steps.iter().map(Step::render));
        let hint = self.ceiling_hint(&function.domain, label, &steps);
        let diagnostic = Diagnostic::error(
            "E7401",
            format!(
                "`{}` is {what} and cannot perform `{label}`.",
                function.name
            ),
            function.span.clone(),
        )
        .with_path(path);
        self.diagnostics.push(match hint {
            Some(hint) => diagnostic.with_hint(hint),
            None => diagnostic,
        });
    }

    fn ceiling_hint(&self, domain: &Domain, label: &Label, steps: &[Step]) -> Option<String> {
        match domain {
            Domain::Pure => Some("`pure` functions cannot perform effects.".into()),
            Domain::Server if label == &Label::of(SERVER_CALL) => {
                let called = steps
                    .last()
                    .filter(|step| step.operation)
                    .and_then(|step| step.name.strip_prefix("server."));
                Some(match called {
                    Some(name) => {
                        format!("Call `{name}(...)` directly; `server.` is only for client code.")
                    }
                    None => "Call the function directly; `server.` is only for client code.".into(),
                })
            }
            Domain::Client if label.to_string().starts_with("db.") => {
                let endpoint = steps.iter().find(|step| {
                    !step.operation
                        && self.function_domains.get(&step.name) == Some(&Domain::Server)
                });
                Some(match endpoint {
                    Some(step) => format!("Call it through `server.{}(...)`.", step.name),
                    None => "Database access runs on the server: move it into a `server fun` and call it with `server.f(...)`.".into(),
                })
            }
            _ => None,
        }
    }

    /// Follows recorded origins from `start` down to the operation that produces `label`.
    /// A call made before the callee's effects were known (recursion) is found by looking at
    /// the callee's row now.
    fn effect_trace(&self, start: &str, label: &Label) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut current = start.to_string();
        let mut seen = BTreeSet::new();
        while seen.insert(current.clone()) {
            let origin = self
                .effect_origins
                .get(&current)
                .and_then(|origins| origins.get(label))
                .cloned()
                .or_else(|| self.later_call(&current, label));
            match origin {
                Some(Origin::Call { callee, span }) => {
                    steps.push(Step {
                        name: callee.clone(),
                        span,
                        operation: false,
                    });
                    current = callee;
                }
                Some(Origin::Operation { text, span }) => {
                    steps.push(Step {
                        name: text,
                        span,
                        operation: true,
                    });
                    break;
                }
                None => break,
            }
        }
        steps
    }

    /// The first call made by `function` whose callee's row now contains `label`.
    fn later_call(&self, function: &str, label: &Label) -> Option<Origin> {
        self.calls.get(function)?.iter().find_map(|(callee, span)| {
            let scheme = self.globals.get(callee)?;
            let Type::Fun(_, _, row) = &scheme.ty else {
                return None;
            };
            self.table
                .resolve_row(row)
                .labels
                .contains(label)
                .then(|| Origin::Call {
                    callee: callee.clone(),
                    span: span.clone(),
                })
        })
    }

    /// Opens the top row of an instantiated function type, so a function whose effects are
    /// fixed can still be used where more effects are allowed.
    fn open_top(&mut self, ty: Type) -> Type {
        let Type::Fun(params, result, row) = ty else {
            return ty;
        };
        let row = self.table.resolve_row(&row);
        if row.tail.is_some() {
            return Type::Fun(params, result, row);
        }
        let tail = self.table.fresh_row_var(self.level);
        Type::Fun(params, result, Row::with_tail(row.labels, tail))
    }

    /// Before generalization, a row variable that occurs only in the own rows of a group's
    /// functions stands for nothing more: close it, so schemes read `! {db.read}` instead of
    /// `! {db.read | 'e}`. Occurrences are counted across every member of the group, because
    /// the members share rows through their calls (closing a tail that also appears in a
    /// sibling's parameter would make that parameter pure). Rows of returned functions stay
    /// open (a caller may still join them with other effects, e.g. two branches returning
    /// different closures); the display hides them when they are unconstrained. Variables in
    /// parameters are never closed (that would lose generality).
    fn close_top_rows(&mut self, types: &[Type]) {
        let mut occurrences: BTreeMap<RowVarId, (usize, bool)> = BTreeMap::new();
        let mut top: BTreeMap<RowVarId, usize> = BTreeMap::new();
        for ty in types {
            let resolved = self.table.resolve(ty);
            row_occurrences(&resolved, true, &mut occurrences);
            if let Type::Fun(_, _, row) = &resolved
                && let Some(tail) = row.tail
            {
                *top.entry(tail).or_default() += 1;
            }
        }
        for (tail, count) in top {
            if occurrences
                .get(&tail)
                .is_some_and(|(total, _)| *total == count)
                && let RowState::Unbound { level } = self.table.row_state(tail).clone()
                && level > self.level
            {
                self.table.set_row(tail, RowState::Link(Row::empty()));
            }
        }
    }

    /// `effects [...]` must equal what the body performs.
    fn check_declared_effects(&mut self, function: &Function, declared: &Row, produced: &Row) {
        let produced = self.table.resolve_row(produced);
        let declared = self.table.resolve_row(declared);
        let listed: Vec<String> = declared.labels.iter().map(Label::to_string).collect();
        let ceiling = ceiling(&function.domain);
        let outside = |label: &Label| {
            ceiling
                .as_ref()
                .is_some_and(|ceiling| !label.is_rigid() && !ceiling.contains(label))
        };
        // Labels outside the ceiling were already reported as E7401.
        for label in produced
            .labels
            .difference(&declared.labels)
            .filter(|label| !outside(label))
        {
            let mut path = vec![function.name.clone()];
            path.extend(
                self.effect_trace(&function.name, label)
                    .iter()
                    .map(Step::render),
            );
            self.diagnostics.push(
                Diagnostic::error(
                    "E7402",
                    format!(
                        "`{}` performs `{label}`, but its effects declare only [{}].",
                        function.name,
                        listed.join(", ")
                    ),
                    function.span.clone(),
                )
                .with_path(path)
                .with_hint(self.add_effect_hint(&function.name, label)),
            );
        }
        for label in declared.labels.difference(&produced.labels) {
            // Effect variables included implicitly (no `effects [...]` clause) are not a
            // promise to perform them.
            if label.is_rigid() && !function.effects_declared {
                continue;
            }
            self.diagnostics.push(
                Diagnostic::error(
                    "E7403",
                    format!(
                        "`{}` declares `{label}`, but never performs it.",
                        function.name
                    ),
                    function.span.clone(),
                )
                .with_hint(format!("Remove `{label}` from `effects [...]`.")),
            );
        }
        // The body's row ends here, unless it is the declaration's own effect variable.
        if let Some(tail) = produced.tail
            && produced.tail != declared.tail
            && matches!(self.table.row_state(tail), RowState::Unbound { .. })
        {
            self.table.set_row(tail, RowState::Link(Row::empty()));
        }
    }

    /// Suggests declaring `label`, pointing at where the body first produces it.
    fn add_effect_hint(&self, function: &str, label: &Label) -> String {
        let origin = self
            .effect_origins
            .get(function)
            .and_then(|origins| origins.get(label));
        let source = match origin {
            Some(Origin::Operation { text, span }) => {
                format!(
                    " (it comes from `{text}` at {}:{}:{})",
                    span.file, span.line, span.column
                )
            }
            Some(Origin::Call { callee, span }) => format!(
                " (it comes from the call to `{callee}` at {}:{}:{})",
                span.file, span.line, span.column
            ),
            None => String::new(),
        };
        format!("Add `{label}` to `effects [...]`{source}.")
    }

    // ---- ambiguity -----------------------------------------------------------------

    /// Constrained variables still unbound at the end of a function have no concrete type.
    fn report_ambiguous(&mut self, function: &str) {
        let key = function.to_string();
        let entries: Vec<(Type, Span)> = self
            .nodes
            .range((key.clone(), 0)..=(key, NodeId::MAX))
            .map(|(_, entry)| entry.clone())
            .collect();
        let mut seen = BTreeSet::new();
        for (ty, span) in entries {
            for id in free_vars(&self.table, &ty) {
                if let VarState::Unbound {
                    constraint: Some(constraint),
                    ..
                } = self.table.state(id).clone()
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
                format!(
                    "Ambiguous type: a value used with {} never receives a concrete type.",
                    constraint.describe()
                ),
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
        self.nodes
            .insert((self.function.clone(), id), (ty.clone(), span.clone()));
    }

    fn error(&mut self, code: &'static str, message: impl Into<String>, span: &Span) {
        self.diagnostics
            .push(Diagnostic::error(code, message, span.clone()));
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
        let Err(error) = unify(&mut self.table, expected, found) else {
            return;
        };
        let diagnostic = match error {
            UnifyError::Mismatch { expected, found } => {
                mismatch_error(&expected, &found, span, reason)
            }
            // `unify` widens row mismatches to the enclosing function types; this arm only
            // guards against a bare row reaching here.
            UnifyError::RowMismatch { .. } => mismatch_error(
                &self.table.resolve(expected),
                &self.table.resolve(found),
                span,
                reason,
            ),
            UnifyError::Infinite { var, ty } => {
                let names = display_all(&[&Type::Var(var), &ty]);
                Diagnostic::error(
                    "E7107",
                    format!(
                        "Infinite type: `{}` would have to equal `{}`.",
                        names[0], names[1]
                    ),
                    span.clone(),
                )
            }
            UnifyError::Constraint { constraint, ty } => {
                self.constraint_error(constraint, &ty, span)
            }
        };
        self.diagnostics.push(diagnostic);
    }

    fn require(&mut self, constraint: Constraint, ty: &Type, span: &Span) {
        if let Err(UnifyError::Constraint { constraint, ty }) =
            constrain(&mut self.table, constraint, ty)
        {
            let diagnostic = self.constraint_error(constraint, &ty, span);
            self.diagnostics.push(diagnostic);
        }
    }

    fn constraint_error(&self, constraint: Constraint, ty: &Type, span: &Span) -> Diagnostic {
        let diagnostic = Diagnostic::error(
            "E7301",
            format!(
                "`{}` does not support {}.",
                display(ty),
                constraint.describe()
            ),
            span.clone(),
        );
        match ty {
            Type::Con(name, _)
                if constraint == Constraint::Eq
                    && name != "Ui"
                    && self.table.is_non_equatable(name) =>
            {
                diagnostic.with_hint(format!(
                    "`{name}` has a field that contains a function or a UI tree."
                ))
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
                Stmt::Val {
                    name,
                    annotation,
                    value: expr,
                    span,
                } => self.bind_val(name, annotation.as_ref(), expr, span),
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
            let declared = self.env.lower_with_effects(
                annotation,
                &self.rigid,
                &self.effect_scope,
                &mut self.diagnostics,
            );
            self.unify_at(
                &declared,
                &ty,
                &value.span,
                Some(format!("the annotation on `{name}`")),
            );
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
            ExprKind::Call {
                callee,
                args,
                children,
            } => self.infer_call(callee, args, children.as_ref(), &expr.span),
            ExprKind::Field { target, name } => self.infer_field(target, name, &expr.span),
            ExprKind::SafeField { target, name } => {
                let receiver = self.infer(target);
                let inner = self.fresh();
                let reported = self.diagnostics.len();
                self.unify_at(
                    &Type::option(inner.clone()),
                    &receiver,
                    &target.span,
                    Some("the receiver of `?.`".into()),
                );
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
            ExprKind::Unary {
                op: UnaryOp::Neg,
                operand,
            } => {
                let ty = self.infer(operand);
                self.require(Constraint::Sub, &ty, &operand.span);
                ty
            }
            ExprKind::Unary {
                op: UnaryOp::Not,
                operand,
            } => {
                let ty = self.infer(operand);
                self.unify_at(
                    &Type::con("Boolean"),
                    &ty,
                    &operand.span,
                    Some("the `!` operator".into()),
                );
                Type::con("Boolean")
            }
            ExprKind::Binary { op, left, right } => self.infer_binary(*op, left, right, &expr.span),
            ExprKind::Elvis { value, fallback } => {
                let ty = self.infer(value);
                let inner = self.fresh();
                self.unify_at(
                    &Type::option(inner.clone()),
                    &ty,
                    &value.span,
                    Some("the left side of `?:`".into()),
                );
                let fallback_ty = self.infer(fallback);
                self.unify_at(
                    &inner,
                    &fallback_ty,
                    &fallback.span,
                    Some("the left side of `?:`".into()),
                );
                inner
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let condition_ty = self.infer(condition);
                self.unify_at(
                    &Type::con("Boolean"),
                    &condition_ty,
                    &condition.span,
                    Some("an `if` condition".into()),
                );
                let then_ty = self.infer(then_branch);
                let else_ty = self.infer(else_branch);
                self.unify_at(
                    &then_ty,
                    &else_ty,
                    &else_branch.span,
                    Some("the first `if` branch".into()),
                );
                then_ty
            }
            ExprKind::When { scrutinee, arms } => {
                self.infer_when(expr.id, scrutinee, arms, &expr.span)
            }
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
        if let Some(scheme) = self
            .locals
            .iter()
            .rev()
            .find(|(local, _)| local == name)
            .map(|(_, scheme)| scheme.clone())
        {
            let (ty, rows) = instantiate_with_rows(&mut self.table, &scheme, self.level);
            self.row_instances.extend(
                rows.into_iter()
                    .map(|(quantified, fresh)| (fresh, quantified)),
            );
            return self.open_top(ty);
        }
        if let Some(scheme) = self.globals.get(name).cloned() {
            let ty = instantiate(&mut self.table, &scheme, self.level);
            if self.domain == Domain::Client
                && self.function_domains.get(name) == Some(&Domain::Server)
            {
                self.error(
                    "E7404",
                    format!("`{name}` is a server endpoint; call it through `server.{name}(...)`."),
                    span,
                );
                // Its own effects are not charged: the error above already covers the use.
                return match ty {
                    Type::Fun(params, result, _) => {
                        let row = self.table.fresh_row(self.level);
                        Type::Fun(params, result, row)
                    }
                    other => other,
                };
            }
            return self.open_top(ty);
        }
        if self.env.ctors.contains_key(name) {
            return self.constructor_value(name);
        }
        if UI_NODES.contains(&name) {
            self.error(
                "E7004",
                format!("UI node `{name}` must be called, e.g. `{name}(...)`."),
                span,
            );
            return Type::Error;
        }
        self.error("E7103", format!("Unknown name `{name}`."), span);
        Type::Error
    }

    fn constructor_value(&mut self, name: &str) -> Type {
        let (fields, result) = self.env.instantiate_ctor(name, &mut self.table, self.level);
        if fields.is_empty() {
            result
        } else {
            Type::Fun(fields, Box::new(result), Row::empty())
        }
    }

    fn infer_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        children: Option<&Block>,
        span: &Span,
    ) -> Type {
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
        if let Type::Fun(params, result, row) = &shallow
            && params.len() == args.len()
        {
            let reason = self.parameter_reason(callee);
            for (param, arg) in params.iter().zip(args) {
                let arg_ty = self.infer_argument(arg, param);
                self.unify_at(param, &arg_ty, &arg.span, reason.clone());
            }
            let origin = self.call_origin(callee, span);
            self.absorb(row, origin, span);
            return (**result).clone();
        }
        let arg_types: Vec<Type> = args.iter().map(|arg| self.infer(arg)).collect();
        match shallow {
            Type::Fun(params, result, _) => {
                self.error(
                    "E7102",
                    format!(
                        "This function expects {} argument(s), but {} were given.",
                        params.len(),
                        args.len()
                    ),
                    span,
                );
                *result
            }
            Type::Error => Type::Error,
            Type::Var(_) => {
                let result = self.fresh();
                let row = self.table.fresh_row(self.level);
                let function = Type::Fun(arg_types, Box::new(result.clone()), row.clone());
                self.unify_at(&callee_ty, &function, &callee.span, None);
                self.absorb(&row, None, span);
                result
            }
            other => {
                let shown = self.shown(&other);
                self.error(
                    "E7101",
                    format!("A value of type `{shown}` is not a function."),
                    &callee.span,
                );
                Type::Error
            }
        }
    }

    /// A lambda passed where a function of the same arity is expected takes its parameter
    /// types from that function; any other argument is inferred on its own.
    fn infer_argument(&mut self, arg: &Expr, expected: &Type) -> Type {
        if let ExprKind::Lambda { params, body } = &arg.kind
            && let Type::Fun(seed, _, _) = self.table.shallow(expected)
            && seed.len() == params.len()
        {
            let ty = self.infer_lambda(params, body, Some(&seed));
            self.record(arg.id, &ty, &arg.span);
            return ty;
        }
        self.infer(arg)
    }

    fn infer_lambda(
        &mut self,
        params: &[(String, Span)],
        body: &Block,
        seed: Option<&[Type]>,
    ) -> Type {
        let mark = self.locals.len();
        let mut names = BTreeSet::new();
        let mut param_types = Vec::new();
        for (index, (name, span)) in params.iter().enumerate() {
            if !names.insert(name.as_str()) {
                self.error(
                    "E7113",
                    format!("`{name}` is bound more than once in this lambda."),
                    span,
                );
            }
            let ty = match seed.and_then(|types| types.get(index)) {
                Some(ty) => ty.clone(),
                None => self.fresh(),
            };
            param_types.push(ty.clone());
            self.locals.push((name.clone(), Scheme::mono(ty)));
        }
        let fresh = self.table.fresh_row(self.level);
        let saved = std::mem::replace(&mut self.context, fresh);
        self.lambda_depth += 1;
        self.records.push(EffectRecord::default());
        let result = self.infer_block(body);
        let record = self.records.pop().unwrap_or_default();
        self.lambda_depth -= 1;
        let row = std::mem::replace(&mut self.context, saved);
        self.lambda_records.push((row.clone(), record));
        self.locals.truncate(mark);
        Type::Fun(param_types, Box::new(result), row)
    }

    fn parameter_reason(&self, callee: &Expr) -> Option<String> {
        let ExprKind::Name(name) = &callee.kind else {
            return None;
        };
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
            self.unify_at(
                &element,
                &ty,
                &arg.span,
                Some("the first element of `listOf`".into()),
            );
        }
        Type::list(element)
    }

    fn infer_field(&mut self, target: &Expr, name: &str, span: &Span) -> Type {
        if let ExprKind::Name(base) = &target.kind
            && !self.is_shadowed(base)
        {
            if base == "server" {
                let declared = self.function_spans.contains_key(name);
                if declared && !self.endpoints.contains(name) {
                    self.error(
                        "E7404",
                        format!(
                            "`server.{name}` is not a server endpoint; declare it `server fun`."
                        ),
                        span,
                    );
                    return Type::Error;
                }
                return match self.globals.get(name).cloned().filter(|_| declared) {
                    Some(scheme) => match instantiate(&mut self.table, &scheme, self.level) {
                        // Open, like any instantiated function, so it fits where more
                        // effects are allowed.
                        Type::Fun(params, result, _) => {
                            let tail = self.table.fresh_row_var(self.level);
                            Type::Fun(
                                params,
                                result,
                                Row::with_tail([Label::of(SERVER_CALL)], tail),
                            )
                        }
                        other => other,
                    },
                    None => {
                        self.error(
                            "E7103",
                            format!("`server.{name}` does not name a function."),
                            span,
                        );
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
                            format!(
                                "Cannot access `{name}` on a nullable value of type `{shown}`."
                            ),
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
                        let record =
                            self.env
                                .instantiate_record(owner, &mut self.table, self.level);
                        self.unify_at(&record, target, span, None);
                        let Type::Con(record_name, args) = &record else {
                            return Type::Error;
                        };
                        self.env
                            .field_type(record_name, args, name)
                            .unwrap_or(Type::Error)
                    }
                    [] => {
                        self.error(
                            "E7104",
                            format!("No record type has a field `{name}`."),
                            span,
                        );
                        Type::Error
                    }
                    _ => {
                        self.diagnostics.push(
                            Diagnostic::error(
                                "E7106",
                                format!(
                                    "The field `{name}` exists in several types ({}).",
                                    owners.join(", ")
                                ),
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
                self.unify_at(
                    &int,
                    &right_ty,
                    &right.span,
                    Some("the `%` operator".into()),
                );
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
                self.unify_at(
                    &left_ty,
                    &right_ty,
                    &right.span,
                    Some(format!("the left operand of `{}`", op.symbol())),
                );
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
            Type::Con(name, _) if name == "Int" || name == "Decimal" => {
                Type::option(operand.clone())
            }
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
        let insert =
            db_method(target).filter(|(_, method)| *method == "insert" && !self.is_shadowed("db"));
        let Some((table, _)) = insert else {
            for field in fields {
                self.infer(&field.value);
            }
            self.error(
                "E7004",
                "`{ field = value }` blocks are only supported on `db.<table>.insert`.",
                span,
            );
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
                self.error(
                    "E7110",
                    format!("Column `{}` is assigned more than once.", field.name),
                    &field.span,
                );
                continue;
            }
            match columns.get(&field.name) {
                Some(column) => {
                    let expected = self.column_type(column, &field.span);
                    let reason = Some(format!("column `{table}.{}`", field.name));
                    self.unify_at(&expected, &found, &field.value.span, reason);
                }
                None => self.error(
                    "E7104",
                    format!("Table `{table}` has no column `{}`.", field.name),
                    &field.span,
                ),
            }
        }
        self.perform(DB_WRITE, format!("db.{table}.insert"), span);
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
        self.unify_at(
            &key,
            &found,
            &args[0].span,
            Some(format!("the primary key of table `{table}`")),
        );
        self.perform(DB_READ, format!("db.{table}.{method}"), span);
        if method == "find" {
            Type::option(model)
        } else {
            model
        }
    }

    /// The primary-key type and model type of a table, which must map to a declared entity.
    fn db_table(&mut self, table: &str, span: &Span) -> Option<(Type, Type)> {
        let schema = self.schema;
        let Some(definition) = schema.tables.get(table) else {
            self.error("E7104", format!("Unknown database table `{table}`."), span);
            return None;
        };
        let is_entity = self
            .env
            .types
            .get(&definition.model)
            .is_some_and(|info| info.kind == TypeKind::Entity);
        if !is_entity {
            self.error(
                "E7103",
                format!(
                    "Table `{table}` maps to `{}`, which is not a declared entity.",
                    definition.model
                ),
                span,
            );
            return None;
        }
        let key = match definition.columns.get(&definition.primary_key) {
            Some(column) => self.column_type(column, span),
            None => {
                self.error(
                    "E7104",
                    format!(
                        "Primary-key column `{}` is missing from table `{table}` in the database schema.",
                        definition.primary_key
                    ),
                    span,
                );
                Type::Error
            }
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
        if column.nullable {
            Type::option(base)
        } else {
            base
        }
    }

    fn infer_ui(
        &mut self,
        name: &str,
        args: &[Expr],
        children: Option<&Block>,
        span: &Span,
    ) -> Type {
        if name == "Text" {
            if args.len() != 1 {
                self.error(
                    "E7102",
                    format!("`Text` expects 1 argument, but {} were given.", args.len()),
                    span,
                );
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
                            format!(
                                "`Text` cannot display a value of type `{}`.",
                                display(&resolved)
                            ),
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
                Type::Con(type_name, type_args)
                    if type_args.is_empty() && (type_name == "String" || type_name == "Int") => {}
                other => {
                    let shown = self.shown(&other);
                    let mut diagnostic = Diagnostic::error(
                        "E7101",
                        format!("UI node arguments must be `String` or `Int`, found `{shown}`."),
                        arg.span.clone(),
                    );
                    if matches!(other, Type::Var(_)) {
                        diagnostic = diagnostic
                            .with_hint("Annotate the parameter's type, e.g. `label: String`.");
                    }
                    self.diagnostics.push(diagnostic);
                }
            }
        }
        if let Some(block) = children {
            let mark = self.locals.len();
            for stmt in &block.stmts {
                match stmt {
                    Stmt::Val {
                        name,
                        annotation,
                        value,
                        span,
                    } => self.bind_val(name, annotation.as_ref(), value, span),
                    Stmt::Expr(expr) => {
                        let ty = self.infer(expr);
                        self.unify_at(
                            &Type::con("Ui"),
                            &ty,
                            &expr.span,
                            Some("a UI child block".into()),
                        );
                    }
                }
            }
            self.locals.truncate(mark);
            self.record(block.id, &Type::con("Ui"), &block.span);
        }
        Type::con("Ui")
    }

    // ---- when and patterns ---------------------------------------------------------

    fn infer_when(&mut self, id: NodeId, scrutinee: &Expr, arms: &[Arm], span: &Span) -> Type {
        let subject = self.infer(scrutinee);
        // A scrutinee of still unknown type matched against `null` is optional, which the arms
        // before the `null` arm must already see. A known type is left to the `null` pattern,
        // which reports a mismatch once, at the pattern.
        if arms
            .iter()
            .any(|arm| matches!(arm.pattern, Pattern::Null(_)))
            && matches!(self.table.shallow(&subject), Type::Var(_))
        {
            let inner = self.fresh();
            self.unify_at(
                &Type::option(inner),
                &subject,
                &scrutinee.span,
                Some("a `null` pattern".into()),
            );
        }
        let result = self.fresh();
        let mut patterns = Vec::new();
        let mut invalid_pattern = false;
        let mut null_seen = false;
        for arm in arms {
            let mark = self.locals.len();
            let mut bound = BTreeSet::new();
            let reported = self.diagnostics.len();
            let pattern = match (&arm.pattern, self.optional_inner(&subject)) {
                // Smart cast: after a `null` arm, a variable arm binds the non-null value.
                (Pattern::Name(name, span), Some(inner)) if null_seen && self.is_variable(name) => {
                    self.bind_variable(name, &inner, span, &mut bound);
                    Pat::Ctor("Some".into(), vec![Pat::Wild])
                }
                _ => self.check_pattern(&arm.pattern, &subject, &mut bound),
            };
            null_seen |= matches!(arm.pattern, Pattern::Null(_));
            invalid_pattern |= self.diagnostics.len() > reported;
            let body = self.infer(&arm.body);
            self.unify_at(
                &result,
                &body,
                &arm.body.span,
                Some("the first `when` branch".into()),
            );
            self.locals.truncate(mark);
            patterns.push(pattern);
        }
        // An invalid pattern stands in as `_`, which would make later arms look redundant.
        let subject_ty = self.table.resolve(&subject);
        if !invalid_pattern && !subject_ty.contains_error() {
            let report = exhaustive::check_match(self.env, &subject_ty, &patterns);
            for index in report.redundant {
                self.error(
                    "E7202",
                    "This `when` branch can never be reached.",
                    &arms[index].span,
                );
            }
            if let Some(missing) = report.missing {
                self.error(
                    "E7201",
                    format!(
                        "`when` is not exhaustive; missing case: {}.",
                        exhaustive::render(&missing)
                    ),
                    span,
                );
            }
        }
        self.patterns.insert((self.function.clone(), id), patterns);
        result
    }

    fn optional_inner(&self, ty: &Type) -> Option<Type> {
        match self.table.shallow(ty) {
            Type::Con(name, mut args) if name == "Option" && args.len() == 1 => args.pop(),
            _ => None,
        }
    }

    /// A lowercase name that is not a constructor binds a variable in a pattern.
    fn is_variable(&self, name: &str) -> bool {
        !self.env.ctors.contains_key(name) && !name.chars().next().is_some_and(char::is_uppercase)
    }

    fn bind_variable(&mut self, name: &str, ty: &Type, span: &Span, bound: &mut BTreeSet<String>) {
        if !bound.insert(name.to_string()) {
            self.error(
                "E7113",
                format!("`{name}` is bound more than once in this pattern."),
                span,
            );
        }
        self.locals
            .push((name.to_string(), Scheme::mono(ty.clone())));
    }

    /// Checks `pattern` against `ty`. Variables and `_` are irrefutable and take `ty` as is.
    /// At a type already known to be `T?`, a constructor or literal pattern describes a
    /// non-null value: it is checked against `T`, unwrapping one layer per pattern level.
    fn check_pattern(&mut self, pattern: &Pattern, ty: &Type, bound: &mut BTreeSet<String>) -> Pat {
        let refutable = match pattern {
            Pattern::Wildcard | Pattern::Null(_) | Pattern::Double(..) => false,
            Pattern::Name(name, _) => !self.is_variable(name),
            _ => true,
        };
        if refutable && let Some(inner) = self.optional_inner(ty) {
            let pattern = self.check_value_pattern(pattern, &inner, bound);
            return Pat::Ctor("Some".into(), vec![pattern]);
        }
        self.check_value_pattern(pattern, ty, bound)
    }

    fn check_value_pattern(
        &mut self,
        pattern: &Pattern,
        ty: &Type,
        bound: &mut BTreeSet<String>,
    ) -> Pat {
        match pattern {
            Pattern::Wildcard => Pat::Wild,
            Pattern::Null(span) => {
                let inner = self.fresh();
                self.unify_at(
                    &Type::option(inner),
                    ty,
                    span,
                    Some("a `null` pattern".into()),
                );
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
                self.error(
                    "E7109",
                    format!("The floating-point literal `{text}` cannot be used as a pattern."),
                    span,
                );
                Pat::Wild
            }
            Pattern::Name(name, span) => {
                if let Some(ctor) = self.env.ctors.get(name) {
                    if !ctor.fields.is_empty() {
                        self.error(
                            "E7102",
                            format!(
                                "Constructor `{name}` has {} field(s); write `{name}(...)`.",
                                ctor.fields.len()
                            ),
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
                self.bind_variable(name, ty, span, bound);
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
                        format!(
                            "Constructor `{name}` has {} field(s), but the pattern lists {}.",
                            fields.len(),
                            args.len()
                        ),
                        span,
                    );
                    return Pat::Wild;
                }
                let patterns = args
                    .iter()
                    .zip(&fields)
                    .map(|(arg, field)| self.check_pattern(arg, field, bound))
                    .collect();
                Pat::Ctor(name.clone(), patterns)
            }
        }
    }
}

/// `db.<table>.<method>` as written, before any shadowing check.
fn db_method(expr: &Expr) -> Option<(&str, &str)> {
    let ExprKind::Field {
        target,
        name: method,
    } = &expr.kind
    else {
        return None;
    };
    let ExprKind::Field {
        target: root,
        name: table,
    } = &target.kind
    else {
        return None;
    };
    match &root.kind {
        ExprKind::Name(db) if db == "db" => Some((table.as_str(), method.as_str())),
        _ => None,
    }
}

/// Effect variables named in a function's signature: parameter and return types and the
/// `effects [...]` clause.
fn effect_variable_names(function: &Function) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = function
        .effects
        .iter()
        .filter(|name| !name.contains('.'))
        .cloned()
        .collect();
    names.extend(parameter_effect_variables(function));
    if let Some(type_ref) = &function.return_type {
        type_effect_variables(type_ref, &function.span, &mut names);
    }
    names
}

/// Effect variables named in the parameter types of a function.
fn parameter_effect_variables(function: &Function) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for parameter in &function.parameters {
        if let Some(type_ref) = &parameter.type_ref {
            type_effect_variables(type_ref, &function.span, &mut names);
        }
    }
    names
}

fn type_effect_variables(type_ref: &TypeRef, span: &Span, names: &mut BTreeSet<String>) {
    if let Ok(te) = parse_type_text(&type_ref.display(), span) {
        te.effect_variables(names);
    }
}

/// Whether a function's effects are fixed by its declaration (and therefore checked exactly).
fn effects_known(function: &Function) -> bool {
    function.effects_declared
        || function.domain == Domain::Pure
        || (function.domain == Domain::Server && !function.component)
}

fn is_fully_annotated(function: &Function) -> bool {
    function
        .parameters
        .iter()
        .all(|parameter| parameter.type_ref.is_some())
        && (function.component || function.return_type.is_some())
        && effects_known(function)
}

fn mismatch_error(
    expected: &Type,
    found: &Type,
    span: &Span,
    reason: Option<String>,
) -> Diagnostic {
    let names = display_all(&[expected, found]);
    let diagnostic = Diagnostic::error(
        "E7101",
        format!(
            "Type mismatch: expected `{}`, found `{}`.",
            names[0], names[1]
        ),
        span.clone(),
    );
    match reason {
        Some(reason) => diagnostic.with_hint(format!("`{}` is required by {reason}.", names[0])),
        None => diagnostic,
    }
}

fn builtin(
    table: &mut TypeTable,
    arity: usize,
    rows: usize,
    build: impl Fn(&[Type], &[RowVarId]) -> Type,
) -> Scheme {
    let vars: Vec<Type> = (0..arity).map(|_| table.fresh(GENERIC_LEVEL)).collect();
    let row_vars: Vec<RowVarId> = (0..rows)
        .map(|_| table.fresh_row_var(GENERIC_LEVEL))
        .collect();
    let ids = vars
        .iter()
        .filter_map(|var| {
            if let Type::Var(id) = var {
                Some(*id)
            } else {
                None
            }
        })
        .collect();
    Scheme {
        vars: ids,
        row_vars: row_vars.clone(),
        ty: build(&vars, &row_vars),
    }
}

/// The most effects a function of this domain may perform (`None` = no ceiling).
fn ceiling(domain: &Domain) -> Option<BTreeSet<Label>> {
    match domain {
        Domain::Pure => Some(BTreeSet::new()),
        Domain::Server => Some([Label::of(DB_READ), Label::of(DB_WRITE)].into()),
        Domain::Client => Some([Label::of(SERVER_CALL)].into()),
        Domain::Inferred | Domain::Worker | Domain::Database | Domain::External => None,
    }
}

/// How E7401 names the domain whose ceiling was exceeded.
fn domain_description(domain: &Domain) -> &'static str {
    match domain {
        Domain::Pure => "a `pure` function",
        Domain::Client => "client code (it runs in the browser)",
        Domain::Server => "a server endpoint",
        _ => "this function",
    }
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
    let index: BTreeMap<&str, usize> = functions
        .iter()
        .enumerate()
        .map(|(position, function)| (function.name.as_str(), position))
        .collect();
    let edges: Vec<Vec<usize>> = functions
        .iter()
        .map(|function| {
            let mut names = BTreeSet::new();
            if let Some(block) = &function.expr {
                collect_block(block, &mut names);
            }
            names
                .iter()
                .filter_map(|name| index.get(name.as_str()).copied())
                .collect()
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
                Some(index) if self.on_stack[other] => {
                    self.low[vertex] = self.low[vertex].min(index)
                }
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
        ExprKind::Call {
            callee,
            args,
            children,
        } => {
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
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
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
            fields
                .iter()
                .for_each(|field| collect_expr(&field.value, names));
        }
        ExprKind::Int(_)
        | ExprKind::Double(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bool(_)
        | ExprKind::Null => {}
    }
}

#[cfg(test)]
mod tests {
    use crate::types::testing::{errors, infer, scheme};

    const PEDIDO: &str = "sealed class Pedido {\n    data class Pendente(val itens: Int)\n    data class Pago(val valor: Money)\n    data object Cancelado\n}\n";

    #[test]
    fn elvis_chains_are_right_associative() {
        assert_eq!(
            scheme("fun f(a: Int?, b: Int?): Int = a ?: b ?: 0", "f"),
            "(Int?, Int?) -> Int"
        );
    }

    #[test]
    fn nested_optional_annotation_is_supported() {
        assert_eq!(
            scheme("fun f(a: Int??): Int? = a ?: null", "f"),
            "(Int??) -> Int?"
        );
    }

    #[test]
    fn infers_principal_types() {
        assert_eq!(scheme("fun ident(x) = x", "ident"), "('a) -> 'a");
        assert_eq!(
            scheme("fun constante(x, y) = x", "constante"),
            "('a, 'b) -> 'a"
        );
        assert_eq!(
            scheme("fun compose(f, g, x) = f(g(x))", "compose"),
            "(('a) -> 'b ! 'e, ('c) -> 'a ! 'e, 'c) -> 'b ! 'e"
        );
        assert_eq!(
            scheme("fun aplica(f, x) = f(x)", "aplica"),
            "(('a) -> 'b ! 'e, 'a) -> 'b ! 'e"
        );
    }

    #[test]
    fn literals_resolve_operator_constraints() {
        assert_eq!(scheme("fun dobro(x) = x * 2", "dobro"), "(Int) -> Int");
        assert_eq!(
            scheme("fun concat(a, b) = a + b + \"\"", "concat"),
            "(String, String) -> String"
        );
        assert_eq!(
            scheme("fun media(a: Double, b: Double) = (a + b) / 2.0", "media"),
            "(Double, Double) -> Double"
        );
        assert_eq!(
            scheme("fun menor(a: Instant, b: Instant) = a < b", "menor"),
            "(Instant, Instant) -> Boolean"
        );
    }

    #[test]
    fn integer_division_and_remainder_are_nullable() {
        assert_eq!(
            scheme("fun metade(x: Int) = x / 2", "metade"),
            "(Int) -> Int?"
        );
        assert_eq!(
            scheme("fun resto(x: Int) = (x % 2) ?: 0", "resto"),
            "(Int) -> Int"
        );
        assert_eq!(errors("fun f(x: Int): Int = x / 2"), vec!["E7101"]);
    }

    #[test]
    fn rejects_ambiguous_and_unsupported_operators() {
        assert_eq!(errors("fun soma(a, b) = a + b"), vec!["E7302"]);
        assert_eq!(errors("fun divide(a, b) = a / b"), vec!["E7302"]);
        assert_eq!(
            errors("fun f(a: Boolean, b: Boolean) = a + b"),
            vec!["E7301"]
        );
        assert_eq!(errors("fun f(a: String, b: String) = a - b"), vec!["E7301"]);
        assert_eq!(
            errors("fun g(x: Int): Int = x\nfun f(): Boolean = g == g"),
            vec!["E7301"]
        );
        assert_eq!(
            errors("fun <T> igual(a: T, b: T): Boolean = a == b"),
            vec!["E7301"]
        );
    }

    #[test]
    fn equality_rejects_types_that_contain_functions() {
        let holder = "data class H(val run: (Int) -> Int)\n";
        assert_eq!(
            errors(&format!("{holder}fun f(a: H, b: H): Boolean = a == b")),
            vec!["E7301"]
        );
        let wrapper =
            format!("{holder}data class W(val h: H)\nfun f(a: W, b: W): Boolean = a == b");
        assert_eq!(errors(&wrapper), vec!["E7301"]);
        let sealed = "sealed class S {\n    data class C(val k: () -> Int)\n    data object D\n}\n";
        assert_eq!(
            errors(&format!("{sealed}fun f(a: S, b: S): Boolean = a == b")),
            vec!["E7301"]
        );
        assert_eq!(
            errors("fun f(a: Ui, b: Ui): Boolean = a == b"),
            vec!["E7301"]
        );
        let plain = "data class P(val x: Int)\nfun f(a: P, b: P): Boolean = a == b";
        assert_eq!(scheme(plain, "f"), "(P, P) -> Boolean");
        let boxed = "data class Box<T>(val v: T)\n";
        assert_eq!(
            scheme(
                &format!("{boxed}fun f(a: Box<Int>, b: Box<Int>): Boolean = a == b"),
                "f"
            ),
            "(Box<Int>, Box<Int>) -> Boolean"
        );
        let functions =
            format!("{boxed}fun f(a: Box<(Int) -> Int>, b: Box<(Int) -> Int>): Boolean = a == b");
        assert_eq!(errors(&functions), vec!["E7301"]);
        let local = format!("{holder}fun f(x: H): Boolean {{\n    val y = x\n    y == x\n}}");
        assert_eq!(errors(&local), vec!["E7301"]);
    }

    /// The largest inputs the parser accepts type-check without exhausting the stack the
    /// `bind` binary gives the compiler; inputs past the limits stop at the parser.
    #[test]
    fn inputs_at_the_parser_limits_type_check() {
        crate::with_compiler_stack(|| {
            let chain = vec!["x"; 1_001].join(" + ");
            assert_eq!(
                scheme(&format!("fun f(x: Int): Int = {chain}"), "f"),
                "(Int) -> Int"
            );
            let nested = format!("{}x{}", "(x + ".repeat(255), ")".repeat(255));
            assert_eq!(
                scheme(&format!("fun f(x: Int): Int = {nested}"), "f"),
                "(Int) -> Int"
            );
            let mixed = format!(
                "{}{}{}",
                "-(".repeat(127),
                vec!["x"; 744].join(" - "),
                ")".repeat(127)
            );
            assert_eq!(
                scheme(&format!("fun f(x: Int): Int = {mixed}"), "f"),
                "(Int) -> Int"
            );
            let calls = format!("{}x{}", "g(".repeat(254), ")".repeat(254));
            assert_eq!(
                scheme(
                    &format!("fun g(x: Int): Int = x\nfun f(x: Int): Int = {calls}"),
                    "f"
                ),
                "(Int) -> Int"
            );
            let ifs = format!(
                "{}x{}",
                "if (true) { ".repeat(126),
                " } else { x }".repeat(126)
            );
            assert_eq!(
                scheme(&format!("fun f(x: Int): Int = {ifs}"), "f"),
                "(Int) -> Int"
            );
            let whens = format!("{}x{}", "when (x) { _ -> ".repeat(255), " }".repeat(255));
            assert_eq!(
                scheme(&format!("fun f(x: Int): Int = {whens}"), "f"),
                "(Int) -> Int"
            );
            let lambdas = format!("{}x{}", "{ y -> ".repeat(250), " }".repeat(250));
            assert!(infer(&format!("fun f(x: Int) = {lambdas}")).is_ok());
            assert_eq!(
                errors(&format!(
                    "fun f(x: Int): Int = {}",
                    vec!["x"; 1_500].join(" + ")
                )),
                vec!["E7004"]
            );
            assert_eq!(
                errors(&format!(
                    "fun f(x: Int): Int = {}x{}",
                    "(".repeat(300),
                    ")".repeat(300)
                )),
                vec!["E7004"]
            );
        });
    }

    #[test]
    fn generalizes_local_values() {
        let source =
            "fun f(): Boolean {\n    val id = { x -> x }\n    val n = id(1)\n    id(n == 1)\n}";
        assert_eq!(scheme(source, "f"), "() -> Boolean");
    }

    #[test]
    fn annotated_type_parameters_are_rigid() {
        assert_eq!(
            scheme("fun <T> primeiro(xs: List<T>): T? = first(xs)", "primeiro"),
            "(List<'a>) -> 'a?"
        );
        assert_eq!(errors("fun <T> errado(x: T): Int = x"), vec!["E7101"]);
        assert_eq!(
            scheme(
                "fun <A, B> mapear(xs: List<A>, f: (A) -> B): List<B> = map(xs, f)",
                "mapear"
            ),
            "(List<'a>, ('a) -> 'b) -> List<'b>"
        );
    }

    #[test]
    fn infers_mutual_recursion() {
        let source = "fun par(n) = if (n == 0) true else impar(n - 1)\nfun impar(n) = if (n == 0) false else par(n - 1)";
        assert_eq!(scheme(source, "par"), "(Int) -> Boolean");
        assert_eq!(scheme(source, "impar"), "(Int) -> Boolean");
        assert_eq!(
            scheme(
                "fun fatorial(n) = if (n <= 1) 1 else n * fatorial(n - 1)",
                "fatorial"
            ),
            "(Int) -> Int"
        );
    }

    #[test]
    fn rejects_infinite_types() {
        assert_eq!(errors("fun auto(x) = x(x)"), vec!["E7107"]);
    }

    #[test]
    fn discarded_values_are_errors() {
        assert_eq!(errors("fun f(): Int {\n    1 + 1\n    2\n}"), vec!["E7108"]);
        assert_eq!(
            scheme("fun f(): Int {\n    val _ = 1 + 1\n    2\n}", "f"),
            "() -> Int"
        );
        let operator_on_next_line =
            "fun f(a: Int, b: Int): Int {\n    val y = a\n        - b\n    y\n}";
        assert_eq!(errors(operator_on_next_line), vec!["E7108"]);
    }

    #[test]
    fn returns_and_lambdas() {
        assert_eq!(
            scheme("fun f(x: Int): Int {\n    return x\n}", "f"),
            "(Int) -> Int"
        );
        assert_eq!(
            errors("fun f(): Int {\n    val g = { x -> return x }\n    g(1)\n}"),
            vec!["E7111"]
        );
        assert_eq!(errors("fun f(): Int {\n    return \"a\"\n}"), vec!["E7101"]);
        assert_eq!(
            errors("fun f(c: Boolean) = if (c) 1 else \"a\""),
            vec!["E7101"]
        );
    }

    #[test]
    fn when_must_be_exhaustive_and_free_of_redundancy() {
        let complete = format!(
            "{PEDIDO}fun f(p: Pedido): Int = when (p) {{\n    Pendente(n) -> n\n    Pago(_) -> 1\n    Cancelado -> 0\n}}"
        );
        assert_eq!(scheme(&complete, "f"), "(Pedido) -> Int");
        let missing = format!(
            "{PEDIDO}fun f(p: Pedido): Int = when (p) {{\n    Pendente(n) -> n\n    Cancelado -> 0\n}}"
        );
        assert_eq!(errors(&missing), vec!["E7201"]);
        let redundant = format!(
            "{PEDIDO}fun f(p: Pedido): Int = when (p) {{\n    _ -> 0\n    Cancelado -> 1\n}}"
        );
        assert_eq!(errors(&redundant), vec!["E7202"]);
        assert_eq!(
            errors(&format!(
                "{PEDIDO}fun f(p: Pedido): Int = when (p) {{ Pendente(a, b) -> 1; _ -> 0 }}"
            )),
            vec!["E7102"]
        );
    }

    #[test]
    fn nullable_patterns() {
        assert_eq!(
            scheme(
                "fun f(x: Int?): Int = when (x) {\n    null -> 0\n    n -> n\n}",
                "f"
            ),
            "(Int?) -> Int"
        );
        assert_eq!(
            errors("fun f(x: Int?): Int = when (x) { n -> n }"),
            vec!["E7101"]
        );
        assert_eq!(
            scheme(
                "fun f(x) = when (x) {\n    null -> 0\n    n -> n + 1\n}",
                "f"
            ),
            "(Int?) -> Int"
        );
    }

    #[test]
    fn variable_patterns_bind_the_type_as_is() {
        let record = "data class U(val nome: String, val email: String?)\n";
        assert_eq!(
            scheme(
                &format!("{record}fun f(u: U): String = when (u) {{ U(n, e) -> n }}"),
                "f"
            ),
            "(U) -> String"
        );
        let email = format!("{record}fun f(u: U): String = when (u) {{ U(_, e) -> e ?: \"\" }}");
        assert_eq!(scheme(&email, "f"), "(U) -> String");
        assert_eq!(
            scheme("fun f(x: Int?): Int = when (x) { null -> 0; n -> n }", "f"),
            "(Int?) -> Int"
        );
        assert_eq!(
            scheme("fun f(x: Int?): Int? = when (x) { n -> n }", "f"),
            "(Int?) -> Int?"
        );
        assert_eq!(
            scheme(
                "fun f(x: Int??): Int = when (x) { null -> 0; y -> y ?: 1 }",
                "f"
            ),
            "(Int??) -> Int"
        );
        // Only a top-level variable arm after a top-level `null` arm is smart-cast; before it,
        // the variable takes `Int?` and already covers `null`.
        assert_eq!(
            errors("fun f(x: Int?): Int = when (x) { n -> n; null -> 0 }"),
            vec!["E7101", "E7202"]
        );
        assert_eq!(
            errors("fun f(x: Int?): Int = when (x) { n -> n ?: 0; null -> 0 }"),
            vec!["E7202"]
        );
    }

    fn diagnostic_codes(source: &str) -> Vec<&'static str> {
        let (tokens, _) = crate::lexer::lex("t.bd", source);
        let (program, parse_diagnostics) = crate::parser::parse(tokens);
        assert!(parse_diagnostics.is_empty(), "{parse_diagnostics:?}");
        let (_, diagnostics) =
            crate::types::check(&program, &crate::schema::DatabaseSchema::default());
        diagnostics.iter().map(|item| item.code).collect()
    }

    #[test]
    fn null_pattern_on_a_non_optional_scrutinee_is_reported_once() {
        assert_eq!(
            diagnostic_codes("fun f(x: Int): Int = when (x) { null -> 0; _ -> 1 }"),
            vec!["E7101"]
        );
        assert_eq!(
            diagnostic_codes("fun f(x: String): Int = when (x) { \"a\" -> 1; null -> 0; _ -> 1 }"),
            vec!["E7101"]
        );
        assert_eq!(
            scheme("fun f(x) = when (x) { 1 -> 1; null -> 0; _ -> 2 }", "f"),
            "(Int?) -> Int"
        );
    }

    #[test]
    fn elaborated_patterns_are_recorded_per_when() {
        use crate::types::exhaustive::Pat;
        let (tokens, _) = crate::lexer::lex(
            "t.bd",
            "fun f(x: Int?): Int = when (x) { null -> 0; 1 -> 1; n -> n }",
        );
        let (program, _) = crate::parser::parse(tokens);
        let (typed, diagnostics) =
            crate::types::check(&program, &crate::schema::DatabaseSchema::default());
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let patterns: Vec<&Vec<Pat>> = typed.patterns.values().collect();
        let some = |inner: Pat| Pat::Ctor("Some".into(), vec![inner]);
        assert_eq!(
            patterns,
            vec![&vec![
                Pat::Ctor("null".into(), Vec::new()),
                some(Pat::Lit("1".into())),
                some(Pat::Wild)
            ]]
        );
        assert!(typed.patterns.keys().all(|(function, _)| function == "f"));
    }

    #[test]
    fn constructor_and_literal_patterns_unwrap_one_optional_layer() {
        assert_eq!(
            scheme(
                "fun f(x: Int?): Int = when (x) { null -> 0; 1 -> 1; _ -> 2 }",
                "f"
            ),
            "(Int?) -> Int"
        );
        assert_eq!(
            scheme("fun f(x: Int?): Int = when (x) { 1 -> 1; _ -> 2 }", "f"),
            "(Int?) -> Int"
        );
        assert_eq!(
            errors("fun f(x: Int??): Int = when (x) { null -> 0; 1 -> 1; _ -> 2 }"),
            vec!["E7101"]
        );
        let pedido = format!(
            "{PEDIDO}fun f(p: Pedido?): Int = when (p) {{\n    null -> 0\n    Pendente(n) -> n\n    Pago(_) -> 1\n    Cancelado -> 2\n}}"
        );
        assert_eq!(scheme(&pedido, "f"), "(Pedido?) -> Int");
        let missing = format!(
            "{PEDIDO}fun f(p: Pedido?): Int = when (p) {{\n    Pendente(n) -> n\n    Pago(_) -> 1\n    Cancelado -> 2\n}}"
        );
        assert_eq!(errors(&missing), vec!["E7201"]);
        let nested = "data class W(val v: Int?)\nfun f(w: W): Int = when (w) { W(null) -> 0; W(1) -> 1; W(n) -> n ?: 2 }";
        assert_eq!(scheme(nested, "f"), "(W) -> Int");
    }

    #[test]
    fn literal_patterns() {
        let source = "fun f(x: Int): String = when (x) {\n    0 -> \"zero\"\n    _ -> \"outro\"\n}";
        assert_eq!(scheme(source, "f"), "(Int) -> String");
        assert_eq!(
            errors("fun f(x: Int): String = when (x) { 0 -> \"zero\" }"),
            vec!["E7201"]
        );
        assert_eq!(
            errors("fun f(x: Double): Int = when (x) { 1.5 -> 1; _ -> 0 }"),
            vec!["E7109"]
        );
        assert_eq!(
            errors("fun f(b: Boolean): Int = when (b) { true -> 1; true -> 2; false -> 3 }"),
            vec!["E7202"]
        );
    }

    #[test]
    fn records_and_field_access() {
        let source = "data class P(val x: Int, val nome: String?)\nfun getX(p) = p.x\nfun nome(p: P?) = p?.nome ?: \"\"";
        assert_eq!(scheme(source, "getX"), "(P) -> Int");
        assert_eq!(scheme(source, "nome"), "(P?) -> String");
        assert_eq!(
            errors("data class A(val x: Int)\ndata class B(val x: Int)\nfun f(v) = v.x"),
            vec!["E7106"]
        );
        assert_eq!(
            errors("data class A(val x: Int)\nfun f(a: A) = a.y"),
            vec!["E7104"]
        );
        assert_eq!(
            errors("data class A(val x: Int)\nfun f(a: A?) = a.x"),
            vec!["E7101"]
        );
    }

    #[test]
    fn constructors_results_and_enums() {
        assert_eq!(scheme("fun ok() = Ok(1)", "ok"), "() -> Result<Int, 'a>");
        assert_eq!(
            scheme("fun erro(s: String): Result<Int, String> = Err(s)", "erro"),
            "(String) -> Result<Int, String>"
        );
        assert_eq!(
            scheme(
                "enum class Cor { Azul, Verde }\nfun azul() = Cor.Azul",
                "azul"
            ),
            "() -> Cor"
        );
        assert_eq!(
            scheme("data class P(val x: Int)\nfun p() = P(1)", "p"),
            "() -> P"
        );
        assert_eq!(
            errors("data class P(val x: Int)\nfun p() = P(1, 2)"),
            vec!["E7102"]
        );
        assert_eq!(
            scheme("fun xs() = listOf(1, 2, 3)", "xs"),
            "() -> List<Int>"
        );
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
        assert_eq!(
            errors("fun <A, B> f(x: A, y: B) = if (true) x else y"),
            vec!["E7101"]
        );
    }

    #[test]
    fn duplicate_parameters_are_errors() {
        assert_eq!(errors("fun f(x, x) = x"), vec!["E7113"]);
    }

    #[test]
    fn lambda_arguments_see_the_expected_parameter_types() {
        let records = "data class A(val valor: Int)\ndata class B(val valor: Int)\n";
        let mapped =
            format!("{records}fun f(xs: List<A>): List<Int> = map(xs, {{ p -> p.valor }})");
        assert_eq!(scheme(&mapped, "f"), "(List<A>) -> List<Int>");
        let folded = "fun f(xs: List<Double>): Double = fold(xs, 1.0, { acc, x -> acc / x })";
        assert_eq!(scheme(folded, "f"), "(List<Double>) -> Double");
    }

    #[test]
    fn unknown_uppercase_pattern_names_are_not_bindings() {
        let sealed = "sealed class P {\n    data object Um\n    data object Dois\n}\n";
        assert_eq!(
            errors(&format!(
                "{sealed}fun f(p: P): Int = when (p) {{ Um -> 1; Doiss -> 2 }}"
            )),
            vec!["E7103"]
        );
        assert_eq!(
            scheme(
                &format!("{sealed}fun f(p: P): Int = when (p) {{ Um -> 1; outro -> 2 }}"),
                "f"
            ),
            "(P) -> Int"
        );
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
        schemes
            .into_iter()
            .find(|(other, _)| other == name)
            .unwrap()
            .1
    }

    #[test]
    fn database_operations() {
        assert_eq!(
            db_scheme(
                "server fun buscar(id: ClienteId): Cliente effects [db.read] {\n    return db.clientes.require(id)\n}",
                "buscar"
            ),
            "(ClienteId) -> Cliente ! {db.read}"
        );
        assert_eq!(
            db_scheme(
                "server fun talvez(id: ClienteId): Cliente? effects [db.read] = db.clientes.find(id)",
                "talvez"
            ),
            "(ClienteId) -> Cliente? ! {db.read}"
        );
        assert_eq!(
            db_scheme(
                "server fun criar(nome: String): Cliente effects [db.write] {\n    return db.clientes.insert {\n        nome = nome\n        apelido = null\n    }\n}",
                "criar"
            ),
            "(String) -> Cliente ! {db.write}"
        );
    }

    #[test]
    fn database_errors() {
        assert_eq!(
            with_db("server fun f(id: ClienteId): Cliente = db.pedidos.require(id)"),
            Err(vec!["E7104".into()])
        );
        assert_eq!(
            with_db(
                "server fun f(id: String): Cliente effects [db.read] = db.clientes.require(id)"
            ),
            Err(vec!["E7101".into()])
        );
        assert_eq!(
            with_db(
                "server fun f(n: Int): Cliente effects [db.write] = db.clientes.insert { nome = n }"
            ),
            Err(vec!["E7101".into()])
        );
        assert_eq!(
            with_db(
                "server fun f(n: String): Cliente effects [db.write] = db.clientes.insert { idade = n }"
            ),
            Err(vec!["E7104".into()])
        );
        assert_eq!(
            with_db("server fun f(id: ClienteId): Cliente = db.clientes.delete(id)"),
            Err(vec!["E7004".into()])
        );
        assert_eq!(
            with_db("fun f(c: Cliente) = c.nome { x = 1 }"),
            Err(vec!["E7004".into()])
        );
    }

    #[test]
    fn missing_primary_key_column_is_reported() {
        let csv = "table,model,primary_key,column,type,nullable,required,has_default,engine\ncustomers,Cliente,uid,id,ClienteId,false,true,true,postgresql\ncustomers,Cliente,uid,nome,String,false,true,false,postgresql\n";
        let source = format!(
            "{ENTIDADE}server fun f(id: ClienteId): Cliente effects [db.read] = db.customers.require(id)"
        );
        assert_eq!(
            infer_with_schema(&source, &schema(csv)),
            Err(vec!["E7104".into()])
        );
        let (tokens, _) = crate::lexer::lex("t.bd", &source);
        let (program, _) = crate::parser::parse(tokens);
        let (_, diagnostics) = crate::types::check(&program, &schema(csv));
        let messages: Vec<&str> = diagnostics
            .iter()
            .map(|item| item.message.as_str())
            .collect();
        assert_eq!(
            messages,
            vec![
                "Primary-key column `uid` is missing from table `customers` in the database schema."
            ]
        );
    }

    #[test]
    fn server_calls_and_environment_values() {
        let source = "environment development {\n    secret val token: String = env(\"T\", default = \"x\")\n    runtime val porta: Int = envOrNull(\"PORT\")\n}\nserver fun eco(x: Int): Int = x\nclient fun usa(): Int = server.eco(1)\nclient fun segredo(): String = token\nfun porta2() = porta ?: 80\n";
        assert_eq!(scheme(source, "usa"), "() -> Int ! {server.call}");
        assert_eq!(scheme(source, "segredo"), "() -> String");
        assert_eq!(scheme(source, "porta2"), "() -> Int");
        assert_eq!(
            errors("client fun f(): Int = server.nada(1)"),
            vec!["E7103"]
        );
    }

    #[test]
    fn ui_trees() {
        let page = "data class Item(val nome: String, val preco: Money)\nclient component Pagina(item: Item) {\n    UI {\n        Section(\"hero\", \"top\") {\n            Heading(1, \"titulo\") { Text(item.nome) }\n            Text(item.preco)\n        }\n    }\n}\n";
        assert_eq!(scheme(page, "Pagina"), "(Item) -> Ui");
        assert_eq!(
            errors("client component P(xs: List<String>) {\n    UI { Text(xs) }\n}"),
            vec!["E7105"]
        );
        assert_eq!(
            errors("client component P() {\n    UI { Div(true) { Text(\"a\") } }\n}"),
            vec!["E7101"]
        );
        assert_eq!(errors("client component P() {\n    1\n}"), vec!["E7101"]);
        assert_eq!(
            errors("client component P() {\n    UI { 1 }\n}"),
            vec!["E7101"]
        );
        assert_eq!(
            errors("fun f(x: Int) = f(x) { Text(\"a\") }"),
            vec!["E7004"]
        );
    }

    #[test]
    fn server_only_names_program_functions_and_names_do_not_collide() {
        assert_eq!(errors("fun f() = server.first"), vec!["E7103"]);
        let env = "environment development {\n    secret val token: String = env(\"T\", default = \"x\")\n}\n";
        assert_eq!(
            errors(&format!("{env}fun f() = server.token")),
            vec!["E7103"]
        );
        assert_eq!(errors(&format!("{env}fun token(): Int = 1")), vec!["E7110"]);
    }

    #[test]
    fn unannotated_ui_arguments_get_a_hint() {
        assert_eq!(
            errors("client component P(x) {\n    UI { Text(x) }\n}"),
            vec!["E7003", "E7105"]
        );
    }

    #[test]
    fn user_names_shadow_bind_forms() {
        assert_eq!(
            scheme("fun Text(x: Int): Int = x\nfun f() = Text(1)", "f"),
            "() -> Int"
        );
        assert_eq!(
            scheme(
                "data class D(val clientes: Int)\nfun f(db: D) = db.clientes",
                "f"
            ),
            "(D) -> Int"
        );
        assert_eq!(
            scheme("data class Item(val n: Int)\nfun f() = Item(1)", "f"),
            "() -> Item"
        );
        assert_eq!(
            scheme("data class Link(val u: Url)\nfun f(u: Url) = Link(u)", "f"),
            "(Url) -> Link"
        );
        assert_eq!(
            scheme(
                "data class S(val eco: Int)\nfun f(server: S) = server.eco",
                "f"
            ),
            "(S) -> Int"
        );
    }

    fn db_schemes(source: &str) -> Vec<(String, String)> {
        with_db(source).unwrap_or_else(|codes| panic!("{codes:?}"))
    }

    fn scheme_in(schemes: &[(String, String)], name: &str) -> String {
        schemes
            .iter()
            .find(|(other, _)| other == name)
            .unwrap()
            .1
            .clone()
    }

    #[test]
    fn effects_propagate_through_calls_and_recursion() {
        let schemes = db_schemes(
            "fun ler(id: ClienteId) = db.clientes.require(id)\nfun usa(id: ClienteId) = ler(id).nome\nfun a(n: Int, id: ClienteId): Cliente = if (n == 0) db.clientes.require(id) else b(n - 1, id)\nfun b(n: Int, id: ClienteId): Cliente = a(n, id)\nfun puro(x: Int) = x + 1\n",
        );
        assert_eq!(
            scheme_in(&schemes, "ler"),
            "(ClienteId) -> Cliente ! {db.read}"
        );
        assert_eq!(
            scheme_in(&schemes, "usa"),
            "(ClienteId) -> String ! {db.read}"
        );
        assert_eq!(
            scheme_in(&schemes, "a"),
            "(Int, ClienteId) -> Cliente ! {db.read}"
        );
        assert_eq!(
            scheme_in(&schemes, "b"),
            "(Int, ClienteId) -> Cliente ! {db.read}"
        );
        assert_eq!(scheme_in(&schemes, "puro"), "(Int) -> Int");
    }

    #[test]
    fn effects_are_polymorphic() {
        assert_eq!(
            scheme("fun aplica(f, x) = f(x)", "aplica"),
            "(('a) -> 'b ! 'e, 'a) -> 'b ! 'e"
        );
        let schemes = db_schemes(
            "fun nomes(ids: List<ClienteId>) = map(ids, { id -> db.clientes.require(id).nome })\nfun dobros(xs: List<Int>) = map(xs, { x -> x * 2 })\n",
        );
        assert_eq!(
            scheme_in(&schemes, "nomes"),
            "(List<ClienteId>) -> List<String> ! {db.read}"
        );
        assert_eq!(scheme_in(&schemes, "dobros"), "(List<Int>) -> List<Int>");
    }

    #[test]
    fn pure_functions_fit_where_effects_are_allowed() {
        let schemes = db_schemes(
            "fun g(f: (Int) -> Int ! [db.read]): Int = f(1)\nfun dobro(x: Int): Int = x * 2\nfun usa() = g(dobro)\n",
        );
        assert_eq!(
            scheme_in(&schemes, "g"),
            "((Int) -> Int ! {db.read}) -> Int ! {db.read}"
        );
        assert_eq!(scheme_in(&schemes, "usa"), "() -> Int ! {db.read}");
        let rejected = with_db(
            "fun h(f: (Int) -> String): String = f(1)\nfun usa(id: ClienteId) = h({ x -> db.clientes.require(id).nome })\n",
        );
        assert_eq!(rejected, Err(vec!["E7101".to_string()]));
    }

    #[test]
    fn lambdas_carry_their_own_effects() {
        let schemes = db_schemes("fun criar(id: ClienteId) = { -> db.clientes.require(id) }\n");
        assert_eq!(
            scheme_in(&schemes, "criar"),
            "(ClienteId) -> (() -> Cliente ! {db.read})"
        );
    }

    #[test]
    fn server_calls_produce_only_server_call() {
        let schemes = db_schemes(
            "server fun buscar(id: ClienteId): Cliente effects [db.read] = db.clientes.require(id)\nclient fun tela(id: ClienteId) = server.buscar(id).nome\nclient fun todos(ids: List<ClienteId>) = map(ids, server.buscar)\n",
        );
        assert_eq!(
            scheme_in(&schemes, "buscar"),
            "(ClienteId) -> Cliente ! {db.read}"
        );
        assert_eq!(
            scheme_in(&schemes, "tela"),
            "(ClienteId) -> String ! {server.call}"
        );
        assert_eq!(
            scheme_in(&schemes, "todos"),
            "(List<ClienteId>) -> List<Cliente> ! {server.call}"
        );
    }

    #[test]
    fn declared_effects_must_match_exactly() {
        assert_eq!(
            with_db("server fun f(id: ClienteId): Cliente = db.clientes.require(id)\n"),
            Err(vec!["E7402".to_string()])
        );
        assert_eq!(
            with_db(
                "server fun f(id: ClienteId): Cliente effects [db.read, db.write] = db.clientes.require(id)\n"
            ),
            Err(vec!["E7403".to_string()])
        );
        assert_eq!(
            with_db("fun ler(id: ClienteId) = db.clientes.require(id)\nserver fun f(id: ClienteId): Cliente effects [db.read] = ler(id)\n")
                .map(|schemes| scheme_in(&schemes, "f")),
            Ok("(ClienteId) -> Cliente ! {db.read}".to_string())
        );
        assert_eq!(
            with_db(
                "fun ler(id: ClienteId) = db.clientes.require(id)\nserver fun f(id: ClienteId): Cliente = ler(id)\n"
            ),
            Err(vec!["E7402".to_string()])
        );
    }

    #[test]
    fn typed_program_lists_the_effects_of_each_function() {
        let source = format!(
            "{ENTIDADE}fun ler(id: ClienteId) = db.clientes.require(id)\nserver fun buscar(id: ClienteId): Cliente effects [db.read] = ler(id)\nclient fun tela(id: ClienteId) = server.buscar(id).nome\nfun puro(x: Int) = x + 1\n"
        );
        let (tokens, _) = crate::lexer::lex("test.bd", &source);
        let (program, _) = crate::parser::parse(tokens);
        let (typed, diagnostics) = crate::types::check(&program, &schema(CLIENTES));
        assert!(
            !crate::diagnostic::has_errors(&diagnostics),
            "{diagnostics:?}"
        );
        assert_eq!(typed.effects["ler"], vec!["db.read".to_string()]);
        assert_eq!(typed.effects["buscar"], vec!["db.read".to_string()]);
        assert_eq!(typed.effects["tela"], vec!["server.call".to_string()]);
        assert!(typed.effects["puro"].is_empty());
    }

    #[test]
    fn mutual_recursion_with_effects() {
        let schemes = db_schemes(
            "fun a(n: Int, id: ClienteId): Int = if (n == 0) length(listOf(db.clientes.require(id))) else b(n, id)\nfun b(n: Int, id: ClienteId): Int = a(n - 1, id)\n",
        );
        assert_eq!(
            scheme_in(&schemes, "a"),
            "(Int, ClienteId) -> Int ! {db.read}"
        );
        assert_eq!(
            scheme_in(&schemes, "b"),
            "(Int, ClienteId) -> Int ! {db.read}"
        );
    }

    #[test]
    fn mixed_groups_check_declared_effects_after_all_bodies() {
        let a = "fun a(n: Int, id: ClienteId): Int = if (n == 0) length(listOf(db.clientes.require(id))) else b(n - 1, id)\n";
        let b = "fun b(n: Int, id: ClienteId) effects [db.read] = a(n, id)\n";
        for source in [format!("{b}{a}"), format!("{a}{b}")] {
            let schemes = db_schemes(&source);
            assert_eq!(
                scheme_in(&schemes, "a"),
                "(Int, ClienteId) -> Int ! {db.read}"
            );
            assert_eq!(
                scheme_in(&schemes, "b"),
                "(Int, ClienteId) -> Int ! {db.read}"
            );
        }
    }

    #[test]
    fn rows_of_returned_functions_stay_open() {
        let schemes = db_schemes(
            "fun gravar(nome: String) = db.clientes.insert { nome = nome }\nfun criar(id: ClienteId) = { -> db.clientes.require(id) }\nfun outro() = { -> gravar(\"x\") }\nfun pick(c: Boolean, id: ClienteId) = if (c) criar(id) else outro()\n",
        );
        assert_eq!(
            scheme_in(&schemes, "criar"),
            "(ClienteId) -> (() -> Cliente ! {db.read})"
        );
        assert_eq!(
            scheme_in(&schemes, "pick"),
            "(Boolean, ClienteId) -> (() -> Cliente ! {db.read, db.write})"
        );
    }

    #[test]
    fn row_variables_in_parameters_are_displayed() {
        assert_eq!(
            scheme("fun k(f: () -> Int ! [e]) = 1", "k"),
            "(() -> Int ! 'e) -> Int"
        );
    }

    #[test]
    fn server_functions_as_values_can_gain_effects() {
        let schemes = db_schemes(
            "server fun buscar(id: ClienteId): Cliente effects [db.read] = db.clientes.require(id)\nfun h(f: (ClienteId) -> Cliente ! [server.call, db.read], id: ClienteId) = f(id)\nfun t(id: ClienteId) = h(server.buscar, id).nome\n",
        );
        assert_eq!(
            scheme_in(&schemes, "t"),
            "(ClienteId) -> String ! {db.read, server.call}"
        );
    }

    fn db_errors(source: &str) -> Vec<String> {
        with_db(source)
            .err()
            .unwrap_or_else(|| panic!("expected errors"))
    }

    #[test]
    fn domains_cap_the_effects() {
        assert_eq!(
            db_errors("pure fun p(id: ClienteId): String = db.clientes.require(id).nome\n"),
            vec!["E7401"]
        );
        assert_eq!(
            db_errors(
                "client component P(id: ClienteId) {\n    val c = db.clientes.require(id)\n    UI { Text(c.nome) }\n}\n"
            ),
            vec!["E7401"]
        );
        assert_eq!(
            db_errors(
                "fun ler(id: ClienteId) = db.clientes.require(id)\nclient fun tela(id: ClienteId) = ler(id).nome\n"
            ),
            vec!["E7401"]
        );
        assert_eq!(
            db_errors(
                "server fun a(id: ClienteId): Cliente effects [db.read] = db.clientes.require(id)\nserver fun b(id: ClienteId): Cliente = server.a(id)\n"
            ),
            vec!["E7401"]
        );
        assert_eq!(
            db_errors("pure fun p(): Int effects [db.read] = 1\n"),
            vec!["E7401"]
        );
    }

    #[test]
    fn client_code_reaches_endpoints_only_through_server_calls() {
        assert_eq!(
            db_errors("server fun eco(x: Int): Int = x\nclient fun usa(): Int = eco(1)\n"),
            vec!["E7404"]
        );
        let schemes =
            db_schemes("server fun eco(x: Int): Int = x\nclient fun usa(): Int = server.eco(1)\n");
        assert_eq!(scheme_in(&schemes, "usa"), "() -> Int ! {server.call}");
    }

    #[test]
    fn lambdas_only_count_when_called() {
        let defined = "client component P(id: ClienteId) {\n    val f = { -> db.clientes.require(id) }\n    UI { Text(\"x\") }\n}\n";
        assert!(with_db(defined).is_ok());
        let called = "client component P(id: ClienteId) {\n    val f = { -> db.clientes.require(id) }\n    val c = f()\n    UI { Text(c.nome) }\n}\n";
        assert_eq!(db_errors(called), vec!["E7401"]);
    }

    #[test]
    fn effect_variables_in_signatures() {
        let mapear =
            "fun <A, B> mapear(xs: List<A>, f: (A) -> B ! [e]): List<B> effects [e] = map(xs, f)\n";
        let schemes = db_schemes(&format!(
            "{mapear}fun nomes(ids: List<ClienteId>) = mapear(ids, {{ id -> db.clientes.require(id).nome }})\nfun dobros(xs: List<Int>) = mapear(xs, {{ x -> x * 2 }})\n"
        ));
        assert_eq!(
            scheme_in(&schemes, "mapear"),
            "(List<'a>, ('a) -> 'b ! 'e) -> List<'b> ! 'e"
        );
        assert_eq!(
            scheme_in(&schemes, "nomes"),
            "(List<ClienteId>) -> List<String> ! {db.read}"
        );
        assert_eq!(scheme_in(&schemes, "dobros"), "(List<Int>) -> List<Int>");
        assert_eq!(
            db_errors("fun <A> chama(f: () -> A ! [e]): A effects [] = f()\n"),
            vec!["E7402"]
        );
        assert_eq!(
            db_errors("fun ignora(f: () -> Int ! [e]): Int effects [e] = 1\n"),
            vec!["E7403"]
        );
        let flexible = db_schemes("fun chama(f: () -> Int ! [e]) = f()\n");
        assert_eq!(
            scheme_in(&flexible, "chama"),
            "(() -> Int ! 'e) -> Int ! 'e"
        );
        assert_eq!(
            db_errors("server fun f(): Int effects [db.delete] = 1\n"),
            vec!["E7405"]
        );
    }

    #[test]
    fn effect_diagnostics_show_the_path() {
        let source = format!(
            "{ENTIDADE}fun ler(id: ClienteId) = db.clientes.require(id)\nserver fun f(id: ClienteId): Cliente = ler(id)\nclient fun tela(id: ClienteId) = ler(id).nome\n"
        );
        let (tokens, _) = crate::lexer::lex("test.bd", &source);
        let (program, _) = crate::parser::parse(tokens);
        let (_, diagnostics) = crate::types::check(&program, &schema(CLIENTES));
        for code in ["E7402", "E7401"] {
            let diagnostic = diagnostics
                .iter()
                .find(|item| item.code == code)
                .unwrap_or_else(|| panic!("{code} missing: {diagnostics:?}"));
            assert_eq!(diagnostic.path.len(), 3, "{code}: {:?}", diagnostic.path);
            assert!(
                diagnostic.path[1].starts_with("ler ("),
                "{:?}",
                diagnostic.path
            );
            assert!(
                diagnostic.path[2].starts_with("db.clientes.require ("),
                "{:?}",
                diagnostic.path
            );
        }
    }

    fn db_diagnostics(source: &str) -> Vec<crate::diagnostic::Diagnostic> {
        let source = format!("{ENTIDADE}{source}");
        let (tokens, _) = crate::lexer::lex("test.bd", &source);
        let (program, _) = crate::parser::parse(tokens);
        crate::types::check(&program, &schema(CLIENTES)).1
    }

    fn only_e7401(source: &str) -> crate::diagnostic::Diagnostic {
        let diagnostics = db_diagnostics(source);
        let codes: Vec<&str> = diagnostics.iter().map(|item| item.code).collect();
        assert_eq!(codes, vec!["E7401"], "{diagnostics:?}");
        diagnostics.into_iter().next().unwrap()
    }

    #[test]
    fn ceilings_do_not_travel_with_rows() {
        let client = "client fun t(f) = f()\n";
        let plain = format!(
            "{client}fun a(k: ClienteId): Cliente {{\n    val x = t({{ -> 1 }})\n    return db.clientes.require(k)\n}}\n"
        );
        assert!(with_db(&plain).is_ok(), "{:?}", with_db(&plain));
        let server = format!(
            "{client}server fun a(k: ClienteId): Cliente effects [db.read] {{\n    val x = t({{ -> 1 }})\n    return db.clientes.require(k)\n}}\n"
        );
        assert!(with_db(&server).is_ok(), "{:?}", with_db(&server));
        let recursion = only_e7401(
            "client fun b(n: Int, k: ClienteId) = a(n, k)\nfun a(n: Int, k: ClienteId) = if (n == 0) db.clientes.require(k).nome else b(n - 1, k)\n",
        );
        assert!(
            recursion.message.starts_with("`b`"),
            "{}",
            recursion.message
        );
    }

    #[test]
    fn effect_variables_are_polymorphic_in_every_domain() {
        for t in [
            "pure fun t(f: () -> Cliente ! [e]): Cliente effects [e] = f()\n",
            "pure fun t(f: () -> Cliente ! [e]) = f()\n",
        ] {
            let schemes = db_schemes(&format!(
                "{t}fun u(k: ClienteId) = t({{ -> db.clientes.require(k) }})\n"
            ));
            assert_eq!(
                scheme_in(&schemes, "u"),
                "(ClienteId) -> Cliente ! {db.read}",
                "{t}"
            );
            let component = only_e7401(&format!(
                "{t}client component P(k: ClienteId) {{\n    val c = t({{ -> db.clientes.require(k) }})\n    UI {{ Text(c.nome) }}\n}}\n"
            ));
            assert!(
                component.message.starts_with("`P`"),
                "{}",
                component.message
            );
        }
    }

    #[test]
    fn endpoints_used_as_values_in_client_code_are_rejected() {
        assert_eq!(
            db_errors(
                "server fun eco(x: Int): Int = x\nclient fun usa(): Int {\n    val g = eco\n    g(1)\n}\n"
            ),
            vec!["E7404"]
        );
        assert_eq!(
            db_errors(
                "server fun eco(x: Int): Int = x\nclient fun usa(xs: List<Int>) = map(xs, eco)\n"
            ),
            vec!["E7404"]
        );
    }

    #[test]
    fn ceiling_paths_and_hints() {
        let through_map = only_e7401(
            "fun ler(id: ClienteId) = db.clientes.require(id)\nclient component P(ids: List<ClienteId>) {\n    val xs = map(ids, { id -> ler(id) })\n    UI { Text(\"x\") }\n}\n",
        );
        assert_eq!(through_map.path.len(), 3, "{:?}", through_map.path);
        assert_eq!(through_map.path[0], "P");
        assert!(
            through_map.path[1].starts_with("ler ("),
            "{:?}",
            through_map.path
        );
        assert!(
            through_map.path[2].starts_with("db.clientes.require ("),
            "{:?}",
            through_map.path
        );
        assert_eq!(
            through_map.hint.as_deref(),
            Some(
                "Database access runs on the server: move it into a `server fun` and call it with `server.f(...)`."
            )
        );
        let server_call = only_e7401(
            "server fun a(id: ClienteId): Cliente effects [db.read] = db.clientes.require(id)\nfun h(id: ClienteId) = server.a(id)\nserver fun b(id: ClienteId): Cliente = h(id)\n",
        );
        assert!(
            server_call.path.last().unwrap().starts_with("server.a ("),
            "{:?}",
            server_call.path
        );
        assert_eq!(
            server_call.hint.as_deref(),
            Some("Call `a(...)` directly; `server.` is only for client code.")
        );
        let pure = only_e7401("pure fun p(id: ClienteId): String = db.clientes.require(id).nome\n");
        assert_eq!(
            pure.hint.as_deref(),
            Some("`pure` functions cannot perform effects.")
        );
        let endpoint = only_e7401(
            "server fun x(id: ClienteId): Cliente effects [db.read] = db.clientes.require(id)\nfun h(id: ClienteId) = x(id)\nclient fun c(id: ClienteId) = h(id).nome\n",
        );
        assert_eq!(
            endpoint.hint.as_deref(),
            Some("Call it through `server.x(...)`.")
        );
    }

    #[test]
    fn declared_effect_variables_are_enforced_without_a_full_signature() {
        let reads = "fun ap(id: ClienteId, f: () -> Int ! [e]) effects [e] = f() + length(listOf(db.clientes.require(id).nome))\n";
        let diagnostics = db_diagnostics(reads);
        let codes: Vec<&str> = diagnostics.iter().map(|item| item.code).collect();
        assert_eq!(codes, vec!["E7402"], "{diagnostics:?}");
        assert_eq!(diagnostics[0].path.len(), 2, "{:?}", diagnostics[0].path);
        assert!(
            diagnostics[0].path[1].starts_with("db.clientes.require ("),
            "{:?}",
            diagnostics[0].path
        );
        assert_eq!(
            db_errors("fun ap(x: Int, f: () -> Int ! [e]) effects [e] = x\n"),
            vec!["E7403"]
        );
        // The fully annotated versions behave the same.
        assert_eq!(
            db_errors(
                "fun ap(id: ClienteId, f: () -> Int ! [e]): Int effects [e] = f() + length(listOf(db.clientes.require(id).nome))\n"
            ),
            vec!["E7402"]
        );
        assert_eq!(
            db_errors("fun ap(x: Int, f: () -> Int ! [e]): Int effects [e] = x\n"),
            vec!["E7403"]
        );
        // A variable used exactly stays generic in the published scheme.
        let schemes = db_schemes(
            "fun ap(x, f: () -> Int ! [e]) effects [e] = f() + x\nfun usa(id: ClienteId) = ap(1, { -> length(listOf(db.clientes.require(id))) })\n",
        );
        assert_eq!(
            scheme_in(&schemes, "ap"),
            "(Int, () -> Int ! 'e) -> Int ! 'e"
        );
        assert_eq!(scheme_in(&schemes, "usa"), "(ClienteId) -> Int ! {db.read}");
    }

    #[test]
    fn group_members_share_row_variables_before_closing() {
        let schemes = db_schemes(
            "fun a(n: Int): Int = if (n == 0) 0 else b(n, { -> 1 })\nfun b(n: Int, g) = g() + a(n - 1)\nfun use(id: ClienteId) = b(1, { -> length(listOf(db.clientes.require(id).nome)) })\n",
        );
        assert_eq!(
            scheme_in(&schemes, "b"),
            "(Int, () -> Int ! 'e) -> Int ! 'e"
        );
        assert_eq!(scheme_in(&schemes, "use"), "(ClienteId) -> Int ! {db.read}");
    }

    #[test]
    fn server_calls_require_a_server_endpoint() {
        for target in [
            "fun ler(id: ClienteId) = db.clientes.require(id)\n",
            "pure fun ler(id: ClienteId): ClienteId = id\n",
            "client fun ler(id: ClienteId): ClienteId = id\n",
            "client component ler(id: ClienteId) {\n    UI { Text(\"x\") }\n}\n",
        ] {
            let diagnostics = db_diagnostics(&format!(
                "{target}client fun t(id: ClienteId) = server.ler(id)\n"
            ));
            let codes: Vec<&str> = diagnostics.iter().map(|item| item.code).collect();
            assert_eq!(codes, vec!["E7404"], "{target}: {diagnostics:?}");
            assert_eq!(
                diagnostics[0].message,
                "`server.ler` is not a server endpoint; declare it `server fun`."
            );
        }
    }

    #[test]
    fn origins_inside_uncalled_lambdas_are_not_recorded() {
        let diagnostics = db_diagnostics(
            "fun ler(id: ClienteId) = db.clientes.require(id)\nserver fun s(id: ClienteId): Cliente {\n    val f = { -> db.clientes.require(id) }\n    return ler(id)\n}\n",
        );
        let codes: Vec<&str> = diagnostics.iter().map(|item| item.code).collect();
        assert_eq!(codes, vec!["E7402"], "{diagnostics:?}");
        let path = &diagnostics[0].path;
        assert_eq!(path.len(), 3, "{path:?}");
        assert!(path[1].starts_with("ler ("), "{path:?}");
        assert!(
            diagnostics[0]
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("the call to `ler`")),
            "{:?}",
            diagnostics[0].hint
        );
        // A lambda that is called still contributes its origin.
        let called = db_diagnostics(
            "server fun s(id: ClienteId): Cliente {\n    val f = { -> db.clientes.require(id) }\n    return f()\n}\n",
        );
        assert_eq!(called.len(), 1, "{called:?}");
        assert!(
            called[0]
                .path
                .last()
                .unwrap()
                .starts_with("db.clientes.require ("),
            "{:?}",
            called[0].path
        );
    }
}
