use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::ast::*;
use crate::config::{Backend, Mode, ProjectConfig, TargetConfig, valid_environment_name};
use crate::core::ir::CoreProgram;
use crate::diagnostic::{Diagnostic, Severity, Span, has_errors};
pub use crate::json::{array as json_array, string as json};
use crate::lexer::lex;
use crate::parser::expr::parse_type_text;
use crate::parser::parse;
use crate::schema::{DatabaseSchema, stable_hash};
use crate::syntax::expr::TypeExpr;

#[derive(Clone, Debug)]
pub struct EnvironmentStatus {
    pub variable: String,
    pub type_name: String,
    pub lifecycle: String,
    pub required: bool,
    pub resolved: bool,
    pub secret: bool,
    pub public: bool,
}

#[derive(Clone, Debug)]
pub struct RpcContract {
    pub name: String,
    pub parameters: Vec<Parameter>,
    pub return_type: TypeRef,
    pub path: String,
}

pub use crate::core::analysis::{ApplicationGraph, DatabaseOperation};

#[derive(Clone, Debug)]
pub struct VerifiedApplication {
    pub project_name: String,
    pub project_version: String,
    pub target: String,
    pub source_hash: String,
    pub program: Program,
    pub graph: ApplicationGraph,
    pub environment: Vec<EnvironmentStatus>,
    pub database: DatabaseSchema,
    pub database_operations: Vec<DatabaseOperation>,
    pub rpc_contracts: Vec<RpcContract>,
    pub required_capabilities: BTreeSet<String>,
    pub unsafe_operations: Vec<String>,
    /// The validated typed core IR: the input of the typed code generators.
    pub core: CoreProgram,
}

pub struct CompileResult {
    pub application: Option<VerifiedApplication>,
    pub diagnostics: Vec<Diagnostic>,
    pub module_count: usize,
}

/// The file `--emit-ir` writes in the generated workspace (never a deployable artifact).
pub const EMITTED_IR: &str = "verified-application.ir.json";
/// The file `--emit-graph` writes in the generated workspace (never a deployable artifact).
pub const EMITTED_GRAPH: &str = "application.graph.txt";

/// The parsed sources of a project.
struct Sources {
    program: Program,
    /// Every source's path relative to the source directory (`/`-separated) and content: the
    /// input of `sourceHash`, independent of where the project is checked out.
    canonical: String,
    module_count: usize,
}

pub fn verify(config: &ProjectConfig, target: &str) -> CompileResult {
    let mut diagnostics = Vec::new();
    let sources = load_sources(config, &mut diagnostics);
    let module_count = sources.module_count;
    let abort = |diagnostics| CompileResult {
        application: None,
        diagnostics,
        module_count,
    };
    if has_errors(&diagnostics) {
        return abort(diagnostics);
    }
    let Sources {
        program, canonical, ..
    } = sources;
    let database = load_database(config, &mut diagnostics);
    let Some(target_config) = resolve_target(config, target, &mut diagnostics) else {
        return abort(diagnostics);
    };

    let (typed, type_diagnostics) = crate::types::check(&program, &database);
    let type_errors = has_errors(&type_diagnostics);
    diagnostics.extend(type_diagnostics);
    if type_errors {
        return abort(diagnostics);
    }
    // Declarations (duplicates, unknown types, `var` fields) and every database access
    // (tables, models, columns, operations) were checked by the type checker above.
    let required_capabilities = required_capabilities(&program, &typed);
    let mut environment = validate_environment(&program, config, &mut diagnostics);
    environment.sort_by(|a, b| a.variable.cmp(&b.variable));
    // Secrets reaching client code (E4102) are found on the IR (`core::analysis`).
    target_diagnostics(
        &program,
        &database,
        config,
        target,
        target_config,
        &required_capabilities,
        &mut diagnostics,
    );
    if config.warnings_as_errors {
        for item in &mut diagnostics {
            if item.severity == Severity::Warning {
                item.severity = Severity::Error;
            }
        }
    }
    let rpc_contracts = rpc_contracts(&program);
    // A program that passes every user-facing check is lowered to the core IR and validated.
    // This runs last so user diagnostics are never masked.
    let core = if has_errors(&diagnostics) {
        None
    } else {
        lower_and_check(
            &program,
            &typed,
            &database,
            config,
            target,
            target_config,
            &mut diagnostics,
        )
    };
    let application = core.map(|core| {
        let mut graph = crate::core::analysis::application_graph(&core);
        // The IR does not carry the declarations: each value's variable comes from the AST.
        for declaration in &program.environment {
            graph.add_edge(
                format!("environment.{}", declaration.name),
                format!("env.{}", declaration.variable),
            );
        }
        let database_operations = crate::core::analysis::database_operations(&core);
        VerifiedApplication {
            project_name: config.name.clone(),
            project_version: config.version.clone(),
            target: target.into(),
            source_hash: stable_hash(&canonical),
            program,
            graph,
            environment,
            database,
            database_operations,
            rpc_contracts,
            required_capabilities,
            unsafe_operations: Vec::new(),
            core,
        }
    });
    CompileResult {
        application,
        diagnostics,
        module_count,
    }
}

/// Lexes and parses every source file (E0001-E0003 for unreadable or missing sources).
fn load_sources(config: &ProjectConfig, diagnostics: &mut Vec<Diagnostic>) -> Sources {
    let mut sources = Sources {
        program: Program::default(),
        canonical: String::new(),
        module_count: 0,
    };
    let files = match source_files(&config.source) {
        Ok(files) => files,
        Err(error) => {
            diagnostics.push(Diagnostic::error("E0001", error, Span::default()));
            return sources;
        }
    };
    sources.module_count = files.len();
    for path in &files {
        match fs::read_to_string(path) {
            Ok(source) => {
                sources
                    .canonical
                    .push_str(&portable_relative(&config.source, path));
                sources.canonical.push('\0');
                sources.canonical.push_str(&source);
                sources.canonical.push('\0');
                let relative = path
                    .strip_prefix(&config.root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                let (tokens, mut lex_diagnostics) = lex(&relative, &source);
                diagnostics.append(&mut lex_diagnostics);
                let (module, mut parse_diagnostics) = parse(tokens);
                diagnostics.append(&mut parse_diagnostics);
                sources.program.merge(module);
            }
            Err(error) => diagnostics.push(Diagnostic::error(
                "E0002",
                format!("Cannot read {}: {error}", path.display()),
                Span::default(),
            )),
        }
    }
    if files.is_empty() {
        diagnostics.push(Diagnostic::error(
            "E0003",
            format!(
                "No .bind or .bd source files found in {}.",
                config.source.display()
            ),
            Span::default(),
        ));
    }
    sources
}

/// `path` relative to `root` with `/` separators.
fn portable_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// The database schema snapshot (E5101 when it cannot be loaded).
fn load_database(config: &ProjectConfig, diagnostics: &mut Vec<Diagnostic>) -> DatabaseSchema {
    match &config.database_schema {
        Some(path) => DatabaseSchema::load(path).unwrap_or_else(|error| {
            diagnostics.push(Diagnostic::error("E5101", error, Span::default()));
            DatabaseSchema::default()
        }),
        None => DatabaseSchema::default(),
    }
}

/// The declared target (E8001 when it is not).
fn resolve_target<'a>(
    config: &'a ProjectConfig,
    target: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<&'a TargetConfig> {
    let found = config.targets.get(target);
    if found.is_none() {
        diagnostics.push(
            Diagnostic::error(
                "E8001",
                format!("Target `{target}` is not declared in bind.toml."),
                Span::default(),
            )
            .with_hint(format!(
                "Available targets: {}",
                config
                    .targets
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        );
    }
    found
}

/// The capabilities the program needs: `ui.dom` for client code, `server.http` for server code
/// and every effect label except `server.call`.
fn required_capabilities(
    program: &Program,
    typed: &crate::types::TypedProgram,
) -> BTreeSet<String> {
    let mut required = BTreeSet::new();
    for function in &program.functions {
        match function.domain {
            Domain::Client => {
                required.insert("ui.dom".into());
            }
            Domain::Server => {
                required.insert("server.http".into());
            }
            _ => {}
        }
        for label in typed.effects.get(&function.name).into_iter().flatten() {
            if label != "server.call" {
                required.insert(label.clone());
            }
        }
    }
    required
}

/// What the target requires of the program: capabilities (E8101), frontend settings and the
/// stylesheet (E8311), a built-in generator (E8204), one page (E8301), the server surface of
/// its profile (E8309) and the certified surface (E8302).
fn target_diagnostics(
    program: &Program,
    database: &DatabaseSchema,
    config: &ProjectConfig,
    target: &str,
    target_config: &TargetConfig,
    required_capabilities: &BTreeSet<String>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let provided: BTreeSet<_> = target_config.capabilities.iter().cloned().collect();
    let missing: Vec<_> = required_capabilities
        .difference(&provided)
        .cloned()
        .collect();
    if !missing.is_empty() {
        diagnostics.push(Diagnostic::error("E8101", format!("Target `{target}` cannot implement this application.\n\nMissing capabilities:\n    {}", missing.join("\n    ")), Span::default()));
    }
    for problem in crate::backend::js::plan::target_problems(target, target_config) {
        diagnostics.push(Diagnostic::error(
            "E8311",
            problem.message(),
            Span::default(),
        ));
    }
    // The page's stylesheet is read when `generate` writes `web/styles.css`: a missing or
    // unreadable file is reported here, so `generate` never fails on a verified program.
    if target_config.client == "web"
        && let Some(path) = &config.web_stylesheet
        && let Err(error) = std::fs::read_to_string(path)
    {
        diagnostics.push(Diagnostic::error(
            "E8311",
            format!(
                "The web stylesheet `{}` (`[web].stylesheet` in bind.toml) cannot be read ({error}): create the file, fix the path (relative to bind.toml) or remove the setting to use the default stylesheet.",
                path.display()
            ),
            Span::default(),
        ));
    }
    let backend = target_config.backend();
    if backend == Backend::Unsupported {
        diagnostics.push(Diagnostic::error("E8204", format!("No built-in generator can preserve the semantics of target `{target}` ({} + {}, deployment {}).", target_config.client, target_config.server, target_config.deployment), Span::default()));
    }
    if backend == Backend::CloudflareWasm && target_config.mode == Some(Mode::Ssr) {
        diagnostics.push(Diagnostic::error(
            "E8311",
            "The first Cloudflare Workers WebAssembly profile supports `mode = \"split\"`; its host ABI does not implement SSR.",
            Span::default(),
        ));
    }
    if target_config.client == "web" {
        // One page per application, certified or not: the component is its entry point.
        let components: Vec<_> = program
            .functions
            .iter()
            .filter(|function| function.domain == Domain::Client && function.component)
            .collect();
        if components.len() > 1 {
            diagnostics.push(Diagnostic::error(
                "E8301",
                "The web profile accepts one client component as its application entry point.",
                components[1].span.clone(),
            ));
        }
    }
    match backend {
        Backend::RustWeb(_) => validate_server_surface(
            program,
            database,
            crate::certified::profile_version(),
            true,
            diagnostics,
        ),
        Backend::CloudflareWasm => validate_server_surface(
            program,
            database,
            crate::cloudflare::PROFILE_VERSION,
            false,
            diagnostics,
        ),
        Backend::Unsupported => {}
    }
    if target_config.certified {
        validate_certified_surface(program, diagnostics);
    }
}

/// The RPC procedures: the server functions that are not server components.
fn rpc_contracts(program: &Program) -> Vec<RpcContract> {
    program
        .functions
        .iter()
        .filter(|item| item.domain == Domain::Server && !item.component)
        .map(|function| RpcContract {
            name: function.name.clone(),
            parameters: function.parameters.clone(),
            return_type: function.declared_return(),
            path: format!("/api/{}", function.name),
        })
        .collect()
}

/// Lowers a program without user errors to the core IR, validates it and runs the checks of the
/// target's backends on it. An inconsistent IR is a compiler bug (E9001) and stops verification
/// before any code is generated.
fn lower_and_check(
    program: &Program,
    typed: &crate::types::TypedProgram,
    database: &DatabaseSchema,
    config: &ProjectConfig,
    target: &str,
    target_config: &TargetConfig,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<CoreProgram> {
    let backend = target_config.backend();
    let core = crate::core::lower::lower(program, typed, database);
    // The certified database runtime needs its credential declared (E8307) as soon as the
    // program accesses the database: any `Db` node of the IR, as the backend generates one
    // database function per operation.
    if matches!(backend, Backend::RustWeb(_))
        && target_config.certified
        && !crate::core::analysis::database_operations(&core).is_empty()
    {
        validate_database_credential(program, config, diagnostics);
    }
    // Diagnostics computed from types (E4102, E5201, E5303, E5204, E2301) come before validation:
    // a body-less function with a non-`Unit` result (E2301) is lowered with a `Unit` body,
    // which validation would report as a compiler bug.
    let typed_problems = crate::core::analysis::typed_diagnostics(&core, program, database);
    let bodiless_result = typed_problems
        .diagnostics
        .iter()
        .any(|item| item.code == "E2301");
    diagnostics.extend(typed_problems.diagnostics.iter().cloned());
    if bodiless_result {
        return None;
    }
    let problems = crate::core::validate::validate(&core);
    if !problems.is_empty() {
        diagnostics.push(Diagnostic::error(
            "E9001",
            format!(
                "Internal compiler error: the core IR is inconsistent ({}). This is a bug in bindc.",
                problems.join("; ")
            ),
            Span::default(),
        ));
        return None;
    }
    let (mut server_diagnostics, profile) = match backend {
        Backend::RustWeb(_) => (
            rust_server_diagnostics(&core, program, database, &typed_problems),
            crate::certified::profile_version(),
        ),
        Backend::CloudflareWasm => (
            cloudflare_server_diagnostics(&core, program, database, &typed_problems),
            crate::cloudflare::PROFILE_VERSION,
        ),
        Backend::Unsupported => (Vec::new(), crate::certified::profile_version()),
    };
    if target_config.client == "web" {
        server_diagnostics.extend(frontend_diagnostics(
            &core,
            program,
            target,
            profile,
            &typed_problems,
        ));
    }
    // The `ssr` page renders in a read-only transaction: a page that reaches a database write
    // is refused here, with the same message generation would give.
    if matches!(backend, Backend::RustWeb(_))
        && target_config.mode == Some(Mode::Ssr)
        && let Some(component) = program
            .functions
            .iter()
            .find(|function| function.domain == Domain::Client && function.component)
    {
        server_diagnostics.extend(
            crate::backend::rust::ssr_page_problems(&core, &component.name)
                .into_iter()
                .map(|problem| Diagnostic::error("E8311", problem, component.span.clone())),
        );
    }
    diagnostics.extend(server_diagnostics);
    (!has_errors(diagnostics)).then_some(core)
}

fn validate_certified_surface(program: &Program, diagnostics: &mut Vec<Diagnostic>) {
    // A sealed type without variants has no values: neither the checker nor the parser rejects
    // it, and the certified data model does not accept it.
    for declaration in &program.types {
        if declaration.kind == TypeKind::Sealed && declaration.cases.is_empty() {
            diagnostics.push(Diagnostic::error(
                "E8302",
                format!("Sealed type `{}` has no variants.", declaration.name),
                declaration.span.clone(),
            ));
        }
    }
}

/// E8307: a certified program that accesses the database reads its URL from a non-optional
/// runtime secret declared for the configured variable (`[database.main].env`).
fn validate_database_credential(
    program: &Program,
    config: &ProjectConfig,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let configured_variable = config.database_url_env.as_deref();
    let database_environment = configured_variable.and_then(|variable| {
        program
            .environment
            .iter()
            .find(|declaration| declaration.variable == variable)
    });
    match database_environment {
        Some(declaration)
            if configured_variable.is_some_and(valid_environment_name)
                && !declaration.optional
                && declaration.default.is_none()
                && declaration.secret
                && !declaration.public
                && matches!(declaration.lifecycle.as_str(), "runtime" | "compile_and_runtime") => {}
        _ => diagnostics.push(
            Diagnostic::error(
                "E8307",
                "The certified database runtime requires a non-optional runtime secret declaration for the configured database environment variable.",
                Span::default(),
            )
            .with_hint(
                "Declare `runtime secret val databaseUrl: DatabaseUrl = env(\"DATABASE_URL\")` and reference the same variable from `[database.main].env`.",
            ),
        ),
    }
}

/// What keeps the web frontend of a verified program from being generated (E8311): the same
/// plan `backend::js::page::web_files` and the SSR build from, so `generate` never fails.
///
/// A function's "uses an `environment` value" problem is left out when E4102 already reported
/// every value it reads (`typed`).
fn frontend_diagnostics(
    core: &CoreProgram,
    program: &Program,
    target: &str,
    profile: &str,
    typed: &crate::core::analysis::TypedDiagnostics,
) -> Vec<Diagnostic> {
    use crate::backend::js::plan::FrontendProblem;
    crate::backend::js::plan::program_problems(core, program, target)
        .into_iter()
        .filter(|problem| match problem {
            FrontendProblem::Unsupported { function, what, .. }
                if *what == crate::backend::js::plan::ENVIRONMENT_PROBLEM =>
            {
                !reported_as_secret(core, function, typed)
            }
            _ => true,
        })
        .map(|problem| {
            let span = problem
                .function()
                .and_then(|name| program.functions.iter().find(|item| item.name == name))
                .map(|item| item.span.clone())
                .unwrap_or_default();
            Diagnostic::error("E8311", problem.message_for(profile), span)
        })
        .collect()
}

/// Checks of every target whose server is generated by the Rust backend (certified or not):
/// endpoint signatures must be wire types and `Duration` cannot be stored (E8309).
fn validate_server_surface(
    program: &Program,
    database: &DatabaseSchema,
    profile: &str,
    database_types: bool,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for function in &program.functions {
        if function.domain == Domain::Server && !function.component {
            for type_ref in function
                .parameters
                .iter()
                .map(Parameter::declared_type)
                .chain(std::iter::once(function.declared_return()))
            {
                let Some(parsed) =
                    parse_declared_type(&type_ref.display(), &function.span, diagnostics)
                else {
                    continue;
                };
                if !v2_wire_type(&parsed, program, &mut BTreeSet::new()) {
                    diagnostics.push(Diagnostic::error(
                        "E8309",
                        format!(
                            "Endpoint `{}` uses `{}`, which cannot cross the wire in profile {}.",
                            function.name,
                            render_type_expr(&parsed),
                            profile
                        ),
                        function.span.clone(),
                    ));
                }
            }
        }
    }

    if !database_types {
        return;
    }

    // `Duration` has no database mapping: neither entity fields nor schema columns may use it.
    for declaration in &program.types {
        if declaration.kind != TypeKind::Entity {
            continue;
        }
        for field in &declaration.fields {
            let Some(parsed) =
                parse_declared_type(&field.type_ref.display(), &field.span, diagnostics)
            else {
                continue;
            };
            if mentions_duration(&parsed, program, 0) {
                diagnostics.push(Diagnostic::error(
                    "E8309",
                    format!(
                        "Entity field `{}.{}` uses `{}`, which has no database mapping in profile {}.",
                        declaration.name,
                        field.name,
                        render_type_expr(&parsed),
                        profile
                    ),
                    field.span.clone(),
                ));
            }
        }
    }
    // Only the tables the program may use: those mapped to a declared model. A table named by a
    // database operation is one of them, since the type checker requires its model to be a
    // declared entity (E7103).
    let used = database
        .tables
        .values()
        .filter(|table| program.types.iter().any(|item| item.name == table.model));
    for table in used {
        for column in table.columns.values() {
            let Ok(parsed) = parse_type_text(&column.type_name, &Span::default()) else {
                diagnostics.push(Diagnostic::error(
                    "E5101",
                    format!(
                        "Column `{}.{}` has type `{}`, which cannot be parsed as a type.",
                        table.name, column.name, column.type_name
                    ),
                    Span::default(),
                ));
                continue;
            };
            if mentions_duration(&parsed, program, 0) {
                diagnostics.push(Diagnostic::error(
                    "E8309",
                    format!(
                        "Column `{}.{}` uses `{}`, which has no database mapping in profile {}.",
                        table.name, column.name, column.type_name, profile
                    ),
                    Span::default(),
                ));
            }
        }
    }
}

/// The location of the function `name`, or the default span when there is none.
fn function_span(program: &Program, name: &str) -> Span {
    program
        .functions
        .iter()
        .find(|item| item.name == name)
        .map(|item| item.span.clone())
        .unwrap_or_default()
}

/// How a server problem names its function: the endpoint itself, or a function it reaches.
fn endpoint_subject(function: &str, endpoint: &str) -> String {
    if function == endpoint {
        format!("Endpoint `{endpoint}`")
    } else {
        format!("Function `{function}`, reachable from endpoint `{endpoint}`,")
    }
}

/// Whether a backend's "uses an `environment` value" problem was already reported as E4102
/// for every value `function` reads (so it is not reported twice).
fn already_reported(
    core: &CoreProgram,
    function: &str,
    what: &str,
    typed: &crate::core::analysis::TypedDiagnostics,
) -> bool {
    what == crate::backend::rust::ENVIRONMENT_PROBLEM && reported_as_secret(core, function, typed)
}

/// What keeps the Rust backend from generating the server of a verified program: E8310 for
/// code or columns the profile does not support, E9001 for inconsistencies. Run on the core
/// IR so `generate` never fails on a program `verify` accepted.
fn rust_server_diagnostics(
    core: &CoreProgram,
    program: &Program,
    database: &DatabaseSchema,
    typed: &crate::core::analysis::TypedDiagnostics,
) -> Vec<Diagnostic> {
    use crate::backend::rust::ServerProblem;
    crate::backend::rust::server_problems(core, database)
        .into_iter()
        .filter(|problem| match problem {
            ServerProblem::Unsupported { function, what, .. } => {
                !already_reported(core, function, what, typed)
            }
            _ => true,
        })
        .map(|problem| {
            let span = match &problem {
                ServerProblem::Unsupported { function, .. }
                | ServerProblem::Bodiless { function, .. } => function_span(program, function),
                ServerProblem::Database(error) => {
                    let model = error
                        .model
                        .as_ref()
                        .and_then(|model| program.types.iter().find(|item| item.name == *model));
                    let field = model
                        .zip(error.column.as_ref())
                        .and_then(|(model, column)| {
                            model.fields.iter().find(|field| field.name == *column)
                        });
                    field
                        .map(|field| field.span.clone())
                        .or_else(|| model.map(|model| model.span.clone()))
                        .unwrap_or_default()
                }
                ServerProblem::Internal(_) => Span::default(),
            };
            let code = match problem {
                ServerProblem::Internal(_) => "E9001",
                _ => "E8310",
            };
            Diagnostic::error(code, problem.message(), span)
        })
        .collect()
}

/// What keeps the first Cloudflare Worker backend from generating an endpoint. The profile is
/// intentionally narrower than the Rust backend: pure RPC functions are direct IR → Wasm,
/// while database and environment bindings wait for an explicit Cloudflare host ABI.
fn cloudflare_server_diagnostics(
    core: &CoreProgram,
    program: &Program,
    database: &DatabaseSchema,
    typed: &crate::core::analysis::TypedDiagnostics,
) -> Vec<Diagnostic> {
    use crate::backend::rust::ServerProblem;
    let profile = crate::cloudflare::PROFILE_VERSION;
    let mut diagnostics = Vec::new();
    let mut rejected_functions = BTreeSet::new();
    for problem in crate::backend::rust::server_problems(core, database) {
        let (function, message) = match &problem {
            ServerProblem::Unsupported {
                function,
                endpoint,
                what,
            } => {
                if already_reported(core, function, what, typed) {
                    continue;
                }
                (
                    function,
                    format!(
                        "{} uses {what}, which profile {profile} does not support.",
                        endpoint_subject(function, endpoint)
                    ),
                )
            }
            ServerProblem::Bodiless { function, endpoint } => (
                function,
                format!(
                    "{} has no body, so profile {profile} cannot run it.",
                    endpoint_subject(function, endpoint)
                ),
            ),
            // The first Worker profile rejects every database operation below, so Rust/Postgres
            // mapping diagnostics would be irrelevant and misleading here.
            ServerProblem::Database(_) => continue,
            ServerProblem::Internal(message) => {
                diagnostics.push(Diagnostic::error(
                    "E9001",
                    format!("Internal compiler error: {message}. This is a bug in bindc."),
                    Span::default(),
                ));
                continue;
            }
        };
        rejected_functions.insert(function.clone());
        diagnostics.push(Diagnostic::error(
            "E8310",
            message,
            function_span(program, function),
        ));
    }

    if let Some(operation) = crate::core::analysis::database_operations(core).first() {
        rejected_functions.insert(operation.function.clone());
        diagnostics.push(
            Diagnostic::error(
                "E8310",
                format!(
                    "Endpoint code uses `db.{}.{}`. Profile {profile} does not yet choose between Cloudflare D1 and PostgreSQL through Hyperdrive, so it refuses to change database semantics implicitly.",
                    operation.table, operation.method,
                ),
                function_span(program, &operation.function),
            )
            .with_hint(
                "Use a pure endpoint for this first Worker profile; the next profile will add an explicit D1/Hyperdrive binding.",
            ),
        );
    }
    for problem in crate::backend::wasm::problems(core) {
        if rejected_functions.contains(&problem.function) {
            continue;
        }
        let subject = if problem.function == problem.endpoint {
            format!("Endpoint `{}`", problem.endpoint)
        } else {
            format!(
                "Function `{}` (reachable from endpoint `{}`)",
                problem.function, problem.endpoint
            )
        };
        diagnostics.push(
            Diagnostic::error(
                "E8312",
                format!(
                    "{subject} {}. Profile {profile} refuses to generate code that would change Bind semantics.",
                    problem.reason,
                ),
                function_span(program, &problem.function),
            )
            .with_hint(
                "Use Boolean, Double, Float, or Unit in pure, non-recursive server computation; unsupported constructs must be implemented by the Wasm backend before they can compile.",
            ),
        );
    }
    diagnostics
}

/// Whether E4102 reported every `environment` value `function` reads.
fn reported_as_secret(
    core: &CoreProgram,
    function: &str,
    typed: &crate::core::analysis::TypedDiagnostics,
) -> bool {
    core.functions
        .iter()
        .find(|item| item.name == function)
        .is_some_and(|item| typed.reports_environment_of(item))
}

/// Parses a type the checker already accepted; a failure is a compiler bug (E9001).
fn parse_declared_type(
    text: &str,
    span: &Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<TypeExpr> {
    match parse_type_text(text, span) {
        Ok(parsed) => Some(parsed),
        Err(_) => {
            diagnostics.push(Diagnostic::error(
                "E9001",
                format!(
                    "Internal compiler error: the verified type `{text}` cannot be parsed again. This is a bug in bindc."
                ),
                span.clone(),
            ));
            None
        }
    }
}

/// Wire types of the Rust server: the scalars of the certified wire format (everything but
/// functions, `Duration` and `Long`, which the JS frontend cannot hold exactly); `T?`, `List`,
/// `Set`, `Map` and `Result` of wire types; user types (records, entities, distinct types, enum
/// and sealed classes, with or without fields) whose fields are wire types, generic ones at
/// wire type arguments.
fn v2_wire_type(ty: &TypeExpr, program: &Program, visiting: &mut BTreeSet<String>) -> bool {
    wire_type_in(ty, program, &BTreeSet::new(), visiting)
}

/// [`v2_wire_type`] inside a generic declaration whose type parameters are `params`: they stand
/// for the type arguments, which the use site checks.
fn wire_type_in(
    ty: &TypeExpr,
    program: &Program,
    params: &BTreeSet<String>,
    visiting: &mut BTreeSet<String>,
) -> bool {
    const SCALARS: &[&str] = &[
        "Unit", "String", "Int", "Float", "Double", "Decimal", "Boolean", "UUID", "Date",
        "DateTime", "Instant", "Money", "Email", "Url",
    ];
    match ty {
        TypeExpr::Function { .. } => false,
        TypeExpr::Nullable(inner) => wire_type_in(inner, program, params, visiting),
        TypeExpr::Named { name, args, .. } => {
            if args.is_empty() && (SCALARS.contains(&name.as_str()) || params.contains(name)) {
                return true;
            }
            let all_wire = |visiting: &mut BTreeSet<String>| {
                args.iter()
                    .all(|arg| wire_type_in(arg, program, params, visiting))
            };
            let builtin_arity = match name.as_str() {
                "List" | "Set" => Some(1),
                "Map" | "Result" => Some(2),
                _ => None,
            };
            let Some(declaration) = program.types.iter().find(|item| item.name == *name) else {
                return builtin_arity == Some(args.len()) && all_wire(visiting);
            };
            if args.len() != declaration.type_params.len() || !all_wire(visiting) {
                return false;
            }
            if !visiting.insert(name.clone()) {
                // A recursive occurrence: decided by the enclosing check.
                return true;
            }
            let inner: BTreeSet<String> = declaration.type_params.iter().cloned().collect();
            let field_is_wire =
                |type_ref: &TypeRef, span: &Span, visiting: &mut BTreeSet<String>| {
                    parse_type_text(&type_ref.display(), span)
                        .is_ok_and(|parsed| wire_type_in(&parsed, program, &inner, visiting))
                };
            let wire = match declaration.kind {
                TypeKind::Data | TypeKind::Entity => declaration
                    .fields
                    .iter()
                    .all(|field| field_is_wire(&field.type_ref, &field.span, visiting)),
                TypeKind::Distinct => declaration.underlying.as_ref().is_some_and(|underlying| {
                    field_is_wire(underlying, &declaration.span, visiting)
                }),
                TypeKind::Enum | TypeKind::Sealed => declaration.cases.iter().all(|case| {
                    case.fields
                        .iter()
                        .all(|field| field_is_wire(&field.type_ref, &field.span, visiting))
                }),
            };
            visiting.remove(name);
            wire
        }
    }
}

/// Whether `ty` mentions `Duration`, directly or through a `distinct type`.
fn mentions_duration(ty: &TypeExpr, program: &Program, depth: usize) -> bool {
    if depth > program.types.len() {
        return false;
    }
    match ty {
        TypeExpr::Nullable(inner) => mentions_duration(inner, program, depth),
        TypeExpr::Function { params, result, .. } => {
            params
                .iter()
                .any(|param| mentions_duration(param, program, depth))
                || mentions_duration(result, program, depth)
        }
        TypeExpr::Named { name, args, .. } => {
            name == "Duration"
                || args
                    .iter()
                    .any(|arg| mentions_duration(arg, program, depth))
                || program
                    .types
                    .iter()
                    .find(|item| item.name == *name && item.kind == TypeKind::Distinct)
                    .and_then(|item| item.underlying.as_ref())
                    .and_then(|underlying| {
                        parse_type_text(&underlying.display(), &Span::default()).ok()
                    })
                    .is_some_and(|parsed| mentions_duration(&parsed, program, depth + 1))
        }
    }
}

/// A type as written in Bind source (`List<Int>?`, `(Int) -> Int`).
fn render_type_expr(ty: &TypeExpr) -> String {
    match ty {
        TypeExpr::Named { name, args, .. } if args.is_empty() => name.clone(),
        TypeExpr::Named { name, args, .. } => format!(
            "{name}<{}>",
            args.iter()
                .map(render_type_expr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        TypeExpr::Nullable(inner) => match **inner {
            TypeExpr::Function { .. } => format!("({})?", render_type_expr(inner)),
            _ => format!("{}?", render_type_expr(inner)),
        },
        TypeExpr::Function { params, result, .. } => format!(
            "({}) -> {}",
            params
                .iter()
                .map(render_type_expr)
                .collect::<Vec<_>>()
                .join(", "),
            render_type_expr(result)
        ),
    }
}

fn validate_environment(
    program: &Program,
    config: &ProjectConfig,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<EnvironmentStatus> {
    let mut result = Vec::new();
    for declaration in &program.environment {
        let required = !declaration.optional && declaration.default.is_none();
        let value = env::var(&declaration.variable).ok();
        let resolved = value.is_some() || declaration.default.is_some();
        // A `runtime` value is provided by the deployment, not by the build machine: it is only
        // checked when it happens to be set. Values used at compile time must be defined.
        if required && value.is_none() && declaration.lifecycle != "runtime" {
            diagnostics.push(
                Diagnostic::error(
                    "E4001",
                    format!(
                        "Required environment variable is not defined:\n    {}",
                        declaration.variable
                    ),
                    declaration.span.clone(),
                )
                .with_path(vec![
                    format!("environment.{}", declaration.name),
                    format!("env.{}", declaration.variable),
                ]),
            );
        }
        if let Some(value) = &value
            && let Err(reason) = validate_environment_value(&declaration.type_ref.name, value)
        {
            diagnostics.push(Diagnostic::error("E4002", format!("Environment variable `{}` cannot be decoded as {}.\nReceived: {:?}\nReason: {reason}", declaration.variable, declaration.type_ref.name, if declaration.secret { "<redacted>" } else { value }), declaration.span.clone()));
        }
        result.push(EnvironmentStatus {
            variable: declaration.variable.clone(),
            type_name: declaration.type_ref.display(),
            lifecycle: declaration.lifecycle.clone(),
            required,
            resolved,
            secret: declaration.secret,
            public: declaration.public,
        });
    }
    if config.database_verification != "static"
        && let Some(variable) = &config.database_url_env
        && env::var(variable).is_err()
    {
        diagnostics.push(Diagnostic::error(
            "E4001",
            format!(
                "Database verification mode `{}` requires environment variable `{variable}`.",
                config.database_verification
            ),
            Span::default(),
        ));
    }
    result
}

fn validate_environment_value(type_name: &str, value: &str) -> Result<(), &'static str> {
    match type_name {
        "Int" | "Long" | "Port" => value
            .parse::<i64>()
            .map(|_| ())
            .map_err(|_| "expected an integer"),
        "Boolean" => value
            .parse::<bool>()
            .map(|_| ())
            .map_err(|_| "expected true or false"),
        "Url" | "DatabaseUrl" => {
            if value.contains("://") {
                Ok(())
            } else {
                Err("expected a URL with a scheme")
            }
        }
        "Duration" => {
            if value.ends_with('s') || value.ends_with("ms") {
                Ok(())
            } else {
                Err("expected a duration such as 30s or 250ms")
            }
        }
        _ => Ok(()),
    }
}

/// Every `.bind`/`.bd` file under `root`, sorted. Symbolic links inside the source directory
/// are refused: a link could pull files from outside the project into the build (and into
/// `sourceHash`), or loop.
fn source_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    fn visit(path: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
        for entry in fs::read_dir(path)
            .map_err(|error| format!("Cannot read source directory {}: {error}", path.display()))?
        {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
            if kind.is_symlink() {
                return Err(format!(
                    "Symbolic link {} in the source directory: Bind does not follow links in sources; move or copy the files instead.",
                    path.display()
                ));
            } else if kind.is_dir() {
                visit(&path, output)?;
            } else if path
                .extension()
                .is_some_and(|ext| ext == "bind" || ext == "bd")
            {
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

pub fn ir_json(app: &VerifiedApplication) -> String {
    let mut output = String::from("{\n  \"irVersion\": \"0.1\",");
    output.push_str(&format!(
        "\n  \"project\": {},\n  \"target\": {},\n  \"sourceHash\": {},",
        json(&app.project_name),
        json(&app.target),
        json(&app.source_hash)
    ));
    output.push_str("\n  \"functions\": [");
    for (index, function) in app.program.functions.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str(&format!("\n    {{\"name\":{},\"domain\":{},\"returnType\":{},\"suspend\":{},\"effects\":{},\"requires\":{}}}", json(&function.name), json(function.domain.as_str()), json(&function.declared_return().display()), function.suspend, json_array(&function.effects), json_array(&function.requires)));
    }
    output.push_str("\n  ],\n  \"rpcContracts\": [");
    for (index, rpc) in app.rpc_contracts.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str(&format!(
            "\n    {{\"name\":{},\"path\":{},\"returnType\":{}}}",
            json(&rpc.name),
            json(&rpc.path),
            json(&rpc.return_type.display())
        ));
    }
    output.push_str("\n  ]\n}\n");
    output
}

#[cfg(test)]
mod tests;
