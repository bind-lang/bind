# Compiler architecture

```text
source files
    ↓
Kotlin-compatible lexer → Bind parser → typed AST
    ↓
whole-application verification
    ├── types and nominal identity
    ├── effects and deployment domains
    ├── environment and secret flows
    ├── PostgreSQL schema and query IR
    └── RPC graph and target capabilities
    ↓
VerifiedApplication
    ├── Rust core ──→ AWS Lambda adapter
    │            └─→ Vercel Rust adapters
    ├── bind-json-v1 schema and vectors
    └── Elm client and codecs
```

Generation receives `VerifiedApplication`, not raw source. If verification emits an error, no target generator is called.

The compiler-owned Rust library is the semantic center of both deployment targets. It defines domain representations, strict request structures, SQL generated from database IR, row decoding, status mapping, and JSON responses. AWS and Vercel adapters are thin transport layers.

The Elm generator owns the corresponding JSON decoders and request encoders. A response that violates the contract becomes an explicit `Http.BadBody` state instead of being accepted as an unchecked value.

## Dependency policy

The certified profile does not expose a general package declaration. Bind owns exact direct dependency versions, Cargo produces the complete transitive lock, and Vercel's frontend build uses a committed generated npm lock with `npm ci`. This provides a smaller and stronger supply-chain boundary than creating a new resolver while still relying on mature ecosystem package managers.

A future Bind package ecosystem should distribute audited Bind source or semantic IR. It does not need to replace Cargo/npm until Bind has requirements their lock models cannot express.
