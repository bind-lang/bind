# Cliente Elm gerado a partir da IR tipada

- Data: 2026-10-11
- Status: aprovado em conversa, aguardando revisão da spec escrita
- Subprojeto: 3c. Depende de 3a/3b (IR tipada e servidor Rust, já no `main`). 3d (remover a análise por tokens restante e ampliar o perfil) terá spec própria.

## Objetivo

Hoje o cliente Elm não usa a IR. `src/certified.rs` varre os tokens do componente procurando `val x = server.f(...)` e as chamadas `Text(...)`. No caso dinâmico, a árvore de UI escrita pelo usuário é ignorada: a tela é um formulário fixo, e `Text` só aceita um literal ou `binding.campo`. A IR, por sua vez, transforma qualquer nó de UI num `Ui` vazio. Há programas que passam no `check` e quebram no `generate`.

Este subprojeto faz o cliente ser gerado só a partir da `CoreProgram`, como já acontece com o servidor. Assim as garantias do verificador (tipos, `when` exaustivo, efeitos, `Int` exato) chegam ao código que roda no navegador.

### Decisões tomadas

| Decisão | Escolha |
|---|---|
| Escopo | Fiel ao que a linguagem já tem: árvore de UI real, expressões tipadas nos argumentos e em `Text`, `if`/`when` na UI, várias chamadas `server.f`. Sem eventos nem estado no cliente (subprojeto próprio de design de linguagem) |
| Parâmetros do componente | Lidos da query string da URL com os codecs estritos do fio; a avaliação começa ao carregar; sem formulário gerado |
| Falhas de RPC | Implícitas na linguagem: o gerador mostra telas padrão de carregando e erro até a avaliação terminar; nenhuma mudança no sistema de tipos |
| `Int` no cliente | Runtime Elm próprio com BigInt exato; mesma semântica do servidor; sem dependência externa nova |
| Abordagem | Convenção uniforme com `Task`: toda função Bind vira uma função Elm que devolve `Task BindError a`, espelhando o servidor assíncrono |

### Fora do escopo

- Eventos, estado no cliente, formulários e navegação entre páginas.
- `*` e `/` de `Decimal`/`Money` e aritmética de `Float` (f32) no código do cliente (recusados com diagnóstico; continuam disponíveis no servidor).
- Remover os scanners de tokens que não pertencem ao cliente (3d).
- O backend JS/TS (depois do subprojeto 3).

## 1. UI na IR e lowering do cliente

### Estrutura

```text
ExprKind::Ui { node: UiNode, args: [Expr], children: Expr }   // children: List<Ui>
UiNode = enum fechado com os nós de UI_NODES
         (Page, Container, Div, Grid, Stack, Header, Nav, Main, Section, Article,
          Aside, Footer, Paragraph, Span, Strong, Emphasis, Small, List, Item, Pre,
          Code, Heading, Link, Rule, Text, UI)
```

- O bloco de filhos é baixado para uma expressão do tipo `List<Ui>` feita de `Let` e `listOf`. Um `val` dentro do bloco vale para os filhos seguintes sem regra nova.
- `if` e `when` com tipo `Ui` são preservados. Quando o verificador aceita `if` sem `else` dentro da UI, a lowering usa um nó vazio explícito (`UiNode::Empty`, que não renderiza nada). Quando não aceita, a regra atual continua valendo.
- `Text(expr)`: a lowering insere uma primitiva de exibição concreta por tipo, para que nenhum backend precise adivinhar a formatação:

| Tipo | Primitiva | Forma exibida |
|---|---|---|
| `String`, `Email`, `Url` | `DisplayString` | o próprio texto |
| `Int` | `DisplayInt` | decimal canônico |
| `Long` | `DisplayLong` | decimal |
| `Double` | `DisplayDouble` | a forma de `String.fromFloat` do Elm, documentada e replicada pelo avaliador |
| `Float` | `DisplayFloat` | igual a `Double` sobre o valor f32 recebido |
| `Boolean` | `DisplayBool` | `true`/`false` |
| `Decimal`/`Money` | `DisplayDecimal` | a forma canônica do `serde-str` |
| `UUID`, datas | `DisplayText` | a forma canônica ISO do fio |
| distinct | primitiva do tipo base | igual ao tipo base |
| `T?` | não exibível | o verificador já exige um tipo exibível (E7105) |

- `ServerCall { function, args }` passa a carregar os argumentos de verdade. Hoje o cliente os ignora e manda os parâmetros do componente pelo nome.
- O cliente inclui as funções `client` e as compartilhadas alcançáveis a partir do componente, pelo mesmo fecho por `Global` que o servidor usa.

### Validador e avaliador

- O validador confere cada `Ui`: aridade e tipos dos argumentos por nó (as mesmas regras de `infer_ui`), `children` com tipo `List<Ui>`, `Text` com exatamente um argumento exibível e sem filhos.
- `core::eval` ganha o valor `Ui { node, args: [Value], children: [Value] }` com forma canônica de impressão `Nó(arg, …){filho, …}` (`{}` omitido quando não há filhos). É a base do teste diferencial do cliente.
- `core::eval` passa a avaliar `Double` (f64, mesma semântica IEEE em Rust e JavaScript) e implementa `DisplayDouble` replicando o `Number.prototype.toString` do JavaScript, que é o que o `String.fromFloat` do Elm usa. Assim o teste diferencial cobre também a exibição de `Double`.
- O backend Rust continua recusando `Ui`; a E8310 já impede que UI seja alcançável a partir de um endpoint.

## 2. Tradução da IR para Elm

### Convenção de chamada

```elm
f : Ctx -> A -> B -> Task BindError R
f ctx a b =
    enter ctx |> Task.andThen (\ctx1 -> ...)
```

- `enter` incrementa a profundidade e falha com `RecursionLimit` acima de `maxDepth = 10000`. Conta só a entrada de funções Bind e lambdas, nunca builtins, como no servidor e no avaliador.
- O agendador do Elm executa as cadeias de `Task.andThen` num laço, e não por recursão na pilha do JavaScript. A profundidade de 10.000 cabe no navegador e no node com a pilha padrão; isso é um requisito testado (seção 4).

### Tipos

| Bind | Elm |
|---|---|
| `data class` / `entity` | `type alias` de registro |
| `sealed class` / `enum class` | tipo customizado |
| `T?` | `Maybe T` |
| `Result<T, E>` | `BindRuntime.BindResult e t` |
| `List<T>` | `List T` |
| `Int` | `BindRuntime.Int` |
| `Decimal` / `Money` | `BindRuntime.Decimal` |
| `Double` / `Float` | `Float` |
| `Long` | `Int` do Elm (o fio já recusa `Long`; só aparece em código local) |
| `String`, `Email`, `Url`, `UUID`, datas | `String` (forma canônica) |
| função | `Ctx -> A -> Task BindError B` |
| `Ui` | `BindRuntime.Node` |

- Nomes do usuário recebem prefixo ou escape determinístico e injetivo, para não colidir com palavras reservadas do Elm, com `Model`/`Msg`/`main` nem com os nomes do runtime. Tipos e funções do usuário ficam num módulo gerado próprio (`App.elm`), como o módulo `app` do Rust.

### Expressões

- `Let` → `Task.andThen` com lambda; `If` → `if`; `Seq` → `Task.andThen (\_ -> …)`.
- `Match` sobre tipos do usuário, `Maybe` e `BindResult` → `case` (o compilador Elm checa a exaustividade de novo). `Match` com literais `Int`/`String` → cadeia de `if` terminando no braço curinga exigido pelo verificador.
- `Return v` → falha interna `Return v` capturada na fronteira da função corrente (`Task.onError`), que a converte em sucesso. Nunca escapa da função.
- `Prim`:
  - `Int*` → funções do runtime; `IntDivChecked`/`IntRemChecked` → `Maybe`.
  - `Double*` → operadores do Elm.
  - `DecimalAdd`/`DecimalSub`/`DecimalNeg` → funções checadas do runtime (`Overflow`).
  - Comparações e igualdade → estruturais. `Int` e `Decimal` são sempre mantidos em forma normalizada, para que o `==` do Elm valha.
  - `StringConcat` → `++`; `Display*` → seção 1.
- Builtins (`map`, `fold`, `first`, `get`, `length`, `listOf`, `emptyList`) → runtime, com `map`/`fold` em `Task`.
- `ServerCall` → `Http.task`: POST na rota do contrato, corpo codificado com os codecs do contrato, resposta decodificada.
  - Erro HTTP com corpo `{"error": …}` → `Remote mensagem`. Outros erros de rede ou de decodificação → `Remote` com uma mensagem genérica.
  - As chamadas acontecem em sequência, na ordem de avaliação. Não há mais limite de uma chamada por componente.
- `Ui` → construção de um `BindRuntime.Node`. O `view` converte a árvore em `Html` no fim, de modo que a avaliação inteira continua dentro da `Task`.
- `Lambda` → função Elm na convenção uniforme; `Global`/`Builtin`/construtor usados como valor → lambda equivalente.

### Página

```elm
type Model = Loading | Failed BindError | Loaded Node
init : Url -> ( Model, Cmd Msg )   -- lê os parâmetros e dispara a Task do componente
```

- Sem `server.f`, a `Task` resolve imediatamente.
- O site estático segue o mesmo caminho e o HTML renderizado precisa ser igual ao de hoje.

## 3. Runtime Elm, parâmetros e contrato

### Runtime

`runtime/bind-runtime-elm/` no repositório do `bindc`, com testes próprios executados no node. O código é copiado para o workspace gerado (`web/src/BindRuntime.elm`) e entra no hash, como o runtime Rust. Contém:

- `Int`: sinal mais dígitos em base 10^7, sempre normalizado. Operações `add`, `sub`, `mul`, `neg`, `divChecked`, `remChecked` (truncam em direção a zero; resto com o sinal do dividendo; `Nothing` para divisor zero), `compare`, `toString` e `fromString` canônicos (`-?[0-9]+`, sem zeros à esquerda, sem `-0`, no máximo 1000 dígitos).
- `Decimal`: mantissa `Int` mais escala. `add`/`sub`/`neg` checados com os limites do `rust_decimal` (mantissa < 2^96, escala ≤ 28), comparação e `toString`/`fromString` iguais ao `serde-str` do servidor.
- `Ctx`, `enter`, `maxDepth`, `BindError = RecursionLimit | Overflow | Remote String | BadParameter String`, `BindResult`.
- Builtins de lista.
- `Node` e `render : Node -> Html msg`, com o mesmo mapeamento de nós e classes do gerador estático atual.

### Contrato

- Contrato `bind-json-v3`. O formato no fio é o mesmo do v2 (`Int`, `Decimal` e `Money` como string), mas o cliente passa a aceitar qualquer `Int` canônico de até 1000 dígitos, sem o limite de 2^53.
- Os codecs passam a ser gerados a partir dos tipos da IR, e não dos `TypeRef` da AST.

### Parâmetros da URL

- Cada parâmetro do componente é lido da query string (`?id=…`) com o mesmo decodificador estrito do fio aplicado ao texto.
- Tipos aceitos: `String`, `Email`, `Url`, `Int`, `UUID`, `Date`, `DateTime`, `Instant` e distinct sobre esses tipos. Outros tipos dão E8311.
- Parâmetro ausente, repetido ou inválido → `BadParameter "<nome>"`.

### Telas

- Carregando: "Loading…".
- Erro: título e mensagem. `Remote` mostra a mensagem pública do servidor; `BadParameter` diz qual parâmetro; `RecursionLimit` e `Overflow` mostram mensagens fixas.
- Carregado: a árvore do componente dentro do invólucro `div.bind-site` usado hoje pelo site.
- Saem o cabeçalho fixo "BIND CERTIFIED APPLICATION", o formulário e o botão.

### `elm.json`

As mesmas dependências de hoje. Apenas `elm/url` passa de indireta para direta. O arquivo continua constante e com hash no lock.

## 4. Diagnósticos, perfil e testes

### Fluxo

```text
verify → types::check → core::lower → core::validate
       → plano do servidor (E8309/E8310) + plano do cliente (E8311)
certify/generate → backend Rust (servidor) e backend Elm (cliente), ambos só a partir da CoreProgram
```

O plano do cliente é o mesmo código que o backend Elm usa, como o `DbPlan` do servidor. Nenhum programa verificado falha no `generate`.

### E8311 (cliente)

- Aritmética de `Float` alcançável a partir de um componente.
- `*` ou `/` de `Decimal`/`Money` alcançável a partir de um componente.
- Parâmetro de componente sem forma textual canônica.
- Target web sem componente cliente.

### O que sai

- A análise por tokens do cliente: `component_plan`, `parse_ui_nodes`, a varredura de `Text`, `ComponentPlan`/`UiNode`/`UiArgument`.
- A E8306 e os casos especiais E8301/E8303 que dependiam do formato antigo.
- Os helpers de codec Elm baseados na AST.

### Perfil

- `rust-elm-web-v3`, contrato `bind-json-v3`.
- `docs/certified-profile-v0.3.md`; v0.1 e v0.2 ficam como histórico.
- `website/deploy/` (outro repositório) precisa ser regenerado depois do merge.

### Testes

TDD, como nos subprojetos anteriores.

1. **Unitários:** lowering da UI (filhos com `val`, `if`/`when`, primitivas de exibição, argumentos de `ServerCall`) e validador com nós inválidos montados à mão.
2. **Runtime Elm** (no node): `Int` contra o `BigInt` do JavaScript em ao menos 10.000 casos aleatórios por operação; `Decimal` contra vetores gerados pelo `rust_decimal`; codecs (ida e volta e entradas malformadas); `enter` no limite.
3. **Snapshots:** `tests/codegen/<caso>.elm.expected`, regraváveis com `BIND_BLESS=1`, compilados com `elm make`.
4. **Teste diferencial do cliente:** o gerador de programas do servidor, ampliado com UI (nós, `Text` de cada tipo exibível, `if`/`when` na UI, `val` nos filhos). São 1.000 programas avaliados pelo `core::eval` e pelo Elm gerado executado no node, comparados na forma canônica, com 0 divergências. Inclui a recursão de 20.000 níveis, que precisa dar `RecursionLimit` nos dois lados sem estouro de pilha.
5. **Ponta a ponta:** o `commerce` com o servidor Rust gerado rodando contra Postgres (`BIND_TEST_DATABASE_URL`) e o Elm gerado chamando-o por HTTP no node. Cobre caminho feliz, 404 com a mensagem pública e parâmetro inválido na URL. Sem a variável, o teste é marcado como pulado de forma visível.
6. **Site:** o HTML renderizado é igual ao gerado hoje; `check`, `generate` e `certify` passam.

### Critérios de aceite

1. O backend Elm lê só a `CoreProgram`; a análise por tokens do cliente sai.
2. Nenhum programa verificado falha no `generate` por causa do cliente.
3. `Int` exato no cliente, com a mesma semântica do servidor; o limite de 2^53 sai.
4. Teste diferencial do cliente com ≥ 1.000 programas e 0 divergências, incluindo a recursão de 20.000 níveis.
5. `certify` aprovado nos dois targets com o `commerce` e no site, com o HTML do site inalterado.
6. `cargo test`, clippy e fmt limpos; documentação `docs/certified-profile-v0.3.md`.
