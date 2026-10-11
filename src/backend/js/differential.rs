//! Three-way differential test: the reference evaluator (`core::eval`) against the generated
//! Rust and the generated JavaScript.
//!
//! The generator of the server's differential test (`backend::rust::differential`), in its
//! web mode, writes 1,000 pure, parameterless Bind functions whose results also include
//! `Double`, `Float`, the sum type `Forma` and `Ui` (nodes with `String`/`Int` arguments,
//! `Text` of every displayable type and of a distinct type over `Int`, `if`/`when`/`fold`
//! producing trees, `val` among children, SSR-style prelude components), plus fixed programs:
//! the recursion limit (20,000 levels too), an SSR-style client component and non-finite
//! numbers. The source goes through the real front end. Then:
//!
//! - `core::eval` renders each result canonically (`Value::render`: HTML for `Ui`);
//! - the Rust crate `target/bind-differential-web/` (`render_functions`, `clippy -D warnings`)
//!   prints `Value::render`'s form, with the runtime's `render_html` for `Ui` and
//!   `js_number_to_string` for `Double`/`Float`;
//! - the generated `app.js` (`render_app`) runs in node next to the JS runtime: a harness
//!   (`tests/differential.mjs`) prints the same canonical form from the JS values, rendering
//!   `Ui` with the runtime's `render` into the minimal DOM of `tests/dom.mjs` and serializing it.
//!
//! Every line is `p<i>=<render>`, the render made line-safe (`\` → `\\`, LF → `\n`,
//! CR → `\r`: text nodes may hold raw line breaks). The three outputs must be identical byte
//! for byte; a divergence shows the program and the three lines.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use crate::backend::rust::differential::{
    PROGRAMS, WEB_CONSTRUCTS, WEB_PRELUDE, build_and_run, compile, expected, main_rs_with,
    source_with, web_programs,
};

/// The line-safe form of a render: `\` → `\\`, LF → `\n`, CR → `\r` (injective).
fn line_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// `p<i>=<render>` → `p<i>=<line_safe(render)>`.
fn line_safe_entry(line: &str) -> String {
    match line.split_once('=') {
        Some((name, render)) => format!("{name}={}", line_safe(render)),
        None => line_safe(line),
    }
}

/// The renderer of the web results in the Rust crate's `main`: `Double`/`Float` with the
/// runtime's `js_number_to_string`, `Ui` with its `render_html`, `Forma` as `Value::render`
/// shows constructors.
const RUST_EXTRA: &str = r#"
impl Render for f64 {
    fn render(&self) -> String {
        bind_differential_web::bind_runtime::js_number_to_string(*self)
    }
}

impl Render for f32 {
    fn render(&self) -> String {
        bind_differential_web::bind_runtime::js_number_to_string(f64::from(*self))
    }
}

impl Render for bind_differential_web::bind_runtime::Node {
    fn render(&self) -> String {
        bind_differential_web::bind_runtime::render_html(self)
    }
}

impl<T: Render> Render for Box<T> {
    fn render(&self) -> String {
        (**self).render()
    }
}

impl Render for programs::Forma {
    fn render(&self) -> String {
        match self {
            programs::Forma::Circulo { raio } => format!("Circulo({})", raio.render()),
            programs::Forma::Retangulo { largura, altura } => {
                format!("Retangulo({}, {})", largura.render(), altura.render())
            }
            programs::Forma::Rotulada { nome, dentro } => {
                format!("Rotulada({}, {})", nome.render(), dentro.render())
            }
            programs::Forma::Vazia => "Vazia".to_string(),
        }
    }
}
"#;

/// The body of the Rust crate's `encode`: [`line_safe`].
const RUST_ENCODE: &str = r#"let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out"#;

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn line_safe_is_injective_on_breaks_and_backslashes() {
    assert_eq!(line_safe("a\\nb\nc\rd"), "a\\\\nb\\nc\\rd");
    assert_ne!(line_safe("\\n"), line_safe("\n"));
    assert_eq!(line_safe_entry("p1=<p>x\ny</p>"), "p1=<p>x\\ny</p>");
    assert_eq!(line_safe_entry("p2=a=b"), "p2=a=b");
}

/// Builds the generated crate and the generated `app.js`, runs both, and compares their
/// outputs with the evaluator's, line by line.
#[test]
#[ignore = "builds a large Rust crate and needs node; run with `cargo test differential -- --ignored`"]
fn differential_evaluator_vs_generated_rust_vs_generated_js() {
    if !node_available() {
        eprintln!("skipped: node not found");
        return;
    }
    let started = Instant::now();
    let (programs, counts) = web_programs();
    for construct in WEB_CONSTRUCTS {
        assert!(counts.contains_key(construct), "{construct} not generated");
    }
    let source = source_with(WEB_PRELUDE, &programs);
    let program = compile(&source);
    let expected: Vec<String> = expected(&program, &programs)
        .iter()
        .map(|line| line_safe_entry(line))
        .collect();
    let evaluated = started.elapsed();

    // Rust.
    let names: Vec<String> = program
        .functions
        .iter()
        .map(|function| function.name.clone())
        .collect();
    let module = crate::backend::rust::render_functions(&program, &names);
    let main = main_rs_with(&programs, "bind_differential_web", RUST_EXTRA, RUST_ENCODE);
    let rust_stdout = build_and_run("bind-differential-web", &source, &module, &main);
    let rust: Vec<&str> = rust_stdout.lines().collect();
    let rust_done = started.elapsed();

    // JavaScript.
    let program_names: Vec<String> = programs.iter().map(|(name, _, _)| name.clone()).collect();
    let app = super::render_app(&program, &program_names);
    let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/bind-differential-web-js");
    if folder.exists() {
        fs::remove_dir_all(&folder).unwrap();
    }
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("program.bd"), &source).unwrap();
    fs::write(folder.join("app.js"), &app).unwrap();
    fs::write(folder.join("bind-runtime.js"), super::RUNTIME).unwrap();
    fs::write(
        folder.join("dom.mjs"),
        include_str!("../../../runtime/bind-runtime-js/tests/dom.mjs"),
    )
    .unwrap();
    fs::write(
        folder.join("differential.mjs"),
        include_str!("tests/differential.mjs"),
    )
    .unwrap();
    let list: Vec<String> = programs
        .iter()
        .map(|(name, ty, _)| {
            format!(
                "[{}, {}]",
                super::names::js_string(name),
                super::names::js_string(ty.bind())
            )
        })
        .collect();
    fs::write(
        folder.join("programs.json"),
        format!("[{}]\n", list.join(",\n")),
    )
    .unwrap();
    let run = Command::new("node")
        .arg("differential.mjs")
        .current_dir(&folder)
        .output()
        .expect("run node");
    let js_stdout = String::from_utf8_lossy(&run.stdout).to_string();
    assert!(
        run.status.success(),
        "node differential.mjs failed:\n{}\n{}",
        js_stdout,
        String::from_utf8_lossy(&run.stderr)
    );
    let js: Vec<&str> = js_stdout.lines().collect();

    let mut divergences = Vec::new();
    for (index, (name, ty, function)) in programs.iter().enumerate() {
        let want = &expected[index];
        let from_rust = rust.get(index).copied().unwrap_or("<missing>");
        let from_js = js.get(index).copied().unwrap_or("<missing>");
        if want != from_rust || want != from_js {
            divergences.push(format!(
                "{name}: {}\n{function}evaluator: {want}\n     rust: {from_rust}\n       js: {from_js}",
                ty.bind()
            ));
        }
    }
    let mut by_type = std::collections::BTreeMap::new();
    for (_, ty, _) in &programs[..PROGRAMS] {
        *by_type.entry(ty.bind()).or_insert(0) += 1;
    }
    eprintln!(
        "three-way differential: {} programs (result types {by_type:?}), evaluator+front end {:.1?}, rust {:.1?}, js {:.1?}, total {:.1?}\nconstructs {counts:?}",
        programs.len(),
        evaluated,
        rust_done - evaluated,
        started.elapsed() - rust_done,
        started.elapsed()
    );
    assert_eq!(rust.len(), programs.len(), "rust line count");
    assert_eq!(js.len(), programs.len(), "js line count");
    assert!(
        divergences.is_empty(),
        "{} divergences:\n\n{}",
        divergences.len(),
        divergences.join("\n\n")
    );
}
