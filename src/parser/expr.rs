//! Expression parser: turns the tokens of a function body into the expression AST.
//!
//! Newlines end statements, as in Kotlin: an infix operator continues an expression only
//! when it is on the same line as the previous token, except `&&`, `||` and `?:`.
//! Inside parentheses newlines are insignificant.

use crate::diagnostic::{Diagnostic, Span};
use crate::lexer::{Token, TokenKind, lex};
use crate::syntax::expr::*;

pub fn parse_expression_tokens(tokens: &[Token], fallback: &Span) -> (Block, Vec<Diagnostic>) {
    let mut parser = ExprParser::new(tokens, fallback);
    let span = parser.peek().span.clone();
    let mut stmts = Vec::new();
    if let Some(expr) = parser.expression() {
        stmts.push(Stmt::Expr(expr));
    }
    if !parser.at_end() && parser.diagnostics.is_empty() {
        let token = parser.peek().clone();
        parser.error(
            "E7005",
            format!("Expected a new line or `;` before {}.", describe(&token)),
            token.span,
        );
    }
    let id = parser.next_id();
    (Block { id, stmts, span }, parser.diagnostics)
}

pub fn parse_block_tokens(tokens: &[Token], fallback: &Span) -> (Block, Vec<Diagnostic>) {
    let mut parser = ExprParser::new(tokens, fallback);
    let span = parser.peek().span.clone();
    let stmts = parser.statements(None);
    let id = parser.next_id();
    (Block { id, stmts, span }, parser.diagnostics)
}

/// Parses a type written as text, such as the `TypeRef` names of declarations.
#[allow(clippy::result_large_err)]
pub fn parse_type_text(text: &str, span: &Span) -> Result<TypeExpr, Diagnostic> {
    let (mut tokens, lex_diagnostics) = lex(&span.file, text);
    if let Some(mut diagnostic) = lex_diagnostics.into_iter().next() {
        diagnostic.span = span.clone();
        return Err(diagnostic);
    }
    for token in &mut tokens {
        token.span = span.clone();
    }
    let mut parser = ExprParser::new(&tokens, span);
    let parsed = parser.type_expr();
    let complete = parser.at_end();
    match (parsed, parser.diagnostics.into_iter().next()) {
        (_, Some(diagnostic)) => Err(diagnostic),
        (Some(ty), None) if complete => Ok(ty),
        _ => Err(Diagnostic::error(
            "E7005",
            format!("Invalid type `{text}`."),
            span.clone(),
        )),
    }
}

/// Deepest nesting of expressions, patterns and types the parser descends into, so that
/// hostile input is reported instead of overflowing the stack (here or in later passes).
const MAX_DEPTH: usize = 256;

/// Most operators in the open operator chains, which later passes traverse recursively.
const MAX_CHAIN: usize = 1000;

enum Infix {
    Op(BinaryOp),
    Elvis,
}

enum NumberLiteral {
    Int(String),
    Double(String),
    Float(String),
}

pub(crate) struct ExprParser {
    tokens: Vec<Token>,
    index: usize,
    next_id: NodeId,
    /// Depth of enclosing parentheses; newlines are insignificant when it is non-zero.
    nesting: usize,
    /// Current recursion depth, bounded by `MAX_DEPTH`.
    depth: usize,
    /// Operators consumed by the `binary` loops still open, bounded by `MAX_CHAIN`.
    chain: usize,
    /// Set once a limit is reported; parsing then unwinds without further diagnostics.
    limit_reported: bool,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

impl ExprParser {
    pub(crate) fn new(tokens: &[Token], fallback: &Span) -> Self {
        let mut tokens = tokens.to_vec();
        if tokens
            .last()
            .is_none_or(|token| token.kind != TokenKind::Eof)
        {
            let span = tokens
                .last()
                .map_or_else(|| fallback.clone(), |token| token.span.clone());
            tokens.push(Token {
                kind: TokenKind::Eof,
                text: String::new(),
                span,
            });
        }
        Self {
            tokens,
            index: 0,
            next_id: 0,
            nesting: 0,
            depth: 0,
            chain: 0,
            limit_reported: false,
            diagnostics: Vec::new(),
        }
    }

    // ---- token helpers -------------------------------------------------------------

    fn peek(&self) -> &Token {
        &self.tokens[self.index.min(self.tokens.len() - 1)]
    }

    fn peek_at(&self, offset: usize) -> &Token {
        &self.tokens[(self.index + offset).min(self.tokens.len() - 1)]
    }

    fn at_end(&self) -> bool {
        self.peek().kind == TokenKind::Eof
    }

    fn advance(&mut self) -> Token {
        let token = self.peek().clone();
        if !self.at_end() {
            self.index += 1;
        }
        token
    }

    /// True when the current token is the keyword or symbol `text` (never a string literal).
    fn is(&self, text: &str) -> bool {
        let token = self.peek();
        token.text == text && matches!(token.kind, TokenKind::Identifier | TokenKind::Symbol)
    }

    fn consume(&mut self, text: &str) -> bool {
        if self.is(text) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, text: &str) -> Option<()> {
        if self.consume(text) {
            return Some(());
        }
        let token = self.peek().clone();
        self.error(
            "E7005",
            format!("Expected `{text}`, found {}.", describe(&token)),
            token.span,
        );
        None
    }

    /// Two symbols written without space between them, such as `?` `:` or `!` `!`.
    fn adjacent_pair(&self, first: &str, second: &str) -> bool {
        let (a, b) = (self.peek(), self.peek_at(1));
        a.kind == TokenKind::Symbol
            && a.text == first
            && b.text == second
            && b.span.line == a.span.line
            && b.span.column == a.span.column + a.text.chars().count()
    }

    fn previous_line(&self) -> usize {
        if self.index == 0 {
            0
        } else {
            self.tokens[self.index - 1].span.line
        }
    }

    fn same_line(&self) -> bool {
        self.nesting > 0 || self.peek().span.line == self.previous_line()
    }

    fn identifier(&mut self) -> Option<String> {
        let token = self.peek().clone();
        if token.kind == TokenKind::Identifier {
            self.advance();
            Some(token.text)
        } else {
            self.error(
                "E7005",
                format!("Expected a name, found {}.", describe(&token)),
                token.span,
            );
            None
        }
    }

    fn next_id(&mut self) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn node(&mut self, kind: ExprKind, span: Span) -> Expr {
        Expr {
            id: self.next_id(),
            kind,
            span,
        }
    }

    fn error(&mut self, code: &'static str, message: impl Into<String>, span: Span) {
        self.diagnostics
            .push(Diagnostic::error(code, message, span));
    }

    fn removed(&mut self, what: &str, span: Span) {
        let (message, hint) = match what {
            "var" => (
                "`var` was removed: Bind values are immutable.",
                "Use `val`.",
            ),
            "!!" => (
                "`!!` was removed: it can crash at runtime.",
                "Handle `null` with `when`, `?:` or `?.`.",
            ),
            "throw" | "try" | "catch" => (
                "Exceptions were removed: errors are values in Bind.",
                "Return a `Result<T, E>` or a nullable value.",
            ),
            "while" | "do" | "for" => (
                "Loops were removed: Bind has no mutation to loop over.",
                "Use recursion, `map` or `fold`.",
            ),
            _ => (
                "String interpolation is not supported: the embedded expression would not be type-checked.",
                "Concatenate with `+`, e.g. `\"Hello, \" + name`.",
            ),
        };
        self.diagnostics
            .push(Diagnostic::error("E7001", message, span).with_hint(hint));
    }

    /// Runs `parse` one nesting level deeper, or reports E7004 (once) beyond `MAX_DEPTH`.
    fn nested<T>(&mut self, parse: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        if self.limit_reported {
            return None;
        }
        if self.depth >= MAX_DEPTH {
            self.limit("Expression is nested too deeply (limit 256).");
            return None;
        }
        self.depth += 1;
        let result = parse(self);
        self.depth -= 1;
        result
    }

    fn limit(&mut self, message: &str) {
        if !self.limit_reported {
            self.limit_reported = true;
            let span = self.peek().span.clone();
            self.error("E7004", message, span);
        }
    }

    // ---- expressions ---------------------------------------------------------------

    fn expression(&mut self) -> Option<Expr> {
        self.nested(|parser| parser.binary(1))
    }

    fn infix(&self) -> Option<(Infix, u8, usize)> {
        let token = self.peek();
        if token.kind != TokenKind::Symbol {
            return None;
        }
        let op = |op| Some((Infix::Op(op), 0, 1));
        let (infix, _, width) = match token.text.as_str() {
            "?" if self.adjacent_pair("?", ":") => (Infix::Elvis, 0, 2),
            "||" => op(BinaryOp::Or)?,
            "&&" => op(BinaryOp::And)?,
            "==" => op(BinaryOp::Eq)?,
            "!=" => op(BinaryOp::NotEq)?,
            "<" => op(BinaryOp::Lt)?,
            "<=" => op(BinaryOp::LtEq)?,
            ">" => op(BinaryOp::Gt)?,
            ">=" => op(BinaryOp::GtEq)?,
            "+" => op(BinaryOp::Add)?,
            "-" => op(BinaryOp::Sub)?,
            "*" => op(BinaryOp::Mul)?,
            "/" => op(BinaryOp::Div)?,
            "%" => op(BinaryOp::Rem)?,
            _ => return None,
        };
        let precedence = match &infix {
            Infix::Op(BinaryOp::Or) => 1,
            Infix::Op(BinaryOp::And) => 2,
            Infix::Op(BinaryOp::Eq | BinaryOp::NotEq) => 3,
            Infix::Op(BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq) => 4,
            Infix::Elvis => 5,
            Infix::Op(BinaryOp::Add | BinaryOp::Sub) => 6,
            Infix::Op(BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem) => 7,
        };
        Some((infix, precedence, width))
    }

    fn binary(&mut self, min: u8) -> Option<Expr> {
        let open = self.chain;
        let left = self.binary_chain(min);
        self.chain = open;
        left
    }

    fn binary_chain(&mut self, min: u8) -> Option<Expr> {
        let mut left = self.unary()?;
        while let Some((infix, precedence, width)) = self.infix() {
            if precedence < min {
                break;
            }
            let crosses_lines = matches!(
                infix,
                Infix::Op(BinaryOp::And | BinaryOp::Or) | Infix::Elvis
            );
            if !self.same_line() && !crosses_lines {
                break;
            }
            if self.chain >= MAX_CHAIN {
                self.limit("Operator chain is too long (limit 1000).");
                return None;
            }
            self.chain += 1;
            for _ in 0..width {
                self.advance();
            }
            // `?:` is right-associative (`a ?: b ?: c` is `a ?: (b ?: c)`); the rest are left-associative.
            let next = if matches!(infix, Infix::Elvis) {
                precedence
            } else {
                precedence + 1
            };
            let right = self.binary(next)?;
            let span = left.span.clone();
            let kind = match infix {
                Infix::Op(op) => ExprKind::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                Infix::Elvis => ExprKind::Elvis {
                    value: Box::new(left),
                    fallback: Box::new(right),
                },
            };
            left = self.node(kind, span);
        }
        Some(left)
    }

    fn unary(&mut self) -> Option<Expr> {
        let op = if self.is("-") {
            UnaryOp::Neg
        } else if self.is("!") {
            UnaryOp::Not
        } else {
            return self.postfix();
        };
        let span = self.advance().span;
        let operand = self.nested(Self::unary)?;
        Some(self.node(
            ExprKind::Unary {
                op,
                operand: Box::new(operand),
            },
            span,
        ))
    }

    fn postfix(&mut self) -> Option<Expr> {
        let mut expr = self.primary()?;
        loop {
            if self.is("(") && self.same_line() {
                let mut args = self.arguments()?;
                let children = self.trailing(&mut args);
                let span = expr.span.clone();
                expr = self.node(
                    ExprKind::Call {
                        callee: Box::new(expr),
                        args,
                        children,
                    },
                    span,
                );
            } else if self.is(".") {
                self.advance();
                let name = self.identifier()?;
                let span = expr.span.clone();
                expr = self.node(
                    ExprKind::Field {
                        target: Box::new(expr),
                        name,
                    },
                    span.clone(),
                );
                if self.is("{") && self.same_line() && self.looks_like_builder() {
                    let fields = self.builder_fields()?;
                    expr = self.node(
                        ExprKind::Builder {
                            target: Box::new(expr),
                            fields,
                        },
                        span,
                    );
                }
            } else if self.adjacent_pair("?", ".") {
                self.advance();
                self.advance();
                let name = self.identifier()?;
                let span = expr.span.clone();
                expr = self.node(
                    ExprKind::SafeField {
                        target: Box::new(expr),
                        name,
                    },
                    span,
                );
            } else if self.adjacent_pair("!", "!") && self.same_line() {
                let span = self.advance().span;
                self.advance();
                self.removed("!!", span);
            } else if self.is("{") && self.same_line() && matches!(expr.kind, ExprKind::Name(_)) {
                let mut args = Vec::new();
                let children = self.trailing(&mut args);
                let span = expr.span.clone();
                expr = self.node(
                    ExprKind::Call {
                        callee: Box::new(expr),
                        args,
                        children,
                    },
                    span,
                );
            } else {
                break;
            }
        }
        Some(expr)
    }

    fn arguments(&mut self) -> Option<Vec<Expr>> {
        self.advance(); // `(`
        self.nesting += 1;
        let mut args = Vec::new();
        while !self.is(")") && !self.at_end() {
            if self.peek().kind == TokenKind::Identifier
                && self.peek_at(1).kind == TokenKind::Symbol
                && self.peek_at(1).text == "="
            {
                let span = self.peek().span.clone();
                self.error(
                    "E7004",
                    "Named arguments are not supported; pass arguments by position.",
                    span,
                );
                self.advance();
                self.advance();
            }
            let Some(arg) = self.expression() else {
                self.nesting -= 1;
                return None;
            };
            args.push(arg);
            if !self.consume(",") {
                break;
            }
        }
        self.nesting -= 1;
        self.expect(")")?;
        Some(args)
    }

    fn primary(&mut self) -> Option<Expr> {
        let token = self.peek().clone();
        match token.kind {
            TokenKind::Number => {
                self.advance();
                match classify_number(&token.text) {
                    Ok(NumberLiteral::Int(text)) => {
                        Some(self.node(ExprKind::Int(text), token.span))
                    }
                    Ok(NumberLiteral::Double(text)) => {
                        Some(self.node(ExprKind::Double(text), token.span))
                    }
                    Ok(NumberLiteral::Float(text)) => {
                        Some(self.node(ExprKind::Float(text), token.span))
                    }
                    Err(message) => {
                        self.error("E7004", message, token.span);
                        None
                    }
                }
            }
            TokenKind::String => {
                self.advance();
                if has_interpolation(&token.text) {
                    self.removed("interpolation", token.span.clone());
                }
                Some(self.node(ExprKind::Str(token.text), token.span))
            }
            TokenKind::Character => {
                self.advance();
                self.error(
                    "E7004",
                    "Character literals are not supported; use a String.",
                    token.span,
                );
                None
            }
            TokenKind::Eof => {
                self.error(
                    "E7005",
                    "Expected an expression, found end of input.",
                    token.span,
                );
                None
            }
            TokenKind::Identifier => match token.text.as_str() {
                "true" | "false" => {
                    self.advance();
                    Some(self.node(ExprKind::Bool(token.text == "true"), token.span))
                }
                "null" => {
                    self.advance();
                    Some(self.node(ExprKind::Null, token.span))
                }
                "if" => self.if_expression(),
                "when" => self.when_expression(),
                "return" => {
                    self.advance();
                    let value = self.expression()?;
                    Some(self.node(ExprKind::Return(Box::new(value)), token.span))
                }
                "var" | "throw" | "try" | "catch" | "while" | "do" | "for" => {
                    self.advance();
                    self.removed(&token.text, token.span);
                    None
                }
                _ => {
                    self.advance();
                    Some(self.node(ExprKind::Name(token.text), token.span))
                }
            },
            TokenKind::Symbol if token.text == "(" => {
                self.advance();
                self.nesting += 1;
                let inner = self.expression();
                self.nesting -= 1;
                let inner = inner?;
                self.expect(")")?;
                Some(inner)
            }
            TokenKind::Symbol if token.text == "{" && self.looks_like_lambda() => self.lambda(),
            TokenKind::Symbol if token.text == "{" => {
                self.error(
                    "E7004",
                    "A block cannot be used as a value here; write a lambda as `{ x -> ... }`.",
                    token.span,
                );
                None
            }
            TokenKind::Symbol => {
                self.error(
                    "E7005",
                    format!("Expected an expression, found `{}`.", token.text),
                    token.span,
                );
                None
            }
        }
    }

    // ---- statements and blocks ----------------------------------------------------

    /// Skips to a plausible statement boundary after an error.
    fn recover(&mut self) {
        let mut depth = 0usize;
        while !self.at_end() {
            if depth == 0
                && (self.is(";")
                    || self.is("}")
                    || self.is(")")
                    || (self.nesting == 0
                        && self.index > 0
                        && self.peek().span.line != self.previous_line()))
            {
                return;
            }
            if self.is("(") || self.is("{") || self.is("[") {
                depth += 1;
            }
            if (self.is(")") || self.is("}") || self.is("]")) && depth > 0 {
                depth -= 1;
            }
            self.advance();
        }
    }

    fn at_close(&self, close: Option<&str>) -> bool {
        close.is_some_and(|text| self.is(text))
    }

    fn end_of_item(&mut self, close: Option<&str>) {
        if self.at_end() || self.is(";") || self.at_close(close) || !self.same_line() {
            return;
        }
        let token = self.peek().clone();
        self.error(
            "E7005",
            format!("Expected a new line or `;` before {}.", describe(&token)),
            token.span,
        );
        self.recover();
    }

    fn statements(&mut self, close: Option<&str>) -> Vec<Stmt> {
        let mut stmts = Vec::new();
        loop {
            while self.consume(";") {}
            if self.at_end() || self.at_close(close) {
                break;
            }
            let start = self.index;
            match self.statement() {
                Some(stmt) => {
                    stmts.push(stmt);
                    self.end_of_item(close);
                }
                None => self.recover(),
            }
            if self.index == start {
                self.advance();
            }
        }
        stmts
    }

    fn statement(&mut self) -> Option<Stmt> {
        if self.is("val") {
            let span = self.advance().span;
            let name = self.identifier()?;
            let annotation = if self.consume(":") {
                Some(self.type_expr()?)
            } else {
                None
            };
            self.expect("=")?;
            let value = self.expression()?;
            return Some(Stmt::Val {
                name,
                annotation,
                value,
                span,
            });
        }
        self.expression().map(Stmt::Expr)
    }

    fn block(&mut self) -> Block {
        let span = self.peek().span.clone();
        let saved = std::mem::replace(&mut self.nesting, 0);
        let _ = self.expect("{");
        let stmts = self.statements(Some("}"));
        let _ = self.expect("}");
        self.nesting = saved;
        Block {
            id: self.next_id(),
            stmts,
            span,
        }
    }

    /// `{ x, y -> ... }` or `{ -> ... }`.
    fn looks_like_lambda(&self) -> bool {
        let mut offset = 1;
        if self.peek_at(offset).text == "->" {
            return true;
        }
        loop {
            if self.peek_at(offset).kind != TokenKind::Identifier {
                return false;
            }
            match self.peek_at(offset + 1).text.as_str() {
                "," => offset += 2,
                "->" => return true,
                _ => return false,
            }
        }
    }

    fn lambda(&mut self) -> Option<Expr> {
        let span = self.peek().span.clone();
        let saved = std::mem::replace(&mut self.nesting, 0);
        self.advance(); // `{`, guaranteed by `looks_like_lambda`
        let mut params = Vec::new();
        while !self.is("->") {
            let token = self.advance();
            params.push((token.text, token.span));
            self.consume(",");
        }
        self.advance(); // `->`
        let body_span = self.peek().span.clone();
        let stmts = self.statements(Some("}"));
        let closed = self.expect("}").is_some();
        self.nesting = saved;
        let body = Block {
            id: self.next_id(),
            stmts,
            span: body_span,
        };
        if !closed {
            return None;
        }
        Some(self.node(ExprKind::Lambda { params, body }, span))
    }

    /// A `{` on the same line after a call: a trailing lambda argument or a UI child block.
    fn trailing(&mut self, args: &mut Vec<Expr>) -> Option<Block> {
        if !(self.is("{") && self.same_line()) {
            return None;
        }
        if self.looks_like_lambda() {
            if let Some(lambda) = self.lambda() {
                args.push(lambda);
            }
            return None;
        }
        Some(self.block())
    }

    fn looks_like_builder(&self) -> bool {
        self.peek_at(1).kind == TokenKind::Identifier && self.peek_at(2).text == "="
    }

    fn builder_fields(&mut self) -> Option<Vec<FieldInit>> {
        let saved = std::mem::replace(&mut self.nesting, 0);
        self.advance(); // `{`
        let mut fields = Vec::new();
        loop {
            while self.consume(";") {}
            if self.is("}") || self.at_end() {
                break;
            }
            let start = self.index;
            match self.field_init() {
                Some(field) => {
                    fields.push(field);
                    self.end_of_item(Some("}"));
                }
                None => self.recover(),
            }
            if self.index == start {
                self.advance();
            }
        }
        let closed = self.expect("}").is_some();
        self.nesting = saved;
        closed.then_some(fields)
    }

    fn field_init(&mut self) -> Option<FieldInit> {
        let span = self.peek().span.clone();
        let name = self.identifier()?;
        self.expect("=")?;
        let value = self.expression()?;
        Some(FieldInit { name, value, span })
    }

    fn branch(&mut self) -> Option<Expr> {
        if self.is("{") {
            let span = self.peek().span.clone();
            let block = self.block();
            return Some(self.node(ExprKind::Block(block), span));
        }
        self.expression()
    }

    fn parenthesized(&mut self) -> Option<Expr> {
        self.expect("(")?;
        self.nesting += 1;
        let inner = self.expression();
        self.nesting -= 1;
        let inner = inner?;
        self.expect(")")?;
        Some(inner)
    }

    fn if_expression(&mut self) -> Option<Expr> {
        let span = self.advance().span;
        let condition = self.parenthesized()?;
        let then_branch = self.branch()?;
        if !self.consume("else") {
            let token = self.peek().clone();
            self.error(
                "E7005",
                "`if` is an expression in Bind and requires an `else` branch.",
                token.span,
            );
            return None;
        }
        let else_branch = self.branch()?;
        Some(self.node(
            ExprKind::If {
                condition: Box::new(condition),
                then_branch: Box::new(then_branch),
                else_branch: Box::new(else_branch),
            },
            span,
        ))
    }

    fn when_expression(&mut self) -> Option<Expr> {
        let span = self.advance().span;
        if !self.is("(") {
            self.error(
                "E7004",
                "`when` without a subject is not supported; write `when (value) { ... }`.",
                span,
            );
            return None;
        }
        let scrutinee = self.parenthesized()?;
        let saved = std::mem::replace(&mut self.nesting, 0);
        if self.expect("{").is_none() {
            self.nesting = saved;
            return None;
        }
        let mut arms = Vec::new();
        loop {
            while self.consume(";") {}
            if self.is("}") || self.at_end() {
                break;
            }
            let start = self.index;
            match self.arm() {
                Some(arm) => {
                    arms.push(arm);
                    self.end_of_item(Some("}"));
                }
                None => self.recover(),
            }
            if self.index == start {
                self.advance();
            }
        }
        let closed = self.expect("}").is_some();
        self.nesting = saved;
        if !closed {
            return None;
        }
        Some(self.node(
            ExprKind::When {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            span,
        ))
    }

    fn arm(&mut self) -> Option<Arm> {
        let span = self.peek().span.clone();
        let pattern = self.pattern()?;
        self.expect("->")?;
        let body = self.branch()?;
        Some(Arm {
            pattern,
            body,
            span,
        })
    }

    fn pattern(&mut self) -> Option<Pattern> {
        self.nested(Self::pattern_inner)
    }

    fn pattern_inner(&mut self) -> Option<Pattern> {
        if self.is("-") && self.peek_at(1).kind == TokenKind::Number {
            self.advance();
            let number = self.advance();
            return Some(self.number_pattern(&number, "-"));
        }
        let token = self.peek().clone();
        match token.kind {
            TokenKind::Number => {
                self.advance();
                Some(self.number_pattern(&token, ""))
            }
            TokenKind::String => {
                self.advance();
                if has_interpolation(&token.text) {
                    self.removed("interpolation", token.span.clone());
                }
                Some(Pattern::Str(token.text, token.span))
            }
            TokenKind::Identifier => {
                self.advance();
                match token.text.as_str() {
                    "_" | "else" => Some(Pattern::Wildcard),
                    "null" => Some(Pattern::Null(token.span)),
                    "true" | "false" => Some(Pattern::Bool(token.text == "true", token.span)),
                    _ => {
                        let mut name = token.text.clone();
                        while self.is(".") && self.peek_at(1).kind == TokenKind::Identifier {
                            self.advance();
                            name = self.advance().text;
                        }
                        if !(self.is("(") && self.same_line()) {
                            return Some(Pattern::Name(name, token.span));
                        }
                        self.advance();
                        let mut args = Vec::new();
                        while !self.is(")") && !self.at_end() {
                            args.push(self.pattern()?);
                            if !self.consume(",") {
                                break;
                            }
                        }
                        self.expect(")")?;
                        Some(Pattern::Constructor {
                            name,
                            args,
                            span: token.span,
                        })
                    }
                }
            }
            _ => {
                self.error(
                    "E7005",
                    format!("Expected a pattern, found {}.", describe(&token)),
                    token.span,
                );
                None
            }
        }
    }

    fn number_pattern(&mut self, token: &Token, sign: &str) -> Pattern {
        match classify_number(&token.text) {
            Ok(NumberLiteral::Int(text)) => {
                Pattern::Int(format!("{sign}{text}"), token.span.clone())
            }
            Ok(NumberLiteral::Double(text) | NumberLiteral::Float(text)) => {
                Pattern::Double(format!("{sign}{text}"), token.span.clone())
            }
            Err(message) => {
                self.error("E7004", message, token.span.clone());
                Pattern::Wildcard
            }
        }
    }

    // ---- types ---------------------------------------------------------------------

    pub(crate) fn type_expr(&mut self) -> Option<TypeExpr> {
        self.nested(Self::type_expr_inner)
    }

    /// `! [db.read, e]` after a function type, or nothing.
    fn effect_list(&mut self) -> Option<Vec<String>> {
        if !(self.is("!") && self.peek_at(1).text == "[") {
            return Some(Vec::new());
        }
        self.advance();
        self.advance();
        let mut names = Vec::new();
        while !self.is("]") && !self.at_end() {
            let mut name = self.identifier()?;
            if self.consume(".") {
                name.push('.');
                name.push_str(&self.identifier()?);
            }
            names.push(name);
            if !self.consume(",") {
                break;
            }
        }
        self.expect("]")?;
        Some(names)
    }

    fn type_expr_inner(&mut self) -> Option<TypeExpr> {
        let span = self.peek().span.clone();
        let mut ty = if self.is("(") {
            self.advance();
            let mut params = Vec::new();
            while !self.is(")") && !self.at_end() {
                params.push(self.type_expr()?);
                if !self.consume(",") {
                    break;
                }
            }
            self.expect(")")?;
            if self.consume("->") {
                let result = self.type_expr()?;
                let effects = self.effect_list()?;
                TypeExpr::Function {
                    params,
                    result: Box::new(result),
                    effects,
                }
            } else if params.len() == 1 {
                params.remove(0)
            } else {
                self.error(
                    "E7005",
                    "Expected `->` after the parameter types of a function type.",
                    span,
                );
                return None;
            }
        } else {
            let name = self.identifier()?;
            let mut args = Vec::new();
            if self.consume("<") {
                loop {
                    args.push(self.type_expr()?);
                    if !self.consume(",") {
                        break;
                    }
                }
                self.expect(">")?;
            }
            TypeExpr::Named { name, args, span }
        };
        while self.consume("?") {
            ty = TypeExpr::Nullable(Box::new(ty));
        }
        Some(ty)
    }
}

fn describe(token: &Token) -> String {
    match token.kind {
        TokenKind::Eof => "end of input".into(),
        TokenKind::String => "a string".into(),
        _ => format!("`{}`", token.text),
    }
}

/// `$name` or `${...}`; a `$` followed by anything else is a literal dollar sign.
fn has_interpolation(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars
        .windows(2)
        .any(|pair| pair[0] == '$' && (pair[1] == '{' || pair[1] == '_' || pair[1].is_alphabetic()))
}

fn classify_number(text: &str) -> Result<NumberLiteral, &'static str> {
    let cleaned = text.replace('_', "").to_ascii_lowercase();
    if cleaned.ends_with('u') || cleaned.ends_with("ul") {
        return Err("Unsigned literals are not supported.");
    }
    let radix = |digits: &str, base: u32| {
        u128::from_str_radix(digits.trim_end_matches('l'), base)
            .map_or_else(|_| cleaned.clone(), |value| value.to_string())
    };
    if let Some(hex) = cleaned.strip_prefix("0x") {
        return Ok(NumberLiteral::Int(radix(hex, 16)));
    }
    if let Some(bin) = cleaned.strip_prefix("0b") {
        return Ok(NumberLiteral::Int(radix(bin, 2)));
    }
    if let Some(float) = cleaned.strip_suffix('f') {
        return Ok(NumberLiteral::Float(float.to_string()));
    }
    if cleaned.contains('.') || cleaned.contains('e') {
        return Ok(NumberLiteral::Double(cleaned));
    }
    let digits = cleaned.trim_end_matches('l').trim_start_matches('0');
    Ok(NumberLiteral::Int(if digits.is_empty() {
        "0".into()
    } else {
        digits.to_string()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn tokens(source: &str) -> Vec<Token> {
        let (tokens, diagnostics) = lex("t.bd", source);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        tokens
            .into_iter()
            .filter(|token| token.kind != TokenKind::Eof)
            .collect()
    }

    fn expression(source: &str) -> String {
        let (block, diagnostics) = parse_expression_tokens(&tokens(source), &Span::default());
        assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
        render_block(&block)
    }

    fn codes(source: &str) -> Vec<&'static str> {
        let (_, diagnostics) = parse_expression_tokens(&tokens(source), &Span::default());
        diagnostics.iter().map(|item| item.code).collect()
    }

    #[test]
    fn respects_kotlin_precedence() {
        assert_eq!(expression("1 + 2 * 3"), "{ (1 + (2 * 3)) }");
        assert_eq!(expression("a || b && c == d"), "{ (a || (b && (c == d))) }");
        assert_eq!(expression("a < b + 1"), "{ (a < (b + 1)) }");
        assert_eq!(expression("a ?: b + 1"), "{ (a ?: (b + 1)) }");
        assert_eq!(expression("a ?: b ?: c"), "{ (a ?: (b ?: c)) }");
        assert_eq!(expression("a ?: b == c ?: d"), "{ ((a ?: b) == (c ?: d)) }");
        assert_eq!(expression("a - b - c"), "{ ((a - b) - c) }");
        assert_eq!(expression("-a * !b"), "{ ((-a) * (!b)) }");
        assert_eq!(expression("(a + b) * c"), "{ ((a + b) * c) }");
    }

    #[test]
    fn parses_calls_fields_and_safe_calls() {
        assert_eq!(expression("f(a, g(b))"), "{ f(a, g(b)) }");
        assert_eq!(
            expression("server.getCustomer(id).name"),
            "{ server.getCustomer(id).name }"
        );
        assert_eq!(
            expression("cliente?.apelido ?: nome"),
            "{ (cliente?.apelido ?: nome) }"
        );
        assert_eq!(expression("f(\n    a,\n    b\n)"), "{ f(a, b) }");
    }

    #[test]
    fn classifies_number_literals() {
        assert_eq!(expression("1_000"), "{ 1000 }");
        assert_eq!(expression("0xFF"), "{ 255 }");
        assert_eq!(expression("0b101"), "{ 5 }");
        assert_eq!(expression("42L"), "{ 42 }");
        assert_eq!(expression("2.5"), "{ 2.5 }");
        assert_eq!(expression("1.5f"), "{ 1.5f }");
        assert_eq!(codes("7u"), vec!["E7004"]);
    }

    #[test]
    fn newline_ends_an_expression_except_after_logical_operators() {
        assert_eq!(codes("a\n+ b"), vec!["E7005"]);
        assert_eq!(expression("a &&\n    b"), "{ (a && b) }");
        assert_eq!(expression("a\n    || b"), "{ (a || b) }");
    }

    #[test]
    fn rejects_removed_constructs() {
        assert_eq!(codes("x!!"), vec!["E7001"]);
        assert_eq!(codes("throw erro"), vec!["E7001"]);
        assert_eq!(codes("\"Olá $nome\""), vec!["E7001"]);
        assert_eq!(codes("\"total: ${a + b}\""), vec!["E7001"]);
        assert_eq!(codes("'x'"), vec!["E7004"]);
    }

    #[test]
    fn accepts_literal_dollar_signs() {
        assert_eq!(expression("\"US$ 10\""), "{ \"US$ 10\" }");
        assert_eq!(expression("\"$\""), "{ \"$\" }");
        assert_eq!(expression("\"preço: $10\""), "{ \"preço: $10\" }");
    }

    #[test]
    fn parses_type_text() {
        let span = Span::default();
        let render = |text: &str| render_type(&parse_type_text(text, &span).unwrap());
        assert_eq!(render("List<Map<String,Int>>"), "List<Map<String, Int>>");
        assert_eq!(render("(Int, String)->Int?"), "(Int, String) -> Int?");
        assert_eq!(render("((Int)->Int)?"), "((Int) -> Int)?");
        assert_eq!(render("Int??"), "Int??");
        assert!(parse_type_text("List<", &span).is_err());
    }

    #[test]
    fn parses_effect_rows_in_types() {
        let span = Span::default();
        let render = |text: &str| render_type(&parse_type_text(text, &span).unwrap());
        assert_eq!(
            render("(Int) -> Int ! [db.read, e]"),
            "(Int) -> Int ! [db.read, e]"
        );
        assert_eq!(render("((Int) -> Int ! [e])?"), "((Int) -> Int ! [e])?");
        assert_eq!(render("(A) -> (B) -> C ! [e]"), "(A) -> (B) -> C ! [e]");
        assert_eq!(
            render("(Int)->Int![db.read,e]"),
            "(Int) -> Int ! [db.read, e]"
        );
        assert!(parse_type_text("(Int) -> Int ! [", &span).is_err());
    }

    fn block(source: &str) -> String {
        let (block, diagnostics) = parse_block_tokens(&tokens(source), &Span::default());
        assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
        render_block(&block)
    }

    fn block_codes(source: &str) -> Vec<&'static str> {
        let (_, diagnostics) = parse_block_tokens(&tokens(source), &Span::default());
        diagnostics.iter().map(|item| item.code).collect()
    }

    #[test]
    fn parses_statements_separated_by_newlines_and_semicolons() {
        assert_eq!(
            block("val x = 1\nval y: Int = x + 1\ny"),
            "{ val x = 1; val y: Int = (x + 1); y }"
        );
        assert_eq!(block("a; b"), "{ a; b }");
        assert_eq!(block(""), "{ }");
        assert_eq!(block_codes("a b"), vec!["E7005"]);
    }

    #[test]
    fn operator_on_next_line_starts_a_new_statement() {
        assert_eq!(block("val y = a\n    - b\ny"), "{ val y = a; (-b); y }");
    }

    #[test]
    fn parses_if_and_requires_else() {
        assert_eq!(block("if (c) 1 else 2"), "{ (if (c) 1 else 2) }");
        assert_eq!(
            block("if (c) {\n    1\n} else {\n    2\n}"),
            "{ (if (c) { 1 } else { 2 }) }"
        );
        assert_eq!(block_codes("if (c) 1"), vec!["E7005"]);
    }

    #[test]
    fn parses_when_with_patterns() {
        assert_eq!(
            block("when (p) {\n    Pago(v, _) -> v\n    Cancelado -> 0\n    else -> 1\n}"),
            "{ when (p) { Pago(v, _) -> v; Cancelado -> 0; _ -> 1 } }"
        );
        assert_eq!(
            block(
                "when (x) { null -> 0; -1 -> 1; \"a\" -> 2; true -> 3; Status.Ativo -> 4; n -> n }"
            ),
            "{ when (x) { null -> 0; -1 -> 1; \"a\" -> 2; true -> 3; Ativo -> 4; n -> n } }"
        );
        assert_eq!(block_codes("when { x -> 1 }"), vec!["E7004"]);
    }

    #[test]
    fn parses_lambdas_and_trailing_blocks() {
        assert_eq!(
            block("val f = { x, y -> x + y }"),
            "{ val f = ({ x, y -> (x + y) }) }"
        );
        assert_eq!(block("val g = { -> 1 }"), "{ val g = ({ -> 1 }) }");
        assert_eq!(
            block("map(xs) { x -> x + 1 }"),
            "{ map(xs, ({ x -> (x + 1) })) }"
        );
        assert_eq!(
            block("UI {\n    Div(\"card\") {\n        Text(\"a\")\n        Text(b.name)\n    }\n}"),
            "{ UI() { Div(\"card\") { Text(\"a\"); Text(b.name) } } }"
        );
        assert_eq!(block_codes("val h = { x }"), vec!["E7004"]);
    }

    #[test]
    fn parses_database_insert_builders() {
        assert_eq!(
            block("return db.customers.insert {\n    name = name\n    email = email\n}"),
            "{ (return db.customers.insert { name = name; email = email }) }"
        );
    }

    fn chain(terms: usize) -> String {
        vec!["1"; terms].join(" + ")
    }

    fn nested(depth: usize) -> String {
        format!("{}1{}", "(".repeat(depth), ")".repeat(depth))
    }

    /// Deep inputs run on the stack the `bind` binary gives the compiler (see `main.rs`).
    #[test]
    fn deep_nesting_is_reported_without_overflowing() {
        crate::with_compiler_stack(deep_nesting_cases);
    }

    fn deep_nesting_cases() {
        assert_eq!(codes(&nested(300)), vec!["E7004"]);
        assert_eq!(codes(&nested(2_000)), vec!["E7004"]);
        assert_eq!(codes(&format!("{}x", "- ".repeat(2_000))), vec!["E7004"]);
        let whens = format!("{}x{}", "when (x) { _ -> ".repeat(300), " }".repeat(300));
        assert_eq!(codes(&whens), vec!["E7004"]);
        let patterns = format!(
            "when (x) {{ {}y{} -> 1 }}",
            "C(".repeat(300),
            ")".repeat(300)
        );
        assert_eq!(codes(&patterns), vec!["E7004"]);
        assert!(
            parse_type_text(
                &format!("{}Int{}", "List<".repeat(300), ">".repeat(300)),
                &Span::default()
            )
            .is_err()
        );
        assert!(codes(&nested(200)).is_empty());
    }

    #[test]
    fn long_operator_chains_are_reported_without_overflowing() {
        crate::with_compiler_stack(long_chain_cases);
    }

    fn long_chain_cases() {
        assert_eq!(codes(&chain(1_500)), vec!["E7004"]);
        assert_eq!(codes(&chain(20_000)), vec!["E7004"]);
        assert_eq!(codes(&vec!["a"; 20_000].join(" ?: ")), vec!["E7004"]);
        assert!(codes(&chain(500)).is_empty());
        assert!(codes(&chain(1_001)).is_empty());
    }

    #[test]
    fn recovers_after_removed_constructs() {
        assert_eq!(block_codes("var x = 1\nx"), vec!["E7001"]);
        assert_eq!(block_codes("while (c) {\n    a\n}\nb"), vec!["E7001"]);
        assert_eq!(block_codes("for (x in xs) {\n    a\n}"), vec!["E7001"]);
    }
}
