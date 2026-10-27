//! Differential test: the reference evaluator (`core::eval`) against the generated Rust.
//!
//! A deterministic generator writes 1,000 pure, parameterless Bind functions
//! (`fun p<i>(): T = …`, `T` ∈ {`Int`, `Boolean`, `String`, `List<Int>`, `Int?`}) plus a few
//! fixed programs around the recursion limit, as Bind source. The source goes through the real
//! front end (lexer, parser, type checker, lowering, IR validation). Each function is evaluated
//! by `core::eval`; all of them are rendered into one crate (`target/bind-differential/`) whose
//! `main` runs each `bind_p<i>(Ctx::pure())` on a tokio runtime with 64 MiB worker stacks and
//! prints `p<i>=<render>` in the canonical form of `Value::render`. The crate must pass
//! `cargo clippy -- -D warnings`, and the two outputs must agree line by line.
//!
//! The generator also has a web mode (`Gen::web`, its own seed and prelude additions) for the
//! three-way test in `backend::js::differential`: `Double`, `Float`, `Forma` and `Ui` results.
//! Every draw it adds happens only in that mode, so the server's programs are unchanged
//! (`server_programs_are_unchanged_by_the_web_generator`).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use num_bigint::BigInt;

use crate::core::eval::{EvalError, eval_function};
use crate::core::ir::CoreProgram;
use crate::diagnostic::Diagnostic;
use crate::schema::DatabaseSchema;

/// Number of random programs.
pub(crate) const PROGRAMS: usize = 1000;
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
pub(crate) enum Ty {
    Int,
    Bool,
    Str,
    List,
    OptInt,
    Forma,
    // Only in the three-way test's programs (`Gen::web`).
    Double,
    Float,
    Ui,
    // Collections and rich types, also only in the three-way test's programs.
    Cor,
    SetInt,
    SetStr,
    SetCor,
    MapIntStr,
    MapCorInt,
    Evento,
    CaixaInt,
    CaixaCores,
    ResInt,
}

impl Ty {
    pub(crate) fn bind(self) -> &'static str {
        match self {
            Ty::Int => "Int",
            Ty::Bool => "Boolean",
            Ty::Str => "String",
            Ty::List => "List<Int>",
            Ty::OptInt => "Int?",
            Ty::Forma => "Forma",
            Ty::Double => "Double",
            Ty::Float => "Float",
            Ty::Ui => "Ui",
            Ty::Cor => "Cor",
            Ty::SetInt => "Set<Int>",
            Ty::SetStr => "Set<String>",
            Ty::SetCor => "Set<Cor>",
            Ty::MapIntStr => "Map<Int, String>",
            Ty::MapCorInt => "Map<Cor, Int>",
            Ty::Evento => "Evento",
            Ty::CaixaInt => "Caixa<Int>",
            Ty::CaixaCores => "Caixa<Set<Cor>>",
            Ty::ResInt => "Result<Int, String>",
        }
    }
}

const RESULT_TYPES: [Ty; 5] = [Ty::Int, Ty::Bool, Ty::Str, Ty::List, Ty::OptInt];
const VALUE_TYPES: [Ty; 6] = [Ty::Int, Ty::Bool, Ty::Str, Ty::List, Ty::OptInt, Ty::Forma];
/// Result types of the three-way test's programs: the server's, the sum type, `Double`,
/// `Float` and `Ui` (twice, so UI trees are the most frequent result), then the collections
/// and rich types: an `enum class` declared out of alphabetical order, `Set`s and `Map`s over
/// `Key` types, a sealed class with fields, a generic record at two instances and `Result`.
const WEB_RESULT_TYPES: [Ty; 20] = [
    Ty::Int,
    Ty::Bool,
    Ty::Str,
    Ty::List,
    Ty::OptInt,
    Ty::Forma,
    Ty::Double,
    Ty::Float,
    Ty::Ui,
    Ty::Ui,
    Ty::Cor,
    Ty::SetInt,
    Ty::SetStr,
    Ty::SetCor,
    Ty::MapIntStr,
    Ty::MapCorInt,
    Ty::Evento,
    Ty::CaixaInt,
    Ty::CaixaCores,
    Ty::ResInt,
];
const WEB_VALUE_TYPES: [Ty; 19] = [
    Ty::Int,
    Ty::Bool,
    Ty::Str,
    Ty::List,
    Ty::OptInt,
    Ty::Forma,
    Ty::Double,
    Ty::Float,
    Ty::Ui,
    Ty::Cor,
    Ty::SetInt,
    Ty::SetStr,
    Ty::SetCor,
    Ty::MapIntStr,
    Ty::MapCorInt,
    Ty::Evento,
    Ty::CaixaInt,
    Ty::CaixaCores,
    Ty::ResInt,
];

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
    // A surrogate pair escape is one character, as in Kotlin (a lone surrogate is E1006).
    ("\\uD83D\\uDE00", "😀"),
    ("{", "{"),
    ("}", "}"),
];

/// String pieces of the three-way test: the server's plus HTML-significant text, so `Text`,
/// class names and hrefs need escaping.
const WEB_STRING_PIECES: &[(&str, &str)] = &[
    ("a", "a"),
    ("bind", "bind"),
    (" ", " "),
    ("0", "0"),
    ("\\n", "\n"),
    ("\\t", "\t"),
    ("\\r", "\r"),
    ("\\\"", "\""),
    ("\\\\", "\\"),
    ("\\'", "'"),
    ("\\$", "$"),
    ("\\u0001", "\u{1}"),
    ("\\u001F", "\u{1f}"),
    ("\\u007f", "\u{7f}"),
    ("ção", "ção"),
    ("😀", "😀"),
    ("\\u2028", "\u{2028}"),
    ("\\uD83D\\uDE00", "😀"),
    ("{", "{"),
    ("<", "<"),
    (">", ">"),
    ("&", "&"),
    ("&amp;", "&amp;"),
    ("<script>", "<script>"),
    ("</p>", "</p>"),
    ("&#39;", "&#39;"),
    ("javascript:", "javascript:"),
];

/// Double literals of the three-way test (finite; results reach `Infinity` and `NaN` through
/// arithmetic): ECMAScript's number-to-string boundaries, rounding cases and the extremes.
const DOUBLE_LITERALS: &[&str] = &[
    "0.0",
    "1.0",
    "1.5",
    "0.1",
    "0.2",
    "3.0",
    "100.0",
    "2.5e-3",
    "1e21",
    "1e20",
    "123456789012345680000.0",
    "1e-7",
    "1.5e-6",
    "1e308",
    "1.7976931348623157e308",
    "5e-324",
    "2.2250738585072014e-308",
    "9007199254740993.0",
    "0.30000000000000004",
    "123.456",
    "6.02214076e23",
];

/// Float literals of the three-way test (finite, within the `Float` range).
const FLOAT_LITERALS: &[&str] = &[
    "0.0f",
    "1.0f",
    "0.1f",
    "0.3f",
    "2.5f",
    "100.0f",
    "1e10f",
    "16777217.0f",
    "3.4e38f",
    "1e-45f",
    "1.17549435e-38f",
    "123.456f",
    "1e21f",
];

/// Container UI nodes (class and id arguments).
const CONTAINERS: &[&str] = &[
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
];

/// Link targets: allowed and disallowed schemes, relative URLs, browser normalization.
const HREFS: &[&str] = &[
    "https://x.y/?a=1&b=2",
    "http://x",
    "mailto:a@b.c",
    "tel:+55",
    "/caminho",
    "#topo",
    "?q=1",
    "//host",
    "javascript:alert(1)",
    " JaVaScRiPt:alert(1)",
    "java\\tscript:x",
    "data:text/html,<b>",
    "ftp://x",
    "HTTPS://x.y/",
    "MailTo:a@b.c",
    "hTtP://x",
    "TEL:1",
    "",
];

/// Variables in scope, innermost last.
type Scope = Vec<(String, Ty)>;

pub(crate) struct Gen {
    rng: Rng,
    next_name: usize,
    /// The three-way test's generator: `Double`, `Float` and `Ui` too. Every choice it adds
    /// is drawn only when set, so the server's programs are the same with or without it.
    web: bool,
    /// How many times each construct was generated.
    pub(crate) counts: BTreeMap<&'static str, usize>,
}

impl Gen {
    fn new(seed: u64) -> Gen {
        Gen {
            rng: Rng(seed),
            next_name: 0,
            web: false,
            counts: BTreeMap::new(),
        }
    }

    pub(crate) fn web(seed: u64) -> Gen {
        Gen {
            web: true,
            ..Gen::new(seed)
        }
    }

    fn result_types(&self) -> &'static [Ty] {
        if self.web {
            &WEB_RESULT_TYPES
        } else {
            &RESULT_TYPES
        }
    }

    fn value_types(&self) -> &'static [Ty] {
        if self.web {
            &WEB_VALUE_TYPES
        } else {
            &VALUE_TYPES
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
    pub(crate) fn program(&mut self, index: usize) -> (Ty, String) {
        self.next_name = 0;
        let ty = *self.rng.pick(self.result_types());
        let name = format!("p{index}");
        let source = if self.rng.chance(30) {
            self.count("block body");
            let mut scope = Scope::new();
            let mut lines = Vec::new();
            for _ in 0..self.rng.range(1, 3) {
                let local_ty = *self.rng.pick(self.value_types());
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
            Ty::Double => self.double(d, scope),
            Ty::Float => self.float(d, scope),
            Ty::Ui => self.ui(d, scope),
            Ty::Cor => self.cor(d, scope),
            Ty::SetInt | Ty::SetStr | Ty::SetCor => self.set(ty, d, scope),
            Ty::MapIntStr | Ty::MapCorInt => self.map(ty, d, scope),
            Ty::Evento => self.evento(d, scope),
            Ty::CaixaInt | Ty::CaixaCores => self.caixa(ty, d, scope),
            Ty::ResInt => self.res_int(d, scope),
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
            Ty::Double => self.double_literal(),
            Ty::Float => self.float_literal(),
            Ty::Ui => {
                self.count("UI leaf");
                match self.rng.below(3) {
                    0 => format!("Text({})", self.leaf(Ty::Int)),
                    1 => format!("Text({})", self.string_literal().0),
                    _ => format!("Rule({})", self.string_literal().0),
                }
            }
            Ty::Cor => {
                self.count("enum constructor");
                self.rng.pick(&["Z", "A", "M"]).to_string()
            }
            Ty::SetInt | Ty::SetStr | Ty::SetCor => {
                if self.rng.chance(25) {
                    self.count("emptySet");
                    "emptySet()".into()
                } else {
                    self.count("setOf");
                    let element = set_element(ty);
                    let items: Vec<String> = (0..self.rng.range(1, 4))
                        .map(|_| self.leaf(element))
                        .collect();
                    format!("setOf({})", items.join(", "))
                }
            }
            Ty::MapIntStr | Ty::MapCorInt => {
                self.count("emptyMap");
                if self.rng.chance(30) {
                    "emptyMap()".into()
                } else {
                    self.count("put");
                    let (key, value) = map_entry(ty);
                    format!("put(emptyMap(), {}, {})", self.leaf(key), self.leaf(value))
                }
            }
            Ty::Evento => {
                self.count("sealed with fields constructor");
                if self.rng.chance(30) {
                    "Nada".into()
                } else {
                    format!("Marcado({}, {})", self.leaf(Ty::Cor), self.leaf(Ty::Int))
                }
            }
            Ty::CaixaInt | Ty::CaixaCores => {
                self.count_caixa(ty);
                let inner = self.leaf(caixa_content(ty));
                format!("Caixa({inner}, {})", self.string_literal().0)
            }
            Ty::ResInt => {
                if self.rng.chance(50) {
                    self.count("Ok");
                    format!("Ok({})", self.leaf(Ty::Int))
                } else {
                    self.count("Err");
                    format!("Err({})", self.string_literal().0)
                }
            }
        }
    }

    /// An integer literal as source (decimal, hex, binary, underscores; up to 40 digits),
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
            // Formerly `{n}L`; Long literals are now rejected (E7004). Kept as a branch so the
            // generator's draws are unchanged.
            7 => {
                let n = self.rng.range(0, 99);
                (n.to_string(), BigInt::from(n))
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
        let pieces = if self.web {
            WEB_STRING_PIECES
        } else {
            STRING_PIECES
        };
        for _ in 0..self.rng.range(0, 4) {
            let (piece, piece_value) = *self.rng.pick(pieces);
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
            let local_ty = *self.rng.pick(self.value_types());
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
        if self.web && self.rng.chance(20) {
            return self.web_int(d, scope);
        }
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
        if self.web && self.rng.chance(15) {
            let ty = *self.rng.pick(&[Ty::Double, Ty::Float]);
            let op = *self.rng.pick(&["<", "<=", ">", ">=", "==", "!="]);
            self.count(if ty == Ty::Double {
                "Double comparison"
            } else {
                "Float comparison"
            });
            return format!(
                "({} {op} {})",
                self.expr(ty, d, scope),
                self.expr(ty, d, scope)
            );
        }
        if self.web && self.rng.chance(20) {
            return self.web_bool(d, scope);
        }
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
        if self.web && self.rng.chance(20) {
            return self.web_string(d, scope);
        }
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
        if self.web && self.rng.chance(25) {
            return self.web_list(d, scope);
        }
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
        if self.web && self.rng.chance(20) {
            return self.web_opt_int(d, scope);
        }
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

    fn double_literal(&mut self) -> String {
        if self.rng.chance(15) {
            self.count("Double non-finite");
            return self
                .rng
                .pick(&["(0.0 / 0.0)", "(1e308 * 10.0)", "(-(1e308 * 10.0))"])
                .to_string();
        }
        self.count("Double literal");
        let literal = *self.rng.pick(DOUBLE_LITERALS);
        if self.rng.chance(20) {
            format!("(-{literal})")
        } else {
            literal.to_string()
        }
    }

    fn float_literal(&mut self) -> String {
        if self.rng.chance(15) {
            self.count("Float non-finite");
            return self
                .rng
                .pick(&["(0.0f / 0.0f)", "(3.4e38f * 10.0f)", "(-(3.4e38f * 10.0f))"])
                .to_string();
        }
        self.count("Float literal");
        let literal = *self.rng.pick(FLOAT_LITERALS);
        if self.rng.chance(20) {
            format!("(-{literal})")
        } else {
            literal.to_string()
        }
    }

    /// `Double`/`Float` arithmetic (`+ - * /`, negation) over finite literals and non-finite
    /// leaves (`0.0 / 0.0`, an overflowing product and its negation), so `NaN` and `Infinity`
    /// flow through arithmetic, comparisons, `if`, `when`, `fold`, `Text` and `Float` rounding.
    fn number(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        let double = ty == Ty::Double;
        match self.rng.below(12) {
            0..=2 => self.leaf(ty),
            3..=6 => {
                let op = *self.rng.pick(&["+", "-", "*", "/"]);
                self.count(match (double, op) {
                    (true, "+") => "Double +",
                    (true, "-") => "Double -",
                    (true, "*") => "Double *",
                    (true, _) => "Double /",
                    (false, "+") => "Float +",
                    (false, "-") => "Float -",
                    (false, "*") => "Float *",
                    (false, _) => "Float /",
                });
                format!(
                    "({} {op} {})",
                    self.expr(ty, d, scope),
                    self.expr(ty, d, scope)
                )
            }
            7 => {
                self.count(if double {
                    "Double unary -"
                } else {
                    "Float unary -"
                });
                format!("(-{})", self.expr(ty, d, scope))
            }
            8 => self.if_expr(ty, d, scope),
            9 => self.when_forma(ty, d, scope),
            10 => self.fold(ty, d, scope),
            _ => self.when_int(ty, d, scope),
        }
    }

    fn double(&mut self, d: u32, scope: &Scope) -> String {
        self.number(Ty::Double, d, scope)
    }

    fn float(&mut self, d: u32, scope: &Scope) -> String {
        self.number(Ty::Float, d, scope)
    }

    /// A `Ui` expression: a `UI { … }` fragment, `if`/`when`/`fold` with `Ui` results, or a
    /// call of a prelude component (`cartao`, `desenho`).
    fn ui(&mut self, d: u32, scope: &Scope) -> String {
        match self.rng.below(10) {
            0..=3 => {
                self.count("UI fragment");
                format!("UI {}", self.ui_block(d, scope))
            }
            4 => self.if_expr(Ty::Ui, d, scope),
            5 => self.when_forma(Ty::Ui, d, scope),
            6 => {
                if self.rng.chance(50) {
                    self.when_int(Ty::Ui, d, scope)
                } else {
                    self.when_opt_int(Ty::Ui, d, scope)
                }
            }
            7 => self.fold(Ty::Ui, d, scope),
            8 => {
                self.count("component call");
                format!(
                    "cartao({}, {})",
                    self.expr(Ty::Str, d.min(1), scope),
                    self.expr(Ty::Int, d.min(1), scope)
                )
            }
            _ => {
                self.count("component call");
                format!("desenho({})", self.expr(Ty::Forma, d, scope))
            }
        }
    }

    /// A child block: children and `val`s, one per line.
    fn ui_block(&mut self, d: u32, scope: &Scope) -> String {
        let mut inner = scope.clone();
        let mut lines = Vec::new();
        for _ in 0..self.rng.range(0, 4) {
            if d > 0 && self.rng.chance(20) {
                self.count("val among children");
                let local_ty = *self.rng.pick(self.value_types());
                let local = self.fresh();
                let value = self.expr(local_ty, d - 1, &inner);
                lines.push(format!("val {local}: {} = {value}", local_ty.bind()));
                inner.push((local, local_ty));
            } else {
                let child = self.ui_child(d, &inner);
                lines.push(child);
            }
        }
        if lines.is_empty() {
            return "{ }".into();
        }
        let body = lines.join("\n").replace('\n', "\n    ");
        format!("{{\n    {body}\n}}")
    }

    /// Optional children of an element: none, or a child block.
    fn ui_children(&mut self, d: u32, scope: &Scope) -> String {
        if d > 0 && self.rng.chance(60) {
            format!(" {}", self.ui_block(d - 1, scope))
        } else {
            String::new()
        }
    }

    /// A node argument: a `String` or an `Int` expression.
    fn ui_arg(&mut self, d: u32, scope: &Scope) -> String {
        if self.rng.chance(75) {
            self.expr(Ty::Str, d.min(1), scope)
        } else {
            self.count("Int node argument");
            self.expr(Ty::Int, d.min(1), scope)
        }
    }

    fn ui_child(&mut self, d: u32, scope: &Scope) -> String {
        let small = d.min(2);
        match self.rng.below(15) {
            0 => {
                self.count("Text of String");
                format!("Text({})", self.expr(Ty::Str, small, scope))
            }
            1 => {
                self.count("Text of Int");
                format!("Text({})", self.expr(Ty::Int, small, scope))
            }
            2 => {
                self.count("Text of Boolean");
                format!("Text({})", self.expr(Ty::Bool, small, scope))
            }
            3 => {
                self.count("Text of Double");
                format!("Text({})", self.expr(Ty::Double, small, scope))
            }
            4 => {
                self.count("Text of Float");
                format!("Text({})", self.expr(Ty::Float, small, scope))
            }
            5 => {
                self.count("Text of distinct");
                format!("Text(Codigo({}))", self.expr(Ty::Int, small, scope))
            }
            6 | 7 => {
                self.count("container node");
                let name = *self.rng.pick(CONTAINERS);
                let args: Vec<String> = (0..self.rng.range(0, 3))
                    .map(|_| self.ui_arg(d, scope))
                    .collect();
                let children = self.ui_children(d, scope);
                if args.is_empty() && !children.is_empty() && self.rng.chance(50) {
                    format!("{name}{children}")
                } else {
                    format!("{name}({}){children}", args.join(", "))
                }
            }
            8 => {
                self.count("Heading");
                let mut args = vec![self.expr(Ty::Int, d.min(1), scope)];
                if self.rng.chance(60) {
                    args.push(self.ui_arg(d, scope));
                }
                let children = self.ui_children(d, scope);
                format!("Heading({}){children}", args.join(", "))
            }
            9 => {
                self.count("Link");
                let href = if self.rng.chance(70) {
                    let href = *self.rng.pick(HREFS);
                    format!("\"{href}\"")
                } else {
                    self.expr(Ty::Str, d.min(1), scope)
                };
                let mut args = vec![href];
                for _ in 0..self.rng.range(0, 3) {
                    if self.rng.chance(40) {
                        args.push(self.rng.pick(&["\"_blank\"", "\"_self\""]).to_string());
                    } else {
                        args.push(self.ui_arg(d, scope));
                    }
                }
                let children = self.ui_children(d, scope);
                format!("Link({}){children}", args.join(", "))
            }
            10 => {
                self.count("Rule");
                if self.rng.chance(70) {
                    format!("Rule({})", self.ui_arg(d, scope))
                } else {
                    "Rule()".into()
                }
            }
            11 if d > 0 => {
                self.count("UI if/when child");
                format!(
                    "if ({}) {} else {}",
                    self.expr(Ty::Bool, d - 1, scope),
                    self.ui_child(d - 1, scope),
                    self.ui_child(d - 1, scope)
                )
            }
            12 if d > 0 => {
                self.count("UI if/when child");
                if self.rng.chance(50) {
                    self.when_forma(Ty::Ui, d - 1, scope)
                } else {
                    self.when_opt_int(Ty::Ui, d - 1, scope)
                }
            }
            _ => self.expr(Ty::Ui, d, scope),
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

    fn count_caixa(&mut self, ty: Ty) {
        self.count(if ty == Ty::CaixaInt {
            "generic record at Int"
        } else {
            "generic record at Set<Cor>"
        });
    }

    /// `escolhe(c, a, b)`: the prelude's generic function, at the type `ty`.
    fn generic_choice(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        self.count("generic function");
        format!(
            "escolhe({}, {}, {})",
            self.expr(Ty::Bool, d, scope),
            self.expr(ty, d, scope),
            self.expr(ty, d, scope)
        )
    }

    /// A `Set` expression whose own subterms fix its element type (`emptySet()` alone would
    /// stay ambiguous where nothing else fixes it, e.g. `setSize(emptySet())`).
    fn anchored_set(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        if self.rng.chance(20)
            && let Some(name) = self.var(ty, scope)
        {
            self.count("variable");
            return name;
        }
        let element = set_element(ty);
        match self.rng.below(4) {
            0 => {
                self.count("insert");
                format!(
                    "insert({}, {})",
                    self.expr(ty, d, scope),
                    self.expr(element, d, scope)
                )
            }
            1 => {
                self.count("remove");
                format!(
                    "remove({}, {})",
                    self.expr(ty, d, scope),
                    self.expr(element, d, scope)
                )
            }
            _ => {
                self.count("setOf");
                let items: Vec<String> = (0..self.rng.range(1, 4))
                    .map(|_| self.expr(element, d, scope))
                    .collect();
                format!("setOf({})", items.join(", "))
            }
        }
    }

    /// A `Map` expression whose own subterms fix its key and value types.
    fn anchored_map(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        if self.rng.chance(20)
            && let Some(name) = self.var(ty, scope)
        {
            self.count("variable");
            return name;
        }
        let (key, value) = map_entry(ty);
        match self.rng.below(4) {
            0 => {
                self.count("put");
                format!(
                    "put({}, {}, {})",
                    self.expr(ty, d, scope),
                    self.expr(key, d, scope),
                    self.expr(value, d, scope)
                )
            }
            1 if d > 0 => {
                self.count("removeKey");
                format!(
                    "removeKey({}, {})",
                    self.anchored_map(ty, d - 1, scope),
                    self.expr(key, d - 1, scope)
                )
            }
            _ => {
                self.count("emptyMap");
                self.count("put");
                format!(
                    "put(emptyMap(), {}, {})",
                    self.expr(key, d, scope),
                    self.expr(value, d, scope)
                )
            }
        }
    }

    /// `Int` from the collections and rich types.
    fn web_int(&mut self, d: u32, scope: &Scope) -> String {
        match self.rng.below(12) {
            0 => {
                self.count("setSize");
                let ty = *self.rng.pick(&[Ty::SetInt, Ty::SetStr, Ty::SetCor]);
                format!("setSize({})", self.anchored_set(ty, d, scope))
            }
            1 => {
                self.count("mapSize");
                let ty = *self.rng.pick(&[Ty::MapIntStr, Ty::MapCorInt]);
                format!("mapSize({})", self.anchored_map(ty, d, scope))
            }
            2 => {
                self.count("lookup");
                format!(
                    "(lookup({}, {}) ?: {})",
                    self.expr(Ty::MapCorInt, d, scope),
                    self.expr(Ty::Cor, d, scope),
                    self.expr(Ty::Int, d, scope)
                )
            }
            3 => {
                self.count("setSize as value");
                format!("usaMedeSet(setSize, {})", self.expr(Ty::SetStr, d, scope))
            }
            4 => {
                self.count("mapSize as value");
                format!(
                    "usaMedeMapa(mapSize, {})",
                    self.expr(Ty::MapCorInt, d, scope)
                )
            }
            5 => {
                self.count("toList");
                format!("ordem(toList({}))", self.anchored_set(Ty::SetCor, d, scope))
            }
            6 => {
                self.count("keys");
                format!(
                    "ordem(keys({}))",
                    self.anchored_map(Ty::MapCorInt, d, scope)
                )
            }
            7 => {
                self.count("generic record field");
                format!("({}).valor", self.expr(Ty::CaixaInt, d, scope))
            }
            8 => self.when_cor(Ty::Int, d, scope),
            9 => self.when_evento(Ty::Int, d, scope),
            10 => self.when_res(Ty::Int, d, scope),
            _ => {
                self.count("enum argument");
                format!("indice({})", self.expr(Ty::Cor, d, scope))
            }
        }
    }

    /// `Boolean` from the collections and rich types.
    fn web_bool(&mut self, d: u32, scope: &Scope) -> String {
        match self.rng.below(10) {
            0..=2 => {
                self.count("contains");
                let ty = *self.rng.pick(&[Ty::SetInt, Ty::SetStr, Ty::SetCor]);
                format!(
                    "contains({}, {})",
                    self.expr(ty, d, scope),
                    self.expr(set_element(ty), d, scope)
                )
            }
            3 => {
                self.count("contains as value");
                format!(
                    "usaTem(contains, {}, {})",
                    self.expr(Ty::SetInt, d, scope),
                    self.expr(Ty::Int, d, scope)
                )
            }
            4 => {
                self.count("set equality");
                let ty = *self.rng.pick(&[Ty::SetInt, Ty::SetStr, Ty::SetCor]);
                let op = *self.rng.pick(&["==", "!="]);
                format!(
                    "({} {op} {})",
                    self.anchored_set(ty, d, scope),
                    self.expr(ty, d, scope)
                )
            }
            5 => {
                self.count("map equality");
                let ty = *self.rng.pick(&[Ty::MapIntStr, Ty::MapCorInt]);
                let op = *self.rng.pick(&["==", "!="]);
                format!(
                    "({} {op} {})",
                    self.anchored_map(ty, d, scope),
                    self.expr(ty, d, scope)
                )
            }
            6 => {
                self.count("enum equality");
                let op = *self.rng.pick(&["==", "!="]);
                format!(
                    "({} {op} {})",
                    self.expr(Ty::Cor, d, scope),
                    self.expr(Ty::Cor, d, scope)
                )
            }
            7 => self.when_cor(Ty::Bool, d, scope),
            8 => self.when_evento(Ty::Bool, d, scope),
            _ => self.when_res(Ty::Bool, d, scope),
        }
    }

    /// `String` from the collections and rich types.
    fn web_string(&mut self, d: u32, scope: &Scope) -> String {
        match self.rng.below(7) {
            0 | 1 => {
                self.count("lookup");
                format!(
                    "(lookup({}, {}) ?: {})",
                    self.expr(Ty::MapIntStr, d, scope),
                    self.expr(Ty::Int, d, scope),
                    self.expr(Ty::Str, d, scope)
                )
            }
            2 => {
                self.count("lookup as value");
                format!(
                    "(usaBusca(lookup, {}, {}) ?: {})",
                    self.expr(Ty::MapIntStr, d, scope),
                    self.expr(Ty::Int, d, scope),
                    self.expr(Ty::Str, d, scope)
                )
            }
            3 => {
                self.count("generic record field");
                if self.rng.chance(50) {
                    format!("({}).rotulo", self.expr(Ty::CaixaInt, d, scope))
                } else {
                    // The content fixes the instance (`Caixa(emptySet(), …)` would not).
                    self.count("generic record at Set<Cor>");
                    format!(
                        "Caixa({}, {}).rotulo",
                        self.anchored_set(Ty::SetCor, d, scope),
                        self.expr(Ty::Str, d.min(1), scope)
                    )
                }
            }
            4 => self.when_cor(Ty::Str, d, scope),
            5 => self.when_evento(Ty::Str, d, scope),
            _ => self.when_res(Ty::Str, d, scope),
        }
    }

    /// `List<Int>` from the collections.
    fn web_list(&mut self, d: u32, scope: &Scope) -> String {
        match self.rng.below(7) {
            0 => {
                self.count("toList");
                format!("toList({})", self.anchored_set(Ty::SetInt, d, scope))
            }
            1 => {
                self.count("keys");
                format!("keys({})", self.anchored_map(Ty::MapIntStr, d, scope))
            }
            2 => {
                self.count("values");
                format!("values({})", self.anchored_map(Ty::MapCorInt, d, scope))
            }
            3 => {
                self.count("toList as value");
                format!("usaListaSet(toList, {})", self.expr(Ty::SetInt, d, scope))
            }
            4 => {
                self.count("keys as value");
                format!("usaChaves(keys, {})", self.expr(Ty::MapIntStr, d, scope))
            }
            5 => {
                self.count("values as value");
                format!("usaValores(values, {})", self.expr(Ty::MapCorInt, d, scope))
            }
            _ => {
                self.count("setSize as value");
                format!(
                    "map(listOf({}, {}), setSize)",
                    self.anchored_set(Ty::SetInt, d, scope),
                    self.expr(Ty::SetInt, d, scope)
                )
            }
        }
    }

    /// `Int?` from the collections.
    fn web_opt_int(&mut self, d: u32, scope: &Scope) -> String {
        if self.rng.chance(70) {
            self.count("lookup");
            format!(
                "lookup({}, {})",
                self.expr(Ty::MapCorInt, d, scope),
                self.expr(Ty::Cor, d, scope)
            )
        } else {
            self.count("toList");
            format!("first(toList({}))", self.anchored_set(Ty::SetInt, d, scope))
        }
    }

    fn cor(&mut self, d: u32, scope: &Scope) -> String {
        match self.rng.below(7) {
            0 | 1 => self.leaf(Ty::Cor),
            2 => self.if_expr(Ty::Cor, d, scope),
            3 => self.when_cor(Ty::Cor, d, scope),
            4 => self.when_evento(Ty::Cor, d, scope),
            5 => {
                self.count("keys");
                format!(
                    "(first(keys({})) ?: {})",
                    self.anchored_map(Ty::MapCorInt, d, scope),
                    self.expr(Ty::Cor, d, scope)
                )
            }
            _ => self.generic_choice(Ty::Cor, d, scope),
        }
    }

    /// `Set<Int>`, `Set<String>` or `Set<Cor>`.
    fn set(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        let element = set_element(ty);
        match self.rng.below(13) {
            0 => self.leaf(ty),
            1 | 2 => {
                self.count("setOf");
                let items: Vec<String> = (0..self.rng.range(1, 4))
                    .map(|_| self.expr(element, d, scope))
                    .collect();
                format!("setOf({})", items.join(", "))
            }
            3 | 4 => {
                self.count("insert");
                format!(
                    "insert({}, {})",
                    self.expr(ty, d, scope),
                    self.expr(element, d, scope)
                )
            }
            5 => {
                self.count("remove");
                format!(
                    "remove({}, {})",
                    self.expr(ty, d, scope),
                    self.expr(element, d, scope)
                )
            }
            6 => self.if_expr(ty, d, scope),
            7 => self.fold(ty, d, scope),
            8 => self.generic_choice(ty, d, scope),
            9 => {
                if self.rng.chance(50) {
                    self.when_cor(ty, d, scope)
                } else {
                    self.when_res(ty, d, scope)
                }
            }
            10 => self.when_evento(ty, d, scope),
            11 if ty == Ty::SetInt => match self.rng.below(3) {
                0 => {
                    let name = *self.rng.pick(&["insert", "remove"]);
                    self.count("builtin as fold function");
                    self.count(if name == "insert" {
                        "insert as value"
                    } else {
                        "remove as value"
                    });
                    format!(
                        "fold({}, {}, {name})",
                        self.anchored_list(d, scope),
                        self.expr(Ty::SetInt, d, scope)
                    )
                }
                1 => {
                    let name = *self.rng.pick(&["insert", "remove"]);
                    self.count(if name == "insert" {
                        "insert as value"
                    } else {
                        "remove as value"
                    });
                    format!(
                        "usaSet({name}, {}, {})",
                        self.expr(Ty::SetInt, d, scope),
                        self.expr(Ty::Int, d, scope)
                    )
                }
                _ => {
                    self.count("emptySet as value");
                    "usaVazio(emptySet)".into()
                }
            },
            11 if ty == Ty::SetCor => {
                self.count("generic record field");
                format!("({}).valor", self.expr(Ty::CaixaCores, d, scope))
            }
            _ => self.leaf(ty),
        }
    }

    /// `Map<Int, String>` or `Map<Cor, Int>`.
    fn map(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        let (key, value) = map_entry(ty);
        match self.rng.below(12) {
            0 => self.leaf(ty),
            1..=3 => {
                self.count("put");
                format!(
                    "put({}, {}, {})",
                    self.expr(ty, d, scope),
                    self.expr(key, d, scope),
                    self.expr(value, d, scope)
                )
            }
            4 => {
                self.count("removeKey");
                format!(
                    "removeKey({}, {})",
                    self.expr(ty, d, scope),
                    self.expr(key, d, scope)
                )
            }
            5 => self.if_expr(ty, d, scope),
            6 => self.fold(ty, d, scope),
            7 => self.generic_choice(ty, d, scope),
            8 => {
                if self.rng.chance(50) {
                    self.when_cor(ty, d, scope)
                } else {
                    self.when_res(ty, d, scope)
                }
            }
            9 => self.when_evento(ty, d, scope),
            10 if ty == Ty::MapIntStr => {
                if self.rng.chance(60) {
                    self.count("removeKey as value");
                    format!(
                        "usaTira(removeKey, {}, {})",
                        self.expr(Ty::MapIntStr, d, scope),
                        self.expr(Ty::Int, d, scope)
                    )
                } else {
                    self.count("emptyMap as value");
                    "usaMapaVazio(emptyMap)".into()
                }
            }
            10 => {
                self.count("put as value");
                format!(
                    "usaPoe(put, {}, {}, {})",
                    self.expr(Ty::MapCorInt, d, scope),
                    self.expr(Ty::Cor, d, scope),
                    self.expr(Ty::Int, d, scope)
                )
            }
            _ => self.leaf(ty),
        }
    }

    fn evento(&mut self, d: u32, scope: &Scope) -> String {
        match self.rng.below(9) {
            0 => self.leaf(Ty::Evento),
            1 | 2 => {
                self.count("sealed with fields constructor");
                format!(
                    "Marcado({}, {})",
                    self.expr(Ty::Cor, d, scope),
                    self.expr(Ty::Int, d, scope)
                )
            }
            3 | 4 => {
                self.count("sealed with fields constructor");
                format!(
                    "Agrupado({}, {})",
                    self.expr(Ty::SetInt, d, scope),
                    self.expr(Ty::MapIntStr, d, scope)
                )
            }
            5 => {
                self.count("sealed with fields constructor");
                format!("Embrulhado({})", self.expr(Ty::CaixaInt, d, scope))
            }
            6 => {
                self.count("sealed with fields constructor");
                "Nada".into()
            }
            7 => self.if_expr(Ty::Evento, d, scope),
            _ => {
                if self.rng.chance(50) {
                    self.when_res(Ty::Evento, d, scope)
                } else {
                    self.generic_choice(Ty::Evento, d, scope)
                }
            }
        }
    }

    /// `Caixa<Int>` or `Caixa<Set<Cor>>`.
    fn caixa(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        let content = caixa_content(ty);
        match self.rng.below(7) {
            0 => self.leaf(ty),
            1 | 2 => {
                self.count_caixa(ty);
                format!(
                    "Caixa({}, {})",
                    self.expr(content, d, scope),
                    self.expr(Ty::Str, d.min(1), scope)
                )
            }
            3 => {
                self.count_caixa(ty);
                self.count("generic function");
                format!(
                    "embrulha({}, {})",
                    self.expr(content, d, scope),
                    self.expr(Ty::Str, d.min(1), scope)
                )
            }
            4 => self.if_expr(ty, d, scope),
            5 => self.when_evento(ty, d, scope),
            _ => self.generic_choice(ty, d, scope),
        }
    }

    fn res_int(&mut self, d: u32, scope: &Scope) -> String {
        match self.rng.below(7) {
            0 => self.leaf(Ty::ResInt),
            1 | 2 => {
                self.count("Ok");
                format!("Ok({})", self.expr(Ty::Int, d, scope))
            }
            3 => {
                self.count("Err");
                format!("Err({})", self.expr(Ty::Str, d, scope))
            }
            4 => self.if_expr(Ty::ResInt, d, scope),
            5 => self.when_cor(Ty::ResInt, d, scope),
            _ => self.generic_choice(Ty::ResInt, d, scope),
        }
    }

    /// A `Result<Int, String>` expression whose own subterms fix both of its types (`Ok(1)`
    /// alone leaves the error type open where nothing else fixes it, e.g. as a `when` subject).
    fn anchored_res(&mut self, d: u32, scope: &Scope) -> String {
        if self.rng.chance(20)
            && let Some(name) = self.var(Ty::ResInt, scope)
        {
            self.count("variable");
            return name;
        }
        if self.rng.chance(50) {
            self.count("named function call");
            format!(
                "divide({}, {})",
                self.expr(Ty::Int, d, scope),
                self.expr(Ty::Int, d.min(1), scope)
            )
        } else {
            self.count("named function call");
            format!("confere({})", self.expr(Ty::ResInt, d, scope))
        }
    }

    /// A sub-pattern of type `ty`: a fresh variable or `_`.
    fn bind_subpattern(&mut self, ty: Ty, bound: &mut Vec<(String, Ty)>) -> String {
        if self.rng.chance(60) {
            let name = self.fresh();
            bound.push((name.clone(), ty));
            name
        } else {
            "_".into()
        }
    }

    /// A final `_` or variable arm of type `ty` (the variable bound to `scrutinee`).
    fn catch_all_arm(&mut self, ty: Ty, scrutinee: Ty, d: u32, scope: &Scope) -> (String, String) {
        let mut bound = Vec::new();
        let pattern = self.bind_subpattern(scrutinee, &mut bound);
        let body = self.arm(ty, d, scope, bound);
        (pattern, body)
    }

    /// `when` over a `Cor`: the variants in random order, each maybe left out, then `_` or a
    /// variable unless all three are there.
    fn when_cor(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        self.count("when over enum");
        let scrutinee = self.expr(Ty::Cor, d, scope);
        let mut order = vec!["Z", "A", "M"];
        for i in (1..order.len()).rev() {
            order.swap(i, self.rng.below(i + 1));
        }
        let mut arms = Vec::new();
        let mut complete = true;
        for variant in order {
            if self.rng.chance(75) {
                let body = self.arm(ty, d, scope, vec![]);
                arms.push((variant.to_string(), body));
            } else {
                complete = false;
            }
        }
        if !complete {
            arms.push(self.catch_all_arm(ty, Ty::Cor, d, scope));
        }
        self.when(scrutinee, arms)
    }

    /// `when` over an `Evento` (a sealed class whose variants hold an enum, a `Set`, a `Map`
    /// and a generic record): per variant in random order maybe one refutable arm (`Marcado`
    /// with a variant or a literal inside), then maybe an irrefutable one; `_` or a variable
    /// last unless every variant has an irrefutable arm.
    fn when_evento(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        self.count("when over sealed with fields");
        let scrutinee = self.expr(Ty::Evento, d, scope);
        let mut order = vec![0, 1, 2, 3];
        for i in (1..order.len()).rev() {
            order.swap(i, self.rng.below(i + 1));
        }
        let mut arms = Vec::new();
        let mut complete = true;
        for variant in order {
            if variant == 0 && self.rng.chance(35) {
                let mut bound = Vec::new();
                let pattern = if self.rng.chance(50) {
                    self.count("nested enum pattern");
                    let cor = *self.rng.pick(&["Z", "A", "M"]);
                    format!(
                        "Marcado({cor}, {})",
                        self.bind_subpattern(Ty::Int, &mut bound)
                    )
                } else {
                    let cor = self.bind_subpattern(Ty::Cor, &mut bound);
                    format!("Marcado({cor}, {})", self.int_literal_text().0)
                };
                let body = self.arm(ty, d, scope, bound);
                arms.push((pattern, body));
            }
            if self.rng.chance(70) {
                let mut bound = Vec::new();
                let pattern = match variant {
                    0 => {
                        let cor = self.bind_subpattern(Ty::Cor, &mut bound);
                        let peso = self.bind_subpattern(Ty::Int, &mut bound);
                        format!("Marcado({cor}, {peso})")
                    }
                    1 => {
                        let itens = self.bind_subpattern(Ty::SetInt, &mut bound);
                        let nomes = self.bind_subpattern(Ty::MapIntStr, &mut bound);
                        format!("Agrupado({itens}, {nomes})")
                    }
                    2 => {
                        let caixa = self.bind_subpattern(Ty::CaixaInt, &mut bound);
                        format!("Embrulhado({caixa})")
                    }
                    _ => "Nada".into(),
                };
                let body = self.arm(ty, d, scope, bound);
                arms.push((pattern, body));
            } else {
                complete = false;
            }
        }
        if !complete {
            arms.push(self.catch_all_arm(ty, Ty::Evento, d, scope));
        }
        self.when(scrutinee, arms)
    }

    /// `when` over a `Result<Int, String>`: maybe `Ok(<literal>)` first, then `Ok` and `Err`
    /// in random order (each maybe left out), then `_` or a variable if one is missing.
    fn when_res(&mut self, ty: Ty, d: u32, scope: &Scope) -> String {
        self.count("when over Result");
        let scrutinee = self.anchored_res(d, scope);
        let mut arms = Vec::new();
        if self.rng.chance(35) {
            self.count("Result literal pattern");
            let pattern = format!("Ok({})", self.int_literal_text().0);
            let body = self.arm(ty, d, scope, vec![]);
            arms.push((pattern, body));
        }
        let mut variants = vec![("Ok", Ty::Int), ("Err", Ty::Str)];
        if self.rng.chance(50) {
            variants.reverse();
        }
        let mut complete = true;
        for (variant, field) in variants {
            if self.rng.chance(80) {
                let mut bound = Vec::new();
                let pattern = format!("{variant}({})", self.bind_subpattern(field, &mut bound));
                let body = self.arm(ty, d, scope, bound);
                arms.push((pattern, body));
            } else {
                complete = false;
            }
        }
        if !complete {
            arms.push(self.catch_all_arm(ty, Ty::ResInt, d, scope));
        }
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

/// The element type of a `Set` type.
fn set_element(ty: Ty) -> Ty {
    match ty {
        Ty::SetStr => Ty::Str,
        Ty::SetCor => Ty::Cor,
        _ => Ty::Int,
    }
}

/// The key and value types of a `Map` type.
fn map_entry(ty: Ty) -> (Ty, Ty) {
    match ty {
        Ty::MapCorInt => (Ty::Cor, Ty::Int),
        _ => (Ty::Int, Ty::Str),
    }
}

/// The content type of a `Caixa` instance.
fn caixa_content(ty: Ty) -> Ty {
    match ty {
        Ty::CaixaCores => Ty::SetCor,
        _ => Ty::Int,
    }
}

/// (name, result type, Bind source of the function) of each program.
pub(crate) type Programs = Vec<(String, Ty, String)>;

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

/// Seed of the three-way test's generator.
const WEB_SEED: u64 = 0x00B1_4D00_3E4B_2026;

/// The three-way test's declarations, after [`PRELUDE`]: a distinct type over `Int` and two
/// SSR-style components built from helpers, with `val` children and `if`/`when` in the tree.
pub(crate) const WEB_PRELUDE: &str = "
distinct type Codigo = Int

fun cartao(titulo: String, n: Int): Ui = UI {
    Article(\"cartao\", titulo) {
        val rotulo = \"#\" + titulo
        Heading(n, \"titulo\") { Text(rotulo) }
        if (n > 2) Strong { Text(n) } else Rule(\"sep\")
        when (n) {
            0 -> Text(\"zero\")
            1 -> Small { Text(0.1 + 0.2) }
            _ -> Text(Codigo(n * n))
        }
    }
}

fun desenho(f: Forma): Ui = UI {
    when (f) {
        Circulo(r) -> Span(\"circulo\") { Text(r) }
        Retangulo(l, a) -> Div { Text(l); Text(\"x\"); Text(a) }
        Rotulada(nome, dentro) -> Section(nome) {
            val sub = desenho(dentro)
            sub
        }
        Vazia -> Text(1e21)
    }
}

enum class Cor { Z, A, M }

distinct type Nota = Decimal

data class Caixa<T>(val valor: T, val rotulo: String)

sealed class Evento {
    data class Marcado(val cor: Cor, val peso: Int)
    data class Agrupado(val itens: Set<Int>, val nomes: Map<Int, String>)
    data class Embrulhado(val caixa: Caixa<Int>)
    data object Nada
}

fun indice(c: Cor): Int = when (c) {
    Z -> 1
    A -> 2
    M -> 3
}

fun ordem(cs: List<Cor>): Int = fold(cs, 0, { acc, c -> acc * 10 + indice(c) })

fun divide(a: Int, b: Int): Result<Int, String> = when (a / b) {
    null -> Err(\"divisão por zero\")
    q -> Ok(q)
}

fun confere(r: Result<Int, String>): Result<Int, String> = r

fun <T> escolhe(c: Boolean, a: T, b: T): T = if (c) a else b

fun <T> embrulha(x: T, r: String): Caixa<T> = Caixa(x, r)

fun usaSet(g: (Set<Int>, Int) -> Set<Int>, s: Set<Int>, x: Int): Set<Int> = g(s, x)

fun usaTem(g: (Set<Int>, Int) -> Boolean, s: Set<Int>, x: Int): Boolean = g(s, x)

fun usaMedeSet(g: (Set<String>) -> Int, s: Set<String>): Int = g(s)

fun usaListaSet(g: (Set<Int>) -> List<Int>, s: Set<Int>): List<Int> = g(s)

fun usaVazio(g: () -> Set<Int>): Set<Int> = g()

fun usaPoe(g: (Map<Cor, Int>, Cor, Int) -> Map<Cor, Int>, m: Map<Cor, Int>, c: Cor, n: Int): Map<Cor, Int> = g(m, c, n)

fun usaBusca(g: (Map<Int, String>, Int) -> String?, m: Map<Int, String>, k: Int): String? = g(m, k)

fun usaTira(g: (Map<Int, String>, Int) -> Map<Int, String>, m: Map<Int, String>, k: Int): Map<Int, String> = g(m, k)

fun usaChaves(g: (Map<Int, String>) -> List<Int>, m: Map<Int, String>): List<Int> = g(m)

fun usaValores(g: (Map<Cor, Int>) -> List<Int>, m: Map<Cor, Int>): List<Int> = g(m)

fun usaMedeMapa(g: (Map<Cor, Int>) -> Int, m: Map<Cor, Int>): Int = g(m)

fun usaMapaVazio(g: () -> Map<Int, String>): Map<Int, String> = g()

fun profundo(n: Int): Ui = if (n == 0) UI { Text(\"fim\") } else UI { Div { Div { Div { Div { Div { Div { Div { Div { Div { Div { Div { Div { Div { Div { Div { Div { Div { Div { Div { Div { profundo(n - 1) } } } } } } } } } } } } } } } } } } } } }
";

/// The three-way test's fixed programs: the recursion limit (in plain code and inside a UI
/// tree), an SSR-style component, non-finite `Double`/`Float` results and a tree about
/// 200,000 elements deep.
const WEB_FIXED: &[(Ty, &str)] = &[
    (Ty::Int, "fun {name}(): Int = conta(20000, 0)"),
    (Ty::Int, "fun {name}(): Int = conta(9998, 0)"),
    (Ty::Int, "fun {name}(): Int = conta(9999, 0)"),
    (Ty::Ui, "fun {name}(): Ui = UI { Text(conta(20000, 0)) }"),
    (
        Ty::Ui,
        "client component {name}() {
    UI {
        Main(\"app\", \"raiz\") {
            val total = conta(10, 0)
            Header { Heading(1) { Text(\"Painel <&> 'x'\") } }
            cartao(\"a & b\", total)
            cartao(\"<i>\", 1)
            cartao(\"\", 0)
            if (total > 50) desenho(Rotulada(\"r\", Retangulo(total, -total))) else Text(total)
            when (first(listOf(total))) {
                null -> Rule(\"nada\")
                n -> Paragraph { Text(Codigo(n)); Text(n > 3) }
            }
            Footer { Text(0.1f + 0.2f); Link(\"javascript:alert(1)\", \"c\", \"_blank\", \"rótulo\") { Text(\"x\") } }
        }
    }
}",
    ),
    (Ty::Double, "fun {name}(): Double = 1e308 * 10.0"),
    (Ty::Double, "fun {name}(): Double = (-1e308) * 10.0"),
    (Ty::Double, "fun {name}(): Double = 0.0 / 0.0"),
    (Ty::Double, "fun {name}(): Double = (1e308 * 10.0) - (1e308 * 10.0)"),
    (Ty::Double, "fun {name}(): Double = (-0.0) * 1.0"),
    (Ty::Float, "fun {name}(): Float = 3.4e38f * 10.0f"),
    (Ty::Float, "fun {name}(): Float = 0.0f / 0.0f"),
    (Ty::Float, "fun {name}(): Float = (-1.0f) / 0.0f"),
    (Ty::Bool, "fun {name}(): Boolean = (0.0 / 0.0) == (0.0 / 0.0)"),
    (Ty::Bool, "fun {name}(): Boolean = (0.0 / 0.0) != (0.0 / 0.0)"),
    (Ty::Bool, "fun {name}(): Boolean = (0.0f / 0.0f) < 1.0f"),
    (Ty::Bool, "fun {name}(): Boolean = (-0.0) == 0.0"),
    (
        Ty::Ui,
        "fun {name}(): Ui = UI { Text(0.0 / 0.0); Text(1e308 * 10.0); Text(-(1e308 * 10.0)); Text(-0.0); Text(3.4e38f * 10.0f); Text(0.0f / 0.0f) }",
    ),
    // A tree 199,800 elements deep (9,990 nested calls of 20 nested `Div`s): every renderer
    // and the drop of the tree are iterative.
    (Ty::Ui, "fun {name}(): Ui = profundo(9990)"),
];

/// Number of random programs of the three-way test (twice the server's: it has twice as many
/// result types).
pub(crate) const WEB_PROGRAMS: usize = 2 * PROGRAMS;

/// The three-way test's programs (`WEB_PROGRAMS` random ones, then [`WEB_FIXED`]) and how many
/// times each construct occurs in them.
pub(crate) fn web_programs() -> (Programs, BTreeMap<&'static str, usize>) {
    let mut generator = Gen::web(WEB_SEED);
    let mut out = Vec::new();
    for index in 0..WEB_PROGRAMS {
        let (ty, source) = generator.program(index);
        out.push((format!("p{index}"), ty, source));
    }
    for (offset, (ty, template)) in WEB_FIXED.iter().enumerate() {
        let name = format!("p{}", WEB_PROGRAMS + offset);
        let source = format!("{}\n", template.replace("{name}", &name));
        out.push((name, *ty, source));
    }
    (out, generator.counts)
}

/// The three-way test's functions of `Date`, `DateTime` and `Instant` parameters (Bind has no
/// literal of these types, so they are called with arguments, [`temporal_calls`]): every
/// comparison operator, the values shown in a UI tree with the result of `<`, and the values
/// as `Set` elements and `Map` keys (typed temporal order, never the text order).
pub(crate) const TEMPORAL_PRELUDE: &str = "
fun ordemDate(a: Date, b: Date): List<Boolean> = listOf(a < b, a <= b, a > b, a >= b, a == b, a != b)
fun ordemDateTime(a: DateTime, b: DateTime): List<Boolean> = listOf(a < b, a <= b, a > b, a >= b, a == b, a != b)
fun ordemInstant(a: Instant, b: Instant): List<Boolean> = listOf(a < b, a <= b, a > b, a >= b, a == b, a != b)
fun telaDate(a: Date, b: Date): Ui = UI { Span { Text(a); if (a < b) Text(\" < \") else Text(\" >= \"); Text(b) } }
fun telaDateTime(a: DateTime, b: DateTime): Ui = UI { Span { Text(a); if (a >= b) Text(\" >= \") else Text(\" < \"); Text(b) } }
fun telaInstant(a: Instant, b: Instant): Ui = UI { Span { Text(a); if (a > b) Text(\" > \") else Text(\" <= \"); Text(b) } }
fun conjuntoDate(a: Date, b: Date): Set<Date> = setOf(b, a, b)
fun conjuntoDateTime(a: DateTime, b: DateTime): Set<DateTime> = setOf(b, a, b)
fun conjuntoInstant(a: Instant, b: Instant): Set<Instant> = setOf(b, a, b)
fun mapaDate(a: Date, b: Date): Map<Date, Int> = put(put(put(emptyMap(), b, 1), a, 2), b, 3)
fun mapaDateTime(a: DateTime, b: DateTime): Map<DateTime, Int> = put(put(put(emptyMap(), b, 1), a, 2), b, 3)
fun mapaInstant(a: Instant, b: Instant): Map<Instant, Int> = put(put(put(emptyMap(), b, 1), a, 2), b, 3)
";

/// The temporal functions called on each pair, by name prefix (`ordem<T>` first).
const TEMPORAL_FUNCTIONS: [&str; 4] = ["ordem", "tela", "conjunto", "mapa"];

/// The three-way test's functions of two `Decimal` parameters (Bind has no `Decimal` literal,
/// so they are called with arguments, [`decimal_calls`]): every `Set`/`Map` builtin over
/// `Decimal` elements and keys (by value: `1.0` and `1.00` are one key, and the first text
/// stays, Ruling A), a distinct type over `Decimal` as a key, equality, and `Boolean` keys and
/// elements (`false < true`).
pub(crate) const DECIMAL_PRELUDE: &str = "
fun decConjunto(a: Decimal, b: Decimal): Set<Decimal> = insert(setOf(a), b)
fun decLista(a: Decimal, b: Decimal): List<Decimal> = toList(setOf(b, a, b))
fun decMapa(a: Decimal, b: Decimal): Map<Decimal, Int> = put(put(emptyMap(), a, 1), b, 2)
fun decNotasMapa(a: Decimal, b: Decimal): Map<Nota, Int> = put(put(emptyMap(), Nota(b), 1), Nota(a), 2)
fun decContagem(a: Decimal, b: Decimal): List<Int> = listOf(setSize(setOf(a, b)), mapSize(put(put(emptyMap(), a, \"x\"), b, \"y\")), lookup(put(emptyMap(), a, 7), b) ?: (-1), if (contains(setOf(a), b)) 1 else 0)
fun decRemove(a: Decimal, b: Decimal): Set<Decimal> = remove(setOf(a, b), b)
fun decChaves(a: Decimal, b: Decimal): List<Decimal> = keys(removeKey(put(put(emptyMap(), b, \"y\"), a, \"x\"), b))
fun decValores(a: Decimal, b: Decimal): List<String> = values(put(put(emptyMap(), b, \"b\"), a, \"a\"))
fun decNotas(a: Decimal, b: Decimal): Set<Nota> = setOf(Nota(b), Nota(a))
fun decIguais(a: Decimal, b: Decimal): List<Boolean> = listOf(a == b, setOf(a) == setOf(b), put(emptyMap(), a, 1) == put(emptyMap(), b, 1), Nota(a) == Nota(b))
fun decBooleanos(a: Decimal, b: Decimal): Map<Boolean, Set<Boolean>> = put(put(emptyMap(), b <= a, setOf(a == b)), a < b, setOf(a != b, a >= b))
";

/// The Decimal functions called on each pair.
const DECIMAL_FUNCTIONS: [&str; 11] = [
    "decConjunto",
    "decLista",
    "decMapa",
    "decNotasMapa",
    "decContagem",
    "decRemove",
    "decChaves",
    "decValores",
    "decNotas",
    "decIguais",
    "decBooleanos",
];

/// A call of a [`TEMPORAL_PRELUDE`] or [`DECIMAL_PRELUDE`] function on two canonical wire
/// texts, printed as the line `line` (`t<k>=<render>` or `d<k>=<render>`).
#[derive(Clone, Debug)]
pub(crate) struct Call {
    pub line: String,
    pub function: String,
    pub a: String,
    pub b: String,
    /// The arguments are `Decimal`s (else text values: dates).
    pub decimal: bool,
}

/// The canonical wire text (chrono's serde) of a date with signed `year` (`YYYY` for
/// 0000..=9999, otherwise sign and at least 4 digits), then for a date-time the time of day
/// `seconds` (`:60` for a leap second) and the fraction of `nanos` in 3, 6 or 9 digits (none
/// when zero), then `Z` for an instant.
fn temporal_text(ty: &str, year: i64, month: u32, day: u32, seconds: u32, nanos: u32) -> String {
    let mut text = if (0..=9999).contains(&year) {
        format!("{year:04}-{month:02}-{day:02}")
    } else {
        format!("{year:+05}-{month:02}-{day:02}")
    };
    if ty == "Date" {
        return text;
    }
    let (hour, minute, second) = if seconds >= 86_400 {
        (23, 59, 60)
    } else {
        (seconds / 3600, seconds / 60 % 60, seconds % 60)
    };
    let _ = write!(text, "T{hour:02}:{minute:02}:{second:02}");
    if nanos != 0 {
        if nanos.is_multiple_of(1_000_000) {
            let _ = write!(text, ".{:03}", nanos / 1_000_000);
        } else if nanos.is_multiple_of(1000) {
            let _ = write!(text, ".{:06}", nanos / 1000);
        } else {
            let _ = write!(text, ".{nanos:09}");
        }
    }
    if ty == "Instant" {
        text.push('Z');
    }
    text
}

/// The three-way test's temporal calls: every pair of `runtime/vectors/temporal-order.json`,
/// then random pairs of each type (extended and negative years, fractions of 0, 3, 6 or 9
/// digits, leap seconds; `b` often equal to `a` or one field away from it), each pair through
/// every function of [`TEMPORAL_FUNCTIONS`].
pub(crate) fn temporal_calls() -> Vec<Call> {
    let mut pairs: Vec<(String, String, String)> = Vec::new();
    let vectors = include_str!("../../../runtime/vectors/temporal-order.json");
    for line in vectors.lines().filter(|line| line.contains("\"type\"")) {
        // `{"type":"T","a":"…","b":"…","order":"…"}`: the texts hold no quotes or escapes.
        let fields: Vec<&str> = line.split('"').collect();
        pairs.push((
            fields[3].to_string(),
            fields[7].to_string(),
            fields[11].to_string(),
        ));
    }
    let mut rng = Rng(0x00B1_4D00_7E4F_2026);
    let years: &[i64] = &[
        2026, 2026, 1970, 0, -1, -2, -9999, -10000, 9999, 10000, 12345, 262_142,
    ];
    for ty in ["Date", "DateTime", "Instant"] {
        for _ in 0..60 {
            let random = |rng: &mut Rng| {
                let nanos = match rng.below(5) {
                    0 => 0,
                    1 => rng.below(1000) as u32 * 1_000_000,
                    2 => rng.below(1_000_000) as u32 * 1000,
                    3 => rng.below(1_000_000_000) as u32,
                    _ => 500_000_000,
                };
                let leap = rng.chance(5);
                (
                    *rng.pick(years),
                    1 + rng.below(12) as u32,
                    1 + rng.below(28) as u32,
                    if leap {
                        86_400
                    } else {
                        rng.below(86_400) as u32
                    },
                    nanos,
                )
            };
            let a = random(&mut rng);
            let mut b = a;
            match rng.below(7) {
                0 => {}
                1 => b.4 = random(&mut rng).4,
                2 => b.3 = random(&mut rng).3,
                3 => b.0 = random(&mut rng).0,
                4 => b.2 = random(&mut rng).2,
                5 => b.0 = -a.0,
                _ => b = random(&mut rng),
            }
            let text = |(year, month, day, seconds, nanos): (i64, u32, u32, u32, u32)| {
                temporal_text(ty, year, month, day, seconds, nanos)
            };
            pairs.push((ty.to_string(), text(a), text(b)));
        }
    }
    let mut calls = Vec::new();
    for (ty, a, b) in pairs {
        for prefix in TEMPORAL_FUNCTIONS {
            calls.push(Call {
                line: format!("t{}", calls.len()),
                function: format!("{prefix}{ty}"),
                a: a.clone(),
                b: b.clone(),
                decimal: false,
            });
        }
    }
    calls
}

/// Canonical `Decimal` texts (`rust_decimal`'s `Display` of themselves): equal values at
/// different scales, the 96-bit extremes and the smallest scale-28 value. No negative zero:
/// both runtimes read `-0` as `0`, while the evaluator keeps the text it is given.
const DECIMALS: &[&str] = &[
    "0",
    "0.0",
    "0.00",
    "1",
    "1.0",
    "1.00",
    "2.5",
    "2.50",
    "-1.5",
    "-1.50",
    "10",
    "10.000",
    "0.1",
    "0.10",
    "-0.001",
    "3.14159",
    "100",
    "79228162514264337593543950335",
    "-79228162514264337593543950335",
    "7.9228162514264337593543950335",
    "0.0000000000000000000000000001",
];

/// The three-way test's Decimal calls: every pair of equal values at different scales (both
/// ways, `1.0`/`1.00` first), then random pairs, each through every [`DECIMAL_FUNCTIONS`].
pub(crate) fn decimal_calls() -> Vec<Call> {
    let equal = |a: &str, b: &str| {
        matches!(
            crate::core::eval::decimal_order(a, b),
            Ok(std::cmp::Ordering::Equal)
        )
    };
    let mut pairs: Vec<(&str, &str)> = vec![("1.0", "1.00"), ("1.00", "1.0")];
    for a in DECIMALS {
        for b in DECIMALS {
            if a != b && equal(a, b) && !pairs.contains(&(*a, *b)) {
                pairs.push((a, b));
            }
        }
    }
    let mut rng = Rng(0x00B1_4D00_DEC1_2026);
    for _ in 0..60 {
        pairs.push((*rng.pick(DECIMALS), *rng.pick(DECIMALS)));
    }
    let mut calls = Vec::new();
    for (a, b) in pairs {
        for function in DECIMAL_FUNCTIONS {
            calls.push(Call {
                line: format!("d{}", calls.len()),
                function: function.to_string(),
                a: a.to_string(),
                b: b.to_string(),
                decimal: true,
            });
        }
    }
    calls
}

/// `t<k>=<render>` (temporal) or `d<k>=<render>` (Decimal) for every call, by the reference evaluator (the values are their
/// wire texts).
pub(crate) fn calls_expected(program: &CoreProgram, calls: &[Call]) -> Vec<String> {
    use crate::core::eval::Value;
    calls
        .iter()
        .map(|call| {
            let value = |text: &str| {
                if call.decimal {
                    Value::Decimal(text.into())
                } else {
                    Value::Str(text.into())
                }
            };
            let args = vec![value(&call.a), value(&call.b)];
            let rendered = match eval_function(program, &call.function, args) {
                Ok(value) => value.render(),
                Err(error) => panic!("{}: {error:?}", call.line),
            };
            format!("{}={rendered}", call.line)
        })
        .collect()
}

fn source(programs: &[(String, Ty, String)]) -> String {
    source_with("", programs)
}

/// [`PRELUDE`], then `extra` declarations, then the programs.
pub(crate) fn source_with(extra: &str, programs: &[(String, Ty, String)]) -> String {
    let mut source = String::from(PRELUDE);
    source.push_str(extra);
    for (_, _, function) in programs {
        source.push('\n');
        source.push_str(function);
    }
    source
}

/// The full front end; the generator must only produce programs without any diagnostic and
/// whose IR validates.
pub(crate) fn compile(source: &str) -> CoreProgram {
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
pub(crate) fn expected(program: &CoreProgram, programs: &[(String, Ty, String)]) -> Vec<String> {
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
    main_rs_with(programs, &[], "bind_differential", "", "line")
}

/// `src/main.rs` of a differential crate named `krate`: the renderer plus `extra` items, and
/// `encode`, the body of `fn encode(line: String) -> String` (what is printed after `p<i>=`).
/// The `calls` (see [`Call`]) follow the programs, their arguments read from their
/// wire texts.
pub(crate) fn main_rs_with(
    programs: &[(String, Ty, String)],
    calls: &[Call],
    krate: &str,
    extra: &str,
    encode: &str,
) -> String {
    let mut entries = String::new();
    for (name, _, _) in programs {
        let _ = writeln!(
            entries,
            "        (\"{name}\", entry(programs::bind_{name}(Ctx::pure()))),"
        );
    }
    for call in calls {
        let _ = writeln!(
            entries,
            "        (\"{}\", entry(programs::bind_{}(Ctx::pure(), wire({:?}), wire({:?})))),",
            call.line, call.function, call.a, call.b
        );
    }
    MAIN_TEMPLATE
        .replace("{{ENTRIES}}", &entries)
        .replace("{{CRATE}}", krate)
        .replace("{{EXTRA}}", extra)
        .replace("{{ENCODE}}", encode)
}

/// Writes the crate `target/<name>` (the program source, the generated module and `main`),
/// checks it with `clippy -D warnings` and returns the stdout of its release build's run.
pub(crate) fn build_and_run(name: &str, source: &str, module: &str, main: &str) -> String {
    use std::fs;
    use std::path::PathBuf;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
    let crate_dir = super::tests::test_crate_in(&root, name);
    fs::write(crate_dir.join("program.bd"), source).unwrap();
    fs::write(crate_dir.join("src/programs.rs"), module).unwrap();
    fs::write(
        crate_dir.join("src/lib.rs"),
        "#![forbid(unsafe_code)]\n#![deny(\n    clippy::unwrap_used,\n    clippy::expect_used,\n    clippy::panic,\n    clippy::unreachable\n)]\n\npub mod bind_runtime;\npub mod programs;\n",
    )
    .unwrap();
    fs::write(crate_dir.join("src/main.rs"), main).unwrap();
    // Certified servers must pass `clippy -D warnings`: so must this crate of random programs.
    super::tests::cargo(
        &crate_dir,
        &[
            "clippy",
            "--quiet",
            "--release",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    );
    super::tests::cargo(&crate_dir, &["run", "--quiet", "--release"])
}

/// The renderer replicates `Value::render` and `json_string` of `core::eval` exactly.
const MAIN_TEMPLATE: &str = r##"use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use {{CRATE}}::bind_runtime::{AppError, BoxFuture, Ctx, Int};
use {{CRATE}}::programs;

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
{{EXTRA}}
/// The printed form of a rendered result.
fn encode(line: String) -> String {
    {{ENCODE}}
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
        AppError::RequestFailed => "RequestFailed",
    }
}

type Rendered = Pin<Box<dyn Future<Output = Result<String, AppError>> + Send>>;

/// A value read from its JSON string `text`, as the server reads a request.
#[allow(dead_code)]
fn wire<T: serde::de::DeserializeOwned>(text: &str) -> T {
    match serde_json::from_value(serde_json::Value::String(text.to_string())) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("cannot read {text:?}: {error}");
            std::process::exit(1);
        }
    }
}

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
            println!("{name}={}", encode(line));
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

/// FNV-1a of `text`.
fn fnv(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The server's programs are exactly those of before the three-way test's additions to the
/// generator (same seed, same draws): the hash of their source as of commit 5d426aa, updated
/// when Long literals (`42L`) and lone surrogate escapes became errors and their spellings left
/// the generator.
#[test]
fn server_programs_are_unchanged_by_the_web_generator() {
    let (programs, _) = programs();
    let source = source(&programs);
    assert_eq!(source.len(), 1_102_804);
    assert_eq!(fnv(&source), 0x9145_cf02_0f4b_c91e);
}

/// The three-way test's generator is deterministic, produces well-typed programs only and
/// covers every construct it adds; the evaluator supports all of them.
#[test]
fn web_generator_is_deterministic_and_well_typed() {
    let (first, counts) = web_programs();
    let (second, _) = web_programs();
    assert_eq!(
        first.iter().map(|p| &p.2).collect::<Vec<_>>(),
        second.iter().map(|p| &p.2).collect::<Vec<_>>()
    );
    assert_eq!(first.len(), WEB_PROGRAMS + WEB_FIXED.len());
    for construct in WEB_CONSTRUCTS {
        assert!(
            counts.get(construct).copied().unwrap_or(0) > 0,
            "{construct}"
        );
    }
    for ty in WEB_RESULT_TYPES {
        assert!(
            first[..WEB_PROGRAMS].iter().filter(|p| p.1 == ty).count() >= 50,
            "{ty:?}"
        );
    }
    let program = compile(&source_with(WEB_PRELUDE, &first));
    let lines = expected(&program, &first);
    assert_eq!(
        lines[WEB_PROGRAMS],
        format!("p{WEB_PROGRAMS}=!RecursionLimit")
    );
    assert_eq!(
        lines[WEB_PROGRAMS + 3],
        format!("p{}=!RecursionLimit", WEB_PROGRAMS + 3)
    );
    // The deep tree: 199,800 nested `<div>`s around `fim`.
    let deep = lines.last().unwrap();
    assert_eq!(
        deep.split_once('=').unwrap().1,
        format!("{}fim{}", "<div>".repeat(199_800), "</div>".repeat(199_800))
    );
    // NaN and Infinity are frequent random results, not only fixed ones.
    let numbers: Vec<&String> = lines[..WEB_PROGRAMS]
        .iter()
        .zip(&first[..WEB_PROGRAMS])
        .filter(|(_, p)| matches!(p.1, Ty::Double | Ty::Float))
        .map(|(line, _)| line)
        .collect();
    let nan = numbers.iter().filter(|line| line.ends_with("=NaN")).count();
    let infinite = numbers
        .iter()
        .filter(|line| line.ends_with("=Infinity") || line.ends_with("=-Infinity"))
        .count();
    eprintln!("random Double/Float results: {nan} NaN, {infinite} ±Infinity");
    assert!(nan >= 20, "{nan} NaN results");
    assert!(infinite >= 20, "{infinite} Infinity results");
}

/// The temporal calls compile with the three-way test's programs and the evaluator orders the
/// vector pairs as the vectors say (comparisons, `Set` elements and `Map` keys); the random
/// pairs include many whose text order is wrong.
#[test]
fn temporal_calls_are_well_typed_and_ordered_by_value() {
    let calls = temporal_calls();
    let program = compile(&source_with(TEMPORAL_PRELUDE, &[]));
    let lines = calls_expected(&program, &calls);
    assert_eq!(lines.len(), calls.len());
    let vectors = include_str!("../../../runtime/vectors/temporal-order.json");
    let orders: Vec<&str> = vectors
        .lines()
        .filter(|line| line.contains("\"order\""))
        .map(|line| line.split('"').nth(15).unwrap())
        .collect();
    for (index, order) in orders.iter().enumerate() {
        let comparisons = match *order {
            "<" => "[true, true, false, false, false, true]",
            "=" => "[false, true, false, true, true, false]",
            _ => "[false, false, true, true, false, true]",
        };
        let first = TEMPORAL_FUNCTIONS.len() * index;
        let (a, b) = (&calls[first].a, &calls[first].b);
        let (set, map) = match *order {
            "<" => (
                format!("Set[\"{a}\", \"{b}\"]"),
                format!("Map[\"{a}\": 2, \"{b}\": 3]"),
            ),
            "=" => (format!("Set[\"{b}\"]"), format!("Map[\"{b}\": 3]")),
            _ => (
                format!("Set[\"{b}\", \"{a}\"]"),
                format!("Map[\"{b}\": 3, \"{a}\": 2]"),
            ),
        };
        assert_eq!(lines[first], format!("t{first}={comparisons}"));
        assert!(lines[first + 1].starts_with(&format!("t{}=<span>", first + 1)));
        assert_eq!(lines[first + 2], format!("t{}={set}", first + 2));
        assert_eq!(lines[first + 3], format!("t{}={map}", first + 3));
    }
    let wrong_by_text = calls
        .iter()
        .zip(&lines)
        .filter(|(call, line)| {
            call.function.starts_with("ordem")
                && line.ends_with(if call.a < call.b {
                    "=[false, false, true, true, false, true]"
                } else {
                    "=[true, true, false, false, false, true]"
                })
        })
        .count();
    assert!(wrong_by_text >= 20, "{wrong_by_text}");
    assert!(calls.iter().any(|call| call.a.contains(":60")));
    assert!(calls.iter().any(|call| call.a.starts_with('+')));
    assert!(calls.iter().any(|call| call.a.starts_with('-')));
}

/// The Decimal calls compile with the three-way test's prelude; equal values at different
/// scales are one element or key, and the first text stays (Ruling A).
#[test]
fn decimal_calls_are_well_typed_and_compare_by_value() {
    let calls = decimal_calls();
    let program = compile(&source_with(
        &format!("{WEB_PRELUDE}{DECIMAL_PRELUDE}"),
        &[],
    ));
    let lines = calls_expected(&program, &calls);
    // The first pair is (1.0, 1.00).
    let expected = [
        "Set[1.0]",
        "[1.00]",
        "Map[1.0: 2]",
        "Map[Nota(1.00): 2]",
        "[1, 1, 7, 1]",
        "Set[]",
        "[]",
        "[\"a\"]",
        "Set[Nota(1.00)]",
        "[true, true, true, true]",
        "Map[false: Set[false, true], true: Set[true]]",
    ];
    for (index, want) in expected.iter().enumerate() {
        assert_eq!(lines[index], format!("d{index}={want}"));
    }
    let pairs = calls.len() / DECIMAL_FUNCTIONS.len();
    assert!(pairs >= 70, "{pairs} pairs");
    let equal_scales = calls
        .iter()
        .zip(&lines)
        .filter(|(call, line)| {
            call.function == "decIguais"
                && call.a != call.b
                && line.ends_with("=[true, true, true, true]")
        })
        .count();
    assert!(equal_scales >= 10, "{equal_scales}");
}

/// Constructs every three-way run must contain.
pub(crate) const WEB_CONSTRUCTS: &[&str] = &[
    "UI fragment",
    "val among children",
    "component call",
    "container node",
    "Heading",
    "Link",
    "Rule",
    "Int node argument",
    "Text of String",
    "Text of Int",
    "Text of Boolean",
    "Text of Double",
    "Text of Float",
    "Text of distinct",
    "UI if/when child",
    "Double non-finite",
    "Float non-finite",
    "Double +",
    "Double -",
    "Double *",
    "Double /",
    "Double unary -",
    "Float +",
    "Float -",
    "Float *",
    "Float /",
    "Float unary -",
    "Double comparison",
    "Float comparison",
    "when over sum type",
    "when over Int",
    "when over Int?",
    "fold",
    "if",
    // Collections and rich types: all 14 builtins (13 of them also passed as values; `setOf`
    // is variadic and only called), the enum declared out of alphabetical order, the sealed
    // class with fields, the generic record and function, and `Result`.
    "setOf",
    "emptySet",
    "insert",
    "remove",
    "contains",
    "setSize",
    "toList",
    "emptyMap",
    "put",
    "lookup",
    "removeKey",
    "keys",
    "values",
    "mapSize",
    "emptySet as value",
    "insert as value",
    "remove as value",
    "contains as value",
    "setSize as value",
    "toList as value",
    "emptyMap as value",
    "put as value",
    "lookup as value",
    "removeKey as value",
    "keys as value",
    "values as value",
    "mapSize as value",
    "builtin as fold function",
    "set equality",
    "map equality",
    "enum constructor",
    "enum equality",
    "enum argument",
    "when over enum",
    "sealed with fields constructor",
    "when over sealed with fields",
    "nested enum pattern",
    "generic record at Int",
    "generic record at Set<Cor>",
    "generic record field",
    "generic function",
    "Ok",
    "Err",
    "when over Result",
    "Result literal pattern",
];

/// Builds the generated crate and compares its output with the evaluator, line by line.
#[test]
#[ignore = "builds a large Rust crate; run with `cargo test differential -- --ignored`"]
fn differential_evaluator_vs_generated_rust() {
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
    let stdout = build_and_run("bind-differential", &source, &module, &main_rs(&programs));
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
