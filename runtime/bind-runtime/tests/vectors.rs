//! Cross-runtime vectors in `runtime/vectors/` (see its README): the same files are checked by
//! `bindc` (`core::display`) and by the JS runtime.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bind_runtime::{
    AppError, Int, Node, NodeArg, NodeTag, error_body, js_number_to_string, page_html, render_html,
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
            children,
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
            "page" => assert_eq!(
                page_html(text("title"), text("stylesheet"), text("body")),
                expected
            ),
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
                    "Serialization" => AppError::Serialization(
                        serde_json::from_str::<i32>(&message).unwrap_err(),
                    ),
                    other => panic!("unknown error {other}"),
                };
                let (status, html) = error_body(&error);
                assert_eq!(status, case["status"].as_u64().unwrap() as u16);
                assert_eq!(html, expected);
            }
            other => panic!("unknown kind {other}"),
        }
    }
}
