//! Type environment: declared types, constructors and the built-in vocabulary.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{Field, Program, TypeKind, TypeRef};
use crate::diagnostic::{Diagnostic, Span};
use crate::parser::expr::parse_type_text;
use crate::syntax::expr::TypeExpr;

use super::effects::{Label, Row};
use super::exhaustive::{CtorSig, Signatures};
use super::ty::{Type, TypeTable};

/// Built-in types without type arguments.
pub const SCALARS: &[&str] = &[
    "Unit",
    "String",
    "Int",
    "Long",
    "Float",
    "Double",
    "Decimal",
    "Boolean",
    "UUID",
    "Date",
    "DateTime",
    "Instant",
    "Duration",
    "Money",
    "Email",
    "Url",
    "DatabaseUrl",
    "Port",
    "Secret",
    "Ui",
];

/// Built-in generic types and their arity. `Result` is registered as a sealed type instead.
pub const GENERICS: &[(&str, usize)] = &[("List", 1), ("Set", 1), ("Map", 2), ("Option", 1)];

/// UI nodes accepted by the certified Elm generator in `certified.rs`, plus `UI` and `Text`.
pub const UI_NODES: &[&str] = &[
    "UI",
    "Page",
    "Container",
    "Div",
    "Grid",
    "Stack",
    "Header",
    "Nav",
    "Main",
    "Section",
    "Article",
    "Aside",
    "Footer",
    "Paragraph",
    "Span",
    "Strong",
    "Emphasis",
    "Small",
    "List",
    "Item",
    "Pre",
    "Code",
    "Heading",
    "Link",
    "Rule",
    "Text",
];

/// Types `Text(...)` can display, besides distinct types over them.
const DISPLAY_SCALARS: &[&str] = &[
    "String", "Int", "Long", "Double", "Float", "Decimal", "Boolean", "UUID", "Date", "DateTime",
    "Instant", "Duration", "Money", "Email", "Url",
];

/// Stands for a field type that failed to parse (already reported).
const ERROR_TYPE: &str = "<error>";

#[derive(Clone, Debug)]
pub struct TypeInfo {
    pub kind: TypeKind,
    pub params: Vec<String>,
    pub ctors: Vec<String>,
    pub underlying: Option<TypeExpr>,
}

#[derive(Clone, Debug)]
pub struct CtorInfo {
    pub name: String,
    pub type_name: String,
    pub type_params: Vec<String>,
    pub fields: Vec<(String, TypeExpr)>,
    pub span: Span,
}

#[derive(Clone, Debug, Default)]
pub struct Env {
    pub types: BTreeMap<String, TypeInfo>,
    pub ctors: BTreeMap<String, CtorInfo>,
    /// `Ui` and the user types that cannot support equality (see `non_equatable_types`).
    pub non_equatable: BTreeSet<String>,
}

impl Env {
    pub fn build(program: &Program, diagnostics: &mut Vec<Diagnostic>) -> Env {
        let mut env = Env::default();
        env.install_result();
        let mut functions = BTreeSet::new();
        for function in &program.functions {
            if !functions.insert(function.name.as_str()) {
                diagnostics.push(Diagnostic::error(
                    "E7110",
                    format!("Function `{}` is declared more than once.", function.name),
                    function.span.clone(),
                ));
            }
        }
        for declaration in &program.types {
            if env.type_arity(&declaration.name).is_some() {
                diagnostics.push(Diagnostic::error(
                    "E7110",
                    format!(
                        "Type `{}` is declared more than once or conflicts with a built-in type.",
                        declaration.name
                    ),
                    declaration.span.clone(),
                ));
                continue;
            }
            if declaration.kind == TypeKind::Entity && !declaration.type_params.is_empty() {
                diagnostics.push(Diagnostic::error(
                    "E7004",
                    "An `entity` cannot have type parameters; it maps to a concrete database table.",
                    declaration.span.clone(),
                ));
            }
            let constructors: Vec<(String, Vec<Field>, Span)> = match declaration.kind {
                TypeKind::Data | TypeKind::Entity => {
                    vec![(
                        declaration.name.clone(),
                        declaration.fields.clone(),
                        declaration.span.clone(),
                    )]
                }
                TypeKind::Sealed | TypeKind::Enum => declaration
                    .cases
                    .iter()
                    .map(|case| (case.name.clone(), case.fields.clone(), case.span.clone()))
                    .collect(),
                TypeKind::Distinct => vec![(
                    declaration.name.clone(),
                    vec![Field {
                        name: "value".into(),
                        type_ref: declaration.underlying.clone().unwrap_or_else(TypeRef::unit),
                        mutable: false,
                        span: declaration.span.clone(),
                    }],
                    declaration.span.clone(),
                )],
            };
            let mut names = Vec::new();
            for (name, fields, span) in constructors {
                if let Some(existing) = env.ctors.get(&name) {
                    diagnostics.push(Diagnostic::error(
                        "E7110",
                        format!(
                            "Constructor `{name}` is already declared by `{}` at {}:{}:{}; constructor names must be unique.",
                            existing.type_name, existing.span.file, existing.span.line, existing.span.column
                        ),
                        span,
                    ));
                    continue;
                }
                let mut lowered = Vec::new();
                for field in &fields {
                    if field.mutable {
                        diagnostics.push(
                            Diagnostic::error(
                                "E7002",
                                format!(
                                    "Field `{}` is declared with `var`; Bind values are immutable.",
                                    field.name
                                ),
                                field.span.clone(),
                            )
                            .with_hint("Use `val`."),
                        );
                    }
                    lowered.push((
                        field.name.clone(),
                        parse_field_type(&field.type_ref, &field.span, diagnostics),
                    ));
                }
                names.push(name.clone());
                env.ctors.insert(
                    name.clone(),
                    CtorInfo {
                        name,
                        type_name: declaration.name.clone(),
                        type_params: declaration.type_params.clone(),
                        fields: lowered,
                        span,
                    },
                );
            }
            let underlying = declaration
                .underlying
                .as_ref()
                .map(|type_ref| parse_field_type(type_ref, &declaration.span, &mut Vec::new()));
            env.types.insert(
                declaration.name.clone(),
                TypeInfo {
                    kind: declaration.kind.clone(),
                    params: declaration.type_params.clone(),
                    ctors: names,
                    underlying,
                },
            );
        }
        // Field types can mention types declared later, so they are checked after registration.
        for ctor in env.ctors.values() {
            let rigid = rigid_vars(&ctor.type_params);
            for (_, field) in &ctor.fields {
                env.lower(field, &rigid, diagnostics);
            }
        }
        env.non_equatable = env.non_equatable_types();
        env
    }

    /// A type is non-equatable when a field of one of its constructors contains a function,
    /// `Ui` or another non-equatable type (a fixpoint, so wrappers and recursion are covered).
    /// The type's own parameters do not count: equality checks the type arguments instead.
    fn non_equatable_types(&self) -> BTreeSet<String> {
        let mut set = BTreeSet::from(["Ui".to_string()]);
        loop {
            let mut changed = false;
            for (name, info) in &self.types {
                if set.contains(name) {
                    continue;
                }
                let contains = info
                    .ctors
                    .iter()
                    .filter_map(|ctor| self.ctors.get(ctor))
                    .any(|ctor| {
                        ctor.fields.iter().any(|(_, field)| {
                            contains_non_equatable(field, &ctor.type_params, &set)
                        })
                    });
                if contains {
                    set.insert(name.clone());
                    changed = true;
                }
            }
            if !changed {
                return set;
            }
        }
    }

    fn install_result(&mut self) {
        let param = |name: &str| TypeExpr::Named {
            name: name.into(),
            args: Vec::new(),
            span: Span::default(),
        };
        for (ctor, field, ty) in [("Ok", "value", "T"), ("Err", "error", "E")] {
            self.ctors.insert(
                ctor.into(),
                CtorInfo {
                    name: ctor.into(),
                    type_name: "Result".into(),
                    type_params: vec!["T".into(), "E".into()],
                    fields: vec![(field.into(), param(ty))],
                    span: Span::default(),
                },
            );
        }
        self.types.insert(
            "Result".into(),
            TypeInfo {
                kind: TypeKind::Sealed,
                params: vec!["T".into(), "E".into()],
                ctors: vec!["Ok".into(), "Err".into()],
                underlying: None,
            },
        );
    }

    pub fn type_arity(&self, name: &str) -> Option<usize> {
        if SCALARS.contains(&name) {
            return Some(0);
        }
        if let Some((_, arity)) = GENERICS.iter().find(|(generic, _)| *generic == name) {
            return Some(*arity);
        }
        self.types.get(name).map(|info| info.params.len())
    }

    pub fn lower(
        &self,
        te: &TypeExpr,
        vars: &BTreeMap<String, Type>,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Type {
        self.lower_with_effects(te, vars, &BTreeMap::new(), diagnostics)
    }

    /// Like `lower`, with the effect variables in scope (a function signature's `effects`).
    pub fn lower_with_effects(
        &self,
        te: &TypeExpr,
        vars: &BTreeMap<String, Type>,
        effects: &BTreeMap<String, Row>,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Type {
        match te {
            TypeExpr::Nullable(inner) => {
                Type::option(self.lower_with_effects(inner, vars, effects, diagnostics))
            }
            TypeExpr::Function {
                params,
                result,
                effects: names,
            } => {
                let params = params
                    .iter()
                    .map(|param| self.lower_with_effects(param, vars, effects, diagnostics))
                    .collect();
                let lowered = self.lower_with_effects(result, vars, effects, diagnostics);
                let row = self.lower_row(names, effects, &type_span(result), diagnostics);
                Type::Fun(params, Box::new(lowered), row)
            }
            TypeExpr::Named { name, args, span } => {
                if name == ERROR_TYPE {
                    return Type::Error;
                }
                if let Some(var) = vars.get(name) {
                    if !args.is_empty() {
                        diagnostics.push(Diagnostic::error(
                            "E7102",
                            format!("Type parameter `{name}` does not take type arguments."),
                            span.clone(),
                        ));
                        return Type::Error;
                    }
                    return var.clone();
                }
                let Some(arity) = self.type_arity(name) else {
                    diagnostics.push(Diagnostic::error(
                        "E7103",
                        format!("Unknown type `{name}`."),
                        span.clone(),
                    ));
                    return Type::Error;
                };
                if arity != args.len() {
                    diagnostics.push(Diagnostic::error(
                        "E7102",
                        format!(
                            "Type `{name}` expects {arity} type argument(s), found {}.",
                            args.len()
                        ),
                        span.clone(),
                    ));
                    return Type::Error;
                }
                Type::Con(
                    name.clone(),
                    args.iter()
                        .map(|arg| self.lower_with_effects(arg, vars, effects, diagnostics))
                        .collect(),
                )
            }
        }
    }

    /// The row written as `[db.read, e]`: known labels plus at most one effect variable.
    pub fn lower_row(
        &self,
        names: &[String],
        effects: &BTreeMap<String, Row>,
        span: &Span,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Row {
        let mut row = Row::empty();
        for name in names {
            if !is_effect_name(name) {
                diagnostics.push(Diagnostic::error(
                    "E7405",
                    format!("`{name}` is not a valid effect name."),
                    span.clone(),
                ));
                continue;
            }
            if let Some(label) = Label::parse(name) {
                if label.is_known() {
                    row.labels.insert(label);
                } else {
                    diagnostics.push(Diagnostic::error(
                        "E7405",
                        format!(
                            "Unknown effect `{name}`; the known effects are db.read, db.write and server.call."
                        ),
                        span.clone(),
                    ));
                }
                continue;
            }
            let Some(variable) = effects.get(name) else {
                diagnostics.push(Diagnostic::error(
                    "E7405",
                    format!(
                        "Unknown effect variable `{name}`; effect variables are only allowed in function signatures."
                    ),
                    span.clone(),
                ));
                continue;
            };
            row.labels.extend(variable.labels.iter().cloned());
            match (row.tail, variable.tail) {
                (Some(existing), Some(tail)) if existing != tail => {
                    diagnostics.push(Diagnostic::error(
                        "E7004",
                        "An effect row can contain at most one effect variable.",
                        span.clone(),
                    ))
                }
                (_, Some(tail)) => row.tail = Some(tail),
                _ => {}
            }
        }
        row
    }

    pub fn lower_quiet(&self, te: &TypeExpr, vars: &BTreeMap<String, Type>) -> Type {
        self.lower(te, vars, &mut Vec::new())
    }

    pub fn instantiate_ctor(
        &self,
        name: &str,
        table: &mut TypeTable,
        level: u32,
    ) -> (Vec<Type>, Type) {
        let Some(ctor) = self.ctors.get(name) else {
            return (Vec::new(), Type::Error);
        };
        let mut vars = BTreeMap::new();
        let mut args = Vec::new();
        for param in &ctor.type_params {
            let var = table.fresh(level);
            vars.insert(param.clone(), var.clone());
            args.push(var);
        }
        let fields = ctor
            .fields
            .iter()
            .map(|(_, field)| self.lower_quiet(field, &vars))
            .collect();
        (fields, Type::Con(ctor.type_name.clone(), args))
    }

    pub fn instantiate_record(&self, type_name: &str, table: &mut TypeTable, level: u32) -> Type {
        let arity = self
            .types
            .get(type_name)
            .map_or(0, |info| info.params.len());
        Type::Con(
            type_name.into(),
            (0..arity).map(|_| table.fresh(level)).collect(),
        )
    }

    pub fn field_type(&self, type_name: &str, args: &[Type], field: &str) -> Option<Type> {
        let info = self.types.get(type_name)?;
        if !matches!(info.kind, TypeKind::Data | TypeKind::Entity) {
            return None;
        }
        let ctor = self.ctors.get(type_name)?;
        let vars: BTreeMap<String, Type> = ctor
            .type_params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        ctor.fields
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, te)| self.lower_quiet(te, &vars))
    }

    pub fn records_with_field(&self, field: &str) -> Vec<String> {
        self.types
            .iter()
            .filter(|(_, info)| matches!(info.kind, TypeKind::Data | TypeKind::Entity))
            .filter(|(name, _)| {
                self.ctors
                    .get(*name)
                    .is_some_and(|ctor| ctor.fields.iter().any(|(other, _)| other == field))
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn is_displayable(&self, ty: &Type) -> bool {
        self.displayable(ty, 0)
    }

    fn displayable(&self, ty: &Type, depth: usize) -> bool {
        match ty {
            Type::Error => true,
            Type::Con(name, args) if args.is_empty() => {
                DISPLAY_SCALARS.contains(&name.as_str())
                    || (depth < 16
                        && self.types.get(name).is_some_and(|info| {
                            info.kind == TypeKind::Distinct
                                && info.underlying.as_ref().is_some_and(|underlying| {
                                    self.displayable(
                                        &self.lower_quiet(underlying, &BTreeMap::new()),
                                        depth + 1,
                                    )
                                })
                        }))
            }
            _ => false,
        }
    }
}

impl Signatures for Env {
    fn constructors(&self, ty: &Type) -> Option<Vec<CtorSig>> {
        let Type::Con(name, args) = ty else {
            return None;
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
            _ => {
                let info = self.types.get(name)?;
                let vars: BTreeMap<String, Type> = info
                    .params
                    .iter()
                    .cloned()
                    .zip(args.iter().cloned())
                    .collect();
                Some(
                    info.ctors
                        .iter()
                        .filter_map(|ctor| self.ctors.get(ctor))
                        .map(|ctor| CtorSig {
                            name: ctor.name.clone(),
                            args: ctor
                                .fields
                                .iter()
                                .map(|(_, field)| self.lower_quiet(field, &vars))
                                .collect(),
                        })
                        .collect(),
                )
            }
        }
    }
}

fn contains_non_equatable(te: &TypeExpr, params: &[String], set: &BTreeSet<String>) -> bool {
    match te {
        TypeExpr::Function { .. } => true,
        TypeExpr::Nullable(inner) => contains_non_equatable(inner, params, set),
        TypeExpr::Named { name, args, .. } => {
            (!params.contains(name) && set.contains(name))
                || args
                    .iter()
                    .any(|arg| contains_non_equatable(arg, params, set))
        }
    }
}

/// Type parameters inside the body of an annotated function: they only equal themselves.
/// Identifier segments (ASCII letters, digits, `_`) separated by at most one dot.
fn is_effect_name(name: &str) -> bool {
    name.split('.').count() <= 2
        && name.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

fn type_span(te: &TypeExpr) -> Span {
    match te {
        TypeExpr::Named { span, .. } => span.clone(),
        TypeExpr::Nullable(inner) => type_span(inner),
        TypeExpr::Function { result, .. } => type_span(result),
    }
}

pub fn rigid_vars(params: &[String]) -> BTreeMap<String, Type> {
    params
        .iter()
        .map(|param| (param.clone(), Type::con(&format!("'{param}"))))
        .collect()
}

fn parse_field_type(
    type_ref: &TypeRef,
    span: &Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> TypeExpr {
    parse_type_text(&type_ref.display(), span).unwrap_or_else(|diagnostic| {
        diagnostics.push(diagnostic);
        TypeExpr::Named {
            name: ERROR_TYPE.into(),
            args: Vec::new(),
            span: span.clone(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse;

    fn build(source: &str) -> (Env, Vec<&'static str>) {
        let (tokens, _) = lex("env.bd", source);
        let (program, parse_diagnostics) = parse(tokens);
        assert!(parse_diagnostics.is_empty(), "{parse_diagnostics:?}");
        let mut diagnostics = Vec::new();
        let env = Env::build(&program, &mut diagnostics);
        (env, diagnostics.iter().map(|item| item.code).collect())
    }

    use crate::types::effects::{Label, Row};

    fn lower_text(
        env: &Env,
        text: &str,
        effects: &BTreeMap<String, Row>,
    ) -> (Type, Vec<&'static str>) {
        let te = parse_type_text(text, &Span::default()).unwrap();
        let mut diagnostics = Vec::new();
        let ty = env.lower_with_effects(&te, &BTreeMap::new(), effects, &mut diagnostics);
        (ty, diagnostics.iter().map(|item| item.code).collect())
    }

    #[test]
    fn lowers_function_types_with_effect_rows() {
        let (env, _) = build("");
        let (pure, codes) = lower_text(&env, "(Int) -> Int", &BTreeMap::new());
        assert!(codes.is_empty());
        assert_eq!(
            pure,
            Type::Fun(
                vec![Type::con("Int")],
                Box::new(Type::con("Int")),
                Row::empty()
            )
        );
        let (reads, _) = lower_text(&env, "(Int) -> Int ! [db.read]", &BTreeMap::new());
        let Type::Fun(_, _, row) = reads else {
            unreachable!()
        };
        assert_eq!(row, Row::closed([Label::new("db", "read")]));
        let effects: BTreeMap<String, Row> = [("e".to_string(), Row::with_tail([], 7))].into();
        let (open, _) = lower_text(&env, "(Int) -> Int ! [db.write, e]", &effects);
        let Type::Fun(_, _, row) = open else {
            unreachable!()
        };
        assert_eq!(row, Row::with_tail([Label::new("db", "write")], 7));
    }

    #[test]
    fn rejects_unknown_effects_and_two_variables() {
        let (env, _) = build("");
        assert_eq!(
            lower_text(&env, "(Int) -> Int ! [db.delete]", &BTreeMap::new()).1,
            vec!["E7405"]
        );
        assert_eq!(
            lower_text(&env, "(Int) -> Int ! [e]", &BTreeMap::new()).1,
            vec!["E7405"]
        );
        let effects: BTreeMap<String, Row> = [
            ("e".to_string(), Row::with_tail([], 1)),
            ("f".to_string(), Row::with_tail([], 2)),
        ]
        .into();
        assert_eq!(
            lower_text(&env, "(Int) -> Int ! [e, f]", &effects).1,
            vec!["E7004"]
        );
        assert!(
            build("data class C(val f: (Int) -> Int ! [db.read])")
                .1
                .is_empty()
        );
        assert_eq!(
            build("data class C(val f: (Int) -> Int ! [e])").1,
            vec!["E7405"]
        );
    }

    #[test]
    fn rejects_malformed_effect_names() {
        let (env, _) = build("");
        let names: Vec<String> = vec!["'x.y".into()];
        let mut diagnostics = Vec::new();
        let row = env.lower_row(&names, &BTreeMap::new(), &Span::default(), &mut diagnostics);
        assert_eq!(row, Row::empty());
        let codes: Vec<&str> = diagnostics.iter().map(|item| item.code).collect();
        assert_eq!(codes, vec!["E7405"]);
        for bad in ["a.b.c", ".x", "x.", "", "d-b.read"] {
            let mut diagnostics = Vec::new();
            env.lower_row(
                &[bad.to_string()],
                &BTreeMap::new(),
                &Span::default(),
                &mut diagnostics,
            );
            assert_eq!(diagnostics.len(), 1, "{bad}");
        }
    }

    const TREE: &str = "sealed class Arvore<T> {\n    data class No(val esq: Arvore<T>, val valor: T, val dir: Arvore<T>)\n    data object Folha\n}\ndata class Caixa<T>(val valor: T)\ndistinct type ClienteId = UUID\n";

    #[test]
    fn registers_constructors_of_user_and_builtin_types() {
        let (env, codes) = build(TREE);
        assert!(codes.is_empty(), "{codes:?}");
        assert_eq!(env.types["Arvore"].ctors, vec!["No", "Folha"]);
        assert_eq!(env.ctors["No"].fields.len(), 3);
        assert_eq!(env.types["Result"].ctors, vec!["Ok", "Err"]);
        assert_eq!(env.ctors["ClienteId"].fields[0].0, "value");
        assert_eq!(env.type_arity("Arvore"), Some(1));
        assert_eq!(env.type_arity("Map"), Some(2));
        assert_eq!(env.type_arity("Nada"), None);
    }

    #[test]
    fn instantiates_constructors_with_fresh_variables() {
        let (env, _) = build(TREE);
        let mut table = TypeTable::default();
        let (fields, result) = env.instantiate_ctor("No", &mut table, 1);
        let Type::Con(name, args) = &result else {
            unreachable!()
        };
        assert_eq!(name, "Arvore");
        assert_eq!(fields[1], args[0]);
        assert_eq!(fields[0], result);
    }

    #[test]
    fn substitutes_type_arguments_in_fields_and_signatures() {
        let (env, _) = build(TREE);
        let int = Type::con("Int");
        assert_eq!(
            env.field_type("Caixa", std::slice::from_ref(&int), "valor"),
            Some(int.clone())
        );
        assert_eq!(
            env.field_type("Caixa", std::slice::from_ref(&int), "outro"),
            None
        );
        assert_eq!(env.records_with_field("valor"), vec!["Caixa"]);
        let signatures = env
            .constructors(&Type::Con("Arvore".into(), vec![int.clone()]))
            .unwrap();
        assert_eq!(signatures[0].args[1], int);
        assert!(env.constructors(&Type::con("Int")).is_none());
        assert_eq!(env.constructors(&Type::con("Boolean")).unwrap().len(), 2);
    }

    #[test]
    fn displayable_types() {
        let (env, _) = build(TREE);
        assert!(env.is_displayable(&Type::con("Email")));
        assert!(env.is_displayable(&Type::con("ClienteId")));
        assert!(!env.is_displayable(&Type::list(Type::con("String"))));
        assert!(!env.is_displayable(&Type::con("Caixa")));
    }

    #[test]
    fn reports_declaration_errors() {
        assert_eq!(build("data class C(var x: Int)").1, vec!["E7002"]);
        assert_eq!(build("data class C(val x: Nada)").1, vec!["E7103"]);
        assert_eq!(
            build("data class C(val x: List<Int, Int>)").1,
            vec!["E7102"]
        );
        assert_eq!(
            build("data class C(val x: Int)\ndata class C(val y: Int)").1,
            vec!["E7110"]
        );
        assert_eq!(build("data class String(val x: Int)").1, vec!["E7110"]);
        assert_eq!(
            build("enum class A { X }\nenum class B { X }").1,
            vec!["E7110"]
        );
        assert_eq!(build("fun f() = 1\nfun f() = 2").1, vec!["E7110"]);
        assert_eq!(
            build("entity C<T>(val id: UUID, val x: T)").1,
            vec!["E7004"]
        );
        assert!(build("entity C(val id: UUID, val x: Int)").1.is_empty());
    }
}
