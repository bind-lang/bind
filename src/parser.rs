pub mod expr;

use crate::ast::*;
use crate::diagnostic::Diagnostic;
use crate::lexer::{Token, TokenKind};

pub fn parse(tokens: Vec<Token>) -> (Program, Vec<Diagnostic>) {
    Parser {
        tokens,
        index: 0,
        diagnostics: Vec::new(),
    }
    .program()
}

/// Deepest nesting of function types in a declaration's type (as in the expression parser).
const MAX_TYPE_DEPTH: usize = 256;

struct Parser {
    tokens: Vec<Token>,
    index: usize,
    diagnostics: Vec<Diagnostic>,
}

impl Parser {
    fn program(mut self) -> (Program, Vec<Diagnostic>) {
        let mut program = Program::default();
        while !self.at_eof() {
            let start = self.index;
            match self.peek_text() {
                "package" => {
                    self.advance();
                    program.package = Some(self.qualified_name());
                }
                "import" => self.skip_line(),
                "distinct" => {
                    if let Some(item) = self.distinct_type() {
                        program.types.push(item);
                    }
                }
                "data" if self.peek_n(1) == "class" => {
                    if let Some(item) = self.record_type(TypeKind::Data, 2) {
                        program.types.push(item);
                    }
                }
                "entity" => {
                    if let Some(item) = self.record_type(TypeKind::Entity, 1) {
                        program.types.push(item);
                    }
                }
                "sealed" if self.peek_n(1) == "class" => {
                    if let Some(item) = self.sealed_type() {
                        program.types.push(item);
                    }
                }
                "enum" if self.peek_n(1) == "class" => {
                    if let Some(item) = self.enum_type() {
                        program.types.push(item);
                    }
                }
                "environment" => program.environment.append(&mut self.environment()),
                "pure" | "client" | "server" | "worker" | "database" | "external" | "suspend"
                | "fun" => {
                    if let Some(function) = self.function() {
                        program.functions.push(function);
                    }
                }
                _ => {
                    self.diagnostics.push(Diagnostic::error(
                        "E1101",
                        format!(
                            "Unexpected top-level declaration `{}`.",
                            self.peek_display()
                        ),
                        self.peek().span.clone(),
                    ));
                    self.skip_declaration();
                }
            }
            if self.index == start {
                self.advance();
            }
        }
        (program, self.diagnostics)
    }

    fn distinct_type(&mut self) -> Option<TypeDecl> {
        let span = self.peek().span.clone();
        self.expect("distinct")?;
        self.expect("type")?;
        let name = self.identifier()?;
        self.expect("=")?;
        let underlying = self.type_ref()?;
        Some(TypeDecl {
            name,
            kind: TypeKind::Distinct,
            type_params: vec![],
            underlying: Some(underlying),
            fields: vec![],
            cases: vec![],
            span,
        })
    }

    fn record_type(&mut self, kind: TypeKind, prefix: usize) -> Option<TypeDecl> {
        let span = self.peek().span.clone();
        for _ in 0..prefix {
            self.advance();
        }
        let name = self.identifier()?;
        let type_params = self.type_params();
        let fields = if self.consume("(") {
            self.fields(")")
        } else if self.consume("{") {
            self.fields("}")
        } else {
            Vec::new()
        };
        Some(TypeDecl {
            name,
            kind,
            type_params,
            underlying: None,
            fields,
            cases: vec![],
            span,
        })
    }

    fn fields(&mut self, close: &str) -> Vec<Field> {
        let mut fields = Vec::new();
        while !self.at_eof() && !self.consume(close) {
            let mutable = if self.consume("val") {
                false
            } else {
                self.consume("var")
            };
            let span = self.peek().span.clone();
            let Some(name) = self.identifier() else {
                self.recover_to(&[",", close]);
                self.consume(",");
                continue;
            };
            if self.expect(":").is_none() {
                self.recover_to(&[",", close]);
                self.consume(",");
                continue;
            }
            if let Some(type_ref) = self.type_ref() {
                fields.push(Field {
                    name,
                    type_ref,
                    mutable,
                    span,
                });
            }
            if self.consume("=") {
                self.recover_to(&[",", close]);
            }
            self.consume(",");
        }
        fields
    }

    fn sealed_type(&mut self) -> Option<TypeDecl> {
        let span = self.peek().span.clone();
        self.advance();
        self.advance();
        let name = self.identifier()?;
        let type_params = self.type_params();
        let mut cases = Vec::new();
        if self.consume("{") {
            while !self.at_eof() && !self.consume("}") {
                if self.consume(";") {
                    continue;
                }
                match self.sealed_case() {
                    Some(case) => cases.push(case),
                    None => self.recover_to(&["data", "class", "object", "}"]),
                }
            }
        }
        Some(TypeDecl {
            name,
            kind: TypeKind::Sealed,
            type_params,
            underlying: None,
            fields: vec![],
            cases,
            span,
        })
    }

    fn sealed_case(&mut self) -> Option<Variant> {
        let span = self.peek().span.clone();
        self.consume("data");
        let has_fields = if self.consume("class") {
            true
        } else if self.consume("object") {
            false
        } else {
            self.diagnostics.push(Diagnostic::error(
                "E1104",
                format!(
                    "Expected a sealed variant (`data class` or `data object`), found `{}`.",
                    self.peek_display()
                ),
                self.peek().span.clone(),
            ));
            self.advance();
            return None;
        };
        let name = self.identifier()?;
        let fields = if has_fields && self.consume("(") {
            self.fields(")")
        } else {
            Vec::new()
        };
        if self.consume(":") {
            let _ = self.type_ref();
            if self.consume("(") {
                self.recover_to(&[")"]);
                self.consume(")");
            }
        }
        Some(Variant { name, fields, span })
    }

    fn enum_type(&mut self) -> Option<TypeDecl> {
        let span = self.peek().span.clone();
        self.advance();
        self.advance();
        let name = self.identifier()?;
        let mut cases = Vec::new();
        if self.consume("{") {
            while !self.at_eof() && !self.consume("}") {
                if self.peek().kind == TokenKind::Identifier {
                    let token = self.advance().clone();
                    cases.push(Variant {
                        name: token.text,
                        fields: vec![],
                        span: token.span,
                    });
                } else {
                    self.advance();
                }
                self.consume(",");
            }
        }
        Some(TypeDecl {
            name,
            kind: TypeKind::Enum,
            type_params: vec![],
            underlying: None,
            fields: vec![],
            cases,
            span,
        })
    }

    fn environment(&mut self) -> Vec<EnvironmentDecl> {
        self.advance();
        if self.peek_text() != "{" {
            self.advance();
        }
        if !self.consume("{") {
            return vec![];
        }
        let body = self.block_contents();
        let mut nested = Parser {
            tokens: with_eof(body),
            index: 0,
            diagnostics: Vec::new(),
        };
        let mut result = Vec::new();
        while !nested.at_eof() {
            let start = nested.index;
            let mut lifecycle = "runtime".to_string();
            let mut secret = false;
            let mut public = false;
            loop {
                match nested.peek_text() {
                    "compile" | "runtime" | "compile_and_runtime" => {
                        lifecycle = nested.advance().text.clone()
                    }
                    "secret" => {
                        secret = true;
                        nested.advance();
                    }
                    "public" => {
                        public = true;
                        lifecycle = "public".into();
                        nested.advance();
                    }
                    _ => break,
                }
            }
            if nested.consume("val") || nested.consume("var") {
                let span = nested.peek().span.clone();
                if let (Some(name), Some(_), Some(type_ref)) =
                    (nested.identifier(), nested.expect(":"), nested.type_ref())
                {
                    nested.expect("=");
                    let optional = nested.peek_text() == "envOrNull";
                    if optional || nested.peek_text() == "env" {
                        nested.advance();
                        nested.expect("(");
                        let variable = if nested.peek().kind == TokenKind::String {
                            nested.advance().text.clone()
                        } else {
                            String::new()
                        };
                        let mut default = None;
                        while !nested.at_eof() && !nested.consume(")") {
                            if nested.consume("default") {
                                nested.consume("=");
                                default = Some(nested.advance().text.clone());
                            } else {
                                nested.advance();
                            }
                        }
                        result.push(EnvironmentDecl {
                            name,
                            variable,
                            type_ref,
                            lifecycle,
                            secret,
                            public,
                            optional,
                            default,
                            span,
                        });
                    }
                }
            }
            if nested.index == start {
                nested.advance();
            }
        }
        self.diagnostics.append(&mut nested.diagnostics);
        result
    }

    fn function(&mut self) -> Option<Function> {
        let span = self.peek().span.clone();
        let mut domain = Domain::Inferred;
        let mut suspend = false;
        loop {
            match self.peek_text() {
                "pure" => {
                    domain = Domain::Pure;
                    self.advance();
                }
                "client" => {
                    domain = Domain::Client;
                    self.advance();
                }
                "server" => {
                    domain = Domain::Server;
                    self.advance();
                }
                "worker" => {
                    domain = Domain::Worker;
                    self.advance();
                }
                "database" => {
                    domain = Domain::Database;
                    self.advance();
                }
                "external" => {
                    domain = Domain::External;
                    self.advance();
                }
                "suspend" => {
                    suspend = true;
                    self.advance();
                }
                _ => break,
            }
        }
        let (component, type_params) = if self.consume("component") {
            (true, Vec::new())
        } else {
            self.expect("fun")?;
            (false, self.type_params())
        };
        let name = self.identifier()?;
        self.expect("(")?;
        let mut parameters = Vec::new();
        while !self.at_eof() && !self.consume(")") {
            let parameter_name = self.identifier()?;
            let type_ref = if self.consume(":") {
                Some(self.type_ref()?)
            } else {
                None
            };
            parameters.push(Parameter {
                name: parameter_name,
                type_ref,
            });
            if self.peek_text() == "=" {
                self.diagnostics.push(Diagnostic::error(
                    "E7004",
                    "Default parameter values are not supported.",
                    self.peek().span.clone(),
                ));
                self.advance();
                if !self.skip_default_value() {
                    return None;
                }
            }
            self.consume(",");
        }
        let return_type = if self.consume(":") {
            Some(self.type_ref()?)
        } else {
            None
        };
        let mut effects = Vec::new();
        let mut effects_declared = false;
        let mut requires = Vec::new();
        loop {
            if self.consume("effects") {
                effects_declared = true;
                effects = self.name_list();
            } else if self.consume("requires") {
                requires = self.name_list();
            } else {
                break;
            }
        }
        let (body_kind, body) = if self.consume("{") {
            (BodyKind::Block, self.block_contents())
        } else if self.consume("=") {
            (BodyKind::Expression, self.expression_body(span.line))
        } else {
            (BodyKind::Absent, Vec::new())
        };
        let expr = match body_kind {
            BodyKind::Block => {
                let (block, mut diagnostics) = expr::parse_block_tokens(&body, &span);
                self.diagnostics.append(&mut diagnostics);
                Some(block)
            }
            BodyKind::Expression => {
                let (block, mut diagnostics) = expr::parse_expression_tokens(&body, &span);
                self.diagnostics.append(&mut diagnostics);
                Some(block)
            }
            BodyKind::Absent => None,
        };
        Some(Function {
            name,
            domain,
            component,
            suspend,
            type_params,
            parameters,
            return_type,
            effects,
            effects_declared,
            requires,
            body_kind,
            expr,
            span,
        })
    }

    fn type_params(&mut self) -> Vec<String> {
        let mut params = Vec::new();
        if !self.consume("<") {
            return params;
        }
        while !self.at_eof() && !self.consume(">") {
            if let Some(name) = self.identifier() {
                params.push(name);
            } else {
                self.recover_to(&[",", ">"]);
            }
            self.consume(",");
        }
        params
    }

    fn balanced_group(&mut self) -> String {
        let mut text = String::new();
        let mut depth = 0usize;
        loop {
            let token = self.advance().text.clone();
            if token == "(" {
                depth += 1;
            }
            if token == ")" {
                depth = depth.saturating_sub(1);
            }
            text.push_str(&token);
            if token == "," {
                text.push(' ');
            }
            if depth == 0 || self.at_eof() {
                break;
            }
        }
        text
    }

    fn name_list(&mut self) -> Vec<String> {
        let mut names = Vec::new();
        if !self.consume("[") {
            return names;
        }
        self.names_until_bracket(&mut names);
        names
    }

    /// Comma-separated qualified names up to and including `]`. A token that is not a name
    /// is reported once; the rest of the list is skipped, up to `]` or the end of the line,
    /// without consuming a `)` that closes an enclosing parameter list. A list still open at
    /// the end of the file, or when a declaration starts on a later line, is reported.
    fn names_until_bracket(&mut self, names: &mut Vec<String>) {
        let line = self.previous_line();
        loop {
            if self.consume("]") {
                return;
            }
            if self.at_eof()
                || (self.peek().span.line > line && is_top_level_start(self.peek_text()))
            {
                self.diagnostics.push(Diagnostic::error(
                    "E1103",
                    format!(
                        "Expected `]` to close the effect list, found `{}`.",
                        self.peek_display()
                    ),
                    self.peek().span.clone(),
                ));
                return;
            }
            let start = self.index;
            let name = self.qualified_name();
            self.consume(",");
            if self.index == start {
                let line = self.peek().span.line;
                while !self.at_eof()
                    && self.peek().span.line == line
                    && self.peek_text() != ")"
                    && !self.consume("]")
                {
                    self.advance();
                }
                return;
            }
            names.push(name);
        }
    }

    /// Line of the token just consumed (the current one at the start of the file).
    fn previous_line(&self) -> usize {
        self.tokens
            .get(self.index.saturating_sub(1))
            .map_or(0, |token| token.span.line)
    }

    fn type_ref(&mut self) -> Option<TypeRef> {
        self.type_ref_at(0)
    }

    /// `depth` counts the function types this one is the result of (`() -> () -> …`), bounded
    /// like every other nesting so that hostile input is reported instead of overflowing.
    fn type_ref_at(&mut self, depth: usize) -> Option<TypeRef> {
        if depth >= MAX_TYPE_DEPTH {
            self.diagnostics.push(Diagnostic::error(
                "E7004",
                "Type is nested too deeply (limit 256).",
                self.peek().span.clone(),
            ));
            return None;
        }
        if self.peek_text() == "(" {
            let mut name = self.balanced_group();
            if self.consume("->") {
                let result = self.type_ref_at(depth + 1)?;
                name.push_str("->");
                name.push_str(&result.display());
                if self.peek_text() == "!" && self.peek_n(1) == "[" {
                    self.advance();
                    self.advance();
                    let mut effects = Vec::new();
                    self.names_until_bracket(&mut effects);
                    name.push_str(&format!("![{}]", effects.join(",")));
                }
                return Some(TypeRef {
                    name,
                    nullable: false,
                });
            }
            let nullable = self.question_marks(&mut name);
            return Some(TypeRef { name, nullable });
        }
        let mut name = self.identifier()?;
        if self.peek_text() == "<" {
            let open = self.advance().span.clone();
            let mut depth = 1;
            let mut parens = 0usize;
            name.push('<');
            while depth > 0 {
                // A type argument list never contains these: stop there instead of
                // swallowing the rest of the file when the `>` is missing.
                let text = structural_text(self.peek());
                if self.at_eof() || matches!(text, "{" | "}" | "=") || (text == ")" && parens == 0)
                {
                    self.diagnostics.push(Diagnostic::error(
                        "E7005",
                        "Expected `>` to close the type arguments opened here.",
                        open,
                    ));
                    return None;
                }
                let text = self.advance().text.clone();
                match text.as_str() {
                    "<" => depth += 1,
                    ">" => depth -= 1,
                    "(" => parens += 1,
                    ")" => parens -= 1,
                    _ => {}
                }
                name.push_str(&text);
            }
        }
        let nullable = self.question_marks(&mut name);
        Some(TypeRef { name, nullable })
    }

    /// Consumes trailing `?` markers; the first sets nullability, each extra one nests another
    /// option by appending `?` to the name (so `Int??` is `{ name: "Int?", nullable: true }`).
    fn question_marks(&mut self, name: &mut String) -> bool {
        let mut nullable = false;
        while self.consume("?") {
            if nullable {
                name.push('?');
            }
            nullable = true;
        }
        nullable
    }

    fn block_contents(&mut self) -> Vec<Token> {
        let mut depth = 1;
        let mut output = Vec::new();
        while !self.at_eof() && depth > 0 {
            let token = self.advance().clone();
            if structural_text(&token) == "{" {
                depth += 1;
            }
            if structural_text(&token) == "}" {
                depth -= 1;
            }
            if depth > 0 {
                output.push(token);
            }
        }
        output
    }

    fn expression_body(&mut self, declaration_line: usize) -> Vec<Token> {
        let mut output = Vec::new();
        while !self.at_eof() {
            if !output.is_empty()
                && self.peek().span.line > declaration_line
                && is_top_level_start(self.peek_text())
            {
                break;
            }
            output.push(self.advance().clone());
        }
        output
    }

    fn qualified_name(&mut self) -> String {
        let mut name = self.identifier().unwrap_or_default();
        while self.consume(".") {
            name.push('.');
            name.push_str(&self.identifier().unwrap_or_default());
        }
        name
    }

    fn identifier(&mut self) -> Option<String> {
        if self.peek().kind == TokenKind::Identifier {
            Some(self.advance().text.clone())
        } else {
            self.diagnostics.push(Diagnostic::error(
                "E1102",
                format!("Expected identifier, found `{}`.", self.peek_display()),
                self.peek().span.clone(),
            ));
            None
        }
    }

    fn expect(&mut self, text: &str) -> Option<()> {
        if self.consume(text) {
            Some(())
        } else {
            self.diagnostics.push(Diagnostic::error(
                "E1103",
                format!("Expected `{text}`, found `{}`.", self.peek_display()),
                self.peek().span.clone(),
            ));
            None
        }
    }

    fn consume(&mut self, text: &str) -> bool {
        if self.peek_text() == text {
            self.advance();
            true
        } else {
            false
        }
    }
    fn peek(&self) -> &Token {
        &self.tokens[self.index.min(self.tokens.len() - 1)]
    }
    /// The text of the next token when it is a keyword, name, symbol or number; `""` for a
    /// string or character literal, so a literal `"{"` or `"fun"` is never taken for syntax.
    fn peek_text(&self) -> &str {
        structural_text(self.peek())
    }
    /// The next token as written in a message.
    fn peek_display(&self) -> &str {
        &self.peek().text
    }
    fn peek_n(&self, n: usize) -> &str {
        self.tokens
            .get(self.index + n)
            .map(structural_text)
            .unwrap_or("")
    }
    fn advance(&mut self) -> &Token {
        let current = self.index;
        if !self.at_eof() {
            self.index += 1;
        }
        &self.tokens[current]
    }
    fn at_eof(&self) -> bool {
        self.peek().kind == TokenKind::Eof
    }

    /// Skips a default parameter value up to the `,` or `)` that ends it (outside nested
    /// brackets). Stops early, returning false, at a new line that starts a top-level
    /// declaration, so an unclosed parameter list does not swallow the next declaration.
    fn skip_default_value(&mut self) -> bool {
        let mut depth = 0usize;
        while !self.at_eof() {
            let starts_line =
                self.index > 0 && self.peek().span.line != self.tokens[self.index - 1].span.line;
            if starts_line && is_top_level_start(self.peek_text()) {
                return false;
            }
            match self.peek_text() {
                "," | ")" if depth == 0 => return true,
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => depth = depth.saturating_sub(1),
                _ => {}
            }
            self.advance();
        }
        false
    }

    fn recover_to(&mut self, texts: &[&str]) {
        while !self.at_eof() && !texts.contains(&self.peek_text()) {
            self.advance();
        }
    }
    fn skip_line(&mut self) {
        let line = self.peek().span.line;
        while !self.at_eof() && self.peek().span.line == line {
            self.advance();
        }
    }
    fn skip_declaration(&mut self) {
        if self.consume("{") {
            self.block_contents();
            return;
        }
        let line = self.peek().span.line;
        while !self.at_eof() && self.peek().span.line == line {
            self.advance();
        }
    }
}

fn with_eof(mut tokens: Vec<Token>) -> Vec<Token> {
    let span = tokens
        .last()
        .map(|token| token.span.clone())
        .unwrap_or_default();
    tokens.push(Token {
        kind: TokenKind::Eof,
        text: String::new(),
        span,
    });
    tokens
}

/// A token's text if it can be syntax (keyword, name, symbol, number), else `""`.
fn structural_text(token: &Token) -> &str {
    match token.kind {
        TokenKind::Identifier | TokenKind::Symbol | TokenKind::Number => &token.text,
        TokenKind::String | TokenKind::TemplateString | TokenKind::Character | TokenKind::Eof => "",
    }
}

fn is_top_level_start(text: &str) -> bool {
    [
        "package",
        "import",
        "distinct",
        "data",
        "entity",
        "sealed",
        "enum",
        "environment",
        "pure",
        "client",
        "server",
        "worker",
        "fun",
        "external",
        "database",
    ]
    .contains(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn parse_source(source: &str) -> Program {
        let (tokens, lex_diagnostics) = lex("test.bd", source);
        assert!(lex_diagnostics.is_empty(), "{lex_diagnostics:?}");
        let (program, diagnostics) = parse(tokens);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        program
    }

    /// String literals that spell syntax (`"{"`, `"}"`, `"fun"`, `"->"`, `":"`) are never
    /// taken for it (found by the differential test: a `"{"` in a block body unbalanced it).
    #[test]
    fn string_literals_are_not_syntax() {
        let program = parse_source(
            "fun f(s: String): String {\n    val a = when (s) {\n        \"{\" -> \"a\"\n        _ -> \"b\"\n    }\n    a\n}\nfun g(b: Boolean): String = if (b) \"a\" else\n    \"fun\"\n",
        );
        let names: Vec<&str> = program.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["f", "g"]);
        // `?` followed by the string `":"` is not `?:`; `{ "->" }` is a block, not a lambda.
        for (source, message) in [
            ("fun h(x: String?): String = x ?\":\"\n", "before `?`"),
            (
                "fun k() = map(listOf(1), { \"->\" })\n",
                "A block cannot be used",
            ),
        ] {
            let (tokens, _) = lex("test.bd", source);
            let (_, diagnostics) = parse(tokens);
            assert!(
                diagnostics.iter().any(|d| d.message.contains(message)),
                "{source}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn non_identifier_in_effect_lists_terminates_with_an_error() {
        for source in [
            "fun g(f: (Int) -> Int ! [1]): Int = 1\nfun h() = 2\n",
            "server fun f(): Int effects [1] = 1\nfun h() = 2\n",
            "server fun f(): Int requires [1] = 1\nfun h() = 2\n",
        ] {
            let (tokens, _) = lex("test.bd", source);
            let (program, diagnostics) = parse(tokens);
            assert!(!diagnostics.is_empty(), "{source}");
            assert_eq!(program.functions.len(), 2, "{source}");
            assert_eq!(program.functions[1].name, "h", "{source}");
        }
    }

    #[test]
    fn unterminated_effect_lists_are_reported() {
        for source in [
            "server fun f(): Int effects [",
            "server fun f(): Int effects [db.read",
            "server fun f(): Int effects [db.read,",
            "fun g(f: () -> Int ! [",
        ] {
            let (tokens, _) = lex("test.bd", source);
            let (_, diagnostics) = parse(tokens);
            assert!(!diagnostics.is_empty(), "{source}");
        }
    }

    #[test]
    fn effect_list_recovery_stops_at_the_end_of_the_line() {
        for source in [
            "fun g(f: () -> Int ! [)\nfun h() = 2\n",
            "fun g(f: (Int) -> Int ! [db.read)\nfun h() = 2\n",
            "server fun f(): Int effects [db.read\nfun h() = 2\n",
        ] {
            let (tokens, _) = lex("test.bd", source);
            let (program, diagnostics) = parse(tokens);
            assert!(!diagnostics.is_empty(), "{source}");
            assert_eq!(
                program
                    .functions
                    .last()
                    .map(|function| function.name.as_str()),
                Some("h"),
                "{source}"
            );
        }
    }

    #[test]
    fn plain_functions_have_an_inferred_domain() {
        let program = parse_source(
            "fun a() = 1\npure fun b() = 1\nserver fun c(): Int effects [] = 1\nserver fun d(): Int = 1\n",
        );
        let domains: Vec<Domain> = program
            .functions
            .iter()
            .map(|function| function.domain.clone())
            .collect();
        assert_eq!(
            domains,
            vec![
                Domain::Inferred,
                Domain::Pure,
                Domain::Server,
                Domain::Server
            ]
        );
        let declared: Vec<bool> = program
            .functions
            .iter()
            .map(|function| function.effects_declared)
            .collect();
        assert_eq!(declared, vec![false, false, true, false]);
    }

    #[test]
    fn function_types_carry_effect_rows() {
        let program =
            parse_source("fun g(f: (Int) -> Int ! [db.read, e], h: (Int) -> Int): Int = 1\n");
        let parameters = &program.functions[0].parameters;
        assert_eq!(
            parameters[0].declared_type().display(),
            "(Int)->Int![db.read,e]"
        );
        assert_eq!(parameters[1].declared_type().display(), "(Int)->Int");
    }

    #[test]
    fn parses_nested_optional_types() {
        let program = parse_source("fun f(a: Int??, b: Int?): Int? = a ?: b\n");
        let function = &program.functions[0];
        assert_eq!(function.parameters[0].declared_type().display(), "Int??");
        assert_eq!(function.parameters[1].declared_type().display(), "Int?");
    }

    #[test]
    fn parses_unannotated_parameters_and_generics() {
        let program =
            parse_source("fun dobro(x) = x * 2\nfun <T> primeiro(xs: List<T>): T? = first(xs)\n");
        assert_eq!(program.functions[0].parameters[0].type_ref, None);
        assert_eq!(program.functions[0].return_type, None);
        assert_eq!(program.functions[0].body_kind, BodyKind::Expression);
        assert_eq!(program.functions[1].type_params, vec!["T"]);
        assert_eq!(program.functions[1].declared_return().display(), "T?");
    }

    #[test]
    fn parses_sealed_variants_with_fields_and_type_parameters() {
        let program = parse_source(
            "sealed class Arvore<T> {\n    data class No(val esq: Arvore<T>, val valor: T, val dir: Arvore<T>)\n    data object Folha\n}\n",
        );
        let declaration = &program.types[0];
        assert_eq!(declaration.type_params, vec!["T"]);
        let names: Vec<_> = declaration
            .cases
            .iter()
            .map(|case| case.name.as_str())
            .collect();
        assert_eq!(names, vec!["No", "Folha"]);
        assert_eq!(declaration.cases[0].fields.len(), 3);
        assert_eq!(declaration.cases[0].fields[1].type_ref.display(), "T");
        assert!(declaration.cases[1].fields.is_empty());
    }

    #[test]
    fn parses_function_types_in_annotations() {
        let program =
            parse_source("fun aplica(f: (Int) -> Int, g: ((Int) -> Int)?, x: Int): Int = f(x)\n");
        let parameters = &program.functions[0].parameters;
        assert_eq!(parameters[0].declared_type().display(), "(Int)->Int");
        assert_eq!(parameters[1].declared_type().display(), "((Int)->Int)?");
        assert_eq!(program.functions[0].body_kind, BodyKind::Expression);
    }

    #[test]
    fn functions_without_body_are_absent() {
        let program = parse_source("external fun relogio(): Int\n");
        assert_eq!(program.functions[0].body_kind, BodyKind::Absent);
    }

    #[test]
    fn parses_core_declarations() {
        let source = r#"
            distinct type CustomerId = UUID
            data class Customer(val id: CustomerId, val name: String)
            server fun getCustomer(id: CustomerId): Customer effects [db.read] {
                return db.customers.require(id)
            }
        "#;
        let (tokens, _) = lex("test.bd", source);
        let (program, diagnostics) = parse(tokens);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(program.types.len(), 2);
        assert_eq!(program.functions[0].effects, vec!["db.read"]);
    }

    #[test]
    fn attaches_parsed_bodies_to_functions() {
        let program = parse_source(
            "fun um() = 1\nfun dois(): Int {\n    val x = 1\n    x + 1\n}\nexternal fun tres(): Int\n",
        );
        let rendered: Vec<Option<String>> = program
            .functions
            .iter()
            .map(|function| {
                function
                    .expr
                    .as_ref()
                    .map(crate::syntax::expr::render_block)
            })
            .collect();
        assert_eq!(
            rendered,
            vec![
                Some("{ 1 }".into()),
                Some("{ val x = 1; (x + 1) }".into()),
                None
            ]
        );
    }

    fn parse_with_codes(source: &str) -> (Program, Vec<&'static str>) {
        let (tokens, _) = lex("test.bd", source);
        let (program, diagnostics) = parse(tokens);
        (program, diagnostics.iter().map(|item| item.code).collect())
    }

    #[test]
    fn deep_function_types_in_declarations_are_bounded() {
        let source = format!("fun f(p: {}Int): Int = 1\n", "() -> ".repeat(100_000));
        let (program, codes) = parse_with_codes(&source);
        assert!(program.functions.is_empty());
        assert_eq!(codes.first(), Some(&"E7004"));
        let source = format!("fun f(p: {}Int): Int = 1\n", "() -> ".repeat(10));
        assert!(parse_with_codes(&source).1.is_empty());
    }

    #[test]
    fn unclosed_type_arguments_do_not_swallow_the_file() {
        let (program, codes) = parse_with_codes("fun f(x: List<Int): Int = 1\nfun g(): Int = 2\n");
        assert_eq!(codes.first(), Some(&"E7005"));
        assert!(
            program
                .functions
                .iter()
                .any(|function| function.name == "g")
        );
        let program =
            parse_source("fun f(x: List<(Int) -> Int>, y: Map<String, List<Int>>): Int = 1\n");
        assert_eq!(
            program.functions[0].parameters[0].declared_type().display(),
            "List<(Int)->Int>"
        );
    }

    #[test]
    fn default_parameter_values_are_rejected() {
        let (program, codes) = parse_with_codes("fun f(x: Int = \"oops\"): Int = x\n");
        assert_eq!(codes, vec!["E7004"]);
        assert_eq!(program.functions[0].parameters.len(), 1);
        let (program, codes) = parse_with_codes("fun f(x: Int = g(1, 2), y: Int): Int = x\n");
        assert_eq!(codes, vec!["E7004"]);
        assert_eq!(program.functions[0].parameters.len(), 2);
        let (program, codes) = parse_with_codes("fun f(x: Int = 1\nfun g(): Int = 2\n");
        assert_eq!(codes, vec!["E7004"]);
        let names: Vec<&str> = program
            .functions
            .iter()
            .map(|function| function.name.as_str())
            .collect();
        assert!(names.contains(&"g"), "{names:?}");
        assert_eq!(
            program
                .functions
                .iter()
                .find(|function| function.name == "g")
                .unwrap()
                .body_kind,
            BodyKind::Expression
        );
    }

    #[test]
    fn reports_body_syntax_errors() {
        let (tokens, _) = lex("test.bd", "fun f(): Int {\n    var x = 1\n    x\n}\n");
        let (_, diagnostics) = parse(tokens);
        assert_eq!(
            diagnostics.iter().map(|item| item.code).collect::<Vec<_>>(),
            vec!["E7001"]
        );
    }
}
