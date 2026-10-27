# Cloudflare Worker WebAssembly profile v0.1

`cloudflare-worker-wasm-v1` is Bind's first compiler-owned low-level backend. The compiler consumes the validated `CoreProgram`, lowers accepted server computation directly to WebAssembly Binary Format, and writes `app.wasm` itself. It does not invoke or generate Rust, LLVM IR, WAT, `wasm-bindgen`, Cargo, or another Wasm encoder.

The generated `worker.js` is intentionally a host adapter, not the Bind computation backend. It owns Fetch, strict JSON decoding and encoding, CORS, errors, and Cloudflare bindings. Browser assets use the existing typed JavaScript frontend backend.

## HTTP contract

The Worker answers `/api/<endpoint>`, where `<endpoint>` is the percent-decoded path segment (a malformed escape is `400`, an unknown endpoint `404`, decided before the body is read). Then:

- only `POST` runs an endpoint (`405` with `Allow: POST, OPTIONS` otherwise; `OPTIONS` answers CORS preflights);
- `Content-Type` must be `application/json`, parameters allowed, compared case-insensitively (`415` otherwise);
- without a configured `frontend_origin`, a present `Origin` header must equal the Worker's own origin (`403` otherwise); with one, only that origin is accepted and answered with CORS headers;
- the body is read up to 1 MiB whatever `Content-Length` claims (`413` beyond it), must be strict UTF-8 JSON, and numbers must be finite (`1e400` is `400`, as serde_json rejects it);
- every API response carries `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer` and `Cache-Control: no-store`. Static assets get a `_headers` file with a `default-src 'self'` CSP (the split page's inline boot script is allowed by its hash), `frame-ancestors 'none'`, `X-Frame-Options: DENY` and `nosniff`.

## Target

```toml
[target.cloudflare]
client = "web"
server = "wasm"
mode = "split"
deployment = "cloudflare-workers"
certified = true
capabilities = ["ui.dom", "server.http"]
```

The compatibility date is pinned by the profile. The Worker handles `/api` and `/api/*`; other requests go to the `ASSETS` binding. Wrangler bundles the precompiled `app.wasm` module imported by `worker.js`.

## Compile-time contract

This profile accepts a closed subset whose value semantics map exactly to Wasm v1:

- `Boolean` (`i32`), `Float` (`f32`), `Double` (`f64`), and zero-width `Unit`;
- Boolean literals, and numeric literals that stay finite in their type: a decimal literal is rounded to the nearest `f32`/`f64` exactly as the reference evaluator rounds it (`16777217f` is `16777216`), and one that rounds to infinity is rejected;
- locals, `val`, sequencing, `if`/`else`, and explicit returns;
- direct calls between monomorphic Bind functions;
- Boolean operators, IEEE Float/Double arithmetic, and supported comparisons;
- pure, non-recursive RPC endpoint call graphs.

The following fail during `bind check`, before any target artifact is written:

- `Int`, because Bind `Int` is arbitrary precision and mapping it to `i32` or `i64` would change the language;
- strings, decimal values, collections, options, records, variants, field access, pattern matching, closures, and polymorphism;
- recursion, until the backend can enforce Bind's recursion limit before the Wasm host stack is exhausted;
- database operations, environment reads, UI values, effects, and body-less functions reachable from endpoints;
- `mode = "ssr"` and endpoint types outside the exact ABI.

This rejection boundary is part of the backend. Generation returns an internal contract error if it ever receives a program that verification should have rejected; it never substitutes JavaScript computation or a weaker numeric representation.

## Generated architecture

```text
Bind source
    ↓ parser + types + effects + whole-application verification
CoreProgram
    ├── Bind Wasm backend  → app.wasm   (server computation)
    ├── Worker host        → worker.js  (Fetch, JSON, Cloudflare APIs)
    └── browser backend    → public/*   (HTML/CSS/JS)
```

Only the final Cloudflare/V8 step translates portable Wasm instructions to physical machine instructions. Bind owns the language-to-Wasm lowering, function ABI, instruction selection, module layout, export table, and binary encoding.

## Certification

`bind certify` verifies the whole application, generates the project, asks Node to parse every JavaScript module, validates `app.wasm` with the WebAssembly engine, hashes every deployable artifact, records the Wasm and every artifact hash in the manifest, and pins the JavaScript runtime hash in the target lock (`.bind/locks/<target>.lock`), so editing the program does not invalidate the lock. It does not invoke Cargo or rustc.

The output is deployable with:

```sh
npx wrangler deploy
```

## Next semantic slices

The next backend work should expand semantics, not merely opcodes: arbitrary-precision `Int` and its memory ABI, strings and linear memory, algebraic data layouts, exhaustive pattern matching, then an async typed host ABI for D1/Hyperdrive/KV. Each slice becomes available only after differential tests show agreement with the reference evaluator.
