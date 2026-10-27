use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use crate::certified;
use crate::compiler::{VerifiedApplication, json, json_array};
use crate::config::{ProjectConfig, TargetConfig};

/// The Rust toolchain of the certified profile.
pub const RUST_TOOLCHAIN: &str = "1.98.0";

/// The file that marks a directory as a Bind-generated workspace, which generation may replace.
pub const WORKSPACE_MARKER: &str = "bind-manifest.json";

/// Environment variables that would change how Cargo compiles the generated crate: they are
/// removed from every Cargo invocation so the certified build depends only on the toolchain,
/// the generated sources and the locked dependency graph.
const CARGO_ENVIRONMENT_OVERRIDES: &[&str] = &[
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_ENCODED_RUSTDOCFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
    "CARGO_BUILD_TARGET",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildMode {
    Build,
    Certify,
}

/// How a build treats the committed locks (`bind.lock`, `.bind/locks/<target>.lock` and
/// `.bind/locks/<target>.Cargo.lock`).
#[derive(Clone, Copy, Debug)]
pub struct BuildOptions {
    pub mode: BuildMode,
    /// `--update-lock`: accept a new resolution and rewrite the locks. Without it a lock that
    /// differs is an error, and `certify` also refuses a missing lock.
    pub update_lock: bool,
}

#[derive(Clone, Debug)]
pub struct BuildReport {
    pub rust_artifacts: Vec<PathBuf>,
    /// The files of `web/` (no build step: they are deployed as generated).
    pub web_artifacts: Vec<PathBuf>,
    /// The deployment descriptors (`template.yaml`, `vercel.json`), hashed with the artifacts.
    pub deployment_descriptors: Vec<PathBuf>,
    pub cargo_lock_hash: String,
    /// The hash of `web/bind-runtime.js`, when the page ships it (the split mode's dynamic page).
    pub js_runtime_hash: Option<String>,
    pub target_lock: PathBuf,
    pub manifest: PathBuf,
}

/// The deployment descriptors a generated Rust workspace may contain.
const DEPLOYMENT_DESCRIPTORS: &[&str] = &["template.yaml", "vercel.json"];

pub fn build(
    generated: &Path,
    app: &VerifiedApplication,
    config: &ProjectConfig,
    target: &TargetConfig,
    options: BuildOptions,
) -> Result<BuildReport, String> {
    let mode = options.mode;
    let locks = LockFiles::new(config, &app.target);
    // Configuration-only locks are checked before anything is compiled.
    let workspace_lock = certified::workspace_lock(config);
    locks.check(&locks.workspace, &workspace_lock, options)?;
    locks.require_present(&locks.target, options)?;
    locks.require_present(&locks.cargo, options)?;

    let environment = CargoEnvironment::new(config, app);
    ensure_tool(
        environment.command("cargo", generated).arg("--version"),
        "cargo",
    )?;
    ensure_version(
        environment.command("rustc", generated).arg("--version"),
        "rustc",
        &format!("rustc {RUST_TOOLCHAIN} "),
    )?;
    ensure_version(
        environment.command("cargo", generated).arg("--version"),
        "cargo",
        &format!("cargo {RUST_TOOLCHAIN} "),
    )?;

    let generated_lock = generated.join("Cargo.lock");
    if !generated_lock.exists() {
        run(
            "resolve the pinned Rust dependency graph",
            environment
                .command("cargo", generated)
                .arg("generate-lockfile"),
        )?;
    } else if options.update_lock {
        run(
            "update the locked Rust dependency graph",
            environment
                .command("cargo", generated)
                .args(["update", "--workspace"]),
        )?;
    } else {
        run(
            &format!(
                "fetch the locked Rust dependency graph ({} no longer matches the generated Cargo.toml? Run with --update-lock to accept a new resolution and commit the lock)",
                locks.cargo.display()
            ),
            environment
                .command("cargo", generated)
                .args(["fetch", "--locked"]),
        )?;
    }
    run(
        "format generated Rust",
        environment
            .command("cargo", generated)
            .args(["fmt", "--all"]),
    )?;
    let cargo_target = config.root.join(".bind/cargo-target");
    fs::create_dir_all(&cargo_target)
        .map_err(|error| format!("Cannot create shared Rust build cache: {error}"))?;
    let cargo_target = fs::canonicalize(&cargo_target)
        .map_err(|error| format!("Cannot resolve shared Rust build cache: {error}"))?;
    // The build cache is shared by every target: the lock is held from the first compilation
    // until the staged binaries are hashed, so a concurrent build of another target cannot
    // replace `release/<binary>` in between.
    let _cache_lock = CacheLock::acquire(&cargo_target)?;
    run(
        "compile and test generated Rust",
        environment
            .command("cargo", generated)
            .args(["test", "--locked", "--all-targets"])
            .env("CARGO_TARGET_DIR", &cargo_target),
    )?;
    if mode == BuildMode::Certify {
        run(
            "lint generated Rust without warnings",
            environment
                .command("cargo", generated)
                .args([
                    "clippy",
                    "--locked",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings",
                ])
                .env("CARGO_TARGET_DIR", &cargo_target),
        )?;
    }
    let cargo_toml = fs::read_to_string(generated.join("Cargo.toml"))
        .map_err(|error| format!("Cannot read the generated Cargo.toml: {error}"))?;
    let binaries = binaries(&cargo_toml);
    let mut rust_build = environment.command("cargo", generated);
    rust_build.args(["build", "--release", "--locked"]);
    rust_build.arg(if binaries.is_empty() {
        "--lib"
    } else {
        "--bins"
    });
    rust_build.env("CARGO_TARGET_DIR", &cargo_target);
    run("build generated Rust release artifacts", &mut rust_build)?;

    let web_artifacts = files_under(&generated.join("web"))?;
    for artifact in &web_artifacts {
        require_non_empty(artifact)?;
    }
    let rust_artifacts = stage_rust_artifacts(generated, &cargo_target, &binaries)?;
    for artifact in &rust_artifacts {
        require_non_empty(artifact)?;
    }
    let deployment_descriptors: Vec<PathBuf> = DEPLOYMENT_DESCRIPTORS
        .iter()
        .map(|name| generated.join(name))
        .filter(|path| path.is_file())
        .collect();

    let cargo_lock = fs::read_to_string(&generated_lock)
        .map_err(|error| format!("Cannot read {}: {error}", generated_lock.display()))?;
    let cargo_lock_hash = hash_bytes(cargo_lock.as_bytes());
    let js_runtime = generated.join("web/bind-runtime.js");
    let js_runtime_hash = js_runtime
        .exists()
        .then(|| hash_file(&js_runtime))
        .transpose()?;
    let target_lock =
        certified::dependency_lock(target, Some(&cargo_lock_hash), js_runtime_hash.as_deref());
    // Every lock is checked before any is written: a refused build changes none of them.
    let write_cargo = locks.check(&locks.cargo, &cargo_lock, options)?;
    let write_target = locks.check(&locks.target, &target_lock, options)?;
    let write_workspace = locks.check(&locks.workspace, &workspace_lock, options)?;
    if write_workspace {
        write_lock(&locks.workspace, &workspace_lock)?;
    }
    if write_cargo {
        write_lock(&locks.cargo, &cargo_lock)?;
    }
    if write_target {
        write_lock(&locks.target, &target_lock)?;
    }

    let manifest = generated.join(WORKSPACE_MARKER);
    let artifact_entries = rust_artifacts
        .iter()
        .chain(&web_artifacts)
        .chain(&deployment_descriptors)
        .map(|path| {
            Ok(format!(
                "    {{\"path\":{},\"sha256\":{}}}",
                json(&relative_name(generated, path)),
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
        deployment_descriptors,
        cargo_lock_hash,
        js_runtime_hash,
        target_lock: locks.target,
        manifest,
    })
}

/// The committed locks of one target.
pub(crate) struct LockFiles {
    /// `bind.lock`: the profile, toolchain, pinned dependencies and every target.
    pub workspace: PathBuf,
    /// `.bind/locks/<target>.lock`: the target's resolution hashes.
    pub target: PathBuf,
    /// `.bind/locks/<target>.Cargo.lock`: the complete transitive Rust graph.
    pub cargo: PathBuf,
}

impl LockFiles {
    pub fn new(config: &ProjectConfig, target: &str) -> Self {
        // Target names are restricted to `[a-z0-9][a-z0-9_-]*` by the manifest parser, so these
        // paths always stay inside the lock directory.
        debug_assert!(crate::config::valid_target_name(target));
        let directory = config.lock_directory();
        Self {
            workspace: config.root.join("bind.lock"),
            target: directory.join(format!("{target}.lock")),
            cargo: directory.join(format!("{target}.Cargo.lock")),
        }
    }

    /// `certify` without `--update-lock` refuses to start without a committed lock.
    pub fn require_present(&self, path: &Path, options: BuildOptions) -> Result<(), String> {
        if options.mode == BuildMode::Certify && !options.update_lock && !path.is_file() {
            return Err(missing_lock(path));
        }
        Ok(())
    }

    /// Whether `path` must be (re)written with `expected`. A lock that differs is an error unless
    /// `--update-lock`; a missing one is created by `build` and refused by `certify`.
    pub fn check(
        &self,
        path: &Path,
        expected: &str,
        options: BuildOptions,
    ) -> Result<bool, String> {
        match fs::read_to_string(path) {
            Ok(actual) if actual == expected => Ok(false),
            Ok(_) if options.update_lock => Ok(true),
            Ok(_) => Err(format!(
                "{} does not match this build: the locked resolution changed. Review the change, then run with --update-lock to accept it and commit the new lock.",
                path.display()
            )),
            Err(_) if options.update_lock || options.mode == BuildMode::Build => Ok(true),
            Err(_) => Err(missing_lock(path)),
        }
    }
}

fn missing_lock(path: &Path) -> String {
    format!(
        "{} is missing: `certify` only accepts a committed lock. Run `bind build` (or `bind certify --update-lock`) once and commit the lock files.",
        path.display()
    )
}

pub(crate) fn write_lock(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot create {}: {error}", parent.display()))?;
    }
    fs::write(path, content).map_err(|error| format!("Cannot write {}: {error}", path.display()))
}

/// The environment of the Cargo and rustc processes: build-changing overrides and the
/// application's secrets removed.
struct CargoEnvironment {
    removed: Vec<String>,
}

impl CargoEnvironment {
    fn new(config: &ProjectConfig, app: &VerifiedApplication) -> Self {
        let mut removed: Vec<String> = CARGO_ENVIRONMENT_OVERRIDES
            .iter()
            .map(|name| name.to_string())
            .collect();
        removed.extend(secret_variables(config, app));
        Self { removed }
    }

    fn command(&self, program: &str, directory: &Path) -> Command {
        let mut command = Command::new(program);
        command.current_dir(directory);
        for name in &self.removed {
            command.env_remove(name);
        }
        command
    }
}

/// The variables of the application's secrets (and the database credential): never passed to
/// Cargo, the build scripts of dependencies or the generated tests.
fn secret_variables(config: &ProjectConfig, app: &VerifiedApplication) -> Vec<String> {
    let mut names: Vec<String> = app
        .environment
        .iter()
        .filter(|item| item.secret)
        .map(|item| item.variable.clone())
        .collect();
    names.extend(config.database_url_env.clone());
    names.sort();
    names.dedup();
    names
}

/// An exclusive lock on the shared Rust build cache, released when dropped.
struct CacheLock(fs::File);

impl CacheLock {
    fn acquire(cargo_target: &Path) -> Result<Self, String> {
        let path = cargo_target.join(".bind-build.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|error| format!("Cannot open {}: {error}", path.display()))?;
        file.lock()
            .map_err(|error| format!("Cannot lock {}: {error}", path.display()))?;
        Ok(Self(file))
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// `path` relative to `root`, with `/` separators on every platform.
pub(crate) fn relative_name(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
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

/// Every file under `directory` (recursively), sorted by path.
pub(crate) fn files_under(directory: &Path) -> Result<Vec<PathBuf>, String> {
    fn visit(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
        for entry in fs::read_dir(directory)
            .map_err(|error| format!("Cannot read {}: {error}", directory.display()))?
        {
            let entry =
                entry.map_err(|error| format!("Cannot read {}: {error}", directory.display()))?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
            if kind.is_dir() {
                visit(&path, files)?;
            } else if kind.is_file() {
                files.push(path);
            } else {
                return Err(format!(
                    "Unexpected non-regular file {} in a generated workspace",
                    path.display()
                ));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    visit(directory, &mut files)?;
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

fn ensure_tool(command: &mut Command, program: &str) -> Result<(), String> {
    let status = command
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

/// Checks the version of the tool as it runs in the generated workspace, with the environment
/// Cargo gets: a `rust-toolchain.toml` or rustup override there is taken into account.
fn ensure_version(command: &mut Command, program: &str, expected: &str) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|_| format!("Required tool `{program}` is not installed or not on PATH"))?;
    let actual = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() && version_matches(&actual, expected) {
        Ok(())
    } else {
        Err(format!(
            "Certified profile requires `{program}` version `{}`; found `{actual}` in the generated workspace",
            expected.trim()
        ))
    }
}

fn version_matches(actual: &str, expected: &str) -> bool {
    actual == expected.trim() || actual.starts_with(expected)
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

pub(crate) fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

/// Checks a generated file's relative path: `/`-separated normal components only (no `..`,
/// `.`, empty component, absolute path, drive prefix or `\`).
pub(crate) fn check_relative_path(path: &str) -> Result<(), String> {
    let valid = !path.is_empty()
        && !path.contains('\\')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "Internal compiler error: generated file path `{path}` is not a plain relative path. This is a bug in bindc."
        ))
    }
}

/// `path` made absolute and normalized without touching the file system for the missing part:
/// the longest existing ancestor is canonicalized (symlinks resolved), the rest is appended with
/// `.` and `..` resolved lexically.
fn resolved(path: &Path) -> Result<PathBuf, String> {
    // The root of a manifest given as `bind.toml` is the empty path: the current directory.
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    let absolute = std::path::absolute(path)
        .map_err(|error| format!("Cannot resolve {}: {error}", path.display()))?;
    let mut lexical = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            other => lexical.push(other),
        }
    }
    let mut existing = lexical.as_path();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return Ok(lexical),
        }
    }
    let mut result = fs::canonicalize(existing)
        .map_err(|error| format!("Cannot resolve {}: {error}", existing.display()))?;
    for name in rest.iter().rev() {
        result.push(name);
    }
    Ok(result)
}

/// Checks that `target` may be replaced by a generated workspace: it is not the project
/// directory, an ancestor of it, or a directory holding the project's sources, manifest, schema,
/// stylesheet or locks; it is not a symbolic link; and if it exists it is a directory that is
/// empty or was generated by Bind (it contains `bind-manifest.json`).
pub(crate) fn check_replaceable(config: &ProjectConfig, target: &Path) -> Result<PathBuf, String> {
    let resolved_target = resolved(target)?;
    let mut protected = vec![
        config.root.clone(),
        config.manifest.clone(),
        config.source.clone(),
        config.lock_directory(),
    ];
    protected.extend(config.database_schema.clone());
    protected.extend(config.web_stylesheet.clone());
    for path in protected {
        let path = resolved(&path)?;
        if path.starts_with(&resolved_target) {
            return Err(format!(
                "Refusing to generate into {}: it contains {} (choose an output directory of its own, such as .bind/generated/<target>)",
                target.display(),
                path.display()
            ));
        }
    }
    if resolved_target.file_name().is_none() {
        return Err(format!(
            "Refusing to generate into {}: not a directory name",
            target.display()
        ));
    }
    match fs::symlink_metadata(target) {
        Err(_) => {}
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "Refusing to replace {}: it is a symbolic link",
                target.display()
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(format!(
                "Refusing to replace {}: it is not a directory",
                target.display()
            ));
        }
        Ok(_) => {
            let empty = fs::read_dir(target)
                .map_err(|error| format!("Cannot read {}: {error}", target.display()))?
                .next()
                .is_none();
            if !empty && !target.join(WORKSPACE_MARKER).is_file() {
                return Err(format!(
                    "Refusing to replace {}: it is not empty and was not generated by Bind (no {WORKSPACE_MARKER})",
                    target.display()
                ));
            }
        }
    }
    Ok(resolved_target)
}

/// Replaces the generated workspace `target` with `files`. Every path is checked, the target
/// must be replaceable ([`check_replaceable`]), the files are written to a temporary sibling
/// directory, and only then is the previous workspace swapped out. A failure leaves the
/// previous workspace in place.
pub(crate) fn write_workspace(
    config: &ProjectConfig,
    target: &Path,
    files: &[(String, Vec<u8>)],
) -> Result<(), String> {
    for (path, _) in files {
        check_relative_path(path)?;
    }
    if !files.iter().any(|(path, _)| path == WORKSPACE_MARKER) {
        return Err(format!(
            "Internal compiler error: a generated workspace has no {WORKSPACE_MARKER}. This is a bug in bindc."
        ));
    }
    let resolved_target = check_replaceable(config, target)?;
    let (Some(parent), Some(name)) = (resolved_target.parent(), resolved_target.file_name()) else {
        return Err(format!("Cannot generate into {}", target.display()));
    };
    fs::create_dir_all(parent)
        .map_err(|error| format!("Cannot create {}: {error}", parent.display()))?;
    let name = name.to_string_lossy();
    let staging = parent.join(format!(".{name}.bind-new-{}", std::process::id()));
    let previous = parent.join(format!(".{name}.bind-old-{}", std::process::id()));
    for leftover in [&staging, &previous] {
        if fs::symlink_metadata(leftover).is_ok() {
            fs::remove_dir_all(leftover)
                .map_err(|error| format!("Cannot remove {}: {error}", leftover.display()))?;
        }
    }
    let written = (|| {
        for (path, content) in files {
            let path = staging.join(path);
            if let Some(directory) = path.parent() {
                fs::create_dir_all(directory)
                    .map_err(|error| format!("Cannot create {}: {error}", directory.display()))?;
            }
            fs::write(&path, content)
                .map_err(|error| format!("Cannot write {}: {error}", path.display()))?;
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = written {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    let had_previous = resolved_target.exists();
    if had_previous {
        fs::rename(&resolved_target, &previous).map_err(|error| {
            let _ = fs::remove_dir_all(&staging);
            format!("Cannot replace {}: {error}", target.display())
        })?;
    }
    if let Err(error) = fs::rename(&staging, &resolved_target) {
        if had_previous {
            let _ = fs::rename(&previous, &resolved_target);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("Cannot replace {}: {error}", target.display()));
    }
    if had_previous {
        fs::remove_dir_all(&previous)
            .map_err(|error| format!("Cannot remove {}: {error}", previous.display()))?;
    }
    Ok(())
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

    #[test]
    fn generated_paths_must_be_plain_relative_paths() {
        for good in ["a", "web/app.js", "api/getCustomer.rs", ".bind-x"] {
            assert!(check_relative_path(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "api/../../../pwned.rs",
            "a/./b",
            "a//b",
            "a\\b",
            "a/",
            "..",
        ] {
            assert!(check_relative_path(bad).is_err(), "{bad}");
        }
    }

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> (Scratch, ProjectConfig) {
        let root =
            std::env::temp_dir().join(format!("bindc-pipeline-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("project/src")).unwrap();
        fs::write(root.join("project/src/app.bd"), "").unwrap();
        fs::write(
            root.join("project/bind.toml"),
            "[project]\noutput = \"out\"\n[target.t]\nclient = \"web\"\n",
        )
        .unwrap();
        let config = ProjectConfig::load(&root.join("project/bind.toml")).unwrap();
        (Scratch(root), config)
    }

    fn workspace(content: &str) -> Vec<(String, Vec<u8>)> {
        vec![
            (WORKSPACE_MARKER.to_string(), b"{}".to_vec()),
            ("web/index.html".to_string(), content.as_bytes().to_vec()),
        ]
    }

    #[test]
    fn a_workspace_never_replaces_the_project_or_a_foreign_directory() {
        let (scratch, config) = scratch("replace");
        let root = &scratch.0;
        let project = root.join("project");
        // The project, its ancestors, its sources and its lock directory are refused.
        for target in [
            project.clone(),
            project.join("."),
            project.join("out/.."),
            root.clone(),
            project.join("src"),
            project.join(".bind"),
            project.join(".bind/locks"),
        ] {
            let error = write_workspace(&config, &target, &workspace("x")).unwrap_err();
            assert!(error.contains("Refusing"), "{}: {error}", target.display());
        }
        assert!(project.join("bind.toml").is_file());
        assert!(project.join("src/app.bd").is_file());
        // A directory with foreign files is not replaced.
        let victim = root.join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("important.txt"), "keep").unwrap();
        let error = write_workspace(&config, &victim, &workspace("x")).unwrap_err();
        assert!(error.contains("not generated by Bind"), "{error}");
        assert_eq!(
            fs::read_to_string(victim.join("important.txt")).unwrap(),
            "keep"
        );
        // A new directory, an empty one and a previous workspace are written and replaced.
        let output = project.join("out/t");
        write_workspace(&config, &output, &workspace("one")).unwrap();
        fs::write(output.join("stale.txt"), "old").unwrap();
        write_workspace(&config, &output, &workspace("two")).unwrap();
        assert_eq!(
            fs::read_to_string(output.join("web/index.html")).unwrap(),
            "two"
        );
        assert!(!output.join("stale.txt").exists());
        let empty = root.join("empty");
        fs::create_dir_all(&empty).unwrap();
        write_workspace(&config, &empty, &workspace("x")).unwrap();
        // No staging directory is left behind.
        let leftovers: Vec<_> = fs::read_dir(project.join("out"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["t"]);
        // An escaping file path is refused before anything is written.
        let mut files = workspace("x");
        files.push(("api/../../../pwned.rs".to_string(), Vec::new()));
        assert!(write_workspace(&config, &output, &files).is_err());
        assert!(!root.join("pwned.rs").exists() && !project.join("pwned.rs").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_never_replaced() {
        let (scratch, config) = scratch("symlink");
        let elsewhere = scratch.0.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join(WORKSPACE_MARKER), "{}").unwrap();
        let link = scratch.0.join("project/out/t");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        let error = write_workspace(&config, &link, &workspace("x")).unwrap_err();
        assert!(error.contains("symbolic link"), "{error}");
        assert!(elsewhere.join(WORKSPACE_MARKER).is_file());
    }

    #[test]
    fn locks_are_enforced_unless_updated() {
        let (scratch, config) = scratch("locks");
        let locks = LockFiles::new(&config, "t");
        let certify = BuildOptions {
            mode: BuildMode::Certify,
            update_lock: false,
        };
        let build = BuildOptions {
            mode: BuildMode::Build,
            update_lock: false,
        };
        let update = BuildOptions {
            mode: BuildMode::Certify,
            update_lock: true,
        };
        // Missing: certify refuses, build and --update-lock create it.
        assert!(locks.require_present(&locks.cargo, certify).is_err());
        assert!(locks.check(&locks.cargo, "a", certify).is_err());
        assert!(locks.check(&locks.cargo, "a", build).unwrap());
        assert!(locks.check(&locks.cargo, "a", update).unwrap());
        write_lock(&locks.cargo, "a").unwrap();
        assert!(
            locks
                .cargo
                .starts_with(scratch.0.join("project/.bind/locks"))
        );
        // Present and equal: nothing to write.
        assert!(locks.require_present(&locks.cargo, certify).is_ok());
        assert!(!locks.check(&locks.cargo, "a", certify).unwrap());
        // Different: refused by build and certify, accepted with --update-lock.
        assert!(locks.check(&locks.cargo, "b", certify).is_err());
        assert!(locks.check(&locks.cargo, "b", build).is_err());
        assert!(locks.check(&locks.cargo, "b", update).unwrap());
    }

    #[test]
    fn web_artifacts_are_collected_recursively() {
        let (scratch, _) = scratch("files");
        let web = scratch.0.join("web");
        fs::create_dir_all(web.join("assets/fonts")).unwrap();
        fs::write(web.join("index.html"), "x").unwrap();
        fs::write(web.join("assets/fonts/a.woff2"), "x").unwrap();
        let names: Vec<String> = files_under(&web)
            .unwrap()
            .iter()
            .map(|path| relative_name(&web, path))
            .collect();
        assert_eq!(names, ["assets/fonts/a.woff2", "index.html"]);
    }

    #[test]
    fn versions_match_exactly() {
        assert!(version_matches(
            "rustc 1.98.0 (88d9e12ae 2026-08-18)",
            "rustc 1.98.0 "
        ));
        assert!(!version_matches("rustc 1.98.01 (x)", "rustc 1.98.0 "));
        assert!(version_matches("rustc 1.98.0", "rustc 1.98.0 "));
    }
}
