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
`T??` is written and parsed as `Option<Option<T>>` (for example `Int??`); nested options are never collapsed.

`Int` has arbitrary precision. `Double` and `Float` are IEEE 754. A floating-point literal too large
to be finite (`1e400`, `3.5e38f`) is rejected (E7006); one too small to be told apart from zero
(`1e-400`) is the value 0.0, as in Rust. User types (`data class`, `entity`,
`sealed class`, `enum class`, `distinct type`) are nominal: two types are equal only if they have the
same name and equal arguments. An `entity` cannot have type parameters, because it maps to a concrete
database table (E7004).

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
| `Eq` | every type that contains no function, no `Ui` and no type parameter | `==` `!=` |
| `Ord` | `Add` ∪ {Date, DateTime, Instant} | `<` `<=` `>` `>=` |
| `Add` | `Sub` ∪ {String} | `+` |
| `Sub` | `Num` ∪ {Money, Duration} | `-` (binary and unary) |
| `Num` | {Int, Double, Float, Decimal} | `*` `/` |

`Eq` looks inside user types: a type whose constructor fields contain a function, `Ui` or another
such type (directly, through a wrapper such as `data class W(val h: H)`, or in one variant of a sealed
class) does not support equality (E7301). The type's own parameters do not count, because they are
checked through the type arguments: `Box<Int>` supports `==`, `Box<(Int) -> Int>` does not.

### `Key`: set elements and map keys

`Key` is a closed constraint off the operator chain: the element type of a `Set<T>` and the key type
of a `Map<K, V>` must satisfy it.

| Constraint | Accepted types |
|---|---|
| `Key` | {Int, String, Email, Url, Boolean, UUID, Decimal, Money, Date, DateTime, Instant}, every `enum class`, every `sealed class` whose variants all have no fields (only `data object`s), and every `distinct type` over a `Key` type (directly or through another distinct type) |

`Double`, `Float`, `Long`, `Duration`, records, sealed classes with a variant that has fields (and `Result`), lists, sets, maps, options, functions
and type parameters are not `Key`. The requirement is checked where `Set`/`Map` types are written
(signatures, `val` annotations and the fields of declared types) and where the collection builtins
are instantiated (`setOf`, `insert`, `put`, …). A type parameter never satisfies `Key`, so
`fun <T> f(s: Set<T>)` and `data class B<T>(val s: Set<T>)` are rejected. A variable that must
satisfy both `Key` and an operator constraint gets their intersection: `Key` and `<` accept
{Int, Decimal, Money, String, Date, DateTime, Instant}; `Key` and `+` {Int, Decimal, Money,
String}; `Key` and `-` {Int, Decimal, Money}; `Key` and `*`/`/` {Int, Decimal}. As with the operator
constraints, a `Key` variable is never generalized nor defaulted: `fun f() = emptySet()` and
`val s = emptySet()` are ambiguous (E7302) until annotated (`val s: Set<Int> = emptySet()`).

An unsatisfied `Key` is E7301, the code of every unsatisfied closed constraint:
"`Double` does not support use as a set element or map key (accepted types: …)". A written type is
reported once, at its annotation; the uses of the value do not repeat it. The hint of an ambiguous
`Key` variable suggests annotating the collection or its key (`val s: Set<Int> = emptySet()`).

The canonical order of the keys, the same in the evaluator and both runtimes: numeric for `Int`;
numeric by value for `Decimal` and `Money` (`1.0` and `1.00` are the same key, as are `0` and `-0`);
by Unicode code point for `String`, `Email`, `Url` and `UUID` (canonical lowercase text, so also the
order of its 128 bits); the typed temporal order for `Date`, `DateTime` and `Instant`
(`core::display::compare_temporal`, never the text order); the declaration order of the variants
for an `enum class` or a `sealed class` of `data object`s; `false < true`; a distinct type orders as the type it wraps.

`?:` is right-associative: `a ?: b ?: c` is `a ?: (b ?: c)`, so
`fun f(a: Int?, b: Int?): Int = a ?: b ?: 0` type-checks.

`%` requires `Int` operands and has type `Int?`. If the operand type of `/` is still an unresolved variable
when the division is checked, the result type cannot be decided and the program is rejected (E7302):
annotate a parameter or `val`. `&&`, `||` and `!` require `Boolean`. Literals are not
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

In a function that is not fully annotated, declared type parameters (`fun <T> f(x: T) = …`) are
flexible during inference but must remain generic: if the body forces one to a concrete type, or two of
them to be equal, the program is rejected (E7101). The parameters of a function must have distinct
names (E7113).

Calls check the arguments left to right against the callee's parameter types. A lambda passed where a
function type of the same arity is expected takes its parameter types from that expected type, so
`map(xs, { p -> p.valor })` and `fold(xs, 1.0, { acc, x -> acc / x })` type-check. Field access on a
value whose type is not yet known resolves to the only record type that declares the field (E7106 if
several do, E7104 if none).

Built-in functions: `first(xs): T?`, `get(xs, i): T?`, `length(xs): Int`, `map(xs, f): List<U>`,
`fold(xs, init, f): A` with `f: (A, T) -> A`, `emptyList(): List<T>`, and the variadic `listOf(…)`.
A name declared by the program takes precedence over them.

Collection builtins (all pure and immutable: each returns a new collection; `T` and `K` must
satisfy `Key`):

| `Set<T>` | `Map<K, V>` |
|---|---|
| `setOf(a, b, …): Set<T>` (variadic) | `emptyMap(): Map<K, V>` |
| `emptySet(): Set<T>` | `put(m, k, v): Map<K, V>` |
| `insert(s, x): Set<T>` | `lookup(m, k): V?` |
| `remove(s, x): Set<T>` | `removeKey(m, k): Map<K, V>` |
| `contains(s, x): Boolean` | `keys(m): List<K>` (canonical order) |
| `setSize(s): Int` | `values(m): List<V>` (in the order of their keys) |
| `toList(s): List<T>` (canonical order) | `mapSize(m): Int` |

`setOf(a, b, …)` inserts its arguments left to right. Inserting an element equal to one already in
the set keeps the existing one (and the set); `put` with a key equal to one already in the map
replaces the value and keeps the existing key (putting the `Decimal` key `1.00` into a map that holds
`1.0` keeps the key `1.0`); `remove` and `removeKey` of an absent element return the collection unchanged.
The collection builtins can be passed as values (`map(xss, setSize)`, `fold(xs, s, insert)`) and,
like the other builtins, do not count toward the recursion depth limit. `Set` and `Map` cross the wire
as arrays in canonical order (a `Map` as `[k, v]` pairs) and have no database mapping; see the
[certified profile v0.4](certified-profile-v0.4.md).

Without mutation the value restriction is unnecessary: every `val` is generalized.

`server fun`, `client component` and functions without a body must annotate every parameter and the
result (E7003). A component's result is `Ui`. Default parameter values (`x: Int = 1`) are not
supported (E7004).

A block's value is its last expression (`Unit` if it ends with `val` or is empty). Any other
expression statement must have type `Unit`; discarding a value is an error (E7108). Write
`val _ = e` to discard deliberately. `return e` checks `e` against the function's result and is not
allowed inside lambdas (E7111).

## Effects and domains

Function types carry the effects that calling them performs. Effects are inferred, never guessed
from tokens, and `effects [...]` is a contract the checker verifies exactly.

### Labels

An effect label is a pair `resource.operation`. The v1 labels are `db.read`, `db.write` and
`server.call`. The resource is a declared name (today only `db`); the technology behind it stays in
the declaration. Caches, queues and files will get their own operations (`read`/`write`,
`publish`/`consume`); they have none yet. An unknown resource, operation or effect variable is E7405.

### Rows

A function type is `Fun(params, result, row)`, written `(τ1, …, τn) -> τ ! row`. A row is a set of
labels plus an optional row variable: `{db.read}`, `{db.read | 'e}`, `'e`. A closed empty row (a pure
function) shows nothing.

```text
dobro  : (Int) -> Int
buscar : (ClienteId) -> Cliente ! {db.read}
aplica : (('a) -> 'b ! 'e, 'a) -> 'b ! 'e
map    : (List<'a>, ('a) -> 'b ! 'e) -> List<'b> ! 'e
```

Row variables render as `'e`, `'e1`, `'e2`, …; type variables skip the letter `e` (`'a 'b 'c 'd 'f`).
Display hides a row variable that occurs exactly once among the types shown and in a positive
position, because it constrains nothing. Positions follow polarity: a function's own row and the row of
a returned function are positive, each parameter flips the polarity (so the row of a parameter is
always shown, while that of a parameter's parameter counts as positive), and rows inside type
arguments (`List<() -> X ! {db.read | 'e}>`) count as negative, so they are always shown.

### Annotations

```kotlin
fun aplica(f: (Int) -> Int, x: Int): Int = f(x)                       // f is pure
fun lê(f: (Int) -> Int ! [db.read], x: Int): Int = f(x)               // effects in a parameter
fun <A, B> mapear(xs: List<A>, f: (A) -> B ! [e]): List<B> effects [e] = map(xs, f)
server fun buscar(id: ClienteId): Cliente effects [db.read] = db.clientes.require(id)
```

- In a function type, `! [r1, r2, e]` lists the effects; no `!` means pure.
- Inside `[...]`, a lowercase name without a dot is an effect variable; `resource.operation` is a label.
  A name without a dot that does not start with a lowercase letter (`E`) is E7405.
- `effects [...]` is the row of the function itself. A `server fun` without a clause means `effects []`.
- A row holds at most one effect variable (two distinct ones are E7004).
- Effect variables are generic in the function's scheme and rigid inside its body, annotated or
  inferred (like `T` in `fun <T> f(x: T)`): the body can neither add labels to them nor change the
  declared row of a parameter, so `fun f(g: () -> Int ! [e]) = g() + 1` keeps `g : () -> Int ! 'e`.
- A `pure` or `server` function without an `effects` clause implicitly carries the effect variables
  named in its parameter types.

### Rules

Every function or lambda body is inferred under a context row, an open row `{| ρ}` into which each
effect it produces is unified.

| Construct | Effect in the current context |
|---|---|
| `db.t.require(k)`, `db.t.find(k)` | `db.read` |
| `db.t.insert { … }` | `db.write` |
| `server.f(args)` | `server.call`; the row of `f` does not reach the caller |
| `f(args)` | the row of `f`'s type |
| lambda `{ x -> … }` | none when created; its body has its own row, which becomes the row of the lambda's type |
| UI, literals, `if`, `when`, `val`, operators | only the effects of the subexpressions |

```text
          f : (τ1, …, τn) -> τ ! ρ        Γ ⊢ ei : τi
 (App) ──────────────────────────────────────────────────   context row ⊇ ρ
                  Γ ⊢ f(e1, …, en) : τ

          Γ, x1 : τ1, …, xn : τn ⊢ b : τ ! ρ
 (Lam) ───────────────────────────────────────────────   no effect at creation
        Γ ⊢ { x1, …, xn -> b } : (τ1, …, τn) -> τ ! ρ
```

- Absorption: calling a function adds its row to the context row.
- Row unification treats labels as an unordered set: `{L1 | ρ1} ~ {L2 | ρ2}` gives
  `ρ1 := {L2 \ L1 | ρ3}` and `ρ2 := {L1 \ L2 | ρ3}`. A closed row only unifies with an equal set. There
  is an occurs check and level adjustment.
- Instantiation opens: instantiating a name opens a closed row at the top of its function type with a
  fresh variable. A pure function can be passed where effects are accepted; a lambda that performs
  an effect cannot be passed to a parameter annotated pure (E7101).
- Generalization closes only a function's own top row. The rows of returned functions stay open.
- `server.f` used as a value has the open row `{server.call | …}` (never `f`'s own effects), so
  `map(ids, server.buscar)` in client code performs only `server.call`.
- `server.f` requires `f` to be a `server fun` that is not a component; otherwise it is E7404
  ("`server.f` is not a server endpoint; declare it `server fun`.").
- Origins: each body (the function's and each lambda's) records where its effects come from. A
  lambda's origins join the function's only when its row flows into the function's row (it is called,
  directly, through a `val`, or by the function it is passed to), so diagnostics never point into a
  lambda that never runs.

### Domains as ceilings

| Function | Ceiling |
|---|---|
| `pure fun` | `{}` |
| `server fun` | `{db.read, db.write}` |
| `client fun`, `client component` | `{server.call}` |
| `fun` (no modifier), `worker`, `database`, `external` | none |

Ceilings are checked after each function body (after the whole recursive group for mutually
recursive functions): a concrete label outside the ceiling is E7401 at the function, with the path to
the operation and a hint chosen by (domain, label). Ceilings never travel to callers. They constrain
only concrete labels: effect variables are polymorphic in every domain, and the caller is charged when
it instantiates the variable. E7401 is reported once, at the function that performs the effect: a
caller whose path goes through a function already reported for the same label is not reported again.

```text
BIND ERROR E7401
`tela` is client code (it runs in the browser) and cannot perform `db.read`.
Binding path:
    tela
      -> buscar (src/app.bd:9:5)
      -> db.clientes.require (src/app.bd:4:40)
```

E7404 is structural: any use of a `server fun` name in client code, as a call or as a value, is
rejected, even if it has no effects. Reach it with `server.f(...)`.

`pure` means effect-free: a pure function may call any effect-free function, including an effect-free
`server fun`.

### Exact contracts

The body is inferred under a fresh context row and, after the body (after the whole group for
mutually recursive functions), compared with the declaration. An effect produced but not declared is
E7402; one declared but not produced is E7403. Both E7401 and E7402 carry the binding path from the
function to the operation, and E7402 quotes the clause as written. E7403 is not reported for a
function whose body already has an error.

Declared effect variables are enforced exactly in every function with an `effects` clause, annotated
or not: a declared variable whose row gained concrete labels is E7402 (with the path), and a declared
variable the body never uses is E7403:

```text
BIND ERROR E7402
`chama` performs the effects of `f` (effect variable `e`), but declares only [].
```

A function has its signature known in advance, so it is rigid and joins the annotated path, when its
parameters and result are annotated and its effects are known: it has an `effects [...]` clause, it is
`pure fun` (`[]`), or it is `server fun` (no clause means `[]`). All others, including
`client component` and `fun` without a clause, have their effects inferred with the call-graph
components; their type annotations still apply as constraints. Consequence: polymorphic recursion needs
`effects [...]` (or `pure`).

### Capabilities

The capabilities a program requires come from the typed effects: the existing domain capabilities
(`ui.dom` for client code, `server.http` for server code) plus the inferred resource labels
(`db.read`, `db.write`). `server.call` is not a capability. The typed program records the resolved,
sorted labels of each function; the generated artifacts, including `bind-manifest.json`, are unchanged.

### Diagnostics

| Code | When |
|---|---|
| E7401 | an effect exceeds the domain's ceiling |
| E7402 | an effect is performed but not declared |
| E7403 | an effect is declared but not performed |
| E7404 | a `server fun` is used directly in `client` code, or `server.f` names a non-endpoint |
| E7405 | unknown effect label or effect variable, or an effect variable that is not lowercase |

## Patterns and exhaustiveness

```text
p ::= _ | x | literal | null | C | C(p1, …, pn)
```

A pattern is checked against the scrutinee's type (or, inside a constructor pattern, the field's type):

- `_` and a variable `x` are irrefutable. A variable binds the type at its position as is, at any
  depth: in `when (u) { U(n, e) -> … }` with `data class U(val nome: String, val email: String?)`,
  `e : String?`, and `when (x) { n -> n }` over `Int?` gives `n : Int?`.
- Smart cast: in a `when` whose scrutinee has type `T?`, a top-level variable arm that comes after a
  top-level `null` arm binds `T`, and for exhaustiveness it counts as the non-null case (`null` is
  already covered). So `when (x) { null -> 0; n -> n }` over `Int?` has `n : Int`, and over `Int??`
  `when (x) { null -> 0; y -> y ?: 1 }` has `y : Int?`.
- A constructor or literal pattern checked against a type already known to be `T?` describes a non-null
  value and is checked against `T`. Exactly one optional layer is unwrapped per pattern level: over
  `Int??`, the pattern `1` is checked against `Int?` and rejected.

The rule is independent of arm order except for the smart cast, which by definition applies only after
a `null` arm. The elaborated patterns of each `when`, with optional layers explicit, are recorded in the
typed program for later passes.

A bare pattern name that is not a constructor and starts with an uppercase letter is rejected as an
unknown constructor (E7103), so a misspelled constructor cannot become a catch-all. A name bound twice in
one pattern is E7113.

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
| `server.f` | the scheme of function `f`; only the program's own functions resolve (not builtins or environment values; E7103) |
| an `environment` value | its declared type (`T?` with `envOrNull`); a function with the same name is E7110 |
| `UI { … }`, `Div(…) { … }`, … | `Ui`; arguments must have a known type, `String` or `Int`; every child is `Ui` |
| `Text(x)` | `Ui`; `x` must have a known type: `String`, `Int`, `Long`, `Double`, `Float`, `Decimal`, `Boolean`, `UUID`, `Date`, `DateTime`, `Instant`, `Duration`, `Money`, `Email`, `Url`, or a distinct type over one of them (E7105) |

A name declared by the program (local, parameter, function or constructor) takes precedence over these
forms: a user constructor named like a UI node (for example `data class Link`) is a constructor, not the
UI form. `a?.f` requires `a : A?` and has type `F?`.

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
inferred types are stable under re-annotation, exhaustiveness agrees with brute-force enumeration, and
the parser round-trips its own output. There is no mechanized proof yet.

Inference is principal (every accepted annotation is an instance of the inferred type) except for two
documented, order-dependent conveniences: the flattening of `?.` (see below) and the resolution of a
field access on a value of still unknown type to the only record type that declares the field. Both
depend on what is known when the expression is checked; annotating the value restores principality.

## Known limits

- Nesting is bounded so that hostile input is reported rather than crashing the compiler: expressions,
  patterns and types nest at most 256 levels deep ("Expression is nested too deeply (limit 256).",
  E7004), and the operator chains being parsed hold at most 1000 operators ("Operator chain is too long
  (limit 1000).", E7004).

- `a?.f` flattens to `F?` when the field type `F` is already known to be nullable at that point. When
  that is only learned later (an unannotated generic receiver), the result may be `F??`.
- UI node and `Text` arguments must have a known type: unannotated helper parameters used there need
  annotations.
- Non-termination: a recursive function may loop forever; termination is not checked.
- Effects of caches, queues and files have no operations yet; there is a single database (`db`).
- A row holds at most one effect variable (two distinct ones are E7004). So `f: () -> A ! [db.read, e]`
  and `g: () -> A ! [db.write, e]` cannot be called in the same inferred function: set-based rows
  cannot share a tail with different labels. For the same reason, passing a function of the caller's
  own recursive group as a callback to a function that adds effects to it (`'r ~ {db.read | 'r}`) is
  rejected with "Effect mismatch" (E7101).
- A declared effect can be justified by a recursive cycle: E7403 is not raised when the effect comes
  only through the cycle (an over-approximation, so safe).
- Diagnostic paths stop at locals, and E7401 is reported at the function (the path gives the operation).
- Known gap: the body of an effect-free `server fun` can be reached without `server.` through `pure`
  or plain helpers, since `pure` only means effect-free.
- Known gap: a `client fun` body can run caller-chosen effects through effect variables when it is
  called from server or plain code: `client fun t(f: () -> A ! [e]) = f()` called from a plain function
  with a lambda that reads the database performs `db.read` inside `t`, because ceilings constrain only
  concrete labels and the caller is the one charged.
- No refinement types: properties of values (`balance >= 0`) are not checked.
- `"\$name"` (an escaped dollar followed by a letter) is rejected as interpolation, because the lexer
  has already decoded the escape. Write `"$" + "name"`.
