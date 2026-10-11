# Bind

Bind is a new whole-application language and compiler, not a wrapper around a handwritten React or NestJS project. A `.bind` or `.bd` source file is parsed into a typed application graph, verified across client, server, environment, and PostgreSQL boundaries, and only then lowered to a certified execution profile.

> If it does not bind, it does not build.

Repository: <https://github.com/bind-lang/bind>

## Certified profiles

The current profile is `rust-web-v3` ([certified profile v0.3](docs/certified-profile-v0.3.md)): the type checker lowers the program to a typed core IR, and the Rust server, the Rust SSR renderer and the native HTML/CSS/JS frontend are all generated from that IR alone. The Elm client is gone. A target chooses `mode = "ssr"` (the Rust server renders the page, one transaction per page) or `mode = "split"` (static `web/` calling the API, one transaction per RPC), on AWS Lambda or Vercel. `Int` is exact in the frontend (`BigInt`), the wire contract is `bind-json-v3`, and overflow or deep recursion become typed errors instead of panics. Previous profiles are kept in docs: [v0.2](docs/certified-profile-v0.2.md) (`rust-elm-web-v2`) and [v0.1](docs/certified-profile-v0.1.md) (`rust-elm-web-v1`).

The overall shape is:

```text
Bind source (.bind or .bd)
        │
        ├── Rust server + PostgreSQL + bind-json-v3 RPC
        │       ├── AWS Lambda / provided.al2023
        │       └── Vercel Rust Functions
        │
        ├── Rust SSR (mode = "ssr")
        │
        └── HTML/CSS/JS frontend, native ES modules (mode = "split")
```

### Migrating a `bind.toml` from the Elm client

`client = "elm"` is rejected (E8311). In each target, change it to `client = "web"` and add the required `mode`:

```toml
[target.vercel]
client = "web"       # was "elm"
server = "rust"
mode = "split"       # or "ssr"; required
deployment = "vercel"
certified = true
capabilities = ["ui.dom"]
# api_url = "/api"                            # default; must end in /api
# frontend_origin = "https://app.example.com" # required when api_url is on another origin
```

Then regenerate the deploy directory (`bind generate --target <target>`); the Elm and npm files disappear. A program may have only one `client component` per web target (E8301).

The compiler owns all generated dependencies and pins them. Certified applications cannot add arbitrary target dependencies, raw SQL, target code, or untyped network calls. Cargo remains the underlying resolver, while `bind.lock`, target-specific locks, hashes, and generated contract tests form Bind's reproducibility boundary. A separate package manager is therefore not needed for this profile.

Vercel's Rust runtime is currently Beta. Bind treats that as a deployment-platform limitation, not as a silently stronger guarantee.

## Quick start

Required tool is Rust 1.98.0. The generated frontend needs no npm, bundler or Elm; `node` is only used by some tests.

```sh
cargo test
DATABASE_URL=postgresql://bind:bind@localhost/bind cargo run -- check --target aws-lambda-ssr
DATABASE_URL=postgresql://bind:bind@localhost/bind cargo run -- certify --target aws-lambda-split
DATABASE_URL=postgresql://bind:bind@localhost/bind cargo run -- certify --target vercel-ssr
```

`certify` performs semantic verification, generates target code, resolves locked dependency graphs, runs generated Rust tests, rejects every Rust warning through Clippy, builds release binaries, and hashes the result.

Generated workspaces are placed under `.bind/generated/<target>/`; target resolution attestations are placed under `.bind/locks/`.

## A Bind program

```kotlin
distinct type CustomerId = UUID

entity Customer(
    val id: CustomerId,
    val name: String,
    val email: Email
)

server fun getCustomer(id: CustomerId): Customer
    effects [db.read] {
    return db.customers.require(id)
}

client component CustomerPage(id: CustomerId) {
    val customer = server.getCustomer(id)

    UI {
        Text(customer.name)
        Text(customer.email)
    }
}
```

The Kotlin-compatible promise applies to lexical syntax: identifiers, keywords, literals, comments, whitespace, and operators. Bind has its own grammar and semantics. See [Kotlin lexical compatibility](docs/kotlin-lexical-compatibility.md).

## Commands

```sh
bind check [--target <target>]
bind verify [--target <target>]
bind generate --target <target> [--emit-ir] [--emit-graph]
bind build --target <target> [--emit-ir] [--emit-graph]
bind certify --target <target> [--emit-ir] [--emit-graph]
bind compile --target <target>   # alias for build
bind graph <symbol>
bind impact <resource>
bind db inspect
bind target list
bind target inspect <name>
```

## Native deployment outputs

AWS Lambda generation includes a SAM template configured for Cargo Lambda, `provided.al2023`, arm64, API Gateway, and the `DATABASE_URL` contract:

```sh
cd .bind/generated/aws-lambda-ssr   # or the name of your target
sam build --beta-features
sam deploy --guided
```

Vercel generation includes root Rust functions under `api/`, `vercel.json`, and the static frontend in `web/` (in `ssr` mode `/` is rewritten to the `bind-page` function):

```sh
cd .bind/generated/vercel-split   # or the name of your target
vercel pull --yes --environment preview
vercel build
vercel deploy
```

Neither command is run automatically because both require the user's cloud account and may create remote resources.

## What the certified profile proves

- Hindley-Milner type inference in the OCaml model, with algebraic data types, exhaustive `when`, no exceptions and no mutation (see [type system](docs/type-system.md));
- nominal and nullable types across Bind, Rust, JSON, JavaScript, and PostgreSQL;
- declared function returns and argument compatibility;
- effect declarations and client/server domain rules;
- effects (`db.read`, `db.write`, `server.call`) and deployment domains in the type system, with exact `effects [...]` contracts (see [type system](docs/type-system.md));
- typed environment declarations and secret-to-client rejection;
- database table, field, type, nullability, and required-insert checks against a deterministic schema snapshot;
- compiler-owned RPC paths (`bind-json-v3`), strict request decoding, response codecs, and generated malformed-contract tests;
- target capability negotiation before generation;
- deterministic SHA-256 source, schema, lockfile, and artifact attestations.

No compiler can prove database availability, future uniqueness conflicts, network availability, or a cloud provider's uptime. `certified-host` in the manifest means the generated project passed the local certified toolchain; the cloud-native builders still produce the final Linux deployment artifact.

The exact supported subset and rejection policy are specified in the [certified profile v0.3](docs/certified-profile-v0.3.md) (previous: [v0.2](docs/certified-profile-v0.2.md), [v0.1](docs/certified-profile-v0.1.md)). The implementation architecture is described in [architecture](docs/architecture.md).
