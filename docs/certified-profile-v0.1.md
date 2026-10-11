# Bind certified profile v0.1

Profile identifier: `rust-elm-web-v1`.

> Nota: este perfil foi substituído pelo [v0.2](certified-profile-v0.2.md) (`rust-elm-web-v2`, contrato `bind-json-v2`, servidor gerado da IR tipada). Mantido como referência histórica.

This document defines the finite surface for which `bind certify` may issue an attestation. It is intentionally smaller than the long-term RFC. Unsupported semantics are compilation errors; the compiler never substitutes weaker behavior.

## Execution model

| Layer | Certified lowering |
|---|---|
| Source | `.bind` and `.bd` |
| Browser | Elm 0.19.2 |
| Protocol | JSON, `bind-json-v1`, one POST endpoint per server function |
| Server | safe Rust 1.98, Tokio, Serde |
| Database | PostgreSQL through SQLx and a compiler-verified schema snapshot |
| AWS | Lambda custom runtime `provided.al2023`, arm64, AWS SAM + Cargo Lambda |
| Vercel | one official Rust Function per RPC plus an optimized static Elm output |

The two adapters call the same generated Rust library. Domain types, database queries, error mapping, and serialization are not separately reimplemented by each deployment target.

## Accepted application shape

The v0.1 certified profile accepts:

- data classes, entities, enums, sealed classes, and nominal distinct types;
- scalar values, nullable values, and lists using the profile's portable type mapping;
- server functions with a declared return and declared effects;
- `require`, `find`, and `insert` database operations produced from verified IR;
- one database operation per server function;
- one client component and one `UI` block, either fully static or with one bound server call;
- text inputs for portable textual identifiers and scalar values;
- a finite static UI tree (`Header`, `Nav`, `Main`, `Section`, headings, text, links,
  lists, code, and semantic containers) containing literal values;
- `Text(...)` result nodes containing literals or fields of a bound server result;
- public and runtime environment declarations, with secret flow analysis.

The restriction to one operation and one component is a release boundary, not a parser limitation. It keeps generated behavior total and reviewable while the UI and query IRs grow.

## Rejected surface

Certification fails for raw SQL, arbitrary target code, undeclared external calls, user-selected target packages, unsupported UI nodes, ambiguous control flow, unsupported numeric representations, multiple server calls in the component, or target capabilities that cannot preserve the source semantics.

There is no fallback generator. A target outside Rust + Elm on AWS Lambda or Vercel is rejected before code generation.

## Certification pipeline

`bind certify` performs the following gates in order:

1. Kotlin-compatible lexical analysis and Bind parsing.
2. Whole-application type, effect, environment, schema, RPC, and capability verification.
3. Rust and Elm generation from verified IR only.
4. Exact Cargo resolution in a generated `Cargo.lock`.
5. Generated Rust tests and all-target compilation.
6. Clippy with every warning promoted to an error.
7. Release compilation of every generated Rust entry point.
8. Optimized Elm compilation.
9. For Vercel, exact npm resolution and `npm ci` followed by the same build command declared to Vercel.
10. SHA-256 hashes for sources, schema, dependency manifests, locks, and artifacts.

The root `bind.lock` records the compiler-owned toolchain and dependency policy for all declared targets. `.bind/locks/<target>.lock` records the concrete target resolution hashes. `bind-manifest.json` records the source/schema fingerprints and output hashes.

## Meaning of certification

`certified-host` means that all gates above passed on the machine that invoked Bind. It does not claim that a macOS binary is an AWS artifact. AWS SAM/Cargo Lambda performs the final Linux arm64 build; Vercel's builder performs its own Linux build from the generated, locked Rust source.

Certification cannot prove runtime facts such as credentials, database reachability, network availability, data-dependent constraint conflicts, or provider uptime. Those remain typed runtime errors or deployment checks.
