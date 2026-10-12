# Fim da análise por tokens e tipos ricos no fio

- Data: 2026-10-11
- Status: aprovado em conversa, aguardando revisão da spec escrita
- Subprojeto: 3d. Depende de 3a/3b (IR e servidor Rust) e 3c (frontend web e SSR), já no `main`.

## Objetivo

Depois do 3c, os backends leem só a `CoreProgram`, mas `src/compiler.rs` ainda varre tokens crus do lexer em alguns pontos:

- o grafo da aplicação;
- a extração de operações de banco;
- o vazamento de segredo (E4102);
- as checagens de banco antigas;
- o aviso W2101.

Essas varreduras dão falsos positivos (uma string `"!!"`, uma variável com o nome do segredo) e falsos negativos (segredo que vaza por uma função auxiliar).

O perfil certificado também recusa no fio tipos que a linguagem tem: `sealed` com campos, genéricos instanciados, `Result`, `Set` e `Map`. `Set` e `Map` sequer têm operações.

Este subprojeto troca toda a análise por tokens por análises tipadas sobre a IR, remove `Function.body` do AST, libera os tipos ricos no fio e dá a `Set` e `Map` um conjunto mínimo de operações com semântica igual nos três runtimes.

### Decisões tomadas

| Decisão | Escolha |
|---|---|
| Fonte dos substitutos | A `CoreProgram`, com o mesmo código que os backends usam |
| Ampliação do perfil | Tipos ricos no fio: `sealed` com campos, genéricos instanciados, `Result`, `Set`, `Map` |
| `Set`/`Map` | Builtins mínimas e imutáveis, chaves restritas a tipos `Key` com ordem total |
| Fora do escopo | `Long` como string, `*` e `/` de `Decimal` no frontend, `Duration` no banco |

## 1. Substitutos tipados e remoção dos tokens

### `src/core/analysis.rs`

Percorre a `CoreProgram` e produz, em ordem determinística:

- **Grafo da aplicação:**
  - arestas função → função (`Global`);
  - cliente → endpoint (`ServerCall`);
  - função → tabela e operação (`Db`);
  - `environment.X` → função (`Environment`).

  Alimenta `impact`, `tree`, `application.graph.txt` e o caminho mostrado nos diagnósticos.
- **Operações de banco:** `VerifiedApplication.database_operations` passa a vir de `ExprKind::Db`, pelo mesmo código de `backend::rust::db::operations`.

### Diagnósticos tipados

- **E4102:** um valor de `environment` marcado como segredo, usado por qualquer função alcançável a partir de código cliente, é recusado com o caminho completo (`environment.X -> f -> g -> Componente`). Variável homônima e string deixam de disparar.
- **E5303:** um `insert` precisa atribuir toda coluna obrigatória (não nula e sem default), checado em `DbOp::Insert { columns }` contra o esquema.
- **E5204:** cada campo do modelo de uma tabela usada precisa ter tipo compatível com a coluna, comparando o tipo da IR com o tipo da coluna (não mais por string).
- **E2301:** função sem corpo com retorno diferente de `Unit`, pela ausência de corpo.

### O que sai

- **Código:**
  - `analyze_function`, `insert_assignments`, `compatible`;
  - os laços de W2101 e da E4102 por tokens;
  - `Function.body` (vetor de tokens) do AST. O parser usa os tokens localmente.
- **Diagnósticos já cobertos pelo verificador de tipos:** E5200 (E7104), E5201 da heurística `it.` e de coluna inexistente (E7104), E5202 (E7103), E8204 de operação de tabela (E7004).
- **Checagens certificadas da era v1:**
  - E8303, substituída pela E8309.
  - E8302 e E8308, que o parser e o verificador já impedem. O plano prova isso com testes antes de remover.
- **`TypeDecl.variants`** e as checagens de declaração por string (E2001–E2003). Saem quando o verificador cobre o mesmo erro; senão viram checagens sobre o `TypeExpr` parseado.
- **Permanece:** E8307 (credencial do banco declarada no `environment`), que é regra de segurança.

Para cada código removido, um teste mostra que o programa com o bug correspondente continua recusado por outro diagnóstico.

## 2. Tipos ricos no fio e coleções

### Fio

| Tipo | JSON |
|---|---|
| variante sem campos | `"nomeLowerCamel"` (como hoje) |
| variante com campos | `{"nomeLowerCamel": {campos}}` (formato atual do serde) |
| `Result<T, E>` | `{"Ok": v}` / `{"Err": e}` |
| genérico instanciado | o formato do tipo com os argumentos concretos |
| `Set<T>` | array em ordem canônica; o decodificador aceita qualquer ordem, normaliza e recusa duplicatas |
| `Map<K, V>` | array de pares `[k, v]` em ordem canônica da chave; duplicatas recusadas |

- A E8309 passa a recusar no fio apenas funções, `Duration` e `Long`.
- Os decodificadores continuam estritos: tag desconhecida, campo a mais e duplicatas são recusados.

### Restrição `Key`

Restrição fechada nova no verificador, como `Eq` e `Ord`. Pertencem a `Key`:

- `Int`, `String`, `Email`, `Url`, `Boolean`, `UUID`;
- `Decimal`, `Money`, `Date`, `DateTime`, `Instant`;
- `enum class` (sealed sem campos);
- distinct sobre esses tipos.

`Double`, `Float`, registros, listas e opcionais não são `Key`. Elemento de `Set` e chave de `Map` precisam ser `Key`; caso contrário, erro de tipo.

A ordem canônica é a mesma nos três runtimes:

- numérica para `Int` e `Decimal`;
- por ponto de código para strings;
- temporal tipada para datas;
- ordem de declaração para variantes de `enum`;
- `false < true`.

### Builtins

Todas imutáveis: cada operação devolve uma coleção nova.

| `Set<T>` | `Map<K, V>` |
|---|---|
| `setOf(a, b, …): Set<T>` (variádica) | `emptyMap(): Map<K, V>` |
| `emptySet(): Set<T>` | `put(m, k, v): Map<K, V>` |
| `insert(s, x): Set<T>` | `lookup(m, k): V?` |
| `remove(s, x): Set<T>` | `removeKey(m, k): Map<K, V>` |
| `contains(s, x): Boolean` | `keys(m): List<K>` (ordem canônica) |
| `setSize(s): Int` | `values(m): List<V>` (ordem das chaves) |
| `toList(s): List<T>` (ordem canônica) | `mapSize(m): Int` |

`setSize`, `mapSize`, `toList`, `keys` e `values` não contam profundidade; são builtins.

### Representação

- **Rust:** `Arc<BTreeSet<T>>` / `Arc<BTreeMap<K, V>>`, com `Ord` gerado só para tipos `Key`.
- **JS:** arrays ordenados e congelados (pares para `Map`), operados com o comparador canônico do runtime.
- **Avaliador:** a mesma ordem e a mesma forma canônica de impressão.

## 3. Perfil, testes e critérios de aceite

### Perfil

- `rust-web-v4`, contrato `bind-json-v4`. Os tipos que já cruzavam o fio mantêm o formato.
- `docs/certified-profile-v0.4.md`, com a lista de diagnósticos removidos e seus substitutos. As docs antigas que citam tokens ou Elm ganham nota de histórico.
- `impact`, `tree` e `application.graph.txt` continuam funcionando, agora a partir da IR.

### Testes

TDD, como nos subprojetos anteriores.

1. **Diagnósticos novos:**
   - E4102 direta, via função auxiliar, e sem falso positivo para variável homônima e string;
   - E5303 e E5204 tipadas;
   - E2301;
   - restrição `Key`, com tipos aceitos e recusados.
2. **Remoção:** para cada código removido, um programa com o bug correspondente continua recusado por outro diagnóstico, nomeado no teste.
3. **Grafo:** o grafo do `commerce` é comparado com um arquivo esperado; `impact` e `tree` testados.
4. **Coleções:** builtins no avaliador, no runtime Rust e no runtime JS, com vetores compartilhados em `runtime/vectors/` (ordem canônica, duplicatas, `lookup` ausente, `remove` de ausente).
5. **Diferencial a três vias:** ampliado com `Set`, `Map`, sealed com campos, genéricos instanciados e `Result`; 0 divergências.
6. **Fio:** ida e volta de cada tipo novo nos dois runtimes; recusa de JSON malformado (duplicatas, tag desconhecida, campo a mais, par mal formado).
7. **Ponta a ponta com Postgres:** um endpoint que recebe e devolve `Map` e sealed com campos, nos modos SSR e separado.

### Critérios de aceite

1. Nenhum `Token` é lido fora do lexer e do parser; `Function.body` sai do AST.
2. Todo diagnóstico removido tem cobertura comprovada por outro.
3. E4102 transitiva, E5303 e E5204 tipadas, grafo e operações de banco vindos da IR.
4. Tipos ricos no fio e `Set`/`Map` utilizáveis, com semântica igual no avaliador, no Rust e no JS.
5. Nenhum programa verificado falha no `generate`.
6. `certify` aprovado nas quatro combinações do `commerce` e no site (numa cópia temporária com `client = "web"`).
7. `cargo test`, clippy e fmt limpos; documentação v0.4.
