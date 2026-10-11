// Runs generated `app.js` modules (tests/codegen/web_*.bd) in node: `node check.mjs <case>`,
// next to app.js, bind-runtime.js and dom.mjs. `fetch` is replaced by a fake server that
// checks each request and answers with the JSON the generated Rust server would send.
import * as rt from "./bind-runtime.js";
import * as app from "./app.js";
import { createDocument, serialize } from "./dom.mjs";

function eq(actual, expected, message) {
  if (!Object.is(actual, expected)) {
    throw new Error(`${message}\n  expected: ${String(expected)}\n  actual:   ${String(actual)}`);
  }
}

/** `fetch` answering `routes[url]` (a function of the parsed request body giving the response text). */
function serve(routes) {
  globalThis.fetch = async (url, init) => {
    const route = routes[url];
    if (route === undefined) throw new Error(`unexpected request to ${url}`);
    eq(init.method, "POST", "method");
    return new Response(route(JSON.parse(init.body), init.body), { status: 200 });
  };
}

function html(tree) {
  return rt.render(tree, createDocument()).map(serialize).join("");
}

/** The texts of the root's children (each child is a text node). */
function texts(tree) {
  eq(tree.tag, "UI", "root");
  return tree.children.map((child) => child.text).join("|");
}

const checks = {
  async names() {
    const before = Object.getOwnPropertyNames(Object.prototype).join(",");
    serve({
      "/api/perigo": (body, raw) => {
        eq(raw, '{"x":"5"}', "request body");
        return '{"__proto__": "x", "constructor": "y", "toString": "z"}';
      },
    });
    const tree = await app.Nomes(rt.newCtx(), 5n);
    eq(texts(tree), "x|y|z|6|3", "decoded fields and calls of class/await/default/Object/enter");
    eq({}.x, undefined, "no prototype pollution");
    eq({}.constructor, Object, "Object.prototype.constructor untouched");
    eq(typeof {}.toString, "function", "Object.prototype.toString untouched");
    eq(Object.getOwnPropertyNames(Object.prototype).join(","), before, "Object.prototype unchanged");
  },

  async adt() {
    serve({
      "/api/arvore": (body, raw) => {
        eq(raw, '{"n":"5"}', "arvore request");
        return '{"no": {"esq": "folha", "valor": "5", "dir": "folha"}}';
      },
      "/api/par": (body, raw) => {
        eq(raw, '{"p":{"tag":"t","valor":null},"c":"5"}', "par request");
        return '{"Ok": {"tag": "t", "valor": null}}';
      },
    });
    const tree = await app.Arvores(rt.newCtx(), 5n);
    eq(texts(tree), '5|5|5|10|0|true|outro|menos "um"|dezesseis|0', "sum types, Result, distinct, patterns and early return");
  },

  async generic() {
    serve({
      "/api/eco": (body, raw) => {
        eq(raw, '{"x":"1","xs":["1",null]}', "Int? and List<Int?> on the wire");
        return '["5", null]';
      },
    });
    const tree = await app.Genericos(rt.newCtx(), 4n);
    eq(texts(tree), "9|7|1|0|4|2|5|9", "uniform options through generic code and the wire");
  },

  async lambdas() {
    const tree = await app.Listas(rt.newCtx(), 3n);
    eq(texts(tree), "16|1|-2|7|6|3", "map, fold, builtins as values, nested options");
  },

  async arith() {
    const float = String(Math.fround(Math.fround(0.25 * Math.fround(0.1)) + 1));
    let tree = await app.Contas(rt.newCtx(), 7n, 2n);
    eq(texts(tree), `3|864197523086419752308641975231|1e+21|${float}|true|false`, "Int, Double, Float, comparisons");
    tree = await app.Contas(rt.newCtx(), -7n, 0n);
    eq(texts(tree).split("|")[0], "-7", "division by zero falls back");
    tree = await app.Contas(rt.newCtx(), -7n, 2n);
    eq(texts(tree).split("|")[0], "-3", "division truncates toward zero");
  },

  async ui() {
    const tree = await app.Pagina(rt.newCtx(), "A & B", true, rt.Decimal.parse("1.50"), 3n);
    eq(
      html(tree),
      '<section class="hero" id="top"><h2 class="title">Oi A &amp; B</h2>true31.501.50.10000000149011612' +
        "A &amp; B&lt;script&gt;alert(1)&lt;/script&gt;&lt;yes&gt;" +
        '<span class="card">A &amp; B</span>!<strong>3</strong>' +
        '<a href="https://x" class="c" target="_blank" rel="noopener noreferrer">10000000000000000000000</a></section>',
      "rendered page",
    );
    const empty = await app.Pagina(rt.newCtx(), "x", false, rt.Decimal.parse("0"), 0n);
    const page = html(empty);
    if (!page.includes('<hr class="sep">') || !page.includes("vazio")) throw new Error(`else branches: ${page}`);
  },
};

const name = process.argv[2];
await checks[name]();
console.log(`ok ${name}`);
