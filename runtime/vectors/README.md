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
