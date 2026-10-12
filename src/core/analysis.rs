//! Whole-program analyses over the typed core IR: the application graph and the database
//! operations. They replace the scans of raw lexer tokens; the backends read the same facts
//! through [`operations`], so what `verify` reports and what is generated never diverge.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::types::ty::Type;

use super::ir::{CoreFunction, CoreProgram, CoreType, DbOp, Expr, ExprKind};
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
