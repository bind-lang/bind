# Frontend web (HTML/CSS/JS) e SSR em Rust a partir da IR tipada

- Data: 2026-10-11
- Status: aprovado em conversa, aguardando revisão da spec escrita
- Subprojeto: 3c. Depende de 3a/3b (IR tipada e servidor Rust, já no `main`). Substitui a spec do cliente Elm (`2026-10-11-elm-client-from-ir-design.md`), descartada. 3d (remover os scanners de tokens restantes e ampliar o perfil) terá spec própria.

## Objetivo

Hoje o cliente é gerado em Elm a partir de uma varredura de tokens. A árvore de UI escrita pelo usuário é ignorada no caso dinâmico, `Text` só aceita um literal ou `binding.campo`, e a IR transforma qualquer nó de UI num `Ui` vazio.

Este subprojeto troca o Elm por código nativo do navegador e faz o frontend ser gerado só a partir da `CoreProgram`, como o servidor. Existe uma única pilha suportada: frontend HTML/CSS/JavaScript e backend Rust, implantável de dois modos:

- **SSR (uma aplicação):** o servidor Rust serve as páginas já renderizadas e os endpoints RPC.
- **Separado (duas aplicações):** frontend estático (HTML, CSS e JS) chamando a API Rust por HTTP.

Nos dois modos a página sai igual, e as garantias do verificador (tipos, `when` exaustivo, efeitos, `Int` exato) chegam ao código que roda.

### Decisões tomadas

| Decisão | Escolha |
|---|---|
| Pilha | Uma só: frontend HTML/CSS/JS + backend Rust; o Elm sai |
| Modos | `mode = "ssr"` ou `mode = "split"` no target |
| Escopo da linguagem | Fiel ao que existe: árvore de UI real, expressões tipadas, `if`/`when` na UI, várias chamadas `server.f`. Sem eventos nem estado no cliente (subprojeto próprio) |
| Parâmetros do componente | Lidos da query string com os decodificadores estritos do fio |
| Falhas de RPC | Implícitas: telas padrão de carregando e erro |
| Abordagem | Tradução direta da IR: funções `async` em JS e, para o SSR, Rust com renderização HTML no runtime |
| SSR | Feito pelo próprio servidor Rust, que chama as funções diretamente, sem HTTP; a página SSR não carrega JS por enquanto |
| Deployments | aws-lambda e vercel, cada um com os dois modos |

### Fora do escopo

- Eventos, estado no cliente, formulários, navegação entre páginas e hidratação.
- `*` e `/` de `Decimal`/`Money` no código de componentes (E8311 nos dois modos).
- Automatizar hospedagem estática na AWS (S3/CloudFront ficam documentados).
- Remover os scanners de tokens que não pertencem ao cliente (3d).

## 1. UI na IR

```text
ExprKind::Ui { node: UiNode, args: [Expr], children: Expr }   // children: List<Ui>
UiNode = enum fechado: Ui, Page, Container, Div, Grid, Stack, Header, Nav, Main, Section,
         Article, Aside, Footer, Paragraph, Span, Strong, Emphasis, Small, List, Item, Pre,
         Code, Heading, Link, Rule, Text, Empty
```

- O bloco de filhos é baixado para uma expressão `List<Ui>` feita de `Let` e `listOf`; um `val` no bloco vale para os filhos seguintes.
- `if`/`when` com tipo `Ui` são preservados; `if` sem `else` aceito pelo verificador recebe o nó `Empty`, que não renderiza nada.
- `Text(e)` recebe uma primitiva de exibição concreta por tipo:

| Tipo | Primitiva | Forma exibida |
|---|---|---|
| `String`, `Email`, `Url` | `DisplayString` | o próprio texto |
| `Int` | `DisplayInt` | decimal canônico |
| `Long` | `DisplayLong` | decimal |
| `Double` | `DisplayDouble` | `Number.prototype.toString` do JavaScript |
| `Float` | `DisplayFloat` | igual a `Double`, sobre o valor f32 |
| `Boolean` | `DisplayBool` | `true`/`false` |
| `Decimal`/`Money` | `DisplayDecimal` | forma canônica do `serde-str` |
| `UUID`, datas | `DisplayText` | forma canônica ISO do fio |
| distinct | primitiva do tipo base | igual ao tipo base |

- `ServerCall { function, args }` carrega os argumentos escritos.
- `core::lower::client_functions(program, component)`: o componente e as funções alcançáveis por `Global`, em ordem determinística.
- O validador confere cada `Ui` (aridade e tipos dos argumentos pelas regras de `infer_ui`, filhos `List<Ui>`, `Text` com um argumento já convertido e sem filhos) e as primitivas `Display*`.
- `core::eval` ganha `Value::Ui` e `Value::Double`, com forma canônica de impressão; `DisplayDouble` usa `js_number_to_string`, que replica o algoritmo Number::toString do ECMAScript. A mesma função fica no runtime Rust.
- A E8310 continua impedindo UI alcançável a partir de um endpoint. O backend Rust passa a traduzir `Ui` para o SSR das páginas.

## 2. Frontend JavaScript (modo separado)

### Saída

`web/index.html`, `web/app.js` (código do usuário), `web/bind-runtime.js` (runtime copiado de `runtime/bind-runtime-js/`, com hash) e `web/styles.css`. Módulos ES nativos; sem npm, sem bundler, sem etapa de build.

### Convenção

```js
async function f(ctx, a, b) {
  ctx = await enter(ctx);   // RecursionLimit acima de 10.000; o await deixa a pilha curta
  ...
}
```

- `enter` conta só a entrada de funções Bind e lambdas, nunca builtins, como no servidor e no avaliador. O `await` faz cada nível retomar num microtask, de modo que 10.000 níveis não estouram a pilha do JavaScript.
- Expressões são traduzidas para instruções com variáveis temporárias; o `Return` da IR vira `return`, já que cada função Bind é uma função JS.
- Nomes do usuário recebem prefixo e escape determinístico e injetivo, sem colidir com palavras reservadas do JS nem com os nomes do runtime.

### Representação

| Bind | JS |
|---|---|
| `Int` | `BigInt` |
| `Double` | `number` |
| `Float` | `number`, com `Math.fround` depois de cada operação (IEEE f32 exato) |
| `Boolean` | `boolean` |
| `String`, `Email`, `Url`, `UUID`, datas | string canônica |
| `T?` | `null` ou o valor; `{some: v}` só quando o tipo interno também é opcional |
| `data class` / `entity` | objeto congelado `{campo: …}` |
| `sealed` / `enum` | objeto congelado `{tag: "Variante", …campos}` |
| `Result` | `{tag: "Ok", value}` / `{tag: "Err", value}` |
| `List<T>` | array congelado |
| `Decimal` / `Money` | objeto do runtime: mantissa `BigInt` e escala |
| função | `async (ctx, …) => …` |

### Operações

- `Int`: `BigInt` com `div`/`rem` do runtime (truncam para zero, resto com o sinal do dividendo, `null` para divisor zero).
- `Decimal`/`Money`: exibir, comparar, `+`, `-` e negação exatos e checados com os limites do `rust_decimal` (mantissa < 2^96, escala ≤ 28; estouro → `Overflow`). `*` e `/` dão E8311.
- Igualdade e ordem estruturais por `equals`/`compare` do runtime.
- `when` → `switch` sobre `tag` ou cadeia de `if`; builtins do runtime (`map`/`fold` assíncronos).
- `server.f(args)` → `await rpc(ctx, "/api/f", corpo, decodificador)` via `fetch`, com codecs gerados dos tipos da IR e estritos nos dois sentidos. Erro HTTP com `{"error": …}` → `Remote(mensagem)`; rede, corpo não-JSON ou resposta mal formada → `Remote("Request failed")`.

### UI e página

- Nós de UI são objetos do runtime; a renderização usa `document.createElement` e `textContent`, nunca `innerHTML`.
- Boot: lê `URLSearchParams` com os decodificadores estritos (ausente, repetido ou inválido → `BadParameter`), mostra "Loading…", avalia o componente e renderiza o resultado ou a tela de erro. Uma exceção que não seja `BindError` indica bug do compilador e mostra "Internal error".
- Componente estático (sem parâmetros e sem `server.f`): avaliado em tempo de compilação; `index.html` sai como HTML pronto, sem JS.

## 3. SSR em Rust, configuração e deployments

### `bind.toml`

```toml
[target.vercel]
client = "web"
server = "rust"
mode = "ssr"            # ou "split"
deployment = "vercel"   # ou "aws-lambda"
api_url = "/api"        # só no modo split; padrão "/api"
frontend_origin = "https://app.exemplo.com"   # exigido quando api_url é de outra origem
```

`client = "elm"` dá E8311 com a instrução de troca.

### SSR

- O runtime Rust ganha `Node`, `render_html(&Node) -> String` (com escape de HTML) e as funções de exibição, incluindo `js_number_to_string`.
- O backend Rust traduz `Ui` e `Display*`; as chamadas `server.f` dentro do componente viram chamadas diretas às funções Rust.
- `GET /` serve o componente da aplicação (um componente por aplicação, E8301):
  1. decodifica os parâmetros da query string (inválido → página de erro, status 400);
  2. abre a transação;
  3. avalia o componente;
  4. commit em sucesso, rollback em erro;
  5. responde com o HTML completo. Status: 404 para `NotFound`, 400 para parâmetro inválido, 500 para os demais, sempre com a mensagem pública.
- Mesmo invólucro, mesmas telas, mesmos textos e mesmo CSS do modo separado. A página SSR não carrega JS.
- Componente estático: pré-renderizado em tempo de compilação nos dois modos e servido como arquivo estático.

### Modo separado e origens

- O frontend chama a API em `api_url`.
- Com `api_url` de outra origem, o backend responde CORS só para `frontend_origin` e recusa as demais; sem `frontend_origin`, E8311.

### Deployments

| | SSR | Separado |
|---|---|---|
| vercel | função Rust serve páginas e `/api` | estáticos + funções `/api` no mesmo projeto (mesma origem) |
| aws-lambda | Lambda serve páginas e `/api` | Lambda serve `/api`; `web/` pronto para hospedagem estática (documentada) |

## 4. Diagnósticos, perfil e testes

### Fluxo

```text
verify → types::check → core::lower → core::validate
       → plano do servidor (E8309/E8310) + plano do frontend (E8311)
certify/generate → backend Rust (API + SSR) e backend JS, ambos só a partir da CoreProgram
```

O plano do frontend é o mesmo código que o backend JS usa. Nenhum programa verificado falha no `generate`. As restrições do frontend valem nos dois modos, para que trocar de modo nunca mude quais programas são aceitos.

### E8311

- `*` ou `/` de `Decimal`/`Money` alcançável a partir do componente.
- Parâmetro de componente fora de `String`, `Email`, `Url`, `Int`, `UUID`, `Date`, `DateTime`, `Instant` e distinct sobre esses tipos.
- Target web sem componente cliente.
- `api_url` de outra origem sem `frontend_origin`.
- `client = "elm"`.

### O que sai

- Todo o gerador Elm: `elm.json`, `Main.elm`, codecs Elm, o decodificador de `Int` com limite de 2^53, os testes com `elm make`.
- A análise por tokens do cliente: `component_plan`, `parse_ui_nodes`, a varredura de `Text`.
- A E8306 e os casos especiais E8301/E8303 do formato antigo.

### Perfil

- `rust-web-v3`, contrato `bind-json-v3` (mesmo formato no fio; o frontend aceita qualquer `Int` canônico de até 1000 dígitos).
- `docs/certified-profile-v0.3.md`; v0.1 e v0.2 ficam como histórico.
- `website/` (outro repositório) precisa trocar `client = "elm"` por `client = "web"` e escolher `mode`, e ter `deploy/` regenerado depois do merge.

### Testes

TDD, como nos subprojetos anteriores. Testes que precisam de `node` são `#[ignore]` e, sem a ferramenta, imprimem a linha de pulo e passam; o mesmo para `BIND_TEST_DATABASE_URL`.

1. **Unitários:** lowering da UI e validador com nós inválidos montados à mão.
2. **Runtime JS (node):** `Int` (`div`/`rem` com todos os sinais); `Decimal` contra vetores do `rust_decimal`; `Float` contra f32 do Rust; codecs (ida e volta e entradas malformadas); `enter` no limite; `map`/`fold` sobre 100.000 itens; `<script>` em `Text` aparece escapado.
3. **Snapshots:** `tests/codegen/*.js.expected` e o HTML do SSR.
4. **Diferencial a três vias:** o gerador de programas do servidor, ampliado com UI, `Double` e `Float`. 1.000 programas renderizados pelo `core::eval`, pelo Rust (`render_html`) e pelo JS no node (DOM serializado por um DOM mínimo escrito no próprio teste, sem npm), idênticos byte a byte. Inclui a recursão de 20.000 níveis (`RecursionLimit` nos três, sem estouro de pilha).
5. **Ponta a ponta com Postgres:** o `commerce` nos dois modos. SSR: `GET /?id=…` com 200, 404 e 400, e rollback quando o componente falha depois de gravar. Separado: servidor Rust rodando e o JS no node chamando-o por `fetch` (caminho feliz, 404, parâmetro inválido, 500 não-JSON, servidor fora do ar). CORS: origem declarada aceita, outras recusadas.
6. **Site:** o HTML pré-renderizado é igual ao gerado hoje; `check`, `generate` e `certify` passam com o `bind.toml` atualizado (testado numa cópia temporária, sem modificar o repositório do site).

### Critérios de aceite

1. Frontend JS e SSR gerados só a partir da `CoreProgram`; Elm e tokens do cliente removidos.
2. Nenhum programa verificado falha no `generate`.
3. `Int` exato no frontend.
4. Diferencial a três vias com ≥ 1.000 programas e 0 divergências, incluindo a recursão de 20.000 níveis.
5. `certify` aprovado em aws-lambda e vercel, nos modos SSR e separado, com o `commerce`; site aprovado com HTML inalterado.
6. `cargo test`, clippy e fmt limpos; documentação `docs/certified-profile-v0.3.md`.
