use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::ast::*;
use crate::config::ProjectConfig;
use crate::diagnostic::{Diagnostic, Severity, Span, has_errors};
use crate::lexer::{Token, TokenKind, lex};
use crate::parser::parse;
use crate::schema::{DatabaseSchema, closest, stable_hash};

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
    pub arguments: Vec<String>,
    pub assignments: Vec<(String, String)>,
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
    let application = if has_errors(&diagnostics) {
        None
    } else {
        Some(VerifiedApplication {
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
        })
    };
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

        if function.domain == Domain::Server {
            let function_operations: Vec<_> = operations
                .iter()
                .filter(|operation| operation.function == function.name)
                .collect();
            if function_operations.len() != 1 {
                diagnostics.push(Diagnostic::error(
                    "E8304",
                    format!(
                        "Certified server function `{}` must lower to exactly one structural database operation; found {}.",
                        function.name,
                        function_operations.len()
                    ),
                    function.span.clone(),
                ));
            } else {
                let operation = function_operations[0];
                match operation.method.as_str() {
                    "require" | "find" if operation.arguments.len() != 1 => {
                        diagnostics.push(Diagnostic::error(
                            "E8305",
                            format!(
                                "Database method `{}` requires exactly one primary-key argument.",
                                operation.method
                            ),
                            function.span.clone(),
                        ));
                    }
                    "insert" if operation.assignments.is_empty() => {
                        diagnostics.push(Diagnostic::error(
                            "E8305",
                            "A certified INSERT must declare its assignments structurally.",
                            function.span.clone(),
                        ));
                    }
                    "require" | "find" | "insert" => {}
                    method => diagnostics.push(Diagnostic::error(
                        "E8305",
                        format!("Database method `{method}` is not implemented by the certified Rust generator."),
                        function.span.clone(),
                    )),
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
    let mut locals: BTreeMap<String, TypeRef> = function
        .parameters
        .iter()
        .filter_map(|item| {
            item.type_ref
                .clone()
                .map(|type_ref| (item.name.clone(), type_ref))
        })
        .collect();
    let body = &function.body;
    let mut index = 0;
    while index < body.len() {
        if (body[index].text == "val" || body[index].text == "var")
            && let Some(name) = body
                .get(index + 1)
                .filter(|token| token.kind == TokenKind::Identifier)
                .map(|token| token.text.clone())
        {
            let equals = (index + 2..body.len().min(index + 10))
                .find(|position| body[*position].text == "=");
            if let Some(equals) = equals
                && let Some(value_type) =
                    infer_expression(body, equals + 1, &locals, functions, database)
            {
                locals.insert(name, value_type);
            }
        }
        let (call_name, call_start) = if body.get(index).is_some_and(|token| token.text == "server")
            && body.get(index + 1).is_some_and(|token| token.text == ".")
        {
            (
                body.get(index + 2).map(|token| token.text.clone()),
                index + 2,
            )
        } else if body.get(index + 1).is_some_and(|token| token.text == "(")
            && functions.contains_key(&body[index].text)
            && !(index >= 2 && body[index - 2].text == "server" && body[index - 1].text == ".")
        {
            (Some(body[index].text.clone()), index)
        } else {
            (None, index)
        };
        if let Some(call_name) = call_name
            && let Some(callee) = functions.get(&call_name)
        {
            graph.add_edge(function.name.clone(), call_name.clone());
            validate_call_arguments(
                body,
                call_start + 1,
                callee,
                &locals,
                functions,
                database,
                diagnostics,
            );
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
            let arguments = call_arguments_as_source(body, index + 5);
            operations.push(DatabaseOperation {
                function: function.name.clone(),
                table: table_name.clone(),
                method: method.clone(),
                effect: effect.clone(),
                arguments,
                assignments: assignments.clone(),
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
                    if known_return(function).is_some_and(|declared| declared.name != table.model) {
                        diagnostics.push(Diagnostic::error(
                            "E5205",
                            format!("Database operation on `{}` produces `{}`, but function `{}` declares `{}`.", table.name, table.model, function.name, function.declared_return().display()),
                            function.span.clone(),
                        ));
                    }
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
                        if assignments
                            .iter()
                            .any(|(_, value)| !safe_generated_expression(value))
                        {
                            diagnostics.push(Diagnostic::error(
                                "E8204",
                                "The certified Rust target cannot safely lower one or more INSERT value expressions.",
                                body[index].span.clone(),
                            ).with_hint("Use parameters or parameter fields as INSERT values in the v0.1 target."));
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
    validate_return_contract(function, &locals, functions, database, diagnostics);
    let _ = program;
}

/// Declared return type as far as the legacy pass can trust it: absent
/// annotations and types mentioning the function's type parameters are
/// unknown here (the type checker already verified them).
fn known_return(function: &Function) -> Option<TypeRef> {
    let declared = function.return_type.as_ref()?;
    (!mentions_type_param(declared, &function.type_params)).then(|| declared.clone())
}

fn validate_return_contract(
    function: &Function,
    locals: &BTreeMap<String, TypeRef>,
    functions: &BTreeMap<String, &Function>,
    database: &DatabaseSchema,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(declared_return) = known_return(function) else {
        return;
    };
    if declared_return.name == "Unit" {
        return;
    }
    let return_positions: Vec<_> = function
        .body
        .iter()
        .enumerate()
        .filter_map(|(index, token)| (token.text == "return").then_some(index + 1))
        .collect();
    let expression_positions = if return_positions.is_empty() && !function.body.is_empty() {
        vec![0]
    } else {
        return_positions
    };
    if expression_positions.is_empty() {
        diagnostics.push(Diagnostic::error(
            "E2301",
            format!(
                "Function `{}` declares return type `{}` but has no return expression.",
                function.name,
                declared_return.display()
            ),
            function.span.clone(),
        ));
        return;
    }
    for position in expression_positions {
        if let Some(actual) =
            infer_expression(&function.body, position, locals, functions, database)
            && !compatible(&declared_return, &actual)
        {
            diagnostics.push(Diagnostic::error(
                "E2302",
                format!(
                    "Function `{}` declares `{}` but returns `{}`.",
                    function.name,
                    declared_return.display(),
                    actual.display()
                ),
                function.body[position].span.clone(),
            ));
        }
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

fn safe_generated_expression(value: &str) -> bool {
    let parts: Vec<_> = value.split('.').collect();
    !parts.is_empty()
        && parts.len() <= 2
        && parts.iter().all(|part| {
            !part.is_empty() && part.chars().all(|ch| ch.is_alphanumeric() || ch == '_')
        })
}

fn call_arguments_as_source(body: &[Token], open_index: usize) -> Vec<String> {
    if body.get(open_index).is_none_or(|token| token.text != "(") {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for token in body.iter().skip(open_index + 1) {
        match token.text.as_str() {
            "(" | "[" | "{" => {
                depth += 1;
                current.push_str(&token.text);
            }
            ")" if depth == 0 => {
                if !current.is_empty() {
                    result.push(current);
                }
                break;
            }
            ")" | "]" | "}" => {
                depth = depth.saturating_sub(1);
                current.push_str(&token.text);
            }
            "," if depth == 0 => {
                result.push(current);
                current = String::new();
            }
            _ => current.push_str(&token.text),
        }
    }
    result
}

fn validate_call_arguments(
    body: &[Token],
    open_index: usize,
    callee: &Function,
    locals: &BTreeMap<String, TypeRef>,
    functions: &BTreeMap<String, &Function>,
    database: &DatabaseSchema,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if body.get(open_index).is_none_or(|token| token.text != "(") {
        return;
    }
    let mut arguments = Vec::new();
    let mut start = open_index + 1;
    let mut depth = 0;
    let mut index = start;
    while index < body.len() {
        match body[index].text.as_str() {
            "(" => depth += 1,
            ")" if depth == 0 => {
                if index > start {
                    arguments.push((start, index));
                }
                break;
            }
            ")" => depth -= 1,
            "," if depth == 0 => {
                arguments.push((start, index));
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    if arguments.len() != callee.parameters.len() {
        diagnostics.push(Diagnostic::error(
            "E2201",
            format!(
                "Function `{}` expects {} argument(s), received {}.",
                callee.name,
                callee.parameters.len(),
                arguments.len()
            ),
            body[open_index].span.clone(),
        ));
        return;
    }
    for ((start, _), parameter) in arguments.into_iter().zip(&callee.parameters) {
        let Some(declared) = &parameter.type_ref else {
            continue;
        };
        if mentions_type_param(declared, &callee.type_params) {
            continue;
        }
        if let Some(actual) = infer_expression(body, start, locals, functions, database)
            && !compatible(declared, &actual)
        {
            diagnostics.push(Diagnostic::error(
                "E2202",
                format!(
                    "Argument `{}` of `{}` expects `{}`, received `{}`.",
                    parameter.name,
                    callee.name,
                    declared.display(),
                    actual.display()
                ),
                body[start].span.clone(),
            ));
        }
    }
}

fn infer_expression(
    body: &[Token],
    index: usize,
    locals: &BTreeMap<String, TypeRef>,
    functions: &BTreeMap<String, &Function>,
    database: &DatabaseSchema,
) -> Option<TypeRef> {
    let token = body.get(index)?;
    if token.kind == TokenKind::String {
        return Some(TypeRef::new("String"));
    }
    if token.kind == TokenKind::Number {
        return Some(TypeRef::new(if token.text.contains('.') {
            "Double"
        } else {
            "Int"
        }));
    }
    if token.text == "true" || token.text == "false" {
        return Some(TypeRef::new("Boolean"));
    }
    if token.text == "null" {
        return Some(TypeRef {
            name: "Nothing".into(),
            nullable: true,
        });
    }
    if let Some(local) = locals.get(&token.text) {
        return Some(local.clone());
    }
    let function_name = if token.text == "server" && body.get(index + 1)?.text == "." {
        &body.get(index + 2)?.text
    } else {
        &token.text
    };
    if let Some(function) = functions.get(function_name) {
        return known_return(function);
    }
    if token.text == "db" && body.get(index + 1)?.text == "." {
        let table = database.tables.get(&body.get(index + 2)?.text)?;
        let method = body.get(index + 4)?.text.as_str();
        return Some(TypeRef {
            name: table.model.clone(),
            nullable: ["find", "firstOrNull"].contains(&method),
        });
    }
    None
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
