//! End to end, against PostgreSQL (`BIND_TEST_DATABASE_URL`) and `node`, in both modes.
//!
//! `bindc` verifies and generates (`certified::generate`) the commerce example
//! (`examples/commerce`) for two targets: `ssr` and `split` (with
//! `api_url = "http://api.test/api"` and `frontend_origin = "http://a.test"`, the cross-origin
//! deployment), plus a variant whose only component writes, then fails (`FAILING_PAGE`). The
//! generated libraries (`src/lib.rs` + `src/bind_runtime.rs`, as generated) become three crates
//! of a harness workspace whose integration test (`HARNESS_TEST`):
//!
//! - creates its own schema with the example's `customers` table and drops it at the end, even
//!   when an assertion fails;
//! - serves each library over HTTP on a local port with a minimal HTTP/1.1 server (`std::net`
//!   and the tokio runtime the libraries already use; the routing of the generated Lambda entry
//!   point: `serve` for the `ssr` mode, `cors` then `serve_rpc` for the `split` mode);
//! - SSR: `GET /?id=` of an existing customer (200, name and e-mail escaped), of a missing one
//!   (404, the public message), `?id=xyz` (400); the failing page (404) leaves 0 new rows, and
//!   the same page without the failure writes its row;
//! - CORS: preflight and `POST` from `Origin: http://a.test` accepted, from `http://b.test`
//!   refused (403, no CORS headers);
//! - split: the generated `web/app.js` booted in node (`runtime/bind-runtime-js/tests/dom.mjs`,
//!   node's own `fetch`) behind a `fetch` that behaves as a browser on `http://a.test` would:
//!   `http://api.test` is the test server, every call is preceded by its CORS preflight and the
//!   response must allow the page's origin. Happy path, 404 and invalid parameter render exactly
//!   the SSR's HTML; Review Focus 2: a test double answering `500` with the body `oops`, a page on
//!   the refused origin and the server shut down all show the "Request failed" screen, with no
//!   unhandled exception (node exits non-zero on one);
//! - the wire boundary (`WIRE_PAGE`, both modes of a fourth project): a NaN argument, a NaN
//!   result, a 1001-digit `Int` argument and a 1001-digit `Int` result give the same page in
//!   the `ssr` mode (server calls through the wire codecs in memory) as in the `split` mode
//!   (real RPCs from node): "Bad request" with serde's message for the arguments, "Request
//!   failed" for the results. (A `Long` cannot cross the wire in rust-web-v3, E8309: the
//!   runtime's unit tests cover its ±(2^53 − 1) rule.)
//! - collections and rich types (`RICH_PAGE`, both modes of a fifth project): an endpoint that
//!   receives a `Map<CustomerId, Int>` and returns a sealed class with fields holding a
//!   `Set<String>` and a `Result<Customer, Erro>`, read from the database; the page renders
//!   the same HTML in both modes for a found customer (`Ok`), a missing one (`Err`) and an
//!   empty map (the variant without fields). Review Focus 5: a request whose `Map` has a
//!   malformed pair or a duplicate key is a 400 with serde's message on both servers, and a
//!   response tampered by a test proxy with a duplicated `Set` element shows "Request failed"
//!   in the split page (the same response with its `Set` out of order is accepted).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::backend::rust::tests::cargo;
use crate::backend::rust::transactions::create_table;
use crate::config::ProjectConfig;

const COMMERCE: &str = include_str!("../../../examples/commerce/src/customer.bd");
const SCHEMA: &str = include_str!("../../../examples/commerce/schema/main.csv");

/// The commerce example reading its database URL from `BIND_TEST_DATABASE_URL` (which `verify`
/// requires to be set in this process, as it does `DATABASE_URL` for the example) instead of
/// `DATABASE_URL`; nothing else changes.
fn commerce() -> String {
    let source = COMMERCE.replace("env(\"DATABASE_URL\")", "env(\"BIND_TEST_DATABASE_URL\")");
    assert_ne!(source, COMMERCE);
    source
}

/// The commerce example's page component, replaced by `FAILING_PAGE` in the failing variant.
const CUSTOMER_PAGE: &str = "client component CustomerPage(id: CustomerId) {";

/// A page that writes (`cadastrar`), then fails (`require` of `outro`, missing): the whole page
/// transaction must be rolled back.
const FAILING_PAGE: &str = r#"client component Falha(id: CustomerId, outro: CustomerId, nome: String, email: Email) {
    val existente = server.getCustomer(id)
    val novo = server.cadastrar(nome, email, existente.balance)
    val falta = server.getCustomer(outro)

    UI {
        Text(novo.name)
        Text(falta.name)
    }
}
"#;

/// Server calls at the wire's limits, selected by the page parameter `k` (replaces the
/// commerce page in the wire variant).
const WIRE_PAGE: &str = r#"server fun eco(x: Double): Double = x
server fun naoNumero(): Double = 0.0 / 0.0
server fun ecoInt(n: Int): Int = n
server fun grande(): Int = 10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000

client component Fio(k: Int) {
    UI {
        when (k) {
            1 -> Text(server.eco(0.0 / 0.0))
            2 -> Text(server.naoNumero())
            3 -> Text(server.ecoInt(10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000))
            4 -> Text(server.grande())
            _ -> Text(server.eco(1.5))
        }
    }
}
"#;

/// Collections and rich types on the wire (replaces the commerce page in the rich variant):
/// `resumo` receives a `Map<CustomerId, Int>` and returns a sealed class whose variant with
/// fields holds a `Set<String>` and a `Result<Customer, Erro>`; the page `Rico` renders them.
const RICH_PAGE: &str = r#"data class Erro(val codigo: Int, val mensagem: String)

sealed class Resumo {
    data class Pronto(val tags: Set<String>, val dono: Result<Customer, Erro>, val total: Int, val saldo: Int)
    data object Vazio
}

fun soma(xs: List<Int>): Int = fold(xs, 0, { s, x -> s + x })

fun junta(xs: List<String>): String = fold(xs, "", { s, x -> s + "[" + x + "]" })

fun dono(achado: Customer?): Result<Customer, Erro> = when (achado) {
    null -> Err(Erro(404, "cliente ausente"))
    c -> Ok(c)
}

fun marcas(dono: Result<Customer, Erro>, total: Int): Set<String> = when (dono) {
    Ok(c) -> insert(setOf("vip", "ativo", "vip"), c.name)
    Err(_) -> if (total > 10) setOf("sem dono", "grande") else setOf("sem dono")
}

server fun resumo(id: CustomerId, saldos: Map<CustomerId, Int>): Resumo
    effects [db.read] {
    val achado = dono(db.customers.find(id))
    val total = soma(values(saldos))
    return if (mapSize(saldos) == 0) Vazio else Pronto(marcas(achado, total), achado, total, lookup(saldos, id) ?: -1)
}

fun quem(dono: Result<Customer, Erro>): String = when (dono) {
    Ok(c) -> c.name
    Err(e) -> e.mensagem
}

fun descreve(r: Resumo): String = when (r) {
    Pronto(tags, dono, _, _) -> junta(toList(tags)) + " " + quem(dono)
    Vazio -> "vazio"
}

fun numero(r: Resumo): Int = when (r) {
    Pronto(_, Err(e), total, saldo) -> e.codigo * 1000000 + total * 1000 + saldo
    Pronto(_, _, total, saldo) -> total * 1000 + saldo
    Vazio -> 0
}

fun saldos(id: CustomerId, outro: CustomerId, k: Int): Map<CustomerId, Int> =
    if (k == 0) emptyMap() else put(put(put(emptyMap(), outro, 7), id, k), outro, 5)

client component Rico(id: CustomerId, outro: CustomerId, k: Int) {
    val r = server.resumo(id, saldos(id, outro, k))

    UI {
        Text(descreve(r))
        Text(numero(r))
    }
}
"#;

const MANIFEST: &str = r#"[project]
name = "commerce"
version = "0.1.0"
source = "src"

[database.main]
engine = "postgresql"
schema = "schema/main.csv"
env = "BIND_TEST_DATABASE_URL"
verification = "static"

[target.ssr]
client = "web"
server = "rust"
mode = "ssr"
deployment = "aws-lambda"
certified = true
capabilities = ["ui.dom", "server.http", "db.read", "db.write"]

[target.split]
client = "web"
server = "rust"
mode = "split"
deployment = "aws-lambda"
api_url = "http://api.test/api"
frontend_origin = "http://a.test"
certified = true
capabilities = ["ui.dom", "server.http", "db.read", "db.write"]
"#;

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// A temporary project with `source` as its only Bind file and the commerce schema.
struct Project {
    root: PathBuf,
}

impl Project {
    fn new(name: &str, source: &str) -> Project {
        let root = std::env::temp_dir().join(format!("bindc-e2e-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (path, content) in [
            ("src/customer.bd", source),
            ("schema/main.csv", SCHEMA),
            ("bind.toml", MANIFEST),
        ] {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        Project { root }
    }

    /// Verifies and generates `target`; returns the generated folder.
    fn generate(&self, target: &str) -> PathBuf {
        let config = ProjectConfig::load(&self.root.join("bind.toml")).unwrap();
        let result = crate::compiler::verify(&config, target);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let app = result.application.unwrap();
        let output = self.root.join("generated").join(target);
        crate::certified::generate(&app, &config, Some(&output)).unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A member crate `harness/<directory>` named `name` holding the generated library of
/// `generated` (its `src/lib.rs` and `src/bind_runtime.rs`, unchanged).
fn member(harness: &Path, directory: &str, name: &str, generated: &Path) {
    let crate_dir = harness.join(directory);
    fs::create_dir_all(crate_dir.join("src")).unwrap();
    fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\n{}",
            crate::certified::server_dependencies()
        ),
    )
    .unwrap();
    for file in ["src/lib.rs", "src/bind_runtime.rs"] {
        fs::copy(generated.join(file), crate_dir.join(file)).unwrap();
    }
}

#[test]
#[ignore = "builds Rust crates and needs node; run with `cargo test end_to_end -- --ignored`"]
fn ssr_and_split_against_postgres() {
    if std::env::var_os("BIND_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: BIND_TEST_DATABASE_URL not set");
        return;
    }
    if !node_available() {
        eprintln!("skipped: node not found");
        return;
    }
    let source = commerce();
    assert!(source.contains(CUSTOMER_PAGE));
    let commerce = Project::new("commerce", &source);
    let ssr = commerce.generate("ssr");
    let split = commerce.generate("split");
    let failing_source = format!(
        "{}{FAILING_PAGE}",
        source.split(CUSTOMER_PAGE).next().unwrap()
    );
    let failing = Project::new("failing", &failing_source);
    let fail = failing.generate("ssr");
    let wire_source = format!("{}{WIRE_PAGE}", source.split(CUSTOMER_PAGE).next().unwrap());
    let wire = Project::new("wire", &wire_source);
    let wire_ssr = wire.generate("ssr");
    let wire_split = wire.generate("split");
    let rich_source = format!("{}{RICH_PAGE}", source.split(CUSTOMER_PAGE).next().unwrap());
    let rich = Project::new("rich", &rich_source);
    let rich_ssr = rich.generate("ssr");
    let rich_split = rich.generate("split");

    let harness =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/bind-codegen-tests/bind-e2e");
    if harness.exists() {
        fs::remove_dir_all(&harness).unwrap();
    }
    fs::create_dir_all(harness.join("src")).unwrap();
    fs::create_dir_all(harness.join("tests")).unwrap();
    fs::write(
        harness.join("Cargo.toml"),
        format!(
            "[package]\nname = \"bind-e2e\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[workspace]\n\n[dependencies]\n{}bind-e2e-ssr = {{ path = \"ssr\" }}\nbind-e2e-split = {{ path = \"split\" }}\nbind-e2e-fail = {{ path = \"fail\" }}\nbind-e2e-wire-ssr = {{ path = \"wire-ssr\" }}\nbind-e2e-wire-split = {{ path = \"wire-split\" }}\nbind-e2e-rich-ssr = {{ path = \"rich-ssr\" }}\nbind-e2e-rich-split = {{ path = \"rich-split\" }}\n",
            crate::certified::server_dependencies()
        ),
    )
    .unwrap();
    let runtime = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime/bind-runtime");
    fs::copy(runtime.join("Cargo.lock"), harness.join("Cargo.lock")).unwrap();
    fs::write(
        harness.join("src/lib.rs"),
        "//! Harness of bindc's end-to-end test: see `tests/end_to_end.rs`.\n",
    )
    .unwrap();
    member(&harness, "ssr", "bind-e2e-ssr", &ssr);
    member(&harness, "split", "bind-e2e-split", &split);
    member(&harness, "fail", "bind-e2e-fail", &fail);
    member(&harness, "wire-ssr", "bind-e2e-wire-ssr", &wire_ssr);
    member(&harness, "wire-split", "bind-e2e-wire-split", &wire_split);
    member(&harness, "rich-ssr", "bind-e2e-rich-ssr", &rich_ssr);
    member(&harness, "rich-split", "bind-e2e-rich-split", &rich_split);
    // The split page as generated: index.html, app.js (API_URL = "http://api.test/api") and the
    // runtime.
    fs::create_dir_all(harness.join("web")).unwrap();
    for file in ["index.html", "app.js", "bind-runtime.js"] {
        fs::copy(split.join("web").join(file), harness.join("web").join(file)).unwrap();
    }
    for (directory, generated) in [("web-wire", &wire_split), ("web-rich", &rich_split)] {
        fs::create_dir_all(harness.join(directory)).unwrap();
        for file in ["index.html", "app.js", "bind-runtime.js"] {
            fs::copy(
                generated.join("web").join(file),
                harness.join(directory).join(file),
            )
            .unwrap();
        }
    }
    let index = fs::read_to_string(harness.join("web/index.html")).unwrap();
    assert!(index.contains("from \"/app.js\"; page.boot("), "{index}");
    let app = fs::read_to_string(harness.join("web/app.js")).unwrap();
    assert!(
        app.contains("const API_URL = \"http://api.test/api\";"),
        "{app}"
    );
    fs::write(
        harness.join("dom.mjs"),
        include_str!("../../../runtime/bind-runtime-js/tests/dom.mjs"),
    )
    .unwrap();
    fs::write(harness.join("page.mjs"), PAGE_SCRIPT).unwrap();
    fs::write(
        harness.join("tests/end_to_end.rs"),
        HARNESS_TEST.replace("@@CREATE_TABLE@@", &create_table(SCHEMA)),
    )
    .unwrap();

    cargo(
        &harness,
        &[
            "clippy",
            "--quiet",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    );
    let output = cargo(
        &harness,
        &["test", "--quiet", "--workspace", "--", "--nocapture"],
    );
    println!("end_to_end: ran against live database\n{output}");
    assert!(output.contains("end-to-end: ok"), "{output}");
}

/// Boots the generated split page (`web/app.js`, or the module given as a fourth argument) in
/// node: `node page.mjs <server> <origin> <search> [app]` prints `{"html": <the page's div.bind-site>, "requests": [...]}`. `fetch` behaves as
/// in a browser showing the page on `<origin>`: calls to `http://api.test` go to `<server>`, each
/// after its CORS preflight, and a response that does not allow `<origin>` is a network error
/// (`TypeError`). An unhandled exception or rejection exits non-zero.
const PAGE_SCRIPT: &str = r#"import { createDocument, serialize } from "./dom.mjs";

const [server, pageOrigin, search, app = "./web/app.js"] = process.argv.slice(2);
const { "bind:page": page } = await import(app);
const API_ORIGIN = "http://api.test";
process.on("unhandledRejection", (error) => {
  console.error("unhandled rejection:", error);
  process.exit(2);
});
process.on("uncaughtException", (error) => {
  console.error("uncaught exception:", error);
  process.exit(3);
});

const nodeFetch = globalThis.fetch;
const requests = [];
const list = (value) => (value ?? "").split(",").map((item) => item.trim().toLowerCase()).filter((item) => item !== "");
globalThis.fetch = async (url, init = {}) => {
  const target = new URL(String(url));
  if (target.origin !== API_ORIGIN) throw new TypeError(`unexpected URL ${url}`);
  const actual = new URL(target.pathname + target.search, server);
  const method = init.method ?? "GET";
  const headers = Object.keys(init.headers ?? {}).map((name) => name.toLowerCase()).sort();
  requests.push(`OPTIONS ${target.pathname}`);
  const preflight = await nodeFetch(actual, {
    method: "OPTIONS",
    headers: {
      origin: pageOrigin,
      "access-control-request-method": method,
      "access-control-request-headers": headers.join(","),
    },
  });
  await preflight.arrayBuffer();
  const allowedHeaders = list(preflight.headers.get("access-control-allow-headers"));
  if (
    !preflight.ok ||
    preflight.headers.get("access-control-allow-origin") !== pageOrigin ||
    !list(preflight.headers.get("access-control-allow-methods")).includes(method.toLowerCase()) ||
    headers.some((name) => !allowedHeaders.includes(name))
  ) {
    throw new TypeError("Failed to fetch (CORS preflight refused)");
  }
  requests.push(`${method} ${target.pathname}`);
  const response = await nodeFetch(actual, { ...init, headers: { ...init.headers, origin: pageOrigin } });
  if (response.headers.get("access-control-allow-origin") !== pageOrigin) {
    await response.arrayBuffer();
    throw new TypeError("Failed to fetch (CORS refused)");
  }
  return response;
};

const document = createDocument();
const container = document.createElement("div");
container.setAttribute("class", "bind-site");
document.body.appendChild(container);
await page.boot(container, search, document);
if (document.htmlApiUsed.length > 0) throw new Error(`HTML string APIs used: ${document.htmlApiUsed}`);
console.log(JSON.stringify({ html: serialize(container), requests }));
"#;

/// The harness's integration test; `@@CREATE_TABLE@@` is the example's table.
const HARNESS_TEST: &str = r##"#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::panic::AssertUnwindSafe;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn sql(text: String) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(text)
}

const RPC_CONTENT_TYPE: &str = "application/json; charset=utf-8";
const MISSING: &str = "00000000-0000-4000-8000-000000000000";
const OTHER: &str = "11111111-1111-4111-8111-111111111111";

/// One parsed HTTP request.
struct Request {
    method: String,
    path: String,
    query: String,
    origin: Option<String>,
    body: Vec<u8>,
}

struct Response {
    status: u16,
    content_type: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// Request line, headers (`Content-Length`, `Origin`) and body; `None` on malformed input.
fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut length = 0usize;
    let mut origin = None;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().ok()?,
            "origin" => origin = Some(value.trim().to_string()),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };
    Some(Request { method, path, query, origin, body })
}

fn write_response(mut stream: &TcpStream, head: bool, response: &Response) {
    let reason = match response.status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let body = if head { "" } else { response.body.as_str() };
    let mut text = format!(
        "HTTP/1.1 {} {reason}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n",
        response.status,
        response.content_type,
        body.len()
    );
    for (name, value) in &response.headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str("\r\n");
    text.push_str(body);
    let _ = stream.write_all(text.as_bytes());
    let _ = stream.flush();
}

type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// A minimal HTTP/1.1 server on `127.0.0.1` (one thread per connection, `Connection: close`).
struct Server {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(handler: Handler) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let stop = stop.clone();
            move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let handler = handler.clone();
                    std::thread::spawn(move || {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                        if let Some(request) = read_request(&stream) {
                            let head = request.method == "HEAD";
                            let response = handler(request);
                            write_response(&stream, head, &response);
                        }
                        let _ = stream.shutdown(Shutdown::Both);
                    });
                }
            }
        });
        Server { address, stop, thread: Some(thread) }
    }

    fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Stops accepting and closes the listening socket: later connections are refused.
    fn shut_down(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shut_down();
    }
}

/// The `ssr` routing of the generated Lambda: `HEAD` as `GET` without a body, then `serve`.
macro_rules! page_handler {
    ($krate:ident, $handle:expr, $pool:expr) => {{
        let handle = $handle.clone();
        let pool = $pool.clone();
        let handler: Handler = Arc::new(move |request: Request| {
            let method = if request.method == "HEAD" { "GET".to_string() } else { request.method };
            let response = handle.block_on($krate::serve(&method, request.path, request.query, request.body, pool.clone()));
            Response {
                status: response.status,
                content_type: response.content_type.to_string(),
                headers: Vec::new(),
                body: response.body,
            }
        });
        handler
    }};
}

/// The `split` routing of the generated Lambda with `frontend_origin`: `cors`, then `serve_rpc`.
macro_rules! split_handler {
    ($krate:ident, $handle:expr, $pool:expr) => {{
        let handle = $handle.clone();
        let pool = $pool.clone();
        let handler: Handler = Arc::new(move |request: Request| {
            let cors = $krate::cors(&request.method, request.origin.as_deref());
            let headers: Vec<(String, String)> =
                cors.headers.iter().map(|(name, value)| (name.to_string(), value.clone())).collect();
            if let Some(result) = cors.response {
                return Response { status: result.status, content_type: RPC_CONTENT_TYPE.to_string(), headers, body: result.body };
            }
            let result = handle.block_on($krate::serve_rpc(request.path, request.body, pool.clone()));
            Response { status: result.status, content_type: RPC_CONTENT_TYPE.to_string(), headers, body: result.body }
        });
        handler
    }};
}

/// Test double of a failing API: CORS allows `http://a.test` (so the browser exposes the
/// response), and every call answers `500` with the body `oops`, which is not JSON.
fn oops_handler() -> Handler {
    Arc::new(|request: Request| {
        let mut headers = vec![
            ("access-control-allow-origin".to_string(), "http://a.test".to_string()),
            ("vary".to_string(), "Origin".to_string()),
        ];
        if request.method == "OPTIONS" {
            headers.push(("access-control-allow-methods".to_string(), "POST, OPTIONS".to_string()));
            headers.push(("access-control-allow-headers".to_string(), "content-type, accept".to_string()));
            return Response { status: 204, content_type: "text/plain".to_string(), headers, body: String::new() };
        }
        Response { status: 500, content_type: "text/plain; charset=utf-8".to_string(), headers, body: "oops".to_string() }
    })
}

/// `inner`, with the `tags` of a successful `{"pronto": {...}}` response changed by `tamper`
/// (a test double of a server whose response is corrupted on the way).
fn tampering_handler(inner: Handler, tamper: fn(&mut Vec<serde_json::Value>)) -> Handler {
    Arc::new(move |request: Request| {
        let post = request.method == "POST";
        let mut response = inner(request);
        if post && response.status == 200 {
            let mut value: serde_json::Value = serde_json::from_str(&response.body).unwrap();
            tamper(value["pronto"]["tags"].as_array_mut().unwrap());
            response.body = value.to_string();
        }
        response
    })
}

/// A response read by the raw client.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    fn has_cors_headers(&self) -> bool {
        self.headers.iter().any(|(key, _)| key.starts_with("access-control-"))
    }
}

/// One HTTP request to `server` over a fresh connection.
fn http(server: &Server, method: &str, target: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let mut stream = TcpStream::connect(server.address).unwrap();
    let mut text = format!(
        "{method} {target} HTTP/1.1\r\nhost: {}\r\ncontent-length: {}\r\nconnection: close\r\n",
        server.address,
        body.len()
    );
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str("\r\n");
    text.push_str(body);
    stream.write_all(text.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            (name.trim().to_ascii_lowercase(), value.trim().to_string())
        })
        .collect();
    Reply { status, headers, body: body.to_string() }
}

/// The content of the page's `div.bind-site`.
fn site(page: &str) -> String {
    let start = page.find("<div class=\"bind-site\">").unwrap() + "<div class=\"bind-site\">".len();
    let end = page.rfind("</div>\n  </body>").unwrap();
    page[start..end].to_string()
}

/// The generated split page booted in node on `origin`, its API at `server`: the content of
/// its `div.bind-site` and the requests it made.
fn split_page(server: &str, origin: &str, search: &str) -> (String, Vec<String>) {
    split_page_of("./web/app.js", server, origin, search)
}

/// `split_page` of the split page whose module is `app`.
fn split_page_of(app: &str, server: &str, origin: &str, search: &str) -> (String, Vec<String>) {
    let output = Command::new("node")
        .args(["page.mjs", server, origin, search, app])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "node page.mjs {search} failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let html = value["html"].as_str().unwrap();
    let inner = html
        .strip_prefix("<div class=\"bind-site\">")
        .and_then(|rest| rest.strip_suffix("</div>"))
        .unwrap()
        .to_string();
    let requests = value["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|request| request.as_str().unwrap().to_string())
        .collect();
    (inner, requests)
}

const REQUEST_FAILED: &str = "<main class=\"bind-error\"><h1>Something went wrong</h1><p>Request failed</p></main>";

fn scenarios(runtime: &tokio::runtime::Runtime, url: &str, schema: &str) {
    let pool = runtime.block_on(
        sqlx::postgres::PgPoolOptions::new()
            .after_connect({
                let schema = schema.to_string();
                move |connection, _| {
                    let schema = schema.clone();
                    Box::pin(async move {
                        sqlx::Executor::execute(&mut *connection, sql(format!("SET search_path TO {schema}"))).await?;
                        Ok(())
                    })
                }
            })
            .connect(url),
    )
    .unwrap();
    runtime.block_on(sqlx::query(sql("@@CREATE_TABLE@@".to_string())).execute(&pool)).unwrap();
    let ana: String = runtime
        .block_on(
            sqlx::query_scalar(sql(
                "INSERT INTO customers (name, email, balance) VALUES ('Ana & <Bia>', 'ana@example.com', 12.50) RETURNING id::text".to_string(),
            ))
            .fetch_one(&pool),
        )
        .unwrap();
    let count = |condition: &str| -> i64 {
        runtime
            .block_on(sqlx::query_scalar(sql(format!("SELECT count(*) FROM customers WHERE {condition}"))).fetch_one(&pool))
            .unwrap()
    };
    let handle = runtime.handle().clone();
    let customer = "Ana &amp; &lt;Bia&gt;ana@example.com";

    // SSR: the page with data, `require` of a missing row, an invalid parameter.
    let ssr = Server::start(page_handler!(bind_e2e_ssr, handle, pool));
    let found = http(&ssr, "GET", &format!("/?id={ana}"), &[], "");
    assert_eq!(found.status, 200, "{}", found.body);
    assert_eq!(found.header("content-type"), Some("text/html; charset=utf-8"));
    assert_eq!(site(&found.body), customer);
    let missing = http(&ssr, "GET", &format!("/?id={MISSING}"), &[], "");
    assert_eq!(missing.status, 404, "{}", missing.body);
    let not_found = "<main class=\"bind-error\"><h1>Page not found</h1><p>not found</p></main>";
    assert_eq!(site(&missing.body), not_found);
    let invalid = http(&ssr, "GET", "/?id=xyz", &[], "");
    assert_eq!(invalid.status, 400, "{}", invalid.body);
    let bad_request = "<main class=\"bind-error\"><h1>Bad request</h1><p>invalid parameter: id</p></main>";
    assert_eq!(site(&invalid.body), bad_request);
    let head = http(&ssr, "HEAD", &format!("/?id={ana}"), &[], "");
    assert_eq!((head.status, head.body.as_str()), (200, ""));
    let rpc = http(&ssr, "POST", "/api/getCustomer", &[("content-type", "application/json")], &format!(r#"{{"id":"{ana}"}}"#));
    assert_eq!(rpc.status, 200, "{}", rpc.body);
    let value: serde_json::Value = serde_json::from_str(&rpc.body).unwrap();
    assert_eq!((value["name"].as_str(), value["balance"].as_str()), (Some("Ana & <Bia>"), Some("12.50")));
    println!("ssr: 200 page with data, 404 from require, 400 for ?id=xyz, HEAD and RPC");

    // SSR: a page that writes, then fails, leaves nothing written.
    let fail = Server::start(page_handler!(bind_e2e_fail, handle, pool));
    let before = count("true");
    let failed = http(&fail, "GET", &format!("/?id={ana}&outro={MISSING}&nome=Novo&email=novo%40example.com"), &[], "");
    assert_eq!(failed.status, 404, "{}", failed.body);
    assert_eq!(site(&failed.body), not_found);
    assert_eq!(count("name = 'Novo'"), 0, "the page's insert must be rolled back");
    assert_eq!(count("true"), before);
    let written = http(&fail, "GET", &format!("/?id={ana}&outro={ana}&nome=Novo&email=novo%40example.com"), &[], "");
    assert_eq!(written.status, 200, "{}", written.body);
    assert_eq!(site(&written.body), "NovoAna &amp; &lt;Bia&gt;");
    assert_eq!(count("name = 'Novo' AND email = 'novo@example.com' AND balance = 12.50"), 1);
    println!("ssr: write then fail -> 404 and 0 rows; without the failure the row is committed");

    // CORS of the split server (frontend_origin = "http://a.test").
    let mut split = Server::start(split_handler!(bind_e2e_split, handle, pool));
    let body = format!(r#"{{"id":"{ana}"}}"#);
    let preflight = |origin: &str| {
        http(
            &split,
            "OPTIONS",
            "/api/getCustomer",
            &[("origin", origin), ("access-control-request-method", "POST"), ("access-control-request-headers", "accept,content-type")],
            "",
        )
    };
    let post = |origin: &str| http(&split, "POST", "/api/getCustomer", &[("origin", origin), ("content-type", "application/json")], &body);
    let accepted = preflight("http://a.test");
    assert_eq!(accepted.status, 204, "{}", accepted.body);
    assert_eq!(accepted.header("access-control-allow-origin"), Some("http://a.test"));
    assert!(accepted.header("access-control-allow-methods").unwrap().contains("POST"));
    assert!(accepted.header("access-control-allow-headers").unwrap().contains("content-type"));
    let called = post("http://a.test");
    assert_eq!(called.status, 200, "{}", called.body);
    assert_eq!(called.header("access-control-allow-origin"), Some("http://a.test"));
    assert!(called.body.contains(&ana), "{}", called.body);
    for refused in [preflight("http://b.test"), post("http://b.test")] {
        assert_eq!(refused.status, 403, "{}", refused.body);
        assert!(!refused.has_cors_headers(), "{:?}", refused.headers);
        assert_eq!(refused.body, r#"{"error":"origin not allowed"}"#);
    }
    let direct = http(&split, "POST", "/api/getCustomer", &[("content-type", "application/json")], &body);
    assert_eq!(direct.status, 200, "{}", direct.body);
    assert!(!direct.has_cors_headers());
    println!("cors: preflight and POST from http://a.test accepted, from http://b.test refused (403)");

    // Split: the generated app.js in node, against the split server.
    let server = split.origin();
    let calls = vec!["OPTIONS /api/getCustomer".to_string(), "POST /api/getCustomer".to_string()];
    let (html, requests) = split_page(&server, "http://a.test", &format!("?id={ana}"));
    assert_eq!(html, customer);
    assert_eq!(requests, calls);
    let (html, requests) = split_page(&server, "http://a.test", &format!("?id={MISSING}"));
    assert_eq!(html, not_found);
    assert_eq!(requests, calls);
    let (html, requests) = split_page(&server, "http://a.test", "?id=xyz");
    assert_eq!(html, bad_request);
    assert!(requests.is_empty(), "{requests:?}");
    println!("split: happy path, 404 and invalid parameter render the SSR's HTML");

    // Review Focus 2: no usable response -> the "Request failed" screen, no exception.
    let preflight_only = vec!["OPTIONS /api/getCustomer".to_string()];
    let (html, requests) = split_page(&server, "http://b.test", &format!("?id={ana}"));
    assert_eq!((html.as_str(), requests), (REQUEST_FAILED, preflight_only.clone()));
    let oops = Server::start(oops_handler());
    let (html, requests) = split_page(&oops.origin(), "http://a.test", &format!("?id={ana}"));
    assert_eq!((html.as_str(), requests), (REQUEST_FAILED, calls.clone()));
    split.shut_down();
    assert!(TcpStream::connect(split.address).is_err(), "the server must be down");
    let (html, requests) = split_page(&server, "http://a.test", &format!("?id={ana}"));
    assert_eq!((html.as_str(), requests), (REQUEST_FAILED, preflight_only));
    println!("split: 500 `oops`, refused origin and server down -> \"Request failed\"");

    // The wire boundary: the same page in both modes.
    let wire_ssr = Server::start(page_handler!(bind_e2e_wire_ssr, handle, pool));
    let wire_split = Server::start(split_handler!(bind_e2e_wire_split, handle, pool));
    let bad_request_prefix = "<main class=\"bind-error\"><h1>Bad request</h1><p>";
    for (k, status, expected) in [
        ("0", 200, "1.5".to_string()),
        (
            "1",
            400,
            format!("{bad_request_prefix}invalid type: null, expected f64 at line 1 column 9</p></main>"),
        ),
        ("2", 500, REQUEST_FAILED.to_string()),
        ("3", 400, String::new()),
        ("4", 500, REQUEST_FAILED.to_string()),
    ] {
        let page = http(&wire_ssr, "GET", &format!("/?k={k}"), &[], "");
        assert_eq!(page.status, status, "k={k}: {}", page.body);
        let from_ssr = site(&page.body);
        let (from_split, _) = split_page_of("./web-wire/app.js", &wire_split.origin(), "http://a.test", &format!("?k={k}"));
        assert_eq!(from_ssr, from_split, "k={k}: ssr and split differ");
        if expected.is_empty() {
            assert!(from_ssr.starts_with(&format!("{bad_request_prefix}invalid Int")), "k={k}: {from_ssr}");
        } else {
            assert_eq!(from_ssr, expected, "k={k}");
        }
    }
    println!("wire: NaN and 1001-digit Int, as arguments and as results, give the same page in ssr and split");

    // Collections and rich types: the same page in both modes.
    let rich_ssr = Server::start(page_handler!(bind_e2e_rich_ssr, handle, pool));
    let mut rich_split = Server::start(split_handler!(bind_e2e_rich_split, handle, pool));
    let resumo = vec!["OPTIONS /api/resumo".to_string(), "POST /api/resumo".to_string()];
    for (search, expected) in [
        (format!("?id={ana}&outro={OTHER}&k=30"), "[Ana &amp; &lt;Bia&gt;][ativo][vip] Ana &amp; &lt;Bia&gt;35030"),
        (format!("?id={MISSING}&outro={OTHER}&k=30"), "[grande][sem dono] cliente ausente404035030"),
        (format!("?id={MISSING}&outro={MISSING}&k=3"), "[sem dono] cliente ausente404005005"),
        (format!("?id={ana}&outro={OTHER}&k=0"), "vazio0"),
    ] {
        let page = http(&rich_ssr, "GET", &format!("/{search}"), &[], "");
        assert_eq!(page.status, 200, "{search}: {}", page.body);
        assert_eq!(site(&page.body), expected, "{search}");
        let (html, requests) = split_page_of("./web-rich/app.js", &rich_split.origin(), "http://a.test", &search);
        assert_eq!(html, expected, "{search}: ssr and split differ");
        assert_eq!(requests, resumo, "{search}");
    }
    println!("rich: Map argument, sealed with Set and Result, Ok/Err/fieldless give the same page in ssr and split");

    // Review Focus 5, server: a malformed Map is a 400 with serde's message.
    for (saldos, message) in [
        (format!(r#"[["{ana}"]]"#), "malformed map pair at line 1 column "),
        (format!(r#"[["{ana}","1","2"]]"#), "malformed map pair at line 1 column "),
        (format!(r#"[["{ana}","1"],["{ana}","2"]]"#), "duplicate map key"),
    ] {
        let body = format!(r#"{{"id":"{ana}","saldos":{saldos}}}"#);
        for (server, headers) in [
            (&rich_ssr, vec![("content-type", "application/json")]),
            (&rich_split, vec![("content-type", "application/json"), ("origin", "http://a.test")]),
        ] {
            let reply = http(server, "POST", "/api/resumo", &headers, &body);
            assert_eq!(reply.status, 400, "{body}: {}", reply.body);
            let value: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
            let error = value["error"].as_str().unwrap();
            assert!(error.starts_with(message), "{body}: {error}");
        }
    }
    let well_formed = format!(r#"{{"id":"{ana}","saldos":[["{OTHER}","5"],["{ana}","30"]]}}"#);
    let reply = http(&rich_split, "POST", "/api/resumo", &[("content-type", "application/json"), ("origin", "http://a.test")], &well_formed);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let value: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(value["pronto"]["tags"], serde_json::json!(["Ana & <Bia>", "ativo", "vip"]), "{}", reply.body);
    assert_eq!(value["pronto"]["dono"]["Ok"]["id"].as_str(), Some(ana.as_str()), "{}", reply.body);
    assert_eq!((value["pronto"]["total"].as_str(), value["pronto"]["saldo"].as_str()), (Some("35"), Some("30")));
    println!("rich: Map with a malformed pair or a duplicate key -> 400 with serde's message on both servers");

    // Review Focus 5, frontend: the split server's response, tampered by a proxy.
    let search = format!("?id={ana}&outro={OTHER}&k=30");
    let reversed = Server::start(tampering_handler(split_handler!(bind_e2e_rich_split, handle, pool), |tags| tags.reverse()));
    let (html, requests) = split_page_of("./web-rich/app.js", &reversed.origin(), "http://a.test", &search);
    assert_eq!(html, "[Ana &amp; &lt;Bia&gt;][ativo][vip] Ana &amp; &lt;Bia&gt;35030", "a Set out of order is normalized");
    assert_eq!(requests, resumo);
    let duplicated = Server::start(tampering_handler(split_handler!(bind_e2e_rich_split, handle, pool), |tags| {
        let first = tags[0].clone();
        tags.push(first);
    }));
    let (html, requests) = split_page_of("./web-rich/app.js", &duplicated.origin(), "http://a.test", &search);
    assert_eq!((html.as_str(), requests), (REQUEST_FAILED, resumo.clone()));
    rich_split.shut_down();
    println!("rich: response with a duplicated Set element -> \"Request failed\"; out of order -> normalized");

    runtime.block_on(pool.close());
}

#[test]
fn ssr_and_split_against_postgres() {
    let Ok(url) = std::env::var("BIND_TEST_DATABASE_URL") else {
        eprintln!("skipped: BIND_TEST_DATABASE_URL not set");
        return;
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_stack_size(64 * 1024 * 1024)
        .build()
        .unwrap();
    let schema = format!("bind_e2e_{}", std::process::id());
    let admin = runtime.block_on(sqlx::PgPool::connect(&url)).unwrap();
    runtime.block_on(sqlx::query(sql(format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))).execute(&admin)).unwrap();
    runtime.block_on(sqlx::query(sql(format!("CREATE SCHEMA {schema}"))).execute(&admin)).unwrap();
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| scenarios(&runtime, &url, &schema)));
    // The schema is dropped even when a scenario fails.
    runtime.block_on(sqlx::query(sql(format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))).execute(&admin)).unwrap();
    let left: i64 = runtime
        .block_on(
            sqlx::query_scalar(sql(format!("SELECT count(*) FROM pg_namespace WHERE nspname = '{schema}'")))
                .fetch_one(&admin),
        )
        .unwrap();
    assert_eq!(left, 0);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    println!("end-to-end: ok");
}
"##;
