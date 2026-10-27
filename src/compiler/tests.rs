use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

static FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

fn fixture(source: &str, schema: &str) -> (PathBuf, ProjectConfig) {
    let suffix = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
    let root = env::temp_dir().join(format!("bindc-test-{}-{suffix}", std::process::id()));
    fs::create_dir_all(root.join("app")).unwrap();
    // A web target renders one client component: sources about other parts get a page.
    let page = if source.contains("client component") {
        ""
    } else {
        "\nclient component BindTestPage() {\n    UI {\n        Text(\"test\")\n    }\n}\n"
    };
    fs::write(root.join("app/main.bd"), format!("{source}{page}")).unwrap();
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
client = "web"
server = "rust"
deployment = "aws-lambda"
mode = "ssr"
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
    let prelude =
        "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\n";
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
    let prelude =
        "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\n";
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
fn type_parameters_and_function_types_verify() {
    let source =
        "pure fun <T> ident(x: T): T = x\npure fun aplica(f: (Int) -> Int, x: Int): Int = f(x)\n";
    let (root, config) = fixture(source, EMPTY_SCHEMA);
    let result = verify(&config, "aws-lambda");
    assert!(result.application.is_some(), "{:?}", result.diagnostics);
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
fn server_components_are_not_rpc_procedures() {
    let source = "server component Painel(x: Int) {\n    UI {\n        Text(\"p\")\n    }\n}\nserver fun a(x: Int): Int = x\n";
    let (root, config) = fixture(source, EMPTY_SCHEMA);
    let result = verify(&config, "aws-lambda");
    let app = result.application.expect("verifies");
    let names: Vec<&str> = app
        .rpc_contracts
        .iter()
        .map(|rpc| rpc.name.as_str())
        .collect();
    // The same procedures as the generated router.
    assert_eq!(names, vec!["a"]);
    let router = crate::backend::rust::server_library(&app).unwrap();
    assert!(router.contains("\"/api/a\" =>") && !router.contains("/api/Painel"));
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
fn effectful_val_used_at_two_types_is_a_type_error() {
    // Value restriction: a `val` whose right-hand side is not a syntactic value is not
    // generalized, so using it at two types is a user error, never E9001.
    let source = "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\nfun load(id: CustomerId): Customer effects [db.read] = db.customers.require(id)\nserver fun f(id: CustomerId): Int effects [db.read] {\n    val xs = if (load(id).name == \"a\") emptyList() else emptyList()\n    val a = length(map(xs, { x -> x + 1 }))\n    return length(map(xs, { x -> x == \"b\" }))\n}\n";
    let (root, config) = fixture(source, SCHEMA);
    let result = verify(&config, "aws-lambda");
    assert!(result.application.is_none());
    let found = codes(&result);
    assert_eq!(found, vec!["E7101"], "{:?}", result.diagnostics);
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
fn type_errors_abort_verification() {
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
fn inferred_and_generic_functions_verify() {
    let source = "pure fun dobro(x) = x * 2\npure fun <T> ident(x: T): T = x\npure fun usa(): Int = dobro(ident(2))\n";
    let (root, config) = fixture(source, EMPTY_SCHEMA);
    let result = verify(&config, "aws-lambda");
    assert!(result.application.is_some(), "{:?}", result.diagnostics);
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
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
client = "web"
server = "rust"
deployment = "aws-lambda"
mode = "ssr"
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
            "Endpoint `f` uses `(Int) -> Int`, which cannot cross the wire in profile rust-web-v4.",
        ),
        (
            "server fun g(d: Duration): Int = 1\n",
            "Endpoint `g` uses `Duration`, which cannot cross the wire in profile rust-web-v4.",
        ),
        (
            "server fun h(n: Long): Int = 1\n",
            "Endpoint `h` uses `Long`, which cannot cross the wire in profile rust-web-v4.",
        ),
        (
            "server fun r(): Result<Int, Long> = Ok(1)\n",
            "Endpoint `r` uses `Result<Int, Long>`, which cannot cross the wire in profile rust-web-v4.",
        ),
        (
            "data class Caixa<T>(val x: T)\nserver fun c(x: Caixa<Duration>): Int = 1\n",
            "Endpoint `c` uses `Caixa<Duration>`, which cannot cross the wire in profile rust-web-v4.",
        ),
        (
            "sealed class S { data class A(val f: (Int) -> Int)\n data object B }\nserver fun e(): S = B\n",
            "Endpoint `e` uses `S`, which cannot cross the wire in profile rust-web-v4.",
        ),
        (
            "server fun m(x: Map<String, Duration>): Int = 1\n",
            "Endpoint `m` uses `Map<String, Duration>`, which cannot cross the wire in profile rust-web-v4.",
        ),
        (
            "server fun s(xs: List<Duration>?): Int = 1\n",
            "Endpoint `s` uses `List<Duration>?`, which cannot cross the wire in profile rust-web-v4.",
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

/// `Set`, `Map`, `Result`, sealed classes with fields and instantiated generic types cross
/// the wire (Review Focus 4: one generic record at two instances in one signature).
#[test]
fn rich_types_cross_the_wire() {
    let source = "distinct type CustomerId = UUID\nenum class Cor { Z, A, M }\ndata class Customer(val id: CustomerId, val nome: String)\ndata class Pagina<T>(val itens: List<T>, val total: Int)\nsealed class Evento { data class Criado(val por: Customer, val cores: Set<Cor>)\n data object Nenhum }\nserver fun agrupa(p: Map<CustomerId, Pagina<Customer>>, n: Pagina<Int>): Map<CustomerId, Pagina<Customer>> = p\nserver fun cores(xs: Set<Cor>, e: Evento): Result<List<Cor>, String> = Ok(toList(xs))\nserver fun aninhado(x: Pagina<Pagina<Set<Cor>?>>): Map<Cor, Set<Int>> = emptyMap()\n";
    let (root, config) = certified_fixture(source, EMPTY_SCHEMA);
    let result = verify(&config, "aws-lambda");
    assert!(
        !codes(&result).contains(&"E8309"),
        "{:?}",
        result.diagnostics
    );
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
            "Entity field `Tarefa.prazo` uses `Duration`, which has no database mapping in profile rust-web-v4.",
            "Column `tarefas.prazo` uses `Duration`, which has no database mapping in profile rust-web-v4.",
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
            "Endpoint `b` has no body, so the generated server cannot run it in profile rust-web-v4.",
            "Function `registra` is reachable from endpoint `a` but has no body, so the generated server cannot run it in profile rust-web-v4.",
        ]
    );
}

#[test]
fn endpoints_reaching_environment_values_are_rejected() {
    let source = "environment development {\n    public val appName: String = env(\"BIND_TEST_APP_NAME\", default = \"x\")\n}\nfun nome(): String = appName\nserver fun a(): String = nome()\nserver fun b(): String = nome()\n";
    assert_eq!(
        e8310(source, EMPTY_SCHEMA),
        vec![
            "Function `nome` is reachable from endpoint `a` but uses an `environment` value, which the generated server does not support in profile rust-web-v4."
        ]
    );
}

#[test]
fn endpoints_reaching_ui_trees_are_rejected() {
    let source = "fun tela() = UI { Text(\"x\") }\nserver fun f(): Int {\n    val t = tela()\n    return 1\n}\n";
    assert_eq!(
        e8310(source, EMPTY_SCHEMA),
        vec![
            "Function `tela` is reachable from endpoint `f` but uses a UI tree, which the generated server does not support in profile rust-web-v4."
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
            "Column `itens.cor` has type `Cor`, which has no database mapping in profile rust-web-v4."
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
            "Column `customers.extra` is assigned by an insert, but model `Customer` declares no field `extra`, so the generated server cannot use it in profile rust-web-v4."
        ]
    );
    // (b) A primary key the model does not declare.
    let source = "distinct type CustomerId = UUID\nentity Customer(val name: String)\nserver fun g(id: CustomerId): String effects [db.read] = db.customers.require(id).name\n";
    assert_eq!(
        e8310(source, SCHEMA),
        vec![
            "Column `customers.id` is the primary key of the table, but model `Customer` declares no field `id`, so the generated server cannot use it in profile rust-web-v4."
        ]
    );
}

#[test]
fn endpoints_using_environment_values_directly_are_named() {
    let source = "environment development {\n    public val appName: String = env(\"BIND_TEST_APP_NAME\", default = \"x\")\n}\nserver fun a(): String = appName\n";
    assert_eq!(
        e8310(source, EMPTY_SCHEMA),
        vec![
            "Endpoint `a` uses an `environment` value, which the generated server does not support in profile rust-web-v4."
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
pub(crate) const ENDPOINT_SOURCE: &str = include_str!("../../tests/codegen/endpoint.bd");
pub(crate) const ENDPOINT_SCHEMA: &str = include_str!("../../tests/codegen/endpoint.csv");

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
client = "web"
server = "rust"
deployment = "aws-lambda"
mode = "ssr"
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

/// A fixture with one target `t` declared by `target` (the lines after `[target.t]`).
fn web_fixture(source: &str, target: &str) -> (PathBuf, ProjectConfig) {
    let (root, _) = fixture(source, EMPTY_SCHEMA);
    // Exactly `source`, without the page `fixture` adds.
    fs::write(root.join("app/main.bd"), source).unwrap();
    fs::write(
            root.join("bind.toml"),
            format!("[project]\nname = \"test\"\nsource = \"app\"\n[database.main]\nschema = \"schema.csv\"\nverification = \"static\"\n[target.t]\n{target}capabilities = [\"ui.dom\", \"server.http\", \"db.read\", \"db.write\"]\n"),
        )
        .unwrap();
    let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
    (root, config)
}

const WEB_SSR: &str =
    "client = \"web\"\nserver = \"rust\"\ndeployment = \"aws-lambda\"\nmode = \"ssr\"\n";
const WEB_SPLIT: &str =
    "client = \"web\"\nserver = \"rust\"\ndeployment = \"vercel\"\nmode = \"split\"\n";
const WEB_CLOUDFLARE: &str =
    "client = \"web\"\nserver = \"wasm\"\ndeployment = \"cloudflare-workers\"\nmode = \"split\"\n";

/// The E8311 messages of `source` on a target declared by `target`.
fn e8311(source: &str, target: &str) -> Vec<String> {
    let (root, config) = web_fixture(source, target);
    let result = verify(&config, "t");
    let found: Vec<String> = result
        .diagnostics
        .iter()
        .filter(|item| item.code == "E8311")
        .map(|item| item.message.clone())
        .collect();
    if !found.is_empty() {
        assert!(result.application.is_none());
    }
    fs::remove_dir_all(root).unwrap();
    found
}

const PAGE: &str = "client component Pagina() {\n    UI {\n        Text(\"oi\")\n    }\n}\n";

#[test]
fn cloudflare_accepts_pure_endpoints_and_rejects_database_semantics_explicitly() {
    let pure = "server fun double(value: Double): Double effects [] = value * 2.0\nclient component Home() {\n val answer = server.double(21.0)\n UI { Text(answer) }\n}\n";
    let (pure_root, pure_config) = web_fixture(pure, WEB_CLOUDFLARE);
    let pure_result = verify(&pure_config, "t");
    assert!(
        pure_result.diagnostics.is_empty(),
        "{:?}",
        pure_result.diagnostics
    );
    assert!(pure_result.application.is_some());
    fs::remove_dir_all(pure_root).unwrap();

    let int_source = "server fun double(value: Int): Int effects [] = value * 2\nclient component Home() { UI { Text(\"x\") } }\n";
    let (int_root, int_config) = web_fixture(int_source, WEB_CLOUDFLARE);
    let int_result = verify(&int_config, "t");
    assert!(int_result.application.is_none());
    assert!(int_result.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "E8312"
            && diagnostic
                .message
                .contains("parameter `value` of type `Int`")
            && diagnostic.message.contains("refuses to generate code")
    }));
    fs::remove_dir_all(int_root).unwrap();

    let (root, _) = fixture(SOURCE, SCHEMA);
    fs::write(
            root.join("bind.toml"),
            format!(
                "[project]\nname = \"test\"\nsource = \"app\"\n[database.main]\nschema = \"schema.csv\"\nverification = \"static\"\n[target.t]\n{WEB_CLOUDFLARE}certified = true\ncapabilities = [\"ui.dom\", \"server.http\", \"db.read\"]\n"
            ),
        )
        .unwrap();
    let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
    let result = verify(&config, "t");
    assert!(result.application.is_none());
    assert!(result.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "E8310"
            && diagnostic.message.contains("Cloudflare D1")
            && diagnostic.message.contains("Hyperdrive")
    }));
    assert!(!result.diagnostics.iter().any(|item| item.code == "E8307"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_first_cloudflare_profile_rejects_ssr() {
    let target = WEB_CLOUDFLARE.replace("mode = \"split\"", "mode = \"ssr\"");
    assert_eq!(
            e8311(PAGE, &target),
            vec!["The first Cloudflare Workers WebAssembly profile supports `mode = \"split\"`; its host ABI does not implement SSR.".to_string()]
        );
}

#[test]
fn decimal_multiplication_and_division_reachable_from_the_component_are_rejected() {
    // Bind has no `Decimal` literal: the component reaches the functions by reference.
    let source = "fun dobro(x: Decimal): Decimal = x * x\nfun metade(x: Decimal): Decimal = (x / x) ?: x\nfun solta(x: Decimal): Decimal = x * x\nfun soma(x: Decimal): Decimal = dobro(x + x - x)\nclient component Pagina() {\n    val f = soma\n    val g = metade\n    UI {\n        Text(\"x\")\n    }\n}\n";
    let messages = e8311(source, WEB_SSR);
    assert_eq!(
            messages,
            vec![
                "Function `dobro` is reachable from component `Pagina` but multiplies or divides `Decimal`/`Money` values, which the web frontend does not support in profile rust-web-v4.".to_string(),
                "Function `metade` is reachable from component `Pagina` but multiplies or divides `Decimal`/`Money` values, which the web frontend does not support in profile rust-web-v4.".to_string(),
            ]
        );
    // The same in the split mode: changing modes never changes which programs are accepted.
    assert_eq!(messages, e8311(source, WEB_SPLIT));
}

#[test]
fn component_parameters_must_fit_in_a_url() {
    let source = "distinct type Codigo = UUID\ndistinct type Nome = String\nclient component Pagina(a: String, b: Email, c: Url, d: Int, e: Codigo, f: Date, g: DateTime, h: Instant, i: Nome, j: Boolean, k: Decimal, l: Int?, m: List<Int>) {\n    UI {\n        Text(a)\n    }\n}\n";
    let suffix = ", which a page URL cannot carry in profile rust-web-v4 (allowed: String, Email, Url, Int, UUID, Date, DateTime, Instant and distinct types over them).";
    assert_eq!(
        e8311(source, WEB_SPLIT),
        ["j: Boolean", "k: Decimal", "l: Int?", "m: List<Int>"]
            .iter()
            .map(|parameter| {
                let (name, ty) = parameter.split_once(": ").unwrap();
                format!("Component `Pagina` has parameter `{name}` of type `{ty}`{suffix}")
            })
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_web_target_needs_a_client_component() {
    assert_eq!(
            e8311("server fun a(x: Int): Int = x\n", WEB_SSR),
            vec!["Target `t` has no client component: a web target renders exactly one `client component` as its page.".to_string()]
        );
}

#[test]
fn an_api_on_another_origin_needs_the_frontend_origin() {
    let cross = format!("{WEB_SPLIT}api_url = \"https://api.example.com/api\"\n");
    assert_eq!(
            e8311(PAGE, &cross),
            vec!["`api_url = \"https://api.example.com/api\"` is on another origin, so the target must declare `frontend_origin`, the only browser origin the API answers (CORS).".to_string()]
        );
    let declared = format!("{cross}frontend_origin = \"https://app.example.com\"\n");
    assert!(e8311(PAGE, &declared).is_empty());
    assert!(e8311(PAGE, &format!("{WEB_SPLIT}api_url = \"/api\"\n")).is_empty());
    let with_port = format!(
        "{WEB_SPLIT}api_url = \"http://localhost:8080/api\"\nfrontend_origin = \"http://localhost:3000\"\n"
    );
    assert!(e8311(PAGE, &with_port).is_empty());
    // `api_url` must reach the generated routes, `/api/<endpoint>`.
    for api_url in [
        "/backend",
        "/api/",
        "/v1/api",
        "/api?x=1",
        "api",
        "//api.example.com/api",
        "javascript:alert(1)",
        "https://api.example.com",
        "https://api.example.com/",
        "https://api.example.com/v1/api",
        "https://API.example.com/api",
        "https://user@api.example.com/api",
    ] {
        assert_eq!(
            e8311(
                PAGE,
                &format!(
                    "{WEB_SPLIT}api_url = \"{api_url}\"\nfrontend_origin = \"https://app.example.com\"\n"
                )
            ),
            vec![format!(
                "`api_url = \"{api_url}\"` must be `/api` or a canonical origin followed by `/api` (`https://host[:port]/api`, no trailing slash, query or fragment): the generated server answers at `/api/<endpoint>`."
            )],
            "{api_url}"
        );
    }
    // `frontend_origin` must be what browsers send in `Origin`.
    for origin in [
        "https://app.example.com/page",
        "https://app.example.com/",
        "https://App.example.com",
        "https://user@app.example.com",
        "https://app.example.com:443",
        "http://app.example.com:80",
        "https://app.example.com:0443",
        "https://app..example.com",
        "https://-app.example.com",
        "ftp://app.example.com",
        "app.example.com",
    ] {
        assert_eq!(
            e8311(PAGE, &format!("{cross}frontend_origin = \"{origin}\"\n")),
            vec![format!(
                "`frontend_origin = \"{origin}\"` must be a canonical origin: `http://` or `https://`, a lowercase host and an optional non-default port, without a path, user info or trailing slash."
            )],
            "{origin}"
        );
    }
    for origin in [
        "https://app.example.com:8443",
        "http://[::1]:3000",
        "http://127.0.0.1",
    ] {
        assert!(
            e8311(PAGE, &format!("{cross}frontend_origin = \"{origin}\"\n")).is_empty(),
            "{origin}"
        );
    }
}

#[test]
fn a_missing_web_stylesheet_is_reported_by_verify() {
    let (root, _) = web_fixture(PAGE, WEB_SSR);
    let manifest = fs::read_to_string(root.join("bind.toml")).unwrap();
    fs::write(
        root.join("bind.toml"),
        format!("{manifest}[web]\nstylesheet = \"missing.css\"\n"),
    )
    .unwrap();
    let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
    let result = verify(&config, "t");
    let messages: Vec<&str> = result
        .diagnostics
        .iter()
        .filter(|item| item.code == "E8311")
        .map(|item| item.message.as_str())
        .collect();
    assert_eq!(messages.len(), 1, "{:?}", result.diagnostics);
    assert!(
        messages[0].starts_with("The web stylesheet `"),
        "{}",
        messages[0]
    );
    assert!(messages[0].contains("missing.css"), "{}", messages[0]);
    assert!(result.application.is_none());
    // With the file present the target verifies and generates its stylesheet.
    fs::write(root.join("missing.css"), "body { color: red; }\n").unwrap();
    let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
    let result = verify(&config, "t");
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_frontend_origin_needs_an_api_on_another_origin() {
    let unused = |mode: &str| {
        vec![format!(
            "`frontend_origin = \"https://app.example.com\"` only applies when the API is on another origin (`mode = \"split\"` with `api_url = \"https://host[:port]/api\"`), but target `t` serves its API from the page's own origin, so the generated server answers no CORS: remove `frontend_origin`, or set `api_url` to the API's absolute URL.{mode}"
        )]
    };
    let origin = "frontend_origin = \"https://app.example.com\"\n";
    // The ssr mode has no cross-origin call; nor has the split mode with the default `/api`.
    assert_eq!(e8311(PAGE, &format!("{WEB_SSR}{origin}")), unused(""));
    assert_eq!(e8311(PAGE, &format!("{WEB_SPLIT}{origin}")), unused(""));
    assert_eq!(
        e8311(PAGE, &format!("{WEB_SPLIT}api_url = \"/api\"\n{origin}")),
        unused("")
    );
    assert!(
        e8311(
            PAGE,
            &format!("{WEB_SPLIT}api_url = \"https://api.example.com/api\"\n{origin}")
        )
        .is_empty()
    );
}

#[test]
fn numeric_hosts_are_canonical_only_in_dotted_decimal() {
    let cross = format!("{WEB_SPLIT}api_url = \"https://api.example.com/api\"\n");
    // A browser rewrites these hosts (`http://1` is `http://0.0.0.1`) or rejects them.
    for origin in [
        "http://1",
        "http://999.1.1.1",
        "http://1.2.3",
        "http://1.2.3.4.5",
        "http://01.2.3.4",
        "http://0x7f.0.0.1",
        "http://example.0x1f",
        "http://app.123",
        "http://256.0.0.1",
    ] {
        assert_eq!(
            e8311(PAGE, &format!("{cross}frontend_origin = \"{origin}\"\n")).len(),
            1,
            "{origin}"
        );
    }
    for origin in [
        "http://127.0.0.1:8080",
        "http://10.0.0.255",
        "http://0.0.0.0",
        "https://123.example.com",
        "https://app-1.example.com",
        "http://localhost:3000",
    ] {
        assert!(
            e8311(PAGE, &format!("{cross}frontend_origin = \"{origin}\"\n")).is_empty(),
            "{origin}"
        );
    }
    // `api_url` follows the same rule.
    let messages = e8311(
        PAGE,
        &format!(
            "{WEB_SPLIT}api_url = \"http://1/api\"\nfrontend_origin = \"https://app.example.com\"\n"
        ),
    );
    assert!(
        messages[0].starts_with("`api_url = \"http://1/api\"` must be `/api` or"),
        "{messages:?}"
    );
}

#[test]
fn a_web_target_must_choose_its_mode() {
    let target = "client = \"web\"\nserver = \"rust\"\ndeployment = \"vercel\"\n";
    assert_eq!(
            e8311(PAGE, target),
            vec!["Target `t` must choose its mode: `mode = \"ssr\"` (the server renders the page) or `mode = \"split\"` (static frontend calling the API).".to_string()]
        );
}

#[test]
fn every_web_target_has_one_client_component() {
    let source =
        format!("{PAGE}client component Outra() {{\n    UI {{\n        Text(\"b\")\n    }}\n}}\n");
    for target in [WEB_SSR, WEB_SPLIT] {
        let (root, config) = web_fixture(&source, target);
        let result = verify(&config, "t");
        assert_eq!(codes(&result), vec!["E8301"], "{:?}", result.diagnostics);
        assert!(result.application.is_none());
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn an_unknown_client_has_no_built_in_generator() {
    let target =
        "client = \"native\"\nserver = \"rust\"\nmode = \"split\"\ndeployment = \"vercel\"\n";
    let (root, config) = web_fixture(PAGE, target);
    let result = verify(&config, "t");
    assert_eq!(codes(&result), vec!["E8204"], "{:?}", result.diagnostics);
    assert_eq!(
        result.diagnostics[0].message,
        "No built-in generator can preserve the semantics of target `t` (native + rust, deployment vercel)."
    );
    assert!(result.application.is_none());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn client_code_the_server_cannot_render_is_rejected_in_both_modes() {
    let source = "environment development {\n    public val nome: String = env(\"APP_NAME\", default = \"x\")\n}\nfun mais(a: Duration, b: Duration): Duration = a + b\nfun registra(x: Int): Unit\nfun usa(): String {\n    registra(1)\n    return nome\n}\nclient component Pagina() {\n    val f = mais\n    UI {\n        Text(usa())\n    }\n}\n";
    let expected = vec![
            "Function `mais` is reachable from component `Pagina` but does arithmetic on `Duration` values, which the web frontend does not support in profile rust-web-v4.".to_string(),
            "Function `registra` is reachable from component `Pagina` but has no body, so the web frontend cannot run it in profile rust-web-v4.".to_string(),
            "Function `usa` is reachable from component `Pagina` but uses an `environment` value, which the web frontend does not support in profile rust-web-v4.".to_string(),
        ];
    assert_eq!(e8311(source, WEB_SSR), expected);
    assert_eq!(e8311(source, WEB_SPLIT), expected);
    // The component itself is named as such.
    let source = "environment development {\n    public val nome: String = env(\"APP_NAME\", default = \"x\")\n}\nclient component Pagina() {\n    UI {\n        Text(nome)\n    }\n}\n";
    assert_eq!(
            e8311(source, WEB_SPLIT),
            vec!["Component `Pagina` uses an `environment` value, which the web frontend does not support in profile rust-web-v4.".to_string()]
        );
}

#[test]
fn the_commerce_example_verifies_on_every_target() {
    if env::var_os("DATABASE_URL").is_none() {
        // SAFETY: read only through `std::env`, whose accesses take the same lock.
        unsafe { env::set_var("DATABASE_URL", "postgresql://bind:bind@localhost/bind") };
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
    let names: Vec<&str> = config.targets.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "aws-lambda-split",
            "aws-lambda-ssr",
            "vercel-split",
            "vercel-ssr"
        ]
    );
    for target in names {
        let result = verify(&config, target);
        assert!(
            result.diagnostics.is_empty() && result.application.is_some(),
            "{target}: {:?}",
            result.diagnostics
        );
    }
}

fn operation(function: &str, table: &str, method: &str, effect: &str) -> DatabaseOperation {
    DatabaseOperation {
        function: function.into(),
        table: table.into(),
        method: method.into(),
        effect: effect.into(),
    }
}

/// The database operations computed from the IR are the ones the token scan extracted for
/// every database test program (same functions, order and repetitions), and they are the
/// ones `verify` reports.
#[test]
fn database_operations_from_the_ir_match_the_token_scan() {
    let cases = [
        (
            ENDPOINT_SOURCE,
            ENDPOINT_SCHEMA,
            vec![
                operation("cadastrar", "customers", "insert", "db.write"),
                operation("cadastrar", "customers", "require", "db.read"),
                operation("procurar", "customers", "find", "db.read"),
                operation("falha", "customers", "insert", "db.write"),
                operation("falha", "customers", "require", "db.read"),
            ],
        ),
        (
            include_str!("../../tests/codegen/client_int.bd"),
            include_str!("../../tests/codegen/client_int.csv"),
            vec![operation("reservar", "items", "require", "db.read")],
        ),
        (
            include_str!("../../tests/codegen/names.bd"),
            include_str!("../../tests/codegen/names.csv"),
            vec![operation("procura", "things", "find", "db.read")],
        ),
        (
            include_str!("../../tests/codegen/lints.bd"),
            include_str!("../../tests/codegen/lints.csv"),
            vec![
                operation("largo", "wide", "insert", "db.write"),
                operation("largo", "wide", "require", "db.read"),
            ],
        ),
        (
            include_str!("../../tests/codegen/ssr.bd"),
            include_str!("../../tests/codegen/ssr.csv"),
            vec![operation("getCustomer", "customers", "require", "db.read")],
        ),
        (
            SOURCE,
            SCHEMA,
            vec![operation("getCustomer", "customers", "require", "db.read")],
        ),
    ];
    for (source, schema, expected) in cases {
        let (root, config) = fixture(source, schema);
        let result = verify(&config, "aws-lambda");
        let app = result
            .application
            .unwrap_or_else(|| panic!("{:?}", result.diagnostics));
        assert_eq!(
            crate::core::analysis::database_operations(&app.core),
            expected
        );
        assert_eq!(app.database_operations, expected);
        fs::remove_dir_all(root).unwrap();
    }
}

/// The graph `verify` reports is the one computed from the IR (this program declares no
/// `environment`, so there are no `environment.X -> env.VAR` bindings to add).
#[test]
fn application_graph_comes_from_the_ir() {
    let (root, config) = fixture(ENDPOINT_SOURCE, ENDPOINT_SCHEMA);
    let result = verify(&config, "aws-lambda");
    let app = result
        .application
        .unwrap_or_else(|| panic!("{:?}", result.diagnostics));
    assert_eq!(
        app.graph,
        crate::core::analysis::application_graph(&app.core)
    );
    fs::remove_dir_all(root).unwrap();
}

/// An edge the token scan missed: a function passed as a value (no `(` after its name),
/// which `impact` follows up to the component. (Environment reads are covered in
/// `core::analysis`: neither the generated server nor the web frontend accepts one yet.)
#[test]
fn application_graph_follows_function_values() {
    let source = "pure fun dobro(x: Int): Int = x * 2\npure fun aplica(f: (Int) -> Int, x: Int): Int = f(x)\npure fun usa(): Int = aplica(dobro, 1)\nclient component Pagina() {\n    UI {\n        Text(usa())\n    }\n}\n";
    let (root, config) = fixture(source, EMPTY_SCHEMA);
    let result = verify(&config, "aws-lambda");
    let app = result
        .application
        .unwrap_or_else(|| panic!("{:?}", result.diagnostics));
    assert_eq!(
        app.graph.edges["usa"],
        BTreeSet::from(["aplica".to_string(), "dobro".to_string()])
    );
    assert_eq!(app.graph.impact("dobro"), vec!["Pagina", "usa"]);
    fs::remove_dir_all(root).unwrap();
}

/// A secret `token` declared in the `development` environment.
const SECRET_TOKEN: &str = "environment development {\n    secret val token: String = env(\"BIND_TEST_TOKEN\", default = \"local\")\n}\n";

/// The diagnostics of `code` for `source` on `schema` (none of them in a verified program).
fn with_code(source: &str, schema: &str, code: &str) -> Vec<Diagnostic> {
    let (root, config) = fixture(source, schema);
    let result = verify(&config, "aws-lambda");
    let found: Vec<Diagnostic> = result
        .diagnostics
        .iter()
        .filter(|item| item.code == code)
        .cloned()
        .collect();
    if !found.is_empty() {
        assert!(result.application.is_none());
    }
    fs::remove_dir_all(root).unwrap();
    found
}

#[test]
fn e4102_secret_read_directly_by_a_client_function() {
    let source = format!("{SECRET_TOKEN}client fun leak(): String {{ return token }}\n");
    let found = with_code(&source, EMPTY_SCHEMA, "E4102");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].message,
        "Secret value `token` flows into client code: environment.token -> leak."
    );
    assert_eq!(found[0].path, vec!["environment.token", "leak"]);
    assert_eq!(found[0].span.line, 4);
}

/// Review Focus 2: the secret reaches the client through a lambda passed to a shared
/// function, and through a function reference passed the same way. One precise E4102 each,
/// and no E8311 for the same read.
#[test]
fn e4102_secret_passed_to_a_shared_function_as_a_lambda_or_a_function_value() {
    let shared = "fun aplica(f: () -> String): String = f()\n";
    let lambda = format!(
        "{SECRET_TOKEN}{shared}client component Pagina() {{\n    val t = aplica({{ -> token }})\n    UI {{\n        Text(t)\n    }}\n}}\n"
    );
    let found = with_code(&lambda, EMPTY_SCHEMA, "E4102");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].path, vec!["environment.token", "Pagina"]);
    assert!(with_code(&lambda, EMPTY_SCHEMA, "E8311").is_empty());

    let reference = format!(
        "{SECRET_TOKEN}{shared}fun lerToken(): String = token\nclient component Pagina() {{\n    UI {{\n        Text(aplica(lerToken))\n    }}\n}}\n"
    );
    let found = with_code(&reference, EMPTY_SCHEMA, "E4102");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].message,
        "Secret value `token` flows into client code: environment.token -> lerToken -> Pagina."
    );
    assert_eq!(
        found[0].path,
        vec!["environment.token", "lerToken", "Pagina"]
    );
    assert!(with_code(&reference, EMPTY_SCHEMA, "E8311").is_empty());
}

#[test]
fn e4102_secret_reached_through_helper_functions() {
    let source = format!(
        "{SECRET_TOKEN}fun segredo(): String = token\nfun rotulo(): String = \"x\" + segredo()\nclient component Pagina() {{\n    UI {{\n        Text(rotulo())\n    }}\n}}\n"
    );
    let found = with_code(&source, EMPTY_SCHEMA, "E4102");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].path,
        vec!["environment.token", "segredo", "rotulo", "Pagina"]
    );
    // The span is the first client function of the path: the component (line 6).
    assert_eq!(found[0].span.line, 6);
    assert!(with_code(&source, EMPTY_SCHEMA, "E8311").is_empty());
}

/// A secret read reported as E4102 is not reported again by the server's environment rule
/// (E8310) when an endpoint also reaches it; a non-secret read in the same function is.
#[test]
fn e4102_reads_are_not_reported_again_as_e8310() {
    let source = format!(
        "{SECRET_TOKEN}fun segredo(): String = token\nserver fun a(): String = segredo()\nclient component Pagina() {{\n    UI {{\n        Text(segredo())\n    }}\n}}\n"
    );
    assert_eq!(with_code(&source, EMPTY_SCHEMA, "E4102").len(), 1);
    assert!(with_code(&source, EMPTY_SCHEMA, "E8310").is_empty());

    let mixed = source
            .replace(
                "secret val token",
                "public val nome: String = env(\"BIND_TEST_NOME\", default = \"x\")\n    secret val token",
            )
            .replace("= token", "= token + nome");
    assert_eq!(with_code(&mixed, EMPTY_SCHEMA, "E4102").len(), 1);
    assert_eq!(with_code(&mixed, EMPTY_SCHEMA, "E8310").len(), 1);
    assert_eq!(with_code(&mixed, EMPTY_SCHEMA, "E8311").len(), 1);
}

#[test]
fn e4102_ignores_locals_and_strings_named_like_the_secret() {
    let source = format!(
        "{SECRET_TOKEN}client fun local(): String {{\n    val token = \"token\"\n    return token + \"TOKEN\"\n}}\n"
    );
    let (root, config) = fixture(&source, EMPTY_SCHEMA);
    let result = verify(&config, "aws-lambda");
    assert!(result.application.is_some(), "{:?}", result.diagnostics);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn non_secret_environment_reads_keep_e8311() {
    let source = "environment development {\n    public val appName: String = env(\"BIND_TEST_APP_NAME\", default = \"x\")\n}\nclient component Pagina() {\n    UI {\n        Text(appName)\n    }\n}\n";
    assert!(with_code(source, EMPTY_SCHEMA, "E4102").is_empty());
    assert_eq!(with_code(source, EMPTY_SCHEMA, "E8311").len(), 1);
}

const CUSTOMER_EMAIL_SCHEMA: &str = "table,model,primary_key,column,type,nullable,required,has_default,engine\ncustomers,Customer,id,id,UUID,false,true,true,postgresql\ncustomers,Customer,id,name,String,false,true,false,postgresql\ncustomers,Customer,id,email,Email,false,true,false,postgresql\n";

#[test]
fn e5303_insert_without_a_required_column() {
    let missing = "entity Customer(val id: UUID, val name: String, val email: Email)\nserver fun criar(name: String): UUID effects [db.write] {\n    val c = db.customers.insert {\n        name = name\n    }\n    return c.id\n}\n";
    let found = with_code(missing, CUSTOMER_EMAIL_SCHEMA, "E5303");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].message,
        "INSERT into `customers` is missing required field(s):\n    email : Email NOT NULL"
    );
    assert_eq!(found[0].span.line, 2);

    let complete = "entity Customer(val id: UUID, val name: String, val email: Email)\nserver fun criar(name: String, email: Email): UUID effects [db.write] {\n    val c = db.customers.insert {\n        name = name\n        email = email\n    }\n    return c.id\n}\n";
    let (root, config) = fixture(complete, CUSTOMER_EMAIL_SCHEMA);
    let result = verify(&config, "aws-lambda");
    assert!(result.application.is_some(), "{:?}", result.diagnostics);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn e5204_model_field_type_differs_from_the_column() {
    let source = "entity Customer(val id: UUID, val name: Int)\nserver fun ler(id: UUID): Int effects [db.read] = db.customers.require(id).name\n";
    let found = with_code(source, CUSTOMER_EMAIL_SCHEMA, "E5204");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].message,
        "Database type mismatch for `customers.name`: schema is `String`, application model is `Int`."
    );
    assert_eq!(found[0].span.line, 1);

    // An optional field may read a non-null column; a non-null field a nullable one not.
    let optional = "entity Customer(val id: UUID, val name: String?, val email: Email)\nserver fun ler(id: UUID): Email effects [db.read] = db.customers.require(id).email\n";
    assert!(with_code(optional, CUSTOMER_EMAIL_SCHEMA, "E5204").is_empty());
    let nullable = CUSTOMER_EMAIL_SCHEMA.replace(",name,String,false,", ",name,String,true,");
    let source = "entity Customer(val id: UUID, val name: String, val email: Email)\nserver fun ler(id: UUID): Email effects [db.read] = db.customers.require(id).email\n";
    let found = with_code(source, &nullable, "E5204");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].message,
        "Database type mismatch for `customers.name`: schema is `String?`, application model is `String`."
    );
}

#[test]
fn e2301_body_less_function_with_a_non_unit_result() {
    let found = with_code(
        "external fun g(x: Int): Int\nfun f(): Int = g(1)\n",
        EMPTY_SCHEMA,
        "E2301",
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].message,
        "Function `g` declares return type `Int` but has no return expression."
    );
    assert_eq!(found[0].span.line, 1);
    assert!(
        with_code(
            "fun g(x: Int): Unit\nfun h(x: Int)\nfun f() = g(1)\n",
            EMPTY_SCHEMA,
            "E2301"
        )
        .is_empty()
    );
}

// ---- Coverage of the removed diagnostics ------------------------------------------------
// Each test shows a program with the bug a removed code used to catch still rejected, and
// names the code that catches it now.

/// The diagnostics of a rejected program (no application), on the default or the certified
/// fixture.
fn rejected(source: &str, schema: &str, certified: bool) -> Vec<Diagnostic> {
    let (root, config) = if certified {
        certified_fixture(source, schema)
    } else {
        fixture(source, schema)
    };
    let result = verify(&config, "aws-lambda");
    fs::remove_dir_all(root).unwrap();
    assert!(
        result.application.is_none(),
        "expected a rejection:\n{source}"
    );
    result.diagnostics
}

/// Asserts that `source` is rejected with `code` and with a message containing `message`.
fn assert_rejected_by(source: &str, schema: &str, certified: bool, code: &str, message: &str) {
    let diagnostics = rejected(source, schema, certified);
    assert!(
        diagnostics
            .iter()
            .any(|item| item.code == code && item.message.contains(message)),
        "expected {code} ({message}) for:\n{source}\ngot {diagnostics:?}"
    );
}

const CUSTOMER: &str =
    "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\n";

/// E5200 (unknown table) is E7104 of the type checker.
#[test]
fn coverage_e5200_unknown_table_is_e7104() {
    for body in [
        "server fun f(id: CustomerId): Customer effects [db.read] = db.custmers.require(id)\n",
        "server fun f(id: CustomerId): Customer? effects [db.read] = db.custmers.find(id)\n",
        "server fun f(n: String): Customer effects [db.write] = db.custmers.insert {\n    name = n\n}\n",
        "server fun f(id: CustomerId): Int effects [db.read] = db.custmers.count(id)\n",
    ] {
        assert_rejected_by(
            &format!("{CUSTOMER}{body}"),
            SCHEMA,
            false,
            "E7104",
            "Unknown database table `custmers`",
        );
    }
}

/// E5201 for an inserted column the table lacks is E7104; its `it.` heuristic (a field of a
/// row the table lacks) is E7104 on the record type.
#[test]
fn coverage_e5201_unknown_columns_are_e7104() {
    let insert = format!(
        "{CUSTOMER}server fun f(n: String): Customer effects [db.write] = db.customers.insert {{\n    nme = n\n}}\n"
    );
    assert_rejected_by(
        &insert,
        SCHEMA,
        false,
        "E7104",
        "Table `customers` has no column `nme`",
    );
    let lambda = format!(
        "{CUSTOMER}server fun f(id: CustomerId): List<String> effects [db.read] = map(listOf(db.customers.require(id)), {{ it -> it.nme }})\n"
    );
    assert_rejected_by(
        &lambda,
        SCHEMA,
        false,
        "E7104",
        "`Customer` has no field `nme`",
    );
    let field = format!(
        "{CUSTOMER}server fun f(id: CustomerId): String effects [db.read] = db.customers.require(id).nme\n"
    );
    assert_rejected_by(
        &field,
        SCHEMA,
        false,
        "E7104",
        "`Customer` has no field `nme`",
    );
}

/// E5202 (a table mapped to an unknown model) is E7103.
#[test]
fn coverage_e5202_unknown_model_is_e7103() {
    let schema = SCHEMA.replace(",Customer,", ",Cliente,");
    for body in [
        "server fun f(id: CustomerId): String effects [db.read] = db.customers.require(id).name\n",
        "server fun f(n: String): CustomerId effects [db.write] = db.customers.insert {\n    name = n\n}.id\n",
    ] {
        assert_rejected_by(
            &format!("{CUSTOMER}{body}"),
            &schema,
            false,
            "E7103",
            "Table `customers` maps to `Cliente`, which is not a declared entity",
        );
    }
}

/// E8204 for a table operation the generators cannot lower is E7004.
#[test]
fn coverage_e8204_table_operations_are_e7004() {
    for body in [
        "server fun f(id: CustomerId): Int effects [db.read] = db.customers.count(id)\n",
        "server fun f(id: CustomerId): Customer effects [db.write] = db.customers.delete(id)\n",
        "server fun f(c: Customer): Customer effects [db.write] = db.customers.upsert(c)\n",
        "server fun f(c: Customer): Customer effects [db.write] = db.customers.insert(c)\n",
    ] {
        assert_rejected_by(
            &format!("{CUSTOMER}{body}"),
            SCHEMA,
            false,
            "E7004",
            "Unsupported table operation",
        );
    }
    assert_rejected_by(
        &format!(
            "{CUSTOMER}server fun f(n: String): Customer effects [db.write] = db.customers.update {{\n    name = n\n}}\n"
        ),
        SCHEMA,
        false,
        "E7004",
        "blocks are only supported on `db.<table>.insert`",
    );
}

/// W2101 (`!!`) is the parser's E7001: the operator was removed from the language.
#[test]
fn coverage_w2101_forced_unwrap_is_e7001() {
    assert_rejected_by(
        "pure fun f(x: Int?): Int = x!!\n",
        EMPTY_SCHEMA,
        false,
        "E7001",
        "`!!` was removed",
    );
}

/// E8303 (a data-model field the certified wire format cannot carry) is E8309 where the
/// type crosses the wire (an endpoint's parameter or result, directly or through fields)
/// and E8310 where an entity field cannot be stored. A record that never crosses the wire
/// nor reaches the database may hold any type (as on a target that is not certified).
#[test]
fn coverage_e8303_non_wire_fields_are_e8309_on_endpoints() {
    for (types, signature) in [
        (
            "data class Caixa(val n: Long)\n",
            "server fun f(c: Caixa): Int = 1\n",
        ),
        (
            "data class Caixa(val f: (Int) -> Int)\n",
            "server fun f(): Caixa = Caixa({ x -> x })\n",
        ),
        (
            "data class Dentro(val n: Long)\ndata class Caixa(val d: List<Dentro>?)\n",
            "server fun f(c: Caixa): Int = 1\n",
        ),
        (
            "data class Caixa(val d: Duration)\n",
            "server fun f(c: Caixa): Int = 1\n",
        ),
    ] {
        assert_rejected_by(
            &format!("{types}{signature}"),
            EMPTY_SCHEMA,
            true,
            "E8309",
            "Endpoint `f` uses",
        );
    }
    // An entity field the database cannot store is E8310 (the generated database layer).
    let source = format!(
        "{DATABASE_ENVIRONMENT}distinct type ItemId = UUID\nentity Item(val id: ItemId, val tags: Set<Int>)\nserver fun f(id: ItemId): ItemId effects [db.read] = db.itens.require(id).id\n"
    );
    let schema = format!(
        "{EMPTY_SCHEMA}itens,Item,id,id,ItemId,false,true,true,postgresql\nitens,Item,id,tags,Set<Int>,false,true,false,postgresql\n"
    );
    assert_rejected_by(
        &source,
        &schema,
        true,
        "E8310",
        "Column `itens.tags` has type `Set<Int>`",
    );
}

/// E8302 (a sealed type without variants) stays: neither the parser nor the checker rejects
/// it (the same program verifies on a target that is not certified). It is checked on the
/// parsed variants (`TypeDecl.cases`) instead of the removed `TypeDecl.variants`.
#[test]
fn coverage_e8302_empty_sealed_type_is_kept() {
    let (root, config) = fixture(
        "sealed class Vazio\nserver fun f(): Int = 1\n",
        EMPTY_SCHEMA,
    );
    let result = verify(&config, "aws-lambda");
    fs::remove_dir_all(root).unwrap();
    assert!(result.application.is_some(), "{:?}", result.diagnostics);
    for source in [
        "sealed class Vazio\nserver fun f(): Int = 1\n",
        "sealed class Vazio {}\nserver fun f(): Int = 1\n",
    ] {
        let diagnostics = rejected(source, EMPTY_SCHEMA, true);
        assert!(
            diagnostics.iter().any(|item| item.code == "E8302"),
            "{diagnostics:?}"
        );
    }
}

/// E8308 (a mutable field) is the checker's E7002.
#[test]
fn coverage_e8308_mutable_field_is_e7002() {
    for source in [
        "data class Conta(var saldo: Int)\nserver fun f(): Int = 1\n",
        "entity Conta(val id: UUID, var saldo: Int)\nserver fun f(): Int = 1\n",
        "sealed class S { data class A(var n: Int) }\nserver fun f(): Int = 1\n",
    ] {
        assert_rejected_by(
            source,
            EMPTY_SCHEMA,
            true,
            "E7002",
            "is declared with `var`",
        );
    }
}

/// E2001 and E2002 (declared twice) are E7110, in one file or across files.
#[test]
fn coverage_e2001_e2002_duplicate_declarations_are_e7110() {
    assert_rejected_by(
        "data class A(val x: Int)\ndata class A(val y: Int)\n",
        EMPTY_SCHEMA,
        false,
        "E7110",
        "Type `A` is declared more than once",
    );
    assert_rejected_by(
        "data class String(val x: Int)\n",
        EMPTY_SCHEMA,
        false,
        "E7110",
        "conflicts with a built-in type",
    );
    assert_rejected_by(
        "pure fun f(): Int = 1\npure fun f(): Int = 2\n",
        EMPTY_SCHEMA,
        false,
        "E7110",
        "Function `f` is declared more than once",
    );
    let (root, config) = fixture(
        "data class A(val x: Int)\npure fun f(): Int = 1\n",
        EMPTY_SCHEMA,
    );
    fs::write(
        root.join("app/other.bd"),
        "enum class A { X }\npure fun f(): Int = 2\n",
    )
    .unwrap();
    let result = verify(&config, "aws-lambda");
    fs::remove_dir_all(root).unwrap();
    assert!(result.application.is_none());
    let found: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|item| item.code == "E7110")
        .map(|item| item.message.as_str())
        .collect();
    assert!(
        found.iter().any(|message| message.contains("Type `A`"))
            && found.iter().any(|message| message.contains("Function `f`")),
        "{:?}",
        result.diagnostics
    );
}

/// E2003 (unknown type in a declaration) is E7103, wherever the type is written.
#[test]
fn coverage_e2003_unknown_types_are_e7103() {
    for source in [
        "data class A(val x: Nope)\n",
        "entity A(val id: UUID, val x: List<Nope>)\n",
        "sealed class S { data class A(val x: Nope?) }\n",
        "distinct type D = Nope\n",
        "data class Caixa<T>(val x: T, val y: Nope)\n",
        "pure fun f(x: Nope): Int = 1\n",
        "pure fun f(): Nope? = null\n",
        "pure fun f(g: (Nope) -> Int): Int = 1\n",
        "external fun g(x: Nope): Unit\n",
        "external fun g(): Map<String, Nope>\n",
        "server fun f(x: Int): Result<Nope, String> = Ok(x)\n",
    ] {
        assert_rejected_by(source, EMPTY_SCHEMA, false, "E7103", "Unknown type `Nope`");
    }
}

/// E5201 for a model field without a column stays, computed on the IR (`core::analysis`):
/// the type checker does not compare the model with the table, and every generated query
/// selects every field of the model.
#[test]
fn coverage_e5201_model_field_without_a_column_is_kept_typed() {
    let broken = SCHEMA.replace(",name,String,", ",nme,String,");
    let found = with_code(SOURCE, &broken, "E5201");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].message,
        "Column `customers.name` required by `Customer` does not exist in database `main`."
    );
    assert_eq!(
        found[0].path,
        vec![
            "CustomerPage",
            "getCustomer",
            "database.main.customers.name"
        ]
    );
    assert_eq!(
        found[0].hint.as_deref(),
        Some("Did you mean `customers.nme`?")
    );
    // Located at the model field.
    assert_eq!(found[0].span.line, 3);
    // A table no operation uses is not checked.
    let unused = "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\nserver fun f(): Int = 1\n";
    assert!(with_code(unused, &broken, "E5201").is_empty());
}

/// E8307 is decided by the database operations of the IR (what the backend generates code
/// for), lambdas never called included, instead of a scan for `db .` tokens.
#[test]
fn e8307_follows_the_database_operations_of_the_ir() {
    let schema = SCHEMA;
    let uncalled = format!(
        "{CUSTOMER}server fun f(id: CustomerId): Int {{\n    val ler = {{ x -> db.customers.require(x) }}\n    return 1\n}}\n"
    );
    assert_rejected_by(
        &uncalled,
        schema,
        true,
        "E8307",
        "certified database runtime",
    );
    let direct = format!(
        "{CUSTOMER}server fun f(id: CustomerId): String effects [db.read] = db.customers.require(id).name\n"
    );
    assert_rejected_by(&direct, schema, true, "E8307", "certified database runtime");
    // Declared: verifies.
    let (root, config) = certified_fixture(&format!("{DATABASE_ENVIRONMENT}{direct}"), schema);
    let result = verify(&config, "aws-lambda");
    fs::remove_dir_all(root).unwrap();
    assert!(result.application.is_some(), "{:?}", result.diagnostics);
    // No database access: no credential needed, even with a local named `db`.
    let none = format!(
        "{CUSTOMER}data class Caixa(val customers: Int)\nserver fun f(): Int {{\n    val db = Caixa(1)\n    return db.customers\n}}\n"
    );
    let (root, config) = certified_fixture(&none, schema);
    let result = verify(&config, "aws-lambda");
    fs::remove_dir_all(root).unwrap();
    assert!(result.application.is_some(), "{:?}", result.diagnostics);
}

#[test]
fn the_source_hash_does_not_depend_on_the_project_location() {
    let (first, config) = fixture(SOURCE, SCHEMA);
    let hash = |config: &ProjectConfig| {
        verify(config, "aws-lambda")
            .application
            .expect("verified")
            .source_hash
    };
    let original = hash(&config);
    // The same project reached through another spelling of its path, then moved elsewhere.
    let dotted = ProjectConfig::load(&first.join("app/../bind.toml")).unwrap();
    assert_eq!(hash(&dotted), original);
    let (second, _) = fixture(SOURCE, SCHEMA);
    fs::remove_dir_all(&second).unwrap();
    fs::rename(&first, &second).unwrap();
    let moved = ProjectConfig::load(&second.join("bind.toml")).unwrap();
    assert_eq!(hash(&moved), original);
    fs::remove_dir_all(second).unwrap();
}

#[test]
fn runtime_values_are_not_required_at_compile_time() {
    let variable = "BIND_TEST_RUNTIME_ONLY_SECRET_UNSET";
    let (root, config) = fixture(
        &format!(
            "environment development {{\n    runtime secret val token: String = env(\"{variable}\")\n    compile val build: String = env(\"BIND_TEST_COMPILE_ONLY_UNSET\")\n}}\n{SOURCE}"
        ),
        SCHEMA,
    );
    let result = verify(&config, "aws-lambda");
    let missing: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|item| item.code == "E4001")
        .map(|item| item.message.clone())
        .collect();
    assert_eq!(missing.len(), 1, "{:?}", result.diagnostics);
    assert!(missing[0].contains("BIND_TEST_COMPILE_ONLY_UNSET"));
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn symbolic_links_in_the_source_directory_are_refused() {
    let (root, config) = fixture(SOURCE, SCHEMA);
    std::os::unix::fs::symlink(".", root.join("app/loop")).unwrap();
    let result = verify(&config, "aws-lambda");
    assert_eq!(codes(&result), ["E0001"], "{:?}", result.diagnostics);
    assert!(result.diagnostics[0].message.contains("Symbolic link"));
    fs::remove_dir_all(root).unwrap();
}
