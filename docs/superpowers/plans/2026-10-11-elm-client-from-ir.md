# Cliente Elm a partir da IR — Plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Gerar o cliente Elm só a partir da `CoreProgram`, com a árvore de UI real, `Int` exato e a mesma semântica do servidor, removendo a análise por tokens do cliente.

**Architecture:** A IR ganha nós de UI completos e primitivas de exibição. Um backend novo (`src/backend/elm/`) traduz a IR para Elm na convenção uniforme `Ctx -> … -> Task BindError R`, apoiado num runtime Elm próprio (`runtime/bind-runtime-elm/`) copiado para o workspace gerado. Um plano do cliente, compartilhado entre `verify` e o backend, transforma em diagnóstico (E8311) tudo que o gerador não suporta.

**Tech Stack:** Rust 1.98 (edition 2024), Elm 0.19.2, node (testes), PostgreSQL (teste ponta a ponta opcional).

**Spec:** `docs/superpowers/specs/2026-10-11-elm-client-from-ir-design.md`

## Global Constraints

- Rust 1.98.0, edition 2024; o `bindc` não ganha dependências de produção novas.
- Elm 0.19.2. O `elm.json` gerado tem as mesmas dependências de hoje; só `elm/url` 1.0.0 passa de indireta para direta. Nenhum pacote Elm novo.
- O Elm gerado e o runtime Elm compilam com `elm make --optimize` (portanto sem `Debug.*`) e não contêm `Debug.todo`, recursão sem `Task` em caminhos de profundidade ilimitada, nem funções parciais.
- `maxDepth = 10000`, contado só na entrada de funções Bind e lambdas (nunca em builtins), igual ao servidor e ao `core::eval`.
- `Int` canônico no fio e na URL: `-?[0-9]+`, sem zeros à esquerda, sem `-0`, no máximo 1000 dígitos.
- `Decimal`/`Money` no cliente: mantissa < 2^96, escala ≤ 28, `add`/`sub`/`neg` checados (`Overflow`); `*` e `/` recusados com E8311.
- Perfil `rust-elm-web-v3`, contrato `bind-json-v3`, documentação `docs/certified-profile-v0.3.md`.
- Diagnóstico novo: E8311. Removido: E8306.
- Testes que precisam de `elm`/`node` são `#[ignore]` e, sem as ferramentas, imprimem `skipped: elm/node not found` e passam. Testes com banco usam `BIND_TEST_DATABASE_URL` e imprimem `skipped: BIND_TEST_DATABASE_URL not set` sem ela.
- Depois de cada tarefa: `cargo test`, `cargo clippy --all-targets -- -D warnings` e `cargo fmt --check` passam.
- Commits terminam com `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Branch `elm-client`. Comandos de geração usam `DATABASE_URL=postgresql://bind:bind@localhost/bind` (o `certify` não conecta).
- Não modifique o repositório `../website`.

## Review Focus

1. Parâmetro de URL com codificação percentual e `+` (`?nome=Jo%C3%A3o+Silva`) é decodificado corretamente; parâmetro repetido (`?id=a&id=b`) dá `BadParameter` (Task 6).
2. Servidor respondendo 500 com corpo que não é JSON, ou rede indisponível, vira a tela de erro com mensagem genérica, nunca um travamento (Tasks 6 e 8).
3. `map`/`fold` sobre uma lista de 100.000 itens no cliente não estoura a pilha do JavaScript (Task 3).
4. `Text` com `<script>alert(1)</script>` aparece como texto escapado, nunca como HTML (Task 3).
5. Tipos e funções do usuário chamados `Model`, `Msg`, `Node`, `main`, `view`, `init`, `type`, `case` compilam e não colidem com o código gerado (Task 5).

---

### Task 1: UI na IR, primitivas de exibição e lowering do cliente

**Files:**
- Modify: `src/core/ir.rs`, `src/core/lower.rs`, `src/core/validate.rs`, `src/core/specialize.rs` (se percorrer `ExprKind`), `src/backend/rust/expr.rs` (continuar recusando `Ui`)

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
```
- `core::lower::client_functions(program: &CoreProgram, component: &str) -> Vec<String>`: o componente mais as funções alcançáveis por `Global`, em ordem determinística por nome.
- Regras:
  - O bloco de filhos de um nó vira uma expressão `List<Ui>`: cada filho é um elemento de `listOf`; um `val` no bloco vira `Let` envolvendo o restante da lista; um filho expressão-statement que não seja `Ui` é avaliado com `Seq`. Nó sem bloco → `emptyList()` tipado `List<Ui>`.
  - `if` sem `else` aceito pelo verificador dentro da UI → `If` com `else_branch` = `Ui { node: Empty, args: [], children: [] }`.
  - `Text(e)` → `Ui { node: Text, args: [Prim(Display*, [e])], children: [] }`, com a primitiva escolhida pelo tipo de `e` (tabela da spec, seção 1; distinct usa a primitiva do tipo base, extraindo o valor com `Field`/padrão como a lowering já faz para distinct).
  - `ServerCall` mantém os argumentos escritos.
  - Validador: um `Ui` precisa ter `ty == Ui`; `children` com tipo `List<Ui>`; argumentos por nó iguais às regras de `infer_ui` (`Text` exatamente 1 argumento `String` — já convertido pela primitiva — e `children` vazio); `Display*` com operando do tipo esperado e resultado `String`.
- [ ] **Step 1: Testes que falham** (`src/core/lower.rs`, módulo de testes):
  - `client component C() { UI { Section("a", "b") { val x = "t"\n Text(x)\n Text(1) } } }` → corpo `Ui{Ui, [], [Ui{Section, ["a","b"], Let(x, …, listOf(Ui{Text,[DisplayString(Local x)]}, Ui{Text,[DisplayInt(1)]}))}]}`.
  - `if (b) Text("s")` sem `else` dentro da UI (se o verificador aceitar; caso contrário, um teste que mostra o diagnóstico atual) → `else_branch` `Empty`.
  - `Text` de `Boolean`, `Double`, `Money`, `UUID`, de um distinct sobre `Int` → primitiva certa.
  - `client fun c(id: CustomerId) = server.getCustomer(id)` → `ServerCall{"getCustomer", [Local id]}`.
  - `client_functions` inclui uma função compartilhada chamada pelo componente e exclui uma não alcançável.
  - Validador (`src/core/validate.rs`): IRs montadas à mão e inválidas, com mensagem contendo a palavra-chave: `Text` com 2 argumentos ("arity"); `children` do tipo `List<Int>` ("children"); `DisplayInt` sobre `String` ("operand").
- [ ] **Step 2: Rodar e confirmar a falha.** `cargo test core::`
- [ ] **Step 3: Implementar** as regras acima. Atualize todo `match` sobre `ExprKind` (specialize, validate, eval, backend Rust). No backend Rust, `Ui` e `Display*` continuam inalcançáveis por causa da E8310; trate-os como erro interno do plano, nunca com pânico.
- [ ] **Step 4: Rodar.** `cargo test`, clippy, fmt; `make -C ../website check` → código 0.
- [ ] **Step 5: Commit.** `git commit -m "feat(core): árvore de UI e primitivas de exibição na IR"`

---

### Task 2: Avaliador com UI e `Double`

**Files:**
- Modify: `src/core/eval.rs`

**Interfaces:**
- Consumes: Task 1.
- Produces: `Value::Ui { node: UiNode, args: Vec<Value>, children: Vec<Value> }`, `Value::Double(f64)`; `Value::render` estendido:
  - `Ui` → `Nó(arg, …){filho, …}`, com `{}` omitido sem filhos e `Empty` renderizado como `Empty`;
  - `Double` → a forma de `Number.prototype.toString` do JavaScript (`1`, `0.1`, `1e+21`, `-0` → `0`, `NaN`, `Infinity`, `-Infinity`).
- Regras:
  - `Display*` produz `Value::Str` com a forma da spec. `DisplayDouble` usa a função `js_number_to_string(f64) -> String`, implementada neste arquivo seguindo o algoritmo Number::toString do ECMAScript (o menor número de dígitos que faz ida e volta, com notação exponencial para expoentes ≥ 21 ou ≤ -7).
  - `Double*` com semântica IEEE do Rust (`f64`), que é a mesma do JavaScript.
  - `ServerCall` continua `Unsupported` (o teste diferencial não usa servidor).
- [ ] **Step 1: Testes que falham:**
  - `js_number_to_string`: `0.1+0.2` → `"0.30000000000000004"`; `1e21` → `"1e+21"`; `1e-7` → `"1e-7"`; `123456789012345680000.0` → `"123456789012345680000"`; `-0.0` → `"0"`; `5e-324` → `"5e-324"`; `f64::NAN` → `"NaN"`; `1.0/0.0` → `"Infinity"`.
  - `fun f() = UI { Div { Text(1 + 1) } }` avaliado → render `"UI(){Div(){Text(\"2\")}}"` (ajuste a forma exata à regra acima e documente-a no doc comment de `render`).
  - `fun f(): Double = 0.1 + 0.2` → render `0.30000000000000004`.
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** `cargo test core::eval`, `cargo test`, clippy, fmt.
- [ ] **Step 5: Commit.** `git commit -m "test(core): avaliador com UI, exibição e Double"`

---

### Task 3: Runtime Elm

**Files:**
- Create: `runtime/bind-runtime-elm/src/BindRuntime.elm`, `runtime/bind-runtime-elm/elm.json`, `runtime/bind-runtime-elm/tests/Harness.elm`, `runtime/bind-runtime-elm/tests/run.mjs`, `src/backend/elm/mod.rs` (só `pub const RUNTIME: &str = include_str!(…)` e o módulo de teste), `src/backend/mod.rs` (`pub mod elm;`)
- Modify: `Makefile` (alvo `test-elm-runtime`)

**Interfaces:**
- Produces (módulo `BindRuntime`):
```elm
type Int            -- opaco; sempre normalizado
intFromString : String -> Maybe Int        -- forma canônica, ≤ 1000 dígitos
intToString : Int -> String
intFromLiteral : Bool -> List Int -> Int   -- negativo, dígitos base 10^7 menos significativo primeiro (usado pelo gerador)
intAdd, intSub, intMul : Int -> Int -> Int
intNeg : Int -> Int
intDiv, intRem : Int -> Int -> Maybe Int   -- truncam para zero; resto com sinal do dividendo
intCompare : Int -> Int -> Order
intToIndex : Int -> Maybe Basics.Int

type Decimal        -- opaco; normalizado (sem zeros finais na fração)
decimalFromString : String -> Maybe Decimal   -- forma do serde-str do rust_decimal
decimalToString : Decimal -> String
decimalAdd, decimalSub : Decimal -> Decimal -> Result BindError Decimal
decimalNeg : Decimal -> Decimal
decimalCompare : Decimal -> Decimal -> Order

type BindError = RecursionLimit | Overflow | Remote String | BadParameter String
type BindResult e t = Ok t | Err e
type Ctx            -- opaco
ctxNew : Ctx
maxDepth : Basics.Int   -- 10000
enter : Ctx -> Task BindError Ctx

first : List a -> Maybe a
get : List a -> Int -> Maybe a
length : List a -> Int
listMap : Ctx -> (Ctx -> a -> Task BindError b) -> List a -> Task BindError (List b)
listFold : Ctx -> (Ctx -> b -> a -> Task BindError b) -> b -> List a -> Task BindError b

type Node = Node String (List NodeArg) (List Node) | TextNode String | EmptyNode
type NodeArg = ArgString String | ArgInt Int
render : Node -> Html msg

intDecoder : Json.Decode.Decoder Int
encodeInt : Int -> Json.Encode.Value
decimalDecoder : Json.Decode.Decoder Decimal
encodeDecimal : Decimal -> Json.Encode.Value
errorMessage : BindError -> String
```
- Regras:
  - `listMap`/`listFold` encadeiam com `Task.andThen`, sem recursão na pilha do JS proporcional ao tamanho da lista (use recursão em cauda que devolve `Task` a cada passo, ou um laço sobre `Task.sequence` em blocos).
  - `render` reproduz exatamente o mapeamento de `elm_ui_node` em `src/certified.rs` (tags, classes, `Heading` nível 1–3, `Link` com href/classe/target) e usa `Html.text` para texto, que escapa HTML.
- [ ] **Step 1: Testes que falham** — o harness (`tests/Harness.elm` compilado com `elm make --optimize` e executado por `tests/run.mjs` no node) lê casos de um JSON e escreve resultados; um teste Rust `#[ignore]` em `src/backend/elm/mod.rs` gera os casos, roda o harness e compara:
  - `Int`: 10.000 casos aleatórios (semente fixa, operandos de 0 a 60 dígitos, sinais mistos) por operação (`add`, `sub`, `mul`, `div`, `rem`, `compare`, `toString∘fromString`) contra o `BigInt` do JavaScript calculado em `run.mjs`.
  - `intFromString` recusa `"01"`, `"-0"`, `"+1"`, `""`, `" 1"`, `"1.5"`, 1001 dígitos; aceita 1000 dígitos.
  - `Decimal`: vetores gerados pelo teste Rust com `rust_decimal` (dev-dependency já disponível no runtime Rust? Se não estiver acessível ao `bindc`, gere os vetores com um pequeno binário de teste dentro de `runtime/bind-runtime` e grave-os em `runtime/bind-runtime-elm/tests/decimal-vectors.json`): `add`/`sub` com resultado esperado ou `Overflow`, `toString`/`fromString`.
  - `enter` aplicado 10.000 vezes → `Ok`; 10.001 → `RecursionLimit`.
  - **Review Focus 3:** `listMap` e `listFold` sobre 100.000 itens terminam sem `RangeError`.
  - **Review Focus 4:** `render (TextNode "<script>alert(1)</script>")` serializado (via o harness, que renderiza com `elm/html` para string usando um `Browser`-less `view` impresso pelo DOM simulado do node, ou verificando o `VirtualDom` gerado) contém `&lt;script&gt;`; documente o método escolhido.
- [ ] **Step 2: Rodar e confirmar a falha.** `cargo test backend::elm -- --ignored --nocapture`
- [ ] **Step 3: Implementar** o runtime.
- [ ] **Step 4: Rodar.** O teste ignorado, `make test-elm-runtime`, `cargo test`, clippy, fmt.
- [ ] **Step 5: Commit.** `git add runtime/bind-runtime-elm src Makefile && git commit -m "feat(runtime): runtime Elm com Int exato e Decimal checado"`

---

### Task 4: Backend Elm — tipos e codecs a partir da IR

**Files:**
- Create: `src/backend/elm/types.rs`, `src/backend/elm/names.rs`
- Modify: `src/backend/elm/mod.rs`

**Interfaces:**
- Consumes: Tasks 1 e 3.
- Produces:
  - `names::elm_value(name: &str) -> String` e `names::elm_type(name: &str) -> String`: escape determinístico e injetivo (prefixo `u_`/`U` para todo nome do usuário; caracteres fora de `[A-Za-z0-9]` codificados como `_x<hex>_`; `_` vira `__`).
  - `types::elm_type(ty: &Type, program: &CoreProgram) -> String`, `types::declarations(program: &CoreProgram) -> String` (tipos do usuário), `types::decoder(ty) -> String`, `types::encoder(ty) -> String` e as funções `decode<Nome>`/`encode<Nome>` por tipo declarado.
- Regras:
  - Mapeamento da tabela da spec (seção 2). JSON igual ao do servidor: registros como objetos com os nomes de campo originais; variantes sem campos como string `lower_camel` (reuse `certified::lower_camel`); distinct transparente; `Int`/`Decimal`/`Money` como string pelos codecs do runtime; `T?` como `null`.
  - Os codecs leem só `Type` da IR (nenhum `TypeRef` da AST).
- [ ] **Step 1: Testes que falham:** snapshot `tests/codegen/client_types.bd` → `client_types.elm.expected` com registro, sealed sem campos, distinct sobre `Int` e `List<Int>`, `Int?`, `List<Money>`; um teste que confere a injetividade de `elm_value` (`a_b` vs `a-b` vs `aB`) e que palavras reservadas do Elm (`type`, `case`, `of`, `let`, `in`, `if`, `then`, `else`, `module`, `import`, `exposing`, `as`, `port`) nunca saem sem prefixo.
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** `cargo test backend::elm`, `BIND_BLESS=1` para gravar e revisar o snapshot, clippy, fmt.
- [ ] **Step 5: Commit.** `git commit -m "feat(elm): tipos e codecs gerados da IR"`

---

### Task 5: Backend Elm — expressões e funções (convenção `Task`)

**Files:**
- Create: `src/backend/elm/expr.rs`, `tests/codegen/client_*.bd`, `tests/codegen/client_*.elm.expected`
- Modify: `src/backend/elm/mod.rs`

**Interfaces:**
- Consumes: Tasks 1, 3, 4.
- Produces:
  - `expr::function(function: &CoreFunction, program: &CoreProgram) -> String`.
  - `backend::elm::render_app(program: &CoreProgram, names: &[String]) -> String`: o módulo `App.elm` com tipos, codecs e as funções pedidas.
  - `backend::elm::compile_check(dir: &Path)` (teste): monta `elm.json` + `BindRuntime.elm` + `App.elm` e roda `elm make --optimize`.
- Regras (spec, seção 2):
  - Função: `u_f : Ctx -> A -> Task BindError R`, corpo `BindRuntime.enter ctx |> Task.andThen (\ctx1 -> …)`; `Return` como falha interna capturada na fronteira (`Task.onError`) — implemente com um tipo interno `Flow r = Fail BindError | Returned r` dentro da função e converta na saída, para que `BindError` público não ganhe variante de controle.
  - `Let`/`Seq` → `Task.andThen`; `If`/`Match` → `if`/`case`; `Match` com literais → cadeia de `if`; Option/`BindResult`/tipos do usuário → `case` com padrões pelos `binding_paths`.
  - Primitivas `Int*`/`Decimal*`/`Display*` → runtime; `Double*` → operadores; `StringConcat` → `++`; comparações → `==`, `/=`, `compare`.
  - Literais `Int` → `BindRuntime.intFromLiteral neg [dígitos base 10^7]`, calculados pelo compilador sem dependências.
  - `ServerCall` → chamada a uma função gerada `rpc_<nome> : Ctx -> args -> Task BindError R` (implementada na Task 6; aqui emita a chamada e, nos snapshots puros, garanta que não aparece).
  - `Ui` → construção de `BindRuntime.Node`; argumentos `String` → `ArgString`, `Int` → `ArgInt`.
- [ ] **Step 1: Testes que falham:**
  - Snapshots: `client_arith.bd` (`Int` grande, `/` e `%` com `?:`), `client_adt.bd` (sealed recursivo + `when`), `client_lambdas.bd` (`map`/`fold`), `client_ui.bd` (UI com `val` nos filhos, `if`/`when`, `Text` de cada tipo exibível), `client_names.bd` (**Review Focus 5**: tipos e funções `Model`, `Msg`, `Node`, `main`, `view`, `init`, `type`, `case`).
  - `#[ignore]`: `compile_check` sobre todos os snapshots `client_*` → `elm make --optimize` sem erro.
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** `cargo test backend::elm`, `cargo test backend::elm -- --ignored`, clippy, fmt.
- [ ] **Step 5: Commit.** `git commit -m "feat(elm): tradução da IR para Elm com Task uniforme"`

---

### Task 6: Página, parâmetros da URL, plano do cliente (E8311) e integração ao `certify`

**Files:**
- Create: `src/backend/elm/page.rs`, `src/backend/elm/plan.rs`
- Modify: `src/backend/elm/mod.rs`, `src/certified.rs`, `src/compiler.rs`, `src/pipeline.rs`

**Interfaces:**
- Consumes: Tasks 1–5.
- Produces:
  - `plan::client_problems(program: &CoreProgram, app: &VerifiedApplication) -> Vec<ClientProblem>` (mesmo código usado por `client_files`), convertido em E8311 por `verify` para targets com `client = "elm"`.
  - `backend::elm::client_files(app: &VerifiedApplication) -> Result<Vec<(String, String)>, String>`: `web/elm.json`, `web/src/BindRuntime.elm`, `web/src/App.elm`, `web/src/Main.elm`.
- Regras:
  - `Main.elm`: `Browser.element` (ou `Browser.application` se necessário para ler a URL; prefira `Browser.element` com a URL passada por flag do `index.html` como `window.location.href`), `type Model = Loading | Failed BindError | Loaded Node`, `init` decodifica os parâmetros com `Url.Parser.Query` e o decodificador textual de cada tipo; parâmetro ausente, repetido ou inválido → `Failed (BadParameter nome)`; senão roda `Task.attempt` sobre a `Task` do componente.
  - `rpc_<nome>`: `Http.task` com POST na rota de `app.rpc_contracts`, corpo JSON codificado com os encoders da Task 4 (objeto com os nomes dos parâmetros do endpoint), `resolver` que devolve `Remote <mensagem>` lendo `{"error": …}` em status não 2xx e `Remote "Request failed"` (mensagem genérica fixa) para corpo não-JSON, rede, timeout ou decodificação.
  - Telas: "Loading…"; erro com `BindRuntime.errorMessage`; carregado com `div [class "bind-site"] [render node]`.
  - E8311 (mensagens começando com o nome da função ou do componente): aritmética de `Float`; `DecimalMul`/`DecimalDivChecked`; parâmetro de componente fora de `String`, `Email`, `Url`, `Int`, `UUID`, `Date`, `DateTime`, `Instant` e distinct sobre eles; target web sem componente cliente. A E8301 (mais de um componente cliente) continua como está.
  - O teste `client_int_decoder_runs_in_elm` (que exige recusa acima de 2^53) é substituído por ida e volta de `Int` canônicos grandes (20, 100 e 1000 dígitos) e recusa de 1001 dígitos e das formas não canônicas.
  - Remova: `component_plan`, `ComponentPlan`, `UiNode`/`UiArgument` antigos, `parse_ui_nodes`, `parenthesized`, `elm_main`, `elm_static_main`, `elm_ui_node`, `elm_ui_text`, os helpers de codec Elm baseados na AST, a E8306 e os casos especiais E8301/E8303 do cliente em `compiler.rs`.
  - `PROFILE_VERSION = "rust-elm-web-v3"`, `WIRE_VERSION = "bind-json-v3"`; `ELM_JSON` com `elm/url` 1.0.0 direto; lock e manifesto atualizados.
- [ ] **Step 1: Testes que falham:**
  - `compiler.rs`: cada caso da E8311 (um por regra); o `commerce` e o site continuam sem diagnósticos.
  - **Review Focus 1:** teste `#[ignore]` que compila o `Main.elm` do `commerce` e roda a função de leitura de parâmetros no node com `?id=<uuid>` (válido), `?id=a&id=b` (`BadParameter "id"`), sem `id` (`BadParameter "id"`) e, num componente de teste com parâmetro `String`, `?nome=Jo%C3%A3o+Silva` → `"João Silva"`.
  - Site: o HTML renderizado do `BindWebsite` (render do `Node` no node) é igual ao HTML que o gerador atual produz. Grave o HTML de referência **antes** de remover o gerador antigo, como `tests/codegen/website.html.expected`, gerado a partir do `../website/src/site.bd` atual.
  - `certify --target vercel` no `commerce` (compila o Elm) → sucesso.
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** `cargo test`, `cargo test -- --ignored`, clippy, fmt; os dois `certify`; `make -C ../website check`; `certify` do site com `--out /tmp/bind-website-v3` (apague depois; se o comando reescrever `../website/bind.lock`, reverta com `git -C ../website checkout -- bind.lock` e reporte).
- [ ] **Step 5: Commit.** `git commit -m "feat(elm): cliente gerado da IR, parâmetros da URL e perfil rust-elm-web-v3"`

---

### Task 7: Teste diferencial do cliente

**Files:**
- Create: `src/backend/elm/differential.rs` (`#[cfg(test)]`)
- Modify: `src/backend/rust/differential.rs` (extrair o gerador para reuso, sem mudar a saída do teste do servidor), `src/backend/elm/mod.rs`, `Makefile` (`test-generated` inclui o novo teste)

**Interfaces:**
- Consumes: Tasks 1–5; o gerador de `src/backend/rust/differential.rs`.
- Regras:
  - O gerador ganha o tipo de retorno `Ui` e expressões de UI: nós com argumentos `String`/`Int`, `Text` de `Int`, `String`, `Boolean`, `Double` e de um distinct sobre `Int`, `if`/`when` na UI, `val` nos filhos; e expressões `Double` (`+ - * /` com literais finitos).
  - 1.000 programas avaliados por `core::eval`; os mesmos renderizados num `App.elm` e executados por um `Main` de teste no node (`Platform.worker` que roda cada `Task` em sequência e imprime `p<i>=<render>` via porta, na mesma forma canônica de `Value::render`).
  - Inclui a recursão de 20.000 níveis: `p<i>=!RecursionLimit` nos dois lados, sem `RangeError` (**profundidade de 10.000 no navegador**).
  - Qualquer divergência falha com o programa, o esperado e o obtido. Nunca reduza a quantidade nem tire construções; corrija o código.
- [ ] **Step 1:** escrever o teste.
- [ ] **Step 2:** `cargo test elm_differential -- --ignored --nocapture` e corrigir o código até 0 divergências; cada bug real ganha um teste de regressão próprio.
- [ ] **Step 3:** `cargo test`, clippy, fmt; o diferencial do servidor continua passando.
- [ ] **Step 4: Commit.** `git commit -m "test(elm): teste diferencial avaliador × Elm gerado"`

---

### Task 8: Ponta a ponta — servidor Rust real e cliente Elm no node

**Files:**
- Create: `src/backend/elm/end_to_end.rs` (`#[cfg(test)]`, `#[ignore]`)

**Interfaces:**
- Consumes: Tasks 6 e 7; os helpers de banco e de crate de `src/backend/rust/transactions.rs`.
- Regras:
  - Com `BIND_TEST_DATABASE_URL`: cria um schema temporário, gera o `commerce`, sobe o servidor Rust gerado numa porta local (um binário de teste que chama `handle_rpc` atrás de um servidor HTTP mínimo do próprio crate de teste) e executa o `Main.elm` compilado no node com `XMLHttpRequest` provido por um polyfill escrito no próprio `run.mjs` (sem pacotes npm).
  - Casos: cliente com `?id=<existente>` → `Loaded` com o nome e o e-mail; `?id=<inexistente>` → `Failed (Remote "<mensagem pública de NotFound>")`; `?id=xyz` → `Failed (BadParameter "id")`.
  - **Review Focus 2:** um endpoint de teste cujo servidor responde 500 com corpo `oops` (não JSON) e o servidor desligado → `Failed (Remote "Request failed")`.
  - Schema removido mesmo em falha; sem a variável, imprime a linha de pulo e passa.
- [ ] **Step 1:** escrever o teste.
- [ ] **Step 2:** rodar com a variável e corrigir até passar.
- [ ] **Step 3:** `cargo test`, clippy, fmt.
- [ ] **Step 4: Commit.** `git commit -m "test(elm): ponta a ponta servidor Rust × cliente Elm"`

---

### Task 9: Documentação

**Files:**
- Create: `docs/certified-profile-v0.3.md`
- Modify: `docs/architecture.md`, `README.md`, `docs/certified-profile-v0.2.md` (nota no topo apontando para a v0.3)

- [ ] **Step 1:** `docs/certified-profile-v0.3.md` no formato do v0.2: `rust-elm-web-v3`; cliente gerado da IR (UI real, expressões tipadas, várias chamadas `server.f` em sequência, parâmetros pela URL, telas padrão); runtime Elm; `Int` exato no cliente; `bind-json-v3`; E8311; o que a certificação garante e não garante (sem eventos nem estado; `Float` e `*`/`/` de `Decimal` só no servidor).
- [ ] **Step 2:** `docs/architecture.md` com o backend Elm e o plano do cliente; `README.md` com o perfil v3.
- [ ] **Step 3:** verificação final: `cargo test`, `cargo test -- --ignored` (com `BIND_TEST_DATABASE_URL`), testes do runtime Rust, clippy dos dois crates, fmt, os dois `certify`, o `check` do site.
- [ ] **Step 4: Commit.** `git commit -m "docs: perfil certificado rust-elm-web-v3"`

Mapa dos critérios de aceite da spec:

| Critério | Coberto por |
|---|---|
| 1. Backend Elm lê só a `CoreProgram`; tokens do cliente saem | Tasks 1, 4, 5, 6 |
| 2. Nenhum programa verificado falha no `generate` pelo cliente | Task 6 |
| 3. `Int` exato no cliente; limite de 2^53 sai | Tasks 3, 4, 6 |
| 4. Diferencial ≥ 1.000 programas, 0 divergências, recursão 20.000 | Task 7 |
| 5. `certify` nos dois targets e no site; HTML do site inalterado | Task 6 |
| 6. Testes, clippy, fmt, documentação v0.3 | Task 9 |
