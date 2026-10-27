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
    ├── core::analysis: grafo da aplicação, operações de banco, diagnósticos tipados
    ├── Rust core ──→ AWS Lambda adapter
    │            └─→ Vercel Rust adapters
    ├── Rust SSR (page rendering, same runtime)
    ├── bind-json-v4 schema and vectors
    ├── JS frontend (web/index.html, app.js, bind-runtime.js)
    └── Bind Wasm + Cloudflare host (app.wasm + worker.js + assets)
```

Generation receives `VerifiedApplication`, not raw source; o backend Rust lê somente a `CoreProgram` (IR tipada produzida por `types::check → core::lower → core::validate`), nunca tokens. Uma IR inconsistente é o erro interno E9001. If verification emits an error, no target generator is called.

Os tokens ficam no lexer e no parser: o AST não guarda o corpo das funções em tokens (`Function.body` não existe mais), e nenhuma análise posterior lê tokens.

## Análise sobre a IR (`core::analysis`)

`src/core/analysis.rs` percorre a `CoreProgram` (com `specialize::children`, o mesmo percurso dos backends) e produz, em ordem determinística:

- **o grafo da aplicação** (`application_graph`): arestas de quem usa para o que é usado (função → função, inclusive funções passadas como valor; cliente → endpoint; função → tabela e campos do modelo; função → `environment.X`). Alimenta `bind graph`, `bind impact`, `application.graph.txt` e o caminho dos diagnósticos; as ligações `environment.X -> env.VAR` vêm das declarações do AST;
- **as operações de banco** (`database_operations` e `operations`): os nós `Db` da IR, lambdas incluídas. O backend Rust e o `VerifiedApplication` usam o mesmo código, e a E8307 decide por elas;
- **os diagnósticos tipados** (`typed_diagnostics`), calculados logo depois do `core::lower` e antes do `core::validate`: E4102 (segredo alcançável por código cliente, por busca em largura a partir das funções `Client`, com o caminho), E5303 (`insert` sem coluna obrigatória), E5204 e E5201 (campos do modelo contra as colunas, pelo mesmo `Env::column_type` do verificador) e E2301 (função sem corpo com retorno diferente de `Unit`). O AST só fornece spans, as declarações do `environment` e os campos declarados do modelo.

Os três geradores (servidor Rust, SSR e frontend JS) leem a mesma `CoreProgram`. Na biblioteca Rust gerada, os tipos e funções do programa ficam no módulo `app` e os itens do gerador (banco, requisições, roteador, testes) na raiz, para que nomes do usuário nunca colidam com eles.

## Backends e runtimes

- **Backend Rust** (`src/backend/rust`): biblioteca com módulo `app`, banco, roteador e testes. O runtime (`runtime/bind-runtime`) é copiado como `src/bind_runtime.rs` e fornece `Int`, `Ctx`, `AppError`, transação, limite de profundidade, exibição, HTML e leitura dos parâmetros da URL.
- **SSR** (`mode = "ssr"`): `handle_page`/`serve` avaliam o componente com as mesmas funções do servidor. Todas as chamadas `server.f` da página compartilham uma transação (commit em sucesso, rollback em erro; nenhuma se não há chamada) e cada uma passa pelos codecs do fio em memória (`ssr_call_<f>`: argumentos escritos como o `JSON.stringify` do frontend e decodificados como pela rota RPC, resultado serializado e relido com o rigor dos decodificadores JS), de modo que os dois modos mostram a mesma página para todo programa e entrada; GET/HEAD `/`, parâmetros estritos, status 200/400/404/500. Componentes estáticos são pré-renderizados na geração (`core::eval` com orçamento, `PRERENDER_BUDGET`: trabalho da avaliação, tamanhos e o HTML do resultado, 1 MiB) em `web/index.html`; fora do orçamento voltam a ser renderizados em tempo de execução. Os renderizadores e a liberação das árvores de UI são iterativos nas três implementações.
- **Backend JS** (`src/backend/js`): `plan.rs` decide a página e as regras de E8311, `expr.rs`/`page.rs` emitem `web/app.js` (módulos ES2022, sem npm nem bundler) sobre `runtime/bind-runtime-js` (`web/bind-runtime.js`): `BigInt` para `Int`, `Math.fround` para `Float`, `Option` uniforme `{ some: v }`, nomes de propriedade escapados, `enter` com `await`, renderização sem `innerHTML`. No modo `split` cada RPC tem sua transação; no `ssr` a página não envia JS.
- **Cloudflare WebAssembly** (`src/backend/wasm.rs`, `src/cloudflare.rs`, `src/backend/js/worker.rs`): o compilador baixa a `CoreProgram` diretamente para WebAssembly Binary Format e escreve `app.wasm` sem Rust, LLVM, WAT ou outro encoder. O Worker mínimo contém Fetch, codecs JSON, CORS, assets e futuras bindings assíncronas. A verificação aceita somente a ABI semanticamente exata (`Boolean`, `Float`, `Double`, `Unit` e controle puro não recursivo); qualquer outro nó alcançável por endpoint é erro de compilação.
- **Coleções e tipos ricos no fio**: `Set`/`Map` são `BindSet`/`BindMap` (newtypes sobre `Arc<BTreeSet>`/`Arc<BTreeMap>`, com serde estrito) no Rust, arrays congelados em ordem canônica no JS (com o comparador da chave escolhido na geração) e `Value::Set`/`Value::Map` no avaliador, que é o oráculo da ordem canônica (ver [perfil v0.4](certified-profile-v0.4.md)). `sealed` com campos, genéricos instanciados e `Result` usam o formato do serde, com `deny_unknown_fields` no Rust e decodificadores equivalentes no JS.
- **Vetores compartilhados** (`runtime/vectors/`): `html-trees.json`, `numbers.json`, `pages.json`, `params.json`, `temporal-order.json` (ordem de `Date`/`DateTime`/`Instant`, conferida contra `chrono`), `collections.json` (ordem canônica e builtins de coleção) e `wire-rich.json` (JSON dos tipos ricos, válidos e inválidos) são lidos pelos testes de `bindc` (`core::display`, canônico), do runtime Rust e do runtime JS, o que mantém as três implementações idênticas. O avaliador `src/core/eval.rs` serve ao teste diferencial a três vias (avaliador × Rust × JS) e à pré-renderização.

The compiler-owned typed IR is the semantic center of every deployment target. The Rust library remains the server implementation for AWS and Vercel; the Cloudflare profile translates that IR directly to Wasm instructions and a thin host. AWS and Vercel adapters are transport layers, while the Worker host owns Fetch routing and delegates static assets to Cloudflare.

The JS runtime owns the corresponding JSON decoders and request encoders. A response that violates the contract becomes the "Request failed" screen instead of being accepted as an unchecked value.

## Dependency policy

The certified profile does not expose a general package declaration. Bind owns exact direct dependency versions, Cargo produces the complete transitive lock, and the frontend is native ES modules without npm, so there is no frontend lock. This provides a smaller and stronger supply-chain boundary than creating a new resolver while still relying on Cargo.

A future Bind package ecosystem should distribute audited Bind source or semantic IR. It does not need to replace Cargo until Bind has requirements their lock models cannot express.
