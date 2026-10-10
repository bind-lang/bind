//! Type representation, union-find variables with levels, schemes and rendering.

use std::collections::{BTreeMap, BTreeSet};

pub type TypeVarId = usize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type {
    Var(TypeVarId),
    Con(String, Vec<Type>),
    Fun(Vec<Type>, Box<Type>),
    /// Produced after a reported error; unifies with everything to avoid cascades.
    Error,
}

impl Type {
    pub fn con(name: &str) -> Type {
        Type::Con(name.to_string(), Vec::new())
    }

    pub fn option(inner: Type) -> Type {
        Type::Con("Option".into(), vec![inner])
    }

    pub fn list(inner: Type) -> Type {
        Type::Con("List".into(), vec![inner])
    }

    pub fn contains_error(&self) -> bool {
        match self {
            Type::Error => true,
            Type::Var(_) => false,
            Type::Con(_, args) => args.iter().any(Type::contains_error),
            Type::Fun(params, result) => params.iter().any(Type::contains_error) || result.contains_error(),
        }
    }
}

const NUMERIC: &[&str] = &["Int", "Double", "Float", "Decimal"];

/// Operator constraints over closed sets of types, declared from weakest to strongest:
/// every type a stronger constraint accepts is accepted by the weaker ones, so two
/// constraints on the same variable combine by taking the stronger (`max`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Constraint {
    Eq,
    Ord,
    Add,
    Sub,
    Num,
}

impl Constraint {
    pub fn meet(self, other: Constraint) -> Constraint {
        self.max(other)
    }

    /// Whether a type constructor (ignoring its arguments) satisfies the constraint.
    pub fn accepts(self, name: &str) -> bool {
        match self {
            Constraint::Num => NUMERIC.contains(&name),
            Constraint::Sub => Constraint::Num.accepts(name) || matches!(name, "Money" | "Duration"),
            Constraint::Add => Constraint::Sub.accepts(name) || name == "String",
            Constraint::Ord => {
                Constraint::Add.accepts(name) || matches!(name, "Date" | "DateTime" | "Instant")
            }
            Constraint::Eq => !name.starts_with('\''),
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Constraint::Num => "`*` and `/` (accepted types: Int, Double, Float, Decimal)",
            Constraint::Sub => "`-` (accepted types: Int, Double, Float, Decimal, Money, Duration)",
            Constraint::Add => "`+` (accepted types: Int, Double, Float, Decimal, Money, Duration, String)",
            Constraint::Ord => {
                "ordering comparisons (accepted types: Int, Double, Float, Decimal, Money, Duration, String, Date, DateTime, Instant)"
            }
            Constraint::Eq => "equality (functions and type parameters cannot be compared)",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VarState {
    Unbound { level: u32, constraint: Option<Constraint> },
    Link(Type),
}

#[derive(Clone, Debug, Default)]
pub struct TypeTable {
    vars: Vec<VarState>,
    /// Type constructors that never support equality: `Ui` and user types whose fields
    /// contain a function or another such type (see `Env::build`).
    non_equatable: BTreeSet<String>,
}

impl TypeTable {
    pub fn set_non_equatable(&mut self, names: BTreeSet<String>) {
        self.non_equatable = names;
    }

    pub fn is_non_equatable(&self, name: &str) -> bool {
        self.non_equatable.contains(name)
    }

    pub fn fresh(&mut self, level: u32) -> Type {
        self.vars.push(VarState::Unbound { level, constraint: None });
        Type::Var(self.vars.len() - 1)
    }

    pub fn state(&self, id: TypeVarId) -> &VarState {
        &self.vars[id]
    }

    pub fn set(&mut self, id: TypeVarId, state: VarState) {
        self.vars[id] = state;
    }

    /// Follows links at the head of `ty` only.
    pub fn shallow(&self, ty: &Type) -> Type {
        let mut current = ty.clone();
        while let Type::Var(id) = current
            && let VarState::Link(next) = &self.vars[id]
        {
            current = next.clone();
        }
        current
    }

    /// Follows every link.
    pub fn resolve(&self, ty: &Type) -> Type {
        match self.shallow(ty) {
            Type::Con(name, args) => Type::Con(name, args.iter().map(|arg| self.resolve(arg)).collect()),
            Type::Fun(params, result) => Type::Fun(
                params.iter().map(|param| self.resolve(param)).collect(),
                Box::new(self.resolve(&result)),
            ),
            other => other,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Scheme {
    pub vars: Vec<TypeVarId>,
    pub ty: Type,
}

impl Scheme {
    pub fn mono(ty: Type) -> Scheme {
        Scheme { vars: Vec::new(), ty }
    }
}

/// Unbound variables of `ty`, in order of first appearance.
pub fn free_vars(table: &TypeTable, ty: &Type) -> Vec<TypeVarId> {
    let mut output = Vec::new();
    collect_vars(&table.resolve(ty), &mut output);
    output
}

fn collect_vars(ty: &Type, output: &mut Vec<TypeVarId>) {
    match ty {
        Type::Var(id) => {
            if !output.contains(id) {
                output.push(*id);
            }
        }
        Type::Con(_, args) => args.iter().for_each(|arg| collect_vars(arg, output)),
        Type::Fun(params, result) => {
            params.iter().for_each(|param| collect_vars(param, output));
            collect_vars(result, output);
        }
        Type::Error => {}
    }
}

/// Quantifies the variables created deeper than `level`. Constrained variables are never
/// quantified: they are returned as ambiguous so the caller reports E7302.
pub fn generalize(table: &TypeTable, level: u32, ty: &Type) -> (Scheme, Vec<(TypeVarId, Constraint)>) {
    let resolved = table.resolve(ty);
    let mut vars = Vec::new();
    let mut ambiguous = Vec::new();
    for id in free_vars(table, &resolved) {
        if let VarState::Unbound { level: var_level, constraint } = table.state(id)
            && *var_level > level
        {
            match constraint {
                Some(constraint) => ambiguous.push((id, *constraint)),
                None => vars.push(id),
            }
        }
    }
    (Scheme { vars, ty: resolved }, ambiguous)
}

pub fn instantiate(table: &mut TypeTable, scheme: &Scheme, level: u32) -> Type {
    if scheme.vars.is_empty() {
        return scheme.ty.clone();
    }
    let mapping: BTreeMap<TypeVarId, Type> =
        scheme.vars.iter().map(|id| (*id, table.fresh(level))).collect();
    substitute(&scheme.ty, &mapping)
}

pub fn substitute(ty: &Type, mapping: &BTreeMap<TypeVarId, Type>) -> Type {
    match ty {
        Type::Var(id) => mapping.get(id).cloned().unwrap_or(Type::Var(*id)),
        Type::Con(name, args) => Type::Con(name.clone(), args.iter().map(|arg| substitute(arg, mapping)).collect()),
        Type::Fun(params, result) => Type::Fun(
            params.iter().map(|param| substitute(param, mapping)).collect(),
            Box::new(substitute(result, mapping)),
        ),
        Type::Error => Type::Error,
    }
}

/// Renders a resolved type; variables become `'a`, `'b`... in order of appearance.
pub fn display(ty: &Type) -> String {
    display_all(&[ty]).remove(0)
}

/// Renders several types with shared variable names (for "expected X, found Y").
pub fn display_all(types: &[&Type]) -> Vec<String> {
    let mut names = BTreeMap::new();
    types.iter().map(|ty| render(ty, &mut names)).collect()
}

fn render(ty: &Type, names: &mut BTreeMap<TypeVarId, String>) -> String {
    match ty {
        Type::Var(id) => {
            let next = names.len();
            names.entry(*id).or_insert_with(|| var_name(next)).clone()
        }
        Type::Con(name, args) if name == "Option" && args.len() == 1 => {
            let inner = render(&args[0], names);
            if matches!(args[0], Type::Fun(..)) { format!("({inner})?") } else { format!("{inner}?") }
        }
        Type::Con(name, args) if args.is_empty() => name.clone(),
        Type::Con(name, args) => {
            let rendered: Vec<String> = args.iter().map(|arg| render(arg, names)).collect();
            format!("{name}<{}>", rendered.join(", "))
        }
        Type::Fun(params, result) => {
            let rendered: Vec<String> = params.iter().map(|param| render(param, names)).collect();
            let result = render(result, names);
            format!("({}) -> {result}", rendered.join(", "))
        }
        Type::Error => "<error>".into(),
    }
}

fn var_name(index: usize) -> String {
    let letter = char::from(b'a' + (index % 26) as u8);
    if index < 26 { format!("'{letter}") } else { format!("'{letter}{}", index / 26) }
}
