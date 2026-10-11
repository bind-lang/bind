//! Generates the `Decimal` and `f32` vectors of the JS runtime
//! (`runtime/bind-runtime-js/tests/decimal-vectors.json` and `f32-vectors.json`) from
//! `rust_decimal` and Rust's `f32`, and fails when the committed files differ from the generated
//! content. Regenerate with `BIND_BLESS=1 cargo test --test js_vectors` and commit the result.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::str::FromStr;

use bind_runtime::{decimal_add, decimal_neg, decimal_sub, js_number_to_string};
use rust_decimal::Decimal;
use serde_json::{Value, json};

/// Deterministic xorshift64* generator, so the vectors are reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const MAX_MANTISSA: u128 = (1u128 << 96) - 1;

fn decimal(negative: bool, mantissa: u128, scale: u32) -> Decimal {
    let mut value = Decimal::from_parts(
        mantissa as u32,
        (mantissa >> 32) as u32,
        (mantissa >> 64) as u32,
        false,
        scale,
    );
    // `from_parts` drops the sign of a zero; `set_sign_negative` keeps it (`-0.00`).
    value.set_sign_negative(negative);
    value
}

fn edge_decimals() -> Vec<Decimal> {
    let p10 = |n: u32| 10u128.pow(n);
    let cases: &[(bool, u128, u32)] = &[
        (false, 0, 0),
        (true, 0, 0),
        (false, 0, 2),
        (true, 0, 2),
        (false, 0, 28),
        (false, 1, 0),
        (true, 1, 0),
        (false, 1, 1),
        (false, 5, 1),
        (true, 15, 1),
        (false, 25, 2),
        (false, 1, 28),
        (true, 1, 28),
        (false, 5 * p10(27), 28),
        (false, p10(28), 28),
        (false, 999_999_999, 0),
        (false, u32::MAX as u128, 0),
        (true, u32::MAX as u128, 9),
        (false, 1u128 << 32, 0),
        (false, 1u128 << 32, 5),
        (false, u64::MAX as u128, 0),
        (true, 1u128 << 64, 3),
        (false, MAX_MANTISSA, 0),
        (true, MAX_MANTISSA, 0),
        (false, MAX_MANTISSA - 1, 0),
        (false, MAX_MANTISSA, 1),
        (true, MAX_MANTISSA, 14),
        (false, MAX_MANTISSA, 28),
        (false, MAX_MANTISSA / 10, 0),
        (false, 12_345_678_901_234_567_890_123_456_789, 10),
        (true, 12_345_678_901_234_567_890_123_456_789, 28),
        (false, 50_000_000_000_000_000_000_000_000_005, 1),
    ];
    cases
        .iter()
        .map(|&(negative, mantissa, scale)| decimal(negative, mantissa, scale))
        .collect()
}

fn random_decimal(rng: &mut Rng) -> Decimal {
    let bits = rng.below(97) as u32;
    let raw = ((rng.next() as u128) << 64) | rng.next() as u128;
    let mantissa = if bits == 0 {
        0
    } else {
        (raw >> (128 - bits)) & MAX_MANTISSA
    };
    // Bias toward large mantissas, where alignment overflows and rounding happen.
    let mantissa = if rng.below(4) == 0 {
        MAX_MANTISSA - (mantissa & 0xFFFF)
    } else {
        mantissa
    };
    let scale = rng.below(29) as u32;
    decimal(rng.below(2) == 0, mantissa, scale)
}

fn result(value: Result<Decimal, bind_runtime::AppError>) -> Value {
    match value {
        Ok(value) => json!(value.to_string()),
        Err(_) => Value::Null,
    }
}

fn binary_case(a: &Decimal, b: &Decimal) -> Value {
    json!({
        "a": a.to_string(),
        "b": b.to_string(),
        "add": result(decimal_add(a, b)),
        "sub": result(decimal_sub(a, b)),
        "cmp": a.cmp(b) as i8,
    })
}

fn decimal_vectors() -> Value {
    let edges = edge_decimals();
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    let mut binary = Vec::new();
    for a in &edges {
        for b in &edges {
            binary.push(binary_case(a, b));
        }
    }
    for _ in 0..1500 {
        let a = random_decimal(&mut rng);
        let b = random_decimal(&mut rng);
        binary.push(binary_case(&a, &b));
    }
    let mut neg: Vec<Value> = edges
        .iter()
        .map(|a| json!({"a": a.to_string(), "result": result(decimal_neg(a))}))
        .collect();
    for _ in 0..50 {
        let a = random_decimal(&mut rng);
        neg.push(json!({"a": a.to_string(), "result": result(decimal_neg(&a))}));
    }
    let mut texts: Vec<String> = edges.iter().map(|d| d.to_string()).collect();
    for text in [
        "-0",
        "-0.00",
        "0.50",
        "01",
        "00",
        "+1",
        ".5",
        "1.",
        "1e5",
        "1_000",
        " 1",
        "1 ",
        "",
        "-",
        "--1",
        "0x10",
        "1.0000000000000000000000000000",
        "1.00000000000000000000000000000",
        "0.00000000000000000000000000001",
        "79228162514264337593543950335",
        "79228162514264337593543950336",
        "-79228162514264337593543950336",
        "7.9228162514264337593543950335",
        "7.9228162514264337593543950336",
        "123456789012345678901234567890",
        "١٢",
    ] {
        texts.push(text.to_string());
    }
    let parse: Vec<Value> = texts
        .iter()
        .map(|text| {
            let value = Decimal::from_str(text).ok().map(|d| d.to_string());
            json!({"text": text, "value": value})
        })
        .collect();
    json!({"parse": parse, "neg": neg, "binary": binary})
}

fn f32_bits(value: f32) -> String {
    format!("{:08x}", value.to_bits())
}

fn edge_f32s() -> Vec<f32> {
    vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.1,
        1.5,
        3.0,
        1.0e-45,
        -1.0e-45,
        f32::MIN_POSITIVE,
        f32::MAX,
        f32::MIN,
        f32::EPSILON,
        16_777_216.0,
        16_777_217.0,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        1010.4383,
        -87.76602,
        1.0e10,
        3.4e-38,
    ]
}

fn random_f32(rng: &mut Rng) -> f32 {
    match rng.below(3) {
        // Any bit pattern (subnormals, NaNs and infinities included).
        0 => f32::from_bits(rng.next() as u32),
        // Moderate magnitudes, where sums keep many significant digits.
        1 => (rng.below(2_000_000) as f32 - 1_000_000.0) / (1 + rng.below(1000)) as f32,
        _ => f32::from_bits(0x3000_0000 + rng.below(0x2000_0000) as u32),
    }
}

fn f32_vectors() -> Value {
    let edges = edge_f32s();
    let mut rng = Rng(0xFEDC_BA98_7654_3210);
    let mut pairs = Vec::new();
    for a in &edges {
        for b in &edges {
            pairs.push((*a, *b));
        }
    }
    for _ in 0..1000 {
        pairs.push((random_f32(&mut rng), random_f32(&mut rng)));
    }
    let binary: Vec<Value> = pairs
        .iter()
        .map(|&(a, b)| {
            json!({
                "a": f32_bits(a),
                "b": f32_bits(b),
                "add": f32_bits(a + b),
                "sub": f32_bits(a - b),
                "mul": f32_bits(a * b),
                "div": f32_bits(a / b),
            })
        })
        .collect();
    let mut doubles: Vec<f64> = vec![
        0.1,
        -0.1,
        1.0e40,
        -1.0e40,
        1.0e-50,
        f64::MAX,
        f64::MIN_POSITIVE,
        16_777_217.0,
        0.300_000_000_000_000_04,
        1.000_000_059_604_644_8,
        1.000_000_178_813_934_3,
        f64::INFINITY,
        f64::NAN,
    ];
    for _ in 0..200 {
        let value = f64::from_bits(rng.next());
        doubles.push(value);
        doubles.push(rng.below(1_000_000_000) as f64 / (1 + rng.below(100_000)) as f64);
    }
    let from_f64: Vec<Value> = doubles
        .iter()
        .map(|&d| json!({"f64": format!("{:016x}", d.to_bits()), "f32": f32_bits(d as f32)}))
        .collect();
    let mut texts: Vec<f32> = edges.clone();
    for _ in 0..200 {
        texts.push(random_f32(&mut rng));
    }
    let text: Vec<Value> = texts
        .iter()
        .map(|&f| json!({"bits": f32_bits(f), "text": js_number_to_string(f as f64)}))
        .collect();
    json!({"binary": binary, "fromF64": from_f64, "text": text})
}

/// One entry per line inside each top-level array, so diffs stay readable.
fn layout(value: &Value) -> String {
    let Value::Object(sections) = value else {
        panic!("expected an object");
    };
    let parts: Vec<String> = sections
        .iter()
        .map(|(name, entries)| {
            let lines: Vec<String> = entries
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| format!("    {}", serde_json::to_string(entry).unwrap()))
                .collect();
            format!("  {}: [\n{}\n  ]", json!(name), lines.join(",\n"))
        })
        .collect();
    format!("{{\n{}\n}}\n", parts.join(",\n"))
}

fn check(name: &str, value: &Value) {
    let path = format!(
        "{}/../bind-runtime-js/tests/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let generated = layout(value);
    if std::env::var_os("BIND_BLESS").is_some() {
        std::fs::write(&path, &generated).unwrap();
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "{path} is stale: regenerate with BIND_BLESS=1 cargo test --test js_vectors"
    );
}

#[test]
fn decimal_vectors_file() {
    check("decimal-vectors.json", &decimal_vectors());
}

#[test]
fn f32_vectors_file() {
    check("f32-vectors.json", &f32_vectors());
}
