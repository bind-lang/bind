//! Review Focus 2: an RPC that fails after an `insert` leaves nothing written. Builds the
//! generated server of the commerce example plus a failing endpoint and runs it against
//! PostgreSQL (`BIND_TEST_DATABASE_URL`).

use std::fs;

use super::library;
use super::tests::{cargo, test_crate_in};

/// The commerce example plus an endpoint that inserts, then requires a row that is missing.
fn source() -> String {
    let mut source = include_str!("../../../examples/commerce/src/customer.bd").to_string();
    source.push_str(
        r#"
server fun falha(nome: String, email: Email, saldo: Money, outro: CustomerId): Customer
    effects [db.read, db.write] {
    val _ = db.customers.insert { name = nome; email = email; balance = saldo }
    return db.customers.require(outro)
}
"#,
    );
    source
}

/// `CREATE TABLE` for the CSV schema's `customers` table (the example only uses these types).
pub(crate) fn create_table(csv: &str) -> String {
    let columns: Vec<String> = csv
        .lines()
        .skip(2)
        .filter(|line| line.starts_with("customers,"))
        .map(|line| {
            let fields: Vec<&str> = line.split(',').collect();
            let field = |index: usize| {
                fields
                    .get(index)
                    .copied()
                    .unwrap_or_else(|| panic!("schema line without column {index}: {line}"))
            };
            let (name, kind, nullable, has_default) =
                (field(3), field(4), field(5) == "true", field(7) == "true");
            let sql_type = match kind {
                "CustomerId" => "uuid",
                "Money" => "numeric(20, 2)",
                "String" | "Email" => "text",
                other => panic!("create_table: unknown column type {other} for {name}"),
            };
            format!(
                "{name} {sql_type}{}{}",
                if nullable { "" } else { " NOT NULL" },
                match (name, has_default) {
                    ("id", true) => " PRIMARY KEY DEFAULT gen_random_uuid()",
                    _ => "",
                }
            )
        })
        .collect();
    format!("CREATE TABLE customers ({})", columns.join(", "))
}

const TEST: &str = r##"#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

fn sql(text: String) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(text)
}

#[test]
fn failed_rpc_rolls_back_the_insert() {
    let Ok(url) = std::env::var("BIND_TEST_DATABASE_URL") else {
        eprintln!("skipped: BIND_TEST_DATABASE_URL not set");
        return;
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_stack_size(64 * 1024 * 1024)
        .build()
        .unwrap();
    runtime.block_on(async move {
        let schema = format!("bind_test_{}", std::process::id());
        let admin = sqlx::PgPool::connect(&url).await.unwrap();
        sqlx::query(sql(format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))).execute(&admin).await.unwrap();
        sqlx::query(sql(format!("CREATE SCHEMA {schema}"))).execute(&admin).await.unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .after_connect({
                let schema = schema.clone();
                move |connection, _| {
                    let schema = schema.clone();
                    Box::pin(async move {
                        sqlx::Executor::execute(&mut *connection, sql(format!("SET search_path TO {schema}"))).await?;
                        Ok(())
                    })
                }
            })
            .connect(&url)
            .await
            .unwrap();
        sqlx::query(sql("@@CREATE_TABLE@@".to_string())).execute(&pool).await.unwrap();
        let count = || async {
            sqlx::query_scalar::<_, i64>(sql("SELECT count(*) FROM customers".to_string()))
                .fetch_one(&pool)
                .await
                .unwrap()
        };

        let failed = bind_server_transactions::handle_rpc(
            "/api/falha",
            br#"{"nome":"n","email":"e@x.y","saldo":"1.00","outro":"00000000-0000-4000-8000-000000000000"}"#,
            &pool,
        )
        .await;
        let after_failed = count().await;

        let created = bind_server_transactions::handle_rpc(
            "/api/cadastrar",
            br#"{"nome":"Ana","email":"a@b.c","saldo":"12.50"}"#,
            &pool,
        )
        .await;
        let after_created = count().await;

        pool.close().await;
        sqlx::query(sql(format!("DROP SCHEMA {schema} CASCADE"))).execute(&admin).await.unwrap();
        assert_eq!(failed.status, 404, "{}", failed.body);
        assert_eq!(after_failed, 0, "the insert must be rolled back");
        assert_eq!(created.status, 200, "{}", created.body);
        assert_eq!(after_created, 1);
    });
}
"##;

#[test]
#[ignore = "builds a Rust crate; run with `cargo test transactions -- --ignored`"]
fn failed_rpc_rolls_back_the_insert() {
    if std::env::var_os("BIND_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: BIND_TEST_DATABASE_URL not set");
        return;
    }
    let csv = include_str!("../../../examples/commerce/schema/main.csv");
    let schema = crate::core::lower::tests::schema(csv);
    let program = crate::core::lower::tests::compile_with(&source(), &schema);
    let module = library(&program, &schema).unwrap();
    let root =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/bind-codegen-tests");
    let crate_dir = test_crate_in(&root, "bind-server-transactions");
    fs::write(crate_dir.join("src/lib.rs"), module).unwrap();
    fs::create_dir_all(crate_dir.join("tests")).unwrap();
    fs::write(
        crate_dir.join("tests/transactions.rs"),
        TEST.replace("@@CREATE_TABLE@@", &create_table(csv)),
    )
    .unwrap();
    cargo(
        &crate_dir,
        &["clippy", "--quiet", "--all-targets", "--", "-D", "warnings"],
    );
    let output = cargo(&crate_dir, &["test", "--quiet"]);
    eprintln!("transactions: ran against live database\n{output}");
}
