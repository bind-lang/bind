//! Differential test: the reference evaluator (`core::eval`) against the generated Rust.
//!
//! A deterministic generator writes 1,000 pure, parameterless Bind functions
//! (`fun p<i>(): T = …`, `T` ∈ {`Int`, `Boolean`, `String`, `List<Int>`, `Int?`}) plus a few
//! fixed programs around the recursion limit, as Bind source. The source goes through the real
//! front end (lexer, parser, type checker, lowering, IR validation). Each function is evaluated
//! by `core::eval`; all of them are rendered into one crate (`target/bind-differential/`) whose
//! `main` runs each `bind_p<i>(Ctx::pure())` on a tokio runtime with 64 MiB worker stacks and
//! prints `p<i>=<render>` in the canonical form of `Value::render`. The two outputs must agree
//! line by line.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use num_bigint::BigInt;

use crate::core::eval::{EvalError, eval_function};
use crate::core::ir::CoreProgram;
use crate::diagnostic::Diagnostic;
use crate::schema::DatabaseSchema;

/// Number of random programs.
const PROGRAMS: usize = 1000;
/// Seed of the generator.
const SEED: u64 = 0x00B1_4D00_D1FF_2026;
/// Expression depth budget of a program body.
const DEPTH: u32 = 4;

/// The fixed declarations every program may use: a sum type (recursive, with a `String` field
/// and a field-less variant), a record, a recursive helper with a decreasing counter, a named
/// function used as a value and a higher-order function.
const PRELUDE: &str = "sealed class Forma {
    data class Circulo(val raio: Int)
    data class Retangulo(val largura: Int, val altura: Int)
    data class Rotulada(val nome: String, val dentro: Forma)
    data object Vazia
}

data class Par(val primeiro: Int, val segundo: String)

fun conta(n: Int, acc: Int): Int = if (n <= 0) acc else conta(n - 1, acc + n)

fun dobra(x: Int): Int = x * 2

fun aplica(g: (Int) -> Int, x: Int): Int = g(x)
";

/// Programs at the call-depth limit (10,000 nested Bind calls). `p<k>` itself is one call and
/// `conta(n, 0)` is `n + 1` more: `conta(9998, 0)` reaches exactly 10,000, `conta(9999, 0)`
/// is one call too many, and 20,000 is far beyond (Review Focus 3).
const FIXED: [(&str, &str); 3] = [
    ("Int", "conta(20000, 0)"),
    ("Int", "conta(9998, 0)"),
    ("Int", "conta(9999, 0)"),
];

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (n > 0).
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn range(&mut self, low: i64, high: i64) -> i64 {
        low + (self.next() % (high - low + 1) as u64) as i64
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
    Int,
    Bool,
    Str,
    List,
    OptInt,
    Forma,
}

impl Ty {
    fn bind(self) -> &'static str {
        match self {
            Ty::Int => "Int",
            Ty::Bool => "Boolean",
            Ty::Str => "String",
            Ty::List => "List<Int>",
            Ty::OptInt => "Int?",
            Ty::Forma => "Forma",
        }
    }
}

const RESULT_TYPES: [Ty; 5] = [Ty::Int, Ty::Bool, Ty::Str, Ty::List, Ty::OptInt];
const VALUE_TYPES: [Ty; 6] = [Ty::Int, Ty::Bool, Ty::Str, Ty::List, Ty::OptInt, Ty::Forma];

/// Pieces of string literals: (Bind source, value).
const STRING_PIECES: &[(&str, &str)] = &[
    ("a", "a"),
    ("bind", "bind"),
    ("Zz", "Zz"),
    (" ", " "),
    ("0", "0"),
    ("\\n", "\n"),
    ("\\t", "\t"),
    ("\\r", "\r"),
    ("\\\"", "\""),
    ("\\\\", "\\"),
    ("\\'", "'"),
    ("\\$", "$"),
    ("\\b", "\u{8}"),
    ("\\u0001", "\u{1}"),
    ("\\u000c", "\u{c}"),
    ("\\u001F", "\u{1f}"),
    ("\\u007f", "\u{7f}"),
    ("\\u00e9", "é"),
    ("ção", "ção"),
    ("é", "é"),
    ("😀", "😀"),
    ("\\u2028", "\u{2028}"),
    // A lone surrogate escape is kept as its six source characters.
    ("\\uD800", "\\uD800"),
    ("{", "{"),
    ("}", "}"),
];

/// Variables in scope, innermost last.
type Scope = Vec<(String, Ty)>;

struct Gen {
    rng: Rng,
    next_name: usize,
    /// How many times each construct was generated.
    counts: BTreeMap<&'static str, usize>,
}

impl Gen {
    fn new(seed: u64) -> Gen {
        Gen {
            rng: Rng(seed),
            next_name: 0,
            counts: BTreeMap::new(),
        }
    }

    fn count(&mut self, construct: &'static str) {
        *self.counts.entry(construct).or_default() += 1;
    }

    fn fresh(&mut self) -> String {
        self.next_name += 1;
        format!("v{}", self.next_name)
    }

    /// `fun p<i>(): T …`: an expression body or a block with `val`s and an early `return`.
    fn program(&mut self, index: usize) -> (Ty, String) {
        self.next_name = 0;
        let ty = *self.rng.pick(&RESULT_TYPES);
        let name = format!("p{index}");
        let source = if self.rng.chance(30) {
            self.count("block body");
            let mut scope = Scope::new();
            let mut lines = Vec::new();
            for _ in 0..self.rng.range(1, 3) {
                let local_ty = *self.rng.pick(&VALUE_TYPES);
                let local = self.fresh();
                let value = if self.rng.chance(25) {
                    self.count("early return");
                    format!(
                        "if ({}) return {} else {}",
                        self.expr(Ty::Bool, 2, &scope),
                        self.expr(ty, 2, &scope),
                        self.expr(local_ty, 2, &scope)
                    )
                } else {
                    self.count("val");
                    self.expr(local_ty, 3, &scope)
                };
                lines.push(format!("    val {local}: {} = {value}", local_ty.bind()));
                scope.push((local, local_ty));
            }
            let result = self.expr(ty, DEPTH - 1, &scope);
            format!(
                "fun {name}(): {} {{\n{}\n    {result}\n}}\n",
                ty.bind(),
                lines.join("\n")
            )
        } else {
            format!(
                "fun {name}(): {} = {}\n",
                ty.bind(),
                self.expr(ty, DEPTH, &Scope::new())
            )
        };
        (ty, source)
    }

    fn var(&mut self, ty: Ty, scope: &Scope) -> Option<String> {
        let candidates: Vec<&String> = scope
            .iter()
            .filter(|(_, t)| *t == ty)
            .map(|(name, _)| name)
            .collect();
        if candidates.is_empty() {
            None
        } else {
            Some(self.rng.pick(&candidates).to_string())
        }
    }

    fn expr(&mut self, ty: Ty, depth: u32, scope: &Scope) -> String {
        if (depth == 0 || self.rng.chance(15))
            && self.rng.chance(60)
            && let Some(name) = self.var(ty, scope)
        {
            self.count("variable");
            return name;
        }
        if depth == 0 {
            return self.leaf(ty);
        }
        let d = depth - 1;
        match ty {
            Ty::Int => self.int(d, scope),
            Ty::Bool => self.boolean(d, scope),
            Ty::Str => self.string(d, scope),
            Ty::List => self.list(d, scope),
            Ty::OptInt => self.opt_int(d, scope),
            Ty::Forma => self.forma(d, scope),
        }
    }

    fn leaf(&mut self, ty: Ty) -> String {
        match ty {
            Ty::Int => self.int_literal().0,
            Ty::Bool => {
                self.count("boolean literal");
                self.rng.pick(&["true", "false"]).to_string()
            }
            Ty::Str => self.string_literal().0,
            Ty::List => {
                self.count("listOf");
                let items: Vec<String> = (0..self.rng.range(0, 3))
                    .map(|_| self.int_literal().0)
                    .collect();
                format!("listOf({})", items.join(", "))
            }
            Ty::OptInt => {
                if self.rng.chance(50) {
                    self.count("null");
                    "null".into()
                } else {
                    self.count("first");
                    format!("first(listOf({}))", self.int_literal().0)
                }
            }
            Ty::Forma => {
                self.count("constructor");
                if self.rng.chance(30) {
                    "Vazia".into()
                } else {
                    format!("Circulo({})", self.leaf(Ty::Int))
                }
            }
        }
    }

    /// An integer literal as source (decimal, hex, binary, underscores, `L`; up to 40 digits),
    /// in an expression position (negative ones parenthesized), and its value.
    fn int_literal(&mut self) -> (String, BigInt) {
        self.count("integer literal");
        let (text, value) = self.int_literal_text();
        if text.starts_with('-') {
            (format!("({text})"), value)
        } else {
            (text, value)
        }
    }

    /// An integer literal as written in a pattern (`-5`, `0x1f`, …) and its value.
    fn int_literal_text(&mut self) -> (String, BigInt) {
        let negative = self.rng.chance(25);
        let (digits, value) = match self.rng.below(10) {
            0..=3 => {
                let n = self.rng.range(0, 20);
                (n.to_string(), BigInt::from(n))
            }
            4 => {
                let n = self.rng.range(0, 1_000_000);
                let text = n.to_string();
                // `1_000_000`-style grouping.
                let mut grouped = String::new();
                for (i, c) in text.chars().enumerate() {
                    if i > 0 && (text.len() - i).is_multiple_of(3) {
                        grouped.push('_');
                    }
                    grouped.push(c);
                }
                (grouped, BigInt::from(n))
            }
            5 => {
                let n = self.rng.range(0, 0xFFFF);
                (format!("0x{n:X}"), BigInt::from(n))
            }
            6 => {
                let n = self.rng.range(0, 64);
                (format!("0b{n:b}"), BigInt::from(n))
            }
            7 => {
                let n = self.rng.range(0, 99);
                (format!("{n}L"), BigInt::from(n))
            }
            _ => {
                let length = self.rng.range(10, 40) as usize;
                let mut text = String::new();
                text.push(char::from(b'1' + self.rng.below(9) as u8));
                for _ in 1..length {
                    text.push(char::from(b'0' + self.rng.below(10) as u8));
                }
                let value = BigInt::parse_bytes(text.as_bytes(), 10).unwrap();
                (text, value)
            }
        };
        if negative {
            (format!("-{digits}"), -value)
        } else {
            (digits, value)
        }
    }

    fn string_literal(&mut self) -> (String, String) {
        self.count("string literal");
        let mut source = String::from("\"");
        let mut value = String::new();
        for _ in 0..self.rng.range(0, 4) {
            let (piece, piece_value) = *self.rng.pick(STRING_PIECES);
            source.push_str(piece);
            value.push_str(piece_value);
        }
        source.push('"');
        (source, value)
    }

    /// A lambda body: an expression, sometimes after a `val`.
    fn lambda_body(&mut self, ty: Ty, depth: u32, scope: &Scope) -> String {
        if depth > 0 && self.rng.chance(20) {
            self.count("val in lambda");
            let local_ty = *self.rng.pick(&VALUE_TYPES);
            let local = self.fresh();
            let value = self.expr(local_ty, depth - 1, scope);
            let mut inner = scope.clone();
            inner.push((local.clone(), local_ty));
            let body = self.expr(ty, depth - 1, &inner);
            format!("val {local}: {} = {value}\n{body}", local_ty.bind())
        } else {
            self.expr(ty, depth, scope)
        }
    }

    /// `fold(xs, init, { acc, x -> … })` with an accumulator of type `ty`.
    fn fold(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        self.count("fold");
        let list = self.anchored_list(d, scope);
        let init = self.expr(ty, d, scope);
        let (acc, x) = (self.fresh(), self.fresh());
        let mut inner = scope.clone();
        inner.push((acc.clone(), ty));
        inner.push((x.clone(), Ty::Int));
        let body = self.lambda_body(ty, d, &inner);
        format!("fold({list}, {init}, {{ {acc}, {x} -> {body} }})")
    }

    fn if_expr(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        self.count("if");
        format!(
            "(if ({}) {} else {})",
            self.expr(Ty::Bool, d, scope),
            self.expr(ty, d, scope),
            self.expr(ty, d, scope)
        )
    }

    fn int(&mut self, d: u32, scope: &Scope) -> String {
        match self.rng.below(24) {
            0 | 1 => self.int_literal().0,
            2 | 3 => {
                let op = *self.rng.pick(&["+", "-", "*"]);
                self.count(match op {
                    "+" => "int +",
                    "-" => "int -",
                    _ => "int *",
                });
                format!(
                    "({} {op} {})",
                    self.expr(Ty::Int, d, scope),
                    self.expr(Ty::Int, d, scope)
                )
            }
            4 => {
                self.count("unary -");
                format!("(-{})", self.expr(Ty::Int, d, scope))
            }
            5 | 6 => {
                let op = *self.rng.pick(&["/", "%"]);
                self.count(if op == "/" { "int / ?:" } else { "int % ?:" });
                let divisor = if self.rng.chance(30) {
                    self.rng.pick(&["0", "1", "(-1)", "2", "(-3)"]).to_string()
                } else {
                    self.expr(Ty::Int, d, scope)
                };
                format!(
                    "(({} {op} {divisor}) ?: {})",
                    self.expr(Ty::Int, d, scope),
                    self.expr(Ty::Int, d, scope)
                )
            }
            7 => self.if_expr(Ty::Int, d, scope),
            8 | 9 => self.when_forma(Ty::Int, d, scope),
            10 => self.when_opt_int(Ty::Int, d, scope),
            11 => self.when_int(Ty::Int, d, scope),
            12 => self.when_string(Ty::Int, d, scope),
            13 => self.when_bool(Ty::Int, d, scope),
            14 => self.fold(Ty::Int, d, scope),
            15 => {
                self.count("length");
                format!("length({})", self.anchored_list(d, scope))
            }
            16 => {
                self.count("first ?:");
                format!(
                    "(first({}) ?: {})",
                    self.expr(Ty::List, d, scope),
                    self.expr(Ty::Int, d, scope)
                )
            }
            17 => {
                self.count("get ?:");
                format!(
                    "(get({}, {}) ?: {})",
                    self.expr(Ty::List, d, scope),
                    self.expr(Ty::Int, d.min(1), scope),
                    self.expr(Ty::Int, d, scope)
                )
            }
            18 => {
                self.count("Int? ?:");
                format!(
                    "({} ?: {})",
                    self.expr(Ty::OptInt, d, scope),
                    self.expr(Ty::Int, d, scope)
                )
            }
            19 => {
                self.count("recursive helper");
                format!(
                    "conta({}, {})",
                    self.rng.range(0, 50),
                    self.expr(Ty::Int, d, scope)
                )
            }
            20 => {
                self.count("named function call");
                format!("dobra({})", self.expr(Ty::Int, d, scope))
            }
            21 => {
                self.count("lambda argument");
                let y = self.fresh();
                let mut inner = scope.clone();
                inner.push((y.clone(), Ty::Int));
                let body = self.lambda_body(Ty::Int, d, &inner);
                format!(
                    "aplica({{ {y} -> {body} }}, {})",
                    self.expr(Ty::Int, d, scope)
                )
            }
            22 => {
                self.count("record field");
                format!(
                    "Par({}, {}).primeiro",
                    self.expr(Ty::Int, d, scope),
                    self.expr(Ty::Str, d.min(1), scope)
                )
            }
            _ => self.leaf(Ty::Int),
        }
    }

    fn boolean(&mut self, d: u32, scope: &Scope) -> String {
        match self.rng.below(14) {
            0 => self.leaf(Ty::Bool),
            1..=3 => {
                let op = *self.rng.pick(&["<", "<=", ">", ">=", "==", "!="]);
                self.count("int comparison");
                format!(
                    "({} {op} {})",
                    self.expr(Ty::Int, d, scope),
                    self.expr(Ty::Int, d, scope)
                )
            }
            4 => {
                let op = *self.rng.pick(&["<", ">=", "==", "!="]);
                self.count("string comparison");
                format!(
                    "({} {op} {})",
                    self.expr(Ty::Str, d, scope),
                    self.expr(Ty::Str, d, scope)
                )
            }
            5 => {
                self.count("list equality");
                // A non-empty literal on the left fixes the element type.
                let op = *self.rng.pick(&["==", "!="]);
                format!(
                    "(listOf({}) {op} {})",
                    self.expr(Ty::Int, d, scope),
                    self.expr(Ty::List, d, scope)
                )
            }
            6 => {
                self.count("Int? equality");
                if self.rng.chance(50) {
                    format!("({} == null)", self.anchored_opt_int(d, scope))
                } else {
                    format!(
                        "({} != {})",
                        self.anchored_opt_int(d, scope),
                        self.expr(Ty::OptInt, d, scope)
                    )
                }
            }
            7 => {
                self.count("!");
                format!("!{}", self.atom_bool(d, scope))
            }
            8 => {
                let op = *self.rng.pick(&["&&", "||"]);
                self.count(if op == "&&" { "&&" } else { "||" });
                format!(
                    "({} {op} {})",
                    self.expr(Ty::Bool, d, scope),
                    self.expr(Ty::Bool, d, scope)
                )
            }
            9 => self.if_expr(Ty::Bool, d, scope),
            10 => self.when_forma(Ty::Bool, d, scope),
            11 => self.fold(Ty::Bool, d, scope),
            12 => {
                self.count("boolean equality");
                format!(
                    "({} == {})",
                    self.expr(Ty::Bool, d, scope),
                    self.expr(Ty::Bool, d, scope)
                )
            }
            _ => self.when_opt_int(Ty::Bool, d, scope),
        }
    }

    /// A Boolean expression that is safe after `!` (parenthesized or atomic).
    fn atom_bool(&mut self, d: u32, scope: &Scope) -> String {
        let inner = self.expr(Ty::Bool, d, scope);
        if inner.starts_with('(') && inner.ends_with(')') {
            inner
        } else {
            format!("({inner})")
        }
    }

    fn string(&mut self, d: u32, scope: &Scope) -> String {
        match self.rng.below(10) {
            0 | 1 => self.string_literal().0,
            2 | 3 => {
                self.count("string +");
                format!(
                    "({} + {})",
                    self.expr(Ty::Str, d, scope),
                    self.expr(Ty::Str, d, scope)
                )
            }
            4 => self.if_expr(Ty::Str, d, scope),
            5 => self.when_int(Ty::Str, d, scope),
            6 => self.when_string(Ty::Str, d, scope),
            7 => self.when_forma(Ty::Str, d, scope),
            8 => self.fold(Ty::Str, d, scope),
            _ => {
                self.count("record field");
                format!(
                    "Par({}, {}).segundo",
                    self.expr(Ty::Int, d.min(1), scope),
                    self.expr(Ty::Str, d, scope)
                )
            }
        }
    }

    fn list(&mut self, d: u32, scope: &Scope) -> String {
        match self.rng.below(9) {
            0 | 1 => {
                self.count("listOf");
                let items: Vec<String> = (0..self.rng.range(0, 4))
                    .map(|_| self.expr(Ty::Int, d, scope))
                    .collect();
                format!("listOf({})", items.join(", "))
            }
            2 | 3 => {
                self.count("map with lambda");
                let list = self.anchored_list(d, scope);
                let x = self.fresh();
                let mut inner = scope.clone();
                inner.push((x.clone(), Ty::Int));
                let body = self.lambda_body(Ty::Int, d, &inner);
                format!("map({list}, {{ {x} -> {body} }})")
            }
            4 => {
                self.count("map with named function");
                format!("map({}, dobra)", self.expr(Ty::List, d, scope))
            }
            5 => self.if_expr(Ty::List, d, scope),
            6 => self.fold(Ty::List, d, scope),
            7 => self.when_forma(Ty::List, d, scope),
            _ => {
                self.count("emptyList");
                "emptyList()".into()
            }
        }
    }

    fn opt_int(&mut self, d: u32, scope: &Scope) -> String {
        match self.rng.below(9) {
            0 => self.leaf(Ty::OptInt),
            1 | 2 => {
                let op = *self.rng.pick(&["/", "%"]);
                self.count(if op == "/" { "int /" } else { "int %" });
                let divisor = if self.rng.chance(30) {
                    "0".to_string()
                } else {
                    self.expr(Ty::Int, d, scope)
                };
                format!("({} {op} {divisor})", self.expr(Ty::Int, d, scope))
            }
            3 => {
                self.count("first");
                format!("first({})", self.expr(Ty::List, d, scope))
            }
            4 => {
                self.count("get");
                format!(
                    "get({}, {})",
                    self.expr(Ty::List, d, scope),
                    self.expr(Ty::Int, d.min(1), scope)
                )
            }
            5 => self.if_expr(Ty::OptInt, d, scope),
            6 => self.when_opt_int(Ty::OptInt, d, scope),
            7 => self.when_forma(Ty::OptInt, d, scope),
            _ => self.fold(Ty::OptInt, d, scope),
        }
    }

    /// A `List<Int>` expression whose own subterms fix its element type. Lists that a `fold`,
    /// a `map` with a lambda or `length` consume are anchored, so every lambda parameter that
    /// ranges over a list is an `Int` and an `Int` expression is never of an unknown type.
    fn anchored_list(&mut self, d: u32, scope: &Scope) -> String {
        if self.rng.chance(20)
            && let Some(name) = self.var(Ty::List, scope)
        {
            self.count("variable");
            return name;
        }
        match self.rng.below(4) {
            0 if d > 0 => {
                self.count("if");
                format!(
                    "(if ({}) {} else {})",
                    self.expr(Ty::Bool, d - 1, scope),
                    self.anchored_list(d - 1, scope),
                    self.expr(Ty::List, d - 1, scope)
                )
            }
            1 => {
                self.count("map with named function");
                format!("map({}, dobra)", self.expr(Ty::List, d, scope))
            }
            _ => {
                self.count("listOf");
                let items: Vec<String> = (0..self.rng.range(1, 4))
                    .map(|_| self.expr(Ty::Int, d, scope))
                    .collect();
                format!("listOf({})", items.join(", "))
            }
        }
    }

    /// An `Int?` expression whose own subterms fix its type (`null`, `listOf()` alone would
    /// leave `'a?` ambiguous where nothing else fixes it, e.g. `null == null`).
    fn anchored_opt_int(&mut self, d: u32, scope: &Scope) -> String {
        if self.rng.chance(20)
            && let Some(name) = self.var(Ty::OptInt, scope)
        {
            self.count("variable");
            return name;
        }
        match self.rng.below(6) {
            0 => {
                let op = *self.rng.pick(&["/", "%"]);
                self.count(if op == "/" { "int /" } else { "int %" });
                format!(
                    "({} {op} {})",
                    self.expr(Ty::Int, d, scope),
                    self.expr(Ty::Int, d, scope)
                )
            }
            1 => {
                self.count("first");
                format!("first(listOf({}))", self.expr(Ty::Int, d, scope))
            }
            2 => {
                self.count("get");
                format!(
                    "get(listOf({}, {}), {})",
                    self.expr(Ty::Int, d, scope),
                    self.expr(Ty::Int, d, scope),
                    self.expr(Ty::Int, d.min(1), scope)
                )
            }
            3 if d > 0 => {
                self.count("if");
                format!(
                    "(if ({}) {} else {})",
                    self.expr(Ty::Bool, d - 1, scope),
                    self.anchored_opt_int(d - 1, scope),
                    self.expr(Ty::OptInt, d - 1, scope)
                )
            }
            4 if d > 0 => {
                self.count("fold");
                let list = self.expr(Ty::List, d - 1, scope);
                let init = self.anchored_opt_int(d - 1, scope);
                let (acc, x) = (self.fresh(), self.fresh());
                let mut inner = scope.clone();
                inner.push((acc.clone(), Ty::OptInt));
                inner.push((x.clone(), Ty::Int));
                let body = self.lambda_body(Ty::OptInt, d - 1, &inner);
                format!("fold({list}, {init}, {{ {acc}, {x} -> {body} }})")
            }
            _ => {
                self.count("map with named function");
                format!("first(map({}, dobra))", self.expr(Ty::List, d, scope))
            }
        }
    }

    fn forma(&mut self, d: u32, scope: &Scope) -> String {
        match self.rng.below(7) {
            0 => {
                self.count("constructor");
                format!("Circulo({})", self.expr(Ty::Int, d, scope))
            }
            1 => {
                self.count("constructor");
                format!(
                    "Retangulo({}, {})",
                    self.expr(Ty::Int, d, scope),
                    self.expr(Ty::Int, d, scope)
                )
            }
            2 | 3 => {
                self.count("constructor");
                format!(
                    "Rotulada({}, {})",
                    self.expr(Ty::Str, d.min(1), scope),
                    self.expr(Ty::Forma, d, scope)
                )
            }
            4 => {
                self.count("constructor");
                "Vazia".into()
            }
            5 => self.if_expr(Ty::Forma, d, scope),
            _ => self.fold(Ty::Forma, d, scope),
        }
    }

    fn when(&mut self, scrutinee: String, arms: Vec<(String, String)>) -> String {
        let arms: Vec<String> = arms
            .into_iter()
            .map(|(pattern, body)| format!("    {pattern} -> {}", body.replace('\n', "\n    ")))
            .collect();
        format!("when ({scrutinee}) {{\n{}\n}}", arms.join("\n"))
    }

    /// An arm body of type `ty` in `scope` extended with `bound`.
    fn arm(&mut self, ty: Ty, d: u32, scope: &Scope, bound: Vec<(String, Ty)>) -> String {
        let mut inner = scope.clone();
        inner.extend(bound);
        self.expr(ty, d, &inner)
    }

    /// An `Int` sub-pattern: a variable, `_`, or (when `refutable`) a literal.
    fn int_subpattern(&mut self, refutable: bool, bound: &mut Vec<(String, Ty)>) -> String {
        if refutable {
            return self.int_literal_text().0;
        }
        if self.rng.chance(60) {
            let name = self.fresh();
            bound.push((name.clone(), Ty::Int));
            name
        } else {
            "_".into()
        }
    }

    fn str_subpattern(&mut self, refutable: bool, bound: &mut Vec<(String, Ty)>) -> String {
        if refutable {
            return self.string_literal().0;
        }
        if self.rng.chance(60) {
            let name = self.fresh();
            bound.push((name.clone(), Ty::Str));
            name
        } else {
            "_".into()
        }
    }

    /// A pattern for one `Forma` constructor. A refutable pattern does not match every value
    /// of that constructor; an irrefutable one does.
    fn forma_pattern(
        &mut self,
        ctor: usize,
        refutable: bool,
        depth: u32,
        bound: &mut Vec<(String, Ty)>,
    ) -> String {
        match ctor {
            0 => format!("Circulo({})", self.int_subpattern(refutable, bound)),
            1 => {
                let which = self.rng.below(2);
                let a = self.int_subpattern(refutable && which == 0, bound);
                let b = self.int_subpattern(refutable && which == 1, bound);
                format!("Retangulo({a}, {b})")
            }
            2 => {
                if !refutable {
                    let name = self.str_subpattern(false, bound);
                    let inner = if self.rng.chance(50) {
                        let local = self.fresh();
                        bound.push((local.clone(), Ty::Forma));
                        local
                    } else {
                        "_".into()
                    };
                    return format!("Rotulada({name}, {inner})");
                }
                // Refutable through the name, the nested shape, or both.
                let (name_refutable, inner_refutable) = match self.rng.below(3) {
                    0 => (true, false),
                    1 => (false, true),
                    _ => (true, true),
                };
                let name = self.str_subpattern(name_refutable, bound);
                let inner = if inner_refutable {
                    self.count("nested pattern");
                    let inner_ctor = self.rng.below(4);
                    let deeper = depth > 0 && inner_ctor != 3 && self.rng.chance(50);
                    self.forma_pattern(inner_ctor, deeper, depth.saturating_sub(1), bound)
                } else if self.rng.chance(50) {
                    let local = self.fresh();
                    bound.push((local.clone(), Ty::Forma));
                    local
                } else {
                    "_".into()
                };
                format!("Rotulada({name}, {inner})")
            }
            _ => "Vazia".into(),
        }
    }

    /// `when` over a `Forma`: per constructor (in random order) maybe one refutable arm, then
    /// maybe an irrefutable one; a final `_` or variable arm unless every constructor has an
    /// irrefutable arm. No arm is ever redundant.
    fn when_forma(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        self.count("when over sum type");
        let scrutinee = self.expr(Ty::Forma, d, scope);
        let mut order = vec![0, 1, 2, 3];
        for i in (1..order.len()).rev() {
            order.swap(i, self.rng.below(i + 1));
        }
        let mut arms = Vec::new();
        let mut complete = true;
        for ctor in order {
            if ctor != 3 && self.rng.chance(35) {
                let mut bound = Vec::new();
                let pattern = self.forma_pattern(ctor, true, 1, &mut bound);
                let body = self.arm(ty, d, scope, bound);
                arms.push((pattern, body));
            }
            if self.rng.chance(70) {
                let mut bound = Vec::new();
                let pattern = self.forma_pattern(ctor, false, 1, &mut bound);
                let body = self.arm(ty, d, scope, bound);
                arms.push((pattern, body));
            } else {
                complete = false;
            }
        }
        if !complete {
            let mut bound = Vec::new();
            let pattern = if self.rng.chance(50) {
                let name = self.fresh();
                bound.push((name.clone(), Ty::Forma));
                name
            } else {
                "_".into()
            };
            let body = self.arm(ty, d, scope, bound);
            arms.push((pattern, body));
        }
        self.when(scrutinee, arms)
    }

    /// Distinct integer literals (by value) for patterns.
    fn int_patterns(&mut self, count: usize) -> Vec<String> {
        let mut seen: Vec<BigInt> = Vec::new();
        let mut out = Vec::new();
        while out.len() < count {
            let (text, value) = self.int_literal_text();
            if !seen.contains(&value) {
                seen.push(value);
                out.push(text);
            }
        }
        out
    }

    fn when_int(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        self.count("when over Int");
        let scrutinee = self.expr(Ty::Int, d, scope);
        let mut arms = Vec::new();
        let count = self.rng.range(1, 3) as usize;
        for pattern in self.int_patterns(count) {
            let body = self.arm(ty, d, scope, vec![]);
            arms.push((pattern, body));
        }
        let (pattern, bound) = self.catch_all(Ty::Int);
        let body = self.arm(ty, d, scope, bound);
        arms.push((pattern, body));
        self.when(scrutinee, arms)
    }

    fn when_string(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        self.count("when over String");
        let scrutinee = self.expr(Ty::Str, d, scope);
        let mut seen: Vec<String> = Vec::new();
        let mut arms = Vec::new();
        for _ in 0..self.rng.range(1, 3) {
            let (text, value) = self.string_literal();
            if seen.contains(&value) {
                continue;
            }
            seen.push(value);
            let body = self.arm(ty, d, scope, vec![]);
            arms.push((text, body));
        }
        let (pattern, bound) = self.catch_all(Ty::Str);
        let body = self.arm(ty, d, scope, bound);
        arms.push((pattern, body));
        self.when(scrutinee, arms)
    }

    fn when_bool(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        self.count("when over Boolean");
        let scrutinee = self.expr(Ty::Bool, d, scope);
        let first = *self.rng.pick(&["true", "false"]);
        let second = if self.rng.chance(50) {
            if first == "true" { "false" } else { "true" }
        } else {
            "_"
        };
        let a = self.arm(ty, d, scope, vec![]);
        let b = self.arm(ty, d, scope, vec![]);
        self.when(scrutinee, vec![(first.into(), a), (second.into(), b)])
    }

    /// `when` over an `Int?`: literal arms and `null` in random order, then a variable (smart
    /// cast to `Int`, since `null` came first) or `_`.
    fn when_opt_int(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        self.count("when over Int?");
        let scrutinee = self.anchored_opt_int(d, scope);
        let count = self.rng.range(0, 2) as usize;
        let mut patterns = self.int_patterns(count);
        let position = self.rng.below(patterns.len() + 1);
        patterns.insert(position, "null".into());
        let mut arms = Vec::new();
        for pattern in patterns {
            let body = self.arm(ty, d, scope, vec![]);
            arms.push((pattern, body));
        }
        let (pattern, bound) = self.catch_all(Ty::Int);
        let body = self.arm(ty, d, scope, bound);
        arms.push((pattern, body));
        self.when(scrutinee, arms)
    }

    fn catch_all(&mut self, ty: Ty) -> (String, Vec<(String, Ty)>) {
        if self.rng.chance(50) {
            let name = self.fresh();
            (name.clone(), vec![(name, ty)])
        } else {
            ("_".into(), vec![])
        }
    }
}

/// (name, result type, Bind source of the function) of each program.
type Programs = Vec<(String, Ty, String)>;

/// The generated programs and how many times each construct occurs in them.
fn programs() -> (Programs, BTreeMap<&'static str, usize>) {
    let mut generator = Gen::new(SEED);
    let mut out = Vec::new();
    for index in 0..PROGRAMS {
        let (ty, source) = generator.program(index);
        out.push((format!("p{index}"), ty, source));
    }
    for (offset, (ty, body)) in FIXED.iter().enumerate() {
        let name = format!("p{}", PROGRAMS + offset);
        out.push((
            name.clone(),
            Ty::Int,
            format!("fun {name}(): {ty} = {body}\n"),
        ));
    }
    (out, generator.counts)
}

fn source(programs: &[(String, Ty, String)]) -> String {
    let mut source = String::from(PRELUDE);
    for (_, _, function) in programs {
        source.push('\n');
        source.push_str(function);
    }
    source
}

/// The full front end; the generator must only produce programs without any diagnostic and
/// whose IR validates.
fn compile(source: &str) -> CoreProgram {
    let (tokens, mut diagnostics) = crate::lexer::lex("differential.bd", source);
    let (program, mut parse_diagnostics) = crate::parser::parse(tokens);
    diagnostics.append(&mut parse_diagnostics);
    let schema = DatabaseSchema::default();
    let (typed, mut check_diagnostics) = crate::types::check(&program, &schema);
    diagnostics.append(&mut check_diagnostics);
    assert!(
        diagnostics.is_empty(),
        "generator bug: {}",
        diagnostics
            .iter()
            .map(|diagnostic: &Diagnostic| format!(
                "{} {} at {:?}: {}",
                diagnostic.code,
                diagnostic.message,
                diagnostic.span,
                excerpt(source, diagnostic)
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let core = crate::core::lower::lower(&program, &typed, &schema);
    let problems = crate::core::validate::validate(&core);
    assert!(problems.is_empty(), "invalid IR: {problems:?}");
    core
}

fn excerpt(source: &str, diagnostic: &Diagnostic) -> String {
    let line = diagnostic.span.line;
    source
        .lines()
        .skip(line.saturating_sub(3))
        .take(5)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `p<i>=<render>` for every program, by the reference evaluator.
fn expected(program: &CoreProgram, programs: &[(String, Ty, String)]) -> Vec<String> {
    programs
        .iter()
        .map(|(name, _, function)| {
            let rendered = match eval_function(program, name, vec![]) {
                Ok(value) => value.render(),
                Err(EvalError::RecursionLimit) => "!RecursionLimit".into(),
                Err(EvalError::Unsupported(what)) => {
                    panic!("{name}: the evaluator does not support {what}\n{function}")
                }
            };
            format!("{name}={rendered}")
        })
        .collect()
}

/// `src/main.rs` of the differential crate.
fn main_rs(programs: &[(String, Ty, String)]) -> String {
    let mut entries = String::new();
    for (name, _, _) in programs {
        let _ = writeln!(
            entries,
            "        (\"{name}\", entry(programs::bind_{name}(Ctx::pure()))),"
        );
    }
    MAIN_TEMPLATE.replace("{{ENTRIES}}", &entries)
}

/// The renderer replicates `Value::render` and `json_string` of `core::eval` exactly.
const MAIN_TEMPLATE: &str = r##"use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bind_differential::bind_runtime::{AppError, BoxFuture, Ctx, Int};
use bind_differential::programs;

trait Render {
    fn render(&self) -> String;
}

impl Render for Int {
    fn render(&self) -> String {
        self.to_string()
    }
}

impl Render for bool {
    fn render(&self) -> String {
        self.to_string()
    }
}

impl Render for String {
    fn render(&self) -> String {
        let mut out = String::from("\"");
        for c in self.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    let _ = write!(out, "\\u{:04x}", c as u32);
                }
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
}

impl Render for () {
    fn render(&self) -> String {
        "()".to_string()
    }
}

impl<T: Render> Render for Option<T> {
    fn render(&self) -> String {
        match self {
            None => "null".to_string(),
            Some(value) => value.render(),
        }
    }
}

impl<T: Render> Render for Arc<Vec<T>> {
    fn render(&self) -> String {
        let items: Vec<String> = self.iter().map(Render::render).collect();
        format!("[{}]", items.join(", "))
    }
}

fn variant(error: &AppError) -> &'static str {
    match error {
        AppError::BadRequest(_) => "BadRequest",
        AppError::NotFound(_) => "NotFound",
        AppError::Database(_) => "Database",
        AppError::Serialization(_) => "Serialization",
        AppError::Overflow(_) => "Overflow",
        AppError::RecursionLimit => "RecursionLimit",
        AppError::Internal(_) => "Internal",
    }
}

type Rendered = Pin<Box<dyn Future<Output = Result<String, AppError>> + Send>>;

fn entry<T: Render + Send + 'static>(future: BoxFuture<T>) -> Rendered {
    Box::pin(async move { future.await.map(|value| value.render()) })
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_stack_size(64 * 1024 * 1024)
        .build();
    let Ok(runtime) = runtime else {
        eprintln!("cannot start the tokio runtime");
        std::process::exit(1);
    };
    let entries: Vec<(&str, Rendered)> = vec![
{{ENTRIES}}    ];
    runtime.block_on(async move {
        for (name, future) in entries {
            let line = match tokio::spawn(future).await {
                Ok(Ok(value)) => value,
                Ok(Err(error)) => format!("!{}", variant(&error)),
                Err(error) => format!("!Panic({error})"),
            };
            println!("{name}={line}");
        }
    });
}
"##;

#[test]
fn generator_is_deterministic_and_well_typed() {
    let (first, counts) = programs();
    let (second, _) = programs();
    assert_eq!(
        first.iter().map(|p| &p.2).collect::<Vec<_>>(),
        second.iter().map(|p| &p.2).collect::<Vec<_>>()
    );
    assert_eq!(first.len(), PROGRAMS + FIXED.len());
    for construct in [
        "fold",
        "map with lambda",
        "when over sum type",
        "when over Int?",
        "recursive helper",
        "string +",
        "int / ?:",
        "int % ?:",
        "if",
        "listOf",
        "early return",
    ] {
        assert!(
            counts.get(construct).copied().unwrap_or(0) > 0,
            "{construct}"
        );
    }
    let program = compile(&source(&first));
    // Every program evaluates (no `Unsupported`); the deep ones hit the limit.
    let lines = expected(&program, &first);
    assert_eq!(lines[PROGRAMS], format!("p{PROGRAMS}=!RecursionLimit"));
    assert_eq!(lines[PROGRAMS + 1], format!("p{}=49985001", PROGRAMS + 1));
    assert_eq!(
        lines[PROGRAMS + 2],
        format!("p{}=!RecursionLimit", PROGRAMS + 2)
    );
}

/// Builds the generated crate and compares its output with the evaluator, line by line.
#[test]
#[ignore = "builds a large Rust crate; run with `cargo test differential -- --ignored`"]
fn differential_evaluator_vs_generated_rust() {
    use std::fs;
    use std::path::PathBuf;
    use std::time::Instant;

    let started = Instant::now();
    let (programs, counts) = programs();
    let source = source(&programs);
    let program = compile(&source);
    let expected = expected(&program, &programs);
    let evaluated = started.elapsed();

    let names: Vec<String> = program
        .functions
        .iter()
        .map(|function| function.name.clone())
        .collect();
    let module = super::render_functions(&program, &names);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
    let crate_dir = super::tests::test_crate_in(&root, "bind-differential");
    fs::write(crate_dir.join("program.bd"), &source).unwrap();
    fs::write(crate_dir.join("src/programs.rs"), module).unwrap();
    fs::write(
        crate_dir.join("src/lib.rs"),
        "#![forbid(unsafe_code)]\n#![deny(\n    clippy::unwrap_used,\n    clippy::expect_used,\n    clippy::panic,\n    clippy::unreachable\n)]\n\npub mod bind_runtime;\npub mod programs;\n",
    )
    .unwrap();
    fs::write(crate_dir.join("src/main.rs"), main_rs(&programs)).unwrap();
    let stdout = super::tests::cargo(&crate_dir, &["run", "--quiet", "--release"]);
    let actual: Vec<&str> = stdout.lines().collect();

    let mut divergences = Vec::new();
    for (index, (name, _, function)) in programs.iter().enumerate() {
        let want = &expected[index];
        let got = actual.get(index).copied().unwrap_or("<missing>");
        if want != got {
            divergences.push(format!(
                "{name}\n{function}expected: {want}\n     got: {got}"
            ));
        }
    }
    eprintln!(
        "differential: {} programs, evaluator+front end {:.1?}, total {:.1?}, constructs {counts:?}",
        programs.len(),
        evaluated,
        started.elapsed()
    );
    assert_eq!(actual.len(), programs.len(), "line count");
    assert!(
        divergences.is_empty(),
        "{} divergences:\n\n{}",
        divergences.len(),
        divergences.join("\n\n")
    );
}
