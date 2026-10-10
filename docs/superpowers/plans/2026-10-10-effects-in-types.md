# Efeitos e domínios no sistema de tipos: plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Levar efeitos (`recurso.operação`) e domínios (`pure`/`server`/`client`) para o sistema de tipos do Bind, com inferência, polimorfismo de efeito e contratos `effects [...]` exatos, substituindo a análise de efeitos por tokens.

**Architecture:** Cada `Type::Fun` ganha uma linha de efeitos (`Row`: conjunto de rótulos + variável de linha opcional), unificada no mesmo union-find com níveis das variáveis de tipo. O verificador mantém uma linha de contexto por corpo; operações de banco e `server.f` produzem rótulos, chamadas absorvem a linha do chamado (aberta na instanciação), lambdas têm linha própria e a generalização fecha linhas que só aparecem no topo. Domínios são tetos nas variáveis de linha. `compiler::verify` passa a derivar capacidades dos efeitos tipados e perde as regras E6xxx.

**Tech Stack:** Rust 1.98.0 (edition 2024), `proptest =1.11.0` (dev, já presente).

**Spec:** `docs/superpowers/specs/2026-10-10-effects-in-types-design.md` (leia inteira; ela é a autoridade). Contexto do subprojeto 1: `docs/type-system.md`.

## Global Constraints

- Rust 1.98.0, edition 2024; nenhuma dependência nova.
- Codegen (`src/certified.rs`) sem mudança de comportamento: os artefatos gerados (incluindo `bind-manifest.json`, que contém as capacidades) devem ter hashes idênticos ao baseline da Task 1.
- Rótulos da v1: exatamente `db.read`, `db.write`, `server.call`.
- Tetos: `pure` = `{}`; `server` = `{db.read, db.write}`; `client` (fun e component) = `{server.call}`; `fun` sem modificador e os domínios `worker`/`database`/`external` = sem teto.
- Códigos novos: E7401 teto excedido; E7402 efeito não declarado; E7403 efeito declarado e não produzido; E7404 `server fun` chamada direto de código `client`; E7405 rótulo ou variável de efeito desconhecido. Mensagens em inglês, como as existentes.
- Exibição: linha vazia fechada não aparece; `! {db.read}`; `! 'e`; `! {db.read | 'e}`. Variáveis de linha se chamam `'e`, `'e1`, `'e2`…; variáveis de tipo pulam a letra `e` (`'a 'b 'c 'd 'f …`).
- Sintaxe em anotações: `(A) -> B ! [db.read, e]`; ausência de `!` = pura. Identificador minúsculo sem ponto dentro de `[...]` = variável de efeito.
- Diagnósticos determinísticos (ordenados por arquivo, linha, coluna, código).
- Nenhum `panic!`/estouro de pilha alcançável por entrada do usuário.
- `cargo test` verde e `cargo clippy --all-targets -- -D warnings` limpo ao fim de **cada** tarefa (não há mais exceção de `dead_code`: o módulo `types` já está ligado ao compilador). `cargo fmt` antes de cada commit.
- Toda mensagem de commit termina com `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Branch: `effects-in-types` no repositório `bind-lang`.

## Review Focus

1. **Função pura passada onde se aceitam efeitos.** `fun g(f: (Int) -> Int ! [db.read]) …` chamada com `dobro` (puro) deve ser aceita; uma lambda que grava passada a um parâmetro anotado puro deve ser rejeitada (E7101). Teste na Task 3.
2. **Lambda definida mas nunca chamada não produz efeito.** `client component` que cria `val f = { -> db.t.require(k) }` sem chamá-la não deve dar E7401; chamar `f()` deve. Teste na Task 4.
3. **`server.f` como valor.** `map(ids, server.buscar)` no cliente produz só `server.call`, nunca `db.read`. Teste na Task 4.
4. **Recursão mútua com efeitos.** `fun a(n) = if (n == 0) db.t.require(k) else b(n - 1)` e `fun b(n) = a(n)` → ambos `! {db.read}`. Teste na Task 3.
5. **Capacidades idênticas.** O `bind-manifest.json` do exemplo `commerce` e do site não muda (hash). Teste na Task 5.

## Estrutura de arquivos

| Arquivo | Ação | Responsabilidade |
|---|---|---|
| `src/types/effects.rs` | criar | `Label`, `Row`, `RowState`, rótulos conhecidos, interseção de tetos |
| `src/types/ty.rs` | modificar | `Type::Fun` com `Row`; variáveis de linha na `TypeTable`; `Scheme.row_vars`; generalização, instanciação, substituição e exibição com linhas |
| `src/types/unify.rs` | modificar | `unify_rows`, tetos, ajuste de nível de linhas, novos `UnifyError` |
| `src/types/env.rs` | modificar | `lower_with_effects` (tipos função com `! [...]`), E7405 |
| `src/types/infer.rs` | modificar | linha de contexto, origem dos efeitos, polimorfismo, domínios, exatidão, `TypedProgram.effects` |
| `src/types/mod.rs` | modificar | `pub mod effects;` |
| `src/syntax/expr.rs` | modificar | `TypeExpr::Function.effects`, renderização |
| `src/parser/expr.rs` | modificar | `! [...]` em `type_expr` |
| `src/parser.rs` | modificar | `Domain::Inferred` para `fun`; `effects_declared`; `! [...]` em `type_ref` |
| `src/ast.rs` | modificar | `Domain::Inferred`; `Function.effects_declared` |
| `src/compiler.rs` | modificar | remove E6001/E6101/E6102/E6201; capacidades a partir dos efeitos tipados |
| `src/types/props.rs`, `tests/typecheck/*` | modificar/criar | propriedades e golden de efeitos |
| `docs/type-system.md` | modificar | seção de efeitos |

---

### Task 1: Linhas de efeito na representação de tipos

**Files:**
- Create: `src/types/effects.rs`
- Modify: `src/types/mod.rs`, `src/types/ty.rs`, `src/types/unify.rs`, `src/types/env.rs`, `src/types/infer.rs`, `src/types/props.rs` (só as atualizações mecânicas de `Type::Fun`)

**Interfaces:**
- Produces (`types::effects`): `RowVarId = usize`; `Label { resource, operation }` com `Label::new`, `Label::rigid(name)`, `is_rigid`, `Label::parse(&str) -> Option<Label>`, `is_known`, `Display`; `Row { labels: BTreeSet<Label>, tail: Option<RowVarId> }` com `Row::empty`, `Row::closed`, `Row::with_tail`, `is_pure`; `RowState { Unbound { level, ceiling: Option<BTreeSet<Label>> }, Link(Row) }`; `intersect(&Option<BTreeSet<Label>>, &Option<BTreeSet<Label>>)`; constantes `DB_READ`, `DB_WRITE`, `SERVER_CALL` (pares `(&str, &str)`).
- Produces (`types::ty`): `Type::Fun(Vec<Type>, Box<Type>, Row)`; `TypeTable::{fresh_row_var(level, ceiling) -> RowVarId, fresh_row(level) -> Row, row_state, set_row, resolve_row(&Row) -> Row}`; `Scheme { vars, row_vars: Vec<RowVarId>, ty }`; `free_row_vars(&TypeTable, &Type) -> Vec<RowVarId>`; `substitute(ty, types: &BTreeMap<TypeVarId, Type>, rows: &BTreeMap<RowVarId, RowVarId>)`; `display`/`display_all` exibem linhas.
- Produces (`types::unify`): `unify_rows(&mut TypeTable, expected: &Row, found: &Row) -> Result<(), UnifyError>`; `UnifyError::RowMismatch { expected: Row, found: Row }` e `UnifyError::Ceiling { label: Label, ceiling: BTreeSet<Label> }`.
- Comportamento: nesta tarefa todas as funções continuam puras (`Row::empty()`), então nenhum esquema exibido muda. Todos os testes existentes continuam passando sem alteração de expectativa.

- [ ] **Step 1: Capturar o baseline dos artefatos (antes de qualquer mudança)**

```bash
cd /Users/leonardocaldas/Workspace/pessoal/bind/bind-lang
export DATABASE_URL=postgresql://bind:bind@localhost/bind
rm -rf /tmp/bind-fx-baseline && mkdir -p /tmp/bind-fx-baseline
cargo run --quiet -- generate --target aws-lambda --out /tmp/bind-fx-baseline/aws-lambda
cargo run --quiet -- generate --target vercel --out /tmp/bind-fx-baseline/vercel
(cd ../website && cargo run --quiet --manifest-path ../bind-lang/Cargo.toml -- generate --manifest bind.toml --target vercel --out /tmp/bind-fx-baseline/website)
(cd /tmp/bind-fx-baseline && find . -type f -print0 | sort -z | xargs -0 shasum -a 256) > /tmp/bind-fx-baseline.sha256
wc -l /tmp/bind-fx-baseline.sha256
```
Expected: os três `generate` terminam com código 0; o arquivo tem 37 linhas. Se algum falhar, pare e reporte BLOCKED.

- [ ] **Step 2: Escrever os testes que falham**

Crie `src/types/effects.rs` só com os testes:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_displays_labels() {
        let label = Label::parse("db.read").unwrap();
        assert_eq!(label, Label::new("db", "read"));
        assert_eq!(label.to_string(), "db.read");
        assert!(label.is_known());
        assert!(!Label::new("db", "delete").is_known());
        assert_eq!(Label::parse("db"), None);
        assert_eq!(Label::parse("a.b.c"), None);
        let rigid = Label::rigid("e");
        assert!(rigid.is_rigid());
        assert_eq!(rigid.to_string(), "e");
    }

    #[test]
    fn intersects_ceilings() {
        let read: BTreeSet<Label> = [Label::new("db", "read")].into();
        let both: BTreeSet<Label> = [Label::new("db", "read"), Label::new("db", "write")].into();
        assert_eq!(intersect(&Some(both.clone()), &Some(read.clone())), Some(read.clone()));
        assert_eq!(intersect(&None, &Some(both.clone())), Some(both));
        assert_eq!(intersect(&None, &None), None);
    }
}
```

Adicione ao `mod tests` de `src/types/unify.rs`:

```rust
    use crate::types::effects::{Label, Row};

    fn read() -> Label {
        Label::new("db", "read")
    }

    fn write() -> Label {
        Label::new("db", "write")
    }

    #[test]
    fn open_rows_unify_by_exchanging_missing_labels() {
        let mut table = TypeTable::default();
        let x = table.fresh_row_var(1, None);
        let y = table.fresh_row_var(1, None);
        let a = Row::with_tail([read()], x);
        let b = Row::with_tail([write()], y);
        unify_rows(&mut table, &a, &b).unwrap();
        let resolved_a = table.resolve_row(&a);
        let resolved_b = table.resolve_row(&b);
        assert_eq!(resolved_a.labels, [read(), write()].into());
        assert_eq!(resolved_a, resolved_b);
        assert!(resolved_a.tail.is_some());
    }

    #[test]
    fn closed_rows_unify_only_when_equal() {
        let mut table = TypeTable::default();
        assert!(unify_rows(&mut table, &Row::closed([read()]), &Row::closed([read()])).is_ok());
        assert!(matches!(
            unify_rows(&mut table, &Row::closed([read()]), &Row::closed([write()])),
            Err(UnifyError::RowMismatch { .. })
        ));
        let x = table.fresh_row_var(1, None);
        unify_rows(&mut table, &Row::with_tail([read()], x), &Row::closed([read(), write()])).unwrap();
        assert_eq!(table.resolve_row(&Row::with_tail([], x)), Row::closed([write()]));
        let y = table.fresh_row_var(1, None);
        assert!(unify_rows(&mut table, &Row::with_tail([read()], y), &Row::empty()).is_err());
    }

    #[test]
    fn same_tail_with_different_labels_is_a_mismatch() {
        let mut table = TypeTable::default();
        let x = table.fresh_row_var(1, None);
        assert!(unify_rows(&mut table, &Row::with_tail([read()], x), &Row::with_tail([], x)).is_err());
    }

    #[test]
    fn ceilings_reject_labels_and_propagate_by_intersection() {
        let mut table = TypeTable::default();
        let client = table.fresh_row_var(1, Some([Label::new("server", "call")].into()));
        let error = unify_rows(&mut table, &Row::with_tail([], client), &Row::closed([read()])).unwrap_err();
        assert_eq!(error, UnifyError::Ceiling { label: read(), ceiling: [Label::new("server", "call")].into() });
        let server = table.fresh_row_var(1, Some([read(), write()].into()));
        let free = table.fresh_row_var(1, None);
        unify_rows(&mut table, &Row::with_tail([], server), &Row::with_tail([], free)).unwrap();
        assert!(unify_rows(&mut table, &Row::with_tail([], free), &Row::closed([Label::new("server", "call")])).is_err());
        let rigid = table.fresh_row_var(1, Some(BTreeSet::new()));
        assert!(unify_rows(&mut table, &Row::with_tail([], rigid), &Row::closed([Label::rigid("e")])).is_ok());
    }

    #[test]
    fn function_types_unify_their_rows() {
        let mut table = TypeTable::default();
        let x = table.fresh_row_var(1, None);
        let a = Type::Fun(vec![int()], Box::new(int()), Row::with_tail([], x));
        let b = Type::Fun(vec![int()], Box::new(int()), Row::closed([read()]));
        unify(&mut table, &a, &b).unwrap();
        assert_eq!(display(&table.resolve(&a)), "(Int) -> Int ! {db.read}");
        let pure = Type::Fun(vec![int()], Box::new(int()), Row::empty());
        assert!(matches!(unify(&mut table, &pure, &b), Err(UnifyError::Mismatch { .. })));
    }

    #[test]
    fn row_variables_are_generalized_and_instantiated() {
        let mut table = TypeTable::default();
        let a = table.fresh(2);
        let e = table.fresh_row_var(2, None);
        let inner = Type::Fun(vec![a.clone()], Box::new(a.clone()), Row::with_tail([], e));
        let ty = Type::Fun(vec![inner, a], Box::new(Type::con("Int")), Row::with_tail([], e));
        let (scheme, _) = generalize(&table, 1, &ty);
        assert_eq!(scheme.row_vars.len(), 1);
        let first = instantiate(&mut table, &scheme, 1);
        let second = instantiate(&mut table, &scheme, 1);
        assert_ne!(first, second);
        assert_eq!(display(&first), "(('a) -> 'a ! 'e, 'a) -> Int ! 'e");
    }

    #[test]
    fn renders_rows_and_skips_e_for_type_variables() {
        let read_row = Row::closed([read()]);
        let pure = Type::Fun(vec![int()], Box::new(int()), Row::empty());
        assert_eq!(display(&pure), "(Int) -> Int");
        let reads = Type::Fun(vec![], Box::new(int()), read_row);
        assert_eq!(display(&reads), "() -> Int ! {db.read}");
        let mut table = TypeTable::default();
        let e = table.fresh_row_var(1, None);
        let both = Type::Fun(vec![], Box::new(int()), Row::with_tail([read(), write()], e));
        assert_eq!(display(&both), "() -> Int ! {db.read, db.write | 'e}");
        let vars: Vec<Type> = (0..5).map(Type::Var).collect();
        let five = Type::Fun(vars, Box::new(int()), Row::empty());
        assert_eq!(display(&five), "('a, 'b, 'c, 'd, 'f) -> Int");
        let curried = Type::Fun(vec![int()], Box::new(reads.clone()), Row::empty());
        assert_eq!(display(&curried), "(Int) -> (() -> Int ! {db.read})");
        let optional = Type::option(reads);
        assert_eq!(display(&optional), "(() -> Int ! {db.read})?");
    }
```

- [ ] **Step 3: Rodar e confirmar a falha**

Run: `cargo test types:: 2>&1 | tail -5`
Expected: FAIL de compilação (módulo `effects` e `unify_rows` não existem). Adicione `pub mod effects;` em `src/types/mod.rs` (ordem alfabética, antes de `env`).

- [ ] **Step 4: Implementar `src/types/effects.rs` (antes dos testes)**

```rust
//! Effect rows (Koka-style): a set of `resource.operation` labels plus an optional row
//! variable that stands for "whatever further effects". A function type carries the row of
//! effects that happen when it is called.

use std::collections::BTreeSet;
use std::fmt;

pub type RowVarId = usize;

pub const DB_READ: (&str, &str) = ("db", "read");
pub const DB_WRITE: (&str, &str) = ("db", "write");
pub const SERVER_CALL: (&str, &str) = ("server", "call");

/// Labels of the v1 vocabulary. Caches, queues and files will add theirs when the language
/// gains operations on them.
const KNOWN: &[(&str, &str)] = &[DB_READ, DB_WRITE, SERVER_CALL];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Label {
    pub resource: String,
    pub operation: String,
}

impl Label {
    pub fn new(resource: &str, operation: &str) -> Label {
        Label { resource: resource.to_string(), operation: operation.to_string() }
    }

    pub fn of(pair: (&str, &str)) -> Label {
        Label::new(pair.0, pair.1)
    }

    /// An effect variable inside the body of a function annotated with `effects [e]`: it only
    /// equals itself and is exempt from domain ceilings (the caller decides what it becomes).
    pub fn rigid(name: &str) -> Label {
        Label { resource: format!("'{name}"), operation: String::new() }
    }

    pub fn is_rigid(&self) -> bool {
        self.resource.starts_with('\'')
    }

    /// `resource.operation` with exactly one dot.
    pub fn parse(text: &str) -> Option<Label> {
        let (resource, operation) = text.split_once('.')?;
        if resource.is_empty() || operation.is_empty() || operation.contains('.') {
            return None;
        }
        Some(Label::new(resource, operation))
    }

    pub fn is_known(&self) -> bool {
        KNOWN.contains(&(self.resource.as_str(), self.operation.as_str()))
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_rigid() {
            write!(f, "{}", &self.resource[1..])
        } else {
            write!(f, "{}.{}", self.resource, self.operation)
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub labels: BTreeSet<Label>,
    pub tail: Option<RowVarId>,
}

impl Row {
    pub fn empty() -> Row {
        Row::default()
    }

    pub fn closed(labels: impl IntoIterator<Item = Label>) -> Row {
        Row { labels: labels.into_iter().collect(), tail: None }
    }

    pub fn with_tail(labels: impl IntoIterator<Item = Label>, tail: RowVarId) -> Row {
        Row { labels: labels.into_iter().collect(), tail: Some(tail) }
    }

    pub fn is_pure(&self) -> bool {
        self.labels.is_empty() && self.tail.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowState {
    /// `ceiling`: the most labels this variable may ever stand for (domains), `None` = any.
    Unbound { level: u32, ceiling: Option<BTreeSet<Label>> },
    Link(Row),
}

pub fn intersect(a: &Option<BTreeSet<Label>>, b: &Option<BTreeSet<Label>>) -> Option<BTreeSet<Label>> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.intersection(y).cloned().collect()),
        (Some(x), None) | (None, Some(x)) => Some(x.clone()),
        (None, None) => None,
    }
}
```

- [ ] **Step 5: Atualizar `src/types/ty.rs`**

1. Imports: `use super::effects::{Label, Row, RowState, RowVarId};` (mantenha `BTreeMap, BTreeSet`).
2. `Type::Fun(Vec<Type>, Box<Type>, Row)`. Em `contains_error`, o braço passa a ser `Type::Fun(params, result, _)`.
3. `TypeTable` ganha o campo `rows: Vec<RowState>` e os métodos:

```rust
    pub fn fresh_row_var(&mut self, level: u32, ceiling: Option<BTreeSet<Label>>) -> RowVarId {
        self.rows.push(RowState::Unbound { level, ceiling });
        self.rows.len() - 1
    }

    /// An open row with no labels yet.
    pub fn fresh_row(&mut self, level: u32) -> Row {
        let var = self.fresh_row_var(level, None);
        Row::with_tail([], var)
    }

    pub fn row_state(&self, id: RowVarId) -> &RowState {
        &self.rows[id]
    }

    pub fn set_row(&mut self, id: RowVarId, state: RowState) {
        self.rows[id] = state;
    }

    /// Follows the tail links, collecting labels; the result's tail is unbound or absent.
    pub fn resolve_row(&self, row: &Row) -> Row {
        let mut labels = row.labels.clone();
        let mut tail = row.tail;
        while let Some(id) = tail
            && let RowState::Link(next) = &self.rows[id]
        {
            labels.extend(next.labels.iter().cloned());
            tail = next.tail;
        }
        Row { labels, tail }
    }
```
   Em `resolve`, o braço de função passa a ser:
```rust
            Type::Fun(params, result, row) => Type::Fun(
                params.iter().map(|param| self.resolve(param)).collect(),
                Box::new(self.resolve(&result)),
                self.resolve_row(&row),
            ),
```
4. `Scheme` ganha `pub row_vars: Vec<RowVarId>`; `Scheme::mono` inicializa com `Vec::new()`.
5. Adicione:
```rust
/// Unbound row variables of `ty`, in order of first appearance.
pub fn free_row_vars(table: &TypeTable, ty: &Type) -> Vec<RowVarId> {
    let mut output = Vec::new();
    collect_row_vars(&table.resolve(ty), &mut output);
    output
}

fn collect_row_vars(ty: &Type, output: &mut Vec<RowVarId>) {
    match ty {
        Type::Con(_, args) => args.iter().for_each(|arg| collect_row_vars(arg, output)),
        Type::Fun(params, result, row) => {
            params.iter().for_each(|param| collect_row_vars(param, output));
            collect_row_vars(result, output);
            if let Some(tail) = row.tail
                && !output.contains(&tail)
            {
                output.push(tail);
            }
        }
        Type::Var(_) | Type::Error => {}
    }
}
```
   e o braço `Type::Fun(params, result, _)` em `collect_vars`.
6. `generalize` também quantifica variáveis de linha mais profundas que `level`:
```rust
    let row_vars = free_row_vars(table, &resolved)
        .into_iter()
        .filter(|id| matches!(table.row_state(*id), RowState::Unbound { level: var_level, .. } if *var_level > level))
        .collect();
    (Scheme { vars, row_vars, ty: resolved }, ambiguous)
```
7. `instantiate` mapeia as duas espécies (o teto da variável quantificada é copiado):
```rust
pub fn instantiate(table: &mut TypeTable, scheme: &Scheme, level: u32) -> Type {
    if scheme.vars.is_empty() && scheme.row_vars.is_empty() {
        return scheme.ty.clone();
    }
    let types: BTreeMap<TypeVarId, Type> = scheme.vars.iter().map(|id| (*id, table.fresh(level))).collect();
    let mut rows = BTreeMap::new();
    for id in &scheme.row_vars {
        let ceiling = match table.row_state(*id) {
            RowState::Unbound { ceiling, .. } => ceiling.clone(),
            RowState::Link(_) => None,
        };
        rows.insert(*id, table.fresh_row_var(level, ceiling));
    }
    substitute(&scheme.ty, &types, &rows)
}

pub fn substitute(ty: &Type, types: &BTreeMap<TypeVarId, Type>, rows: &BTreeMap<RowVarId, RowVarId>) -> Type {
    match ty {
        Type::Var(id) => types.get(id).cloned().unwrap_or(Type::Var(*id)),
        Type::Con(name, args) => Type::Con(name.clone(), args.iter().map(|arg| substitute(arg, types, rows)).collect()),
        Type::Fun(params, result, row) => Type::Fun(
            params.iter().map(|param| substitute(param, types, rows)).collect(),
            Box::new(substitute(result, types, rows)),
            Row {
                labels: row.labels.clone(),
                tail: row.tail.map(|tail| rows.get(&tail).copied().unwrap_or(tail)),
            },
        ),
        Type::Error => Type::Error,
    }
}
```
8. Exibição. Substitua `display_all`, `render` e `var_name` por:
```rust
/// Renders several types with shared variable names (for "expected X, found Y").
pub fn display_all(types: &[&Type]) -> Vec<String> {
    let mut names = Names::default();
    types.iter().map(|ty| render(ty, &mut names)).collect()
}

#[derive(Default)]
struct Names {
    types: BTreeMap<TypeVarId, String>,
    rows: BTreeMap<RowVarId, String>,
}

fn render(ty: &Type, names: &mut Names) -> String {
    match ty {
        Type::Var(id) => {
            let next = names.types.len();
            names.types.entry(*id).or_insert_with(|| type_var_name(next)).clone()
        }
        Type::Con(name, args) if name == "Option" && args.len() == 1 => {
            let inner = render(&args[0], names);
            if matches!(args[0], Type::Fun(..)) { format!("({inner})?") } else { format!("{inner}?") }
        }
        Type::Con(name, args) if args.is_empty() => name.clone(),
        Type::Con(name, args) => {
            let rendered: Vec<String> = args.iter().map(|arg| render(arg, names)).collect();
            format!("{name}<{}>", rendered.join(", "))
        }
        Type::Fun(params, result, row) => {
            let rendered: Vec<String> = params.iter().map(|param| render(param, names)).collect();
            let inner = render(result, names);
            // A function result is parenthesized when either arrow shows a row, so `!` always
            // belongs to the arrow it is written next to.
            let result = match result.as_ref() {
                Type::Fun(_, _, inner_row) if !row.is_pure() || !inner_row.is_pure() => format!("({inner})"),
                _ => inner,
            };
            format!("({}) -> {result}{}", rendered.join(", "), render_row(row, names))
        }
        Type::Error => "<error>".into(),
    }
}

/// ` ! {db.read}`, ` ! 'e`, ` ! {db.read | 'e}`, or nothing for a pure function.
fn render_row(row: &Row, names: &mut Names) -> String {
    if row.is_pure() {
        return String::new();
    }
    let labels: Vec<String> = row.labels.iter().map(Label::to_string).collect();
    let tail = row.tail.map(|id| {
        let next = names.rows.len();
        names.rows.entry(id).or_insert_with(|| row_var_name(next)).clone()
    });
    match (labels.is_empty(), tail) {
        (true, Some(tail)) => format!(" ! {tail}"),
        (false, Some(tail)) => format!(" ! {{{} | {tail}}}", labels.join(", ")),
        _ => format!(" ! {{{}}}", labels.join(", ")),
    }
}

/// `'a 'b 'c 'd 'f ...`: the letter `e` is reserved for effect (row) variables.
fn type_var_name(index: usize) -> String {
    const LETTERS: &[u8] = b"abcdfghijklmnopqrstuvwxyz";
    let letter = char::from(LETTERS[index % LETTERS.len()]);
    if index < LETTERS.len() { format!("'{letter}") } else { format!("'{letter}{}", index / LETTERS.len()) }
}

/// `'e 'e1 'e2 ...`
fn row_var_name(index: usize) -> String {
    if index == 0 { "'e".into() } else { format!("'e{index}") }
}
```

- [ ] **Step 6: Atualizar `src/types/unify.rs`**

1. Imports: `use std::collections::BTreeSet;` e `use super::effects::{Label, Row, RowState, RowVarId, intersect};`.
2. `UnifyError` ganha:
```rust
    RowMismatch { expected: Row, found: Row },
    Ceiling { label: Label, ceiling: BTreeSet<Label> },
```
   Em `widen`, `RowMismatch` também é alargado para `Mismatch` com os tipos externos (o braço `UnifyError::Mismatch { .. } | UnifyError::RowMismatch { .. } => …`).
3. O braço de funções em `unify`:
```rust
        (Type::Fun(p1, r1, e1), Type::Fun(p2, r2, e2)) if p1.len() == p2.len() => {
            for (x, y) in p1.iter().zip(p2) {
                unify(table, x, y).map_err(|error| widen(table, error, &a, &b))?;
            }
            unify(table, r1, r2).map_err(|error| widen(table, error, &a, &b))?;
            unify_rows(table, e1, e2).map_err(|error| widen(table, error, &a, &b))
        }
```
4. Em `occurs_and_adjust`, o braço de função também baixa o nível da cauda da linha:
```rust
        Type::Fun(params, result, row) => {
            params.iter().try_for_each(|param| occurs_and_adjust(table, var, level, param))?;
            occurs_and_adjust(table, var, level, &result)?;
            adjust_row_level(table, &row, level);
            Ok(())
        }
```
   com
```rust
fn adjust_row_level(table: &mut TypeTable, row: &Row, level: u32) {
    if let Some(tail) = table.resolve_row(row).tail
        && let RowState::Unbound { level: tail_level, ceiling } = table.row_state(tail).clone()
        && tail_level > level
    {
        table.set_row(tail, RowState::Unbound { level, ceiling });
    }
}
```
5. Em `constrain`, `Type::Fun(..)` continua sendo `Err(fail(table))`.
6. Adicione:
```rust
/// Unifies two effect rows (labels are distinct and unordered). Open rows exchange the labels
/// each lacks through a shared fresh tail; a closed row only equals the same label set.
pub fn unify_rows(table: &mut TypeTable, expected: &Row, found: &Row) -> Result<(), UnifyError> {
    let a = table.resolve_row(expected);
    let b = table.resolve_row(found);
    let only_a: BTreeSet<Label> = a.labels.difference(&b.labels).cloned().collect();
    let only_b: BTreeSet<Label> = b.labels.difference(&a.labels).cloned().collect();
    let mismatch = || UnifyError::RowMismatch { expected: a.clone(), found: b.clone() };
    match (a.tail, b.tail) {
        (None, None) => {
            if only_a.is_empty() && only_b.is_empty() { Ok(()) } else { Err(mismatch()) }
        }
        (Some(x), None) => {
            if !only_a.is_empty() {
                return Err(mismatch());
            }
            bind_row(table, x, Row::closed(only_b))
        }
        (None, Some(y)) => {
            if !only_b.is_empty() {
                return Err(mismatch());
            }
            bind_row(table, y, Row::closed(only_a))
        }
        (Some(x), Some(y)) if x == y => {
            if only_a.is_empty() && only_b.is_empty() { Ok(()) } else { Err(mismatch()) }
        }
        (Some(x), Some(y)) => {
            let (x_level, x_ceiling) = unbound_row(table, x);
            let (y_level, y_ceiling) = unbound_row(table, y);
            let fresh = table.fresh_row_var(x_level.min(y_level), intersect(&x_ceiling, &y_ceiling));
            bind_row(table, x, Row::with_tail(only_b, fresh))?;
            bind_row(table, y, Row::with_tail(only_a, fresh))
        }
    }
}

fn unbound_row(table: &TypeTable, id: RowVarId) -> (u32, Option<BTreeSet<Label>>) {
    match table.row_state(id) {
        RowState::Unbound { level, ceiling } => (*level, ceiling.clone()),
        RowState::Link(_) => unreachable!("`resolve_row` never returns a linked tail"),
    }
}

/// Links a row variable, enforcing its ceiling (rigid labels are exempt) and handing the
/// ceiling and level down to the new tail.
fn bind_row(table: &mut TypeTable, var: RowVarId, row: Row) -> Result<(), UnifyError> {
    let (level, ceiling) = unbound_row(table, var);
    if let Some(ceiling) = &ceiling
        && let Some(label) = row.labels.iter().find(|label| !label.is_rigid() && !ceiling.contains(label))
    {
        return Err(UnifyError::Ceiling { label: label.clone(), ceiling: ceiling.clone() });
    }
    if let Some(tail) = row.tail {
        let (tail_level, tail_ceiling) = unbound_row(table, tail);
        table.set_row(tail, RowState::Unbound { level: tail_level.min(level), ceiling: intersect(&tail_ceiling, &ceiling) });
    }
    table.set_row(var, RowState::Link(row));
    Ok(())
}
```

- [ ] **Step 7: Atualizações mecânicas (guiadas pelo compilador)**

Rode `cargo build 2>&1 | grep -E "^error" -A6` e corrija cada erro com estas regras:

| Padrão antigo | Novo |
|---|---|
| construção `Type::Fun(params, result)` em `env.rs`, `infer.rs` | `Type::Fun(params, result, Row::empty())` |
| padrão `Type::Fun(params, result)` | `Type::Fun(params, result, _)` (ou um nome, se a linha for usada) |
| padrão `Type::Fun(..)` | inalterado |
| `Scheme { vars, ty }` (`infer.rs::annotated_scheme`, `builtin`) | `Scheme { vars, row_vars: Vec::new(), ty }` |
| `substitute(ty, &mapping)` em `props.rs` | `substitute(ty, &mapping, &BTreeMap::new())` |
| `Type::Fun(..)` em geradores de `props.rs` (`arb_type`, `arb_ground`) | com `Row::empty()` |
| `match` sobre `UnifyError` em `infer.rs::unify_at` | acrescente `UnifyError::RowMismatch { .. }` ao braço de `Mismatch` (renderize os tipos externos que `widen` já passou) e um braço `UnifyError::Ceiling { label, .. }` que, por ora, emite E7401 com a mensagem "This code cannot perform `{label}`." (a Task 4 refina) |

Em `props.rs`, `to_source`, `is_instance` e `type_vars` ganham o braço de linha: `to_source` renderiza a linha com a mesma sintaxe da anotação (`! [db.read, e0]`, variáveis nomeadas `e0`, `e1`… numa segunda tabela de nomes; linha pura não escreve nada); `is_instance` compara linhas: a linha geral com cauda casa com qualquer linha específica que contenha seus rótulos (amarrando a variável de cauda de forma consistente), e linhas fechadas exigem rótulos iguais.

- [ ] **Step 8: Rodar tudo**

Run: `cargo test 2>&1 | tail -5`
Expected: PASS (todos os testes existentes, sem mudança de expectativa, mais os novos).

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -3` e `cargo fmt --check`
Expected: limpos.

- [ ] **Step 9: Commit**

```bash
git add src/types
git commit -m "feat(types): linhas de efeito no tipo das funções

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Sintaxe de efeitos, domínio inferido e tipos função com linha

**Files:**
- Modify: `src/ast.rs`, `src/parser.rs`, `src/parser/expr.rs`, `src/syntax/expr.rs`, `src/types/env.rs`, `src/compiler.rs` (uma linha)

**Interfaces:**
- Consumes: `Row`, `Label` (Task 1).
- Produces:
  - `Domain::Inferred` (`as_str() == "inferred"`), domínio de `fun` sem modificador; `pure fun` continua `Domain::Pure`.
  - `Function.effects_declared: bool` (`true` quando a cláusula `effects [...]` existe, mesmo vazia).
  - `TypeExpr::Function { params, result, effects: Vec<String> }` (`effects` vazio = pura); `TypeExpr::effect_variables(&self, &mut BTreeSet<String>)`; `render_type` escreve ` ! [db.read, e]`.
  - Parser de declarações: `TypeRef` de tipo função com linha vira texto `(A)->B![db.read,e]`.
  - `Env::lower_with_effects(te, vars, effects: &BTreeMap<String, Row>, diagnostics) -> Type`; `Env::lower` passa a delegar com mapa vazio; `Env::lower_row(names: &[String], effects: &BTreeMap<String, Row>, span: &Span, diagnostics) -> Row` (usado pela Task 4 para a cláusula `effects`).
- Erros: rótulo desconhecido ou variável fora de assinatura → E7405; duas variáveis distintas na mesma linha → E7004.

- [ ] **Step 1: Escrever os testes que falham**

Em `src/parser.rs` (`mod tests`):

```rust
    #[test]
    fn plain_functions_have_an_inferred_domain() {
        let program = parse_source(
            "fun a() = 1\npure fun b() = 1\nserver fun c(): Int effects [] = 1\nserver fun d(): Int = 1\n",
        );
        let domains: Vec<Domain> = program.functions.iter().map(|function| function.domain.clone()).collect();
        assert_eq!(domains, vec![Domain::Inferred, Domain::Pure, Domain::Server, Domain::Server]);
        let declared: Vec<bool> = program.functions.iter().map(|function| function.effects_declared).collect();
        assert_eq!(declared, vec![false, false, true, false]);
    }

    #[test]
    fn function_types_carry_effect_rows() {
        let program = parse_source("fun g(f: (Int) -> Int ! [db.read, e], h: (Int) -> Int): Int = 1\n");
        let parameters = &program.functions[0].parameters;
        assert_eq!(parameters[0].declared_type().display(), "(Int)->Int![db.read,e]");
        assert_eq!(parameters[1].declared_type().display(), "(Int)->Int");
    }
```

Em `src/parser/expr.rs` (`mod tests`), acrescente ao teste `parses_type_text` (ou num teste novo `parses_effect_rows_in_types`):

```rust
        assert_eq!(render("(Int) -> Int ! [db.read, e]"), "(Int) -> Int ! [db.read, e]");
        assert_eq!(render("((Int) -> Int ! [e])?"), "((Int) -> Int ! [e])?");
        assert_eq!(render("(A) -> (B) -> C ! [e]"), "(A) -> (B) -> C ! [e]");
        assert_eq!(render("(Int)->Int![db.read,e]"), "(Int) -> Int ! [db.read, e]");
        assert!(parse_type_text("(Int) -> Int ! [", &span).is_err());
```

Em `src/types/env.rs` (`mod tests`):

```rust
    use crate::types::effects::{Label, Row};

    fn lower_text(env: &Env, text: &str, effects: &BTreeMap<String, Row>) -> (Type, Vec<&'static str>) {
        let te = parse_type_text(text, &Span::default()).unwrap();
        let mut diagnostics = Vec::new();
        let ty = env.lower_with_effects(&te, &BTreeMap::new(), effects, &mut diagnostics);
        (ty, diagnostics.iter().map(|item| item.code).collect())
    }

    #[test]
    fn lowers_function_types_with_effect_rows() {
        let (env, _) = build("");
        let (pure, codes) = lower_text(&env, "(Int) -> Int", &BTreeMap::new());
        assert!(codes.is_empty());
        assert_eq!(pure, Type::Fun(vec![Type::con("Int")], Box::new(Type::con("Int")), Row::empty()));
        let (reads, _) = lower_text(&env, "(Int) -> Int ! [db.read]", &BTreeMap::new());
        let Type::Fun(_, _, row) = reads else { unreachable!() };
        assert_eq!(row, Row::closed([Label::new("db", "read")]));
        let effects: BTreeMap<String, Row> = [("e".to_string(), Row::with_tail([], 7))].into();
        let (open, _) = lower_text(&env, "(Int) -> Int ! [db.write, e]", &effects);
        let Type::Fun(_, _, row) = open else { unreachable!() };
        assert_eq!(row, Row::with_tail([Label::new("db", "write")], 7));
    }

    #[test]
    fn rejects_unknown_effects_and_two_variables() {
        let (env, _) = build("");
        assert_eq!(lower_text(&env, "(Int) -> Int ! [db.delete]", &BTreeMap::new()).1, vec!["E7405"]);
        assert_eq!(lower_text(&env, "(Int) -> Int ! [e]", &BTreeMap::new()).1, vec!["E7405"]);
        let effects: BTreeMap<String, Row> =
            [("e".to_string(), Row::with_tail([], 1)), ("f".to_string(), Row::with_tail([], 2))].into();
        assert_eq!(lower_text(&env, "(Int) -> Int ! [e, f]", &effects).1, vec!["E7004"]);
        assert!(build("data class C(val f: (Int) -> Int ! [db.read])").1.is_empty());
        assert_eq!(build("data class C(val f: (Int) -> Int ! [e])").1, vec!["E7405"]);
    }
```
(Importe `parse_type_text` e `Span` no módulo de testes se ainda não estiverem.)

- [ ] **Step 2: Rodar e confirmar a falha**

Run: `cargo test 2>&1 | tail -5`
Expected: FAIL de compilação (`Domain::Inferred`, `effects_declared`, `lower_with_effects` não existem).

- [ ] **Step 3: Implementar**

1. `src/ast.rs`: acrescente `Inferred` ao `enum Domain` com `Self::Inferred => "inferred"` em `as_str`; acrescente `pub effects_declared: bool,` a `Function` (depois de `effects`).
2. `src/parser.rs::function`: `let mut domain = Domain::Inferred;` (o braço `"pure"` continua atribuindo `Domain::Pure`). No laço de `effects`/`requires`, marque `effects_declared = true` quando `self.consume("effects")` (declare `let mut effects_declared = false;` antes) e preencha o campo no literal `Function`.
3. `src/parser.rs::type_ref`, no ramo de tipo função, depois de `name.push_str(&result.display());`:
```rust
                if self.peek_text() == "!" && self.peek_n(1) == "[" {
                    self.advance();
                    self.advance();
                    let mut effects = Vec::new();
                    while !self.at_eof() && !self.consume("]") {
                        effects.push(self.qualified_name());
                        self.consume(",");
                    }
                    name.push_str(&format!("![{}]", effects.join(",")));
                }
```
4. `src/syntax/expr.rs`:
   - `TypeExpr::Function { params: Vec<TypeExpr>, result: Box<TypeExpr>, effects: Vec<String> }`.
   - `render_type`: `TypeExpr::Function { params, result, effects }` renderiza `({params}) -> {result}` seguido de ` ! [{effects joined by ", "}]` quando `effects` não é vazio.
   - Adicione:
```rust
impl TypeExpr {
    /// Effect variables (lowercase names without a dot) mentioned in function types.
    pub fn effect_variables(&self, output: &mut BTreeSet<String>) {
        match self {
            TypeExpr::Named { args, .. } => args.iter().for_each(|arg| arg.effect_variables(output)),
            TypeExpr::Nullable(inner) => inner.effect_variables(output),
            TypeExpr::Function { params, result, effects } => {
                params.iter().for_each(|param| param.effect_variables(output));
                result.effect_variables(output);
                output.extend(effects.iter().filter(|name| !name.contains('.')).cloned());
            }
        }
    }
}
```
     (importe `std::collections::BTreeSet`).
5. `src/parser/expr.rs::type_expr_inner`, no ramo `if self.consume("->")`:
```rust
                let result = self.type_expr()?;
                let effects = self.effect_list()?;
                TypeExpr::Function { params, result: Box::new(result), effects }
```
   com o método
```rust
    /// `! [db.read, e]` after a function type, or nothing.
    fn effect_list(&mut self) -> Option<Vec<String>> {
        if !(self.is("!") && self.peek_at(1).text == "[") {
            return Some(Vec::new());
        }
        self.advance();
        self.advance();
        let mut names = Vec::new();
        while !self.is("]") && !self.at_end() {
            let mut name = self.identifier()?;
            if self.consume(".") {
                name.push('.');
                name.push_str(&self.identifier()?);
            }
            names.push(name);
            if !self.consume(",") {
                break;
            }
        }
        self.expect("]")?;
        Some(names)
    }
```
6. `src/types/env.rs`:
   - `lower` vira `self.lower_with_effects(te, vars, &BTreeMap::new(), diagnostics)`; mova o corpo atual para `pub fn lower_with_effects(&self, te: &TypeExpr, vars: &BTreeMap<String, Type>, effects: &BTreeMap<String, Row>, diagnostics: &mut Vec<Diagnostic>) -> Type`, passando `effects` nas chamadas recursivas. O ramo de função:
```rust
            TypeExpr::Function { params, result, effects: names } => {
                let params = params.iter().map(|param| self.lower_with_effects(param, vars, effects, diagnostics)).collect();
                let lowered = self.lower_with_effects(result, vars, effects, diagnostics);
                let row = self.lower_row(names, effects, &type_span(result), diagnostics);
                Type::Fun(params, Box::new(lowered), row)
            }
```
   - Adicione:
```rust
    /// The row written as `[db.read, e]`: known labels plus at most one effect variable.
    pub fn lower_row(&self, names: &[String], effects: &BTreeMap<String, Row>, span: &Span, diagnostics: &mut Vec<Diagnostic>) -> Row {
        let mut row = Row::empty();
        for name in names {
            if let Some(label) = Label::parse(name) {
                if label.is_known() {
                    row.labels.insert(label);
                } else {
                    diagnostics.push(Diagnostic::error(
                        "E7405",
                        format!("Unknown effect `{name}`; the known effects are db.read, db.write and server.call."),
                        span.clone(),
                    ));
                }
                continue;
            }
            let Some(variable) = effects.get(name) else {
                diagnostics.push(Diagnostic::error(
                    "E7405",
                    format!("Unknown effect variable `{name}`; effect variables are only allowed in function signatures."),
                    span.clone(),
                ));
                continue;
            };
            row.labels.extend(variable.labels.iter().cloned());
            match (row.tail, variable.tail) {
                (Some(existing), Some(tail)) if existing != tail => diagnostics.push(Diagnostic::error(
                    "E7004",
                    "An effect row can contain at most one effect variable.",
                    span.clone(),
                )),
                (_, Some(tail)) => row.tail = Some(tail),
                _ => {}
            }
        }
        row
    }
```
     e a função livre:
```rust
fn type_span(te: &TypeExpr) -> Span {
    match te {
        TypeExpr::Named { span, .. } => span.clone(),
        TypeExpr::Nullable(inner) => type_span(inner),
        TypeExpr::Function { result, .. } => type_span(result),
    }
}
```
   - `contains_non_equatable`: o padrão `TypeExpr::Function { .. }` não muda.
   - Imports: `use super::effects::{Label, Row};`.
7. `src/compiler.rs` (análise antiga, removida na Task 5): para não regredir até lá, a regra E6102 passa a aceitar também funções `Inferred`:
```rust
            if function.domain == Domain::Pure
                && !matches!(callee.domain, Domain::Pure | Domain::Inferred)
```

- [ ] **Step 4: Rodar tudo**

Run: `cargo test 2>&1 | tail -5`
Expected: PASS.

Run: `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`
Expected: limpos.

Run: `DATABASE_URL=postgresql://bind:bind@localhost/bind cargo run --quiet -- check --target aws-lambda && make -C ../website check`
Expected: código 0.

- [ ] **Step 5: Commit**

```bash
git add src
git commit -m "feat(parser): sintaxe de efeitos e domínio inferido para fun

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Inferência e propagação de efeitos, contratos exatos

**Files:**
- Modify: `src/types/infer.rs`, `tests/typecheck/infer_basics.expected`, `tests/typecheck/db.expected`

**Interfaces:**
- Consumes: Tasks 1 e 2 (`Row`, `Label`, `unify_rows`, `resolve_row`, `fresh_row`, `fresh_row_var`, `Env::lower_row`, `Domain::Inferred`, `Function.effects_declared`).
- Produces:
  - `TypedProgram.effects: BTreeMap<String, Vec<String>>` (rótulos resolvidos de cada função, ordenados, sem rótulos rígidos nem variáveis).
  - Funções livres em `infer.rs`: `effects_known(&Function) -> bool` (= `effects_declared || domain == Pure || (domain == Server && !component)`) e `is_fully_annotated` passa a exigir também `effects_known`.
  - Métodos do `Checker` usados pela Task 4: `perform(pair, text, span)`, `absorb(&Row, callee: Option<&str>, span)`, `check_declared_effects(&Function, &Row)`, campo `context: Row`, campo `effect_origins: BTreeMap<String, BTreeMap<Label, Origin>>` com `enum Origin { Operation { text: String, span: Span }, Call { callee: String, span: Span } }`.
- Nesta tarefa ainda **não** há tetos de domínio, E7404 nem variáveis de efeito em assinaturas (Task 4). Variáveis de efeito escritas em `effects [...]` geram E7405 por enquanto (mapa vazio no `lower_row`).

- [ ] **Step 1: Escrever os testes que falham**

Acrescente ao `mod tests` de `src/types/infer.rs` (o helper `with_db`, `ENTIDADE` e `CLIENTES` já existem):

```rust
    fn db_schemes(source: &str) -> Vec<(String, String)> {
        with_db(source).unwrap_or_else(|codes| panic!("{codes:?}"))
    }

    fn scheme_in(schemes: &[(String, String)], name: &str) -> String {
        schemes.iter().find(|(other, _)| other == name).unwrap().1.clone()
    }

    #[test]
    fn effects_propagate_through_calls_and_recursion() {
        let schemes = db_schemes(
            "fun ler(id: ClienteId) = db.clientes.require(id)\nfun usa(id: ClienteId) = ler(id).nome\nfun a(n: Int, id: ClienteId): Cliente = if (n == 0) db.clientes.require(id) else b(n - 1, id)\nfun b(n: Int, id: ClienteId): Cliente = a(n, id)\nfun puro(x: Int) = x + 1\n",
        );
        assert_eq!(scheme_in(&schemes, "ler"), "(ClienteId) -> Cliente ! {db.read}");
        assert_eq!(scheme_in(&schemes, "usa"), "(ClienteId) -> String ! {db.read}");
        assert_eq!(scheme_in(&schemes, "a"), "(Int, ClienteId) -> Cliente ! {db.read}");
        assert_eq!(scheme_in(&schemes, "b"), "(Int, ClienteId) -> Cliente ! {db.read}");
        assert_eq!(scheme_in(&schemes, "puro"), "(Int) -> Int");
    }

    #[test]
    fn effects_are_polymorphic() {
        assert_eq!(scheme("fun aplica(f, x) = f(x)", "aplica"), "(('a) -> 'b ! 'e, 'a) -> 'b ! 'e");
        let schemes = db_schemes(
            "fun nomes(ids: List<ClienteId>) = map(ids, { id -> db.clientes.require(id).nome })\nfun dobros(xs: List<Int>) = map(xs, { x -> x * 2 })\n",
        );
        assert_eq!(scheme_in(&schemes, "nomes"), "(List<ClienteId>) -> List<String> ! {db.read}");
        assert_eq!(scheme_in(&schemes, "dobros"), "(List<Int>) -> List<Int>");
    }

    #[test]
    fn pure_functions_fit_where_effects_are_allowed() {
        let schemes = db_schemes(
            "fun g(f: (Int) -> Int ! [db.read]): Int = f(1)\nfun dobro(x: Int): Int = x * 2\nfun usa() = g(dobro)\n",
        );
        assert_eq!(scheme_in(&schemes, "g"), "((Int) -> Int ! {db.read}) -> Int ! {db.read}");
        assert_eq!(scheme_in(&schemes, "usa"), "() -> Int ! {db.read}");
        let rejected = with_db(
            "fun h(f: (Int) -> String): String = f(1)\nfun usa(id: ClienteId) = h({ x -> db.clientes.require(id).nome })\n",
        );
        assert_eq!(rejected, Err(vec!["E7101".to_string()]));
    }

    #[test]
    fn lambdas_carry_their_own_effects() {
        let schemes = db_schemes("fun criar(id: ClienteId) = { -> db.clientes.require(id) }\n");
        assert_eq!(scheme_in(&schemes, "criar"), "(ClienteId) -> (() -> Cliente ! {db.read})");
    }

    #[test]
    fn server_calls_produce_only_server_call() {
        let schemes = db_schemes(
            "server fun buscar(id: ClienteId): Cliente effects [db.read] = db.clientes.require(id)\nclient fun tela(id: ClienteId) = server.buscar(id).nome\nclient fun todos(ids: List<ClienteId>) = map(ids, server.buscar)\n",
        );
        assert_eq!(scheme_in(&schemes, "buscar"), "(ClienteId) -> Cliente ! {db.read}");
        assert_eq!(scheme_in(&schemes, "tela"), "(ClienteId) -> String ! {server.call}");
        assert_eq!(scheme_in(&schemes, "todos"), "(List<ClienteId>) -> List<Cliente> ! {server.call}");
    }

    #[test]
    fn declared_effects_must_match_exactly() {
        assert_eq!(
            with_db("server fun f(id: ClienteId): Cliente = db.clientes.require(id)\n"),
            Err(vec!["E7402".to_string()])
        );
        assert_eq!(
            with_db("server fun f(id: ClienteId): Cliente effects [db.read, db.write] = db.clientes.require(id)\n"),
            Err(vec!["E7403".to_string()])
        );
        assert_eq!(
            with_db("fun ler(id: ClienteId) = db.clientes.require(id)\nserver fun f(id: ClienteId): Cliente effects [db.read] = ler(id)\n")
                .map(|schemes| scheme_in(&schemes, "f")),
            Ok("(ClienteId) -> Cliente ! {db.read}".to_string())
        );
        assert_eq!(
            with_db("fun ler(id: ClienteId) = db.clientes.require(id)\nserver fun f(id: ClienteId): Cliente = ler(id)\n"),
            Err(vec!["E7402".to_string()])
        );
    }

    #[test]
    fn typed_program_lists_the_effects_of_each_function() {
        let source = format!(
            "{ENTIDADE}fun ler(id: ClienteId) = db.clientes.require(id)\nserver fun buscar(id: ClienteId): Cliente effects [db.read] = ler(id)\nclient fun tela(id: ClienteId) = server.buscar(id).nome\nfun puro(x: Int) = x + 1\n"
        );
        let (tokens, _) = crate::lexer::lex("test.bd", &source);
        let (program, _) = crate::parser::parse(tokens);
        let (typed, diagnostics) = crate::types::check(&program, &schema(CLIENTES));
        assert!(!crate::diagnostic::has_errors(&diagnostics), "{diagnostics:?}");
        assert_eq!(typed.effects["ler"], vec!["db.read".to_string()]);
        assert_eq!(typed.effects["buscar"], vec!["db.read".to_string()]);
        assert_eq!(typed.effects["tela"], vec!["server.call".to_string()]);
        assert!(typed.effects["puro"].is_empty());
    }

    #[test]
    fn mutual_recursion_with_effects() {
        let schemes = db_schemes(
            "fun a(n: Int, id: ClienteId): Int = if (n == 0) length(listOf(db.clientes.require(id))) else b(n, id)\nfun b(n: Int, id: ClienteId): Int = a(n - 1, id)\n",
        );
        assert_eq!(scheme_in(&schemes, "a"), "(Int, ClienteId) -> Int ! {db.read}");
        assert_eq!(scheme_in(&schemes, "b"), "(Int, ClienteId) -> Int ! {db.read}");
    }
```

- [ ] **Step 2: Rodar e confirmar a falha**

Run: `cargo test types::infer 2>&1 | tail -10`
Expected: FAIL (os novos testes; `TypedProgram.effects` não existe).

- [ ] **Step 3: Implementar em `src/types/infer.rs`**

1. Imports: `use crate::ast::Domain;` (junto dos outros de `ast`) e `use super::effects::{DB_READ, DB_WRITE, Label, Row, RowState, SERVER_CALL};`; `use super::unify::unify_rows;`.
2. `TypedProgram` ganha:
```rust
    /// Resolved effect labels of each function (e.g. `["db.read"]`), sorted.
    pub effects: BTreeMap<String, Vec<String>>,
```
3. `Checker` ganha os campos `context: Row` (inicialize com `Row::empty()`) e `effect_origins: BTreeMap<String, BTreeMap<Label, Origin>>`, e adicione:
```rust
/// Where a function's effect first comes from, for diagnostics that show the path.
#[derive(Clone, Debug)]
enum Origin {
    Operation { text: String, span: Span },
    Call { callee: String, span: Span },
}
```
4. Funções livres:
```rust
/// Whether a function's effects are fixed by its declaration (and therefore checked exactly).
fn effects_known(function: &Function) -> bool {
    function.effects_declared
        || function.domain == Domain::Pure
        || (function.domain == Domain::Server && !function.component)
}

fn is_fully_annotated(function: &Function) -> bool {
    function.parameters.iter().all(|parameter| parameter.type_ref.is_some())
        && (function.component || function.return_type.is_some())
        && effects_known(function)
}
```
5. `signature` constrói a linha: se `effects_known(function)`, a linha declarada `self.env.lower_row(&function.effects, &BTreeMap::new(), &function.span, diagnostics)` (com o mesmo esquema `report`/`sink` de `lower_ref`; numa `server fun` sem cláusula ou numa `pure fun` a lista é vazia, então a linha é `Row::empty()`); senão, uma linha aberta nova `self.table.fresh_row(self.level)`. O retorno vira `Type::Fun(params, Box::new(result), row)`.
6. Novos métodos:
```rust
    /// An operation's effect joins the current context row.
    fn perform(&mut self, pair: (&str, &str), text: String, span: &Span) {
        let label = Label::of(pair);
        let tail = self.table.fresh_row_var(self.level, None);
        let produced = Row::with_tail([label.clone()], tail);
        let context = self.context.clone();
        match unify_rows(&mut self.table, &context, &produced) {
            Ok(()) => self.note_origin(label, Origin::Operation { text, span: span.clone() }),
            Err(error) => self.effect_error(error, span),
        }
    }

    /// Calling a function brings its row into the current context. A closed row is opened
    /// first, so a pure callee never closes the caller's row.
    fn absorb(&mut self, row: &Row, callee: Option<&str>, span: &Span) {
        let resolved = self.table.resolve_row(row);
        let opened = match resolved.tail {
            Some(_) => resolved.clone(),
            None => {
                let tail = self.table.fresh_row_var(self.level, None);
                Row::with_tail(resolved.labels.iter().cloned(), tail)
            }
        };
        let context = self.context.clone();
        match unify_rows(&mut self.table, &context, &opened) {
            Ok(()) => {
                if let Some(callee) = callee {
                    for label in &resolved.labels {
                        let origin = Origin::Call { callee: callee.to_string(), span: span.clone() };
                        self.note_origin(label.clone(), origin);
                    }
                }
            }
            Err(error) => self.effect_error(error, span),
        }
    }

    fn note_origin(&mut self, label: Label, origin: Origin) {
        self.effect_origins.entry(self.function.clone()).or_default().entry(label).or_insert(origin);
    }

    /// Placeholder reporting until domains arrive (Task 4 replaces it with E7401 and paths).
    fn effect_error(&mut self, error: UnifyError, span: &Span) {
        let message = match error {
            UnifyError::Ceiling { label, .. } => format!("This code cannot perform `{label}`."),
            other => format!("Effect mismatch: {other:?}."),
        };
        self.error("E7401", message, span);
    }

    /// Opens the top row of an instantiated function type, so a function whose effects are
    /// fixed can still be used where more effects are allowed.
    fn open_top(&mut self, ty: Type) -> Type {
        let Type::Fun(params, result, row) = ty else { return ty };
        let row = self.table.resolve_row(&row);
        if row.tail.is_some() {
            return Type::Fun(params, result, row);
        }
        let tail = self.table.fresh_row_var(self.level, None);
        Type::Fun(params, result, Row::with_tail(row.labels, tail))
    }

    /// Before generalization, a row variable that occurs exactly once, in a positive position
    /// (the function's own row, or the row of a function it returns), stands for nothing more:
    /// close it, so schemes read `! {db.read}` instead of `! {db.read | 'e}`. Variables that
    /// occur in parameters or inside type arguments are left open (closing them would make the
    /// function less general).
    fn close_positive_rows(&mut self, ty: &Type) {
        let resolved = self.table.resolve(ty);
        let mut occurrences: BTreeMap<RowVarId, (usize, bool)> = BTreeMap::new();
        row_occurrences(&resolved, true, &mut occurrences);
        for (tail, (count, positive)) in occurrences {
            if count == 1
                && positive
                && let RowState::Unbound { level, .. } = self.table.row_state(tail).clone()
                && level > self.level
            {
                self.table.set_row(tail, RowState::Link(Row::empty()));
            }
        }
    }

    /// `effects [...]` must equal what the body performs.
    fn check_declared_effects(&mut self, function: &Function, declared: &Row) {
        let produced = self.table.resolve_row(&self.context);
        let declared = self.table.resolve_row(declared);
        let listed: Vec<String> = declared.labels.iter().map(Label::to_string).collect();
        for label in produced.labels.difference(&declared.labels) {
            self.diagnostics.push(
                Diagnostic::error(
                    "E7402",
                    format!("`{}` performs `{label}`, but its effects declare only [{}].", function.name, listed.join(", ")),
                    function.span.clone(),
                )
                .with_hint(format!("Add `{label}` to `effects [...]`.")),
            );
        }
        for label in declared.labels.difference(&produced.labels) {
            self.diagnostics.push(
                Diagnostic::error(
                    "E7403",
                    format!("`{}` declares `{label}`, but never performs it.", function.name),
                    function.span.clone(),
                )
                .with_hint(format!("Remove `{label}` from `effects [...]`.")),
            );
        }
        if let Some(tail) = produced.tail
            && matches!(self.table.row_state(tail), RowState::Unbound { .. })
        {
            self.table.set_row(tail, RowState::Link(Row::empty()));
        }
    }
```
7. `check_body`: o padrão vira `let Type::Fun(params, result, row) = ty else { return };`. Antes de inferir o bloco:
```rust
        let declared = effects_known(function);
        self.context = if declared { self.table.fresh_row(self.level) } else { row.clone() };
```
   e, depois do `unify_at` do resultado, `if declared { self.check_declared_effects(function, row); }`. No fim, `self.context = Row::empty();`.
8. `infer_group`: depois de `self.level -= 1;` e antes do laço que generaliza, `for ty in &types { self.close_positive_rows(ty); }`.
9. `check_declared_generics`: um parâmetro de tipo declarado que termina como variável livre **com restrição de operador** foi especializado pela restrição. Nesse caso emita E7301 (não E7101), com a mensagem "Type parameter `{name}` of `{f}` is declared generic, but the body requires {constraint.describe()}.", e ligue a variável a `Type::Error` (para que `report_ambiguous` não repita o erro como E7302). Trate esse caso antes do tratamento atual de `Type::Var(id)`. Motivo: com esta tarefa, funções anotadas sem efeitos conhecidos passam pelo caminho inferido, e `fun <T> igual(a: T, b: T): Boolean = a == b` deve continuar dando E7301.
10. `infer_name`: os dois `return instantiate(...)` passam a `return self.open_top(instantiate(...))` (crie a variável antes para não emprestar `self` duas vezes).
11. `infer_field`, ramo `server`: depois de instanciar, troque a linha pelo efeito da chamada remota:
```rust
                    Some(scheme) => match instantiate(&mut self.table, &scheme, self.level) {
                        Type::Fun(params, result, _) => Type::Fun(params, result, Row::closed([Label::of(SERVER_CALL)])),
                        other => other,
                    },
```
12. `infer_call`:
    - no caminho `Type::Fun(params, result, row)` com aridade certa, depois de verificar os argumentos: `self.absorb(&row, callee_name(callee), span);` com
```rust
fn callee_name(callee: &Expr) -> Option<&str> {
    match &callee.kind {
        ExprKind::Name(name) => Some(name.as_str()),
        _ => None,
    }
}
```
    e, também como função livre:
```rust
/// Counts row-variable occurrences and whether all of them are in positive positions
/// (function results flip nothing; parameters flip polarity; type arguments count as negative).
fn row_occurrences(ty: &Type, positive: bool, output: &mut BTreeMap<RowVarId, (usize, bool)>) {
    match ty {
        Type::Fun(params, result, row) => {
            params.iter().for_each(|param| row_occurrences(param, !positive, output));
            row_occurrences(result, positive, output);
            if let Some(tail) = row.tail {
                let entry = output.entry(tail).or_insert((0, true));
                entry.0 += 1;
                entry.1 &= positive;
            }
        }
        Type::Con(_, args) => args.iter().for_each(|arg| row_occurrences(arg, false, output)),
        Type::Var(_) | Type::Error => {}
    }
}
```
    - no caminho `Type::Var(_)`: crie `let row = self.table.fresh_row(self.level);`, use `Type::Fun(arg_types, Box::new(result.clone()), row.clone())` na unificação e depois `self.absorb(&row, None, span);`.
    - o caminho de aridade errada não absorve.
13. `infer_lambda`:
```rust
        let fresh = self.table.fresh_row(self.level);
        let saved = std::mem::replace(&mut self.context, fresh);
        self.lambda_depth += 1;
        let result = self.infer_block(body);
        self.lambda_depth -= 1;
        let row = std::mem::replace(&mut self.context, saved);
        self.locals.truncate(mark);
        Type::Fun(param_types, Box::new(result), row)
```
14. Operações de banco: em `infer_db_call`, logo antes do `if method == "find"` final, `self.perform(DB_READ, format!("db.{table}.{method}"), span);`. Em `infer_builder`, antes de `model` no fim, `self.perform(DB_WRITE, format!("db.{table}.insert"), span);`.
15. Builtins polimórficos no efeito. Troque `builtin` por:
```rust
fn builtin(table: &mut TypeTable, arity: usize, rows: usize, build: impl Fn(&[Type], &[RowVarId]) -> Type) -> Scheme {
    let vars: Vec<Type> = (0..arity).map(|_| table.fresh(GENERIC_LEVEL)).collect();
    let row_vars: Vec<RowVarId> = (0..rows).map(|_| table.fresh_row_var(GENERIC_LEVEL, None)).collect();
    let ids = vars.iter().filter_map(|var| if let Type::Var(id) = var { Some(*id) } else { None }).collect();
    Scheme { vars: ids, row_vars: row_vars.clone(), ty: build(&vars, &row_vars) }
}
```
    `first`, `get`, `length`, `emptyList` usam `rows = 0` e `Row::empty()`. `map` e `fold` usam `rows = 1`, com a mesma variável na função recebida e na própria função:
```rust
            ("map", builtin(table, 2, 1, |v, r| {
                let mapper = Type::Fun(vec![v[0].clone()], Box::new(v[1].clone()), Row::with_tail([], r[0]));
                Type::Fun(vec![list(&v[0]), mapper], Box::new(list(&v[1])), Row::with_tail([], r[0]))
            })),
            ("fold", builtin(table, 2, 1, |v, r| {
                let step = Type::Fun(vec![v[1].clone(), v[0].clone()], Box::new(v[1].clone()), Row::with_tail([], r[0]));
                Type::Fun(vec![list(&v[0]), v[1].clone(), step], Box::new(v[1].clone()), Row::with_tail([], r[0]))
            })),
```
    (Importe `RowVarId` de `effects`.)
16. `finish`: preencha `typed.effects`:
```rust
                let labels = match &ty {
                    Type::Fun(_, _, row) => row.labels.iter().filter(|label| !label.is_rigid()).map(Label::to_string).collect(),
                    _ => Vec::new(),
                };
                typed.effects.insert(function.name.clone(), labels);
```

- [ ] **Step 4: Atualizar expectativas existentes (somente pelos motivos abaixo)**

Rode `cargo test 2>&1 | grep -E "panicked|FAILED" -A4` e ajuste apenas testes que falham por um destes motivos, citando cada um no relatório:
1. o esquema de uma função com efeitos passa a mostrá-los (ex.: `compose` → `(('a) -> 'b ! 'e, ('c) -> 'a ! 'e, 'c) -> 'b ! 'e`; `aplica` → `(('a) -> 'b ! 'e, 'a) -> 'b ! 'e`; `Pagina` no golden `db` → `(ClienteId) -> Ui ! {server.call}`; `usa` em `server_calls_and_environment_values` → `() -> Int ! {server.call}`; funções `server` de `database_operations` → `… ! {db.read}` / `… ! {db.write}`);
2. uma `server fun` de teste que acessa o banco sem cláusula precisa ganhar o `effects [...]` correspondente (a spec define que a ausência significa `[]`).
Atualize `tests/typecheck/infer_basics.expected` e `tests/typecheck/db.expected` à mão (não use `BIND_BLESS`), e confira que os demais `.expected` não mudaram. Qualquer outra falha é um bug desta tarefa: corrija o código, não o teste.

- [ ] **Step 5: Rodar tudo**

Run: `cargo test 2>&1 | tail -5`
Expected: PASS.

Run: `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`
Expected: limpos.

Run: `DATABASE_URL=postgresql://bind:bind@localhost/bind cargo run --quiet -- check --target aws-lambda && make -C ../website check`
Expected: código 0.

- [ ] **Step 6: Commit**

```bash
git add src tests
git commit -m "feat(types): inferência de efeitos, polimorfismo e contratos exatos

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Domínios, tetos, variáveis de efeito e caminhos

**Files:**
- Modify: `src/types/infer.rs`

**Interfaces:**
- Consumes: Task 3 (`perform`, `absorb`, `check_declared_effects`, `effect_error`, `Origin`, `effects_known`, `signature`, `lower_ref`).
- Produces: E7401 (teto, com caminho e dica), E7404, variáveis de efeito em assinaturas (genéricas no esquema, rígidas no corpo, flexíveis no caminho inferido), E7405 na cláusula `effects`, rótulo declarado fora do teto → E7401. `Diagnostic.path` preenchido em E7401 e E7402.

- [ ] **Step 1: Escrever os testes que falham**

```rust
    fn db_errors(source: &str) -> Vec<String> {
        with_db(source).err().unwrap_or_else(|| panic!("expected errors"))
    }

    #[test]
    fn domains_cap_the_effects() {
        assert_eq!(db_errors("pure fun p(id: ClienteId): String = db.clientes.require(id).nome\n"), vec!["E7401"]);
        assert_eq!(
            db_errors("client component P(id: ClienteId) {\n    val c = db.clientes.require(id)\n    UI { Text(c.nome) }\n}\n"),
            vec!["E7401"]
        );
        assert_eq!(
            db_errors("fun ler(id: ClienteId) = db.clientes.require(id)\nclient fun tela(id: ClienteId) = ler(id).nome\n"),
            vec!["E7401"]
        );
        assert_eq!(
            db_errors("server fun a(id: ClienteId): Cliente effects [db.read] = db.clientes.require(id)\nserver fun b(id: ClienteId): Cliente = server.a(id)\n"),
            vec!["E7401"]
        );
        assert_eq!(db_errors("pure fun p(): Int effects [db.read] = 1\n"), vec!["E7401"]);
    }

    #[test]
    fn client_code_reaches_endpoints_only_through_server_calls() {
        assert_eq!(
            db_errors("server fun eco(x: Int): Int = x\nclient fun usa(): Int = eco(1)\n"),
            vec!["E7404"]
        );
        let schemes = db_schemes("server fun eco(x: Int): Int = x\nclient fun usa(): Int = server.eco(1)\n");
        assert_eq!(scheme_in(&schemes, "usa"), "() -> Int ! {server.call}");
    }

    #[test]
    fn lambdas_only_count_when_called() {
        let defined = "client component P(id: ClienteId) {\n    val f = { -> db.clientes.require(id) }\n    UI { Text(\"x\") }\n}\n";
        assert!(with_db(defined).is_ok());
        let called = "client component P(id: ClienteId) {\n    val f = { -> db.clientes.require(id) }\n    val c = f()\n    UI { Text(c.nome) }\n}\n";
        assert_eq!(db_errors(called), vec!["E7401"]);
    }

    #[test]
    fn effect_variables_in_signatures() {
        let mapear = "fun <A, B> mapear(xs: List<A>, f: (A) -> B ! [e]): List<B> effects [e] = map(xs, f)\n";
        let schemes = db_schemes(&format!(
            "{mapear}fun nomes(ids: List<ClienteId>) = mapear(ids, {{ id -> db.clientes.require(id).nome }})\nfun dobros(xs: List<Int>) = mapear(xs, {{ x -> x * 2 }})\n"
        ));
        assert_eq!(scheme_in(&schemes, "mapear"), "(List<'a>, ('a) -> 'b ! 'e) -> List<'b> ! 'e");
        assert_eq!(scheme_in(&schemes, "nomes"), "(List<ClienteId>) -> List<String> ! {db.read}");
        assert_eq!(scheme_in(&schemes, "dobros"), "(List<Int>) -> List<Int>");
        assert_eq!(db_errors("fun <A> chama(f: () -> A ! [e]): A effects [] = f()\n"), vec!["E7402"]);
        assert_eq!(db_errors("fun ignora(f: () -> Int ! [e]): Int effects [e] = 1\n"), vec!["E7403"]);
        let flexible = db_schemes("fun chama(f: () -> Int ! [e]) = f()\n");
        assert_eq!(scheme_in(&flexible, "chama"), "(() -> Int ! 'e) -> Int ! 'e");
        assert_eq!(db_errors("server fun f(): Int effects [db.delete] = 1\n"), vec!["E7405"]);
    }

    #[test]
    fn effect_diagnostics_show_the_path() {
        let source = format!(
            "{ENTIDADE}fun ler(id: ClienteId) = db.clientes.require(id)\nserver fun f(id: ClienteId): Cliente = ler(id)\nclient fun tela(id: ClienteId) = ler(id).nome\n"
        );
        let (tokens, _) = crate::lexer::lex("test.bd", &source);
        let (program, _) = crate::parser::parse(tokens);
        let (_, diagnostics) = crate::types::check(&program, &schema(CLIENTES));
        for code in ["E7402", "E7401"] {
            let diagnostic = diagnostics.iter().find(|item| item.code == code).unwrap_or_else(|| panic!("{code} missing: {diagnostics:?}"));
            assert_eq!(diagnostic.path.len(), 3, "{code}: {:?}", diagnostic.path);
            assert!(diagnostic.path[1].starts_with("ler ("), "{:?}", diagnostic.path);
            assert!(diagnostic.path[2].starts_with("db.clientes.require ("), "{:?}", diagnostic.path);
        }
    }
```

- [ ] **Step 2: Rodar e confirmar a falha**

Run: `cargo test types::infer 2>&1 | tail -10`
Expected: FAIL nos novos testes.

- [ ] **Step 3: Implementar em `src/types/infer.rs`**

1. Função livre do teto:
```rust
/// The most effects a function of this domain may perform (`None` = no ceiling).
fn ceiling(domain: &Domain) -> Option<BTreeSet<Label>> {
    match domain {
        Domain::Pure => Some(BTreeSet::new()),
        Domain::Server => Some([Label::of(DB_READ), Label::of(DB_WRITE)].into()),
        Domain::Client => Some([Label::of(SERVER_CALL)].into()),
        Domain::Inferred | Domain::Worker | Domain::Database | Domain::External => None,
    }
}
```
2. `Checker` ganha `domain: Domain` (domínio do corpo atual; inicialize `Domain::Inferred`), `function_domains: BTreeMap<String, Domain>` (preenchido em `run` junto de `function_spans`) e `effect_scope: BTreeMap<String, Row>` (variáveis de efeito do corpo atual).
3. Variáveis de efeito de uma assinatura:
```rust
    /// Effect variables named in a function's signature: parameter and return types and the
    /// `effects [...]` clause.
    fn effect_variable_names(function: &Function) -> BTreeSet<String> {
        let mut names: BTreeSet<String> =
            function.effects.iter().filter(|name| !name.contains('.')).cloned().collect();
        let types = function.parameters.iter().filter_map(|parameter| parameter.type_ref.as_ref()).chain(function.return_type.as_ref());
        for type_ref in types {
            if let Ok(te) = parse_type_text(&type_ref.display(), &function.span) {
                te.effect_variables(&mut names);
            }
        }
        names
    }
```
   (função associada ou livre; não precisa de `self`).
   - `annotated_scheme`: para cada nome, `let var = self.table.fresh_row_var(GENERIC_LEVEL, None);` → mapa `name → Row::with_tail([], var)`; os `var` entram em `Scheme.row_vars`.
   - `check_annotated`: mapa `name → Row::closed([Label::rigid(name)])`; guarde-o em `self.effect_scope` durante o corpo.
   - `infer_group`: mapa `name → Row::with_tail([], self.table.fresh_row_var(self.level, None))`; guarde-o em `self.effect_scope` durante o corpo de cada função.
   - `signature` e `lower_ref` recebem o mapa (`effects: &BTreeMap<String, Row>`) e o repassam a `Env::lower_with_effects` e `Env::lower_row`. `install_environment` passa um mapa vazio.
   - Anotações de `val` dentro do corpo (`bind_val`) passam a usar `lower_with_effects(annotation, &self.rigid, &self.effect_scope, …)`.
4. `signature`, linha das funções com efeitos conhecidos: depois de calcular a linha declarada, se a função tem teto e algum rótulo declarado (não rígido) está fora dele, emita E7401 ("`{f}` is {what} and cannot declare `{label}`.", com `what` da tabela do item 6) quando `report` for verdadeiro, e remova o rótulo da linha (evita um E7403 em cascata). Linha das funções inferidas: `Row::with_tail([], self.table.fresh_row_var(self.level, ceiling(&function.domain)))`.
5. `check_body`: defina `self.domain = function.domain.clone()` no início; a linha de contexto das funções com efeitos conhecidos passa a ser `Row::with_tail([], self.table.fresh_row_var(self.level, ceiling(&function.domain)))`. No fim, restaure `self.domain = Domain::Inferred` e `self.effect_scope.clear()`.
6. Substitua `effect_error` por uma versão com caminho:
```rust
    fn effect_error(&mut self, error: UnifyError, via: Via<'_>, span: &Span) {
        let UnifyError::Ceiling { label, .. } = error else {
            self.error("E7101", format!("Effect mismatch in `{}`.", self.function), span);
            return;
        };
        let what = match self.domain {
            Domain::Pure => "a `pure` function",
            Domain::Client => "client code (it runs in the browser)",
            Domain::Server => "a server endpoint",
            _ => "this function",
        };
        let place = format!("{}:{}:{}", span.file, span.line, span.column);
        let mut path = vec![self.function.clone()];
        let hint = match via {
            Via::Operation(text) => {
                path.push(format!("{text} ({place})"));
                format!("Move `{text}` to a `server fun` and call it with `server.f(...)`.")
            }
            Via::Call(callee) => {
                path.push(format!("{callee} ({place})"));
                path.extend(self.effect_path(callee, &label));
                format!("`{callee}` performs `{label}`; call it from a `server fun` instead.")
            }
        };
        self.diagnostics.push(
            Diagnostic::error("E7401", format!("`{}` is {what} and cannot perform `{label}`.", self.function), span.clone())
                .with_path(path)
                .with_hint(hint),
        );
    }

    /// Follows recorded origins from `start` down to the operation that produces `label`.
    fn effect_path(&self, start: &str, label: &Label) -> Vec<String> {
        let mut path = Vec::new();
        let mut current = start.to_string();
        let mut seen = BTreeSet::new();
        while seen.insert(current.clone()) {
            match self.effect_origins.get(&current).and_then(|origins| origins.get(label)) {
                Some(Origin::Call { callee, span }) => {
                    path.push(format!("{callee} ({}:{}:{})", span.file, span.line, span.column));
                    current = callee.clone();
                }
                Some(Origin::Operation { text, span }) => {
                    path.push(format!("{text} ({}:{}:{})", span.file, span.line, span.column));
                    break;
                }
                None => break,
            }
        }
        path
    }
```
   com
```rust
#[derive(Clone, Copy)]
enum Via<'a> {
    Operation(&'a str),
    Call(&'a str),
}
```
   `perform` chama `self.effect_error(error, Via::Operation(&text), span)`; `absorb` chama `self.effect_error(error, callee.map_or(Via::Operation("a call"), Via::Call), span)`.
7. `check_declared_effects`: cada E7402 recebe `.with_path(path)`, onde `path = [nome da função]` seguido de `self.effect_path(&function.name, &label)`.
8. E7404, em `infer_call`, antes de inferir o chamado:
```rust
        let endpoint_from_client = matches!(&callee.kind, ExprKind::Name(name)
            if !self.is_local(name)
                && self.domain == Domain::Client
                && self.function_domains.get(name) == Some(&Domain::Server));
        if endpoint_from_client && let ExprKind::Name(name) = &callee.kind {
            self.error("E7404", format!("`{name}` is a server endpoint; call it through `server.{name}(...)`."), &callee.span);
        }
```
   e, no caminho `Type::Fun` com aridade certa, só chame `absorb` quando `!endpoint_from_client` (o erro já foi dado; não repita como E7401).

- [ ] **Step 4: Rodar tudo**

Run: `cargo test 2>&1 | tail -5`
Expected: PASS. Testes antigos não devem precisar de mudança nesta tarefa; se algum falhar, é bug desta tarefa (exceto se o teste tiver uma `pure fun`/`client fun` com acesso ao banco, que agora é E7401: nesse caso reporte o caso e a mudança).

Run: `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`
Expected: limpos.

Run: `DATABASE_URL=postgresql://bind:bind@localhost/bind cargo run --quiet -- check --target aws-lambda && make -C ../website check`
Expected: código 0.

- [ ] **Step 5: Commit**

```bash
git add src
git commit -m "feat(types): tetos de domínio, variáveis de efeito e caminho até o efeito

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Integração ao compilador

**Files:**
- Modify: `src/compiler.rs`

**Interfaces:**
- Consumes: `TypedProgram.effects` (Task 3).
- Produces: `compiler::verify` sem as regras E6001, E6101, E6102 e E6201; capacidades exigidas = capacidades de domínio (`ui.dom`, `server.http`, como hoje) ∪ rótulos dos efeitos tipados de cada função, exceto `server.call`.

- [ ] **Step 1: Escrever os testes que falham**

No `mod tests` de `src/compiler.rs`:

```rust
    fn codes(result: &CompileResult) -> Vec<&'static str> {
        result.diagnostics.iter().map(|item| item.code).collect()
    }

    #[test]
    fn effect_rules_come_from_the_type_checker() {
        let cases = [
            ("pure fun p(id: CustomerId): String = db.customers.require(id).name\n", "E7401"),
            ("server fun eco(x: Int): Int = x\nclient fun usa(): Int = eco(1)\n", "E7404"),
            ("server fun f(id: CustomerId): Customer = db.customers.require(id)\n", "E7402"),
        ];
        for (body, expected) in cases {
            let source = format!("distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\n{body}");
            let (root, config) = fixture(&source, SCHEMA);
            let result = verify(&config, "aws-lambda");
            let found = codes(&result);
            assert!(found.contains(&expected), "{body}: {found:?}");
            assert!(!found.iter().any(|code| code.starts_with("E6")), "{body}: {found:?}");
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn capabilities_come_from_typed_effects() {
        let (root, config) = fixture(SOURCE, SCHEMA);
        let app = verify(&config, "aws-lambda").application.expect("application");
        let expected: BTreeSet<String> = ["db.read", "server.http", "ui.dom"].into_iter().map(String::from).collect();
        assert_eq!(app.required_capabilities, expected);
        fs::remove_dir_all(root).unwrap();

        let helper = "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\nfun ler(id: CustomerId) = db.customers.require(id)\nserver fun getCustomer(id: CustomerId): Customer effects [db.read] = ler(id)\n";
        let (root, config) = fixture(helper, SCHEMA);
        let result = verify(&config, "aws-lambda");
        let app = result.application.unwrap_or_else(|| panic!("{:?}", result.diagnostics));
        assert!(app.required_capabilities.contains("db.read"));
        fs::remove_dir_all(root).unwrap();
    }
```

- [ ] **Step 2: Rodar e confirmar a falha**

Run: `cargo test compiler:: 2>&1 | tail -8`
Expected: FAIL (`capabilities_come_from_typed_effects` no caso com função auxiliar: a análise antiga não vê o efeito indireto; o caso `E7404`/`E7401` também pode vir acompanhado de E6xxx).

- [ ] **Step 3: Implementar**

1. Em `verify`, use o resultado do verificador: `let (typed, type_diagnostics) = crate::types::check(&program, &database);`.
2. No laço que já insere `ui.dom`/`server.http` por domínio, acrescente os efeitos tipados:
```rust
        for label in typed.effects.get(&function.name).into_iter().flatten() {
            if label != "server.call" {
                required_capabilities.insert(label.clone());
            }
        }
```
3. Em `analyze_function`, remova:
   - o bloco E6102 (função `pure` chamando outro domínio, incluindo o ajuste da Task 2);
   - o bloco E6201 (cliente chamando o servidor sem `server.`);
   - o bloco E6101 (cliente executando efeito de banco);
   - o laço E6001 (efeito não declarado) e a variável `inferred_effects`;
   - as inserções em `required_capabilities` a partir de efeitos detectados por tokens e de `function.effects`.
   O cálculo de `effect` usado em `DatabaseOperation` continua (o codegen depende dele). Se o parâmetro `required_capabilities` de `analyze_function` ficar sem uso, remova-o da assinatura e da chamada.
4. Confirme com `grep -n '"E60\|"E61\|"E62' src/compiler.rs` que não sobra nenhum desses códigos.

- [ ] **Step 4: Rodar tudo e a regressão**

Run: `cargo test 2>&1 | tail -5`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`
Expected: tudo verde e limpo.

```bash
export DATABASE_URL=postgresql://bind:bind@localhost/bind
cargo run --quiet -- check --target aws-lambda
cargo run --quiet -- check --target vercel
make -C ../website check
rm -rf /tmp/bind-fx-after && mkdir -p /tmp/bind-fx-after
cargo run --quiet -- generate --target aws-lambda --out /tmp/bind-fx-after/aws-lambda
cargo run --quiet -- generate --target vercel --out /tmp/bind-fx-after/vercel
(cd ../website && cargo run --quiet --manifest-path ../bind-lang/Cargo.toml -- generate --manifest bind.toml --target vercel --out /tmp/bind-fx-after/website)
(cd /tmp/bind-fx-after && find . -type f -print0 | sort -z | xargs -0 shasum -a 256) > /tmp/bind-fx-after.sha256
diff /tmp/bind-fx-baseline.sha256 /tmp/bind-fx-after.sha256 && echo "artefatos idênticos"
```
Expected: os `check` terminam com código 0 e a última linha é `artefatos idênticos`. Qualquer diferença (em especial no `bind-manifest.json`) é bug desta tarefa.

- [ ] **Step 5: Commit**

```bash
git add src/compiler.rs
git commit -m "feat(compiler): efeitos e capacidades vêm do verificador de tipos

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 6: Golden e propriedades de efeitos

**Files:**
- Create: `tests/typecheck/effects_ok.bd`, `effects_ok.csv`, `effects_ok.expected`, `err_effect_ceiling.bd`, `err_effect_undeclared.bd`, `err_effect_unused.bd`, `err_effect_endpoint.bd`, `err_effect_unknown.bd` (e seus `.expected`)
- Modify: `src/types/props.rs`

**Interfaces:**
- Consumes: tudo das Tasks 1 a 5; runner golden existente (primeira linha `// expect: …`, `<caso>.csv` vira o schema, `BIND_BLESS=1` regrava `.expected`).

- [ ] **Step 1: Casos golden**

`tests/typecheck/effects_ok.csv` (mesmo conteúdo de `db.csv`):
```
table,model,primary_key,column,type,nullable,required,has_default,engine
clientes,Cliente,id,id,ClienteId,false,true,true,postgresql
clientes,Cliente,id,nome,String,false,true,false,postgresql
clientes,Cliente,id,email,Email,false,true,false,postgresql
```

`tests/typecheck/effects_ok.bd`:
```kotlin
// expect: ok
distinct type ClienteId = UUID
entity Cliente(val id: ClienteId, val nome: String, val email: Email)
fun ler(id: ClienteId) = db.clientes.require(id)
fun aplica(f, x) = f(x)
fun <A, B> mapear(xs: List<A>, f: (A) -> B ! [e]): List<B> effects [e] = map(xs, f)
fun nomes(ids: List<ClienteId>) = mapear(ids, { id -> ler(id).nome })
server fun buscar(id: ClienteId): Cliente effects [db.read] = ler(id)
client fun tela(id: ClienteId) = server.buscar(id).nome
```

`tests/typecheck/effects_ok.expected` (à mão):
```
ler : (ClienteId) -> Cliente ! {db.read}
aplica : (('a) -> 'b ! 'e, 'a) -> 'b ! 'e
mapear : (List<'a>, ('a) -> 'b ! 'e) -> List<'b> ! 'e
nomes : (List<ClienteId>) -> List<String> ! {db.read}
buscar : (ClienteId) -> Cliente ! {db.read}
tela : (ClienteId) -> String ! {server.call}
```

Casos de erro (sem schema):

| Arquivo | Conteúdo |
|---|---|
| `err_effect_ceiling.bd` | `// expect: E7401`<br>`server fun a(x: Int): Int = x`<br>`server fun b(x: Int): Int = server.a(x)` |
| `err_effect_undeclared.bd` | `// expect: E7402`<br>`fun <A> chama(f: () -> A ! [e]): A effects [] = f()` |
| `err_effect_unused.bd` | `// expect: E7403`<br>`server fun f(x: Int): Int effects [db.read] = x` |
| `err_effect_endpoint.bd` | `// expect: E7404`<br>`server fun eco(x: Int): Int = x`<br>`client fun usa(): Int = eco(1)` |
| `err_effect_unknown.bd` | `// expect: E7405`<br>`server fun f(x: Int): Int effects [db.delete] = x` |

(`<br>` marca quebra de linha.) Gere os `.expected` dos erros com `BIND_BLESS=1 cargo test golden`, revise cada mensagem (linha e coluna apontam para a função ou chamada certa; o texto é compreensível) e confira com `git status` que nenhum `.expected` existente mudou.

- [ ] **Step 2: Propriedades**

Acrescente a `src/types/props.rs`:

```rust
// ---- 6. effect rows -------------------------------------------------------------------

use super::effects::{Label, Row};
use super::unify::unify_rows;

fn arb_row(vars: usize) -> impl Strategy<Value = Row> {
    let labels = proptest::collection::btree_set(
        prop::sample::select(vec![("db", "read"), ("db", "write"), ("server", "call")]).prop_map(Label::of),
        0..3,
    );
    (labels, proptest::option::of(0..vars)).prop_map(|(labels, tail)| Row { labels, tail })
}

proptest! {
    #![proptest_config(config())]
    #[test]
    fn row_unification_produces_a_unifier(a in arb_row(3), b in arb_row(3)) {
        let mut table = TypeTable::default();
        for _ in 0..3 {
            table.fresh_row_var(1, None);
        }
        if unify_rows(&mut table, &a, &b).is_ok() {
            prop_assert_eq!(table.resolve_row(&a), table.resolve_row(&b));
        }
    }
}

// ---- 7. inferred effects equal call-graph reachability --------------------------------

#[derive(Clone, Debug)]
enum Action {
    Read,
    Write,
    Call(usize),
    CallViaLambda(usize),
}

fn arb_effect_program() -> impl Strategy<Value = Vec<Vec<Action>>> {
    (1usize..5).prop_flat_map(|count| {
        let action = prop_oneof![
            Just(Action::Read),
            Just(Action::Write),
            (0..count).prop_map(Action::Call),
            (0..count).prop_map(Action::CallViaLambda),
        ];
        proptest::collection::vec(proptest::collection::vec(action, 0..4), count)
    })
}

const EFFECT_PRELUDE: &str =
    "distinct type ClienteId = UUID\nentity Cliente(val id: ClienteId, val nome: String)\nfun aplica(f, x) = f(x)\n";
const EFFECT_SCHEMA: &str = "table,model,primary_key,column,type,nullable,required,has_default,engine\nclientes,Cliente,id,id,ClienteId,false,true,true,postgresql\nclientes,Cliente,id,nome,String,false,true,false,postgresql\n";

fn effect_schema() -> &'static DatabaseSchema {
    static SCHEMA: std::sync::OnceLock<DatabaseSchema> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("bind-effect-props-{}.csv", std::process::id()));
        std::fs::write(&path, EFFECT_SCHEMA).unwrap();
        let schema = DatabaseSchema::load(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        schema
    })
}

fn render_effect_program(program: &[Vec<Action>], clauses: Option<&[BTreeSet<String>]>) -> String {
    let mut source = EFFECT_PRELUDE.to_string();
    for (index, actions) in program.iter().enumerate() {
        let clause = clauses
            .map(|all| format!(" effects [{}]", all[index].iter().cloned().collect::<Vec<_>>().join(", ")))
            .unwrap_or_default();
        source.push_str(&format!("fun f{index}(id: ClienteId): ClienteId{clause} {{\n"));
        for action in actions {
            let expr = match action {
                Action::Read => "db.clientes.require(id)".to_string(),
                Action::Write => "db.clientes.insert { nome = \"x\" }".to_string(),
                Action::Call(other) => format!("f{other}(id)"),
                Action::CallViaLambda(other) => format!("aplica({{ x -> f{other}(x) }}, id)"),
            };
            source.push_str(&format!("    val _ = {expr}\n"));
        }
        source.push_str("    id\n}\n");
    }
    source
}

/// The expected effects: each function's own operations plus those of every function it can
/// reach through calls (directly or through a lambda).
fn reachable_effects(program: &[Vec<Action>]) -> Vec<BTreeSet<String>> {
    (0..program.len())
        .map(|start| {
            let mut effects = BTreeSet::new();
            let mut stack = vec![start];
            let mut seen = BTreeSet::new();
            while let Some(current) = stack.pop() {
                if !seen.insert(current) {
                    continue;
                }
                for action in &program[current] {
                    match action {
                        Action::Read => {
                            effects.insert("db.read".to_string());
                        }
                        Action::Write => {
                            effects.insert("db.write".to_string());
                        }
                        Action::Call(other) | Action::CallViaLambda(other) => stack.push(*other),
                    }
                }
            }
            effects
        })
        .collect()
}

fn inferred_effects(source: &str) -> Result<BTreeMap<String, Vec<String>>, String> {
    let (tokens, mut diagnostics) = lex("effects.bd", source);
    let (program, mut parse_diagnostics) = parse(tokens);
    diagnostics.append(&mut parse_diagnostics);
    if has_errors(&diagnostics) {
        return Err(format!("{diagnostics:?}"));
    }
    let (typed, type_diagnostics) = super::check(&program, effect_schema());
    if has_errors(&type_diagnostics) {
        return Err(format!("{type_diagnostics:?}"));
    }
    Ok(typed.effects)
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn inferred_effects_equal_call_graph_reachability(program in arb_effect_program()) {
        let source = render_effect_program(&program, None);
        let effects = inferred_effects(&source).map_err(|error| TestCaseError::fail(format!("{error}\n{source}")))?;
        for (index, expected) in reachable_effects(&program).into_iter().enumerate() {
            let found: BTreeSet<String> = effects[&format!("f{index}")].iter().cloned().collect();
            prop_assert_eq!(&found, &expected, "f{} in\n{}", index, source);
        }
    }

    #[test]
    fn declaring_the_inferred_effects_is_stable(program in arb_effect_program()) {
        let expected = reachable_effects(&program);
        let source = render_effect_program(&program, Some(&expected));
        let effects = inferred_effects(&source).map_err(|error| TestCaseError::fail(format!("{error}\n{source}")))?;
        for (index, expected) in expected.iter().enumerate() {
            let found: BTreeSet<String> = effects[&format!("f{index}")].iter().cloned().collect();
            prop_assert_eq!(&found, expected, "f{} in\n{}", index, source);
        }
    }
}
```
(Agrupe os `use` no topo do arquivo, sem duplicar os existentes; `BTreeSet` e `TestCaseError` vêm de `std::collections` e `proptest::prelude`.)

- [ ] **Step 3: Rodar**

Run: `cargo test golden 2>&1 | tail -3` e `cargo test types::props 2>&1 | tail -10`
Expected: PASS. Uma propriedade que falha é um bug nas Tasks 1 a 4: reporte o contraexemplo minimizado e o diagnóstico; não enfraqueça a propriedade.

Run: `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`
Expected: verde e limpo.

- [ ] **Step 4: Commit**

```bash
git add tests/typecheck src/types/props.rs proptest-regressions
git commit -m "test(types): golden e propriedades de efeitos

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 7: Documentação e verificação final

**Files:**
- Modify: `docs/type-system.md`, `README.md`

**Interfaces:**
- Consumes: o comportamento implementado; o documento descreve o que o código faz (se o texto abaixo divergir do código, o código é a referência e a divergência deve ser reportada).

- [ ] **Step 1: Seção "Effects" em `docs/type-system.md`**

Acrescente uma seção `## Effects and domains` (depois de "Functions") cobrindo, de forma concisa e com exemplos:
1. **Labels** `resource.operation`; v1: `db.read`, `db.write`, `server.call`; o recurso é um nome declarado (hoje `db`), a tecnologia fica na declaração; caches, filas e arquivos ganharão operações próprias (`read`/`write`, `publish`/`consume`).
2. **Rows**: `Fun(params, result, row)`; exibição `! {db.read}`, `! 'e`, `! {db.read | 'e}`; pura não exibe nada; variáveis de linha `'e`, `'e1`…; variáveis de tipo pulam `e`.
3. **Annotation syntax**: `(A) -> B ! [db.read, e]`; ausência de `!` = pura; `effects [...]`; `server fun` sem cláusula = `[]`; variáveis de efeito (genéricas no esquema, rígidas no corpo de funções anotadas, flexíveis nas inferidas).
4. **Rules** (pode usar regras de inferência como as existentes): origem dos efeitos (tabela da spec), absorção nas chamadas, abertura na instanciação, fechamento de linhas positivas na generalização, linha própria das lambdas, `server.f` = `server.call`.
5. **Domains as ceilings** (tabela), E7401 com caminho, E7404.
6. **Exact contracts**: E7402, E7403; quando uma função tem a assinatura conhecida de antemão (inclui efeitos conhecidos); consequência: polimorfismo recursivo exige `effects [...]` (ou `pure`).
7. **Capabilities** derivadas dos efeitos tipados.
8. Atualize "Known limits": efeitos de cache/fila/arquivos ainda sem operações; um único banco.
Atualize também a tabela de diagnósticos do documento (se houver) com E7401–E7405.

- [ ] **Step 2: README**

Em `## What v0.1 proves`, acrescente: `- effects (`db.read`, `db.write`, `server.call`) and deployment domains in the type system, with exact `effects [...]` contracts (see [type system](docs/type-system.md));`.

- [ ] **Step 3: Verificação final**

```bash
cargo test 2>&1 | tail -3
cargo clippy --all-targets -- -D warnings 2>&1 | tail -2
cargo fmt --check
export DATABASE_URL=postgresql://bind:bind@localhost/bind
cargo run --quiet -- check --target aws-lambda
cargo run --quiet -- check --target vercel
make -C ../website check
```
Expected: tudo verde; os três `check` com código 0.

Mapa dos critérios de aceite da spec:

| Critério | Coberto por |
|---|---|
| 1. Efeitos propagados (diretos, indiretos, recursivos, lambdas) | Task 3 (testes), Task 6 (propriedade de alcançabilidade) |
| 2. Tetos e E7401 com caminho | Task 4 |
| 3. `effects [...]` exato e variáveis rígidas | Tasks 3 e 4 |
| 4. E6xxx removidos, cobertos por E74xx | Task 5 |
| 5. Capacidades dos efeitos tipados, hashes idênticos | Task 5 |
| 6. clippy e fmt limpos | este passo |
| 7. `docs/type-system.md` | Step 1 |

- [ ] **Step 4: Commit**

```bash
git add docs/type-system.md README.md
git commit -m "docs: efeitos e domínios no sistema de tipos

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```
