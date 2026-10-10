//! Property tests for the type system. Each property runs at least 1,000 cases.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::effects::{Label, Row, RowVarId};
use super::exhaustive::{CtorSig, Pat, Signatures, check_match};
use super::ty::*;
use super::unify::{unify, unify_rows};
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

// ---- 6. effect rows -------------------------------------------------------------------

fn arb_row(vars: usize) -> impl Strategy<Value = Row> {
    let labels = proptest::collection::btree_set(
        prop::sample::select(vec![("db", "read"), ("db", "write"), ("server", "call")])
            .prop_map(Label::of),
        0..3,
    );
    (labels, proptest::option::of(0..vars)).prop_map(|(labels, tail)| Row { labels, tail })
}

proptest! {
    #![proptest_config(config())]
    #[test]
    fn row_unification_produces_a_unifier(a in arb_row(3), b in arb_row(3)) {
        let mut table = TypeTable::default();
        for _ in 0..3 {
            table.fresh_row_var(1);
        }
        if unify_rows(&mut table, &a, &b).is_ok() {
            prop_assert_eq!(table.resolve_row(&a), table.resolve_row(&b));
        }
    }
}

// ---- 7. inferred effects equal call-graph reachability --------------------------------

/// The argument a call passes to a higher-order function's `g` parameter.
#[derive(Clone, Debug)]
enum Arg {
    /// `{ y -> y }`
    Identity,
    /// `{ y -> db.clientes.require(y).id }`
    Reads,
    /// `fK` as a value (a first-order function; a higher-order one is passed as `Identity`).
    Function(usize),
}

#[derive(Clone, Debug)]
enum Action {
    Read,
    Write,
    Call(usize, Arg),
    CallViaLambda(usize, Arg),
    /// `g(id)`, in a higher-order function.
    CallParameter,
}

#[derive(Clone, Debug)]
struct EffectFunction {
    actions: Vec<Action>,
    /// Declares `: ClienteId` (otherwise the result is inferred).
    returns: bool,
    /// Takes `g: (ClienteId) -> ClienteId ! [e]`.
    higher_order: bool,
    /// Declares its (expected) effects in the random-clause programs.
    declared: bool,
}

impl EffectFunction {
    fn calls_parameter(&self) -> bool {
        self.higher_order
            && self
                .actions
                .iter()
                .any(|action| matches!(action, Action::CallParameter))
    }
}

fn arb_effect_program() -> impl Strategy<Value = Vec<EffectFunction>> {
    (1usize..5)
        .prop_flat_map(|count| {
            let arg = prop_oneof![
                Just(Arg::Identity),
                Just(Arg::Reads),
                (0..count).prop_map(Arg::Function),
            ];
            let action = prop_oneof![
                Just(Action::Read),
                Just(Action::Write),
                (0..count, arg.clone()).prop_map(|(other, arg)| Action::Call(other, arg)),
                (0..count, arg).prop_map(|(other, arg)| Action::CallViaLambda(other, arg)),
                Just(Action::CallParameter),
            ];
            let function = (
                proptest::collection::vec(action, 0..4),
                any::<bool>(),
                any::<bool>(),
                any::<bool>(),
            )
                .prop_map(|(actions, returns, higher_order, declared)| {
                    EffectFunction {
                        actions,
                        returns,
                        higher_order,
                        declared,
                    }
                });
            proptest::collection::vec(function, count)
        })
        .prop_map(normalize)
}

/// Keeps generated programs inside what monomorphic recursion can type precisely: only
/// higher-order functions call their parameter, and a call to a higher-order function never
/// closes a cycle (inside a recursive group, its parameter row would be shared by every
/// call). Offending actions become reads.
fn normalize(mut program: Vec<EffectFunction>) -> Vec<EffectFunction> {
    let count = program.len();
    let higher: Vec<bool> = program
        .iter()
        .map(|function| function.higher_order)
        .collect();
    for function in &mut program {
        if !function.higher_order {
            for action in &mut function.actions {
                if matches!(action, Action::CallParameter) {
                    *action = Action::Read;
                }
            }
        }
    }
    let mut reach = vec![vec![false; count]; count];
    for (index, function) in program.iter().enumerate() {
        for action in &function.actions {
            if let Action::Call(other, arg) | Action::CallViaLambda(other, arg) = action {
                reach[index][*other] = true;
                if let Arg::Function(value) = arg
                    && !higher[*value]
                {
                    reach[index][*value] = true;
                }
            }
        }
    }
    for middle in 0..count {
        for from in 0..count {
            for to in 0..count {
                if reach[from][middle] && reach[middle][to] {
                    reach[from][to] = true;
                }
            }
        }
    }
    for (index, function) in program.iter_mut().enumerate() {
        for action in &mut function.actions {
            if let Action::Call(other, _) | Action::CallViaLambda(other, _) = action
                && higher[*other]
                && (*other == index || reach[*other][index])
            {
                *action = Action::Read;
            }
            // A function of the caller's own recursive group passed as `g` would join its own
            // row with a row that extends it (`'r ~ {db.read | 'r}`), which set-based rows
            // reject (a known limitation), so it is passed as a lambda instead.
            if let Action::Call(_, arg) | Action::CallViaLambda(_, arg) = action
                && let Arg::Function(value) = arg
                && (*value == index || reach[*value][index])
            {
                *arg = Arg::Identity;
            }
        }
    }
    program
}

const EFFECT_PRELUDE: &str = "distinct type ClienteId = UUID\nentity Cliente(val id: ClienteId, val nome: String)\nfun aplica(f, x) = f(x)\n";
const EFFECT_SCHEMA: &str = "table,model,primary_key,column,type,nullable,required,has_default,engine\nclientes,Cliente,id,id,ClienteId,false,true,true,postgresql\nclientes,Cliente,id,nome,String,false,true,false,postgresql\n";

fn effect_schema() -> &'static DatabaseSchema {
    static SCHEMA: std::sync::OnceLock<DatabaseSchema> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("bind-effect-props-{}.csv", std::process::id()));
        std::fs::write(&path, EFFECT_SCHEMA).unwrap();
        let schema = DatabaseSchema::load(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        schema
    })
}

/// Which functions declare `effects [...]` (with the expected effects).
#[derive(Clone, Copy, PartialEq)]
enum Clauses {
    None,
    Random,
    All,
}

fn render_effect_program(
    program: &[EffectFunction],
    clauses: Clauses,
    expected: &[BTreeSet<String>],
) -> String {
    let mut source = EFFECT_PRELUDE.to_string();
    for (index, function) in program.iter().enumerate() {
        let declares = match clauses {
            Clauses::None => false,
            Clauses::Random => function.declared,
            Clauses::All => true,
        };
        let clause = if declares {
            let mut items: Vec<String> = expected[index].iter().cloned().collect();
            if function.calls_parameter() {
                items.push("e".into());
            }
            format!(" effects [{}]", items.join(", "))
        } else {
            String::new()
        };
        let parameter = if function.higher_order {
            ", g: (ClienteId) -> ClienteId ! [e]"
        } else {
            ""
        };
        let result = if function.returns { ": ClienteId" } else { "" };
        source.push_str(&format!(
            "fun f{index}(id: ClienteId{parameter}){result}{clause} {{\n"
        ));
        for action in &function.actions {
            let expr = match action {
                Action::Read => "db.clientes.require(id)".to_string(),
                Action::Write => "db.clientes.insert { nome = \"x\" }".to_string(),
                Action::CallParameter => "g(id)".to_string(),
                Action::Call(other, arg) => render_call(program, *other, arg, "id"),
                Action::CallViaLambda(other, arg) => format!(
                    "aplica({{ x -> {} }}, id)",
                    render_call(program, *other, arg, "x")
                ),
            };
            source.push_str(&format!("    val _ = {expr}\n"));
        }
        source.push_str("    id\n}\n");
    }
    source
}

fn render_call(program: &[EffectFunction], callee: usize, arg: &Arg, key: &str) -> String {
    if !program[callee].higher_order {
        return format!("f{callee}({key})");
    }
    let arg = match arg {
        Arg::Reads => "{ y -> db.clientes.require(y).id }".to_string(),
        Arg::Function(value) if !program[*value].higher_order => format!("f{value}"),
        Arg::Identity | Arg::Function(_) => "{ y -> y }".to_string(),
    };
    format!("f{callee}({key}, {arg})")
}

/// The expected effects, computed on the program itself (not by the type checker): each
/// function's own operations plus those of every function it calls (directly or through a
/// lambda), plus the effects of the argument it passes to a higher-order function that
/// calls its parameter; iterated to a fixed point, which covers recursion.
fn reachable_effects(program: &[EffectFunction]) -> Vec<BTreeSet<String>> {
    let mut effects = vec![BTreeSet::new(); program.len()];
    loop {
        let mut changed = false;
        for (index, function) in program.iter().enumerate() {
            let mut next: BTreeSet<String> = effects[index].clone();
            for action in &function.actions {
                match action {
                    Action::Read => {
                        next.insert("db.read".to_string());
                    }
                    Action::Write => {
                        next.insert("db.write".to_string());
                    }
                    // The parameter's effects are the caller's (through `e`).
                    Action::CallParameter => {}
                    Action::Call(other, arg) | Action::CallViaLambda(other, arg) => {
                        next.extend(effects[*other].iter().cloned());
                        if program[*other].calls_parameter() {
                            match arg {
                                Arg::Reads => {
                                    next.insert("db.read".to_string());
                                }
                                Arg::Function(value) if !program[*value].higher_order => {
                                    next.extend(effects[*value].iter().cloned());
                                }
                                Arg::Identity | Arg::Function(_) => {}
                            }
                        }
                    }
                }
            }
            if next != effects[index] {
                effects[index] = next;
                changed = true;
            }
        }
        if !changed {
            return effects;
        }
    }
}

type Checked = (BTreeMap<String, Vec<String>>, BTreeMap<String, String>);

/// Effects and schemes of every function, or the diagnostics.
fn check_effect_program(source: &str) -> Result<Checked, String> {
    let (tokens, mut diagnostics) = lex("effects.bd", source);
    let (program, mut parse_diagnostics) = parse(tokens);
    diagnostics.append(&mut parse_diagnostics);
    if has_errors(&diagnostics) {
        return Err(format!("{diagnostics:?}"));
    }
    let (typed, type_diagnostics) = super::check(&program, effect_schema());
    if has_errors(&type_diagnostics) {
        return Err(format!("{type_diagnostics:?}"));
    }
    Ok((typed.effects, typed.schemes))
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn inferred_effects_equal_call_graph_reachability(program in arb_effect_program()) {
        let expected = reachable_effects(&program);
        let source = render_effect_program(&program, Clauses::Random, &expected);
        let (effects, _) = check_effect_program(&source).map_err(|error| TestCaseError::fail(format!("{error}\n{source}")))?;
        for (index, expected) in expected.iter().enumerate() {
            let found: BTreeSet<String> = effects[&format!("f{index}")].iter().cloned().collect();
            prop_assert_eq!(&found, expected, "f{} in\n{}", index, source);
        }
    }

    #[test]
    fn declaring_the_inferred_effects_is_stable(program in arb_effect_program()) {
        let expected = reachable_effects(&program);
        let plain = render_effect_program(&program, Clauses::None, &expected);
        let (_, before) = check_effect_program(&plain).map_err(|error| TestCaseError::fail(format!("{error}\n{plain}")))?;
        let annotated = render_effect_program(&program, Clauses::All, &expected);
        let (effects, after) = check_effect_program(&annotated).map_err(|error| TestCaseError::fail(format!("{error}\n{annotated}")))?;
        for (index, expected) in expected.iter().enumerate() {
            let found: BTreeSet<String> = effects[&format!("f{index}")].iter().cloned().collect();
            prop_assert_eq!(&found, expected, "f{} in\n{}", index, annotated);
        }
        prop_assert_eq!(&before, &after, "schemes changed by annotating:\n{}\n---\n{}", plain, annotated);
    }
}
