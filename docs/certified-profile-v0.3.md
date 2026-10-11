# Bind certified profile v0.3

Profile identifier: `rust-web-v3`.

Este documento define a superfície finita para a qual `bind certify` emite uma atestação no perfil v3. O v3 substitui o v2 ([v0.2](certified-profile-v0.2.md)) no cliente: o cliente Elm foi removido e o frontend passa a ser HTML, CSS e JavaScript nativos, mais um renderizador SSR em Rust, ambos gerados da mesma IR tipada (`CoreProgram`) que já gerava o servidor. A pilha é única: Bind, HTML/CSS/JS e Rust. Semântica não suportada continua sendo erro de compilação; o compilador nunca a substitui por um comportamento mais fraco.

## Execution model

| Layer | Certified lowering |
|---|---|
| Source | `.bind` and `.bd` |
| Browser | HTML, CSS e módulos ES2022 nativos (`web/index.html`, `web/app.js`, `web/bind-runtime.js`, `web/styles.css`); sem npm, sem bundler, sem etapa de build |
| Protocol | JSON, `bind-json-v3`, one POST endpoint per server function |
| Server | safe Rust 1.98, Tokio, Serde, gerado da IR tipada; runtime copiado como `src/bind_runtime.rs` |
| Database | PostgreSQL through SQLx and a compiler-verified schema snapshot |
| AWS | Lambda custom runtime `provided.al2023`, arm64, AWS SAM + Cargo Lambda |
| Vercel | `web/` servido como está, uma função Rust oficial por endpoint e, no modo `ssr`, a função `bind-page` |

Nenhum token de código-fonte é consultado por nenhum backend. `types::check → core::lower → core::validate` produz a `CoreProgram`; dela saem o servidor Rust, o SSR (renderizador Rust com o mesmo runtime) e o frontend JS. Os runtimes são `runtime/bind-runtime` (Rust, copiado como `src/bind_runtime.rs`) e `runtime/bind-runtime-js` (copiado como `web/bind-runtime.js`). O avaliador de referência `src/core/eval.rs` serve ao teste diferencial a três vias (avaliador, Rust gerado e JS gerado) e à pré-renderização.

## Configuração do alvo (`bind.toml`)

```toml
[target.vercel-ssr]
client = "web"
server = "rust"
mode = "ssr"            # "ssr" | "split"; obrigatório
deployment = "vercel"   # "vercel" | "aws-lambda"
certified = true
capabilities = ["ui.dom", "server.http", "db.read", "db.write"]
# api_url = "/api"                           # padrão
# frontend_origin = "https://app.exemplo.com" # exigido quando api_url é de outra origem
```

- `client = "web"` e `server = "rust"`. `client = "elm"` não existe mais: é recusado com E8311 e uma sugestão de correção (`client = "web"` mais `mode`).
- `mode` é obrigatório; a ausência dele é E8311.
- `api_url`: o caminho deve ser exatamente `/api`, relativo (`/api`, o padrão) ou uma origem canônica seguida de `/api` (`https://host[:porta]/api`, sem barra final, consulta ou fragmento). O servidor gerado só responde em `/api/<endpoint>`; prefixos personalizados não são suportados.
- `frontend_origin`: origem canônica (`http://` ou `https://`, host em minúsculas, porta não padrão opcional, sem caminho, credenciais ou barra final). É obrigatória quando `api_url` aponta para outra origem.
- Dois componentes `client component` no mesmo programa dão E8301 em todo target web; a página é o único componente.
- Metadados da página vêm de `[web]` (`title`, `description`, `language`, `stylesheet`); o shell inclui `lang`, `description`, `theme-color`, tags `og:*` e o favicon, e é idêntico (byte a byte, verificado por vetores compartilhados) nos dois modos e nas três implementações.

## Modos e deployments

| Deployment | `ssr` | `split` |
|---|---|---|
| `aws-lambda` | uma Lambda serve `/` (a página, ou o `web/index.html` pré-renderizado), `/styles.css` e `/api/*` | a Lambda serve `/api/*`; `web/` fica pronto para hospedagem estática (S3, CloudFront, ...) |
| `vercel` | `web/` estático (CSS), uma função por endpoint e a função `bind-page`, para a qual `/` é reescrita | `web/` estático e uma função por endpoint, mesma origem |

No Vercel o modo `ssr` usa uma função por endpoint mais `bind-page` (e não uma função única); o comportamento observável é o de um único servidor.

**Componentes estáticos** (sem parâmetros e sem chamada de servidor) são pré-renderizados em tempo de compilação, pelo avaliador `core::eval`, em `web/index.html`, sem JavaScript, nos dois modos. A avaliação tem um orçamento (2.000.000 passos, 1024 limbs por `Int`, 4 MiB por `String`); se o componente excede o orçamento, falha ou usa algo que o avaliador não modela, a página é renderizada em tempo de execução (onde uma falha vira a tela de erro com seu status). Uma aplicação sem endpoints e com página estática não tem servidor: `web/` é tudo.

**SSR** (`mode = "ssr"`, componente dinâmico): o servidor responde a `GET` e `HEAD` em `/`.

- Os parâmetros do componente vêm da query string e são estritos: decodificação percentual, `+` vale espaço; parâmetro ausente, repetido, com `%` malformado, UTF-8 inválido ou rejeitado pelo decodificador do tipo dá 400. Tipos aceitos: `String`, `Email`, `Url`, `Int`, `UUID`, `Date`, `DateTime`, `Instant` e `distinct type` sobre eles (formas canônicas estritas para `Int`, `UUID` e datas).
- **Uma transação para todas as chamadas de servidor da página**: commit se a página renderiza, rollback em erro. Cada chamada `server.f` começa com profundidade 0, como um RPC separado. Uma página sem chamada de servidor não abre transação.
- Status: 200, 404 (`require` sem resultado), 400 (`BadRequest`, parâmetro inválido) e 500 (demais erros), com mensagem pública e página de erro no mesmo shell. Erros internos, de banco, estouro e limite de recursão têm mensagem genérica.

**Split** (`mode = "split"`): `web/app.js` roda o componente no navegador e chama o API por `rpc`. **Cada RPC tem sua própria transação**, como no v0.2. A consequência observável: um endpoint que escreve e depois falha em outra chamada da mesma página faz commit da escrita em `split` e rollback em `ssr`. Falha de rede, resposta 500 sem JSON ou servidor indisponível viram a tela "Request failed" (ou a mensagem pública do erro), nunca exceção não tratada.

## Accepted application shape

**Servidor.** Como no v0.2 (toda a linguagem; E8309 para tipos que não cruzam o fio; E8310 para valor de ambiente, árvore de UI ou função sem corpo alcançável por um endpoint).

**Cliente.** Um `client component` com um bloco `UI`:

- UI real: nós `Page`, `Container`, `Div`, `Grid`, `Stack`, `Header`, `Nav`, `Main`, `Section`, `Article`, `Aside`, `Footer`, `Paragraph`, `Span`, `Strong`, `Emphasis`, `Small`, `List`, `Item`, `Pre`, `Code`, `Heading`, `Link`, `Rule`, `Text` e `Empty`, com filhos arbitrários.
- Expressões tipadas: `val`, `if`, `when`, funções e lambdas do usuário, listas e suas operações, `Result`, recursão, aritmética e comparações; o componente não é limitado a literais e campos.
- Várias chamadas `server.f`, em qualquer lugar do componente e das funções que ele alcança.
- Parâmetros do componente lidos da URL (query string), com os tipos acima.
- Telas padrão: "Loading…" enquanto as chamadas estão em voo (split), "Request failed" ou a mensagem pública em erro, página 404, 400 e 500 no SSR.
- `Link` só emite `href` para `http`, `https`, `mailto`, `tel` ou URL relativa (lista de permissão, depois de remover caracteres <= U+0020 e tab/LF/CR); qualquer outro esquema vira `href=""`. Vale nos três renderizadores. `Text` é sempre escapado (`& < > " '`).

**Tipos de fio.** Como no v0.2. `Int` é exato no frontend: `BigInt` no JS, string canônica `-?[0-9]+` (até 1000 dígitos) no fio e na URL.

## Diagnostics

| Código | Significado |
|---|---|
| E8301 | mais de um `client component` em target web (todos os targets web) |
| E8309 | parâmetro ou retorno de endpoint que não é tipo de fio, ou coluna `Duration` |
| E8310 | valor de ambiente, árvore de UI ou função sem corpo alcançável por um endpoint; problemas do plano de banco |
| E8311 | o alvo ou o programa não tem frontend web (ver abaixo) |
| E5101 | snapshot de schema que não carrega ou tipo de coluna não interpretável |
| E7006 | literal `Double` ou `Float` fora do intervalo |
| E9001 | erro interno: IR inconsistente |

E8306 (limites do cliente Elm) foi removido. **E8311** cobre:

- `client = "elm"` (com a correção sugerida);
- `mode` ausente;
- `api_url` que não é `/api` nem origem canônica mais `/api`;
- `api_url` de outra origem sem `frontend_origin`;
- `frontend_origin` que não é uma origem canônica;
- programa sem `client component`;
- parâmetro do componente de um tipo que a URL não carrega (fora de `String`, `Email`, `Url`, `Int`, `UUID`, `Date`, `DateTime`, `Instant` e `distinct` sobre eles);
- componente, ou função alcançável a partir dele, que usa um valor de ambiente;
- componente, ou função alcançável, sem corpo;
- aritmética de `Duration` (`+`, `-`, negação) alcançável a partir do componente;
- `*` ou `/` sobre `Decimal` ou `Money` alcançável a partir do componente (nos dois modos).

Essas regras valem igualmente nos dois modos, de modo que ambos aceitam exatamente os mesmos programas e `generate` é total para todo programa verificado.

## Representação e exibição

Os escalares e o fio são os do v0.2; o contrato é `bind-json-v3` (mesmo formato no fio; o frontend aceita qualquer `Int` canônico de até 1000 dígitos).

| Bind | Rust | JavaScript | Exibição |
|---|---|---|---|
| `Int` | `Int` (BigInt) | `BigInt` | decimal canônico |
| `Float` | `f32` | `Math.fround`, literais pré-arredondados | `Number.prototype.toString` do JS |
| `Double` | `f64` | `Number` | `Number.prototype.toString` do JS |
| `Decimal`, `Money` | `rust_decimal::Decimal` | decimal com sinal explícito (mantissa < 2^96, escala <= 28) | formato do `rust_decimal`, inclusive `-0` |
| `Date`, `DateTime`, `Instant` | `chrono` | texto canônico | ISO-8601 |
| `Duration` | `chrono::TimeDelta` | texto | ISO-8601 de duração, como o `Display` do `TimeDelta` |
| `T?` | `Option<T>` | `null` ou `{ some: v }` (uniforme; o fio não muda) | n/a |
| `data class`, `entity` | struct | objeto com nomes de propriedade escapados (`u_<nome>`); o JSON usa os nomes originais | n/a |

`Decimal`/`Money` em componentes suportam `+`, `-`, negação, comparação e exibição; estouro de mantissa ou escala dá `Overflow`. Funções e tipos chamados `class`, `await`, `default`, `Object`, `enter` e campos como `__proto__`, `constructor` e `toString` geram JS válido e decodificam JSON sem poluir protótipos.

`enter` (a contagem de profundidade, 10.000) sempre faz `await` no JS, de modo que `map`/`fold` sobre 100.000 itens não estouram a pilha. Cada implementação (avaliador, Rust, JS) segue as mesmas regras de exibição; os vetores compartilhados ficam em `runtime/vectors/` (`html-trees.json`, `numbers.json`, `pages.json`, `params.json`), e os de `Decimal` e `f32` do JS em `runtime/bind-runtime-js/tests/`.

## CORS

Só existe com `api_url` de outra origem (`frontend_origin`). A Rust gerada decide por requisição:

- sem cabeçalho `Origin`: tratada normalmente (não há navegador);
- `Origin` igual a `frontend_origin`: resposta com `access-control-allow-origin` e `vary: Origin`; o preflight `OPTIONS` é respondido com 204, `access-control-allow-methods: POST, OPTIONS`, `access-control-allow-headers: content-type, accept` e `access-control-max-age: 600`;
- qualquer outra origem: 403 com `{"error":"origin not allowed"}` e sem cabeçalhos CORS.

## Certification pipeline

1. Kotlin-compatible lexical analysis and Bind parsing.
2. Whole-application verification (tipos, efeitos, ambiente, schema, RPC, capacidades, lowering e validação da IR, regras do frontend web).
3. Rust, SSR e JS gerados da IR verificada apenas; pré-renderização dos componentes estáticos.
4. Exact Cargo resolution in a generated `Cargo.lock`.
5. Generated Rust tests and all-target compilation.
6. Clippy with every warning promoted to an error (o código gerado permite `clippy::all` e nega `unwrap`, `expect`, `panic` e `unreachable`).
7. Release compilation of every generated Rust entry point.
8. SHA-256 hashes for sources, schema, manifests, locks, artifacts e do runtime JS enviado em `web/`.

Não há etapa de Elm nem de npm: o Vercel serve `web/` como está. O `bind.lock` raiz registra `rust-web-v3`; projetos existentes veem o lock mudar ao regenerar.

## Meaning of certification

`certified-host` continua significando que todos os gates passaram na máquina que invocou o Bind. Os testes do `bindc` dão verificação adicional: um teste diferencial a três vias compara o avaliador da IR, o Rust gerado e o JS gerado (com `node`) em mais de 1.000 programas sem divergências e com recursão de 20.000 níveis; vetores compartilhados fixam HTML, números, parâmetros e páginas nas três implementações; testes com banco real (`BIND_TEST_DATABASE_URL`) confirmam commit e rollback do SSR e do split.

A certificação **não** garante:

- eventos, estado ou hidratação: a UI é uma função pura dos parâmetros e das respostas; a página SSR não envia JavaScript;
- `*` e `/` de `Decimal`/`Money` no cliente: só existem no servidor (E8311 no componente);
- `Long` acima de 2^53 no frontend JS (não suportado);
- que um `Double` não finito devolvido por um endpoint se comporte igual nos dois modos: o SSR exibe `NaN`/`Infinity`, enquanto o split falha ao decodificar a resposta JSON;
- orçamento de pré-renderização por custo: ele limita passos, não o custo de cada passo; um componente pesado pode deixar `generate` lento, e um que estoura o orçamento é renderizado em tempo de execução;
- terminação, arredondamento de escala de `Money` pelas colunas `numeric(p,s)`, erros de faixa inteira do banco (500);
- reprodutibilidade do `Cargo.lock` transitivo ao longo do tempo: é re-resolvido no registry quando o perfil muda (o hash fica registrado);
- autenticação: os endpoints gerados não têm nenhuma;
- registro em log de erros de banco;
- credenciais, alcance do banco, rede ou disponibilidade do provedor.

## Migração de um projeto Elm

No `bind.toml` do alvo, troque `client = "elm"` por `client = "web"`, adicione `mode = "ssr"` ou `mode = "split"` (obrigatório) e, se a API ficar em outra origem, `api_url` e `frontend_origin`. Regenere o diretório de deploy (`bind generate --target <alvo>`); `elm.json`, `package.json` e os artefatos do Elm desaparecem. No Vercel, o resultado é `web/` mais as funções Rust.
