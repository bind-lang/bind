//! Endpoints of the generated server: request types, the RPC router and the per-call
//! transaction (commit on `Ok`, rollback on `Err`); in the `ssr` mode, the page at `GET /`
//! (`handle_page`, same transaction discipline) and the request router `serve`.

use std::collections::{BTreeMap, BTreeSet};

use crate::core::ir::{CoreFunction, CoreProgram, CoreType};
use crate::types::ty::Type;

use super::types::{DISTINCT_FIELD, Layout, RUNTIME, field_ident, field_line};

/// The request type of each endpoint: `<Name>Input`, unique among them and distinct from the
/// other items at the crate root (user types live in the program module).
pub fn input_names(endpoints: &[&CoreFunction]) -> BTreeMap<String, String> {
    let mut taken: BTreeSet<String> = BTreeSet::from(["HttpResult".to_string()]);
    let mut names = BTreeMap::new();
    for endpoint in endpoints {
        let mut name = format!("{}Input", crate::certified::pascal_case(&endpoint.name));
        while taken.contains(&name) {
            name.push('_');
        }
        taken.insert(name.clone());
        names.insert(endpoint.name.clone(), name);
    }
    names
}

/// `struct <Name>Input` for every endpoint: its parameters, unknown fields rejected.
pub fn inputs(
    endpoints: &[&CoreFunction],
    names: &BTreeMap<String, String>,
    layout: &Layout,
) -> String {
    let none = BTreeMap::new();
    let mut output = String::new();
    for endpoint in endpoints {
        output.push_str(&format!(
            "#[derive(::serde::Deserialize)]\n#[serde(deny_unknown_fields)]\nstruct {} {{\n",
            names[&endpoint.name]
        ));
        for (_, name, ty) in &endpoint.params {
            output.push_str(&field_line("", name, &layout.ty(ty, &none), true));
        }
        output.push_str("}\n\n");
    }
    output
}

/// The HTTP result type, the router and the helpers that run one call in a transaction.
pub fn router(endpoints: &[&CoreFunction], names: &BTreeMap<String, String>) -> String {
    let mut output = format!(
        "/// Response of one RPC call: HTTP status and JSON body.
#[derive(Debug)]
pub struct HttpResult {{
pub status: u16,
pub body: ::std::string::String,
}}

/// `{{\"error\": message}}` with the error's status; the message never exposes internals.
fn error_result(error: &{RUNTIME}::AppError) -> HttpResult {{
let body = ::serde_json::json!({{ \"error\": error.public_message() }}).to_string();
HttpResult {{ status: error.status(), body }}
}}

/// Ends the call's transaction: serializes the value, then commits; any error (including
/// the commit's) rolls back and becomes the response. A call never reports success
/// without a committed transaction.
async fn finish<T: ::serde::Serialize>(
ctx: {RUNTIME}::Ctx,
result: ::std::result::Result<T, {RUNTIME}::AppError>,
) -> HttpResult {{
let transaction = match ctx.take_transaction().await {{
::std::result::Result::Ok(transaction) => transaction,
::std::result::Result::Err(error) => return error_result(&error),
}};
::std::mem::drop(ctx);
let body = result.and_then(|value| ::serde_json::to_string(&value).map_err({RUNTIME}::AppError::from));
match body {{
::std::result::Result::Ok(body) => match transaction.commit().await {{
::std::result::Result::Ok(()) => HttpResult {{ status: 200, body }},
::std::result::Result::Err(error) => error_result(&{RUNTIME}::AppError::from(error)),
}},
::std::result::Result::Err(error) => {{
// The error is the response; a failed rollback leaves nothing committed either.
let _ = transaction.rollback().await;
error_result(&error)
}}
}}
}}

/// Decodes the request, opens the transaction and builds the call's context.
async fn begin<I: ::serde::de::DeserializeOwned>(
body: &[u8],
pool: &::sqlx::PgPool,
) -> ::std::result::Result<(I, {RUNTIME}::Ctx), HttpResult> {{
let input = ::serde_json::from_slice(body)
.map_err(|error| error_result(&{RUNTIME}::AppError::BadRequest(error.to_string())))?;
let transaction = pool
.begin()
.await
.map_err(|error| error_result(&{RUNTIME}::AppError::from(error)))?;
::std::result::Result::Ok((input, {RUNTIME}::Ctx::with_transaction(transaction)))
}}

/// Runs the RPC procedure at `path` with the JSON `body`, in one transaction.
pub async fn handle_rpc(path: &str, body: &[u8], pool: &::sqlx::PgPool) -> HttpResult {{
match path.trim_end_matches('/') {{
"
    );
    for endpoint in endpoints {
        let input = &names[&endpoint.name];
        let mut arguments = vec!["ctx.clone()".to_string()];
        for (_, name, _) in &endpoint.params {
            arguments.push(format!("input.{}", field_ident(name)));
        }
        output.push_str(&format!(
            "{:?} => {{\nlet (input, ctx) = match begin::<{input}>(body, pool).await {{\n::std::result::Result::Ok(started) => started,\n::std::result::Result::Err(response) => return response,\n}};\nlet result = {}::bind_{}({}).await;\nfinish(ctx, result).await\n}}\n",
            format!("/api/{}", endpoint.name),
            super::PROGRAM_MODULE,
            endpoint.name,
            arguments.join(", ")
        ));
    }
    output.push_str(&format!(
        "_ => error_result(&{RUNTIME}::AppError::NotFound(::std::string::String::from(\"RPC procedure not found\"))),
}}
}}

/// `handle_rpc` on a runtime worker thread, whose stack is sized for deep Bind recursion
/// (the thread that drives the HTTP runtime may have a smaller one).
pub async fn serve_rpc(
path: ::std::string::String,
body: ::std::vec::Vec<u8>,
pool: ::sqlx::PgPool,
) -> HttpResult {{
match ::tokio::spawn(async move {{ handle_rpc(&path, &body, &pool).await }}).await {{
::std::result::Result::Ok(result) => result,
::std::result::Result::Err(_) => error_result(&{RUNTIME}::AppError::Internal(::std::string::String::from(\"the request task failed\"))),
}}
}}

"
    ));
    output
}

/// The Rust type a page parameter of type `ty` is decoded as (`FromParam` in the runtime) and
/// the distinct types wrapping it, outermost first; `None` when a URL cannot carry it.
fn param_type(ty: &Type, program: &CoreProgram) -> Option<(Type, Vec<String>)> {
    let Type::Con(name, args) = ty else {
        return None;
    };
    if !args.is_empty() {
        return None;
    }
    let distinct = program
        .types
        .iter()
        .find_map(|declaration| match declaration {
            CoreType::Distinct {
                name: declared,
                underlying,
            } if declared == name => Some(underlying),
            _ => None,
        });
    if let Some(underlying) = distinct {
        let (base, mut wrappers) = param_type(underlying, program)?;
        wrappers.insert(0, name.clone());
        return Some((base, wrappers));
    }
    let user = program.types.iter().any(|declaration| match declaration {
        CoreType::Record { name: declared, .. } | CoreType::Sum { name: declared, .. } => {
            declared == name
        }
        CoreType::Distinct { .. } => false,
    });
    let allowed = [
        "String", "Email", "Url", "Int", "UUID", "Date", "DateTime", "Instant",
    ];
    (!user && allowed.contains(&name.as_str())).then(|| (ty.clone(), Vec::new()))
}

/// The page of the `ssr` mode: content types, the shell's title and stylesheet, `handle_page`
/// (`GET /`: the component rendered in one transaction), `serve_page` and the request router
/// `serve`. Without server calls the component runs without a transaction.
pub fn page(
    component: &CoreFunction,
    ssr: &super::SsrPage,
    uses_server: bool,
    program: &CoreProgram,
    layout: &Layout,
) -> Result<String, String> {
    let none = BTreeMap::new();
    let mut decode = String::new();
    let mut arguments = Vec::new();
    for (index, (_, name, ty)) in component.params.iter().enumerate() {
        let Some((base, wrappers)) = param_type(ty, program) else {
            return Err(format!(
                "Component `{}` has parameter `{name}` of type `{}`, which a page URL cannot carry in profile {}.",
                component.name,
                crate::types::ty::display(ty),
                crate::certified::profile_version()
            ));
        };
        let mut value = "value".to_string();
        for wrapper in wrappers.iter().rev() {
            value = format!(
                "{} {{ {DISTINCT_FIELD}: {value} }}",
                layout.type_path(wrapper)
            );
        }
        let argument = format!("a{index}");
        decode.push_str(&format!(
            "let {argument} = match {RUNTIME}::param::<{}>(&params, {name:?}) {{
::std::result::Result::Ok(value) => {value},
::std::result::Result::Err(error) => return page_error(&error),
}};
",
            layout.ty(&base, &none)
        ));
        arguments.push(argument);
    }
    if !decode.is_empty() {
        decode.insert_str(
            0,
            &format!(
                "let params = {RUNTIME}::QueryParams::parse(query);
"
            ),
        );
    }
    let call = |ctx: &str| {
        let mut values = vec![ctx.to_string()];
        values.extend(arguments.iter().cloned());
        format!(
            "{}::bind_{}({}).await",
            super::PROGRAM_MODULE,
            component.name,
            values.join(", ")
        )
    };
    let run = if uses_server {
        format!(
            "let transaction = match pool.begin().await {{
::std::result::Result::Ok(transaction) => transaction,
::std::result::Result::Err(error) => return page_error(&{RUNTIME}::AppError::from(error)),
}};
let ctx = {RUNTIME}::Ctx::with_transaction(transaction);
let result = {};
let transaction = match ctx.take_transaction().await {{
::std::result::Result::Ok(transaction) => transaction,
::std::result::Result::Err(error) => return page_error(&error),
}};
::std::mem::drop(ctx);
match result {{
::std::result::Result::Ok(node) => match transaction.commit().await {{
::std::result::Result::Ok(()) => page_result(&node),
::std::result::Result::Err(error) => page_error(&{RUNTIME}::AppError::from(error)),
}},
::std::result::Result::Err(error) => {{
// The error is the page; a failed rollback leaves nothing committed either.
let _ = transaction.rollback().await;
page_error(&error)
}}
}}",
            call("ctx.clone()")
        )
    } else {
        format!(
            "// The component makes no server call: no transaction.
match {} {{
::std::result::Result::Ok(node) => page_result(&node),
::std::result::Result::Err(error) => page_error(&error),
}}",
            call(&format!("{RUNTIME}::Ctx::pure()"))
        )
    };
    Ok(format!(
        "/// Content type of the page responses (`handle_page`).
pub const PAGE_CONTENT_TYPE: &str = \"text/html; charset=utf-8\";

/// Content type of the RPC responses (`handle_rpc`).
pub const RPC_CONTENT_TYPE: &str = \"application/json; charset=utf-8\";

/// Title of the page shell.
const PAGE_TITLE: &str = {title:?};

/// Stylesheet URL of the page shell.
const PAGE_STYLESHEET: &str = {stylesheet:?};

/// The error page of `error`, with its status; the message never exposes internals.
fn page_error(error: &{RUNTIME}::AppError) -> HttpResult {{
let (status, body) = {RUNTIME}::error_page(error, PAGE_TITLE, PAGE_STYLESHEET);
HttpResult {{ status, body }}
}}

/// The page of a rendered component.
fn page_result(node: &{RUNTIME}::Node) -> HttpResult {{
let body = {RUNTIME}::page_html(PAGE_TITLE, PAGE_STYLESHEET, &{RUNTIME}::render_html(node));
HttpResult {{ status: 200, body }}
}}

/// `GET /`: the page of component `{name}` (HTML, `PAGE_CONTENT_TYPE`). Its parameters come
/// from the query string `query` (absent, repeated or invalid: a 400 page naming the
/// parameter); server calls run in one transaction, committed when the page renders and rolled
/// back on an error, whose page has the error's status.
pub async fn handle_page(query: &str, pool: &::sqlx::PgPool) -> HttpResult {{
{decode}{run}
}}

/// `handle_page` on a runtime worker thread (see `serve_rpc`).
pub async fn serve_page(query: ::std::string::String, pool: ::sqlx::PgPool) -> HttpResult {{
match ::tokio::spawn(async move {{ handle_page(&query, &pool).await }}).await {{
::std::result::Result::Ok(result) => result,
::std::result::Result::Err(_) => page_error(&{RUNTIME}::AppError::Internal(::std::string::String::from(\"the request task failed\"))),
}}
}}

/// An HTTP response: status, content type and body.
#[derive(Debug)]
pub struct HttpResponse {{
pub status: u16,
pub content_type: &'static str,
pub body: ::std::string::String,
}}

/// Routes one request: `GET /` is the page (`query` is its query string, with or without the
/// leading `?`), every other request an RPC call (`serve_rpc`).
pub async fn serve(
method: &str,
path: ::std::string::String,
query: ::std::string::String,
body: ::std::vec::Vec<u8>,
pool: ::sqlx::PgPool,
) -> HttpResponse {{
if method == \"GET\" && (path.is_empty() || path == \"/\") {{
let result = serve_page(query, pool).await;
HttpResponse {{ status: result.status, content_type: PAGE_CONTENT_TYPE, body: result.body }}
}} else {{
let result = serve_rpc(path, body, pool).await;
HttpResponse {{ status: result.status, content_type: RPC_CONTENT_TYPE, body: result.body }}
}}
}}

",
        title = ssr.title,
        stylesheet = ssr.stylesheet,
        name = component.name,
    ))
}

/// The page tests of the generated crate: a missing parameter is a 400 page naming it.
pub fn page_tests(component: &CoreFunction) -> String {
    let Some((_, name, _)) = component.params.first() else {
        return String::new();
    };
    format!(
        "
#[test]
fn pages_with_a_missing_parameter_are_400() -> ::std::result::Result<(), ::std::boxed::Box<dyn ::std::error::Error>> {{
let runtime = ::tokio::runtime::Builder::new_current_thread().enable_all().build()?;
runtime.block_on(async {{
let pool = ::sqlx::postgres::PgPoolOptions::new()
.connect_lazy(\"postgresql://bind-test.invalid/none\")?;
let page = super::handle_page(\"\", &pool).await;
assert_eq!(page.status, 400);
assert!(page.body.contains(&crate::bind_runtime::bad_parameter_message({name:?})));
::std::result::Result::Ok(())
}})
}}
"
    )
}

/// Tests compiled into the generated crate: request decoding and routing (no database), then
/// `extra` (the page tests in the `ssr` mode).
pub fn tests(endpoints: &[&CoreFunction], names: &BTreeMap<String, String>, extra: &str) -> String {
    let mut checks = String::new();
    for endpoint in endpoints {
        checks.push_str(&format!(
            "assert!(::serde_json::from_str::<super::{}>({:?}).is_err());\n",
            names[&endpoint.name], "{\"bindUnknownField\":0}"
        ));
    }
    // Without endpoints every path is an unknown procedure; there is no request to malform.
    let malformed = endpoints
        .first()
        .map(|endpoint| {
            format!(
                "let malformed = super::handle_rpc({:?}, b\"not json\", &pool).await;\nassert_eq!(malformed.status, 400);\n",
                format!("/api/{}", endpoint.name)
            )
        })
        .unwrap_or_default();
    format!(
        "#[cfg(test)]
mod tests {{
#[test]
fn requests_with_unknown_fields_are_rejected() {{
{checks}}}

#[test]
fn unknown_procedures_and_malformed_requests_never_reach_the_database()
-> ::std::result::Result<(), ::std::boxed::Box<dyn ::std::error::Error>> {{
let runtime = ::tokio::runtime::Builder::new_current_thread().enable_all().build()?;
runtime.block_on(async {{
let pool = ::sqlx::postgres::PgPoolOptions::new()
.connect_lazy(\"postgresql://bind-test.invalid/none\")?;
let missing = super::handle_rpc(\"/api/bindMissingProcedure\", b\"{{}}\", &pool).await;
assert_eq!(missing.status, 404);
{malformed}::std::result::Result::Ok(())
}})
}}
{extra}}}
"
    )
}

/// CORS of the API when the target declares `frontend_origin` (the frontend is on another
/// origin): `FRONTEND_ORIGIN`, the `Cors` decision type and `cors`, which the entry points call
/// before routing an RPC request, and their tests.
pub fn cors(origin: &str) -> String {
    format!(
        "/// The only browser origin allowed to call the API (`frontend_origin`, CORS).
pub const FRONTEND_ORIGIN: &str = {origin:?};

/// The CORS decision for one RPC request: headers to add to its response, and the response that
/// ends the request here (a preflight, or a refused origin), if any.
#[derive(Debug)]
pub struct Cors {{
pub headers: ::std::vec::Vec<(&'static str, ::std::string::String)>,
pub response: ::std::option::Option<HttpResult>,
}}

/// CORS for a request with `method` and the `Origin` header `origin`. Without an `Origin` the
/// request is handled as usual (no browser is involved). From `FRONTEND_ORIGIN` it is handled
/// with the CORS headers, and a preflight (`OPTIONS`) is answered here (204). From any other
/// origin it is refused (403, no CORS headers: the browser exposes nothing to that page).
pub fn cors(method: &str, origin: ::std::option::Option<&str>) -> Cors {{
match origin {{
::std::option::Option::None => Cors {{ headers: ::std::vec::Vec::new(), response: ::std::option::Option::None }},
::std::option::Option::Some(origin) if origin == FRONTEND_ORIGIN => {{
let mut headers = ::std::vec![
(\"access-control-allow-origin\", ::std::string::String::from(FRONTEND_ORIGIN)),
(\"vary\", ::std::string::String::from(\"Origin\")),
];
if method == \"OPTIONS\" {{
headers.push((\"access-control-allow-methods\", ::std::string::String::from(\"POST, OPTIONS\")));
headers.push((\"access-control-allow-headers\", ::std::string::String::from(\"content-type, accept\")));
headers.push((\"access-control-max-age\", ::std::string::String::from(\"600\")));
let response = HttpResult {{ status: 204, body: ::std::string::String::new() }};
Cors {{ headers, response: ::std::option::Option::Some(response) }}
}} else {{
Cors {{ headers, response: ::std::option::Option::None }}
}}
}}
::std::option::Option::Some(_) => {{
let body = ::serde_json::json!({{ \"error\": \"origin not allowed\" }}).to_string();
Cors {{
headers: ::std::vec![(\"vary\", ::std::string::String::from(\"Origin\"))],
response: ::std::option::Option::Some(HttpResult {{ status: 403, body }}),
}}
}}
}}
}}

#[cfg(test)]
mod cors_tests {{
#[test]
fn only_the_frontend_origin_may_call_the_api() {{
let preflight = super::cors(\"OPTIONS\", ::std::option::Option::Some(super::FRONTEND_ORIGIN));
assert_eq!(preflight.response.as_ref().map(|response| response.status), ::std::option::Option::Some(204));
assert!(preflight.headers.iter().any(|(name, value)| *name == \"access-control-allow-origin\" && value == super::FRONTEND_ORIGIN));
assert!(preflight.headers.iter().any(|(name, _)| *name == \"access-control-allow-methods\"));
let call = super::cors(\"POST\", ::std::option::Option::Some(super::FRONTEND_ORIGIN));
assert!(call.response.is_none());
assert!(call.headers.iter().any(|(name, value)| *name == \"access-control-allow-origin\" && value == super::FRONTEND_ORIGIN));
for method in [\"OPTIONS\", \"POST\"] {{
let refused = super::cors(method, ::std::option::Option::Some(\"https://other.example\"));
assert_eq!(refused.response.as_ref().map(|response| response.status), ::std::option::Option::Some(403));
assert!(!refused.headers.iter().any(|(name, _)| name.starts_with(\"access-control-\")));
}}
let direct = super::cors(\"POST\", ::std::option::Option::None);
assert!(direct.response.is_none() && direct.headers.is_empty());
}}
}}
"
    )
}
