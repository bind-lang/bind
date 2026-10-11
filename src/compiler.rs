use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::ast::*;
use crate::config::ProjectConfig;
use crate::core::ir::CoreProgram;
use crate::diagnostic::{Diagnostic, Severity, Span, has_errors};
use crate::lexer::{Token, TokenKind, lex};
use crate::parser::expr::parse_type_text;
use crate::parser::parse;
use crate::schema::{DatabaseSchema, closest, stable_hash};
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

#[derive(Clone, Debug)]
pub struct DatabaseOperation {
    pub function: String,
    pub table: String,
    pub method: String,
    pub effect: String,
}

#[derive(Clone, Debug, Default)]
pub struct ApplicationGraph {
    pub edges: BTreeMap<String, BTreeSet<String>>,
}

impl ApplicationGraph {
    pub fn add_node(&mut self, node: impl Into<String>) {
        self.edges.entry(node.into()).or_default();
    }
    pub fn add_edge(&mut self, from: impl Into<String>, to: impl Into<String>) {
        self.edges.entry(from.into()).or_default().insert(to.into());
    }

    pub fn tree(&self, root: &str) -> String {
        let mut output = String::new();
        let mut seen = BTreeSet::new();
        self.render(root, "", true, &mut seen, &mut output);
        output
    }

    fn render(
        &self,
        node: &str,
        prefix: &str,
        last: bool,
        seen: &mut BTreeSet<String>,
        output: &mut String,
    ) {
        if prefix.is_empty() {
            output.push_str(node);
            output.push('\n');
        } else {
            output.push_str(prefix);
            output.push_str(if last { "└── " } else { "├── " });
            output.push_str(node);
            output.push('\n');
        }
        if !seen.insert(node.to_string()) {
            return;
        }
        let children: Vec<_> = self
            .edges
            .get(node)
            .into_iter()
            .flat_map(|items| items.iter())
            .collect();
        for (index, child) in children.iter().enumerate() {
            let next = if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}{}", if last { "    " } else { "│   " })
            };
            let child_prefix = if prefix.is_empty() {
                String::new()
            } else {
                next
            };
            if prefix.is_empty() {
                let marker = if index + 1 == children.len() {
                    "└── "
                } else {
                    "├── "
                };
                output.push_str(marker);
                output.push_str(child);
                output.push('\n');
                if seen.insert((*child).clone()) {
                    self.render_children(
                        child,
                        if index + 1 == children.len() {
                            "    "
                        } else {
                            "│   "
                        },
                        seen,
                        output,
                    );
                }
            } else {
                self.render(
                    child,
                    &child_prefix,
                    index + 1 == children.len(),
                    seen,
                    output,
                );
            }
        }
    }

    fn render_children(
        &self,
        node: &str,
        prefix: &str,
        seen: &mut BTreeSet<String>,
        output: &mut String,
    ) {
        let children: Vec<_> = self
            .edges
            .get(node)
            .into_iter()
            .flat_map(|items| items.iter())
            .collect();
        for (index, child) in children.iter().enumerate() {
            output.push_str(prefix);
            output.push_str(if index + 1 == children.len() {
                "└── "
            } else {
                "├── "
            });
            output.push_str(child);
            output.push('\n');
            if seen.insert((*child).clone()) {
                let next = format!(
                    "{prefix}{}",
                    if index + 1 == children.len() {
                        "    "
                    } else {
                        "│   "
                    }
                );
                self.render_children(child, &next, seen, output);
            }
        }
    }

    pub fn path_to(&self, target: &str) -> Vec<String> {
        let mut reverse: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (from, targets) in &self.edges {
            for to in targets {
                reverse.entry(to).or_default().push(from);
            }
        }
        let mut queue = VecDeque::from([(target, vec![target.to_string()])]);
        let mut seen = BTreeSet::new();
        while let Some((node, path)) = queue.pop_front() {
            if !seen.insert(node) {
                continue;
            }
            let parents = reverse.get(node).cloned().unwrap_or_default();
            if parents.is_empty() {
                return path.into_iter().rev().collect();
            }
            for parent in parents {
                let mut next = path.clone();
                next.push(parent.to_string());
                queue.push_back((parent, next));
            }
        }
        vec![target.into()]
    }

    pub fn impact(&self, target: &str) -> Vec<String> {
        let mut reverse: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (from, targets) in &self.edges {
            for to in targets {
                reverse.entry(to).or_default().push(from);
            }
        }
        let mut queue = VecDeque::from([target]);
        let mut found = BTreeSet::new();
        while let Some(node) = queue.pop_front() {
            for parent in reverse.get(node).cloned().unwrap_or_default() {
                if found.insert(parent.to_string()) {
                    queue.push_back(parent);
                }
            }
        }
        found.into_iter().collect()
    }
}

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

pub fn verify(config: &ProjectConfig, target: &str) -> CompileResult {
    let mut diagnostics = Vec::new();
    let files = match source_files(&config.source) {
        Ok(files) => files,
        Err(error) => {
            return CompileResult {
                application: None,
                diagnostics: vec![Diagnostic::error("E0001", error, Span::default())],
                module_count: 0,
            };
        }
    };
    let mut program = Program::default();
    let mut source_canonical = String::new();
    for path in &files {
        match fs::read_to_string(path) {
            Ok(source) => {
                source_canonical.push_str(&path.to_string_lossy());
                source_canonical.push('\0');
                source_canonical.push_str(&source);
                let relative = path
                    .strip_prefix(&config.root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                let (tokens, mut lex_diagnostics) = lex(&relative, &source);
                diagnostics.append(&mut lex_diagnostics);
                let (module, mut parse_diagnostics) = parse(tokens);
                diagnostics.append(&mut parse_diagnostics);
                program.merge(module);
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
    if has_errors(&diagnostics) {
        return CompileResult {
            application: None,
            diagnostics,
            module_count: files.len(),
        };
    }

    let database = match &config.database_schema {
        Some(path) => match DatabaseSchema::load(path) {
            Ok(schema) => schema,
            Err(error) => {
                diagnostics.push(Diagnostic::error("E5101", error, Span::default()));
                DatabaseSchema::default()
            }
        },
        None => DatabaseSchema::default(),
    };
    let Some(target_config) = config.targets.get(target) else {
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
        return CompileResult {
            application: None,
            diagnostics,
            module_count: files.len(),
        };
    };

    let (typed, type_diagnostics) = crate::types::check(&program, &database);
    let type_errors = has_errors(&type_diagnostics);
    diagnostics.extend(type_diagnostics);
    if type_errors {
        return CompileResult {
            application: None,
            diagnostics,
            module_count: files.len(),
        };
    }
    let mut graph = ApplicationGraph::default();
    let type_map: BTreeMap<_, _> = program
        .types
        .iter()
        .map(|item| (item.name.clone(), item))
        .collect();
    let function_map: BTreeMap<_, _> = program
        .functions
        .iter()
        .map(|item| (item.name.clone(), item))
        .collect();
    validate_declarations(&program, &type_map, &function_map, &mut diagnostics);

    let mut required_capabilities = BTreeSet::new();
    let mut database_operations = Vec::new();
    let mut pending_database_errors = Vec::new();
    for function in &program.functions {
        graph.add_node(function.name.clone());
        match function.domain {
            Domain::Client => {
                required_capabilities.insert("ui.dom".into());
            }
            Domain::Server => {
                required_capabilities.insert("server.http".into());
            }
            _ => {}
        }
        for label in typed.effects.get(&function.name).into_iter().flatten() {
            if label != "server.call" {
                required_capabilities.insert(label.clone());
            }
        }
        analyze_function(
            function,
            &program,
            &type_map,
            &function_map,
            &database,
            &mut graph,
            &mut database_operations,
            &mut diagnostics,
            &mut pending_database_errors,
        );
        if config.strict && function.body.iter().any(|token| token.text == "!!") {
            diagnostics.push(Diagnostic::warning("W2101", format!("Function `{}` uses a forced nullable unwrap (`!!`).", function.name), function.span.clone())
                .with_hint("Prefer explicit null handling; strict projects may promote this warning to an error."));
        }
    }

    let mut environment = validate_environment(&program, config, &mut diagnostics);
    environment.sort_by(|a, b| a.variable.cmp(&b.variable));
    for declaration in &program.environment {
        graph.add_edge(
            format!("environment.{}", declaration.name),
            format!("env.{}", declaration.variable),
        );
        if declaration.secret {
            for function in &program.functions {
                if function.domain == Domain::Client
                    && function
                        .body
                        .iter()
                        .any(|token| token.text == declaration.name)
                {
                    diagnostics.push(
                        Diagnostic::error(
                            "E4102",
                            format!(
                                "Secret value `{}` flows into client artifact.",
                                declaration.name
                            ),
                            function.span.clone(),
                        )
                        .with_path(vec![
                            format!("env.{}", declaration.variable),
                            format!("environment.{}", declaration.name),
                            function.name.clone(),
                            "browser bundle".into(),
                        ]),
                    );
                }
            }
        }
    }

    for (function, resource, mut diagnostic) in pending_database_errors {
        graph.add_edge(function.clone(), resource.clone());
        diagnostic.path = graph.path_to(&function);
        diagnostic.path.push(resource);
        diagnostics.push(diagnostic);
    }

    let provided: BTreeSet<_> = target_config.capabilities.iter().cloned().collect();
    let missing: Vec<_> = required_capabilities
        .difference(&provided)
        .cloned()
        .collect();
    if !missing.is_empty() {
        diagnostics.push(Diagnostic::error("E8101", format!("Target `{target}` cannot implement this application.\n\nMissing capabilities:\n    {}", missing.join("\n    ")), Span::default()));
    }
    let built_in_target = target_config.client == "elm"
        && target_config.server == "rust"
        && matches!(target_config.deployment.as_str(), "aws-lambda" | "vercel");
    if !built_in_target {
        diagnostics.push(Diagnostic::error("E8204", format!("No built-in generator can preserve the semantics of target `{target}` ({} + {}, deployment {}).", target_config.client, target_config.server, target_config.deployment), Span::default()));
    }
    if target_config.server == "rust" {
        validate_rust_server_surface(&program, &database, &database_operations, &mut diagnostics);
    }
    if target_config.certified {
        validate_certified_surface(&program, &database_operations, config, &mut diagnostics);
    }

    if config.warnings_as_errors {
        for item in &mut diagnostics {
            if item.severity == Severity::Warning {
                item.severity = Severity::Error;
            }
        }
    }
    let rpc_contracts = program
        .functions
        .iter()
        .filter(|item| item.domain == Domain::Server)
        .map(|function| RpcContract {
            name: function.name.clone(),
            parameters: function.parameters.clone(),
            return_type: function.declared_return(),
            path: format!("/api/{}", function.name),
        })
        .collect();
    // A program that passes every user-facing check is lowered to the core IR and validated.
    // This runs last so user diagnostics are never masked: an inconsistent IR is a compiler bug
    // (E9001) and stops verification before any code is generated.
    let core = if has_errors(&diagnostics) {
        None
    } else {
        let core = crate::core::lower::lower(&program, &typed, &database);
        let problems = crate::core::validate::validate(&core);
        if problems.is_empty() {
            let server_diagnostics = if target_config.server == "rust" {
                rust_server_diagnostics(&core, &program, &database)
            } else {
                Vec::new()
            };
            let usable = !has_errors(&server_diagnostics);
            diagnostics.extend(server_diagnostics);
            usable.then_some(core)
        } else {
            diagnostics.push(Diagnostic::error(
                "E9001",
                format!(
                    "Internal compiler error: the core IR is inconsistent ({}). This is a bug in bindc.",
                    problems.join("; ")
                ),
                Span::default(),
            ));
            None
        }
    };
    let application = core.map(|core| VerifiedApplication {
        project_name: config.name.clone(),
        project_version: config.version.clone(),
        target: target.into(),
        source_hash: stable_hash(&source_canonical),
        program,
        graph,
        environment,
        database,
        database_operations,
        rpc_contracts,
        required_capabilities,
        unsafe_operations: Vec::new(),
        core,
    });
    CompileResult {
        application,
        diagnostics,
        module_count: files.len(),
    }
}

fn validate_declarations(
    program: &Program,
    types: &BTreeMap<String, &TypeDecl>,
    functions: &BTreeMap<String, &Function>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut seen = BTreeSet::new();
    for item in &program.types {
        if !seen.insert(item.name.clone()) {
            diagnostics.push(Diagnostic::error(
                "E2001",
                format!("Type `{}` is declared more than once.", item.name),
                item.span.clone(),
            ));
        }
        if let Some(underlying) = &item.underlying {
            validate_type(underlying, types, &item.span, diagnostics);
        }
        for field in item
            .fields
            .iter()
            .chain(item.cases.iter().flat_map(|case| &case.fields))
        {
            if !mentions_type_param(&field.type_ref, &item.type_params) {
                validate_type(&field.type_ref, types, &field.span, diagnostics);
            }
        }
    }
    seen.clear();
    for item in &program.functions {
        if !seen.insert(item.name.clone()) {
            diagnostics.push(Diagnostic::error(
                "E2002",
                format!("Function `{}` is declared more than once.", item.name),
                item.span.clone(),
            ));
        }
        if let Some(return_type) = &item.return_type
            && !mentions_type_param(return_type, &item.type_params)
        {
            validate_type(return_type, types, &item.span, diagnostics);
        }
        for parameter in &item.parameters {
            if let Some(type_ref) = &parameter.type_ref
                && !mentions_type_param(type_ref, &item.type_params)
            {
                validate_type(type_ref, types, &item.span, diagnostics);
            }
        }
    }
    let _ = functions;
}

/// Type parameters and function types are verified by the type checker (`types`), not here.
fn mentions_type_param(type_ref: &TypeRef, params: &[String]) -> bool {
    type_ref.name.starts_with('(')
        || type_ref
            .name
            .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
            .any(|part| params.iter().any(|param| param == part))
}

fn validate_type(
    type_ref: &TypeRef,
    types: &BTreeMap<String, &TypeDecl>,
    span: &Span,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let base = type_ref
        .name
        .split(['<', ','])
        .next()
        .unwrap_or(&type_ref.name)
        .trim_end_matches('?');
    let builtins = [
        "Unit",
        "String",
        "Int",
        "Long",
        "Float",
        "Double",
        "Decimal",
        "Boolean",
        "UUID",
        "Date",
        "DateTime",
        "Instant",
        "Duration",
        "Money",
        "Email",
        "Url",
        "DatabaseUrl",
        "Port",
        "Secret",
        "Result",
        "List",
        "Set",
        "Map",
    ];
    if !builtins.contains(&base) && !types.contains_key(base) {
        diagnostics.push(Diagnostic::error(
            "E2003",
            format!("Unknown type `{}`.", type_ref.name),
            span.clone(),
        ));
    }
}

fn validate_certified_surface(
    program: &Program,
    operations: &[DatabaseOperation],
    config: &ProjectConfig,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let components: Vec<_> = program
        .functions
        .iter()
        .filter(|function| function.domain == Domain::Client && function.component)
        .collect();
    if components.len() > 1 {
        diagnostics.push(Diagnostic::error(
            "E8301",
            "The certified web profile currently accepts one client component as its application entry point.",
            components[1].span.clone(),
        ));
    }

    for declaration in &program.types {
        if declaration.kind == TypeKind::Sealed && declaration.variants.is_empty() {
            diagnostics.push(Diagnostic::error(
                "E8302",
                format!("Sealed type `{}` has no variants.", declaration.name),
                declaration.span.clone(),
            ));
        }
        for field in &declaration.fields {
            if field.mutable {
                diagnostics.push(Diagnostic::error(
                    "E8308",
                    format!(
                        "Mutable field `{}.{}` is outside the immutable certified data model.",
                        declaration.name, field.name
                    ),
                    field.span.clone(),
                ));
            }
            if !certified_wire_type(&field.type_ref.name, program) {
                diagnostics.push(Diagnostic::error(
                    "E8303",
                    format!(
                        "Type `{}` is not supported by the certified wire format yet.",
                        field.type_ref.display()
                    ),
                    field.span.clone(),
                ));
            }
        }
    }

    for function in &program.functions {
        // Client functions are compiled by the token-based Elm generator, whose wire format
        // is the original one. Endpoints are checked against the v2 wire types (E8309, for
        // every Rust-server target); other server code may use the whole language.
        if function.domain == Domain::Client {
            for type_ref in function
                .parameters
                .iter()
                .map(Parameter::declared_type)
                .chain(std::iter::once(function.declared_return()))
            {
                if !certified_wire_type(&type_ref.name, program) {
                    diagnostics.push(Diagnostic::error(
                        "E8303",
                        format!(
                            "Function `{}` exposes unsupported certified wire type `{}`.",
                            function.name,
                            type_ref.display()
                        ),
                        function.span.clone(),
                    ));
                }
            }
        }
    }

    for component in components {
        let server_calls = component
            .body
            .windows(3)
            .filter(|window| {
                window[0].text == "server"
                    && window[1].text == "."
                    && window[2].kind == TokenKind::Identifier
            })
            .count();
        if server_calls > 1 || !component.body.iter().any(|token| token.text == "UI") {
            diagnostics.push(Diagnostic::error(
                "E8306",
                format!(
                    "Certified component `{}` must contain at most one bound server call and exactly one UI block.",
                    component.name
                ),
                component.span.clone(),
            ));
        }
    }

    if !operations.is_empty() {
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
                    && matches!(declaration.lifecycle.as_str(), "runtime" | "compile_and_runtime") =>
            {
            }
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
}

/// Checks of every target whose server is generated by the Rust backend (certified or not):
/// endpoint signatures must be wire types and `Duration` cannot be stored (E8309).
fn validate_rust_server_surface(
    program: &Program,
    database: &DatabaseSchema,
    operations: &[DatabaseOperation],
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
                            crate::certified::profile_version()
                        ),
                        function.span.clone(),
                    ));
                }
            }
        }
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
                        crate::certified::profile_version()
                    ),
                    field.span.clone(),
                ));
            }
        }
    }
    // Only the tables the program uses: one mapped to a declared model or named by a database
    // operation.
    let used = database.tables.values().filter(|table| {
        program.types.iter().any(|item| item.name == table.model)
            || operations
                .iter()
                .any(|operation| operation.table == table.name)
    });
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
                        table.name,
                        column.name,
                        column.type_name,
                        crate::certified::profile_version()
                    ),
                    Span::default(),
                ));
            }
        }
    }
}

/// What keeps the Rust backend from generating the server of a verified program: E8310 for
/// code or columns the profile does not support, E9001 for inconsistencies. Run on the core
/// IR so `generate` never fails on a program `verify` accepted.
fn rust_server_diagnostics(
    core: &CoreProgram,
    program: &Program,
    database: &DatabaseSchema,
) -> Vec<Diagnostic> {
    use crate::backend::rust::ServerProblem;
    crate::backend::rust::server_problems(core, database)
        .into_iter()
        .map(|problem| {
            let span = match &problem {
                ServerProblem::Unsupported { function, .. }
                | ServerProblem::Bodiless { function, .. } => program
                    .functions
                    .iter()
                    .find(|item| item.name == *function)
                    .map(|item| item.span.clone()),
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
                }
                ServerProblem::Internal(_) => None,
            };
            let code = match problem {
                ServerProblem::Internal(_) => "E9001",
                _ => "E8310",
            };
            Diagnostic::error(code, problem.message(), span.unwrap_or_default())
        })
        .collect()
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

fn valid_environment_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn certified_wire_type(name: &str, program: &Program) -> bool {
    let base = name.split('<').next().unwrap_or(name).trim_end_matches('?');
    let scalar = [
        "Unit", "String", "Int", "Float", "Double", "Decimal", "Boolean", "UUID", "Date",
        "DateTime", "Instant", "Duration", "Money", "Email", "Url",
    ];
    if scalar.contains(&base) {
        return true;
    }
    if base == "List" {
        return generic_arguments(name).is_some_and(|arguments| {
            arguments.len() == 1 && certified_wire_type(&arguments[0], program)
        });
    }
    program
        .types
        .iter()
        .any(|declaration| declaration.name == base)
}

/// Wire types of profile rust-elm-web-v2: the scalars of the certified wire format except
/// `Duration`; `data class`/`entity` (and `distinct type`) whose fields are wire types;
/// `enum class`/`sealed class` whose variants have no fields; `List<T>` and `T?` of wire
/// types. Generic user types, functions, `Result`, `Set` and `Map` are not wire types.
fn v2_wire_type(ty: &TypeExpr, program: &Program, visiting: &mut BTreeSet<String>) -> bool {
    const SCALARS: &[&str] = &[
        "Unit", "String", "Int", "Float", "Double", "Decimal", "Boolean", "UUID", "Date",
        "DateTime", "Instant", "Money", "Email", "Url",
    ];
    match ty {
        TypeExpr::Function { .. } => false,
        TypeExpr::Nullable(inner) => v2_wire_type(inner, program, visiting),
        TypeExpr::Named { name, args, .. } => {
            if args.is_empty() && SCALARS.contains(&name.as_str()) {
                return true;
            }
            if name == "List" {
                return matches!(args.as_slice(), [inner] if v2_wire_type(inner, program, visiting));
            }
            let Some(declaration) = program.types.iter().find(|item| item.name == *name) else {
                return false;
            };
            if !args.is_empty() || !declaration.type_params.is_empty() {
                return false;
            }
            if !visiting.insert(name.clone()) {
                // A recursive occurrence: decided by the enclosing check.
                return true;
            }
            let field_is_wire =
                |type_ref: &TypeRef, span: &Span, visiting: &mut BTreeSet<String>| {
                    parse_type_text(&type_ref.display(), span)
                        .is_ok_and(|parsed| v2_wire_type(&parsed, program, visiting))
                };
            let wire = match declaration.kind {
                TypeKind::Data | TypeKind::Entity => declaration
                    .fields
                    .iter()
                    .all(|field| field_is_wire(&field.type_ref, &field.span, visiting)),
                TypeKind::Distinct => declaration.underlying.as_ref().is_some_and(|underlying| {
                    field_is_wire(underlying, &declaration.span, visiting)
                }),
                TypeKind::Enum | TypeKind::Sealed => {
                    declaration.cases.iter().all(|case| case.fields.is_empty())
                }
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

fn generic_arguments(name: &str) -> Option<Vec<String>> {
    let start = name.find('<')?;
    let end = name.rfind('>')?;
    let mut arguments = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for ch in name[start + 1..end].chars() {
        match ch {
            '<' => {
                depth += 1;
                current.push(ch);
            }
            '>' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                arguments.push(current.clone());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        arguments.push(current);
    }
    Some(arguments)
}

#[allow(clippy::too_many_arguments)]
fn analyze_function(
    function: &Function,
    program: &Program,
    types: &BTreeMap<String, &TypeDecl>,
    functions: &BTreeMap<String, &Function>,
    database: &DatabaseSchema,
    graph: &mut ApplicationGraph,
    operations: &mut Vec<DatabaseOperation>,
    diagnostics: &mut Vec<Diagnostic>,
    pending_database_errors: &mut Vec<(String, String, Diagnostic)>,
) {
    let body = &function.body;
    let mut index = 0;
    while index < body.len() {
        // Calls only feed the application graph: the type checker verifies them.
        let call_name = if body.get(index).is_some_and(|token| token.text == "server")
            && body.get(index + 1).is_some_and(|token| token.text == ".")
        {
            body.get(index + 2).map(|token| token.text.clone())
        } else if body.get(index + 1).is_some_and(|token| token.text == "(")
            && functions.contains_key(&body[index].text)
            && !(index >= 2 && body[index - 2].text == "server" && body[index - 1].text == ".")
        {
            Some(body[index].text.clone())
        } else {
            None
        };
        if let Some(call_name) = call_name
            && functions.contains_key(&call_name)
        {
            graph.add_edge(function.name.clone(), call_name);
        }
        if body.get(index).is_some_and(|token| token.text == "db")
            && body.get(index + 1).is_some_and(|token| token.text == ".")
        {
            let table_name = body
                .get(index + 2)
                .map(|token| token.text.clone())
                .unwrap_or_default();
            let method = body
                .get(index + 4)
                .map(|token| token.text.clone())
                .unwrap_or_default();
            let write = ["insert", "update", "delete", "upsert"].contains(&method.as_str());
            let effect = if write { "db.write" } else { "db.read" }.to_string();
            let assignments = if method == "insert" {
                insert_assignments(body, index + 5)
            } else {
                Vec::new()
            };
            operations.push(DatabaseOperation {
                function: function.name.clone(),
                table: table_name.clone(),
                method: method.clone(),
                effect: effect.clone(),
            });
            graph.add_edge(function.name.clone(), format!("database.main.{table_name}"));
            match database.tables.get(&table_name) {
                None => {
                    let hint = closest(&table_name, database.tables.keys())
                        .map(|name| format!("Did you mean `{name}`?"));
                    let mut diagnostic = Diagnostic::error(
                        "E5200",
                        format!("Database table `{table_name}` does not exist in database `main`."),
                        body[index].span.clone(),
                    );
                    if let Some(hint) = hint {
                        diagnostic = diagnostic.with_hint(hint);
                    }
                    pending_database_errors.push((
                        function.name.clone(),
                        format!("database.main.{table_name}"),
                        diagnostic,
                    ));
                }
                Some(table) => {
                    if !write && !["require", "find"].contains(&method.as_str()) {
                        diagnostics.push(Diagnostic::error(
                            "E8204",
                            format!("The built-in generators cannot lower database read method `{method}` without losing query semantics."),
                            body[index].span.clone(),
                        ));
                    }
                    if method == "insert" {
                        let assigned: BTreeSet<_> =
                            assignments.iter().map(|(name, _)| name.as_str()).collect();
                        for (name, _) in &assignments {
                            if !table.columns.contains_key(name) {
                                diagnostics.push(Diagnostic::error(
                                    "E5201",
                                    format!(
                                        "Column `{}.{name}` does not exist in database `main`.",
                                        table.name
                                    ),
                                    body[index].span.clone(),
                                ));
                            }
                        }
                        let missing: Vec<_> = table
                            .columns
                            .values()
                            .filter(|column| {
                                column.required
                                    && !column.nullable
                                    && !column.has_default
                                    && !assigned.contains(column.name.as_str())
                            })
                            .collect();
                        if !missing.is_empty() {
                            diagnostics.push(Diagnostic::error(
                                "E5303",
                                format!(
                                    "INSERT into `{}` is missing required field(s):\n    {}",
                                    table.name,
                                    missing
                                        .iter()
                                        .map(|column| format!(
                                            "{} : {} NOT NULL",
                                            column.name, column.type_name
                                        ))
                                        .collect::<Vec<_>>()
                                        .join("\n    ")
                                ),
                                body[index].span.clone(),
                            ));
                        }
                    } else if write {
                        diagnostics.push(Diagnostic::error(
                            "E8204",
                            format!("The built-in generators cannot lower database method `{method}` safely."),
                            body[index].span.clone(),
                        ));
                    }
                    if let Some(model) = types.get(&table.model) {
                        for field in &model.fields {
                            graph.add_edge(
                                function.name.clone(),
                                format!("database.main.{}.{}", table.name, field.name),
                            );
                            if !table.columns.contains_key(&field.name) {
                                let resource =
                                    format!("database.main.{}.{}", table.name, field.name);
                                let hint = closest(&field.name, table.columns.keys());
                                let mut diagnostic = Diagnostic::error(
                                    "E5201",
                                    format!(
                                        "Column `{}.{}` required by `{}` does not exist in database `main`.",
                                        table.name, field.name, model.name
                                    ),
                                    field.span.clone(),
                                );
                                if let Some(hint) = hint {
                                    diagnostic = diagnostic.with_hint(format!(
                                        "Did you mean `{}.{hint}`?",
                                        table.name
                                    ));
                                }
                                pending_database_errors.push((
                                    function.name.clone(),
                                    resource,
                                    diagnostic,
                                ));
                            } else if let Some(column) = table.columns.get(&field.name)
                                && !compatible(
                                    &field.type_ref,
                                    &TypeRef {
                                        name: column.type_name.clone(),
                                        nullable: column.nullable,
                                    },
                                )
                            {
                                diagnostics.push(Diagnostic::error("E5204", format!("Database type mismatch for `{}.{}`: schema is `{}{}`, application model is `{}`.", table.name, field.name, column.type_name, if column.nullable { "?" } else { "" }, field.type_ref.display()), field.span.clone()));
                            }
                        }
                    } else {
                        diagnostics.push(Diagnostic::error(
                            "E5202",
                            format!(
                                "Database table `{table_name}` maps to unknown model `{}`.",
                                table.model
                            ),
                            body[index].span.clone(),
                        ));
                    }
                    for window in body.windows(3) {
                        if window[0].text == "it"
                            && window[1].text == "."
                            && !table.columns.contains_key(&window[2].text)
                        {
                            pending_database_errors.push((
                                function.name.clone(),
                                format!("database.main.{}.{}", table.name, window[2].text),
                                Diagnostic::error(
                                    "E5201",
                                    format!(
                                        "Column `{}.{}` does not exist in database `main`.",
                                        table.name, window[2].text
                                    ),
                                    window[2].span.clone(),
                                ),
                            ));
                        }
                    }
                }
            }
        }
        index += 1;
    }
    validate_return_contract(function, diagnostics);
    let _ = program;
}

/// Declared return type as far as the legacy pass can trust it: absent
/// annotations and types mentioning the function's type parameters are
/// unknown here (the type checker already verified them).
fn known_return(function: &Function) -> Option<TypeRef> {
    let declared = function.return_type.as_ref()?;
    (!mentions_type_param(declared, &function.type_params)).then(|| declared.clone())
}

/// A declared non-`Unit` return needs a return expression. Whether the returned value has
/// the declared type is the type checker's job.
fn validate_return_contract(function: &Function, diagnostics: &mut Vec<Diagnostic>) {
    let Some(declared_return) = known_return(function) else {
        return;
    };
    if declared_return.name == "Unit" {
        return;
    }
    if function.body.is_empty() {
        diagnostics.push(Diagnostic::error(
            "E2301",
            format!(
                "Function `{}` declares return type `{}` but has no return expression.",
                function.name,
                declared_return.display()
            ),
            function.span.clone(),
        ));
    }
}

fn insert_assignments(body: &[Token], brace_index: usize) -> Vec<(String, String)> {
    if body.get(brace_index).is_none_or(|token| token.text != "{") {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut index = brace_index + 1;
    let mut depth = 1;
    while index + 2 < body.len() && depth > 0 {
        if body[index].text == "{" {
            depth += 1;
            index += 1;
            continue;
        }
        if body[index].text == "}" {
            depth -= 1;
            index += 1;
            continue;
        }
        if depth == 1 && body[index].kind == TokenKind::Identifier && body[index + 1].text == "=" {
            let name = body[index].text.clone();
            let line = body[index + 2].span.line;
            let mut value = String::new();
            index += 2;
            while index < body.len()
                && body[index].span.line == line
                && body[index].text != ","
                && body[index].text != "}"
            {
                value.push_str(&body[index].text);
                index += 1;
            }
            result.push((name, value));
        } else {
            index += 1;
        }
    }
    result
}

fn compatible(expected: &TypeRef, actual: &TypeRef) -> bool {
    if actual.name == "Nothing" {
        return expected.nullable;
    }
    expected.name == actual.name && (expected.nullable || !actual.nullable)
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
        if required && value.is_none() {
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

fn source_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    fn visit(path: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
        for entry in fs::read_dir(path)
            .map_err(|error| format!("Cannot read source directory {}: {error}", path.display()))?
        {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            if path.is_dir() {
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

pub fn json(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}

pub fn json_array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|item| json(item))
            .collect::<Vec<_>>()
            .join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

    fn fixture(source: &str, schema: &str) -> (PathBuf, ProjectConfig) {
        let suffix = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!("bindc-test-{}-{suffix}", std::process::id()));
        fs::create_dir_all(root.join("app")).unwrap();
        fs::write(root.join("app/main.bd"), source).unwrap();
        fs::write(root.join("schema.csv"), schema).unwrap();
        fs::write(
            root.join("bind.toml"),
            r#"[project]
name = "test"
source = "app"
[database.main]
schema = "schema.csv"
verification = "static"
[target.aws-lambda]
client = "elm"
server = "rust"
deployment = "aws-lambda"
capabilities = ["ui.dom", "server.http", "db.read", "db.write"]
"#,
        )
        .unwrap();
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        (root, config)
    }

    const SOURCE: &str = r#"
distinct type CustomerId = UUID
entity Customer(val id: CustomerId, val name: String)
server fun getCustomer(id: CustomerId): Customer effects [db.read] {
    return db.customers.require(id)
}
client component CustomerPage(id: CustomerId) {
    val customer = server.getCustomer(id)
    UI { Text(customer.name) }
}
"#;

    const SCHEMA: &str = "table,model,primary_key,column,type,nullable,required,has_default,engine\ncustomers,Customer,id,id,CustomerId,false,true,true,postgresql\ncustomers,Customer,id,name,String,false,true,false,postgresql\n";

    const EMPTY_SCHEMA: &str =
        "table,model,primary_key,column,type,nullable,required,has_default,engine\n";

    fn codes(result: &CompileResult) -> Vec<&'static str> {
        result.diagnostics.iter().map(|item| item.code).collect()
    }

    #[test]
    fn effect_rules_come_from_the_type_checker() {
        let cases = [
            (
                "pure fun p(id: CustomerId): String = db.customers.require(id).name\n",
                "E7401",
            ),
            (
                "server fun eco(x: Int): Int = x\nclient fun usa(): Int = eco(1)\n",
                "E7404",
            ),
            (
                "server fun f(id: CustomerId): Customer = db.customers.require(id)\n",
                "E7402",
            ),
        ];
        for (body, expected) in cases {
            let source = format!(
                "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\n{body}"
            );
            let (root, config) = fixture(&source, SCHEMA);
            let result = verify(&config, "aws-lambda");
            let found = codes(&result);
            assert!(found.contains(&expected), "{body}: {found:?}");
            assert!(
                !found.iter().any(|code| code.starts_with("E6")),
                "{body}: {found:?}"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn pure_code_may_call_only_effect_free_functions() {
        let prelude = "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\n";
        let reader = format!(
            "{prelude}server fun leitor(id: CustomerId): Customer effects [db.read] = db.customers.require(id)\npure fun p(id: CustomerId): String = leitor(id).name\n"
        );
        let (root, config) = fixture(&reader, SCHEMA);
        let found = codes(&verify(&config, "aws-lambda"));
        assert!(found.contains(&"E7401"), "{found:?}");
        assert!(
            !found.iter().any(|code| code.starts_with("E6")),
            "{found:?}"
        );
        fs::remove_dir_all(root).unwrap();

        // Accepted behavior: `pure` means effect-free, so an effect-free
        // `server fun` may be called from pure code (legacy E6102 is gone).
        let echo =
            format!("{prelude}server fun eco(x: Int): Int = x\npure fun q(x: Int): Int = eco(x)\n");
        let (root, config) = fixture(&echo, SCHEMA);
        let result = verify(&config, "aws-lambda");
        assert!(result.application.is_some(), "{:?}", result.diagnostics);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capabilities_come_from_typed_effects() {
        let (root, config) = fixture(SOURCE, SCHEMA);
        let app = verify(&config, "aws-lambda")
            .application
            .expect("application");
        let expected: BTreeSet<String> = ["db.read", "server.http", "ui.dom"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(app.required_capabilities, expected);
        fs::remove_dir_all(root).unwrap();

        let helper = "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\nfun ler(id: CustomerId) = db.customers.require(id)\nserver fun getCustomer(id: CustomerId): Customer effects [db.read] = ler(id)\n";
        let (root, config) = fixture(helper, SCHEMA);
        let result = verify(&config, "aws-lambda");
        let app = result
            .application
            .unwrap_or_else(|| panic!("{:?}", result.diagnostics));
        assert!(app.required_capabilities.contains("db.read"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capabilities_exclude_effects_of_lambdas_never_called() {
        let prelude = "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\n";
        let uncalled = format!(
            "{prelude}client component Page(id: CustomerId) {{\n    val load = {{ -> db.customers.require(id) }}\n    UI {{ Text(\"x\") }}\n}}\n"
        );
        let (root, config) = fixture(&uncalled, SCHEMA);
        let result = verify(&config, "aws-lambda");
        let app = result
            .application
            .unwrap_or_else(|| panic!("{:?}", result.diagnostics));
        assert!(
            !app.required_capabilities.contains("db.read"),
            "{:?}",
            app.required_capabilities
        );
        fs::remove_dir_all(root).unwrap();

        let called = format!(
            "{prelude}fun reader(id: CustomerId): Customer {{\n    val load = {{ -> db.customers.require(id) }}\n    return load()\n}}\n"
        );
        let (root, config) = fixture(&called, SCHEMA);
        let result = verify(&config, "aws-lambda");
        let app = result
            .application
            .unwrap_or_else(|| panic!("{:?}", result.diagnostics));
        assert!(
            app.required_capabilities.contains("db.read"),
            "{:?}",
            app.required_capabilities
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_validation_skips_type_parameters_and_function_types() {
        let source = "pure fun <T> ident(x: T): T = x\npure fun aplica(f: (Int) -> Int, x: Int): Int = f(x)\n";
        let (root, config) = fixture(source, EMPTY_SCHEMA);
        let result = verify(&config, "aws-lambda");
        assert!(
            !result.diagnostics.iter().any(|item| item.code == "E2003"),
            "{:?}",
            result.diagnostics
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn binds_complete_application() {
        let (root, config) = fixture(SOURCE, SCHEMA);
        let result = verify(&config, "aws-lambda");
        assert!(result.application.is_some(), "{:?}", result.diagnostics);
        let app = result.application.unwrap();
        assert!(app.graph.edges["CustomerPage"].contains("getCustomer"));
        assert_eq!(
            app.graph.impact("database.main.customers.name"),
            vec!["CustomerPage".to_string(), "getCustomer".to_string()]
        );
        assert_eq!(app.rpc_contracts[0].path, "/api/getCustomer");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verified_application_carries_the_validated_core_ir() {
        let (root, config) = fixture(SOURCE, SCHEMA);
        let result = verify(&config, "aws-lambda");
        assert!(
            !codes(&result).contains(&"E9001"),
            "{:?}",
            result.diagnostics
        );
        let app = result
            .application
            .unwrap_or_else(|| panic!("{:?}", result.diagnostics));
        let names: Vec<&str> = app
            .core
            .functions
            .iter()
            .map(|function| function.name.as_str())
            .collect();
        assert!(names.contains(&"getCustomer"), "{names:?}");
        assert!(names.contains(&"CustomerPage"), "{names:?}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn effectful_generalized_val_used_at_two_types_is_an_internal_error() {
        // The checker accepts it, but the lowering cannot copy an effectful value per instance,
        // so the IR is inconsistent and `verify` stops with E9001 instead of generating code.
        let source = "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\nfun load(id: CustomerId): Customer effects [db.read] = db.customers.require(id)\nserver fun f(id: CustomerId): Int effects [db.read] {\n    val xs = if (load(id).name == \"a\") emptyList() else emptyList()\n    val a = length(map(xs, { x -> x + 1 }))\n    return length(map(xs, { x -> x == \"b\" }))\n}\n";
        let (root, config) = fixture(source, SCHEMA);
        let result = verify(&config, "aws-lambda");
        assert!(result.application.is_none());
        let found = codes(&result);
        assert_eq!(found, vec!["E9001"], "{:?}", result.diagnostics);
        assert!(
            result.diagnostics[0]
                .message
                .starts_with("Internal compiler error: the core IR is inconsistent ("),
            "{}",
            result.diagnostics[0].message
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn body_less_functions_are_lowered_and_user_errors_come_first() {
        let (root, config) = fixture("fun g(x: Int): Unit\nfun f() = g(1)\n", EMPTY_SCHEMA);
        let result = verify(&config, "aws-lambda");
        assert!(
            !codes(&result).contains(&"E9001"),
            "{:?}",
            result.diagnostics
        );
        let app = result
            .application
            .unwrap_or_else(|| panic!("{:?}", result.diagnostics));
        assert!(
            app.core
                .functions
                .iter()
                .any(|function| function.name == "g")
        );
        fs::remove_dir_all(root).unwrap();

        // A body-less function with another return type is a user error, never masked by E9001.
        let (root, config) = fixture(
            "external fun g(x: Int): Int\nfun f(): Int = g(1)\n",
            EMPTY_SCHEMA,
        );
        let result = verify(&config, "aws-lambda");
        let found = codes(&result);
        assert!(found.contains(&"E2301"), "{found:?}");
        assert!(!found.contains(&"E9001"), "{found:?}");
        assert!(result.application.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn accepts_bind_and_bd_file_extensions() {
        let (root, config) = fixture(SOURCE, SCHEMA);
        fs::rename(root.join("app/main.bd"), root.join("app/main.bind")).unwrap();
        let result = verify(&config, "aws-lambda");
        assert!(result.application.is_some(), "{:?}", result.diagnostics);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn broken_database_column_aborts_before_generation() {
        let broken = SCHEMA.replace(",name,String,", ",full_name,String,");
        let (root, config) = fixture(SOURCE, &broken);
        let result = verify(&config, "aws-lambda");
        assert!(result.application.is_none());
        let diagnostic = result
            .diagnostics
            .iter()
            .find(|item| item.code == "E5201")
            .expect("database diagnostic");
        assert_eq!(
            diagnostic.path.first().map(String::as_str),
            Some("CustomerPage")
        );
        assert!(diagnostic.path.last().unwrap().ends_with("customers.name"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nominal_ids_do_not_mix() {
        let source = r#"
distinct type CustomerId = UUID
distinct type OrderId = UUID
pure fun getOrder(id: OrderId): Unit {}
pure fun wrong(id: CustomerId): Unit { getOrder(id) }
"#;
        let (root, config) = fixture(
            source,
            "table,model,primary_key,column,type,nullable,required,has_default,engine\n",
        );
        let result = verify(&config, "aws-lambda");
        assert!(
            result.diagnostics.iter().any(|item| item.code == "E7101"),
            "{:?}",
            result.diagnostics
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn type_errors_abort_before_the_legacy_analysis() {
        let (root, config) = fixture("pure fun f(): Int = \"texto\"\n", EMPTY_SCHEMA);
        let result = verify(&config, "aws-lambda");
        assert!(result.application.is_none());
        let codes: Vec<_> = result.diagnostics.iter().map(|item| item.code).collect();
        assert_eq!(codes, vec!["E7101"], "{:?}", result.diagnostics);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn type_errors_are_ordered_by_file_and_line() {
        let (root, config) = fixture(
            "pure fun b(): Int = \"x\"\npure fun c(): Int = true\n",
            EMPTY_SCHEMA,
        );
        fs::write(root.join("app/zeta.bd"), "pure fun a(): Int = \"y\"\n").unwrap();
        let result = verify(&config, "aws-lambda");
        let positions: Vec<_> = result
            .diagnostics
            .iter()
            .map(|item| (item.span.file.clone(), item.span.line))
            .collect();
        assert_eq!(
            positions,
            vec![
                ("app/main.bd".to_string(), 1),
                ("app/main.bd".to_string(), 2),
                ("app/zeta.bd".to_string(), 1)
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inferred_and_generic_functions_pass_the_legacy_analysis() {
        let source = "pure fun dobro(x) = x * 2\npure fun <T> ident(x: T): T = x\npure fun usa(): Int = dobro(ident(2))\n";
        let (root, config) = fixture(source, EMPTY_SCHEMA);
        let result = verify(&config, "aws-lambda");
        let bad = result.diagnostics.iter().any(|item| {
            item.code == "E2202" || item.code == "E5205" || item.severity == Severity::Error
        });
        assert!(!bad, "{:?}", result.diagnostics);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn secret_cannot_flow_to_client() {
        let source = r#"
environment development { secret val token: String = env("BIND_TEST_TOKEN", default = "local") }
client fun leak(): String { return token }
"#;
        let (root, config) = fixture(
            source,
            "table,model,primary_key,column,type,nullable,required,has_default,engine\n",
        );
        let result = verify(&config, "aws-lambda");
        assert!(result.diagnostics.iter().any(|item| item.code == "E4102"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn insert_requires_all_non_default_columns() {
        let source = r#"
entity Customer(val id: UUID, val name: String, val email: Email)
server fun createCustomer(name: String): Customer effects [db.write] {
    return db.customers.insert {
        name = name
    }
}
"#;
        let schema = "table,model,primary_key,column,type,nullable,required,has_default,engine\ncustomers,Customer,id,id,UUID,false,true,true,postgresql\ncustomers,Customer,id,name,String,false,true,false,postgresql\ncustomers,Customer,id,email,Email,false,true,false,postgresql\n";
        let (root, config) = fixture(source, schema);
        let result = verify(&config, "aws-lambda");
        assert!(result.diagnostics.iter().any(|item| item.code == "E5303"));
        fs::remove_dir_all(root).unwrap();
    }

    /// A certified aws-lambda fixture whose database URL comes from
    /// `BIND_TEST_CERTIFIED_DATABASE_URL` (set here: `verify` requires it to be defined).
    fn certified_fixture(source: &str, schema: &str) -> (PathBuf, ProjectConfig) {
        static SET: std::sync::Once = std::sync::Once::new();
        // SAFETY: the variable is only read through `std::env`, whose accesses are serialized
        // with this write by the standard library's environment lock.
        SET.call_once(|| unsafe {
            env::set_var(
                "BIND_TEST_CERTIFIED_DATABASE_URL",
                "postgresql://bind@localhost/bind",
            )
        });
        let (root, _) = fixture(source, schema);
        fs::write(
            root.join("bind.toml"),
            r#"[project]
name = "test"
source = "app"
[database.main]
schema = "schema.csv"
env = "BIND_TEST_CERTIFIED_DATABASE_URL"
verification = "static"
[target.aws-lambda]
client = "elm"
server = "rust"
deployment = "aws-lambda"
certified = true
capabilities = ["ui.dom", "server.http", "db.read", "db.write"]
"#,
        )
        .unwrap();
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        (root, config)
    }

    /// Declares the runtime database URL, read from `BIND_TEST_CERTIFIED_DATABASE_URL`.
    const DATABASE_ENVIRONMENT: &str = "environment development {\n    runtime secret val databaseUrl: DatabaseUrl = env(\"BIND_TEST_CERTIFIED_DATABASE_URL\")\n}\n";

    #[test]
    fn certified_endpoints_reject_types_that_cannot_cross_the_wire() {
        let cases = [
            (
                "server fun f(x: Int): (Int) -> Int effects [] = { y -> y }\n",
                "Endpoint `f` uses `(Int) -> Int`, which cannot cross the wire in profile rust-elm-web-v2.",
            ),
            (
                "server fun g(d: Duration): Int = 1\n",
                "Endpoint `g` uses `Duration`, which cannot cross the wire in profile rust-elm-web-v2.",
            ),
            (
                "sealed class S { data class A(val n: Int) }\nserver fun h(): S = A(1)\n",
                "Endpoint `h` uses `S`, which cannot cross the wire in profile rust-elm-web-v2.",
            ),
            (
                "server fun r(): Result<Int, String> = Ok(1)\n",
                "Endpoint `r` uses `Result<Int, String>`, which cannot cross the wire in profile rust-elm-web-v2.",
            ),
            (
                "server fun s(xs: List<Duration>?): Int = 1\n",
                "Endpoint `s` uses `List<Duration>?`, which cannot cross the wire in profile rust-elm-web-v2.",
            ),
        ];
        for (source, message) in cases {
            let (root, config) = certified_fixture(source, EMPTY_SCHEMA);
            let result = verify(&config, "aws-lambda");
            let found: Vec<_> = result
                .diagnostics
                .iter()
                .filter(|item| item.code == "E8309")
                .map(|item| item.message.as_str())
                .collect();
            assert_eq!(found, vec![message], "{source}: {:?}", result.diagnostics);
            assert!(result.application.is_none());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn certified_wire_types_and_helpers_with_functions_are_accepted() {
        let source = "enum class Cor { Azul, Verde }\nsealed class Forma { data object Ponto }\ndata class Par(val a: Int, val cores: List<Cor>, val nota: String?)\nfun aplica(f: (Int) -> Int, x: Int): Int = f(x)\nfun espera(d: Duration): Duration = d\nserver fun usa(p: Par, xs: List<Int?>, f: Forma): Par effects [] {\n    val n = aplica({ y -> y + 1 }, p.a)\n    return Par(n, p.cores, p.nota)\n}\n";
        let (root, config) = certified_fixture(source, EMPTY_SCHEMA);
        let result = verify(&config, "aws-lambda");
        assert!(result.application.is_some(), "{:?}", result.diagnostics);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duration_columns_are_rejected_in_the_certified_profile() {
        let source = format!(
            "{DATABASE_ENVIRONMENT}distinct type TarefaId = UUID\nentity Tarefa(val id: TarefaId, val prazo: Duration)\nfun ler(id: TarefaId): Tarefa effects [db.read] = db.tarefas.require(id)\n"
        );
        let schema = format!(
            "{EMPTY_SCHEMA}tarefas,Tarefa,id,id,TarefaId,false,true,true,postgresql\ntarefas,Tarefa,id,prazo,Duration,false,true,false,postgresql\n"
        );
        let (root, config) = certified_fixture(&source, &schema);
        let result = verify(&config, "aws-lambda");
        let found: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|item| item.code == "E8309")
            .map(|item| item.message.as_str())
            .collect();
        assert_eq!(
            found,
            vec![
                "Entity field `Tarefa.prazo` uses `Duration`, which has no database mapping in profile rust-elm-web-v2.",
                "Column `tarefas.prazo` uses `Duration`, which has no database mapping in profile rust-elm-web-v2.",
            ],
            "{:?}",
            result.diagnostics
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wire_types_are_checked_for_every_rust_server_target() {
        // `fixture`'s target is not certified.
        let source = "server fun bad(x: Int): (Int) -> Int effects [] = { y -> y }\n";
        let (root, config) = fixture(source, EMPTY_SCHEMA);
        assert!(!config.targets["aws-lambda"].certified);
        let result = verify(&config, "aws-lambda");
        assert!(
            codes(&result).contains(&"E8309"),
            "{:?}",
            result.diagnostics
        );
        assert!(result.application.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    fn e8310(source: &str, schema: &str) -> Vec<String> {
        let (root, config) = fixture(source, schema);
        let result = verify(&config, "aws-lambda");
        let found: Vec<String> = result
            .diagnostics
            .iter()
            .filter(|item| item.code == "E8310")
            .map(|item| item.message.clone())
            .collect();
        if !found.is_empty() {
            assert!(result.application.is_none());
        }
        fs::remove_dir_all(root).unwrap();
        found
    }

    #[test]
    fn endpoints_reaching_bodiless_functions_are_rejected() {
        // `solta` has no body either, but no endpoint reaches it.
        let source = "fun registra(x: Int): Unit\nfun solta(x: Int): Unit\nfun usa(x: Int): Int {\n    registra(x)\n    return x\n}\nserver fun a(x: Int): Int = usa(x)\nserver fun b(x: Int): Unit\n";
        assert_eq!(
            e8310(source, EMPTY_SCHEMA),
            vec![
                "Endpoint `b` has no body, so the generated server cannot run it in profile rust-elm-web-v2.",
                "Function `registra` is reachable from endpoint `a` but has no body, so the generated server cannot run it in profile rust-elm-web-v2.",
            ]
        );
    }

    #[test]
    fn endpoints_reaching_environment_values_are_rejected() {
        let source = "environment development {\n    public val appName: String = env(\"BIND_TEST_APP_NAME\", default = \"x\")\n}\nfun nome(): String = appName\nserver fun a(): String = nome()\nserver fun b(): String = nome()\n";
        assert_eq!(
            e8310(source, EMPTY_SCHEMA),
            vec![
                "Function `nome` is reachable from endpoint `a` but uses an `environment` value, which the generated server does not support in profile rust-elm-web-v2."
            ]
        );
    }

    #[test]
    fn endpoints_reaching_ui_trees_are_rejected() {
        let source = "fun tela() = UI { Text(\"x\") }\nserver fun f(): Int {\n    val t = tela()\n    return 1\n}\n";
        assert_eq!(
            e8310(source, EMPTY_SCHEMA),
            vec![
                "Function `tela` is reachable from endpoint `f` but uses a UI tree, which the generated server does not support in profile rust-elm-web-v2."
            ]
        );
    }

    #[test]
    fn columns_without_a_database_mapping_are_rejected() {
        let source = "enum class Cor { Azul, Verde }\ndistinct type ItemId = UUID\nentity Item(val id: ItemId, val cor: Cor)\nserver fun ler(id: ItemId): ItemId effects [db.read] = db.itens.require(id).id\n";
        let schema = format!(
            "{EMPTY_SCHEMA}itens,Item,id,id,ItemId,false,true,true,postgresql\nitens,Item,id,cor,Cor,false,true,false,postgresql\n"
        );
        assert_eq!(
            e8310(source, &schema),
            vec![
                "Column `itens.cor` has type `Cor`, which has no database mapping in profile rust-elm-web-v2."
            ]
        );
        // A table no endpoint uses does not matter.
        let unused = "enum class Cor { Azul, Verde }\ndistinct type ItemId = UUID\nentity Item(val id: ItemId, val cor: Cor)\nserver fun nada(): Int = 1\n";
        assert!(e8310(unused, &schema).is_empty());
    }

    #[test]
    fn database_operations_the_model_cannot_back_are_rejected() {
        // (a) An inserted column the model does not declare.
        let source = "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\nserver fun f(n: String): CustomerId effects [db.write] {\n    val c = db.customers.insert {\n        name = n\n        extra = n\n    }\n    return c.id\n}\n";
        let schema =
            format!("{SCHEMA}customers,Customer,id,extra,String,false,false,false,postgresql\n");
        assert_eq!(
            e8310(source, &schema),
            vec![
                "Column `customers.extra` is assigned by an insert, but model `Customer` declares no field `extra`, so the generated server cannot use it in profile rust-elm-web-v2."
            ]
        );
        // (b) A primary key the model does not declare.
        let source = "distinct type CustomerId = UUID\nentity Customer(val name: String)\nserver fun g(id: CustomerId): String effects [db.read] = db.customers.require(id).name\n";
        assert_eq!(
            e8310(source, SCHEMA),
            vec![
                "Column `customers.id` is the primary key of the table, but model `Customer` declares no field `id`, so the generated server cannot use it in profile rust-elm-web-v2."
            ]
        );
    }

    #[test]
    fn endpoints_using_environment_values_directly_are_named() {
        let source = "environment development {\n    public val appName: String = env(\"BIND_TEST_APP_NAME\", default = \"x\")\n}\nserver fun a(): String = appName\n";
        assert_eq!(
            e8310(source, EMPTY_SCHEMA),
            vec![
                "Endpoint `a` uses an `environment` value, which the generated server does not support in profile rust-elm-web-v2."
            ]
        );
    }

    #[test]
    fn unused_schema_tables_are_not_checked_for_the_server() {
        let source = "server fun f(): Int = 1\n";
        let schema = format!(
            "{EMPTY_SCHEMA}esperas,Espera,id,prazo,Duration,false,true,false,postgresql\nruins,Ruim,id,x,List<,false,true,false,postgresql\n"
        );
        let (root, config) = fixture(source, &schema);
        let result = verify(&config, "aws-lambda");
        assert!(result.application.is_some(), "{:?}", result.diagnostics);
        fs::remove_dir_all(root).unwrap();

        // In a table the program uses, a malformed column type is a schema error, not E9001.
        let source =
            "distinct type RuimId = UUID\nentity Ruim(val id: RuimId)\nserver fun f(): Int = 1\n";
        let schema = format!(
            "{EMPTY_SCHEMA}ruins,Ruim,id,id,RuimId,false,true,true,postgresql\nruins,Ruim,id,x,List<,false,true,false,postgresql\n"
        );
        let (root, config) = fixture(source, &schema);
        let result = verify(&config, "aws-lambda");
        let found = codes(&result);
        assert_eq!(found, vec!["E5101"], "{:?}", result.diagnostics);
        fs::remove_dir_all(root).unwrap();
    }

    /// The endpoint of `tests/codegen/endpoint.bd`: a helper, a `when` and two database
    /// operations in one endpoint.
    pub(crate) const ENDPOINT_SOURCE: &str = include_str!("../tests/codegen/endpoint.bd");
    pub(crate) const ENDPOINT_SCHEMA: &str = include_str!("../tests/codegen/endpoint.csv");

    #[test]
    fn certified_endpoint_with_helper_when_and_two_database_operations() {
        let source = format!("{DATABASE_ENVIRONMENT}{ENDPOINT_SOURCE}");
        let (root, config) = certified_fixture(&source, ENDPOINT_SCHEMA);
        let result = verify(&config, "aws-lambda");
        assert!(!has_errors(&result.diagnostics), "{:?}", result.diagnostics);
        let app = result
            .application
            .unwrap_or_else(|| panic!("{:?}", result.diagnostics));
        let endpoint = app
            .core
            .functions
            .iter()
            .find(|function| function.name == "cadastrar")
            .unwrap();
        assert!(endpoint.endpoint);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn certified_database_rejects_an_optional_runtime_url() {
        let source = format!(
            "environment development {{ runtime secret val databaseUrl: DatabaseUrl = envOrNull(\"DATABASE_URL\") }}\n{SOURCE}"
        );
        let (root, _) = fixture(&source, SCHEMA);
        fs::write(
            root.join("bind.toml"),
            r#"[project]
name = "test"
source = "app"
[database.main]
schema = "schema.csv"
env = "DATABASE_URL"
verification = "static"
[target.aws-lambda]
client = "elm"
server = "rust"
deployment = "aws-lambda"
certified = true
capabilities = ["ui.dom", "server.http", "db.read", "db.write"]
"#,
        )
        .unwrap();
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        let result = verify(&config, "aws-lambda");
        assert!(result.diagnostics.iter().any(|item| item.code == "E8307"));
        fs::remove_dir_all(root).unwrap();
    }
}
