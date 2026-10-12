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

/** Runs `call`, which must fail with BindError("Remote", "Request failed"). */
async function requestFails(call, message) {
  try {
    await call();
  } catch (error) {
    if (!(error instanceof rt.BindError)) throw new Error(`${message}: expected BindError, got ${error}`);
    eq(error.kind, "Remote", message);
    eq(error.message, rt.TEXTS.requestFailed, message);
    return;
  }
  throw new Error(`${message}: expected Request failed, nothing thrown`);
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
  async collections() {
    // Review Focus 1 and 3: the request carries the map with one key (put keeps the first key,
    // replaces the value) and the set in declaration order; the answer comes in any order.
    let precos = '[["2", "b"], ["1.0", "a"]]';
    let cores = '["m", "a", "z"]';
    serve({
      "/api/precos": (body, raw) => {
        eq(raw, '{"m":[["1.00","y"]]}', "Map<Decimal, String> request");
        return precos;
      },
      "/api/ecoCores": (body, raw) => {
        eq(raw, '{"s":["z","a","m"]}', "Set<Cor> request in declaration order");
        return cores;
      },
    });
    const run = () => app.Colecoes(rt.newCtx(), rt.Decimal.parse("1.00"), 1n);
    eq(texts(await run()), "a|2|zam|zam|zm|1020305|true", "lookup(m, 1.00) finds 1.0; declaration order");
    // Review Focus 5: duplicates by value and malformed pairs or sets are Request failed.
    for (const answer of ['[["1.0", "a"], ["1.00", "b"]]', '[["a"]]', '[["1", "a", "b"]]', '[[]]', '{"1": "a"}']) {
      precos = answer;
      await requestFails(run, `precos answering ${answer}`);
    }
    precos = "[]";
    for (const answer of ['["z", "z"]', '["Z"]', '"z"']) {
      cores = answer;
      await requestFails(run, `ecoCores answering ${answer}`);
    }
  },

  async rich() {
    // Review Focus 4: Pagina<Customer> and Pagina<Int> in one signature, each with its codec.
    const customers = '{"itens": [{"id": "1", "name": "B"}, {"id": "2", "name": "C"}], "total": "9"}';
    const ints = '{"itens": ["4", "5"], "total": "2"}';
    let agrupa = customers;
    let contagem = ints;
    let resultado = '{"Ok": ["3", "1"]}';
    let evento = '{"criado": {"em": "2025-02-02T00:00:00Z", "por": {"id": "1", "name": "Bia"}}}';
    serve({
      "/api/agrupa": (body, raw) => {
        eq(
          raw,
          '{"paginas":[["7",{"itens":[{"id":"7","name":"Ana"}],"total":"1"}]],"numeros":{"itens":["7","2"],"total":"2"}}',
          "agrupa request",
        );
        return agrupa;
      },
      "/api/contagem": (body, raw) => {
        eq(raw, '{"numeros":{"itens":["7"],"total":"1"}}', "contagem request");
        return contagem;
      },
      "/api/resultado": (body, raw) => {
        eq(raw, '{"x":"7"}', "resultado request");
        return resultado;
      },
      "/api/evento": (body, raw) => {
        eq(raw, '{"e":{"criado":{"em":"2024-01-01T10:00:00Z","por":{"id":"7","name":"Ana"}}}}', "evento request");
        return evento;
      },
    });
    const run = () => app.Paginas(rt.newCtx(), 7n, "2024-01-01T10:00:00Z");
    eq(texts(await run()), "2|9|405|103|Bia|2025-02-02T00:00:00Z", "rich wire types");
    resultado = '{"Err": "negativo"}';
    evento = '"nenhum"';
    eq(texts(await run()), "2|9|405|-1|nada|2024-01-01T10:00:00Z", "Err and a unit variant");
    evento = '{"removido": {"motivo": "x"}}';
    eq(texts(await run()).split("|")[4], "-x", "a variant with fields");
    // Each instance rejects the other's JSON.
    agrupa = ints;
    await requestFails(run, "Pagina<Customer> answered with a Pagina<Int>");
    agrupa = customers;
    contagem = customers;
    await requestFails(run, "Pagina<Int> answered with a Pagina<Customer>");
    contagem = ints;
    for (const answer of ['{"removido": {"motivo": "x", "extra": "1"}}', '{"nenhum": null}', '"criado"', '{"apagado": {}}']) {
      evento = answer;
      await requestFails(run, `evento answering ${answer}`);
    }
    evento = '"nenhum"';
    for (const answer of ['{"Ok": ["1", "1"]}', '{"ok": ["1"]}', '{"Ok": [], "Err": "x"}']) {
      resultado = answer;
      await requestFails(run, `resultado answering ${answer}`);
    }
  },
};

const name = process.argv[2];
await checks[name]();
console.log(`ok ${name}`);
