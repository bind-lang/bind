use std::path::PathBuf;

use crate::certified;
use crate::compiler::{ir_json, verify};
use crate::config::ProjectConfig;
use crate::diagnostic::has_errors;
use crate::pipeline::{self, BuildMode};

pub fn run(args: Vec<String>) -> Result<(), String> {
    if args.is_empty() || matches!(args[0].as_str(), "help" | "--help" | "-h") {
        print_help();
        return Ok(());
    }
    let manifest = option(&args, "--manifest")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("bind.toml"));
    let config = ProjectConfig::load(&manifest)?;
    match args[0].as_str() {
        "check" | "verify" => {
            let target = option(&args, "--target").unwrap_or_else(|| default_target(&config));
            let result = verify(&config, &target);
            print_diagnostics(&result.diagnostics);
            if has_errors(&result.diagnostics) {
                return Err("Compilation aborted.".into());
            }
            println!(
                "✓ Parsed {} module(s)\n✓ Types\n✓ Effects\n✓ Environment\n✓ Database\n✓ Server bindings\n✓ Client bindings\n\nApplication bound successfully.",
                result.module_count
            );
        }
        "generate" | "compile" | "build" | "certify" => {
            let command = args[0].as_str();
            let target = option(&args, "--target").unwrap_or_else(|| default_target(&config));
            let result = verify(&config, &target);
            print_diagnostics(&result.diagnostics);
            let Some(app) = result.application else {
                return Err("Compilation aborted. No target code was generated.".into());
            };
            let output = option(&args, "--out").map(PathBuf::from);
            let target_config = config
                .targets
                .get(&target)
                .ok_or_else(|| format!("Target `{target}` is not declared"))?;
            let generated = certified::generate(&app, &config, output.as_deref())?;
            if args.iter().any(|arg| arg == "--emit-ir") {
                std::fs::write(
                    generated.join("verified-application.ir.json"),
                    ir_json(&app),
                )
                .map_err(|error| error.to_string())?;
            }
            if args.iter().any(|arg| arg == "--emit-graph") {
                std::fs::write(
                    generated.join("application.graph.txt"),
                    graph_text(&app.graph),
                )
                .map_err(|error| error.to_string())?;
            }
            if command == "generate" {
                println!(
                    "✓ Parsed {} module(s)\n✓ Whole-application verification\n✓ Target generation\n\nApplication generated successfully.\nGenerated: {}",
                    result.module_count,
                    generated.display()
                );
            } else {
                let mode = if command == "certify" {
                    BuildMode::Certify
                } else {
                    BuildMode::Build
                };
                let report = pipeline::build(&generated, &app, &config, target_config, mode)?;
                println!(
                    "✓ Parsed {} module(s)\n✓ Whole-application verification\n✓ Rust target compilation and tests\n{}✓ Artifact hashing and lockfile\n\nApplication {} successfully.\nManifest: {}\nTarget lock: {}\nWeb:\n  {}\nRust artifacts:\n  {}\nCargo lock: {}\nJS runtime: {}",
                    result.module_count,
                    if mode == BuildMode::Certify {
                        "✓ Rust lint policy\n"
                    } else {
                        ""
                    },
                    if mode == BuildMode::Certify {
                        "certified"
                    } else {
                        "built"
                    },
                    report.manifest.display(),
                    report.target_lock.display(),
                    report
                        .web_artifacts
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n  "),
                    report
                        .rust_artifacts
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n  "),
                    report.cargo_lock_hash,
                    report.js_runtime_hash.as_deref().unwrap_or("not shipped")
                );
            }
        }
        "graph" => {
            let root =
                positional(&args, 1).ok_or_else(|| "Usage: bind graph <symbol>".to_string())?;
            let app = verified(&config, &args)?;
            println!("{}", graph_output(&app, &root));
        }
        "impact" => {
            let resource =
                positional(&args, 1).ok_or_else(|| "Usage: bind impact <resource>".to_string())?;
            let app = verified(&config, &args)?;
            print!("{}", impact_output(&app, &resource));
        }
        "target" => target_command(&args, &config)?,
        "db" if args.get(1).map(String::as_str) == Some("inspect") => {
            let app = verified(&config, &args)?;
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
        command => return Err(format!("Unknown command `{command}`. Run `bind help`.")),
    }
    Ok(())
}

fn verified(
    config: &ProjectConfig,
    args: &[String],
) -> Result<crate::compiler::VerifiedApplication, String> {
    let target = option(args, "--target").unwrap_or_else(|| default_target(config));
    let result = verify(config, &target);
    print_diagnostics(&result.diagnostics);
    result
        .application
        .ok_or_else(|| "Compilation aborted.".into())
}

fn target_command(args: &[String], config: &ProjectConfig) -> Result<(), String> {
    match args.get(1).map(String::as_str) {
        Some("list") => {
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
        Some("inspect") => {
            let name = positional(args, 2)
                .ok_or_else(|| "Usage: bind target inspect <name>".to_string())?;
            let target = config
                .targets
                .get(&name)
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
        }
        _ => return Err("Usage: bind target <list|inspect>".into()),
    }
    Ok(())
}

fn option(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1).cloned())
        .or_else(|| {
            args.iter()
                .find_map(|arg| arg.strip_prefix(&format!("{name}=")).map(str::to_string))
        })
}

fn positional(args: &[String], index: usize) -> Option<String> {
    args.get(index)
        .filter(|item| !item.starts_with('-'))
        .cloned()
}
fn default_target(config: &ProjectConfig) -> String {
    config
        .targets
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| "aws-lambda".into())
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
        r#"Bind 0.1 — whole-application compiler

Usage:
  bind check [--target <target>]
  bind verify [--target <target>]
  bind generate --target <target> [--emit-ir] [--emit-graph]
  bind build --target <target> [--emit-ir] [--emit-graph]
  bind certify --target <target> [--emit-ir] [--emit-graph]
  bind compile --target <target>  Alias for `bind build`
  bind graph <symbol>
  bind impact <resource>
  bind db inspect
  bind target list
  bind target inspect <name>

Options:
  --manifest <path>  Project manifest (default: bind.toml)
  --out <path>       Override generated workspace path
"#
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
}
