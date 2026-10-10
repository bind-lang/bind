# Kotlin lexical compatibility contract

Bind source uses Kotlin's lexical surface as its compatibility baseline. The normative reference for the current implementation is:

- Kotlin specification branch: `release`
- Kotlin specification revision: `2f7aa0524ec27e788dfacd550f144809f2e0254c`
- Grammar: `grammar/src/main/antlr/KotlinLexer.g4`
- Unicode categories: `grammar/src/main/antlr/UnicodeClasses.g4`

Official sources:

- <https://kotlinlang.org/grammar/>
- <https://github.com/Kotlin/kotlin-spec/blob/release/grammar/src/main/antlr/KotlinLexer.g4>

## Guarantee boundary

The Bind lexer accepts the Kotlin lexical forms defined by that grammar revision:

- Kotlin whitespace, CR/LF/CRLF, shebangs, line comments, and nested block comments;
- Unicode identifiers in categories `Lu`, `Ll`, `Lt`, `Lm`, `Lo`, and digits in `Nd`;
- escaped backtick identifiers and labelled jumps/references;
- decimal, hexadecimal, binary, long, unsigned, float, exponent, and underscore-separated literals;
- character literals and Kotlin escape sequences;
- line strings, multiline raw strings, field references, and nested `${...}` interpolation;
- Kotlin separators and operators, using longest-match token boundaries.

Bind deliberately exposes coarser internal token categories than the ANTLR grammar. For example, hard and soft keywords share the `Identifier` category, and complete string literals are retained as one `String` token instead of exposing lexer-mode fragments. This does not change which source spellings are accepted, but means Bind's internal token stream is not an ABI-compatible copy of ANTLR's token IDs.

Lexical compatibility does not imply Kotlin parser or runtime compatibility. Bind adds declarations such as `server`, `client`, `entity`, `environment`, and `effects`, and its parser and semantic model remain Bind-specific.

When the Kotlin grammar revision changes, compatibility must be reviewed and this pinned revision updated together with conformance tests.
