use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use crate::certified;
use crate::compiler::{VerifiedApplication, json, json_array};
use crate::config::{ProjectConfig, TargetConfig};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildMode {
    Build,
    Certify,
}

#[derive(Clone, Debug)]
pub struct BuildReport {
    pub rust_artifacts: Vec<PathBuf>,
    pub web_artifact: PathBuf,
    pub cargo_lock_hash: String,
    pub npm_lock_hash: Option<String>,
    pub elm_json_hash: String,
    pub target_lock: PathBuf,
    pub manifest: PathBuf,
}

pub fn build(
    generated: &Path,
    app: &VerifiedApplication,
    config: &ProjectConfig,
    target: &TargetConfig,
    mode: BuildMode,
) -> Result<BuildReport, String> {
    ensure_tool("cargo", &["--version"])?;
    ensure_version("rustc", &["--version"], "rustc 1.98.0 ")?;
    ensure_version("elm", &["--version"], "0.19.2")?;
    if target.deployment == "vercel" {
        ensure_tool("npm", &["--version"])?;
    }

    if !generated.join("Cargo.lock").exists() {
        run(
            "resolve the pinned Rust dependency graph",
            Command::new("cargo")
                .arg("generate-lockfile")
                .current_dir(generated),
        )?;
    }
    run(
        "format generated Rust",
        Command::new("cargo")
            .args(["fmt", "--all"])
            .current_dir(generated),
    )?;
    let cargo_target = config.root.join(".bind/cargo-target");
    fs::create_dir_all(&cargo_target)
        .map_err(|error| format!("Cannot create shared Rust build cache: {error}"))?;
    let cargo_target = fs::canonicalize(&cargo_target)
        .map_err(|error| format!("Cannot resolve shared Rust build cache: {error}"))?;
    run(
        "compile and test generated Rust",
        Command::new("cargo")
            .args(["test", "--locked", "--all-targets"])
            .env("CARGO_TARGET_DIR", &cargo_target)
            .current_dir(generated),
    )?;
    if mode == BuildMode::Certify {
        run(
            "lint generated Rust without warnings",
            Command::new("cargo")
                .args([
                    "clippy",
                    "--locked",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings",
                ])
                .env("CARGO_TARGET_DIR", &cargo_target)
                .current_dir(generated),
        )?;
    }
    let mut rust_build = Command::new("cargo");
    rust_build.args(["build", "--release", "--locked"]);
    rust_build.arg(if app.rpc_contracts.is_empty() {
        "--lib"
    } else {
        "--bins"
    });
    rust_build
        .env("CARGO_TARGET_DIR", &cargo_target)
        .current_dir(generated);
    run("build generated Rust release artifacts", &mut rust_build)?;

    let web = generated.join("web");
    let web_artifact = web.join("dist/main.js");
    if target.deployment == "vercel" {
        if !generated.join("package-lock.json").exists() {
            run(
                "resolve the pinned npm dependency graph",
                Command::new("npm")
                    .args(["install", "--package-lock-only", "--no-audit", "--no-fund"])
                    .current_dir(generated),
            )?;
        }
        run(
            "install the locked Vercel web toolchain",
            Command::new("npm")
                .args(["ci", "--no-audit", "--no-fund"])
                .current_dir(generated),
        )?;
        run(
            "compile generated Elm through the Vercel build command",
            Command::new("npm")
                .args(["run", "build:web"])
                .current_dir(generated),
        )?;
    } else {
        run(
            "compile generated Elm",
            Command::new("elm")
                .args([
                    "make",
                    "src/Main.elm",
                    "--optimize",
                    "--output",
                    "dist/main.js",
                ])
                .current_dir(&web),
        )?;
    }
    fs::copy(web.join("index.html"), web.join("dist/index.html"))
        .map_err(|error| format!("Cannot stage web/index.html: {error}"))?;
    fs::copy(web.join("styles.css"), web.join("dist/styles.css"))
        .map_err(|error| format!("Cannot stage web/styles.css: {error}"))?;

    require_non_empty(&web_artifact)?;
    let rust_artifacts = stage_rust_artifacts(generated, &cargo_target, app, target)?;
    for artifact in &rust_artifacts {
        require_non_empty(artifact)?;
    }

    let cargo_lock_hash = hash_file(&generated.join("Cargo.lock"))?;
    let npm_lock_hash = generated
        .join("package-lock.json")
        .exists()
        .then(|| hash_file(&generated.join("package-lock.json")))
        .transpose()?;
    let elm_json_hash = hash_file(&web.join("elm.json"))?;
    fs::write(
        config.root.join("bind.lock"),
        certified::workspace_lock(config),
    )
    .map_err(|error| format!("Cannot write bind.lock: {error}"))?;
    let lock_directory = config.root.join(".bind/locks");
    fs::create_dir_all(&lock_directory)
        .map_err(|error| format!("Cannot create target lock directory: {error}"))?;
    let target_lock = lock_directory.join(format!("{}.lock", app.target));
    fs::write(
        &target_lock,
        certified::dependency_lock(
            target,
            Some(&cargo_lock_hash),
            npm_lock_hash.as_deref(),
            Some(&elm_json_hash),
        ),
    )
    .map_err(|error| format!("Cannot write {}: {error}", target_lock.display()))?;

    let manifest = generated.join("bind-manifest.json");
    let artifact_entries = rust_artifacts
        .iter()
        .chain(std::iter::once(&web_artifact))
        .map(|path| {
            let relative = path.strip_prefix(generated).unwrap_or(path);
            Ok(format!(
                "    {{\"path\":{},\"sha256\":{}}}",
                json(&relative.to_string_lossy()),
                json(&hash_file(path)?)
            ))
        })
        .collect::<Result<Vec<_>, String>>()?
        .join(",\n");
    let certification_state = if mode == BuildMode::Certify {
        "certified-host"
    } else {
        "built-host"
    };
    let environment_entries = app
        .environment
        .iter()
        .map(|item| {
            format!(
                "    {{\"variable\":{},\"type\":{},\"lifecycle\":{},\"required\":{},\"resolvedDuringBuild\":{},\"secret\":{},\"public\":{}}}",
                json(&item.variable),
                json(&item.type_name),
                json(&item.lifecycle),
                item.required,
                item.resolved,
                item.secret,
                item.public
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    let database_entries = app
        .database_operations
        .iter()
        .map(|item| {
            format!(
                "    {{\"function\":{},\"table\":{},\"method\":{},\"effect\":{}}}",
                json(&item.function),
                json(&item.table),
                json(&item.method),
                json(&item.effect)
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    fs::write(
        &manifest,
        format!(
            "{{\n  \"languageVersion\": \"0.1.0\",\n  \"compilerVersion\": \"bindc-0.1.0\",\n  \"profile\": {},\n  \"certified\": {},\n  \"certificationState\": {},\n  \"project\": {},\n  \"target\": {},\n  \"deployment\": {},\n  \"sourceHash\": {},\n  \"schemaFingerprint\": {},\n  \"cargoLockSha256\": {},\n  \"npmLockSha256\": {},\n  \"elmJsonSha256\": {},\n  \"wireVersion\": {},\n  \"environment\": [\n{environment_entries}\n  ],\n  \"databaseOperations\": [\n{database_entries}\n  ],\n  \"unsafeOperations\": {},\n  \"artifacts\": [\n{artifact_entries}\n  ]\n}}\n",
            json(certified::profile_version()),
            mode == BuildMode::Certify,
            json(certification_state),
            json(&app.project_name),
            json(&app.target),
            json(&target.deployment),
            json(&app.source_hash),
            json(&app.database.fingerprint),
            json(&cargo_lock_hash),
            npm_lock_hash
                .as_deref()
                .map(json)
                .unwrap_or_else(|| "null".to_string()),
            json(&elm_json_hash),
            json(certified::wire_version()),
            json_array(&app.unsafe_operations),
        ),
    )
    .map_err(|error| format!("Cannot write {}: {error}", manifest.display()))?;

    Ok(BuildReport {
        rust_artifacts,
        web_artifact,
        cargo_lock_hash,
        npm_lock_hash,
        elm_json_hash,
        target_lock,
        manifest,
    })
}

fn stage_rust_artifacts(
    generated: &Path,
    cargo_target: &Path,
    app: &VerifiedApplication,
    target: &TargetConfig,
) -> Result<Vec<PathBuf>, String> {
    let release = cargo_target.join("release");
    let names = if target.deployment == "aws-lambda" {
        vec!["bootstrap".to_string()]
    } else {
        app.rpc_contracts
            .iter()
            .map(|rpc| snake_case(&rpc.name))
            .collect::<Vec<_>>()
    };
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let destination = generated.join("artifacts");
    fs::create_dir_all(&destination)
        .map_err(|error| format!("Cannot create artifact directory: {error}"))?;
    names
        .into_iter()
        .map(|name| {
            let source = release.join(&name);
            require_non_empty(&source)?;
            let staged = destination.join(&name);
            fs::copy(&source, &staged).map_err(|error| {
                format!(
                    "Cannot stage Rust artifact {} as {}: {error}",
                    source.display(),
                    staged.display()
                )
            })?;
            Ok(staged)
        })
        .collect()
}

fn ensure_tool(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| format!("Required tool `{program}` is not installed or not on PATH"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Required tool `{program}` is not usable"))
    }
}

fn ensure_version(program: &str, args: &[&str], expected: &str) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|_| format!("Required tool `{program}` is not installed or not on PATH"))?;
    let actual = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() && (actual == expected || actual.starts_with(expected)) {
        Ok(())
    } else {
        Err(format!(
            "Certified profile requires `{program}` version `{expected}`; found `{actual}`"
        ))
    }
}

fn run(label: &str, command: &mut Command) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|error| format!("Cannot {label}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "Failed to {label}.\n\nstdout:\n{stdout}\n\nstderr:\n{stderr}"
    ))
}

fn require_non_empty(path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Expected artifact {}: {error}", path.display()))?;
    if metadata.len() == 0 {
        Err(format!("Artifact {} is empty", path.display()))
    } else {
        Ok(())
    }
}

fn hash_file(path: &Path) -> Result<String, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("Cannot hash {}: {error}", path.display()))?;
    let digest = Sha256::digest(bytes);
    Ok(format!("sha256:{digest:x}"))
}

fn snake_case(value: &str) -> String {
    let mut output = String::new();
    for (index, ch) in value.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index > 0 {
                output.push('_');
            }
            output.push(ch.to_ascii_lowercase());
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            output.push(ch.to_ascii_lowercase());
        } else {
            output.push('_');
        }
    }
    output
}
