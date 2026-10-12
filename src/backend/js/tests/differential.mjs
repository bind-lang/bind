// The JS side of the three-way differential test (src/backend/js/differential.rs):
// `node differential.mjs`, next to app.js, bind-runtime.js, dom.mjs, types.json (the program's
// declarations: name -> {kind: "record", params, fields: [[property, type]]} |
// {kind: "sum", params, variants: {tag: fields}} | {kind: "distinct", underlying}) and
// programs.json ([[name, result type], ...], or [line, result type, function, [arguments],
// "text" | "decimal"] for a call of an exported function on arguments read from their wire
// texts: the temporal and Decimal calls). A type is [name, ...arguments]; a declaration's
// parameter is its 'P marker. Runs each exported `p<i>(ctx)` (or `function(ctx, ...arguments)`)
// in order and prints `p<i>=<render>` in the canonical form of `Value::render` in bindc's
// `core::eval`, made line-safe (`\` -> `\\`, LF -> `\n`, CR -> `\r`), driven by the type (a Set
// and a List are both arrays, a distinct value is its underlying value):
//   Int -> decimal; Boolean -> true/false; String and the other text types -> JSON string
//   (only `"`, `\`, LF, CR, tab and the other controls < U+0020 escaped, as `\u00xx`);
//   Double/Float -> String(x) (ECMAScript Number::toString); Decimal/Money -> its text;
//   T? -> null or the value; List -> [a, b]; Set -> Set[a, b]; Map -> Map[k: v, ...]; a
//   record or variant -> Name or Name(field, ...) in field order; Result -> Ok(v) / Err(e); a
//   distinct value -> Name(value); Unit -> (); Ui -> the HTML of its DOM nodes (rt.render into
//   the minimal DOM, serialized with the canonical escaping; the root is a fragment). A Bind
//   error prints `!<kind>`; any other exception `!Exception(...)`.
import { readFileSync } from "node:fs";
import * as rt from "./bind-runtime.js";
import * as app from "./app.js";
import { createDocument, serialize } from "./dom.mjs";

function jsonString(text) {
  let out = '"';
  for (const c of text) {
    switch (c) {
      case '"': out += '\\"'; break;
      case "\\": out += "\\\\"; break;
      case "\n": out += "\\n"; break;
      case "\r": out += "\\r"; break;
      case "\t": out += "\\t"; break;
      default: {
        const code = c.codePointAt(0);
        out += code < 0x20 ? `\\u${code.toString(16).padStart(4, "0")}` : c;
      }
    }
  }
  return `${out}"`;
}

const types = JSON.parse(readFileSync("types.json", "utf8"));

/** `type` with the declaration parameters of `env` (marker -> type) replaced. */
function substitute(type, env) {
  const [name, ...args] = type;
  if (args.length === 0 && Object.hasOwn(env, name)) return env[name];
  return [name, ...args.map((arg) => substitute(arg, env))];
}

function constructor(name, value, fields, env) {
  if (fields.length === 0) return name;
  return `${name}(${fields.map(([property, type]) => show(value[property], substitute(type, env))).join(", ")})`;
}

function show(value, type) {
  const [name, ...args] = type;
  switch (name) {
    case "Int": case "Long": return value.toString();
    case "Boolean": return String(value);
    case "Double": case "Float": return String(value);
    case "Decimal": case "Money": return rt.displayDecimal(value);
    case "Unit": return "()";
    case "Option": return value === null ? "null" : show(value.some, args[0]);
    case "List": return `[${value.map((item) => show(item, args[0])).join(", ")}]`;
    case "Set": return `Set[${value.map((item) => show(item, args[0])).join(", ")}]`;
    case "Map": return `Map[${value.map(([key, item]) => `${show(key, args[0])}: ${show(item, args[1])}`).join(", ")}]`;
    case "Result": return `${value.tag}(${show(value.value, value.tag === "Ok" ? args[0] : args[1])})`;
    case "Ui": return html(value);
    default: break;
  }
  const declaration = types[name];
  if (declaration === undefined) {
    if (typeof value === "string") return jsonString(value);
    throw new Error(`no canonical form for ${JSON.stringify(type)}`);
  }
  if (declaration.kind === "distinct") return `${name}(${show(value, declaration.underlying)})`;
  const env = Object.fromEntries(declaration.params.map((param, i) => [param, args[i]]));
  if (declaration.kind === "record") return constructor(name, value, declaration.fields, env);
  return constructor(value.tag, value, declaration.variants[value.tag], env);
}

function html(tree) {
  const document = createDocument();
  const out = rt.render(tree, document).map(serialize).join("");
  if (document.htmlApiUsed.length > 0) throw new Error(`HTML string APIs: ${document.htmlApiUsed}`);
  return out;
}

function lineSafe(text) {
  return text.replace(/[\\\n\r]/g, (c) => (c === "\\" ? "\\\\" : c === "\n" ? "\\n" : "\\r"));
}

const programs = JSON.parse(readFileSync("programs.json", "utf8"));
const lines = [];
for (const [name, type, callee, args, kind] of programs) {
  let line;
  try {
    const values = (args ?? []).map((text) => (kind === "decimal" ? rt.Decimal.parse(text) : text));
    const value = await app[callee ?? name](rt.newCtx(), ...values);
    line = show(value, type);
  } catch (error) {
    line = error instanceof rt.BindError ? `!${error.kind}` : `!Exception(${error?.stack ?? error})`;
  }
  lines.push(`${name}=${lineSafe(line)}`);
}
process.stdout.write(`${lines.join("\n")}\n`);
