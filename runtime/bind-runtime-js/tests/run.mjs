// Tests of the JS runtime. Run with `node runtime/bind-runtime-js/tests/run.mjs` (no npm
// packages). Shared vectors: runtime/vectors/*.json (bindc, Rust runtime and this runtime agree)
// and tests/decimal-vectors.json, tests/f32-vectors.json (generated from rust_decimal and Rust's
// f32 by runtime/bind-runtime/tests/js_vectors.rs).

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import * as rt from "../bind-runtime.js";
import { createDocument, serialize } from "./dom.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const readJson = (path) => JSON.parse(readFileSync(join(here, path), "utf8"));

const tests = [];
const test = (name, body) => tests.push({ name, body });

class AssertionError extends Error {}
function assert(condition, message) {
  if (!condition) throw new AssertionError(message);
}
function eq(actual, expected, message) {
  if (!Object.is(actual, expected)) {
    throw new AssertionError(`${message ?? ""}\n  expected: ${show(expected)}\n  actual:   ${show(actual)}`);
  }
}
function show(value) {
  if (typeof value === "bigint") return `${value}n`;
  if (typeof value === "string") return JSON.stringify(value);
  return String(value);
}
async function rejects(promise, kind, message) {
  try {
    await promise;
  } catch (error) {
    assert(error instanceof rt.BindError, `${message}: expected BindError, got ${error}`);
    eq(error.kind, kind, message);
    return error;
  }
  throw new AssertionError(`${message}: expected BindError(${kind}), nothing thrown`);
}
function throwsKind(body, kind, message) {
  try {
    body();
  } catch (error) {
    assert(error instanceof rt.BindError, `${message}: expected BindError, got ${error}`);
    eq(error.kind, kind, message);
    return error;
  }
  throw new AssertionError(`${message}: expected BindError(${kind}), nothing thrown`);
}
function throwsDecode(body, message) {
  try {
    body();
  } catch (error) {
    assert(error instanceof rt.DecodeError, `${message}: expected DecodeError, got ${error}`);
    return;
  }
  throw new AssertionError(`${message}: expected DecodeError, nothing thrown`);
}

// ---- Int ----

test("intParse accepts canonical integers only", () => {
  for (const text of ["0", "-1", "1", "42", "-9007199254740993", "1".repeat(1000), "-" + "9".repeat(1000)]) {
    eq(rt.intParse(text), BigInt(text), text.slice(0, 20));
    eq(rt.intToString(rt.intParse(text)), text);
  }
  for (const text of ["01", "-0", "+1", "", " 1", "1 ", "1.5", "-", "--1", "1e3", "0x10", "１", "1".repeat(1001), "-" + "1".repeat(1001)]) {
    eq(rt.intParse(text), null, JSON.stringify(text.slice(0, 20)));
  }
  eq(rt.intParse(5), null, "not a string");
  eq(rt.intParse(null), null, "null");
});

test("intDiv and intRem truncate toward zero; remainder has the dividend's sign", () => {
  const cases = [
    [7n, 2n, 3n, 1n],
    [-7n, 2n, -3n, -1n],
    [7n, -2n, -3n, 1n],
    [-7n, -2n, 3n, -1n],
    [6n, 3n, 2n, 0n],
    [0n, -5n, 0n, 0n],
  ];
  for (const [a, b, q, r] of cases) {
    eq(rt.intDiv(a, b).some, q, `${a} / ${b}`);
    eq(rt.intRem(a, b).some, r, `${a} % ${b}`);
  }
  eq(rt.intDiv(1n, 0n), null, "div by zero");
  eq(rt.intRem(1n, 0n), null, "rem by zero");
  eq(rt.intDiv(0n, 0n), null, "0 / 0");
});

// ---- Decimal (vectors from rust_decimal) ----

const decimals = readJson("decimal-vectors.json");

test("Decimal.parse agrees with rust_decimal and is strict", () => {
  assert(decimals.parse.length >= 40, "parse vectors");
  for (const { text, value } of decimals.parse) {
    const parsed = rt.Decimal.parse(text);
    if (parsed !== null) {
      eq(parsed.toString(), value, `parse ${JSON.stringify(text)} (accepted, must equal rust_decimal)`);
    }
    // Every canonical text (what rust_decimal prints) is accepted.
    if (value === text) assert(parsed !== null, `canonical ${JSON.stringify(text)} rejected`);
  }
  for (const text of ["01", "+1", ".5", "1.", "1_000", "1e5", " 1", "-", "0.00000000000000000000000000001", "79228162514264337593543950336"]) {
    eq(rt.Decimal.parse(text), null, `strict: ${JSON.stringify(text)}`);
  }
  eq(rt.Decimal.parse("-0").toString(), "0", "rust_decimal parses -0 as 0");
  // More than 29 digits never fit in 96 bits: rejected before any BigInt conversion.
  eq(rt.Decimal.parse("100000000000000000000000000000"), null, "30 digits");
  eq(rt.Decimal.parse("0.0000000000000000000000000001").toString(), "0.0000000000000000000000000001", "29 digits");
  eq(rt.Decimal.parse("79228162514264337593543950335").toString(), "79228162514264337593543950335", "2^96 - 1");
  // The cap is checked before BigInt: a counting wrapper sees no conversion for long inputs.
  const realBigInt = globalThis.BigInt;
  let conversions = 0;
  globalThis.BigInt = (value) => {
    conversions += 1;
    return realBigInt(value);
  };
  try {
    eq(rt.Decimal.parse("9".repeat(1000000)), null, "a million digits");
    eq(rt.Decimal.parse("1." + "0".repeat(28) + "0"), null, "29 decimals");
    eq(rt.Decimal.parse("1" + "0".repeat(29)), null, "30 digits");
    eq(conversions, 0, "no BigInt conversion of an over-long input");
    rt.Decimal.parse("12.5");
    eq(conversions, 1, "the wrapper counts conversions");
  } finally {
    globalThis.BigInt = realBigInt;
  }
  eq(rt.Decimal.parse(1), null, "not a string");
  assert(Object.isFrozen(rt.Decimal.parse("1.5")), "frozen");
});

function dec(text) {
  // Vector inputs come from rust_decimal's Display, which may print a negative zero ("-0.00");
  // build those from parts (parse reads "-0" as zero, like rust_decimal).
  const parsed = rt.Decimal.parse(text);
  if (parsed !== null && text.startsWith("-") && parsed.isZero()) {
    return rt.Decimal.fromParts(true, 0n, parsed.scale);
  }
  assert(parsed !== null, `vector input ${text}`);
  return parsed;
}

function checked(op) {
  try {
    return op().toString();
  } catch (error) {
    if (error instanceof rt.BindError && error.kind === "Overflow") return null;
    throw error;
  }
}

test("Decimal toString round-trips rust_decimal's Display (sign of zero kept)", () => {
  for (const { a, b } of decimals.binary) {
    eq(dec(a).toString(), a);
    eq(dec(b).toString(), b);
    eq(rt.displayDecimal(dec(a)), a);
  }
});

test("decimalAdd / decimalSub / decimalCompare agree with rust_decimal", () => {
  assert(decimals.binary.length >= 2000, "binary vectors");
  let overflows = 0;
  for (const { a, b, add, sub, cmp } of decimals.binary) {
    eq(checked(() => rt.decimalAdd(dec(a), dec(b))), add, `${a} + ${b}`);
    eq(checked(() => rt.decimalSub(dec(a), dec(b))), sub, `${a} - ${b}`);
    eq(rt.decimalCompare(dec(a), dec(b)), cmp, `compare ${a} ${b}`);
    if (add === null) overflows += 1;
  }
  assert(overflows > 0, "vectors include overflows");
});

test("decimalNeg agrees with rust_decimal", () => {
  for (const { a, result } of decimals.neg) {
    eq(checked(() => rt.decimalNeg(dec(a))), result, `-(${a})`);
  }
});

test("Decimal overflow is BindError(Overflow)", () => {
  const max = rt.Decimal.parse("79228162514264337593543950335");
  throwsKind(() => rt.decimalAdd(max, rt.Decimal.parse("1")), "Overflow", "max + 1");
  throwsKind(() => rt.decimalSub(rt.decimalNeg(max), rt.Decimal.parse("1")), "Overflow", "-max - 1");
});

// ---- f32 (vectors from Rust's f32) ----

const f32s = readJson("f32-vectors.json");
const scratch = new DataView(new ArrayBuffer(8));
function fromF32Bits(hex) {
  scratch.setUint32(0, parseInt(hex, 16));
  return scratch.getFloat32(0);
}
function fromF64Bits(hex) {
  scratch.setBigUint64(0, BigInt(`0x${hex}`));
  return scratch.getFloat64(0);
}
function sameF32(actual, expectedHex, message) {
  const expected = fromF32Bits(expectedHex);
  if (Number.isNaN(expected)) {
    assert(Number.isNaN(actual), `${message}: expected NaN, got ${actual}`);
    return;
  }
  eq(actual, expected, message);
}

test("f32 arithmetic (Math.fround after each operation) equals Rust f32", () => {
  assert(f32s.binary.length >= 1000, "f32 vectors");
  for (const c of f32s.binary) {
    const a = fromF32Bits(c.a);
    const b = fromF32Bits(c.b);
    sameF32(rt.f32(a + b), c.add, `${c.a} + ${c.b}`);
    sameF32(rt.f32(a - b), c.sub, `${c.a} - ${c.b}`);
    sameF32(rt.f32(a * b), c.mul, `${c.a} * ${c.b}`);
    sameF32(rt.f32(a / b), c.div, `${c.a} / ${c.b}`);
  }
  for (const c of f32s.fromF64) sameF32(rt.f32(fromF64Bits(c.f64)), c.f32, `f32(${c.f64})`);
  for (const c of f32s.text) eq(rt.jsNumberToString(fromF32Bits(c.bits)), c.text, `display ${c.bits}`);
});

// ---- Numbers (runtime/vectors/numbers.json) ----

test("jsNumberToString matches numbers.json", () => {
  const cases = readJson("../../vectors/numbers.json");
  assert(cases.length >= 20, "number vectors");
  for (const { bits, text } of cases) eq(rt.jsNumberToString(fromF64Bits(bits)), text, bits);
});

// ---- Date / DateTime / Instant order (runtime/vectors/temporal-order.json) ----

test("compareDate / compareDateTime / compareInstant match temporal-order.json", () => {
  const cases = readJson("../../vectors/temporal-order.json");
  assert(cases.length >= 30, "temporal vectors");
  const compare = { Date: rt.compareDate, DateTime: rt.compareDateTime, Instant: rt.compareInstant };
  const symbol = (order) => (order < 0 ? "<" : order > 0 ? ">" : "=");
  for (const { type, a, b, order } of cases) {
    eq(symbol(compare[type](a, b)), order, `${type} ${a} ${b}`);
    eq(symbol(compare[type](b, a)), symbol(-compare[type](a, b)), `${type} ${b} ${a}`);
  }
  // Text order would be wrong: "Z" sorts after "." and "+" before digits.
  eq(rt.compareInstant("2026-01-01T10:00:00Z", "2026-01-01T10:00:00.500Z"), -1);
  eq(rt.compareDate("+10000-01-01", "9999-12-31"), 1);
  eq(rt.compareDate("x", "2026-01-01"), 1, "not a temporal text sorts last");
});

// ---- enter / recursion limit ----

test("enter: 10000 nested entries pass, the 10001st is RecursionLimit", async () => {
  eq(rt.MAX_DEPTH, 10000);
  let ctx = rt.newCtx();
  for (let i = 0; i < 10000; i++) ctx = await rt.enter(ctx);
  await rejects(rt.enter(ctx), "RecursionLimit", "10001st entry");
});

test("enter always awaits (returns a promise)", () => {
  const result = rt.enter(rt.newCtx());
  assert(result instanceof Promise, "enter returns a promise");
});

test("real recursion: 10000 levels pass; 100000 levels give RecursionLimit, never RangeError", async () => {
  async function deep(ctx, n) {
    ctx = await rt.enter(ctx);
    if (n === 0) return 0;
    return 1 + (await deep(ctx, n - 1));
  }
  eq(await deep(rt.newCtx(), 9999), 9999);
  await rejects(deep(rt.newCtx(), 100000), "RecursionLimit", "100000 levels");
});

// ---- Lists ----

test("map / fold over 100000 items finish (Review Focus 3)", async () => {
  const xs = rt.listOf(...Array.from({ length: 100000 }, (_, i) => BigInt(i)));
  const lambda = async (ctx, x) => {
    ctx = await rt.enter(ctx);
    return x * 2n;
  };
  const doubled = await rt.map(rt.newCtx(), xs, lambda);
  eq(doubled.length, 100000);
  eq(doubled[99999], 199998n);
  assert(Object.isFrozen(doubled), "map result frozen");
  const sum = await rt.fold(rt.newCtx(), xs, 0n, async (ctx, acc, x) => {
    ctx = await rt.enter(ctx);
    return acc + x;
  });
  eq(sum, 4999950000n);
});

test("first / get / length / listOf / emptyList", () => {
  const xs = rt.listOf(1n, 2n, 3n);
  assert(Object.isFrozen(xs), "listOf frozen");
  eq(rt.first(xs).some, 1n);
  eq(rt.first(rt.emptyList()), null);
  eq(rt.get(xs, 2n).some, 3n);
  eq(rt.get(xs, 3n), null);
  eq(rt.get(xs, -1n), null);
  eq(rt.get(xs, 10n ** 30n), null);
  eq(rt.length(xs), 3n);
  eq(rt.length(rt.emptyList()), 0n);
  assert(Object.isFrozen(rt.emptyList()), "emptyList frozen");
  // Uniform options: a null element (None) is some(null), distinct from the empty list's null.
  const opts = rt.listOf(null);
  eq(rt.first(opts).some, null);
  assert(Object.isFrozen(rt.first(opts)), "some frozen");
  eq(rt.get(opts, 0n).some, null);
});

// ---- equals / compare ----

test("equals and compare are structural", () => {
  const d = (t) => rt.Decimal.parse(t);
  assert(rt.equals(1n, 1n) && !rt.equals(1n, 2n), "bigint");
  assert(rt.equals(d("1.0"), d("1.00")), "decimal scale");
  assert(rt.equals(rt.Decimal.fromParts(true, 0n, 0), d("0")), "decimal -0 == 0");
  assert(!rt.equals(NaN, NaN) && rt.equals(0, -0), "double");
  assert(rt.equals(null, null) && !rt.equals(null, 0n), "null");
  const r = (a, b) => rt.record([["a", a], ["b", b]]);
  assert(rt.equals(r(1n, rt.listOf("x")), r(1n, rt.listOf("x"))), "records");
  assert(!rt.equals(r(1n, rt.listOf("x")), r(1n, rt.listOf("y"))), "records differ");
  assert(!rt.equals(rt.listOf(1n), rt.listOf(1n, 2n)), "lengths");
  assert(rt.equals(rt.variant("Ok", [["value", 1n]]), rt.variant("Ok", [["value", 1n]])), "variants");
  assert(!rt.equals(rt.variant("Ok", [["value", 1n]]), rt.variant("Err", [["value", 1n]])), "tags");
  eq(rt.compare(1n, 2n), -1);
  eq(rt.compare(2n, 2n), 0);
  eq(rt.compare(d("-0.5"), d("-0.01")), -1);
  eq(rt.compare("b", "a"), 1);
  // Code point order (Rust's String order), not UTF-16 code units.
  eq(rt.compare("\u{1F600}", "￿"), 1);
  eq(rt.compare(null, 0n), -1);
  eq(rt.compare(rt.listOf(1n, 2n), rt.listOf(1n, 3n)), -1);
  eq(rt.compare(rt.listOf(1n), rt.listOf(1n, 0n)), -1);
  assert(Number.isNaN(rt.compare(NaN, 1)), "unordered doubles");
  eq(rt.compare(false, true), -1);
});

// ---- UI and rendering ----

function treeNode(tree) {
  if (tree.node === "Empty") return rt.EMPTY;
  if (tree.node === "Text") return rt.text(tree.args[0]);
  const args = tree.args.map((arg) => (typeof arg === "string" ? arg : BigInt(arg.int)));
  return rt.node(tree.node, args, tree.children.map(treeNode));
}

function renderToHtml(tree, document) {
  return rt.render(tree, document).map(serialize).join("");
}

test("render escapes text and never uses HTML strings (Review Focus 4)", () => {
  const document = createDocument();
  const html = renderToHtml(rt.text("<script>alert(1)</script>"), document);
  assert(html.includes("&lt;script&gt;alert(1)&lt;/script&gt;"), html);
  assert(!html.includes("<script"), html);
  const nodes = rt.render(rt.node("Div", [], [rt.text("<b>x</b>")]), document);
  eq(nodes[0].childNodes.length, 1, "one text child");
  eq(nodes[0].childNodes[0].nodeType, 3, "a text node");
  eq(nodes[0].textContent, "<b>x</b>");
  eq(document.htmlApiUsed.length, 0, "HTML string APIs used");
});

test("render matches render_html on html-trees.json", () => {
  const cases = readJson("../../vectors/html-trees.json");
  assert(cases.length >= 20, "html vectors");
  const document = createDocument();
  for (const { tree, html } of cases) eq(renderToHtml(treeNode(tree), document), html, JSON.stringify(tree));
  eq(document.htmlApiUsed.length, 0, "HTML string APIs used");
});

test("node rejects unknown tags", () => {
  let threw = false;
  try {
    rt.node("Script", [], []);
  } catch {
    threw = true;
  }
  assert(threw, "unknown tag accepted");
});

test("pageHtml and errorBody match pages.json", () => {
  const cases = readJson("../../vectors/pages.json");
  assert(cases.length >= 6, "page vectors");
  const document = createDocument();
  for (const c of cases) {
    if (c.kind === "page") {
      const meta = { title: c.title, description: c.description, language: c.language, stylesheet: c.stylesheet };
      eq(rt.pageHtml(meta, c.body), c.html, c.title);
    } else if (c.kind === "bad_parameter") {
      // The boot code shows BindError("BadParameter", name) as errorBody(kind, message).
      const { status, tree } = rt.errorBody("BadParameter", c.name);
      eq(status, c.status, c.name);
      eq(renderToHtml(tree, document), c.html, c.name);
    } else {
      const { status, tree } = rt.errorBody(c.error, c.message);
      eq(status, c.status, c.error);
      eq(renderToHtml(tree, document), c.html, c.error);
    }
  }
});

test("TEXTS equal the Rust runtime's screen texts", () => {
  const source = readFileSync(join(here, "../../bind-runtime/src/lib.rs"), "utf8");
  const constant = (name) => {
    const match = source.match(new RegExp(`pub const ${name}: &str = "([^"]*)";`));
    assert(match, name);
    return match[1];
  };
  eq(rt.TEXTS.loading, constant("TEXT_LOADING"));
  eq(rt.PAGE_ICON, constant("PAGE_ICON"));
  eq(rt.TEXTS.requestFailed, constant("TEXT_REQUEST_FAILED"));
  eq(rt.TEXTS.titleBadRequest, constant("TITLE_BAD_REQUEST"));
  eq(rt.TEXTS.titleNotFound, constant("TITLE_NOT_FOUND"));
  eq(rt.TEXTS.titleError, constant("TITLE_ERROR"));
  assert(Object.isFrozen(rt.TEXTS), "frozen");
});

test("the runtime source uses no HTML-string or code-evaluation APIs", () => {
  const source = readFileSync(join(here, "../bind-runtime.js"), "utf8");
  for (const pattern of [/innerHTML/, /outerHTML/, /insertAdjacentHTML/, /document\.write/, /\beval\b/, /\bFunction\s*\(/, /new\s+Function/]) {
    assert(!pattern.test(source), `runtime source matches ${pattern}`);
  }
});

// ---- Decoders and records (Review Focus 5) ----

test("record / decodeRecord: no prototype pollution, frozen, strict", () => {
  const fields = [
    ["__proto__", rt.decodeString],
    ["constructor", rt.decodeInt],
    ["toString", rt.decodeOptional(rt.decodeDecimal)],
    ["hasOwnProperty", rt.decodeOptional(rt.decodeBool)],
  ];
  const decode = rt.decodeRecord(fields);
  const json = JSON.parse('{"__proto__": "p", "constructor": "12", "toString": "1.50"}');
  const value = decode(json);
  eq(Object.getPrototypeOf(value), null, "null prototype");
  assert(Object.isFrozen(value), "frozen");
  eq(value.__proto__, "p");
  eq(value.constructor, 12n);
  eq(value.toString.some.toString(), "1.50");
  eq(value.hasOwnProperty, null, "absent optional field is null");
  eq(Object.prototype.p, undefined, "Object.prototype untouched");
  eq({}.constructor, Object, "Object.prototype untouched");
  throwsDecode(() => decode(JSON.parse('{"__proto__": "p", "constructor": "12", "toString": null, "x": 1}')), "unknown field");
  throwsDecode(() => decode(JSON.parse('{"__proto__": "p", "toString": null}')), "missing field");
  throwsDecode(() => decode(JSON.parse('{"__proto__": "p", "constructor": "012", "toString": null}')), "non-canonical Int");
  throwsDecode(() => decode([]), "array");
  throwsDecode(() => decode(null), "null");
  const built = rt.record([["__proto__", 1n], ["constructor", 2n]]);
  eq(Object.getPrototypeOf(built), null);
  eq(built.__proto__, 1n);
  assert(Object.isFrozen(built), "record frozen");
  throwsDecode(() => rt.decodeList(rt.decodeInt)(["1", 2]), "list item");
  eq(rt.decodeList(rt.decodeInt)(["1", "-2"])[1], -2n);
  assert(Object.isFrozen(rt.decodeList(rt.decodeInt)([])), "list frozen");
  // serde_json writes an f32 with its shortest f32 digits ("0.1"): rounded, like serde's cast.
  eq(rt.decodeFloat(0.1), Math.fround(0.1), "f32 digits round to the f32");
  eq(rt.decodeFloat(0.5), 0.5);
  throwsDecode(() => rt.decodeFloat("0.5"), "float from string");
  eq(rt.decodeDouble(0.1), 0.1);
  throwsDecode(() => rt.decodeDouble("1"), "double from string");
  // JSON.parse("1e400") is Infinity; serde_json rejects the literal (NumberOutOfRange).
  throwsDecode(() => rt.decodeDouble(JSON.parse("1e400")), "double out of range");
  throwsDecode(() => rt.decodeDouble(NaN), "double NaN");
  throwsDecode(() => rt.decodeFloat(JSON.parse("-1e400")), "float out of range");
  eq(rt.decodeFloat(1e39), Infinity, "a finite f64 beyond f32 casts to infinity, as serde");
});

test("decoders with property names; Unit, Long, pairs and some", () => {
  const decode = rt.decodeRecord([
    ["tag", rt.decodeString, "u_tag"],
    ["__proto__", rt.decodeInt, "u_____proto____"],
    ["n", rt.decodeOptional(rt.decodeInt), "u_n"],
  ]);
  const value = decode(JSON.parse('{"tag": "t", "__proto__": "1"}'));
  eq(value.u_tag, "t");
  eq(value.u_____proto____, 1n);
  eq(value.u_n, null, "missing optional field");
  eq(decode(JSON.parse('{"tag": "t", "__proto__": "1", "n": "4"}')).u_n.some, 4n, "present optional is some");
  const nestedOption = rt.decodeOptional(rt.decodeOptional(rt.decodeInt));
  eq(nestedOption(null), null);
  eq(nestedOption("3").some.some, 3n);
  eq(Object.keys(value).join(","), "u_tag,u_____proto____,u_n");
  eq(Object.prototype.u_tag, undefined);
  throwsDecode(() => decode({ u_tag: "t", __proto__: "1" }), "property names are not JSON names");
  const shape = rt.decodeVariant([["box", "Box", [["tag", rt.decodeInt, "u_tag"]]]]);
  const box = shape({ box: { tag: "2" } });
  eq(box.tag, "Box", "the variant tag is not overwritten by a field named tag");
  eq(box.u_tag, 2n);
  eq(rt.decodeUnit(null), undefined);
  throwsDecode(() => rt.decodeUnit(0), "unit");
  eq(rt.decodeLong(-42), -42n);
  throwsDecode(() => rt.decodeLong(1.5), "fractional long");
  throwsDecode(() => rt.decodeLong(2 ** 53), "inexact long");
  throwsDecode(() => rt.decodeLong("1"), "long from string");
  eq(rt.encodeLong(-42n), -42);
  let threw = false;
  try {
    rt.encodeLong(2n ** 53n);
  } catch (error) {
    threw = error instanceof TypeError;
  }
  assert(threw, "encodeLong beyond 2^53 - 1");
  const pair = rt.decodePair(rt.decodeString, rt.decodeInt)(["a", "1"]);
  eq(pair[0], "a");
  eq(pair[1], 1n);
  assert(Object.isFrozen(pair), "pair frozen");
  throwsDecode(() => rt.decodePair(rt.decodeString, rt.decodeInt)(["a"]), "short pair");
  const nested = rt.some(null);
  eq(nested.some, null);
  assert(Object.isFrozen(nested), "some frozen");
  assert(rt.equals(rt.some(1n), rt.first([1n])), "some equals first");
  assert(!rt.equals(rt.some(null), null), "Some(None) is not None");
  eq(rt.compare(null, rt.some(null)), -1, "None sorts first");
  eq(rt.compare(rt.some(1n), rt.some(2n)), -1, "Some by value");
});

test("decodeVariant and decodeResult follow serde's externally tagged form", () => {
  const shape = rt.decodeVariant([
    ["circle", "Circle", [["radius", rt.decodeInt]]],
    ["none", "None", null],
    ["toString", "ToString", null],
  ]);
  const circle = shape({ circle: { radius: "3" } });
  eq(circle.tag, "Circle");
  eq(circle.radius, 3n);
  eq(Object.getPrototypeOf(circle), null);
  eq(shape("none").tag, "None");
  eq(shape("toString").tag, "ToString");
  throwsDecode(() => shape("constructor"), "inherited name");
  throwsDecode(() => shape({ circle: { radius: "3" }, none: null }), "two keys");
  throwsDecode(() => shape({ circle: { radius: "3", x: 1 } }), "unknown field");
  const result = rt.decodeResult(rt.decodeInt, rt.decodeString);
  eq(result({ Ok: "1" }).value, 1n);
  eq(result({ Err: "e" }).tag, "Err");
  throwsDecode(() => result({ Ok: "1", Err: "e" }), "both");
});

// ---- rpc ----

function withFetch(fake, body) {
  const saved = globalThis.fetch;
  globalThis.fetch = fake;
  return body().finally(() => {
    globalThis.fetch = saved;
  });
}
const respond = (status, text) => async () => new Response(text, { status });

test("rpc posts JSON and decodes the response", async () => {
  let seen;
  await withFetch(
    async (url, init) => {
      seen = { url, init };
      return new Response('{"total":"5"}', { status: 200 });
    },
    async () => {
      const decode = rt.decodeRecord([["total", rt.decodeInt]]);
      const value = await rt.rpc(rt.newCtx(), "/api/f", { a: "1" }, decode);
      eq(value.total, 5n);
    },
  );
  eq(seen.url, "/api/f");
  eq(seen.init.method, "POST");
  eq(seen.init.body, '{"a":"1"}');
  eq(seen.init.headers["content-type"], "application/json");
});

test("rpc: a body that cannot be serialized is a TypeError, not Remote", async () => {
  let called = false;
  await withFetch(
    async () => {
      called = true;
      return new Response("1", { status: 200 });
    },
    async () => {
      let error;
      try {
        await rt.rpc(rt.newCtx(), "/api/f", { a: 1n }, rt.decodeDouble);
      } catch (caught) {
        error = caught;
      }
      assert(error instanceof TypeError, `expected TypeError, got ${error}`);
    },
  );
  assert(!called, "fetch not called");
});

test("rpc failures become BindError(Remote) (Review Focus 2)", async () => {
  const ctx = rt.newCtx();
  const decode = rt.decodeInt;
  const failed = rt.TEXTS.requestFailed;
  let error = await withFetch(respond(400, '{"error":"bad <id>"}'), () => rejects(rt.rpc(ctx, "/api/f", {}, decode), "Remote", "400 json"));
  eq(error.message, "bad <id>");
  error = await withFetch(respond(500, "<html>oops</html>"), () => rejects(rt.rpc(ctx, "/api/f", {}, decode), "Remote", "500 html"));
  eq(error.message, failed);
  error = await withFetch(respond(500, '{"message":"x"}'), () => rejects(rt.rpc(ctx, "/api/f", {}, decode), "Remote", "500 no error"));
  eq(error.message, failed);
  error = await withFetch(respond(500, '{"error":5}'), () => rejects(rt.rpc(ctx, "/api/f", {}, decode), "Remote", "500 error not string"));
  eq(error.message, failed);
  error = await withFetch(async () => { throw new TypeError("network"); }, () => rejects(rt.rpc(ctx, "/api/f", {}, decode), "Remote", "network"));
  eq(error.message, failed);
  error = await withFetch(respond(200, "not json"), () => rejects(rt.rpc(ctx, "/api/f", {}, decode), "Remote", "200 not json"));
  eq(error.message, failed);
  error = await withFetch(respond(200, '"01"'), () => rejects(rt.rpc(ctx, "/api/f", {}, decode), "Remote", "200 malformed"));
  eq(error.message, failed);
  error = await withFetch(respond(200, "5"), () => rejects(rt.rpc(ctx, "/api/f", {}, decode), "Remote", "200 wrong type"));
  eq(error.message, failed);
});

// ---- readParams ----

test("readParams: percent-decoding and + (Review Focus 1); absent, repeated or invalid is BadParameter", () => {
  const specs = [["nome", rt.textString], ["id", rt.textInt]];
  const params = rt.readParams("?nome=Jo%C3%A3o+Silva&id=42&extra=1", specs);
  eq(params.nome, "João Silva");
  eq(params.id, 42n);
  eq(Object.getPrototypeOf(params), null);
  assert(Object.isFrozen(params), "frozen");
  let error = throwsKind(() => rt.readParams("?nome=a&nome=b&id=1", specs), "BadParameter", "repeated");
  eq(error.message, "nome");
  error = throwsKind(() => rt.readParams("?nome=a", specs), "BadParameter", "absent");
  eq(error.message, "id");
  error = throwsKind(() => rt.readParams("?nome=a&id=01", specs), "BadParameter", "invalid");
  eq(error.message, "id");
  const tricky = rt.readParams("__proto__=x&constructor=-0.50", [["__proto__", rt.textString], ["constructor", rt.textDecimal]]);
  eq(tricky.__proto__, "x");
  eq(tricky.constructor.toString(), "-0.50");
  eq(rt.readParams("b=true", [["b", rt.textBool]]).b, true);
  throwsKind(() => rt.readParams("b=True", [["b", rt.textBool]]), "BadParameter", "bool");
});

test("readParams and the text decoders match params.json (shared with the SSR)", () => {
  const decoders = {
    String: rt.textString,
    Int: rt.textInt,
    UUID: rt.textUuid,
    Date: rt.textDate,
    DateTime: rt.textDateTime,
    Instant: rt.textInstant,
  };
  const shown = (value) => (typeof value === "bigint" ? value.toString() : value);
  const cases = readJson("../../vectors/params.json");
  assert(cases.length >= 40, "param vectors");
  for (const c of cases) {
    const label = JSON.stringify(c);
    if (c.kind === "text") {
      let value;
      let valid = true;
      try {
        value = decoders[c.type](c.text);
      } catch (error) {
        assert(error instanceof rt.DecodeError, `${label}: ${error}`);
        valid = false;
      }
      eq(valid, c.valid, label);
      if (valid) eq(shown(value), c.text, label);
    } else {
      const specs = c.params.map(([name, type]) => [name, decoders[type]]);
      if (c.error !== undefined) {
        const error = throwsKind(() => rt.readParams(c.query, specs), "BadParameter", label);
        eq(error.message, c.error, label);
      } else {
        const params = rt.readParams(c.query, specs);
        eq(Object.keys(params).length, Object.keys(c.values).length, label);
        for (const [name, value] of Object.entries(c.values)) eq(shown(params[name]), value, label);
      }
    }
  }
});

// ---- Page screens (boot) ----

test("LOADING renders the loading screen bindc writes into the split page", () => {
  eq(renderToHtml(rt.LOADING, createDocument()), '<main class="bind-loading"><p>Loading…</p></main>');
});

test("rpc keeps the HTTP status of an error response", async () => {
  const ctx = rt.newCtx();
  let error = await withFetch(respond(404, '{"error":"not found"}'), () => rejects(rt.rpc(ctx, "/api/f", {}, rt.decodeInt), "Remote", "404"));
  eq(error.status, 404);
  error = await withFetch(respond(500, "oops"), () => rejects(rt.rpc(ctx, "/api/f", {}, rt.decodeInt), "Remote", "500 html"));
  eq(error.status, undefined, "no status when the body is not the server's error");
});

test("errorScreen shows a failure as the SSR shows the same AppError", () => {
  const document = createDocument();
  const screen = (error) => {
    const { status, tree } = rt.errorScreen(error);
    return `${status} ${renderToHtml(tree, document)}`;
  };
  const remote = (message, status) => Object.assign(new rt.BindError("Remote", message), { status });
  const body = (title, message) => `<main class="bind-error"><h1>${title}</h1><p>${message}</p></main>`;
  eq(screen(new rt.BindError("BadParameter", "id")), `400 ${body("Bad request", "invalid parameter: id")}`);
  eq(screen(new rt.BindError("RecursionLimit", "x")), `500 ${body("Something went wrong", "recursion limit exceeded")}`);
  eq(screen(new rt.BindError("Overflow", "x")), `500 ${body("Something went wrong", "numeric overflow")}`);
  // The server's public messages, with the status of its response.
  eq(screen(remote("not found", 404)), `404 ${body("Page not found", "not found")}`);
  eq(screen(remote("bad <id>", 400)), `400 ${body("Bad request", "bad &lt;id&gt;")}`);
  eq(screen(remote("database error", 500)), `500 ${body("Something went wrong", "database error")}`);
  // Network failures and bodies that are not the server's error (Review Focus 2).
  eq(screen(new rt.BindError("Remote", rt.TEXTS.requestFailed)), `500 ${body("Something went wrong", "Request failed")}`);
  // Anything else is a compiler bug: the internal error screen, no details.
  eq(screen(new TypeError("secret detail")), `500 ${body("Something went wrong", "internal error")}`);
});

// ---- Deep values (no RangeError at any depth) ----

test("render, serialize and mount a 200000-level tree", () => {
  const depth = 200000;
  let tree = rt.text("x");
  for (let i = 0; i < depth; i++) tree = rt.node("Div", [], [tree]);
  const document = createDocument();
  const nodes = rt.render(rt.node("UI", [], [tree, rt.EMPTY, rt.text("<y>")]), document);
  eq(nodes.length, 2, "a fragment's children");
  const html = nodes.map(serialize).join("");
  eq(html.length, depth * "<div></div>".length + 1 + "&lt;y&gt;".length);
  assert(html.startsWith("<div><div>") && html.endsWith("</div></div>&lt;y&gt;"), "html");
  const container = document.createElement("div");
  rt.mount(container, nodes);
  eq(container.childNodes.length, 2);
  rt.mount(container, rt.render(rt.text("z"), document));
  eq(serialize(container), "<div>z</div>");
  // A fragment of 300000 top-level nodes mounts without spreading them as arguments.
  const wide = rt.render(rt.node("UI", [], Array.from({ length: 300000 }, () => rt.text("a"))), document);
  rt.mount(container, wide);
  eq(container.childNodes.length, 300000);
});

test("equals and compare on values nested 200000 deep", () => {
  const nest = (leaf) => {
    let value = rt.variant("Folha", [["u_v", leaf]]);
    for (let i = 0; i < 200000; i++) value = rt.variant("No", [["u_d", value]]);
    return value;
  };
  eq(rt.equals(nest(1n), nest(1n)), true);
  eq(rt.equals(nest(1n), nest(2n)), false);
  let list = rt.listOf(1n);
  for (let i = 0; i < 200000; i++) list = rt.listOf(list);
  let other = rt.listOf(2n);
  for (let i = 0; i < 200000; i++) other = rt.listOf(other);
  eq(rt.compare(list, other), -1);
  eq(rt.compare(other, list), 1);
  eq(rt.compare(list, list), 0);
  // Unchanged results on small values.
  eq(rt.compare(rt.listOf(1n, 2n), rt.listOf(1n)), 1);
  eq(rt.compare(rt.listOf(), rt.listOf(0n)), -1);
  eq(rt.compare(null, rt.some(1n)), -1);
  eq(rt.compare(rt.some(2n), rt.some(1n)), 1);
  assert(Number.isNaN(rt.compare(rt.listOf(NaN), rt.listOf(1))), "unordered");
  eq(rt.equals(rt.record([["a", NaN]]), rt.record([["a", NaN]])), false);
  eq(rt.equals(rt.listOf(rt.some(null)), rt.listOf(rt.some(null))), true);
  eq(rt.equals(rt.listOf(rt.some(null)), rt.listOf(null)), false);
});

// ---- Set / Map (runtime/vectors/collections.json and wire-rich.json) ----

// The fixture types of the vectors (runtime/vectors/README.md), with the codecs and key
// comparators bindc generates for them (backend::js::codecs).
const COR = rt.decodeVariant([["z", "Z", null], ["a", "A", null], ["m", "M", null]]);
const corOrder = rt.variantOrder(["Z", "A", "M"]);
const encodeCor = (v) => ({ Z: "z", A: "a", M: "m" })[v.tag];
const decimalCodec = { decode: rt.decodeDecimal, encode: (v) => v.toString(), compare: rt.decimalCompare };
const stringCodec = { decode: rt.decodeString, encode: (v) => v, compare: rt.compare };
const KEYS = {
  Int: { decode: rt.decodeInt, encode: (v) => v.toString(), compare: rt.compare },
  Decimal: decimalCodec,
  Nota: decimalCodec,
  String: stringCodec,
  UUID: stringCodec,
  Codigo: stringCodec,
  Boolean: { decode: rt.decodeBool, encode: (v) => v, compare: rt.compare },
  Cor: { decode: COR, encode: encodeCor, compare: corOrder },
  Date: { decode: rt.decodeString, encode: (v) => v, compare: rt.compareDate },
  DateTime: { decode: rt.decodeString, encode: (v) => v, compare: rt.compareDateTime },
  Instant: { decode: rt.decodeString, encode: (v) => v, compare: rt.compareInstant },
};

test("Set and Map builtins match collections.json (canonical order, Ruling A)", () => {
  const cases = readJson("../../vectors/collections.json");
  assert(cases.length >= 20, "collection vectors");
  for (const c of cases) {
    const label = JSON.stringify(c).slice(0, 120);
    const key = KEYS[c.kind === "set" ? c.element : c.key];
    assert(key !== undefined, `codec of ${label}`);
    const k = (json) => key.decode(json);
    if (c.kind === "set") {
      let s = c.setOf.length === 0 ? rt.emptySet() : rt.setOf(key.compare, ...c.setOf.map(k));
      for (const [step, x] of c.steps) {
        const before = s;
        s = step === "insert" ? rt.setInsert(key.compare, s, k(x)) : rt.setRemove(key.compare, s, k(x));
        assert(Object.isFrozen(s), `frozen after ${step}: ${label}`);
        assert(before !== s || rt.setContains(key.compare, s, k(x)) === (step === "insert"), label);
      }
      const list = rt.setToList(s);
      eq(JSON.stringify(list.map(key.encode)), JSON.stringify(c.toList), `toList: ${label}`);
      eq(rt.setSize(s), BigInt(c.toList.length), `setSize: ${label}`);
      for (const [x, expected] of c.contains) eq(rt.setContains(key.compare, s, k(x)), expected, `contains ${x}: ${label}`);
      // The wire form is the same array, and decoding it gives the same set.
      const decode = rt.decodeSet(key.decode, key.compare);
      assert(rt.equals(decode(c.toList), s), `wire: ${label}`);
    } else {
      let m = rt.emptyMap();
      for (const [step, x, v] of c.steps) {
        m = step === "put" ? rt.mapPut(key.compare, m, k(x), v) : rt.mapRemoveKey(key.compare, m, k(x));
        assert(Object.isFrozen(m) && m.every((pair) => Object.isFrozen(pair)), `frozen after ${step}: ${label}`);
      }
      eq(JSON.stringify(m.map(([x, v]) => [key.encode(x), v])), JSON.stringify(c.entries), `entries: ${label}`);
      eq(JSON.stringify(rt.mapKeys(m).map(key.encode)), JSON.stringify(c.entries.map((e) => e[0])), `keys: ${label}`);
      eq(JSON.stringify(rt.mapValues(m)), JSON.stringify(c.entries.map((e) => e[1])), `values: ${label}`);
      eq(rt.mapSize(m), BigInt(c.entries.length), `mapSize: ${label}`);
      for (const [x, expected] of c.lookup) {
        const found = rt.mapLookup(key.compare, m, k(x));
        eq(found === null ? null : found.some, expected, `lookup ${x}: ${label}`);
      }
      const decode = rt.decodeMap(key.decode, rt.decodeString, key.compare);
      assert(rt.equals(decode(c.entries), m), `wire: ${label}`);
    }
  }
});

test("collections are immutable: updates never change their input", () => {
  const s = rt.setOf(rt.compare, 3n, 1n);
  const t = rt.setInsert(rt.compare, s, 2n);
  eq(s.join(","), "1,3");
  eq(t.join(","), "1,2,3");
  eq(rt.setInsert(rt.compare, t, 2n), t, "inserting a present element returns the same set");
  eq(rt.setRemove(rt.compare, t, 9n), t, "removing an absent element returns the same set");
  assert(Object.isFrozen(rt.emptySet()) && Object.isFrozen(rt.emptyMap()), "empty collections frozen");
  const m = rt.mapPut(rt.compare, rt.emptyMap(), "a", 1n);
  const n = rt.mapPut(rt.compare, m, "a", 2n);
  eq(rt.mapLookup(rt.compare, m, "a").some, 1n);
  eq(rt.mapLookup(rt.compare, n, "a").some, 2n);
  eq(rt.mapRemoveKey(rt.compare, n, "b"), n, "removing an absent key returns the same map");
  eq(rt.mapLookup(rt.compare, rt.emptyMap(), "a"), null);
  // A lookup of an optional value keeps the Option layers apart: Some(None) is not None.
  const optional = rt.mapPut(rt.compare, rt.emptyMap(), "a", null);
  eq(rt.mapLookup(rt.compare, optional, "a").some, null);
  // Long literals: setFrom takes an array.
  const many = Array.from({ length: 5000 }, (_, i) => BigInt(4999 - i));
  const big = rt.setFrom(rt.compare, many.concat(many));
  eq(big.length, 5000);
  eq(big[0], 0n);
  eq(big[4999], 4999n);
});

test("Review Focus 1: Decimal keys compare by value (1.0 = 1.00, -0 = 0)", () => {
  const d = (text) => rt.Decimal.parse(text);
  const m = rt.decodeMap(rt.decodeDecimal, rt.decodeString, rt.decimalCompare)([["1.0", "a"], ["0.5", "b"]]);
  eq(rt.mapLookup(rt.decimalCompare, m, d("1.00")).some, "a", "lookup(m, 1.00) finds the entry written as 1.0");
  const put = rt.mapPut(rt.decimalCompare, m, d("1.000"), "c");
  eq(rt.mapKeys(put)[1].toString(), "1.0", "put keeps the existing key's text");
  eq(rt.mapLookup(rt.decimalCompare, put, d("1")).some, "c");
  throwsDecode(() => rt.decodeMap(rt.decodeDecimal, rt.decodeString, rt.decimalCompare)([["1.0", "a"], ["1.00", "b"]]), "1.0 and 1.00 are one key");
  throwsDecode(() => rt.decodeSet(rt.decodeDecimal, rt.decimalCompare)(["0", "-0"]), "0 and -0 are one element");
  eq(rt.setOf(rt.decimalCompare, d("1.00"), d("1.0")).map(String).join(","), "1.00", "setOf keeps the first");
  assert(rt.equals(rt.setOf(rt.decimalCompare, d("2.50")), rt.setOf(rt.decimalCompare, d("2.5"))), "equality by value");
});

test("Review Focus 3: enum keys follow declaration order, not the alphabet", () => {
  const z = COR("z");
  const a = COR("a");
  const m = COR("m");
  eq(rt.setOf(corOrder, m, a, z, a).map(encodeCor).join(","), "z,a,m");
  const map = rt.mapPut(corOrder, rt.mapPut(corOrder, rt.emptyMap(), m, 1n), z, 2n);
  eq(rt.mapKeys(map).map(encodeCor).join(","), "z,m");
  eq(rt.decodeSet(COR, corOrder)(["m", "a", "z"]).map(encodeCor).join(","), "z,a,m");
});

/** [decoder, encoder] of a vector type such as `Map<Codigo, Pagina<Ponto>>`. */
function codecOf(type) {
  let at = 0;
  const parse = () => {
    const name = /^[A-Za-z]+/.exec(type.slice(at))[0];
    at += name.length;
    const args = [];
    if (type[at] === "<") {
      at += 1;
      for (;;) {
        args.push(parse());
        if (type[at] === ",") at += 2;
        else break;
      }
      at += 1; // ">"
    }
    return { name, args };
  };
  const build = ({ name, args }) => {
    const key = KEYS[name];
    if (key !== undefined && args.length === 0) return { ...key };
    const [a, b] = args.map(build);
    switch (name) {
      case "Set":
        return { decode: rt.decodeSet(a.decode, a.compare), encode: (v) => v.map(a.encode) };
      case "Map":
        return { decode: rt.decodeMap(a.decode, b.decode, a.compare), encode: (v) => v.map(([k, x]) => [a.encode(k), b.encode(x)]) };
      case "Result":
        return {
          decode: rt.decodeResult(a.decode, b.decode),
          encode: (v) => (v.tag === "Ok" ? { Ok: a.encode(v.value) } : { Err: b.encode(v.value) }),
        };
      case "Ponto": {
        const int = KEYS.Int;
        return {
          decode: rt.decodeRecord([["x", int.decode, "u_x"], ["y", int.decode, "u_y"]]),
          encode: (v) => ({ x: int.encode(v.u_x), y: int.encode(v.u_y) }),
        };
      }
      case "Pagina":
        return {
          decode: rt.decodeRecord([["itens", rt.decodeList(a.decode), "u_itens"], ["total", rt.decodeInt, "u_total"]]),
          encode: (v) => ({ itens: v.u_itens.map(a.encode), total: v.u_total.toString() }),
        };
      case "Forma":
        return {
          decode: rt.decodeVariant([
            ["circulo", "Circulo", [["raio", rt.decodeInt, "u_raio"]]],
            ["retangulo", "Retangulo", [["largura", rt.decodeInt, "u_largura"], ["altura", rt.decodeInt, "u_altura"]]],
            ["vazia", "Vazia", null],
          ]),
          encode: (v) =>
            v.tag === "Circulo"
              ? { circulo: { raio: v.u_raio.toString() } }
              : v.tag === "Retangulo"
                ? { retangulo: { largura: v.u_largura.toString(), altura: v.u_altura.toString() } }
                : "vazia",
        };
      default:
        throw new Error(`no codec for ${name}`);
    }
  };
  return build(parse());
}

test("rich wire types match wire-rich.json (strict decoders, canonical encoders)", () => {
  const cases = readJson("../../vectors/wire-rich.json");
  assert(cases.length >= 50, "wire vectors");
  for (const c of cases) {
    const label = `${c.type} ${JSON.stringify(c.json)}`;
    const codec = codecOf(c.type);
    if (!c.valid) {
      throwsDecode(() => codec.decode(c.json), label);
      continue;
    }
    const value = codec.decode(c.json);
    eq(JSON.stringify(codec.encode(value)), JSON.stringify(c.canonical), label);
    assert(rt.equals(codec.decode(c.canonical), value), `canonical decodes to the same value: ${label}`);
  }
  // Kept strict on purpose (the Rust server is lenient here): a unit variant is a bare string.
  throwsDecode(() => codecOf("Forma").decode({ vazia: null }), '{"vazia": null}');
});

test("Review Focus 5: rpc answers with a malformed map or a duplicate set element are Request failed", async () => {
  for (const [text, decode] of [
    ['[["a"]]', rt.decodeMap(rt.decodeString, rt.decodeInt, rt.compare)],
    ['[["a", "1", "2"]]', rt.decodeMap(rt.decodeString, rt.decodeInt, rt.compare)],
    ['["1", "1"]', rt.decodeSet(rt.decodeInt, rt.compare)],
  ]) {
    await withFetch(respond(200, text), async () => {
      const error = await rejects(rt.rpc(rt.newCtx(), "/api/x", {}, decode), "Remote", text);
      eq(error.message, rt.TEXTS.requestFailed, text);
    });
  }
});

// ---- Runner ----

let failures = 0;
for (const { name, body } of tests) {
  try {
    await body();
    console.log(`ok   ${name}`);
  } catch (error) {
    failures += 1;
    console.log(`FAIL ${name}\n  ${error instanceof AssertionError ? error.message : error?.stack ?? error}`);
  }
}
console.log(`${tests.length - failures} passed; ${failures} failed`);
process.exitCode = failures === 0 ? 0 : 1;
