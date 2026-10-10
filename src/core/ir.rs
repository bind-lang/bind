//! The typed core IR: the single input of code generators. Every expression carries its
//! resolved type; operators are concrete; sugar and blocks are gone; patterns are elaborated.

use crate::types::exhaustive::Pat;
use crate::types::ty::Type;

pub type LocalId = usize;

#[derive(Clone, Debug, PartialEq)]
pub struct CoreProgram {
    pub types: Vec<CoreType>,
    pub functions: Vec<CoreFunction>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CoreType {
    Record {
        name: String,
        params: Vec<String>,
        fields: Vec<(String, Type)>,
    },
    Sum {
        name: String,
        params: Vec<String>,
        variants: Vec<(String, Vec<(String, Type)>)>,
    },
    Distinct {
        name: String,
        underlying: Type,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Domain {
    Pure,
    Inferred,
    Server,
    Client,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CoreFunction {
    pub name: String,
    pub domain: Domain,
    /// A `server fun` that is not a component: an RPC endpoint.
    pub endpoint: bool,
    /// Type variables of the scheme, in order of appearance (rendered by backends as generics).
    pub generics: Vec<usize>,
    pub params: Vec<(LocalId, String, Type)>,
    pub result: Type,
    /// Resolved effect labels, e.g. `["db.read"]`.
    pub effects: Vec<String>,
    pub body: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub ty: Type,
    pub kind: ExprKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    /// Decimal digits of an arbitrary-precision integer.
    Int(String),
    Double(String),
    Float(String),
    Str(String),
    Bool(bool),
    Unit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prim {
    IntAdd,
    IntSub,
    IntMul,
    IntNeg,
    IntDivChecked,
    IntRemChecked,
    DoubleAdd,
    DoubleSub,
    DoubleMul,
    DoubleDiv,
    DoubleNeg,
    FloatAdd,
    FloatSub,
    FloatMul,
    FloatDiv,
    FloatNeg,
    DecimalAdd,
    DecimalSub,
    DecimalMul,
    DecimalDivChecked,
    DecimalNeg,
    DurationAdd,
    DurationSub,
    DurationNeg,
    StringConcat,
    /// Structural equality / ordering on the operand type (`ty` of the operands).
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Not,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DbOp {
    Require,
    Find,
    Insert { columns: Vec<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    Lit(Literal),
    Local(LocalId),
    /// A program function; `instance` lists the concrete types for the callee's generics.
    Global {
        name: String,
        instance: Vec<Type>,
    },
    Builtin {
        name: String,
        instance: Vec<Type>,
    },
    Environment(String),
    /// Construct a record, variant, distinct value, `Some` or `null`.
    Ctor {
        type_name: String,
        ctor: String,
        args: Vec<Expr>,
    },
    None,
    Some(Box<Expr>),
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    /// `server.f` is not part of server code (Task 7 rejects it there); kept for completeness.
    ServerCall {
        function: String,
        args: Vec<Expr>,
    },
    Lambda {
        params: Vec<(LocalId, String, Type)>,
        body: Box<Expr>,
    },
    Let {
        local: LocalId,
        name: String,
        value: Box<Expr>,
        body: Box<Expr>,
    },
    /// Evaluate `value` for its effects, discard it, continue with `body`.
    Seq {
        value: Box<Expr>,
        body: Box<Expr>,
    },
    If {
        condition: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<Arm>,
    },
    Field {
        target: Box<Expr>,
        name: String,
    },
    Prim {
        op: Prim,
        args: Vec<Expr>,
    },
    Db {
        op: DbOp,
        table: String,
        args: Vec<Expr>,
    },
    Return(Box<Expr>),
    /// A UI tree: never part of server code; carried so client lowering (3c) can reuse the IR.
    Ui,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arm {
    /// The elaborated pattern (`Some(...)` explicit) recorded by the type checker.
    pub pattern: Pat,
    /// Variables bound by the pattern, in pattern order, with their types.
    pub bindings: Vec<(LocalId, String, Type)>,
    /// For variable bindings, the path from the scrutinee: constructor field indices, with
    /// `Some` unwraps written as index 0 of constructor `Some`.
    pub binding_paths: Vec<Vec<(String, usize)>>,
    pub body: Expr,
}
