use std::fs;
use std::path::{Path, PathBuf};

use crate::ast::{TypeDecl, TypeKind, TypeRef};
use crate::backend::js::page::{WebBuild, page_meta, web_build};
use crate::backend::rust::SsrPage;
use crate::compiler::{VerifiedApplication, json, json_array};
use crate::config::{Mode, ProjectConfig, TargetConfig};
use crate::diagnostic::Span;
use crate::parser::expr::parse_type_text;
use crate::syntax::expr::TypeExpr;

const PROFILE_VERSION: &str = "rust-web-v3";
/// JSON contract of the profile: `Int`, `Decimal` and `Money` travel as strings (the format of
/// `bind-json-v2`; the frontend accepts any canonical `Int` of up to 1000 digits).
const WIRE_VERSION: &str = "bind-json-v3";
// Dependencies of the generated server, pinned with `=`. They match the runtime's
// `runtime/bind-runtime/Cargo.toml`, whose source the generated crate compiles.
const SERDE: &str = "1.0.229";
const SERDE_JSON: &str = "1.0.151";
const TOKIO: &str = "1.53.1";
const UUID: &str = "1.28.0";
const SQLX: &str = "0.9.0";
const NUM_BIGINT: &str = "0.4.6";
const BIGDECIMAL: &str = "0.4.8";
const RUST_DECIMAL: &str = "1.37.2";
const CHRONO: &str = "0.4.41";
/// The runtime of the generated server, written to its `src/bind_runtime.rs`.
const RUNTIME_SOURCE: &str = include_str!("../runtime/bind-runtime/src/lib.rs");
const LAMBDA_HTTP: &str = "1.3.1";
const VERCEL_RUNTIME: &str = "2.4.0";
const HTTP_BODY_UTIL: &str = "0.1.5";
/// The Vercel function serving the page in the `ssr` mode (`/` is rewritten to it). Its name
/// has a `-`, which no Bind endpoint name has, so it never meets an endpoint's function.
const VERCEL_PAGE_FUNCTION: &str = "bind-page";

/// How the generated application is deployed, derived from the target and its frontend plan
/// (the same plan `verify` checks, so it never fails on a verified program).
#[derive(Clone, Debug)]
struct Layout {
    web: WebBuild,
    /// Whether a Rust server is generated: there are endpoints, or the `ssr` mode renders a
    /// dynamic page. Without one, `web/` is the whole application.
    server: bool,
    /// The `ssr` mode with a dynamic page: the server renders it at `GET /`.
    ssr_page: bool,
}

pub fn generate(
    app: &VerifiedApplication,
    config: &ProjectConfig,
    output_override: Option<&Path>,
) -> Result<PathBuf, String> {
    let target_config = config
        .targets
        .get(&app.target)
        .ok_or_else(|| format!("Target `{}` disappeared from bind.toml", app.target))?;
    if target_config.client != "web" || target_config.server != "rust" {
        return Err(format!(
            "Target `{}` is not the certified Rust + web profile",
            app.target
        ));
    }
    if !matches!(target_config.deployment.as_str(), "aws-lambda" | "vercel") {
        return Err(format!(
            "Unsupported certified deployment `{}`",
            target_config.deployment
        ));
    }

    let target = output_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config.output.join(&app.target));
    // A lock resolved for another profile's dependencies is re-resolved instead of kept.
    let same_profile = fs::read_to_string(target.join("bind-manifest.json"))
        .is_ok_and(|manifest| manifest.contains(&format!("\"profile\": \"{PROFILE_VERSION}\"")));
    let previous_cargo_toml = fs::read_to_string(target.join("Cargo.toml")).ok();
    let preserved_cargo_lock = fs::read_to_string(target.join("Cargo.lock"))
        .ok()
        .filter(|_| same_profile);
    // Everything that can fail is computed first: a failure leaves the previous output intact.
    let mut files = workspace_files(app, config, target_config)?;
    let cargo_toml = files
        .iter()
        .find(|(path, _)| path == "Cargo.toml")
        .map(|(_, content)| content.as_str());
    // A lock resolved for other dependencies would fail `--locked`: it is re-resolved.
    let preserved_cargo_lock = preserved_cargo_lock
        .filter(|_| same_dependencies(previous_cargo_toml.as_deref(), cargo_toml));
    if let Some(lock) = preserved_cargo_lock {
        files.push(("Cargo.lock".to_string(), lock));
    }
    if target.exists() {
        fs::remove_dir_all(&target)
            .map_err(|error| format!("Cannot replace {}: {error}", target.display()))?;
    }
    for (path, content) in &files {
        let path = target.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        write(&path, content)?;
    }
    Ok(target)
}

/// Whether two generated `Cargo.toml`s declare the same `[dependencies]` table (binaries and
/// profiles do not affect the lock).
fn same_dependencies(previous: Option<&str>, next: Option<&str>) -> bool {
    fn table(manifest: &str) -> Option<&str> {
        let (_, rest) = manifest.split_once("\n[dependencies]\n")?;
        Some(
            rest.split_once("\n[")
                .map_or(rest, |(table, _)| table)
                .trim(),
        )
    }
    matches!((previous.and_then(table), next.and_then(table)), (Some(a), Some(b)) if a == b)
}

fn layout(
    app: &VerifiedApplication,
    config: &ProjectConfig,
    target: &TargetConfig,
) -> Result<Layout, String> {
    let web = web_build(app, config, target)?;
    let ssr_page = target.mode == Some(Mode::Ssr) && !web.is_static;
    Ok(Layout {
        server: !app.rpc_contracts.is_empty() || ssr_page,
        ssr_page,
        web,
    })
}

/// Every generated file (relative path, content) except the preserved lock file.
///
/// | Deployment | `ssr` | `split` |
/// |---|---|---|
/// | `aws-lambda` | the Lambda serves `/` (the page, or the pre-rendered `web/index.html`), `/styles.css` and `/api/*` | the Lambda serves `/api/*`; `web/` is ready for static hosting |
/// | `vercel` | `web/` static, one function per endpoint and the page function (`/` is rewritten to it) | `web/` static and one function per endpoint, same origin |
///
/// An application without endpoints and with a static page has no server: `web/` is all of it.
fn workspace_files(
    app: &VerifiedApplication,
    config: &ProjectConfig,
    target_config: &TargetConfig,
) -> Result<Vec<(String, String)>, String> {
    let layout = layout(app, config, target_config)?;
    let mut files: Vec<(String, String)> = Vec::new();
    let mut add = |path: &str, content: String| files.push((path.to_string(), content));
    add("Cargo.toml", cargo_toml(app, target_config, &layout));
    let frontend_origin = crate::backend::js::plan::cors_origin(target_config);
    if layout.server {
        let meta = page_meta(config);
        let page = SsrPage {
            component: &layout.web.plan.component,
            title: meta.title,
            description: meta.description,
            language: meta.language,
            stylesheet: meta.stylesheet,
        };
        let library = crate::backend::rust::server_library_with(
            app,
            layout.ssr_page.then_some(&page),
            frontend_origin,
        )?;
        add("src/lib.rs", library);
        add("src/bind_runtime.rs", RUNTIME_SOURCE.to_string());
    } else {
        add("src/lib.rs", STATIC_LIBRARY.to_string());
    }
    let database_environment = config.database_url_env.as_deref().unwrap_or("DATABASE_URL");
    let entry = Entry {
        ssr: target_config.mode == Some(Mode::Ssr),
        ssr_page: layout.ssr_page,
        cors: frontend_origin.is_some(),
    };
    match target_config.deployment.as_str() {
        "aws-lambda" if layout.server => {
            add("src/main.rs", aws_main(database_environment, &entry));
            add(
                "template.yaml",
                aws_template(app, database_environment, entry.ssr),
            );
        }
        "aws-lambda" => {}
        "vercel" => {
            for rpc in &app.rpc_contracts {
                add(
                    &format!("api/{}.rs", rpc.name),
                    vercel_rpc_main(&rpc.path, database_environment, entry.cors),
                );
            }
            if layout.ssr_page {
                add(
                    &format!("api/{VERCEL_PAGE_FUNCTION}.rs"),
                    vercel_page_main(database_environment),
                );
            }
            add("vercel.json", vercel_json(layout.ssr_page));
        }
        other => return Err(format!("Unsupported certified deployment `{other}`")),
    }
    for (path, content) in &layout.web.files {
        add(path, content.clone());
    }
    add("contracts/schema.json", contract_schema(app));
    add("contracts/vectors.json", contract_vectors(app));
    add("bind-manifest.json", generated_manifest(app, target_config));
    add("README.md", generated_readme(target_config, &layout));
    Ok(files)
}

pub fn profile_version() -> &'static str {
    PROFILE_VERSION
}

pub fn wire_version() -> &'static str {
    WIRE_VERSION
}

/// The lock of one target: the profile, the target, the toolchain, every pinned dependency and
/// the hashes of the resolved Cargo lock and, when `web/` ships it, of the JS runtime.
pub fn dependency_lock(
    target: &TargetConfig,
    cargo_lock_hash: Option<&str>,
    js_runtime_hash: Option<&str>,
) -> String {
    let dependencies = locked_dependencies();
    let cargo_lock = cargo_lock_hash.unwrap_or("unresolved");
    let js_runtime = js_runtime_hash
        .map(|hash| format!("jsRuntimeSha256 = \"{hash}\"\n"))
        .unwrap_or_default();
    format!(
        "# Generated by bindc. Do not edit.\n\n[bind]\nlanguage = \"0.1.0\"\nprofile = \"{PROFILE_VERSION}\"\n\n[target]\nclient = \"web\"\nserver = \"rust\"\nmode = \"{}\"\ndeployment = \"{}\"\n\n[toolchain]\nrust = \"1.98.0\"\nrustEdition = \"2024\"\n\n{dependencies}\n[resolution]\ncargoLockSha256 = \"{cargo_lock}\"\n{js_runtime}",
        target.mode_name(),
        target.deployment
    )
}

pub fn workspace_lock(config: &ProjectConfig) -> String {
    let targets = config
        .targets
        .iter()
        .map(|(name, target)| {
            format!(
                "[targets.{name}]\nclient = \"{}\"\nserver = \"{}\"\nmode = \"{}\"\ndeployment = \"{}\"\ncertified = {}\n",
                target.client,
                target.server,
                target.mode_name(),
                target.deployment,
                target.certified
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let dependencies = locked_dependencies();
    format!(
        "# Generated by bindc. Do not edit.\n\n[bind]\nlanguage = \"0.1.0\"\nprofile = \"{PROFILE_VERSION}\"\n\n[toolchain]\nrust = \"1.98.0\"\nrustEdition = \"2024\"\n\n{dependencies}\n{targets}"
    )
}

/// The `[dependencies]` table of `bind.lock` and the target locks: every pinned version.
fn locked_dependencies() -> String {
    format!(
        "[dependencies]\nserde = \"{SERDE}\"\nserdeJson = \"{SERDE_JSON}\"\ntokio = \"{TOKIO}\"\nuuid = \"{UUID}\"\nsqlx = \"{SQLX}\"\nnumBigint = \"{NUM_BIGINT}\"\nbigdecimal = \"{BIGDECIMAL}\"\nrustDecimal = \"{RUST_DECIMAL}\"\nchrono = \"{CHRONO}\"\nlambdaHttp = \"{LAMBDA_HTTP}\"\nvercelRuntime = \"{VERCEL_RUNTIME}\"\nhttpBodyUtil = \"{HTTP_BODY_UTIL}\"\n"
    )
}

/// The `[dependencies]` of a generated server crate shared by every deployment: the
/// runtime's dependencies (same versions and features) plus serde's `rc` (lists are
/// `Arc<Vec<T>>`) and `uuid` with `serde` (generated types hold `Uuid`s).
pub(crate) fn server_dependencies() -> String {
    format!(
        "serde = {{ version = \"={SERDE}\", features = [\"derive\", \"rc\"] }}\nserde_json = \"={SERDE_JSON}\"\ntokio = {{ version = \"={TOKIO}\", features = [\"rt-multi-thread\", \"macros\", \"sync\"] }}\nsqlx = {{ version = \"={SQLX}\", default-features = false, features = [\"runtime-tokio\", \"tls-rustls\", \"postgres\", \"uuid\", \"chrono\", \"bigdecimal\", \"rust_decimal\"] }}\nuuid = {{ version = \"={UUID}\", features = [\"serde\"] }}\nnum-bigint = \"={NUM_BIGINT}\"\nbigdecimal = \"={BIGDECIMAL}\"\nrust_decimal = {{ version = \"={RUST_DECIMAL}\", features = [\"serde-str\"] }}\nchrono = {{ version = \"={CHRONO}\", features = [\"serde\"] }}\n"
    )
}

fn write(path: &Path, content: &str) -> Result<(), String> {
    fs::write(path, content).map_err(|error| format!("Cannot write {}: {error}", path.display()))
}

/// The binaries of a generated crate: the Lambda's `bootstrap`, or one Vercel function per
/// endpoint plus the page function. None without a server.
fn binaries(
    app: &VerifiedApplication,
    target: &TargetConfig,
    layout: &Layout,
) -> Vec<(String, String)> {
    if !layout.server {
        return Vec::new();
    }
    if target.deployment == "aws-lambda" {
        return vec![("bootstrap".to_string(), "src/main.rs".to_string())];
    }
    let mut bins: Vec<(String, String)> = app
        .rpc_contracts
        .iter()
        .map(|rpc| (snake_case(&rpc.name), format!("api/{}.rs", rpc.name)))
        .collect();
    if layout.ssr_page {
        bins.push((
            VERCEL_PAGE_FUNCTION.to_string(),
            format!("api/{VERCEL_PAGE_FUNCTION}.rs"),
        ));
    }
    bins
}

fn cargo_toml(app: &VerifiedApplication, target: &TargetConfig, layout: &Layout) -> String {
    let package = package_name(&app.project_name);
    if !layout.server {
        return format!(
            "[package]\nname = \"{package}\"\nversion = \"{}\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\n",
            app.project_version
        );
    }
    let platform_dependency = if target.deployment == "aws-lambda" {
        format!(
            "lambda_http = {{ version = \"={LAMBDA_HTTP}\", default-features = false, features = [\"apigw_http\"] }}\n"
        )
    } else {
        format!("vercel_runtime = \"={VERCEL_RUNTIME}\"\nhttp-body-util = \"={HTTP_BODY_UTIL}\"\n")
    };
    let bins: String = binaries(app, target, layout)
        .iter()
        .map(|(name, path)| format!("[[bin]]\nname = \"{name}\"\npath = \"{path}\"\n\n"))
        .collect();
    format!(
        "[package]\nname = \"{package}\"\nversion = \"{}\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\n{}{platform_dependency}\n{bins}\n[profile.release]\nstrip = true\nlto = \"fat\"\ncodegen-units = 1\npanic = \"abort\"\n",
        app.project_version,
        server_dependencies()
    )
}

/// The procedures of the `bind-json-v3` contract: for each input field and the output, the
/// Bind type and its JSON encoding (`Int`, `Decimal` and `Money` are strings).
fn contract_schema(app: &VerifiedApplication) -> String {
    let describe = |type_ref: &TypeRef| {
        format!(
            "{{\"type\":{},\"json\":{}}}",
            json(&type_ref.display()),
            json(&wire_json(&parse_wire_type(type_ref), app, 0))
        )
    };
    let procedures = app
        .rpc_contracts
        .iter()
        .map(|rpc| {
            format!(
                "    {}: {{\"path\":{},\"input\":{{{}}},\"output\":{}}}",
                json(&rpc.name),
                json(&rpc.path),
                rpc.parameters
                    .iter()
                    .map(|parameter| format!(
                        "{}:{}",
                        json(&parameter.name),
                        describe(&parameter.declared_type())
                    ))
                    .collect::<Vec<_>>()
                    .join(","),
                describe(&rpc.return_type)
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        "{{\n  \"wireVersion\": \"{WIRE_VERSION}\",\n  \"procedures\": {{\n{procedures}\n  }}\n}}\n"
    )
}

/// For each procedure, a valid input and malformed ones: each field in turn replaced by a
/// value of the wrong shape (for example `"1.5"` for an `Int`), and an unknown field.
fn contract_vectors(app: &VerifiedApplication) -> String {
    let vectors = app
        .rpc_contracts
        .iter()
        .map(|rpc| {
            let types: Vec<TypeExpr> = rpc
                .parameters
                .iter()
                .map(|parameter| parse_wire_type(&parameter.declared_type()))
                .collect();
            let object = |replaced: Option<(usize, &str)>, extra: bool| {
                let mut fields: Vec<String> = rpc
                    .parameters
                    .iter()
                    .zip(&types)
                    .enumerate()
                    .map(|(index, (parameter, ty))| {
                        let value = match replaced {
                            Some((target, value)) if target == index => value.to_string(),
                            _ => sample_json(ty, app, 0),
                        };
                        format!("{}:{value}", json(&parameter.name))
                    })
                    .collect();
                if extra {
                    fields.push("\"bindUnknownField\":0".to_string());
                }
                format!("{{{}}}", fields.join(","))
            };
            let mut invalid = Vec::new();
            for (index, ty) in types.iter().enumerate() {
                if let Some(value) = malformed_json(ty, app) {
                    invalid.push(object(Some((index, &value)), false));
                }
            }
            invalid.push(object(None, true));
            format!(
                "    {{\"procedure\":{},\"validInput\":{},\"invalidInputs\":[{}]}}",
                json(&rpc.name),
                object(None, false),
                invalid.join(",")
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{{\n  \"wireVersion\": \"{WIRE_VERSION}\",\n  \"vectors\": [\n{vectors}\n  ]\n}}\n")
}

/// The structure of a declared wire type (an unparsable one, never produced by a verified
/// program, reads as `Unit`).
fn parse_wire_type(type_ref: &TypeRef) -> TypeExpr {
    parse_type_text(&type_ref.display(), &Span::default()).unwrap_or(TypeExpr::Named {
        name: "Unit".to_string(),
        args: Vec::new(),
        span: Span::default(),
    })
}

/// Nesting beyond which recursive types stop being expanded in samples.
const SAMPLE_DEPTH: usize = 8;

/// The JSON encoding of a wire type: `string`, `number`, `boolean`, `null`, `array` or
/// `object`, with `|null` when optional.
fn wire_json(ty: &TypeExpr, app: &VerifiedApplication, depth: usize) -> String {
    match ty {
        TypeExpr::Nullable(inner) => format!("{}|null", wire_json(inner, app, depth)),
        TypeExpr::Function { .. } => "unsupported".to_string(),
        TypeExpr::Named { name, .. } => match name.as_str() {
            "Unit" => "null".to_string(),
            "Float" | "Double" => "number".to_string(),
            "Boolean" => "boolean".to_string(),
            "List" => "array".to_string(),
            _ => match declaration(name, app) {
                Some(declaration) => match declaration.kind {
                    TypeKind::Data | TypeKind::Entity => "object".to_string(),
                    TypeKind::Enum | TypeKind::Sealed => "string".to_string(),
                    TypeKind::Distinct => match &declaration.underlying {
                        Some(underlying) if depth < SAMPLE_DEPTH => {
                            wire_json(&parse_wire_type(underlying), app, depth + 1)
                        }
                        _ => "unsupported".to_string(),
                    },
                },
                None => "string".to_string(),
            },
        },
    }
}

/// A valid JSON value of a wire type.
fn sample_json(ty: &TypeExpr, app: &VerifiedApplication, depth: usize) -> String {
    let TypeExpr::Named { name, .. } = ty else {
        return "null".to_string();
    };
    match name.as_str() {
        "Unit" => "null".to_string(),
        "UUID" => json("00000000-0000-4000-8000-000000000000"),
        "String" | "Email" | "Url" => json("sample"),
        "Int" | "Decimal" | "Money" => json("0"),
        "Date" => json("2026-01-01"),
        "DateTime" => json("2026-01-01T00:00:00"),
        "Instant" => json("2026-01-01T00:00:00Z"),
        "Float" | "Double" => "0".to_string(),
        "Boolean" => "false".to_string(),
        "List" => "[]".to_string(),
        _ if depth >= SAMPLE_DEPTH => "null".to_string(),
        _ => match declaration(name, app) {
            Some(declaration) => match declaration.kind {
                TypeKind::Data | TypeKind::Entity => format!(
                    "{{{}}}",
                    declaration
                        .fields
                        .iter()
                        .map(|field| format!(
                            "{}:{}",
                            json(&field.name),
                            sample_json(&parse_wire_type(&field.type_ref), app, depth + 1)
                        ))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                TypeKind::Enum | TypeKind::Sealed => declaration
                    .variants
                    .first()
                    .map_or_else(|| "null".to_string(), |variant| json(&lower_camel(variant))),
                TypeKind::Distinct => declaration.underlying.as_ref().map_or_else(
                    || "null".to_string(),
                    |underlying| sample_json(&parse_wire_type(underlying), app, depth + 1),
                ),
            },
            None => "null".to_string(),
        },
    }
}

/// A JSON value that a wire type must reject, if the type has one.
fn malformed_json(ty: &TypeExpr, app: &VerifiedApplication) -> Option<String> {
    let TypeExpr::Named { name, .. } = ty else {
        // Optional: a value of the wrong shape for the inner type.
        return match ty {
            TypeExpr::Nullable(inner) => malformed_json(inner, app),
            _ => None,
        };
    };
    Some(match name.as_str() {
        "Unit" => return None,
        "Int" => json("1.5"),
        "Decimal" | "Money" => json("1.5.0"),
        "UUID" => json("not-a-uuid"),
        "Date" => json("2026-13-01"),
        "DateTime" | "Instant" => json("2026-01-01 25:00"),
        "String" | "Email" | "Url" => "0".to_string(),
        "Float" | "Double" => json("0"),
        "Boolean" => json("false"),
        "List" => "{}".to_string(),
        _ => match declaration(name, app)?.kind {
            TypeKind::Data | TypeKind::Entity => "[]".to_string(),
            TypeKind::Enum | TypeKind::Sealed => json("bindUnknownVariant"),
            TypeKind::Distinct => {
                let underlying = declaration(name, app)?.underlying.as_ref()?;
                return malformed_json(&parse_wire_type(underlying), app);
            }
        },
    })
}

fn declaration<'a>(name: &str, app: &'a VerifiedApplication) -> Option<&'a TypeDecl> {
    app.program
        .types
        .iter()
        .find(|declaration| declaration.name == name)
}

/// The manifest's `apiUrl`: the base URL the split page calls (JSON), `null` in the `ssr` mode.
pub fn manifest_api_url(target: &TargetConfig) -> String {
    match target.mode {
        Some(Mode::Split) => json(&target.api_url),
        _ => "null".to_string(),
    }
}

/// The manifest's `frontendOrigin`: the only origin the API answers (CORS), or `null`.
pub fn manifest_frontend_origin(target: &TargetConfig) -> String {
    crate::backend::js::plan::cors_origin(target).map_or_else(|| "null".to_string(), json)
}

fn generated_manifest(app: &VerifiedApplication, target: &TargetConfig) -> String {
    format!(
        "{{\n  \"languageVersion\": \"0.1.0\",\n  \"compilerVersion\": \"bindc-0.1.0\",\n  \"profile\": \"{PROFILE_VERSION}\",\n  \"certified\": false,\n  \"certificationState\": \"generated\",\n  \"project\": {},\n  \"target\": {},\n  \"mode\": {},\n  \"deployment\": {},\n  \"sourceHash\": {},\n  \"schemaFingerprint\": {},\n  \"wireVersion\": \"{WIRE_VERSION}\",\n  \"apiUrl\": {},\n  \"frontendOrigin\": {},\n  \"requiredCapabilities\": {}\n}}\n",
        json(&app.project_name),
        json(&app.target),
        json(target.mode_name()),
        json(&target.deployment),
        json(&app.source_hash),
        json(&app.database.fingerprint),
        manifest_api_url(target),
        manifest_frontend_origin(target),
        json_array(
            &app.required_capabilities
                .iter()
                .cloned()
                .collect::<Vec<_>>()
        )
    )
}

/// The SAM template: one Lambda behind an HTTP API, serving every path in the `ssr` mode and
/// `/api/*` in the `split` mode.
fn aws_template(app: &VerifiedApplication, database_environment: &str, ssr: bool) -> String {
    let events = if ssr {
        "        Root:\n          Type: HttpApi\n          Properties:\n            Path: /\n            Method: ANY\n        Proxy:\n          Type: HttpApi\n          Properties:\n            Path: /{proxy+}\n            Method: ANY\n"
    } else {
        "        Api:\n          Type: HttpApi\n          Properties:\n            Path: /api/{proxy+}\n            Method: ANY\n"
    };
    format!(
        "AWSTemplateFormatVersion: '2010-09-09'\nTransform: AWS::Serverless-2016-10-31\nDescription: Bind certified deployment for {}\nResources:\n  BindApi:\n    Type: AWS::Serverless::Function\n    Metadata:\n      BuildMethod: rust-cargolambda\n      BuildProperties:\n        Binary: bootstrap\n    Properties:\n      Runtime: provided.al2023\n      Architectures: [arm64]\n      Handler: bootstrap\n      CodeUri: ./\n      Events:\n{events}      Environment:\n        Variables:\n          {}: !Ref DatabaseUrl\nParameters:\n  DatabaseUrl:\n    Type: String\n    NoEcho: true\n",
        app.project_name, database_environment
    )
}

/// What an entry point (the Lambda's `main`, a Vercel function) serves.
#[derive(Clone, Copy, Debug)]
struct Entry {
    /// The `ssr` mode: the application is one origin, pages included.
    ssr: bool,
    /// The server renders the page (`serve`); otherwise, in the `ssr` mode, the page is the
    /// pre-rendered `web/index.html`.
    ssr_page: bool,
    /// The target declares `frontend_origin`: RPC requests go through the library's `cors`.
    cors: bool,
}

/// The Lambda entry point. The tokio runtime is built by hand: its worker threads get
/// 64 MiB stacks, and every request runs on one of them (`serve_rpc`/`serve_page` spawn it), so
/// deep Bind recursion reaches `RecursionLimit` instead of overflowing a stack. `HEAD` is
/// answered like `GET`, without a body.
fn aws_main(database_environment: &str, entry: &Entry) -> String {
    let mut constants = String::new();
    let mut routes = String::new();
    if entry.cors || !entry.ssr_page {
        constants.push_str("\n/// Content type of the RPC responses.\nconst RPC_CONTENT_TYPE: &str = \"application/json; charset=utf-8\";\n");
    }
    if entry.ssr {
        routes.push_str("    let read = head || method == \"GET\";\n");
        constants.push_str("\n/// The stylesheet the page links (`/styles.css`).\nconst STYLESHEET: &str = include_str!(\"../web/styles.css\");\n");
        routes.push_str("    if read && path == \"/styles.css\" {\n        return respond(200, \"text/css; charset=utf-8\", if head { String::new() } else { STYLESHEET.to_string() }, &[]);\n    }\n");
        if !entry.ssr_page {
            constants.push_str("\n/// The page, pre-rendered at compile time (`/`).\nconst PAGE: &str = include_str!(\"../web/index.html\");\n");
            routes.push_str("    if read && (path.is_empty() || path == \"/\") {\n        return respond(200, \"text/html; charset=utf-8\", if head { String::new() } else { PAGE.to_string() }, &[]);\n    }\n");
        }
    }
    let mut headers = "&[]";
    if entry.cors {
        routes.push_str("    let origin = request.headers().get(\"origin\").and_then(|value| value.to_str().ok());\n    let cors = bind_app::cors(&method, origin);\n    if let Some(result) = cors.response {\n        return respond(result.status, RPC_CONTENT_TYPE, result.body, &cors.headers);\n    }\n");
        headers = "&cors.headers";
    }
    let serve = if entry.ssr_page {
        format!(
            "    let query = request.uri().query().unwrap_or(\"\").to_string();\n    let method = if head {{ \"GET\" }} else {{ method.as_str() }};\n    let response = bind_app::serve(method, path, query, body(&request), pool).await;\n    respond(response.status, response.content_type, if head {{ String::new() }} else {{ response.body }}, {headers})\n"
        )
    } else {
        format!(
            "    let result = bind_app::serve_rpc(path, body(&request), pool).await;\n    respond(result.status, RPC_CONTENT_TYPE, if head {{ String::new() }} else {{ result.body }}, {headers})\n"
        )
    };
    AWS_MAIN
        .replace("__BIND_DATABASE_ENV__", &rust_string(database_environment))
        .replace("__BIND_CONSTANTS__", &constants)
        .replace("__BIND_ROUTES__", &format!("{routes}{serve}"))
}

const AWS_MAIN: &str = r#"// GENERATED BY BINDC. DO NOT EDIT.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable)]

use lambda_http::{Body, Error, Request, Response, run, service_fn};
use sqlx::{PgPool, postgres::PgPoolOptions};

__BIND_CONSTANTS__
fn main() -> Result<(), Error> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(64 * 1024 * 1024)
        .build()?;
    runtime.block_on(serve())
}

async fn serve() -> Result<(), Error> {
    let database_url = std::env::var(__BIND_DATABASE_ENV__)?;
    let pool = PgPoolOptions::new().max_connections(5).connect_lazy(&database_url)?;
    run(service_fn(move |request: Request| {
        let pool = pool.clone();
        async move { handle(request, pool).await }
    }))
    .await
}

fn body(request: &Request) -> Vec<u8> {
    match request.body() {
        Body::Empty => Vec::new(),
        Body::Text(value) => value.as_bytes().to_vec(),
        Body::Binary(value) => value.clone(),
        _ => Vec::new(),
    }
}

fn respond(
    status: u16,
    content_type: &str,
    body: String,
    headers: &[(&'static str, String)],
) -> Result<Response<String>, Error> {
    let mut response = Response::builder()
        .status(status)
        .header("content-type", content_type);
    for (name, value) in headers {
        response = response.header(*name, value.as_str());
    }
    response.body(body).map_err(Error::from)
}

async fn handle(request: Request, pool: PgPool) -> Result<Response<String>, Error> {
    let method = request.method().as_str().to_string();
    let head = method == "HEAD";
    let path = request.uri().path().to_string();
__BIND_ROUTES__}
"#;

/// The common part of a Vercel function: a hand-built tokio runtime with 64 MiB worker stacks,
/// as in `AWS_MAIN`, a lazy pool and `respond`; `handler` is the function's own.
fn vercel_function(database_environment: &str, uses: &str, handler: &str) -> String {
    format!(
        "// GENERATED BY BINDC. DO NOT EDIT.\n#![forbid(unsafe_code)]\n#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable)]\n\n{uses}use sqlx::{{PgPool, postgres::PgPoolOptions}};\nuse vercel_runtime::{{Error, Request, Response, run, service_fn}};\n\nfn main() -> Result<(), Error> {{\n    let runtime = tokio::runtime::Builder::new_multi_thread()\n        .enable_all()\n        .thread_stack_size(64 * 1024 * 1024)\n        .build()?;\n    runtime.block_on(serve())\n}}\n\nasync fn serve() -> Result<(), Error> {{\n    let database_url = std::env::var({})?;\n    let pool = PgPoolOptions::new().max_connections(5).connect_lazy(&database_url)?;\n    run(service_fn(move |request| {{\n        let pool = pool.clone();\n        async move {{ handler(request, pool).await }}\n    }})).await\n}}\n\nfn respond(\n    status: u16,\n    content_type: &str,\n    body: String,\n    headers: &[(&'static str, String)],\n) -> Result<Response<String>, Error> {{\n    let mut response = Response::builder()\n        .status(status)\n        .header(\"content-type\", content_type);\n    for (name, value) in headers {{\n        response = response.header(*name, value.as_str());\n    }}\n    Ok(response.body(body)?)\n}}\n\n{handler}",
        rust_string(database_environment),
    )
}

/// The Vercel function of the endpoint at `path` (CORS first when `cors`).
fn vercel_rpc_main(path: &str, database_environment: &str, cors: bool) -> String {
    let (check, headers) = if cors {
        (
            "    let origin = request.headers().get(\"origin\").and_then(|value| value.to_str().ok()).map(str::to_string);\n    let cors = bind_app::cors(request.method().as_str(), origin.as_deref());\n    if let Some(result) = cors.response {\n        return respond(result.status, \"application/json; charset=utf-8\", result.body, &cors.headers);\n    }\n",
            "&cors.headers",
        )
    } else {
        ("", "&[]")
    };
    vercel_function(
        database_environment,
        "use http_body_util::BodyExt;\n",
        &format!(
            "async fn handler(request: Request, pool: PgPool) -> Result<Response<String>, Error> {{\n{check}    let bytes = request.into_body().collect().await?.to_bytes();\n    let result = {}::serve_rpc({}.to_string(), bytes.to_vec(), pool).await;\n    respond(result.status, \"application/json; charset=utf-8\", result.body, {headers})\n}}\n",
            package_name_for_code(),
            rust_string(path)
        ),
    )
}

/// The Vercel page function of the `ssr` mode (`vercel.json` rewrites `/` to it): `serve` with
/// the request's query string; `HEAD` like `GET`, without a body.
fn vercel_page_main(database_environment: &str) -> String {
    vercel_function(
        database_environment,
        "use http_body_util::BodyExt;\n",
        &format!(
            "async fn handler(request: Request, pool: PgPool) -> Result<Response<String>, Error> {{\n    let head = request.method() == \"HEAD\";\n    let method = if head {{ \"GET\".to_string() }} else {{ request.method().as_str().to_string() }};\n    let query = request.uri().query().unwrap_or(\"\").to_string();\n    let bytes = request.into_body().collect().await?.to_bytes();\n    let response = {}::serve(&method, \"/\".to_string(), query, bytes.to_vec(), pool).await;\n    respond(response.status, response.content_type, if head {{ String::new() }} else {{ response.body }}, &[])\n}}\n",
            package_name_for_code()
        ),
    )
}

/// `vercel.json`: `web/` is served as is (no build step); in the `ssr` mode `/` is rewritten to
/// the page function (static files win over rewrites, and `web/` then has no `index.html`).
fn vercel_json(ssr_page: bool) -> String {
    let rewrites = if ssr_page {
        format!(
            ",\n  \"rewrites\": [{{ \"source\": \"/\", \"destination\": \"/api/{VERCEL_PAGE_FUNCTION}\" }}]"
        )
    } else {
        String::new()
    };
    format!(
        "{{\n  \"$schema\": \"https://openapi.vercel.sh/vercel.json\",\n  \"outputDirectory\": \"web\"{rewrites}\n}}\n"
    )
}

fn generated_readme(target: &TargetConfig, layout: &Layout) -> String {
    let page = if layout.web.is_static {
        "The page is static: `web/index.html` was rendered at compile time and loads no JavaScript."
    } else if target.mode == Some(Mode::Ssr) {
        "The page is rendered by the Rust server at `GET /` and loads no JavaScript."
    } else {
        "The page is `web/index.html`, `web/app.js` and `web/bind-runtime.js`: native ES modules, no build step. It calls the API at `api_url`."
    };
    let deployment = match (target.deployment.as_str(), target.mode, layout.server) {
        (_, _, false) => "There is no server: deploy `web/` to any static host.",
        ("aws-lambda", Some(Mode::Ssr), true) => {
            "Run `sam build --beta-features`, then `sam deploy --guided`. The Lambda serves `/`, `/styles.css` and `/api/*`."
        }
        ("aws-lambda", _, true) => {
            "Run `sam build --beta-features`, then `sam deploy --guided`. The Lambda serves `/api/*`; host `web/` statically (for example S3 and CloudFront) and set `api_url` (and `frontend_origin` when the API is on another origin)."
        }
        _ => {
            "Run `vercel deploy`. Vercel serves `web/` as is and builds every Rust function declared in Cargo.toml."
        }
    };
    format!(
        "# Generated Bind application\n\nProfile: `{PROFILE_VERSION}`  \nMode: `{}`  \nDeployment: `{}`\n\nThis directory is generated. `bind build` compiles and tests the Rust server; `bind certify` also lints it and writes an artifact attestation.\n\n{page}\n\n{deployment}\n",
        target.mode_name(),
        target.deployment
    )
}

fn package_name(name: &str) -> String {
    let _ = name;
    "bind-app".to_string()
}

// Generated binaries refer to the Cargo package library; Cargo normalizes '-' to '_'.
fn package_name_for_code() -> &'static str {
    "bind_app"
}

fn rust_string(value: &str) -> String {
    format!("{:?}", value)
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

pub(crate) fn pascal_case(value: &str) -> String {
    let mut output = String::new();
    let mut upper = true;
    for ch in value.chars() {
        if !ch.is_ascii_alphanumeric() {
            upper = true;
        } else if upper {
            output.push(ch.to_ascii_uppercase());
            upper = false;
        } else {
            output.push(ch);
        }
    }
    output
}

pub(crate) fn lower_camel(value: &str) -> String {
    let pascal = pascal_case(value);
    let mut chars = pascal.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_lowercase(), chars.as_str()),
        None => String::new(),
    }
}

/// `src/lib.rs` of an application with a static client and no endpoints.
const STATIC_LIBRARY: &str = "// GENERATED BY BINDC. DO NOT EDIT.\n#![forbid(unsafe_code)]\n#![deny(unused_must_use)]\n\n/// Marker for a Bind application with a static client and no server boundary.\npub const STATIC_CLIENT: bool = true;\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn static_client_has_no_runtime_server() {\n        assert!(std::hint::black_box(super::STATIC_CLIENT));\n    }\n}\n";

#[cfg(test)]
mod tests {
    use super::*;

    /// The server dependencies are the runtime's, at the same pinned versions.
    #[test]
    fn server_dependencies_match_the_runtime() {
        let manifest = include_str!("../runtime/bind-runtime/Cargo.toml");
        let runtime = manifest.split_once("[dependencies]").unwrap().1;
        let generated = server_dependencies();
        for line in runtime
            .lines()
            .filter(|line| line.contains("version = ") || line.contains(" = \"="))
        {
            let name = line.split_whitespace().next().unwrap();
            let version = line.split('"').find(|part| part.starts_with('=')).unwrap();
            let generated_line = generated
                .lines()
                .find(|candidate| candidate.split_whitespace().next() == Some(name))
                .unwrap_or_else(|| panic!("`{name}` is missing from the generated dependencies"));
            assert!(
                generated_line.contains(&format!("\"{version}\"")),
                "{name}: {generated_line} vs {line}"
            );
        }
        assert!(generated.contains("features = [\"derive\", \"rc\"]"));
    }

    /// A generation that fails leaves the previous output untouched.
    #[test]
    fn a_lock_is_kept_only_for_the_same_dependencies() {
        let manifest = |dependencies: &str, bins: &str| {
            format!(
                "[package]\nname = \"bind-app\"\n\n[dependencies]\n{dependencies}\n{bins}\n[profile.release]\nstrip = true\n"
            )
        };
        let old = manifest("serde = \"=1\"\nfutures = \"=0.3.31\"\n", "");
        let new = manifest("serde = \"=1\"\n", "");
        let more_bins = manifest(
            "serde = \"=1\"\n",
            "[[bin]]\nname = \"a\"\npath = \"api/a.rs\"\n",
        );
        assert!(!same_dependencies(Some(&old), Some(&new)));
        assert!(same_dependencies(Some(&new), Some(&more_bins)));
        assert!(!same_dependencies(None, Some(&new)));
    }

    const CUSTOMER_SOURCE: &str = "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\nserver fun getCustomer(id: CustomerId): Customer effects [db.read] = db.customers.require(id)\nclient component CustomerPage(id: CustomerId) {\n    val customer = server.getCustomer(id)\n    UI { Text(customer.name) }\n}\n";
    const CUSTOMER_SCHEMA: &str = "table,model,primary_key,column,type,nullable,required,has_default,engine\ncustomers,Customer,id,id,CustomerId,false,true,true,postgresql\ncustomers,Customer,id,name,String,false,true,false,postgresql\n";

    /// A project in a temporary directory with `source`, `schema` and the target `t` declared by
    /// `target`, verified; the directory is removed on drop.
    struct Project {
        root: PathBuf,
        config: ProjectConfig,
        app: VerifiedApplication,
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn project(name: &str, source: &str, schema: &str, target: &str) -> Project {
        let root =
            std::env::temp_dir().join(format!("bindc-certified-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("app")).unwrap();
        fs::write(root.join("app/main.bd"), source).unwrap();
        fs::write(root.join("schema.csv"), schema).unwrap();
        fs::write(
            root.join("bind.toml"),
            format!("[project]\nname = \"test\"\nsource = \"app\"\n[web]\ntitle = \"Shop\"\n[database.main]\nschema = \"schema.csv\"\nverification = \"static\"\n[target.t]\nclient = \"web\"\nserver = \"rust\"\n{target}capabilities = [\"ui.dom\", \"server.http\", \"db.read\"]\n"),
        )
        .unwrap();
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        let result = crate::compiler::verify(&config, "t");
        let app = result
            .application
            .unwrap_or_else(|| panic!("{:?}", result.diagnostics));
        Project { root, config, app }
    }

    fn paths(files: &[(String, String)]) -> Vec<&str> {
        let mut paths: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
        paths.sort();
        paths
    }

    fn file<'a>(files: &'a [(String, String)], path: &str) -> &'a str {
        files
            .iter()
            .find(|(candidate, _)| candidate == path)
            .map(|(_, content)| content.as_str())
            .unwrap_or_else(|| panic!("no {path}"))
    }

    #[test]
    fn deployments_of_the_four_combinations() {
        let target = |deployment: &str, mode: &str| {
            format!("deployment = \"{deployment}\"\nmode = \"{mode}\"\n")
        };
        // aws-lambda, ssr: the Lambda serves the page, the stylesheet and the API.
        let p = project(
            "aws-ssr",
            CUSTOMER_SOURCE,
            CUSTOMER_SCHEMA,
            &target("aws-lambda", "ssr"),
        );
        let files = workspace_files(&p.app, &p.config, &p.config.targets["t"]).unwrap();
        assert_eq!(
            paths(&files),
            [
                "Cargo.toml",
                "README.md",
                "bind-manifest.json",
                "contracts/schema.json",
                "contracts/vectors.json",
                "src/bind_runtime.rs",
                "src/lib.rs",
                "src/main.rs",
                "template.yaml",
                "web/styles.css"
            ]
        );
        assert!(file(&files, "src/lib.rs").contains("pub async fn handle_page("));
        let main = file(&files, "src/main.rs");
        assert!(main.contains("bind_app::serve(method, path, query, body(&request), pool)"));
        assert!(main.contains("include_str!(\"../web/styles.css\")"));
        assert!(file(&files, "template.yaml").contains("Path: /{proxy+}"));
        assert!(file(&files, "README.md").contains("Mode: `ssr`"));
        // aws-lambda, split: the Lambda serves the API; web/ is the static frontend.
        let p = project(
            "aws-split",
            CUSTOMER_SOURCE,
            CUSTOMER_SCHEMA,
            &target("aws-lambda", "split"),
        );
        let files = workspace_files(&p.app, &p.config, &p.config.targets["t"]).unwrap();
        assert!(
            paths(&files).contains(&"web/app.js") && paths(&files).contains(&"web/bind-runtime.js")
        );
        assert!(!file(&files, "src/lib.rs").contains("handle_page"));
        assert!(
            file(&files, "src/main.rs").contains("bind_app::serve_rpc(path, body(&request), pool)")
        );
        assert!(file(&files, "template.yaml").contains("Path: /api/{proxy+}"));
        assert!(file(&files, "web/index.html").contains("<title>Shop</title>"));
        // vercel, ssr: one function per endpoint and the page function, `/` rewritten to it.
        let p = project(
            "vercel-ssr",
            CUSTOMER_SOURCE,
            CUSTOMER_SCHEMA,
            &target("vercel", "ssr"),
        );
        let files = workspace_files(&p.app, &p.config, &p.config.targets["t"]).unwrap();
        assert_eq!(
            paths(&files),
            [
                "Cargo.toml",
                "README.md",
                "api/bind-page.rs",
                "api/getCustomer.rs",
                "bind-manifest.json",
                "contracts/schema.json",
                "contracts/vectors.json",
                "src/bind_runtime.rs",
                "src/lib.rs",
                "vercel.json",
                "web/styles.css"
            ]
        );
        assert!(file(&files, "vercel.json").contains(
            "\"rewrites\": [{ \"source\": \"/\", \"destination\": \"/api/bind-page\" }]"
        ));
        assert!(
            file(&files, "Cargo.toml")
                .contains("[[bin]]\nname = \"bind-page\"\npath = \"api/bind-page.rs\"")
        );
        assert!(
            file(&files, "api/bind-page.rs")
                .contains("bind_app::serve(&method, \"/\".to_string(), query")
        );
        // vercel, split: static web/ and the endpoint functions, same origin.
        let p = project(
            "vercel-split",
            CUSTOMER_SOURCE,
            CUSTOMER_SCHEMA,
            &target("vercel", "split"),
        );
        let files = workspace_files(&p.app, &p.config, &p.config.targets["t"]).unwrap();
        assert!(!paths(&files).contains(&"api/bind-page.rs"));
        assert!(!file(&files, "vercel.json").contains("rewrites"));
        assert!(file(&files, "web/app.js").contains("const API_URL = \"/api\";"));
    }

    #[test]
    fn a_cross_origin_api_answers_cors_for_the_frontend_origin_only() {
        let p = project(
            "cors",
            CUSTOMER_SOURCE,
            CUSTOMER_SCHEMA,
            "deployment = \"aws-lambda\"\nmode = \"split\"\napi_url = \"https://api.example.com/api\"\nfrontend_origin = \"https://app.example.com\"\n",
        );
        let files = workspace_files(&p.app, &p.config, &p.config.targets["t"]).unwrap();
        assert!(
            file(&files, "src/lib.rs")
                .contains("pub const FRONTEND_ORIGIN: &str = \"https://app.example.com\";")
        );
        assert!(
            file(&files, "src/main.rs").contains("let cors = bind_app::cors(&method, origin);")
        );
        assert!(
            file(&files, "web/app.js").contains("const API_URL = \"https://api.example.com/api\";")
        );
        // Same origin by default: no CORS at all.
        let p = project(
            "no-cors",
            CUSTOMER_SOURCE,
            CUSTOMER_SCHEMA,
            "deployment = \"aws-lambda\"\nmode = \"split\"\n",
        );
        let files = workspace_files(&p.app, &p.config, &p.config.targets["t"]).unwrap();
        assert!(!file(&files, "src/lib.rs").contains("FRONTEND_ORIGIN"));
        assert!(!file(&files, "src/main.rs").contains("cors"));
    }

    #[test]
    fn static_pages_are_prerendered_in_both_modes() {
        let source = "client component Site() {\n    UI {\n        Heading(1, \"t\") { Text(\"<Bind>\") }\n    }\n}\n";
        let empty = "table,model,primary_key,column,type,nullable,required,has_default,engine\n";
        let expected = crate::backend::js::page::page_html(
            &crate::backend::js::page::PageMeta {
                title: "Shop",
                description: None,
                language: "en",
                stylesheet: "/styles.css",
            },
            "<h1 class=\"t\">&lt;Bind&gt;</h1>",
        );
        for (name, target) in [
            ("static-ssr", "deployment = \"vercel\"\nmode = \"ssr\"\n"),
            (
                "static-split",
                "deployment = \"vercel\"\nmode = \"split\"\n",
            ),
            (
                "static-aws",
                "deployment = \"aws-lambda\"\nmode = \"ssr\"\n",
            ),
        ] {
            let p = project(name, source, empty, target);
            let files = workspace_files(&p.app, &p.config, &p.config.targets["t"]).unwrap();
            assert_eq!(file(&files, "web/index.html"), expected, "{name}");
            assert!(
                expected
                    .contains("<div class=\"bind-site\"><h1 class=\"t\">&lt;Bind&gt;</h1></div>")
            );
            // No JS, and no server: web/ is the whole application.
            assert!(
                !paths(&files)
                    .iter()
                    .any(|path| path.ends_with(".js") || path.starts_with("api/")),
                "{name}"
            );
            assert_eq!(file(&files, "src/lib.rs"), STATIC_LIBRARY, "{name}");
            assert!(!paths(&files).contains(&"src/main.rs"), "{name}");
        }
    }

    #[test]
    fn failed_generation_keeps_the_previous_output() {
        let mut p = project(
            "failed",
            CUSTOMER_SOURCE,
            CUSTOMER_SCHEMA,
            "deployment = \"aws-lambda\"\nmode = \"ssr\"\n",
        );
        let output = p.root.join("out");
        fs::create_dir_all(&output).unwrap();
        fs::write(output.join("previous.txt"), "kept").unwrap();
        // Simulate a backend failure: an endpoint reading an `environment` value, which
        // `verify` would have rejected (E8310).
        let endpoint = p
            .app
            .core
            .functions
            .iter_mut()
            .find(|function| function.endpoint)
            .unwrap();
        let ty = endpoint.body.ty.clone();
        endpoint.body = crate::core::ir::Expr {
            ty,
            kind: crate::core::ir::ExprKind::Environment("databaseUrl".to_string()),
        };
        let error = generate(&p.app, &p.config, Some(&output)).unwrap_err();
        assert!(error.contains("environment"), "{error}");
        assert_eq!(
            fs::read_to_string(output.join("previous.txt")).unwrap(),
            "kept"
        );
        assert!(!output.join("Cargo.toml").exists());
    }

    /// `generate` on `examples/commerce` (the repository's `bind.toml`) for the four targets,
    /// then `cargo clippy --all-targets -- -D warnings` and `cargo test` in the generated crate.
    #[test]
    #[ignore = "generates and builds Rust crates; run with `cargo test -- --ignored`"]
    fn generated_commerce_builds_without_warnings() {
        if std::env::var_os("DATABASE_URL").is_none() {
            // SAFETY: read only through `std::env`, whose accesses take the same lock.
            unsafe { std::env::set_var("DATABASE_URL", "postgresql://bind:bind@localhost/bind") };
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        for target in [
            "aws-lambda-ssr",
            "aws-lambda-split",
            "vercel-ssr",
            "vercel-split",
        ] {
            let result = crate::compiler::verify(&config, target);
            let app = result
                .application
                .unwrap_or_else(|| panic!("{target}: {:?}", result.diagnostics));
            let output = root
                .join("target/bind-codegen-tests")
                .join(format!("commerce-{target}"));
            let generated = generate(&app, &config, Some(&output)).unwrap();
            fs::copy(
                root.join("runtime/bind-runtime/Cargo.lock"),
                generated.join("Cargo.lock"),
            )
            .unwrap();
            crate::backend::rust::tests::cargo(
                &generated,
                &["clippy", "--quiet", "--all-targets", "--", "-D", "warnings"],
            );
            crate::backend::rust::tests::cargo(&generated, &["test", "--quiet"]);
        }
    }

    #[test]
    fn locks_and_manifests_record_the_js_runtime_only_when_shipped_and_the_api_settings() {
        let split = TargetConfig {
            client: "web".into(),
            server: "rust".into(),
            deployment: "aws-lambda".into(),
            mode: Some(Mode::Split),
            api_url: "https://api.example.com/api".into(),
            frontend_origin: Some("https://app.example.com".into()),
            ..TargetConfig::default()
        };
        let lock = dependency_lock(&split, Some("sha256:c"), Some("sha256:j"));
        assert!(lock.contains("mode = \"split\""));
        assert!(lock.contains("cargoLockSha256 = \"sha256:c\"\njsRuntimeSha256 = \"sha256:j\"\n"));
        assert!(!dependency_lock(&split, Some("sha256:c"), None).contains("jsRuntimeSha256"));
        assert_eq!(manifest_api_url(&split), "\"https://api.example.com/api\"");
        assert_eq!(
            manifest_frontend_origin(&split),
            "\"https://app.example.com\""
        );
        let ssr = TargetConfig {
            mode: Some(Mode::Ssr),
            frontend_origin: None,
            ..split
        };
        assert_eq!(manifest_api_url(&ssr), "null");
        assert_eq!(manifest_frontend_origin(&ssr), "null");
        let p = project(
            "manifest",
            CUSTOMER_SOURCE,
            CUSTOMER_SCHEMA,
            "deployment = \"vercel\"\nmode = \"split\"\n",
        );
        let files = workspace_files(&p.app, &p.config, &p.config.targets["t"]).unwrap();
        let manifest = file(&files, "bind-manifest.json");
        assert!(manifest.contains("\"mode\": \"split\""));
        assert!(manifest.contains("\"apiUrl\": \"/api\",\n  \"frontendOrigin\": null,"));
    }

    #[test]
    fn converts_identifiers() {
        assert_eq!(snake_case("getCustomer"), "get_customer");
        assert_eq!(pascal_case("get_customer"), "GetCustomer");
        assert_eq!(lower_camel("CustomerId"), "customerId");
    }
}
