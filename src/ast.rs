use crate::diagnostic::Span;
use crate::lexer::Token;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeRef {
    pub name: String,
    pub nullable: bool,
}

impl TypeRef {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            nullable: false,
        }
    }

    pub fn unit() -> Self {
        Self::new("Unit")
    }

    pub fn display(&self) -> String {
        format!("{}{}", self.name, if self.nullable { "?" } else { "" })
    }
}

#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    pub type_ref: TypeRef,
    pub mutable: bool,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TypeKind {
    Data,
    Entity,
    Distinct,
    Sealed,
    Enum,
}

#[derive(Clone, Debug)]
pub struct TypeDecl {
    pub name: String,
    pub kind: TypeKind,
    pub underlying: Option<TypeRef>,
    pub fields: Vec<Field>,
    pub variants: Vec<String>,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Domain {
    Pure,
    Client,
    Server,
    Worker,
    Database,
    External,
}

impl Domain {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pure => "pure",
            Self::Client => "client",
            Self::Server => "server",
            Self::Worker => "worker",
            Self::Database => "database",
            Self::External => "external",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Parameter {
    pub name: String,
    pub type_ref: TypeRef,
}

#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    pub domain: Domain,
    pub component: bool,
    pub suspend: bool,
    pub parameters: Vec<Parameter>,
    pub return_type: TypeRef,
    pub effects: Vec<String>,
    pub requires: Vec<String>,
    pub body: Vec<Token>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct EnvironmentDecl {
    pub name: String,
    pub variable: String,
    pub type_ref: TypeRef,
    pub lifecycle: String,
    pub secret: bool,
    pub public: bool,
    pub optional: bool,
    pub default: Option<String>,
    pub span: Span,
}

#[derive(Clone, Debug, Default)]
pub struct Program {
    pub package: Option<String>,
    pub types: Vec<TypeDecl>,
    pub functions: Vec<Function>,
    pub environment: Vec<EnvironmentDecl>,
}

impl Program {
    pub fn merge(&mut self, mut other: Program) {
        if self.package.is_none() {
            self.package = other.package.take();
        }
        self.types.append(&mut other.types);
        self.functions.append(&mut other.functions);
        self.environment.append(&mut other.environment);
    }
}
