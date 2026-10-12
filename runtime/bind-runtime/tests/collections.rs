//! `Set`/`Map` vectors in `runtime/vectors/` (see its README): `collections.json` (operations and
//! their canonical results, Ruling A) and `wire-rich.json` (JSON round trips and malformed inputs
//! of the rich wire types). The JS runtime checks the same files.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bind_runtime::{
    BindMap, BindResult, BindSet, Int, empty_map, empty_set, js_json, map_keys, map_lookup,
    map_put, map_remove_key, map_size, map_values, set_contains, set_insert, set_of, set_remove,
    set_size, set_to_list, wire_decode_request, wire_response,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

fn vectors(name: &str) -> Vec<Value> {
    let path = format!("{}/../vectors/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap();
    match serde_json::from_str(&text).unwrap() {
        Value::Array(items) => items,
        other => panic!("{path}: expected an array, got {other}"),
    }
}

// The fixture types of the README, declared as the Rust backend declares them.

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
enum Cor {
    #[serde(rename = "z")]
    Z,
    #[serde(rename = "a")]
    A,
    #[serde(rename = "m")]
    M,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
struct Nota {
    value: rust_decimal::Decimal,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
struct Codigo {
    value: uuid::Uuid,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Ponto {
    x: Int,
    y: Int,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Pagina<T> {
    itens: Vec<T>,
    total: Int,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
enum Forma {
    #[serde(rename = "circulo")]
    Circulo { raio: Int },
    #[serde(rename = "retangulo")]
    Retangulo { largura: Int, altura: Int },
    #[serde(rename = "vazia")]
    Vazia,
}

fn value<T: DeserializeOwned>(json: &Value) -> T {
    serde_json::from_value(json.clone()).unwrap_or_else(|error| panic!("{json}: {error}"))
}

fn json<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap()
}

fn run_set<T: Ord + Clone + Serialize + DeserializeOwned>(case: &Value) {
    let items: Vec<T> = case["setOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(value)
        .collect();
    let mut set: BindSet<T> = if items.is_empty() {
        empty_set()
    } else {
        set_of(items)
    };
    for step in case["steps"].as_array().unwrap() {
        let item = value::<T>(&step[1]);
        set = match step[0].as_str().unwrap() {
            "insert" => set_insert(set, item),
            "remove" => set_remove(set, item),
            other => panic!("unknown step {other}"),
        };
    }
    let list = &case["toList"];
    assert_eq!(&json(&*set_to_list(&set)), list, "{case}");
    // The wire form is the canonical list too.
    assert_eq!(&json(&set), list, "{case}");
    assert_eq!(
        set_size(&set),
        Int::from_i64(list.as_array().unwrap().len() as i64),
        "{case}"
    );
    for check in case["contains"].as_array().unwrap() {
        let item = value::<T>(&check[0]);
        assert_eq!(
            Value::Bool(set_contains(&set, &item)),
            check[1],
            "{case}: contains {}",
            check[0]
        );
    }
}

fn run_map<K: Ord + Clone + Serialize + DeserializeOwned>(case: &Value) {
    let mut map: BindMap<K, String> = empty_map();
    for step in case["steps"].as_array().unwrap() {
        let key = value::<K>(&step[1]);
        map = match step[0].as_str().unwrap() {
            "put" => map_put(map, key, value(&step[2])),
            "removeKey" => map_remove_key(map, key),
            other => panic!("unknown step {other}"),
        };
    }
    let entries = case["entries"].as_array().unwrap();
    assert_eq!(&json(&map), &case["entries"], "{case}");
    let keys: Vec<Value> = entries.iter().map(|pair| pair[0].clone()).collect();
    let values: Vec<Value> = entries.iter().map(|pair| pair[1].clone()).collect();
    assert_eq!(json(&*map_keys(&map)), Value::Array(keys), "{case}");
    assert_eq!(json(&*map_values(&map)), Value::Array(values), "{case}");
    assert_eq!(
        map_size(&map),
        Int::from_i64(entries.len() as i64),
        "{case}"
    );
    for check in case["lookup"].as_array().unwrap() {
        let key = value::<K>(&check[0]);
        assert_eq!(
            json(&map_lookup(&map, &key)),
            check[1],
            "{case}: lookup {}",
            check[0]
        );
    }
}

#[test]
fn collection_vectors() {
    let cases = vectors("collections.json");
    assert!(cases.len() >= 20);
    for case in &cases {
        match case["kind"].as_str().unwrap() {
            "set" => match case["element"].as_str().unwrap() {
                "Int" => run_set::<Int>(case),
                "Decimal" => run_set::<rust_decimal::Decimal>(case),
                "Nota" => run_set::<Nota>(case),
                "String" => run_set::<String>(case),
                "Boolean" => run_set::<bool>(case),
                "Cor" => run_set::<Cor>(case),
                "Date" => run_set::<chrono::NaiveDate>(case),
                "DateTime" => run_set::<chrono::NaiveDateTime>(case),
                "Instant" => run_set::<chrono::DateTime<chrono::Utc>>(case),
                "UUID" => run_set::<uuid::Uuid>(case),
                other => panic!("unknown element type {other}"),
            },
            "map" => match case["key"].as_str().unwrap() {
                "Int" => run_map::<Int>(case),
                "Decimal" => run_map::<rust_decimal::Decimal>(case),
                "String" => run_map::<String>(case),
                "Boolean" => run_map::<bool>(case),
                "Cor" => run_map::<Cor>(case),
                "Instant" => run_map::<chrono::DateTime<chrono::Utc>>(case),
                other => panic!("unknown key type {other}"),
            },
            other => panic!("unknown kind {other}"),
        }
    }
}

/// One wire case at the Rust type `T`: the server's request decoder, the response encoder and
/// the SSR round trip (`js_json`, then `wire_response`) all agree with the vector.
fn run_wire<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(case: &Value) {
    let input = case["json"].to_string();
    let body = format!("{{\"x\":{input}}}");
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request<T> {
        x: T,
    }
    let decoded = wire_decode_request::<Request<T>>(&body).map(|request| request.x);
    if case["valid"] == Value::Bool(false) {
        assert!(decoded.is_err(), "{case}: accepted as {decoded:?}");
        assert!(
            serde_json::from_value::<T>(case["json"].clone()).is_err(),
            "{case}"
        );
        return;
    }
    let decoded = decoded.unwrap_or_else(|error| panic!("{case}: {error}"));
    let canonical = &case["canonical"];
    assert_eq!(&json(&decoded), canonical, "{case}");
    let written: Value = serde_json::from_str(&js_json(&decoded).unwrap()).unwrap();
    assert_eq!(&written, canonical, "{case}");
    let back = wire_response(decoded).unwrap();
    assert_eq!(&json(&back), canonical, "{case}");
    // The canonical form decodes to the same value.
    assert_eq!(value::<T>(canonical), back, "{case}");
}

#[test]
fn rich_wire_vectors() {
    let cases = vectors("wire-rich.json");
    assert!(cases.len() >= 40);
    for case in &cases {
        match case["type"].as_str().unwrap() {
            "Set<Int>" => run_wire::<BindSet<Int>>(case),
            "Set<Decimal>" => run_wire::<BindSet<rust_decimal::Decimal>>(case),
            "Set<String>" => run_wire::<BindSet<String>>(case),
            "Set<Cor>" => run_wire::<BindSet<Cor>>(case),
            "Set<Instant>" => run_wire::<BindSet<chrono::DateTime<chrono::Utc>>>(case),
            "Map<String, Int>" => run_wire::<BindMap<String, Int>>(case),
            "Map<Decimal, String>" => run_wire::<BindMap<rust_decimal::Decimal, String>>(case),
            "Map<Cor, Int>" => run_wire::<BindMap<Cor, Int>>(case),
            "Map<Int, Set<Int>>" => run_wire::<BindMap<Int, BindSet<Int>>>(case),
            "Result<Int, String>" => run_wire::<BindResult<Int, String>>(case),
            "Forma" => run_wire::<Forma>(case),
            "Pagina<Int>" => run_wire::<Pagina<Int>>(case),
            "Pagina<Ponto>" => run_wire::<Pagina<Ponto>>(case),
            "Map<Codigo, Pagina<Ponto>>" => run_wire::<BindMap<Codigo, Pagina<Ponto>>>(case),
            other => panic!("unknown type {other}"),
        }
    }
}

/// Review Focus 1: `Decimal` keys of the same value and different scales are one key, and a
/// lookup at any scale finds the entry written first.
#[test]
fn decimal_keys_are_compared_by_value() {
    let decimal = |text: &str| text.parse::<rust_decimal::Decimal>().unwrap();
    let map: BindMap<rust_decimal::Decimal, Int> =
        serde_json::from_str(r#"[["1.0","1"],["2","2"]]"#).unwrap();
    assert_eq!(map_lookup(&map, &decimal("1.00")), Some(Int::from_i64(1)));
    assert!(
        serde_json::from_str::<BindMap<rust_decimal::Decimal, Int>>(
            r#"[["1.0","1"],["1.00","2"]]"#
        )
        .is_err()
    );
    assert!(serde_json::from_str::<BindSet<rust_decimal::Decimal>>(r#"["1.0","1.00"]"#).is_err());
    let set = set_insert(set_of(vec![decimal("1.0")]), decimal("1.00"));
    assert_eq!(json(&set), serde_json::json!(["1.0"]));
    let map = map_put(map, decimal("1.000"), Int::from_i64(9));
    assert_eq!(json(&map), serde_json::json!([["1.0", "9"], ["2", "2"]]));
}

/// Unshared collections are updated in place; shared ones are copied (immutability).
#[test]
fn updates_never_change_a_shared_collection() {
    let one = set_of(vec![Int::from_i64(1)]);
    let two = set_insert(one.clone(), Int::from_i64(2));
    assert_eq!(json(&one), serde_json::json!(["1"]));
    assert_eq!(json(&two), serde_json::json!(["1", "2"]));
    let empty = set_remove(two.clone(), Int::from_i64(1));
    assert_eq!(json(&two), serde_json::json!(["1", "2"]));
    assert_eq!(json(&empty), serde_json::json!(["2"]));
    let map = map_put(empty_map(), "a".to_string(), Int::from_i64(1));
    let other = map_put(map.clone(), "a".to_string(), Int::from_i64(2));
    assert_eq!(map_lookup(&map, &"a".to_string()), Some(Int::from_i64(1)));
    assert_eq!(map_lookup(&other, &"a".to_string()), Some(Int::from_i64(2)));
    let removed = map_remove_key(other.clone(), "a".to_string());
    assert_eq!(map_size(&removed), Int::from_i64(0));
    assert_eq!(map_size(&other), Int::from_i64(1));
}
