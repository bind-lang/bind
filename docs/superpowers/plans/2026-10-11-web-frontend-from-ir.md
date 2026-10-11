# Frontend web e SSR a partir da IR — Plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Trocar o cliente Elm por um frontend HTML/CSS/JS nativo e um SSR em Rust, ambos gerados só a partir da `CoreProgram`, com a mesma página nos modos `ssr` e `split`.

**Architecture:** A IR ganha nós de UI e primitivas de exibição. Um backend novo (`src/backend/js/`) traduz a IR para funções `async` em módulos ES, apoiado num runtime JS próprio (`runtime/bind-runtime-js/`). O backend Rust passa a traduzir UI e serve as páginas no modo SSR, com renderização HTML no runtime Rust. Um plano do frontend compartilhado entre `verify` e o backend JS gera a E8311.

**Tech Stack:** Rust 1.98 (edition 2024), JavaScript ES2022 (módulos nativos, `BigInt`), node (testes), PostgreSQL (testes ponta a ponta opcionais).

**Spec:** `docs/superpowers/specs/2026-10-11-web-frontend-from-ir-design.md`

## Global Constraints

- Rust 1.98.0, edition 2024; o `bindc` não ganha dependências de produção novas.
- O JS gerado e o runtime JS são módulos ES2022 nativos, sem npm, sem bundler, sem etapa de build; não usam `innerHTML`, `eval`, `Function(...)` nem `document.write`.
- `maxDepth = 10000`, contado só na entrada de funções Bind e lambdas (nunca em builtins), igual no Rust, no JS e no `core::eval`. No JS, `enter` sempre faz `await`.
- `Int` canônico no fio e na URL: `-?[0-9]+`, sem zeros à esquerda, sem `-0`, no máximo 1000 dígitos.
- `Decimal`/`Money` em componentes: `+`, `-`, negação, comparação e exibição; mantissa < 2^96, escala ≤ 28; estouro → `Overflow`; `*` e `/` → E8311 (nos dois modos).
- `DisplayDouble`/`DisplayFloat` seguem o `Number.prototype.toString` do JavaScript (`js_number_to_string`), idêntico no `core::eval`, no runtime Rust e no JS.
- Perfil `rust-web-v3`, contrato `bind-json-v3`, documentação `docs/certified-profile-v0.3.md`.
- `bind.toml`: `client = "web"`, `server = "rust"`, `mode = "ssr" | "split"`, `api_url` (padrão `/api`), `frontend_origin`.
- Diagnóstico novo: E8311. Removido: E8306. Todo o gerador Elm sai.
- Sem panics, `unwrap`, `expect`, `unreachable!` no código Rust gerado e no runtime Rust (deny-list atual).
- Testes que precisam de `node` são `#[ignore]` e, sem a ferramenta, imprimem `skipped: node not found` e passam. Testes com banco usam `BIND_TEST_DATABASE_URL` e imprimem `skipped: BIND_TEST_DATABASE_URL not set` sem ela.
- Depois de cada tarefa: `cargo test`, `cargo clippy --all-targets -- -D warnings` e `cargo fmt --check` passam.
- Commits terminam com `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Branch `web-frontend`. Comandos de geração usam `DATABASE_URL=postgresql://bind:bind@localhost/bind` (o `certify` não conecta).
- Não modifique o repositório `../website`; testes do site usam uma cópia temporária com o `bind.toml` atualizado.

## Review Focus

1. Parâmetro de URL com codificação percentual e `+` (`?nome=Jo%C3%A3o+Silva`) é decodificado como `João Silva` nos dois modos; parâmetro repetido (`?id=a&id=b`) dá `BadParameter` (Tasks 6 e 7).
2. Servidor respondendo 500 com corpo que não é JSON, ou indisponível, vira a tela de erro com "Request failed", nunca uma exceção não tratada (Task 9).
3. `map`/`fold` sobre 100.000 itens no JS terminam sem `RangeError` (Task 4).
4. `Text("<script>alert(1)</script>")` aparece escapado no HTML do SSR e como texto no DOM do JS (Tasks 3 e 4).
5. Registro com campos `__proto__`, `constructor` e `toString`, e funções/tipos chamados `class`, `await`, `default`, `Object`, `enter`: o JSON decodifica sem poluir protótipos e o JS gerado executa (Task 5).

---

### Task 1: UI na IR, primitivas de exibição e lowering do frontend

**Files:**
- Modify: `src/core/ir.rs`, `src/core/lower.rs`, `src/core/validate.rs`, `src/core/specialize.rs`, `src/core/eval.rs` (só para compilar: `Ui`/`Display*` → `Unsupported` até a Task 2), `src/backend/rust/expr.rs` (só para compilar: erro interno do plano, sem pânico, até a Task 6)

**Interfaces:**
- Produces:
```rust
pub enum UiNode { Ui, Page, Container, Div, Grid, Stack, Header, Nav, Main, Section, Article,
    Aside, Footer, Paragraph, Span, Strong, Emphasis, Small, List, Item, Pre, Code, Heading,
    Link, Rule, Text, Empty }
impl UiNode { pub fn from_name(name: &str) -> Option<UiNode>; pub fn name(&self) -> &'static str; }
// ExprKind::Ui substitui a variante unitária:
Ui { node: UiNode, args: Vec<Expr>, children: Box<Expr> }   // children: List<Ui>
// Prim ganha:
DisplayString, DisplayInt, DisplayLong, DisplayDouble, DisplayFloat, DisplayBool, DisplayDecimal, DisplayText
// src/core/lower.rs:
pub fn client_functions(program: &CoreProgram, component: &str) -> Vec<String>
```
- Regras:
  - Bloco de filhos → expressão `List<Ui>`: cada filho é um elemento de `listOf`; um `val` vira `Let` envolvendo o restante; um statement que não seja `Ui` vira `Seq`. Nó sem bloco → `emptyList()` tipado `List<Ui>`.
  - `if` sem `else` aceito pelo verificador dentro da UI → `else_branch` = `Ui{Empty, [], emptyList}`.
  - `Text(e)` → `Ui{Text, [Prim(Display*, [e])], emptyList}`, primitiva pela tabela da spec (seção 1); distinct usa a do tipo base, extraindo o valor como a lowering já faz para distinct.
  - `ServerCall` mantém os argumentos escritos.
  - Validador: `Ui` com `ty == Ui`; `children` com tipo `List<Ui>`; argumentos por nó pelas regras de `infer_ui`; `Text` com exatamente 1 argumento `String` e filhos vazios; `Display*` com operando do tipo esperado e resultado `String`.
- [ ] **Step 1: Testes que falham** (módulo de testes de `src/core/lower.rs`):
  - `client component C() { UI { Section("a", "b") { val x = "t"\n Text(x)\n Text(1) } } }` → corpo `Ui{Ui, [], listOf(Ui{Section, ["a","b"], Let(x, "t", listOf(Ui{Text,[DisplayString(Local x)]}, Ui{Text,[DisplayInt(1)]}))})}`.
  - `if (b) Text("s")` sem `else` na UI: `else_branch` `Empty` (ou, se o verificador recusar, um teste que documenta o diagnóstico atual).
  - `Text` de `Boolean`, `Double`, `Float`, `Money`, `UUID` e de um distinct sobre `Int` → primitiva certa.
  - `client fun c(id: CustomerId) = server.getCustomer(id)` → `ServerCall{"getCustomer", [Local id]}`.
  - `client_functions` inclui uma função compartilhada chamada pelo componente e exclui uma não alcançável.
  - Validador (IR montada à mão): `Text` com 2 argumentos ("arity"); `children` `List<Int>` ("children"); `DisplayInt` sobre `String` ("operand").
- [ ] **Step 2: Rodar e confirmar a falha.** `cargo test core::`
- [ ] **Step 3: Implementar.** Atualize todo `match` sobre `ExprKind`.
- [ ] **Step 4: Rodar.** `cargo test`, clippy, fmt; `make -C ../website check` → código 0.
- [ ] **Step 5: Commit.** `git commit -m "feat(core): árvore de UI e primitivas de exibição na IR"`

---

### Task 2: Avaliador com UI, `Double` e `Float`

**Files:**
- Modify: `src/core/eval.rs`
- Create: `src/core/display.rs` (`pub fn js_number_to_string(value: f64) -> String`, `pub fn display_decimal(...)`, demais formatos de exibição; usado pelo avaliador e, na Task 3, copiado para o runtime Rust)

**Interfaces:**
- Consumes: Task 1.
- Produces: `Value::Ui { node: UiNode, args: Vec<Value>, children: Vec<Value> }`, `Value::Double(f64)`, `Value::Float(f32)`; `Value::render`:
  - `Ui` → o HTML que a página renderiza para o nó (mesma função de renderização HTML da Task 3, implementada em `src/core/display.rs` como `render_html_value`), para que as três vias comparem HTML;
  - `Double`/`Float` → `js_number_to_string` (para `Float`, sobre o valor convertido para f64).
- Regras: `Double*` com `f64` do Rust; `Float*` com `f32` do Rust; `Display*` pela tabela da spec. `ServerCall` continua `Unsupported`.
- [ ] **Step 1: Testes que falham:**
  - `js_number_to_string`: `0.1+0.2` → `"0.30000000000000004"`; `1e21` → `"1e+21"`; `1e-7` → `"1e-7"`; `123456789012345680000.0` → `"123456789012345680000"`; `-0.0` → `"0"`; `5e-324` → `"5e-324"`; `f64::NAN` → `"NaN"`; `f64::INFINITY` → `"Infinity"`; `1.5e300` → `"1.5e+300"`; `0.000001` → `"0.000001"`.
  - `fun f(): Float = 0.1f + 0.2f` → render igual a `js_number_to_string((0.1f32 + 0.2f32) as f64)`.
  - `fun f() = UI { Div { Text(1 + 1) } }` → render `<div>…<p>2</p>…</div>` na forma exata documentada no doc comment de `render_html_value` (mesmos tags e classes do mapeamento atual de `elm_ui_node` em `src/certified.rs`).
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** `cargo test core::`, `cargo test`, clippy, fmt.
- [ ] **Step 5: Commit.** `git commit -m "test(core): avaliador com UI, Double e Float"`

---

### Task 3: Runtime Rust — nós, HTML e exibição

**Files:**
- Modify: `runtime/bind-runtime/src/lib.rs`

**Interfaces:**
- Consumes: Task 2 (`src/core/display.rs` como referência de comportamento).
- Produces (em `bind_runtime`):
```rust
pub enum Node { Element { tag: NodeTag, args: Vec<NodeArg>, children: Vec<Node> }, Text(String), Empty }
pub enum NodeArg { Str(String), Int(Int) }
pub fn render_html(node: &Node) -> String;              // escapa & < > " '
pub fn page_html(title: &str, stylesheet: &str, body: &str) -> String;   // invólucro único dos dois modos
pub fn error_page(error: &AppError) -> (u16, String);   // status e HTML da tela de erro
pub fn js_number_to_string(value: f64) -> String;
pub fn display_decimal(value: &rust_decimal::Decimal) -> String;
```
- Regras: `render_html` produz byte a byte o mesmo HTML de `render_html_value` (Task 2) para a mesma árvore; textos das telas ("Loading…", títulos e mensagens de erro) definidos aqui como constantes públicas e reutilizados pelo runtime JS (Task 4) com os mesmos valores.
- [ ] **Step 1: Testes que falham** (no `lib.rs`):
  - Os mesmos vetores de `js_number_to_string` da Task 2.
  - **Review Focus 4:** `render_html(&Node::Text("<script>alert(1)</script>".into()))` contém `&lt;script&gt;alert(1)&lt;/script&gt;` e nenhum `<script`.
  - `error_page(&AppError::NotFound(..))` → status 404 e a mensagem pública; `BadRequest` → 400; `Internal` → 500 sem detalhes.
  - Um teste no `bindc` (`src/core/display.rs`) confere que `render_html_value` e o `render_html` do runtime (via o mesmo vetor de árvores serializado em JSON num arquivo de teste) dão o mesmo HTML para 20 árvores fixas.
- [ ] **Step 2: Rodar e confirmar a falha.** `cargo test --manifest-path runtime/bind-runtime/Cargo.toml`
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** testes, clippy e fmt do runtime; `cargo test` do `bindc`.
- [ ] **Step 5: Commit.** `git commit -m "feat(runtime): nós de UI, HTML escapado e exibição no runtime Rust"`

---

### Task 4: Runtime JS

**Files:**
- Create: `runtime/bind-runtime-js/bind-runtime.js`, `runtime/bind-runtime-js/tests/run.mjs`, `runtime/bind-runtime-js/tests/dom.mjs` (DOM mínimo: `createElement`, `createTextNode`, `textContent`, `setAttribute`, `appendChild`, `replaceChildren`, serialização `outerHTML` com o mesmo escape do Rust), `src/backend/js/mod.rs` (`pub const RUNTIME: &str = include_str!(…)` e testes), `src/backend/mod.rs` (`pub mod js;`)
- Modify: `Makefile` (alvo `test-js-runtime`)

**Interfaces:**
- Produces (exports de `bind-runtime.js`):
```js
export const MAX_DEPTH = 10000;
export class BindError { constructor(kind, message) }   // kind: "RecursionLimit" | "Overflow" | "Remote" | "BadParameter"
export function newCtx();
export async function enter(ctx);                         // sempre await; RecursionLimit acima de MAX_DEPTH
export function intParse(text);                           // BigInt | null (forma canônica, ≤ 1000 dígitos)
export function intToString(n);
export function intDiv(a, b); export function intRem(a, b);   // BigInt | null
export class Decimal { static parse(text); toString(); }   // mantissa BigInt + escala, normalizado
export function decimalAdd(a, b); export function decimalSub(a, b); export function decimalNeg(a);  // lançam BindError("Overflow")
export function decimalCompare(a, b);
export function f32(x);                                   // Math.fround
export function equals(a, b); export function compare(a, b);
export function first(xs); export function get(xs, i); export function length(xs);
export async function map(ctx, xs, f); export async function fold(ctx, xs, init, f);
export function listOf(...xs); export function emptyList();
export function jsNumberToString(x);                      // = String(x), documentado
export function displayDecimal(d);
export function node(tag, args, children); export function text(s); export const EMPTY;
export function render(node, document);                   // createElement/textContent; nunca innerHTML
export async function rpc(ctx, url, body, decode);        // fetch; erros → BindError("Remote", …)
export function readParams(search, specs);                // URLSearchParams + decodificadores estritos
export const TEXTS;                                       // os mesmos textos das telas do runtime Rust
```
- Regras: `map`/`fold` iteram com `for` e `await` (sem recursão); objetos decodificados de JSON são criados com `Object.create(null)` ou com `Object.defineProperty`, nunca por atribuição a chaves arbitrárias, e congelados.
- [ ] **Step 1: Testes que falham** — `tests/run.mjs` executado pelo node, chamado por um teste Rust `#[ignore]` em `src/backend/js/mod.rs`:
  - `intParse` recusa `"01"`, `"-0"`, `"+1"`, `""`, `" 1"`, `"1.5"`, 1001 dígitos; aceita 1000 dígitos; `intDiv`/`intRem` com os quatro pares de sinais e divisor zero.
  - `Decimal`: vetores gerados por um teste do runtime Rust (`runtime/bind-runtime`, que já depende de `rust_decimal`) gravados em `runtime/bind-runtime-js/tests/decimal-vectors.json`: `add`/`sub` com resultado ou `Overflow`, `toString`/`parse`, `compare`.
  - `f32`: vetores de operações f32 gerados pelo mesmo teste Rust.
  - `jsNumberToString` sobre os vetores da Task 2.
  - `enter` 10.000 vezes aninhadas → ok; 10.001 → `RecursionLimit`; recursão real de 100.000 níveis via uma função que chama `enter` → `RecursionLimit` sem `RangeError`.
  - **Review Focus 3:** `map`/`fold` sobre 100.000 itens terminam.
  - **Review Focus 4:** `render(text("<script>alert(1)</script>"), dom)` serializado contém `&lt;script&gt;`; o DOM mínimo registra que `innerHTML` nunca foi usado.
  - `render` de 20 árvores fixas produz o mesmo HTML que o `render_html` do Rust (Task 3), lido do mesmo arquivo de vetores.
- [ ] **Step 2: Rodar e confirmar a falha.** `cargo test backend::js -- --ignored --nocapture`
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** o teste ignorado, `make test-js-runtime`, `cargo test`, clippy, fmt.
- [ ] **Step 5: Commit.** `git add runtime/bind-runtime-js runtime/bind-runtime src Makefile && git commit -m "feat(runtime): runtime JS com BigInt, Decimal checado e renderização segura"`

---

### Task 5: Backend JS — nomes, tipos, codecs e expressões

**Files:**
- Create: `src/backend/js/names.rs`, `src/backend/js/codecs.rs`, `src/backend/js/expr.rs`, `tests/codegen/web_*.bd`, `tests/codegen/web_*.js.expected`
- Modify: `src/backend/js/mod.rs`

**Interfaces:**
- Consumes: Tasks 1, 2, 4.
- Produces:
  - `names::js_value(name: &str) -> String`: escape determinístico e injetivo com prefixo `u_` em todo nome do usuário (`_` → `__`, fora de `[A-Za-z0-9]` → `_x<hex>_`).
  - `codecs::decoder(ty: &Type, program: &CoreProgram) -> String`, `codecs::encoder(...)`: só a partir de `Type` da IR; JSON igual ao do servidor (registros com nomes de campo originais, variantes sem campos em `lower_camel` via `certified::lower_camel`, distinct transparente, `Int`/`Decimal`/`Money` como string, `null` para `None`).
  - `expr::function(function: &CoreFunction, program: &CoreProgram) -> String`.
  - `backend::js::render_app(program: &CoreProgram, names: &[String]) -> String`: o módulo `app.js` com `import * as rt from "./bind-runtime.js"`, codecs e as funções pedidas, exportando o componente.
- Regras (spec, seção 2): funções `async function u_f(ctx, …) { ctx = await rt.enter(ctx); … }`; expressões em instruções com temporários (`Return` → `return`); `Match` → `switch` sobre `tag` ou cadeia de `if`; `Option` aninhado com `{some: v}`; `Float*` com `rt.f32`; `Int` com `BigInt` e `rt.intDiv`/`rt.intRem`; literais `Int` como `123n`; `ServerCall` → `await rt.rpc(ctx, API_URL + "/<rota>", corpo, decoder)` com `API_URL` vindo de uma constante gerada; `Ui` → `rt.node`/`rt.text`/`rt.EMPTY`.
- [ ] **Step 1: Testes que falham:**
  - Snapshots `web_arith.bd` (`Int` grande, `/` e `%` com `?:`), `web_adt.bd` (sealed recursivo + `when`), `web_lambdas.bd` (`map`/`fold`), `web_ui.bd` (UI com `val` nos filhos, `if`/`when`, `Text` de cada tipo exibível), `web_names.bd` (**Review Focus 5**: campos `__proto__`, `constructor`, `toString`; nomes `class`, `await`, `default`, `Object`, `enter`).
  - `#[ignore]`: cada snapshot passa em `node --check` e, para `web_names`, um script node decodifica `{"__proto__": "x", "constructor": "y", "toString": "z"}` com o decoder gerado, confere os três campos e que `({}).x === undefined` (sem poluição de protótipo).
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** `cargo test backend::js`, `BIND_BLESS=1` para gravar e revisar os snapshots, o teste ignorado, clippy, fmt.
- [ ] **Step 5: Commit.** `git commit -m "feat(js): tradução da IR para JavaScript nativo"`

---

### Task 6: SSR no backend Rust

**Files:**
- Modify: `src/backend/rust/expr.rs`, `src/backend/rust/mod.rs`, `src/backend/rust/endpoints.rs`
- Create: `tests/codegen/ssr.bd`, `tests/codegen/ssr.csv`, `tests/codegen/ssr.rs.expected`

**Interfaces:**
- Consumes: Tasks 1, 3.
- Produces: `Ui` e `Display*` traduzidos para chamadas do runtime (`Node`, `display_*`, `js_number_to_string`); `pub async fn handle_page(query: &str, pool: &PgPool) -> HttpResult` no crate gerado, ligado ao roteador em `GET /` quando o modo é `ssr`.
- Regras (spec, seção 3): decodifica os parâmetros da query string com os mesmos decodificadores estritos (percent-decoding e `+` como espaço; ausente/repetido/inválido → 400 com `error_page`); abre transação; avalia o componente com `ServerCall` como chamada direta à função Rust do endpoint; commit em `Ok`, rollback em `Err`; responde `page_html(...)` com status 200 ou o de `error_page`. Content-Type `text/html; charset=utf-8`.
- [ ] **Step 1: Testes que falham:**
  - Snapshot `ssr.bd` (componente com parâmetro `CustomerId`, `server.getCustomer(id)`, UI com `Text` dos campos) → `ssr.rs.expected`.
  - `#[ignore]` (crate gerado compilado e testado com `cargo test`, sem banco): `handle_page` com query inválida → 400 e a mensagem de parâmetro; **Review Focus 1**: num componente com parâmetro `String`, `?nome=Jo%C3%A3o+Silva` → HTML contém `João Silva`; `?id=a&id=b` → 400.
  - O crate gerado passa `cargo clippy -- -D warnings`.
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** `cargo test`, `cargo test backend:: -- --ignored`, clippy, fmt.
- [ ] **Step 5: Commit.** `git commit -m "feat(backend): SSR das páginas no servidor Rust"`

---

### Task 7: Modos, plano do frontend (E8311), integração ao `certify` e remoção do Elm

**Files:**
- Create: `src/backend/js/plan.rs`, `src/backend/js/page.rs`, `tests/codegen/website.html.expected`
- Modify: `src/backend/js/mod.rs`, `src/certified.rs`, `src/compiler.rs`, `src/config.rs` (ou onde `TargetConfig` é lido), `src/pipeline.rs`

**Interfaces:**
- Consumes: Tasks 1–6.
- Produces:
  - `TargetConfig { client: "web", server: "rust", mode: Mode::{Ssr, Split}, api_url: String, frontend_origin: Option<String>, … }`.
  - `plan::frontend_problems(program: &CoreProgram, app: &VerifiedApplication, target: &TargetConfig) -> Vec<FrontendProblem>` (mesmo código usado por `web_files`), convertido em E8311 por `verify`.
  - `backend::js::web_files(app, target) -> Result<Vec<(String, String)>, String>`: `web/index.html`, `web/app.js`, `web/bind-runtime.js`, `web/styles.css`; componente estático → `index.html` pré-renderizado (via `core::eval` + `render_html_value`), sem JS.
- Regras:
  - **Antes de remover o gerador antigo:** grave `tests/codegen/website.html.expected` com o HTML que o gerador atual produz para `../website/src/site.bd` (renderizado pelo Elm compilado, ou equivalentemente pela árvore estática atual — documente o método) e um teste que compara o HTML pré-renderizado novo com ele.
  - E8311 (mensagem iniciando pelo nome da função ou do componente): `DecimalMul`/`DecimalDivChecked` alcançáveis a partir do componente; parâmetro de componente fora de `String`, `Email`, `Url`, `Int`, `UUID`, `Date`, `DateTime`, `Instant` e distinct sobre eles; target sem componente cliente; `api_url` absoluta de outra origem sem `frontend_origin`; `client = "elm"` (mensagem com a troca para `client = "web"` e `mode`).
  - Deployments (spec, seção 3): vercel SSR (uma função Rust serve `/` e `/api/*`), vercel split (estáticos + funções `/api/*`), aws-lambda SSR (Lambda serve `/` e `/api/*`), aws-lambda split (Lambda serve `/api/*`; `web/` pronto); CORS no backend só para `frontend_origin` quando definido (pré-voo `OPTIONS` incluído), recusando outras origens.
  - Remova: `component_plan`, `ComponentPlan`, `UiNode`/`UiArgument` antigos, `parse_ui_nodes`, `parenthesized`, `elm_main`, `elm_static_main`, `elm_ui_node`, `elm_ui_text`, todos os codecs/tipos Elm, `ELM_JSON`, `INT_CODEC`, os testes `client_int*` com `elm make`, a E8306 e os casos especiais E8301/E8303 do cliente em `compiler.rs`. Atualize o `examples/commerce/bind.toml` para `client = "web"` com um target por combinação usada nos testes.
  - `PROFILE_VERSION = "rust-web-v3"`, `WIRE_VERSION = "bind-json-v3"`; lock e manifesto atualizados (hash dos arquivos web e do runtime JS).
- [ ] **Step 1: Testes que falham:**
  - `compiler.rs`: um teste por regra da E8311; o `commerce` sem diagnósticos.
  - `website.html.expected` (gravado antes da remoção) igual ao pré-render novo, a partir de uma cópia temporária do site com `client = "web"` e `mode = "split"`.
  - **Review Focus 1** (modo split): teste `#[ignore]` que roda `rt.readParams` do `web/app.js` gerado no node com `?id=<uuid>`, `?id=a&id=b`, sem `id`, e `?nome=Jo%C3%A3o+Silva` num componente com parâmetro `String`.
  - `certify` do `commerce` nas quatro combinações (vercel/aws-lambda × ssr/split) → sucesso.
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** `cargo test`, `cargo test -- --ignored`, clippy, fmt; os quatro `certify`; `make -C ../website check` (o `check` do site atual dará a E8311 de `client = "elm"`: confirme a mensagem e reporte; o repositório do site não é alterado).
- [ ] **Step 5: Commit.** `git commit -m "feat(web): frontend HTML/JS gerado da IR, modos ssr/split e perfil rust-web-v3; remove o Elm"`

---

### Task 8: Teste diferencial a três vias

**Files:**
- Create: `src/backend/js/differential.rs` (`#[cfg(test)]`)
- Modify: `src/backend/rust/differential.rs` (extrair o gerador para reuso sem mudar o teste do servidor; acrescentar a via HTML do Rust), `Makefile` (`test-generated` inclui o novo teste)

**Interfaces:**
- Consumes: Tasks 1–6; o gerador de `src/backend/rust/differential.rs`.
- Regras:
  - O gerador ganha o tipo de retorno `Ui` (nós com argumentos `String`/`Int`, `Text` de `Int`, `String`, `Boolean`, `Double`, `Float` e de um distinct sobre `Int`, `if`/`when` na UI, `val` nos filhos) e expressões `Double`/`Float` (`+ - * /` com literais finitos, incluindo resultados `Infinity`/`NaN`).
  - 1.000 programas: `core::eval` (render canônico, HTML para `Ui`), o crate Rust gerado (`render_html` para `Ui`) e o `app.js` gerado executado no node com o DOM mínimo da Task 4 (serialização HTML). As três saídas idênticas byte a byte.
  - Inclui a recursão de 20.000 níveis: `!RecursionLimit` nas três vias, sem `RangeError` nem estouro de pilha.
  - Divergência falha com o programa e as três saídas. Nunca reduza a quantidade nem tire construções; corrija o código, com um teste de regressão por bug real.
- [ ] **Step 1:** escrever o teste.
- [ ] **Step 2:** `cargo test differential -- --ignored --nocapture` até 0 divergências.
- [ ] **Step 3:** `cargo test`, clippy, fmt.
- [ ] **Step 4: Commit.** `git commit -m "test(web): diferencial a três vias avaliador × Rust × JS"`

---

### Task 9: Ponta a ponta com Postgres nos dois modos

**Files:**
- Create: `src/backend/js/end_to_end.rs` (`#[cfg(test)]`, `#[ignore]`)

**Interfaces:**
- Consumes: Tasks 6 e 7; helpers de banco e de crate de `src/backend/rust/transactions.rs`.
- Regras:
  - Com `BIND_TEST_DATABASE_URL`: schema temporário, `commerce` gerado; um binário de teste no crate gerado expõe `handle_rpc`/`handle_page` num servidor HTTP mínimo em porta local (só `std::net` e o tokio já presente).
  - SSR: `GET /?id=<existente>` → 200 com nome e e-mail; `?id=<inexistente>` → 404 com a mensagem pública; `?id=xyz` → 400; um componente de teste que chama um endpoint que grava e depois falha → status de erro e 0 linhas (rollback).
  - Split: o `web/app.js` gerado executado no node (DOM mínimo; `fetch` nativo do node) contra o servidor: caminho feliz, 404, parâmetro inválido. **Review Focus 2:** um endpoint de teste que responde 500 com corpo `oops` e o servidor desligado → tela com "Request failed".
  - CORS: com `frontend_origin = "http://a.test"`, pré-voo e POST com `Origin: http://a.test` aceitos; com `Origin: http://b.test` recusados.
  - Schema removido mesmo em falha; sem a variável, imprime a linha de pulo e passa.
- [ ] **Step 1:** escrever o teste.
- [ ] **Step 2:** rodar com a variável e corrigir até passar.
- [ ] **Step 3:** `cargo test`, clippy, fmt.
- [ ] **Step 4: Commit.** `git commit -m "test(web): ponta a ponta SSR e split com Postgres"`

---

### Task 10: Documentação

**Files:**
- Create: `docs/certified-profile-v0.3.md`
- Modify: `docs/architecture.md`, `README.md`, `docs/certified-profile-v0.2.md` (nota no topo apontando para a v0.3)

- [ ] **Step 1:** `docs/certified-profile-v0.3.md` no formato do v0.2: `rust-web-v3`; pilha única HTML/CSS/JS + Rust; modos `ssr`/`split` e os quatro deployments; UI real, expressões tipadas, várias chamadas `server.f`, parâmetros pela URL, telas padrão; runtimes Rust e JS; `Int` exato no frontend; `bind-json-v3`; CORS; E8311; o que a certificação garante e não garante (sem eventos nem estado; `*`/`/` de `Decimal` só no servidor; página SSR sem JS).
- [ ] **Step 2:** `docs/architecture.md` com o backend JS, o SSR e o plano do frontend; `README.md` com o perfil v3 e a instrução de migração do `bind.toml` (`client = "web"`, `mode`).
- [ ] **Step 3:** verificação final: `cargo test`, `cargo test -- --ignored` (com `BIND_TEST_DATABASE_URL`), testes do runtime Rust, `make test-js-runtime`, clippy dos dois crates, fmt, os quatro `certify` do `commerce` e o `certify` do site numa cópia temporária com o `bind.toml` atualizado.
- [ ] **Step 4: Commit.** `git commit -m "docs: perfil certificado rust-web-v3"`

Mapa dos critérios de aceite da spec:

| Critério | Coberto por |
|---|---|
| 1. Frontend JS e SSR só da `CoreProgram`; Elm e tokens removidos | Tasks 1, 5, 6, 7 |
| 2. Nenhum programa verificado falha no `generate` | Task 7 |
| 3. `Int` exato no frontend | Tasks 4, 5 |
| 4. Diferencial a três vias ≥ 1.000 programas, 0 divergências, recursão 20.000 | Task 8 |
| 5. `certify` em aws-lambda e vercel × ssr e split; site com HTML inalterado | Tasks 7, 10 |
| 6. Testes, clippy, fmt, documentação v0.3 | Task 10 |
