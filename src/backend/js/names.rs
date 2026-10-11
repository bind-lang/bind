//! JavaScript names of user names.
//!
//! Every user name (function, local, record field, variant field) becomes `u_` followed by an
//! injective escape of the name: ASCII letters and digits are kept, `_` becomes `__` and any
//! other character becomes `_x<hex>_` (its code point in lowercase hex). So no generated name
//! is a JS reserved word (`u_class`), a global (`u_Object`) or a runtime name (the runtime is
//! only reached as `rt.<name>`), and two different user names never meet.
//!
//! Locals add `_<id>` (their IR id): an escape never has a single `_` followed by a digit, so a
//! local's name is never a function's name, and shadowing locals stay distinct. Generated
//! helpers (`ctx`, temporaries `t<n>`, codecs `dec<n>`/`enc<n>`, `API_URL`, `rt`) never start
//! with `u_`.

use std::fmt::Write as _;

/// The JS identifier of the user name `name`: `u_` and the escape.
pub fn js_value(name: &str) -> String {
    let mut output = String::from("u_");
    for c in name.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' => output.push(c),
            '_' => output.push_str("__"),
            other => {
                let _ = write!(output, "_x{:x}_", u32::from(other));
            }
        }
    }
    output
}

/// The JS identifier of local `id` named `name`.
pub fn js_local(name: &str, id: usize) -> String {
    format!("{}_{id}", js_value(name))
}

/// A JS string literal (double-quoted) with the exact contents of `text`. Only `"`, `\`, and
/// control characters are escaped, so the literal never spans lines.
pub fn js_string(text: &str) -> String {
    let mut output = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            // U+2028/U+2029 are valid in ES2019 string literals, but escaping them keeps the
            // line structure of the generated file obvious to every tool.
            c if u32::from(c) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(output, "\\u{:04x}", u32::from(c));
            }
            c => output.push(c),
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn escapes_are_prefixed_and_injective() {
        assert_eq!(js_value("class"), "u_class");
        assert_eq!(js_value("__proto__"), "u_____proto____");
        assert_eq!(js_value("a_b"), "u_a__b");
        assert_eq!(js_value("a b"), "u_a_x20_b");
        assert_eq!(js_value("ação"), "u_a_xe7__xe3_o");
        assert_eq!(js_local("x", 3), "u_x_3");
        let names = [
            "a_b", "a__b", "a_x5f_b", "a b", "a_x20_b", "_", "__", "x", "x_1", "ação", "",
        ];
        let escaped: BTreeSet<String> = names.iter().map(|name| js_value(name)).collect();
        assert_eq!(escaped.len(), names.len());
        // A local is never a function name: `u_x_1` (local) vs `u_x__1` (function `x_1`).
        assert_ne!(js_local("x", 1), js_value("x_1"));
        for name in names {
            let value = js_value(name);
            assert!(value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        }
    }

    #[test]
    fn strings_stay_on_one_line() {
        assert_eq!(js_string("a\"b\\c\nd\u{1}é"), "\"a\\\"b\\\\c\\nd\\u0001é\"");
        assert_eq!(js_string("\u{2028}"), "\"\\u2028\"");
    }
}
