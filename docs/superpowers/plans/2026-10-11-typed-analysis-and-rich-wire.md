# Fim da análise por tokens e tipos ricos no fio — Plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Substituir toda a análise por tokens por análises tipadas sobre a IR e liberar no fio `sealed` com campos, genéricos instanciados, `Result`, `Set` e `Map` (com builtins mínimas), com semântica igual no avaliador, no Rust e no JS.

**Architecture:** Um módulo `src/core/analysis.rs` calcula grafo, operações de banco e os diagnósticos tipados a partir da `CoreProgram`; `src/compiler.rs` perde as varreduras de tokens e o AST perde `Function.body`. O verificador ganha a restrição fechada `Key` e as builtins de coleção; os dois runtimes e os dois backends ganham coleções e codecs dos tipos ricos, pinados por vetores compartilhados e pelo diferencial a três vias.

**Tech Stack:** Rust 1.98 (edition 2024), JavaScript ES2022 (módulos nativos), node (testes), PostgreSQL (ponta a ponta opcional).

**Spec:** `docs/superpowers/specs/2026-10-11-typed-analysis-and-rich-wire-design.md`

## Global Constraints

- Rust 1.98.0, edition 2024; o `bindc` não ganha dependências de produção novas; o JS continua sem npm, bundler ou etapa de build.
- Nenhum `Token` é lido fora do lexer e do parser ao fim do plano; `Function.body` sai do AST.
- Todo diagnóstico removido tem um teste que mostra o programa com o bug correspondente recusado por outro código, nomeado no teste.
- Restrição `Key`: `Int`, `String`, `Email`, `Url`, `Boolean`, `UUID`, `Decimal`, `Money`, `Date`, `DateTime`, `Instant`, `enum class` (sealed sem campos) e distinct sobre esses tipos. Nada mais.
- Ordem canônica igual nos três runtimes: numérica (`Int`, `Decimal` por valor, então `1.0` e `1.00` são a mesma chave), por ponto de código (strings), temporal tipada (datas), ordem de declaração (variantes de `enum`), `false < true`.
- Builtins: `setOf` (variádica), `emptySet`, `insert`, `remove`, `contains`, `setSize`, `toList`, `emptyMap`, `put`, `lookup` (devolve `V?`), `removeKey`, `keys`, `values`, `mapSize`. Imutáveis. Não contam profundidade.
- JSON: variante sem campos `"nomeLowerCamel"`; com campos `{"nomeLowerCamel": {campos}}`; `Result` `{"Ok": v}`/`{"Err": e}`; `Set` array em ordem canônica (decodificador aceita qualquer ordem, normaliza, recusa duplicatas); `Map` array de pares `[k, v]` em ordem canônica da chave (duplicatas recusadas). Tag desconhecida, campo a mais e par mal formado são recusados.
- E8309 passa a recusar no fio apenas funções, `Duration` e `Long`.
- Perfil `rust-web-v4`, contrato `bind-json-v4`, documentação `docs/certified-profile-v0.4.md`.
- Sem panics, `unwrap`, `expect`, `unreachable!` no Rust gerado e no runtime; sem `innerHTML`/`eval` no JS.
- Testes com `node` são `#[ignore]` e imprimem `skipped: node not found` sem ele; testes com banco usam `BIND_TEST_DATABASE_URL` e imprimem `skipped: BIND_TEST_DATABASE_URL not set` sem ela.
- Depois de cada tarefa: `cargo test`, `cargo clippy --all-targets -- -D warnings` e `cargo fmt --check` passam.
- Commits terminam com `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Branch `typed-analysis`. `DATABASE_URL=postgresql://bind:bind@localhost/bind` para `certify` (não conecta). Não modifique `../website`.

## Review Focus

1. Chaves `Decimal` de mesma grandeza e escalas diferentes (`"1.0"` e `"1.00"`) num `Map`/`Set` vindo do fio são duplicatas nos dois runtimes, e `lookup(m, 1.00)` acha a entrada gravada como `1.0` (Tasks 5 e 6).
2. Segredo do `environment` que chega ao cliente através de uma lambda passada como argumento a uma função compartilhada é recusado pela E4102 (Task 2).
3. `enum` cujas variantes, em ordem de declaração, não estão em ordem alfabética (`Z`, `A`, `M`): `toList(setOf(...))` e o JSON saem na ordem de declaração nos três runtimes (Tasks 4, 5 e 6).
4. Registro genérico instanciado com dois tipos diferentes na mesma assinatura de endpoint (`Pagina<Customer>` e `Pagina<Int>`) codifica e decodifica cada instância com o codec certo (Tasks 5 e 6).
5. JSON com `Map` de par mal formado (`[["a"]]`, `[["a", 1, 2]]`) ou `Set` com duplicata é recusado com 400 no servidor e "Request failed" no frontend (Tasks 5, 6 e 8).

---

### Task 1: Grafo e operações de banco a partir da IR

**Files:**
- Create: `src/core/analysis.rs`
- Modify: `src/core/mod.rs`, `src/compiler.rs`, `src/cli.rs` (se precisar ajustar o consumo do grafo)

**Interfaces:**
- Produces:
```rust
pub fn application_graph(program: &CoreProgram) -> ApplicationGraph;   // tipo ApplicationGraph existente em compiler.rs (mova-o para analysis.rs se for mais limpo)
pub fn database_operations(program: &CoreProgram) -> Vec<DatabaseOperation>;  // mesmo código que backend::rust::db::operations
```
- Regras: arestas função → função (`Global`), cliente → endpoint (`ServerCall`), função → tabela/operação (`Db`), `environment.X` → função (`Environment`); ordem determinística; o grafo e as operações substituem os que `analyze_function` monta a partir de tokens; `VerifiedApplication.database_operations` e `graph` passam a vir daqui.
- [ ] **Step 1: Testes que falham:** grafo do `examples/commerce` comparado com `tests/analysis/commerce.graph.expected` (gere-o a partir do grafo atual, revise por olho e documente diferenças intencionais — por exemplo, arestas que a varredura por tokens perdia); `impact` e `tree` do `commerce` via funções de `cli.rs` produzem a mesma saída que hoje (ou a corrigida, documentada); `database_operations` igual à extraída por tokens hoje para o `commerce` e para os programas de teste existentes de banco.
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar** e trocar a origem em `verify`; ainda não remova `analyze_function` (Task 3).
- [ ] **Step 4: Rodar.** `cargo test`, clippy, fmt.
- [ ] **Step 5: Commit.** `git commit -m "feat(core): grafo e operações de banco calculados a partir da IR"`

---

### Task 2: Diagnósticos tipados (E4102, E5303, E5204, E2301)

**Files:**
- Modify: `src/core/analysis.rs`, `src/compiler.rs`

**Interfaces:**
- Produces: `pub fn typed_diagnostics(program: &CoreProgram, app_ast: &Program, schema: &DatabaseSchema, environment: &…) -> Vec<Diagnostic>` (assinatura exata a critério do implementador, documentada), chamado por `verify` depois da lowering e validação, só quando não há erros anteriores.
- Regras:
  - **E4102:** para cada valor de `environment` marcado como segredo, recusar se alcançável a partir de código cliente (componentes e funções `client`, mais toda função alcançável deles por `Global`, inclusive lambdas passadas como argumento), com o caminho `environment.X -> f -> … -> Componente` na mensagem; spans da primeira função cliente do caminho.
  - **E5303:** `DbOp::Insert { columns }` que não cobre toda coluna com `required && !nullable && !has_default`.
  - **E5204:** campo do modelo de tabela usada cujo tipo da IR não corresponde ao tipo da coluna (mesma regra de mapeamento que `backend::rust::db` usa).
  - **E2301:** função sem corpo (`body_kind == Absent` / `expr.is_none()`) com retorno diferente de `Unit`.
- [ ] **Step 1: Testes que falham:** E4102 direta; **Review Focus 2** (lambda passada a função compartilhada); via função auxiliar; sem falso positivo para variável local com o nome do segredo e para a string `"TOKEN"`; E5303 com e sem a coluna; E5204 com tipo incompatível; E2301 com e sem retorno `Unit`. Todos via `compiler::verify`.
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar.** Os checks por tokens antigos ainda existem; garanta que cada novo código não duplica o antigo no mesmo teste (desligue a versão por tokens do mesmo código).
- [ ] **Step 4: Rodar.** `cargo test`, clippy, fmt.
- [ ] **Step 5: Commit.** `git commit -m "feat(core): E4102 transitiva, E5303, E5204 e E2301 tipadas"`

---

### Task 3: Remoção da análise por tokens e dos diagnósticos cobertos

**Files:**
- Modify: `src/compiler.rs`, `src/ast.rs`, `src/parser.rs`, `src/types/mod.rs`, `src/core/lower.rs` (onde `BodyKind`/`body` são lidos)

**Interfaces:**
- Consumes: Tasks 1 e 2.
- Regras:
  - Remover `analyze_function`, `insert_assignments`, `compatible`, os laços de W2101 e da E4102 por tokens, `validate_return_contract` por tokens.
  - Remover `Function.body` do AST (o parser mantém os tokens como variável local).
  - Remover os diagnósticos cobertos: E5200, E5201, E5202, E8204 (operação de tabela), W2101; E8303, E8302, E8308; `TypeDecl.variants` e E2001–E2003 quando cobertos pelo verificador (senão reescreva sobre o `TypeExpr` parseado).
  - Antes de remover cada código, escreva o teste de cobertura: um programa com o bug que o código pegava continua recusado, e o teste nomeia o código substituto (E7104, E7103, E7004, E7001, E7110…). Se algum bug deixar de ser pego, NÃO remova o código: reescreva-o tipado e reporte.
  - Ajuste os testes antigos que dependiam dos códigos removidos.
- [ ] **Step 1: Testes de cobertura** (um por código removido) — devem passar já antes da remoção.
- [ ] **Step 2: Remover** e rodar.
- [ ] **Step 3:** `grep` mostrando que nenhum `Token` é lido fora de `src/lexer.rs`, `src/parser.rs`, `src/parser/` e helpers `#[cfg(test)]`; inclua o resultado no relatório.
- [ ] **Step 4: Rodar.** `cargo test`, `cargo test -- --ignored` (diferenciais), clippy, fmt; `make -C ../website check` numa cópia temporária com `client = "web"` e `mode = "split"`.
- [ ] **Step 5: Commit.** `git commit -m "refactor: remove a análise por tokens e os diagnósticos já cobertos"`

---

### Task 4: Restrição `Key` e builtins de coleção no verificador, na lowering e no avaliador

**Files:**
- Modify: `src/types/env.rs`, `src/types/infer.rs`, `src/core/lower.rs`, `src/core/validate.rs`, `src/core/eval.rs`, `src/core/display.rs` (se a impressão canônica mudar), `docs/type-system.md`

**Interfaces:**
- Produces: restrição fechada `Key` (mesmo mecanismo de `Eq`/`Ord`, sem defaulting; erro com código novo ou existente de restrição não satisfeita — documente); as 14 builtins com esquemas em `BUILTINS`/`builtin_scheme` (`setOf` variádica como `listOf`); valores `Value::Set(Vec<Value>)`/`Value::Map(Vec<(Value, Value)>)` no avaliador, sempre em ordem canônica e sem duplicatas; impressão canônica `Set[a, b]` e `Map[k: v, …]` (documente a forma exata no doc comment de `render`).
- Regras: ordem canônica dos Global Constraints, com `Decimal` comparado por valor; `enum` por índice de declaração (a IR tem a ordem das variantes em `CoreType::Sum`).
- [ ] **Step 1: Testes que falham:** verificador aceita `Set<Int>`, `Map<CustomerId, Customer>`, `Set<Cor>` (`enum`), recusa `Set<Double>`, `Map<Customer, Int>`, `Set<Int?>`; lowering das builtins; avaliador: `toList(setOf(3, 1, 3))` = `[1, 3]`; `lookup(put(emptyMap(), 1.0, "a"), 1.00)` = `"a"` (**Review Focus 1**); **Review Focus 3**: `enum Cor { Z, A, M }` → `toList(setOf(M, Z, A))` = `[Z, A, M]`; `remove` de ausente devolve a mesma coleção; `keys`/`values` em ordem das chaves.
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** `cargo test`, clippy, fmt.
- [ ] **Step 5: Commit.** `git commit -m "feat(types): restrição Key e builtins de Set/Map"`

---

### Task 5: Runtime e backend Rust — coleções e tipos ricos no fio

**Files:**
- Modify: `runtime/bind-runtime/src/lib.rs`, `src/backend/rust/types.rs`, `src/backend/rust/expr.rs`, `src/compiler.rs` (E8309), `tests/codegen/*`
- Create: `runtime/vectors/collections.json`, `runtime/vectors/wire-rich.json`

**Interfaces:**
- Consumes: Task 4.
- Produces: `Set<T>` → `Arc<BTreeSet<T>>`, `Map<K, V>` → `Arc<BTreeMap<K, V>>`; `Ord`/`Eq` gerados só para tipos `Key` (para `enum`, ordem de declaração; para distinct, a do tipo base; `Decimal` por valor — use um newtype de chave no runtime se o `Ord` do `rust_decimal` não for por valor); funções do runtime para as builtins; serde de `Set` (array canônico, duplicatas → erro) e `Map` (array de pares, duplicatas e pares mal formados → erro); `sealed` com campos, genéricos instanciados e `Result` no fio; E8309 recusa só funções, `Duration` e `Long`.
- [ ] **Step 1: Testes que falham:** vetores `collections.json` (operações e resultados canônicos) e `wire-rich.json` (ida e volta e entradas malformadas de cada tipo novo) checados pelo runtime; snapshot `tests/codegen/collections.bd`; crate gerado com um endpoint `Map<CustomerId, Pagina<Customer>>` e `Pagina<Int>` (**Review Focus 4**) compila, passa clippy e um teste no crate faz ida e volta; **Review Focus 1** e **5** no runtime (`"1.0"`/`"1.00"` duplicatas; `[["a"]]`, `[["a",1,2]]`, Set com duplicata → erro); E8309 aceita os novos tipos e ainda recusa função/`Duration`/`Long`.
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** testes, clippy e fmt do runtime; `cargo test`, `cargo test backend:: -- --ignored`, clippy, fmt.
- [ ] **Step 5: Commit.** `git commit -m "feat(rust): Set/Map, sealed com campos, genéricos e Result no fio"`

---

### Task 6: Runtime e backend JS — coleções e tipos ricos no fio

**Files:**
- Modify: `runtime/bind-runtime-js/bind-runtime.js`, `runtime/bind-runtime-js/tests/run.mjs`, `src/backend/js/codecs.rs`, `src/backend/js/expr.rs`, `src/backend/js/plan.rs` (se alguma restrição do frontend mudar), `tests/codegen/web_*`

**Interfaces:**
- Consumes: Tasks 4 e 5 (vetores compartilhados).
- Produces: `Set` como array congelado em ordem canônica; `Map` como array congelado de pares `[k, v]` congelados; `rt.keyCompare(type, a, b)` ou comparadores por tipo `Key` (Decimal por valor; `enum` pelo índice de declaração, que o gerador passa ao runtime); as 14 builtins; decodificadores/encodificadores estritos de `Set`, `Map`, variantes com campos, genéricos instanciados (codecs combinados por instância) e `Result`.
- [ ] **Step 1: Testes que falham:** o runtime JS checa `runtime/vectors/collections.json` e `wire-rich.json`; snapshot `tests/codegen/web_collections.bd`; **Review Focus 1, 3, 4 e 5** no node (`"1.0"`/`"1.00"`; enum fora de ordem alfabética; `Pagina<Customer>` e `Pagina<Int>`; JSON malformado → `Remote("Request failed")`).
- [ ] **Step 2: Rodar e confirmar a falha.**
- [ ] **Step 3: Implementar.**
- [ ] **Step 4: Rodar.** `make test-js-runtime`, `cargo test`, `cargo test backend::js -- --ignored`, clippy, fmt.
- [ ] **Step 5: Commit.** `git commit -m "feat(js): Set/Map, sealed com campos, genéricos e Result no fio"`

---

### Task 7: Diferencial a três vias ampliado

**Files:**
- Modify: `src/backend/rust/differential.rs`, `src/backend/js/differential.rs`

**Interfaces:**
- Consumes: Tasks 4–6.
- Regras: o gerador ganha `Set`/`Map` de tipos `Key` (incluindo `Decimal` de escalas diferentes e um `enum` fora de ordem alfabética), todas as 14 builtins, sealed com campos (construção e `when`), genéricos instanciados e `Result`; contadores em `WEB_CONSTRUCTS` (ou equivalente) e asserção de presença mínima de cada construção; o programa de servidor do diferencial do servidor continua byte a byte igual (guarda de hash); 0 divergências nos dois diferenciais. Nunca reduza quantidade nem construções; corrija o código com teste de regressão por bug real.
- [ ] **Step 1:** ampliar o gerador e as asserções de presença.
- [ ] **Step 2:** `cargo test differential -- --ignored --nocapture` até 0 divergências.
- [ ] **Step 3:** `cargo test`, clippy, fmt.
- [ ] **Step 4: Commit.** `git commit -m "test: diferencial a três vias com coleções e tipos ricos"`

---

### Task 8: Ponta a ponta com Postgres — tipos ricos nos dois modos

**Files:**
- Modify: `src/backend/js/end_to_end.rs` (ou um teste novo ao lado, reutilizando seus helpers)

**Interfaces:**
- Consumes: Tasks 5–7.
- Regras: com `BIND_TEST_DATABASE_URL`, um projeto de teste com um endpoint que recebe `Map<CustomerId, Int>` e devolve um sealed com campos contendo `Set<String>` e `Result<Customer, Erro>`; um componente que o chama e renderiza os valores; SSR e split produzem o mesmo HTML; **Review Focus 5** de ponta a ponta: o servidor recebe `Map` com par mal formado → 400 com a mensagem pública; resposta adulterada pelo servidor de teste com `Set` duplicado → "Request failed" no split. Schema removido mesmo em falha; linha de pulo sem a variável.
- [ ] **Step 1:** escrever o teste.
- [ ] **Step 2:** rodar com a variável até passar.
- [ ] **Step 3:** `cargo test`, clippy, fmt.
- [ ] **Step 4: Commit.** `git commit -m "test: ponta a ponta com coleções e tipos ricos nos dois modos"`

---

### Task 9: Perfil v4 e documentação

**Files:**
- Create: `docs/certified-profile-v0.4.md`
- Modify: `src/certified.rs` (`PROFILE_VERSION = "rust-web-v4"`, `WIRE_VERSION = "bind-json-v4"`), `src/pipeline.rs` se necessário, `docs/architecture.md`, `docs/type-system.md`, `README.md`, `docs/certified-profile-v0.3.md` e `docs/certified-profile-v0.2.md` (nota de histórico)

- [ ] **Step 1:** subir as versões do perfil e do contrato; atualizar lock/manifesto e testes que conferem as versões.
- [ ] **Step 2:** `docs/certified-profile-v0.4.md` no formato do v0.3: tipos ricos no fio com o JSON de cada um; restrição `Key` e ordem canônica; builtins de coleção; diagnósticos removidos e seus substitutos (tabela); E4102 transitiva, E5303, E5204, E2301 tipadas; o que continua fora (`Long`, `Duration` no banco, `*`/`/` de `Decimal` no frontend).
- [ ] **Step 3:** `docs/type-system.md` com `Key` e as builtins; `docs/architecture.md` com `core::analysis`; `README.md` com o perfil v4.
- [ ] **Step 4:** verificação final: `cargo test`, `cargo test -- --ignored` (com `BIND_TEST_DATABASE_URL`), testes do runtime Rust, `make test-js-runtime`, clippy dos dois crates, fmt, os quatro `certify` do `commerce` e o `certify` do site numa cópia temporária com `client = "web"`, `mode = "split"`.
- [ ] **Step 5: Commit.** `git commit -m "docs: perfil certificado rust-web-v4"`

Mapa dos critérios de aceite da spec:

| Critério | Coberto por |
|---|---|
| 1. Nenhum `Token` fora do lexer/parser; `Function.body` removido | Task 3 |
| 2. Diagnósticos removidos com cobertura comprovada | Task 3 |
| 3. E4102 transitiva, E5303, E5204, grafo e operações da IR | Tasks 1, 2 |
| 4. Tipos ricos no fio e `Set`/`Map` com semântica igual nos três | Tasks 4–7 |
| 5. Nenhum programa verificado falha no `generate` | Tasks 5, 6 (planos), 7 |
| 6. `certify` nos quatro alvos e no site | Task 9 |
| 7. Testes, clippy, fmt e documentação v0.4 | Task 9 |
