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
                        format!("Unexpected top-level declaration `{}`.", self.peek_text()),
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
            variants: vec![],
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
            variants: vec![],
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
            variants: cases.iter().map(|case| case.name.clone()).collect(),
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
                    self.peek_text()
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
            variants: cases.iter().map(|case| case.name.clone()).collect(),
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
        let mut domain = Domain::Pure;
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
        let mut requires = Vec::new();
        loop {
            if self.consume("effects") {
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
            requires,
            body_kind,
            body,
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
        while !self.at_eof() && !self.consume("]") {
            names.push(self.qualified_name());
            self.consume(",");
        }
        names
    }

    fn type_ref(&mut self) -> Option<TypeRef> {
        if self.peek_text() == "(" {
            let mut name = self.balanced_group();
            if self.consume("->") {
                let result = self.type_ref()?;
                name.push_str("->");
                name.push_str(&result.display());
                return Some(TypeRef {
                    name,
                    nullable: false,
                });
            }
            let nullable = self.question_marks(&mut name);
            return Some(TypeRef { name, nullable });
        }
        let mut name = self.identifier()?;
        if self.consume("<") {
            let mut depth = 1;
            name.push('<');
            while !self.at_eof() && depth > 0 {
                let text = self.advance().text.clone();
                if text == "<" {
                    depth += 1;
                }
                if text == ">" {
                    depth -= 1;
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
            if token.text == "{" {
                depth += 1;
            }
            if token.text == "}" {
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
                format!("Expected identifier, found `{}`.", self.peek_text()),
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
                format!("Expected `{text}`, found `{}`.", self.peek_text()),
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
    fn peek_text(&self) -> &str {
        &self.peek().text
    }
    fn peek_n(&self, n: usize) -> &str {
        self.tokens
            .get(self.index + n)
            .map(|t| t.text.as_str())
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
        assert_eq!(declaration.variants, vec!["No", "Folha"]);
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
