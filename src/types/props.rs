//! Property tests for the type system. Each property runs at least 1,000 cases.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::effects::{Row, RowVarId};
use super::exhaustive::{CtorSig, Pat, Signatures, check_match};
use super::ty::*;
use super::unify::unify;
use crate::diagnostic::{Span, has_errors};
use crate::lexer::{Token, TokenKind, lex};
use crate::parser::expr::parse_expression_tokens;
use crate::parser::parse;
use crate::schema::DatabaseSchema;
use crate::syntax::expr::*;

fn config() -> ProptestConfig {
    ProptestConfig::with_cases(1000)
}

// ---- 1. unification produces a unifier ---------------------------------------------

fn arb_type(vars: usize) -> impl Strategy<Value = Type> {
    let leaf = prop_oneof![
        (0..vars).prop_map(Type::Var),
        Just(Type::con("Int")),
        Just(Type::con("String"))
    ];
    leaf.prop_recursive(3, 16, 2, |inner| {
        prop_oneof![
            inner.clone().prop_map(Type::list),
            inner.clone().prop_map(Type::option),
            (proptest::collection::vec(inner.clone(), 0..3), inner)
                .prop_map(|(params, result)| Type::Fun(params, Box::new(result), Row::empty())),
        ]
    })
}

proptest! {
    #![proptest_config(config())]
    #[test]
    fn unification_produces_a_unifier(a in arb_type(4), b in arb_type(4)) {
        let mut table = TypeTable::default();
        for _ in 0..4 {
            table.fresh(1);
        }
        if unify(&mut table, &a, &b).is_ok() {
            prop_assert_eq!(table.resolve(&a), table.resolve(&b));
        }
    }
}

// ---- 2 and 3. principal types and stability ------------------------------------------

struct Entry {
    name: &'static str,
    params: &'static [&'static str],
    body: &'static str,
}

const CORPUS: &[Entry] = &[
    Entry {
        name: "ident",
        params: &["a"],
        body: "a",
    },
    Entry {
        name: "constante",
        params: &["a", "b"],
        body: "a",
    },
    Entry {
        name: "aplica",
        params: &["f", "x"],
        body: "f(x)",
    },
    Entry {
        name: "compose",
        params: &["f", "g", "x"],
        body: "f(g(x))",
    },
    Entry {
        name: "dobro",
        params: &["a"],
        body: "a * 2",
    },
    Entry {
        name: "ouPadrao",
        params: &["a", "b"],
        body: "a ?: b",
    },
    Entry {
        name: "cabeca",
        params: &["xs"],
        body: "first(xs)",
    },
    Entry {
        name: "par",
        params: &["a", "b"],
        body: "listOf(a, b)",
    },
    Entry {
        name: "troca",
        params: &["f", "a", "b"],
        body: "f(b, a)",
    },
    Entry {
        name: "igualA",
        params: &["a", "b"],
        body: "a == b + 1",
    },
];

fn source(entry: &Entry) -> String {
    format!(
        "fun {}({}) = {}\n",
        entry.name,
        entry.params.join(", "),
        entry.body
    )
}

/// The signature of `name`, or `None` if the source does not parse or type-check.
fn signature_of(source: &str, name: &str) -> Option<Type> {
    let (tokens, mut diagnostics) = lex("prop.bd", source);
    let (program, mut parse_diagnostics) = parse(tokens);
    diagnostics.append(&mut parse_diagnostics);
    if has_errors(&diagnostics) {
        return None;
    }
    let (typed, type_diagnostics) = super::check(&program, &DatabaseSchema::default());
    if has_errors(&type_diagnostics) {
        return None;
    }
    typed.signatures.get(name).cloned()
}

/// Names of type variables (`T0`, `T1`...) and row variables (`e0`, `e1`...) in source.
#[derive(Default)]
struct SourceNames {
    types: BTreeMap<TypeVarId, String>,
    rows: BTreeMap<RowVarId, String>,
}

/// Bind source for a type, naming variables `T0`, `T1`... and effect variables `e0`, `e1`...
fn to_source(ty: &Type, names: &mut SourceNames) -> String {
    match ty {
        Type::Var(id) => {
            let next = names.types.len();
            names
                .types
                .entry(*id)
                .or_insert_with(|| format!("T{next}"))
                .clone()
        }
        Type::Con(name, args) if name == "Option" => {
            let inner = to_source(&args[0], names);
            if matches!(args[0], Type::Fun(..)) {
                format!("({inner})?")
            } else {
                format!("{inner}?")
            }
        }
        Type::Con(name, args) if args.is_empty() => name.clone(),
        Type::Con(name, args) => {
            let rendered: Vec<String> = args.iter().map(|arg| to_source(arg, names)).collect();
            format!("{name}<{}>", rendered.join(", "))
        }
        Type::Fun(params, result, row) => {
            let rendered: Vec<String> =
                params.iter().map(|param| to_source(param, names)).collect();
            let inner = to_source(result, names);
            let result = match result.as_ref() {
                Type::Fun(_, _, inner_row) if !row.is_pure() || !inner_row.is_pure() => {
                    format!("({inner})")
                }
                _ => inner,
            };
            format!(
                "({}) -> {result}{}",
                rendered.join(", "),
                row_source(row, names)
            )
        }
        Type::Error => "Unit".into(),
    }
}

/// ` ! [db.read, e0]` with the annotation syntax; nothing for a pure row.
fn row_source(row: &Row, names: &mut SourceNames) -> String {
    if row.is_pure() {
        return String::new();
    }
    format!(" ! [{}]", row_items(row, names))
}

/// `db.read, e0`: the labels of a row, then its effect variable.
fn row_items(row: &Row, names: &mut SourceNames) -> String {
    let mut items: Vec<String> = row.labels.iter().map(ToString::to_string).collect();
    if let Some(tail) = row.tail {
        let next = names.rows.len();
        items.push(
            names
                .rows
                .entry(tail)
                .or_insert_with(|| format!("e{next}"))
                .clone(),
        );
    }
    items.join(", ")
}

/// The corpus entry annotated with the given function type, including its own effects
/// (`effects [...]`, so the body is checked against them exactly).
fn annotate(entry: &Entry, ty: &Type) -> String {
    let Type::Fun(params, result, row) = ty else {
        unreachable!("corpus entries are functions")
    };
    let mut names = SourceNames::default();
    let params: Vec<String> = entry
        .params
        .iter()
        .zip(params)
        .map(|(name, param)| format!("{name}: {}", to_source(param, &mut names)))
        .collect();
    let result = to_source(result, &mut names);
    let effects = row_items(row, &mut names);
    let generics = if names.types.is_empty() {
        String::new()
    } else {
        let mut ordered: Vec<&String> = names.types.values().collect();
        ordered.sort_by_key(|name| name[1..].parse::<usize>().unwrap_or(0));
        format!(
            "<{}> ",
            ordered
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "fun {generics}{}({}): {result} effects [{effects}] = {}\n",
        entry.name,
        params.join(", "),
        entry.body
    )
}

fn is_instance(general: &Type, specific: &Type) -> bool {
    #[derive(Default)]
    struct Bound {
        types: BTreeMap<TypeVarId, Type>,
        rows: BTreeMap<RowVarId, Row>,
    }

    fn matches(general: &Type, specific: &Type, bound: &mut Bound) -> bool {
        match (general, specific) {
            (Type::Var(id), _) => match bound.types.get(id) {
                Some(existing) => existing == specific,
                None => {
                    bound.types.insert(*id, specific.clone());
                    true
                }
            },
            (Type::Con(a, xs), Type::Con(b, ys)) => {
                a == b
                    && xs.len() == ys.len()
                    && xs.iter().zip(ys).all(|(x, y)| matches(x, y, bound))
            }
            (Type::Fun(p, r, e), Type::Fun(q, s, f)) => {
                p.len() == q.len()
                    && p.iter().zip(q).all(|(x, y)| matches(x, y, bound))
                    && matches(r, s, bound)
                    && rows_match(e, f, bound)
            }
            _ => false,
        }
    }

    /// Rows are sets: a general row `{L | v}` matches a specific row `{M | ...}` when
    /// `L ⊆ M`, binding `v` consistently to the rest (`M \ L` plus the specific tail); a
    /// closed general row only matches a closed row with the same labels.
    fn rows_match(general: &Row, specific: &Row, bound: &mut Bound) -> bool {
        let Some(tail) = general.tail else {
            return specific.tail.is_none() && general.labels == specific.labels;
        };
        if !general.labels.is_subset(&specific.labels) {
            return false;
        }
        let rest = Row {
            labels: specific
                .labels
                .difference(&general.labels)
                .cloned()
                .collect(),
            tail: specific.tail,
        };
        match bound.rows.get(&tail) {
            Some(existing) => *existing == rest,
            None => {
                bound.rows.insert(tail, rest);
                true
            }
        }
    }

    matches(general, specific, &mut Bound::default())
}

fn type_vars(ty: &Type, output: &mut Vec<TypeVarId>) {
    match ty {
        Type::Var(id) => {
            if !output.contains(id) {
                output.push(*id);
            }
        }
        Type::Con(_, args) => args.iter().for_each(|arg| type_vars(arg, output)),
        Type::Fun(params, result, _) => {
            params.iter().for_each(|param| type_vars(param, output));
            type_vars(result, output);
        }
        Type::Error => {}
    }
}

fn arb_ground() -> impl Strategy<Value = Type> {
    prop_oneof![
        Just(Type::con("Int")),
        Just(Type::con("String")),
        Just(Type::con("Boolean")),
        Just(Type::list(Type::con("Int"))),
        Just(Type::option(Type::con("Int"))),
        Just(Type::Fun(
            vec![Type::con("Int")],
            Box::new(Type::con("Int")),
            Row::empty()
        )),
    ]
}

#[test]
fn annotating_with_the_inferred_type_is_stable() {
    for entry in CORPUS {
        let inferred = signature_of(&source(entry), entry.name)
            .unwrap_or_else(|| panic!("{} must type-check", entry.name));
        let annotated = annotate(entry, &inferred);
        let again = signature_of(&annotated, entry.name)
            .unwrap_or_else(|| panic!("annotated version rejected:\n{annotated}"));
        assert_eq!(display(&inferred), display(&again), "{annotated}");
    }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn accepted_annotations_are_instances_of_the_inferred_type(
        index in 0..CORPUS.len(),
        picks in proptest::collection::vec(arb_ground(), 4),
    ) {
        let entry = &CORPUS[index];
        let inferred = signature_of(&source(entry), entry.name).unwrap();
        let arity = entry.params.len();
        let annotation = Type::Fun(picks[..arity].to_vec(), Box::new(picks[arity].clone()), Row::empty());
        if let Some(accepted) = signature_of(&annotate(entry, &annotation), entry.name) {
            prop_assert!(is_instance(&inferred, &accepted), "{} accepted `{}`, not an instance of `{}`", entry.name, display(&accepted), display(&inferred));
        }
    }

    #[test]
    fn instances_of_the_inferred_type_are_accepted(
        index in 0..CORPUS.len(),
        picks in proptest::collection::vec(arb_ground(), 4),
    ) {
        let entry = &CORPUS[index];
        let inferred = signature_of(&source(entry), entry.name).unwrap();
        let mut vars = Vec::new();
        type_vars(&inferred, &mut vars);
        let mapping: BTreeMap<TypeVarId, Type> = vars.into_iter().zip(picks).collect();
        let instance = substitute(&inferred, &mapping, &BTreeMap::new());
        let annotated = annotate(entry, &instance);
        prop_assert!(signature_of(&annotated, entry.name).is_some(), "rejected instance:\n{}", annotated);
    }
}

// ---- 4. exhaustiveness agrees with brute force ---------------------------------------

/// A random finite universe of algebraic types. Type `i` only refers to types `j > i`, and
/// the last type has two field-less constructors, so every type has finitely many values.
struct Universe {
    types: Vec<Vec<(String, Vec<usize>)>>,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

impl Universe {
    fn random(rng: &mut Rng) -> Universe {
        // Three types keep brute-force enumeration small (at most 3 * 12 * 12 values of `T0`).
        let count = 3;
        let mut types = Vec::new();
        for index in 0..count {
            if index == count - 1 {
                types.push(vec![
                    ("Sim".to_string(), vec![]),
                    ("Nao".to_string(), vec![]),
                ]);
                continue;
            }
            let constructors = 1 + rng.below(3);
            let ctors = (0..constructors)
                .map(|ctor| {
                    let arity = rng.below(3);
                    let args = (0..arity)
                        .map(|_| index + 1 + rng.below(count - index - 1))
                        .collect();
                    (format!("C{index}_{ctor}"), args)
                })
                .collect();
            types.push(ctors);
        }
        Universe { types }
    }

    fn pattern(&self, rng: &mut Rng, ty: usize, depth: usize) -> Pat {
        if depth == 0 || rng.below(3) == 0 {
            return Pat::Wild;
        }
        let ctors = &self.types[ty];
        let (name, args) = &ctors[rng.below(ctors.len())];
        Pat::Ctor(
            name.clone(),
            args.iter()
                .map(|arg| self.pattern(rng, *arg, depth - 1))
                .collect(),
        )
    }

    fn values(&self, ty: usize) -> Vec<Pat> {
        let mut output = Vec::new();
        for (name, args) in &self.types[ty] {
            let mut partial: Vec<Vec<Pat>> = vec![Vec::new()];
            for arg in args {
                let choices = self.values(*arg);
                partial = partial
                    .into_iter()
                    .flat_map(|prefix| {
                        choices.iter().map(move |choice| {
                            let mut next = prefix.clone();
                            next.push(choice.clone());
                            next
                        })
                    })
                    .collect();
            }
            output.extend(
                partial
                    .into_iter()
                    .map(|args| Pat::Ctor(name.clone(), args)),
            );
        }
        output
    }
}

impl Signatures for Universe {
    fn constructors(&self, ty: &Type) -> Option<Vec<CtorSig>> {
        let Type::Con(name, _) = ty else { return None };
        let index: usize = name.strip_prefix('T')?.parse().ok()?;
        Some(
            self.types[index]
                .iter()
                .map(|(ctor, args)| CtorSig {
                    name: ctor.clone(),
                    args: args
                        .iter()
                        .map(|arg| Type::con(&format!("T{arg}")))
                        .collect(),
                })
                .collect(),
        )
    }
}

fn matches_value(pattern: &Pat, value: &Pat) -> bool {
    match (pattern, value) {
        (Pat::Wild, _) => true,
        (Pat::Ctor(a, xs), Pat::Ctor(b, ys)) => {
            a == b && xs.iter().zip(ys).all(|(x, y)| matches_value(x, y))
        }
        _ => false,
    }
}

proptest! {
    #![proptest_config(config())]
    #[test]
    fn exhaustiveness_agrees_with_brute_force(seed in any::<u64>()) {
        let mut rng = Rng(seed | 1);
        let universe = Universe::random(&mut rng);
        let row_count = 1 + rng.below(5);
        let rows: Vec<Pat> = (0..row_count).map(|_| universe.pattern(&mut rng, 0, 3)).collect();
        let report = check_match(&universe, &Type::con("T0"), &rows);
        let values = universe.values(0);
        let exhaustive = values.iter().all(|value| rows.iter().any(|row| matches_value(row, value)));
        prop_assert_eq!(report.missing.is_none(), exhaustive);
        if let Some(witness) = &report.missing {
            let real = values.iter().any(|value| matches_value(witness, value) && !rows.iter().any(|row| matches_value(row, value)));
            prop_assert!(real, "witness {:?} is matched by an arm", witness);
        }
        for index in 0..rows.len() {
            let unreachable = !values
                .iter()
                .any(|value| matches_value(&rows[index], value) && !rows[..index].iter().any(|row| matches_value(row, value)));
            prop_assert_eq!(report.redundant.contains(&index), unreachable, "arm {}", index);
        }
    }
}

// ---- 5. parser round trip ------------------------------------------------------------

fn node(kind: ExprKind) -> Expr {
    Expr {
        id: 0,
        kind,
        span: Span::default(),
    }
}

fn arb_name() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "xs", "valor", "f"]).prop_map(str::to_string)
}

fn arb_pattern() -> impl Strategy<Value = Pattern> {
    let leaf = prop_oneof![
        Just(Pattern::Wildcard),
        arb_name().prop_map(|name| Pattern::Name(name, Span::default())),
        (0u32..100).prop_map(|value| Pattern::Int(value.to_string(), Span::default())),
        "[a-z ]{0,5}".prop_map(|text| Pattern::Str(text, Span::default())),
        any::<bool>().prop_map(|value| Pattern::Bool(value, Span::default())),
        Just(Pattern::Null(Span::default())),
    ];
    leaf.prop_recursive(2, 8, 2, |inner| {
        (
            prop::sample::select(vec!["Pago", "No"]),
            proptest::collection::vec(inner, 0..3),
        )
            .prop_map(|(name, args)| Pattern::Constructor {
                name: name.to_string(),
                args,
                span: Span::default(),
            })
    })
}

fn arb_binary_op() -> impl Strategy<Value = BinaryOp> {
    prop::sample::select(vec![
        BinaryOp::Add,
        BinaryOp::Sub,
        BinaryOp::Mul,
        BinaryOp::Div,
        BinaryOp::Rem,
        BinaryOp::Eq,
        BinaryOp::NotEq,
        BinaryOp::Lt,
        BinaryOp::LtEq,
        BinaryOp::Gt,
        BinaryOp::GtEq,
        BinaryOp::And,
        BinaryOp::Or,
    ])
}

fn arb_expr() -> impl Strategy<Value = Expr> {
    let leaf = prop_oneof![
        (0u32..1000).prop_map(|value| node(ExprKind::Int(value.to_string()))),
        "[a-z ]{0,6}".prop_map(|text| node(ExprKind::Str(text))),
        any::<bool>().prop_map(|value| node(ExprKind::Bool(value))),
        Just(node(ExprKind::Null)),
        arb_name().prop_map(|name| node(ExprKind::Name(name))),
    ];
    leaf.prop_recursive(4, 32, 3, |inner| {
        let boxed = |expr: Expr| Box::new(expr);
        prop_oneof![
            (inner.clone(), inner.clone(), arb_binary_op()).prop_map(
                move |(left, right, op)| node(ExprKind::Binary {
                    op,
                    left: boxed(left),
                    right: boxed(right)
                })
            ),
            (inner.clone(), any::<bool>()).prop_map(move |(operand, negative)| node(
                ExprKind::Unary {
                    op: if negative { UnaryOp::Neg } else { UnaryOp::Not },
                    operand: boxed(operand),
                }
            )),
            (inner.clone(), inner.clone()).prop_map(move |(value, fallback)| node(
                ExprKind::Elvis {
                    value: boxed(value),
                    fallback: boxed(fallback)
                }
            )),
            (inner.clone(), arb_name()).prop_map(move |(target, name)| node(ExprKind::Field {
                target: boxed(target),
                name
            })),
            (inner.clone(), arb_name()).prop_map(move |(target, name)| node(ExprKind::SafeField {
                target: boxed(target),
                name
            })),
            (arb_name(), proptest::collection::vec(inner.clone(), 0..3)).prop_map(
                move |(name, args)| node(ExprKind::Call {
                    callee: boxed(node(ExprKind::Name(name))),
                    args,
                    children: None,
                })
            ),
            (inner.clone(), inner.clone(), inner.clone()).prop_map(
                move |(condition, then_branch, else_branch)| node(ExprKind::If {
                    condition: boxed(condition),
                    then_branch: boxed(then_branch),
                    else_branch: boxed(else_branch),
                })
            ),
            (proptest::collection::vec(arb_name(), 0..3), inner.clone()).prop_map(
                |(names, body)| node(ExprKind::Lambda {
                    params: names
                        .into_iter()
                        .map(|name| (name, Span::default()))
                        .collect(),
                    body: Block {
                        id: 0,
                        stmts: vec![Stmt::Expr(body)],
                        span: Span::default()
                    },
                })
            ),
            (
                inner.clone(),
                proptest::collection::vec((arb_pattern(), inner), 1..3)
            )
                .prop_map(move |(scrutinee, arms)| {
                    node(ExprKind::When {
                        scrutinee: boxed(scrutinee),
                        arms: arms
                            .into_iter()
                            .map(|(pattern, body)| Arm {
                                pattern,
                                body,
                                span: Span::default(),
                            })
                            .collect(),
                    })
                }),
        ]
    })
}

fn reparse(source: &str) -> Result<String, String> {
    let (tokens, lex_diagnostics) = lex("prop.bd", source);
    if !lex_diagnostics.is_empty() {
        return Err(format!("{lex_diagnostics:?}"));
    }
    let tokens: Vec<Token> = tokens
        .into_iter()
        .filter(|token| token.kind != TokenKind::Eof)
        .collect();
    let (block, diagnostics) = parse_expression_tokens(&tokens, &Span::default());
    if !diagnostics.is_empty() {
        return Err(format!("{diagnostics:?}"));
    }
    Ok(render_block(&block))
}

proptest! {
    #![proptest_config(config())]
    #[test]
    fn parser_round_trips_rendered_expressions(expr in arb_expr()) {
        let rendered = render_expr(&expr);
        let expected = format!("{{ {rendered} }}");
        prop_assert_eq!(reparse(&rendered), Ok(expected), "source: {}", rendered);
    }
}
