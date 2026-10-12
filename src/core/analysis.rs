//! Whole-program analyses over the typed core IR: the application graph, the database
//! operations and the diagnostics computed from types (E4102, E5303, E5204, E2301). They replace the scans of raw lexer tokens; the backends read the same facts
//! through [`operations`], so what `verify` reports and what is generated never diverge.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::ast::Program;
use crate::diagnostic::{Diagnostic, Span};
use crate::schema::DatabaseSchema;
use crate::types::env::Env;
use crate::types::ty::Type;

use super::ir::{CoreFunction, CoreProgram, CoreType, DbOp, Domain, Expr, ExprKind};
use super::specialize::children;

/// One database operation of a function, as listed in the build manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseOperation {
    pub function: String,
    pub table: String,
    /// `require`, `find` or `insert`.
    pub method: String,
    /// `db.read` or `db.write`.
    pub effect: String,
}

/// One database operation used by the generated code.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DbKey {
    Require(String),
    Find(String),
    /// Table and the assigned columns, in source order.
    Insert(String, Vec<String>),
}

impl DbKey {
    pub fn of(op: &DbOp, table: &str) -> DbKey {
        match op {
            DbOp::Require => DbKey::Require(table.to_string()),
            DbOp::Find => DbKey::Find(table.to_string()),
            DbOp::Insert { columns } => DbKey::Insert(table.to_string(), columns.clone()),
        }
    }

    pub fn table(&self) -> &str {
        match self {
            DbKey::Require(table) | DbKey::Find(table) | DbKey::Insert(table, _) => table,
        }
    }

    /// The source method name: `require`, `find` or `insert`.
    pub fn method(&self) -> &'static str {
        match self {
            DbKey::Require(_) => "require",
            DbKey::Find(_) => "find",
            DbKey::Insert(..) => "insert",
        }
    }

    /// The effect label the operation needs: `db.write` for an insert, `db.read` otherwise.
    pub fn effect(&self) -> &'static str {
        match self {
            DbKey::Insert(..) => "db.write",
            DbKey::Require(_) | DbKey::Find(_) => "db.read",
        }
    }
}

/// Every `Db` node of `expr` (lambda and `when` bodies included), in source order, with
/// repetitions.
fn db_nodes<'e>(expr: &'e Expr, output: &mut Vec<&'e Expr>) {
    if let ExprKind::Db { .. } = &expr.kind {
        output.push(expr);
    }
    for child in children(expr) {
        db_nodes(child, output);
    }
}

fn db_key(expr: &Expr) -> Option<DbKey> {
    match &expr.kind {
        ExprKind::Db { op, table, .. } => Some(DbKey::of(op, table)),
        _ => None,
    }
}

/// The database operations in `functions` (lambdas included).
pub fn operations(functions: &[&CoreFunction]) -> BTreeSet<DbKey> {
    let mut nodes = Vec::new();
    for function in functions {
        db_nodes(&function.body, &mut nodes);
    }
    nodes.into_iter().filter_map(db_key).collect()
}

/// Every database operation of the program: functions in declaration order, and in each one
/// every `Db` node in source order (an operation written twice is listed twice).
pub fn database_operations(program: &CoreProgram) -> Vec<DatabaseOperation> {
    let mut output = Vec::new();
    for function in &program.functions {
        let mut nodes = Vec::new();
        db_nodes(&function.body, &mut nodes);
        output.extend(
            nodes
                .into_iter()
                .filter_map(db_key)
                .map(|key| DatabaseOperation {
                    function: function.name.clone(),
                    table: key.table().to_string(),
                    method: key.method().to_string(),
                    effect: key.effect().to_string(),
                }),
        );
    }
    output
}

/// The application graph: what each function uses. Nodes are program functions, database
/// resources (`database.main.<table>` and `database.main.<table>.<column>`), environment values
/// (`environment.<name>`) and environment variables (`env.<VARIABLE>`, added by `verify` from
/// the declarations, which the IR does not carry).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplicationGraph {
    pub edges: BTreeMap<String, BTreeSet<String>>,
}

/// The graph of `program`. For each function `f` (always a node), over its whole body (lambdas
/// included):
/// - `f -> g` for every reference to program function `g` (`Global`: a call or a function
///   passed as a value);
/// - `f -> g` for every call `server.g(...)` (`ServerCall`);
/// - `f -> database.main.<table>` and `f -> database.main.<table>.<field>` for every field of
///   the table's model (the record the `Db` node returns) for every database operation;
/// - `f -> environment.<name>` for every environment value it reads (`Environment`).
///
/// Edges point from the user to the used, so `impact(x)` lists everything that depends on `x`.
pub fn application_graph(program: &CoreProgram) -> ApplicationGraph {
    let records: BTreeMap<&str, &[(String, Type)]> = program
        .types
        .iter()
        .filter_map(|ty| match ty {
            CoreType::Record { name, fields, .. } => Some((name.as_str(), fields.as_slice())),
            _ => None,
        })
        .collect();
    let functions: BTreeSet<&str> = program
        .functions
        .iter()
        .map(|function| function.name.as_str())
        .collect();
    let mut graph = ApplicationGraph::default();
    for function in &program.functions {
        graph.add_node(function.name.clone());
        let mut used = BTreeSet::new();
        uses(&function.body, &records, &functions, &mut used);
        for target in used {
            graph.add_edge(function.name.clone(), target);
        }
    }
    graph
}

fn uses(
    expr: &Expr,
    records: &BTreeMap<&str, &[(String, Type)]>,
    functions: &BTreeSet<&str>,
    output: &mut BTreeSet<String>,
) {
    match &expr.kind {
        ExprKind::Global { name, .. } | ExprKind::ServerCall { function: name, .. }
            if functions.contains(name.as_str()) =>
        {
            output.insert(name.clone());
        }
        ExprKind::Environment(name) => {
            output.insert(format!("environment.{name}"));
        }
        ExprKind::Db { table, .. } => {
            output.insert(format!("database.main.{table}"));
            let model = match &expr.ty {
                Type::Con(name, args) if name == "Option" && args.len() == 1 => args.first(),
                ty => Some(ty),
            };
            if let Some(Type::Con(model, _)) = model
                && let Some(fields) = records.get(model.as_str())
            {
                for (field, _) in fields.iter() {
                    output.insert(format!("database.main.{table}.{field}"));
                }
            }
        }
        _ => {}
    }
    for child in children(expr) {
        uses(child, records, functions, output);
    }
}

impl ApplicationGraph {
    pub fn add_node(&mut self, node: impl Into<String>) {
        self.edges.entry(node.into()).or_default();
    }
    pub fn add_edge(&mut self, from: impl Into<String>, to: impl Into<String>) {
        self.edges.entry(from.into()).or_default().insert(to.into());
    }
}

impl ApplicationGraph {
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

/// The diagnostics computed from the typed IR (see [`typed_diagnostics`]).
#[derive(Clone, Debug, Default)]
pub struct TypedDiagnostics {
    pub diagnostics: Vec<Diagnostic>,
    /// The environment reads reported as E4102: `(function, environment value)`.
    pub secret_reads: BTreeSet<(String, String)>,
}

impl TypedDiagnostics {
    /// Whether every `environment` value `function` reads was already reported as a secret
    /// reaching client code (E4102): the generators' "uses an `environment` value" rule (E8310,
    /// E8311) then adds nothing for it.
    pub fn reports_environment_of(&self, function: &CoreFunction) -> bool {
        let reads = environment_reads(&function.body);
        !reads.is_empty()
            && reads.iter().all(|name| {
                self.secret_reads
                    .contains(&(function.name.clone(), name.clone()))
            })
    }
}

/// The diagnostics `verify` computes from the lowered program, with the AST (spans, the
/// `environment` declarations, the declared model fields) and the database schema:
/// - **E4102:** a secret `environment` value read by a function reachable from client code
///   (client components and `client` functions, then every function they reference by
///   `Global`, lambdas and function values included), with the path
///   `environment.X -> f -> … -> client root`, located at the first client function of the path;
/// - **E5303:** an `insert` that does not assign every column that is required, not nullable
///   and without a default;
/// - **E5204:** a field of the model of a table the program uses whose type differs from the
///   column's (the checker's mapping, [`Env::column_type`]); an optional field may read a
///   non-null column;
/// - **E2301:** a body-less function whose result is not `Unit`.
///
/// It runs on the lowered IR before validation: a body-less function is lowered with a `Unit`
/// body, which validation rejects when its result is not `Unit` (E2301 comes first).
pub fn typed_diagnostics(
    program: &CoreProgram,
    ast: &Program,
    schema: &DatabaseSchema,
) -> TypedDiagnostics {
    let mut output = TypedDiagnostics::default();
    secret_leaks(program, ast, &mut output);
    missing_insert_columns(program, ast, schema, &mut output.diagnostics);
    model_column_mismatches(program, ast, schema, &mut output.diagnostics);
    bodiless_results(program, ast, &mut output.diagnostics);
    output
}

fn ast_span(ast: &Program, function: &str) -> Span {
    ast.functions
        .iter()
        .find(|item| item.name == function)
        .map(|item| item.span.clone())
        .unwrap_or_default()
}

/// The `environment` values `expr` reads (lambdas included).
fn environment_reads(expr: &Expr) -> BTreeSet<String> {
    fn visit(expr: &Expr, output: &mut BTreeSet<String>) {
        if let ExprKind::Environment(name) = &expr.kind {
            output.insert(name.clone());
        }
        for child in children(expr) {
            visit(child, output);
        }
    }
    let mut output = BTreeSet::new();
    visit(expr, &mut output);
    output
}

/// The program functions `expr` references by `Global` (calls and function values).
fn global_references<'e>(expr: &'e Expr, output: &mut BTreeSet<&'e str>) {
    if let ExprKind::Global { name, .. } = &expr.kind {
        output.insert(name);
    }
    for child in children(expr) {
        global_references(child, output);
    }
}

/// E4102: one diagnostic per function reachable from client code and secret value it reads,
/// with a shortest path to a client root (roots in declaration order).
fn secret_leaks(program: &CoreProgram, ast: &Program, output: &mut TypedDiagnostics) {
    let secrets: BTreeSet<&str> = ast
        .environment
        .iter()
        .filter(|declaration| declaration.secret)
        .map(|declaration| declaration.name.as_str())
        .collect();
    if secrets.is_empty() {
        return;
    }
    let functions: BTreeMap<&str, &CoreFunction> = program
        .functions
        .iter()
        .map(|function| (function.name.as_str(), function))
        .collect();
    // Breadth-first from every client root: `parent[g] = f` when `f` references `g`.
    let mut parent: BTreeMap<&str, Option<&str>> = BTreeMap::new();
    let mut queue = VecDeque::new();
    for function in &program.functions {
        if function.domain == Domain::Client {
            parent.insert(&function.name, None);
            queue.push_back(function);
        }
    }
    while let Some(function) = queue.pop_front() {
        let mut referenced = BTreeSet::new();
        global_references(&function.body, &mut referenced);
        for name in referenced {
            if let Some(callee) = functions.get(name)
                && !parent.contains_key(name)
            {
                parent.insert(&callee.name, Some(&function.name));
                queue.push_back(callee);
            }
        }
    }
    for function in &program.functions {
        if !parent.contains_key(function.name.as_str()) {
            continue;
        }
        for name in environment_reads(&function.body) {
            if !secrets.contains(name.as_str()) {
                continue;
            }
            let mut chain = vec![function.name.as_str()];
            while let Some(Some(next)) = chain.last().and_then(|last| parent.get(last)) {
                chain.push(next);
            }
            let located = chain
                .iter()
                .find(|item| {
                    functions
                        .get(*item)
                        .is_some_and(|f| f.domain == Domain::Client)
                })
                .copied()
                .unwrap_or(function.name.as_str());
            let path: Vec<String> = std::iter::once(format!("environment.{name}"))
                .chain(chain.iter().map(|item| item.to_string()))
                .collect();
            output.diagnostics.push(
                Diagnostic::error(
                    "E4102",
                    format!(
                        "Secret value `{name}` flows into client code: {}.",
                        path.join(" -> ")
                    ),
                    ast_span(ast, located),
                )
                .with_path(path)
                .with_hint(
                    "Read secrets only in server code and send the client just what it needs through an endpoint.",
                ),
            );
            output
                .secret_reads
                .insert((function.name.clone(), name.clone()));
        }
    }
}

/// E5303: every `insert` assigns each column that is required, not nullable and without a
/// default.
fn missing_insert_columns(
    program: &CoreProgram,
    ast: &Program,
    schema: &DatabaseSchema,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for function in &program.functions {
        let mut nodes = Vec::new();
        db_nodes(&function.body, &mut nodes);
        for node in nodes {
            let ExprKind::Db {
                op: DbOp::Insert { columns },
                table,
                ..
            } = &node.kind
            else {
                continue;
            };
            let Some(definition) = schema.tables.get(table) else {
                continue;
            };
            let missing: Vec<String> = definition
                .columns
                .values()
                .filter(|column| {
                    column.required
                        && !column.nullable
                        && !column.has_default
                        && !columns.contains(&column.name)
                })
                .map(|column| format!("{} : {} NOT NULL", column.name, column.type_name))
                .collect();
            if !missing.is_empty() {
                diagnostics.push(Diagnostic::error(
                    "E5303",
                    format!(
                        "INSERT into `{table}` is missing required field(s):\n    {}",
                        missing.join("\n    ")
                    ),
                    ast_span(ast, &function.name),
                ));
            }
        }
    }
}

/// E5204: for each table a database operation uses (in order of first use), each field of its
/// model with a column must have the column's type, or be the optional version of it.
fn model_column_mismatches(
    program: &CoreProgram,
    ast: &Program,
    schema: &DatabaseSchema,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut tables: Vec<&str> = Vec::new();
    for function in &program.functions {
        let mut nodes = Vec::new();
        db_nodes(&function.body, &mut nodes);
        for node in nodes {
            if let ExprKind::Db { table, .. } = &node.kind
                && !tables.contains(&table.as_str())
            {
                tables.push(table);
            }
        }
    }
    if tables.is_empty() {
        return;
    }
    let env = Env::build(ast, &mut Vec::new());
    for table in tables {
        let Some(definition) = schema.tables.get(table) else {
            continue;
        };
        let Some(fields) = program.types.iter().find_map(|ty| match ty {
            CoreType::Record { name, fields, .. } if *name == definition.model => Some(fields),
            _ => None,
        }) else {
            continue;
        };
        let declared = ast.types.iter().find(|ty| ty.name == definition.model);
        for (field, ty) in fields {
            let Some(column) = definition.columns.get(field) else {
                continue;
            };
            let expected = env.column_type(column, &Span::default(), &mut Vec::new());
            if *ty == expected || *ty == Type::option(expected.clone()) {
                continue;
            }
            let declared_field = declared
                .and_then(|declared| declared.fields.iter().find(|item| item.name == *field));
            let shown = declared_field
                .map(|item| item.type_ref.display())
                .unwrap_or_else(|| crate::types::ty::display(ty));
            diagnostics.push(Diagnostic::error(
                "E5204",
                format!(
                    "Database type mismatch for `{table}.{field}`: schema is `{}{}`, application model is `{shown}`.",
                    column.type_name,
                    if column.nullable { "?" } else { "" }
                ),
                declared_field
                    .map(|item| item.span.clone())
                    .unwrap_or_default(),
            ));
        }
    }
}

/// E2301: a function without a body can only return `Unit`.
fn bodiless_results(program: &CoreProgram, ast: &Program, diagnostics: &mut Vec<Diagnostic>) {
    for function in &program.functions {
        if !function.bodiless || function.result == Type::con("Unit") {
            continue;
        }
        let declared = ast.functions.iter().find(|item| item.name == function.name);
        let shown = declared
            .and_then(|item| item.return_type.as_ref())
            .map(|ty| ty.display())
            .unwrap_or_else(|| crate::types::ty::display(&function.result));
        diagnostics.push(Diagnostic::error(
            "E2301",
            format!(
                "Function `{}` declares return type `{shown}` but has no return expression.",
                function.name
            ),
            ast_span(ast, &function.name),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::lower::tests::{compile, compile_with, schema};

    fn targets(graph: &ApplicationGraph, from: &str) -> Vec<String> {
        graph.edges[from].iter().cloned().collect()
    }

    #[test]
    fn edges_for_calls_server_calls_environment_and_recursion() {
        let program = compile(
            "environment development {\n    public val appName: String = env(\"APP\", default = \"x\")\n}\npure fun dobro(x: Int): Int = x * 2\npure fun conta(n: Int): Int = if (n == 0) 0 else conta(n - 1)\nfun nome(): String = appName\nserver fun total(x: Int): Int = dobro(x)\nclient component Pagina() {\n    val t = server.total(2)\n    UI {\n        Text(nome())\n    }\n}\n",
        );
        let graph = application_graph(&program);
        assert_eq!(targets(&graph, "dobro"), Vec::<String>::new());
        assert_eq!(targets(&graph, "conta"), vec!["conta"]);
        assert_eq!(targets(&graph, "nome"), vec!["environment.appName"]);
        assert_eq!(targets(&graph, "total"), vec!["dobro"]);
        assert_eq!(targets(&graph, "Pagina"), vec!["nome", "total"]);
        assert_eq!(graph.impact("environment.appName"), vec!["Pagina", "nome"]);
    }

    #[test]
    fn database_edges_name_the_table_and_the_model_columns() {
        let csv = "table,model,primary_key,column,type,nullable,required,has_default,engine\nitems,Item,id,id,ItemId,false,true,true,postgresql\nitems,Item,id,name,String,false,true,false,postgresql\n";
        let program = compile_with(
            "distinct type ItemId = UUID\nentity Item(val id: ItemId, val name: String)\nserver fun procura(id: ItemId): String effects [db.read] {\n    val busca = { -> db.items.find(id) }\n    return busca()?.name ?: \"\"\n}\nserver fun cria(): ItemId effects [db.read, db.write] {\n    val novo = db.items.insert {\n        name = \"a\"\n    }\n    val de_novo = db.items.require(novo.id)\n    val outra_vez = db.items.require(novo.id)\n    return de_novo.id\n}\n",
            &schema(csv),
        );
        let graph = application_graph(&program);
        let columns = vec![
            "database.main.items",
            "database.main.items.id",
            "database.main.items.name",
        ];
        assert_eq!(targets(&graph, "procura"), columns);
        assert_eq!(targets(&graph, "cria"), columns);
        let operations: Vec<(String, String, String)> = database_operations(&program)
            .into_iter()
            .map(|op| (op.function, op.method, op.effect))
            .collect();
        let expected = [
            ("procura", "find", "db.read"),
            ("cria", "insert", "db.write"),
            ("cria", "require", "db.read"),
            ("cria", "require", "db.read"),
        ];
        assert_eq!(
            operations,
            expected
                .map(|(f, m, e)| (f.to_string(), m.to_string(), e.to_string()))
                .to_vec()
        );
    }
}
