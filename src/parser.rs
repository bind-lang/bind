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
            underlying: Some(underlying),
            fields: vec![],
            variants: vec![],
            span,
        })
    }

    fn record_type(&mut self, kind: TypeKind, prefix: usize) -> Option<TypeDecl> {
        let span = self.peek().span.clone();
        for _ in 0..prefix {
            self.advance();
        }
        let name = self.identifier()?;
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
            underlying: None,
            fields,
            variants: vec![],
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
        let mut variants = Vec::new();
        if self.consume("{") {
            let body = self.block_contents();
            for window in body.windows(3) {
                if window[0].text == "data"
                    && (window[1].text == "class" || window[1].text == "object")
                {
                    variants.push(window[2].text.clone());
                }
            }
        }
        Some(TypeDecl {
            name,
            kind: TypeKind::Sealed,
            underlying: None,
            fields: vec![],
            variants,
            span,
        })
    }

    fn enum_type(&mut self) -> Option<TypeDecl> {
        let span = self.peek().span.clone();
        self.advance();
        self.advance();
        let name = self.identifier()?;
        let mut variants = Vec::new();
        if self.consume("{") {
            while !self.at_eof() && !self.consume("}") {
                if self.peek().kind == TokenKind::Identifier {
                    variants.push(self.advance().text.clone());
                } else {
                    self.advance();
                }
                self.consume(",");
            }
        }
        Some(TypeDecl {
            name,
            kind: TypeKind::Enum,
            underlying: None,
            fields: vec![],
            variants,
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
        let component = if self.consume("component") {
            true
        } else {
            self.expect("fun")?;
            false
        };
        let name = self.identifier()?;
        self.expect("(")?;
        let mut parameters = Vec::new();
        while !self.at_eof() && !self.consume(")") {
            let parameter_name = self.identifier()?;
            self.expect(":")?;
            let type_ref = self.type_ref()?;
            parameters.push(Parameter {
                name: parameter_name,
                type_ref,
            });
            if self.consume("=") {
                self.recover_to(&[",", ")"]);
            }
            self.consume(",");
        }
        let return_type = if self.consume(":") {
            self.type_ref()?
        } else {
            TypeRef::unit()
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
        let body = if self.consume("{") {
            self.block_contents()
        } else if self.consume("=") {
            self.expression_body(span.line)
        } else {
            Vec::new()
        };
        Some(Function {
            name,
            domain,
            component,
            suspend,
            parameters,
            return_type,
            effects,
            requires,
            body,
            span,
        })
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
        let nullable = self.consume("?");
        Some(TypeRef { name, nullable })
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
    ]
    .contains(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

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
}
