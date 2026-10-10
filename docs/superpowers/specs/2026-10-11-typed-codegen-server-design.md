# Geração de código a partir da IR tipada: núcleo e servidor Rust

- Data: 2026-10-11
- Status: aprovado em conversa, aguardando revisão da spec escrita
- Subprojeto: 3 (partes 3a e 3b). 3c (cliente Elm a partir da IR) e 3d (remoção da análise por tokens e ampliação do perfil) terão specs próprias.

## Objetivo

Hoje o codegen não executa nenhuma lógica do programa: cada `server fun` precisa ser exatamente uma operação de banco, que o gerador traduz por padrões lidos dos tokens. Tudo que os subprojetos 1 e 2 tornaram verificável (`when`, ADTs, lambdas, recursão, funções auxiliares, efeitos) é verificado, mas não gerado. Este subprojeto cria uma IR tipada como única entrada dos backends e um backend Rust que gera o servidor inteiro a partir dela, para que as garantias do verificador cheguem ao código que roda.

### Decisões tomadas

| Decisão | Escolha |
|---|---|
| Ordem | Servidor (Rust) primeiro; cliente Elm no 3c |
| Escopo desta spec | 3a (IR do núcleo) + 3b (backend Rust) |
| `Int` | Precisão arbitrária: `BigInt` no Rust, `numeric` no PostgreSQL, string no JSON |
| Atomicidade | Toda chamada RPC roda numa transação (commit em `Ok`, rollback em `Err`) |
| Abordagem do backend | Dados idiomáticos (`enum`/`struct`/`match`) com convenção de chamada uniforme: toda função é assíncrona, retorna `Result` e recebe o contexto da requisição |

### Fora do escopo

- Gerar o cliente Elm a partir da IR (3c); o cliente só muda o necessário para o contrato (`Int` como string).
- Remover a análise antiga por tokens (3d); ela continua alimentando o cliente.
- `Duration` como coluna de banco ou no fio.
- Garantia de terminação (recursão infinita termina com erro de profundidade, não é rejeitada em compilação).

## 1. IR do núcleo (3a)

### Módulos

`src/core/ir.rs` (estruturas), `src/core/lower.rs` (AST + `TypedProgram` → IR), `src/core/validate.rs` (checagem de consistência), `src/core/eval.rs` (avaliador de referência, só em testes).

### Estrutura

```text
CoreProgram { types: [CoreType], functions: [CoreFunction] }

CoreType     = Record   { name, params, fields: [(nome, Type)] }
             | Sum      { name, params, variants: [(nome, [(campo, Type)])] }   // sealed, enum, Result
             | Distinct { name, underlying }

CoreFunction { name, domain, endpoint: bool, generics, params: [(Local, Type)],
               result: Type, effects: [Label], body: Expr }

Expr (todo nó carrega seu Type resolvido)
  = Lit(Int grande | Double | Float | String | Bool | Unit)
  | Local(id) | Global(nome, instanciação dos genéricos) | Ctor(nome, args)
  | Call(callee, args) | Lambda(params, body)
  | Let(local, valor, corpo)
  | If | Match(scrutinee, [(Pat elaborado, corpo)])
  | Field(alvo, nome) | Prim(op concreto, args) | Db(Require | Find | Insert{campos}, tabela, args)
  | Return(valor)
```

### O que a tradução resolve

- Operadores concretos pelo tipo dos operandos: `IntAdd`, `MoneyAdd`, `StringConcat`, `IntDivChecked` (devolve `Option`), comparações por tipo etc.
- Açúcar removido: `?:` e `?.` viram `Match`; blocos e `val` viram `Let` encadeados; `val _ = e` vira avaliação descartada.
- Padrões elaborados a partir de `TypedProgram.patterns`, com `Some(...)` explícito.
- Resolução de nomes: o verificador passa a registrar, para cada nó `Name`, o que ele é (local, função global, construtor, builtin, valor de `environment`), num campo novo de `TypedProgram`; a IR usa esse registro em vez de repetir a lógica de resolução.
- Genéricos: variáveis de tipo de uma função polimórfica viram parâmetros genéricos. Variáveis livres sem restrição em código monomórfico viram `Unit` (o valor nunca é inspecionado).

### Validador

Roda nos testes e no `certify`. Confere que o tipo de cada nó bate com o dos filhos, que não sobrou nome sem resolver e que todo `Match` continua exaustivo. Uma violação é bug do compilador: gera **E9001** ("internal compiler error") com a mensagem do validador e interrompe a geração.

## 2. Tradução para Rust (3b)

### Convenção de chamada

```rust
fn nome<T: Clone + Send + Sync + 'static, ...>(ctx: Ctx, a: A, b: B)
    -> BoxFuture<'static, Result<R, AppError>>
{ Box::pin(async move { ... }) }
```

`Ctx`: transação da requisição (compartilhada com segurança entre as funções da mesma chamada) e contador de profundidade de chamadas.

### Tipos

| Bind | Rust |
|---|---|
| `Int` | `BigInt` |
| `data class` / `entity` | `struct` (`Clone`; `PartialEq` se o tipo admite `==`) |
| `sealed class` / `enum class` | `enum` |
| `T?` | `Option<T>` |
| `Result<T, E>` | `enum BindResult<T, E> { Ok(T), Err(E) }` |
| `List<T>` | `Arc<Vec<T>>` |
| função | `Arc<dyn Fn(Ctx, A) -> BoxFuture<'static, Result<B, AppError>> + Send + Sync>` |

### Expressões

- `Let` → `let`; `If` → `if`; `Return` → `return Ok(...)`. Locais renomeados de forma determinística (`v12_nome`).
- `Match` sobre ADTs, `Option` e `BindResult` → `match` do Rust (segunda checagem de exaustividade, pelo `rustc`).
- `Match` com literais `Int`/`String` → cadeia de `if` terminando no braço `_`/variável exigido pelo verificador. O código gerado não contém `unreachable!()`.
- Lambdas → closures que capturam por cópia.

### Banco e endpoints

- Para cada tabela usada: `require`/`find`/`insert` sobre a transação do `Ctx`, com SQL parametrizado (sem concatenar valores). `require` sem resultado → `AppError::NotFound`.
- Roteador: decodifica a requisição, abre a transação, chama a função, faz commit em `Ok` e rollback em `Err`, codifica a resposta.

### Ausência de pânico

- `BigInt` nunca estoura; divisão e resto inteiros são checados (`Option`).
- `Decimal`/`Money` e `Duration` usam operações checadas; overflow vira `AppError::Overflow` (status 500, documentado).
- Profundidade de chamadas limitada pelo `Ctx` (10.000); excedê-la gera `AppError::RecursionLimit`.
- Nenhum `panic!`, `unreachable!` ou `unwrap` alcançável no código gerado ou no runtime.

## 3. Runtime, escalares e banco

### Runtime

Crate `runtime/bind-runtime/` no repositório do `bindc`, com testes próprios. Seu código-fonte é copiado para o workspace gerado (o projeto gerado continua autocontido, determinístico e com hash). Contém `Ctx`, `AppError`, `BindResult`, o tipo das funções, as builtins `first`, `get`, `length`, `map`, `fold`, `emptyList`, `listOf`, a aritmética checada e a serialização JSON dos escalares.

### Escalares

| Bind | Rust | PostgreSQL | JSON |
|---|---|---|---|
| `Int` | `BigInt` | `numeric` (via `BigDecimal`) | string |
| `Long` | `i64` | `bigint` | número |
| `Double` / `Float` | `f64` / `f32` | `double precision` / `real` | número |
| `Decimal` / `Money` | `rust_decimal::Decimal` | `numeric` | string |
| `Boolean` | `bool` | `boolean` | booleano |
| `String`, `Email`, `Url` | `String` | `text` | string |
| `UUID` | `uuid::Uuid` | `uuid` | string |
| `Date` / `DateTime` / `Instant` | `NaiveDate` / `NaiveDateTime` / `DateTime<Utc>` | `date` / `timestamp` / `timestamptz` | string ISO-8601 |
| `Duration` | `chrono::TimeDelta` | não suportado | não suportado |

### Dependências e perfil

- Novas dependências do código gerado, fixadas com `=` pelo compilador: `num-bigint`, `bigdecimal`, `rust_decimal`, `chrono`, `futures`, e as features correspondentes do `sqlx`. As versões exatas são escolhidas no plano.
- Perfil certificado passa a `rust-elm-web-v2`. Os artefatos gerados mudam; o critério deixa de ser hash idêntico e passa a ser `certify` aprovado mais testes de contrato e comportamento. O `website/deploy/` (commitado em outro repositório) precisa ser regenerado após o merge.

## 4. Contrato, cliente e perfil certificado

- Contrato JSON `bind-json-v2`: `Int`, `Decimal`, `Money` como string; schema e vetores de contrato atualizados, inclusive com vetores malformados.
- Cliente Elm: campos `Int` decodificados de string; string que não é inteiro ou passa de 2^53 → falha de decodificação (`Http.BadBody`); `Int` codificado como string na ida. O resto do cliente segue o perfil atual até o 3c.
- Perfil certificado do servidor:
  - sai a restrição de uma operação de banco por `server fun` (E8304);
  - endpoints usam a linguagem inteira, com várias operações de banco na mesma transação;
  - o Rust inclui as funções alcançáveis a partir de algum endpoint pelo grafo de chamadas da IR;
  - parâmetros e retorno de endpoints precisam ser tipos de fio (sem funções, sem `Duration`), E8xxx caso contrário.
- Fluxo:
```text
verify → types::check → core::lower → core::validate → VerifiedApplication { core, … }
certify/generate → backend Rust lê CoreProgram (servidor)
                 → backend Elm atual (cliente, até o 3c)
```
- A extração por tokens (`DatabaseOperation`) deixa de alimentar o servidor.

## 5. Testes e critérios de aceite

TDD, como nos subprojetos anteriores.

1. **IR**: testes unitários da tradução por construção; testes do validador com IR inválida montada à mão.
2. **Runtime**: `BigInt` em JSON (ida e volta, entrada malformada); aritmética checada gerando erro; builtins de lista; limite de profundidade.
3. **Snapshots**: `tests/codegen/<caso>.bd` → `<caso>.rs.expected`, regraváveis com `BIND_BLESS=1`.
4. **Teste diferencial**: avaliador de referência da IR (`core::eval`, código sem banco) contra o Rust gerado, compilado e executado, sobre programas puros aleatórios (inteiros grandes, ADTs, `when`, lambdas, `map`/`fold`, recursão limitada, `Int / Int`), agrupados em poucos crates de teste; ≥ 1.000 programas e 0 divergências.
5. **Banco e transações**: testes gerados de rollback (endpoint que grava e depois falha não deixa linha) e de `NotFound`; rodam com `BIND_TEST_DATABASE_URL`, senão são marcados como pulados no relatório.
6. **Ponta a ponta**: `examples/commerce` ganha um endpoint com função auxiliar, `when` e duas operações de banco; `certify --target aws-lambda` e `--target vercel` passam; o site passa em `check`, `generate` e `certify`.

### Critérios de aceite

1. O backend Rust do servidor lê só a `CoreProgram`.
2. Endpoints usam a linguagem inteira, com várias operações de banco numa transação; E8304 removido.
3. `Int` de precisão arbitrária de ponta a ponta (Rust, `numeric`, string no JSON, decodificação estrita no Elm).
4. Nenhum `panic!`, `unreachable!` ou `unwrap` alcançável no código gerado ou no runtime; overflow de `Decimal` e recursão profunda viram erros tipados.
5. Teste diferencial com ≥ 1.000 programas e 0 divergências.
6. `certify` aprovado nos dois targets com o `commerce` ampliado; site aprovado.
7. `cargo test`, clippy e fmt limpos; documentação do perfil `rust-elm-web-v2` publicada.
