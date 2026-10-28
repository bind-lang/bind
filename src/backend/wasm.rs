//! Bind's WebAssembly backend.
//!
//! This module writes WebAssembly Binary Format directly from the validated `CoreProgram`.
//! It intentionally has no LLVM, Rust-target, WAT, `wasm-bindgen`, or encoder dependency.
//! The accepted subset is closed: verification calls [`problems`] before generation, and every
//! construct outside this module's exact semantic mapping is a compile-time error.

use std::collections::{BTreeMap, BTreeSet};

use crate::core::ir::{CoreFunction, CoreProgram, Expr, ExprKind, Literal, LocalId, Prim};
use crate::types::ty::{Type, display};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValType {
    I32,
    F32,
    F64,
}

impl ValType {
    fn byte(self) -> u8 {
        match self {
            Self::I32 => 0x7f,
            Self::F32 => 0x7d,
            Self::F64 => 0x7c,
        }
    }
}

/// A user-facing reason why an endpoint cannot be represented without changing Bind semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Problem {
    pub function: String,
    pub endpoint: String,
    pub reason: String,
}

/// Returns every unsupported construct reachable from an endpoint, deterministically.
pub fn problems(program: &CoreProgram) -> Vec<Problem> {
    let mut endpoints: Vec<&CoreFunction> = program
        .functions
        .iter()
        .filter(|function| function.endpoint)
        .collect();
    endpoints.sort_by(|a, b| a.name.cmp(&b.name));

    let mut origin = BTreeMap::new();
    for endpoint in &endpoints {
        for function in crate::backend::rust::reachable(program, &[*endpoint]) {
            origin
                .entry(function.name.as_str())
                .or_insert(endpoint.name.as_str());
        }
    }
    let functions = crate::backend::rust::reachable(program, &endpoints);
    let names: BTreeSet<&str> = functions
        .iter()
        .map(|function| function.name.as_str())
        .collect();
    let recursive = recursive_functions(&functions, &names);
    let mut output = Vec::new();
    for function in functions {
        let endpoint = origin
            .get(function.name.as_str())
            .copied()
            .unwrap_or(function.name.as_str());
        let reason = function_problem(function, &names).or_else(|| {
            recursive.contains(function.name.as_str()).then(|| {
                "is recursive; this first Wasm ABI rejects recursion until Bind can enforce its recursion limit before the host stack is exhausted".to_string()
            })
        });
        if let Some(reason) = reason {
            output.push(Problem {
                function: function.name.clone(),
                endpoint: endpoint.to_string(),
                reason,
            });
        }
    }
    output
}

fn function_problem(function: &CoreFunction, names: &BTreeSet<&str>) -> Option<String> {
    if !function.generics.is_empty() {
        return Some(
            "is polymorphic; monomorphization is not implemented by the first Wasm ABI".into(),
        );
    }
    if !function.effects.is_empty() {
        return Some(format!(
            "has effects `{}`; the first Wasm ABI accepts pure computation only",
            function.effects.join(", ")
        ));
    }
    for (_, name, ty) in &function.params {
        if wasm_type(ty).is_none() && !is_unit(ty) {
            return Some(format!(
                "has parameter `{name}` of type `{}`, which the first Wasm ABI cannot represent exactly",
                display(ty)
            ));
        }
    }
    if wasm_type(&function.result).is_none() && !is_unit(&function.result) {
        return Some(format!(
            "returns `{}`, which the first Wasm ABI cannot represent exactly",
            display(&function.result)
        ));
    }
    expression_problem(&function.body, names)
}

fn expression_problem(expr: &Expr, names: &BTreeSet<&str>) -> Option<String> {
    if !matches!(expr.kind, ExprKind::Return(_))
        && wasm_type(&expr.ty).is_none()
        && !is_unit(&expr.ty)
    {
        return Some(format!(
            "contains an expression of type `{}`, which the first Wasm ABI cannot represent exactly",
            display(&expr.ty)
        ));
    }
    match &expr.kind {
        ExprKind::Lit(literal) => match literal {
            Literal::Bool(_) | Literal::Unit => None,
            Literal::Int(text) | Literal::Double(text) if scalar(&expr.ty) == Some("Double") => {
                text.parse::<f64>()
                    .ok()
                    .filter(|value| value.is_finite())
                    .is_none()
                    .then(|| "contains a Double literal that is not finite as a WebAssembly f64".into())
            }
            Literal::Int(text) | Literal::Float(text) if scalar(&expr.ty) == Some("Float") => text
                .parse::<f32>()
                .ok()
                .filter(|value| value.is_finite())
                .is_none()
                .then(|| "contains a Float literal that is not finite as a WebAssembly f32 (it rounds to infinity)".into()),
            _ => Some("contains a literal whose Bind representation is not implemented by the first Wasm ABI".into()),
        },
        ExprKind::Local(_) => None,
        ExprKind::Call { callee, args } => {
            let ExprKind::Global { name, instance } = &callee.kind else {
                return Some("calls a function value; the first Wasm ABI supports direct calls only".into());
            };
            if !instance.is_empty() {
                return Some("calls a polymorphic function; monomorphization is not implemented by the first Wasm ABI".into());
            }
            if !names.contains(name.as_str()) {
                return Some(format!("calls `{name}`, which is not in the verified Wasm module"));
            }
            args.iter().find_map(|arg| expression_problem(arg, names))
        }
        ExprKind::Let { value, body, .. } | ExprKind::Seq { value, body } => {
            expression_problem(value, names).or_else(|| expression_problem(body, names))
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => expression_problem(condition, names)
            .or_else(|| expression_problem(then_branch, names))
            .or_else(|| expression_problem(else_branch, names)),
        ExprKind::Prim { op, args } => {
            if !supported_prim(*op, args) {
                return Some(format!(
                    "uses primitive `{op:?}`, which the first Wasm ABI cannot preserve"
                ));
            }
            args.iter().find_map(|arg| expression_problem(arg, names))
        }
        ExprKind::Return(value) => expression_problem(value, names),
        ExprKind::Global { .. } => {
            Some("uses a function as a value; the first Wasm ABI supports direct calls only".into())
        }
        ExprKind::Builtin { .. } => Some("uses a builtin as a function value".into()),
        ExprKind::Environment(_) => Some("reads an environment value".into()),
        ExprKind::Ctor { .. } => Some("constructs an aggregate value".into()),
        ExprKind::None | ExprKind::Some(_) => Some("constructs an optional value".into()),
        ExprKind::ServerCall { .. } => Some("contains a nested server call".into()),
        ExprKind::Lambda { .. } => Some("creates a closure".into()),
        ExprKind::Match { .. } => Some("uses pattern matching".into()),
        ExprKind::Field { .. } => Some("reads an aggregate field".into()),
        ExprKind::Db { .. } => Some("accesses a database".into()),
        ExprKind::Ui { .. } => Some("constructs a UI tree".into()),
    }
}

fn supported_prim(op: Prim, args: &[Expr]) -> bool {
    use Prim::*;
    let ty = args.first().and_then(|arg| scalar(&arg.ty));
    match op {
        DoubleAdd | DoubleSub | DoubleMul | DoubleDiv | DoubleNeg => ty == Some("Double"),
        FloatAdd | FloatSub | FloatMul | FloatDiv | FloatNeg => ty == Some("Float"),
        Not => ty == Some("Boolean"),
        Eq | NotEq => matches!(ty, Some("Boolean" | "Double" | "Float" | "Unit")),
        Lt | LtEq | Gt | GtEq => matches!(ty, Some("Double" | "Float" | "Unit")),
        _ => false,
    }
}

fn recursive_functions(functions: &[&CoreFunction], names: &BTreeSet<&str>) -> BTreeSet<String> {
    let graph: BTreeMap<&str, Vec<&str>> = functions
        .iter()
        .map(|function| {
            let mut calls = Vec::new();
            direct_calls(&function.body, &mut calls);
            calls.retain(|name| names.contains(name));
            calls.sort_unstable();
            calls.dedup();
            (function.name.as_str(), calls)
        })
        .collect();
    let mut recursive = BTreeSet::new();
    for start in graph.keys().copied() {
        let mut pending = graph.get(start).cloned().unwrap_or_default();
        let mut seen = BTreeSet::new();
        while let Some(name) = pending.pop() {
            if name == start {
                recursive.insert(start.to_string());
                break;
            }
            if seen.insert(name) {
                pending.extend(graph.get(name).into_iter().flatten().copied());
            }
        }
    }
    recursive
}

fn direct_calls<'a>(expr: &'a Expr, output: &mut Vec<&'a str>) {
    match &expr.kind {
        ExprKind::Call { callee, args } => {
            if let ExprKind::Global { name, .. } = &callee.kind {
                output.push(name);
            }
            for arg in args {
                direct_calls(arg, output);
            }
        }
        ExprKind::Let { value, body, .. } | ExprKind::Seq { value, body } => {
            direct_calls(value, output);
            direct_calls(body, output);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            direct_calls(condition, output);
            direct_calls(then_branch, output);
            direct_calls(else_branch, output);
        }
        ExprKind::Prim { args, .. } => args.iter().for_each(|arg| direct_calls(arg, output)),
        ExprKind::Return(value) => direct_calls(value, output),
        _ => {}
    }
}

/// Compiles the endpoint closure to a standalone WebAssembly binary.
pub fn compile(program: &CoreProgram) -> Result<Vec<u8>, String> {
    let found = problems(program);
    if !found.is_empty() {
        return Err(format!(
            "Wasm generation received an unverified program: {}",
            found
                .iter()
                .map(|problem| format!("{} {}", problem.function, problem.reason))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let mut endpoints: Vec<&CoreFunction> =
        program.functions.iter().filter(|f| f.endpoint).collect();
    endpoints.sort_by(|a, b| a.name.cmp(&b.name));
    let functions = crate::backend::rust::reachable(program, &endpoints);
    let indices: BTreeMap<&str, u32> = functions
        .iter()
        .enumerate()
        .map(|(index, function)| (function.name.as_str(), index as u32))
        .collect();

    let mut module = b"\0asm\x01\0\0\0".to_vec();

    let mut types = Vec::new();
    u32_leb(functions.len() as u32, &mut types);
    for function in &functions {
        types.push(0x60);
        let params: Vec<ValType> = function
            .params
            .iter()
            .filter_map(|(_, _, ty)| wasm_type(ty))
            .collect();
        u32_leb(params.len() as u32, &mut types);
        types.extend(params.into_iter().map(ValType::byte));
        if let Some(result) = wasm_type(&function.result) {
            types.push(1);
            types.push(result.byte());
        } else {
            types.push(0);
        }
    }
    section(1, types, &mut module);

    let mut function_section = Vec::new();
    u32_leb(functions.len() as u32, &mut function_section);
    for index in 0..functions.len() {
        u32_leb(index as u32, &mut function_section);
    }
    section(3, function_section, &mut module);

    let mut exports = Vec::new();
    u32_leb(endpoints.len() as u32, &mut exports);
    for endpoint in endpoints {
        name(&endpoint.name, &mut exports);
        exports.push(0x00);
        u32_leb(indices[endpoint.name.as_str()], &mut exports);
    }
    section(7, exports, &mut module);

    let mut codes = Vec::new();
    u32_leb(functions.len() as u32, &mut codes);
    for function in functions {
        let body = compile_function(function, &indices)?;
        u32_leb(body.len() as u32, &mut codes);
        codes.extend(body);
    }
    section(10, codes, &mut module);
    Ok(module)
}

fn compile_function(
    function: &CoreFunction,
    indices: &BTreeMap<&str, u32>,
) -> Result<Vec<u8>, String> {
    let mut locals: BTreeMap<LocalId, Option<u32>> = BTreeMap::new();
    let mut next = 0u32;
    for (local, _, ty) in &function.params {
        locals.insert(
            *local,
            wasm_type(ty).map(|_| {
                let index = next;
                next += 1;
                index
            }),
        );
    }
    let mut declarations = Vec::new();
    collect_locals(&function.body, &mut declarations);
    declarations.sort_by_key(|(local, _)| *local);
    declarations.dedup_by_key(|(local, _)| *local);
    let mut wasm_locals = Vec::new();
    for (local, ty) in declarations {
        let index = wasm_type(ty).map(|kind| {
            let index = next;
            next += 1;
            wasm_locals.push(kind);
            index
        });
        locals.insert(local, index);
    }

    let mut body = Vec::new();
    u32_leb(wasm_locals.len() as u32, &mut body);
    for kind in wasm_locals {
        body.push(1);
        body.push(kind.byte());
    }
    emit_expr(&function.body, &locals, indices, &mut body)?;
    body.push(0x0b);
    Ok(body)
}

fn collect_locals<'a>(expr: &'a Expr, output: &mut Vec<(LocalId, &'a Type)>) {
    match &expr.kind {
        ExprKind::Let {
            local, value, body, ..
        } => {
            output.push((*local, &value.ty));
            collect_locals(value, output);
            collect_locals(body, output);
        }
        ExprKind::Seq { value, body } => {
            collect_locals(value, output);
            collect_locals(body, output);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_locals(condition, output);
            collect_locals(then_branch, output);
            collect_locals(else_branch, output);
        }
        ExprKind::Call { args, .. } | ExprKind::Prim { args, .. } => {
            args.iter().for_each(|arg| collect_locals(arg, output));
        }
        ExprKind::Return(value) => collect_locals(value, output),
        _ => {}
    }
}

fn emit_expr(
    expr: &Expr,
    locals: &BTreeMap<LocalId, Option<u32>>,
    indices: &BTreeMap<&str, u32>,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    match &expr.kind {
        ExprKind::Lit(Literal::Bool(value)) => {
            out.push(0x41);
            i32_leb(i32::from(*value), out);
        }
        ExprKind::Lit(Literal::Unit) => {}
        ExprKind::Lit(Literal::Int(text) | Literal::Double(text))
            if scalar(&expr.ty) == Some("Double") =>
        {
            out.push(0x44);
            out.extend(
                text.parse::<f64>()
                    .map_err(|error| error.to_string())?
                    .to_le_bytes(),
            );
        }
        ExprKind::Lit(Literal::Int(text) | Literal::Float(text))
            if scalar(&expr.ty) == Some("Float") =>
        {
            out.push(0x43);
            out.extend(
                text.parse::<f32>()
                    .map_err(|error| error.to_string())?
                    .to_le_bytes(),
            );
        }
        ExprKind::Local(local) => {
            if let Some(index) = local_slot(locals, *local)? {
                out.push(0x20);
                u32_leb(index, out);
            }
        }
        ExprKind::Call { callee, args } => {
            for arg in args {
                emit_expr(arg, locals, indices, out)?;
            }
            let ExprKind::Global { name, .. } = &callee.kind else {
                return Err("non-direct call reached Wasm generation".into());
            };
            out.push(0x10);
            u32_leb(
                *indices
                    .get(name.as_str())
                    .ok_or_else(|| format!("missing function `{name}`"))?,
                out,
            );
        }
        ExprKind::Let {
            local, value, body, ..
        } => {
            emit_expr(value, locals, indices, out)?;
            if let Some(index) = local_slot(locals, *local)? {
                out.push(0x21);
                u32_leb(index, out);
            }
            emit_expr(body, locals, indices, out)?;
        }
        ExprKind::Seq { value, body } => {
            emit_expr(value, locals, indices, out)?;
            if wasm_type(&value.ty).is_some() {
                out.push(0x1a);
            }
            emit_expr(body, locals, indices, out)?;
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            emit_expr(condition, locals, indices, out)?;
            out.push(0x04);
            out.push(wasm_type(&expr.ty).map_or(0x40, ValType::byte));
            emit_expr(then_branch, locals, indices, out)?;
            out.push(0x05);
            emit_expr(else_branch, locals, indices, out)?;
            out.push(0x0b);
        }
        ExprKind::Prim { op, args } => emit_prim(*op, args, locals, indices, out)?,
        ExprKind::Return(value) => {
            emit_expr(value, locals, indices, out)?;
            out.push(0x0f);
        }
        _ => return Err("unsupported expression reached Wasm generation".into()),
    }
    Ok(())
}

/// The Wasm slot of `local`: `Ok(None)` for a declared zero-width (`Unit`) local, an internal
/// error for a local the function never declared (emitting nothing would produce invalid Wasm).
fn local_slot(
    locals: &BTreeMap<LocalId, Option<u32>>,
    local: LocalId,
) -> Result<Option<u32>, String> {
    locals
        .get(&local)
        .copied()
        .ok_or_else(|| format!("local {local} reached Wasm generation without a declaration"))
}

fn emit_prim(
    op: Prim,
    args: &[Expr],
    locals: &BTreeMap<LocalId, Option<u32>>,
    indices: &BTreeMap<&str, u32>,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    use Prim::*;
    let operand = args.first().and_then(|arg| scalar(&arg.ty));
    for arg in args {
        emit_expr(arg, locals, indices, out)?;
    }
    let opcode = match op {
        DoubleAdd => 0xa0,
        DoubleSub => 0xa1,
        DoubleMul => 0xa2,
        DoubleDiv => 0xa3,
        DoubleNeg => 0x9a,
        FloatAdd => 0x92,
        FloatSub => 0x93,
        FloatMul => 0x94,
        FloatDiv => 0x95,
        FloatNeg => 0x8c,
        Not => 0x45,
        Eq if operand == Some("Boolean") => 0x46,
        NotEq if operand == Some("Boolean") => 0x47,
        Eq if operand == Some("Double") => 0x61,
        NotEq if operand == Some("Double") => 0x62,
        Lt if operand == Some("Double") => 0x63,
        Gt if operand == Some("Double") => 0x64,
        LtEq if operand == Some("Double") => 0x65,
        GtEq if operand == Some("Double") => 0x66,
        Eq if operand == Some("Float") => 0x5b,
        NotEq if operand == Some("Float") => 0x5c,
        Lt if operand == Some("Float") => 0x5d,
        Gt if operand == Some("Float") => 0x5e,
        LtEq if operand == Some("Float") => 0x5f,
        GtEq if operand == Some("Float") => 0x60,
        Eq | LtEq | GtEq if operand == Some("Unit") => {
            out.push(0x41);
            i32_leb(1, out);
            return Ok(());
        }
        NotEq | Lt | Gt if operand == Some("Unit") => {
            out.push(0x41);
            i32_leb(0, out);
            return Ok(());
        }
        _ => {
            return Err(format!(
                "unsupported primitive `{op:?}` reached Wasm generation"
            ));
        }
    };
    out.push(opcode);
    Ok(())
}

fn scalar(ty: &Type) -> Option<&str> {
    match ty {
        Type::Con(name, args) if args.is_empty() => Some(name),
        _ => None,
    }
}

fn is_unit(ty: &Type) -> bool {
    scalar(ty) == Some("Unit")
}

fn wasm_type(ty: &Type) -> Option<ValType> {
    match scalar(ty) {
        Some("Boolean") => Some(ValType::I32),
        Some("Float") => Some(ValType::F32),
        Some("Double") => Some(ValType::F64),
        _ => None,
    }
}

fn section(id: u8, contents: Vec<u8>, module: &mut Vec<u8>) {
    module.push(id);
    u32_leb(contents.len() as u32, module);
    module.extend(contents);
}

fn name(value: &str, output: &mut Vec<u8>) {
    u32_leb(value.len() as u32, output);
    output.extend(value.as_bytes());
}

fn u32_leb(mut value: u32, output: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn i32_leb(mut value: i32, output: &mut Vec<u8>) {
    loop {
        let byte = (value as u8) & 0x7f;
        value >>= 7;
        let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
        output.push(if done { byte } else { byte | 0x80 });
        if done {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_valid_executable_binary_without_a_toolchain() {
        let program = crate::core::lower::tests::compile(
            "fun choose(flag: Boolean, a: Double, b: Double): Double = if (flag) a else b\nserver fun calculate(value: Double): Double effects [] = choose(value > 10.0, value * 2.0, -value)\n",
        );
        let binary = compile(&program).unwrap();
        assert_eq!(&binary[..8], b"\0asm\x01\0\0\0");
        let root = std::env::temp_dir().join(format!("bind-wasm-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("app.wasm");
        std::fs::write(&path, binary).unwrap();
        let output = std::process::Command::new("node")
            .args([
                "-e",
                "const fs=require('fs');const m=new WebAssembly.Module(fs.readFileSync(process.argv[1]));const x=new WebAssembly.Instance(m).exports;process.stdout.write(String(x.calculate(11)));",
                path.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "22");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_arbitrary_precision_int_before_generation() {
        let program = crate::core::lower::tests::compile(
            "server fun double(value: Int): Int effects [] = value * 2\n",
        );
        let found = problems(&program);
        assert_eq!(found.len(), 1);
        assert!(found[0].reason.contains("parameter `value` of type `Int`"));
        assert!(compile(&program).is_err());
    }

    #[test]
    fn an_undeclared_local_is_an_internal_error_not_invalid_wasm() {
        let locals = BTreeMap::from([(0, Some(0)), (1, None)]);
        assert_eq!(local_slot(&locals, 0), Ok(Some(0)));
        // A declared Unit local has no slot and emits nothing.
        assert_eq!(local_slot(&locals, 1), Ok(None));
        assert!(
            local_slot(&locals, 2)
                .unwrap_err()
                .contains("without a declaration")
        );
    }

    #[test]
    fn rejects_recursion_before_the_host_can_overflow() {
        let program = crate::core::lower::tests::compile(
            "server fun loop(value: Double): Double effects [] = loop(value)\n",
        );
        assert!(problems(&program)[0].reason.contains("recursive"));
    }
}
