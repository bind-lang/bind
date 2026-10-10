# Geração de código a partir da IR tipada (núcleo e servidor): plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Criar uma IR tipada (`CoreProgram`) como única entrada do backend do servidor e um backend Rust que gera endpoints com a linguagem inteira (funções auxiliares, `when`, ADTs, lambdas, recursão, várias operações de banco numa transação), com `Int` de precisão arbitrária de ponta a ponta.

**Architecture:** O verificador passa a registrar a resolução de cada nome; `core::lower` constrói a IR a partir da AST e do `TypedProgram`; `core::validate` confere a IR (erro = E9001, bug do compilador); um crate de runtime (`runtime/bind-runtime`) é copiado para o workspace gerado; o backend `src/backend/rust/` traduz a IR para Rust com convenção de chamada uniforme (`async`, `Result`, `Ctx`). O cliente Elm só muda os codecs de `Int`. Um avaliador de referência (`core::eval`) serve de oráculo para um teste diferencial contra o Rust gerado.

**Tech Stack:** Rust 1.98.0 (edition 2024); código gerado: `num-bigint`, `bigdecimal`, `rust_decimal`, `chrono`, `futures`, `sqlx` 0.9 (já usado), `tokio`, `serde`.

**Spec:** `docs/superpowers/specs/2026-10-11-typed-codegen-server-design.md` (autoridade). Contexto: `docs/type-system.md`, `docs/certified-profile-v0.1.md`, `docs/architecture.md`.

## Global Constraints

- Rust 1.98.0, edition 2024. O compilador `bindc` não ganha dependências de produção novas.
- Dependências do **código gerado** fixadas com `=` em `src/certified.rs` (constantes ao lado das existentes): `NUM_BIGINT = "0.4.6"`, `BIGDECIMAL = "0.4.8"`, `RUST_DECIMAL = "1.37.2"`, `CHRONO = "0.4.41"`, `FUTURES = "0.3.31"`. Se uma feature do `sqlx` 0.9 exigir outra versão de `bigdecimal`/`rust_decimal`/`chrono`, use a versão exigida pelo `sqlx` e registre a troca no relatório; nunca use faixas de versão.
- Perfil certificado `rust-elm-web-v2`; contrato `bind-json-v2` (`wireVersion` no manifesto).
- Representações: `Int` → `num_bigint::BigInt` (PostgreSQL `numeric` via `BigDecimal`, JSON string); `Decimal`/`Money` → `rust_decimal::Decimal` (`numeric`, JSON string); `Date`/`DateTime`/`Instant` → `chrono::NaiveDate`/`NaiveDateTime`/`DateTime<Utc>` (`date`/`timestamp`/`timestamptz`, JSON string ISO-8601); `Duration` → `chrono::TimeDelta` só em memória; `List<T>` → `Arc<Vec<T>>`; `Result<T, E>` → `BindResult<T, E>`; funções → `Fun` (ver Task 5).
- Convenção de chamada: toda função do Bind vira `fn f<…>(ctx: Ctx, …) -> BoxFuture<'static, Result<R, AppError>>`.
- Toda chamada RPC roda numa transação: commit em `Ok`, rollback em `Err`.
- Limite de profundidade de chamadas: 10.000 (`AppError::RecursionLimit`).
- Código gerado e runtime sem `panic!`, `unreachable!`, `unwrap`, `expect` ou indexação que possa falhar (`#![forbid(unsafe_code)]` continua; acrescente `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable)]` no runtime e no `lib.rs` gerado).
- Códigos novos: E8309 (endpoint com parâmetro/retorno que não é tipo de fio, ou `Duration` como coluna), E9001 (erro interno: IR inválida). E8304 deixa de existir.
- `cargo test` verde, `cargo clippy --all-targets -- -D warnings` e `cargo fmt --check` limpos ao fim de cada tarefa. Commits terminam com `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` (ou o modelo que de fato escreveu o commit).
- Branch `typed-codegen` no repositório `bind-lang`. Comandos que geram/certificam usam `DATABASE_URL=postgresql://bind:bind@localhost/bind`.

## Review Focus

1. **`Int` maior que 2^53 chegando ao cliente.** O Elm deve falhar a decodificação (`Http.BadBody`), nunca mostrar um número arredondado. Teste na Task 8.
2. **`require` sem resultado depois de um `insert` no mesmo endpoint.** A transação inteira é desfeita; nenhuma linha fica gravada. Teste na Task 10 (com `BIND_TEST_DATABASE_URL`).
3. **Recursão muito profunda** (ex.: 100.000 chamadas). Resposta com erro `RecursionLimit`, sem derrubar o processo. Teste na Task 5 (runtime) e na Task 9 (programa gerado).
4. **Overflow de `Money`.** Soma acima do limite do `Decimal` vira `AppError::Overflow` (status 500), sem pânico. Teste na Task 5.
5. **Endpoint que retorna uma função ou `Duration`.** Rejeitado com E8309 antes da geração. Teste na Task 7.

## Estrutura de arquivos

| Arquivo | Ação | Responsabilidade |
|---|---|---|
| `src/types/infer.rs` | modificar | registrar `Resolution` por nó `Name`/`Field` em `TypedProgram.resolutions` |
| `src/core/mod.rs` | criar | `pub mod ir; pub mod lower; pub mod validate;` (`#[cfg(test)] pub mod eval;`) |
| `src/core/ir.rs` | criar | estruturas da IR |
| `src/core/lower.rs` | criar | AST + `TypedProgram` → `CoreProgram` |
| `src/core/validate.rs` | criar | consistência da IR (E9001) |
| `src/core/eval.rs` | criar | avaliador de referência (só testes) |
| `runtime/bind-runtime/` | criar | crate de runtime do código gerado (Cargo.toml próprio, `src/lib.rs`, testes) |
| `src/backend/mod.rs`, `src/backend/rust/{mod,types,expr,db,endpoints}.rs` | criar | backend Rust a partir da IR |
| `src/compiler.rs` | modificar | `VerifiedApplication.core`, lowering + validação, E8309, remoção de E8304 |
| `src/certified.rs` | modificar | usar o backend novo para o servidor, dependências, perfil v2, contrato v2, codecs Elm de `Int` |
| `src/pipeline.rs` | modificar | `wireVersion` v2 |
| `src/main.rs` | modificar | `mod core; mod backend;` |
| `tests/codegen/*` | criar | snapshots do Rust gerado |
| `examples/commerce/src/*.bd`, `examples/commerce/schema/main.csv` | modificar | endpoint ampliado |
| `docs/certified-profile-v0.2.md`, `docs/architecture.md`, `README.md` | criar/modificar | documentação |

---

### Task 1: Resolução de nomes registrada pelo verificador

**Files:**
- Modify: `src/types/infer.rs`

**Interfaces:**
- Produces (`types::infer`, reexportado em `types`):
```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// A parameter, `val`, lambda parameter or pattern binding of the current body.
    Local,
    /// A top-level function of the program.
    Function(String),
    /// `first`, `get`, `length`, `map`, `fold`, `emptyList`, `listOf`.
    Builtin(String),
    /// A constructor used as a value or called (`Pago(…)`, `Ok(…)`, `Cancelado`).
    Constructor(String),
    /// A value declared in `environment`.
    Environment(String),
    /// `server.f` (recorded on the `Field` node).
    ServerCall(String),
    /// `Tipo.Variante` (recorded on the `Field` node): the constructor name.
    Variant(String),
}
```
  `TypedProgram.resolutions: BTreeMap<(String, NodeId), Resolution>` (chave: função, nó). Nomes de nós de UI, `db.<t>.<op>` e `listOf` chamados diretamente também são registrados: `listOf` como `Builtin("listOf")`; nós de UI e operações de banco **não** precisam de registro (o lowering os reconhece estruturalmente, como o verificador).
- Regra: o registro reflete exatamente a escolha que o verificador fez (mesma ordem: locais, globais, construtores…), inclusive com sombreamento.

- [ ] **Step 1: Testes que falham** (em `src/types/infer.rs`, `mod tests`):

```rust
    fn resolutions(source: &str) -> Vec<(NodeId, Resolution)> {
        let (tokens, _) = crate::lexer::lex("test.bd", source);
        let (program, _) = crate::parser::parse(tokens);
        let (typed, diagnostics) = crate::types::check(&program, &DatabaseSchema::default());
        assert!(!crate::diagnostic::has_errors(&diagnostics), "{diagnostics:?}");
        typed.resolutions.into_iter().filter(|((function, _), _)| function == "f").map(|((_, id), r)| (id, r)).collect()
    }

    #[test]
    fn records_how_each_name_was_resolved() {
        let source = "sealed class Cor {\n    data object Azul\n    data class Rgb(val r: Int)\n}\nfun g(x: Int) = x\nfun f(x: Int): Int {\n    val y = g(x)\n    val c = Cor.Azul\n    val d = Rgb(1)\n    val xs = listOf(y)\n    length(xs)\n}\n";
        let found: Vec<Resolution> = resolutions(source).into_iter().map(|(_, r)| r).collect();
        assert!(found.contains(&Resolution::Function("g".into())));
        assert!(found.contains(&Resolution::Local));
        assert!(found.contains(&Resolution::Variant("Azul".into())));
        assert!(found.contains(&Resolution::Constructor("Rgb".into())));
        assert!(found.contains(&Resolution::Builtin("listOf".into())));
        assert!(found.contains(&Resolution::Builtin("length".into())));
    }

    #[test]
    fn shadowing_is_recorded_as_local() {
        let found: Vec<Resolution> = resolutions("fun length(x: Int) = x\nfun f(length: Int): Int = length\n").into_iter().map(|(_, r)| r).collect();
        assert_eq!(found, vec![Resolution::Local]);
    }
```

- [ ] **Step 2: Rodar e confirmar a falha.** `cargo test types::infer::tests::records 2>&1 | tail -5` → erro de compilação (`Resolution` inexistente).

- [ ] **Step 3: Implementar.** Acrescente o enum e o campo `resolutions` (`TypedProgram`, preenchido em `finish` a partir de um campo `resolutions` do `Checker`). Registre no braço `ExprKind::Name` de `infer` (antes de chamar `infer_name`, decida a resolução com a mesma ordem de `infer_name`: local → global (função do programa, builtin, valor de `environment`) → construtor) e no braço `ExprKind::Field` quando `infer_field` trata `server.f` (`ServerCall`) ou `Tipo.Variante` (`Variant`). Distinga função do programa de builtin e de `environment` usando `function_spans` (funções do programa) e o nome do valor de `environment` (guarde os nomes em `install_environment`). Exporte `Resolution` em `src/types/mod.rs` (`pub use infer::{Resolution, TypedProgram};`).

- [ ] **Step 4: Rodar.** `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` → verdes.

- [ ] **Step 5: Commit.** `git add src/types && git commit -m "feat(types): registra a resolução de cada nome"`

---

### Task 2: IR do núcleo e tradução (lowering)

**Files:**
- Create: `src/core/mod.rs`, `src/core/ir.rs`, `src/core/lower.rs`
- Modify: `src/main.rs` (`mod core;`)

**Interfaces:**
- Consumes: `TypedProgram` (`signatures`, `node_types`, `patterns`, `effects`, `resolutions`), `Program`, `Env` não é necessário (os tipos declarados vêm de `Program.types`), `types::ty::{Type, display}`, `types::exhaustive::Pat`, `types::effects::Row`.
- Produces: `core::ir` (abaixo) e `core::lower::lower(program: &Program, typed: &TypedProgram, schema: &DatabaseSchema) -> CoreProgram`.

- [ ] **Step 1: Criar `src/core/ir.rs`**

```rust
//! The typed core IR: the single input of code generators. Every expression carries its
//! resolved type; operators are concrete; sugar and blocks are gone; patterns are elaborated.

use crate::types::exhaustive::Pat;
use crate::types::ty::Type;

pub type LocalId = usize;

#[derive(Clone, Debug, PartialEq)]
pub struct CoreProgram {
    pub types: Vec<CoreType>,
    pub functions: Vec<CoreFunction>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CoreType {
    Record { name: String, params: Vec<String>, fields: Vec<(String, Type)> },
    Sum { name: String, params: Vec<String>, variants: Vec<(String, Vec<(String, Type)>)> },
    Distinct { name: String, underlying: Type },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Domain {
    Pure,
    Inferred,
    Server,
    Client,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CoreFunction {
    pub name: String,
    pub domain: Domain,
    /// A `server fun` that is not a component: an RPC endpoint.
    pub endpoint: bool,
    /// Type variables of the scheme, in order of appearance (rendered by backends as generics).
    pub generics: Vec<usize>,
    pub params: Vec<(LocalId, String, Type)>,
    pub result: Type,
    /// Resolved effect labels, e.g. `["db.read"]`.
    pub effects: Vec<String>,
    pub body: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub ty: Type,
    pub kind: ExprKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    /// Decimal digits of an arbitrary-precision integer.
    Int(String),
    Double(String),
    Float(String),
    Str(String),
    Bool(bool),
    Unit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prim {
    IntAdd, IntSub, IntMul, IntNeg, IntDivChecked, IntRemChecked,
    DoubleAdd, DoubleSub, DoubleMul, DoubleDiv, DoubleNeg,
    FloatAdd, FloatSub, FloatMul, FloatDiv, FloatNeg,
    DecimalAdd, DecimalSub, DecimalMul, DecimalDivChecked, DecimalNeg,
    DurationAdd, DurationSub, DurationNeg,
    StringConcat,
    /// Structural equality / ordering on the operand type (`ty` of the operands).
    Eq, NotEq, Lt, LtEq, Gt, GtEq,
    And, Or, Not,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DbOp {
    Require,
    Find,
    Insert { columns: Vec<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    Lit(Literal),
    Local(LocalId),
    /// A program function; `instance` lists the concrete types for the callee's generics.
    Global { name: String, instance: Vec<Type> },
    Builtin { name: String, instance: Vec<Type> },
    Environment(String),
    /// Construct a record, variant, distinct value, `Some` or `null`.
    Ctor { type_name: String, ctor: String, args: Vec<Expr> },
    None,
    Some(Box<Expr>),
    Call { callee: Box<Expr>, args: Vec<Expr> },
    /// `server.f` is not part of server code (Task 7 rejects it there); kept for completeness.
    ServerCall { function: String, args: Vec<Expr> },
    Lambda { params: Vec<(LocalId, String, Type)>, body: Box<Expr> },
    Let { local: LocalId, name: String, value: Box<Expr>, body: Box<Expr> },
    /// Evaluate `value` for its effects, discard it, continue with `body`.
    Seq { value: Box<Expr>, body: Box<Expr> },
    If { condition: Box<Expr>, then_branch: Box<Expr>, else_branch: Box<Expr> },
    Match { scrutinee: Box<Expr>, arms: Vec<Arm> },
    Field { target: Box<Expr>, name: String },
    Prim { op: Prim, args: Vec<Expr> },
    Db { op: DbOp, table: String, args: Vec<Expr> },
    Return(Box<Expr>),
    /// A UI tree: never part of server code; carried so client lowering (3c) can reuse the IR.
    Ui,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arm {
    /// The elaborated pattern (`Some(...)` explicit) recorded by the type checker.
    pub pattern: Pat,
    /// Variables bound by the pattern, in pattern order, with their types.
    pub bindings: Vec<(LocalId, String, Type)>,
    /// For variable bindings, the path from the scrutinee: constructor field indices, with
    /// `Some` unwraps written as index 0 of constructor `Some`.
    pub binding_paths: Vec<Vec<(String, usize)>>,
    pub body: Expr,
}
```

- [ ] **Step 2: Testes que falham** em `src/core/lower.rs` (`mod tests`). Use um helper que compila o fonte (lex, parse, `types::check`) e chama `lower`; compare com renderizações curtas de uma função auxiliar de teste `render(&Expr) -> String` que vocês definem no próprio módulo de testes (ex.: `IntAdd(Local(0), Lit(1))`). Casos obrigatórios:
  1. `fun f(x: Int) = x + 1` → corpo `Prim IntAdd [Local x, Lit Int "1"]`, `result` `Int`.
  2. `fun f(a: Money, b: Money) = a + b` → `DecimalAdd`; `fun f(a: String, b: String) = a + b` → `StringConcat`; `fun f(a: Double, b: Double) = a / b` → `DoubleDiv`; `fun f(a: Int, b: Int) = a / b` → `IntDivChecked` com tipo `Int?`.
  3. `fun f(x: Int?) = x ?: 0` → `Match` com braços `null` e `Some(_)` (variável ligada com tipo `Int`).
  4. `data class P(val n: String?)\nfun f(p: P?) = p?.n` → `Match` aninhado, tipo `String?`.
  5. Bloco `{ val a = 1\n val _ = a\n a + 1 }` → `Let a … Seq … Prim`.
  6. `sealed class C { data object A\n data class B(val n: Int) }\nfun f(c: C) = when (c) {\n A -> 0\n B(n) -> n\n}` → `Match` com `Pat::Ctor("A")` e `Pat::Ctor("B", [Wild])`, `binding_paths` de `n` = `[("B", 0)]`.
  7. `fun <T> ident(x: T): T = x\nfun f() = ident(1)` → em `f`, `Call(Global{name:"ident", instance:[Int]}, [Lit 1])`; `ident.generics.len() == 1`.
  8. `fun f(xs: List<Int>) = map(xs, { x -> x * 2 })` → `Call(Builtin{name:"map", instance:[Int, Int]}, [Local xs, Lambda])`.
  9. Com o schema de teste de clientes: `server fun f(id: ClienteId): Cliente effects [db.read] = db.clientes.require(id)` → `Db{Require, "clientes", [Local id]}`; `endpoint == true`; `effects == ["db.read"]`. `insert { nome = n }` → `DbOp::Insert{columns:["nome"]}` com os valores na ordem das colunas escritas.
  10. Tipos: `CoreType::Sum` para `sealed`/`enum`/`Result`, `Record` para `data class`/`entity`, `Distinct` para `distinct type`.

- [ ] **Step 3: Implementar `lower`.** Regras:
  - Percorra `program.functions` na ordem do arquivo; para cada função, `generics` = variáveis livres de `typed.signatures[name]` em ordem de aparição; parâmetros com `LocalId` novos; `result`/tipos dos parâmetros a partir da assinatura.
  - Escopo de locais: pilha `Vec<(String, LocalId)>`, mesmas regras do verificador (parâmetros, `val`, parâmetros de lambda, ligações de padrão), sombreamento pelo mais recente.
  - Tipo de cada nó: `typed.node_types[(função, id)]` (resolvido); para nós sem registro (não deveria ocorrer), use `Type::Error` e deixe o validador acusar.
  - `Name`: use `typed.resolutions`. `Local` → `Local(id do escopo)`; `Function(n)` → `Global{name, instance}` onde `instance` é obtida casando o esquema genérico (`typed.signatures[n]`) com o tipo do nó (casamento estrutural, variável → tipo); `Builtin(n)` → `Builtin{name, instance}` (mesma técnica usando os esquemas das builtins: reproduza-os num helper `builtin_scheme(name) -> Type` em `lower.rs`, com variáveis 0,1,…); `Constructor(c)` → construtor sem argumentos (`Ctor` com `args` vazio) ou valor função (lambda que constrói — gere `Lambda` com parâmetros do tipo dos campos e corpo `Ctor`); `Environment(n)` → `Environment(n)`.
  - `Call` com callee `Name` resolvido como `Constructor` → `Ctor{args}` direto; `listOf(...)` → `Builtin listOf` aplicado; nós de UI → `Ui`; `db.t.require/find(k)` e `db.t.insert {…}` → `Db`.
  - `Binary`: escolha o `Prim` pelo tipo resolvido do operando esquerdo (`Int`→`Int*`, `Double`, `Float`, `Decimal`/`Money`→`Decimal*`, `Duration`→`Duration*`, `String`+`String`→`StringConcat`, comparações/igualdade → `Eq`…`GtEq`, `%` → `IntRemChecked`, `/` em `Int` → `IntDivChecked`, em `Decimal` → `DecimalDivChecked`). `Unary` → `IntNeg`/`DoubleNeg`/…/`Not`.
  - `Elvis(a, b)` → `Match a { null -> b; Some(x) -> x }`; `SafeField(t, n)` → `Match t { null -> None; Some(x) -> (campo é T? ? x.n : Some(x.n)) }`.
  - `Block` → `Let`/`Seq` encadeados; bloco vazio → `Lit Unit`.
  - `When` → `Match` com os padrões de `typed.patterns[(função, id do when)]`; `bindings`/`binding_paths` calculados percorrendo o padrão sintático junto com o `Pat` elaborado.
  - `If`, `Lambda`, `Field`, `Return` → os nós correspondentes.
  - `endpoint = domain == Server && !component`; `effects = typed.effects[name]`.
  - Tipos: a partir de `program.types` (`TypeDecl.kind`, `cases`, `fields`, `type_params`) + `Result` embutido; tipos dos campos com `crate::parser::expr::parse_type_text` + uma conversão `TypeExpr → Type` local (sem `Env`; parâmetros de tipo viram `Type::Con("'T")` só como marcadores de nome — guarde também `params: Vec<String>`).

- [ ] **Step 4: Rodar.** `cargo test core::`, `cargo test`, clippy, fmt → verdes.

- [ ] **Step 5: Commit.** `git add src/core src/main.rs && git commit -m "feat(core): IR tipada do núcleo e lowering"`

---

### Task 3: Validador da IR e integração ao `verify`

**Files:**
- Create: `src/core/validate.rs`
- Modify: `src/core/mod.rs`, `src/compiler.rs`

**Interfaces:**
- Produces: `core::validate::validate(program: &CoreProgram) -> Vec<String>` (mensagens; vazio = válido); `VerifiedApplication.core: CoreProgram`; diagnóstico E9001.
- Regras do validador:
  1. Nenhum nó tem `Type::Error` ou variável de tipo livre fora de `generics` da função.
  2. `Local(id)` só aparece dentro do escopo que o declara.
  3. `Global{name}` existe em `functions`; `Builtin{name}` é uma das sete builtins; `Ctor{type_name, ctor}` existe nos tipos com o número certo de argumentos.
  4. Tipos batem: `If` com condição `Boolean` e ramos iguais ao tipo do nó; `Prim` com operandos do tipo esperado pelo operador; `Call` com aridade e tipos de argumentos iguais aos do tipo função do callee; `Let`/`Seq` com o tipo do corpo; `Match` com braços do tipo do nó.
  5. Todo `Match` é exaustivo (reuse `types::exhaustive::check_match` com uma implementação de `Signatures` sobre `CoreProgram.types`).
  6. Um `Db` fora de lambdas só aparece em funções cujo `effects` contém o rótulo correspondente (`db.read` para `Require`/`Find`, `db.write` para `Insert`); dentro de uma `Lambda`, o rótulo precisa estar na linha do tipo função da lambda mais interna (uma lambda nunca chamada pode conter efeitos que a função não tem).

- [ ] **Step 1: Testes que falham** (`src/core/validate.rs`):
  - Um programa real (o mesmo do caso 6 da Task 2) produz `validate(...) == []`.
  - IRs montadas à mão e inválidas, cada uma com a mensagem esperada contendo uma palavra-chave: `If` com condição `Int` ("condition"); `Local(99)` fora de escopo ("scope"); `Global{"nada"}` ("unknown function"); `Prim IntAdd` com um `String` ("operand"); `Match` sem o braço `null` sobre `Int?` ("exhaustive"); `Db Insert` numa função com `effects == []` ("effect").
  - Em `src/compiler.rs`: um teste que injeta um validador com falha não é viável sem gancho; em vez disso, teste que `verify(SOURCE)` produz `application.core.functions` com `getCustomer` e `CustomerPage`, e que nenhum diagnóstico E9001 aparece.

- [ ] **Step 2: Rodar e confirmar a falha.**

- [ ] **Step 3: Implementar.** `validate` conforme as regras. Em `compiler::verify`, depois do verificador de tipos sem erros: `let core = crate::core::lower::lower(&program, &typed, &database);`, `let problems = crate::core::validate::validate(&core);`; se houver problemas, emita um único `Diagnostic::error("E9001", format!("Internal compiler error: the core IR is inconsistent ({}). This is a bug in bindc.", problems.join("; ")), Span::default())` e retorne sem `application`. Acrescente `pub core: CoreProgram` a `VerifiedApplication` e preencha-o.

- [ ] **Step 4: Rodar.** `cargo test`, clippy, fmt; `DATABASE_URL=… cargo run --quiet -- check --target aws-lambda` e `make -C ../website check` → código 0.

- [ ] **Step 5: Commit.** `git add src && git commit -m "feat(core): validador da IR e E9001"`

---

### Task 4: Avaliador de referência (`core::eval`)

**Files:**
- Create: `src/core/eval.rs` (`#[cfg(test)]`)
- Modify: `src/core/mod.rs`, `Cargo.toml` (dev-dependency `num-bigint = "=0.4.6"`)

**Interfaces:**
- Produces (só em testes): `core::eval::{Value, eval_function(program: &CoreProgram, name: &str, args: Vec<Value>) -> Result<Value, EvalError>}` e `Value::render(&self) -> String` (forma canônica usada pelo teste diferencial: inteiros em decimal, strings com aspas JSON, `true`/`false`, `null`, `Some(v)` renderizado como `v`, construtores como `Nome(a, b)`, listas como `[a, b]`, `()` para Unit).
- `Value`: `Int(BigInt)`, `Bool`, `Str`, `Unit`, `None`, `Some(Box)`, `Ctor{name, fields}`, `List(Vec)`, `Closure{params, body, env}`, `Function(name)`, `Builtin(name)`.
- `EvalError`: `Unsupported(String)` (banco, `Double`/`Float`/`Decimal`/datas, UI, `Environment`), `RecursionLimit`.
- Semântica: igual à do Rust gerado (Task 6). Divisão e resto inteiros truncam em direção a zero, com sinal do resto igual ao do dividendo, `None` para divisor zero; `IntDivChecked`/`IntRemChecked` devolvem `Some`/`None`; `Return` encerra a função corrente; limite de profundidade 10.000 (`RecursionLimit`).

- [ ] **Step 1: Testes que falham** (`src/core/eval.rs`): compile o fonte com o helper da Task 2 e avalie:
  - `fun f(x: Int) = x * x + 1` com `x = 12345678901234567890` → `"152415787532388367504953347995733866101"` (= x² + 1).
  - `fun f(a: Int, b: Int) = (a / b) ?: -1` com `(7, 2)` → `"3"`, `(-7, 2)` → `"-3"`, `(7, 0)` → `"-1"`; `fun f(a: Int, b: Int) = (a % b) ?: 99` com `(-7, 2)` → `"-1"`.
  - ADT + `when`: árvore `sealed class Arvore { data class No(val e: Arvore, val v: Int, val d: Arvore)\n data object Folha }` com `soma` recursiva sobre `No(No(Folha, 1, Folha), 2, Folha)` → `"3"`.
  - `fun f(xs: List<Int>) = fold(map(xs, { x -> x * 2 }), 0, { s, x -> s + x })` com `[1, 2, 3]` → `"12"`.
  - `fun f(x: Int?): Int = when (x) {\n null -> 0\n n -> n + 1\n}` com `null` → `"0"`, com `4` → `"5"`.
  - `fun loop(n: Int): Int = if (n == 0) 0 else loop(n - 1)` com `20000` → `Err(RecursionLimit)`.
  - Banco → `Err(Unsupported)`.
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar** um interpretador recursivo sobre a IR (ambiente de locais por `LocalId`; `Match` usando `pattern` + `binding_paths`; `Global` busca a função no programa; `Builtin` implementa as sete builtins; contador de profundidade em cada chamada de função/closure).
- [ ] **Step 4: Rodar.** `cargo test core::eval`, `cargo test`, clippy, fmt.
- [ ] **Step 5: Commit.** `git commit -m "test(core): avaliador de referência da IR"`

---

### Task 5: Runtime do código gerado (`runtime/bind-runtime`)

**Files:**
- Create: `runtime/bind-runtime/Cargo.toml`, `runtime/bind-runtime/src/lib.rs`
- Modify: `Makefile` (`test` também roda `cargo test --manifest-path runtime/bind-runtime/Cargo.toml`)

**Interfaces:**
- Produces (módulo copiado para o crate gerado como `src/bind_runtime.rs` e referido como `crate::bind_runtime`):
```rust
pub const MAX_DEPTH: u32 = 10_000;
pub type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, AppError>> + Send>>;

#[derive(Debug)]
pub enum AppError { BadRequest(String), NotFound(String), Database(sqlx::Error), Serialization(serde_json::Error), Overflow(String), RecursionLimit, Internal(String) }
impl AppError { pub fn status(&self) -> u16; pub fn public_message(&self) -> String }  // 400, 404, 500…; nunca expõe detalhes de banco

#[derive(Clone)]
pub struct Ctx { /* transação opcional compartilhada (Arc<tokio::sync::Mutex<sqlx::Transaction<'static, sqlx::Postgres>>>) + profundidade */ }
impl Ctx {
    pub fn pure() -> Ctx;                                         // sem banco (testes, código puro)
    pub fn with_transaction(tx: sqlx::Transaction<'static, sqlx::Postgres>) -> Ctx;
    pub fn enter(&self) -> Result<Ctx, AppError>;                 // depth + 1, ou RecursionLimit
    pub async fn transaction(&self) -> Result<tokio::sync::MutexGuard<'_, sqlx::Transaction<'static, sqlx::Postgres>>, AppError>; // Internal se não há banco
    pub fn into_transaction(self) -> Option<sqlx::Transaction<'static, sqlx::Postgres>>; // para commit/rollback no roteador
}

/// Arbitrary-precision integer; JSON string; PostgreSQL `numeric` (must be integral).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Int(pub num_bigint::BigInt);
impl Int {
    pub fn from_i64(value: i64) -> Int;
    /// Infallible construction of big literals: sign and base-2^32 digits, least significant first.
    pub fn from_parts(negative: bool, digits: &[u32]) -> Int;
    pub fn add(&self, o: &Int) -> Int; pub fn sub(&self, o: &Int) -> Int; pub fn mul(&self, o: &Int) -> Int; pub fn neg(&self) -> Int;
    pub fn div_checked(&self, o: &Int) -> Option<Int>;  // trunca em direção a zero; None se o == 0
    pub fn rem_checked(&self, o: &Int) -> Option<Int>;  // sinal do dividendo; None se o == 0
    pub fn to_index(&self) -> Option<usize>;
}
// Serialize/Deserialize como string estrita: `-?[0-9]+`, sem espaços, sem `+`, sem zeros à esquerda (exceto "0").
// sqlx::{Type, Encode, Decode} para Postgres via bigdecimal::BigDecimal (Decode falha se não for inteiro).

pub fn decimal_add(a: &rust_decimal::Decimal, b: &rust_decimal::Decimal) -> Result<rust_decimal::Decimal, AppError>; // + sub, mul, neg
pub fn decimal_div_checked(a, b) -> Result<Option<Decimal>, AppError>; // Ok(None) se b == 0; Err(Overflow) se estourar
pub fn duration_add(a: &chrono::TimeDelta, b: &chrono::TimeDelta) -> Result<chrono::TimeDelta, AppError>; // + sub, neg

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum BindResult<T, E> { Ok(T), Err(E) }

pub fn first<T: Clone>(xs: &std::sync::Arc<Vec<T>>) -> Option<T>;
pub fn get<T: Clone>(xs: &std::sync::Arc<Vec<T>>, index: &Int) -> Option<T>;
pub fn length<T>(xs: &std::sync::Arc<Vec<T>>) -> Int;
pub fn empty_list<T>() -> std::sync::Arc<Vec<T>>;
pub fn list_of<T>(items: Vec<T>) -> std::sync::Arc<Vec<T>>;
pub async fn map<A: Clone + Send + Sync + 'static, B: Send + 'static>(ctx: Ctx, xs: std::sync::Arc<Vec<A>>, f: std::sync::Arc<dyn Fn(Ctx, A) -> BoxFuture<B> + Send + Sync>) -> Result<std::sync::Arc<Vec<B>>, AppError>;
pub async fn fold<A: Clone + Send + Sync + 'static, B: Send + 'static>(ctx: Ctx, xs: std::sync::Arc<Vec<A>>, initial: B, f: std::sync::Arc<dyn Fn(Ctx, B, A) -> BoxFuture<B> + Send + Sync>) -> Result<B, AppError>;
```
- `runtime/bind-runtime/Cargo.toml`: as mesmas dependências e versões fixadas do código gerado (`serde`, `serde_json`, `tokio`, `sqlx` com features `runtime-tokio, tls-rustls, postgres, uuid, chrono, bigdecimal, rust_decimal`, `num-bigint`, `bigdecimal`, `rust_decimal` com feature `serde-str`, `chrono` com feature `serde`, `futures`). O arquivo `src/lib.rs` deve compilar tanto como crate próprio quanto como módulo `crate::bind_runtime` (sem `crate::` absolutos internos; use `super::`/caminhos relativos ou nomes plenos de crates externos).
- `src/certified.rs` incorpora o fonte com `include_str!("../runtime/bind-runtime/src/lib.rs")` (Task 7).

- [ ] **Step 1: Testes que falham** (no próprio `lib.rs`, `#[cfg(test)]`):
  - `Int` JSON: `"12345678901234567890123"` ida e volta; rejeita `"1.5"`, `" 1"`, `"+1"`, `"01"`, `"1e3"`, número JSON `1`.
  - `from_parts(false, &[0, 1])` == 2^32; `from_parts(true, &[5])` == −5.
  - `div_checked`/`rem_checked`: `(-7, 2)` → `-3` e `-1`; divisor zero → `None`.
  - `decimal_add(Decimal::MAX, 1)` → `Err(AppError::Overflow(_))`; `decimal_div_checked(1, 0)` → `Ok(None)`.
  - `Ctx::pure().enter()` repetido `MAX_DEPTH` vezes → `Ok`; mais uma → `Err(RecursionLimit)`.
  - `map`/`fold` com closures `Arc::new(|ctx, x| Box::pin(async move { … }))` produzem os valores esperados; um erro na closure interrompe e propaga.
  - **Review Focus 3:** uma função recursiva escrita no estilo gerado (`fn r(ctx: Ctx, n: Int) -> BoxFuture<Int>` que chama `ctx.enter()?` e a si mesma) com `n = 100_000`, executada num `tokio` runtime cujas threads têm pilha de 64 MiB (`Builder::new_multi_thread().thread_stack_size(64 * 1024 * 1024)`), termina com `Err(RecursionLimit)` sem estouro.
  - `AppError::public_message` para `Database`/`Internal` não contém o texto interno.
- [ ] **Step 2: Rodar e confirmar a falha.** `cargo test --manifest-path runtime/bind-runtime/Cargo.toml`
- [ ] **Step 3: Implementar.** Sem `unwrap`/`expect`/`panic!`/`unreachable!` (o `#![deny(...)]` dos Global Constraints vale aqui).
- [ ] **Step 4: Rodar.** testes do runtime + `cargo clippy --manifest-path runtime/bind-runtime/Cargo.toml --all-targets -- -D warnings` + `cargo fmt --manifest-path runtime/bind-runtime/Cargo.toml --check`; `cargo test` do `bindc` continua verde.
- [ ] **Step 5: Commit.** `git add runtime Makefile && git commit -m "feat(runtime): runtime do código gerado"`

---

### Task 6: Backend Rust — tipos e expressões

**Files:**
- Create: `src/backend/mod.rs`, `src/backend/rust/mod.rs`, `src/backend/rust/types.rs`, `src/backend/rust/expr.rs`, `tests/codegen/*.bd`, `tests/codegen/*.rs.expected`
- Modify: `src/main.rs` (`mod backend;`)

**Interfaces:**
- Consumes: `CoreProgram` (Task 2), API do runtime (Task 5).
- Produces:
  - `backend::rust::types::rust_type(ty: &Type, program: &CoreProgram) -> String` e `type_declarations(program: &CoreProgram) -> String` (structs/enums com `#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]`, mais `PartialEq` quando o tipo admite `==` — mesma regra do verificador: sem funções nem `Ui` dentro).
  - `backend::rust::expr::function_item(function: &CoreFunction, program: &CoreProgram) -> String` (uma função na convenção uniforme).
  - `backend::rust::render_functions(program: &CoreProgram, names: &[String]) -> String` (tipos + funções pedidas, sem banco/roteador; usado pelos snapshots, pelo teste diferencial e pela Task 7).
- Regras de tradução:
  - Mapeamento de tipos dos Global Constraints; variáveis de tipo da função → genéricos `T0, T1…` com `T: Clone + Send + Sync + 'static`; tipo função `(A, B) -> R ! …` → `std::sync::Arc<dyn Fn(Ctx, A, B) -> BoxFuture<R> + Send + Sync>`.
  - Cada função: `pub fn nome<T…>(ctx: Ctx, p0: A, …) -> BoxFuture<R> { Box::pin(async move { let ctx = ctx.enter()?; … }) }` (nome em snake_case com prefixo `bind_` para não colidir; locais `v{id}_{nome}`).
  - Toda expressão é traduzida para uma expressão Rust de tipo `R` (já sem `Result`), propagando erros com `?` e esperando chamadas com `.await?`. Uso de um local clona (`v3_x.clone()`).
  - `Lit Int` → `Int::from_i64(n)` quando cabe em `i64`, senão `Int::from_parts(neg, &[…])` com os dígitos base 2^32 calculados pelo compilador (implemente a conversão decimal→base 2^32 em `backend/rust/expr.rs` sem dependências).
  - `Prim`: `Int*` → métodos de `Int`; `IntDivChecked` → `a.div_checked(&b)`; `Decimal*` → funções `decimal_*(…)?`; `Duration*` → `duration_*(…)?`; `Double*`/`Float*` → operadores nativos; `StringConcat` → `format!("{a}{b}")`; `Eq`/`NotEq` → `==`/`!=`; ordenações → `<` etc.; `And`/`Or` com curto-circuito.
  - `Global` → `bind_nome::<instância…>(ctx.clone(), args…).await?`; `Builtin` aplicada → função correspondente do runtime (`map`/`fold` com `.await?`); `Global`/`Builtin`/construtor usados como valor → closure `Arc::new(move |ctx, a| Box::pin(async move { … }))`.
  - `Call` de um valor função → `(f)(ctx.clone(), args…).await?`.
  - `Lambda` → `Arc::new(move |ctx: Ctx, p…| { let capturas = capturas.clone(); Box::pin(async move { let ctx = ctx.enter()?; Ok(corpo) }) })`.
  - `Let` → `{ let vN = valor; corpo }`; `Seq` → `{ let _ = valor; corpo }`; `If` → `if c { a } else { b }`; `Return(v)` → `return Ok(v)`.
  - `Match` sobre `Sum`/`Option`/`BindResult` → `match` do Rust com os padrões elaborados (ligações por `binding_paths`); `Match` com literais `Int`/`String`/`Boolean` → cadeia `if/else` terminando no braço curinga/variável.
  - `Ctor` de record → `Nome { campo: … }`; de variante → `Tipo::Variante { … }` ou `Tipo::Variante`; `None`/`Some`.
  - `Field` → `alvo.campo.clone()`.
  - `Db`, `Environment`, `ServerCall`, `Ui` → nesta tarefa, `Err(AppError::Internal(...))` não é aceitável: gere-os na Task 7; aqui `render_functions` recebe só funções sem esses nós (o teste usa funções puras).
- [ ] **Step 1: Testes que falham.**
  - Snapshots: `tests/codegen/arithmetic.bd` (`fun f(x: Int) = x * x + 1`), `adt.bd` (árvore + `when`), `lambdas.bd` (`map`/`fold`), `optional.bd` (`?:`, `?.`, `when` com `null`), `generic.bd` (`fun <T> ident(x: T): T = x` e uso). Um teste em `src/backend/rust/mod.rs` gera `render_functions` para cada caso e compara com `<caso>.rs.expected` (regravável com `BIND_BLESS=1`).
  - Compilação (marcado `#[ignore]`, rodado explicitamente): monta um crate temporário em `target/bind-codegen-tests/` com `Cargo.toml` (dependências fixadas da Task 5), `src/bind_runtime.rs` (fonte do runtime), `src/lib.rs` com os snapshots, e roda `cargo check` + `cargo clippy -- -D warnings`. Deve passar sem warnings.
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar** as regras acima.
- [ ] **Step 4: Rodar.** `cargo test backend::`, `BIND_BLESS=1` para gerar os `.expected` (revise cada um), `cargo test backend:: -- --ignored` (compilação), `cargo test`, clippy, fmt.
- [ ] **Step 5: Commit.** `git commit -m "feat(backend): tradução da IR para Rust (tipos e expressões)"`

---

### Task 7: Banco, endpoints, transações e integração ao `certify`

**Files:**
- Create: `src/backend/rust/db.rs`, `src/backend/rust/endpoints.rs`
- Modify: `src/backend/rust/mod.rs`, `src/certified.rs`, `src/compiler.rs`, `src/pipeline.rs`

**Interfaces:**
- Consumes: Tasks 2, 5, 6; `DatabaseSchema`.
- Produces: `backend::rust::server_library(app: &VerifiedApplication) -> Result<String, String>` (o `src/lib.rs` do crate gerado, com `mod bind_runtime;`), usado por `certified::generate` no lugar da geração antiga do servidor.
- Regras:
  - **Funções incluídas:** endpoints + funções do programa alcançáveis a partir deles por `Global` na IR (fecho transitivo, ordem determinística por nome).
  - **Banco (`db.rs`):** para cada par (tabela, operação) usado, `async fn db_<op>_<tabela>(ctx: &Ctx, …) -> Result<…, AppError>` usando `ctx.transaction().await?`. SQL parametrizado como hoje (`SELECT … WHERE pk = $1 LIMIT 1`, `INSERT … RETURNING …`), com casts por tipo: `Int` → `::numeric`, `UUID` → `::uuid`, `Decimal`/`Money` → `::numeric`, `Boolean` → `::boolean`, `Date` → `::date`, `DateTime` → `::timestamp`, `Instant` → `::timestamptz`. Decodificação de linha para a `struct` do modelo com os tipos novos. `require` sem linha → `AppError::NotFound("<Modelo> not found")`.
  - **Endpoints (`endpoints.rs`):** `struct <Nome>Input` com `#[serde(deny_unknown_fields)]` e campos dos parâmetros; roteador `pub async fn handle_rpc(path: &str, body: &[u8], pool: &PgPool) -> HttpResult` que decodifica (400 em erro), abre `pool.begin()`, cria `Ctx::with_transaction`, chama `bind_<nome>(ctx.clone(), …).await`, solta o `ctx` e faz `commit` em `Ok` (erro de commit → 500) ou `rollback` em `Err`; resposta JSON como hoje (`HttpResult { status, body }`), mensagens públicas via `AppError::public_message`.
  - **Pilha:** `aws_main`/`vercel_main` passam a construir o runtime com `tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(64 * 1024 * 1024).build()` em vez de `#[tokio::main]` (sem `unwrap`: erro de construção vira erro de `main`).
  - **`certified.rs`:** `cargo_toml` ganha as dependências novas (features: `sqlx` + `chrono`, `bigdecimal`, `rust_decimal`; `rust_decimal` com `serde-str`; `chrono` com `serde`; `futures`); `dependency_lock`/`workspace_lock` listam as novas versões; `PROFILE_VERSION = "rust-elm-web-v2"`; `src/bind_runtime.rs` escrito com `include_str!("../runtime/bind-runtime/src/lib.rs")`; `contract_schema`/`contract_vectors` representam `Int`, `Decimal`, `Money` como string (vetores válidos e malformados, ex. `"1.5"` para `Int`). Remova as funções antigas de geração do servidor que ficarem sem uso (`rust_server_function`, `rust_row_decoder`, …); o que o cliente Elm ainda usa permanece.
  - **`pipeline.rs`:** `wireVersion` → `bind-json-v2`.
  - **`compiler.rs`:** remova o E8304 da checagem do perfil certificado; acrescente **E8309**: em target certificado, parâmetro ou retorno de endpoint que não seja tipo de fio (permitidos: escalares exceto `Duration`, `data class`/`entity`, `enum class`/`sealed class` sem campos nas variantes, `List<T>` e `T?` de tipos de fio) e coluna de banco/campo de entidade do tipo `Duration` → `"Endpoint `{f}` uses `{tipo}`, which cannot cross the wire in profile rust-elm-web-v2."` (ou a variante para coluna).
- [ ] **Step 1: Testes que falham.**
  - `compiler.rs`: (a) com target certificado, `server fun f(x: Int): (Int) -> Int effects [] = { y -> y }` → E8309 (**Review Focus 5**); `server fun g(d: Duration): Int = 1` → E8309; `sealed class S { data class A(val n: Int) }` + `server fun h(): S = A(1)` → E8309. (b) um endpoint com função auxiliar, `when` e duas operações de banco (insert seguido de require do id retornado) passa no perfil certificado (sem E8304, sem E9001).
  - `backend/rust`: snapshot `tests/codegen/endpoint.bd` (o programa do item b) → `endpoint.rs.expected` com `server_library`.
  - Ignorado/explícito: gerar o `commerce` atual com `generate` para um diretório temporário e rodar `cargo check` + `cargo clippy -- -D warnings` + `cargo test` dentro dele.
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** `cargo test`, `cargo test -- --ignored` (compilação), clippy, fmt; `DATABASE_URL=… cargo run --quiet -- certify --target aws-lambda` e `--target vercel` → sucesso.
- [ ] **Step 5: Commit.** `git commit -m "feat(backend): endpoints com transação e perfil rust-elm-web-v2"`

---

### Task 8: Contrato `Int` como string no cliente Elm

**Files:**
- Modify: `src/certified.rs` (geração Elm)

**Interfaces:**
- Produces: no `Main.elm` gerado, `intDecoder : D.Decoder Int` e `encodeInt : Int -> E.Value`, usados por todo campo/parâmetro `Int`.
- Regras:
  - `intDecoder` aceita só `-?[0-9]+` sem zeros à esquerda (exceto `"0"`) e com valor absoluto ≤ 9007199254740991 (2^53 − 1), comparando por quantidade de dígitos e, com 16 dígitos, lexicograficamente com `"9007199254740991"`; qualquer outra entrada → `D.fail` com mensagem. Não use `elm/regex` (o `elm.json` gerado não muda de dependências).
  - `encodeInt n = E.string (String.fromInt n)`; entradas de texto para parâmetros `Int` continuam aceitando o que o usuário digita e são codificadas como string.
  - `Long` continua número.
- [ ] **Step 1: Teste que falha:** snapshot do `Main.elm` gerado para um componente que chama um endpoint com parâmetro `Int` e retorno com campo `Int` (`tests/codegen/client_int.bd` → `client_int.elm.expected`), contendo `intDecoder`/`encodeInt` nos lugares certos. (**Review Focus 1** é coberto pela regra do decodificador; a Task 10 compila com `elm make`.)
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** `cargo test`, clippy, fmt; `certify --target vercel` (compila o Elm) → sucesso.
- [ ] **Step 5: Commit.** `git commit -m "feat(elm): Int trafega como string no contrato bind-json-v2"`

---

### Task 9: Teste diferencial (avaliador × Rust gerado)

**Files:**
- Create: `src/backend/rust/differential.rs` (`#[cfg(test)]`)
- Modify: `src/backend/rust/mod.rs`, `Makefile` (alvo `test-generated` que roda `cargo test -- --ignored`)

**Interfaces:**
- Consumes: `core::eval` (Task 4), `backend::rust::render_functions` (Task 6), runtime (Task 5).
- Regras:
  - Gerador (proptest com semente fixa ou PRNG determinístico) de **1.000** programas puros sem parâmetros: `fun p<i>(): <T> = <expr>` com `T` ∈ {`Int`, `Boolean`, `String`, `List<Int>`, `Int?`} e expressões sobre literais inteiros grandes (até 40 dígitos), aritmética (`+ - *`, `/` e `%` com `?:`), comparações, `if`, `when` sobre um ADT fixo e sobre `Int?`, lambdas com `map`/`fold`, `listOf`, concatenação de strings, e uma função auxiliar recursiva com contador decrescente (profundidade ≤ 50).
  - Cada programa é avaliado por `core::eval`; os 1.000 são renderizados num único crate temporário (`target/bind-differential/`) com um `main` que executa cada `bind_p<i>(Ctx::pure())` num runtime com pilha de 64 MiB e imprime `p<i>=<render>` usando a mesma forma canônica de `Value::render`.
  - Inclui um programa com recursão de profundidade 20.000: ambos os lados devem dar `RecursionLimit` (impresso como `p<i>=!RecursionLimit`) — **Review Focus 3**.
  - O teste (marcado `#[ignore]`) compila o crate (`cargo run --release`) e compara linha a linha; qualquer divergência falha com o programa, o valor esperado e o obtido.
- [ ] **Step 1:** escrever o teste (falha enquanto algo diverge ou não compila).
- [ ] **Step 2:** `cargo test differential -- --ignored` e corrigir **o código** (lowering, backend, runtime ou avaliador) até 0 divergências. Nunca reduza a quantidade de programas nem exclua construções do gerador para passar; se uma construção não puder ser suportada, reporte.
- [ ] **Step 3:** `cargo test`, clippy, fmt.
- [ ] **Step 4: Commit.** `git commit -m "test(backend): teste diferencial avaliador × Rust gerado"`

---

### Task 10: Exemplo ampliado, transações num banco real e certificação

**Files:**
- Modify: `examples/commerce/src/customer.bd`
- Create: `src/backend/rust/transactions.rs` (`#[cfg(test)]`, ignorado sem `BIND_TEST_DATABASE_URL`)

**Interfaces:**
- Consumes: tudo das Tasks 1–9.
- [ ] **Step 1: Ampliar o exemplo.** Acrescente a `examples/commerce/src/customer.bd`:
```kotlin
fun normalizar(nome: String) = when (nome) {
    "" -> "sem nome"
    outro -> outro
}

server fun cadastrar(nome: String, email: Email, saldo: Money): Customer
    effects [db.read, db.write] {
    val novo = db.customers.insert {
        name = normalizar(nome)
        email = email
        balance = saldo
    }
    return db.customers.require(novo.id)
}
```
- [ ] **Step 2: Teste de transação (Review Focus 2).** Com `BIND_TEST_DATABASE_URL` definido: o teste cria um schema temporário (`CREATE SCHEMA bind_test_<pid>`; `search_path`), cria a tabela `customers` a partir do schema CSV do exemplo, gera um crate a partir de um programa com o endpoint
```kotlin
server fun falha(nome: String, email: Email, saldo: Money, outro: CustomerId): Customer
    effects [db.read, db.write] {
    val _ = db.customers.insert { name = nome; email = email; balance = saldo }
    return db.customers.require(outro)
}
```
  chama `handle_rpc("/api/falha", …)` com um `outro` inexistente, espera status 404 e confere `SELECT count(*) FROM customers` = 0; depois chama `cadastrar` e confere status 200 e uma linha gravada; remove o schema no fim. Sem a variável, o teste imprime "skipped: BIND_TEST_DATABASE_URL not set" e passa — o relatório deve dizer qual dos dois aconteceu.
- [ ] **Step 3: Certificação.**
```bash
export DATABASE_URL=postgresql://bind:bind@localhost/bind
cargo run --quiet -- certify --target aws-lambda
cargo run --quiet -- certify --target vercel
make -C ../website check
(cd ../website && cargo run --quiet --manifest-path ../bind-lang/Cargo.toml -- certify --manifest bind.toml --target vercel --out /tmp/bind-website-v2)
```
  Todos devem terminar com sucesso. **Não** regenere `../website/deploy/` (outro repositório; o humano decide depois).
- [ ] **Step 4:** `cargo test`, `cargo test -- --ignored`, clippy, fmt.
- [ ] **Step 5: Commit.** `git add examples src && git commit -m "feat: exemplo commerce com endpoint completo e teste de transação"`

---

### Task 11: Documentação

**Files:**
- Create: `docs/certified-profile-v0.2.md`
- Modify: `docs/architecture.md`, `README.md`, `docs/certified-profile-v0.1.md` (nota no topo apontando para a v0.2)

- [ ] **Step 1:** `docs/certified-profile-v0.2.md` no formato do v0.1: identificador `rust-elm-web-v2`; tabela de camadas; superfície aceita (servidor com a linguagem inteira; cliente como no v0.1; tipos de fio; E8309); representação dos escalares (tabela dos Global Constraints); transação por chamada; limite de profundidade e `AppError`; contrato `bind-json-v2`; pipeline de certificação (inalterado, mais o runtime copiado); o que a certificação garante e não garante.
- [ ] **Step 2:** `docs/architecture.md`: o fluxo `types::check → core::lower → core::validate → backend` e o runtime; `README.md`: mencione a IR tipada, o servidor gerado com a linguagem inteira e o perfil v2.
- [ ] **Step 3:** verificação final: `cargo test`, `cargo test -- --ignored`, `cargo test --manifest-path runtime/bind-runtime/Cargo.toml`, clippy (dos dois crates), fmt, os dois `certify` e o `check` do site.
- [ ] **Step 4: Commit.** `git commit -m "docs: perfil certificado rust-elm-web-v2"`

Mapa dos critérios de aceite da spec:

| Critério | Coberto por |
|---|---|
| 1. Servidor lê só a `CoreProgram` | Task 7 |
| 2. Linguagem inteira nos endpoints, transação, sem E8304 | Tasks 6, 7, 10 |
| 3. `Int` de precisão arbitrária de ponta a ponta | Tasks 5, 6, 7, 8 |
| 4. Sem pânico; overflow e recursão como erros tipados | Tasks 5, 6, 9 |
| 5. Diferencial ≥ 1.000 programas, 0 divergências | Task 9 |
| 6. `certify` nos dois targets e no site | Task 10 |
| 7. Testes, clippy, fmt, documentação v0.2 | Task 11 |
