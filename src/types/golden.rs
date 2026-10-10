//! Golden tests: `tests/typecheck/<case>.bd` against `<case>.expected`.
//! The first line of each case declares the expected error codes (`// expect: ok` or codes),
//! which is checked independently of the blessed output.

use std::fs;
use std::path::{Path, PathBuf};

use crate::diagnostic::{Severity, has_errors};
use crate::lexer::lex;
use crate::parser::parse;
use crate::schema::DatabaseSchema;

fn run_case(path: &Path) -> (String, Vec<String>) {
    let source = fs::read_to_string(path).unwrap();
    let file = path.file_name().unwrap().to_string_lossy().to_string();
    let (tokens, mut diagnostics) = lex(&file, &source);
    let (program, mut parse_diagnostics) = parse(tokens);
    diagnostics.append(&mut parse_diagnostics);
    let mut schemes = None;
    if !has_errors(&diagnostics) {
        let schema_path = path.with_extension("csv");
        let schema = if schema_path.exists() {
            DatabaseSchema::load(&schema_path).unwrap()
        } else {
            DatabaseSchema::default()
        };
        let (typed, mut type_diagnostics) = super::check(&program, &schema);
        diagnostics.append(&mut type_diagnostics);
        schemes = Some(typed.schemes);
    }
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|item| item.severity == Severity::Error)
        .collect();
    let mut lines = Vec::new();
    let mut codes: Vec<String> = errors.iter().map(|item| item.code.to_string()).collect();
    if errors.is_empty()
        && let Some(schemes) = schemes
    {
        for function in &program.functions {
            lines.push(format!("{} : {}", function.name, schemes[&function.name]));
        }
    }
    for item in errors {
        lines.push(format!(
            "{} {}:{} {}",
            item.code, item.span.line, item.span.column, item.message
        ));
    }
    codes.sort();
    codes.dedup();
    (lines.join("\n") + "\n", codes)
}

fn declared_codes(source: &str) -> Vec<String> {
    let first = source.lines().next().unwrap_or_default();
    let list = first
        .strip_prefix("// expect:")
        .expect("a golden case must start with `// expect:`")
        .trim();
    let mut codes: Vec<String> = if list == "ok" {
        Vec::new()
    } else {
        list.split_whitespace().map(str::to_string).collect()
    };
    codes.sort();
    codes.dedup();
    codes
}

#[test]
fn golden_cases() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/typecheck");
    let bless = std::env::var_os("BIND_BLESS").is_some();
    let mut cases: Vec<PathBuf> = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "bd"))
        .collect();
    cases.sort();
    assert!(
        !cases.is_empty(),
        "no golden cases in {}",
        directory.display()
    );
    let mut failures = Vec::new();
    for case in cases {
        let expected_codes = declared_codes(&fs::read_to_string(&case).unwrap());
        let (output, codes) = run_case(&case);
        if codes != expected_codes {
            failures.push(format!(
                "{}: expected {expected_codes:?}, got {codes:?}\n{output}",
                case.display()
            ));
            continue;
        }
        let expected_path = case.with_extension("expected");
        if bless {
            fs::write(&expected_path, &output).unwrap();
            continue;
        }
        let expected = fs::read_to_string(&expected_path).unwrap_or_default();
        if expected != output {
            failures.push(format!(
                "{}: output differs\n--- expected\n{expected}--- actual\n{output}",
                case.display()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
