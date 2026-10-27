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
    /// The files of `web/` (no build step: they are deployed as generated).
    pub web_artifacts: Vec<PathBuf>,
    pub cargo_lock_hash: String,
    /// The hash of `web/bind-runtime.js`, when the page ships it (the split mode's dynamic page).
    pub js_runtime_hash: Option<String>,
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
    let cargo_toml = fs::read_to_string(generated.join("Cargo.toml"))
        .map_err(|error| format!("Cannot read the generated Cargo.toml: {error}"))?;
    let binaries = binaries(&cargo_toml);
    let mut rust_build = Command::new("cargo");
    rust_build.args(["build", "--release", "--locked"]);
    rust_build.arg(if binaries.is_empty() {
        "--lib"
    } else {
        "--bins"
    });
    rust_build
        .env("CARGO_TARGET_DIR", &cargo_target)
        .current_dir(generated);
    run("build generated Rust release artifacts", &mut rust_build)?;

    let web_artifacts = web_artifacts(&generated.join("web"))?;
    for artifact in &web_artifacts {
        require_non_empty(artifact)?;
    }
    let rust_artifacts = stage_rust_artifacts(generated, &cargo_target, &binaries)?;
    for artifact in &rust_artifacts {
        require_non_empty(artifact)?;
    }

    let cargo_lock_hash = hash_file(&generated.join("Cargo.lock"))?;
    let js_runtime = generated.join("web/bind-runtime.js");
    let js_runtime_hash = js_runtime
        .exists()
        .then(|| hash_file(&js_runtime))
        .transpose()?;
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
        certified::dependency_lock(target, Some(&cargo_lock_hash), js_runtime_hash.as_deref()),
    )
    .map_err(|error| format!("Cannot write {}: {error}", target_lock.display()))?;

    let manifest = generated.join("bind-manifest.json");
    let artifact_entries = rust_artifacts
        .iter()
        .chain(&web_artifacts)
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
            "{{\n  \"languageVersion\": \"0.1.0\",\n  \"compilerVersion\": \"bindc-0.1.0\",\n  \"profile\": {},\n  \"certified\": {},\n  \"certificationState\": {},\n  \"project\": {},\n  \"target\": {},\n  \"mode\": {},\n  \"deployment\": {},\n  \"sourceHash\": {},\n  \"schemaFingerprint\": {},\n  \"cargoLockSha256\": {},\n{}  \"wireVersion\": {},\n  \"apiUrl\": {},\n  \"frontendOrigin\": {},\n  \"environment\": [\n{environment_entries}\n  ],\n  \"databaseOperations\": [\n{database_entries}\n  ],\n  \"unsafeOperations\": {},\n  \"artifacts\": [\n{artifact_entries}\n  ]\n}}\n",
            json(certified::profile_version()),
            mode == BuildMode::Certify,
            json(certification_state),
            json(&app.project_name),
            json(&app.target),
            json(target.mode_name()),
            json(&target.deployment),
            json(&app.source_hash),
            json(&app.database.fingerprint),
            json(&cargo_lock_hash),
            js_runtime_hash
                .as_deref()
                .map(|hash| format!("  \"jsRuntimeSha256\": {},\n", json(hash)))
                .unwrap_or_default(),
            json(certified::wire_version()),
            certified::manifest_api_url(target),
            certified::manifest_frontend_origin(target),
            json_array(&app.unsafe_operations),
        ),
    )
    .map_err(|error| format!("Cannot write {}: {error}", manifest.display()))?;

    Ok(BuildReport {
        rust_artifacts,
        web_artifacts,
        cargo_lock_hash,
        js_runtime_hash,
        target_lock,
        manifest,
    })
}

/// The `[[bin]]` names of a generated `Cargo.toml`, in order.
fn binaries(cargo_toml: &str) -> Vec<String> {
    cargo_toml
        .split("[[bin]]\n")
        .skip(1)
        .filter_map(|section| {
            let line = section.lines().next()?;
            let name = line.strip_prefix("name = \"")?.strip_suffix('"')?;
            Some(name.to_string())
        })
        .collect()
}

/// Every file of the generated `web/`, sorted by path.
fn web_artifacts(web: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for entry in
        fs::read_dir(web).map_err(|error| format!("Cannot read {}: {error}", web.display()))?
    {
        let path = entry
            .map_err(|error| format!("Cannot read {}: {error}", web.display()))?
            .path();
        if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn stage_rust_artifacts(
    generated: &Path,
    cargo_target: &Path,
    names: &[String],
) -> Result<Vec<PathBuf>, String> {
    let release = cargo_target.join("release");
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let destination = generated.join("artifacts");
    fs::create_dir_all(&destination)
        .map_err(|error| format!("Cannot create artifact directory: {error}"))?;
    names
        .iter()
        .map(|name| {
            let source = release.join(name);
            require_non_empty(&source)?;
            let staged = destination.join(name);
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

pub(crate) fn hash_file(path: &Path) -> Result<String, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("Cannot hash {}: {error}", path.display()))?;
    Ok(hash_bytes(&bytes))
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binaries_are_read_from_the_generated_manifest() {
        let manifest = "[package]\nname = \"bind-app\"\n\n[dependencies]\nserde = \"=1\"\n\n[[bin]]\nname = \"get_customer\"\npath = \"api/getCustomer.rs\"\n\n[[bin]]\nname = \"bind-page\"\npath = \"api/bind-page.rs\"\n\n[profile.release]\nstrip = true\n";
        assert_eq!(binaries(manifest), ["get_customer", "bind-page"]);
        assert!(binaries("[package]\nname = \"bind-app\"\n").is_empty());
    }
}
