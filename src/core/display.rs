//! Canonical display and HTML functions of the web frontend. `core::eval` uses them; the Rust
//! runtime (SSR) and the JS runtime replicate them byte for byte, and the three-way differential
//! test compares the results.

use super::ir::UiNode;

/// ECMAScript `Number::toString(10)`, i.e. `String(x)` for a JS number (ECMA-262 §6.1.6.1.20):
/// `NaN`, `Infinity`, `-Infinity`; `-0` and `0` give `"0"`; a negative value gives `"-"` and the
/// text of its absolute value. Otherwise, with `k` the shortest round-trip decimal digits
/// `d1…dk` and `n` such that the value is `0.d1…dk × 10^n`:
/// - `k ≤ n ≤ 21`: the digits followed by `n − k` zeros (`100`, `123456789012345680000`);
/// - `0 < n ≤ 21`: the first `n` digits, `.`, the remaining digits (`123.456`);
/// - `−6 < n ≤ 0`: `0.`, `−n` zeros, the digits (`0.000001`);
/// - otherwise exponential: `d1`, then `.d2…dk` when `k > 1`, then `e`, `+` or `-`, and
///   `|n − 1|` (`1e+21`, `1.5e-7`, `5e-324`).
///
/// The digits are those of ECMAScript: the fewest that read back as the value and, among
/// those, the nearest to it, a tie going to the even last digit (see `shortest_digits`).
///
/// A `Float` is shown by this function on its value converted to `f64`, as JS does with the
/// result of `Math.fround`.
pub fn js_number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value == 0.0 {
        return "0".into();
    }
    if value < 0.0 {
        return format!("-{}", js_number_to_string(-value));
    }
    if value.is_infinite() {
        return "Infinity".into();
    }
    let (digits, exponent) = shortest_digits(value);
    let k = digits.len() as i64;
    let n = exponent + 1;
    if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        let (whole, fraction) = digits.split_at(n as usize);
        format!("{whole}.{fraction}")
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let sign = if n - 1 < 0 { '-' } else { '+' };
        let (first, rest) = digits.split_at(1);
        let rest = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        format!("{first}{rest}e{sign}{}", (n - 1).abs())
    }
}

/// The shortest round-trip digits of a finite positive `value` and the exponent of the first
/// one (`value ≈ d1.d2…dk × 10^exponent`), as ECMAScript selects them. `{:e}` gives the fewest
/// digits `k` that read back as the value, but breaks a tie between two equally near
/// `k`-digit candidates upward, where ECMAScript takes the even one; so the value is rounded
/// exactly to `k` digits (`{:.*e}` rounds half to even) and that result is used when it also
/// reads back as the value.
fn shortest_digits(value: f64) -> (String, i64) {
    let split = |scientific: String| -> (String, i64) {
        let (mantissa, exponent) = scientific
            .split_once('e')
            .unwrap_or((scientific.as_str(), "0"));
        let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
        (digits, exponent.parse().unwrap_or(0))
    };
    let shortest = split(format!("{value:e}"));
    let precision = shortest.0.len().saturating_sub(1);
    let rounded = format!("{value:.precision$e}");
    if rounded.parse::<f64>() == Ok(value) {
        let (digits, exponent) = split(rounded);
        let trimmed = digits.trim_end_matches('0');
        if !trimmed.is_empty() {
            return (trimmed.to_string(), exponent);
        }
    }
    shortest
}

/// The text of a `rust_decimal::Decimal` with sign flag `negative`, mantissa `mantissa`
/// (< 2^96) and scale `scale` (≤ 28): its `Display`, which is also the `serde-str` form. The
/// mantissa's decimal digits are left-padded with zeros to more than `scale` digits and a `.`
/// is put `scale` digits from the right (none when `scale` is 0); trailing zeros are kept
/// (`1.50`, `0.00`). The sign flag is printed even for zero (`-0`, `-0.00`), as
/// `rust_decimal` does for a negated zero.
pub fn display_decimal(negative: bool, mantissa: u128, scale: u32) -> String {
    let scale = scale as usize;
    let mut digits = mantissa.to_string();
    if digits.len() <= scale {
        digits = format!("{}{digits}", "0".repeat(scale + 1 - digits.len()));
    }
    let (whole, fraction) = digits.split_at(digits.len() - scale);
    let sign = if negative { "-" } else { "" };
    if fraction.is_empty() {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{fraction}")
    }
}

/// The text of a `chrono::TimeDelta` of `seconds` seconds plus `nanos` (< 10^9) nanoseconds
/// (chrono's representation, so `-0.5 s` is `seconds = -1, nanos = 500_000_000`): its
/// `Display`, an ISO-8601 duration in seconds only. Zero is `P0D`; otherwise `P`, `T`, the
/// whole seconds of the absolute value, a `.` and the nanoseconds without trailing zeros when
/// they are not zero, and `S`; a negative duration is prefixed by `-` (`-PT0.5S`, `PT86400S`).
pub fn display_duration(seconds: i64, nanos: u32) -> String {
    let (sign, seconds, nanos) = if seconds < 0 {
        let seconds = i128::from(seconds);
        if nanos == 0 {
            ("-", -seconds, 0)
        } else {
            ("-", -seconds - 1, 1_000_000_000 - nanos)
        }
    } else {
        ("", i128::from(seconds), nanos)
    };
    if seconds == 0 && nanos == 0 {
        return "P0D".into();
    }
    let fraction = if nanos == 0 {
        String::new()
    } else {
        format!(".{}", format!("{nanos:09}").trim_end_matches('0'))
    };
    format!("{sign}PT{seconds}{fraction}S")
}

/// An argument of a UI node: a `String` or the decimal digits of an `Int`.
#[derive(Clone, Debug, PartialEq)]
pub enum UiArg {
    Str(String),
    Int(String),
}

/// A rendered UI value.
#[derive(Clone, Debug, PartialEq)]
pub struct UiTree {
    pub node: UiNode,
    pub args: Vec<UiArg>,
    pub children: Vec<UiTree>,
}

/// The HTML of a UI value: the markup the former Elm client (`elm_ui_node` and
/// `elm_static_main` in `certified.rs`) put in the DOM, serialized like `outerHTML` but with
/// the escaping below. No whitespace or newline is added between nodes.
///
/// Escaping (`escape_html`), in text and in attribute values alike: `&` → `&amp;`,
/// `<` → `&lt;`, `>` → `&gt;`, `"` → `&quot;`, `'` → `&#39;`; everything else (newlines,
/// non-ASCII) is written as is. An attribute is written ` name="escaped value"`, in the order
/// listed below, and only when its argument is present.
///
/// An argument is used as text as is (`String`) or as its decimal digits (`Int`); arguments
/// beyond those listed are ignored. Children are rendered in order between the tags.
///
/// | Node | HTML |
/// |---|---|
/// | `UI` (the root) | `<div class="bind-site">children</div>` (the Elm `view` wrapper) |
/// | `Text(s)` | the escaped text `s`, no element (an Elm `text` node); children ignored |
/// | `Empty` | nothing (the empty string) |
/// | `Page`, `Container`, `Div`, `Grid`, `Stack` | `<div class id>children</div>` |
/// | `Header`, `Nav`, `Main`, `Section`, `Article`, `Aside`, `Footer` | `<header class id>`, `<nav …>`, `<main …>`, `<section …>`, `<article …>`, `<aside …>`, `<footer …>` |
/// | `Paragraph`, `Span`, `Strong`, `Emphasis`, `Small` | `<p class id>`, `<span …>`, `<strong …>`, `<em …>`, `<small …>` |
/// | `List`, `Item`, `Pre`, `Code` | `<ul class id>`, `<li …>`, `<pre …>`, `<code …>` |
/// | `Heading(level, class)` | `<hN class>children</hN>`, see below |
/// | `Link(href, class, target, label)` | `<a href class target rel aria-label>children</a>`, see below |
/// | `Rule(class)` | `<hr class>`: a void element, no end tag; children ignored |
///
/// - Containers: the first argument is `class`, the second `id`.
/// - `Heading`: `N` is the `Int` level clamped to 1..=6; a missing or `String` level gives 1.
/// - `Link`: `href` is the first argument, but the empty string when it matches the Elm
///   sanitizer `^\s*j\s*a\s*v\s*a\s*s\s*c\s*r\s*i\s*p\s*t\s*:` (ASCII case-insensitive, `\s`
///   as in JavaScript: see `is_js_whitespace`); `class` the second, `target` the third;
///   `rel="noopener noreferrer"` follows `target` exactly when the target is `_blank`;
///   `aria-label` is the fourth.
///
/// Example: `UI { Section("hero", "top") { Heading(1) { Text("A & B") } } }` →
/// `<div class="bind-site"><section class="hero" id="top"><h1>A &amp; B</h1></section></div>`.
pub fn render_html_value(tree: &UiTree) -> String {
    let mut out = String::new();
    render_into(&mut out, tree);
    out
}

fn render_into(out: &mut String, tree: &UiTree) {
    let arg = |index: usize| tree.args.get(index).map(arg_text);
    let mut attributes: Vec<(&str, String)> = Vec::new();
    let mut push = |name: &'static str, value: Option<&str>| {
        if let Some(value) = value {
            attributes.push((name, value.to_string()));
        }
    };
    let tag: String = match tree.node {
        UiNode::Empty => return,
        UiNode::Text => {
            if let Some(text) = arg(0) {
                out.push_str(&escape_html(text));
            }
            return;
        }
        UiNode::Ui => {
            push("class", Some("bind-site"));
            "div".into()
        }
        UiNode::Heading => {
            let level = match tree.args.first() {
                Some(UiArg::Int(digits)) => heading_level(digits),
                _ => 1,
            };
            push("class", arg(1));
            format!("h{level}")
        }
        UiNode::Link => {
            let href = arg(0).map(|url| if is_javascript_uri(url) { "" } else { url });
            push("href", href);
            push("class", arg(1));
            push("target", arg(2));
            if arg(2) == Some("_blank") {
                push("rel", Some("noopener noreferrer"));
            }
            push("aria-label", arg(3));
            "a".into()
        }
        UiNode::Rule => {
            push("class", arg(0));
            out.push_str("<hr");
            write_attributes(out, &attributes);
            out.push('>');
            return;
        }
        container => {
            push("class", arg(0));
            push("id", arg(1));
            container_tag(container).into()
        }
    };
    out.push('<');
    out.push_str(&tag);
    write_attributes(out, &attributes);
    out.push('>');
    for child in &tree.children {
        render_into(out, child);
    }
    out.push_str("</");
    out.push_str(&tag);
    out.push('>');
}

fn arg_text(arg: &UiArg) -> &str {
    match arg {
        UiArg::Str(text) | UiArg::Int(text) => text,
    }
}

fn write_attributes(out: &mut String, attributes: &[(&str, String)]) {
    for (name, value) in attributes {
        out.push(' ');
        out.push_str(name);
        out.push_str("=\"");
        out.push_str(&escape_html(value));
        out.push('"');
    }
}

/// The tag of a container node (every node but `UI`, `Text`, `Empty`, `Heading`, `Link` and
/// `Rule`).
fn container_tag(node: UiNode) -> &'static str {
    match node {
        UiNode::Header => "header",
        UiNode::Nav => "nav",
        UiNode::Main => "main",
        UiNode::Section => "section",
        UiNode::Article => "article",
        UiNode::Aside => "aside",
        UiNode::Footer => "footer",
        UiNode::Paragraph => "p",
        UiNode::Span => "span",
        UiNode::Strong => "strong",
        UiNode::Emphasis => "em",
        UiNode::Small => "small",
        UiNode::List => "ul",
        UiNode::Item => "li",
        UiNode::Pre => "pre",
        UiNode::Code => "code",
        _ => "div",
    }
}

/// The heading level of an `Int` argument's decimal digits, clamped to 1..=6.
fn heading_level(digits: &str) -> u8 {
    if digits.starts_with('-') {
        return 1;
    }
    match digits.parse::<u64>() {
        Ok(level) => level.clamp(1, 6) as u8,
        Err(_) => 6,
    }
}

/// HTML escaping of text and attribute values: `&`, `<`, `>`, `"` and `'`.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Elm's `href` sanitizer `/^\s*j\s*a\s*v\s*a\s*s\s*c\s*r\s*i\s*p\s*t\s*:/i`.
fn is_javascript_uri(url: &str) -> bool {
    let mut chars = url.chars().filter(|c| !is_js_whitespace(*c));
    "javascript:".chars().all(|expected| {
        chars
            .next()
            .is_some_and(|c| c.eq_ignore_ascii_case(&expected))
    })
}

/// JavaScript's `\s`: WhiteSpace and LineTerminator — U+0009..U+000D, U+0020, U+00A0,
/// U+1680, U+2000..U+200A, U+2028, U+2029, U+202F, U+205F, U+3000 and U+FEFF.
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_follow_ecmascript() {
        let cases: &[(f64, &str)] = &[
            (0.1 + 0.2, "0.30000000000000004"),
            (1e21, "1e+21"),
            (1e-7, "1e-7"),
            (123456789012345680000.0, "123456789012345680000"),
            (-0.0, "0"),
            (5e-324, "5e-324"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
            (1.5e300, "1.5e+300"),
            (0.000001, "0.000001"),
            (0.0000012, "0.0000012"),
            (1.2e-7, "1.2e-7"),
            (0.0, "0"),
            (1.0, "1"),
            (-1.5, "-1.5"),
            (100.0, "100"),
            (123.456, "123.456"),
            (1e20, "100000000000000000000"),
            (1.5e21, "1.5e+21"),
            (-1e-7, "-1e-7"),
            (f64::MAX, "1.7976931348623157e+308"),
            (f64::MIN_POSITIVE, "2.2250738585072014e-308"),
            (0.1f32 as f64, "0.10000000149011612"),
            // Ties between two shortest candidates go to the even digit (`{:e}` gives `…313`).
            (1010.0 + 14362.0 / 32768.0, "1010.4382934570312"),
            (-87.76602172851562, "-87.76602172851562"),
        ];
        for (value, expected) in cases {
            assert_eq!(js_number_to_string(*value), *expected, "{value:e}");
        }
    }

    #[test]
    fn decimals_match_rust_decimal() {
        let cases: &[(bool, u128, u32)] = &[
            (false, 0, 0),
            (false, 0, 2),
            (true, 0, 0),
            (true, 0, 3),
            (false, 5, 3),
            (true, 150, 2),
            (false, 123456, 0),
            (false, 1, 28),
            (true, (1u128 << 96) - 1, 28),
            (false, (1u128 << 96) - 1, 0),
            (false, 1000, 1),
        ];
        for &(negative, mantissa, scale) in cases {
            let lo = mantissa as u32;
            let mid = (mantissa >> 32) as u32;
            let hi = (mantissa >> 64) as u32;
            // `from_parts` drops the sign of zero; negation keeps it (`-0.00`).
            let positive = rust_decimal::Decimal::from_parts(lo, mid, hi, false, scale);
            let expected = if negative { -positive } else { positive };
            assert_eq!(
                display_decimal(negative, mantissa, scale),
                expected.to_string(),
                "{negative} {mantissa} {scale}"
            );
        }
    }

    #[test]
    fn durations_match_chrono() {
        let cases: &[(i64, u32)] = &[
            (0, 0),
            (1, 0),
            (-1, 0),
            (0, 500_000_000),
            (-1, 500_000_000),
            (86_400, 1),
            (-86_401, 999_999_999),
            (12, 120_000_000),
            (i64::MAX / 1000, 807_000_000),
            (-(i64::MAX / 1000), 193_000_000),
        ];
        for &(seconds, nanos) in cases {
            let expected = chrono::TimeDelta::new(seconds, nanos).expect("in range");
            assert_eq!(
                display_duration(seconds, nanos),
                expected.to_string(),
                "{seconds} {nanos}"
            );
        }
    }

    fn s(text: &str) -> UiArg {
        UiArg::Str(text.into())
    }

    fn n(node: UiNode, args: Vec<UiArg>, children: Vec<UiTree>) -> UiTree {
        UiTree {
            node,
            args,
            children,
        }
    }

    fn text(value: &str) -> UiTree {
        n(UiNode::Text, vec![s(value)], vec![])
    }

    #[test]
    fn html_of_containers_and_text() {
        let tree = n(
            UiNode::Ui,
            vec![],
            vec![n(
                UiNode::Page,
                vec![s("page")],
                vec![
                    n(UiNode::Section, vec![s("hero wrap"), s("top")], vec![]),
                    n(UiNode::Main, vec![], vec![text("a")]),
                    n(
                        UiNode::Paragraph,
                        vec![s("eyebrow")],
                        vec![
                            n(UiNode::Span, vec![s("dot")], vec![text("●")]),
                            text("WHOLE\nAPP"),
                        ],
                    ),
                ],
            )],
        );
        assert_eq!(
            render_html_value(&tree),
            "<div class=\"bind-site\"><div class=\"page\"><section class=\"hero wrap\" id=\"top\">\
             </section><main>a</main><p class=\"eyebrow\"><span class=\"dot\">●</span>WHOLE\nAPP\
             </p></div></div>"
        );
    }

    #[test]
    fn html_element_names() {
        let cases = [
            (UiNode::Container, "div"),
            (UiNode::Div, "div"),
            (UiNode::Grid, "div"),
            (UiNode::Stack, "div"),
            (UiNode::Header, "header"),
            (UiNode::Nav, "nav"),
            (UiNode::Article, "article"),
            (UiNode::Aside, "aside"),
            (UiNode::Footer, "footer"),
            (UiNode::Strong, "strong"),
            (UiNode::Emphasis, "em"),
            (UiNode::Small, "small"),
            (UiNode::List, "ul"),
            (UiNode::Item, "li"),
            (UiNode::Pre, "pre"),
            (UiNode::Code, "code"),
        ];
        for (node, tag) in cases {
            assert_eq!(
                render_html_value(&n(node, vec![], vec![text("x")])),
                format!("<{tag}>x</{tag}>")
            );
        }
        assert_eq!(render_html_value(&n(UiNode::Empty, vec![], vec![])), "");
    }

    #[test]
    fn html_heading_link_and_rule() {
        let heading = |level: &str, args: Vec<UiArg>| {
            let mut all = vec![UiArg::Int(level.into())];
            all.extend(args);
            render_html_value(&n(UiNode::Heading, all, vec![text("t")]))
        };
        assert_eq!(heading("1", vec![s("hero")]), "<h1 class=\"hero\">t</h1>");
        assert_eq!(heading("3", vec![]), "<h3>t</h3>");
        assert_eq!(heading("6", vec![]), "<h6>t</h6>");
        assert_eq!(heading("0", vec![]), "<h1>t</h1>");
        assert_eq!(heading("-5", vec![]), "<h1>t</h1>");
        assert_eq!(heading("99999999999999999999", vec![]), "<h6>t</h6>");
        assert_eq!(
            render_html_value(&n(UiNode::Heading, vec![s("2")], vec![])),
            "<h1></h1>"
        );

        let link = |args: Vec<UiArg>| render_html_value(&n(UiNode::Link, args, vec![text("go")]));
        assert_eq!(link(vec![s("#top")]), "<a href=\"#top\">go</a>");
        assert_eq!(
            link(vec![s("#top"), s("brand")]),
            "<a href=\"#top\" class=\"brand\">go</a>"
        );
        assert_eq!(
            link(vec![s("https://x"), s("c"), s("_blank"), s("Label")]),
            "<a href=\"https://x\" class=\"c\" target=\"_blank\" rel=\"noopener noreferrer\" \
             aria-label=\"Label\">go</a>"
        );
        assert_eq!(
            link(vec![s("/a"), s("c"), s("_self")]),
            "<a href=\"/a\" class=\"c\" target=\"_self\">go</a>"
        );
        assert_eq!(
            link(vec![s(" Java\tScript:alert(1)")]),
            "<a href=\"\">go</a>"
        );
        assert_eq!(
            link(vec![s("\u{feff}javascript\u{3000}:x")]),
            "<a href=\"\">go</a>"
        );
        assert_eq!(
            link(vec![s("javascripts:x")]),
            "<a href=\"javascripts:x\">go</a>"
        );
        assert_eq!(link(vec![]), "<a>go</a>");

        assert_eq!(render_html_value(&n(UiNode::Rule, vec![], vec![])), "<hr>");
        assert_eq!(
            render_html_value(&n(UiNode::Rule, vec![s("sep")], vec![text("ignored")])),
            "<hr class=\"sep\">"
        );
    }

    #[test]
    fn html_escapes_text_and_attributes() {
        assert_eq!(
            render_html_value(&text("<script>alert('x' & \"y\")</script>")),
            "&lt;script&gt;alert(&#39;x&#39; &amp; &quot;y&quot;)&lt;/script&gt;"
        );
        assert_eq!(
            render_html_value(&n(
                UiNode::Div,
                vec![s("a\"b<c>"), UiArg::Int("7".into()), s("extra")],
                vec![]
            )),
            "<div class=\"a&quot;b&lt;c&gt;\" id=\"7\"></div>"
        );
    }
}
