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
    │   (types::check → core::lower → core::validate: CoreProgram, IR tipada)
    ├── Rust core ──→ AWS Lambda adapter
    │            └─→ Vercel Rust adapters
    ├── bind-json-v2 schema and vectors
    └── Elm client and codecs
```

Generation receives `VerifiedApplication`, not raw source; o backend Rust lê somente a `CoreProgram` (IR tipada produzida por `types::check → core::lower → core::validate`), nunca tokens. Uma IR inconsistente é o erro interno E9001. If verification emits an error, no target generator is called.

O cliente Elm ainda é gerado da `VerifiedApplication` (AST e tokens verificados) até o subprojeto 3c. Na biblioteca Rust gerada, os tipos e funções do programa ficam no módulo `app` e os itens do gerador (banco, requisições, roteador, testes) na raiz, para que nomes do usuário nunca colidam com eles.

O runtime (`runtime/bind-runtime`) é copiado para o crate gerado como `src/bind_runtime.rs` e fornece `Int`, `Ctx`, `AppError`, transação por chamada e limite de profundidade. O avaliador de referência em `src/core/eval.rs` serve ao teste diferencial contra o Rust gerado.

The compiler-owned Rust library is the semantic center of both deployment targets. It defines domain representations, strict request structures, SQL generated from database IR, row decoding, status mapping, and JSON responses. AWS and Vercel adapters are thin transport layers.

The Elm generator owns the corresponding JSON decoders and request encoders. A response that violates the contract becomes an explicit `Http.BadBody` state instead of being accepted as an unchecked value.

## Dependency policy

The certified profile does not expose a general package declaration. Bind owns exact direct dependency versions, Cargo produces the complete transitive lock, and Vercel's frontend build uses a committed generated npm lock with `npm ci`. This provides a smaller and stronger supply-chain boundary than creating a new resolver while still relying on mature ecosystem package managers.

A future Bind package ecosystem should distribute audited Bind source or semantic IR. It does not need to replace Cargo/npm until Bind has requirements their lock models cannot express.
