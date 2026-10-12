# Bind certified profile v0.4

Profile identifier: `rust-web-v4`.

Este documento define a superfície finita para a qual `bind certify` emite uma atestação no perfil v4. O v4 mantém a pilha do v3 ([v0.3](certified-profile-v0.3.md)): HTML/CSS/JS nativos, SSR em Rust e servidor Rust, todos gerados da mesma IR tipada (`CoreProgram`). O que muda:

- **Fim da análise por tokens.** O grafo da aplicação, as operações de banco e os diagnósticos de segredo, de `insert`, de tipos de coluna e de retorno passam a ser calculados sobre a IR (`core::analysis`), com o mesmo código que os backends usam. `Function.body` (o vetor de tokens) saiu do AST; nenhum token é lido fora do lexer e do parser.
- **Tipos ricos no fio.** `sealed` com campos, genéricos instanciados, `Result`, `Set` e `Map` cruzam o fio, no contrato `bind-json-v4`. Os tipos que já cruzavam mantêm o formato.
- **Coleções.** `Set` e `Map` ganham builtins imutáveis, com chaves restritas à nova restrição `Key` e uma ordem canônica igual no avaliador, no Rust e no JS.

Semântica não suportada continua sendo erro de compilação; o compilador nunca a substitui por um comportamento mais fraco.

## Execution model

| Layer | Certified lowering |
|---|---|
| Source | `.bind` and `.bd` |
| Browser | HTML, CSS e módulos ES2022 nativos (`web/index.html`, `web/app.js`, `web/bind-runtime.js`, `web/styles.css`); sem npm, sem bundler, sem etapa de build |
| Protocol | JSON, `bind-json-v4`, one POST endpoint per server function |
| Server | safe Rust 1.98, Tokio, Serde, gerado da IR tipada; runtime copiado como `src/bind_runtime.rs` |
| Database | PostgreSQL through SQLx and a compiler-verified schema snapshot |
| AWS | Lambda custom runtime `provided.al2023`, arm64, AWS SAM + Cargo Lambda |
| Vercel | `web/` servido como está, uma função Rust oficial por endpoint e, no modo `ssr`, a função `bind-page` |

`types::check → core::lower → core::validate` produz a `CoreProgram`. Dela saem:

- o servidor Rust, o SSR e o frontend JS, como no v3;
- `core::analysis`: o grafo da aplicação (`impact`, `tree`, `application.graph.txt` e o caminho dos diagnósticos), as operações de banco do `VerifiedApplication` e os diagnósticos tipados (ver "Análise tipada").

Os runtimes são `runtime/bind-runtime` (Rust) e `runtime/bind-runtime-js` (JS). O avaliador de referência `src/core/eval.rs` é o oráculo da ordem canônica e das coleções, e serve ao teste diferencial a três vias e à pré-renderização.

## Configuração, modos e deployments

Iguais ao v0.3: `client = "web"`, `server = "rust"`, `mode = "ssr"` ou `"split"` obrigatório, `deployment = "vercel"` ou `"aws-lambda"`, as mesmas regras de `api_url` e `frontend_origin`, o mesmo CORS, a mesma pré-renderização com orçamento e as mesmas transações (uma por página no `ssr`, uma por RPC no `split`). As chamadas `server.f` do SSR continuam passando pelos codecs do fio em memória, agora com os tipos ricos, de modo que os dois modos mostram a mesma página para todo programa e entrada.

A chave `strict` de `[compiler]` não tem mais efeito: era lida apenas pelo aviso W2101, que saiu. Ela continua aceita (e ignorada) no `bind.toml`.

## Accepted application shape

**Servidor.** Toda a linguagem, como no v0.3. O que muda são os tipos aceitos nos parâmetros e retornos dos endpoints (abaixo) e a remoção da E8303: registros certificados que nunca cruzam o fio nem vão ao banco podem conter `Long`, funções, `Set`, `Map`, `Result` e parâmetros de tipo, como já acontecia nos alvos não certificados. A E8309 (fio) e a E8310 (mapeamento do banco) guardam as fronteiras. Uma `entity` certificada também pode ter coluna `Long` (`int8`/`i64`, o mapeamento que os alvos não certificados já usavam).

**Cliente.** Como no v0.3, mais `Set`, `Map`, `sealed` com campos, genéricos e `Result` nas expressões do componente e nas respostas das chamadas `server.f`. Os parâmetros do componente lidos da URL não mudam (`String`, `Email`, `Url`, `Int`, `UUID`, `Date`, `DateTime`, `Instant` e `distinct` sobre eles).

## Tipos de fio (`bind-json-v4`)

A E8309 passa a recusar no fio apenas **funções, `Duration` e `Long`** (diretos ou dentro de qualquer outro tipo), além dos tipos de configuração (`DatabaseUrl`, `Port`, `Secret`) e de `Ui`. Todo o resto cruza:

- os escalares do v0.3, `T?` e `List<T>`;
- `Set<T>`, `Map<K, V>` e `Result<T, E>` de tipos de fio;
- `data class`, `entity`, `distinct type`, `enum class` e `sealed class` (inclusive com campos) cujos campos são de fio, inclusive recursivos;
- genéricos instanciados com o número certo de argumentos, todos de fio.

| Tipo | JSON | Exemplo |
|---|---|---|
| variante sem campos | `"nomeLowerCamel"` (como antes) | `"vazia"` |
| variante com campos | `{"nomeLowerCamel": {campos}}` | `{"circulo": {"raio": "2"}}` |
| `Result<T, E>` | `{"Ok": v}` ou `{"Err": e}` | `{"Ok": "1"}` |
| genérico instanciado | o formato do tipo com os argumentos concretos | `Pagina<Int>`: `{"itens": ["1", "2"], "total": "2"}` |
| `Set<T>` | array em ordem canônica | `["1", "3", "10"]` |
| `Map<K, V>` | array de pares `[k, v]` em ordem canônica da chave | `[["a", "1"], ["b", "2"]]` |

Os escalares mantêm a forma do v0.3 (`Int`, `Decimal` e `Money` como string; um `distinct` com a forma do tipo que embrulha). Os nomes dos campos no JSON são os originais.

Os decodificadores continuam estritos, nos dois runtimes:

- `Set`: aceita qualquer ordem e normaliza; recusa duplicatas pela ordem canônica, de modo que `["1.0", "1.00"]` e `["0", "-0"]` (`Set<Decimal>`) são duplicatas;
- `Map`: cada par deve ter exatamente dois elementos (`[["a"]]`, `[["a", "1", "2"]]` e `[[]]` são recusados); chave duplicada é recusada; um objeto JSON não é um `Map`;
- variante: tag desconhecida, tag com outra caixa (`"Vazia"`, `{"Circulo": …}`), variante com campos enviada como string, objeto com duas tags e campo a mais são recusados;
- `Result`: exatamente uma das chaves `Ok` e `Err`;
- registro: campo a mais é recusado (no Rust, `#[serde(deny_unknown_fields)]` em todo registro e soma gerados; antes do v4 o servidor ignorava campos a mais em silêncio).

Cada instância de um genérico tem seu próprio codec: um endpoint que recebe `Pagina<Customer>` e `Pagina<Int>` decodifica cada uma com o seu e recusa o JSON da outra. No servidor, um JSON recusado dá 400 antes de abrir a transação; no frontend, uma resposta recusada dá a tela "Request failed".

Os vetores `runtime/vectors/wire-rich.json` (57 casos, válidos com a forma canônica e inválidos) são lidos pelos testes do runtime Rust, do runtime JS e dos codecs que o `bindc` gera.

## Restrição `Key` e ordem canônica

O elemento de um `Set<T>` e a chave de um `Map<K, V>` precisam satisfazer `Key`, uma restrição fechada do verificador, como `Eq` e `Ord`. Pertencem a `Key`:

- `Int`, `String`, `Email`, `Url`, `Boolean`, `UUID`;
- `Decimal`, `Money`, `Date`, `DateTime`, `Instant`;
- `enum class` e `sealed class` cujas variantes não têm campos (só `data object`);
- `distinct type` sobre esses tipos (direto ou por outro `distinct`).

`Double`, `Float`, `Long`, `Duration`, registros, `sealed` com campos, `Result`, listas, coleções, opcionais, funções e parâmetros de tipo não são `Key`: E7301 ("`Double` does not support use as a set element or map key (accepted types: …)"). Um parâmetro de tipo nunca é `Key`, então `fun <T> f(s: Set<T>)` é recusado. Uma variável `Key` não é generalizada nem recebe padrão: `val s = emptySet()` é E7302 até ser anotada (`val s: Set<Int> = emptySet()`). Detalhes em [type system](type-system.md).

A ordem canônica é a mesma no avaliador, no Rust e no JS:

| Chave | Ordem |
|---|---|
| `Int` | numérica |
| `Decimal`, `Money` | numérica por valor: `1.0` e `1.00` são a mesma chave, assim como `0` e `-0` |
| `String`, `Email`, `Url`, `UUID` | por ponto de código (não por unidade UTF-16: `！` < `😀`); o `UUID` é o texto canônico em minúsculas, o que coincide com a ordem dos 128 bits |
| `Date`, `DateTime`, `Instant` | temporal tipada (a de `compare_temporal`), nunca a do texto |
| `enum class`, `sealed` só de `data object` | ordem de declaração: `enum class Cor { Z, A, M }` dá `Z < A < M` |
| `Boolean` | `false < true` |
| `distinct` | a do tipo embrulhado |

**Escala de `Decimal` (regra A).** Quando duas chaves `Decimal` são iguais por valor mas têm escalas diferentes:

- o decodificador as recusa como duplicatas;
- em valores construídos no código, fica a escala da primeira inserção: `setOf(1.00, 1.0)` é `Set[1.00]`, e `put` de `1.00` num `Map` que tem `1.0` troca só o valor e mantém a chave `1.0`;
- `lookup(m, 1.00)` acha a entrada gravada como `1.0`.

A regra é idêntica nos três runtimes.

## Builtins de coleção

Todas são imutáveis: cada operação devolve uma coleção nova e nunca altera a recebida. `T` e `K` precisam ser `Key`.

| `Set<T>` | `Map<K, V>` |
|---|---|
| `setOf(a, b, …): Set<T>` (variádica, insere da esquerda para a direita) | `emptyMap(): Map<K, V>` |
| `emptySet(): Set<T>` | `put(m, k, v): Map<K, V>` |
| `insert(s, x): Set<T>` | `lookup(m, k): V?` |
| `remove(s, x): Set<T>` | `removeKey(m, k): Map<K, V>` |
| `contains(s, x): Boolean` | `keys(m): List<K>` (ordem canônica) |
| `setSize(s): Int` | `values(m): List<V>` (na ordem das chaves) |
| `toList(s): List<T>` (ordem canônica) | `mapSize(m): Int` |

- `insert` de um elemento igual a um existente mantém o existente;
- `remove` e `removeKey` de um ausente devolvem a coleção sem mudança;
- as builtins podem ser passadas como valor (`map(xss, setSize)`, `fold(xs, s, insert)`);
- não contam profundidade, como as demais builtins.

| Runtime | Representação |
|---|---|
| Rust | `BindSet<T>` e `BindMap<K, V>`, newtypes sobre `Arc<BTreeSet<T>>` e `Arc<BTreeMap<K, V>>` com serde estrito; os tipos `Key` do programa derivam `Eq`/`Ord` (ordem de declaração para variantes); atualizações copiam só quando o `Arc` é compartilhado |
| JS | arrays congelados em ordem canônica (pares `[k, v]` congelados para `Map`), com busca binária e um comparador por tipo de chave (`rt.compare`, `rt.decimalCompare`, `rt.compareDate`/`DateTime`/`Instant`, `rt.variantOrder`) escolhido na geração |
| Avaliador | `Value::Set` e `Value::Map` na mesma ordem; impressão `Set[1, 3]` e `Map[1: "a"]` |

`Set` e `Map` não têm mapeamento de banco: uma coluna `Set<Int>` continua sendo E8310. Os vetores `runtime/vectors/collections.json` (23 casos: ordem canônica, duplicatas, `lookup` ausente, `remove` de ausente, regra A) são lidos pelo avaliador e pelos dois runtimes.

## Análise tipada

Todos os diagnósticos abaixo são calculados sobre a `CoreProgram` depois de `core::lower`. Por isso só aparecem quando nada falhou antes: um programa com outro erro pode precisar de duas rodadas para mostrar todos os erros, mas nunca é aceito.

- **E4102 (transitiva).** Um valor de `environment` marcado como segredo, lido por qualquer função alcançável a partir de código cliente (componentes e `client fun`), é recusado com o caminho completo: ``Secret value `token` flows into client code: environment.token -> segredo -> rotulo -> Pagina.``
  - A busca segue chamadas e funções passadas como valor, com lambdas incluídas. Um segredo lido numa lambda passada a uma função compartilhada é recusado.
  - Variável local e string com o nome do segredo não disparam mais.
  - Quando a E4102 já cobre todas as leituras de ambiente de uma função, a E8310/E8311 de "uses an `environment` value" não se repete para ela.
- **E5303.** Um `insert` precisa atribuir toda coluna obrigatória (não nula e sem default), conferido contra o schema. O span é a função, porque a IR não tem spans de expressão.
- **E5204.** Cada campo do modelo de uma tabela usada precisa ter o tipo da coluna (ou seu opcional sobre coluna não nula). A comparação é entre o tipo da IR e o tipo da coluna, pelo mesmo mapeamento do verificador, não mais por texto.
- **E2301.** Uma função sem corpo com retorno diferente de `Unit` é recusada, inclusive `fun <T> g(): T`.
- **E5201 (campo de modelo sem coluna).** Mantida, reescrita sobre a IR: uma por (tabela usada, campo ausente), no campo do modelo, com caminho (`CustomerPage -> getCustomer -> database.main.customers.name`) e sugestão.
- **E8307.** Mantida: alvo certificado com alguma operação de banco na IR (inclusive em lambda nunca chamada) precisa declarar a credencial no `environment`. Uma variável local chamada `db` não dispara mais.
- **Grafo.** As arestas vão de quem usa para o que é usado:
  - função → função, para chamadas e para funções passadas como valor;
  - cliente → endpoint;
  - função → tabela e campos do modelo;
  - função → `environment.X`.

  `impact env.VAR` agora continua até as funções que leem o valor e quem as chama.

## Diagnósticos removidos

Cada código removido tem um teste (`compiler::tests::coverage_*`) que mostra o programa com o bug correspondente recusado pelo substituto.

| Removido | Programa com o bug | Substituto |
|---|---|---|
| E5200 tabela desconhecida | `db.custmers.require(id)` | E7104 "Unknown database table `custmers`" |
| E5201 coluna inexistente num `insert` | `db.customers.insert { nme = n }` | E7104 "Table `customers` has no column `nme`" |
| E5201 heurística `it.` | `map(listOf(db.customers.require(id)), { it -> it.nme })` | E7104 "`Customer` has no field `nme`" |
| E5202 tabela com modelo desconhecido | schema com modelo `Cliente` não declarado | E7103 "Table `customers` maps to `Cliente`, which is not a declared entity" |
| E8204 operação de tabela | `.count(id)`, `.delete(id)`, `.upsert(c)`, `.update { }` | E7004 "Unsupported table operation" (`.update { }`: "`{ field = value }` blocks are only supported on `db.<table>.insert`") |
| W2101 `!!` | `x!!` | E7001 "`!!` was removed" (parser) |
| E8303 campo de modelo que não é de fio | `Caixa(val n: Long)` como parâmetro de endpoint; entidade com campo `Set<Int>` | E8309 no endpoint; E8310 na coluna |
| E8308 campo mutável | `var` em `data class`, `entity` ou variante | E7002 (verificador) |
| E2001, E2002 declaração duplicada | tipo ou função declarados duas vezes, `data class String` | E7110 |
| E2003 tipo desconhecido | `Nope` em campo, parâmetro, retorno, `distinct`, `Map`/`Result` | E7103 "Unknown type `Nope`" |

Continuam: E5201 só para campo de modelo sem coluna, E8302 (`sealed` vazio em alvo certificado, que nem o parser nem o verificador recusam), E8204 "No built-in generator can preserve the semantics of target …" (outra regra) e E8307.

## Diagnostics

| Código | Significado |
|---|---|
| E2301 | função sem corpo com retorno diferente de `Unit` |
| E4102 | segredo do `environment` alcançável por código cliente, com o caminho |
| E5201 | campo do modelo sem coluna na tabela usada |
| E5204 | tipo do campo do modelo diferente do tipo da coluna |
| E5303 | `insert` sem coluna obrigatória |
| E7301 | restrição fechada não satisfeita, inclusive `Key` |
| E7302 | variável restrita ambígua, inclusive `Key` (`emptySet()` sem anotação) |
| E8301 | mais de um `client component` em target web |
| E8302 | `sealed class` sem variantes em alvo certificado |
| E8307 | alvo certificado com operação de banco sem a credencial no `environment` |
| E8309 | endpoint com função, `Duration` ou `Long` no fio; coluna `Duration` |
| E8310 | valor de ambiente, árvore de UI ou função sem corpo alcançável por um endpoint; coluna sem mapeamento de banco (inclusive `Set`/`Map`); problemas do plano de banco |
| E8311 | o alvo ou o programa não tem frontend web (as mesmas regras do v0.3) |
| E5101 | snapshot de schema que não carrega ou tipo de coluna não interpretável |
| E7006 | literal `Double` ou `Float` fora do intervalo |
| E9001 | erro interno: IR inconsistente (o validador também confere `Key` nas instâncias das builtins) |

## Certification pipeline

1. Kotlin-compatible lexical analysis and Bind parsing (os tokens ficam no parser).
2. Whole-application verification: tipos e restrições (inclusive `Key`), efeitos, lowering, diagnósticos tipados de `core::analysis`, validação da IR, regras do servidor e do frontend web.
3. Rust, SSR e JS gerados da IR verificada apenas; pré-renderização dos componentes estáticos.
4. Exact Cargo resolution in a generated `Cargo.lock`.
5. Generated Rust tests and all-target compilation.
6. Clippy with every warning promoted to an error (o código gerado nega `unwrap`, `expect`, `panic` e `unreachable`).
7. Release compilation of every generated Rust entry point.
8. SHA-256 hashes for sources, schema, manifests, locks, artifacts e do runtime JS enviado em `web/`.

O `bind.lock` raiz e os manifestos registram `rust-web-v4` e `bind-json-v4`; projetos existentes veem o lock mudar ao regenerar.

## Meaning of certification

`certified-host` continua significando que todos os gates passaram na máquina que invocou o Bind. Os testes do `bindc` dão verificação adicional:

- o diferencial a três vias (avaliador × Rust gerado × JS gerado, com `node`) roda 2019 programas e 1748 chamadas com `Decimal` e datas, sem divergências. Os programas usam as 14 builtins (chamadas e como valor), `Set` e `Map` de `Int`, `String`, `Boolean`, `Decimal`, `enum` fora da ordem alfabética e datas, `sealed` com campos, um registro genérico em duas instâncias e `Result`;
- o diferencial do servidor continua com os mesmos 1003 programas (fonte inalterada, conferida por hash);
- os vetores `collections.json` e `wire-rich.json` fixam a ordem, a regra A e o fio nos três runtimes;
- um teste ponta a ponta com Postgres real (`BIND_TEST_DATABASE_URL`) usa um endpoint que recebe `Map` e devolve `sealed` com campos, `Set` e `Result`. Ele confere que:
  - as páginas SSR e split são iguais;
  - `Map` com par mal formado ou chave duplicada dá 400 no servidor;
  - uma resposta com `Set` duplicado dá "Request failed" no frontend.

A certificação **não** garante:

- `Long` no fio (E8309) nem acima de 2^53 no frontend JS;
- `Duration` no banco (E8309) nem no fio;
- `*` e `/` de `Decimal`/`Money` no cliente: só existem no servidor (E8311 no componente);
- `Set`/`Map` como colunas do banco (E8310);
- chaves de tipos genéricos: um parâmetro de tipo nunca é `Key`, então não há funções genéricas sobre `Set<T>`;
- rigor idêntico nestes casos limite do fio:
  - o servidor Rust aceita `{"vazia": null}` para uma variante sem campos, que o JS recusa;
  - o servidor Rust aceita um registro ou uma variante com campos escritos como array JSON (`["1", "1"]` para `Pagina<Int>`, com tipos e aridade conferidos), que o JS recusa;
  - o JS decodifica `UUID`, `Date`, `DateTime` e `Instant` como texto, sem validar o formato (como no v3), inclusive em elementos de `Set` e chaves de `Map`: aceita um `UUID` como `"abc"`, e `"A…"` e `"a…"` são chaves distintas no JS, enquanto o Rust recusa o primeiro e normaliza os dois;
  - o JS aceita um objeto JSON com chave repetida (fica a última), que o Rust recusa.

  Todos só afetam JSON que nenhum codec gerado produz;
- eventos, estado ou hidratação; página pesada pré-renderizada; terminação; arredondamento de `Money` em colunas `numeric(p,s)`; erros de faixa inteira do banco; reprodutibilidade do `Cargo.lock` transitivo ao longo do tempo; autenticação; registro em log de erros de banco; credenciais, rede ou disponibilidade do provedor (como no v0.3).

## Migração do v3

Não há mudança no `bind.toml`. Regenere o diretório de deploy (`bind generate --target <alvo>`): o lock e o manifesto passam a `rust-web-v4`/`bind-json-v4`. Programas que o v3 aceitava continuam aceitos e com o mesmo JSON, com duas exceções:

- o servidor gerado recusa campos a mais em registros, como o frontend já fazia;
- uma variável local chamada `db`, ou com o nome de um segredo, deixa de disparar E8307/E4102.

Os programas que o v3 recusava com os códigos removidos continuam recusados, pelos substitutos da tabela acima.
