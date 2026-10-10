//! Static type system: Hindley-Milner inference, algebraic data types and exhaustive matching.

pub mod env;
pub mod exhaustive;
pub mod infer;
pub mod ty;
pub mod unify;

use crate::ast::{BodyKind, Domain, Program};
use crate::diagnostic::Diagnostic;
use crate::schema::DatabaseSchema;

pub use infer::TypedProgram;

/// Type-checks a whole program. Diagnostics are sorted by file, line, column and code.
pub fn check(program: &Program, schema: &DatabaseSchema) -> (TypedProgram, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    check_boundaries(program, &mut diagnostics);
    let env = env::Env::build(program, &mut diagnostics);
    let (typed, mut inferred) = infer::check_program(program, &env, schema);
    diagnostics.append(&mut inferred);
    diagnostics.sort_by(|a, b| {
        (&a.span.file, a.span.line, a.span.column, a.code).cmp(&(&b.span.file, b.span.line, b.span.column, b.code))
    });
    (typed, diagnostics)
}

/// Boundary signatures are contracts (RPC, database, UI), so they are never inferred.
fn check_boundaries(program: &Program, diagnostics: &mut Vec<Diagnostic>) {
    for function in &program.functions {
        let what = if function.component {
            "Component"
        } else if function.domain == Domain::Server {
            "Server function"
        } else if function.body_kind == BodyKind::Absent {
            "Function without a body"
        } else {
            continue;
        };
        for parameter in function.parameters.iter().filter(|parameter| parameter.type_ref.is_none()) {
            diagnostics.push(
                Diagnostic::error(
                    "E7003",
                    format!("{what} `{}` must declare the type of parameter `{}`.", function.name, parameter.name),
                    function.span.clone(),
                )
                .with_hint("Boundary signatures are contracts; write `name: Type`."),
            );
        }
        if !function.component && function.return_type.is_none() {
            diagnostics.push(
                Diagnostic::error(
                    "E7003",
                    format!("{what} `{}` must declare its return type.", function.name),
                    function.span.clone(),
                )
                .with_hint("Write `: Type` after the parameter list."),
            );
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use crate::diagnostic::{Severity, has_errors};
    use crate::lexer::lex;
    use crate::parser::parse;
    use crate::schema::DatabaseSchema;

    /// Inferred schemes in declaration order, or the sorted, de-duplicated error codes
    /// (parse errors stop before type checking, as in `compiler::verify`).
    pub fn infer(source: &str) -> Result<Vec<(String, String)>, Vec<String>> {
        infer_with_schema(source, &DatabaseSchema::default())
    }

    pub fn infer_with_schema(source: &str, schema: &DatabaseSchema) -> Result<Vec<(String, String)>, Vec<String>> {
        let (tokens, mut diagnostics) = lex("test.bd", source);
        let (program, mut parse_diagnostics) = parse(tokens);
        diagnostics.append(&mut parse_diagnostics);
        if !has_errors(&diagnostics) {
            let (typed, mut type_diagnostics) = super::check(&program, schema);
            diagnostics.append(&mut type_diagnostics);
            if !has_errors(&diagnostics) {
                return Ok(program
                    .functions
                    .iter()
                    .map(|function| (function.name.clone(), typed.schemes[&function.name].clone()))
                    .collect());
            }
        }
        let mut codes: Vec<String> = diagnostics
            .iter()
            .filter(|item| item.severity == Severity::Error)
            .map(|item| item.code.to_string())
            .collect();
        codes.sort();
        codes.dedup();
        Err(codes)
    }

    pub fn scheme(source: &str, name: &str) -> String {
        let schemes = infer(source).unwrap_or_else(|codes| panic!("expected `{name}` to type-check, got {codes:?}"));
        schemes.into_iter().find(|(other, _)| other == name).map(|(_, scheme)| scheme).unwrap()
    }

    pub fn errors(source: &str) -> Vec<String> {
        match infer(source) {
            Ok(schemes) => panic!("expected type errors, got {schemes:?}"),
            Err(codes) => codes,
        }
    }
}
