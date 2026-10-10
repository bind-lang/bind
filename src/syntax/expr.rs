//! Expression, pattern and type syntax for function bodies.

use std::collections::BTreeSet;

use crate::diagnostic::Span;

/// Identifies an expression or block inside one function body.
pub type NodeId = usize;

#[derive(Clone, Debug, PartialEq)]
pub enum TypeExpr {
    Named {
        name: String,
        args: Vec<TypeExpr>,
        span: Span,
    },
    Function {
        params: Vec<TypeExpr>,
        result: Box<TypeExpr>,
        /// Effect labels and variables written after `!`; empty means pure.
        effects: Vec<String>,
    },
    Nullable(Box<TypeExpr>),
}

impl TypeExpr {
    /// Effect variables (lowercase names without a dot) mentioned in function types.
    pub fn effect_variables(&self, output: &mut BTreeSet<String>) {
        match self {
            TypeExpr::Named { args, .. } => {
                args.iter().for_each(|arg| arg.effect_variables(output))
            }
            TypeExpr::Nullable(inner) => inner.effect_variables(output),
            TypeExpr::Function {
                params,
                result,
                effects,
            } => {
                params
                    .iter()
                    .for_each(|param| param.effect_variables(output));
                result.effect_variables(output);
                output.extend(effects.iter().filter(|name| !name.contains('.')).cloned());
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub id: NodeId,
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    /// Decimal digits; hexadecimal and binary literals are normalized when they fit in `u128`.
    Int(String),
    Double(String),
    /// Written with an `f` suffix, stored without it.
    Float(String),
    Str(String),
    Bool(bool),
    Null,
    Name(String),
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        children: Option<Block>,
    },
    Field {
        target: Box<Expr>,
        name: String,
    },
    SafeField {
        target: Box<Expr>,
        name: String,
    },
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Elvis {
        value: Box<Expr>,
        fallback: Box<Expr>,
    },
    If {
        condition: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    When {
        scrutinee: Box<Expr>,
        arms: Vec<Arm>,
    },
    Lambda {
        params: Vec<(String, Span)>,
        body: Block,
    },
    Block(Block),
    Return(Box<Expr>),
    /// `target { field = value ... }`, used by `db.<table>.insert`.
    Builder {
        target: Box<Expr>,
        fields: Vec<FieldInit>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
}

impl BinaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Rem => "%",
            Self::Eq => "==",
            Self::NotEq => "!=",
            Self::Lt => "<",
            Self::LtEq => "<=",
            Self::Gt => ">",
            Self::GtEq => ">=",
            Self::And => "&&",
            Self::Or => "||",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub id: NodeId,
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Val {
        name: String,
        annotation: Option<TypeExpr>,
        value: Expr,
        span: Span,
    },
    Expr(Expr),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arm {
    pub pattern: Pattern,
    pub body: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Pattern {
    Wildcard,
    /// A binding or a field-less constructor; the type checker decides.
    Name(String, Span),
    Constructor {
        name: String,
        args: Vec<Pattern>,
        span: Span,
    },
    Int(String, Span),
    Str(String, Span),
    Bool(bool, Span),
    /// Parsed so it can be rejected with a clear message (E7109).
    Double(String, Span),
    Null(Span),
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldInit {
    pub name: String,
    pub value: Expr,
    pub span: Span,
}

/// Renders an expression as Bind source. Binary and unary operations are fully
/// parenthesized, so `parse(render(e))` renders back to the same text.
pub fn render_expr(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Int(text) | ExprKind::Double(text) => text.clone(),
        ExprKind::Float(text) => format!("{text}f"),
        ExprKind::Str(text) => format!("{text:?}"),
        ExprKind::Bool(value) => value.to_string(),
        ExprKind::Null => "null".into(),
        ExprKind::Name(name) => name.clone(),
        ExprKind::Call {
            callee,
            args,
            children,
        } => {
            let mut text = format!("{}({})", render_expr(callee), join(args, render_expr));
            if let Some(block) = children {
                text.push(' ');
                text.push_str(&render_block(block));
            }
            text
        }
        ExprKind::Field { target, name } => format!("{}.{name}", render_target(target)),
        ExprKind::SafeField { target, name } => format!("{}?.{name}", render_target(target)),
        ExprKind::Unary { op, operand } => {
            let symbol = match op {
                UnaryOp::Neg => "-",
                UnaryOp::Not => "!",
            };
            format!("({symbol}{})", render_expr(operand))
        }
        ExprKind::Binary { op, left, right } => {
            format!(
                "({} {} {})",
                render_expr(left),
                op.symbol(),
                render_expr(right)
            )
        }
        ExprKind::Elvis { value, fallback } => {
            format!("({} ?: {})", render_expr(value), render_expr(fallback))
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => format!(
            "(if ({}) {} else {})",
            render_expr(condition),
            render_expr(then_branch),
            render_expr(else_branch)
        ),
        ExprKind::When { scrutinee, arms } => {
            let arms: Vec<String> = arms
                .iter()
                .map(|arm| {
                    format!(
                        "{} -> {}",
                        render_pattern(&arm.pattern),
                        render_expr(&arm.body)
                    )
                })
                .collect();
            format!(
                "when ({}) {{ {} }}",
                render_expr(scrutinee),
                arms.join("; ")
            )
        }
        ExprKind::Lambda { params, body } => {
            let names: Vec<&str> = params.iter().map(|(name, _)| name.as_str()).collect();
            let prefix = if names.is_empty() {
                String::new()
            } else {
                format!("{} ", names.join(", "))
            };
            format!("({{ {prefix}-> {} }})", render_stmts(&body.stmts))
        }
        ExprKind::Block(block) => render_block(block),
        ExprKind::Return(value) => format!("(return {})", render_expr(value)),
        ExprKind::Builder { target, fields } => {
            let fields: Vec<String> = fields
                .iter()
                .map(|field| format!("{} = {}", field.name, render_expr(&field.value)))
                .collect();
            format!("{} {{ {} }}", render_expr(target), fields.join("; "))
        }
    }
}

pub fn render_block(block: &Block) -> String {
    if block.stmts.is_empty() {
        "{ }".into()
    } else {
        format!("{{ {} }}", render_stmts(&block.stmts))
    }
}

pub fn render_pattern(pattern: &Pattern) -> String {
    match pattern {
        Pattern::Wildcard => "_".into(),
        Pattern::Name(name, _) => name.clone(),
        Pattern::Constructor { name, args, .. } => {
            format!("{name}({})", join(args, render_pattern))
        }
        Pattern::Int(text, _) | Pattern::Double(text, _) => text.clone(),
        Pattern::Str(text, _) => format!("{text:?}"),
        Pattern::Bool(value, _) => value.to_string(),
        Pattern::Null(_) => "null".into(),
    }
}

pub fn render_type(ty: &TypeExpr) -> String {
    match ty {
        TypeExpr::Named { name, args, .. } if args.is_empty() => name.clone(),
        TypeExpr::Named { name, args, .. } => format!("{name}<{}>", join(args, render_type)),
        TypeExpr::Function {
            params,
            result,
            effects,
        } => {
            let base = format!("({}) -> {}", join(params, render_type), render_type(result));
            if effects.is_empty() {
                base
            } else {
                format!("{base} ! [{}]", effects.join(", "))
            }
        }
        TypeExpr::Nullable(inner) => match inner.as_ref() {
            TypeExpr::Function { .. } => format!("({})?", render_type(inner)),
            _ => format!("{}?", render_type(inner)),
        },
    }
}

fn render_stmts(stmts: &[Stmt]) -> String {
    let rendered: Vec<String> = stmts
        .iter()
        .map(|stmt| match stmt {
            Stmt::Val {
                name,
                annotation: Some(ty),
                value,
                ..
            } => {
                format!("val {name}: {} = {}", render_type(ty), render_expr(value))
            }
            Stmt::Val {
                name,
                annotation: None,
                value,
                ..
            } => {
                format!("val {name} = {}", render_expr(value))
            }
            Stmt::Expr(expr) => render_expr(expr),
        })
        .collect();
    rendered.join("; ")
}

/// Number literals are parenthesized before `.` so `5.x` is not lexed as a number.
fn render_target(target: &Expr) -> String {
    match target.kind {
        ExprKind::Int(_) | ExprKind::Double(_) | ExprKind::Float(_) => {
            format!("({})", render_expr(target))
        }
        _ => render_expr(target),
    }
}

fn join<T>(items: &[T], render: impl Fn(&T) -> String) -> String {
    items.iter().map(render).collect::<Vec<_>>().join(", ")
}
