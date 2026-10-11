# Cross-runtime vectors

Shared test data that keeps the three implementations of the display and HTML functions in
agreement: `bindc` (`src/core/display.rs`, canonical), the Rust runtime
(`runtime/bind-runtime`) and the JS runtime. Each implementation has a test that reads these
files and asserts equality.

Regenerate from the canonical implementation with
`BIND_BLESS=1 cargo test core::display` (bindc only), and commit the result.

## `html-trees.json`

An array of `{ "tree": <tree>, "html": <string> }`; `html` is what `render_html_value` /
`render_html` must produce for `tree`.

A `<tree>` is `{ "node": <name>, "args": [<arg>...], "children": [<tree>...] }`.

- `node` is the Bind name of a UI node: `UI` (the root), `Page`, `Container`, `Div`, `Grid`,
  `Stack`, `Header`, `Nav`, `Main`, `Section`, `Article`, `Aside`, `Footer`, `Paragraph`,
  `Span`, `Strong`, `Emphasis`, `Small`, `List`, `Item`, `Pre`, `Code`, `Heading`, `Link`,
  `Rule`, `Text` or `Empty`.
- an `<arg>` is a JSON string (a `String`) or `{ "int": "<decimal digits>" }` (an `Int`,
  canonical text, arbitrary size).
- `Text` has one string argument; `Empty` has none. `args` and `children` are always present.

## `numbers.json`

An array of `{ "bits": <16 hex digits>, "text": <string> }`: the IEEE-754 bits of an `f64`
and its ECMAScript `Number.prototype.toString` text (`js_number_to_string`). Bits are used so
`NaN`, `-0` and infinities are unambiguous.

The values were cross-checked against node (`String(x)` of the `f64` with those bits) and
agree on every case. The bindc test also fails when the committed file differs from the
generated case list (run with `BIND_BLESS=1` after changing the list).

## `pages.json`

An array of cases checked by the runtime test (and the JS runtime), hand-written from the
specification of the page shell and the error screen:

- `{ "kind": "page", "title", "stylesheet", "body", "html" }`: `page_html(title, stylesheet,
  body)` must give `html` (title and stylesheet are HTML-escaped, `body` is inserted as is).
- `{ "kind": "error_body", "error", "message", "status", "html" }`: `error` is one of
  `BadRequest`, `NotFound`, `Internal`, `Overflow`, `RecursionLimit` with its `message`;
  `error_body(error)` must give `status` and `html` (`<main class="bind-error">` with the
  title of the status class and the public message, both escaped).

## Links

`html-trees.json` includes the `Link` href allowlist cases: after trimming characters
<= U+0020 and removing tab/LF/CR, a URL with a scheme is emitted only for `http`, `https`,
`mailto`, `tel` (case-insensitive); otherwise the `href` is `""`. URLs without a scheme are
emitted as is.

## JS runtime only: `Decimal` and `f32`

`runtime/bind-runtime-js/tests/decimal-vectors.json` and `f32-vectors.json` are generated from
`rust_decimal` and Rust's `f32` by `runtime/bind-runtime/tests/js_vectors.rs`, which fails when
the committed files differ from the generated content (regenerate with
`BIND_BLESS=1 cargo test --manifest-path runtime/bind-runtime/Cargo.toml --test js_vectors`).
They live next to the JS tests because only the JS runtime reimplements these operations.
