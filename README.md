# Bind

Bind is a new whole-application language and compiler, not a wrapper around a handwritten React or NestJS project. A `.bind` or `.bd` source file is parsed into a typed application graph, verified across client, server, environment, and PostgreSQL boundaries, and only then lowered to a certified execution profile.

> If it does not bind, it does not build.

Repository: <https://github.com/bind-lang/bind>

## Certified v0.1 profile

The first deliberately narrow profile is:

```text
Bind source (.bind or .bd)
        │
        ├── Rust server + PostgreSQL + bind-json-v1 RPC
        │       ├── AWS Lambda / provided.al2023
        │       └── Vercel Rust Functions
        │
        └── Elm browser application
```

The compiler owns all generated dependencies and pins them. Certified applications cannot add arbitrary target dependencies, raw SQL, target code, or untyped network calls. Cargo, npm, and Elm remain the underlying resolvers, while `bind.lock`, target-specific locks, hashes, and generated contract tests form Bind's reproducibility boundary. A separate package manager is therefore not needed for this profile.

Vercel's Rust runtime is currently Beta. Bind treats that as a deployment-platform limitation, not as a silently stronger guarantee.

## Quick start

Required tools are Rust 1.98.0 and Elm 0.19.2. Vercel builds also use npm.

```sh
cargo test
DATABASE_URL=postgresql://bind:bind@localhost/bind cargo run -- check --target aws-lambda
DATABASE_URL=postgresql://bind:bind@localhost/bind cargo run -- certify --target aws-lambda
DATABASE_URL=postgresql://bind:bind@localhost/bind cargo run -- certify --target vercel
```

`certify` performs semantic verification, generates target code, resolves locked dependency graphs, runs generated Rust tests, rejects every Rust warning through Clippy, compiles optimized Elm, builds release binaries, and hashes the result.

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
bind check [--target aws-lambda|vercel]
bind verify [--target aws-lambda|vercel]
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
cd .bind/generated/aws-lambda
sam build --beta-features
sam deploy --guided
```

Vercel generation includes root Rust functions under `api/`, pinned npm/Elm inputs, `vercel.json`, and the optimized static frontend:

```sh
cd .bind/generated/vercel
vercel pull --yes --environment preview
vercel build
vercel deploy
```

Neither command is run automatically because both require the user's cloud account and may create remote resources.

## What v0.1 proves

- Hindley-Milner type inference in the OCaml model, with algebraic data types, exhaustive `when`, no exceptions and no mutation (see [type system](docs/type-system.md));
- nominal and nullable types across Bind, Rust, JSON, Elm, and PostgreSQL;
- declared function returns and argument compatibility;
- effect declarations and client/server domain rules;
- typed environment declarations and secret-to-client rejection;
- database table, field, type, nullability, and required-insert checks against a deterministic schema snapshot;
- compiler-owned RPC paths, strict request decoding, response codecs, and generated malformed-contract tests;
- target capability negotiation before generation;
- deterministic SHA-256 source, schema, lockfile, and artifact attestations.

No compiler can prove database availability, future uniqueness conflicts, network availability, or a cloud provider's uptime. `certified-host` in the manifest means the generated project passed the local certified toolchain; the cloud-native builders still produce the final Linux deployment artifact.

The exact supported subset and rejection policy are specified in the [certified profile](docs/certified-profile-v0.1.md). The implementation architecture is described in [architecture](docs/architecture.md).
