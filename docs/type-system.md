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
inferred types are principal and stable under re-annotation, exhaustiveness agrees with brute-force
enumeration, and the parser round-trips its own output. There is no mechanized proof yet.

## Known limits

- `a?.f` flattens to `F?` when the field type `F` is already known to be nullable at that point. When
  that is only learned later (an unannotated generic receiver), the result may be `F??`.
- UI node and `Text` arguments must have a known type: unannotated helper parameters used there need
  annotations.
- Non-termination: a recursive function may loop forever; termination is not checked.
- Effects (`effects [db.read]`) and deployment domains are still verified by the legacy analysis, not
  by the type system.
- No refinement types: properties of values (`balance >= 0`) are not checked.
- `"\$name"` (an escaped dollar followed by a letter) is rejected as interpolation, because the lexer
  has already decoded the escape. Write `"$" + "name"`.
