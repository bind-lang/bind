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
use crate::config::{Backend, ProjectConfig, TargetConfig};
use crate::pipeline::{BuildMode, BuildOptions, LockFiles, hash_file, relative_name, write_lock};

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
    target.backend() == Backend::CloudflareWasm
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
        .unwrap_or_else(|| config.target_output(&app.target));
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
    let mut files: Vec<(String, Vec<u8>)> = files
        .into_iter()
        .map(|(path, content)| (path, content.into_bytes()))
        .collect();
    files.push(("app.wasm".to_string(), wasm));
    crate::pipeline::write_workspace(config, &target, &files)?;
    Ok(target)
}

/// Files a build writes next to the generated project that are not deployed: never hashed.
const NOT_DEPLOYED: &[&str] = &[
    crate::pipeline::WORKSPACE_MARKER,
    "README.md",
    crate::compiler::EMITTED_IR,
    crate::compiler::EMITTED_GRAPH,
];

pub fn build(
    generated: &Path,
    app: &VerifiedApplication,
    config: &ProjectConfig,
    target: &TargetConfig,
    options: BuildOptions,
) -> Result<BuildReport, String> {
    let mode = options.mode;
    let locks = LockFiles::new(config, &app.target);
    let workspace_lock = crate::certified::workspace_lock(config);
    locks.check(&locks.workspace, &workspace_lock, options)?;
    locks.require_present(&locks.target, options)?;

    // `certify` requires node; `build` validates with it whenever it is installed.
    let node = node_available();
    if mode == BuildMode::Certify && !node {
        return Err("Cloudflare certification requires a usable `node` on PATH".to_string());
    }
    if node {
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
    let artifact_hashes = artifacts
        .iter()
        .map(|path| Ok((relative_name(generated, path), hash_file(path)?)))
        .collect::<Result<Vec<_>, String>>()?;
    // The lock records what the build depends on (profile, host ABI, compiler-owned runtime),
    // not the application's own artifacts, which change with every source edit and are hashed
    // in `bind-manifest.json`.
    let target_lock = dependency_lock(target, &js_runtime_hash);
    let write_target = locks.check(&locks.target, &target_lock, options)?;
    let write_workspace = locks.check(&locks.workspace, &workspace_lock, options)?;
    if write_workspace {
        write_lock(&locks.workspace, &workspace_lock)?;
    }
    if write_target {
        write_lock(&locks.target, &target_lock)?;
    }
    let target_lock = locks.target;

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

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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
    Ok(crate::pipeline::files_under(root)?
        .into_iter()
        .filter(|path| {
            path.parent() != Some(root)
                || !path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| NOT_DEPLOYED.contains(&name))
        })
        .collect())
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

fn dependency_lock(target: &TargetConfig, runtime_hash: &str) -> String {
    format!(
        "# Generated by bindc. Do not edit.\n\n[bind]\nlanguage = \"0.1.0\"\nprofile = \"{PROFILE_VERSION}\"\n\n[target]\nclient = \"web\"\nserver = \"wasm\"\nmode = \"{}\"\ndeployment = \"{}\"\n\n[toolchain]\ncloudflareCompatibilityDate = \"{COMPATIBILITY_DATE}\"\nwasmBackend = \"bindc-direct-v1\"\n\n[resolution]\njsRuntimeSha256 = \"{runtime_hash}\"\n",
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
            BuildOptions {
                mode: BuildMode::Build,
                update_lock: false,
            },
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
        // The build recorded the locks; the target lock holds no application artifact hash.
        let lock = fs::read_to_string(&report.target_lock).unwrap();
        assert!(
            lock.contains("jsRuntimeSha256") && !lock.contains("worker.js"),
            "{lock}"
        );
        assert!(root.join("bind.lock").is_file());
        // Emitted IR and graph files are never deployable artifacts.
        fs::write(generated.join(crate::compiler::EMITTED_IR), "{}").unwrap();
        fs::write(generated.join(crate::compiler::EMITTED_GRAPH), "").unwrap();
        let names: Vec<String> = deploy_artifacts(&generated)
            .unwrap()
            .iter()
            .map(|path| relative_name(&generated, path))
            .collect();
        assert!(names.contains(&"wrangler.jsonc".to_string()));
        assert!(
            !names
                .iter()
                .any(|name| name.ends_with(".ir.json") || name.ends_with("graph.txt")),
            "{names:?}"
        );
        // certify now accepts the committed locks; a changed lock is refused without
        // --update-lock and accepted with it.
        let certify = BuildOptions {
            mode: BuildMode::Certify,
            update_lock: false,
        };
        if node_available() {
            build(
                &generated,
                &app,
                &config,
                &config.targets["worker"],
                certify,
            )
            .unwrap();
            fs::write(&report.target_lock, "tampered").unwrap();
            let error = build(
                &generated,
                &app,
                &config,
                &config.targets["worker"],
                certify,
            )
            .unwrap_err();
            assert!(error.contains("does not match"), "{error}");
            let update = BuildOptions {
                update_lock: true,
                ..certify
            };
            build(&generated, &app, &config, &config.targets["worker"], update).unwrap();
            assert_eq!(fs::read_to_string(&report.target_lock).unwrap(), lock);
        }
        // Regenerating replaces the previous project; a foreign directory is never replaced.
        generate(&app, &config, None).unwrap();
        let foreign = root.join("foreign");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("important.txt"), "keep").unwrap();
        assert!(generate(&app, &config, Some(&foreign)).is_err());
        assert!(foreign.join("important.txt").is_file());
        assert!(generate(&app, &config, Some(&root)).is_err());
        assert!(root.join("bind.toml").is_file());
        fs::remove_dir_all(root).unwrap();
    }
}
