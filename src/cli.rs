use std::path::{Path, PathBuf};

use crate::certified;
use crate::compiler::{EMITTED_GRAPH, EMITTED_IR, ir_json, verify};
use crate::config::{Backend, ProjectConfig, TargetConfig};
use crate::diagnostic::has_errors;
use crate::pipeline::{self, BuildMode, BuildOptions};

/// Why the CLI stopped: a usage error (exit status 2) or a failed command (exit status 1).
#[derive(Debug, PartialEq, Eq)]
pub enum CliError {
    Usage(String),
    Failure(String),
}

impl CliError {
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Usage(_) => 2,
            CliError::Failure(_) => 1,
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Usage(message) => write!(formatter, "{message}\nRun `bind help` for usage."),
            CliError::Failure(message) => write!(formatter, "{message}"),
        }
    }
}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        CliError::Failure(message)
    }
}

fn usage(message: impl Into<String>) -> CliError {
    CliError::Usage(message.into())
}

/// The parsed command line.
#[derive(Debug, Default, PartialEq, Eq)]
struct Arguments {
    /// The command and its positional arguments.
    positional: Vec<String>,
    manifest: Option<String>,
    target: Option<String>,
    out: Option<String>,
    emit_ir: bool,
    emit_graph: bool,
    update_lock: bool,
}

/// Options that take a value (`--name value` or `--name=value`).
const VALUE_OPTIONS: &[&str] = &["--manifest", "--target", "--out"];
/// Options without a value.
const FLAGS: &[&str] = &["--emit-ir", "--emit-graph", "--update-lock"];

fn parse_arguments(args: &[String]) -> Result<Arguments, CliError> {
    let mut parsed = Arguments::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        index += 1;
        if !arg.starts_with('-') || arg == "-" {
            parsed.positional.push(arg.clone());
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if VALUE_OPTIONS.contains(&name) {
            let value = match inline {
                Some(value) => value,
                None => {
                    let value = args
                        .get(index)
                        .filter(|value| !value.starts_with("--"))
                        .ok_or_else(|| usage(format!("`{name}` needs a value")))?;
                    index += 1;
                    value.clone()
                }
            };
            if value.is_empty() {
                return Err(usage(format!("`{name}` needs a value")));
            }
            let slot = match name {
                "--manifest" => &mut parsed.manifest,
                "--target" => &mut parsed.target,
                _ => &mut parsed.out,
            };
            if slot.replace(value).is_some() {
                return Err(usage(format!("`{name}` is given more than once")));
            }
        } else if FLAGS.contains(&name) {
            if inline.is_some() {
                return Err(usage(format!("`{name}` takes no value")));
            }
            match name {
                "--emit-ir" => parsed.emit_ir = true,
                "--emit-graph" => parsed.emit_graph = true,
                _ => parsed.update_lock = true,
            }
        } else {
            return Err(usage(format!("Unknown option `{arg}`")));
        }
    }
    Ok(parsed)
}

impl Arguments {
    /// Rejects options the command does not accept and a wrong number of positional arguments.
    fn expect(&self, command: &str, positional: usize, options: &[&str]) -> Result<(), CliError> {
        let given = [
            ("--manifest", self.manifest.is_some()),
            ("--target", self.target.is_some()),
            ("--out", self.out.is_some()),
            ("--emit-ir", self.emit_ir),
            ("--emit-graph", self.emit_graph),
            ("--update-lock", self.update_lock),
        ];
        for (name, present) in given {
            if present && !options.contains(&name) {
                return Err(usage(format!("`bind {command}` does not accept `{name}`")));
            }
        }
        if self.positional.len() > positional {
            return Err(usage(format!(
                "Unexpected argument `{}` for `bind {command}`",
                self.positional[positional]
            )));
        }
        if self.positional.len() < positional {
            return Err(usage(format!("`bind {command}` is missing an argument")));
        }
        Ok(())
    }
}

const TARGET_OPTIONS: &[&str] = &["--manifest", "--target"];
const GENERATE_OPTIONS: &[&str] = &[
    "--manifest",
    "--target",
    "--out",
    "--emit-ir",
    "--emit-graph",
];
const BUILD_OPTIONS: &[&str] = &[
    "--manifest",
    "--target",
    "--out",
    "--emit-ir",
    "--emit-graph",
    "--update-lock",
];

pub fn run(args: Vec<String>) -> Result<(), CliError> {
    // Help and version never read the manifest.
    if args.is_empty()
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
        || args[0] == "help"
    {
        print_help();
        return Ok(());
    }
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--version" | "-V"))
        || args[0] == "version"
    {
        println!("bind {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let arguments = parse_arguments(&args)?;
    let Some(command) = arguments.positional.first().cloned() else {
        return Err(usage("Missing command"));
    };
    let manifest = PathBuf::from(arguments.manifest.as_deref().unwrap_or("bind.toml"));
    match command.as_str() {
        "check" | "verify" => {
            arguments.expect(&command, 1, TARGET_OPTIONS)?;
            let config = ProjectConfig::load(&manifest)?;
            let target = select_target(&config, &arguments)?;
            let result = verify(&config, &target);
            print_diagnostics(&result.diagnostics);
            if has_errors(&result.diagnostics) {
                return Err(CliError::Failure("Compilation aborted.".into()));
            }
            println!(
                "✓ Parsed {} module(s)\n✓ Types\n✓ Effects\n✓ Environment\n✓ Database\n✓ Server bindings\n✓ Client bindings\n\nApplication bound successfully.",
                result.module_count
            );
        }
        "generate" | "compile" | "build" | "certify" => {
            let options = if command == "generate" {
                GENERATE_OPTIONS
            } else {
                BUILD_OPTIONS
            };
            arguments.expect(&command, 1, options)?;
            let config = ProjectConfig::load(&manifest)?;
            let target = select_target(&config, &arguments)?;
            let target_config = config
                .targets
                .get(&target)
                .ok_or_else(|| format!("Target `{target}` is not declared"))?;
            if command == "certify" && !target_config.certified {
                return Err(CliError::Failure(format!(
                    "Target `{target}` is not declared `certified = true`: `bind certify` only certifies targets checked against the certified surface. Use `bind build`, or add `certified = true` to [target.{target}]."
                )));
            }
            generate_and_build(&command, &arguments, &config, &target, target_config)?;
        }
        "graph" | "impact" => {
            arguments.expect(&command, 2, TARGET_OPTIONS)?;
            let config = ProjectConfig::load(&manifest)?;
            let app = verified(&config, &arguments)?;
            let subject = &arguments.positional[1];
            if command == "graph" {
                println!("{}", graph_output(&app, subject));
            } else {
                print!("{}", impact_output(&app, subject));
            }
        }
        "target" => {
            let config = || ProjectConfig::load(&manifest);
            match arguments.positional.get(1).map(String::as_str) {
                Some("list") => {
                    arguments.expect("target list", 2, &["--manifest"])?;
                    target_list(&config()?);
                }
                Some("inspect") => {
                    arguments.expect("target inspect", 3, &["--manifest"])?;
                    target_inspect(&config()?, &arguments.positional[2])?;
                }
                _ => return Err(usage("Usage: bind target <list|inspect <name>>")),
            }
        }
        "db" if arguments.positional.get(1).map(String::as_str) == Some("inspect") => {
            arguments.expect("db inspect", 2, TARGET_OPTIONS)?;
            let config = ProjectConfig::load(&manifest)?;
            let app = verified(&config, &arguments)?;
            println!(
                "Database: main\nEngine: {}\nFingerprint: {}",
                app.database.engine, app.database.fingerprint
            );
            for table in app.database.tables.values() {
                println!("\n{} -> {}", table.name, table.model);
                for column in table.columns.values() {
                    println!(
                        "  {}: {}{}",
                        column.name,
                        column.type_name,
                        if column.nullable { "?" } else { "" }
                    );
                }
            }
        }
        command => return Err(usage(format!("Unknown command `{command}`"))),
    }
    Ok(())
}

/// `generate`, `build` and `certify`: generation, then (except for `generate`) the target's
/// build pipeline; the `--emit-*` files are written last, so they are never hashed.
fn generate_and_build(
    command: &str,
    arguments: &Arguments,
    config: &ProjectConfig,
    target: &str,
    target_config: &TargetConfig,
) -> Result<(), CliError> {
    let result = verify(config, target);
    print_diagnostics(&result.diagnostics);
    let Some(app) = result.application else {
        return Err(CliError::Failure(
            "Compilation aborted. No target code was generated.".into(),
        ));
    };
    let output = arguments.out.as_deref().map(Path::new);
    let cloudflare = target_config.backend() == Backend::CloudflareWasm;
    let generated = if cloudflare {
        crate::cloudflare::generate(&app, config, output)?
    } else {
        certified::generate(&app, config, output)?
    };
    if command == "generate" {
        emit(arguments, &generated, &app)?;
        println!(
            "✓ Parsed {} module(s)\n✓ Whole-application verification\n✓ Target generation\n\nApplication generated successfully.\nGenerated: {}",
            result.module_count,
            generated.display()
        );
        return Ok(());
    }
    let mode = if command == "certify" {
        BuildMode::Certify
    } else {
        BuildMode::Build
    };
    let options = BuildOptions {
        mode,
        update_lock: arguments.update_lock,
    };
    let state = if mode == BuildMode::Certify {
        "certified"
    } else {
        "built"
    };
    let list = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n  ")
    };
    if cloudflare {
        let report = crate::cloudflare::build(&generated, &app, config, target_config, options)?;
        emit(arguments, &generated, &app)?;
        println!(
            "✓ Parsed {} module(s)\n✓ Whole-application verification\n✓ Direct Bind IR → WebAssembly generation (no Rust target or LLVM)\n{}✓ Artifact hashing and lockfile\n\nApplication {state} successfully.\nManifest: {}\nTarget lock: {}\nWorker artifacts:\n  {}\nWasm module: {}\nJS runtime: {}",
            result.module_count,
            if mode == BuildMode::Certify {
                "✓ Generated JavaScript and WebAssembly validation\n"
            } else {
                ""
            },
            report.manifest.display(),
            report.target_lock.display(),
            list(&report.artifacts),
            report.wasm_hash,
            report.js_runtime_hash,
        );
    } else {
        let report = pipeline::build(&generated, &app, config, target_config, options)?;
        emit(arguments, &generated, &app)?;
        println!(
            "✓ Parsed {} module(s)\n✓ Whole-application verification\n✓ Rust target compilation and tests\n{}✓ Artifact hashing and lockfile\n\nApplication {state} successfully.\nManifest: {}\nTarget lock: {}\nWeb:\n  {}\nDeployment descriptors:\n  {}\nRust artifacts:\n  {}\nCargo lock: {}\nJS runtime: {}",
            result.module_count,
            if mode == BuildMode::Certify {
                "✓ Rust lint policy\n"
            } else {
                ""
            },
            report.manifest.display(),
            report.target_lock.display(),
            list(&report.web_artifacts),
            list(&report.deployment_descriptors),
            list(&report.rust_artifacts),
            report.cargo_lock_hash,
            report.js_runtime_hash.as_deref().unwrap_or("not shipped")
        );
    }
    Ok(())
}

/// Writes the `--emit-ir` and `--emit-graph` files.
fn emit(
    arguments: &Arguments,
    generated: &Path,
    app: &crate::compiler::VerifiedApplication,
) -> Result<(), CliError> {
    if arguments.emit_ir {
        std::fs::write(generated.join(EMITTED_IR), ir_json(app))
            .map_err(|error| error.to_string())?;
    }
    if arguments.emit_graph {
        std::fs::write(generated.join(EMITTED_GRAPH), graph_text(&app.graph))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// The target to compile: `--target`, or the only declared target. With several targets the
/// choice is never implicit.
fn select_target(config: &ProjectConfig, arguments: &Arguments) -> Result<String, CliError> {
    if let Some(target) = &arguments.target {
        return Ok(target.clone());
    }
    match config.targets.keys().collect::<Vec<_>>().as_slice() {
        [only] => Ok((*only).clone()),
        names => Err(usage(format!(
            "Several targets are declared ({}): choose one with --target <name>",
            names
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn verified(
    config: &ProjectConfig,
    arguments: &Arguments,
) -> Result<crate::compiler::VerifiedApplication, CliError> {
    let target = select_target(config, arguments)?;
    let result = verify(config, &target);
    print_diagnostics(&result.diagnostics);
    result
        .application
        .ok_or_else(|| CliError::Failure("Compilation aborted.".into()))
}

fn target_list(config: &ProjectConfig) {
    for (name, target) in &config.targets {
        println!(
            "{name}\t{} + {}\t{}\t{}\t{}",
            target.client,
            target.server,
            target.mode_name(),
            target.deployment,
            if target.certified {
                "certified"
            } else {
                "experimental"
            }
        );
    }
}

fn target_inspect(config: &ProjectConfig, name: &str) -> Result<(), CliError> {
    let target = config
        .targets
        .get(name)
        .ok_or_else(|| format!("Unknown target `{name}`"))?;
    println!(
        "Target: {name}\nClient: {}\nServer: {}\nMode: {}\nDeployment: {}\nCertification profile: {}\nCapabilities:\n  {}",
        target.client,
        target.server,
        target.mode_name(),
        target.deployment,
        if target.certified {
            "certified"
        } else {
            "experimental"
        },
        target.capabilities.join("\n  ")
    );
    Ok(())
}

fn print_diagnostics(items: &[crate::diagnostic::Diagnostic]) {
    for item in items {
        eprintln!("{item}");
    }
}
/// What `bind graph <symbol>` prints: the tree of what `root` uses.
fn graph_output(app: &crate::compiler::VerifiedApplication, root: &str) -> String {
    app.graph.tree(root)
}

/// What `bind impact <resource>` prints: everything that depends on `resource`.
fn impact_output(app: &crate::compiler::VerifiedApplication, resource: &str) -> String {
    let mut output = format!("Affected by {resource}:\n\n");
    for item in app.graph.impact(resource) {
        output.push_str(&format!("  {item}\n"));
    }
    output
}

fn graph_text(graph: &crate::compiler::ApplicationGraph) -> String {
    graph
        .edges
        .iter()
        .map(|(from, targets)| {
            format!(
                "{from}: {}",
                targets.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn print_help() {
    println!(
        r#"Bind {} — whole-application compiler

Usage:
  bind check [--target <target>]
  bind verify [--target <target>]
  bind generate [--target <target>] [--out <path>] [--emit-ir] [--emit-graph]
  bind build [--target <target>] [--out <path>] [--update-lock] [--emit-ir] [--emit-graph]
  bind certify [--target <target>] [--out <path>] [--update-lock] [--emit-ir] [--emit-graph]
  bind compile ...                 Alias for `bind build`
  bind graph <symbol> [--target <target>]
  bind impact <resource> [--target <target>]
  bind db inspect [--target <target>]
  bind target list
  bind target inspect <name>
  bind version

Options:
  --manifest <path>  Project manifest (default: bind.toml)
  --target <name>    Target to compile; required when bind.toml declares several
  --out <path>       Generated workspace path (default: <output>/<target>); an existing
                     directory is only replaced if it is empty or was generated by Bind
  --update-lock      Accept a new dependency resolution and rewrite bind.lock and
                     .bind/locks/<target>.* (otherwise a lock that differs is an error, and
                     `certify` also refuses a missing lock)
"#,
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `examples/commerce` verified for its SSR target (the repository's `bind.toml`).
    fn commerce() -> crate::compiler::VerifiedApplication {
        if std::env::var_os("DATABASE_URL").is_none() {
            // SAFETY: read only through `std::env`, whose accesses take the same lock.
            unsafe { std::env::set_var("DATABASE_URL", "postgresql://bind:bind@localhost/bind") };
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        let result = verify(&config, "aws-lambda-ssr");
        result
            .application
            .unwrap_or_else(|| panic!("{:?}", result.diagnostics))
    }

    #[test]
    fn commerce_graph_is_computed_from_the_ir() {
        let app = commerce();
        assert_eq!(
            graph_text(&app.graph),
            include_str!("../tests/analysis/commerce.graph.expected")
        );
        let mut from_ir = crate::core::analysis::application_graph(&app.core);
        from_ir.add_edge("environment.applicationName", "env.APP_NAME");
        from_ir.add_edge("environment.databaseUrl", "env.DATABASE_URL");
        assert_eq!(app.graph, from_ir);
    }

    #[test]
    fn commerce_graph_and_impact_commands() {
        let app = commerce();
        assert_eq!(
            graph_output(&app, "CustomerPage"),
            "CustomerPage\n\
             └── getCustomer\n\
             \x20   ├── database.main.customers\n\
             \x20   ├── database.main.customers.balance\n\
             \x20   ├── database.main.customers.email\n\
             \x20   ├── database.main.customers.id\n\
             \x20   └── database.main.customers.name\n"
        );
        assert_eq!(
            graph_output(&app, "cadastrar"),
            "cadastrar\n\
             ├── database.main.customers\n\
             ├── database.main.customers.balance\n\
             ├── database.main.customers.email\n\
             ├── database.main.customers.id\n\
             ├── database.main.customers.name\n\
             └── normalizar\n"
        );
        assert_eq!(
            impact_output(&app, "database.main.customers.name"),
            "Affected by database.main.customers.name:\n\n  CustomerPage\n  cadastrar\n  getCustomer\n"
        );
        assert_eq!(
            impact_output(&app, "env.DATABASE_URL"),
            "Affected by env.DATABASE_URL:\n\n  environment.databaseUrl\n"
        );
    }

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn options_need_values_and_unknown_options_are_refused() {
        let parsed = parse_arguments(&args(&[
            "build",
            "--target=a",
            "--out",
            "x",
            "--update-lock",
        ]))
        .unwrap();
        assert_eq!(parsed.positional, ["build"]);
        assert_eq!(parsed.target.as_deref(), Some("a"));
        assert_eq!(parsed.out.as_deref(), Some("x"));
        assert!(parsed.update_lock);
        for (line, expected) in [
            (&["generate", "--target"][..], "needs a value"),
            (&["generate", "--target", "--emit-ir"][..], "needs a value"),
            (&["generate", "--target="][..], "needs a value"),
            (&["generate", "--emit-irr"][..], "Unknown option"),
            (&["generate", "--emit-ir=yes"][..], "takes no value"),
            (
                &["generate", "--target", "a", "--target", "b"][..],
                "more than once",
            ),
        ] {
            match parse_arguments(&args(line)) {
                Err(CliError::Usage(message)) => {
                    assert!(message.contains(expected), "{line:?}: {message}")
                }
                other => panic!("{line:?}: {other:?}"),
            }
        }
        let parsed = parse_arguments(&args(&["check", "--update-lock"])).unwrap();
        assert!(matches!(
            parsed.expect("check", 1, TARGET_OPTIONS),
            Err(CliError::Usage(_))
        ));
        let parsed = parse_arguments(&args(&["check", "extra"])).unwrap();
        assert!(parsed.expect("check", 1, TARGET_OPTIONS).is_err());
        assert_eq!(CliError::Usage(String::new()).exit_code(), 2);
        assert_eq!(CliError::Failure(String::new()).exit_code(), 1);
    }

    #[test]
    fn help_and_version_do_not_read_the_manifest() {
        for line in [
            &["--version"][..],
            &["version"],
            &["help"],
            &["check", "--help"],
        ] {
            let mut line = args(line);
            line.extend(args(&["--manifest", "/nonexistent/bind.toml"]));
            assert_eq!(run(line.clone()), Ok(()), "{line:?}");
        }
    }

    #[test]
    fn several_targets_require_an_explicit_target_and_certify_requires_certified() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        let none = Arguments::default();
        match select_target(&config, &none) {
            Err(CliError::Usage(message)) => assert!(message.contains("--target"), "{message}"),
            other => panic!("{other:?}"),
        }
        let explicit = Arguments {
            target: Some("vercel-ssr".into()),
            ..Arguments::default()
        };
        assert_eq!(select_target(&config, &explicit).unwrap(), "vercel-ssr");
        let temp = std::env::temp_dir().join(format!("bindc-cli-{}", std::process::id()));
        std::fs::create_dir_all(temp.join("src")).unwrap();
        std::fs::write(
            temp.join("src/app.bd"),
            "server fun double(value: Double): Double effects [] = value * 2.0\nclient component Home() {\n val answer = server.double(21.0)\n UI { Text(answer) }\n}\n",
        )
        .unwrap();
        std::fs::write(
            temp.join("bind.toml"),
            "[target.worker]\nclient = \"web\"\nserver = \"wasm\"\nmode = \"split\"\ndeployment = \"cloudflare-workers\"\ncapabilities = [\"ui.dom\", \"server.http\"]\n",
        )
        .unwrap();
        let manifest = temp.join("bind.toml").to_string_lossy().to_string();
        match run(args(&["certify", "--manifest", &manifest])) {
            Err(CliError::Failure(message)) => {
                assert!(message.contains("certified = true"), "{message}")
            }
            other => panic!("{other:?}"),
        }
        assert!(!temp.join(".bind").exists());
        std::fs::remove_dir_all(temp).unwrap();
    }
}
