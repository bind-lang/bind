//! The JSON writer of every compiler-generated JSON document (manifests, contracts, the IR
//! export): strings are escaped completely, so any manifest value produces valid JSON that is
//! also safe to embed in a JavaScript source.

/// `value` as a JSON string literal: `"` and `\` escaped, every control character (U+0000 to
/// U+001F and U+007F) and the JavaScript line terminators U+2028/U+2029 written as escapes.
pub fn string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            '\u{0}'..='\u{1f}' | '\u{7f}' | '\u{2028}' | '\u{2029}' => {
                output.push_str(&format!("\\u{:04x}", ch as u32));
            }
            _ => output.push(ch),
        }
    }
    output.push('"');
    output
}

/// `values` as a JSON array of strings.
pub fn array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|item| string(item))
            .collect::<Vec<_>>()
            .join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_control_character_and_line_separator_is_escaped() {
        assert_eq!(string("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(string("a\tb\nc\rd"), r#""a\tb\nc\rd""#);
        assert_eq!(string("\u{0}\u{1f}\u{7f}"), r#""\u0000\u001f\u007f""#);
        assert_eq!(string("\u{2028}\u{2029}"), "\"\\u2028\\u2029\"");
        assert_eq!(string("café"), "\"café\"");
        for code in 0u32..0x80 {
            let ch = char::from_u32(code).unwrap();
            let encoded = string(&ch.to_string());
            assert!(
                encoded.chars().all(|c| !c.is_control()),
                "{code:#x} -> {encoded}"
            );
        }
        assert_eq!(array(&["a".into(), "b\n".into()]), r#"["a","b\n"]"#);
    }
}
