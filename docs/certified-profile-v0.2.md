# Bind certified profile v0.2

Profile identifier: `rust-elm-web-v2`.

> Nota: este perfil foi substituído pelo [v0.3](certified-profile-v0.3.md) (`rust-web-v3`, contrato `bind-json-v3`, frontend HTML/CSS/JS e SSR gerados da IR; o cliente Elm foi removido). Mantido como referência histórica.

Este documento define a superfície finita para a qual `bind certify` emite uma atestação no perfil v2. O v2 substitui o v1 ([v0.1](certified-profile-v0.1.md)) no servidor: o código Rust é gerado a partir de uma IR tipada (`CoreProgram`), não mais de padrões de tokens, e os endpoints aceitam a linguagem inteira. Semântica não suportada continua sendo erro de compilação; o compilador nunca a substitui por um comportamento mais fraco.

## Execution model

| Layer | Certified lowering |
|---|---|
| Source | `.bind` and `.bd` |
| Browser | Elm 0.19.2 (inalterado em relação ao v0.1) |
| Protocol | JSON, `bind-json-v2`, one POST endpoint per server function |
| Server | safe Rust 1.98, Tokio, Serde, gerado da IR tipada; runtime copiado como `src/bind_runtime.rs` |
| Database | PostgreSQL through SQLx and a compiler-verified schema snapshot; uma transação por chamada |
| AWS | Lambda custom runtime `provided.al2023`, arm64, AWS SAM + Cargo Lambda |
| Vercel | one official Rust Function per RPC plus an optimized static Elm output |

Os dois adaptadores chamam a mesma biblioteca Rust gerada. O servidor lê somente a `CoreProgram` produzida por `types::check → core::lower → core::validate`; nenhum token de código-fonte é consultado pelo backend. O cliente Elm, por enquanto, continua sendo gerado da AST e dos tokens verificados, como no v0.1 (passa a vir da IR no subprojeto 3c).

Na biblioteca gerada, os tipos e as funções do programa ficam no módulo `app`; a camada de banco, os tipos de requisição, o roteador (`handle_rpc`, `serve_rpc`, `HttpResult`) e os testes ficam na raiz do crate, de modo que nenhum nome do usuário colide com um nome gerado. Tipos distintos viram structs `T { value }` e nomes que são palavras-chave do Rust viram identificadores raw (`r#match`) ou ganham um sufixo `_` (`self`, `Self`, `super`, `crate`, `_`).

## Accepted application shape

**Servidor.** Toda a linguagem: funções, lambdas, `val`, `if`, `when` exaustivo, `Result`, listas e suas operações, aritmética, recursão e as operações de banco `require`, `find` e `insert` (com várias operações por endpoint). Valores `val` polimórficos livres de efeitos usados em mais de um tipo são monomorfizados; um `val` polimórfico com efeitos usado em mais de um tipo é rejeitado com E9001 (limite documentado). Funções sem corpo não fazem parte do perfil: uma função sem corpo (ou um endpoint sem corpo) alcançável a partir de um endpoint é rejeitada com E8310 em todo target com servidor Rust; fora disso ela continua valendo `Unit`. Um `return` usado como operando (`(return 1) + 2`, `f(x, return y)`, `when (return z)`) avalia os operandos anteriores, da esquerda para a direita, e sai da função. Literais de ponto flutuante grandes demais para serem finitos (`1e400`, `3.5e38f`) são rejeitados com E7006; os pequenos demais (`1e-400`) valem 0.0.

**Cliente.** Como no v0.1: um componente e um bloco `UI`, estático ou com uma chamada de servidor ligada, nós de UI finitos e `Text(...)` com literais ou campos do resultado. Os limites do cliente Elm (por exemplo, `Text` fora do subconjunto, `List<Int?>` no parser de tipos do Elm) não mudaram e ficam para o subprojeto 3c.

**Tipos de fio.** Parâmetros e retornos de endpoints devem ser tipos de fio:

- os escalares `Unit`, `String`, `Int`, `Float`, `Double`, `Decimal`, `Boolean`, `UUID`, `Date`, `DateTime`, `Instant`, `Money`, `Email` e `Url`;
- `data class` e `entity` sem parâmetros de tipo cujos campos são tipos de fio;
- `distinct type` sobre um tipo de fio;
- `enum` e `sealed class` sem parâmetros de tipo e sem campos nas variantes;
- `List<T>` de tipo de fio;
- `T?` de tipo de fio.

Funções, `Result`, `Duration`, `Long`, `Port`, `Secret`, `DatabaseUrl`, `Set`/`Map`, tipos do usuário genéricos (`data class Caixa<T>`, mesmo instanciados) e variantes com campos não cruzam o fio. Colunas `Duration` também são rejeitadas.

## Diagnostics

| Código | Significado |
|---|---|
| E8309 | parâmetro ou retorno de endpoint que não é tipo de fio, ou coluna `Duration`; roda para todo target com `server = "rust"`, certificado ou não |
| E8310 | valor de ambiente, árvore de UI ou função sem corpo alcançável por um endpoint (ou endpoint sem corpo); problemas do plano de banco: tabela sem modelo de registro, coluna de chave primária ou de `insert` ausente do modelo, tipo de campo sem mapeamento para o banco |
| E5101 | snapshot de schema que não carrega; em targets com servidor Rust, também tipo de coluna não interpretável numa tabela cujo modelo é declarado no programa ou que aparece numa operação de banco |
| E7006 | literal `Double` ou `Float` fora do intervalo (infinito); vale para qualquer target |
| E9001 | erro interno: IR inconsistente (o validador rejeitou a saída do `lower`) |

E8304 (uma operação de banco por endpoint) deixou de existir. Lowering e validação só rodam depois que todas as verificações do usuário passam, de modo que E9001 não mascara erros do programa.

## Representação dos escalares

| Bind | Rust | PostgreSQL | JSON |
|---|---|---|---|
| `Int` | `Int` (newtype sobre `num_bigint::BigInt`) | `numeric` via `BigDecimal` | string canônica |
| `Decimal`, `Money` | `rust_decimal::Decimal` | `numeric` | string |
| `Date` | `chrono::NaiveDate` | `date` | string ISO-8601 |
| `DateTime` | `chrono::NaiveDateTime` | `timestamp` | string ISO-8601 |
| `Instant` | `chrono::DateTime<Utc>` | `timestamptz` | string ISO-8601 |
| `Duration` | `chrono::TimeDelta` | (sem coluna) | (não cruza o fio) |
| `List<T>` | `Arc<Vec<T>>` | n/a | array |
| `Result<T, E>` | `BindResult<T, E>` | n/a | n/a |
| funções | `Fun` | n/a | n/a |

`Int` tem precisão arbitrária. Seu JSON é a string canônica `-?[0-9]+`: sem zeros à esquerda, sem `-0`, no máximo 1000 dígitos (`MAX_INT_DIGITS`); qualquer outra forma é recusada com 400. O Elm decodifica `Int` de string apenas quando |n| <= 2^53 - 1; acima disso a decodificação falha (`Http.BadBody`), nunca exibe um número arredondado. `Decimal` e `Money` também trafegam como strings. `Long` continua rejeitado no fio (restrição herdada do v1): endpoints com `Long` ficam indisponíveis até existir um codec de string.

## Transação por chamada

Toda chamada RPC roda numa transação PostgreSQL: commit em `Ok`, rollback em `Err`. O roteador obtém a transação por `take_transaction`, de modo que uma cópia sobrevivente do `Ctx` nunca descarta um commit em silêncio, e nunca reporta sucesso sem o commit ter sido concluído. Um `require` sem resultado depois de um `insert` no mesmo endpoint desfaz a transação inteira. Erros de banco viram resposta 500 com mensagem genérica; hoje eles não são registrados em log (limite conhecido).

## Limite de profundidade e AppError

Cada função e lambda do Bind recebe `Ctx` e devolve `Result<R, AppError>`. A profundidade de chamadas é contada na entrada de função ou lambda do Bind (não em builtins) e limitada a 10.000; acima disso a resposta é `AppError::RecursionLimit`, sem derrubar o processo. Os servidores gerados executam em workers do Tokio com pilha de 64 MiB. Overflow de `Decimal`, `Money` ou `Duration` produz `AppError::Overflow` (status 500). O código gerado e o runtime não contêm pânicos: `#![forbid(unsafe_code)]` mais lints que negam `unwrap`, `expect`, `panic` e `unreachable` (grupo `restriction` do Clippy). O crate gerado permite `clippy::all`: os lints padrão do Clippy julgam a lógica e a forma do código, e o código gerado espelha o programa Bind (`3.14`, `d > 1.0 && d < 0.0`, oito parâmetros, uma variante grande, …), que é responsabilidade de quem o escreveu. As negações acima não pertencem a `clippy::all` e continuam valendo; o runtime passa no Clippy sem essa permissão.

## Contrato `bind-json-v2`

Um endpoint POST por função de servidor (componentes de servidor não são procedimentos RPC e não entram no contrato), com corpo estrito: campos desconhecidos são recusados, assim como campos ausentes de tipo não anulável; um campo ausente de tipo `T?` é lido como `null`. Em relação ao `bind-json-v1`: `Int`, `Decimal` e `Money` trafegam como strings; o formato de `contracts` mudou; o manifesto registra `wireVersion = "bind-json-v2"`. Vetores de teste malformados são gerados junto com o contrato.

## Certification pipeline

Os gates e a ordem do v0.1 permanecem os mesmos:

1. Kotlin-compatible lexical analysis and Bind parsing.
2. Whole-application type, effect, environment, schema, RPC, and capability verification (agora também lowering e validação da IR).
3. Rust generation from the verified IR only (o runtime é copiado para o crate gerado como `src/bind_runtime.rs`); Elm generation from the verified application (AST e tokens verificados, como no v0.1, até o subprojeto 3c).
4. Exact Cargo resolution in a generated `Cargo.lock`.
5. Generated Rust tests and all-target compilation.
6. Clippy with every warning promoted to an error (o código gerado permite `clippy::all` e nega `unwrap`, `expect`, `panic` e `unreachable`; ver acima).
7. Release compilation of every generated Rust entry point.
8. Optimized Elm compilation.
9. For Vercel, exact npm resolution and `npm ci`.
10. SHA-256 hashes for sources, schema, dependency manifests, locks, and artifacts.

As dependências do código gerado (`num-bigint`, `bigdecimal`, `rust_decimal`, `chrono`) são fixadas com `=` em `src/certified.rs`. O `bind.lock` raiz passa a registrar `rust-elm-web-v2`: projetos existentes veem o lock mudar de v1 para v2 ao regenerar.

## Meaning of certification

`certified-host` continua significando que todos os gates passaram na máquina que invocou o Bind. Os testes do próprio `bindc` (não executados por `bind certify`) dão verificação adicional ao v2: um teste diferencial compara o avaliador da IR com o Rust gerado em mais de 1.000 programas sem divergências e passa o crate gerado pelo Clippy com `-D warnings`; corpora de snapshots (incluindo formas que disparam lints e nomes do usuário que colidiriam com nomes gerados) são compilados com Clippy; e testes com banco real (`BIND_TEST_DATABASE_URL`) confirmam commit e rollback.

A certificação **não** garante:

- terminação (um programa pode recursar até o limite ou laçar sem recursão profunda);
- arredondamento de escala de `Money` pelas colunas `numeric(p,s)`, desconhecidas do snapshot de schema;
- erros de faixa inteira do banco, que aparecem como 500;
- reprodutibilidade do `Cargo.lock` transitivo ao longo do tempo: ele é re-resolvido no registry quando o perfil muda (o hash fica registrado);
- autenticação: os endpoints gerados não têm nenhuma (achado em aberto);
- registro em log de erros de banco;
- credenciais, alcance do banco, rede ou disponibilidade do provedor (erros tipados em tempo de execução);
- os limites do cliente Elm, adiados para o subprojeto 3c.

Nota: o verificador de tipos não aceita `String` para coluna `String?` (não há subtipagem `T <: T?`).
