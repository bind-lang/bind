# Núcleo da linguagem (HM, ADTs, exaustividade): plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Dar ao Bind um sistema de tipos correto no modelo OCaml (inferência Hindley-Milner, ADTs com dados, `when` exaustivo, sem exceções, sem mutação), rodando como passo de verificação antes da análise atual.

**Architecture:** Um parser de expressões (Pratt) transforma o corpo de cada função em uma AST (`Function.expr`). O módulo `types` monta o ambiente de tipos, infere com unificação union-find e generalização por níveis, verifica exaustividade com o algoritmo de Maranget e devolve `TypedProgram` + diagnósticos E7xxx. `compiler::verify` aborta antes da análise antiga se houver erro. O codegen não muda.

**Tech Stack:** Rust 1.98.0 (edition 2024), sem dependências novas de produção; `proptest =1.11.0` como dev-dependency.

**Spec:** `docs/superpowers/specs/2026-10-09-type-system-core-design.md` (inclui os adendos A a D; leia inteira antes de começar).

## Global Constraints

- Rust 1.98.0, edition 2024; let-chains (`if let … && …`) são permitidos e já usados no código.
- Dependências fixadas com `=`. Única nova: `proptest = "=1.11.0"` em `[dev-dependencies]`.
- O codegen (`src/certified.rs`) não pode mudar de comportamento: artefatos gerados devem ter hashes idênticos ao baseline da Task 1.
- Códigos de diagnóstico: E70xx sintaxe/construções removidas, E71xx tipos, E72xx exaustividade, E73xx restrições. Mensagens em inglês, no estilo das existentes (`Diagnostic::error(code, message, span).with_hint(...)`).
- Tabela de códigos usada neste plano: E7001 construção removida; E7002 campo `var`; E7003 anotação ausente em fronteira; E7004 sintaxe não suportada; E7005 token inesperado; E7101 tipos incompatíveis; E7102 aridade; E7103 nome/tipo desconhecido; E7104 campo/coluna/tabela desconhecida; E7105 `Text` não exibível; E7106 campo ambíguo; E7107 tipo infinito; E7108 valor descartado; E7109 padrão de ponto flutuante; E7110 declaração duplicada; E7111 `return` em lambda; E7113 variável ligada duas vezes; E7201 `when` não exaustivo; E7202 braço redundante; E7301 tipo não suporta operador; E7302 tipo ambíguo.
- Saída determinística: diagnósticos ordenados por (arquivo, linha, coluna, código); variáveis de tipo renderizadas como `'a`, `'b`… em ordem de aparição.
- Nenhum `panic!` alcançável por entrada do usuário; `unreachable!` só para invariantes internas.
- Verificação por tarefa: `cargo test` verde e `cargo clippy --all-targets -- -D warnings -A dead_code`. O módulo `types` só é ligado ao compilador na Task 9, então `dead_code` é tolerado até lá. Da Task 9 em diante: `cargo clippy --all-targets -- -D warnings` sem exceções.
- Toda mensagem de commit termina com `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Trabalhe no branch `type-system-core` do repositório `bind-lang`.

## Review Focus

1. **Operador no início de linha.** `val y = a` seguido de `- b` na linha de baixo vira dois statements (regra do Kotlin). O `- b` descartado precisa dar E7108, nunca ser aceito em silêncio. Teste na Task 7.
2. **`$` literal em strings.** `"US$ 10"`, `"$"` e `"preço: $10"` são aceitos; só `"$nome"` e `"${expr}"` dão E7001. Teste na Task 2.
3. **Nomes do Bind sombreados pelo usuário.** Um `val db`, um parâmetro `server` ou uma `fun Text(...)` do usuário vencem as formas embutidas. Teste na Task 8.
4. **Erros em vários arquivos.** Diagnósticos de dois arquivos saem ordenados por arquivo e linha. Teste na Task 9.
5. **Sem cascata.** `fun f() = naoExiste.campo + 1` produz só E7103, não E7104/E7301 também. Teste na Task 7.

## Estrutura de arquivos

| Arquivo | Ação | Responsabilidade |
|---|---|---|
| `src/ast.rs` | modificar | `Variant`, `BodyKind`, `type_params`, anotações opcionais, `Function.expr` |
| `src/parser.rs` | modificar | genéricos, variantes com campos, tipos função, anotações opcionais, chama o parser de expressões |
| `src/syntax.rs` | criar | `pub mod expr;` |
| `src/syntax/expr.rs` | criar | AST de expressões/padrões/tipos e renderização |
| `src/parser/expr.rs` | criar | parser Pratt de expressões, padrões e tipos |
| `src/types/mod.rs` | criar | `check()`, verificação de fronteiras (E7003), helpers de teste |
| `src/types/ty.rs` | criar | `Type`, `TypeTable`, níveis, `Constraint`, `Scheme`, generalização, renderização |
| `src/types/unify.rs` | criar | unificação, occurs check, restrições |
| `src/types/exhaustive.rs` | criar | Maranget: exaustividade e redundância |
| `src/types/env.rs` | criar | tipos declarados, construtores, vocabulário embutido |
| `src/types/infer.rs` | criar | inferência HM |
| `src/types/golden.rs` | criar | runner dos testes golden (só em teste, Task 10) |
| `src/types/props.rs` | criar | testes de propriedade (só em teste) |
| `tests/typecheck/*.bd`, `*.expected`, `*.csv` | criar | casos golden |
| `src/compiler.rs` | modificar | consumidores da Task 1; chamada a `types::check` |
| `src/certified.rs` | modificar | só os consumidores mecânicos da Task 1 |
| `src/main.rs` | modificar | `mod syntax; mod types;` |
| `Cargo.toml` | modificar | dev-dependency `proptest` |
| `docs/type-system.md`, `README.md` | criar/modificar | regras formais e limites |

---

### Task 1: Baseline, AST e parser de declarações

**Files:**
- Modify: `src/ast.rs`, `src/parser.rs`, `src/compiler.rs`, `src/certified.rs`

**Interfaces:**
- Produces:
  - `pub struct Variant { pub name: String, pub fields: Vec<Field>, pub span: Span }`
  - `TypeDecl` ganha `pub type_params: Vec<String>` e `pub cases: Vec<Variant>` (`variants: Vec<String>` continua, com os nomes de `cases`)
  - `pub enum BodyKind { Block, Expression, Absent }`
  - `Parameter.type_ref: Option<TypeRef>` e `Parameter::declared_type(&self) -> TypeRef`
  - `Function` ganha `pub type_params: Vec<String>`, `pub body_kind: BodyKind`; `return_type: Option<TypeRef>`; `Function::declared_return(&self) -> TypeRef`
  - Tipos função em anotações viram texto: `(Int, String)->Int`; tipo parentizado anulável: `((Int)->Int)` com `nullable: true`

- [ ] **Step 1: Capturar o baseline dos artefatos gerados (antes de qualquer mudança)**

```bash
cd /Users/leonardocaldas/Workspace/pessoal/bind/bind-lang
export DATABASE_URL=postgresql://bind:bind@localhost/bind
rm -rf /tmp/bind-baseline && mkdir -p /tmp/bind-baseline
cargo run --quiet -- generate --target aws-lambda --out /tmp/bind-baseline/aws-lambda
cargo run --quiet -- generate --target vercel --out /tmp/bind-baseline/vercel
(cd ../website && cargo run --quiet --manifest-path ../bind-lang/Cargo.toml -- generate --manifest bind.toml --target vercel --out /tmp/bind-baseline/website)
(cd /tmp/bind-baseline && find . -type f -print0 | sort -z | xargs -0 shasum -a 256) > /tmp/bind-baseline.sha256
wc -l /tmp/bind-baseline.sha256
```
Expected: os três comandos terminam com código 0 e o arquivo de hashes tem pelo menos uma linha. Se algum `generate` falhar, pare e reporte (o baseline é pré-requisito do critério de regressão).

- [ ] **Step 2: Escrever os testes do parser que falham**

Adicione ao `mod tests` de `src/parser.rs`:

```rust
    fn parse_source(source: &str) -> Program {
        let (tokens, lex_diagnostics) = lex("test.bd", source);
        assert!(lex_diagnostics.is_empty(), "{lex_diagnostics:?}");
        let (program, diagnostics) = parse(tokens);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        program
    }

    #[test]
    fn parses_unannotated_parameters_and_generics() {
        let program = parse_source("fun dobro(x) = x * 2\nfun <T> primeiro(xs: List<T>): T? = first(xs)\n");
        assert_eq!(program.functions[0].parameters[0].type_ref, None);
        assert_eq!(program.functions[0].return_type, None);
        assert_eq!(program.functions[0].body_kind, BodyKind::Expression);
        assert_eq!(program.functions[1].type_params, vec!["T"]);
        assert_eq!(program.functions[1].declared_return().display(), "T?");
    }

    #[test]
    fn parses_sealed_variants_with_fields_and_type_parameters() {
        let program = parse_source(
            "sealed class Arvore<T> {\n    data class No(val esq: Arvore<T>, val valor: T, val dir: Arvore<T>)\n    data object Folha\n}\n",
        );
        let declaration = &program.types[0];
        assert_eq!(declaration.type_params, vec!["T"]);
        assert_eq!(declaration.variants, vec!["No", "Folha"]);
        assert_eq!(declaration.cases[0].fields.len(), 3);
        assert_eq!(declaration.cases[0].fields[1].type_ref.display(), "T");
        assert!(declaration.cases[1].fields.is_empty());
    }

    #[test]
    fn parses_function_types_in_annotations() {
        let program = parse_source("fun aplica(f: (Int) -> Int, g: ((Int) -> Int)?, x: Int): Int = f(x)\n");
        let parameters = &program.functions[0].parameters;
        assert_eq!(parameters[0].declared_type().display(), "(Int)->Int");
        assert_eq!(parameters[1].declared_type().display(), "((Int)->Int)?");
        assert_eq!(program.functions[0].body_kind, BodyKind::Expression);
    }

    #[test]
    fn functions_without_body_are_absent() {
        let program = parse_source("external fun relogio(): Int\n");
        assert_eq!(program.functions[0].body_kind, BodyKind::Absent);
    }
```

E ao `mod tests` de `src/compiler.rs`, depois de `const SCHEMA`:

```rust
    const EMPTY_SCHEMA: &str =
        "table,model,primary_key,column,type,nullable,required,has_default,engine\n";

    #[test]
    fn legacy_validation_skips_type_parameters_and_function_types() {
        let source = "pure fun <T> ident(x: T): T = x\npure fun aplica(f: (Int) -> Int, x: Int): Int = f(x)\n";
        let (root, config) = fixture(source, EMPTY_SCHEMA);
        let result = verify(&config, "aws-lambda");
        assert!(
            !result.diagnostics.iter().any(|item| item.code == "E2003"),
            "{:?}",
            result.diagnostics
        );
        fs::remove_dir_all(root).unwrap();
    }
```

- [ ] **Step 3: Rodar e confirmar a falha**

Run: `cargo test parser::tests 2>&1 | tail -20`
Expected: FAIL de compilação (`no field type_params`, `BodyKind` não existe etc.).

- [ ] **Step 4: Atualizar `src/ast.rs`**

Substitua as definições de `TypeDecl`, `Parameter` e `Function` por:

```rust
#[derive(Clone, Debug)]
pub struct Variant {
    pub name: String,
    pub fields: Vec<Field>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TypeDecl {
    pub name: String,
    pub kind: TypeKind,
    pub type_params: Vec<String>,
    pub underlying: Option<TypeRef>,
    pub fields: Vec<Field>,
    /// Variant names, kept for the legacy analysis and generators.
    pub variants: Vec<String>,
    /// Variants of `sealed class` and `enum class`, with their fields.
    pub cases: Vec<Variant>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Parameter {
    pub name: String,
    pub type_ref: Option<TypeRef>,
}

impl Parameter {
    /// Declared type, or `Unit` when absent. Boundary functions without annotations are
    /// rejected with E7003 before code generation, so generators never see the fallback.
    pub fn declared_type(&self) -> TypeRef {
        self.type_ref.clone().unwrap_or_else(TypeRef::unit)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BodyKind {
    Block,
    Expression,
    Absent,
}

#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    pub domain: Domain,
    pub component: bool,
    pub suspend: bool,
    pub type_params: Vec<String>,
    pub parameters: Vec<Parameter>,
    pub return_type: Option<TypeRef>,
    pub effects: Vec<String>,
    pub requires: Vec<String>,
    pub body_kind: BodyKind,
    pub body: Vec<Token>,
    pub span: Span,
}

impl Function {
    /// Declared return type, or `Unit` when absent (the legacy default).
    pub fn declared_return(&self) -> TypeRef {
        self.return_type.clone().unwrap_or_else(TypeRef::unit)
    }
}
```

- [ ] **Step 5: Atualizar `src/parser.rs`**

1. Adicione o método (dentro de `impl Parser`):

```rust
    fn type_params(&mut self) -> Vec<String> {
        let mut params = Vec::new();
        if !self.consume("<") {
            return params;
        }
        while !self.at_eof() && !self.consume(">") {
            if let Some(name) = self.identifier() {
                params.push(name);
            } else {
                self.recover_to(&[",", ">"]);
            }
            self.consume(",");
        }
        params
    }

    fn balanced_group(&mut self) -> String {
        let mut text = String::new();
        let mut depth = 0usize;
        loop {
            let token = self.advance().text.clone();
            if token == "(" {
                depth += 1;
            }
            if token == ")" {
                depth = depth.saturating_sub(1);
            }
            text.push_str(&token);
            if token == "," {
                text.push(' ');
            }
            if depth == 0 || self.at_eof() {
                break;
            }
        }
        text
    }
```

2. Em `distinct_type`, adicione `type_params: vec![],` e `cases: vec![],` ao literal `TypeDecl`.

3. Em `record_type`, logo após `let name = self.identifier()?;` adicione `let type_params = self.type_params();` e, no literal, `type_params,` e `cases: vec![],`.

4. Substitua `sealed_type` inteiro e adicione `sealed_case`:

```rust
    fn sealed_type(&mut self) -> Option<TypeDecl> {
        let span = self.peek().span.clone();
        self.advance();
        self.advance();
        let name = self.identifier()?;
        let type_params = self.type_params();
        let mut cases = Vec::new();
        if self.consume("{") {
            while !self.at_eof() && !self.consume("}") {
                if self.consume(";") {
                    continue;
                }
                match self.sealed_case() {
                    Some(case) => cases.push(case),
                    None => self.recover_to(&["data", "class", "object", "}"]),
                }
            }
        }
        Some(TypeDecl {
            name,
            kind: TypeKind::Sealed,
            type_params,
            underlying: None,
            fields: vec![],
            variants: cases.iter().map(|case| case.name.clone()).collect(),
            cases,
            span,
        })
    }

    fn sealed_case(&mut self) -> Option<Variant> {
        let span = self.peek().span.clone();
        self.consume("data");
        let has_fields = if self.consume("class") {
            true
        } else if self.consume("object") {
            false
        } else {
            self.diagnostics.push(Diagnostic::error(
                "E1104",
                format!(
                    "Expected a sealed variant (`data class` or `data object`), found `{}`.",
                    self.peek_text()
                ),
                self.peek().span.clone(),
            ));
            self.advance();
            return None;
        };
        let name = self.identifier()?;
        let fields = if has_fields && self.consume("(") {
            self.fields(")")
        } else {
            Vec::new()
        };
        if self.consume(":") {
            let _ = self.type_ref();
            if self.consume("(") {
                self.recover_to(&[")"]);
                self.consume(")");
            }
        }
        Some(Variant { name, fields, span })
    }
```

5. Em `enum_type`, troque a coleta de nomes por variantes com span:

```rust
        let mut cases = Vec::new();
        if self.consume("{") {
            while !self.at_eof() && !self.consume("}") {
                if self.peek().kind == TokenKind::Identifier {
                    let token = self.advance().clone();
                    cases.push(Variant {
                        name: token.text,
                        fields: vec![],
                        span: token.span,
                    });
                } else {
                    self.advance();
                }
                self.consume(",");
            }
        }
        Some(TypeDecl {
            name,
            kind: TypeKind::Enum,
            type_params: vec![],
            underlying: None,
            fields: vec![],
            variants: cases.iter().map(|case| case.name.clone()).collect(),
            cases,
            span,
        })
```

6. Em `function`, troque o trecho de `component` até o literal final:

```rust
        let (component, type_params) = if self.consume("component") {
            (true, Vec::new())
        } else {
            self.expect("fun")?;
            (false, self.type_params())
        };
        let name = self.identifier()?;
        self.expect("(")?;
        let mut parameters = Vec::new();
        while !self.at_eof() && !self.consume(")") {
            let parameter_name = self.identifier()?;
            let type_ref = if self.consume(":") {
                Some(self.type_ref()?)
            } else {
                None
            };
            parameters.push(Parameter {
                name: parameter_name,
                type_ref,
            });
            if self.consume("=") {
                self.recover_to(&[",", ")"]);
            }
            self.consume(",");
        }
        let return_type = if self.consume(":") {
            Some(self.type_ref()?)
        } else {
            None
        };
        let mut effects = Vec::new();
        let mut requires = Vec::new();
        loop {
            if self.consume("effects") {
                effects = self.name_list();
            } else if self.consume("requires") {
                requires = self.name_list();
            } else {
                break;
            }
        }
        let (body_kind, body) = if self.consume("{") {
            (BodyKind::Block, self.block_contents())
        } else if self.consume("=") {
            (BodyKind::Expression, self.expression_body(span.line))
        } else {
            (BodyKind::Absent, Vec::new())
        };
        Some(Function {
            name,
            domain,
            component,
            suspend,
            type_params,
            parameters,
            return_type,
            effects,
            requires,
            body_kind,
            body,
            span,
        })
```

7. No início de `type_ref`, antes de `let mut name = self.identifier()?;`, adicione:

```rust
        if self.peek_text() == "(" {
            let mut name = self.balanced_group();
            if self.consume("->") {
                let result = self.type_ref()?;
                name.push_str("->");
                name.push_str(&result.display());
                return Some(TypeRef {
                    name,
                    nullable: false,
                });
            }
            let nullable = self.consume("?");
            return Some(TypeRef { name, nullable });
        }
```

8. Em `is_top_level_start`, adicione `"external"` e `"database"` à lista, para que um corpo de expressão pare antes delas.

- [ ] **Step 6: Corrigir os consumidores (guiado pelo compilador)**

Rode `cargo build 2>&1 | grep -E "^error" -A6` e corrija cada erro com estas regras exatas:

| Padrão antigo | Novo |
|---|---|
| `function.return_type` / `procedure.return_type` / `item.return_type` usado como `TypeRef` ou `&TypeRef` | `function.declared_return()` (ou `&function.declared_return()`) |
| `parameter.type_ref` usado como `TypeRef`/`&TypeRef` em `certified.rs` | `parameter.declared_type()` (ou `&parameter.declared_type()`) |
| `parameter.type_ref.name` (`certified.rs:641-642`) | `parameter.declared_type().name` |
| `return_type: function.return_type.clone()` em `RpcContract` (`compiler.rs:446`) | `return_type: function.declared_return()` |

Casos especiais:

`src/compiler.rs:610-615` (iteração em `validate_certified_surface`), troque por valores próprios:

```rust
        for type_ref in function
            .parameters
            .iter()
            .map(Parameter::declared_type)
            .chain(std::iter::once(function.declared_return()))
        {
```
(O corpo do laço continua igual; `type_ref` agora é `TypeRef` por valor.)

`src/compiler.rs:800-804` (locals em `analyze_function`):

```rust
    let mut locals: BTreeMap<String, TypeRef> = function
        .parameters
        .iter()
        .filter_map(|item| item.type_ref.clone().map(|type_ref| (item.name.clone(), type_ref)))
        .collect();
```

`src/compiler.rs` `validate_declarations`: substitua os laços de campos e de funções por:

```rust
        for field in item.fields.iter().chain(item.cases.iter().flat_map(|case| &case.fields)) {
            if !mentions_type_param(&field.type_ref, &item.type_params) {
                validate_type(&field.type_ref, types, &field.span, diagnostics);
            }
        }
```
e
```rust
        if let Some(return_type) = &item.return_type
            && !mentions_type_param(return_type, &item.type_params)
        {
            validate_type(return_type, types, &item.span, diagnostics);
        }
        for parameter in &item.parameters {
            if let Some(type_ref) = &parameter.type_ref
                && !mentions_type_param(type_ref, &item.type_params)
            {
                validate_type(type_ref, types, &item.span, diagnostics);
            }
        }
```
e adicione a função livre:

```rust
/// Type parameters and function types are verified by the type checker (`types`), not here.
fn mentions_type_param(type_ref: &TypeRef, params: &[String]) -> bool {
    type_ref.name.starts_with('(')
        || type_ref
            .name
            .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
            .any(|part| params.iter().any(|param| param == part))
}
```

- [ ] **Step 7: Rodar os testes**

Run: `cargo test 2>&1 | tail -5`
Expected: PASS, 29 testes (24 antigos + 5 novos).

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`
Expected: sem warnings.

- [ ] **Step 8: Commit**

```bash
git add src/ast.rs src/parser.rs src/compiler.rs src/certified.rs
git commit -m "feat(parser): genéricos, variantes com campos e anotações opcionais

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

Observação: se `src/` ainda não estiver versionado neste branch, o `git add` acima versiona os arquivos inteiros. Isso é esperado; confirme com o humano antes, se ainda não foi feito.

---

### Task 2: AST de expressões e parser de expressões (núcleo)

**Files:**
- Create: `src/syntax.rs`, `src/syntax/expr.rs`, `src/parser/expr.rs`
- Modify: `src/main.rs`, `src/parser.rs` (só a linha `pub mod expr;`)

**Interfaces:**
- Produces (`crate::syntax::expr`): `NodeId`, `TypeExpr`, `Expr`, `ExprKind`, `UnaryOp`, `BinaryOp` (+ `symbol()`), `Block`, `Stmt`, `Arm`, `Pattern`, `FieldInit`, `render_expr`, `render_block`, `render_pattern`, `render_type`.
- Produces (`crate::parser::expr`): `parse_expression_tokens(tokens: &[Token], fallback: &Span) -> (Block, Vec<Diagnostic>)`, `parse_type_text(text: &str, span: &Span) -> Result<TypeExpr, Diagnostic>`, `struct ExprParser` (interno ao módulo).
- Nesta tarefa o parser cobre literais, nomes, parênteses, operadores, chamadas, campos, `?.`, `!!`, `?:`, construções removidas e tipos. Blocos, lambdas, `if`, `when`, padrões e builders vêm na Task 3.

- [ ] **Step 1: Criar `src/syntax.rs` e `src/syntax/expr.rs`**

`src/syntax.rs`:
```rust
//! Syntax trees produced by the expression parser.

pub mod expr;
```

`src/syntax/expr.rs`:
```rust
//! Expression, pattern and type syntax for function bodies.

use crate::diagnostic::Span;

/// Identifies an expression or block inside one function body.
pub type NodeId = usize;

#[derive(Clone, Debug, PartialEq)]
pub enum TypeExpr {
    Named { name: String, args: Vec<TypeExpr>, span: Span },
    Function { params: Vec<TypeExpr>, result: Box<TypeExpr> },
    Nullable(Box<TypeExpr>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub id: NodeId,
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    /// Decimal digits; hexadecimal and binary literals are normalized when they fit in `u128`.
    Int(String),
    Double(String),
    /// Written with an `f` suffix, stored without it.
    Float(String),
    Str(String),
    Bool(bool),
    Null,
    Name(String),
    Call { callee: Box<Expr>, args: Vec<Expr>, children: Option<Block> },
    Field { target: Box<Expr>, name: String },
    SafeField { target: Box<Expr>, name: String },
    Unary { op: UnaryOp, operand: Box<Expr> },
    Binary { op: BinaryOp, left: Box<Expr>, right: Box<Expr> },
    Elvis { value: Box<Expr>, fallback: Box<Expr> },
    If { condition: Box<Expr>, then_branch: Box<Expr>, else_branch: Box<Expr> },
    When { scrutinee: Box<Expr>, arms: Vec<Arm> },
    Lambda { params: Vec<(String, Span)>, body: Block },
    Block(Block),
    Return(Box<Expr>),
    /// `target { field = value ... }`, used by `db.<table>.insert`.
    Builder { target: Box<Expr>, fields: Vec<FieldInit> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
}

impl BinaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Rem => "%",
            Self::Eq => "==",
            Self::NotEq => "!=",
            Self::Lt => "<",
            Self::LtEq => "<=",
            Self::Gt => ">",
            Self::GtEq => ">=",
            Self::And => "&&",
            Self::Or => "||",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub id: NodeId,
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Val { name: String, annotation: Option<TypeExpr>, value: Expr, span: Span },
    Expr(Expr),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arm {
    pub pattern: Pattern,
    pub body: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Pattern {
    Wildcard,
    /// A binding or a field-less constructor; the type checker decides.
    Name(String, Span),
    Constructor { name: String, args: Vec<Pattern>, span: Span },
    Int(String, Span),
    Str(String, Span),
    Bool(bool, Span),
    /// Parsed so it can be rejected with a clear message (E7109).
    Double(String, Span),
    Null(Span),
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldInit {
    pub name: String,
    pub value: Expr,
    pub span: Span,
}

/// Renders an expression as Bind source. Binary and unary operations are fully
/// parenthesized, so `parse(render(e))` renders back to the same text.
pub fn render_expr(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Int(text) | ExprKind::Double(text) => text.clone(),
        ExprKind::Float(text) => format!("{text}f"),
        ExprKind::Str(text) => format!("{text:?}"),
        ExprKind::Bool(value) => value.to_string(),
        ExprKind::Null => "null".into(),
        ExprKind::Name(name) => name.clone(),
        ExprKind::Call { callee, args, children } => {
            let mut text = format!("{}({})", render_expr(callee), join(args, render_expr));
            if let Some(block) = children {
                text.push(' ');
                text.push_str(&render_block(block));
            }
            text
        }
        ExprKind::Field { target, name } => format!("{}.{name}", render_target(target)),
        ExprKind::SafeField { target, name } => format!("{}?.{name}", render_target(target)),
        ExprKind::Unary { op, operand } => {
            let symbol = match op {
                UnaryOp::Neg => "-",
                UnaryOp::Not => "!",
            };
            format!("({symbol}{})", render_expr(operand))
        }
        ExprKind::Binary { op, left, right } => {
            format!("({} {} {})", render_expr(left), op.symbol(), render_expr(right))
        }
        ExprKind::Elvis { value, fallback } => {
            format!("({} ?: {})", render_expr(value), render_expr(fallback))
        }
        ExprKind::If { condition, then_branch, else_branch } => format!(
            "(if ({}) {} else {})",
            render_expr(condition),
            render_expr(then_branch),
            render_expr(else_branch)
        ),
        ExprKind::When { scrutinee, arms } => {
            let arms: Vec<String> = arms
                .iter()
                .map(|arm| format!("{} -> {}", render_pattern(&arm.pattern), render_expr(&arm.body)))
                .collect();
            format!("when ({}) {{ {} }}", render_expr(scrutinee), arms.join("; "))
        }
        ExprKind::Lambda { params, body } => {
            let names: Vec<&str> = params.iter().map(|(name, _)| name.as_str()).collect();
            let prefix = if names.is_empty() { String::new() } else { format!("{} ", names.join(", ")) };
            format!("({{ {prefix}-> {} }})", render_stmts(&body.stmts))
        }
        ExprKind::Block(block) => render_block(block),
        ExprKind::Return(value) => format!("(return {})", render_expr(value)),
        ExprKind::Builder { target, fields } => {
            let fields: Vec<String> = fields
                .iter()
                .map(|field| format!("{} = {}", field.name, render_expr(&field.value)))
                .collect();
            format!("{} {{ {} }}", render_expr(target), fields.join("; "))
        }
    }
}

pub fn render_block(block: &Block) -> String {
    if block.stmts.is_empty() {
        "{ }".into()
    } else {
        format!("{{ {} }}", render_stmts(&block.stmts))
    }
}

pub fn render_pattern(pattern: &Pattern) -> String {
    match pattern {
        Pattern::Wildcard => "_".into(),
        Pattern::Name(name, _) => name.clone(),
        Pattern::Constructor { name, args, .. } => format!("{name}({})", join(args, render_pattern)),
        Pattern::Int(text, _) | Pattern::Double(text, _) => text.clone(),
        Pattern::Str(text, _) => format!("{text:?}"),
        Pattern::Bool(value, _) => value.to_string(),
        Pattern::Null(_) => "null".into(),
    }
}

pub fn render_type(ty: &TypeExpr) -> String {
    match ty {
        TypeExpr::Named { name, args, .. } if args.is_empty() => name.clone(),
        TypeExpr::Named { name, args, .. } => format!("{name}<{}>", join(args, render_type)),
        TypeExpr::Function { params, result } => {
            format!("({}) -> {}", join(params, render_type), render_type(result))
        }
        TypeExpr::Nullable(inner) => match inner.as_ref() {
            TypeExpr::Function { .. } => format!("({})?", render_type(inner)),
            _ => format!("{}?", render_type(inner)),
        },
    }
}

fn render_stmts(stmts: &[Stmt]) -> String {
    let rendered: Vec<String> = stmts
        .iter()
        .map(|stmt| match stmt {
            Stmt::Val { name, annotation: Some(ty), value, .. } => {
                format!("val {name}: {} = {}", render_type(ty), render_expr(value))
            }
            Stmt::Val { name, annotation: None, value, .. } => {
                format!("val {name} = {}", render_expr(value))
            }
            Stmt::Expr(expr) => render_expr(expr),
        })
        .collect();
    rendered.join("; ")
}

/// Number literals are parenthesized before `.` so `5.x` is not lexed as a number.
fn render_target(target: &Expr) -> String {
    match target.kind {
        ExprKind::Int(_) | ExprKind::Double(_) | ExprKind::Float(_) => {
            format!("({})", render_expr(target))
        }
        _ => render_expr(target),
    }
}

fn join<T>(items: &[T], render: impl Fn(&T) -> String) -> String {
    items.iter().map(render).collect::<Vec<_>>().join(", ")
}
```

Em `src/main.rs`, adicione `mod syntax;` (ordem alfabética, depois de `mod schema;`). Em `src/parser.rs`, adicione na primeira linha `pub mod expr;`.

- [ ] **Step 2: Escrever os testes que falham**

Crie `src/parser/expr.rs` só com o módulo de testes (o código entra no Step 4):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn tokens(source: &str) -> Vec<Token> {
        let (tokens, diagnostics) = lex("t.bd", source);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        tokens.into_iter().filter(|token| token.kind != TokenKind::Eof).collect()
    }

    fn expression(source: &str) -> String {
        let (block, diagnostics) = parse_expression_tokens(&tokens(source), &Span::default());
        assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
        render_block(&block)
    }

    fn codes(source: &str) -> Vec<&'static str> {
        let (_, diagnostics) = parse_expression_tokens(&tokens(source), &Span::default());
        diagnostics.iter().map(|item| item.code).collect()
    }

    #[test]
    fn respects_kotlin_precedence() {
        assert_eq!(expression("1 + 2 * 3"), "{ (1 + (2 * 3)) }");
        assert_eq!(expression("a || b && c == d"), "{ (a || (b && (c == d))) }");
        assert_eq!(expression("a < b + 1"), "{ (a < (b + 1)) }");
        assert_eq!(expression("a ?: b + 1"), "{ (a ?: (b + 1)) }");
        assert_eq!(expression("a - b - c"), "{ ((a - b) - c) }");
        assert_eq!(expression("-a * !b"), "{ ((-a) * (!b)) }");
        assert_eq!(expression("(a + b) * c"), "{ ((a + b) * c) }");
    }

    #[test]
    fn parses_calls_fields_and_safe_calls() {
        assert_eq!(expression("f(a, g(b))"), "{ f(a, g(b)) }");
        assert_eq!(expression("server.getCustomer(id).name"), "{ server.getCustomer(id).name }");
        assert_eq!(expression("cliente?.apelido ?: nome"), "{ (cliente?.apelido ?: nome) }");
        assert_eq!(expression("f(\n    a,\n    b\n)"), "{ f(a, b) }");
    }

    #[test]
    fn classifies_number_literals() {
        assert_eq!(expression("1_000"), "{ 1000 }");
        assert_eq!(expression("0xFF"), "{ 255 }");
        assert_eq!(expression("0b101"), "{ 5 }");
        assert_eq!(expression("42L"), "{ 42 }");
        assert_eq!(expression("2.5"), "{ 2.5 }");
        assert_eq!(expression("1.5f"), "{ 1.5f }");
        assert_eq!(codes("7u"), vec!["E7004"]);
    }

    #[test]
    fn newline_ends_an_expression_except_after_logical_operators() {
        assert_eq!(codes("a\n+ b"), vec!["E7005"]);
        assert_eq!(expression("a &&\n    b"), "{ (a && b) }");
        assert_eq!(expression("a\n    || b"), "{ (a || b) }");
    }

    #[test]
    fn rejects_removed_constructs() {
        assert_eq!(codes("x!!"), vec!["E7001"]);
        assert_eq!(codes("throw erro"), vec!["E7001"]);
        assert_eq!(codes("\"Olá $nome\""), vec!["E7001"]);
        assert_eq!(codes("\"total: ${a + b}\""), vec!["E7001"]);
        assert_eq!(codes("'x'"), vec!["E7004"]);
    }

    #[test]
    fn accepts_literal_dollar_signs() {
        assert_eq!(expression("\"US$ 10\""), "{ \"US$ 10\" }");
        assert_eq!(expression("\"$\""), "{ \"$\" }");
        assert_eq!(expression("\"preço: $10\""), "{ \"preço: $10\" }");
    }

    #[test]
    fn parses_type_text() {
        let span = Span::default();
        let render = |text: &str| render_type(&parse_type_text(text, &span).unwrap());
        assert_eq!(render("List<Map<String,Int>>"), "List<Map<String, Int>>");
        assert_eq!(render("(Int, String)->Int?"), "(Int, String) -> Int?");
        assert_eq!(render("((Int)->Int)?"), "((Int) -> Int)?");
        assert_eq!(render("Int??"), "Int??");
        assert!(parse_type_text("List<", &span).is_err());
    }
}
```

- [ ] **Step 3: Rodar e confirmar a falha**

Run: `cargo test parser::expr 2>&1 | tail -5`
Expected: FAIL de compilação (`parse_expression_tokens` não existe).

- [ ] **Step 4: Implementar o parser (antes do módulo de testes em `src/parser/expr.rs`)**

```rust
//! Expression parser: turns the tokens of a function body into the expression AST.
//!
//! Newlines end statements, as in Kotlin: an infix operator continues an expression only
//! when it is on the same line as the previous token, except `&&`, `||` and `?:`.
//! Inside parentheses newlines are insignificant.

use crate::diagnostic::{Diagnostic, Span};
use crate::lexer::{Token, TokenKind, lex};
use crate::syntax::expr::*;

pub fn parse_expression_tokens(tokens: &[Token], fallback: &Span) -> (Block, Vec<Diagnostic>) {
    let mut parser = ExprParser::new(tokens, fallback);
    let span = parser.peek().span.clone();
    let mut stmts = Vec::new();
    if let Some(expr) = parser.expression() {
        stmts.push(Stmt::Expr(expr));
    }
    if !parser.at_end() && parser.diagnostics.is_empty() {
        let token = parser.peek().clone();
        parser.error(
            "E7005",
            format!("Expected a new line or `;` before {}.", describe(&token)),
            token.span,
        );
    }
    let id = parser.next_id();
    (Block { id, stmts, span }, parser.diagnostics)
}

/// Parses a type written as text, such as the `TypeRef` names of declarations.
pub fn parse_type_text(text: &str, span: &Span) -> Result<TypeExpr, Diagnostic> {
    let (mut tokens, lex_diagnostics) = lex(&span.file, text);
    if let Some(mut diagnostic) = lex_diagnostics.into_iter().next() {
        diagnostic.span = span.clone();
        return Err(diagnostic);
    }
    for token in &mut tokens {
        token.span = span.clone();
    }
    let mut parser = ExprParser::new(&tokens, span);
    let parsed = parser.type_expr();
    let complete = parser.at_end();
    match (parsed, parser.diagnostics.into_iter().next()) {
        (_, Some(diagnostic)) => Err(diagnostic),
        (Some(ty), None) if complete => Ok(ty),
        _ => Err(Diagnostic::error("E7005", format!("Invalid type `{text}`."), span.clone())),
    }
}

enum Infix {
    Op(BinaryOp),
    Elvis,
}

enum NumberLiteral {
    Int(String),
    Double(String),
    Float(String),
}

pub(crate) struct ExprParser {
    tokens: Vec<Token>,
    index: usize,
    next_id: NodeId,
    /// Depth of enclosing parentheses; newlines are insignificant when it is non-zero.
    nesting: usize,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

impl ExprParser {
    pub(crate) fn new(tokens: &[Token], fallback: &Span) -> Self {
        let mut tokens = tokens.to_vec();
        if tokens.last().is_none_or(|token| token.kind != TokenKind::Eof) {
            let span = tokens.last().map_or_else(|| fallback.clone(), |token| token.span.clone());
            tokens.push(Token { kind: TokenKind::Eof, text: String::new(), span });
        }
        Self { tokens, index: 0, next_id: 0, nesting: 0, diagnostics: Vec::new() }
    }

    // ---- token helpers -------------------------------------------------------------

    fn peek(&self) -> &Token {
        &self.tokens[self.index.min(self.tokens.len() - 1)]
    }

    fn peek_at(&self, offset: usize) -> &Token {
        &self.tokens[(self.index + offset).min(self.tokens.len() - 1)]
    }

    fn at_end(&self) -> bool {
        self.peek().kind == TokenKind::Eof
    }

    fn advance(&mut self) -> Token {
        let token = self.peek().clone();
        if !self.at_end() {
            self.index += 1;
        }
        token
    }

    /// True when the current token is the keyword or symbol `text` (never a string literal).
    fn is(&self, text: &str) -> bool {
        let token = self.peek();
        token.text == text && matches!(token.kind, TokenKind::Identifier | TokenKind::Symbol)
    }

    fn consume(&mut self, text: &str) -> bool {
        if self.is(text) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, text: &str) -> Option<()> {
        if self.consume(text) {
            return Some(());
        }
        let token = self.peek().clone();
        self.error("E7005", format!("Expected `{text}`, found {}.", describe(&token)), token.span);
        None
    }

    /// Two symbols written without space between them, such as `?` `:` or `!` `!`.
    fn adjacent_pair(&self, first: &str, second: &str) -> bool {
        let (a, b) = (self.peek(), self.peek_at(1));
        a.kind == TokenKind::Symbol
            && a.text == first
            && b.text == second
            && b.span.line == a.span.line
            && b.span.column == a.span.column + a.text.chars().count()
    }

    fn previous_line(&self) -> usize {
        if self.index == 0 { 0 } else { self.tokens[self.index - 1].span.line }
    }

    fn same_line(&self) -> bool {
        self.nesting > 0 || self.peek().span.line == self.previous_line()
    }

    fn identifier(&mut self) -> Option<String> {
        let token = self.peek().clone();
        if token.kind == TokenKind::Identifier {
            self.advance();
            Some(token.text)
        } else {
            self.error("E7005", format!("Expected a name, found {}.", describe(&token)), token.span);
            None
        }
    }

    fn next_id(&mut self) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn node(&mut self, kind: ExprKind, span: Span) -> Expr {
        Expr { id: self.next_id(), kind, span }
    }

    fn error(&mut self, code: &'static str, message: impl Into<String>, span: Span) {
        self.diagnostics.push(Diagnostic::error(code, message, span));
    }

    fn removed(&mut self, what: &str, span: Span) {
        let (message, hint) = match what {
            "var" => ("`var` was removed: Bind values are immutable.", "Use `val`."),
            "!!" => ("`!!` was removed: it can crash at runtime.", "Handle `null` with `when`, `?:` or `?.`."),
            "throw" | "try" | "catch" => (
                "Exceptions were removed: errors are values in Bind.",
                "Return a `Result<T, E>` or a nullable value.",
            ),
            "while" | "do" | "for" => (
                "Loops were removed: Bind has no mutation to loop over.",
                "Use recursion, `map` or `fold`.",
            ),
            _ => (
                "String interpolation is not supported: the embedded expression would not be type-checked.",
                "Concatenate with `+`, e.g. `\"Hello, \" + name`.",
            ),
        };
        self.diagnostics.push(Diagnostic::error("E7001", message, span).with_hint(hint));
    }

    // ---- expressions ---------------------------------------------------------------

    fn expression(&mut self) -> Option<Expr> {
        self.binary(1)
    }

    fn infix(&self) -> Option<(Infix, u8, usize)> {
        let token = self.peek();
        if token.kind != TokenKind::Symbol {
            return None;
        }
        let op = |op| Some((Infix::Op(op), 0, 1));
        let (infix, _, width) = match token.text.as_str() {
            "?" if self.adjacent_pair("?", ":") => (Infix::Elvis, 0, 2),
            "||" => op(BinaryOp::Or)?,
            "&&" => op(BinaryOp::And)?,
            "==" => op(BinaryOp::Eq)?,
            "!=" => op(BinaryOp::NotEq)?,
            "<" => op(BinaryOp::Lt)?,
            "<=" => op(BinaryOp::LtEq)?,
            ">" => op(BinaryOp::Gt)?,
            ">=" => op(BinaryOp::GtEq)?,
            "+" => op(BinaryOp::Add)?,
            "-" => op(BinaryOp::Sub)?,
            "*" => op(BinaryOp::Mul)?,
            "/" => op(BinaryOp::Div)?,
            "%" => op(BinaryOp::Rem)?,
            _ => return None,
        };
        let precedence = match &infix {
            Infix::Op(BinaryOp::Or) => 1,
            Infix::Op(BinaryOp::And) => 2,
            Infix::Op(BinaryOp::Eq | BinaryOp::NotEq) => 3,
            Infix::Op(BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq) => 4,
            Infix::Elvis => 5,
            Infix::Op(BinaryOp::Add | BinaryOp::Sub) => 6,
            Infix::Op(BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem) => 7,
        };
        Some((infix, precedence, width))
    }

    fn binary(&mut self, min: u8) -> Option<Expr> {
        let mut left = self.unary()?;
        while let Some((infix, precedence, width)) = self.infix() {
            if precedence < min {
                break;
            }
            let crosses_lines = matches!(infix, Infix::Op(BinaryOp::And | BinaryOp::Or) | Infix::Elvis);
            if !self.same_line() && !crosses_lines {
                break;
            }
            for _ in 0..width {
                self.advance();
            }
            let right = self.binary(precedence + 1)?;
            let span = left.span.clone();
            let kind = match infix {
                Infix::Op(op) => ExprKind::Binary { op, left: Box::new(left), right: Box::new(right) },
                Infix::Elvis => ExprKind::Elvis { value: Box::new(left), fallback: Box::new(right) },
            };
            left = self.node(kind, span);
        }
        Some(left)
    }

    fn unary(&mut self) -> Option<Expr> {
        let op = if self.is("-") {
            UnaryOp::Neg
        } else if self.is("!") {
            UnaryOp::Not
        } else {
            return self.postfix();
        };
        let span = self.advance().span;
        let operand = self.unary()?;
        Some(self.node(ExprKind::Unary { op, operand: Box::new(operand) }, span))
    }

    fn postfix(&mut self) -> Option<Expr> {
        let mut expr = self.primary()?;
        loop {
            if self.is("(") && self.same_line() {
                let args = self.arguments()?;
                let span = expr.span.clone();
                expr = self.node(ExprKind::Call { callee: Box::new(expr), args, children: None }, span);
            } else if self.is(".") {
                self.advance();
                let name = self.identifier()?;
                let span = expr.span.clone();
                expr = self.node(ExprKind::Field { target: Box::new(expr), name }, span);
            } else if self.adjacent_pair("?", ".") {
                self.advance();
                self.advance();
                let name = self.identifier()?;
                let span = expr.span.clone();
                expr = self.node(ExprKind::SafeField { target: Box::new(expr), name }, span);
            } else if self.adjacent_pair("!", "!") && self.same_line() {
                let span = self.advance().span;
                self.advance();
                self.removed("!!", span);
            } else {
                break;
            }
        }
        Some(expr)
    }

    fn arguments(&mut self) -> Option<Vec<Expr>> {
        self.advance(); // `(`
        self.nesting += 1;
        let mut args = Vec::new();
        while !self.is(")") && !self.at_end() {
            if self.peek().kind == TokenKind::Identifier
                && self.peek_at(1).kind == TokenKind::Symbol
                && self.peek_at(1).text == "="
            {
                let span = self.peek().span.clone();
                self.error("E7004", "Named arguments are not supported; pass arguments by position.", span);
                self.advance();
                self.advance();
            }
            let Some(arg) = self.expression() else {
                self.nesting -= 1;
                return None;
            };
            args.push(arg);
            if !self.consume(",") {
                break;
            }
        }
        self.nesting -= 1;
        self.expect(")")?;
        Some(args)
    }

    fn primary(&mut self) -> Option<Expr> {
        let token = self.peek().clone();
        match token.kind {
            TokenKind::Number => {
                self.advance();
                match classify_number(&token.text) {
                    Ok(NumberLiteral::Int(text)) => Some(self.node(ExprKind::Int(text), token.span)),
                    Ok(NumberLiteral::Double(text)) => Some(self.node(ExprKind::Double(text), token.span)),
                    Ok(NumberLiteral::Float(text)) => Some(self.node(ExprKind::Float(text), token.span)),
                    Err(message) => {
                        self.error("E7004", message, token.span);
                        None
                    }
                }
            }
            TokenKind::String => {
                self.advance();
                if has_interpolation(&token.text) {
                    self.removed("interpolation", token.span.clone());
                }
                Some(self.node(ExprKind::Str(token.text), token.span))
            }
            TokenKind::Character => {
                self.advance();
                self.error("E7004", "Character literals are not supported; use a String.", token.span);
                None
            }
            TokenKind::Eof => {
                self.error("E7005", "Expected an expression, found end of input.", token.span);
                None
            }
            TokenKind::Identifier => match token.text.as_str() {
                "true" | "false" => {
                    self.advance();
                    Some(self.node(ExprKind::Bool(token.text == "true"), token.span))
                }
                "null" => {
                    self.advance();
                    Some(self.node(ExprKind::Null, token.span))
                }
                "return" => {
                    self.advance();
                    let value = self.expression()?;
                    Some(self.node(ExprKind::Return(Box::new(value)), token.span))
                }
                "var" | "throw" | "try" | "catch" | "while" | "do" | "for" => {
                    self.advance();
                    self.removed(&token.text, token.span);
                    None
                }
                _ => {
                    self.advance();
                    Some(self.node(ExprKind::Name(token.text), token.span))
                }
            },
            TokenKind::Symbol if token.text == "(" => {
                self.advance();
                self.nesting += 1;
                let inner = self.expression();
                self.nesting -= 1;
                let inner = inner?;
                self.expect(")")?;
                Some(inner)
            }
            TokenKind::Symbol => {
                self.error("E7005", format!("Expected an expression, found `{}`.", token.text), token.span);
                None
            }
        }
    }

    // ---- types ---------------------------------------------------------------------

    pub(crate) fn type_expr(&mut self) -> Option<TypeExpr> {
        let span = self.peek().span.clone();
        let mut ty = if self.is("(") {
            self.advance();
            let mut params = Vec::new();
            while !self.is(")") && !self.at_end() {
                params.push(self.type_expr()?);
                if !self.consume(",") {
                    break;
                }
            }
            self.expect(")")?;
            if self.consume("->") {
                let result = self.type_expr()?;
                TypeExpr::Function { params, result: Box::new(result) }
            } else if params.len() == 1 {
                params.remove(0)
            } else {
                self.error("E7005", "Expected `->` after the parameter types of a function type.", span);
                return None;
            }
        } else {
            let name = self.identifier()?;
            let mut args = Vec::new();
            if self.consume("<") {
                loop {
                    args.push(self.type_expr()?);
                    if !self.consume(",") {
                        break;
                    }
                }
                self.expect(">")?;
            }
            TypeExpr::Named { name, args, span }
        };
        while self.consume("?") {
            ty = TypeExpr::Nullable(Box::new(ty));
        }
        Some(ty)
    }
}

fn describe(token: &Token) -> String {
    match token.kind {
        TokenKind::Eof => "end of input".into(),
        TokenKind::String => "a string".into(),
        _ => format!("`{}`", token.text),
    }
}

/// `$name` or `${...}`; a `$` followed by anything else is a literal dollar sign.
fn has_interpolation(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars
        .windows(2)
        .any(|pair| pair[0] == '$' && (pair[1] == '{' || pair[1] == '_' || pair[1].is_alphabetic()))
}

fn classify_number(text: &str) -> Result<NumberLiteral, &'static str> {
    let cleaned = text.replace('_', "").to_ascii_lowercase();
    if cleaned.ends_with('u') || cleaned.ends_with("ul") {
        return Err("Unsigned literals are not supported.");
    }
    let radix = |digits: &str, base: u32| {
        u128::from_str_radix(digits.trim_end_matches('l'), base)
            .map_or_else(|_| cleaned.clone(), |value| value.to_string())
    };
    if let Some(hex) = cleaned.strip_prefix("0x") {
        return Ok(NumberLiteral::Int(radix(hex, 16)));
    }
    if let Some(bin) = cleaned.strip_prefix("0b") {
        return Ok(NumberLiteral::Int(radix(bin, 2)));
    }
    if let Some(float) = cleaned.strip_suffix('f') {
        return Ok(NumberLiteral::Float(float.to_string()));
    }
    if cleaned.contains('.') || cleaned.contains('e') {
        return Ok(NumberLiteral::Double(cleaned));
    }
    let digits = cleaned.trim_end_matches('l').trim_start_matches('0');
    Ok(NumberLiteral::Int(if digits.is_empty() { "0".into() } else { digits.to_string() }))
}
```

Nota sobre `infix`: o closure `op` devolve `Option<(Infix, u8, usize)>` com precedência 0 provisória; a precedência real é calculada logo abaixo. O `?` nunca falha (sempre `Some`).

- [ ] **Step 5: Rodar os testes**

Run: `cargo test parser::expr 2>&1 | tail -5`
Expected: PASS (7 testes).

Run: `cargo clippy --all-targets -- -D warnings -A dead_code 2>&1 | tail -3`
Expected: sem warnings.

- [ ] **Step 6: Commit**

```bash
git add src/syntax.rs src/syntax/expr.rs src/parser/expr.rs src/parser.rs src/main.rs
git commit -m "feat(parser): AST e parser Pratt de expressões

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Blocos, lambdas, `if`, `when`, padrões, builders e integração ao parser

**Files:**
- Modify: `src/parser/expr.rs`, `src/ast.rs`, `src/parser.rs`

**Interfaces:**
- Consumes: tudo da Task 2.
- Produces:
  - `parse_block_tokens(tokens: &[Token], fallback: &Span) -> (Block, Vec<Diagnostic>)`
  - `Function.expr: Option<crate::syntax::expr::Block>`: `Some` para `BodyKind::Block` e `BodyKind::Expression` (um corpo de expressão vira um bloco com um único `Stmt::Expr`), `None` para `Absent`.
  - Ids de nó são únicos dentro de cada função (cada corpo tem seu próprio contador).

- [ ] **Step 1: Escrever os testes que falham**

Adicione ao `mod tests` de `src/parser/expr.rs`:

```rust
    fn block(source: &str) -> String {
        let (block, diagnostics) = parse_block_tokens(&tokens(source), &Span::default());
        assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
        render_block(&block)
    }

    fn block_codes(source: &str) -> Vec<&'static str> {
        let (_, diagnostics) = parse_block_tokens(&tokens(source), &Span::default());
        diagnostics.iter().map(|item| item.code).collect()
    }

    #[test]
    fn parses_statements_separated_by_newlines_and_semicolons() {
        assert_eq!(block("val x = 1\nval y: Int = x + 1\ny"), "{ val x = 1; val y: Int = (x + 1); y }");
        assert_eq!(block("a; b"), "{ a; b }");
        assert_eq!(block(""), "{ }");
        assert_eq!(block_codes("a b"), vec!["E7005"]);
    }

    #[test]
    fn operator_on_next_line_starts_a_new_statement() {
        assert_eq!(block("val y = a\n    - b\ny"), "{ val y = a; (-b); y }");
    }

    #[test]
    fn parses_if_and_requires_else() {
        assert_eq!(block("if (c) 1 else 2"), "{ (if (c) 1 else 2) }");
        assert_eq!(block("if (c) {\n    1\n} else {\n    2\n}"), "{ (if (c) { 1 } else { 2 }) }");
        assert_eq!(block_codes("if (c) 1"), vec!["E7005"]);
    }

    #[test]
    fn parses_when_with_patterns() {
        assert_eq!(
            block("when (p) {\n    Pago(v, _) -> v\n    Cancelado -> 0\n    else -> 1\n}"),
            "{ when (p) { Pago(v, _) -> v; Cancelado -> 0; _ -> 1 } }"
        );
        assert_eq!(
            block("when (x) { null -> 0; -1 -> 1; \"a\" -> 2; true -> 3; Status.Ativo -> 4; n -> n }"),
            "{ when (x) { null -> 0; -1 -> 1; \"a\" -> 2; true -> 3; Ativo -> 4; n -> n } }"
        );
        assert_eq!(block_codes("when { x -> 1 }"), vec!["E7004"]);
    }

    #[test]
    fn parses_lambdas_and_trailing_blocks() {
        assert_eq!(block("val f = { x, y -> x + y }"), "{ val f = ({ x, y -> (x + y) }) }");
        assert_eq!(block("val g = { -> 1 }"), "{ val g = ({ -> 1 }) }");
        assert_eq!(block("map(xs) { x -> x + 1 }"), "{ map(xs, ({ x -> (x + 1) })) }");
        assert_eq!(
            block("UI {\n    Div(\"card\") {\n        Text(\"a\")\n        Text(b.name)\n    }\n}"),
            "{ UI() { Div(\"card\") { Text(\"a\"); Text(b.name) } } }"
        );
        assert_eq!(block_codes("val h = { x }"), vec!["E7004"]);
    }

    #[test]
    fn parses_database_insert_builders() {
        assert_eq!(
            block("return db.customers.insert {\n    name = name\n    email = email\n}"),
            "{ (return db.customers.insert { name = name; email = email }) }"
        );
    }

    #[test]
    fn recovers_after_removed_constructs() {
        assert_eq!(block_codes("var x = 1\nx"), vec!["E7001"]);
        assert_eq!(block_codes("while (c) {\n    a\n}\nb"), vec!["E7001"]);
        assert_eq!(block_codes("for (x in xs) {\n    a\n}"), vec!["E7001"]);
    }
```

E ao `mod tests` de `src/parser.rs`:

```rust
    #[test]
    fn attaches_parsed_bodies_to_functions() {
        let program = parse_source(
            "fun um() = 1\nfun dois(): Int {\n    val x = 1\n    x + 1\n}\nexternal fun tres(): Int\n",
        );
        let rendered: Vec<Option<String>> = program
            .functions
            .iter()
            .map(|function| function.expr.as_ref().map(crate::syntax::expr::render_block))
            .collect();
        assert_eq!(
            rendered,
            vec![Some("{ 1 }".into()), Some("{ val x = 1; (x + 1) }".into()), None]
        );
    }

    #[test]
    fn reports_body_syntax_errors() {
        let (tokens, _) = lex("test.bd", "fun f(): Int {\n    var x = 1\n    x\n}\n");
        let (_, diagnostics) = parse(tokens);
        assert_eq!(diagnostics.iter().map(|item| item.code).collect::<Vec<_>>(), vec!["E7001"]);
    }
```

- [ ] **Step 2: Rodar e confirmar a falha**

Run: `cargo test parser 2>&1 | tail -5`
Expected: FAIL de compilação (`parse_block_tokens` e `Function.expr` não existem).

- [ ] **Step 3: Implementar em `src/parser/expr.rs`**

1. Adicione a função pública ao lado de `parse_expression_tokens`:

```rust
pub fn parse_block_tokens(tokens: &[Token], fallback: &Span) -> (Block, Vec<Diagnostic>) {
    let mut parser = ExprParser::new(tokens, fallback);
    let span = parser.peek().span.clone();
    let stmts = parser.statements(None);
    let id = parser.next_id();
    (Block { id, stmts, span }, parser.diagnostics)
}
```

2. Adicione estes métodos a `impl ExprParser` (seção nova `// ---- statements and blocks`):

```rust
    /// Skips to a plausible statement boundary after an error.
    fn recover(&mut self) {
        let mut depth = 0usize;
        while !self.at_end() {
            if depth == 0
                && (self.is(";")
                    || self.is("}")
                    || self.is(")")
                    || (self.nesting == 0 && self.index > 0 && self.peek().span.line != self.previous_line()))
            {
                return;
            }
            if self.is("(") || self.is("{") || self.is("[") {
                depth += 1;
            }
            if (self.is(")") || self.is("}") || self.is("]")) && depth > 0 {
                depth -= 1;
            }
            self.advance();
        }
    }

    fn at_close(&self, close: Option<&str>) -> bool {
        close.is_some_and(|text| self.is(text))
    }

    fn end_of_item(&mut self, close: Option<&str>) {
        if self.at_end() || self.is(";") || self.at_close(close) || !self.same_line() {
            return;
        }
        let token = self.peek().clone();
        self.error("E7005", format!("Expected a new line or `;` before {}.", describe(&token)), token.span);
        self.recover();
    }

    fn statements(&mut self, close: Option<&str>) -> Vec<Stmt> {
        let mut stmts = Vec::new();
        loop {
            while self.consume(";") {}
            if self.at_end() || self.at_close(close) {
                break;
            }
            let start = self.index;
            match self.statement() {
                Some(stmt) => {
                    stmts.push(stmt);
                    self.end_of_item(close);
                }
                None => self.recover(),
            }
            if self.index == start {
                self.advance();
            }
        }
        stmts
    }

    fn statement(&mut self) -> Option<Stmt> {
        if self.is("val") {
            let span = self.advance().span;
            let name = self.identifier()?;
            let annotation = if self.consume(":") { Some(self.type_expr()?) } else { None };
            self.expect("=")?;
            let value = self.expression()?;
            return Some(Stmt::Val { name, annotation, value, span });
        }
        self.expression().map(Stmt::Expr)
    }

    fn block(&mut self) -> Block {
        let span = self.peek().span.clone();
        let saved = std::mem::replace(&mut self.nesting, 0);
        let _ = self.expect("{");
        let stmts = self.statements(Some("}"));
        let _ = self.expect("}");
        self.nesting = saved;
        Block { id: self.next_id(), stmts, span }
    }

    /// `{ x, y -> ... }` or `{ -> ... }`.
    fn looks_like_lambda(&self) -> bool {
        let mut offset = 1;
        if self.peek_at(offset).text == "->" {
            return true;
        }
        loop {
            if self.peek_at(offset).kind != TokenKind::Identifier {
                return false;
            }
            match self.peek_at(offset + 1).text.as_str() {
                "," => offset += 2,
                "->" => return true,
                _ => return false,
            }
        }
    }

    fn lambda(&mut self) -> Option<Expr> {
        let span = self.peek().span.clone();
        let saved = std::mem::replace(&mut self.nesting, 0);
        self.advance(); // `{`, guaranteed by `looks_like_lambda`
        let mut params = Vec::new();
        while !self.is("->") {
            let token = self.advance();
            params.push((token.text, token.span));
            self.consume(",");
        }
        self.advance(); // `->`
        let body_span = self.peek().span.clone();
        let stmts = self.statements(Some("}"));
        let closed = self.expect("}").is_some();
        self.nesting = saved;
        let body = Block { id: self.next_id(), stmts, span: body_span };
        if !closed {
            return None;
        }
        Some(self.node(ExprKind::Lambda { params, body }, span))
    }

    /// A `{` on the same line after a call: a trailing lambda argument or a UI child block.
    fn trailing(&mut self, args: &mut Vec<Expr>) -> Option<Block> {
        if !(self.is("{") && self.same_line()) {
            return None;
        }
        if self.looks_like_lambda() {
            if let Some(lambda) = self.lambda() {
                args.push(lambda);
            }
            return None;
        }
        Some(self.block())
    }

    fn looks_like_builder(&self) -> bool {
        self.peek_at(1).kind == TokenKind::Identifier && self.peek_at(2).text == "="
    }

    fn builder_fields(&mut self) -> Option<Vec<FieldInit>> {
        let saved = std::mem::replace(&mut self.nesting, 0);
        self.advance(); // `{`
        let mut fields = Vec::new();
        loop {
            while self.consume(";") {}
            if self.is("}") || self.at_end() {
                break;
            }
            let start = self.index;
            match self.field_init() {
                Some(field) => {
                    fields.push(field);
                    self.end_of_item(Some("}"));
                }
                None => self.recover(),
            }
            if self.index == start {
                self.advance();
            }
        }
        let closed = self.expect("}").is_some();
        self.nesting = saved;
        closed.then_some(fields)
    }

    fn field_init(&mut self) -> Option<FieldInit> {
        let span = self.peek().span.clone();
        let name = self.identifier()?;
        self.expect("=")?;
        let value = self.expression()?;
        Some(FieldInit { name, value, span })
    }

    fn branch(&mut self) -> Option<Expr> {
        if self.is("{") {
            let span = self.peek().span.clone();
            let block = self.block();
            return Some(self.node(ExprKind::Block(block), span));
        }
        self.expression()
    }

    fn parenthesized(&mut self) -> Option<Expr> {
        self.expect("(")?;
        self.nesting += 1;
        let inner = self.expression();
        self.nesting -= 1;
        let inner = inner?;
        self.expect(")")?;
        Some(inner)
    }

    fn if_expression(&mut self) -> Option<Expr> {
        let span = self.advance().span;
        let condition = self.parenthesized()?;
        let then_branch = self.branch()?;
        if !self.consume("else") {
            let token = self.peek().clone();
            self.error("E7005", "`if` is an expression in Bind and requires an `else` branch.", token.span);
            return None;
        }
        let else_branch = self.branch()?;
        Some(self.node(
            ExprKind::If {
                condition: Box::new(condition),
                then_branch: Box::new(then_branch),
                else_branch: Box::new(else_branch),
            },
            span,
        ))
    }

    fn when_expression(&mut self) -> Option<Expr> {
        let span = self.advance().span;
        if !self.is("(") {
            self.error("E7004", "`when` without a subject is not supported; write `when (value) { ... }`.", span);
            return None;
        }
        let scrutinee = self.parenthesized()?;
        let saved = std::mem::replace(&mut self.nesting, 0);
        if self.expect("{").is_none() {
            self.nesting = saved;
            return None;
        }
        let mut arms = Vec::new();
        loop {
            while self.consume(";") {}
            if self.is("}") || self.at_end() {
                break;
            }
            let start = self.index;
            match self.arm() {
                Some(arm) => {
                    arms.push(arm);
                    self.end_of_item(Some("}"));
                }
                None => self.recover(),
            }
            if self.index == start {
                self.advance();
            }
        }
        let closed = self.expect("}").is_some();
        self.nesting = saved;
        if !closed {
            return None;
        }
        Some(self.node(ExprKind::When { scrutinee: Box::new(scrutinee), arms }, span))
    }

    fn arm(&mut self) -> Option<Arm> {
        let span = self.peek().span.clone();
        let pattern = self.pattern()?;
        self.expect("->")?;
        let body = self.branch()?;
        Some(Arm { pattern, body, span })
    }

    fn pattern(&mut self) -> Option<Pattern> {
        if self.is("-") && self.peek_at(1).kind == TokenKind::Number {
            self.advance();
            let number = self.advance();
            return Some(self.number_pattern(&number, "-"));
        }
        let token = self.peek().clone();
        match token.kind {
            TokenKind::Number => {
                self.advance();
                Some(self.number_pattern(&token, ""))
            }
            TokenKind::String => {
                self.advance();
                if has_interpolation(&token.text) {
                    self.removed("interpolation", token.span.clone());
                }
                Some(Pattern::Str(token.text, token.span))
            }
            TokenKind::Identifier => {
                self.advance();
                match token.text.as_str() {
                    "_" | "else" => Some(Pattern::Wildcard),
                    "null" => Some(Pattern::Null(token.span)),
                    "true" | "false" => Some(Pattern::Bool(token.text == "true", token.span)),
                    _ => {
                        let mut name = token.text.clone();
                        while self.is(".") && self.peek_at(1).kind == TokenKind::Identifier {
                            self.advance();
                            name = self.advance().text;
                        }
                        if !(self.is("(") && self.same_line()) {
                            return Some(Pattern::Name(name, token.span));
                        }
                        self.advance();
                        let mut args = Vec::new();
                        while !self.is(")") && !self.at_end() {
                            args.push(self.pattern()?);
                            if !self.consume(",") {
                                break;
                            }
                        }
                        self.expect(")")?;
                        Some(Pattern::Constructor { name, args, span: token.span })
                    }
                }
            }
            _ => {
                self.error("E7005", format!("Expected a pattern, found {}.", describe(&token)), token.span);
                None
            }
        }
    }

    fn number_pattern(&mut self, token: &Token, sign: &str) -> Pattern {
        match classify_number(&token.text) {
            Ok(NumberLiteral::Int(text)) => Pattern::Int(format!("{sign}{text}"), token.span.clone()),
            Ok(NumberLiteral::Double(text) | NumberLiteral::Float(text)) => {
                Pattern::Double(format!("{sign}{text}"), token.span.clone())
            }
            Err(message) => {
                self.error("E7004", message, token.span.clone());
                Pattern::Wildcard
            }
        }
    }
```

3. Substitua `postfix` inteiro por:

```rust
    fn postfix(&mut self) -> Option<Expr> {
        let mut expr = self.primary()?;
        loop {
            if self.is("(") && self.same_line() {
                let mut args = self.arguments()?;
                let children = self.trailing(&mut args);
                let span = expr.span.clone();
                expr = self.node(ExprKind::Call { callee: Box::new(expr), args, children }, span);
            } else if self.is(".") {
                self.advance();
                let name = self.identifier()?;
                let span = expr.span.clone();
                expr = self.node(ExprKind::Field { target: Box::new(expr), name }, span.clone());
                if self.is("{") && self.same_line() && self.looks_like_builder() {
                    let fields = self.builder_fields()?;
                    expr = self.node(ExprKind::Builder { target: Box::new(expr), fields }, span);
                }
            } else if self.adjacent_pair("?", ".") {
                self.advance();
                self.advance();
                let name = self.identifier()?;
                let span = expr.span.clone();
                expr = self.node(ExprKind::SafeField { target: Box::new(expr), name }, span);
            } else if self.adjacent_pair("!", "!") && self.same_line() {
                let span = self.advance().span;
                self.advance();
                self.removed("!!", span);
            } else if self.is("{") && self.same_line() && matches!(expr.kind, ExprKind::Name(_)) {
                let mut args = Vec::new();
                let children = self.trailing(&mut args);
                let span = expr.span.clone();
                expr = self.node(ExprKind::Call { callee: Box::new(expr), args, children }, span);
            } else {
                break;
            }
        }
        Some(expr)
    }
```

4. Em `primary`, dentro do braço `TokenKind::Identifier`, adicione antes de `"return"`:

```rust
                "if" => self.if_expression(),
                "when" => self.when_expression(),
```
e substitua o braço genérico `TokenKind::Symbol => { … }` por:

```rust
            TokenKind::Symbol if token.text == "{" && self.looks_like_lambda() => self.lambda(),
            TokenKind::Symbol if token.text == "{" => {
                self.error(
                    "E7004",
                    "A block cannot be used as a value here; write a lambda as `{ x -> ... }`.",
                    token.span,
                );
                None
            }
            TokenKind::Symbol => {
                self.error("E7005", format!("Expected an expression, found `{}`.", token.text), token.span);
                None
            }
```

- [ ] **Step 4: Ligar ao parser de declarações**

Em `src/ast.rs`, adicione a `Function` (depois de `body`):

```rust
    /// Parsed body; `None` for functions without a body.
    pub expr: Option<crate::syntax::expr::Block>,
```

Em `src/parser.rs::function`, logo após calcular `(body_kind, body)`:

```rust
        let expr = match body_kind {
            BodyKind::Block => {
                let (block, mut diagnostics) = expr::parse_block_tokens(&body, &span);
                self.diagnostics.append(&mut diagnostics);
                Some(block)
            }
            BodyKind::Expression => {
                let (block, mut diagnostics) = expr::parse_expression_tokens(&body, &span);
                self.diagnostics.append(&mut diagnostics);
                Some(block)
            }
            BodyKind::Absent => None,
        };
```
e adicione `expr,` ao literal `Function` (depois de `body,`).

- [ ] **Step 5: Rodar os testes e a regressão**

Run: `cargo test 2>&1 | tail -5`
Expected: PASS (todos, incluindo os 24 originais: nenhum corpo existente usa construções removidas).

Run:
```bash
DATABASE_URL=postgresql://bind:bind@localhost/bind cargo run --quiet -- check --target aws-lambda
make -C ../website check
```
Expected: ambos terminam com código 0 (o corpo do site, 262 linhas de UI, parseia sem erro).

Run: `cargo clippy --all-targets -- -D warnings -A dead_code 2>&1 | tail -3`
Expected: sem warnings.

- [ ] **Step 6: Commit**

```bash
git add src/parser/expr.rs src/parser.rs src/ast.rs
git commit -m "feat(parser): blocos, lambdas, if/when, padrões e corpos parseados

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Tipos, tabela union-find e unificação

**Files:**
- Create: `src/types/mod.rs` (provisório, só declara os submódulos desta tarefa), `src/types/ty.rs`, `src/types/unify.rs`
- Modify: `src/main.rs` (`mod types;`)

**Interfaces:**
- Produces (`types::ty`): `TypeVarId = usize`; `enum Type { Var(TypeVarId), Con(String, Vec<Type>), Fun(Vec<Type>, Box<Type>), Error }` com `con`, `option`, `list`, `contains_error`; `enum Constraint { Eq, Ord, Add, Sub, Num }` (ordem = força crescente) com `meet`, `accepts`, `describe`; `enum VarState { Unbound { level: u32, constraint: Option<Constraint> }, Link(Type) }`; `struct TypeTable` com `fresh`, `state`, `set`, `shallow`, `resolve`; `struct Scheme { vars: Vec<TypeVarId>, ty: Type }` com `mono`; funções `free_vars`, `generalize`, `instantiate`, `substitute`, `display`, `display_all`.
- Produces (`types::unify`): `enum UnifyError { Mismatch { expected, found }, Infinite { var, ty }, Constraint { constraint, ty } }`; `unify(&mut TypeTable, expected: &Type, found: &Type) -> Result<(), UnifyError>`; `constrain(&mut TypeTable, Constraint, &Type) -> Result<(), UnifyError>`.
- Convenção: tipos rígidos (parâmetros de tipo dentro do corpo de uma função anotada) são `Type::Con("'T", [])`. Nenhuma restrição os aceita.

- [ ] **Step 1: Criar `src/types/mod.rs` provisório**

```rust
//! Static type system: Hindley-Milner inference, algebraic data types and exhaustive matching.

pub mod ty;
pub mod unify;
```
Adicione `mod types;` em `src/main.rs` (depois de `mod syntax;`).

- [ ] **Step 2: Escrever os testes que falham**

Crie `src/types/unify.rs` só com os testes:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ty::*;

    fn int() -> Type {
        Type::con("Int")
    }

    #[test]
    fn binds_variables_and_resolves() {
        let mut table = TypeTable::default();
        let a = table.fresh(1);
        unify(&mut table, &Type::list(a.clone()), &Type::list(int())).unwrap();
        assert_eq!(table.resolve(&a), int());
    }

    #[test]
    fn reports_mismatch_with_enclosing_types() {
        let mut table = TypeTable::default();
        let error = unify(&mut table, &Type::list(int()), &Type::list(Type::con("String"))).unwrap_err();
        assert_eq!(
            error,
            UnifyError::Mismatch { expected: Type::list(int()), found: Type::list(Type::con("String")) }
        );
    }

    #[test]
    fn occurs_check_rejects_infinite_types() {
        let mut table = TypeTable::default();
        let a = table.fresh(1);
        let error = unify(&mut table, &a, &Type::list(a.clone())).unwrap_err();
        assert!(matches!(error, UnifyError::Infinite { .. }));
    }

    #[test]
    fn unification_lowers_levels() {
        let mut table = TypeTable::default();
        let outer = table.fresh(1);
        let inner = table.fresh(3);
        unify(&mut table, &outer, &Type::list(inner.clone())).unwrap();
        let Type::Var(id) = inner else { unreachable!() };
        assert_eq!(table.state(id), &VarState::Unbound { level: 1, constraint: None });
    }

    #[test]
    fn constraints_accept_and_reject_types() {
        let mut table = TypeTable::default();
        let a = table.fresh(1);
        constrain(&mut table, Constraint::Num, &a).unwrap();
        assert!(unify(&mut table, &a, &Type::con("String")).is_err());
        let b = table.fresh(1);
        constrain(&mut table, Constraint::Add, &b).unwrap();
        unify(&mut table, &b, &Type::con("String")).unwrap();
        assert!(constrain(&mut table, Constraint::Sub, &Type::con("String")).is_err());
        assert!(constrain(&mut table, Constraint::Ord, &Type::con("Instant")).is_ok());
    }

    #[test]
    fn merging_variables_keeps_the_stronger_constraint() {
        let mut table = TypeTable::default();
        let a = table.fresh(1);
        let b = table.fresh(1);
        constrain(&mut table, Constraint::Add, &a).unwrap();
        constrain(&mut table, Constraint::Num, &b).unwrap();
        unify(&mut table, &a, &b).unwrap();
        assert!(unify(&mut table, &a, &Type::con("String")).is_err());
        unify(&mut table, &a, &Type::con("Double")).unwrap();
    }

    #[test]
    fn equality_rejects_functions_and_rigid_types() {
        let mut table = TypeTable::default();
        let function = Type::Fun(vec![int()], Box::new(int()));
        assert!(constrain(&mut table, Constraint::Eq, &function).is_err());
        assert!(constrain(&mut table, Constraint::Eq, &Type::list(function)).is_err());
        assert!(constrain(&mut table, Constraint::Eq, &Type::con("'T")).is_err());
        let a = table.fresh(1);
        constrain(&mut table, Constraint::Eq, &Type::list(a.clone())).unwrap();
        assert!(unify(&mut table, &a, &Type::Fun(vec![], Box::new(int()))).is_err());
    }

    #[test]
    fn error_type_unifies_with_everything() {
        let mut table = TypeTable::default();
        unify(&mut table, &Type::Error, &int()).unwrap();
        constrain(&mut table, Constraint::Num, &Type::Error).unwrap();
    }

    #[test]
    fn generalizes_and_instantiates_by_level() {
        let mut table = TypeTable::default();
        let a = table.fresh(2);
        let ty = Type::Fun(vec![a.clone()], Box::new(a));
        let (scheme, ambiguous) = generalize(&table, 1, &ty);
        assert!(ambiguous.is_empty());
        assert_eq!(scheme.vars.len(), 1);
        let first = instantiate(&mut table, &scheme, 1);
        let second = instantiate(&mut table, &scheme, 1);
        assert_ne!(first, second);
        assert_eq!(display(&first), "('a) -> 'a");
        let c = table.fresh(2);
        constrain(&mut table, Constraint::Num, &c).unwrap();
        let (_, ambiguous) = generalize(&table, 1, &c);
        assert_eq!(ambiguous.len(), 1);
    }

    #[test]
    fn renders_with_normalized_names() {
        let ty = Type::Fun(vec![Type::Var(7), Type::Var(3)], Box::new(Type::Var(7)));
        assert_eq!(display(&ty), "('a, 'b) -> 'a");
        assert_eq!(display(&Type::option(Type::option(int()))), "Int??");
        assert_eq!(display(&Type::option(Type::Fun(vec![], Box::new(int())))), "(() -> Int)?");
        let names = display_all(&[&Type::Var(4), &Type::list(Type::Var(4))]);
        assert_eq!(names, vec!["'a".to_string(), "List<'a>".to_string()]);
    }
}
```

- [ ] **Step 3: Rodar e confirmar a falha**

Run: `cargo test types:: 2>&1 | tail -5`
Expected: FAIL de compilação (`ty` vazio, `unify` não existe).

- [ ] **Step 4: Implementar `src/types/ty.rs`**

```rust
//! Type representation, union-find variables with levels, schemes and rendering.

use std::collections::BTreeMap;

pub type TypeVarId = usize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type {
    Var(TypeVarId),
    Con(String, Vec<Type>),
    Fun(Vec<Type>, Box<Type>),
    /// Produced after a reported error; unifies with everything to avoid cascades.
    Error,
}

impl Type {
    pub fn con(name: &str) -> Type {
        Type::Con(name.to_string(), Vec::new())
    }

    pub fn option(inner: Type) -> Type {
        Type::Con("Option".into(), vec![inner])
    }

    pub fn list(inner: Type) -> Type {
        Type::Con("List".into(), vec![inner])
    }

    pub fn contains_error(&self) -> bool {
        match self {
            Type::Error => true,
            Type::Var(_) => false,
            Type::Con(_, args) => args.iter().any(Type::contains_error),
            Type::Fun(params, result) => params.iter().any(Type::contains_error) || result.contains_error(),
        }
    }
}

const NUMERIC: &[&str] = &["Int", "Double", "Float", "Decimal"];

/// Operator constraints over closed sets of types, declared from weakest to strongest:
/// every type a stronger constraint accepts is accepted by the weaker ones, so two
/// constraints on the same variable combine by taking the stronger (`max`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Constraint {
    Eq,
    Ord,
    Add,
    Sub,
    Num,
}

impl Constraint {
    pub fn meet(self, other: Constraint) -> Constraint {
        self.max(other)
    }

    /// Whether a type constructor (ignoring its arguments) satisfies the constraint.
    pub fn accepts(self, name: &str) -> bool {
        match self {
            Constraint::Num => NUMERIC.contains(&name),
            Constraint::Sub => Constraint::Num.accepts(name) || matches!(name, "Money" | "Duration"),
            Constraint::Add => Constraint::Sub.accepts(name) || name == "String",
            Constraint::Ord => {
                Constraint::Add.accepts(name) || matches!(name, "Date" | "DateTime" | "Instant")
            }
            Constraint::Eq => !name.starts_with('\''),
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Constraint::Num => "`*` and `/` (accepted types: Int, Double, Float, Decimal)",
            Constraint::Sub => "`-` (accepted types: Int, Double, Float, Decimal, Money, Duration)",
            Constraint::Add => "`+` (accepted types: Int, Double, Float, Decimal, Money, Duration, String)",
            Constraint::Ord => {
                "ordering comparisons (accepted types: Int, Double, Float, Decimal, Money, Duration, String, Date, DateTime, Instant)"
            }
            Constraint::Eq => "equality (functions and type parameters cannot be compared)",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VarState {
    Unbound { level: u32, constraint: Option<Constraint> },
    Link(Type),
}

#[derive(Clone, Debug, Default)]
pub struct TypeTable {
    vars: Vec<VarState>,
}

impl TypeTable {
    pub fn fresh(&mut self, level: u32) -> Type {
        self.vars.push(VarState::Unbound { level, constraint: None });
        Type::Var(self.vars.len() - 1)
    }

    pub fn state(&self, id: TypeVarId) -> &VarState {
        &self.vars[id]
    }

    pub fn set(&mut self, id: TypeVarId, state: VarState) {
        self.vars[id] = state;
    }

    /// Follows links at the head of `ty` only.
    pub fn shallow(&self, ty: &Type) -> Type {
        let mut current = ty.clone();
        while let Type::Var(id) = current
            && let VarState::Link(next) = &self.vars[id]
        {
            current = next.clone();
        }
        current
    }

    /// Follows every link.
    pub fn resolve(&self, ty: &Type) -> Type {
        match self.shallow(ty) {
            Type::Con(name, args) => Type::Con(name, args.iter().map(|arg| self.resolve(arg)).collect()),
            Type::Fun(params, result) => Type::Fun(
                params.iter().map(|param| self.resolve(param)).collect(),
                Box::new(self.resolve(&result)),
            ),
            other => other,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Scheme {
    pub vars: Vec<TypeVarId>,
    pub ty: Type,
}

impl Scheme {
    pub fn mono(ty: Type) -> Scheme {
        Scheme { vars: Vec::new(), ty }
    }
}

/// Unbound variables of `ty`, in order of first appearance.
pub fn free_vars(table: &TypeTable, ty: &Type) -> Vec<TypeVarId> {
    let mut output = Vec::new();
    collect_vars(&table.resolve(ty), &mut output);
    output
}

fn collect_vars(ty: &Type, output: &mut Vec<TypeVarId>) {
    match ty {
        Type::Var(id) => {
            if !output.contains(id) {
                output.push(*id);
            }
        }
        Type::Con(_, args) => args.iter().for_each(|arg| collect_vars(arg, output)),
        Type::Fun(params, result) => {
            params.iter().for_each(|param| collect_vars(param, output));
            collect_vars(result, output);
        }
        Type::Error => {}
    }
}

/// Quantifies the variables created deeper than `level`. Constrained variables are never
/// quantified: they are returned as ambiguous so the caller reports E7302.
pub fn generalize(table: &TypeTable, level: u32, ty: &Type) -> (Scheme, Vec<(TypeVarId, Constraint)>) {
    let resolved = table.resolve(ty);
    let mut vars = Vec::new();
    let mut ambiguous = Vec::new();
    for id in free_vars(table, &resolved) {
        if let VarState::Unbound { level: var_level, constraint } = table.state(id)
            && *var_level > level
        {
            match constraint {
                Some(constraint) => ambiguous.push((id, *constraint)),
                None => vars.push(id),
            }
        }
    }
    (Scheme { vars, ty: resolved }, ambiguous)
}

pub fn instantiate(table: &mut TypeTable, scheme: &Scheme, level: u32) -> Type {
    if scheme.vars.is_empty() {
        return scheme.ty.clone();
    }
    let mapping: BTreeMap<TypeVarId, Type> =
        scheme.vars.iter().map(|id| (*id, table.fresh(level))).collect();
    substitute(&scheme.ty, &mapping)
}

pub fn substitute(ty: &Type, mapping: &BTreeMap<TypeVarId, Type>) -> Type {
    match ty {
        Type::Var(id) => mapping.get(id).cloned().unwrap_or(Type::Var(*id)),
        Type::Con(name, args) => Type::Con(name.clone(), args.iter().map(|arg| substitute(arg, mapping)).collect()),
        Type::Fun(params, result) => Type::Fun(
            params.iter().map(|param| substitute(param, mapping)).collect(),
            Box::new(substitute(result, mapping)),
        ),
        Type::Error => Type::Error,
    }
}

/// Renders a resolved type; variables become `'a`, `'b`... in order of appearance.
pub fn display(ty: &Type) -> String {
    display_all(&[ty]).remove(0)
}

/// Renders several types with shared variable names (for "expected X, found Y").
pub fn display_all(types: &[&Type]) -> Vec<String> {
    let mut names = BTreeMap::new();
    types.iter().map(|ty| render(ty, &mut names)).collect()
}

fn render(ty: &Type, names: &mut BTreeMap<TypeVarId, String>) -> String {
    match ty {
        Type::Var(id) => {
            let next = names.len();
            names.entry(*id).or_insert_with(|| var_name(next)).clone()
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
        Type::Fun(params, result) => {
            let rendered: Vec<String> = params.iter().map(|param| render(param, names)).collect();
            let result = render(result, names);
            format!("({}) -> {result}", rendered.join(", "))
        }
        Type::Error => "<error>".into(),
    }
}

fn var_name(index: usize) -> String {
    let letter = char::from(b'a' + (index % 26) as u8);
    if index < 26 { format!("'{letter}") } else { format!("'{letter}{}", index / 26) }
}
```

- [ ] **Step 5: Implementar `src/types/unify.rs` (antes dos testes)**

```rust
//! Unification with occurs check, level adjustment and constraint propagation.

use super::ty::{Constraint, Type, TypeTable, TypeVarId, VarState};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnifyError {
    Mismatch { expected: Type, found: Type },
    Infinite { var: TypeVarId, ty: Type },
    Constraint { constraint: Constraint, ty: Type },
}

pub fn unify(table: &mut TypeTable, expected: &Type, found: &Type) -> Result<(), UnifyError> {
    let a = table.shallow(expected);
    let b = table.shallow(found);
    match (&a, &b) {
        (Type::Error, _) | (_, Type::Error) => Ok(()),
        (Type::Var(x), Type::Var(y)) if x == y => Ok(()),
        (Type::Var(x), _) => bind(table, *x, &b),
        (_, Type::Var(y)) => bind(table, *y, &a),
        (Type::Con(n1, a1), Type::Con(n2, a2)) if n1 == n2 && a1.len() == a2.len() => {
            for (x, y) in a1.iter().zip(a2) {
                unify(table, x, y).map_err(|error| widen(table, error, &a, &b))?;
            }
            Ok(())
        }
        (Type::Fun(p1, r1), Type::Fun(p2, r2)) if p1.len() == p2.len() => {
            for (x, y) in p1.iter().zip(p2) {
                unify(table, x, y).map_err(|error| widen(table, error, &a, &b))?;
            }
            unify(table, r1, r2).map_err(|error| widen(table, error, &a, &b))
        }
        _ => Err(UnifyError::Mismatch { expected: table.resolve(&a), found: table.resolve(&b) }),
    }
}

/// A nested mismatch is reported with the enclosing types, which is what the programmer wrote.
fn widen(table: &TypeTable, error: UnifyError, expected: &Type, found: &Type) -> UnifyError {
    match error {
        UnifyError::Mismatch { .. } => {
            UnifyError::Mismatch { expected: table.resolve(expected), found: table.resolve(found) }
        }
        other => other,
    }
}

fn bind(table: &mut TypeTable, var: TypeVarId, ty: &Type) -> Result<(), UnifyError> {
    let VarState::Unbound { level, constraint } = table.state(var).clone() else {
        unreachable!("`shallow` never returns a linked variable")
    };
    if let Type::Var(other) = ty {
        let VarState::Unbound { level: other_level, constraint: other_constraint } = table.state(*other).clone()
        else {
            unreachable!("`shallow` never returns a linked variable")
        };
        let merged = match (constraint, other_constraint) {
            (Some(a), Some(b)) => Some(a.meet(b)),
            (a, b) => a.or(b),
        };
        table.set(*other, VarState::Unbound { level: level.min(other_level), constraint: merged });
        table.set(var, VarState::Link(ty.clone()));
        return Ok(());
    }
    occurs_and_adjust(table, var, level, ty).map_err(|_| UnifyError::Infinite { var, ty: table.resolve(ty) })?;
    // A rejected type links the variable to `Error`: the problem is reported once, and later
    // uses neither cascade nor surface as an ambiguous (still constrained) variable.
    if let Some(constraint) = constraint
        && let Err(error) = constrain(table, constraint, ty)
    {
        table.set(var, VarState::Link(Type::Error));
        return Err(error);
    }
    table.set(var, VarState::Link(ty.clone()));
    Ok(())
}

fn occurs_and_adjust(table: &mut TypeTable, var: TypeVarId, level: u32, ty: &Type) -> Result<(), ()> {
    match table.shallow(ty) {
        Type::Var(id) => {
            if id == var {
                return Err(());
            }
            if let VarState::Unbound { level: other_level, constraint } = table.state(id).clone()
                && other_level > level
            {
                table.set(id, VarState::Unbound { level, constraint });
            }
            Ok(())
        }
        Type::Con(_, args) => args.iter().try_for_each(|arg| occurs_and_adjust(table, var, level, arg)),
        Type::Fun(params, result) => {
            params.iter().try_for_each(|param| occurs_and_adjust(table, var, level, param))?;
            occurs_and_adjust(table, var, level, &result)
        }
        Type::Error => Ok(()),
    }
}

/// Requires `ty` to satisfy `constraint`; an unbound variable records it for later.
pub fn constrain(table: &mut TypeTable, constraint: Constraint, ty: &Type) -> Result<(), UnifyError> {
    let fail = |table: &TypeTable| UnifyError::Constraint { constraint, ty: table.resolve(ty) };
    match table.shallow(ty) {
        Type::Error => Ok(()),
        Type::Var(id) => {
            if let VarState::Unbound { level, constraint: existing } = table.state(id).clone() {
                let merged = existing.map_or(constraint, |existing| existing.meet(constraint));
                table.set(id, VarState::Unbound { level, constraint: Some(merged) });
            }
            Ok(())
        }
        Type::Con(name, args) => {
            if !constraint.accepts(&name) {
                return Err(fail(table));
            }
            if constraint == Constraint::Eq {
                for arg in &args {
                    constrain(table, Constraint::Eq, arg).map_err(|_| fail(table))?;
                }
                return Ok(());
            }
            if args.is_empty() { Ok(()) } else { Err(fail(table)) }
        }
        Type::Fun(..) => Err(fail(table)),
    }
}
```

Nota: se o clippy reclamar de `Result<(), ()>` (`result_unit_err` só vale para itens públicos; `occurs_and_adjust` é privado), mantenha.

- [ ] **Step 6: Rodar os testes**

Run: `cargo test types:: 2>&1 | tail -5`
Expected: PASS (10 testes).

Run: `cargo clippy --all-targets -- -D warnings -A dead_code 2>&1 | tail -3`
Expected: sem warnings.

- [ ] **Step 7: Commit**

```bash
git add src/types src/main.rs
git commit -m "feat(types): tipos, union-find com níveis e unificação

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Exaustividade e redundância (Maranget)

**Files:**
- Create: `src/types/exhaustive.rs`
- Modify: `src/types/mod.rs` (`pub mod exhaustive;`)

**Interfaces:**
- Consumes: `Type` (Task 4).
- Produces:
  - `enum Pat { Wild, Ctor(String, Vec<Pat>), Lit(String) }`. Construtores internos: `"true"`/`"false"` (Boolean), `"null"`/`"Some"` (Option). Literais de string guardam a forma com aspas (`"\"a\""`), inteiros a forma decimal.
  - `struct CtorSig { pub name: String, pub args: Vec<Type> }` com `CtorSig::nullary(name)`
  - `trait Signatures { fn constructors(&self, ty: &Type) -> Option<Vec<CtorSig>>; }` (`None` = infinitos valores)
  - `struct MatchReport { pub missing: Option<Pat>, pub redundant: Vec<usize> }`
  - `check_match(sig: &dyn Signatures, ty: &Type, arms: &[Pat]) -> MatchReport`
  - `render(pat: &Pat) -> String` (`Some(p)` aparece como `p`; `Some(_)` como `<non-null value>`)

Referência: L. Maranget, *Warnings for pattern matching*, JFP 2007. `useful(P, q)` decide se existe um valor casado por `q` e por nenhuma linha de `P`, e devolve um testemunho.

- [ ] **Step 1: Escrever os testes que falham**

Crie `src/types/exhaustive.rs` com os testes:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;

    impl Signatures for Fake {
        fn constructors(&self, ty: &Type) -> Option<Vec<CtorSig>> {
            let Type::Con(name, args) = ty else { return None };
            let boolean = || Type::con("Boolean");
            match name.as_str() {
                "Boolean" => Some(vec![CtorSig::nullary("true"), CtorSig::nullary("false")]),
                "Option" => Some(vec![
                    CtorSig::nullary("null"),
                    CtorSig { name: "Some".into(), args: args.clone() },
                ]),
                "Pedido" => Some(vec![
                    CtorSig { name: "Pendente".into(), args: vec![Type::con("Int")] },
                    CtorSig { name: "Pago".into(), args: vec![boolean()] },
                    CtorSig::nullary("Cancelado"),
                ]),
                "Par" => Some(vec![CtorSig { name: "Par".into(), args: vec![boolean(), boolean()] }]),
                _ => None,
            }
        }
    }

    fn ctor(name: &str, args: Vec<Pat>) -> Pat {
        Pat::Ctor(name.into(), args)
    }

    fn pedido() -> Type {
        Type::con("Pedido")
    }

    #[test]
    fn complete_match_has_no_missing_case() {
        let arms = [ctor("Pendente", vec![Pat::Wild]), ctor("Pago", vec![Pat::Wild]), ctor("Cancelado", vec![])];
        let report = check_match(&Fake, &pedido(), &arms);
        assert_eq!(report.missing, None);
        assert!(report.redundant.is_empty());
    }

    #[test]
    fn reports_a_missing_constructor() {
        let arms = [ctor("Pendente", vec![Pat::Wild]), ctor("Cancelado", vec![])];
        let report = check_match(&Fake, &pedido(), &arms);
        assert_eq!(report.missing.as_ref().map(render), Some("Pago(_)".into()));
    }

    #[test]
    fn reports_missing_nested_values() {
        let arms = [
            ctor("Pendente", vec![Pat::Wild]),
            ctor("Pago", vec![ctor("true", vec![])]),
            ctor("Cancelado", vec![]),
        ];
        let report = check_match(&Fake, &pedido(), &arms);
        assert_eq!(report.missing.as_ref().map(render), Some("Pago(false)".into()));
    }

    #[test]
    fn finds_the_classic_pair_witness() {
        let t = || ctor("true", vec![]);
        let arms = [ctor("Par", vec![t(), Pat::Wild]), ctor("Par", vec![Pat::Wild, t()])];
        let report = check_match(&Fake, &Type::con("Par"), &arms);
        assert_eq!(report.missing.as_ref().map(render), Some("Par(false, false)".into()));
    }

    #[test]
    fn reports_redundant_arms() {
        let arms = [Pat::Wild, ctor("Cancelado", vec![])];
        assert_eq!(check_match(&Fake, &pedido(), &arms).redundant, vec![1]);
        let arms = [ctor("Cancelado", vec![]), ctor("Cancelado", vec![]), Pat::Wild];
        assert_eq!(check_match(&Fake, &pedido(), &arms).redundant, vec![1]);
    }

    #[test]
    fn options_need_null_and_a_value() {
        let option = Type::option(Type::con("Int"));
        let report = check_match(&Fake, &option, &[ctor("Some", vec![Pat::Wild])]);
        assert_eq!(report.missing.as_ref().map(render), Some("null".into()));
        let report = check_match(&Fake, &option, &[ctor("null", vec![])]);
        assert_eq!(report.missing.as_ref().map(render), Some("<non-null value>".into()));
        let report = check_match(&Fake, &option, &[ctor("null", vec![]), ctor("Some", vec![Pat::Wild])]);
        assert_eq!(report.missing, None);
    }

    #[test]
    fn literals_of_infinite_types_need_a_wildcard() {
        let int = Type::con("Int");
        let arms = [Pat::Lit("0".into()), Pat::Lit("1".into())];
        let report = check_match(&Fake, &int, &arms);
        assert_eq!(report.missing.as_ref().map(render), Some("_".into()));
        let arms = [Pat::Lit("0".into()), Pat::Lit("0".into()), Pat::Wild];
        let report = check_match(&Fake, &int, &arms);
        assert_eq!(report.missing, None);
        assert_eq!(report.redundant, vec![1]);
    }
}
```

- [ ] **Step 2: Rodar e confirmar a falha**

Run: `cargo test types::exhaustive 2>&1 | tail -5`
Expected: FAIL de compilação. (Lembre de adicionar `pub mod exhaustive;` em `src/types/mod.rs`.)

- [ ] **Step 3: Implementar (antes dos testes)**

```rust
//! Exhaustiveness and redundancy of `when`, using Maranget's usefulness algorithm
//! ("Warnings for pattern matching", Journal of Functional Programming, 2007).

use super::ty::Type;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pat {
    Wild,
    Ctor(String, Vec<Pat>),
    Lit(String),
}

#[derive(Clone, Debug)]
pub struct CtorSig {
    pub name: String,
    pub args: Vec<Type>,
}

impl CtorSig {
    pub fn nullary(name: &str) -> CtorSig {
        CtorSig { name: name.into(), args: Vec::new() }
    }
}

pub trait Signatures {
    /// Every constructor of `ty`, or `None` when the type has infinitely many values.
    fn constructors(&self, ty: &Type) -> Option<Vec<CtorSig>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchReport {
    /// A value no arm matches, if any.
    pub missing: Option<Pat>,
    /// Indices of arms that can never be reached.
    pub redundant: Vec<usize>,
}

pub fn check_match(sig: &dyn Signatures, ty: &Type, arms: &[Pat]) -> MatchReport {
    let types = std::slice::from_ref(ty);
    let mut rows: Vec<Vec<Pat>> = Vec::new();
    let mut redundant = Vec::new();
    for (index, arm) in arms.iter().enumerate() {
        if useful(sig, &rows, std::slice::from_ref(arm), types).is_none() {
            redundant.push(index);
        }
        rows.push(vec![arm.clone()]);
    }
    let missing = useful(sig, &rows, &[Pat::Wild], types).map(|mut witness| witness.remove(0));
    MatchReport { missing, redundant }
}

pub fn render(pat: &Pat) -> String {
    match pat {
        Pat::Wild => "_".into(),
        Pat::Lit(text) => text.clone(),
        Pat::Ctor(name, args) if name == "Some" => match args.first() {
            Some(Pat::Wild) | None => "<non-null value>".into(),
            Some(inner) => render(inner),
        },
        Pat::Ctor(name, args) if args.is_empty() => name.clone(),
        Pat::Ctor(name, args) => {
            let rendered: Vec<String> = args.iter().map(render).collect();
            format!("{name}({})", rendered.join(", "))
        }
    }
}

/// Returns a witness vector matched by `vector` and by no row of `rows`, or `None`.
fn useful(sig: &dyn Signatures, rows: &[Vec<Pat>], vector: &[Pat], types: &[Type]) -> Option<Vec<Pat>> {
    let Some((head, rest)) = vector.split_first() else {
        return rows.is_empty().then(Vec::new);
    };
    let head_type = &types[0];
    let rest_types = &types[1..];
    match head {
        Pat::Ctor(name, args) => {
            let arg_types = constructor_args(sig, head_type, name, args.len());
            let specialized = specialize(rows, name, args.len());
            let vector: Vec<Pat> = args.iter().chain(rest).cloned().collect();
            let types: Vec<Type> = arg_types.into_iter().chain(rest_types.iter().cloned()).collect();
            useful(sig, &specialized, &vector, &types).map(|witness| rebuild(name, args.len(), witness))
        }
        Pat::Lit(literal) => {
            let specialized = specialize_literal(rows, literal);
            useful(sig, &specialized, rest, rest_types).map(|mut witness| {
                witness.insert(0, Pat::Lit(literal.clone()));
                witness
            })
        }
        Pat::Wild => {
            let used = head_constructors(rows);
            let all = sig.constructors(head_type);
            if let Some(all) = &all
                && !all.is_empty()
                && all.iter().all(|ctor| used.contains(&ctor.name))
            {
                for ctor in all {
                    let specialized = specialize(rows, &ctor.name, ctor.args.len());
                    let vector: Vec<Pat> = std::iter::repeat_n(Pat::Wild, ctor.args.len()).chain(rest.iter().cloned()).collect();
                    let types: Vec<Type> = ctor.args.iter().chain(rest_types).cloned().collect();
                    if let Some(witness) = useful(sig, &specialized, &vector, &types) {
                        return Some(rebuild(&ctor.name, ctor.args.len(), witness));
                    }
                }
                return None;
            }
            let defaults = default_rows(rows);
            useful(sig, &defaults, rest, rest_types).map(|mut witness| {
                let head = all
                    .as_ref()
                    .and_then(|all| all.iter().find(|ctor| !used.contains(&ctor.name)))
                    .map_or(Pat::Wild, |ctor| Pat::Ctor(ctor.name.clone(), vec![Pat::Wild; ctor.args.len()]));
                witness.insert(0, head);
                witness
            })
        }
    }
}

fn constructor_args(sig: &dyn Signatures, ty: &Type, name: &str, arity: usize) -> Vec<Type> {
    sig.constructors(ty)
        .and_then(|all| all.into_iter().find(|ctor| ctor.name == name))
        .map_or_else(|| vec![Type::Error; arity], |ctor| ctor.args)
}

fn head_constructors(rows: &[Vec<Pat>]) -> Vec<String> {
    let mut names = Vec::new();
    for row in rows {
        if let Some(Pat::Ctor(name, _)) = row.first()
            && !names.contains(name)
        {
            names.push(name.clone());
        }
    }
    names
}

fn specialize(rows: &[Vec<Pat>], name: &str, arity: usize) -> Vec<Vec<Pat>> {
    rows.iter()
        .filter_map(|row| match row.split_first()? {
            (Pat::Ctor(other, args), rest) if other == name => Some(args.iter().chain(rest).cloned().collect()),
            (Pat::Wild, rest) => Some(std::iter::repeat_n(Pat::Wild, arity).chain(rest.iter().cloned()).collect()),
            _ => None,
        })
        .collect()
}

fn specialize_literal(rows: &[Vec<Pat>], literal: &str) -> Vec<Vec<Pat>> {
    rows.iter()
        .filter_map(|row| match row.split_first()? {
            (Pat::Lit(other), rest) if other == literal => Some(rest.to_vec()),
            (Pat::Wild, rest) => Some(rest.to_vec()),
            _ => None,
        })
        .collect()
}

fn default_rows(rows: &[Vec<Pat>]) -> Vec<Vec<Pat>> {
    rows.iter()
        .filter_map(|row| match row.split_first()? {
            (Pat::Wild, rest) => Some(rest.to_vec()),
            _ => None,
        })
        .collect()
}

fn rebuild(name: &str, arity: usize, mut witness: Vec<Pat>) -> Vec<Pat> {
    let rest = witness.split_off(arity.min(witness.len()));
    let mut output = vec![Pat::Ctor(name.into(), witness)];
    output.extend(rest);
    output
}
```

- [ ] **Step 4: Rodar os testes**

Run: `cargo test types::exhaustive 2>&1 | tail -5`
Expected: PASS (7 testes).

Run: `cargo clippy --all-targets -- -D warnings -A dead_code 2>&1 | tail -3`
Expected: sem warnings.

- [ ] **Step 5: Commit**

```bash
git add src/types/exhaustive.rs src/types/mod.rs
git commit -m "feat(types): exaustividade e redundância com o algoritmo de Maranget

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 6: Ambiente de tipos

**Files:**
- Create: `src/types/env.rs`
- Modify: `src/types/mod.rs` (`pub mod env;`)

**Interfaces:**
- Consumes: `parse_type_text` (Task 2), `Type`, `TypeTable` (Task 4), `Signatures`, `CtorSig` (Task 5), `TypeDecl.cases`/`type_params` (Task 1).
- Produces:
  - constantes `SCALARS`, `GENERICS`, `UI_NODES`
  - `struct TypeInfo { kind: TypeKind, params: Vec<String>, ctors: Vec<String>, underlying: Option<TypeExpr> }`
  - `struct CtorInfo { name, type_name, type_params: Vec<String>, fields: Vec<(String, TypeExpr)>, span }`
  - `struct Env { types: BTreeMap<String, TypeInfo>, ctors: BTreeMap<String, CtorInfo> }` com:
    - `Env::build(&Program, &mut Vec<Diagnostic>) -> Env` (emite E7002, E7103, E7102, E7110)
    - `type_arity(&str) -> Option<usize>`
    - `lower(&TypeExpr, vars: &BTreeMap<String, Type>, &mut Vec<Diagnostic>) -> Type` e `lower_quiet(&TypeExpr, vars) -> Type`
    - `instantiate_ctor(name, &mut TypeTable, level) -> (Vec<Type>, Type)` (tipos dos campos, tipo resultante)
    - `instantiate_record(type_name, &mut TypeTable, level) -> Type`
    - `field_type(type_name, args: &[Type], field) -> Option<Type>` (só `data class`/`entity`)
    - `records_with_field(field) -> Vec<String>` (ordem alfabética)
    - `is_displayable(&Type) -> bool`
  - `rigid_vars(params: &[String]) -> BTreeMap<String, Type>` (mapeia `T` para `Con("'T")`)
  - `impl Signatures for Env`
- `Result<T, E>` é registrado como tipo selado embutido com construtores `Ok(value: T)` e `Err(error: E)`. Um `distinct type X = U` ganha o construtor `X(value: U)`.

- [ ] **Step 1: Escrever os testes que falham**

Crie `src/types/env.rs` com os testes:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse;

    fn build(source: &str) -> (Env, Vec<&'static str>) {
        let (tokens, _) = lex("env.bd", source);
        let (program, parse_diagnostics) = parse(tokens);
        assert!(parse_diagnostics.is_empty(), "{parse_diagnostics:?}");
        let mut diagnostics = Vec::new();
        let env = Env::build(&program, &mut diagnostics);
        (env, diagnostics.iter().map(|item| item.code).collect())
    }

    const TREE: &str = "sealed class Arvore<T> {\n    data class No(val esq: Arvore<T>, val valor: T, val dir: Arvore<T>)\n    data object Folha\n}\ndata class Caixa<T>(val valor: T)\ndistinct type ClienteId = UUID\n";

    #[test]
    fn registers_constructors_of_user_and_builtin_types() {
        let (env, codes) = build(TREE);
        assert!(codes.is_empty(), "{codes:?}");
        assert_eq!(env.types["Arvore"].ctors, vec!["No", "Folha"]);
        assert_eq!(env.ctors["No"].fields.len(), 3);
        assert_eq!(env.types["Result"].ctors, vec!["Ok", "Err"]);
        assert_eq!(env.ctors["ClienteId"].fields[0].0, "value");
        assert_eq!(env.type_arity("Arvore"), Some(1));
        assert_eq!(env.type_arity("Map"), Some(2));
        assert_eq!(env.type_arity("Nada"), None);
    }

    #[test]
    fn instantiates_constructors_with_fresh_variables() {
        let (env, _) = build(TREE);
        let mut table = TypeTable::default();
        let (fields, result) = env.instantiate_ctor("No", &mut table, 1);
        let Type::Con(name, args) = &result else { unreachable!() };
        assert_eq!(name, "Arvore");
        assert_eq!(fields[1], args[0]);
        assert_eq!(fields[0], result);
    }

    #[test]
    fn substitutes_type_arguments_in_fields_and_signatures() {
        let (env, _) = build(TREE);
        let int = Type::con("Int");
        assert_eq!(env.field_type("Caixa", std::slice::from_ref(&int), "valor"), Some(int.clone()));
        assert_eq!(env.field_type("Caixa", std::slice::from_ref(&int), "outro"), None);
        assert_eq!(env.records_with_field("valor"), vec!["Caixa"]);
        let signatures = env.constructors(&Type::Con("Arvore".into(), vec![int.clone()])).unwrap();
        assert_eq!(signatures[0].args[1], int);
        assert!(env.constructors(&Type::con("Int")).is_none());
        assert_eq!(env.constructors(&Type::con("Boolean")).unwrap().len(), 2);
    }

    #[test]
    fn displayable_types() {
        let (env, _) = build(TREE);
        assert!(env.is_displayable(&Type::con("Email")));
        assert!(env.is_displayable(&Type::con("ClienteId")));
        assert!(!env.is_displayable(&Type::list(Type::con("String"))));
        assert!(!env.is_displayable(&Type::con("Caixa")));
    }

    #[test]
    fn reports_declaration_errors() {
        assert_eq!(build("data class C(var x: Int)").1, vec!["E7002"]);
        assert_eq!(build("data class C(val x: Nada)").1, vec!["E7103"]);
        assert_eq!(build("data class C(val x: List<Int, Int>)").1, vec!["E7102"]);
        assert_eq!(build("data class C(val x: Int)\ndata class C(val y: Int)").1, vec!["E7110"]);
        assert_eq!(build("data class String(val x: Int)").1, vec!["E7110"]);
        assert_eq!(build("enum class A { X }\nenum class B { X }").1, vec!["E7110"]);
        assert_eq!(build("fun f() = 1\nfun f() = 2").1, vec!["E7110"]);
    }
}
```

- [ ] **Step 2: Rodar e confirmar a falha**

Run: `cargo test types::env 2>&1 | tail -5`
Expected: FAIL de compilação.

- [ ] **Step 3: Implementar (antes dos testes)**

```rust
//! Type environment: declared types, constructors and the built-in vocabulary.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{Field, Program, TypeKind, TypeRef};
use crate::diagnostic::{Diagnostic, Span};
use crate::parser::expr::parse_type_text;
use crate::syntax::expr::TypeExpr;

use super::exhaustive::{CtorSig, Signatures};
use super::ty::{Type, TypeTable};

/// Built-in types without type arguments.
pub const SCALARS: &[&str] = &[
    "Unit", "String", "Int", "Long", "Float", "Double", "Decimal", "Boolean", "UUID", "Date",
    "DateTime", "Instant", "Duration", "Money", "Email", "Url", "DatabaseUrl", "Port", "Secret", "Ui",
];

/// Built-in generic types and their arity. `Result` is registered as a sealed type instead.
pub const GENERICS: &[(&str, usize)] = &[("List", 1), ("Set", 1), ("Map", 2), ("Option", 1)];

/// UI nodes accepted by the certified Elm generator in `certified.rs`, plus `UI` and `Text`.
pub const UI_NODES: &[&str] = &[
    "UI", "Page", "Container", "Div", "Grid", "Stack", "Header", "Nav", "Main", "Section",
    "Article", "Aside", "Footer", "Paragraph", "Span", "Strong", "Emphasis", "Small", "List",
    "Item", "Pre", "Code", "Heading", "Link", "Rule", "Text",
];

/// Types `Text(...)` can display, besides distinct types over them.
const DISPLAY_SCALARS: &[&str] = &[
    "String", "Int", "Long", "Double", "Float", "Decimal", "Boolean", "UUID", "Date", "DateTime",
    "Instant", "Duration", "Money", "Email", "Url",
];

/// Stands for a field type that failed to parse (already reported).
const ERROR_TYPE: &str = "<error>";

#[derive(Clone, Debug)]
pub struct TypeInfo {
    pub kind: TypeKind,
    pub params: Vec<String>,
    pub ctors: Vec<String>,
    pub underlying: Option<TypeExpr>,
}

#[derive(Clone, Debug)]
pub struct CtorInfo {
    pub name: String,
    pub type_name: String,
    pub type_params: Vec<String>,
    pub fields: Vec<(String, TypeExpr)>,
    pub span: Span,
}

#[derive(Clone, Debug, Default)]
pub struct Env {
    pub types: BTreeMap<String, TypeInfo>,
    pub ctors: BTreeMap<String, CtorInfo>,
}

impl Env {
    pub fn build(program: &Program, diagnostics: &mut Vec<Diagnostic>) -> Env {
        let mut env = Env::default();
        env.install_result();
        let mut functions = BTreeSet::new();
        for function in &program.functions {
            if !functions.insert(function.name.as_str()) {
                diagnostics.push(Diagnostic::error(
                    "E7110",
                    format!("Function `{}` is declared more than once.", function.name),
                    function.span.clone(),
                ));
            }
        }
        for declaration in &program.types {
            if env.type_arity(&declaration.name).is_some() {
                diagnostics.push(Diagnostic::error(
                    "E7110",
                    format!(
                        "Type `{}` is declared more than once or conflicts with a built-in type.",
                        declaration.name
                    ),
                    declaration.span.clone(),
                ));
                continue;
            }
            let constructors: Vec<(String, Vec<Field>, Span)> = match declaration.kind {
                TypeKind::Data | TypeKind::Entity => {
                    vec![(declaration.name.clone(), declaration.fields.clone(), declaration.span.clone())]
                }
                TypeKind::Sealed | TypeKind::Enum => declaration
                    .cases
                    .iter()
                    .map(|case| (case.name.clone(), case.fields.clone(), case.span.clone()))
                    .collect(),
                TypeKind::Distinct => vec![(
                    declaration.name.clone(),
                    vec![Field {
                        name: "value".into(),
                        type_ref: declaration.underlying.clone().unwrap_or_else(TypeRef::unit),
                        mutable: false,
                        span: declaration.span.clone(),
                    }],
                    declaration.span.clone(),
                )],
            };
            let mut names = Vec::new();
            for (name, fields, span) in constructors {
                if let Some(existing) = env.ctors.get(&name) {
                    diagnostics.push(Diagnostic::error(
                        "E7110",
                        format!(
                            "Constructor `{name}` is already declared by `{}` at {}:{}:{}; constructor names must be unique.",
                            existing.type_name, existing.span.file, existing.span.line, existing.span.column
                        ),
                        span,
                    ));
                    continue;
                }
                let mut lowered = Vec::new();
                for field in &fields {
                    if field.mutable {
                        diagnostics.push(
                            Diagnostic::error(
                                "E7002",
                                format!("Field `{}` is declared with `var`; Bind values are immutable.", field.name),
                                field.span.clone(),
                            )
                            .with_hint("Use `val`."),
                        );
                    }
                    lowered.push((field.name.clone(), parse_field_type(&field.type_ref, &field.span, diagnostics)));
                }
                names.push(name.clone());
                env.ctors.insert(
                    name.clone(),
                    CtorInfo {
                        name,
                        type_name: declaration.name.clone(),
                        type_params: declaration.type_params.clone(),
                        fields: lowered,
                        span,
                    },
                );
            }
            let underlying = declaration
                .underlying
                .as_ref()
                .map(|type_ref| parse_field_type(type_ref, &declaration.span, &mut Vec::new()));
            env.types.insert(
                declaration.name.clone(),
                TypeInfo {
                    kind: declaration.kind.clone(),
                    params: declaration.type_params.clone(),
                    ctors: names,
                    underlying,
                },
            );
        }
        // Field types can mention types declared later, so they are checked after registration.
        for ctor in env.ctors.values() {
            let rigid = rigid_vars(&ctor.type_params);
            for (_, field) in &ctor.fields {
                env.lower(field, &rigid, diagnostics);
            }
        }
        env
    }

    fn install_result(&mut self) {
        let param = |name: &str| TypeExpr::Named { name: name.into(), args: Vec::new(), span: Span::default() };
        for (ctor, field, ty) in [("Ok", "value", "T"), ("Err", "error", "E")] {
            self.ctors.insert(
                ctor.into(),
                CtorInfo {
                    name: ctor.into(),
                    type_name: "Result".into(),
                    type_params: vec!["T".into(), "E".into()],
                    fields: vec![(field.into(), param(ty))],
                    span: Span::default(),
                },
            );
        }
        self.types.insert(
            "Result".into(),
            TypeInfo {
                kind: TypeKind::Sealed,
                params: vec!["T".into(), "E".into()],
                ctors: vec!["Ok".into(), "Err".into()],
                underlying: None,
            },
        );
    }

    pub fn type_arity(&self, name: &str) -> Option<usize> {
        if SCALARS.contains(&name) {
            return Some(0);
        }
        if let Some((_, arity)) = GENERICS.iter().find(|(generic, _)| *generic == name) {
            return Some(*arity);
        }
        self.types.get(name).map(|info| info.params.len())
    }

    pub fn lower(&self, te: &TypeExpr, vars: &BTreeMap<String, Type>, diagnostics: &mut Vec<Diagnostic>) -> Type {
        match te {
            TypeExpr::Nullable(inner) => Type::option(self.lower(inner, vars, diagnostics)),
            TypeExpr::Function { params, result } => {
                let params = params.iter().map(|param| self.lower(param, vars, diagnostics)).collect();
                Type::Fun(params, Box::new(self.lower(result, vars, diagnostics)))
            }
            TypeExpr::Named { name, args, span } => {
                if name == ERROR_TYPE {
                    return Type::Error;
                }
                if let Some(var) = vars.get(name) {
                    if !args.is_empty() {
                        diagnostics.push(Diagnostic::error(
                            "E7102",
                            format!("Type parameter `{name}` does not take type arguments."),
                            span.clone(),
                        ));
                        return Type::Error;
                    }
                    return var.clone();
                }
                let Some(arity) = self.type_arity(name) else {
                    diagnostics.push(Diagnostic::error("E7103", format!("Unknown type `{name}`."), span.clone()));
                    return Type::Error;
                };
                if arity != args.len() {
                    diagnostics.push(Diagnostic::error(
                        "E7102",
                        format!("Type `{name}` expects {arity} type argument(s), found {}.", args.len()),
                        span.clone(),
                    ));
                    return Type::Error;
                }
                Type::Con(name.clone(), args.iter().map(|arg| self.lower(arg, vars, diagnostics)).collect())
            }
        }
    }

    pub fn lower_quiet(&self, te: &TypeExpr, vars: &BTreeMap<String, Type>) -> Type {
        self.lower(te, vars, &mut Vec::new())
    }

    pub fn instantiate_ctor(&self, name: &str, table: &mut TypeTable, level: u32) -> (Vec<Type>, Type) {
        let Some(ctor) = self.ctors.get(name) else {
            return (Vec::new(), Type::Error);
        };
        let mut vars = BTreeMap::new();
        let mut args = Vec::new();
        for param in &ctor.type_params {
            let var = table.fresh(level);
            vars.insert(param.clone(), var.clone());
            args.push(var);
        }
        let fields = ctor.fields.iter().map(|(_, field)| self.lower_quiet(field, &vars)).collect();
        (fields, Type::Con(ctor.type_name.clone(), args))
    }

    pub fn instantiate_record(&self, type_name: &str, table: &mut TypeTable, level: u32) -> Type {
        let arity = self.types.get(type_name).map_or(0, |info| info.params.len());
        Type::Con(type_name.into(), (0..arity).map(|_| table.fresh(level)).collect())
    }

    pub fn field_type(&self, type_name: &str, args: &[Type], field: &str) -> Option<Type> {
        let info = self.types.get(type_name)?;
        if !matches!(info.kind, TypeKind::Data | TypeKind::Entity) {
            return None;
        }
        let ctor = self.ctors.get(type_name)?;
        let vars: BTreeMap<String, Type> = ctor.type_params.iter().cloned().zip(args.iter().cloned()).collect();
        ctor.fields.iter().find(|(name, _)| name == field).map(|(_, te)| self.lower_quiet(te, &vars))
    }

    pub fn records_with_field(&self, field: &str) -> Vec<String> {
        self.types
            .iter()
            .filter(|(_, info)| matches!(info.kind, TypeKind::Data | TypeKind::Entity))
            .filter(|(name, _)| {
                self.ctors.get(*name).is_some_and(|ctor| ctor.fields.iter().any(|(other, _)| other == field))
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn is_displayable(&self, ty: &Type) -> bool {
        self.displayable(ty, 0)
    }

    fn displayable(&self, ty: &Type, depth: usize) -> bool {
        match ty {
            Type::Error => true,
            Type::Con(name, args) if args.is_empty() => {
                DISPLAY_SCALARS.contains(&name.as_str())
                    || (depth < 16
                        && self.types.get(name).is_some_and(|info| {
                            info.kind == TypeKind::Distinct
                                && info.underlying.as_ref().is_some_and(|underlying| {
                                    self.displayable(&self.lower_quiet(underlying, &BTreeMap::new()), depth + 1)
                                })
                        }))
            }
            _ => false,
        }
    }
}

impl Signatures for Env {
    fn constructors(&self, ty: &Type) -> Option<Vec<CtorSig>> {
        let Type::Con(name, args) = ty else { return None };
        match name.as_str() {
            "Boolean" => Some(vec![CtorSig::nullary("true"), CtorSig::nullary("false")]),
            "Option" => Some(vec![
                CtorSig::nullary("null"),
                CtorSig { name: "Some".into(), args: vec![args.first().cloned().unwrap_or(Type::Error)] },
            ]),
            _ => {
                let info = self.types.get(name)?;
                let vars: BTreeMap<String, Type> = info.params.iter().cloned().zip(args.iter().cloned()).collect();
                Some(
                    info.ctors
                        .iter()
                        .filter_map(|ctor| self.ctors.get(ctor))
                        .map(|ctor| CtorSig {
                            name: ctor.name.clone(),
                            args: ctor.fields.iter().map(|(_, field)| self.lower_quiet(field, &vars)).collect(),
                        })
                        .collect(),
                )
            }
        }
    }
}

/// Type parameters inside the body of an annotated function: they only equal themselves.
pub fn rigid_vars(params: &[String]) -> BTreeMap<String, Type> {
    params.iter().map(|param| (param.clone(), Type::con(&format!("'{param}")))).collect()
}

fn parse_field_type(type_ref: &TypeRef, span: &Span, diagnostics: &mut Vec<Diagnostic>) -> TypeExpr {
    parse_type_text(&type_ref.display(), span).unwrap_or_else(|diagnostic| {
        diagnostics.push(diagnostic);
        TypeExpr::Named { name: ERROR_TYPE.into(), args: Vec::new(), span: span.clone() }
    })
}
```

- [ ] **Step 4: Rodar os testes**

Run: `cargo test types::env 2>&1 | tail -5`
Expected: PASS (5 testes).

Run: `cargo clippy --all-targets -- -D warnings -A dead_code 2>&1 | tail -3`
Expected: sem warnings.

- [ ] **Step 5: Commit**

```bash
git add src/types/env.rs src/types/mod.rs
git commit -m "feat(types): ambiente de tipos e construtores

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 7: Inferência HM da linguagem pura e `types::check`

**Files:**
- Create: `src/types/infer.rs`
- Modify: `src/types/mod.rs` (versão completa abaixo)

**Interfaces:**
- Consumes: Tasks 2 a 6.
- Produces:
  - `types::check(program: &Program, _schema: &DatabaseSchema) -> (TypedProgram, Vec<Diagnostic>)` (o schema passa a ser usado na Task 8)
  - `TypedProgram { schemes: BTreeMap<String, String>, signatures: BTreeMap<String, Type>, node_types: BTreeMap<(String, NodeId), Type> }`
  - `infer::check_program(program: &Program, env: &Env) -> (TypedProgram, Vec<Diagnostic>)` (ganha `schema` na Task 8)
  - helpers de teste em `types::testing`: `infer`, `infer_with_schema`, `scheme`, `errors`
- Regras implementadas nesta tarefa: literais; nomes (locais, globais, builtins `first get length map fold emptyList listOf`, construtores, `Tipo.Variante`); chamadas; campos (inclusive resolução no estilo OCaml para tipo ainda desconhecido); `?.`; `?:`; operadores com restrições; `if`; `when` com padrões e exaustividade; lambdas; `val` com generalização; `return`; descarte (E7108); funções de topo por SCC; funções anotadas com tipos rígidos; ambiguidade (E7302); fronteiras (E7003). Builders dão E7004 (a Task 8 adiciona `db.<tabela>.insert`). `UI`, `db` e `server` ainda são nomes desconhecidos (Task 8).

- [ ] **Step 1: Escrever `src/types/mod.rs` completo**

```rust
//! Static type system: Hindley-Milner inference, algebraic data types and exhaustive matching.

pub mod env;
pub mod exhaustive;
pub mod infer;
pub mod ty;
pub mod unify;

use crate::ast::{BodyKind, Domain, Program};
use crate::diagnostic::Diagnostic;
use crate::schema::DatabaseSchema;

pub use infer::TypedProgram;

/// Type-checks a whole program. Diagnostics are sorted by file, line, column and code.
pub fn check(program: &Program, _schema: &DatabaseSchema) -> (TypedProgram, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    check_boundaries(program, &mut diagnostics);
    let env = env::Env::build(program, &mut diagnostics);
    let (typed, mut inferred) = infer::check_program(program, &env);
    diagnostics.append(&mut inferred);
    diagnostics.sort_by(|a, b| {
        (&a.span.file, a.span.line, a.span.column, a.code).cmp(&(&b.span.file, b.span.line, b.span.column, b.code))
    });
    (typed, diagnostics)
}

/// Boundary signatures are contracts (RPC, database, UI), so they are never inferred.
fn check_boundaries(program: &Program, diagnostics: &mut Vec<Diagnostic>) {
    for function in &program.functions {
        let what = if function.component {
            "Component"
        } else if function.domain == Domain::Server {
            "Server function"
        } else if function.body_kind == BodyKind::Absent {
            "Function without a body"
        } else {
            continue;
        };
        for parameter in function.parameters.iter().filter(|parameter| parameter.type_ref.is_none()) {
            diagnostics.push(
                Diagnostic::error(
                    "E7003",
                    format!("{what} `{}` must declare the type of parameter `{}`.", function.name, parameter.name),
                    function.span.clone(),
                )
                .with_hint("Boundary signatures are contracts; write `name: Type`."),
            );
        }
        if !function.component && function.return_type.is_none() {
            diagnostics.push(
                Diagnostic::error(
                    "E7003",
                    format!("{what} `{}` must declare its return type.", function.name),
                    function.span.clone(),
                )
                .with_hint("Write `: Type` after the parameter list."),
            );
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use crate::diagnostic::{Severity, has_errors};
    use crate::lexer::lex;
    use crate::parser::parse;
    use crate::schema::DatabaseSchema;

    /// Inferred schemes in declaration order, or the sorted, de-duplicated error codes
    /// (parse errors stop before type checking, as in `compiler::verify`).
    pub fn infer(source: &str) -> Result<Vec<(String, String)>, Vec<String>> {
        infer_with_schema(source, &DatabaseSchema::default())
    }

    pub fn infer_with_schema(source: &str, schema: &DatabaseSchema) -> Result<Vec<(String, String)>, Vec<String>> {
        let (tokens, mut diagnostics) = lex("test.bd", source);
        let (program, mut parse_diagnostics) = parse(tokens);
        diagnostics.append(&mut parse_diagnostics);
        if !has_errors(&diagnostics) {
            let (typed, mut type_diagnostics) = super::check(&program, schema);
            diagnostics.append(&mut type_diagnostics);
            if !has_errors(&diagnostics) {
                return Ok(program
                    .functions
                    .iter()
                    .map(|function| (function.name.clone(), typed.schemes[&function.name].clone()))
                    .collect());
            }
        }
        let mut codes: Vec<String> = diagnostics
            .iter()
            .filter(|item| item.severity == Severity::Error)
            .map(|item| item.code.to_string())
            .collect();
        codes.sort();
        codes.dedup();
        Err(codes)
    }

    pub fn scheme(source: &str, name: &str) -> String {
        let schemes = infer(source).unwrap_or_else(|codes| panic!("expected `{name}` to type-check, got {codes:?}"));
        schemes.into_iter().find(|(other, _)| other == name).map(|(_, scheme)| scheme).unwrap()
    }

    pub fn errors(source: &str) -> Vec<String> {
        match infer(source) {
            Ok(schemes) => panic!("expected type errors, got {schemes:?}"),
            Err(codes) => codes,
        }
    }
}
```

- [ ] **Step 2: Escrever os testes que falham**

Crie `src/types/infer.rs` com:

```rust
#[cfg(test)]
mod tests {
    use crate::types::testing::{errors, scheme};

    const PEDIDO: &str = "sealed class Pedido {\n    data class Pendente(val itens: Int)\n    data class Pago(val valor: Money)\n    data object Cancelado\n}\n";

    #[test]
    fn infers_principal_types() {
        assert_eq!(scheme("fun ident(x) = x", "ident"), "('a) -> 'a");
        assert_eq!(scheme("fun constante(x, y) = x", "constante"), "('a, 'b) -> 'a");
        assert_eq!(scheme("fun compose(f, g, x) = f(g(x))", "compose"), "(('a) -> 'b, ('c) -> 'a, 'c) -> 'b");
        assert_eq!(scheme("fun aplica(f, x) = f(x)", "aplica"), "(('a) -> 'b, 'a) -> 'b");
    }

    #[test]
    fn literals_resolve_operator_constraints() {
        assert_eq!(scheme("fun dobro(x) = x * 2", "dobro"), "(Int) -> Int");
        assert_eq!(scheme("fun concat(a, b) = a + b + \"\"", "concat"), "(String, String) -> String");
        assert_eq!(scheme("fun media(a: Double, b: Double) = (a + b) / 2.0", "media"), "(Double, Double) -> Double");
        assert_eq!(scheme("fun menor(a: Instant, b: Instant) = a < b", "menor"), "(Instant, Instant) -> Boolean");
    }

    #[test]
    fn integer_division_and_remainder_are_nullable() {
        assert_eq!(scheme("fun metade(x: Int) = x / 2", "metade"), "(Int) -> Int?");
        assert_eq!(scheme("fun resto(x: Int) = (x % 2) ?: 0", "resto"), "(Int) -> Int");
        assert_eq!(errors("fun f(x: Int): Int = x / 2"), vec!["E7101"]);
    }

    #[test]
    fn rejects_ambiguous_and_unsupported_operators() {
        assert_eq!(errors("fun soma(a, b) = a + b"), vec!["E7302"]);
        assert_eq!(errors("fun divide(a, b) = a / b"), vec!["E7302"]);
        assert_eq!(errors("fun f(a: Boolean, b: Boolean) = a + b"), vec!["E7301"]);
        assert_eq!(errors("fun f(a: String, b: String) = a - b"), vec!["E7301"]);
        assert_eq!(errors("fun g(x: Int): Int = x\nfun f(): Boolean = g == g"), vec!["E7301"]);
        assert_eq!(errors("fun <T> igual(a: T, b: T): Boolean = a == b"), vec!["E7301"]);
    }

    #[test]
    fn generalizes_local_values() {
        let source = "fun f(): Boolean {\n    val id = { x -> x }\n    val n = id(1)\n    id(n == 1)\n}";
        assert_eq!(scheme(source, "f"), "() -> Boolean");
    }

    #[test]
    fn annotated_type_parameters_are_rigid() {
        assert_eq!(scheme("fun <T> primeiro(xs: List<T>): T? = first(xs)", "primeiro"), "(List<'a>) -> 'a?");
        assert_eq!(errors("fun <T> errado(x: T): Int = x"), vec!["E7101"]);
        assert_eq!(
            scheme("fun <A, B> mapear(xs: List<A>, f: (A) -> B): List<B> = map(xs, f)", "mapear"),
            "(List<'a>, ('a) -> 'b) -> List<'b>"
        );
    }

    #[test]
    fn infers_mutual_recursion() {
        let source = "fun par(n) = if (n == 0) true else impar(n - 1)\nfun impar(n) = if (n == 0) false else par(n - 1)";
        assert_eq!(scheme(source, "par"), "(Int) -> Boolean");
        assert_eq!(scheme(source, "impar"), "(Int) -> Boolean");
        assert_eq!(scheme("fun fatorial(n) = if (n <= 1) 1 else n * fatorial(n - 1)", "fatorial"), "(Int) -> Int");
    }

    #[test]
    fn rejects_infinite_types() {
        assert_eq!(errors("fun auto(x) = x(x)"), vec!["E7107"]);
    }

    #[test]
    fn discarded_values_are_errors() {
        assert_eq!(errors("fun f(): Int {\n    1 + 1\n    2\n}"), vec!["E7108"]);
        assert_eq!(scheme("fun f(): Int {\n    val _ = 1 + 1\n    2\n}", "f"), "() -> Int");
        let operator_on_next_line = "fun f(a: Int, b: Int): Int {\n    val y = a\n        - b\n    y\n}";
        assert_eq!(errors(operator_on_next_line), vec!["E7108"]);
    }

    #[test]
    fn returns_and_lambdas() {
        assert_eq!(scheme("fun f(x: Int): Int {\n    return x\n}", "f"), "(Int) -> Int");
        assert_eq!(errors("fun f(): Int {\n    val g = { x -> return x }\n    g(1)\n}"), vec!["E7111"]);
        assert_eq!(errors("fun f(): Int {\n    return \"a\"\n}"), vec!["E7101"]);
        assert_eq!(errors("fun f(c: Boolean) = if (c) 1 else \"a\""), vec!["E7101"]);
    }

    #[test]
    fn when_must_be_exhaustive_and_free_of_redundancy() {
        let complete = format!("{PEDIDO}fun f(p: Pedido): Int = when (p) {{\n    Pendente(n) -> n\n    Pago(_) -> 1\n    Cancelado -> 0\n}}");
        assert_eq!(scheme(&complete, "f"), "(Pedido) -> Int");
        let missing = format!("{PEDIDO}fun f(p: Pedido): Int = when (p) {{\n    Pendente(n) -> n\n    Cancelado -> 0\n}}");
        assert_eq!(errors(&missing), vec!["E7201"]);
        let redundant = format!("{PEDIDO}fun f(p: Pedido): Int = when (p) {{\n    _ -> 0\n    Cancelado -> 1\n}}");
        assert_eq!(errors(&redundant), vec!["E7202"]);
        assert_eq!(errors(&format!("{PEDIDO}fun f(p: Pedido): Int = when (p) {{ Pendente(a, b) -> 1; _ -> 0 }}")), vec!["E7102"]);
    }

    #[test]
    fn nullable_patterns() {
        assert_eq!(scheme("fun f(x: Int?): Int = when (x) {\n    null -> 0\n    n -> n\n}", "f"), "(Int?) -> Int");
        assert_eq!(errors("fun f(x: Int?): Int = when (x) { n -> n }"), vec!["E7201"]);
        assert_eq!(scheme("fun f(x) = when (x) {\n    null -> 0\n    n -> n + 1\n}", "f"), "(Int?) -> Int");
    }

    #[test]
    fn literal_patterns() {
        let source = "fun f(x: Int): String = when (x) {\n    0 -> \"zero\"\n    _ -> \"outro\"\n}";
        assert_eq!(scheme(source, "f"), "(Int) -> String");
        assert_eq!(errors("fun f(x: Int): String = when (x) { 0 -> \"zero\" }"), vec!["E7201"]);
        assert_eq!(errors("fun f(x: Double): Int = when (x) { 1.5 -> 1; _ -> 0 }"), vec!["E7109"]);
        assert_eq!(errors("fun f(b: Boolean): Int = when (b) { true -> 1; true -> 2; false -> 3 }"), vec!["E7202"]);
    }

    #[test]
    fn records_and_field_access() {
        let source = "data class P(val x: Int, val nome: String?)\nfun getX(p) = p.x\nfun nome(p: P?) = p?.nome ?: \"\"";
        assert_eq!(scheme(source, "getX"), "(P) -> Int");
        assert_eq!(scheme(source, "nome"), "(P?) -> String");
        assert_eq!(errors("data class A(val x: Int)\ndata class B(val x: Int)\nfun f(v) = v.x"), vec!["E7106"]);
        assert_eq!(errors("data class A(val x: Int)\nfun f(a: A) = a.y"), vec!["E7104"]);
        assert_eq!(errors("data class A(val x: Int)\nfun f(a: A?) = a.x"), vec!["E7101"]);
    }

    #[test]
    fn constructors_results_and_enums() {
        assert_eq!(scheme("fun ok() = Ok(1)", "ok"), "() -> Result<Int, 'a>");
        assert_eq!(scheme("fun erro(s: String): Result<Int, String> = Err(s)", "erro"), "(String) -> Result<Int, String>");
        assert_eq!(scheme("enum class Cor { Azul, Verde }\nfun azul() = Cor.Azul", "azul"), "() -> Cor");
        assert_eq!(scheme("data class P(val x: Int)\nfun p() = P(1)", "p"), "() -> P");
        assert_eq!(errors("data class P(val x: Int)\nfun p() = P(1, 2)"), vec!["E7102"]);
        assert_eq!(scheme("fun xs() = listOf(1, 2, 3)", "xs"), "() -> List<Int>");
        assert_eq!(errors("fun xs() = listOf(1, \"a\")"), vec!["E7101"]);
    }

    #[test]
    fn unknown_names_do_not_cascade() {
        assert_eq!(errors("fun f() = nada"), vec!["E7103"]);
        assert_eq!(errors("fun f(x: Nada) = x"), vec!["E7103"]);
        assert_eq!(errors("fun f() = naoExiste.campo + 1"), vec!["E7103"]);
    }

    #[test]
    fn boundaries_require_annotations() {
        assert_eq!(errors("server fun f(x) = x"), vec!["E7003"]);
        assert_eq!(errors("external fun g(x: Int)"), vec!["E7003"]);
    }
}
```

- [ ] **Step 3: Rodar e confirmar a falha**

Run: `cargo test types::infer 2>&1 | tail -5`
Expected: FAIL de compilação (`check_program` não existe).

- [ ] **Step 4: Implementar `src/types/infer.rs` (antes dos testes)**

```rust
//! Hindley-Milner inference with level-based generalization (Rémy), as in OCaml's compiler.
//!
//! Bind is stricter than OCaml: operator constraints must resolve to a concrete type,
//! discarded non-`Unit` values are errors and `when` must be exhaustive.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{Function, Program, TypeKind, TypeRef};
use crate::diagnostic::{Diagnostic, Span};
use crate::parser::expr::parse_type_text;
use crate::syntax::expr::*;

use super::env::{Env, rigid_vars};
use super::exhaustive::{self, Pat};
use super::ty::*;
use super::unify::{UnifyError, constrain, unify};

/// Level of quantified variables in schemes; never adjusted by unification.
const GENERIC_LEVEL: u32 = u32::MAX;

#[allow(dead_code)] // Consumed by subprojects 2 and 3 (typed IR for verification and code generation).
#[derive(Clone, Debug, Default)]
pub struct TypedProgram {
    /// Rendered type scheme of each function, e.g. `('a) -> 'a`.
    pub schemes: BTreeMap<String, String>,
    /// Resolved type of each function.
    pub signatures: BTreeMap<String, Type>,
    /// Resolved type of each expression and block, keyed by function and node.
    pub node_types: BTreeMap<(String, NodeId), Type>,
}

pub fn check_program(program: &Program, env: &Env) -> (TypedProgram, Vec<Diagnostic>) {
    let mut checker = Checker::new(env);
    checker.run(program);
    let typed = checker.finish(program);
    (typed, checker.diagnostics)
}

struct Checker<'a> {
    env: &'a Env,
    table: TypeTable,
    level: u32,
    globals: BTreeMap<String, Scheme>,
    function_spans: BTreeMap<String, Span>,
    locals: Vec<(String, Scheme)>,
    /// Type parameters in scope: rigid in annotated functions, flexible in inferred ones.
    rigid: BTreeMap<String, Type>,
    function: String,
    return_type: Option<Type>,
    lambda_depth: usize,
    nodes: BTreeMap<(String, NodeId), (Type, Span)>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Checker<'a> {
    fn new(env: &'a Env) -> Self {
        Self {
            env,
            table: TypeTable::default(),
            level: 0,
            globals: BTreeMap::new(),
            function_spans: BTreeMap::new(),
            locals: Vec::new(),
            rigid: BTreeMap::new(),
            function: String::new(),
            return_type: None,
            lambda_depth: 0,
            nodes: BTreeMap::new(),
            diagnostics: Vec::new(),
        }
    }

    // ---- program level -------------------------------------------------------------

    fn run(&mut self, program: &Program) {
        self.install_builtins();
        for function in &program.functions {
            self.function_spans.insert(function.name.clone(), function.span.clone());
        }
        let (annotated, inferred): (Vec<&Function>, Vec<&Function>) =
            program.functions.iter().partition(|function| is_fully_annotated(function));
        for function in &annotated {
            let scheme = self.annotated_scheme(function);
            self.globals.insert(function.name.clone(), scheme);
        }
        for group in components(&inferred) {
            let members: Vec<&Function> = group.iter().map(|&index| inferred[index]).collect();
            self.infer_group(&members);
        }
        for function in annotated {
            self.check_annotated(function);
        }
    }

    fn install_builtins(&mut self) {
        let table = &mut self.table;
        let list = |ty: &Type| Type::list(ty.clone());
        let builtins = [
            ("first", builtin(table, 1, |v| Type::Fun(vec![list(&v[0])], Box::new(Type::option(v[0].clone()))))),
            (
                "get",
                builtin(table, 1, |v| {
                    Type::Fun(vec![list(&v[0]), Type::con("Int")], Box::new(Type::option(v[0].clone())))
                }),
            ),
            ("length", builtin(table, 1, |v| Type::Fun(vec![list(&v[0])], Box::new(Type::con("Int"))))),
            (
                "map",
                builtin(table, 2, |v| {
                    let mapper = Type::Fun(vec![v[0].clone()], Box::new(v[1].clone()));
                    Type::Fun(vec![list(&v[0]), mapper], Box::new(list(&v[1])))
                }),
            ),
            (
                "fold",
                builtin(table, 2, |v| {
                    let step = Type::Fun(vec![v[1].clone(), v[0].clone()], Box::new(v[1].clone()));
                    Type::Fun(vec![list(&v[0]), v[1].clone(), step], Box::new(v[1].clone()))
                }),
            ),
            ("emptyList", builtin(table, 1, |v| Type::Fun(Vec::new(), Box::new(list(&v[0]))))),
        ];
        for (name, scheme) in builtins {
            self.globals.insert(name.into(), scheme);
        }
    }

    fn annotated_scheme(&mut self, function: &Function) -> Scheme {
        let mut vars = BTreeMap::new();
        let mut ids = Vec::new();
        for name in &function.type_params {
            let var = self.table.fresh(GENERIC_LEVEL);
            if let Type::Var(id) = var {
                ids.push(id);
            }
            vars.insert(name.clone(), var);
        }
        let ty = self.signature(function, &vars, true);
        Scheme { vars: ids, ty }
    }

    /// Function type from the declaration; missing annotations become fresh variables.
    fn signature(&mut self, function: &Function, vars: &BTreeMap<String, Type>, report: bool) -> Type {
        let mut params = Vec::new();
        for parameter in &function.parameters {
            let ty = match &parameter.type_ref {
                Some(type_ref) => self.lower_ref(type_ref, &function.span, vars, report),
                None => self.fresh(),
            };
            params.push(ty);
        }
        let result = if function.component {
            Type::con("Ui")
        } else {
            match &function.return_type {
                Some(type_ref) => self.lower_ref(type_ref, &function.span, vars, report),
                None => self.fresh(),
            }
        };
        Type::Fun(params, Box::new(result))
    }

    fn lower_ref(&mut self, type_ref: &TypeRef, span: &Span, vars: &BTreeMap<String, Type>, report: bool) -> Type {
        let mut sink = Vec::new();
        let diagnostics = if report { &mut self.diagnostics } else { &mut sink };
        match parse_type_text(&type_ref.display(), span) {
            Ok(te) => self.env.lower(&te, vars, diagnostics),
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                Type::Error
            }
        }
    }

    fn infer_group(&mut self, group: &[&Function]) {
        self.level += 1;
        let mut types = Vec::new();
        let mut scopes = Vec::new();
        for function in group {
            let mut scope = BTreeMap::new();
            for name in &function.type_params {
                scope.insert(name.clone(), self.fresh());
            }
            let ty = self.signature(function, &scope, true);
            self.globals.insert(function.name.clone(), Scheme::mono(ty.clone()));
            types.push(ty);
            scopes.push(scope);
        }
        for ((function, ty), scope) in group.iter().zip(&types).zip(scopes) {
            self.rigid = scope;
            self.check_body(function, ty);
        }
        self.rigid.clear();
        for function in group {
            self.report_ambiguous(&function.name);
        }
        self.level -= 1;
        for (function, ty) in group.iter().zip(&types) {
            let (scheme, ambiguous) = generalize(&self.table, self.level, ty);
            self.report_constraints(ambiguous, &function.span);
            self.globals.insert(function.name.clone(), scheme);
        }
    }

    fn check_annotated(&mut self, function: &Function) {
        self.level += 1;
        self.rigid = rigid_vars(&function.type_params);
        let rigid = self.rigid.clone();
        let ty = self.signature(function, &rigid, false);
        self.check_body(function, &ty);
        self.report_ambiguous(&function.name);
        self.rigid.clear();
        self.level -= 1;
    }

    fn check_body(&mut self, function: &Function, ty: &Type) {
        let Type::Fun(params, result) = ty else { return };
        let Some(block) = &function.expr else { return };
        self.function = function.name.clone();
        self.return_type = Some((**result).clone());
        self.locals.clear();
        for (parameter, param) in function.parameters.iter().zip(params) {
            self.locals.push((parameter.name.clone(), Scheme::mono(param.clone())));
        }
        let body = self.infer_block(block);
        let reason = if function.component {
            "a component body, which must end with a UI tree".to_string()
        } else {
            format!("the declared result of `{}`", function.name)
        };
        self.unify_at(result, &body, &result_span(block), Some(reason));
        self.locals.clear();
        self.return_type = None;
    }

    fn finish(&self, program: &Program) -> TypedProgram {
        let mut typed = TypedProgram::default();
        for function in &program.functions {
            if let Some(scheme) = self.globals.get(&function.name) {
                let ty = self.table.resolve(&scheme.ty);
                typed.schemes.insert(function.name.clone(), display(&ty));
                typed.signatures.insert(function.name.clone(), ty);
            }
        }
        typed.node_types = self.nodes.iter().map(|(key, (ty, _))| (key.clone(), self.table.resolve(ty))).collect();
        typed
    }

    // ---- ambiguity -----------------------------------------------------------------

    /// Constrained variables still unbound at the end of a function have no concrete type.
    fn report_ambiguous(&mut self, function: &str) {
        let key = function.to_string();
        let entries: Vec<(Type, Span)> =
            self.nodes.range((key.clone(), 0)..=(key, NodeId::MAX)).map(|(_, entry)| entry.clone()).collect();
        let mut seen = BTreeSet::new();
        for (ty, span) in entries {
            for id in free_vars(&self.table, &ty) {
                if let VarState::Unbound { constraint: Some(constraint), .. } = self.table.state(id).clone()
                    && seen.insert(id)
                {
                    self.ambiguity(constraint, &span);
                    self.table.set(id, VarState::Link(Type::Error));
                }
            }
        }
    }

    fn report_constraints(&mut self, ambiguous: Vec<(TypeVarId, Constraint)>, span: &Span) {
        for (id, constraint) in ambiguous {
            self.ambiguity(constraint, span);
            self.table.set(id, VarState::Link(Type::Error));
        }
    }

    fn ambiguity(&mut self, constraint: Constraint, span: &Span) {
        self.diagnostics.push(
            Diagnostic::error(
                "E7302",
                format!("Ambiguous type: a value used with {} never receives a concrete type.", constraint.describe()),
                span.clone(),
            )
            .with_hint("Annotate a parameter or value, e.g. `a: Int` or `a: Double`."),
        );
    }

    // ---- helpers -------------------------------------------------------------------

    fn fresh(&mut self) -> Type {
        self.table.fresh(self.level)
    }

    fn record(&mut self, id: NodeId, ty: &Type, span: &Span) {
        self.nodes.insert((self.function.clone(), id), (ty.clone(), span.clone()));
    }

    fn error(&mut self, code: &'static str, message: impl Into<String>, span: &Span) {
        self.diagnostics.push(Diagnostic::error(code, message, span.clone()));
    }

    fn shown(&self, ty: &Type) -> String {
        display(&self.table.resolve(ty))
    }

    fn is_local(&self, name: &str) -> bool {
        self.locals.iter().any(|(local, _)| local == name)
    }

    fn is_shadowed(&self, name: &str) -> bool {
        self.is_local(name) || self.globals.contains_key(name)
    }

    fn unify_at(&mut self, expected: &Type, found: &Type, span: &Span, reason: Option<String>) {
        let Err(error) = unify(&mut self.table, expected, found) else { return };
        let diagnostic = match error {
            UnifyError::Mismatch { expected, found } => {
                let names = display_all(&[&expected, &found]);
                let diagnostic = Diagnostic::error(
                    "E7101",
                    format!("Type mismatch: expected `{}`, found `{}`.", names[0], names[1]),
                    span.clone(),
                );
                match reason {
                    Some(reason) => diagnostic.with_hint(format!("`{}` is required by {reason}.", names[0])),
                    None => diagnostic,
                }
            }
            UnifyError::Infinite { var, ty } => {
                let names = display_all(&[&Type::Var(var), &ty]);
                Diagnostic::error(
                    "E7107",
                    format!("Infinite type: `{}` would have to equal `{}`.", names[0], names[1]),
                    span.clone(),
                )
            }
            UnifyError::Constraint { constraint, ty } => Diagnostic::error(
                "E7301",
                format!("`{}` does not support {}.", display(&ty), constraint.describe()),
                span.clone(),
            ),
        };
        self.diagnostics.push(diagnostic);
    }

    fn require(&mut self, constraint: Constraint, ty: &Type, span: &Span) {
        if let Err(UnifyError::Constraint { constraint, ty }) = constrain(&mut self.table, constraint, ty) {
            self.error("E7301", format!("`{}` does not support {}.", display(&ty), constraint.describe()), span);
        }
    }

    // ---- blocks and statements -----------------------------------------------------

    fn infer_block(&mut self, block: &Block) -> Type {
        let mark = self.locals.len();
        let mut value = Type::con("Unit");
        for (index, stmt) in block.stmts.iter().enumerate() {
            let last = index + 1 == block.stmts.len();
            match stmt {
                Stmt::Val { name, annotation, value: expr, span } => self.bind_val(name, annotation.as_ref(), expr, span),
                Stmt::Expr(expr) => {
                    let ty = self.infer(expr);
                    if last {
                        value = ty;
                    } else {
                        self.discard(&ty, expr);
                    }
                }
            }
        }
        self.locals.truncate(mark);
        self.record(block.id, &value, &block.span);
        value
    }

    fn bind_val(&mut self, name: &str, annotation: Option<&TypeExpr>, value: &Expr, span: &Span) {
        self.level += 1;
        let mut ty = self.infer(value);
        if let Some(annotation) = annotation {
            let declared = self.env.lower(annotation, &self.rigid, &mut self.diagnostics);
            self.unify_at(&declared, &ty, &value.span, Some(format!("the annotation on `{name}`")));
            ty = declared;
        }
        self.level -= 1;
        let (scheme, ambiguous) = generalize(&self.table, self.level, &ty);
        self.report_constraints(ambiguous, span);
        if name != "_" {
            self.locals.push((name.to_string(), scheme));
        }
    }

    fn discard(&mut self, ty: &Type, expr: &Expr) {
        if unify(&mut self.table, &Type::con("Unit"), ty).is_ok() {
            return;
        }
        let shown = self.shown(ty);
        self.diagnostics.push(
            Diagnostic::error(
                "E7108",
                format!(
                    "The value of `{}` (type `{shown}`) is computed and then discarded.",
                    abbreviate(&render_expr(expr))
                ),
                expr.span.clone(),
            )
            .with_hint("Use the value, or write `val _ = ...` to discard it explicitly."),
        );
    }

    // ---- expressions ---------------------------------------------------------------

    fn infer(&mut self, expr: &Expr) -> Type {
        let ty = match &expr.kind {
            ExprKind::Int(_) => Type::con("Int"),
            ExprKind::Double(_) => Type::con("Double"),
            ExprKind::Float(_) => Type::con("Float"),
            ExprKind::Str(_) => Type::con("String"),
            ExprKind::Bool(_) => Type::con("Boolean"),
            ExprKind::Null => {
                let inner = self.fresh();
                Type::option(inner)
            }
            ExprKind::Name(name) => self.infer_name(name, &expr.span),
            ExprKind::Call { callee, args, children } => self.infer_call(callee, args, children.as_ref(), &expr.span),
            ExprKind::Field { target, name } => self.infer_field(target, name, &expr.span),
            ExprKind::SafeField { target, name } => {
                let receiver = self.infer(target);
                let inner = self.fresh();
                self.unify_at(&Type::option(inner.clone()), &receiver, &target.span, Some("the receiver of `?.`".into()));
                let field = self.field_of(&inner, name, &expr.span);
                match self.table.shallow(&field) {
                    Type::Con(option, _) if option == "Option" => field,
                    _ => Type::option(field),
                }
            }
            ExprKind::Unary { op: UnaryOp::Neg, operand } => {
                let ty = self.infer(operand);
                self.require(Constraint::Sub, &ty, &operand.span);
                ty
            }
            ExprKind::Unary { op: UnaryOp::Not, operand } => {
                let ty = self.infer(operand);
                self.unify_at(&Type::con("Boolean"), &ty, &operand.span, Some("the `!` operator".into()));
                Type::con("Boolean")
            }
            ExprKind::Binary { op, left, right } => self.infer_binary(*op, left, right, &expr.span),
            ExprKind::Elvis { value, fallback } => {
                let ty = self.infer(value);
                let inner = self.fresh();
                self.unify_at(&Type::option(inner.clone()), &ty, &value.span, Some("the left side of `?:`".into()));
                let fallback_ty = self.infer(fallback);
                self.unify_at(&inner, &fallback_ty, &fallback.span, Some("the left side of `?:`".into()));
                inner
            }
            ExprKind::If { condition, then_branch, else_branch } => {
                let condition_ty = self.infer(condition);
                self.unify_at(&Type::con("Boolean"), &condition_ty, &condition.span, Some("an `if` condition".into()));
                let then_ty = self.infer(then_branch);
                let else_ty = self.infer(else_branch);
                self.unify_at(&then_ty, &else_ty, &else_branch.span, Some("the first `if` branch".into()));
                then_ty
            }
            ExprKind::When { scrutinee, arms } => self.infer_when(scrutinee, arms, &expr.span),
            ExprKind::Lambda { params, body } => {
                let mark = self.locals.len();
                let mut names = BTreeSet::new();
                let mut param_types = Vec::new();
                for (name, span) in params {
                    if !names.insert(name.as_str()) {
                        self.error("E7113", format!("`{name}` is bound more than once in this lambda."), span);
                    }
                    let ty = self.fresh();
                    param_types.push(ty.clone());
                    self.locals.push((name.clone(), Scheme::mono(ty)));
                }
                self.lambda_depth += 1;
                let result = self.infer_block(body);
                self.lambda_depth -= 1;
                self.locals.truncate(mark);
                Type::Fun(param_types, Box::new(result))
            }
            ExprKind::Block(block) => self.infer_block(block),
            ExprKind::Return(value) => {
                let found = self.infer(value);
                if self.lambda_depth > 0 {
                    self.error(
                        "E7111",
                        "`return` is not allowed inside a lambda; the lambda's value is its last expression.",
                        &expr.span,
                    );
                } else if let Some(expected) = self.return_type.clone() {
                    let reason = format!("the return type of `{}`", self.function);
                    self.unify_at(&expected, &found, &value.span, Some(reason));
                }
                self.fresh()
            }
            ExprKind::Builder { fields, .. } => self.infer_builder(fields, &expr.span),
        };
        self.record(expr.id, &ty, &expr.span);
        ty
    }

    fn infer_name(&mut self, name: &str, span: &Span) -> Type {
        if let Some(scheme) = self.locals.iter().rev().find(|(local, _)| local == name).map(|(_, scheme)| scheme.clone()) {
            return instantiate(&mut self.table, &scheme, self.level);
        }
        if let Some(scheme) = self.globals.get(name).cloned() {
            return instantiate(&mut self.table, &scheme, self.level);
        }
        if self.env.ctors.contains_key(name) {
            return self.constructor_value(name);
        }
        self.error("E7103", format!("Unknown name `{name}`."), span);
        Type::Error
    }

    fn constructor_value(&mut self, name: &str) -> Type {
        let (fields, result) = self.env.instantiate_ctor(name, &mut self.table, self.level);
        if fields.is_empty() { result } else { Type::Fun(fields, Box::new(result)) }
    }

    fn infer_call(&mut self, callee: &Expr, args: &[Expr], children: Option<&Block>, span: &Span) -> Type {
        if let ExprKind::Name(name) = &callee.kind
            && name == "listOf"
            && !self.is_shadowed(name)
        {
            return self.infer_list_of(args, children, span);
        }
        if let Some(block) = children {
            self.error("E7004", "Only UI nodes accept a child block.", &block.span);
        }
        let callee_ty = self.infer(callee);
        let arg_types: Vec<Type> = args.iter().map(|arg| self.infer(arg)).collect();
        match self.table.shallow(&callee_ty) {
            Type::Fun(params, result) => {
                if params.len() != args.len() {
                    self.error(
                        "E7102",
                        format!("This function expects {} argument(s), but {} were given.", params.len(), args.len()),
                        span,
                    );
                    return *result;
                }
                let reason = self.parameter_reason(callee);
                for ((param, arg_ty), arg) in params.iter().zip(&arg_types).zip(args) {
                    self.unify_at(param, arg_ty, &arg.span, reason.clone());
                }
                *result
            }
            Type::Error => Type::Error,
            Type::Var(_) => {
                let result = self.fresh();
                let function = Type::Fun(arg_types, Box::new(result.clone()));
                self.unify_at(&callee_ty, &function, &callee.span, None);
                result
            }
            other => {
                let shown = self.shown(&other);
                self.error("E7101", format!("A value of type `{shown}` is not a function."), &callee.span);
                Type::Error
            }
        }
    }

    fn parameter_reason(&self, callee: &Expr) -> Option<String> {
        let ExprKind::Name(name) = &callee.kind else { return None };
        match self.function_spans.get(name) {
            Some(span) if !self.is_local(name) => Some(format!(
                "a parameter of `{name}` (declared at {}:{}:{})",
                span.file, span.line, span.column
            )),
            _ => Some(format!("a parameter of `{name}`")),
        }
    }

    fn infer_list_of(&mut self, args: &[Expr], children: Option<&Block>, span: &Span) -> Type {
        if children.is_some() {
            self.error("E7004", "`listOf` does not accept a child block.", span);
        }
        let element = self.fresh();
        for arg in args {
            let ty = self.infer(arg);
            self.unify_at(&element, &ty, &arg.span, Some("the first element of `listOf`".into()));
        }
        Type::list(element)
    }

    fn infer_field(&mut self, target: &Expr, name: &str, span: &Span) -> Type {
        if let ExprKind::Name(base) = &target.kind
            && !self.is_shadowed(base)
            && let Some(info) = self.env.types.get(base)
            && matches!(info.kind, TypeKind::Enum | TypeKind::Sealed)
        {
            if info.ctors.iter().any(|ctor| ctor == name) {
                return self.constructor_value(name);
            }
            self.error("E7104", format!("`{base}` has no variant `{name}`."), span);
            return Type::Error;
        }
        let target_ty = self.infer(target);
        self.field_of(&target_ty, name, span)
    }

    fn field_of(&mut self, target: &Type, name: &str, span: &Span) -> Type {
        match self.table.shallow(target) {
            Type::Error => Type::Error,
            Type::Con(type_name, args) => {
                if type_name == "Option" {
                    let shown = self.shown(target);
                    self.diagnostics.push(
                        Diagnostic::error(
                            "E7101",
                            format!("Cannot access `{name}` on a nullable value of type `{shown}`."),
                            span.clone(),
                        )
                        .with_hint("Use `?.`, `?:` or a `when` that handles `null`."),
                    );
                    return Type::Error;
                }
                match self.env.field_type(&type_name, &args, name) {
                    Some(ty) => ty,
                    None => {
                        let shown = self.shown(target);
                        self.error("E7104", format!("`{shown}` has no field `{name}`."), span);
                        Type::Error
                    }
                }
            }
            Type::Var(_) => {
                let owners = self.env.records_with_field(name);
                match owners.as_slice() {
                    [owner] => {
                        let record = self.env.instantiate_record(owner, &mut self.table, self.level);
                        self.unify_at(&record, target, span, None);
                        let Type::Con(record_name, args) = &record else { return Type::Error };
                        self.env.field_type(record_name, args, name).unwrap_or(Type::Error)
                    }
                    [] => {
                        self.error("E7104", format!("No record type has a field `{name}`."), span);
                        Type::Error
                    }
                    _ => {
                        self.diagnostics.push(
                            Diagnostic::error(
                                "E7106",
                                format!("The field `{name}` exists in several types ({}).", owners.join(", ")),
                                span.clone(),
                            )
                            .with_hint("Annotate the value's type."),
                        );
                        Type::Error
                    }
                }
            }
            Type::Fun(..) => {
                self.error("E7104", format!("A function has no field `{name}`."), span);
                Type::Error
            }
        }
    }

    fn infer_binary(&mut self, op: BinaryOp, left: &Expr, right: &Expr, span: &Span) -> Type {
        let left_ty = self.infer(left);
        let right_ty = self.infer(right);
        let boolean = Type::con("Boolean");
        match op {
            BinaryOp::And | BinaryOp::Or => {
                let reason = Some(format!("the `{}` operator", op.symbol()));
                self.unify_at(&boolean, &left_ty, &left.span, reason.clone());
                self.unify_at(&boolean, &right_ty, &right.span, reason);
                boolean
            }
            BinaryOp::Rem => {
                let int = Type::con("Int");
                self.unify_at(&int, &left_ty, &left.span, Some("the `%` operator".into()));
                self.unify_at(&int, &right_ty, &right.span, Some("the `%` operator".into()));
                Type::option(int)
            }
            _ => {
                let constraint = match op {
                    BinaryOp::Add => Constraint::Add,
                    BinaryOp::Sub => Constraint::Sub,
                    BinaryOp::Mul | BinaryOp::Div => Constraint::Num,
                    BinaryOp::Eq | BinaryOp::NotEq => Constraint::Eq,
                    _ => Constraint::Ord,
                };
                self.unify_at(&left_ty, &right_ty, &right.span, Some(format!("the left operand of `{}`", op.symbol())));
                self.require(constraint, &left_ty, span);
                match op {
                    BinaryOp::Div => self.division_result(&left_ty, span),
                    BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => left_ty,
                    _ => boolean,
                }
            }
        }
    }

    /// `Int` and `Decimal` division can divide by zero, so it is nullable; IEEE floats are not.
    fn division_result(&mut self, operand: &Type, span: &Span) -> Type {
        match self.table.shallow(operand) {
            Type::Con(name, _) if name == "Int" || name == "Decimal" => Type::option(operand.clone()),
            Type::Var(id) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        "E7302",
                        "Ambiguous type: the result of `/` depends on the operand type, which is not known yet.",
                        span.clone(),
                    )
                    .with_hint("Annotate a parameter or value, e.g. `a: Int` or `a: Double`."),
                );
                self.table.set(id, VarState::Link(Type::Error));
                Type::Error
            }
            _ => operand.clone(),
        }
    }

    fn infer_builder(&mut self, fields: &[FieldInit], span: &Span) -> Type {
        for field in fields {
            self.infer(&field.value);
        }
        self.error("E7004", "`{ field = value }` blocks are only supported on `db.<table>.insert`.", span);
        Type::Error
    }

    // ---- when and patterns ---------------------------------------------------------

    fn infer_when(&mut self, scrutinee: &Expr, arms: &[Arm], span: &Span) -> Type {
        let subject = self.infer(scrutinee);
        if arms.iter().any(|arm| matches!(arm.pattern, Pattern::Null(_))) {
            let inner = self.fresh();
            self.unify_at(&Type::option(inner), &subject, &scrutinee.span, Some("a `null` pattern".into()));
        }
        let result = self.fresh();
        let mut patterns = Vec::new();
        let mut invalid_pattern = false;
        for arm in arms {
            let mark = self.locals.len();
            let mut bound = BTreeSet::new();
            let reported = self.diagnostics.len();
            let pattern = self.check_pattern(&arm.pattern, &subject, &mut bound);
            invalid_pattern |= self.diagnostics.len() > reported;
            let body = self.infer(&arm.body);
            self.unify_at(&result, &body, &arm.body.span, Some("the first `when` branch".into()));
            self.locals.truncate(mark);
            patterns.push(pattern);
        }
        // An invalid pattern stands in as `_`, which would make later arms look redundant.
        let subject_ty = self.table.resolve(&subject);
        if !invalid_pattern && !subject_ty.contains_error() {
            let report = exhaustive::check_match(self.env, &subject_ty, &patterns);
            for index in report.redundant {
                self.error("E7202", "This `when` branch can never be reached.", &arms[index].span);
            }
            if let Some(missing) = report.missing {
                self.error(
                    "E7201",
                    format!("`when` is not exhaustive; missing case: {}.", exhaustive::render(&missing)),
                    span,
                );
            }
        }
        result
    }

    /// Checks `pattern` against `ty`. At a type already known to be `T?`, every pattern other
    /// than `_` and `null` describes a non-null value and is checked against `T`.
    fn check_pattern(&mut self, pattern: &Pattern, ty: &Type, bound: &mut BTreeSet<String>) -> Pat {
        if !matches!(pattern, Pattern::Wildcard | Pattern::Null(_))
            && let Type::Con(name, args) = self.table.shallow(ty)
            && name == "Option"
            && args.len() == 1
        {
            let inner = self.check_pattern(pattern, &args[0], bound);
            return Pat::Ctor("Some".into(), vec![inner]);
        }
        match pattern {
            Pattern::Wildcard => Pat::Wild,
            Pattern::Null(span) => {
                let inner = self.fresh();
                self.unify_at(&Type::option(inner), ty, span, Some("a `null` pattern".into()));
                Pat::Ctor("null".into(), Vec::new())
            }
            Pattern::Bool(value, span) => {
                self.unify_at(ty, &Type::con("Boolean"), span, None);
                Pat::Ctor(value.to_string(), Vec::new())
            }
            Pattern::Int(text, span) => {
                self.unify_at(ty, &Type::con("Int"), span, None);
                Pat::Lit(text.clone())
            }
            Pattern::Str(text, span) => {
                self.unify_at(ty, &Type::con("String"), span, None);
                Pat::Lit(format!("{text:?}"))
            }
            Pattern::Double(text, span) => {
                self.error("E7109", format!("The floating-point literal `{text}` cannot be used as a pattern."), span);
                Pat::Wild
            }
            Pattern::Name(name, span) => {
                if let Some(ctor) = self.env.ctors.get(name) {
                    if !ctor.fields.is_empty() {
                        self.error(
                            "E7102",
                            format!("Constructor `{name}` has {} field(s); write `{name}(...)`.", ctor.fields.len()),
                            span,
                        );
                        return Pat::Wild;
                    }
                    let (_, result) = self.env.instantiate_ctor(name, &mut self.table, self.level);
                    self.unify_at(ty, &result, span, None);
                    return Pat::Ctor(name.clone(), Vec::new());
                }
                if !bound.insert(name.clone()) {
                    self.error("E7113", format!("`{name}` is bound more than once in this pattern."), span);
                }
                self.locals.push((name.clone(), Scheme::mono(ty.clone())));
                Pat::Wild
            }
            Pattern::Constructor { name, args, span } => {
                if !self.env.ctors.contains_key(name) {
                    self.error("E7103", format!("Unknown constructor `{name}`."), span);
                    return Pat::Wild;
                }
                let (fields, result) = self.env.instantiate_ctor(name, &mut self.table, self.level);
                self.unify_at(ty, &result, span, None);
                if fields.len() != args.len() {
                    self.error(
                        "E7102",
                        format!("Constructor `{name}` has {} field(s), but the pattern lists {}.", fields.len(), args.len()),
                        span,
                    );
                    return Pat::Wild;
                }
                let patterns = args.iter().zip(&fields).map(|(arg, field)| self.check_pattern(arg, field, bound)).collect();
                Pat::Ctor(name.clone(), patterns)
            }
        }
    }
}

fn is_fully_annotated(function: &Function) -> bool {
    function.parameters.iter().all(|parameter| parameter.type_ref.is_some())
        && (function.component || function.return_type.is_some())
}

fn builtin(table: &mut TypeTable, arity: usize, build: impl Fn(&[Type]) -> Type) -> Scheme {
    let vars: Vec<Type> = (0..arity).map(|_| table.fresh(GENERIC_LEVEL)).collect();
    let ids = vars.iter().filter_map(|var| if let Type::Var(id) = var { Some(*id) } else { None }).collect();
    Scheme { vars: ids, ty: build(&vars) }
}

fn result_span(block: &Block) -> Span {
    match block.stmts.last() {
        Some(Stmt::Expr(expr)) => expr.span.clone(),
        Some(Stmt::Val { span, .. }) => span.clone(),
        None => block.span.clone(),
    }
}

fn abbreviate(text: &str) -> String {
    if text.chars().count() <= 60 {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(59).collect::<String>())
    }
}

/// Strongly connected components of the call graph among `functions`, dependencies first
/// (Tarjan). Each component is inferred together, which supports mutual recursion.
fn components(functions: &[&Function]) -> Vec<Vec<usize>> {
    let index: BTreeMap<&str, usize> =
        functions.iter().enumerate().map(|(position, function)| (function.name.as_str(), position)).collect();
    let edges: Vec<Vec<usize>> = functions
        .iter()
        .map(|function| {
            let mut names = BTreeSet::new();
            if let Some(block) = &function.expr {
                collect_block(block, &mut names);
            }
            names.iter().filter_map(|name| index.get(name.as_str()).copied()).collect()
        })
        .collect();
    let mut tarjan = Tarjan {
        edges: &edges,
        next: 0,
        indices: vec![None; functions.len()],
        low: vec![0; functions.len()],
        stack: Vec::new(),
        on_stack: vec![false; functions.len()],
        output: Vec::new(),
    };
    for vertex in 0..functions.len() {
        if tarjan.indices[vertex].is_none() {
            tarjan.visit(vertex);
        }
    }
    tarjan.output
}

struct Tarjan<'a> {
    edges: &'a [Vec<usize>],
    next: usize,
    indices: Vec<Option<usize>>,
    low: Vec<usize>,
    stack: Vec<usize>,
    on_stack: Vec<bool>,
    output: Vec<Vec<usize>>,
}

impl Tarjan<'_> {
    fn visit(&mut self, vertex: usize) {
        self.indices[vertex] = Some(self.next);
        self.low[vertex] = self.next;
        self.next += 1;
        self.stack.push(vertex);
        self.on_stack[vertex] = true;
        let edges = self.edges;
        for &other in &edges[vertex] {
            match self.indices[other] {
                None => {
                    self.visit(other);
                    self.low[vertex] = self.low[vertex].min(self.low[other]);
                }
                Some(index) if self.on_stack[other] => self.low[vertex] = self.low[vertex].min(index),
                Some(_) => {}
            }
        }
        if Some(self.low[vertex]) == self.indices[vertex] {
            let mut component = Vec::new();
            while let Some(member) = self.stack.pop() {
                self.on_stack[member] = false;
                component.push(member);
                if member == vertex {
                    break;
                }
            }
            component.sort_unstable();
            self.output.push(component);
        }
    }
}

/// Names referenced by a body (over-approximated: shadowing is ignored).
fn collect_block(block: &Block, names: &mut BTreeSet<String>) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Val { value, .. } | Stmt::Expr(value) => collect_expr(value, names),
        }
    }
}

fn collect_expr(expr: &Expr, names: &mut BTreeSet<String>) {
    match &expr.kind {
        ExprKind::Name(name) => {
            names.insert(name.clone());
        }
        ExprKind::Field { target, name } => {
            if let ExprKind::Name(base) = &target.kind
                && base == "server"
            {
                names.insert(name.clone());
            }
            collect_expr(target, names);
        }
        ExprKind::SafeField { target, .. } => collect_expr(target, names),
        ExprKind::Call { callee, args, children } => {
            collect_expr(callee, names);
            args.iter().for_each(|arg| collect_expr(arg, names));
            if let Some(block) = children {
                collect_block(block, names);
            }
        }
        ExprKind::Unary { operand, .. } => collect_expr(operand, names),
        ExprKind::Binary { left, right, .. } => {
            collect_expr(left, names);
            collect_expr(right, names);
        }
        ExprKind::Elvis { value, fallback } => {
            collect_expr(value, names);
            collect_expr(fallback, names);
        }
        ExprKind::If { condition, then_branch, else_branch } => {
            collect_expr(condition, names);
            collect_expr(then_branch, names);
            collect_expr(else_branch, names);
        }
        ExprKind::When { scrutinee, arms } => {
            collect_expr(scrutinee, names);
            arms.iter().for_each(|arm| collect_expr(&arm.body, names));
        }
        ExprKind::Lambda { body, .. } | ExprKind::Block(body) => collect_block(body, names),
        ExprKind::Return(value) => collect_expr(value, names),
        ExprKind::Builder { target, fields } => {
            collect_expr(target, names);
            fields.iter().for_each(|field| collect_expr(&field.value, names));
        }
        ExprKind::Int(_) | ExprKind::Double(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Bool(_) | ExprKind::Null => {}
    }
}
```

- [ ] **Step 5: Rodar os testes**

Run: `cargo test types:: 2>&1 | tail -5`
Expected: PASS (todos os testes de `types`, 17 novos em `infer`).

Se um teste de esquema falhar só pela ordem dos nomes `'a`/`'b`, confira a ordem de aparição em `display` antes de mudar a expectativa: a ordem é parâmetros da esquerda para a direita, depois o resultado.

Run: `cargo clippy --all-targets -- -D warnings -A dead_code 2>&1 | tail -3`
Expected: sem warnings.

- [ ] **Step 6: Commit**

```bash
git add src/types/infer.rs src/types/mod.rs
git commit -m "feat(types): inferência Hindley-Milner, when exaustivo e ambiguidade

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 8: Formas do Bind: `db`, `server`, `environment` e DSL de UI

**Files:**
- Modify: `src/types/infer.rs`, `src/types/mod.rs`

**Interfaces:**
- Consumes: Task 7; `DatabaseSchema`, `Table`, `Column` (`src/schema.rs`); `UI_NODES` (Task 6).
- Produces: `infer::check_program(program: &Program, env: &Env, schema: &DatabaseSchema)`; `types::check` passa a usar o schema (renomeie `_schema` para `schema`).
- Regras (adendo A da spec): `db.<t>.require(k): Modelo`, `db.<t>.find(k): Modelo?`, `db.<t>.insert { c = e }: Modelo`; `server.f` tem o esquema da função `f`; valores de `environment` são globais (`T?` quando `envOrNull`); `UI { … }` e os nós de `UI_NODES` têm tipo `Ui`, argumentos `String`/`Int`, filhos `Ui`; `Text(x)` exige `x` exibível. Um nome declarado pelo usuário (local, parâmetro, função) sempre vence a forma embutida.

- [ ] **Step 1: Escrever os testes que falham**

Adicione ao `mod tests` de `src/types/infer.rs`:

```rust
    use crate::schema::DatabaseSchema;
    use crate::types::testing::infer_with_schema;

    const CLIENTES: &str = "table,model,primary_key,column,type,nullable,required,has_default,engine\nclientes,Cliente,id,id,ClienteId,false,true,true,postgresql\nclientes,Cliente,id,nome,String,false,true,false,postgresql\nclientes,Cliente,id,apelido,String,true,false,false,postgresql\n";

    const ENTIDADE: &str = "distinct type ClienteId = UUID\nentity Cliente(val id: ClienteId, val nome: String, val apelido: String?)\n";

    fn schema(csv: &str) -> DatabaseSchema {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("bind-types-{}-{id}.csv", std::process::id()));
        std::fs::write(&path, csv).unwrap();
        let schema = DatabaseSchema::load(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        schema
    }

    fn with_db(source: &str) -> Result<Vec<(String, String)>, Vec<String>> {
        infer_with_schema(&format!("{ENTIDADE}{source}"), &schema(CLIENTES))
    }

    fn db_scheme(source: &str, name: &str) -> String {
        let schemes = with_db(source).unwrap_or_else(|codes| panic!("{codes:?}"));
        schemes.into_iter().find(|(other, _)| other == name).unwrap().1
    }

    #[test]
    fn database_operations() {
        assert_eq!(
            db_scheme("server fun buscar(id: ClienteId): Cliente {\n    return db.clientes.require(id)\n}", "buscar"),
            "(ClienteId) -> Cliente"
        );
        assert_eq!(db_scheme("server fun talvez(id: ClienteId): Cliente? = db.clientes.find(id)", "talvez"), "(ClienteId) -> Cliente?");
        assert_eq!(
            db_scheme("server fun criar(nome: String): Cliente {\n    return db.clientes.insert {\n        nome = nome\n        apelido = null\n    }\n}", "criar"),
            "(String) -> Cliente"
        );
    }

    #[test]
    fn database_errors() {
        assert_eq!(with_db("server fun f(id: ClienteId): Cliente = db.pedidos.require(id)"), Err(vec!["E7104".into()]));
        assert_eq!(with_db("server fun f(id: String): Cliente = db.clientes.require(id)"), Err(vec!["E7101".into()]));
        assert_eq!(with_db("server fun f(n: Int): Cliente = db.clientes.insert { nome = n }"), Err(vec!["E7101".into()]));
        assert_eq!(with_db("server fun f(n: String): Cliente = db.clientes.insert { idade = n }"), Err(vec!["E7104".into()]));
        assert_eq!(with_db("server fun f(id: ClienteId): Cliente = db.clientes.delete(id)"), Err(vec!["E7004".into()]));
        assert_eq!(with_db("fun f(c: Cliente) = c.nome { x = 1 }"), Err(vec!["E7004".into()]));
    }

    #[test]
    fn server_calls_and_environment_values() {
        let source = "environment development {\n    secret val token: String = env(\"T\", default = \"x\")\n    runtime val porta: Int = envOrNull(\"PORT\")\n}\nserver fun eco(x: Int): Int = x\nclient fun usa(): Int = server.eco(1)\nclient fun segredo(): String = token\nfun porta2() = porta ?: 80\n";
        assert_eq!(scheme(source, "usa"), "() -> Int");
        assert_eq!(scheme(source, "segredo"), "() -> String");
        assert_eq!(scheme(source, "porta2"), "() -> Int");
        assert_eq!(errors("client fun f(): Int = server.nada(1)"), vec!["E7103"]);
    }

    #[test]
    fn ui_trees() {
        let page = "data class Item(val nome: String, val preco: Money)\nclient component Pagina(item: Item) {\n    UI {\n        Section(\"hero\", \"top\") {\n            Heading(1, \"titulo\") { Text(item.nome) }\n            Text(item.preco)\n        }\n    }\n}\n";
        assert_eq!(scheme(page, "Pagina"), "(Item) -> Ui");
        assert_eq!(errors("client component P(xs: List<String>) {\n    UI { Text(xs) }\n}"), vec!["E7105"]);
        assert_eq!(errors("client component P() {\n    UI { Div(true) { Text(\"a\") } }\n}"), vec!["E7101"]);
        assert_eq!(errors("client component P() {\n    1\n}"), vec!["E7101"]);
        assert_eq!(errors("client component P() {\n    UI { 1 }\n}"), vec!["E7101"]);
        assert_eq!(errors("fun f(x: Int) = f(x) { Text(\"a\") }"), vec!["E7004"]);
    }

    #[test]
    fn user_names_shadow_bind_forms() {
        assert_eq!(scheme("fun Text(x: Int): Int = x\nfun f() = Text(1)", "f"), "() -> Int");
        assert_eq!(scheme("data class D(val clientes: Int)\nfun f(db: D) = db.clientes", "f"), "(D) -> Int");
        assert_eq!(scheme("data class S(val eco: Int)\nfun f(server: S) = server.eco", "f"), "(S) -> Int");
    }
```

- [ ] **Step 2: Rodar e confirmar a falha**

Run: `cargo test types::infer 2>&1 | tail -8`
Expected: FAIL (os novos testes falham com E7103 para `db`/`server`/`UI`/`token`).

- [ ] **Step 3: Implementar**

1. `src/types/mod.rs`: renomeie `_schema` para `schema` e troque a chamada por `infer::check_program(program, &env, schema)`.

2. `src/types/infer.rs`, imports: adicione `use crate::schema::{Column, DatabaseSchema};` e troque `use super::env::{Env, rigid_vars};` por `use super::env::{Env, UI_NODES, rigid_vars};`.

3. `check_program` e `Checker::new` recebem o schema:

```rust
pub fn check_program(program: &Program, env: &Env, schema: &DatabaseSchema) -> (TypedProgram, Vec<Diagnostic>) {
    let mut checker = Checker::new(env, schema);
    checker.run(program);
    let typed = checker.finish(program);
    (typed, checker.diagnostics)
}
```
Adicione o campo `schema: &'a DatabaseSchema,` ao struct (depois de `env`), o parâmetro `schema: &'a DatabaseSchema` a `new` e `schema,` ao literal.

4. Em `run`, logo depois de `self.install_builtins();`, adicione `self.install_environment(program);` e o método:

```rust
    fn install_environment(&mut self, program: &Program) {
        for declaration in &program.environment {
            let base = self.lower_ref(&declaration.type_ref, &declaration.span, &BTreeMap::new(), true);
            let ty = if declaration.optional { Type::option(base) } else { base };
            self.globals.insert(declaration.name.clone(), Scheme::mono(ty));
        }
    }
```

5. Em `infer_call`, substitua o primeiro `if let … listOf …` por:

```rust
        if let ExprKind::Name(name) = &callee.kind
            && !self.is_shadowed(name)
        {
            if UI_NODES.contains(&name.as_str()) {
                return self.infer_ui(name, args, children, span);
            }
            if name == "listOf" {
                return self.infer_list_of(args, children, span);
            }
        }
        if let Some((table, method)) = db_method(callee)
            && !self.is_shadowed("db")
        {
            return self.infer_db_call(table, method, args, span);
        }
```

6. Em `infer_field`, antes do bloco do enum, adicione:

```rust
        if let ExprKind::Name(base) = &target.kind
            && !self.is_shadowed(base)
        {
            if base == "server" {
                return match self.globals.get(name).cloned() {
                    Some(scheme) => instantiate(&mut self.table, &scheme, self.level),
                    None => {
                        self.error("E7103", format!("`server.{name}` does not name a function."), span);
                        Type::Error
                    }
                };
            }
            if base == "db" {
                self.error(
                    "E7004",
                    "Database tables are used through `db.<table>.require(...)`, `find(...)` or `insert { ... }`.",
                    span,
                );
                return Type::Error;
            }
        }
```

7. Em `infer_name`, antes do erro E7103, adicione:

```rust
        if UI_NODES.contains(&name) {
            self.error("E7004", format!("UI node `{name}` must be called, e.g. `{name}(...)`."), span);
            return Type::Error;
        }
```

8. Troque o braço `ExprKind::Builder { fields, .. } => self.infer_builder(fields, &expr.span),` por `ExprKind::Builder { target, fields } => self.infer_builder(target, fields, &expr.span),` e substitua `infer_builder` por:

```rust
    fn infer_builder(&mut self, target: &Expr, fields: &[FieldInit], span: &Span) -> Type {
        let insert = db_method(target).filter(|(_, method)| *method == "insert" && !self.is_shadowed("db"));
        let Some((table, _)) = insert else {
            for field in fields {
                self.infer(&field.value);
            }
            self.error("E7004", "`{ field = value }` blocks are only supported on `db.<table>.insert`.", span);
            return Type::Error;
        };
        let schema = self.schema;
        let Some((_, model)) = self.db_table(table, span) else {
            for field in fields {
                self.infer(&field.value);
            }
            return Type::Error;
        };
        let columns = &schema.tables[table].columns;
        let mut seen = BTreeSet::new();
        for field in fields {
            let found = self.infer(&field.value);
            if !seen.insert(field.name.as_str()) {
                self.error("E7110", format!("Column `{}` is assigned more than once.", field.name), &field.span);
                continue;
            }
            match columns.get(&field.name) {
                Some(column) => {
                    let expected = self.column_type(column, &field.span);
                    let reason = Some(format!("column `{table}.{}`", field.name));
                    self.unify_at(&expected, &found, &field.value.span, reason);
                }
                None => self.error("E7104", format!("Table `{table}` has no column `{}`.", field.name), &field.span),
            }
        }
        model
    }
```

9. Adicione os métodos:

```rust
    fn infer_db_call(&mut self, table: &str, method: &str, args: &[Expr], span: &Span) -> Type {
        let Some((key, model)) = self.db_table(table, span) else {
            for arg in args {
                self.infer(arg);
            }
            return Type::Error;
        };
        if !matches!(method, "require" | "find") {
            for arg in args {
                self.infer(arg);
            }
            self.error(
                "E7004",
                format!("Unsupported table operation `{method}`; use `require`, `find` or `insert {{ ... }}`."),
                span,
            );
            return Type::Error;
        }
        if args.len() != 1 {
            for arg in args {
                self.infer(arg);
            }
            self.error(
                "E7102",
                format!("`db.{table}.{method}` expects 1 argument (the primary key), but {} were given.", args.len()),
                span,
            );
            return Type::Error;
        }
        let found = self.infer(&args[0]);
        self.unify_at(&key, &found, &args[0].span, Some(format!("the primary key of table `{table}`")));
        if method == "find" { Type::option(model) } else { model }
    }

    /// The primary-key type and model type of a table, which must map to a declared entity.
    fn db_table(&mut self, table: &str, span: &Span) -> Option<(Type, Type)> {
        let schema = self.schema;
        let Some(definition) = schema.tables.get(table) else {
            self.error("E7104", format!("Unknown database table `{table}`."), span);
            return None;
        };
        let is_entity = self.env.types.get(&definition.model).is_some_and(|info| info.kind == TypeKind::Entity);
        if !is_entity {
            self.error(
                "E7103",
                format!("Table `{table}` maps to `{}`, which is not a declared entity.", definition.model),
                span,
            );
            return None;
        }
        let key = match definition.columns.get(&definition.primary_key) {
            Some(column) => self.column_type(column, span),
            None => Type::Error,
        };
        Some((key, Type::con(&definition.model)))
    }

    fn column_type(&mut self, column: &Column, span: &Span) -> Type {
        let base = match parse_type_text(&column.type_name, span) {
            Ok(te) => self.env.lower(&te, &BTreeMap::new(), &mut self.diagnostics),
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                Type::Error
            }
        };
        if column.nullable { Type::option(base) } else { base }
    }

    fn infer_ui(&mut self, name: &str, args: &[Expr], children: Option<&Block>, span: &Span) -> Type {
        if name == "Text" {
            if args.len() != 1 {
                self.error("E7102", format!("`Text` expects 1 argument, but {} were given.", args.len()), span);
            }
            for arg in args {
                let ty = self.infer(arg);
                let resolved = self.table.resolve(&ty);
                if !self.env.is_displayable(&resolved) {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "E7105",
                            format!("`Text` cannot display a value of type `{}`.", display(&resolved)),
                            arg.span.clone(),
                        )
                        .with_hint("Display a String, number, Boolean, domain scalar or a distinct type over one of them."),
                    );
                }
            }
            if let Some(block) = children {
                self.error("E7004", "`Text` cannot have children.", &block.span);
            }
            return Type::con("Ui");
        }
        for arg in args {
            let ty = self.infer(arg);
            match self.table.shallow(&ty) {
                Type::Error => {}
                Type::Con(type_name, type_args) if type_args.is_empty() && (type_name == "String" || type_name == "Int") => {}
                other => {
                    let shown = self.shown(&other);
                    self.error("E7101", format!("UI node arguments must be `String` or `Int`, found `{shown}`."), &arg.span);
                }
            }
        }
        if let Some(block) = children {
            let mark = self.locals.len();
            for stmt in &block.stmts {
                match stmt {
                    Stmt::Val { name, annotation, value, span } => self.bind_val(name, annotation.as_ref(), value, span),
                    Stmt::Expr(expr) => {
                        let ty = self.infer(expr);
                        self.unify_at(&Type::con("Ui"), &ty, &expr.span, Some("a UI child block".into()));
                    }
                }
            }
            self.locals.truncate(mark);
            self.record(block.id, &Type::con("Ui"), &block.span);
        }
        Type::con("Ui")
    }
```

10. Adicione a função livre:

```rust
/// `db.<table>.<method>` as written, before any shadowing check.
fn db_method(expr: &Expr) -> Option<(&str, &str)> {
    let ExprKind::Field { target, name: method } = &expr.kind else { return None };
    let ExprKind::Field { target: root, name: table } = &target.kind else { return None };
    match &root.kind {
        ExprKind::Name(db) if db == "db" => Some((table.as_str(), method.as_str())),
        _ => None,
    }
}
```

- [ ] **Step 4: Rodar os testes**

Run: `cargo test types:: 2>&1 | tail -5`
Expected: PASS.

Run: `cargo clippy --all-targets -- -D warnings -A dead_code 2>&1 | tail -3`
Expected: sem warnings.

- [ ] **Step 5: Commit**

```bash
git add src/types/infer.rs src/types/mod.rs
git commit -m "feat(types): tipos para db, server, environment e a DSL de UI

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 9: Integração ao compilador e regressão

**Files:**
- Modify: `src/compiler.rs`

**Interfaces:**
- Consumes: `crate::types::check(&Program, &DatabaseSchema)`.
- Produces: `compiler::verify` roda o verificador de tipos depois de carregar o schema e resolver o target, e retorna sem `application` se houver erro E7xxx, antes da análise antiga.

- [ ] **Step 1: Escrever os testes que falham**

No `mod tests` de `src/compiler.rs`, troque a asserção de `nominal_ids_do_not_mix`:

```rust
        assert!(result.diagnostics.iter().any(|item| item.code == "E7101"), "{:?}", result.diagnostics);
```

E adicione:

```rust
    #[test]
    fn type_errors_abort_before_the_legacy_analysis() {
        let (root, config) = fixture("pure fun f(): Int = \"texto\"\n", EMPTY_SCHEMA);
        let result = verify(&config, "aws-lambda");
        assert!(result.application.is_none());
        let codes: Vec<_> = result.diagnostics.iter().map(|item| item.code).collect();
        assert_eq!(codes, vec!["E7101"], "{:?}", result.diagnostics);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn type_errors_are_ordered_by_file_and_line() {
        let (root, config) = fixture("pure fun b(): Int = \"x\"\npure fun c(): Int = true\n", EMPTY_SCHEMA);
        fs::write(root.join("app/zeta.bd"), "pure fun a(): Int = \"y\"\n").unwrap();
        let result = verify(&config, "aws-lambda");
        let positions: Vec<_> = result
            .diagnostics
            .iter()
            .map(|item| (item.span.file.clone(), item.span.line))
            .collect();
        assert_eq!(
            positions,
            vec![
                ("app/main.bd".to_string(), 1),
                ("app/main.bd".to_string(), 2),
                ("app/zeta.bd".to_string(), 1)
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }
```

- [ ] **Step 2: Rodar e confirmar a falha**

Run: `cargo test compiler:: 2>&1 | tail -8`
Expected: FAIL nos três testes (o verificador ainda não roda em `verify`).

- [ ] **Step 3: Ligar o verificador**

Em `src/compiler.rs::verify`, logo depois do bloco `let Some(target_config) = … else { … };` e antes de `let mut graph = ApplicationGraph::default();`:

```rust
    let (_typed, type_diagnostics) = crate::types::check(&program, &database);
    let type_errors = has_errors(&type_diagnostics);
    diagnostics.extend(type_diagnostics);
    if type_errors {
        return CompileResult {
            application: None,
            diagnostics,
            module_count: files.len(),
        };
    }
```

- [ ] **Step 4: Rodar a suíte inteira e o clippy sem exceções**

Run: `cargo test 2>&1 | tail -5`
Expected: PASS (todos).

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -5`
Expected: sem warnings. Se aparecer `dead_code`, o item realmente não é usado em código de produção: remova-o, ou, se for parte da interface para os subprojetos 2 e 3 (só `TypedProgram`), mantenha o `#[allow(dead_code)]` já existente. Não adicione outros `allow`.

- [ ] **Step 5: Regressão do exemplo, do site e dos artefatos**

```bash
export DATABASE_URL=postgresql://bind:bind@localhost/bind
cargo run --quiet -- check --target aws-lambda
cargo run --quiet -- check --target vercel
make -C ../website check
rm -rf /tmp/bind-after && mkdir -p /tmp/bind-after
cargo run --quiet -- generate --target aws-lambda --out /tmp/bind-after/aws-lambda
cargo run --quiet -- generate --target vercel --out /tmp/bind-after/vercel
(cd ../website && cargo run --quiet --manifest-path ../bind-lang/Cargo.toml -- generate --manifest bind.toml --target vercel --out /tmp/bind-after/website)
(cd /tmp/bind-after && find . -type f -print0 | sort -z | xargs -0 shasum -a 256) > /tmp/bind-after.sha256
diff /tmp/bind-baseline.sha256 /tmp/bind-after.sha256 && echo "artefatos idênticos"
```
Expected: todos os `check` terminam com código 0, e a última linha imprime `artefatos idênticos`. Qualquer diferença de hash é um bug desta implementação (o codegen não deveria mudar): investigue antes de seguir.

- [ ] **Step 6: Commit**

```bash
git add src/compiler.rs
git commit -m "feat(compiler): verificação de tipos antes da análise legada

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 10: Testes golden

**Files:**
- Create: `src/types/golden.rs`, `tests/typecheck/*.bd`, `tests/typecheck/*.expected`, `tests/typecheck/db.csv`
- Modify: `src/types/mod.rs` (`#[cfg(test)] mod golden;`)

**Interfaces:**
- Consumes: `types::check`, lexer, parser, `DatabaseSchema::load`.
- Formato dos casos: a primeira linha de cada `.bd` é `// expect: ok` ou `// expect: E7101 E7201` (códigos esperados, em qualquer ordem). Um `<caso>.csv` ao lado vira o schema do banco. A saída de um caso `ok` é uma linha `nome : esquema` por função, na ordem do arquivo; a de um caso com erro é uma linha `CÓDIGO linha:coluna mensagem` por diagnóstico. `BIND_BLESS=1 cargo test golden` regrava os `.expected`.

- [ ] **Step 1: Escrever o runner**

`src/types/golden.rs`:

```rust
//! Golden tests: `tests/typecheck/<case>.bd` against `<case>.expected`.
//! The first line of each case declares the expected error codes (`// expect: ok` or codes),
//! which is checked independently of the blessed output.

use std::fs;
use std::path::{Path, PathBuf};

use crate::diagnostic::{Severity, has_errors};
use crate::lexer::lex;
use crate::parser::parse;
use crate::schema::DatabaseSchema;

fn run_case(path: &Path) -> (String, Vec<String>) {
    let source = fs::read_to_string(path).unwrap();
    let file = path.file_name().unwrap().to_string_lossy().to_string();
    let (tokens, mut diagnostics) = lex(&file, &source);
    let (program, mut parse_diagnostics) = parse(tokens);
    diagnostics.append(&mut parse_diagnostics);
    let mut schemes = None;
    if !has_errors(&diagnostics) {
        let schema_path = path.with_extension("csv");
        let schema = if schema_path.exists() { DatabaseSchema::load(&schema_path).unwrap() } else { DatabaseSchema::default() };
        let (typed, mut type_diagnostics) = super::check(&program, &schema);
        diagnostics.append(&mut type_diagnostics);
        schemes = Some(typed.schemes);
    }
    let errors: Vec<_> = diagnostics.iter().filter(|item| item.severity == Severity::Error).collect();
    let mut lines = Vec::new();
    let mut codes: Vec<String> = errors.iter().map(|item| item.code.to_string()).collect();
    if errors.is_empty()
        && let Some(schemes) = schemes
    {
        for function in &program.functions {
            lines.push(format!("{} : {}", function.name, schemes[&function.name]));
        }
    }
    for item in errors {
        lines.push(format!("{} {}:{} {}", item.code, item.span.line, item.span.column, item.message));
    }
    codes.sort();
    codes.dedup();
    (lines.join("\n") + "\n", codes)
}

fn declared_codes(source: &str) -> Vec<String> {
    let first = source.lines().next().unwrap_or_default();
    let list = first.strip_prefix("// expect:").expect("a golden case must start with `// expect:`").trim();
    let mut codes: Vec<String> = if list == "ok" { Vec::new() } else { list.split_whitespace().map(str::to_string).collect() };
    codes.sort();
    codes.dedup();
    codes
}

#[test]
fn golden_cases() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/typecheck");
    let bless = std::env::var_os("BIND_BLESS").is_some();
    let mut cases: Vec<PathBuf> = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "bd"))
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "no golden cases in {}", directory.display());
    let mut failures = Vec::new();
    for case in cases {
        let expected_codes = declared_codes(&fs::read_to_string(&case).unwrap());
        let (output, codes) = run_case(&case);
        if codes != expected_codes {
            failures.push(format!("{}: expected {expected_codes:?}, got {codes:?}\n{output}", case.display()));
            continue;
        }
        let expected_path = case.with_extension("expected");
        if bless {
            fs::write(&expected_path, &output).unwrap();
            continue;
        }
        let expected = fs::read_to_string(&expected_path).unwrap_or_default();
        if expected != output {
            failures.push(format!("{}: output differs\n--- expected\n{expected}--- actual\n{output}", case.display()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
```

Em `src/types/mod.rs`, adicione depois das declarações de módulo:

```rust
#[cfg(test)]
mod golden;
```

- [ ] **Step 2: Criar os casos que passam (com `.expected` escritos à mão)**

`tests/typecheck/infer_basics.bd`:
```kotlin
// expect: ok
fun ident(x) = x
fun constante(x, y) = x
fun compose(f, g, x) = f(g(x))
fun dobro(x) = x * 2
fun aplica(f, x) = f(x)
fun concat(a, b) = a + b + ""
```
`tests/typecheck/infer_basics.expected`:
```
ident : ('a) -> 'a
constante : ('a, 'b) -> 'a
compose : (('a) -> 'b, ('c) -> 'a, 'c) -> 'b
dobro : (Int) -> Int
aplica : (('a) -> 'b, 'a) -> 'b
concat : (String, String) -> String
```

`tests/typecheck/generics.bd`:
```kotlin
// expect: ok
fun <T> primeiro(xs: List<T>): T? = first(xs)
fun <A, B> mapear(xs: List<A>, f: (A) -> B): List<B> = map(xs, f)
fun tamanho(xs: List<String>): Int = length(xs)
fun usa(): Int? = primeiro(listOf(1, 2, 3))
```
`tests/typecheck/generics.expected`:
```
primeiro : (List<'a>) -> 'a?
mapear : (List<'a>, ('a) -> 'b) -> List<'b>
tamanho : (List<String>) -> Int
usa : () -> Int?
```

`tests/typecheck/recursion.bd`:
```kotlin
// expect: ok
fun par(n) = if (n == 0) true else impar(n - 1)
fun impar(n) = if (n == 0) false else par(n - 1)
fun fatorial(n) = if (n <= 1) 1 else n * fatorial(n - 1)
```
`tests/typecheck/recursion.expected`:
```
par : (Int) -> Boolean
impar : (Int) -> Boolean
fatorial : (Int) -> Int
```

`tests/typecheck/adt.bd`:
```kotlin
// expect: ok
sealed class Arvore<T> {
    data class No(val esq: Arvore<T>, val valor: T, val dir: Arvore<T>)
    data object Folha
}

fun tamanho(arvore) = when (arvore) {
    Folha -> 0
    No(esq, _, dir) -> tamanho(esq) + 1 + tamanho(dir)
}

fun soma(arvore: Arvore<Int>): Int = when (arvore) {
    Folha -> 0
    No(esq, valor, dir) -> soma(esq) + valor + soma(dir)
}
```
`tests/typecheck/adt.expected`:
```
tamanho : (Arvore<'a>) -> Int
soma : (Arvore<Int>) -> Int
```

`tests/typecheck/option.bd`:
```kotlin
// expect: ok
fun ouZero(x: Int?): Int = x ?: 0
fun descreve(x: Int?): String = when (x) {
    null -> "nada"
    n -> "algo"
}
data class Cliente(val nome: String, val apelido: String?)
fun apelidoOuNome(c: Cliente?): String = c?.apelido ?: "anônimo"
fun metade(x: Int) = x / 2
```
`tests/typecheck/option.expected`:
```
ouZero : (Int?) -> Int
descreve : (Int?) -> String
apelidoOuNome : (Cliente?) -> String
metade : (Int) -> Int?
```

`tests/typecheck/db.csv`:
```
table,model,primary_key,column,type,nullable,required,has_default,engine
clientes,Cliente,id,id,ClienteId,false,true,true,postgresql
clientes,Cliente,id,nome,String,false,true,false,postgresql
clientes,Cliente,id,email,Email,false,true,false,postgresql
```

`tests/typecheck/db.bd`:
```kotlin
// expect: ok
distinct type ClienteId = UUID
entity Cliente(val id: ClienteId, val nome: String, val email: Email)
server fun buscar(id: ClienteId): Cliente effects [db.read] {
    return db.clientes.require(id)
}
server fun talvez(id: ClienteId): Cliente? effects [db.read] = db.clientes.find(id)
server fun criar(nome: String, email: Email): Cliente effects [db.write] {
    return db.clientes.insert {
        nome = nome
        email = email
    }
}
client component Pagina(id: ClienteId) {
    val cliente = server.buscar(id)
    UI {
        Div("cartao") {
            Text(cliente.nome)
            Text(cliente.email)
        }
    }
}
```
`tests/typecheck/db.expected`:
```
buscar : (ClienteId) -> Cliente
talvez : (ClienteId) -> Cliente?
criar : (String, Email) -> Cliente
Pagina : (ClienteId) -> Ui
```

- [ ] **Step 3: Criar os casos de erro (um por erro da spec)**

Cada arquivo abaixo vai em `tests/typecheck/` com exatamente este conteúdo:

| Arquivo | Conteúdo |
|---|---|
| `err_mismatch.bd` | `// expect: E7101`<br>`distinct type ClienteId = UUID`<br>`distinct type PedidoId = UUID`<br>`fun pedido(id: PedidoId): Unit {}`<br>`fun errado(id: ClienteId): Unit { pedido(id) }` |
| `err_division.bd` | `// expect: E7101`<br>`fun metade(x: Int): Int = x / 2` |
| `err_unknown.bd` | `// expect: E7103`<br>`fun f() = naoExiste + 1` |
| `err_arity.bd` | `// expect: E7102`<br>`fun soma2(a: Int, b: Int): Int = a + b`<br>`fun usa(): Int = soma2(1)` |
| `err_infinite.bd` | `// expect: E7107`<br>`fun auto(x) = x(x)` |
| `err_ambiguous.bd` | `// expect: E7302`<br>`fun soma(a, b) = a + b` |
| `err_constraint.bd` | `// expect: E7301`<br>`fun f(a: Boolean, b: Boolean) = a + b` |
| `err_eq_function.bd` | `// expect: E7301`<br>`fun g(x: Int): Int = x`<br>`fun f(): Boolean = g == g` |
| `err_nonexhaustive.bd` | `// expect: E7201`<br>`sealed class Pedido {`<br>`    data class Pendente(val itens: Int)`<br>`    data class Pago(val valor: Money)`<br>`    data object Cancelado`<br>`}`<br>`fun rotulo(p: Pedido): String = when (p) {`<br>`    Pendente(_) -> "pendente"`<br>`    Cancelado -> "cancelado"`<br>`}` |
| `err_redundant.bd` | igual ao anterior, mas com `// expect: E7202` e os braços `Pendente(_) -> "pendente"`, `_ -> "outro"`, `Cancelado -> "cancelado"` |
| `err_nullable_binding.bd` | `// expect: E7201`<br>`fun f(x: Int?): Int = when (x) { n -> n }` |
| `err_discard.bd` | `// expect: E7108`<br>`fun dobro(x: Int): Int = x * 2`<br>`fun f(): Int {`<br>`    dobro(1)`<br>`    2`<br>`}` |
| `err_removed.bd` | `// expect: E7001`<br>`fun a(): Int {`<br>`    var x = 1`<br>`    x`<br>`}`<br>`fun b(x: Int?): Int = x!!`<br>`fun c(nome: String): String = "Olá $nome"` |
| `err_if_without_else.bd` | `// expect: E7005`<br>`fun f(x: Boolean): Int = if (x) 1` |
| `err_boundary.bd` | `// expect: E7003`<br>`server fun buscar(id) = id` |
| `err_var_field.bd` | `// expect: E7002`<br>`data class Conta(var saldo: Int)` |
| `err_ui.bd` | `// expect: E7105`<br>`client component Tela(itens: List<String>) {`<br>`    UI { Text(itens) }`<br>`}` |
| `err_return_lambda.bd` | `// expect: E7111`<br>`fun f(): Int {`<br>`    val g = { x -> return x }`<br>`    g(1)`<br>`}` |
| `err_field.bd` | `// expect: E7104`<br>`data class A(val x: Int)`<br>`fun f(a: A): Int = a.y` |
| `err_double_pattern.bd` | `// expect: E7109`<br>`fun f(x: Double): Int = when (x) { 1.5 -> 1; _ -> 0 }` |

(`<br>` marca quebra de linha; não escreva `<br>` nos arquivos. No caso `err_removed.bd`, as linhas `var x = 1` e `x` são indentadas com quatro espaços.)

- [ ] **Step 4: Gerar os `.expected` dos casos de erro e revisar**

Run: `BIND_BLESS=1 cargo test golden 2>&1 | tail -5`
Expected: PASS. Os códigos já são verificados pela primeira linha de cada caso, mesmo em modo bless.

Revise cada `tests/typecheck/err_*.expected` gerado: a mensagem deve apontar para a linha e coluna do erro real e ser compreensível para quem escreveu o código. Os `.expected` dos casos `ok` escritos no Step 2 não podem ter mudado: confira com `git diff tests/typecheck/*.expected` (só arquivos novos de erro devem aparecer como não rastreados).

- [ ] **Step 5: Rodar sem bless**

Run: `cargo test golden 2>&1 | tail -5`
Expected: PASS.

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`
Expected: sem warnings.

- [ ] **Step 6: Commit**

```bash
git add src/types/golden.rs src/types/mod.rs tests/typecheck
git commit -m "test(types): casos golden de inferência e diagnósticos

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 11: Testes de propriedade

**Files:**
- Create: `src/types/props.rs`
- Modify: `Cargo.toml`, `src/types/mod.rs` (`#[cfg(test)] mod props;`)

**Interfaces:**
- Consumes: `unify`, `TypeTable`, `display`, `substitute` (Task 4); `check_match`, `Signatures`, `CtorSig`, `Pat` (Task 5); `types::check`, `TypedProgram.signatures` (Task 7); `parse_expression_tokens`, `render_block` (Tasks 2 e 3).
- Cada propriedade roda com pelo menos 1.000 casos.

- [ ] **Step 1: Adicionar a dev-dependency**

Em `Cargo.toml`, depois de `[dependencies]`:

```toml
[dev-dependencies]
proptest = "=1.11.0"
```
Run: `cargo test --no-run 2>&1 | tail -3`
Expected: compila (o `Cargo.lock` passa a incluir `proptest` e suas dependências).

- [ ] **Step 2: Escrever as propriedades**

`src/types/props.rs`:

```rust
//! Property tests for the type system. Each property runs at least 1,000 cases.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::exhaustive::{CtorSig, Pat, Signatures, check_match};
use super::ty::*;
use super::unify::unify;
use crate::diagnostic::{Span, has_errors};
use crate::lexer::{Token, TokenKind, lex};
use crate::parser::expr::parse_expression_tokens;
use crate::parser::parse;
use crate::schema::DatabaseSchema;
use crate::syntax::expr::*;

fn config() -> ProptestConfig {
    ProptestConfig::with_cases(1000)
}

// ---- 1. unification produces a unifier ---------------------------------------------

fn arb_type(vars: usize) -> impl Strategy<Value = Type> {
    let leaf = prop_oneof![(0..vars).prop_map(Type::Var), Just(Type::con("Int")), Just(Type::con("String"))];
    leaf.prop_recursive(3, 16, 2, |inner| {
        prop_oneof![
            inner.clone().prop_map(Type::list),
            inner.clone().prop_map(Type::option),
            (proptest::collection::vec(inner.clone(), 0..3), inner)
                .prop_map(|(params, result)| Type::Fun(params, Box::new(result))),
        ]
    })
}

proptest! {
    #![proptest_config(config())]
    #[test]
    fn unification_produces_a_unifier(a in arb_type(4), b in arb_type(4)) {
        let mut table = TypeTable::default();
        for _ in 0..4 {
            table.fresh(1);
        }
        if unify(&mut table, &a, &b).is_ok() {
            prop_assert_eq!(table.resolve(&a), table.resolve(&b));
        }
    }
}

// ---- 2 and 3. principal types and stability ------------------------------------------

struct Entry {
    name: &'static str,
    params: &'static [&'static str],
    body: &'static str,
}

const CORPUS: &[Entry] = &[
    Entry { name: "ident", params: &["a"], body: "a" },
    Entry { name: "constante", params: &["a", "b"], body: "a" },
    Entry { name: "aplica", params: &["f", "x"], body: "f(x)" },
    Entry { name: "compose", params: &["f", "g", "x"], body: "f(g(x))" },
    Entry { name: "dobro", params: &["a"], body: "a * 2" },
    Entry { name: "ouPadrao", params: &["a", "b"], body: "a ?: b" },
    Entry { name: "cabeca", params: &["xs"], body: "first(xs)" },
    Entry { name: "par", params: &["a", "b"], body: "listOf(a, b)" },
    Entry { name: "troca", params: &["f", "a", "b"], body: "f(b, a)" },
    Entry { name: "igualA", params: &["a", "b"], body: "a == b + 1" },
];

fn source(entry: &Entry) -> String {
    format!("fun {}({}) = {}\n", entry.name, entry.params.join(", "), entry.body)
}

/// The signature of `name`, or `None` if the source does not parse or type-check.
fn signature_of(source: &str, name: &str) -> Option<Type> {
    let (tokens, mut diagnostics) = lex("prop.bd", source);
    let (program, mut parse_diagnostics) = parse(tokens);
    diagnostics.append(&mut parse_diagnostics);
    if has_errors(&diagnostics) {
        return None;
    }
    let (typed, type_diagnostics) = super::check(&program, &DatabaseSchema::default());
    if has_errors(&type_diagnostics) {
        return None;
    }
    typed.signatures.get(name).cloned()
}

/// Bind source for a type, naming variables `T0`, `T1`...
fn to_source(ty: &Type, names: &mut BTreeMap<TypeVarId, String>) -> String {
    match ty {
        Type::Var(id) => {
            let next = names.len();
            names.entry(*id).or_insert_with(|| format!("T{next}")).clone()
        }
        Type::Con(name, args) if name == "Option" => {
            let inner = to_source(&args[0], names);
            if matches!(args[0], Type::Fun(..)) { format!("({inner})?") } else { format!("{inner}?") }
        }
        Type::Con(name, args) if args.is_empty() => name.clone(),
        Type::Con(name, args) => {
            let rendered: Vec<String> = args.iter().map(|arg| to_source(arg, names)).collect();
            format!("{name}<{}>", rendered.join(", "))
        }
        Type::Fun(params, result) => {
            let rendered: Vec<String> = params.iter().map(|param| to_source(param, names)).collect();
            format!("({}) -> {}", rendered.join(", "), to_source(result, names))
        }
        Type::Error => "Unit".into(),
    }
}

/// The corpus entry annotated with the given function type.
fn annotate(entry: &Entry, ty: &Type) -> String {
    let Type::Fun(params, result) = ty else { unreachable!("corpus entries are functions") };
    let mut names = BTreeMap::new();
    let params: Vec<String> = entry
        .params
        .iter()
        .zip(params)
        .map(|(name, param)| format!("{name}: {}", to_source(param, &mut names)))
        .collect();
    let result = to_source(result, &mut names);
    let generics = if names.is_empty() {
        String::new()
    } else {
        let mut ordered: Vec<&String> = names.values().collect();
        ordered.sort_by_key(|name| name[1..].parse::<usize>().unwrap_or(0));
        format!("<{}> ", ordered.iter().map(|name| name.as_str()).collect::<Vec<_>>().join(", "))
    };
    format!("fun {generics}{}({}): {result} = {}\n", entry.name, params.join(", "), entry.body)
}

fn is_instance(general: &Type, specific: &Type) -> bool {
    fn matches(general: &Type, specific: &Type, bound: &mut BTreeMap<TypeVarId, Type>) -> bool {
        match (general, specific) {
            (Type::Var(id), _) => match bound.get(id) {
                Some(existing) => existing == specific,
                None => {
                    bound.insert(*id, specific.clone());
                    true
                }
            },
            (Type::Con(a, xs), Type::Con(b, ys)) => {
                a == b && xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| matches(x, y, bound))
            }
            (Type::Fun(p, r), Type::Fun(q, s)) => {
                p.len() == q.len() && p.iter().zip(q).all(|(x, y)| matches(x, y, bound)) && matches(r, s, bound)
            }
            _ => false,
        }
    }
    matches(general, specific, &mut BTreeMap::new())
}

fn type_vars(ty: &Type, output: &mut Vec<TypeVarId>) {
    match ty {
        Type::Var(id) => {
            if !output.contains(id) {
                output.push(*id);
            }
        }
        Type::Con(_, args) => args.iter().for_each(|arg| type_vars(arg, output)),
        Type::Fun(params, result) => {
            params.iter().for_each(|param| type_vars(param, output));
            type_vars(result, output);
        }
        Type::Error => {}
    }
}

fn arb_ground() -> impl Strategy<Value = Type> {
    prop_oneof![
        Just(Type::con("Int")),
        Just(Type::con("String")),
        Just(Type::con("Boolean")),
        Just(Type::list(Type::con("Int"))),
        Just(Type::option(Type::con("Int"))),
        Just(Type::Fun(vec![Type::con("Int")], Box::new(Type::con("Int")))),
    ]
}

#[test]
fn annotating_with_the_inferred_type_is_stable() {
    for entry in CORPUS {
        let inferred = signature_of(&source(entry), entry.name).unwrap_or_else(|| panic!("{} must type-check", entry.name));
        let annotated = annotate(entry, &inferred);
        let again = signature_of(&annotated, entry.name).unwrap_or_else(|| panic!("annotated version rejected:\n{annotated}"));
        assert_eq!(display(&inferred), display(&again), "{annotated}");
    }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn accepted_annotations_are_instances_of_the_inferred_type(
        index in 0..CORPUS.len(),
        picks in proptest::collection::vec(arb_ground(), 4),
    ) {
        let entry = &CORPUS[index];
        let inferred = signature_of(&source(entry), entry.name).unwrap();
        let arity = entry.params.len();
        let annotation = Type::Fun(picks[..arity].to_vec(), Box::new(picks[arity].clone()));
        if let Some(accepted) = signature_of(&annotate(entry, &annotation), entry.name) {
            prop_assert!(is_instance(&inferred, &accepted), "{} accepted `{}`, not an instance of `{}`", entry.name, display(&accepted), display(&inferred));
        }
    }

    #[test]
    fn instances_of_the_inferred_type_are_accepted(
        index in 0..CORPUS.len(),
        picks in proptest::collection::vec(arb_ground(), 4),
    ) {
        let entry = &CORPUS[index];
        let inferred = signature_of(&source(entry), entry.name).unwrap();
        let mut vars = Vec::new();
        type_vars(&inferred, &mut vars);
        let mapping: BTreeMap<TypeVarId, Type> = vars.into_iter().zip(picks).collect();
        let instance = substitute(&inferred, &mapping);
        let annotated = annotate(entry, &instance);
        prop_assert!(signature_of(&annotated, entry.name).is_some(), "rejected instance:\n{}", annotated);
    }
}

// ---- 4. exhaustiveness agrees with brute force ---------------------------------------

/// A random finite universe of algebraic types. Type `i` only refers to types `j > i`, and
/// the last type has two field-less constructors, so every type has finitely many values.
struct Universe {
    types: Vec<Vec<(String, Vec<usize>)>>,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

impl Universe {
    fn random(rng: &mut Rng) -> Universe {
        // Three types keep brute-force enumeration small (at most 3 * 12 * 12 values of `T0`).
        let count = 3;
        let mut types = Vec::new();
        for index in 0..count {
            if index == count - 1 {
                types.push(vec![("Sim".to_string(), vec![]), ("Nao".to_string(), vec![])]);
                continue;
            }
            let constructors = 1 + rng.below(3);
            let ctors = (0..constructors)
                .map(|ctor| {
                    let arity = rng.below(3);
                    let args = (0..arity).map(|_| index + 1 + rng.below(count - index - 1)).collect();
                    (format!("C{index}_{ctor}"), args)
                })
                .collect();
            types.push(ctors);
        }
        Universe { types }
    }

    fn pattern(&self, rng: &mut Rng, ty: usize, depth: usize) -> Pat {
        if depth == 0 || rng.below(3) == 0 {
            return Pat::Wild;
        }
        let ctors = &self.types[ty];
        let (name, args) = &ctors[rng.below(ctors.len())];
        Pat::Ctor(name.clone(), args.iter().map(|arg| self.pattern(rng, *arg, depth - 1)).collect())
    }

    fn values(&self, ty: usize) -> Vec<Pat> {
        let mut output = Vec::new();
        for (name, args) in &self.types[ty] {
            let mut partial: Vec<Vec<Pat>> = vec![Vec::new()];
            for arg in args {
                let choices = self.values(*arg);
                partial = partial
                    .into_iter()
                    .flat_map(|prefix| {
                        choices.iter().map(move |choice| {
                            let mut next = prefix.clone();
                            next.push(choice.clone());
                            next
                        })
                    })
                    .collect();
            }
            output.extend(partial.into_iter().map(|args| Pat::Ctor(name.clone(), args)));
        }
        output
    }
}

impl Signatures for Universe {
    fn constructors(&self, ty: &Type) -> Option<Vec<CtorSig>> {
        let Type::Con(name, _) = ty else { return None };
        let index: usize = name.strip_prefix('T')?.parse().ok()?;
        Some(
            self.types[index]
                .iter()
                .map(|(ctor, args)| CtorSig {
                    name: ctor.clone(),
                    args: args.iter().map(|arg| Type::con(&format!("T{arg}"))).collect(),
                })
                .collect(),
        )
    }
}

fn matches_value(pattern: &Pat, value: &Pat) -> bool {
    match (pattern, value) {
        (Pat::Wild, _) => true,
        (Pat::Ctor(a, xs), Pat::Ctor(b, ys)) => a == b && xs.iter().zip(ys).all(|(x, y)| matches_value(x, y)),
        _ => false,
    }
}

proptest! {
    #![proptest_config(config())]
    #[test]
    fn exhaustiveness_agrees_with_brute_force(seed in any::<u64>()) {
        let mut rng = Rng(seed | 1);
        let universe = Universe::random(&mut rng);
        let row_count = 1 + rng.below(5);
        let rows: Vec<Pat> = (0..row_count).map(|_| universe.pattern(&mut rng, 0, 3)).collect();
        let report = check_match(&universe, &Type::con("T0"), &rows);
        let values = universe.values(0);
        let exhaustive = values.iter().all(|value| rows.iter().any(|row| matches_value(row, value)));
        prop_assert_eq!(report.missing.is_none(), exhaustive);
        if let Some(witness) = &report.missing {
            let real = values.iter().any(|value| matches_value(witness, value) && !rows.iter().any(|row| matches_value(row, value)));
            prop_assert!(real, "witness {:?} is matched by an arm", witness);
        }
        for index in 0..rows.len() {
            let unreachable = !values
                .iter()
                .any(|value| matches_value(&rows[index], value) && !rows[..index].iter().any(|row| matches_value(row, value)));
            prop_assert_eq!(report.redundant.contains(&index), unreachable, "arm {}", index);
        }
    }
}

// ---- 5. parser round trip ------------------------------------------------------------

fn node(kind: ExprKind) -> Expr {
    Expr { id: 0, kind, span: Span::default() }
}

fn arb_name() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "xs", "valor", "f"]).prop_map(str::to_string)
}

fn arb_pattern() -> impl Strategy<Value = Pattern> {
    let leaf = prop_oneof![
        Just(Pattern::Wildcard),
        arb_name().prop_map(|name| Pattern::Name(name, Span::default())),
        (0u32..100).prop_map(|value| Pattern::Int(value.to_string(), Span::default())),
        "[a-z ]{0,5}".prop_map(|text| Pattern::Str(text, Span::default())),
        any::<bool>().prop_map(|value| Pattern::Bool(value, Span::default())),
        Just(Pattern::Null(Span::default())),
    ];
    leaf.prop_recursive(2, 8, 2, |inner| {
        (prop::sample::select(vec!["Pago", "No"]), proptest::collection::vec(inner, 0..3)).prop_map(|(name, args)| {
            Pattern::Constructor { name: name.to_string(), args, span: Span::default() }
        })
    })
}

fn arb_binary_op() -> impl Strategy<Value = BinaryOp> {
    prop::sample::select(vec![
        BinaryOp::Add, BinaryOp::Sub, BinaryOp::Mul, BinaryOp::Div, BinaryOp::Rem, BinaryOp::Eq,
        BinaryOp::NotEq, BinaryOp::Lt, BinaryOp::LtEq, BinaryOp::Gt, BinaryOp::GtEq, BinaryOp::And, BinaryOp::Or,
    ])
}

fn arb_expr() -> impl Strategy<Value = Expr> {
    let leaf = prop_oneof![
        (0u32..1000).prop_map(|value| node(ExprKind::Int(value.to_string()))),
        "[a-z ]{0,6}".prop_map(|text| node(ExprKind::Str(text))),
        any::<bool>().prop_map(|value| node(ExprKind::Bool(value))),
        Just(node(ExprKind::Null)),
        arb_name().prop_map(|name| node(ExprKind::Name(name))),
    ];
    leaf.prop_recursive(4, 32, 3, |inner| {
        let boxed = |expr: Expr| Box::new(expr);
        prop_oneof![
            (inner.clone(), inner.clone(), arb_binary_op())
                .prop_map(move |(left, right, op)| node(ExprKind::Binary { op, left: boxed(left), right: boxed(right) })),
            (inner.clone(), any::<bool>()).prop_map(move |(operand, negative)| node(ExprKind::Unary {
                op: if negative { UnaryOp::Neg } else { UnaryOp::Not },
                operand: boxed(operand),
            })),
            (inner.clone(), inner.clone())
                .prop_map(move |(value, fallback)| node(ExprKind::Elvis { value: boxed(value), fallback: boxed(fallback) })),
            (inner.clone(), arb_name()).prop_map(move |(target, name)| node(ExprKind::Field { target: boxed(target), name })),
            (inner.clone(), arb_name()).prop_map(move |(target, name)| node(ExprKind::SafeField { target: boxed(target), name })),
            (arb_name(), proptest::collection::vec(inner.clone(), 0..3)).prop_map(move |(name, args)| node(ExprKind::Call {
                callee: boxed(node(ExprKind::Name(name))),
                args,
                children: None,
            })),
            (inner.clone(), inner.clone(), inner.clone()).prop_map(move |(condition, then_branch, else_branch)| node(ExprKind::If {
                condition: boxed(condition),
                then_branch: boxed(then_branch),
                else_branch: boxed(else_branch),
            })),
            (proptest::collection::vec(arb_name(), 0..3), inner.clone()).prop_map(|(names, body)| node(ExprKind::Lambda {
                params: names.into_iter().map(|name| (name, Span::default())).collect(),
                body: Block { id: 0, stmts: vec![Stmt::Expr(body)], span: Span::default() },
            })),
            (inner.clone(), proptest::collection::vec((arb_pattern(), inner), 1..3)).prop_map(move |(scrutinee, arms)| {
                node(ExprKind::When {
                    scrutinee: boxed(scrutinee),
                    arms: arms.into_iter().map(|(pattern, body)| Arm { pattern, body, span: Span::default() }).collect(),
                })
            }),
        ]
    })
}

fn reparse(source: &str) -> Result<String, String> {
    let (tokens, lex_diagnostics) = lex("prop.bd", source);
    if !lex_diagnostics.is_empty() {
        return Err(format!("{lex_diagnostics:?}"));
    }
    let tokens: Vec<Token> = tokens.into_iter().filter(|token| token.kind != TokenKind::Eof).collect();
    let (block, diagnostics) = parse_expression_tokens(&tokens, &Span::default());
    if !diagnostics.is_empty() {
        return Err(format!("{diagnostics:?}"));
    }
    Ok(render_block(&block))
}

proptest! {
    #![proptest_config(config())]
    #[test]
    fn parser_round_trips_rendered_expressions(expr in arb_expr()) {
        let rendered = render_expr(&expr);
        let expected = format!("{{ {rendered} }}");
        prop_assert_eq!(reparse(&rendered), Ok(expected), "source: {}", rendered);
    }
}
```

Em `src/types/mod.rs`, adicione:

```rust
#[cfg(test)]
mod props;
```

- [ ] **Step 3: Rodar as propriedades**

Run: `cargo test types::props 2>&1 | tail -10`
Expected: PASS nas cinco propriedades. Uma falha de propriedade é um bug real no código das Tasks 2 a 8 (o `proptest` mostra o contraexemplo minimizado): corrija o código, não a propriedade. As duas exceções conhecidas e aceitáveis, que devem ser tratadas ajustando o gerador e anotando o motivo num comentário, são: identificadores gerados que colidem com palavras-chave e strings com `$`.

- [ ] **Step 4: Clippy e commit**

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`
Expected: sem warnings.

```bash
git add Cargo.toml Cargo.lock src/types/props.rs src/types/mod.rs
git commit -m "test(types): propriedades de unificação, tipo principal, exaustividade e parser

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 12: Documentação formal e verificação final

**Files:**
- Create: `docs/type-system.md`
- Modify: `README.md`

**Interfaces:**
- Consumes: o comportamento implementado nas Tasks 1 a 11. A documentação descreve o que o código faz; se encontrar divergência entre o texto abaixo e o código, o código é a referência e a divergência deve ser reportada ao humano.

- [ ] **Step 1: Escrever `docs/type-system.md`**

````markdown
# Bind type system

This document specifies the static semantics that `bindc` enforces before any code is generated.
Bind follows the OCaml model (Hindley-Milner inference, algebraic data types, pattern matching) and is
stricter where OCaml lets bugs through: incomplete matches are errors, there are no exceptions and no
mutation, and boundary signatures must be written.

## Types

```text
τ ::= α                         type variable
    | C<τ1, …, τn>              constructor: Int, String, List<τ>, Result<τ, τ>, user types
    | (τ1, …, τn) -> τ          function
    | τ?                        Option<τ>

σ ::= ∀α1 … αn. τ               type scheme
```

Built-in scalars: `Unit String Int Long Float Double Decimal Boolean UUID Date DateTime Instant
Duration Money Email Url DatabaseUrl Port Secret Ui`. Built-in generics: `List<T> Set<T> Map<K, V>
Option<T>` (written `T?`) and `Result<T, E>` with constructors `Ok(value: T)` and `Err(error: E)`.

`Int` has arbitrary precision. `Double` and `Float` are IEEE 754. User types (`data class`, `entity`,
`sealed class`, `enum class`, `distinct type`) are nominal: two types are equal only if they have the
same name and equal arguments.

## Judgements

`Γ ⊢ e : τ` reads "in environment Γ, expression e has type τ". `gen(Γ, τ)` quantifies the variables of
τ that are not free in Γ (implemented with levels); `inst(σ)` replaces quantified variables with fresh
ones. `C ⊨ τ` means τ satisfies the operator constraint C.

```text
           x : σ ∈ Γ                                Γ ⊢ e1 : τ1     Γ, x : gen(Γ, τ1) ⊢ rest : τ
 (Var) ──────────────────                 (Val) ─────────────────────────────────────────────
         Γ ⊢ x : inst(σ)                          Γ ⊢ { val x = e1; rest } : τ

        Γ ⊢ f : (τ1, …, τn) -> τ    Γ ⊢ ei : τi                 Γ, x1 : τ1, …, xn : τn ⊢ b : τ
 (App) ────────────────────────────────────────    (Lam) ──────────────────────────────────────
                 Γ ⊢ f(e1, …, en) : τ                     Γ ⊢ { x1, …, xn -> b } : (τ1, …, τn) -> τ

        Γ ⊢ c : Boolean    Γ ⊢ a : τ    Γ ⊢ b : τ                Γ ⊢ a : τ?    Γ ⊢ b : τ
 (If)  ─────────────────────────────────────────    (Elvis) ─────────────────────────────
              Γ ⊢ if (c) a else b : τ                             Γ ⊢ a ?: b : τ

        Γ ⊢ a : τ    Γ ⊢ b : τ    C(⊕) ⊨ τ                     Γ ⊢ a : τ    Γ ⊢ b : τ    Num ⊨ τ
 (Op)  ─────────────────────────────────────    (Div) ──────────────────────────────────────────
              Γ ⊢ a ⊕ b : τ                            Γ ⊢ a / b : τ?  if τ ∈ {Int, Decimal}
                                                       Γ ⊢ a / b : τ   otherwise
```

Operator constraints are closed sets, ordered from weakest to strongest:

| Constraint | Accepted types | Operators |
|---|---|---|
| `Eq` | every type that contains no function and no type parameter | `==` `!=` |
| `Ord` | `Add` ∪ {Date, DateTime, Instant} | `<` `<=` `>` `>=` |
| `Add` | `Sub` ∪ {String} | `+` |
| `Sub` | `Num` ∪ {Money, Duration} | `-` (binary and unary) |
| `Num` | {Int, Double, Float, Decimal} | `*` `/` |

`%` requires `Int` operands and has type `Int?`. `&&`, `||` and `!` require `Boolean`. Literals are not
polymorphic: `2 : Int`, `2.0 : Double`, `2f : Float`.

A constrained variable is never generalized and never defaulted. If it is still unbound when its
`val` or function is generalized, or at the end of the function body, the program is rejected
(E7302).

## Functions

Top-level functions are grouped into strongly connected components of the call graph and inferred
group by group, dependencies first; each group is monomorphic inside and generalized afterwards
(OCaml's `let rec … and …`). A fully annotated function is checked against its annotation; its type
parameters are rigid inside the body, so `fun <T> f(x: T): Int = x` is rejected. Polymorphic recursion
requires annotations.

Without mutation the value restriction is unnecessary: every `val` is generalized.

`server fun`, `client component` and functions without a body must annotate every parameter and the
result (E7003). A component's result is `Ui`.

A block's value is its last expression (`Unit` if it ends with `val` or is empty). Any other
expression statement must have type `Unit`; discarding a value is an error (E7108). Write
`val _ = e` to discard deliberately. `return e` checks `e` against the function's result and is not
allowed inside lambdas (E7111).

## Patterns and exhaustiveness

```text
p ::= _ | x | literal | null | C | C(p1, …, pn)
```

A pattern is checked against the scrutinee's type. At a type already known to be `T?`, every pattern
other than `_` and `null` describes a non-null value and is checked against `T`; therefore
`when (x) { n -> … }` over `Int?` is incomplete, and `n` has type `Int`.

Every `when` is checked with Maranget's usefulness algorithm (*Warnings for pattern matching*, 2007):

- if some value matches no arm, the program is rejected and the message shows that value (E7201);
- if an arm can never be reached, the program is rejected (E7202).

`Boolean`, `T?`, `Result`, sealed classes, enums, data classes and distinct types have finitely many
constructors; other types (`Int`, `String`, …) need `_` or a variable. Floating-point literals cannot be
patterns (E7109).

## Bind-specific forms

| Form | Type |
|---|---|
| `db.t.require(k)` | `M`, where the schema maps table `t` to entity `M` and `k` has the primary-key column type |
| `db.t.find(k)` | `M?` |
| `db.t.insert { c = e … }` | `M`; each `e` has the type of column `c` (`T?` if nullable) |
| `server.f` | the scheme of function `f` |
| an `environment` value | its declared type (`T?` with `envOrNull`) |
| `UI { … }`, `Div(…) { … }`, … | `Ui`; arguments are `String` or `Int`; every child is `Ui` |
| `Text(x)` | `Ui`; `x` is a scalar, or a distinct type over one (E7105) |

A name declared by the program (local, parameter or function) always takes precedence over these
forms.

## No crashes from built-in operations

| Operation | Behaviour |
|---|---|
| List access | `first(xs) : T?`, `get(xs, i) : T?` |
| Integer division and remainder | `Int?` (`null` on division by zero) |
| `Int` arithmetic | arbitrary precision, no overflow |
| `Double`/`Float` division | IEEE 754 (`Infinity`/`NaN`) |

`var`, `!!`, `throw`, `try`, `catch`, `while`, `do`, `for` and string interpolation are rejected
(E7001).

## Soundness

Target property: a well-typed program does not get stuck (progress and preservation). The rules above
are the specification; the implementation is tested rule by rule (`src/types/infer.rs`), by golden
cases (`tests/typecheck/`) and by properties (`src/types/props.rs`): unification yields a unifier,
inferred types are principal and stable under re-annotation, exhaustiveness agrees with brute-force
enumeration, and the parser round-trips its own output. There is no mechanized proof yet.

## Known limits

- Non-termination: a recursive function may loop forever; termination is not checked.
- Effects (`effects [db.read]`) and deployment domains are still verified by the legacy analysis, not
  by the type system.
- No refinement types: properties of values (`balance >= 0`) are not checked.
- `"\$name"` (an escaped dollar followed by a letter) is rejected as interpolation, because the lexer
  has already decoded the escape. Write `"$" + "name"`.
````

- [ ] **Step 2: Atualizar o `README.md`**

Na seção `## What v0.1 proves`, adicione como primeiro item:

```markdown
- Hindley-Milner type inference in the OCaml model, with algebraic data types, exhaustive `when`, no exceptions and no mutation (see [type system](docs/type-system.md));
```

- [ ] **Step 3: Verificação final (critérios de aceite da spec)**

```bash
cargo test 2>&1 | tail -5
cargo clippy --all-targets -- -D warnings 2>&1 | tail -3
export DATABASE_URL=postgresql://bind:bind@localhost/bind
cargo run --quiet -- check --target aws-lambda
cargo run --quiet -- check --target vercel
make -C ../website check
```
Expected: todos os testes passam; clippy sem warnings; os três `check` terminam com código 0.

Confira cada critério de aceite da spec e onde ele é coberto:

| Critério | Coberto por |
|---|---|
| 1. Construções da seção 1 parseiam; removidas dão E70xx | testes de `parser::expr` (Tasks 2 e 3), `err_removed.bd` |
| 2. Esquemas esperados e um golden por erro da seção 2 | `infer` (Task 7), `tests/typecheck` (Task 10) |
| 3. Match incompleto/redundante rejeitado com contraexemplo | `exhaustive` (Task 5), `err_nonexhaustive.bd`, `err_redundant.bd` |
| 4. Propriedades com ≥ 1.000 casos | `props` (Task 11) |
| 5. Clippy limpo e testes verdes | este passo |
| 6. `docs/type-system.md` | Step 1 |
| Regressão: hashes idênticos | Task 9, Step 5 |

- [ ] **Step 4: Commit**

```bash
git add docs/type-system.md README.md
git commit -m "docs: especificação do sistema de tipos

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```
