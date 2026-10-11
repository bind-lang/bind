// The JS side of the three-way differential test (src/backend/js/differential.rs):
// `node differential.mjs`, next to app.js, bind-runtime.js, dom.mjs and programs.json
// ([[name, result type], ...], or [line, result type, function, [arguments]] for a call of an
// exported function on arguments, the temporal calls whose arguments are wire texts). Runs
// each exported `p<i>(ctx)` (or `function(ctx, ...arguments)`) in order and prints
// `p<i>=<render>` in the canonical form of `Value::render` in bindc's `core::eval`, made
// line-safe (`\` -> `\\`, LF -> `\n`, CR -> `\r`):
//   Int -> decimal; Boolean -> true/false; String -> JSON string (only `"`, `\`, LF, CR, tab
//   and the other controls < U+0020 escaped, as `\u00xx`); Double/Float -> String(x)
//   (ECMAScript Number::toString); T? -> null or the value; List -> [a, b]; a constructor ->
//   Name or Name(field, ...) in field order; Unit -> (); Ui -> the HTML of its DOM nodes
//   (rt.render into the minimal DOM, serialized with the canonical escaping; the root is a
//   fragment). A Bind error prints `!<kind>`; any other exception `!Exception(...)`.
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

function show(value) {
  if (value === null) return "null";
  switch (typeof value) {
    case "bigint": return value.toString();
    case "boolean": return String(value);
    case "number": return String(value);
    case "string": return jsonString(value);
    case "undefined": return "()";
    default: break;
  }
  if (Array.isArray(value)) return `[${value.map(show).join(", ")}]`;
  if (Object.hasOwn(value, "some") && Object.keys(value).length === 1) return show(value.some);
  if (Object.hasOwn(value, "tag")) {
    const fields = Object.keys(value).filter((key) => key !== "tag");
    if (fields.length === 0) return value.tag;
    return `${value.tag}(${fields.map((key) => show(value[key])).join(", ")})`;
  }
  throw new Error(`no canonical form for ${JSON.stringify(value)}`);
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
for (const [name, type, callee, args] of programs) {
  let line;
  try {
    const value = await app[callee ?? name](rt.newCtx(), ...(args ?? []));
    line = type === "Ui" ? html(value) : show(value);
  } catch (error) {
    line = error instanceof rt.BindError ? `!${error.kind}` : `!Exception(${error?.stack ?? error})`;
  }
  lines.push(`${name}=${lineSafe(line)}`);
}
process.stdout.write(`${lines.join("\n")}\n`);
