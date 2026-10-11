//! Endpoints of the generated server: request types, the RPC router and the per-call
//! transaction (commit on `Ok`, rollback on `Err`).

use std::collections::{BTreeMap, BTreeSet};

use crate::core::ir::CoreFunction;

use super::types::{Layout, RUNTIME, field_ident, field_line};

/// The request type of each endpoint: `<Name>Input`, made unique against user types.
pub fn input_names(endpoints: &[&CoreFunction], layout: &Layout) -> BTreeMap<String, String> {
    let mut taken: BTreeSet<String> = layout
        .program
        .types
        .iter()
        .map(|ty| match ty {
            crate::core::ir::CoreType::Record { name, .. }
            | crate::core::ir::CoreType::Sum { name, .. }
            | crate::core::ir::CoreType::Distinct { name, .. } => name.clone(),
        })
        .collect();
    taken.extend(["HttpResult".to_string()]);
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
            "{:?} => {{\nlet (input, ctx) = match begin::<{input}>(body, pool).await {{\n::std::result::Result::Ok(started) => started,\n::std::result::Result::Err(response) => return response,\n}};\nlet result = bind_{}({}).await;\nfinish(ctx, result).await\n}}\n",
            format!("/api/{}", endpoint.name),
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

/// Tests compiled into the generated crate: request decoding and routing (no database).
pub fn tests(endpoints: &[&CoreFunction], names: &BTreeMap<String, String>) -> String {
    let mut checks = String::new();
    for endpoint in endpoints {
        checks.push_str(&format!(
            "assert!(::serde_json::from_str::<super::{}>({:?}).is_err());\n",
            names[&endpoint.name], "{\"bindUnknownField\":0}"
        ));
    }
    let first = endpoints
        .first()
        .map(|endpoint| format!("/api/{}", endpoint.name))
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
let malformed = super::handle_rpc({first:?}, b\"not json\", &pool).await;
assert_eq!(malformed.status, 400);
::std::result::Result::Ok(())
}})
}}
}}
"
    )
}
