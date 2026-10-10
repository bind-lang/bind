# Efeitos e domínios no sistema de tipos

- Data: 2026-10-10
- Status: aprovado em conversa, aguardando revisão da spec escrita
- Subprojeto: 2 de 3 (depois de `2026-10-09-type-system-core-design.md`)

## Objetivo

Hoje os efeitos (`effects [db.read]`) e os domínios (`pure`, `server`, `client`) são verificados por heurística sobre tokens, em `compiler.rs::analyze_function`. A heurística só enxerga uma operação de banco escrita **diretamente** no corpo, e por isso deixa passar efeitos indiretos: uma função auxiliar que grava, ou uma lambda passada para `map`. Este subprojeto leva efeitos e domínios para o sistema de tipos, com inferência e propagação por chamadas e lambdas, e transforma as anotações de fronteira em contratos verificados.

### Decisões tomadas

| Decisão | Escolha |
|---|---|
| Forma | Efeitos no tipo das funções, inferidos; nada de mônadas ou `do` |
| Representação | Linhas de efeito com variáveis (estilo Koka), no mesmo motor de unificação |
| Rótulos | `recurso.operação`; a tecnologia fica na declaração do recurso, não no efeito |
| Recursos na v1 | `db` (`read`, `write`) e `server` (`call`) |
| `fun` sem modificador | Domínio e efeitos inferidos pelo uso; só `server fun` vira endpoint RPC |
| `pure fun` | Garante zero efeitos |
| `effects [...]` | Igualdade exata com os efeitos inferidos |
| Domínios | Teto da linha de efeitos de cada função |

### Fora do escopo

- Fluxo de segredos (informação), que continua no verificador antigo (E4102).
- Efeitos definidos pelo usuário e *handlers*.
- Operações de cache, fila e arquivos. O modelo de rótulos as suporta, mas a linguagem ainda não tem operações desses tipos.
- Vários bancos de dados. O `bind.toml` atual admite um; no código ele é `db`.
- Mudanças no codegen (subprojeto 3).

## 1. Representação e sintaxe

### Rótulos e linhas

- Um **rótulo de efeito** é um par *(recurso, operação)*, escrito `recurso.operação`. Na v1: `db.read`, `db.write`, `server.call`.
- O vocabulário de operações depende do tipo de recurso. Os recursos de dados ainda não implementados terão: banco e cache `read`/`write`; fila `publish`/`consume`; arquivos `read`/`write`.
- Uma **linha de efeito** é um conjunto de rótulos mais, opcionalmente, uma variável de linha: `{}`, `{db.read}`, `{db.read | 'e}`.
- `Type::Fun(params, result, row)`: a linha é o efeito latente, ou seja, o que acontece ao chamar a função.
- As variáveis de linha vivem na `TypeTable`, com union-find e níveis, e podem ter um **teto**: o conjunto máximo de rótulos que aceitam.

### Exibição

```text
dobro  : (Int) -> Int
buscar : (ClienteId) -> Cliente ! {db.read}
aplica : (('a) -> 'b ! 'e, 'a) -> 'b ! 'e
map    : (List<'a>, ('a) -> 'b ! 'e) -> List<'b> ! 'e
```

Linha vazia fechada: nada é exibido. Variáveis de linha são renderizadas como `'e`, `'e1`, `'e2`… em ordem de aparição; as variáveis de tipo pulam a letra `e` (`'a 'b 'c 'd 'f`…).

### Sintaxe nas anotações

```kotlin
fun <A, B> mapear(xs: List<A>, f: (A) -> B ! [e]): List<B> effects [e] = map(xs, f)
server fun buscar(id: ClienteId): Cliente effects [db.read] { ... }
fun aplica(f: (Int) -> Int, x: Int): Int = f(x)
```

- Num tipo função anotado, `! [r1, r2, e]` lista os efeitos; a ausência de `!` significa pura (linha vazia fechada).
- Dentro de `[...]`, um identificador minúsculo sem ponto (`e`) é uma variável de efeito; `recurso.operação` é um rótulo.
- `effects [...]` descreve a linha da própria função. Numa `server fun`, omitir a cláusula equivale a `effects []`.
- Rótulo desconhecido (recurso ou operação inexistente) gera E7405.

### Assinatura conhecida de antemão

Uma função tem a assinatura conhecida antes de verificar os corpos (é rígida e entra no caminho das funções anotadas) quando tem parâmetros e retorno anotados **e** os efeitos conhecidos: cláusula `effects [...]`, ou `pure fun` (`[]`), ou `server fun` (sem cláusula = `[]`). As demais, inclusive `client component` e `fun` sem cláusula, têm os efeitos inferidos pelo caminho das SCCs; suas anotações de tipo continuam valendo como restrições.

## 2. Regras de inferência e domínios

### Origem dos efeitos

| Construção | Efeito no contexto atual |
|---|---|
| `db.t.require(k)`, `db.t.find(k)` | `db.read` |
| `db.t.insert { ... }` | `db.write` |
| `server.f(args)` | `server.call`; a linha de `f` não passa para quem chama |
| `f(args)` | a linha do tipo de `f` |
| lambda `{ x -> ... }` | nenhum ao criar; o corpo tem linha própria, que vira a linha do tipo da lambda |
| UI, literais, `if`, `when`, `val`, operadores | só os efeitos das subexpressões |

### Mecânica

- Cada corpo de função ou lambda é inferido com uma **linha de contexto** aberta (`{| 'ρ}`); cada efeito produzido é unificado nela.
- Unificação de linhas (rótulos distintos, sem ordem): `{L1 | ρ1} ~ {L2 | ρ2}` resolve com `ρ1 := {L2 \ L1 | ρ3}` e `ρ2 := {L1 \ L2 | ρ3}`. Linhas fechadas só unificam com conjuntos iguais. Inclui occurs check em variáveis de linha e ajuste de níveis.
- **Instanciação abre**: ao instanciar um nome (global ou `val` generalizado), uma linha fechada no efeito do topo do tipo função ganha uma variável nova. Assim uma função pura pode ser usada onde se aceitam efeitos; uma lambda com efeito não entra num parâmetro anotado como puro.
- **Generalização fecha**: uma variável de linha generalizada que só aparece no efeito do topo é fechada (removida), para que os esquemas fiquem legíveis.
- Variáveis de linha são generalizadas pelos mesmos níveis das variáveis de tipo.

### Domínios como teto

| Função | Teto da linha de contexto |
|---|---|
| `pure fun` | `{}` |
| `server fun` | `{db.read, db.write}` |
| `client fun`, `client component` | `{server.call}` |
| `fun` | sem teto |

- Unificar uma variável com teto com uma linha que tem rótulo fora do teto gera E7401. Ao unir duas variáveis com teto, o resultado fica com a interseção.
- A mensagem de E7401 sugere `server.f(...)` quando o efeito excedente vem de uma `server fun`.
- **Regra estrutural**: chamar uma `server fun` diretamente a partir de código `client` (sem `server.`) gera E7404, mesmo que ela não tenha efeitos.

### Exatidão de `effects [...]`

- O corpo é inferido com uma linha nova (com o teto do domínio); no fim, os rótulos produzidos são comparados com os declarados.
- Produzido e não declarado: E7402. Declarado e não produzido: E7403.
- Variáveis de efeito declaradas são rígidas dentro do corpo (como `'T`): a linha do corpo precisa ser exatamente os rótulos declarados mais essas variáveis.

### Caminho até o efeito

Para cada rótulo de cada função, o verificador guarda a primeira origem (uma operação de banco ou uma chamada a outra função). E7401 e E7402 preenchem `Diagnostic.path` com a cadeia até a operação:

```text
BIND ERROR E7402
`fecharPedido` grava no banco (`db.write`), mas declara só `[db.read]`.
Binding path:
    fecharPedido
      -> registrarAuditoria (src/pedido.bd:31:1)
      -> db.auditoria.insert (src/pedido.bd:34:12)
```

## 3. Integração

- `TypedProgram.effects: BTreeMap<String, Vec<String>>`: rótulos resolvidos de cada função, ordenados, sem variáveis.
- Em `compiler::verify`:
  - Saem da análise antiga as checagens E6001, E6101, E6102 e E6201, e a inserção de capacidades a partir de efeitos detectados por tokens.
  - As capacidades exigidas passam a ser as de domínio já existentes (`ui.dom` para cliente, `server.http` para servidor) mais os rótulos de recurso inferidos (`db.read`, `db.write`); `server.call` não vira capacidade. Para os programas atuais o conjunto é idêntico ao de hoje.
  - Continuam iguais: grafo da aplicação, extração de operações de banco para o codegen, E4102, checagens E8xxx, `--emit-ir`.
- AST: `fun` sem modificador passa a ter um domínio próprio (por exemplo `Domain::Inferred`); hoje o parser o trata como `Pure`. `pure fun` continua `Pure`. O `--emit-ir` passa a mostrar esse domínio para funções sem modificador.
- Parser: `! [ ... ]` em tipos função (parser de declarações e `parse_type_text`); `TypeExpr::Function` ganha a lista de efeitos; `effects [...]` aceita variáveis minúsculas.
- Mudanças visíveis: esquemas com efeitos passam a mostrá-los (`tests/typecheck/db.expected` e testes da Task 8 do subprojeto 1); testes da análise antiga que dependiam de E6xxx migram para E74xx.

### Diagnósticos

| Código | Quando |
|---|---|
| E7401 | Efeito excede o teto do domínio |
| E7402 | Efeito produzido e não declarado |
| E7403 | Efeito declarado e não produzido |
| E7404 | `server fun` chamada diretamente de código `client` |
| E7405 | Rótulo de efeito desconhecido |

## 4. Testes e critérios de aceite

TDD, como no subprojeto 1.

1. **Unitários**: unificação de linhas (rótulos, cauda, occurs check, tetos, generalização e instanciação); cada linha da tabela de origem dos efeitos com caso positivo e negativo; polimorfismo de efeito (`aplica`, `map`); função pura onde se aceita `db.read`; lambda com efeito rejeitada por parâmetro puro; `server.f` gerando só `server.call`; E7404; E7402 e E7403 por caminho indireto com o *binding path* correto; parser de `! [...]` e variáveis em `effects [...]`.
2. **Golden**: um caso de erro por código E7401 a E7405; casos `ok` com esquemas polimórficos no efeito; `db.expected` atualizado.
3. **Propriedades** (≥ 1.000 casos cada):
   - efeitos inferidos de programas aleatórios (chamadas, recursão, operações de banco, lambdas passadas a `aplica`) iguais ao cálculo por alcançabilidade no grafo de chamadas;
   - anotar cada função com os efeitos inferidos e verificar de novo produz os mesmos esquemas, sem E7402/E7403;
   - unificação de linhas produz um unificador.
4. **Regressão**: todos os testes passam (com as atualizações de esquema); `examples/commerce` e o site passam no `check`; hashes dos artefatos gerados idênticos ao baseline, incluindo o `bind-manifest.json`.

### Critérios de aceite

1. Efeitos inferidos e propagados por chamadas diretas, indiretas, recursivas e lambdas; a propriedade de força bruta passa.
2. Tetos de domínio aplicados em `pure`, `server` e `client`, com E7401 e caminho.
3. `effects [...]` exato (E7402/E7403) e variáveis de efeito rígidas.
4. E6001, E6101, E6102 e E6201 removidos da análise antiga, cada um com um teste equivalente em E74xx.
5. Capacidades derivadas dos efeitos tipados, com hashes idênticos.
6. `cargo clippy --all-targets -- -D warnings` e `cargo fmt --check` limpos.
7. `docs/type-system.md` com uma seção de efeitos: regras formais, domínios e o modelo `recurso.operação`.

## Desvios aprovados durante a implementação

1. **Tetos checados depois do corpo.** Os tetos são verificados ao fim de cada função (ao fim do grupo recursivo, em recursão mútua): rótulos concretos fora do teto geram E7401 na função, com o caminho até a operação e uma dica por (domínio, rótulo). Tetos nunca viajam para quem chama.
2. **Variáveis de efeito polimórficas em todo domínio.** Tetos restringem só rótulos concretos; quem chama paga ao instanciar a variável. Uma função `pure`/`server` sem cláusula `effects` carrega implicitamente as variáveis de efeito citadas nos tipos dos parâmetros.
3. **Generalização fecha só a linha de topo da própria função.** As linhas de funções retornadas ficam abertas. A exibição esconde uma variável de linha que ocorre uma só vez e só em posição de resultado; em posição de parâmetro ela sempre aparece.
4. **`server.f` como valor** tem linha aberta `{server.call | …}`. E7404 vale para qualquer uso do nome de uma `server fun` em código `client` (chamada ou valor).
5. **`pure` significa livre de efeitos:** uma função pura pode chamar qualquer função sem efeitos, inclusive uma `server fun` sem efeitos (o antigo E6102 deixou de existir). Lacuna conhecida: o corpo de um endpoint sem efeitos pode ser alcançado sem `server.` por auxiliares `pure` ou comuns. Lacuna conhecida, do mesmo tipo: o corpo de uma `client fun` pode executar efeitos escolhidos por quem chama, através de variáveis de efeito, quando chamada de código servidor ou comum (tetos restringem só rótulos concretos).
6. **Variáveis de efeito rígidas em todo corpo.** As variáveis de efeito citadas numa assinatura são rígidas dentro do corpo também no caminho inferido (com ou sem cláusula `effects`) e viram variáveis genéricas no esquema publicado. Assim, numa função com cláusula, elas são exatas (E7402 se ganham rótulos concretos, E7403 se não são usadas), e o tipo declarado de um parâmetro nunca é reescrito pelo corpo.
