# Núcleo da linguagem: expressões, inferência Hindley-Milner e exaustividade

- Data: 2026-10-09
- Status: aprovado em conversa, aguardando revisão da spec escrita
- Subprojeto: 1 de 3

## Objetivo

O objetivo do Bind é que o compilador rejeite a maior quantidade possível de programas com bug, com garantias matemáticas. Este subprojeto constrói a fundação: um sistema de tipos correto (sound) no modelo do OCaml, mais rígido que ele onde o OCaml deixa bugs passarem.

Hoje os corpos de função são `Vec<Token>` e toda a análise (`compiler.rs::analyze_function`, `certified.rs`) é heurística sobre tokens. Não existe verificação real de tipos nas expressões.

### Decisões tomadas

| Decisão | Escolha |
|---|---|
| Esforço de prova do programador | Nenhum: modelo OCaml (inferência HM, ADTs, pattern matching) |
| Match incompleto | Erro (o OCaml só avisa) |
| Exceções | Não existem; erros são valores (`Result<T, E>`, `T?`) |
| Mutação | Não existe (`var`, campos mutáveis e `ref` são removidos) |
| Anotações | Obrigatórias nas fronteiras (`server fun`, `client component`, `entity`); opcionais no resto |
| Integração | Passo de verificação paralelo (abordagem A): o novo checker roda antes da análise atual, e o codegen atual não muda |

### Decomposição

1. **Núcleo da linguagem** (esta spec): gramática de expressões, AST, inferência HM, ADTs com dados, exaustividade.
2. **Migração da verificação**: efeitos, domínios, banco, segredos e RPC passam a operar sobre a IR tipada.
3. **Migração do codegen**: Rust e Elm gerados a partir da IR tipada; o perfil certificado cresce.

### Fora do escopo

- Tipos refinados ou solver SMT, totalidade (garantia de terminação), tipos dependentes.
- Guardas em padrões, operador de propagação para `Result`, módulos e functors.
- Efeitos no sistema de tipos (continuam no verificador atual até o subprojeto 2).
- Prova mecanizada (Lean ou Coq) da correção.
- Qualquer mudança no codegen ou nos artefatos certificados.

## 1. Sintaxe

Léxico compatível com Kotlin (sem mudanças no lexer), semântica no estilo OCaml.

### Declarações de tipo

```kotlin
sealed class Pedido {
    data class Pendente(val itens: List<Item>)
    data class Pago(val valor: Money, val em: Timestamp)
    data object Cancelado
}

sealed class Arvore<T> {
    data class No(val esq: Arvore<T>, val valor: T, val dir: Arvore<T>)
    data object Folha
}
```

- As variantes de `sealed class` passam a guardar seus campos (hoje `TypeDecl.variants` guarda só nomes).
- `sealed class` e `data class` aceitam parâmetros de tipo; as variantes herdam os parâmetros do tipo pai. `entity` não aceita parâmetros, porque corresponde a uma tabela concreta do banco.
- `enum class` continua existindo como soma sem dados.
- Campos só aceitam `val`. `var` em campo é o erro E7002.

### Funções

```kotlin
fun dobro(x) = x * 2                                   // inferido: (Int) -> Int
fun <T> primeiro(xs: List<T>): T? = first(xs)          // anotação permitida
server fun total(id: PedidoId): Money effects [db.read] { ... } // anotação obrigatória
```

Em `server fun`, `client component` e `entity`, cada parâmetro e o retorno precisam de tipo explícito; a falta é o erro E7003. Nas demais funções, as anotações são opcionais.

### Expressões

- Literais (`Int`, `Float`, `String`, `Bool`, `null`), variáveis, chamadas, acesso a campo, construtores (`Pago(valor, agora)`).
- Operadores aritméticos, de comparação (`== != < <= > >=`) e lógicos (`&& || !`).
- `val x = e` (sem `var`).
- Blocos `{ ...; e }`: o valor do bloco é a última expressão; um bloco vazio vale `Unit`.
- `if (c) a else b`, com `else` obrigatório.
- `when (e) { padrão -> expr ... }`.
- Lambdas `{ x -> e }` e `{ x, y -> e }`.
- `return e`.
- Açúcar para `T?`: `a ?: b` vira `when (a) { null -> b; x -> x }`, e `a?.f` vira `when (a) { null -> null; x -> x.f }`.

### Padrões

`_`, variável, literal, construtor com subpadrões (`Pago(v, _)`, `No(Folha, x, _)`) e `null`.

### Removidos

`var`, `!!`, `throw`, `try`/`catch`, `while`, `do`/`while`, `for`. Cada um gera o erro E7001 com uma sugestão: `val`; `when`/`?:`; `Result`; recursão, `map` ou `fold`.

Nenhum arquivo `.bd` existente (`examples/commerce`, `website/src/site.bd`) usa essas construções.

## 2. Sistema de tipos

### Tipos

- Primitivos: `Int` (precisão arbitrária), `Float` (IEEE 754 de 64 bits), `Bool`, `String`, `Unit`.
- Escalares de domínio que já existem no perfil: `UUID`, `Email`, `Money`, `Timestamp` e os demais de `certified_wire_type`.
- Nominais do usuário: `data class`, `entity`, `sealed class`, `enum class`, `distinct type`. A igualdade de tipos é nominal: nomes diferentes nunca unificam, e não há conversão implícita.
- Embutidos: `List<T>`, `Result<T, E>` (`Ok(T)`, `Err(E)`), `Option<T>` escrito `T?` (`null`, valor). `T??` é `Option<Option<T>>`, sem colapsar.
- Funções: `(A, B) -> C`.

### Inferência

- Unificação por union-find com occurs check.
- Generalização por níveis (Rémy), como no compilador do OCaml.
- Sem value restriction: a linguagem não tem mutação, então a generalização completa é correta.
- As funções de topo são agrupadas em componentes fortemente conexos do grafo de chamadas. Cada componente é inferido de forma monomórfica e depois generalizado, o que suporta recursão mútua.
- A recursão polimórfica exige anotação.
- Funções anotadas: o tipo declarado é instanciado e unificado com o tipo inferido do corpo (verificação, não inferência).
- `return e`: `e` é unificado com o tipo de retorno da função, e a expressão `return` recebe uma variável de tipo nova.
- Literais não são polimórficos: `2 : Int`, `2.0 : Float`.

### Restrições fechadas para operadores

Não há typeclasses. Os operadores sobrecarregados geram restrições sobre conjuntos fechados de tipos:

| Restrição | Tipos aceitos | Operadores |
|---|---|---|
| `Num` | `Int`, `Float` | `* /`, `%` (só `Int`) |
| `Ord` | `Int`, `Float`, `Money`, `String`, `Timestamp` | `< <= > >=` |
| `Add` | `Int`, `Float`, `Money`, `String` | `+`; `-` exceto para `String` |
| `Eq` | qualquer tipo concreto que não contenha função | `== !=` |

- Quando a variável restrita é resolvida para um tipo concreto, o tipo é verificado contra o conjunto. Se não pertencer, o resultado é o erro E7301.
- Se uma variável chega à generalização ainda restrita, o resultado é o erro E7302 (tipo ambíguo, anote). Não existe escolha padrão silenciosa.
- `Int / Int : Int?` e `Int % Int : Int?` (divisão por zero vira `null`). `Float / Float : Float`.

### Correção

`docs/type-system.md` documenta as regras de tipagem como regras de inferência formais, a semântica de cada operação embutida e a propriedade-alvo: programas bem tipados não ficam presos (progresso e preservação). Limites conhecidos: não-terminação por recursão, e efeitos ainda fora do sistema de tipos.

## 3. Exaustividade e ausência de crashes

- Algoritmo de matrizes de Maranget ("Warnings for pattern matching", 2007).
- `when` incompleto gera o erro E7201, com um contraexemplo impresso como padrão (por exemplo `Pago(_, _)`).
- Braço redundante (inalcançável) gera o erro E7202.
- Tipos com construtores finitos: `sealed class`, `enum class`, `Bool`, `T?`, `Result`. Tipos com infinitos valores (`Int`, `Float`, `String` e escalares) exigem `_` ou uma variável.
- `List` não tem padrões na v1. Funções embutidas totais: `first(xs): T?`, `get(xs, i): T?`, `map`, `fold`, `length`.

Nenhuma operação embutida pode falhar em tempo de execução:

| Operação | Semântica |
|---|---|
| Acesso a lista | `first`/`get` retornam `T?` |
| Divisão e resto inteiros | `Int?` (`null` quando o divisor é zero) |
| Aritmética de `Int` | Precisão arbitrária, sem overflow |
| Divisão de `Float` | IEEE 754 (`Infinity`/`NaN`) |

O mapeamento de `Int` para `bigint` na fronteira do PostgreSQL é decidido no subprojeto 3. A única forma restante de um programa bem tipado não produzir valor é recursão infinita.

## 4. Arquitetura

### Fluxo em `compiler::verify`

```
lexer (sem mudança)
  → parser de declarações (sealed com campos, genéricos, sem `var`)
  → parser de expressões: Function.body (tokens) → Function.expr (Block)
  → types::check(&Program) → (TypedProgram, Vec<Diagnostic>)
  → qualquer erro E7xxx aborta antes da análise atual e de todo gerador
  → análise atual por tokens e codegen (sem mudança)
```

`Function` ganha `expr: Option<Block>` ao lado de `body: Vec<Token>`. Os dois convivem até os subprojetos 2 e 3. `TypedProgram` mapeia cada função ao seu esquema de tipo e cada nó de expressão (por id) ao seu tipo resolvido. Ele é a interface para os próximos subprojetos.

### Módulos

| Módulo | Responsabilidade | Depende de |
|---|---|---|
| `src/syntax/expr.rs` | AST de expressões e padrões, com `Span` e id por nó | `diagnostic` |
| `src/parser/expr.rs` | Parser Pratt de tokens para a AST; impressor para testes de ida e volta | `lexer`, `syntax` |
| `src/types/ty.rs` | `Type` (`Var` em union-find, `Con`, `Fun`, `Error`), níveis, `Scheme` | — |
| `src/types/unify.rs` | Unificação, occurs check, ajuste de níveis | `ty` |
| `src/types/env.rs` | Tipos declarados, construtores, assinaturas embutidas | `ast`, `ty` |
| `src/types/infer.rs` | Inferência sobre a AST, coleta e resolução de restrições | `ty`, `unify`, `env` |
| `src/types/exhaustive.rs` | Maranget: exaustividade e redundância | `ty`, `syntax` |
| `src/types/mod.rs` | `check(&Program) -> (TypedProgram, Vec<Diagnostic>)` | todos |

`parser.rs` e `compiler.rs` recebem apenas os pontos de integração.

### Diagnósticos

| Faixa | Categoria |
|---|---|
| E70xx | Sintaxe de expressões e construções removidas |
| E71xx | Tipos: incompatibilidade, tipo infinito, nome desconhecido, aridade |
| E72xx | Exaustividade e redundância |
| E73xx | Restrições de operadores: tipo inválido ou ambíguo |

- Os erros de tipo mostram o tipo esperado, o encontrado e o span que originou a expectativa.
- Uma falha de unificação atribui `Error` ao nó; `Error` unifica com qualquer tipo, sem gerar novos diagnósticos.
- Saída determinística: variáveis de tipo renomeadas para `'a`, `'b`… por ordem de aparição; diagnósticos ordenados por arquivo, linha e coluna.

## 5. Testes e critérios de aceite

TDD: cada regra entra primeiro como um teste que falha.

1. **Unitários por módulo.** Parser (precedência, associatividade, lambdas, `when`, açúcar, recuperação); unificação (occurs check, níveis, `Error`); inferência (para cada regra de `docs/type-system.md`, ao menos um caso positivo e um negativo); exaustividade (exemplos do artigo de Maranget, `Option`, `Result`, aninhamento, redundância).
2. **Golden.** `tests/typecheck/<caso>.bd` e `<caso>.expected`, com os esquemas inferidos ou os diagnósticos exatos.
3. **Propriedades** (`proptest` como dev-dependency fixada com `=`, sem efeito no binário nem no código gerado), com pelo menos 1.000 casos cada:
   - unificador correto: depois de `unify(a, b)`, `a` e `b` resolvem para o mesmo tipo;
   - tipo principal: se uma anotação `T` é aceita, o tipo inferido sem anotação é mais geral que `T`;
   - estabilidade: anotar com os tipos inferidos e verificar de novo produz o mesmo resultado;
   - exaustividade contra força bruta: para tipos algébricos gerados, o veredito de Maranget coincide com a enumeração de valores até a profundidade *n*;
   - ida e volta do parser: `parse(print(ast)) == ast`.
4. **Regressão.** Os 24 testes atuais continuam passando; `examples/commerce` e `website/src/site.bd` passam no `check`; o `certify` produz artefatos com hashes idênticos aos anteriores.

### Critérios de aceite

1. Todas as construções da seção 1 parseiam, e as removidas geram E70xx com sugestão.
2. Os programas de exemplo da documentação inferem os esquemas esperados, e cada erro da seção 2 tem um teste golden.
3. Matches incompletos e redundantes são rejeitados com contraexemplo.
4. Todas as propriedades passam com pelo menos 1.000 casos.
5. `cargo clippy --all-targets -- -D warnings` limpo e `cargo test` verde.
6. `docs/type-system.md` publicado com as regras formais e os limites conhecidos.
