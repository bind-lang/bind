use unicode_general_category::{GeneralCategory, get_general_category};

use crate::diagnostic::{Diagnostic, Span};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenKind {
    Identifier,
    /// A string literal; `text` is its value (escapes decoded).
    String,
    /// A string literal with a template (`$name` or `${…}`, not an escaped `\$`); `text` is
    /// as for `String`. Bind rejects templates (E7001).
    TemplateString,
    Character,
    Number,
    Symbol,
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub kind: TokenKind,
    pub text: String,
    pub span: Span,
}

/// Lexes the Kotlin lexical surface used by Bind.
///
/// Token categories are intentionally coarser than Kotlin's ANTLR token names because the Bind
/// parser does not need to distinguish hard and soft keywords. Token boundaries and accepted
/// spellings follow KotlinLexer.g4; comments and whitespace use Kotlin's hidden channel behavior.
pub fn lex(file: &str, source: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    Lexer::new(file, source).run()
}

fn string_kind(template: bool) -> TokenKind {
    if template {
        TokenKind::TemplateString
    } else {
        TokenKind::String
    }
}

/// Whether the source of a string literal contains a template: `$` followed by `{`, an
/// identifier start (Kotlin's `Letter` or `_`) or a backtick identifier (`$`x``), as in
/// Kotlin's `FieldIdentifier`. In a line string (`escapes`) a `\$` is a literal dollar sign and
/// an escape is skipped as a whole, so in `"\\$x"` the `$` is a template again; a raw string
/// has no escapes.
fn has_template(source: &[char], escapes: bool) -> bool {
    let mut index = 0;
    while index < source.len() {
        match source[index] {
            '\\' if escapes => index += 2,
            '$' if source.get(index + 1).is_some_and(|next| {
                *next == '{'
                    || is_identifier_start(*next)
                    || (*next == '`' && backtick_identifier_follows(&source[index + 1..]))
            }) =>
            {
                return true;
            }
            _ => index += 1,
        }
    }
    false
}

/// Whether `source` starts with a closed, non-empty backtick identifier.
fn backtick_identifier_follows(source: &[char]) -> bool {
    let body = source
        .iter()
        .skip(1)
        .take_while(|ch| !matches!(ch, '`' | '\r' | '\n'));
    let length = body.count();
    length > 0 && source.get(length + 1) == Some(&'`')
}

/// Whether `text` (the content of a backtick-escaped identifier) is itself a valid identifier.
/// Escaping only lets a keyword be used as a name; the text reaches generated code and file
/// names, so it never holds spaces, punctuation or path separators.
fn is_valid_escaped_identifier(text: &[char]) -> bool {
    text.first()
        .is_some_and(|first| is_identifier_start(*first))
        && text[1..].iter().all(|ch| is_identifier_continue(*ch))
}

/// Deepest nesting of `${...}` inside string templates the lexer skips over.
const MAX_INTERPOLATION_DEPTH: usize = 256;

struct Lexer<'a> {
    file: &'a str,
    chars: Vec<char>,
    index: usize,
    line: usize,
    column: usize,
    previous_was_cr: bool,
    /// Open `${...}` templates being skipped, bounded by `MAX_INTERPOLATION_DEPTH`.
    interpolation_depth: usize,
    /// Set once the interpolation limit is reported; the unwinding adds no diagnostics.
    interpolation_overflow: bool,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Lexer<'a> {
    fn new(file: &'a str, source: &str) -> Self {
        Self {
            file,
            chars: source.chars().collect(),
            index: 0,
            line: 1,
            column: 1,
            previous_was_cr: false,
            interpolation_depth: 0,
            interpolation_overflow: false,
            tokens: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn run(mut self) -> (Vec<Token>, Vec<Diagnostic>) {
        if self.starts_with("#!") {
            self.skip_until_newline();
        }
        while let Some(ch) = self.peek(0) {
            if is_kotlin_whitespace(ch) || matches!(ch, '\r' | '\n') {
                self.advance();
            } else if self.starts_with("//") {
                self.skip_until_newline();
            } else if self.starts_with("/*") {
                self.skip_delimited_comment();
            } else if self.starts_with("\"\"\"") {
                self.scan_multiline_string();
            } else if ch == '"' {
                self.scan_line_string();
            } else if ch == '\'' && character_literal_length(&self.chars[self.index..]) > 0 {
                self.scan_character();
            } else if ch == '`' {
                self.scan_escaped_identifier();
            } else if is_identifier_start(ch) {
                self.scan_identifier();
            } else if ch.is_ascii_digit()
                || (ch == '.' && self.peek(1).is_some_and(|next| next.is_ascii_digit()))
            {
                self.scan_number();
            } else if let Some(operator) = self.not_type_operator() {
                let span = self.span();
                self.advance_count(operator.chars().count());
                self.tokens.push(Token {
                    kind: TokenKind::Symbol,
                    text: operator.to_string(),
                    span,
                });
            } else if let Some(operator) = self.operator() {
                let span = self.span();
                self.advance_count(operator.chars().count());
                self.tokens.push(Token {
                    kind: TokenKind::Symbol,
                    text: operator.to_string(),
                    span,
                });
            } else {
                let span = self.span();
                self.advance();
                self.diagnostics.push(Diagnostic::error(
                    "E1003",
                    format!("Unexpected character `{ch}` in Kotlin-compatible lexical input."),
                    span.clone(),
                ));
                self.tokens.push(Token {
                    kind: TokenKind::Symbol,
                    text: ch.to_string(),
                    span,
                });
            }
        }
        self.tokens.push(Token {
            kind: TokenKind::Eof,
            text: String::new(),
            span: self.span(),
        });
        (self.tokens, self.diagnostics)
    }

    fn scan_identifier(&mut self) {
        let span = self.span();
        let start = self.index;
        self.advance();
        while self.peek(0).is_some_and(is_identifier_continue) {
            self.advance();
        }
        let mut text: String = self.chars[start..self.index].iter().collect();

        if text == "as" && self.peek(0) == Some('?') {
            self.advance();
            text.push('?');
            self.tokens.push(Token {
                kind: TokenKind::Symbol,
                text,
                span,
            });
            return;
        }
        if ["return", "continue", "break", "this", "super"].contains(&text.as_str())
            && self.peek(0) == Some('@')
            && self
                .peek(1)
                .is_some_and(|ch| is_identifier_start(ch) || ch == '`')
        {
            self.advance();
            text.push('@');
            if self.peek(0) == Some('`') {
                let label_span = self.span();
                self.advance();
                let label_start = self.index;
                while self
                    .peek(0)
                    .is_some_and(|ch| !matches!(ch, '`' | '\r' | '\n'))
                {
                    text.push(self.advance().expect("escaped label character exists"));
                }
                if !is_valid_escaped_identifier(&self.chars[label_start..self.index]) {
                    self.diagnostics
                        .push(invalid_escaped_identifier(label_span));
                }
                if self.peek(0) == Some('`') {
                    self.advance();
                }
            } else {
                let label_start = self.index;
                self.advance();
                while self.peek(0).is_some_and(is_identifier_continue) {
                    self.advance();
                }
                text.extend(&self.chars[label_start..self.index]);
            }
        }
        self.tokens.push(Token {
            kind: TokenKind::Identifier,
            text,
            span,
        });
    }

    fn scan_escaped_identifier(&mut self) {
        let span = self.span();
        self.advance();
        let start = self.index;
        while let Some(ch) = self.peek(0) {
            if ch == '`' {
                if self.index == start {
                    self.diagnostics.push(Diagnostic::error(
                        "E1004",
                        "An escaped Kotlin identifier cannot be empty.",
                        span.clone(),
                    ));
                }
                if self.index > start
                    && !is_valid_escaped_identifier(&self.chars[start..self.index])
                {
                    self.diagnostics
                        .push(invalid_escaped_identifier(span.clone()));
                }
                let text: String = self.chars[start..self.index].iter().collect();
                self.advance();
                self.tokens.push(Token {
                    kind: TokenKind::Identifier,
                    text,
                    span,
                });
                return;
            }
            if matches!(ch, '\r' | '\n') {
                break;
            }
            self.advance();
        }
        self.diagnostics.push(Diagnostic::error(
            "E1004",
            "Unterminated escaped Kotlin identifier.",
            span.clone(),
        ));
        self.tokens.push(Token {
            kind: TokenKind::Identifier,
            text: self.chars[start..self.index].iter().collect(),
            span,
        });
    }

    fn scan_number(&mut self) {
        let span = self.span();
        let length = number_length(&self.chars[self.index..]);
        if length == 0 {
            let ch = self.advance().expect("number start exists");
            self.tokens.push(Token {
                kind: TokenKind::Symbol,
                text: ch.to_string(),
                span,
            });
            return;
        }
        let start = self.index;
        self.advance_count(length);
        self.tokens.push(Token {
            kind: TokenKind::Number,
            text: self.chars[start..self.index].iter().collect(),
            span,
        });
    }

    fn scan_character(&mut self) {
        let span = self.span();
        let start = self.index;
        self.advance_count(character_literal_length(&self.chars[self.index..]));
        self.tokens.push(Token {
            kind: TokenKind::Character,
            text: self.chars[start + 1..self.index - 1].iter().collect(),
            span,
        });
    }

    fn scan_line_string(&mut self) {
        let span = self.span();
        let start = self.index;
        let (text, closed) = self.line_string_value();
        let template = has_template(&self.chars[start..self.index], true);
        if !closed && !self.interpolation_overflow {
            self.diagnostics.push(Diagnostic::error(
                "E1002",
                "Unterminated Kotlin line string.",
                span.clone(),
            ));
        }
        self.tokens.push(Token {
            kind: string_kind(template),
            text,
            span,
        });
    }

    fn line_string_value(&mut self) -> (String, bool) {
        self.advance();
        let mut text = String::new();
        while let Some(ch) = self.peek(0) {
            match ch {
                '"' => {
                    self.advance();
                    return (text, true);
                }
                '\r' | '\n' => return (text, false),
                '\\' => {
                    let escape_start = self.index;
                    let escape_span = self.span();
                    if let Some(value) = self.scan_escape() {
                        text.push_str(&value);
                    } else {
                        self.diagnostics.push(Diagnostic::error(
                            "E1006",
                            "Invalid escape sequence in Kotlin string.",
                            escape_span,
                        ));
                        // Skip the unknown escaped character; a partial `\u` escape already
                        // consumed what it read (which may not include the closing quote).
                        if self.index == escape_start + 1 {
                            self.advance();
                        }
                    }
                }
                '$' if self.peek(1) == Some('{') => {
                    let start = self.index;
                    let start_span = self.span();
                    if !self.skip_interpolation() {
                        if !self.interpolation_overflow {
                            self.diagnostics.push(Diagnostic::error(
                                "E1007",
                                "Unterminated Kotlin string interpolation.",
                                start_span,
                            ));
                        }
                        return (text, false);
                    }
                    text.extend(&self.chars[start..self.index]);
                }
                _ => {
                    text.push(ch);
                    self.advance();
                }
            }
        }
        (text, false)
    }

    fn scan_multiline_string(&mut self) {
        let span = self.span();
        let start = self.index;
        self.advance_count(3);
        let mut text = String::new();
        let mut closed = false;
        while self.peek(0).is_some() {
            if self.peek(0) == Some('"') {
                let run = self.quote_run();
                if run >= 3 {
                    for _ in 0..run - 3 {
                        text.push('"');
                    }
                    self.advance_count(run);
                    closed = true;
                    break;
                }
            }
            if self.starts_with("${") {
                let start = self.index;
                let start_span = self.span();
                if !self.skip_interpolation() {
                    if !self.interpolation_overflow {
                        self.diagnostics.push(Diagnostic::error(
                            "E1007",
                            "Unterminated Kotlin multiline string interpolation.",
                            start_span,
                        ));
                    }
                    break;
                }
                text.extend(&self.chars[start..self.index]);
            } else {
                text.push(self.advance().expect("multiline character exists"));
            }
        }
        if !closed && !self.interpolation_overflow {
            self.diagnostics.push(Diagnostic::error(
                "E1002",
                "Unterminated Kotlin multiline string.",
                span.clone(),
            ));
        }
        // A raw string has no escapes: every `$name` or `${` in it is a template.
        let template = has_template(&self.chars[start..self.index], false);
        self.tokens.push(Token {
            kind: string_kind(template),
            text,
            span,
        });
    }

    fn skip_interpolation(&mut self) -> bool {
        if self.interpolation_depth >= MAX_INTERPOLATION_DEPTH {
            if !self.interpolation_overflow {
                self.interpolation_overflow = true;
                self.diagnostics.push(Diagnostic::error(
                    "E1007",
                    "String interpolation is nested too deeply (limit 256).",
                    self.span(),
                ));
            }
            self.skip_until_newline();
            return false;
        }
        self.interpolation_depth += 1;
        let closed = self.skip_interpolation_body();
        self.interpolation_depth -= 1;
        closed
    }

    fn skip_interpolation_body(&mut self) -> bool {
        self.advance_count(2);
        let mut depth = 1;
        while self.peek(0).is_some() {
            if self.starts_with("//") {
                self.skip_until_newline();
            } else if self.starts_with("/*") {
                self.skip_delimited_comment();
            } else if self.starts_with("\"\"\"") {
                if !self.skip_multiline_string_value() {
                    return false;
                }
            } else if self.peek(0) == Some('"') {
                if !self.line_string_value().1 {
                    return false;
                }
            } else if self.peek(0) == Some('\'') {
                self.skip_character_value();
            } else if self.peek(0) == Some('{') {
                depth += 1;
                self.advance();
            } else if self.peek(0) == Some('}') {
                depth -= 1;
                self.advance();
                if depth == 0 {
                    return true;
                }
            } else {
                self.advance();
            }
        }
        false
    }

    fn skip_multiline_string_value(&mut self) -> bool {
        self.advance_count(3);
        while self.peek(0).is_some() {
            if self.peek(0) == Some('"') {
                let run = self.quote_run();
                if run >= 3 {
                    self.advance_count(run);
                    return true;
                }
            }
            if self.starts_with("${") {
                self.skip_interpolation();
            } else {
                self.advance();
            }
        }
        false
    }

    fn skip_character_value(&mut self) {
        self.advance();
        if self.peek(0) == Some('\\') {
            self.scan_escape();
        } else {
            self.advance();
        }
        if self.peek(0) == Some('\'') {
            self.advance();
        }
    }

    fn scan_escape(&mut self) -> Option<String> {
        if self.peek(0) != Some('\\') {
            return None;
        }
        self.advance();
        let escaped = self.peek(0)?;
        if escaped == 'u' {
            self.advance();
            let start = self.index;
            for _ in 0..4 {
                if !self.peek(0).is_some_and(|ch| ch.is_ascii_hexdigit()) {
                    return None;
                }
                self.advance();
            }
            let digits: String = self.chars[start..self.index].iter().collect();
            let value = u32::from_str_radix(&digits, 16).ok()?;
            if let Some(ch) = char::from_u32(value) {
                return Some(ch.to_string());
            }
            // Kotlin strings are UTF-16: a high surrogate escape followed by a low surrogate
            // escape is one character (`"\uD83D\uDE00"` is 😀). A lone surrogate is invalid.
            if !(0xD800..0xDC00).contains(&value) || !self.starts_with("\\u") {
                return None;
            }
            let low_digits = self.chars.get(self.index + 2..self.index + 6)?;
            if !low_digits.iter().all(char::is_ascii_hexdigit) {
                return None;
            }
            let low = u32::from_str_radix(&low_digits.iter().collect::<String>(), 16).ok()?;
            if !(0xDC00..0xE000).contains(&low) {
                return None;
            }
            self.advance_count(6);
            let combined = 0x10000 + ((value - 0xD800) << 10) + (low - 0xDC00);
            return char::from_u32(combined).map(|ch| ch.to_string());
        }
        let value = match escaped {
            't' => "\t",
            'b' => "\u{0008}",
            'r' => "\r",
            'n' => "\n",
            '\'' => "'",
            '"' => "\"",
            '\\' => "\\",
            '$' => "$",
            _ => return None,
        };
        self.advance();
        Some(value.to_string())
    }

    fn skip_delimited_comment(&mut self) {
        let span = self.span();
        self.advance_count(2);
        let mut depth = 1;
        while self.peek(0).is_some() {
            if self.starts_with("/*") {
                depth += 1;
                self.advance_count(2);
            } else if self.starts_with("*/") {
                depth -= 1;
                self.advance_count(2);
                if depth == 0 {
                    return;
                }
            } else {
                self.advance();
            }
        }
        self.diagnostics.push(Diagnostic::error(
            "E1001",
            "Unterminated nested Kotlin block comment.",
            span,
        ));
    }

    fn skip_until_newline(&mut self) {
        while self.peek(0).is_some_and(|ch| !matches!(ch, '\r' | '\n')) {
            self.advance();
        }
    }

    fn operator(&self) -> Option<&'static str> {
        const OPERATORS: &[&str] = &[
            "!==", "===", "..<", "...", "+=", "-=", "*=", "/=", "%=", "->", "=>", "..", "::", ";;",
            "++", "--", "&&", "||", "<=", ">=", "!=", "==", "(", ")", "[", "]", "{", "}", "*", "%",
            "/", "+", "-", "!", ":", ";", "=", "#", "@", "?", "<", ">", ".", ",", "&", "'",
        ];
        OPERATORS
            .iter()
            .copied()
            .find(|item| self.starts_with(item))
    }

    fn not_type_operator(&self) -> Option<&'static str> {
        ["!is", "!in"].into_iter().find(|operator| {
            self.starts_with(operator)
                && self.peek(operator.len()).is_some_and(|next| {
                    is_kotlin_whitespace(next)
                        || matches!(next, '\r' | '\n')
                        || self
                            .chars
                            .get(self.index + operator.len()..)
                            .is_some_and(|tail| {
                                tail.starts_with(&['/', '/']) || tail.starts_with(&['/', '*'])
                            })
                })
        })
    }

    fn quote_run(&self) -> usize {
        let mut count = 0;
        while self.peek(count) == Some('"') {
            count += 1;
        }
        count
    }

    fn starts_with(&self, value: &str) -> bool {
        value
            .chars()
            .enumerate()
            .all(|(offset, ch)| self.peek(offset) == Some(ch))
    }

    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.index + offset).copied()
    }

    fn advance_count(&mut self, count: usize) {
        for _ in 0..count {
            self.advance();
        }
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.peek(0)?;
        self.index += 1;
        match ch {
            '\r' => {
                self.line += 1;
                self.column = 1;
                self.previous_was_cr = true;
            }
            '\n' => {
                if !self.previous_was_cr {
                    self.line += 1;
                }
                self.column = 1;
                self.previous_was_cr = false;
            }
            _ => {
                self.column += 1;
                self.previous_was_cr = false;
            }
        }
        Some(ch)
    }

    fn span(&self) -> Span {
        Span {
            file: self.file.into(),
            line: self.line,
            column: self.column,
        }
    }
}

fn invalid_escaped_identifier(span: Span) -> Diagnostic {
    Diagnostic::error(
        "E1004",
        "An escaped identifier must itself be a valid identifier (letters, digits and `_`, not starting with a digit); backticks only allow a keyword as a name.",
        span,
    )
}

fn is_kotlin_whitespace(ch: char) -> bool {
    matches!(ch, '\u{0020}' | '\u{0009}' | '\u{000c}')
}

fn is_identifier_start(ch: char) -> bool {
    ch == '_'
        || matches!(
            get_general_category(ch),
            GeneralCategory::UppercaseLetter
                | GeneralCategory::LowercaseLetter
                | GeneralCategory::TitlecaseLetter
                | GeneralCategory::ModifierLetter
                | GeneralCategory::OtherLetter
        )
}

fn is_identifier_continue(ch: char) -> bool {
    is_identifier_start(ch) || get_general_category(ch) == GeneralCategory::DecimalNumber
}

fn character_literal_length(chars: &[char]) -> usize {
    if chars.first() != Some(&'\'') {
        return 0;
    }
    match chars.get(1) {
        Some('\\') if chars.get(2) == Some(&'u') => {
            if chars.get(3..7).is_some_and(|digits| {
                digits.len() == 4 && digits.iter().all(|digit| digit.is_ascii_hexdigit())
            }) && chars.get(7) == Some(&'\'')
            {
                8
            } else {
                0
            }
        }
        Some('\\')
            if chars.get(2).is_some_and(|escaped| {
                matches!(escaped, 't' | 'b' | 'r' | 'n' | '\'' | '"' | '\\' | '$')
            }) && chars.get(3) == Some(&'\'') =>
        {
            4
        }
        Some(ch) if !matches!(ch, '\r' | '\n' | '\'' | '\\') && chars.get(2) == Some(&'\'') => 3,
        _ => 0,
    }
}

fn number_length(chars: &[char]) -> usize {
    if chars.is_empty() {
        return 0;
    }
    if chars[0] == '.' {
        if chars.get(1).is_none_or(|ch| !ch.is_ascii_digit()) {
            return 0;
        }
        let fraction_end = digit_run_end(chars, 1, |ch| ch.is_ascii_digit());
        let exponent_end = exponent_end(chars, fraction_end).unwrap_or(fraction_end);
        return float_suffix_end(chars, exponent_end);
    }
    if !chars[0].is_ascii_digit() {
        return 0;
    }
    if chars[0] == '0' && matches!(chars.get(1), Some('x' | 'X')) {
        return based_integer_length(chars, |ch| ch.is_ascii_hexdigit());
    }
    if chars[0] == '0' && matches!(chars.get(1), Some('b' | 'B')) {
        return based_integer_length(chars, |ch| matches!(ch, '0' | '1'));
    }

    let integer_end = if chars[0] == '0' {
        1
    } else {
        digit_run_end(chars, 0, |ch| ch.is_ascii_digit())
    };
    if chars.get(integer_end) == Some(&'.')
        && chars.get(integer_end + 1) != Some(&'.')
        && chars
            .get(integer_end + 1)
            .is_some_and(|ch| ch.is_ascii_digit())
    {
        let fraction_end = digit_run_end(chars, integer_end + 1, |ch| ch.is_ascii_digit());
        let exponent_end = exponent_end(chars, fraction_end).unwrap_or(fraction_end);
        return float_suffix_end(chars, exponent_end);
    }
    if let Some(end) = exponent_end(chars, integer_end) {
        return float_suffix_end(chars, end);
    }
    if matches!(chars.get(integer_end), Some('f' | 'F')) {
        return integer_end + 1;
    }
    integer_suffix_end(chars, integer_end)
}

fn based_integer_length(chars: &[char], valid_digit: impl Fn(char) -> bool) -> usize {
    if chars.get(2).is_none_or(|ch| !valid_digit(*ch)) {
        return 1;
    }
    let end = digit_run_end(chars, 2, valid_digit);
    integer_suffix_end(chars, end)
}

fn digit_run_end(chars: &[char], start: usize, valid_digit: impl Fn(char) -> bool) -> usize {
    let mut cursor = start;
    let mut last_digit = start;
    while let Some(ch) = chars.get(cursor) {
        if valid_digit(*ch) {
            cursor += 1;
            last_digit = cursor;
        } else if *ch == '_' {
            cursor += 1;
        } else {
            break;
        }
    }
    last_digit
}

fn exponent_end(chars: &[char], start: usize) -> Option<usize> {
    if !matches!(chars.get(start), Some('e' | 'E')) {
        return None;
    }
    let mut digits = start + 1;
    if matches!(chars.get(digits), Some('+' | '-')) {
        digits += 1;
    }
    if chars.get(digits).is_none_or(|ch| !ch.is_ascii_digit()) {
        return None;
    }
    Some(digit_run_end(chars, digits, |ch| ch.is_ascii_digit()))
}

fn float_suffix_end(chars: &[char], end: usize) -> usize {
    if matches!(chars.get(end), Some('f' | 'F')) {
        end + 1
    } else {
        end
    }
}

fn integer_suffix_end(chars: &[char], end: usize) -> usize {
    match chars.get(end) {
        Some('u' | 'U') => {
            if matches!(chars.get(end + 1), Some('l' | 'L')) {
                end + 2
            } else {
                end + 1
            }
        }
        Some('l' | 'L') => end + 1,
        _ => end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(source: &str) -> Vec<String> {
        let (tokens, diagnostics) = lex("test.bd", source);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        tokens
            .into_iter()
            .filter(|token| token.kind != TokenKind::Eof)
            .map(|token| token.text)
            .collect()
    }

    #[test]
    fn lexes_bind_source_on_kotlin_surface() {
        let tokens = texts("server fun get(id: UserId): User? { return db.users.find(id) }");
        assert!(tokens.contains(&"server".into()));
        assert!(tokens.contains(&"?".into()));
    }

    #[test]
    fn supports_unicode_and_escaped_identifiers() {
        assert_eq!(texts("val ação٢ = `when`"), ["val", "ação٢", "=", "when"]);
    }

    #[test]
    fn supports_every_kotlin_numeric_literal_family() {
        assert_eq!(
            texts("0 42 1__000 0xCA_FEuL 0b1010_0011u 42L .5 1.25e-3F 6E10 9f"),
            [
                "0",
                "42",
                "1__000",
                "0xCA_FEuL",
                "0b1010_0011u",
                "42L",
                ".5",
                "1.25e-3F",
                "6E10",
                "9f"
            ]
        );
    }

    #[test]
    fn uses_kotlin_operator_boundaries() {
        assert_eq!(
            texts("a?.b ?: c!! 0..<10 x===y x!==z ::ref ... as? T"),
            [
                "a", "?", ".", "b", "?", ":", "c", "!", "!", "0", "..<", "10", "x", "===", "y",
                "x", "!==", "z", "::", "ref", "...", "as?", "T"
            ]
        );
        assert_eq!(
            texts("value !is String && item !in list"),
            ["value", "!is", "String", "&&", "item", "!in", "list"]
        );
        assert_eq!(texts("return@loop this@`fun`"), ["return@loop", "this@fun"]);
    }

    #[test]
    fn supports_nested_comments_shebang_and_crlf() {
        let source = "#!/usr/bin/env kotlin\r\n/* outer /* nested */ done */\r\nval answer = 42";
        let (tokens, diagnostics) = lex("script.bd", source);
        assert!(diagnostics.is_empty());
        assert_eq!(tokens[0].text, "val");
        assert_eq!(tokens[0].span.line, 3);
    }

    #[test]
    fn supports_characters_strings_raw_strings_and_interpolation() {
        let source = r#"'x' '\n' '\u03A9' "hello\t$name ${call("nested")}" """raw
text ${map["key"]}""""#;
        let (tokens, diagnostics) = lex("strings.bd", source);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            tokens
                .iter()
                .filter(|token| token.kind == TokenKind::Character)
                .count(),
            3
        );
        assert_eq!(
            tokens
                .iter()
                .filter(|token| token.kind == TokenKind::TemplateString)
                .count(),
            2
        );
    }

    #[test]
    fn an_escaped_dollar_is_not_a_template() {
        let kinds = |source: &str| -> Vec<(TokenKind, String)> {
            let (tokens, diagnostics) = lex("dollar.bd", source);
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            tokens
                .into_iter()
                .filter(|token| token.kind != TokenKind::Eof)
                .map(|token| (token.kind, token.text))
                .collect()
        };
        assert_eq!(
            kinds(r#""\$nome" "a\${b}" "$" "$1""#),
            vec![
                (TokenKind::String, "$nome".to_string()),
                (TokenKind::String, "a${b}".to_string()),
                (TokenKind::String, "$".to_string()),
                (TokenKind::String, "$1".to_string()),
            ]
        );
        assert_eq!(
            kinds(r#""\\$x" "$_y""#),
            vec![
                (TokenKind::TemplateString, "\\$x".to_string()),
                (TokenKind::TemplateString, "$_y".to_string()),
            ]
        );
        // A raw string has no escapes.
        assert_eq!(
            kinds(r#""""\$x""""#),
            vec![(TokenKind::TemplateString, "\\$x".to_string())]
        );
    }

    #[test]
    fn rejects_invalid_kotlin_escapes_and_unterminated_constructs() {
        let (_, invalid_escape) = lex("bad.bd", r#""bad\q""#);
        assert!(invalid_escape.iter().any(|item| item.code == "E1006"));
        let (_, comment) = lex("bad.bd", "/* outer /* nested */");
        assert!(comment.iter().any(|item| item.code == "E1001"));
        let (_, identifier) = lex("bad.bd", "`unterminated");
        assert!(identifier.iter().any(|item| item.code == "E1004"));
    }

    #[test]
    fn covers_official_kotlin_keywords() {
        let keywords = "file field property get set receiver param setparam delegate package import class interface fun object val var typealias constructor by companion init this super typeof where if else when try catch finally for do while throw return continue break as is in out dynamic public private protected internal enum sealed annotation data inner value tailrec operator inline infix external suspend override abstract final open const lateinit vararg noinline crossinline reified expect actual true false null";
        assert_eq!(
            texts(keywords),
            keywords.split_whitespace().collect::<Vec<_>>()
        );
    }

    #[test]
    fn covers_official_kotlin_separators_and_operations() {
        let source = "... . , ( ) [ ] { } * % / + - ++ -- && || ! : ; = += -= *= /= %= -> => .. ..< :: ;; # @ ? < > <= >= != !== as? == === ' &";
        assert_eq!(texts(source), source.split_whitespace().collect::<Vec<_>>());
    }

    #[test]
    fn follows_exact_unicode_identifier_categories() {
        assert_eq!(
            texts("Δelta café 東京 nome٢"),
            ["Δelta", "café", "東京", "nome٢"]
        );
        let (_, diagnostics) = lex("unicode.bd", "Ⅳalue");
        assert!(diagnostics.iter().any(|item| item.code == "E1003"));
    }

    fn codes(source: &str) -> Vec<&'static str> {
        lex("t.bd", source)
            .1
            .into_iter()
            .map(|diagnostic| diagnostic.code)
            .collect()
    }

    fn string_value(source: &str) -> (TokenKind, String) {
        let (tokens, diagnostics) = lex("t.bd", source);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        (tokens[0].kind.clone(), tokens[0].text.clone())
    }

    #[test]
    fn escaped_identifiers_must_be_identifiers() {
        // Backticks only let a keyword be a name: the text reaches generated code and paths.
        assert_eq!(texts("fun `fun`(`val`: Int)")[1], "fun");
        assert_eq!(texts("`ação_2`"), ["ação_2"]);
        for payload in [
            "fun `x() -> i64 { 0 } pub fn evil`() = 1",
            "server fun `../../../pwned`(): Int effects [] = 1",
            "val `a b` = 1",
            "val `1a` = 1",
            "return@`a-b` x",
        ] {
            assert_eq!(codes(payload), vec!["E1004"], "{payload}");
        }
    }

    #[test]
    fn surrogate_pair_escapes_are_one_character() {
        assert_eq!(
            string_value(r#""\uD83D\uDE00!""#),
            (TokenKind::String, "😀!".to_string())
        );
        assert_eq!(codes(r#""\uD800""#), vec!["E1006"]);
        assert_eq!(codes(r#""\uDE00\uD83D""#), vec!["E1006", "E1006"]);
        assert_eq!(codes(r#""\uD83Dx""#), vec!["E1006"]);
        // A partial `\u` escape does not swallow the closing quote.
        let (tokens, diagnostics) = lex("t.bd", r#""\u12" x"#);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(tokens[1].text, "x");
    }

    #[test]
    fn templates_follow_kotlin_field_identifiers() {
        assert_eq!(string_value(r#""$`x`""#).0, TokenKind::TemplateString);
        assert_eq!(string_value(r#""$``""#).0, TokenKind::String);
        // `Ⅳ` is a letter number (Nl), not a Kotlin `Letter`.
        assert_eq!(string_value("\"$Ⅳ\"").0, TokenKind::String);
        assert_eq!(string_value("\"$ação\"").0, TokenKind::TemplateString);
    }

    #[test]
    fn interpolation_nesting_is_bounded() {
        let depth = 10_000;
        let source = format!("{}1{}", "\"${".repeat(depth), "}\"".repeat(depth));
        let found = codes(&source);
        assert_eq!(found, vec!["E1007"]);
        let source = format!("{}1{}", "\"${".repeat(200), "}\"".repeat(200));
        assert!(codes(&source).is_empty());
    }

    #[test]
    fn invalid_escapes_are_reported_in_linear_time() {
        let source = format!("\"{}\"", "\\q".repeat(50_000));
        let started = std::time::Instant::now();
        assert_eq!(codes(&source).len(), 50_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn invalid_character_form_falls_back_to_single_quote_tokens() {
        let (tokens, diagnostics) = lex("char.bd", "'ab'");
        assert!(diagnostics.is_empty());
        assert_eq!(tokens.iter().filter(|token| token.text == "'").count(), 2);
    }
}
