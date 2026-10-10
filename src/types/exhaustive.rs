//! Exhaustiveness and redundancy of `when`, using Maranget's usefulness algorithm
//! ("Warnings for pattern matching", Journal of Functional Programming, 2007).

use super::ty::Type;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pat {
    Wild,
    Ctor(String, Vec<Pat>),
    Lit(String),
}

#[derive(Clone, Debug)]
pub struct CtorSig {
    pub name: String,
    pub args: Vec<Type>,
}

impl CtorSig {
    pub fn nullary(name: &str) -> CtorSig {
        CtorSig { name: name.into(), args: Vec::new() }
    }
}

pub trait Signatures {
    /// Every constructor of `ty`, or `None` when the type has infinitely many values.
    fn constructors(&self, ty: &Type) -> Option<Vec<CtorSig>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchReport {
    /// A value no arm matches, if any.
    pub missing: Option<Pat>,
    /// Indices of arms that can never be reached.
    pub redundant: Vec<usize>,
}

pub fn check_match(sig: &dyn Signatures, ty: &Type, arms: &[Pat]) -> MatchReport {
    let types = std::slice::from_ref(ty);
    let mut rows: Vec<Vec<Pat>> = Vec::new();
    let mut redundant = Vec::new();
    for (index, arm) in arms.iter().enumerate() {
        if useful(sig, &rows, std::slice::from_ref(arm), types).is_none() {
            redundant.push(index);
        }
        rows.push(vec![arm.clone()]);
    }
    let missing = useful(sig, &rows, &[Pat::Wild], types).map(|mut witness| witness.remove(0));
    MatchReport { missing, redundant }
}

pub fn render(pat: &Pat) -> String {
    match pat {
        Pat::Wild => "_".into(),
        Pat::Lit(text) => text.clone(),
        Pat::Ctor(name, args) if name == "Some" => match args.first() {
            Some(Pat::Wild) | None => "<non-null value>".into(),
            Some(inner @ Pat::Ctor(inner_name, _)) if inner_name == "null" || inner_name == "Some" => {
                format!("Some({})", render(inner))
            }
            Some(inner) => render(inner),
        },
        Pat::Ctor(name, args) if args.is_empty() => name.clone(),
        Pat::Ctor(name, args) => {
            let rendered: Vec<String> = args.iter().map(render).collect();
            format!("{name}({})", rendered.join(", "))
        }
    }
}

/// Returns a witness vector matched by `vector` and by no row of `rows`, or `None`.
fn useful(sig: &dyn Signatures, rows: &[Vec<Pat>], vector: &[Pat], types: &[Type]) -> Option<Vec<Pat>> {
    let Some((head, rest)) = vector.split_first() else {
        return rows.is_empty().then(Vec::new);
    };
    let head_type = &types[0];
    let rest_types = &types[1..];
    match head {
        Pat::Ctor(name, args) => {
            let arg_types = constructor_args(sig, head_type, name, args.len());
            let specialized = specialize(rows, name, args.len());
            let vector: Vec<Pat> = args.iter().chain(rest).cloned().collect();
            let types: Vec<Type> = arg_types.into_iter().chain(rest_types.iter().cloned()).collect();
            useful(sig, &specialized, &vector, &types).map(|witness| rebuild(name, args.len(), witness))
        }
        Pat::Lit(literal) => {
            let specialized = specialize_literal(rows, literal);
            useful(sig, &specialized, rest, rest_types).map(|mut witness| {
                witness.insert(0, Pat::Lit(literal.clone()));
                witness
            })
        }
        Pat::Wild => {
            let used = head_constructors(rows);
            let all = sig.constructors(head_type);
            if let Some(all) = &all
                && !all.is_empty()
                && all.iter().all(|ctor| used.contains(&ctor.name))
            {
                for ctor in all {
                    let specialized = specialize(rows, &ctor.name, ctor.args.len());
                    let vector: Vec<Pat> = std::iter::repeat_n(Pat::Wild, ctor.args.len()).chain(rest.iter().cloned()).collect();
                    let types: Vec<Type> = ctor.args.iter().chain(rest_types).cloned().collect();
                    if let Some(witness) = useful(sig, &specialized, &vector, &types) {
                        return Some(rebuild(&ctor.name, ctor.args.len(), witness));
                    }
                }
                return None;
            }
            let defaults = default_rows(rows);
            useful(sig, &defaults, rest, rest_types).map(|mut witness| {
                let head = all
                    .as_ref()
                    .and_then(|all| all.iter().find(|ctor| !used.contains(&ctor.name)))
                    .map_or(Pat::Wild, |ctor| Pat::Ctor(ctor.name.clone(), vec![Pat::Wild; ctor.args.len()]));
                witness.insert(0, head);
                witness
            })
        }
    }
}

fn constructor_args(sig: &dyn Signatures, ty: &Type, name: &str, arity: usize) -> Vec<Type> {
    sig.constructors(ty)
        .and_then(|all| all.into_iter().find(|ctor| ctor.name == name))
        .filter(|ctor| ctor.args.len() == arity)
        .map_or_else(|| vec![Type::Error; arity], |ctor| ctor.args)
}

fn head_constructors(rows: &[Vec<Pat>]) -> Vec<String> {
    let mut names = Vec::new();
    for row in rows {
        if let Some(Pat::Ctor(name, _)) = row.first()
            && !names.contains(name)
        {
            names.push(name.clone());
        }
    }
    names
}

fn specialize(rows: &[Vec<Pat>], name: &str, arity: usize) -> Vec<Vec<Pat>> {
    rows.iter()
        .filter_map(|row| match row.split_first()? {
            (Pat::Ctor(other, args), rest) if other == name && args.len() == arity => Some(args.iter().chain(rest).cloned().collect()),
            (Pat::Wild, rest) => Some(std::iter::repeat_n(Pat::Wild, arity).chain(rest.iter().cloned()).collect()),
            _ => None,
        })
        .collect()
}

fn specialize_literal(rows: &[Vec<Pat>], literal: &str) -> Vec<Vec<Pat>> {
    rows.iter()
        .filter_map(|row| match row.split_first()? {
            (Pat::Lit(other), rest) if other == literal => Some(rest.to_vec()),
            (Pat::Wild, rest) => Some(rest.to_vec()),
            _ => None,
        })
        .collect()
}

fn default_rows(rows: &[Vec<Pat>]) -> Vec<Vec<Pat>> {
    rows.iter()
        .filter_map(|row| match row.split_first()? {
            (Pat::Wild, rest) => Some(rest.to_vec()),
            _ => None,
        })
        .collect()
}

fn rebuild(name: &str, arity: usize, mut witness: Vec<Pat>) -> Vec<Pat> {
    let rest = witness.split_off(arity.min(witness.len()));
    let mut output = vec![Pat::Ctor(name.into(), witness)];
    output.extend(rest);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;

    impl Signatures for Fake {
        fn constructors(&self, ty: &Type) -> Option<Vec<CtorSig>> {
            let Type::Con(name, args) = ty else { return None };
            let boolean = || Type::con("Boolean");
            match name.as_str() {
                "Boolean" => Some(vec![CtorSig::nullary("true"), CtorSig::nullary("false")]),
                "Option" => Some(vec![
                    CtorSig::nullary("null"),
                    CtorSig { name: "Some".into(), args: args.clone() },
                ]),
                "Pedido" => Some(vec![
                    CtorSig { name: "Pendente".into(), args: vec![Type::con("Int")] },
                    CtorSig { name: "Pago".into(), args: vec![boolean()] },
                    CtorSig::nullary("Cancelado"),
                ]),
                "Par" => Some(vec![CtorSig { name: "Par".into(), args: vec![boolean(), boolean()] }]),
                _ => None,
            }
        }
    }

    fn ctor(name: &str, args: Vec<Pat>) -> Pat {
        Pat::Ctor(name.into(), args)
    }

    fn pedido() -> Type {
        Type::con("Pedido")
    }

    #[test]
    fn complete_match_has_no_missing_case() {
        let arms = [ctor("Pendente", vec![Pat::Wild]), ctor("Pago", vec![Pat::Wild]), ctor("Cancelado", vec![])];
        let report = check_match(&Fake, &pedido(), &arms);
        assert_eq!(report.missing, None);
        assert!(report.redundant.is_empty());
    }

    #[test]
    fn reports_a_missing_constructor() {
        let arms = [ctor("Pendente", vec![Pat::Wild]), ctor("Cancelado", vec![])];
        let report = check_match(&Fake, &pedido(), &arms);
        assert_eq!(report.missing.as_ref().map(render), Some("Pago(_)".into()));
    }

    #[test]
    fn reports_missing_nested_values() {
        let arms = [
            ctor("Pendente", vec![Pat::Wild]),
            ctor("Pago", vec![ctor("true", vec![])]),
            ctor("Cancelado", vec![]),
        ];
        let report = check_match(&Fake, &pedido(), &arms);
        assert_eq!(report.missing.as_ref().map(render), Some("Pago(false)".into()));
    }

    #[test]
    fn finds_the_classic_pair_witness() {
        let t = || ctor("true", vec![]);
        let arms = [ctor("Par", vec![t(), Pat::Wild]), ctor("Par", vec![Pat::Wild, t()])];
        let report = check_match(&Fake, &Type::con("Par"), &arms);
        assert_eq!(report.missing.as_ref().map(render), Some("Par(false, false)".into()));
    }

    #[test]
    fn reports_redundant_arms() {
        let arms = [Pat::Wild, ctor("Cancelado", vec![])];
        assert_eq!(check_match(&Fake, &pedido(), &arms).redundant, vec![1]);
        let arms = [ctor("Cancelado", vec![]), ctor("Cancelado", vec![]), Pat::Wild];
        assert_eq!(check_match(&Fake, &pedido(), &arms).redundant, vec![1]);
    }

    #[test]
    fn options_need_null_and_a_value() {
        let option = Type::option(Type::con("Int"));
        let report = check_match(&Fake, &option, &[ctor("Some", vec![Pat::Wild])]);
        assert_eq!(report.missing.as_ref().map(render), Some("null".into()));
        let report = check_match(&Fake, &option, &[ctor("null", vec![])]);
        assert_eq!(report.missing.as_ref().map(render), Some("<non-null value>".into()));
        let report = check_match(&Fake, &option, &[ctor("null", vec![]), ctor("Some", vec![Pat::Wild])]);
        assert_eq!(report.missing, None);
    }

    #[test]
    fn literals_of_infinite_types_need_a_wildcard() {
        let int = Type::con("Int");
        let arms = [Pat::Lit("0".into()), Pat::Lit("1".into())];
        let report = check_match(&Fake, &int, &arms);
        assert_eq!(report.missing.as_ref().map(render), Some("_".into()));
        let arms = [Pat::Lit("0".into()), Pat::Lit("0".into()), Pat::Wild];
        let report = check_match(&Fake, &int, &arms);
        assert_eq!(report.missing, None);
        assert_eq!(report.redundant, vec![1]);
    }

    #[test]
    fn wrong_arity_constructor_does_not_panic() {
        let arms = [ctor("Pago", vec![Pat::Wild, Pat::Wild])];
        let report = check_match(&Fake, &pedido(), &arms);
        assert!(report.missing.is_some());
    }

    #[test]
    fn nested_options_render_the_inner_null() {
        let option = Type::option(Type::option(Type::con("Int")));
        let arms = [ctor("null", vec![]), ctor("Some", vec![ctor("Some", vec![Pat::Wild])])];
        let report = check_match(&Fake, &option, &arms);
        assert_eq!(report.missing.as_ref().map(render), Some("Some(null)".into()));
    }
}
