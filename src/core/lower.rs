//! Lowering: AST + `TypedProgram` → core IR.
//!
//! The lowering trusts the type checker: it runs only on programs without errors and reads
//! every decision (name resolution, node types, elaborated patterns) from `TypedProgram`
//! instead of repeating it. A node the checker did not record gets `Type::Error`, which the
//! validator reports as an internal error.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{self, Function, Program, TypeDecl, TypeKind};
use crate::diagnostic::Span;
use crate::parser::expr::parse_type_text;
use crate::schema::DatabaseSchema;
use crate::syntax::expr::{
    self as syntax, BinaryOp, Block, FieldInit, NodeId, Pattern, Stmt, TypeExpr, UnaryOp,
};
use crate::types::effects::{Label, Row, RowVarId};
use crate::types::env::{UI_NODES, builtin_scheme};
use crate::types::exhaustive::Pat;
use crate::types::ty::Type;
use crate::types::{Resolution, TypedProgram};

use super::ir::*;
use super::specialize::{Specializer, default_vars, map_types};

pub fn lower(program: &Program, typed: &TypedProgram, schema: &DatabaseSchema) -> CoreProgram {
    let types = core_types(program);
    let ctors = constructors(&types);
    let functions = program
        .functions
        .iter()
        .filter_map(|function| {
            let mut lowerer = Lowerer {
                typed,
                schema,
                ctors: &ctors,
                function: function.name.clone(),
                generics: Vec::new(),
                markers: BTreeMap::new(),
                scope: Vec::new(),
                next_local: 0,
            };
            lowerer.function(function)
        })
        .collect();
    CoreProgram { types, functions }
}

// ---- declared types ----------------------------------------------------------------------

/// `Result` first, then the program's types in declaration order.
fn core_types(program: &Program) -> Vec<CoreType> {
    let marker = |name: &str| Type::con(&format!("'{name}"));
    let mut types = vec![CoreType::Sum {
        name: "Result".into(),
        params: vec!["T".into(), "E".into()],
        variants: vec![
            ("Ok".into(), vec![("value".into(), marker("T"))]),
            ("Err".into(), vec![("error".into(), marker("E"))]),
        ],
    }];
    types.extend(program.types.iter().map(core_type));
    types
}

fn core_type(declaration: &TypeDecl) -> CoreType {
    let params = declaration.type_params.clone();
    let fields = |fields: &[ast::Field]| -> Vec<(String, Type)> {
        fields
            .iter()
            .map(|field| {
                let ty = declared_type(&field.type_ref, &field.span, &params);
                (field.name.clone(), ty)
            })
            .collect()
    };
    match declaration.kind {
        TypeKind::Data | TypeKind::Entity => CoreType::Record {
            name: declaration.name.clone(),
            params: params.clone(),
            fields: fields(&declaration.fields),
        },
        TypeKind::Sealed | TypeKind::Enum => CoreType::Sum {
            name: declaration.name.clone(),
            params: params.clone(),
            variants: declaration
                .cases
                .iter()
                .map(|case| (case.name.clone(), fields(&case.fields)))
                .collect(),
        },
        TypeKind::Distinct => CoreType::Distinct {
            name: declaration.name.clone(),
            underlying: declaration
                .underlying
                .as_ref()
                .map_or(Type::Error, |underlying| {
                    declared_type(underlying, &declaration.span, &params)
                }),
        },
    }
}

/// A written type; the names in `params` become the markers `'T` (as in the checker's
/// `rigid_vars`). No environment is consulted: the checker already validated every name.
fn declared_type(type_ref: &ast::TypeRef, span: &Span, params: &[String]) -> Type {
    parse_type_text(&type_ref.display(), span).map_or(Type::Error, |te| from_type_expr(&te, params))
}

fn from_type_expr(te: &TypeExpr, params: &[String]) -> Type {
    match te {
        TypeExpr::Nullable(inner) => Type::option(from_type_expr(inner, params)),
        TypeExpr::Named { name, args, .. } if args.is_empty() && params.contains(name) => {
            Type::con(&format!("'{name}"))
        }
        TypeExpr::Named { name, args, .. } => Type::Con(
            name.clone(),
            args.iter().map(|arg| from_type_expr(arg, params)).collect(),
        ),
        TypeExpr::Function {
            params: inputs,
            result,
            effects,
        } => Type::Fun(
            inputs
                .iter()
                .map(|input| from_type_expr(input, params))
                .collect(),
            Box::new(from_type_expr(result, params)),
            Row::closed(effects.iter().filter_map(|name| Label::parse(name))),
        ),
    }
}

/// A constructor: the type it builds, that type's parameters and its fields (with markers).
struct CtorDef {
    type_name: String,
    params: Vec<String>,
    fields: Vec<(String, Type)>,
}

fn constructors(types: &[CoreType]) -> BTreeMap<String, CtorDef> {
    let mut ctors = BTreeMap::new();
    for ty in types {
        match ty {
            CoreType::Record {
                name,
                params,
                fields,
            } => {
                ctors.insert(
                    name.clone(),
                    CtorDef {
                        type_name: name.clone(),
                        params: params.clone(),
                        fields: fields.clone(),
                    },
                );
            }
            CoreType::Sum {
                name,
                params,
                variants,
            } => {
                for (variant, fields) in variants {
                    ctors.insert(
                        variant.clone(),
                        CtorDef {
                            type_name: name.clone(),
                            params: params.clone(),
                            fields: fields.clone(),
                        },
                    );
                }
            }
            CoreType::Distinct { name, underlying } => {
                ctors.insert(
                    name.clone(),
                    CtorDef {
                        type_name: name.clone(),
                        params: Vec::new(),
                        fields: vec![("value".into(), underlying.clone())],
                    },
                );
            }
        }
    }
    ctors
}

impl CtorDef {
    /// Field types at a concrete instance of the constructed type.
    fn field_types(&self, ty: &Type) -> Vec<Type> {
        let args = match ty {
            Type::Con(name, args) if *name == self.type_name => args.as_slice(),
            _ => &[],
        };
        let markers: BTreeMap<String, Type> = self
            .params
            .iter()
            .zip(args)
            .map(|(param, arg)| (format!("'{param}"), arg.clone()))
            .collect();
        self.fields
            .iter()
            .map(|(_, field)| replace_markers(field, &markers))
            .collect()
    }
}

fn replace_markers(ty: &Type, markers: &BTreeMap<String, Type>) -> Type {
    match ty {
        Type::Con(name, args) if args.is_empty() && name.starts_with('\'') => {
            markers.get(name).cloned().unwrap_or(Type::Error)
        }
        Type::Con(name, args) => Type::Con(
            name.clone(),
            args.iter()
                .map(|arg| replace_markers(arg, markers))
                .collect(),
        ),
        Type::Fun(params, result, row) => Type::Fun(
            params
                .iter()
                .map(|param| replace_markers(param, markers))
                .collect(),
            Box::new(replace_markers(result, markers)),
            row.clone(),
        ),
        other => other.clone(),
    }
}

// ---- type helpers ------------------------------------------------------------------------

/// Type variables of `ty`, in order of first appearance.
pub(super) fn type_vars(ty: &Type, output: &mut Vec<usize>) {
    match ty {
        Type::Var(id) => {
            if !output.contains(id) {
                output.push(*id);
            }
        }
        Type::Con(_, args) => args.iter().for_each(|arg| type_vars(arg, output)),
        Type::Fun(params, result, _) => {
            params.iter().for_each(|param| type_vars(param, output));
            type_vars(result, output);
        }
        Type::Error => {}
    }
}

/// Row variables of `ty` (the tails of its function rows).
fn row_tails(ty: &Type, output: &mut BTreeSet<RowVarId>) {
    match ty {
        Type::Con(_, args) => args.iter().for_each(|arg| row_tails(arg, output)),
        Type::Fun(params, result, row) => {
            params.iter().for_each(|param| row_tails(param, output));
            row_tails(result, output);
            output.extend(row.tail);
        }
        Type::Var(_) | Type::Error => {}
    }
}

/// Binds the variables of `scheme` to the matching parts of `actual` (rows are ignored).
pub(super) fn match_scheme(scheme: &Type, actual: &Type, output: &mut BTreeMap<usize, Type>) {
    match (scheme, actual) {
        (Type::Var(id), _) => {
            output.entry(*id).or_insert_with(|| actual.clone());
        }
        (Type::Con(a, xs), Type::Con(b, ys)) if a == b && xs.len() == ys.len() => {
            for (x, y) in xs.iter().zip(ys) {
                match_scheme(x, y, output);
            }
        }
        (Type::Fun(xs, x, _), Type::Fun(ys, y, _)) if xs.len() == ys.len() => {
            for (x, y) in xs.iter().zip(ys) {
                match_scheme(x, y, output);
            }
            match_scheme(x, y, output);
        }
        _ => {}
    }
}

/// The concrete types of the scheme's variables (in order of appearance) at `actual`.
fn instance(scheme: &Type, actual: &Type) -> Vec<Type> {
    let mut vars = Vec::new();
    type_vars(scheme, &mut vars);
    let mut bound = BTreeMap::new();
    match_scheme(scheme, actual, &mut bound);
    vars.iter()
        .map(|var| bound.get(var).cloned().unwrap_or(Type::Error))
        .collect()
}

/// Pairs a declared type (with `'T` markers) with the checker's type, recording each marker.
fn collect_markers(declared: &Type, actual: &Type, output: &mut BTreeMap<String, Type>) {
    match (declared, actual) {
        (Type::Con(name, args), _) if args.is_empty() && name.starts_with('\'') => {
            output.entry(name.clone()).or_insert_with(|| actual.clone());
        }
        (Type::Con(a, xs), Type::Con(b, ys)) if a == b && xs.len() == ys.len() => {
            for (x, y) in xs.iter().zip(ys) {
                collect_markers(x, y, output);
            }
        }
        (Type::Fun(xs, x, _), Type::Fun(ys, y, _)) if xs.len() == ys.len() => {
            for (x, y) in xs.iter().zip(ys) {
                collect_markers(x, y, output);
            }
            collect_markers(x, y, output);
        }
        _ => {}
    }
}

fn option_inner(ty: &Type) -> Option<&Type> {
    match ty {
        Type::Con(name, args) if name == "Option" && args.len() == 1 => args.first(),
        _ => None,
    }
}

fn scalar_name(ty: &Type) -> Option<&str> {
    match ty {
        Type::Con(name, args) if args.is_empty() => Some(name.as_str()),
        _ => None,
    }
}

fn binary_prim(op: BinaryOp, operand: &Type) -> Option<Prim> {
    let simple = match op {
        BinaryOp::Eq => Some(Prim::Eq),
        BinaryOp::NotEq => Some(Prim::NotEq),
        BinaryOp::Lt => Some(Prim::Lt),
        BinaryOp::LtEq => Some(Prim::LtEq),
        BinaryOp::Gt => Some(Prim::Gt),
        BinaryOp::GtEq => Some(Prim::GtEq),
        _ => None,
    };
    if simple.is_some() {
        return simple;
    }
    use Prim::*;
    Some(match (op, scalar_name(operand)?) {
        (BinaryOp::Add, "Int") => IntAdd,
        (BinaryOp::Sub, "Int") => IntSub,
        (BinaryOp::Mul, "Int") => IntMul,
        (BinaryOp::Div, "Int") => IntDivChecked,
        (BinaryOp::Rem, "Int") => IntRemChecked,
        (BinaryOp::Add, "Double") => DoubleAdd,
        (BinaryOp::Sub, "Double") => DoubleSub,
        (BinaryOp::Mul, "Double") => DoubleMul,
        (BinaryOp::Div, "Double") => DoubleDiv,
        (BinaryOp::Add, "Float") => FloatAdd,
        (BinaryOp::Sub, "Float") => FloatSub,
        (BinaryOp::Mul, "Float") => FloatMul,
        (BinaryOp::Div, "Float") => FloatDiv,
        (BinaryOp::Add, "Decimal" | "Money") => DecimalAdd,
        (BinaryOp::Sub, "Decimal" | "Money") => DecimalSub,
        (BinaryOp::Mul, "Decimal") => DecimalMul,
        (BinaryOp::Div, "Decimal") => DecimalDivChecked,
        (BinaryOp::Add, "Duration") => DurationAdd,
        (BinaryOp::Sub, "Duration") => DurationSub,
        (BinaryOp::Add, "String") => StringConcat,
        _ => return None,
    })
}

fn negation(operand: &Type) -> Option<Prim> {
    Some(match scalar_name(operand)? {
        "Int" => Prim::IntNeg,
        "Double" => Prim::DoubleNeg,
        "Float" => Prim::FloatNeg,
        "Decimal" | "Money" => Prim::DecimalNeg,
        "Duration" => Prim::DurationNeg,
        _ => return None,
    })
}

/// Decimal digits of an integer literal. The parser leaves hexadecimal and binary literals
/// that do not fit in `u128` as written (`0x...`, `0b...`); they are converted here.
fn decimal_digits(text: &str) -> String {
    let (digits, base) = if let Some(hex) = text.strip_prefix("0x") {
        (hex, 16)
    } else if let Some(bin) = text.strip_prefix("0b") {
        (bin, 2)
    } else {
        return text.to_string();
    };
    // Little-endian base-10 digits, multiplied by `base` and incremented per input digit.
    let mut output: Vec<u32> = vec![0];
    for ch in digits.trim_end_matches('l').chars() {
        let Some(mut carry) = ch.to_digit(base) else {
            return text.to_string();
        };
        for digit in &mut output {
            let value = *digit * base + carry;
            *digit = value % 10;
            carry = value / 10;
        }
        while carry > 0 {
            output.push(carry % 10);
            carry /= 10;
        }
    }
    while output.len() > 1 && output.last() == Some(&0) {
        output.pop();
    }
    output
        .iter()
        .rev()
        .filter_map(|digit| char::from_digit(*digit, 10))
        .collect()
}

/// `db.<table>.<method>` as written: the id of the `db` name, the table and the method.
fn db_target(expr: &syntax::Expr) -> Option<(NodeId, &str, &str)> {
    let syntax::ExprKind::Field {
        target,
        name: method,
    } = &expr.kind
    else {
        return None;
    };
    let syntax::ExprKind::Field {
        target: root,
        name: table,
    } = &target.kind
    else {
        return None;
    };
    match &root.kind {
        syntax::ExprKind::Name(db) if db == "db" => Some((root.id, table, method)),
        _ => None,
    }
}

fn domain(domain: &ast::Domain) -> Domain {
    match domain {
        ast::Domain::Pure => Domain::Pure,
        ast::Domain::Inferred => Domain::Inferred,
        ast::Domain::Server => Domain::Server,
        ast::Domain::Client => Domain::Client,
        ast::Domain::Worker | ast::Domain::Database | ast::Domain::External => Domain::Other,
    }
}

fn expr(ty: Type, kind: ExprKind) -> Expr {
    Expr { ty, kind }
}

fn boxed(ty: Type, kind: ExprKind) -> Box<Expr> {
    Box::new(expr(ty, kind))
}

/// A path from a scrutinee to a bound variable (see `Arm::binding_paths`).
type BindingPath = Vec<(String, usize)>;

/// A pattern variable: its name, type and path.
type Binding = (String, Type, BindingPath);

// ---- functions and expressions -----------------------------------------------------------

struct Lowerer<'a> {
    typed: &'a TypedProgram,
    schema: &'a DatabaseSchema,
    ctors: &'a BTreeMap<String, CtorDef>,
    function: String,
    /// Type variables of the function's scheme; any other variable becomes `Unit`.
    generics: Vec<usize>,
    /// Rigid type parameters (`'T`) of an annotated generic body, mapped to the scheme's
    /// variables, so the body and the signature agree.
    markers: BTreeMap<String, Type>,
    scope: Vec<(String, LocalId)>,
    next_local: LocalId,
}

impl Lowerer<'_> {
    fn function(&mut self, function: &Function) -> Option<CoreFunction> {
        let block = function.expr.as_ref()?;
        let signature = self
            .typed
            .signatures
            .get(&function.name)
            .cloned()
            .unwrap_or(Type::Error);
        type_vars(&signature, &mut self.generics);
        let mut effect_rows = BTreeSet::new();
        row_tails(&signature, &mut effect_rows);
        // A signature that is not a function of the declared arity cannot be trusted: the
        // function is kept, with `Type::Error` where the types are missing, for the validator.
        let (param_types, result) = match &signature {
            Type::Fun(params, result, _) if params.len() == function.parameters.len() => {
                (params.clone(), (**result).clone())
            }
            Type::Fun(params, _, _) => (params.clone(), Type::Error),
            _ => (Vec::new(), Type::Error),
        };
        if !function.type_params.is_empty() {
            for (parameter, actual) in function.parameters.iter().zip(&param_types) {
                if let Some(type_ref) = &parameter.type_ref {
                    let declared = declared_type(type_ref, &function.span, &function.type_params);
                    collect_markers(&declared, actual, &mut self.markers);
                }
            }
            if let Some(type_ref) = &function.return_type {
                let declared = declared_type(type_ref, &function.span, &function.type_params);
                collect_markers(&declared, &result, &mut self.markers);
            }
        }
        let params: Vec<(LocalId, String, Type)> = function
            .parameters
            .iter()
            .enumerate()
            .map(|(index, parameter)| {
                let local = self.bind(&parameter.name);
                let ty = param_types
                    .get(index)
                    .map_or(Type::Error, |ty| self.normalize(ty));
                (local, parameter.name.clone(), ty)
            })
            .collect();
        let mut body = self.block(block);
        let mut specializer = Specializer {
            generics: &self.generics,
            effect_rows,
            next_local: self.next_local,
        };
        specializer.run(&mut body);
        self.next_local = specializer.next_local;
        let generics = self.generics.clone();
        let default = |ty: &Type| default_vars(ty, &generics);
        map_types(&mut body, &default);
        let domain = domain(&function.domain);
        Some(CoreFunction {
            name: function.name.clone(),
            endpoint: domain == Domain::Server && !function.component,
            domain,
            params: params
                .into_iter()
                .map(|(local, name, ty)| (local, name, default(&ty)))
                .collect(),
            result: default(&self.normalize(&result)),
            effects: self
                .typed
                .effects
                .get(&function.name)
                .cloned()
                .unwrap_or_default(),
            body,
            generics,
        })
    }

    /// Replaces the rigid markers of an annotated generic body by the scheme's variables.
    /// Other variables are kept: generalized local `val`s are specialized afterwards, and the
    /// variables left over are defaulted to `Unit` (see `specialize`).
    fn normalize(&self, ty: &Type) -> Type {
        match ty {
            Type::Con(name, args) if args.is_empty() && name.starts_with('\'') => self
                .markers
                .get(name)
                .cloned()
                .unwrap_or_else(|| Type::con("Unit")),
            Type::Con(name, args) => Type::Con(
                name.clone(),
                args.iter().map(|arg| self.normalize(arg)).collect(),
            ),
            Type::Fun(params, result, row) => Type::Fun(
                params.iter().map(|param| self.normalize(param)).collect(),
                Box::new(self.normalize(result)),
                row.clone(),
            ),
            other => other.clone(),
        }
    }

    fn node_type(&self, id: NodeId) -> Type {
        self.typed
            .node_types
            .get(&(self.function.clone(), id))
            .map_or(Type::Error, |ty| self.normalize(ty))
    }

    fn resolution(&self, id: NodeId) -> Option<&Resolution> {
        self.typed.resolutions.get(&(self.function.clone(), id))
    }

    fn fresh(&mut self) -> LocalId {
        let local = self.next_local;
        self.next_local += 1;
        local
    }

    fn bind(&mut self, name: &str) -> LocalId {
        let local = self.fresh();
        self.scope.push((name.to_string(), local));
        local
    }

    fn lookup(&self, name: &str) -> Option<LocalId> {
        self.scope
            .iter()
            .rev()
            .find(|(other, _)| other == name)
            .map(|(_, local)| *local)
    }

    fn block(&mut self, block: &Block) -> Expr {
        let mark = self.scope.len();
        let lowered = self.stmts(&block.stmts);
        self.scope.truncate(mark);
        lowered
    }

    /// `val x = e; rest` → `Let`; `val _ = e; rest` and `e; rest` → `Seq`; a block that ends
    /// with a `val` (or is empty) has the value `()`.
    fn stmts(&mut self, stmts: &[Stmt]) -> Expr {
        let Some((first, rest)) = stmts.split_first() else {
            return expr(Type::con("Unit"), ExprKind::Lit(Literal::Unit));
        };
        match first {
            Stmt::Expr(value) if rest.is_empty() => self.expr(value),
            Stmt::Expr(value) => {
                let value = self.expr(value);
                let body = self.stmts(rest);
                expr(
                    body.ty.clone(),
                    ExprKind::Seq {
                        value: Box::new(value),
                        body: Box::new(body),
                    },
                )
            }
            Stmt::Val { name, value, .. } => {
                let value = Box::new(self.expr(value));
                if name == "_" {
                    let body = self.stmts(rest);
                    return expr(
                        body.ty.clone(),
                        ExprKind::Seq {
                            value,
                            body: Box::new(body),
                        },
                    );
                }
                let local = self.bind(name);
                let body = self.stmts(rest);
                expr(
                    body.ty.clone(),
                    ExprKind::Let {
                        local,
                        name: name.clone(),
                        value,
                        body: Box::new(body),
                    },
                )
            }
        }
    }

    fn exprs(&mut self, exprs: &[syntax::Expr]) -> Vec<Expr> {
        exprs.iter().map(|item| self.expr(item)).collect()
    }

    fn expr(&mut self, node: &syntax::Expr) -> Expr {
        use syntax::ExprKind as K;
        let ty = self.node_type(node.id);
        let kind = match &node.kind {
            K::Int(text) => ExprKind::Lit(Literal::Int(decimal_digits(text))),
            K::Double(text) => ExprKind::Lit(Literal::Double(text.clone())),
            K::Float(text) => ExprKind::Lit(Literal::Float(text.clone())),
            K::Str(text) => ExprKind::Lit(Literal::Str(text.clone())),
            K::Bool(value) => ExprKind::Lit(Literal::Bool(*value)),
            K::Null => ExprKind::None,
            K::Name(name) => return self.name(node.id, name, ty),
            K::Call {
                callee,
                args,
                children: _,
            } => return self.call(callee, args, ty),
            K::Field { target, name } => match self.resolution(node.id).cloned() {
                Some(Resolution::Variant(ctor)) => return self.ctor_value(&ctor, ty),
                Some(Resolution::ServerCall(function)) => {
                    return self.server_call_value(&function, ty);
                }
                _ => ExprKind::Field {
                    target: Box::new(self.expr(target)),
                    name: name.clone(),
                },
            },
            K::SafeField { target, name } => return self.safe_field(target, name, ty),
            K::Unary { op, operand } => {
                let operand = self.expr(operand);
                let prim = match op {
                    UnaryOp::Not => Some(Prim::Not),
                    UnaryOp::Neg => negation(&operand.ty),
                };
                return self.prim(prim, vec![operand], ty);
            }
            K::Binary {
                op: op @ (BinaryOp::And | BinaryOp::Or),
                left,
                right,
            } => {
                // Short-circuit kept in the structure: `a && b` is `if (a) b else false`,
                // `a || b` is `if (a) true else b`.
                let condition = Box::new(self.expr(left));
                let right = Box::new(self.expr(right));
                let boolean =
                    |value| boxed(Type::con("Boolean"), ExprKind::Lit(Literal::Bool(value)));
                let (then_branch, else_branch) = if *op == BinaryOp::And {
                    (right, boolean(false))
                } else {
                    (boolean(true), right)
                };
                ExprKind::If {
                    condition,
                    then_branch,
                    else_branch,
                }
            }
            K::Binary { op, left, right } => {
                let left = self.expr(left);
                let right = self.expr(right);
                let prim = binary_prim(*op, &left.ty);
                return self.prim(prim, vec![left, right], ty);
            }
            K::Elvis { value, fallback } => return self.elvis(value, fallback, ty),
            K::If {
                condition,
                then_branch,
                else_branch,
            } => ExprKind::If {
                condition: Box::new(self.expr(condition)),
                then_branch: Box::new(self.expr(then_branch)),
                else_branch: Box::new(self.expr(else_branch)),
            },
            K::When { scrutinee, arms } => {
                let (kind, complete) = self.when(node.id, scrutinee, arms);
                return expr(if complete { ty } else { Type::Error }, kind);
            }
            K::Lambda { params, body } => self.lambda(params, body, &ty),
            K::Block(block) => return self.block(block),
            K::Return(value) => ExprKind::Return(Box::new(self.expr(value))),
            K::Builder { target, fields } => return self.builder(target, fields, ty),
        };
        expr(ty, kind)
    }

    /// An operator the lowering cannot make concrete becomes an error-typed marker, for the
    /// validator to report.
    fn prim(&self, prim: Option<Prim>, args: Vec<Expr>, ty: Type) -> Expr {
        match prim {
            Some(op) => expr(ty, ExprKind::Prim { op, args }),
            None => expr(Type::Error, ExprKind::Lit(Literal::Unit)),
        }
    }

    fn name(&mut self, id: NodeId, name: &str, ty: Type) -> Expr {
        let kind = match self.resolution(id).cloned() {
            Some(Resolution::Local) => match self.lookup(name) {
                Some(local) => ExprKind::Local(local),
                None => return self.unresolved(name),
            },
            Some(Resolution::Function(callee)) => {
                let scheme = self.typed.signatures.get(&callee).unwrap_or(&Type::Error);
                ExprKind::Global {
                    instance: instance(scheme, &ty),
                    name: callee,
                }
            }
            Some(Resolution::Builtin(builtin)) => ExprKind::Builtin {
                instance: instance(
                    &builtin_scheme(&builtin).map_or(Type::Error, |scheme| scheme.ty),
                    &ty,
                ),
                name: builtin,
            },
            Some(Resolution::Constructor(ctor) | Resolution::Variant(ctor)) => {
                return self.ctor_value(&ctor, ty);
            }
            Some(Resolution::Environment(value)) => ExprKind::Environment(value),
            Some(Resolution::ServerCall(function)) => {
                return self.server_call_value(&function, ty);
            }
            None => return self.unresolved(name),
        };
        expr(ty, kind)
    }

    /// A name the checker did not resolve: kept under `Type::Error` for the validator.
    fn unresolved(&self, name: &str) -> Expr {
        expr(
            Type::Error,
            ExprKind::Global {
                name: name.to_string(),
                instance: Vec::new(),
            },
        )
    }

    /// A constructor application; an unknown constructor is typed `Type::Error`.
    fn ctor(&self, ctor: &str, args: Vec<Expr>, ty: Type) -> Expr {
        let (type_name, ty) = match self.ctors.get(ctor) {
            Some(def) => (def.type_name.clone(), ty),
            None => (ctor.to_string(), Type::Error),
        };
        expr(
            ty,
            ExprKind::Ctor {
                type_name,
                ctor: ctor.to_string(),
                args,
            },
        )
    }

    /// A constructor used as a value: the value itself, or a lambda that builds it.
    fn ctor_value(&mut self, ctor: &str, ty: Type) -> Expr {
        let Type::Fun(param_types, result, _) = &ty else {
            return self.ctor(ctor, Vec::new(), ty);
        };
        let names: Vec<String> = self.ctors.get(ctor).map_or_else(Vec::new, |def| {
            def.fields.iter().map(|(name, _)| name.clone()).collect()
        });
        let params: Vec<(LocalId, String, Type)> = param_types
            .iter()
            .enumerate()
            .map(|(index, param)| {
                let name = names
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| format!("arg{index}"));
                (self.fresh(), name, param.clone())
            })
            .collect();
        let args = params
            .iter()
            .map(|(local, _, param)| expr(param.clone(), ExprKind::Local(*local)))
            .collect();
        let body = Box::new(self.ctor(ctor, args, (**result).clone()));
        expr(ty, ExprKind::Lambda { params, body })
    }

    /// `server.f` used as a value: a lambda that performs the call.
    fn server_call_value(&mut self, function: &str, ty: Type) -> Expr {
        let Type::Fun(param_types, result, _) = &ty else {
            return expr(Type::Error, ExprKind::Environment(function.to_string()));
        };
        let params: Vec<(LocalId, String, Type)> = param_types
            .iter()
            .enumerate()
            .map(|(index, param)| (self.fresh(), format!("arg{index}"), param.clone()))
            .collect();
        let args = params
            .iter()
            .map(|(local, _, param)| expr(param.clone(), ExprKind::Local(*local)))
            .collect();
        let body = boxed(
            (**result).clone(),
            ExprKind::ServerCall {
                function: function.to_string(),
                args,
            },
        );
        expr(ty, ExprKind::Lambda { params, body })
    }

    fn call(&mut self, callee: &syntax::Expr, args: &[syntax::Expr], ty: Type) -> Expr {
        if let syntax::ExprKind::Name(name) = &callee.kind {
            match self.resolution(callee.id).cloned() {
                None if UI_NODES.contains(&name.as_str()) => return expr(ty, ExprKind::Ui),
                Some(Resolution::Builtin(builtin)) if builtin == "listOf" => {
                    let args = self.exprs(args);
                    let element = list_element(&ty);
                    let callee_ty = Type::Fun(
                        args.iter().map(|arg| arg.ty.clone()).collect(),
                        Box::new(ty.clone()),
                        Row::empty(),
                    );
                    let callee = boxed(
                        callee_ty,
                        ExprKind::Builtin {
                            name: builtin,
                            instance: vec![element],
                        },
                    );
                    return expr(ty, ExprKind::Call { callee, args });
                }
                Some(Resolution::Constructor(ctor)) => {
                    let args = self.exprs(args);
                    return self.ctor(&ctor, args, ty);
                }
                _ => {}
            }
        }
        if let Some((root, table, method)) = db_target(callee)
            && self.resolution(root).is_none()
        {
            let op = match method {
                "require" => DbOp::Require,
                "find" => DbOp::Find,
                _ => return self.unresolved(method),
            };
            let args = self.exprs(args);
            let ty = self.db_type(ty, table);
            return expr(
                ty,
                ExprKind::Db {
                    op,
                    table: table.to_string(),
                    args,
                },
            );
        }
        if let syntax::ExprKind::Field { .. } = &callee.kind {
            match self.resolution(callee.id).cloned() {
                Some(Resolution::Variant(ctor)) => {
                    let args = self.exprs(args);
                    return self.ctor(&ctor, args, ty);
                }
                Some(Resolution::ServerCall(function)) => {
                    let args = self.exprs(args);
                    return expr(ty, ExprKind::ServerCall { function, args });
                }
                _ => {}
            }
        }
        let callee = Box::new(self.expr(callee));
        let args = self.exprs(args);
        expr(ty, ExprKind::Call { callee, args })
    }

    /// The node's type; the table's model if the checker recorded none.
    fn db_type(&self, ty: Type, table: &str) -> Type {
        match (&ty, self.schema.tables.get(table)) {
            (Type::Error, Some(definition)) => Type::con(&definition.model),
            _ => ty,
        }
    }

    fn builder(&mut self, target: &syntax::Expr, fields: &[FieldInit], ty: Type) -> Expr {
        match db_target(target) {
            Some((root, table, "insert")) if self.resolution(root).is_none() => {
                let columns = fields.iter().map(|field| field.name.clone()).collect();
                let args = fields.iter().map(|field| self.expr(&field.value)).collect();
                let ty = self.db_type(ty, table);
                expr(
                    ty,
                    ExprKind::Db {
                        op: DbOp::Insert { columns },
                        table: table.to_string(),
                        args,
                    },
                )
            }
            _ => self.unresolved("insert"),
        }
    }

    /// `a ?: b` → `match a { null -> b; Some(x) -> x }`.
    fn elvis(&mut self, value: &syntax::Expr, fallback: &syntax::Expr, ty: Type) -> Expr {
        let value = self.expr(value);
        let fallback = self.expr(fallback);
        let inner = option_inner(&value.ty)
            .cloned()
            .unwrap_or_else(|| ty.clone());
        let local = self.fresh();
        let some = Arm {
            pattern: Pat::Ctor("Some".into(), vec![Pat::Wild]),
            bindings: vec![(local, "value".into(), inner.clone())],
            binding_paths: vec![vec![("Some".into(), 0)]],
            body: expr(inner, ExprKind::Local(local)),
        };
        expr(
            ty,
            ExprKind::Match {
                scrutinee: Box::new(value),
                arms: vec![null_arm(fallback), some],
            },
        )
    }

    /// `t?.n` → `match t { null -> null; Some(x) -> x.n }`, wrapping `x.n` in `Some` unless the
    /// checker flattened a field that is already nullable.
    fn safe_field(&mut self, target: &syntax::Expr, name: &str, ty: Type) -> Expr {
        let target = self.expr(target);
        let inner = option_inner(&target.ty).cloned().unwrap_or(Type::Error);
        let field_ty = self.field_type(&inner, name);
        let local = self.fresh();
        let access = expr(
            field_ty.clone(),
            ExprKind::Field {
                target: boxed(inner.clone(), ExprKind::Local(local)),
                name: name.to_string(),
            },
        );
        let present = if field_ty == ty {
            access
        } else {
            expr(ty.clone(), ExprKind::Some(Box::new(access)))
        };
        let some = Arm {
            pattern: Pat::Ctor("Some".into(), vec![Pat::Wild]),
            bindings: vec![(local, "value".into(), inner)],
            binding_paths: vec![vec![("Some".into(), 0)]],
            body: present,
        };
        expr(
            ty.clone(),
            ExprKind::Match {
                scrutinee: Box::new(target),
                arms: vec![null_arm(expr(ty, ExprKind::None)), some],
            },
        )
    }

    /// The type of field `name` of a record type, at its type arguments.
    fn field_type(&self, record: &Type, name: &str) -> Type {
        let Type::Con(type_name, _) = record else {
            return Type::Error;
        };
        let Some(def) = self
            .ctors
            .get(type_name)
            .filter(|def| def.type_name == *type_name)
        else {
            return Type::Error;
        };
        def.fields
            .iter()
            .zip(def.field_types(record))
            .find(|((field, _), _)| field == name)
            .map_or(Type::Error, |(_, ty)| ty)
    }

    /// The `Match`, and whether every arm had its elaborated pattern.
    fn when(
        &mut self,
        id: NodeId,
        scrutinee: &syntax::Expr,
        arms: &[syntax::Arm],
    ) -> (ExprKind, bool) {
        let scrutinee = self.expr(scrutinee);
        let recorded = self.typed.patterns.get(&(self.function.clone(), id));
        // Without one elaborated pattern per arm the match cannot be trusted: the `Match` is
        // typed `Type::Error` (its arms get `_` placeholders).
        let complete = recorded.is_some_and(|patterns| patterns.len() == arms.len());
        let patterns = recorded.cloned().unwrap_or_default();
        let arms = arms
            .iter()
            .enumerate()
            .map(|(index, arm)| {
                let pattern = patterns.get(index).cloned().unwrap_or(Pat::Wild);
                let mut found = Vec::new();
                self.pattern_bindings(
                    &arm.pattern,
                    &pattern,
                    &scrutinee.ty,
                    &mut Vec::new(),
                    &mut found,
                );
                let mark = self.scope.len();
                let mut bindings = Vec::new();
                let mut binding_paths = Vec::new();
                for (name, ty, path) in found {
                    let local = self.bind(&name);
                    bindings.push((local, name, ty));
                    binding_paths.push(path);
                }
                let body = self.expr(&arm.body);
                self.scope.truncate(mark);
                Arm {
                    pattern,
                    bindings,
                    binding_paths,
                    body,
                }
            })
            .collect();
        let kind = ExprKind::Match {
            scrutinee: Box::new(scrutinee),
            arms,
        };
        (kind, complete)
    }

    /// Walks the written pattern with its elaboration, collecting each variable with its type
    /// and path. A `Some` in the elaboration that the source does not write (an optional layer
    /// unwrapped by a constructor, literal or smart-cast variable) is a step of the path.
    fn pattern_bindings(
        &self,
        pattern: &Pattern,
        pat: &Pat,
        ty: &Type,
        path: &mut BindingPath,
        output: &mut Vec<Binding>,
    ) {
        if let (Pat::Ctor(name, inner), Some(unwrapped)) = (pat, option_inner(ty))
            && name == "Some"
            && let [inner] = inner.as_slice()
            && !matches!(pattern, Pattern::Null(_))
        {
            path.push(("Some".into(), 0));
            self.pattern_bindings(pattern, inner, unwrapped, path, output);
            path.pop();
            return;
        }
        match (pattern, pat) {
            (Pattern::Name(name, _), Pat::Wild) => {
                output.push((name.clone(), ty.clone(), path.clone()));
            }
            (Pattern::Constructor { name, args, .. }, Pat::Ctor(_, pats)) => {
                let field_types = self
                    .ctors
                    .get(name)
                    .map_or_else(Vec::new, |def| def.field_types(ty));
                for (index, (arg, pat)) in args.iter().zip(pats).enumerate() {
                    let field = field_types.get(index).cloned().unwrap_or(Type::Error);
                    path.push((name.clone(), index));
                    self.pattern_bindings(arg, pat, &field, path, output);
                    path.pop();
                }
            }
            _ => {}
        }
    }

    fn lambda(&mut self, params: &[(String, Span)], body: &Block, ty: &Type) -> ExprKind {
        let param_types = match ty {
            Type::Fun(param_types, _, _) => param_types.clone(),
            _ => Vec::new(),
        };
        let mark = self.scope.len();
        let params = params
            .iter()
            .enumerate()
            .map(|(index, (name, _))| {
                let local = self.bind(name);
                let ty = param_types.get(index).cloned().unwrap_or(Type::Error);
                (local, name.clone(), ty)
            })
            .collect();
        let body = Box::new(self.block(body));
        self.scope.truncate(mark);
        ExprKind::Lambda { params, body }
    }
}

fn null_arm(body: Expr) -> Arm {
    Arm {
        pattern: Pat::Ctor("null".into(), Vec::new()),
        bindings: Vec::new(),
        binding_paths: Vec::new(),
        body,
    }
}

/// The element type of `List<T>`.
fn list_element(ty: &Type) -> Type {
    match ty {
        Type::Con(name, args) if name == "List" && args.len() == 1 => args[0].clone(),
        _ => Type::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::has_errors;
    use crate::types::exhaustive::Pat;
    use crate::types::ty::{Type, display};

    const CLIENTES: &str = "table,model,primary_key,column,type,nullable,required,has_default,engine\nclientes,Cliente,id,id,ClienteId,false,true,true,postgresql\nclientes,Cliente,id,nome,String,false,true,false,postgresql\nclientes,Cliente,id,apelido,String,true,false,false,postgresql\n";

    const ENTIDADE: &str = "distinct type ClienteId = UUID\nentity Cliente(val id: ClienteId, val nome: String, val apelido: String?)\n";

    fn schema(csv: &str) -> DatabaseSchema {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("bind-core-{}-{id}.csv", std::process::id()));
        std::fs::write(&path, csv).unwrap();
        let schema = DatabaseSchema::load(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        schema
    }

    fn compile_with(source: &str, schema: &DatabaseSchema) -> CoreProgram {
        let (tokens, mut diagnostics) = crate::lexer::lex("test.bd", source);
        let (program, mut parse_diagnostics) = crate::parser::parse(tokens);
        diagnostics.append(&mut parse_diagnostics);
        assert!(!has_errors(&diagnostics), "{diagnostics:?}");
        let (typed, diagnostics) = crate::types::check(&program, schema);
        assert!(!has_errors(&diagnostics), "{diagnostics:?}");
        lower(&program, &typed, schema)
    }

    fn compile(source: &str) -> CoreProgram {
        compile_with(source, &DatabaseSchema::default())
    }

    fn function<'p>(program: &'p CoreProgram, name: &str) -> &'p CoreFunction {
        program
            .functions
            .iter()
            .find(|function| function.name == name)
            .unwrap_or_else(|| panic!("no function `{name}` in {program:#?}"))
    }

    fn body(source: &str, name: &str) -> String {
        render(&function(&compile(source), name).body)
    }

    fn types(list: &[Type]) -> String {
        list.iter().map(display).collect::<Vec<_>>().join(", ")
    }

    fn all(list: &[Expr]) -> String {
        list.iter().map(render).collect::<Vec<_>>().join(", ")
    }

    fn pattern(pat: &Pat) -> String {
        match pat {
            Pat::Wild => "_".into(),
            Pat::Lit(text) => text.clone(),
            Pat::Ctor(name, args) if args.is_empty() => name.clone(),
            Pat::Ctor(name, args) => {
                format!(
                    "{name}({})",
                    args.iter().map(pattern).collect::<Vec<_>>().join(", ")
                )
            }
        }
    }

    /// A short rendering of an expression; `:T` suffixes show the types worth checking.
    fn render(expr: &Expr) -> String {
        match &expr.kind {
            ExprKind::Lit(Literal::Int(text)) => format!("Lit({text})"),
            ExprKind::Lit(Literal::Double(text) | Literal::Float(text)) => format!("Lit({text})"),
            ExprKind::Lit(Literal::Str(text)) => format!("Lit({text:?})"),
            ExprKind::Lit(Literal::Bool(value)) => format!("Lit({value})"),
            ExprKind::Lit(Literal::Unit) => "Lit(())".into(),
            ExprKind::Local(id) => format!("Local({id})"),
            ExprKind::Global { name, instance } => format!("Global({name}[{}])", types(instance)),
            ExprKind::Builtin { name, instance } => {
                format!("Builtin({name}[{}])", types(instance))
            }
            ExprKind::Environment(name) => format!("Env({name})"),
            ExprKind::Ctor { ctor, args, .. } if args.is_empty() => format!("Ctor({ctor})"),
            ExprKind::Ctor { ctor, args, .. } => format!("Ctor({ctor}, {})", all(args)),
            ExprKind::None => format!("None:{}", display(&expr.ty)),
            ExprKind::Some(inner) => format!("Some({})", render(inner)),
            ExprKind::Call { callee, args } => format!("Call({}, [{}])", render(callee), all(args)),
            ExprKind::ServerCall { function, args } => {
                format!("Server({function}, [{}])", all(args))
            }
            ExprKind::Lambda { params, body } => {
                let params: Vec<String> = params
                    .iter()
                    .map(|(id, name, ty)| format!("{id} {name}: {}", display(ty)))
                    .collect();
                format!("Lambda({} -> {})", params.join(", "), render(body))
            }
            ExprKind::Let {
                local,
                name,
                value,
                body,
            } => format!("Let({local} {name} = {}; {})", render(value), render(body)),
            ExprKind::Seq { value, body } => format!("Seq({}; {})", render(value), render(body)),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => format!(
                "If({}, {}, {})",
                render(condition),
                render(then_branch),
                render(else_branch)
            ),
            ExprKind::Match { scrutinee, arms } => {
                let arms: Vec<String> = arms
                    .iter()
                    .map(|arm| {
                        let bindings: Vec<String> = arm
                            .bindings
                            .iter()
                            .map(|(id, name, ty)| format!(" {id} {name}: {}", display(ty)))
                            .collect();
                        format!(
                            "{}{} => {}",
                            pattern(&arm.pattern),
                            bindings.concat(),
                            render(&arm.body)
                        )
                    })
                    .collect();
                format!(
                    "Match:{}({}, [{}])",
                    display(&expr.ty),
                    render(scrutinee),
                    arms.join("; ")
                )
            }
            ExprKind::Field { target, name } => format!("Field({}, {name})", render(target)),
            ExprKind::Prim { op, args } => format!("{op:?}:{}({})", display(&expr.ty), all(args)),
            ExprKind::Db { op, table, args } => format!("Db({op:?}, {table}, [{}])", all(args)),
            ExprKind::Return(value) => format!("Return({})", render(value)),
            ExprKind::Ui => "Ui".into(),
        }
    }

    #[test]
    fn integer_addition_is_concrete() {
        let program = compile("fun f(x: Int) = x + 1");
        let f = function(&program, "f");
        assert_eq!(render(&f.body), "IntAdd:Int(Local(0), Lit(1))");
        assert_eq!(f.result, Type::con("Int"));
        assert_eq!(f.params, vec![(0, "x".to_string(), Type::con("Int"))]);
        assert_eq!(f.domain, Domain::Inferred);
        assert!(!f.endpoint);
        assert!(f.generics.is_empty());
    }

    #[test]
    fn operators_are_chosen_by_operand_type() {
        assert_eq!(
            body("fun f(a: Money, b: Money) = a + b", "f"),
            "DecimalAdd:Money(Local(0), Local(1))"
        );
        assert_eq!(
            body("fun f(a: String, b: String) = a + b", "f"),
            "StringConcat:String(Local(0), Local(1))"
        );
        assert_eq!(
            body("fun f(a: Double, b: Double) = a / b", "f"),
            "DoubleDiv:Double(Local(0), Local(1))"
        );
        assert_eq!(
            body("fun f(a: Int, b: Int) = a / b", "f"),
            "IntDivChecked:Int?(Local(0), Local(1))"
        );
        assert_eq!(
            body("fun f(a: Int, b: Int) = a % b", "f"),
            "IntRemChecked:Int?(Local(0), Local(1))"
        );
        assert_eq!(
            body("fun f(a: Decimal, b: Decimal) = a / b", "f"),
            "DecimalDivChecked:Decimal?(Local(0), Local(1))"
        );
        assert_eq!(
            body("fun f(a: Duration, b: Duration) = a - b", "f"),
            "DurationSub:Duration(Local(0), Local(1))"
        );
        assert_eq!(
            body("fun f(a: Float) = -a", "f"),
            "FloatNeg:Float(Local(0))"
        );
        assert_eq!(
            body("fun f(a: Int, b: Int) = !(a < b) || a == b", "f"),
            "If(Not:Boolean(Lt:Boolean(Local(0), Local(1))), Lit(true), Eq:Boolean(Local(0), Local(1)))"
        );
    }

    #[test]
    fn elvis_becomes_a_match() {
        assert_eq!(
            body("fun f(x: Int?) = x ?: 0", "f"),
            "Match:Int(Local(0), [null => Lit(0); Some(_) 1 value: Int => Local(1)])"
        );
        let program = compile("fun f(x: Int?) = x ?: 0");
        let ExprKind::Match { arms, .. } = &function(&program, "f").body.kind else {
            panic!()
        };
        assert_eq!(arms[1].binding_paths, vec![vec![("Some".to_string(), 0)]]);
    }

    #[test]
    fn safe_field_becomes_a_nested_match() {
        assert_eq!(
            body("data class P(val n: String?)\nfun f(p: P?) = p?.n", "f"),
            "Match:String?(Local(0), [null => None:String?; Some(_) 1 value: P => Field(Local(1), n)])"
        );
        assert_eq!(
            body("data class P(val n: String)\nfun f(p: P?) = p?.n", "f"),
            "Match:String?(Local(0), [null => None:String?; Some(_) 1 value: P => Some(Field(Local(1), n))])"
        );
    }

    #[test]
    fn blocks_become_lets_and_sequences() {
        assert_eq!(
            body("fun f(): Int {\n val a = 1\n val _ = a\n a + 1\n}", "f"),
            "Let(0 a = Lit(1); Seq(Local(0); IntAdd:Int(Local(0), Lit(1))))"
        );
        assert_eq!(body("fun f() {\n}", "f"), "Lit(())");
        assert_eq!(
            body("fun f(): Int {\n val a = 1\n return a\n}", "f"),
            "Let(0 a = Lit(1); Return(Local(0)))"
        );
    }

    #[test]
    fn shadowing_takes_the_most_recent_binding() {
        assert_eq!(
            body("fun f(x: Int): Int {\n val x = x + 1\n x\n}", "f"),
            "Let(1 x = IntAdd:Int(Local(0), Lit(1)); Local(1))"
        );
    }

    #[test]
    fn when_uses_the_elaborated_patterns() {
        let source = "sealed class C {\n data object A\n data class B(val n: Int)\n}\nfun f(c: C) = when (c) {\n A -> 0\n B(n) -> n\n}";
        assert_eq!(
            body(source, "f"),
            "Match:Int(Local(0), [A => Lit(0); B(_) 1 n: Int => Local(1)])"
        );
        let program = compile(source);
        let ExprKind::Match { arms, .. } = &function(&program, "f").body.kind else {
            panic!()
        };
        assert_eq!(arms[0].pattern, Pat::Ctor("A".into(), Vec::new()));
        assert_eq!(arms[1].pattern, Pat::Ctor("B".into(), vec![Pat::Wild]));
        assert_eq!(arms[1].binding_paths, vec![vec![("B".to_string(), 0)]]);
    }

    #[test]
    fn pattern_bindings_follow_optional_layers_and_generic_fields() {
        let source = "data class Box<T>(val v: T)\nfun f(b: Box<Int>?) = when (b) {\n null -> 0\n Box(v) -> v\n}";
        assert_eq!(
            body(source, "f"),
            "Match:Int(Local(0), [null => Lit(0); Some(Box(_)) 1 v: Int => Local(1)])"
        );
        let program = compile(source);
        let ExprKind::Match { arms, .. } = &function(&program, "f").body.kind else {
            panic!()
        };
        assert_eq!(
            arms[1].binding_paths,
            vec![vec![("Some".to_string(), 0), ("Box".to_string(), 0)]]
        );
        assert_eq!(
            body(
                "fun f(r: Result<Int, String>) = when (r) {\n Ok(v) -> v\n Err(e) -> 0\n}",
                "f"
            ),
            "Match:Int(Local(0), [Ok(_) 1 v: Int => Local(1); Err(_) 2 e: String => Lit(0)])"
        );
        // Smart cast: a variable after a `null` arm binds the non-null value.
        assert_eq!(
            body("fun f(x: Int?) = when (x) {\n null -> 0\n n -> n\n}", "f"),
            "Match:Int(Local(0), [null => Lit(0); Some(_) 1 n: Int => Local(1)])"
        );
    }

    #[test]
    fn generic_calls_record_their_instance() {
        let program = compile("fun <T> ident(x: T): T = x\nfun f() = ident(1)");
        assert_eq!(
            render(&function(&program, "f").body),
            "Call(Global(ident[Int]), [Lit(1)])"
        );
        let ident = function(&program, "ident");
        assert_eq!(ident.generics.len(), 1);
        let var = Type::Var(ident.generics[0]);
        assert_eq!(ident.params[0].2, var);
        assert_eq!(ident.result, var);
        // The body refers to the same variable as the signature.
        assert_eq!(ident.body.ty, var);
    }

    #[test]
    fn builtins_record_their_instance() {
        assert_eq!(
            body("fun f(xs: List<Int>) = map(xs, { x -> x * 2 })", "f"),
            "Call(Builtin(map[Int, Int]), [Local(0), Lambda(1 x: Int -> IntMul:Int(Local(1), Lit(2)))])"
        );
        assert_eq!(
            body(
                "fun f(xs: List<Int>) = fold(xs, \"\", { acc, x -> acc })",
                "f"
            ),
            "Call(Builtin(fold[Int, String]), [Local(0), Lit(\"\"), Lambda(1 acc: String, 2 x: Int -> Local(1))])"
        );
        assert_eq!(
            body("fun f() = listOf(1, 2)", "f"),
            "Call(Builtin(listOf[Int]), [Lit(1), Lit(2)])"
        );
    }

    #[test]
    fn constructors_build_values() {
        let source = "data class P(val n: Int, val s: String)\nsealed class C {\n data object A\n data class B(val n: Int)\n}\nfun f() = P(1, \"a\")\nfun g() = C.B(2)\nfun h() = A\nfun k() = B\nfun r(): Result<Int, String> = Ok(1)";
        let program = compile(source);
        assert_eq!(
            render(&function(&program, "f").body),
            "Ctor(P, Lit(1), Lit(\"a\"))"
        );
        assert_eq!(render(&function(&program, "g").body), "Ctor(B, Lit(2))");
        assert_eq!(render(&function(&program, "h").body), "Ctor(A)");
        assert_eq!(
            render(&function(&program, "k").body),
            "Lambda(0 n: Int -> Ctor(B, Local(0)))"
        );
        assert_eq!(render(&function(&program, "r").body), "Ctor(Ok, Lit(1))");
        let ExprKind::Ctor { type_name, .. } = &function(&program, "r").body.kind else {
            panic!()
        };
        assert_eq!(type_name, "Result");
        assert_eq!(body("fun f(): Int? = null", "f"), "None:Int?");
    }

    #[test]
    fn database_operations_and_endpoints() {
        let schema = schema(CLIENTES);
        let program = compile_with(
            &format!(
                "{ENTIDADE}server fun f(id: ClienteId): Cliente effects [db.read] = db.clientes.require(id)\nserver fun g(id: ClienteId): Cliente? effects [db.read] = db.clientes.find(id)\nserver fun h(n: String, a: String?): Cliente effects [db.write] = db.clientes.insert {{\n apelido = a\n nome = n\n}}"
            ),
            &schema,
        );
        let f = function(&program, "f");
        assert_eq!(render(&f.body), "Db(Require, clientes, [Local(0)])");
        assert!(f.endpoint);
        assert_eq!(f.domain, Domain::Server);
        assert_eq!(f.effects, vec!["db.read".to_string()]);
        assert_eq!(f.body.ty, Type::con("Cliente"));
        assert_eq!(
            render(&function(&program, "g").body),
            "Db(Find, clientes, [Local(0)])"
        );
        assert_eq!(
            render(&function(&program, "h").body),
            "Db(Insert { columns: [\"apelido\", \"nome\"] }, clientes, [Local(1), Local(0)])"
        );
    }

    #[test]
    fn names_resolve_to_globals_environment_and_server_calls() {
        let program = compile(
            "environment {\n    val porta: Port = env(\"PORT\")\n}\nfun g(x: Int) = x\nserver fun s(x: Int): Int = x\nfun f(): Int = g(1)\nfun p(): Port = porta\nclient fun c(): Int effects [server.call] = server.s(1)",
        );
        assert_eq!(
            render(&function(&program, "f").body),
            "Call(Global(g[]), [Lit(1)])"
        );
        assert_eq!(render(&function(&program, "p").body), "Env(porta)");
        assert_eq!(render(&function(&program, "c").body), "Server(s, [Lit(1)])");
        assert_eq!(function(&program, "c").domain, Domain::Client);
    }

    #[test]
    fn if_lambda_field_and_ui() {
        assert_eq!(
            body(
                "data class P(val n: Int)\nfun f(p: P, b: Boolean) = if (b) p.n else 0",
                "f"
            ),
            "If(Local(1), Field(Local(0), n), Lit(0))"
        );
        let program = compile("client component Home() {\n UI {\n Text(\"oi\")\n }\n}");
        assert_eq!(render(&function(&program, "Home").body), "Ui");
        assert!(!function(&program, "Home").endpoint);
    }

    #[test]
    fn big_hexadecimal_literals_become_decimal_digits() {
        assert_eq!(
            body("fun f() = 0x1_0000_0000_0000_0000_0000_0000_0000_0000", "f"),
            "Lit(340282366920938463463374607431768211456)"
        );
    }

    fn checked(source: &str) -> (Program, TypedProgram) {
        let (tokens, _) = crate::lexer::lex("test.bd", source);
        let (program, _) = crate::parser::parse(tokens);
        let (typed, diagnostics) = crate::types::check(&program, &DatabaseSchema::default());
        assert!(!has_errors(&diagnostics), "{diagnostics:?}");
        (program, typed)
    }

    fn let_value<'e>(expr: &'e Expr, name: &str) -> Vec<&'e Expr> {
        let mut found = Vec::new();
        if let ExprKind::Let {
            name: other,
            value,
            body,
            ..
        } = &expr.kind
        {
            if other == name {
                found.push(value.as_ref());
            }
            found.extend(let_value(body, name));
        }
        found
    }

    #[test]
    fn generalized_vals_are_specialized_to_their_single_use() {
        let source = "fun f(): Int {\n val e = emptyList()\n length(map(e, { x -> x + 1 }))\n}";
        let program = compile(source);
        let values = let_value(&function(&program, "f").body, "e");
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].ty, Type::list(Type::con("Int")));
        assert_eq!(render(values[0]), "Call(Builtin(emptyList[Int]), [])");
        assert_eq!(
            body("fun g(): Int? {\n val n = null\n n\n}", "g"),
            "Let(0 n = None:Int?; Local(0))"
        );
        // Unused: the generalized variable defaults to `Unit`.
        let program = compile("fun h(): Int {\n val e = emptyList()\n 1\n}");
        let values = let_value(&function(&program, "h").body, "e");
        assert_eq!(values[0].ty, Type::list(Type::con("Unit")));
    }

    #[test]
    fn effect_free_generalized_vals_are_copied_per_instance() {
        let source = "fun f(): Int {\n val id = { x -> x }\n val s = id(\"a\")\n id(1)\n}";
        assert_eq!(
            body(source, "f"),
            "Let(1 id = Lambda(0 x: String -> Local(0)); Let(4 id = Lambda(3 x: Int -> Local(3)); Let(2 s = Call(Local(1), [Lit(\"a\")]); Call(Local(4), [Lit(1)]))))"
        );
        let program = compile(source);
        let values = let_value(&function(&program, "f").body, "id");
        let string = Type::con("String");
        let int = Type::con("Int");
        assert!(
            matches!(&values[0].ty, Type::Fun(params, result, _) if params == &vec![string.clone()] && **result == string)
        );
        assert!(
            matches!(&values[1].ty, Type::Fun(params, result, _) if params == &vec![int.clone()] && **result == int)
        );
    }

    #[test]
    fn effectful_generalized_vals_are_never_copied() {
        let schema = schema(CLIENTES);
        let once = "server fun f(id: ClienteId): Int effects [db.read] {\n val xs = if (db.clientes.require(id).nome == \"a\") emptyList() else emptyList()\n return length(map(xs, { x -> x + 1 }))\n}";
        let program = compile_with(&format!("{ENTIDADE}{once}"), &schema);
        let values = let_value(&function(&program, "f").body, "xs");
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].ty, Type::list(Type::con("Int")));
        let twice = "server fun f(id: ClienteId): Int effects [db.read] {\n val xs = if (db.clientes.require(id).nome == \"a\") emptyList() else emptyList()\n val a = length(map(xs, { x -> x + 1 }))\n return length(map(xs, { x -> x == \"b\" }))\n}";
        let program = compile_with(&format!("{ENTIDADE}{twice}"), &schema);
        let values = let_value(&function(&program, "f").body, "xs");
        assert_eq!(values.len(), 1, "an effectful value must not be duplicated");
    }

    #[test]
    fn boolean_operators_keep_short_circuit() {
        let source = "fun even(n: Int): Boolean = n == 0 || odd(n - 1)\nfun odd(n: Int): Boolean = n != 0 && even(n - 1)";
        assert_eq!(
            body(source, "even"),
            "If(Eq:Boolean(Local(0), Lit(0)), Lit(true), Call(Global(odd[]), [IntSub:Int(Local(0), Lit(1))]))"
        );
        assert_eq!(
            body(source, "odd"),
            "If(NotEq:Boolean(Local(0), Lit(0)), Call(Global(even[]), [IntSub:Int(Local(0), Lit(1))]), Lit(false))"
        );
    }

    #[test]
    fn arguments_keep_their_written_order() {
        assert_eq!(
            body(
                "fun g(a: Int, b: Int) = a\nfun f(x: Int) = g(x + 1, x * 2)",
                "f"
            ),
            "Call(Global(g[]), [IntAdd:Int(Local(0), Lit(1)), IntMul:Int(Local(0), Lit(2))])"
        );
    }

    #[test]
    fn elvis_fallback_runs_only_in_the_null_arm() {
        assert_eq!(
            body("fun d(): Int = 0\nfun f(x: Int?) = x ?: d()", "f"),
            "Match:Int(Local(0), [null => Call(Global(d[]), []); Some(_) 1 value: Int => Local(1)])"
        );
    }

    #[test]
    fn nested_optionals_unwrap_one_layer_at_a_time() {
        assert_eq!(
            body(
                "fun f(x: Int??) = when (x) {\n null -> 0\n y -> y ?: 1\n}",
                "f"
            ),
            "Match:Int(Local(0), [null => Lit(0); Some(_) 1 y: Int? => Match:Int(Local(1), [null => Lit(1); Some(_) 2 value: Int => Local(2)])])"
        );
    }

    #[test]
    fn return_inside_a_when_arm() {
        assert_eq!(
            body(
                "fun f(x: Int?): Int {\n val y = when (x) {\n null -> return 0\n n -> n\n }\n y + 1\n}",
                "f"
            ),
            "Let(2 y = Match:Int(Local(0), [null => Return(Lit(0)); Some(_) 1 n: Int => Local(1)]); IntAdd:Int(Local(2), Lit(1)))"
        );
    }

    #[test]
    fn patterns_nested_under_some_inside_a_constructor() {
        let source = "sealed class S {\n data class K(val a: Int?, val n: Int)\n}\nfun f(s: S?) = when (s) {\n null -> 0\n K(null, n) -> n\n K(a, n) -> n\n}";
        assert_eq!(
            body(source, "f"),
            "Match:Int(Local(0), [null => Lit(0); Some(K(null, _)) 1 n: Int => Local(1); Some(K(_, _)) 2 a: Int? 3 n: Int => Local(3)])"
        );
        let program = compile(source);
        let ExprKind::Match { arms, .. } = &function(&program, "f").body.kind else {
            panic!()
        };
        let path = |steps: &[(&str, usize)]| -> Vec<(String, usize)> {
            steps
                .iter()
                .map(|(name, index)| (name.to_string(), *index))
                .collect()
        };
        assert_eq!(arms[1].binding_paths, vec![path(&[("Some", 0), ("K", 1)])]);
        assert_eq!(
            arms[2].binding_paths,
            vec![
                path(&[("Some", 0), ("K", 0)]),
                path(&[("Some", 0), ("K", 1)])
            ]
        );
    }

    #[test]
    fn missing_checker_records_leave_error_types() {
        let source = "fun f(x: Int?): Int = when (x) {\n null -> 0\n n -> n + 1\n}";
        let schema = DatabaseSchema::default();
        let (program, mut typed) = checked(source);
        typed.patterns.clear();
        let core = lower(&program, &typed, &schema);
        assert_eq!(function(&core, "f").body.ty, Type::Error);

        let (program, mut typed) = checked(source);
        typed.signatures.clear();
        let core = lower(&program, &typed, &schema);
        let f = function(&core, "f");
        assert_eq!(f.result, Type::Error);
        assert_eq!(f.params[0].2, Type::Error);

        let (program, mut typed) = checked("fun f(a: Int) = a + 1");
        typed.node_types.retain(|_, ty| *ty != Type::con("Int"));
        let core = lower(&program, &typed, &schema);
        let f = function(&core, "f");
        assert_eq!(f.body.ty, Type::Error);
        assert_eq!(f.body.kind, ExprKind::Lit(Literal::Unit));

        let (program, mut typed) = checked("fun g() = 1\nfun f() = g()");
        typed.resolutions.clear();
        let core = lower(&program, &typed, &schema);
        let ExprKind::Call { callee, .. } = &function(&core, "f").body.kind else {
            panic!()
        };
        assert_eq!(callee.ty, Type::Error);
    }

    #[test]
    fn every_builtin_has_a_shared_scheme() {
        for name in crate::types::env::BUILTINS {
            let scheme = builtin_scheme(name).unwrap();
            let mut vars = Vec::new();
            type_vars(&scheme.ty, &mut vars);
            assert_eq!(vars, (0..scheme.vars).collect::<Vec<_>>(), "{name}");
        }
        assert!(builtin_scheme("listOf").is_none());
    }

    #[test]
    fn declared_types_become_core_types() {
        let program = compile(
            "data class P<T>(val v: T, val n: Int?)\nentity E(val id: Int)\nsealed class C {\n data object A\n data class B(val n: Int)\n}\nenum class Cor { Azul, Verde }\ndistinct type Id = UUID\nfun f() = 1",
        );
        let t = |name: &str| Type::con(name);
        assert_eq!(
            program.types,
            vec![
                CoreType::Sum {
                    name: "Result".into(),
                    params: vec!["T".into(), "E".into()],
                    variants: vec![
                        ("Ok".into(), vec![("value".into(), t("'T"))]),
                        ("Err".into(), vec![("error".into(), t("'E"))]),
                    ],
                },
                CoreType::Record {
                    name: "P".into(),
                    params: vec!["T".into()],
                    fields: vec![("v".into(), t("'T")), ("n".into(), Type::option(t("Int"))),],
                },
                CoreType::Record {
                    name: "E".into(),
                    params: Vec::new(),
                    fields: vec![("id".into(), t("Int"))],
                },
                CoreType::Sum {
                    name: "C".into(),
                    params: Vec::new(),
                    variants: vec![
                        ("A".into(), Vec::new()),
                        ("B".into(), vec![("n".into(), t("Int"))]),
                    ],
                },
                CoreType::Sum {
                    name: "Cor".into(),
                    params: Vec::new(),
                    variants: vec![("Azul".into(), Vec::new()), ("Verde".into(), Vec::new())],
                },
                CoreType::Distinct {
                    name: "Id".into(),
                    underlying: t("UUID"),
                },
            ]
        );
    }
}
