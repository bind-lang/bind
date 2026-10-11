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
    ├── Rust SSR (page rendering, same runtime)
    ├── bind-json-v3 schema and vectors
    └── JS frontend (web/index.html, app.js, bind-runtime.js)
```

Generation receives `VerifiedApplication`, not raw source; o backend Rust lê somente a `CoreProgram` (IR tipada produzida por `types::check → core::lower → core::validate`), nunca tokens. Uma IR inconsistente é o erro interno E9001. If verification emits an error, no target generator is called.

Os três geradores (servidor Rust, SSR e frontend JS) leem a mesma `CoreProgram`. Na biblioteca Rust gerada, os tipos e funções do programa ficam no módulo `app` e os itens do gerador (banco, requisições, roteador, testes) na raiz, para que nomes do usuário nunca colidam com eles.

## Backends e runtimes

- **Backend Rust** (`src/backend/rust`): biblioteca com módulo `app`, banco, roteador e testes. O runtime (`runtime/bind-runtime`) é copiado como `src/bind_runtime.rs` e fornece `Int`, `Ctx`, `AppError`, transação, limite de profundidade, exibição, HTML e leitura dos parâmetros da URL.
- **SSR** (`mode = "ssr"`): `handle_page`/`serve` avaliam o componente com as mesmas funções do servidor. Todas as chamadas `server.f` da página compartilham uma transação (commit em sucesso, rollback em erro; nenhuma se não há chamada) e cada uma passa pelos codecs do fio em memória (`ssr_call_<f>`: argumentos escritos como o `JSON.stringify` do frontend e decodificados como pela rota RPC, resultado serializado e relido com o rigor dos decodificadores JS), de modo que os dois modos mostram a mesma página para todo programa e entrada; GET/HEAD `/`, parâmetros estritos, status 200/400/404/500. Componentes estáticos são pré-renderizados na geração (`core::eval` com orçamento, `PRERENDER_BUDGET`: trabalho da avaliação, tamanhos e o HTML do resultado, 1 MiB) em `web/index.html`; fora do orçamento voltam a ser renderizados em tempo de execução. Os renderizadores e a liberação das árvores de UI são iterativos nas três implementações.
- **Backend JS** (`src/backend/js`): `plan.rs` decide a página e as regras de E8311, `expr.rs`/`page.rs` emitem `web/app.js` (módulos ES2022, sem npm nem bundler) sobre `runtime/bind-runtime-js` (`web/bind-runtime.js`): `BigInt` para `Int`, `Math.fround` para `Float`, `Option` uniforme `{ some: v }`, nomes de propriedade escapados, `enter` com `await`, renderização sem `innerHTML`. No modo `split` cada RPC tem sua transação; no `ssr` a página não envia JS.
- **Vetores compartilhados** (`runtime/vectors/`): `html-trees.json`, `numbers.json`, `pages.json`, `params.json` e `temporal-order.json` (ordem de `Date`/`DateTime`/`Instant`, conferida contra `chrono`) são lidos pelos testes de `bindc` (`core::display`, canônico), do runtime Rust e do runtime JS, o que mantém as três implementações idênticas. O avaliador `src/core/eval.rs` serve ao teste diferencial a três vias (avaliador × Rust × JS) e à pré-renderização.

The compiler-owned Rust library is the semantic center of both deployment targets. It defines domain representations, strict request structures, SQL generated from database IR, row decoding, status mapping, and JSON responses. AWS and Vercel adapters are thin transport layers.

The JS runtime owns the corresponding JSON decoders and request encoders. A response that violates the contract becomes the "Request failed" screen instead of being accepted as an unchecked value.

## Dependency policy

The certified profile does not expose a general package declaration. Bind owns exact direct dependency versions, Cargo produces the complete transitive lock, and the frontend is native ES modules without npm, so there is no frontend lock. This provides a smaller and stronger supply-chain boundary than creating a new resolver while still relying on Cargo.

A future Bind package ecosystem should distribute audited Bind source or semantic IR. It does not need to replace Cargo until Bind has requirements their lock models cannot express.
