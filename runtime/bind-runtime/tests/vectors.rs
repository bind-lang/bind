//! Cross-runtime vectors in `runtime/vectors/` (see its README): the same files are checked by
//! `bindc` (`core::display`) and by the JS runtime.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bind_runtime::{
    AppError, FromParam, Int, Node, NodeArg, NodeTag, PageMeta, QueryParams, bad_parameter,
    display_text, error_body, js_number_to_string, page_html, param, render_html,
};
use serde_json::Value;

fn vectors(name: &str) -> Vec<Value> {
    let path = format!("{}/../vectors/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap();
    match serde_json::from_str(&text).unwrap() {
        Value::Array(items) => items,
        other => panic!("{path}: expected an array, got {other}"),
    }
}

fn node(value: &Value) -> Node {
    let name = value["node"].as_str().unwrap();
    let args = value["args"].as_array().unwrap();
    let children: Vec<Node> = value["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(node)
        .collect();
    match name {
        "Empty" => Node::Empty,
        "Text" => Node::Text(args[0].as_str().unwrap().to_string()),
        _ => Node::Element {
            tag: NodeTag::from_name(name).unwrap(),
            args: args
                .iter()
                .map(|arg| match arg {
                    Value::String(text) => NodeArg::Str(text.clone()),
                    other => {
                        NodeArg::Int(serde_json::from_value::<Int>(other["int"].clone()).unwrap())
                    }
                })
                .collect(),
            children: std::sync::Arc::new(children),
        },
    }
}

#[test]
fn html_vectors() {
    let cases = vectors("html-trees.json");
    assert!(cases.len() >= 20);
    for case in &cases {
        assert_eq!(
            render_html(&node(&case["tree"])),
            case["html"].as_str().unwrap(),
            "{}",
            case["tree"]
        );
    }
}

#[test]
fn number_vectors() {
    let cases = vectors("numbers.json");
    assert!(cases.len() >= 20);
    for case in &cases {
        let bits = u64::from_str_radix(case["bits"].as_str().unwrap(), 16).unwrap();
        assert_eq!(
            js_number_to_string(f64::from_bits(bits)),
            case["text"].as_str().unwrap(),
            "{}",
            case["bits"]
        );
    }
}

#[test]
fn page_vectors() {
    let cases = vectors("pages.json");
    assert!(cases.len() >= 6);
    for case in &cases {
        let text = |key: &str| case[key].as_str().unwrap();
        let expected = text("html");
        match text("kind") {
            "page" => {
                let meta = PageMeta {
                    title: text("title"),
                    description: case["description"].as_str(),
                    language: text("language"),
                    stylesheet: text("stylesheet"),
                };
                assert!(case["description"].is_string() || case["description"].is_null());
                assert_eq!(page_html(&meta, text("body")), expected);
            }
            "error_body" => {
                let message = text("message").to_string();
                let error = match text("error") {
                    "BadRequest" => AppError::BadRequest(message),
                    "NotFound" => AppError::NotFound(message),
                    "Internal" => AppError::Internal(message),
                    "Overflow" => AppError::Overflow(message),
                    "RecursionLimit" => AppError::RecursionLimit,
                    // The message of these kinds is never shown: any inner error will do.
                    "Database" => AppError::Database(sqlx::Error::RowNotFound),
                    "Serialization" => {
                        AppError::Serialization(serde_json::from_str::<i32>(&message).unwrap_err())
                    }
                    other => panic!("unknown error {other}"),
                };
                let (status, html) = error_body(&error);
                assert_eq!(status, case["status"].as_u64().unwrap() as u16);
                assert_eq!(html, expected);
            }
            "bad_parameter" => {
                let (status, html) = error_body(&bad_parameter(text("name")));
                assert_eq!(status, case["status"].as_u64().unwrap() as u16);
                assert_eq!(html, expected);
            }
            other => panic!("unknown kind {other}"),
        }
    }
}

/// The canonical text of a decoded parameter (its wire form): what `Text` shows for it.
fn canonical<T: FromParam + serde::Serialize>(text: &str) -> Option<String> {
    T::from_param(text).map(|value| display_text(&value).unwrap())
}

/// Decodes `text` as a parameter of the Bind type `ty`; `Some(canonical text)` when valid.
fn decode(ty: &str, text: &str) -> Option<String> {
    match ty {
        "String" => String::from_param(text),
        "Int" => Int::from_param(text).map(|value| value.to_string()),
        "UUID" => canonical::<uuid::Uuid>(text),
        "Date" => canonical::<chrono::NaiveDate>(text),
        "DateTime" => canonical::<chrono::NaiveDateTime>(text),
        "Instant" => canonical::<chrono::DateTime<chrono::Utc>>(text),
        other => panic!("unknown parameter type {other}"),
    }
}

/// The value of parameter `name` of type `ty` in `params`, as its canonical text.
fn read(params: &QueryParams, name: &str, ty: &str) -> Result<String, AppError> {
    match ty {
        "String" => param::<String>(params, name),
        "Int" => param::<Int>(params, name).map(|value| value.to_string()),
        "UUID" => param::<uuid::Uuid>(params, name).map(|value| value.to_string()),
        "Instant" => param::<chrono::DateTime<chrono::Utc>>(params, name)
            .map(|value| display_text(&value).unwrap()),
        other => panic!("unknown parameter type {other}"),
    }
}

/// Review Focus 1: the strict URL parameter decoding shared with the JS runtime's `readParams`.
#[test]
fn param_vectors() {
    let cases = vectors("params.json");
    assert!(cases.len() >= 40);
    for case in &cases {
        let text = |key: &str| case[key].as_str().unwrap();
        match text("kind") {
            "text" => {
                let decoded = decode(text("type"), text("text"));
                let valid = case["valid"].as_bool().unwrap();
                assert_eq!(decoded.is_some(), valid, "{case}");
                if let Some(canonical) = decoded {
                    // A valid text is canonical: the value shows (and serializes) as the text.
                    assert_eq!(canonical, text("text"), "{case}");
                }
            }
            "query" => {
                let params = QueryParams::parse(text("query"));
                let specs = case["params"].as_array().unwrap();
                let mut result = Ok(Vec::new());
                for spec in specs {
                    let (name, ty) = (spec[0].as_str().unwrap(), spec[1].as_str().unwrap());
                    match read(&params, name, ty) {
                        Ok(value) => {
                            if let Ok(values) = &mut result {
                                values.push((name.to_string(), value));
                            }
                        }
                        Err(error) => {
                            result = Err(error);
                            break;
                        }
                    }
                }
                match (result, case.get("error")) {
                    (Ok(values), None) => {
                        let expected = case["values"].as_object().unwrap();
                        assert_eq!(values.len(), expected.len(), "{case}");
                        for (name, value) in values {
                            assert_eq!(expected[&name].as_str().unwrap(), value, "{case}");
                        }
                    }
                    (Err(error), Some(name)) => {
                        let expected = bad_parameter(name.as_str().unwrap());
                        assert_eq!(error.public_message(), expected.public_message(), "{case}");
                        assert_eq!(error.status(), 400, "{case}");
                    }
                    (result, _) => panic!("{case}: got {result:?}"),
                }
            }
            other => panic!("unknown kind {other}"),
        }
    }
}

/// The order of `a` and `b` read as chrono values of type `T` (the order of the generated
/// Rust); each text must be canonical (chrono writes it back unchanged).
fn chrono_order<T>(a: &str, b: &str) -> std::cmp::Ordering
where
    T: serde::de::DeserializeOwned + serde::Serialize + Ord,
{
    let read = |text: &str| -> T {
        let value: T = serde_json::from_value(Value::String(text.to_string()))
            .unwrap_or_else(|error| panic!("{text}: {error}"));
        assert_eq!(display_text(&value).unwrap(), text, "not canonical");
        value
    };
    read(a).cmp(&read(b))
}

#[test]
fn temporal_order_vectors() {
    let cases = vectors("temporal-order.json");
    assert!(cases.len() >= 30);
    for case in &cases {
        let (a, b) = (case["a"].as_str().unwrap(), case["b"].as_str().unwrap());
        let order = match case["type"].as_str().unwrap() {
            "Date" => chrono_order::<chrono::NaiveDate>(a, b),
            "DateTime" => chrono_order::<chrono::NaiveDateTime>(a, b),
            "Instant" => chrono_order::<chrono::DateTime<chrono::Utc>>(a, b),
            other => panic!("unknown type {other}"),
        };
        let expected = match order {
            std::cmp::Ordering::Less => "<",
            std::cmp::Ordering::Equal => "=",
            std::cmp::Ordering::Greater => ">",
        };
        assert_eq!(case["order"].as_str().unwrap(), expected, "{case}");
    }
}
