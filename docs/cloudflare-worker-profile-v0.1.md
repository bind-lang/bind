# Cloudflare Worker WebAssembly profile v0.1

`cloudflare-worker-wasm-v1` is Bind's first compiler-owned low-level backend. The compiler consumes the validated `CoreProgram`, lowers accepted server computation directly to WebAssembly Binary Format, and writes `app.wasm` itself. It does not invoke or generate Rust, LLVM IR, WAT, `wasm-bindgen`, Cargo, or another Wasm encoder.

The generated `worker.js` is intentionally a host adapter, not the Bind computation backend. It owns Fetch, strict JSON decoding and encoding, CORS, errors, and Cloudflare bindings. Browser assets use the existing typed JavaScript frontend backend.

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
- finite numeric and Boolean literals;
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

`bind certify` verifies the whole application, generates the project, asks Node to parse every JavaScript module, validates `app.wasm` with the WebAssembly engine, hashes every deployable artifact, and records both the Wasm and JavaScript runtime hashes in the target lock and manifest. It does not invoke Cargo or rustc.

The output is deployable with:

```sh
npx wrangler deploy
```

## Next semantic slices

The next backend work should expand semantics, not merely opcodes: arbitrary-precision `Int` and its memory ABI, strings and linear memory, algebraic data layouts, exhaustive pattern matching, then an async typed host ABI for D1/Hyperdrive/KV. Each slice becomes available only after differential tests show agreement with the reference evaluator.
