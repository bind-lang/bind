//! Cloudflare Workers execution profile.
//!
//! Bind IR is translated directly to WebAssembly Binary Format plus a small JavaScript host for
//! Cloudflare's V8 isolate. There is no generated Rust, LLVM output, WAT toolchain, or
//! `wasm-bindgen`: the Bind compiler owns the lowering and binary encoding.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::backend::js::page::web_build;
use crate::compiler::{VerifiedApplication, json, json_array};
use crate::config::{ProjectConfig, TargetConfig};
use crate::pipeline::{BuildMode, hash_file};

pub const PROFILE_VERSION: &str = "cloudflare-worker-wasm-v1";
pub const COMPATIBILITY_DATE: &str = "2026-10-27";

#[derive(Clone, Debug)]
pub struct BuildReport {
    pub artifacts: Vec<PathBuf>,
    pub js_runtime_hash: String,
    pub wasm_hash: String,
    pub target_lock: PathBuf,
    pub manifest: PathBuf,
}

pub fn is_target(target: &TargetConfig) -> bool {
    target.client == "web" && target.server == "wasm" && target.deployment == "cloudflare-workers"
}

/// Generates a self-contained Wrangler project. The only deployment tool is Wrangler itself;
/// the application has no npm runtime dependencies and needs no bundling step.
pub fn generate(
    app: &VerifiedApplication,
    config: &ProjectConfig,
    output_override: Option<&Path>,
) -> Result<PathBuf, String> {
    let target_config = config
        .targets
        .get(&app.target)
        .ok_or_else(|| format!("Target `{}` disappeared from bind.toml", app.target))?;
    if !is_target(target_config) {
        return Err(format!(
            "Target `{}` is not the Cloudflare Workers WebAssembly profile",
            app.target
        ));
    }
    let target = output_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config.output.join(&app.target));
    let web = web_build(app, config, target_config)?;
    let cors = crate::backend::js::plan::cors_origin(target_config);
    let wasm = crate::backend::wasm::compile(&app.core)?;
    let mut files = vec![
        (
            "worker.js".to_string(),
            crate::backend::js::worker::render(&app.core, cors),
        ),
        (
            "bind-runtime.js".to_string(),
            crate::backend::js::RUNTIME.to_string(),
        ),
        ("wrangler.jsonc".to_string(), wrangler(app)),
        (
            "package.json".to_string(),
            format!(
                "{{\n  \"name\": {},\n  \"private\": true,\n  \"type\": \"module\"\n}}\n",
                json(&worker_name(&app.project_name, &app.target))
            ),
        ),
        (
            "README.md".to_string(),
            generated_readme(target_config, web.is_static),
        ),
        (
            "bind-manifest.json".to_string(),
            manifest(app, target_config, "generated", false, &[], None, None),
        ),
    ];
    for (path, content) in web.files {
        let public = path
            .strip_prefix("web/")
            .map(|path| format!("public/{path}"))
            .ok_or_else(|| format!("Unexpected web artifact `{path}`"))?;
        files.push((public, content));
    }

    // Compute every file before replacing the previous output. A generation failure therefore
    // leaves the last successful project available.
    if target.exists() {
        fs::remove_dir_all(&target)
            .map_err(|error| format!("Cannot replace {}: {error}", target.display()))?;
    }
    for (path, content) in files {
        let path = target.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("Cannot create {}: {error}", parent.display()))?;
        }
        fs::write(&path, content)
            .map_err(|error| format!("Cannot write {}: {error}", path.display()))?;
    }
    fs::write(target.join("app.wasm"), wasm)
        .map_err(|error| format!("Cannot write {}/app.wasm: {error}", target.display()))?;
    Ok(target)
}

pub fn build(
    generated: &Path,
    app: &VerifiedApplication,
    config: &ProjectConfig,
    target: &TargetConfig,
    mode: BuildMode,
) -> Result<BuildReport, String> {
    if mode == BuildMode::Certify {
        ensure_node()?;
        for relative in javascript_files(generated)? {
            let status = Command::new("node")
                .args(["--check", relative.to_string_lossy().as_ref()])
                .current_dir(generated)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .map_err(|error| format!("Cannot validate {}: {error}", relative.display()))?;
            if !status.status.success() {
                return Err(format!(
                    "Generated JavaScript {} is invalid:\n{}",
                    relative.display(),
                    String::from_utf8_lossy(&status.stderr)
                ));
            }
        }
        validate_wasm(generated)?;
    }

    let artifacts = deploy_artifacts(generated)?;
    for artifact in &artifacts {
        let metadata = fs::metadata(artifact)
            .map_err(|error| format!("Expected artifact {}: {error}", artifact.display()))?;
        if metadata.len() == 0 {
            return Err(format!("Artifact {} is empty", artifact.display()));
        }
    }
    let runtime = generated.join("bind-runtime.js");
    let js_runtime_hash = hash_file(&runtime)?;
    let wasm_hash = hash_file(&generated.join("app.wasm"))?;
    let target_lock_dir = config.root.join(".bind/locks");
    fs::create_dir_all(&target_lock_dir)
        .map_err(|error| format!("Cannot create {}: {error}", target_lock_dir.display()))?;
    let target_lock = target_lock_dir.join(format!("{}.lock", app.target));
    let artifact_hashes = artifacts
        .iter()
        .map(|path| {
            let relative = path.strip_prefix(generated).unwrap_or(path);
            Ok((relative.to_string_lossy().to_string(), hash_file(path)?))
        })
        .collect::<Result<Vec<_>, String>>()?;
    fs::write(
        &target_lock,
        dependency_lock(target, &js_runtime_hash, &wasm_hash, &artifact_hashes),
    )
    .map_err(|error| format!("Cannot write {}: {error}", target_lock.display()))?;

    let manifest_path = generated.join("bind-manifest.json");
    let state = if mode == BuildMode::Certify {
        "certified-host"
    } else {
        "built-host"
    };
    fs::write(
        &manifest_path,
        manifest(
            app,
            target,
            state,
            mode == BuildMode::Certify,
            &artifact_hashes,
            Some(&js_runtime_hash),
            Some(&wasm_hash),
        ),
    )
    .map_err(|error| format!("Cannot write {}: {error}", manifest_path.display()))?;

    Ok(BuildReport {
        artifacts,
        js_runtime_hash,
        wasm_hash,
        target_lock,
        manifest: manifest_path,
    })
}

fn ensure_node() -> Result<(), String> {
    let status = Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| "Cloudflare certification requires `node` on PATH".to_string())?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "Cloudflare certification requires a usable `node`".to_string())
}

fn validate_wasm(root: &Path) -> Result<(), String> {
    let output = Command::new("node")
        .args([
            "-e",
            "const fs=require('fs');new WebAssembly.Module(fs.readFileSync('app.wasm'));",
        ])
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("Cannot validate app.wasm: {error}"))?;
    output.status.success().then_some(()).ok_or_else(|| {
        format!(
            "Generated WebAssembly app.wasm is invalid:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn javascript_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    Ok(deploy_artifacts(root)?
        .into_iter()
        .filter_map(|path| {
            (path.extension().is_some_and(|extension| extension == "js"))
                .then(|| path.strip_prefix(root).unwrap_or(&path).to_path_buf())
        })
        .collect())
}

fn deploy_artifacts(root: &Path) -> Result<Vec<PathBuf>, String> {
    fn visit(root: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
        for entry in fs::read_dir(root)
            .map_err(|error| format!("Cannot read {}: {error}", root.display()))?
        {
            let path = entry
                .map_err(|error| format!("Cannot read {}: {error}", root.display()))?
                .path();
            if path.is_dir() {
                visit(&path, output)?;
            } else if !matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("bind-manifest.json" | "README.md")
            ) {
                output.push(path);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    visit(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn worker_name(project: &str, target: &str) -> String {
    let mut name = format!("{project}-{target}")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while name.contains("--") {
        name = name.replace("--", "-");
    }
    name.trim_matches('-').chars().take(63).collect()
}

fn wrangler(app: &VerifiedApplication) -> String {
    format!(
        "{{\n  \"name\": {},\n  \"main\": \"worker.js\",\n  \"compatibility_date\": \"{COMPATIBILITY_DATE}\",\n  \"assets\": {{\n    \"directory\": \"./public\",\n    \"binding\": \"ASSETS\",\n    \"run_worker_first\": [\"/api\", \"/api/*\"]\n  }}\n}}\n",
        json(&worker_name(&app.project_name, &app.target))
    )
}

fn generated_readme(target: &TargetConfig, static_page: bool) -> String {
    format!(
        "# Generated Bind application\n\nProfile: `{PROFILE_VERSION}`  \nMode: `{}`  \nDeployment: `cloudflare-workers`\n\nThis project was generated directly from typed Bind IR. Bind server computation is in `app.wasm`; `worker.js` is the HTTP/JSON/Cloudflare host. It contains no generated Rust, Cargo workspace, LLVM output, `wasm-bindgen`, npm runtime dependency, or external Wasm compiler.{}\n\nValidate locally with `bind certify --target <target>`, then deploy from this directory with `npx wrangler deploy`.\n",
        target.mode_name(),
        if static_page {
            " The page is pre-rendered under `public/`; the Worker handles `/api/*` and delegates assets to Cloudflare."
        } else {
            " The browser module is under `public/`; the Worker handles `/api/*` and delegates assets to Cloudflare."
        }
    )
}

fn dependency_lock(
    target: &TargetConfig,
    runtime_hash: &str,
    wasm_hash: &str,
    artifacts: &[(String, String)],
) -> String {
    let entries = artifacts
        .iter()
        .map(|(path, hash)| format!("{} = \"{hash}\"", json(path)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "# Generated by bindc. Do not edit.\n\n[bind]\nlanguage = \"0.1.0\"\nprofile = \"{PROFILE_VERSION}\"\n\n[target]\nclient = \"web\"\nserver = \"wasm\"\nmode = \"{}\"\ndeployment = \"{}\"\n\n[toolchain]\ncloudflareCompatibilityDate = \"{COMPATIBILITY_DATE}\"\nwasmBackend = \"bindc-direct-v1\"\n\n[resolution]\njsRuntimeSha256 = \"{runtime_hash}\"\nwasmSha256 = \"{wasm_hash}\"\n\n[artifacts]\n{entries}\n",
        target.mode_name(),
        target.deployment,
    )
}

fn manifest(
    app: &VerifiedApplication,
    target: &TargetConfig,
    state: &str,
    certified: bool,
    artifacts: &[(String, String)],
    runtime_hash: Option<&str>,
    wasm_hash: Option<&str>,
) -> String {
    let environment = app
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
                item.public,
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    let artifact_entries = artifacts
        .iter()
        .map(|(path, hash)| format!("    {{\"path\":{},\"sha256\":{}}}", json(path), json(hash)))
        .collect::<Vec<_>>()
        .join(",\n");
    let runtime = match (runtime_hash, wasm_hash) {
        (Some(js), Some(wasm)) => format!(
            "  \"jsRuntimeSha256\": {},\n  \"wasmSha256\": {},\n",
            json(js),
            json(wasm)
        ),
        (Some(js), None) => format!("  \"jsRuntimeSha256\": {},\n", json(js)),
        _ => String::new(),
    };
    format!(
        "{{\n  \"languageVersion\": \"0.1.0\",\n  \"compilerVersion\": \"bindc-0.1.0\",\n  \"profile\": \"{PROFILE_VERSION}\",\n  \"certified\": {certified},\n  \"certificationState\": {},\n  \"project\": {},\n  \"target\": {},\n  \"mode\": {},\n  \"deployment\": \"cloudflare-workers\",\n  \"sourceHash\": {},\n  \"schemaFingerprint\": {},\n{runtime}  \"wireVersion\": {},\n  \"apiUrl\": {},\n  \"frontendOrigin\": {},\n  \"environment\": [\n{environment}\n  ],\n  \"databaseOperations\": [],\n  \"unsafeOperations\": {},\n  \"artifacts\": [\n{artifact_entries}\n  ]\n}}\n",
        json(state),
        json(&app.project_name),
        json(&app.target),
        json(target.mode_name()),
        json(&app.source_hash),
        json(&app.database.fingerprint),
        json(crate::certified::wire_version()),
        json(&target.api_url),
        target
            .frontend_origin
            .as_deref()
            .map(json)
            .unwrap_or_else(|| "null".to_string()),
        json_array(&app.unsafe_operations),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn worker_names_are_cloudflare_safe() {
        assert_eq!(
            worker_name("My App", "Cloudflare Workers"),
            "my-app-cloudflare-workers"
        );
    }

    #[test]
    fn a_worker_project_generates_and_builds_without_cargo() {
        let suffix = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "bind-cloudflare-test-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/app.bd"),
            "server fun double(value: Double): Double effects [] = value * 2.0\nclient component Home() {\n val answer = server.double(21.0)\n UI { Text(answer) }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("bind.toml"),
            "[project]\nname = \"cloudflare-test\"\nsource = \"src\"\noutput = \"out\"\n[target.worker]\nclient = \"web\"\nserver = \"wasm\"\nmode = \"split\"\ndeployment = \"cloudflare-workers\"\ncertified = true\ncapabilities = [\"ui.dom\", \"server.http\"]\n",
        )
        .unwrap();
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        let result = crate::compiler::verify(&config, "worker");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let app = result.application.unwrap();
        let generated = generate(&app, &config, None).unwrap();
        assert!(generated.join("worker.js").is_file());
        assert!(generated.join("app.wasm").is_file());
        assert!(generated.join("public/app.js").is_file());
        assert!(!generated.join("Cargo.toml").exists());
        assert!(!generated.join("src").exists());
        let report = build(
            &generated,
            &app,
            &config,
            &config.targets["worker"],
            BuildMode::Build,
        )
        .unwrap();
        assert!(
            report
                .artifacts
                .iter()
                .any(|path| path.ends_with("worker.js"))
        );
        assert!(
            fs::read_to_string(report.manifest)
                .unwrap()
                .contains("\"profile\": \"cloudflare-worker-wasm-v1\"")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
