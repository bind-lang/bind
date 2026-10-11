//! Runtime support for Bind-generated servers.
//!
//! This file is compiled both as the `bind-runtime` crate and, copied verbatim, as the
//! module `crate::bind_runtime` inside generated crates. It therefore never uses absolute
//! `crate::` paths.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]

use std::fmt;
use std::sync::Arc;

use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgArgumentBuffer, PgTypeInfo, PgValueRef};

/// Maximum nesting of Bind function calls before `AppError::RecursionLimit`.
pub const MAX_DEPTH: u32 = 10_000;

/// Future returned by every generated Bind function.
pub type BoxFuture<T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, AppError>> + Send>>;

/// Error type of every generated Bind function and handler.
#[derive(Debug)]
pub enum AppError {
    BadRequest(String),
    NotFound(String),
    Database(sqlx::Error),
    Serialization(serde_json::Error),
    Overflow(String),
    RecursionLimit,
    Internal(String),
    /// A server call's response the JS frontend could not decode (a non-finite number, an
    /// `Int` of more than `MAX_INT_DIGITS` digits, a `Long` beyond ±(2^53 − 1)): in the `ssr`
    /// mode, the error of the same call in the `split` mode, whose screen says
    /// `TEXT_REQUEST_FAILED` (see [`wire_response`]).
    RequestFailed,
}

impl AppError {
    /// HTTP status: 400 for `BadRequest`, 404 for `NotFound`, 500 for everything else.
    pub fn status(&self) -> u16 {
        match self {
            AppError::BadRequest(_) => 400,
            AppError::NotFound(_) => 404,
            AppError::Database(_)
            | AppError::Serialization(_)
            | AppError::Overflow(_)
            | AppError::RecursionLimit
            | AppError::Internal(_)
            | AppError::RequestFailed => 500,
        }
    }

    /// Message that is safe to send to clients. Only `BadRequest` and `NotFound` carry their
    /// own text (it is authored by the generated code and describes the client's input or the
    /// missing resource); all other variants map to a fixed generic message and never expose
    /// database, serialization or internal details.
    pub fn public_message(&self) -> String {
        match self {
            AppError::BadRequest(message) => message.clone(),
            AppError::NotFound(_) => "not found".to_string(),
            AppError::Database(_) => "database error".to_string(),
            AppError::Serialization(_) => "serialization error".to_string(),
            AppError::Overflow(_) => "numeric overflow".to_string(),
            AppError::RecursionLimit => "recursion limit exceeded".to_string(),
            AppError::Internal(_) => "internal error".to_string(),
            AppError::RequestFailed => TEXT_REQUEST_FAILED.to_string(),
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::BadRequest(m) => write!(f, "bad request: {m}"),
            AppError::NotFound(m) => write!(f, "not found: {m}"),
            AppError::Database(e) => write!(f, "database error: {e}"),
            AppError::Serialization(e) => write!(f, "serialization error: {e}"),
            AppError::Overflow(m) => write!(f, "overflow: {m}"),
            AppError::RecursionLimit => write!(f, "recursion limit exceeded"),
            AppError::Internal(m) => write!(f, "internal error: {m}"),
            AppError::RequestFailed => write!(f, "the response cannot be decoded by the frontend"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        AppError::Database(error)
    }
}

impl From<serde_json::Error> for AppError {
    fn from(error: serde_json::Error) -> Self {
        AppError::Serialization(error)
    }
}

type SharedTransaction =
    Arc<tokio::sync::Mutex<Option<sqlx::Transaction<'static, sqlx::Postgres>>>>;

/// Call context: optional shared transaction plus the current call depth.
#[derive(Clone)]
pub struct Ctx {
    transaction: Option<SharedTransaction>,
    depth: u32,
}

impl Ctx {
    /// Context without a database (tests, pure code).
    pub fn pure() -> Ctx {
        Ctx {
            transaction: None,
            depth: 0,
        }
    }

    pub fn with_transaction(tx: sqlx::Transaction<'static, sqlx::Postgres>) -> Ctx {
        Ctx {
            transaction: Some(Arc::new(tokio::sync::Mutex::new(Some(tx)))),
            depth: 0,
        }
    }

    /// Context of a server function called by the SSR: the same transaction, depth 0, as the
    /// separate RPC the JS frontend makes.
    pub fn server_call(&self) -> Ctx {
        Ctx {
            transaction: self.transaction.clone(),
            depth: 0,
        }
    }

    /// Context for a nested call: depth + 1, or `RecursionLimit` past `MAX_DEPTH`.
    pub fn enter(&self) -> Result<Ctx, AppError> {
        if self.depth >= MAX_DEPTH {
            return Err(AppError::RecursionLimit);
        }
        Ok(Ctx {
            transaction: self.transaction.clone(),
            depth: self.depth + 1,
        })
    }

    /// Locks the shared transaction and returns a guard dereferencing to it. `Internal` when
    /// the context has no database or the transaction was already taken.
    ///
    /// The guard must be held only around a single query, after all of that query's arguments
    /// have been evaluated: the tokio `Mutex` is not re-entrant, so evaluating an argument
    /// that itself needs the transaction while the guard is held would deadlock.
    ///
    /// The future and the guard own their handle on the transaction (no borrow of `self`), so
    /// generated code can hold the guard across a query inside a boxed `Send` future.
    pub fn transaction(
        &self,
    ) -> impl std::future::Future<Output = Result<TransactionGuard, AppError>> + Send + 'static
    {
        let shared = self.transaction.clone();
        async move {
            let shared = shared.ok_or_else(no_transaction)?;
            tokio::sync::OwnedMutexGuard::try_map(shared.lock_owned().await, |slot| slot.as_mut())
                .map_err(|_| no_transaction())
        }
    }

    /// Takes the transaction out of the context (for commit/rollback by the router),
    /// regardless of how many clones of the context exist. `Internal` when the context has
    /// no database or the transaction was already taken. Later `transaction()` calls on any
    /// clone fail with `Internal`.
    pub fn take_transaction(
        &self,
    ) -> impl std::future::Future<
        Output = Result<sqlx::Transaction<'static, sqlx::Postgres>, AppError>,
    > + Send
    + 'static {
        let shared = self.transaction.clone();
        async move {
            let shared = shared.ok_or_else(no_transaction)?;
            let taken = shared.lock_owned().await.take();
            taken.ok_or_else(no_transaction)
        }
    }
}

/// Exclusive access to the request's transaction (see [`Ctx::transaction`]).
pub type TransactionGuard = tokio::sync::OwnedMappedMutexGuard<
    Option<sqlx::Transaction<'static, sqlx::Postgres>>,
    sqlx::Transaction<'static, sqlx::Postgres>,
>;

fn no_transaction() -> AppError {
    AppError::Internal("no database transaction available in this context".to_string())
}

/// Arbitrary-precision integer. JSON: canonical string. PostgreSQL: integral `numeric`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Int(pub num_bigint::BigInt);

impl Int {
    pub fn from_i64(value: i64) -> Int {
        Int(num_bigint::BigInt::from(value))
    }

    /// Builds a literal from sign and base-2^32 digits, least significant first.
    pub fn from_parts(negative: bool, digits: &[u32]) -> Int {
        let sign = if negative {
            num_bigint::Sign::Minus
        } else {
            num_bigint::Sign::Plus
        };
        Int(num_bigint::BigInt::from_biguint(
            sign,
            num_bigint::BigUint::new(digits.to_vec()),
        ))
    }

    pub fn add(&self, o: &Int) -> Int {
        Int(&self.0 + &o.0)
    }

    pub fn sub(&self, o: &Int) -> Int {
        Int(&self.0 - &o.0)
    }

    pub fn mul(&self, o: &Int) -> Int {
        Int(&self.0 * &o.0)
    }

    pub fn neg(&self) -> Int {
        Int(-&self.0)
    }

    /// Division truncating toward zero; `None` when the divisor is zero.
    pub fn div_checked(&self, o: &Int) -> Option<Int> {
        if o.0 == num_bigint::BigInt::default() {
            None
        } else {
            Some(Int(&self.0 / &o.0))
        }
    }

    /// Remainder with the sign of the dividend; `None` when the divisor is zero.
    pub fn rem_checked(&self, o: &Int) -> Option<Int> {
        if o.0 == num_bigint::BigInt::default() {
            None
        } else {
            Some(Int(&self.0 % &o.0))
        }
    }

    pub fn to_index(&self) -> Option<usize> {
        usize::try_from(&self.0).ok()
    }
}

impl fmt::Display for Int {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Longest accepted `Int` JSON string, in digits (the optional '-' not counted). Bounds the
/// cost of parsing untrusted input.
pub const MAX_INT_DIGITS: usize = 1000;

/// Canonical form: `-?[0-9]+`, no leading zeros except "0" itself, no "-0", no `+`/spaces.
fn parse_canonical_int(text: &str) -> Option<num_bigint::BigInt> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let negative = digits.len() != text.len();
    if digits.is_empty()
        || digits.len() > MAX_INT_DIGITS
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    if digits.starts_with('0') && (digits.len() > 1 || negative) {
        return None;
    }
    digits
        .parse::<num_bigint::BigInt>()
        .ok()
        .map(|n| if negative { -n } else { n })
}

impl serde::Serialize for Int {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Int {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IntVisitor;

        impl serde::de::Visitor<'_> for IntVisitor {
            type Value = Int;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a canonical integer string such as \"-123\"")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Int, E> {
                parse_canonical_int(value)
                    .map(Int)
                    .ok_or_else(|| E::custom("invalid Int: expected a canonical integer string"))
            }
        }

        deserializer.deserialize_str(IntVisitor)
    }
}

impl sqlx::Type<sqlx::Postgres> for Int {
    fn type_info() -> PgTypeInfo {
        <bigdecimal::BigDecimal as sqlx::Type<sqlx::Postgres>>::type_info()
    }

    fn compatible(ty: &PgTypeInfo) -> bool {
        <bigdecimal::BigDecimal as sqlx::Type<sqlx::Postgres>>::compatible(ty)
    }
}

impl sqlx::Encode<'_, sqlx::Postgres> for Int {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        let decimal = bigdecimal::BigDecimal::from(self.0.clone());
        <bigdecimal::BigDecimal as sqlx::Encode<sqlx::Postgres>>::encode_by_ref(&decimal, buf)
    }
}

impl sqlx::Decode<'_, sqlx::Postgres> for Int {
    fn decode(value: PgValueRef<'_>) -> Result<Self, BoxDynError> {
        let decimal = <bigdecimal::BigDecimal as sqlx::Decode<sqlx::Postgres>>::decode(value)?;
        if !decimal.is_integer() {
            return Err("numeric value is not an integer".into());
        }
        let (integer, _) = decimal.with_scale(0).into_bigint_and_exponent();
        Ok(Int(integer))
    }
}

pub fn decimal_add(
    a: &rust_decimal::Decimal,
    b: &rust_decimal::Decimal,
) -> Result<rust_decimal::Decimal, AppError> {
    a.checked_add(*b)
        .ok_or_else(|| AppError::Overflow("decimal addition overflowed".to_string()))
}

pub fn decimal_sub(
    a: &rust_decimal::Decimal,
    b: &rust_decimal::Decimal,
) -> Result<rust_decimal::Decimal, AppError> {
    a.checked_sub(*b)
        .ok_or_else(|| AppError::Overflow("decimal subtraction overflowed".to_string()))
}

pub fn decimal_mul(
    a: &rust_decimal::Decimal,
    b: &rust_decimal::Decimal,
) -> Result<rust_decimal::Decimal, AppError> {
    a.checked_mul(*b)
        .ok_or_else(|| AppError::Overflow("decimal multiplication overflowed".to_string()))
}

pub fn decimal_neg(a: &rust_decimal::Decimal) -> Result<rust_decimal::Decimal, AppError> {
    rust_decimal::Decimal::ZERO
        .checked_sub(*a)
        .ok_or_else(|| AppError::Overflow("decimal negation overflowed".to_string()))
}

/// `Ok(None)` when the divisor is zero; `Err(Overflow)` when the quotient does not fit.
pub fn decimal_div_checked(
    a: &rust_decimal::Decimal,
    b: &rust_decimal::Decimal,
) -> Result<Option<rust_decimal::Decimal>, AppError> {
    if b.is_zero() {
        return Ok(None);
    }
    a.checked_div(*b)
        .map(Some)
        .ok_or_else(|| AppError::Overflow("decimal division overflowed".to_string()))
}

pub fn duration_add(
    a: &chrono::TimeDelta,
    b: &chrono::TimeDelta,
) -> Result<chrono::TimeDelta, AppError> {
    a.checked_add(b)
        .ok_or_else(|| AppError::Overflow("duration addition overflowed".to_string()))
}

pub fn duration_sub(
    a: &chrono::TimeDelta,
    b: &chrono::TimeDelta,
) -> Result<chrono::TimeDelta, AppError> {
    a.checked_sub(b)
        .ok_or_else(|| AppError::Overflow("duration subtraction overflowed".to_string()))
}

pub fn duration_neg(a: &chrono::TimeDelta) -> Result<chrono::TimeDelta, AppError> {
    chrono::TimeDelta::zero()
        .checked_sub(a)
        .ok_or_else(|| AppError::Overflow("duration negation overflowed".to_string()))
}

/// Bind's `Result<T, E>`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum BindResult<T, E> {
    Ok(T),
    Err(E),
}

pub fn first<T: Clone>(xs: &Arc<Vec<T>>) -> Option<T> {
    xs.first().cloned()
}

pub fn get<T: Clone>(xs: &Arc<Vec<T>>, index: &Int) -> Option<T> {
    index.to_index().and_then(|i| xs.get(i)).cloned()
}

pub fn length<T>(xs: &Arc<Vec<T>>) -> Int {
    Int(num_bigint::BigInt::from(xs.len()))
}

pub fn empty_list<T>() -> Arc<Vec<T>> {
    Arc::new(Vec::new())
}

pub fn list_of<T>(items: Vec<T>) -> Arc<Vec<T>> {
    Arc::new(items)
}

pub async fn map<A: Clone + Send + Sync + 'static, B: Send + 'static>(
    ctx: Ctx,
    xs: Arc<Vec<A>>,
    f: Arc<dyn Fn(Ctx, A) -> BoxFuture<B> + Send + Sync>,
) -> Result<Arc<Vec<B>>, AppError> {
    let mut out = Vec::with_capacity(xs.len());
    for x in xs.iter() {
        out.push(f(ctx.clone(), x.clone()).await?);
    }
    Ok(Arc::new(out))
}

pub async fn fold<A: Clone + Send + Sync + 'static, B: Send + 'static>(
    ctx: Ctx,
    xs: Arc<Vec<A>>,
    initial: B,
    f: Arc<dyn Fn(Ctx, B, A) -> BoxFuture<B> + Send + Sync>,
) -> Result<B, AppError> {
    let mut acc = initial;
    for x in xs.iter() {
        acc = f(ctx.clone(), acc, x.clone()).await?;
    }
    Ok(acc)
}

// ---------------------------------------------------------------------------------------------
// UI nodes, HTML rendering, screens and display functions. `core::display` in `bindc` is the
// canonical definition; these functions give the same bytes (checked against the shared vectors
// in `runtime/vectors/`).
// ---------------------------------------------------------------------------------------------

/// Text of the loading screen (the JS runtime uses the same value).
pub const TEXT_LOADING: &str = "Loading…";
/// Text shown when a request to the server fails (the JS runtime uses the same value).
pub const TEXT_REQUEST_FAILED: &str = "Request failed";
/// Title and heading of the error screen for a 400 response.
pub const TITLE_BAD_REQUEST: &str = "Bad request";
/// Title and heading of the error screen for a 404 response.
pub const TITLE_NOT_FOUND: &str = "Page not found";
/// Title and heading of the error screen for any other error.
pub const TITLE_ERROR: &str = "Something went wrong";

/// The element kinds of a UI value. `Text` and `Empty` are separate `Node` variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeTag {
    Ui,
    Page,
    Container,
    Div,
    Grid,
    Stack,
    Header,
    Nav,
    Main,
    Section,
    Article,
    Aside,
    Footer,
    Paragraph,
    Span,
    Strong,
    Emphasis,
    Small,
    List,
    Item,
    Pre,
    Code,
    Heading,
    Link,
    Rule,
}

impl NodeTag {
    /// The node for its Bind name (`"UI"` for the root); `Text` and `Empty` are not tags.
    pub fn from_name(name: &str) -> Option<NodeTag> {
        Some(match name {
            "UI" => NodeTag::Ui,
            "Page" => NodeTag::Page,
            "Container" => NodeTag::Container,
            "Div" => NodeTag::Div,
            "Grid" => NodeTag::Grid,
            "Stack" => NodeTag::Stack,
            "Header" => NodeTag::Header,
            "Nav" => NodeTag::Nav,
            "Main" => NodeTag::Main,
            "Section" => NodeTag::Section,
            "Article" => NodeTag::Article,
            "Aside" => NodeTag::Aside,
            "Footer" => NodeTag::Footer,
            "Paragraph" => NodeTag::Paragraph,
            "Span" => NodeTag::Span,
            "Strong" => NodeTag::Strong,
            "Emphasis" => NodeTag::Emphasis,
            "Small" => NodeTag::Small,
            "List" => NodeTag::List,
            "Item" => NodeTag::Item,
            "Pre" => NodeTag::Pre,
            "Code" => NodeTag::Code,
            "Heading" => NodeTag::Heading,
            "Link" => NodeTag::Link,
            "Rule" => NodeTag::Rule,
            _ => return None,
        })
    }

    /// The HTML element of a container tag (`None` for `Ui`, `Heading`, `Link` and `Rule`).
    fn container_element(self) -> Option<&'static str> {
        Some(match self {
            NodeTag::Page | NodeTag::Container | NodeTag::Div | NodeTag::Grid | NodeTag::Stack => {
                "div"
            }
            NodeTag::Header => "header",
            NodeTag::Nav => "nav",
            NodeTag::Main => "main",
            NodeTag::Section => "section",
            NodeTag::Article => "article",
            NodeTag::Aside => "aside",
            NodeTag::Footer => "footer",
            NodeTag::Paragraph => "p",
            NodeTag::Span => "span",
            NodeTag::Strong => "strong",
            NodeTag::Emphasis => "em",
            NodeTag::Small => "small",
            NodeTag::List => "ul",
            NodeTag::Item => "li",
            NodeTag::Pre => "pre",
            NodeTag::Code => "code",
            NodeTag::Ui | NodeTag::Heading | NodeTag::Link | NodeTag::Rule => return None,
        })
    }
}

/// An argument of a UI element.
#[derive(Clone, Debug, PartialEq)]
pub enum NodeArg {
    Str(String),
    Int(Int),
}

/// A UI value. Children are shared: a subtree used twice (a local shown twice) is one
/// subtree, never copied. Dropped iteratively: a tree of any depth is freed without growing
/// the call stack.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Element {
        tag: NodeTag,
        args: Vec<NodeArg>,
        children: Arc<Vec<Node>>,
    },
    Text(String),
    Empty,
}

impl Drop for Node {
    fn drop(&mut self) {
        // Only the last owner of a child list frees it; the children's own lists are moved to
        // `pending` (when this was their last owner too) before the children drop.
        let Node::Element { children, .. } = self else {
            return;
        };
        let Some(items) = Arc::get_mut(children) else {
            return;
        };
        let mut pending: Vec<Node> = std::mem::take(items);
        while let Some(mut node) = pending.pop() {
            if let Node::Element { children, .. } = &mut node
                && let Some(items) = Arc::get_mut(children)
            {
                pending.append(items);
            }
        }
    }
}

/// HTML escaping of text and attribute values: `&`, `<`, `>`, `"` and `'`.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The HTML of a UI value, byte for byte that of `render_html_value` in `bindc`: the `UI`
/// root is a fragment, `Text` an escaped text node, `Empty` nothing; arguments are used
/// leniently (heading level clamped to 1..=6, hrefs with a scheme other than http/https/mailto/tel emptied, extra arguments
/// ignored). The page shell (`page_html`) adds the `bind-site` wrapper. Iterative, with an
/// explicit stack: a tree of any depth renders without growing the call stack.
pub fn render_html(node: &Node) -> String {
    enum Step<'a> {
        Node(&'a Node),
        Close(&'static str),
    }
    let mut out = String::new();
    let mut stack = vec![Step::Node(node)];
    while let Some(step) = stack.pop() {
        let (tag, args, children) = match step {
            Step::Close(element) => {
                out.push_str("</");
                out.push_str(element);
                out.push('>');
                continue;
            }
            Step::Node(Node::Empty) => continue,
            Step::Node(Node::Text(text)) => {
                out.push_str(&escape_html(text));
                continue;
            }
            Step::Node(Node::Element {
                tag,
                args,
                children,
            }) => (*tag, args, children),
        };
        let arg = |index: usize| args.get(index).map(arg_text);
        let mut attributes: Vec<(&str, String)> = Vec::new();
        let mut push = |name: &'static str, value: Option<String>| {
            if let Some(value) = value {
                attributes.push((name, value));
            }
        };
        let element: &'static str = match tag {
            NodeTag::Ui => {
                for child in children.iter().rev() {
                    stack.push(Step::Node(child));
                }
                continue;
            }
            NodeTag::Heading => {
                let level = match args.first() {
                    Some(NodeArg::Int(value)) => heading_level(value),
                    _ => 1,
                };
                push("class", arg(1));
                match level {
                    1 => "h1",
                    2 => "h2",
                    3 => "h3",
                    4 => "h4",
                    5 => "h5",
                    _ => "h6",
                }
            }
            NodeTag::Link => {
                let href = arg(0).map(|url| {
                    if !is_allowed_href(&url) {
                        String::new()
                    } else {
                        url
                    }
                });
                push("href", href);
                push("class", arg(1));
                let target = arg(2);
                let blank = target.as_deref() == Some("_blank");
                push("target", target);
                if blank {
                    push("rel", Some("noopener noreferrer".to_string()));
                }
                push("aria-label", arg(3));
                "a"
            }
            NodeTag::Rule => {
                push("class", arg(0));
                out.push_str("<hr");
                write_attributes(&mut out, &attributes);
                out.push('>');
                continue;
            }
            container => {
                push("class", arg(0));
                push("id", arg(1));
                container.container_element().unwrap_or("div")
            }
        };
        out.push('<');
        out.push_str(element);
        write_attributes(&mut out, &attributes);
        out.push('>');
        stack.push(Step::Close(element));
        for child in children.iter().rev() {
            stack.push(Step::Node(child));
        }
    }
    out
}

fn arg_text(arg: &NodeArg) -> String {
    match arg {
        NodeArg::Str(text) => text.clone(),
        NodeArg::Int(value) => value.to_string(),
    }
}

fn write_attributes(out: &mut String, attributes: &[(&str, String)]) {
    for (name, value) in attributes {
        out.push(' ');
        out.push_str(name);
        out.push_str("=\"");
        out.push_str(&escape_html(value));
        out.push('"');
    }
}

/// The heading level of an `Int`, clamped to 1..=6.
fn heading_level(value: &Int) -> u8 {
    match i64::try_from(&value.0) {
        Ok(level) => level.clamp(1, 6) as u8,
        Err(_) if value.0.sign() == num_bigint::Sign::Minus => 1,
        Err(_) => 6,
    }
}

/// Whether a link's `href` may be emitted as is: the URL is normalized like a browser URL
/// parser does (leading and trailing characters <= U+0020 stripped, ASCII tab, LF and CR
/// removed anywhere); when the result has a scheme (`^[A-Za-z][A-Za-z0-9+.-]*:` before any
/// `/`, `?` or `#`) only `http`, `https`, `mailto` and `tel` (ASCII case-insensitive) are
/// allowed; a URL without a scheme (relative path, `/...`, `#...`, `?...`, `//host`) is allowed.
fn is_allowed_href(url: &str) -> bool {
    let normalized: String = url
        .trim_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    let mut chars = normalized.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return true;
    }
    let mut scheme = normalized[..1].to_string();
    for c in chars {
        if c == ':' {
            return ["http", "https", "mailto", "tel"]
                .iter()
                .any(|allowed| scheme.eq_ignore_ascii_case(allowed));
        }
        if c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-') {
            scheme.push(c);
        } else {
            return true;
        }
    }
    true
}

/// The metadata of the page shell (`[web]` in `bind.toml`): the document title, the optional
/// description (meta description and og:description), the language (`<html lang>`) and the
/// stylesheet URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageMeta<'a> {
    pub title: &'a str,
    pub description: Option<&'a str>,
    pub language: &'a str,
    pub stylesheet: &'a str,
}

/// The page icon of the shell (an inline SVG "B"), written as is.
pub const PAGE_ICON: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 64 64'%3E%3Crect width='64' height='64' fill='%2311130f'/%3E%3Ctext x='32' y='45' text-anchor='middle' font-family='monospace' font-size='40' font-weight='700' fill='%23c8ff36'%3EB%3C/text%3E%3C/svg%3E";

/// The single page shell of both modes: a full HTML document with the metadata of `meta` (each
/// value HTML-escaped; the description lines only when there is one) and `body` (already HTML)
/// inside the one `<div class="bind-site">`. Line by line the markup of the former generator's
/// `index.html`.
pub fn page_html(meta: &PageMeta, body: &str) -> String {
    let description = meta.description.map(escape_html);
    let line = |text: String| format!("    {text}\n");
    let mut head = String::new();
    head.push_str(&line("<meta charset=\"utf-8\">".into()));
    head.push_str(&line(
        "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">".into(),
    ));
    if let Some(description) = &description {
        head.push_str(&line(format!(
            "<meta name=\"description\" content=\"{description}\">"
        )));
    }
    head.push_str(&line(
        "<meta name=\"theme-color\" content=\"#efeee8\">".into(),
    ));
    let title = escape_html(meta.title);
    head.push_str(&line(format!(
        "<meta property=\"og:title\" content=\"{title}\">"
    )));
    if let Some(description) = &description {
        head.push_str(&line(format!(
            "<meta property=\"og:description\" content=\"{description}\">"
        )));
    }
    head.push_str(&line(
        "<meta property=\"og:type\" content=\"website\">".into(),
    ));
    head.push_str(&line(format!("<title>{title}</title>")));
    head.push_str(&line(format!("<link rel=\"icon\" href=\"{PAGE_ICON}\">")));
    head.push_str(&line(format!(
        "<link rel=\"stylesheet\" href=\"{}\">",
        escape_html(meta.stylesheet)
    )));
    format!(
        "<!doctype html>\n<html lang=\"{}\">\n  <head>\n{head}  </head>\n  <body>\n    <div class=\"bind-site\">{body}</div>\n  </body>\n</html>\n",
        escape_html(meta.language)
    )
}

/// The status and the markup of the error screen body: the title for the status class and the
/// public message (never internal details), inside `<main class="bind-error">`.
pub fn error_body(error: &AppError) -> (u16, String) {
    let status = error.status();
    let title = match status {
        400 => TITLE_BAD_REQUEST,
        404 => TITLE_NOT_FOUND,
        _ => TITLE_ERROR,
    };
    let body = format!(
        "<main class=\"bind-error\"><h1>{}</h1><p>{}</p></main>",
        escape_html(title),
        escape_html(&error.public_message())
    );
    (status, body)
}

/// The status and HTML of a whole error page: `page_html(meta, error_body)`.
pub fn error_page(error: &AppError, meta: &PageMeta) -> (u16, String) {
    let (status, body) = error_body(error);
    (status, page_html(meta, &body))
}

/// ECMAScript `Number::toString(10)` of `value` (what `String(x)` gives in JavaScript).
pub fn js_number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value == 0.0 {
        return "0".into();
    }
    if value < 0.0 {
        return format!("-{}", js_number_to_string(-value));
    }
    if value.is_infinite() {
        return "Infinity".into();
    }
    let (digits, exponent) = shortest_digits(value);
    let k = digits.len() as i64;
    let n = exponent + 1;
    if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        let (whole, fraction) = digits.split_at_checked(n as usize).unwrap_or((&digits, ""));
        format!("{whole}.{fraction}")
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let sign = if n - 1 < 0 { '-' } else { '+' };
        let (first, rest) = digits.split_at_checked(1).unwrap_or((&digits, ""));
        let rest = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        format!("{first}{rest}e{sign}{}", (n - 1).abs())
    }
}

/// The shortest round-trip digits of a finite positive `value` and the exponent of the first
/// one, as ECMAScript selects them: `{:e}` gives the fewest digits `k`, but breaks a tie
/// between two equally near candidates upward where ECMAScript takes the even one, so the
/// value is rounded exactly to `k` digits (half to even) when that reads back as the value.
fn shortest_digits(value: f64) -> (String, i64) {
    let split = |scientific: String| -> (String, i64) {
        let (mantissa, exponent) = scientific
            .split_once('e')
            .unwrap_or((scientific.as_str(), "0"));
        let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
        (digits, exponent.parse().unwrap_or(0))
    };
    let shortest = split(format!("{value:e}"));
    let precision = shortest.0.len().saturating_sub(1);
    let rounded = format!("{value:.precision$e}");
    if rounded.parse::<f64>() == Ok(value) {
        let (digits, exponent) = split(rounded);
        let trimmed = digits.trim_end_matches('0');
        if !trimmed.is_empty() {
            return (trimmed.to_string(), exponent);
        }
    }
    shortest
}

/// The text of a `Decimal`: its `Display` (trailing zeros and the sign of a negated zero kept).
pub fn display_decimal(value: &rust_decimal::Decimal) -> String {
    value.to_string()
}

/// The text of a `TimeDelta`: its `Display`, an ISO-8601 duration in seconds.
pub fn display_duration(value: &chrono::TimeDelta) -> String {
    value.to_string()
}

/// The text of a value of a type whose wire form is a JSON string (`UUID`, `Date`,
/// `DateTime`, `Instant`): that string, which is what the JS frontend holds and shows.
pub fn display_text<T: serde::Serialize>(value: &T) -> Result<String, AppError> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(text) => Ok(text),
        other => Ok(other.to_string()),
    }
}

// ---------------------------------------------------------------------------------------------
// Server calls of the SSR page through the wire codecs of the split mode. In the `split` mode a
// `server.f(…)` call is an RPC: the JS frontend encodes the arguments with `JSON.stringify`, the
// server decodes them (`serde_json`, a 400 with serde's message when it cannot), runs `f` and
// writes the result with `serde_json`, which the JS decoders read (a "Request failed" screen
// when they cannot). The SSR page makes the same round trip in memory, so for every program and
// input both modes show the same page: NaN and ±Infinity become `null` (rejected by the decoder
// of the other side), an `Int` of more than `MAX_INT_DIGITS` digits is rejected, a `Long` beyond
// ±(2^53 − 1) cannot be sent (the JS `encodeLong` throws: an internal error) nor received.
// ---------------------------------------------------------------------------------------------

/// Largest integer a JS number holds exactly (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Why a value cannot be written as the JS frontend would write it.
#[derive(Debug)]
pub struct WireError(String);

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WireError {}

impl serde::ser::Error for WireError {
    fn custom<T: fmt::Display>(message: T) -> Self {
        WireError(message.to_string())
    }
}

/// A JSON string exactly as `JSON.stringify` writes it: `"` and `\` escaped, `\b`, `\f`, `\n`,
/// `\r`, `\t`, other controls below U+0020 as `\u00xx` (lowercase), everything else as is.
fn write_js_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u00");
                out.push(char::from(b"0123456789abcdef"[(c as usize) >> 4]));
                out.push(char::from(b"0123456789abcdef"[(c as usize) & 0xf]));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A serde `Serializer` writing the JSON text `JSON.stringify` gives for the JS frontend's
/// encoding of the same value: compact, fields in declaration order, numbers as JavaScript
/// writes them (`js_number_to_string`; NaN and ±Infinity as `null`), integers only within
/// ±(2^53 − 1) (the JS `encodeLong` throws beyond).
struct JsJson<'a> {
    out: &'a mut String,
}

/// The state of a JSON array or object being written.
struct JsCompound<'a, 'b> {
    json: &'b mut JsJson<'a>,
    first: bool,
    /// Closing text: `]`, `}`, `]}` or `}}` (a variant's payload inside its `{"name": …}`).
    close: &'static str,
}

impl JsJson<'_> {
    fn integer(&mut self, negative: bool, magnitude: u128) -> Result<(), WireError> {
        if magnitude > u128::from(MAX_SAFE_INTEGER) {
            return Err(WireError(
                "Long beyond the exact range of JSON numbers".to_string(),
            ));
        }
        if negative && magnitude != 0 {
            self.out.push('-');
        }
        self.out.push_str(&magnitude.to_string());
        Ok(())
    }

    fn open_variant(&mut self, variant: &str) {
        self.out.push('{');
        write_js_string(self.out, variant);
        self.out.push(':');
    }
}

impl JsCompound<'_, '_> {
    fn separator(&mut self) {
        if !self.first {
            self.json.out.push(',');
        }
        self.first = false;
    }

    fn key(&mut self, key: &str) {
        self.separator();
        write_js_string(self.json.out, key);
        self.json.out.push(':');
    }
}

impl<'a, 'b> serde::Serializer for &'b mut JsJson<'a> {
    type Ok = ();
    type Error = WireError;
    type SerializeSeq = JsCompound<'a, 'b>;
    type SerializeTuple = JsCompound<'a, 'b>;
    type SerializeTupleStruct = JsCompound<'a, 'b>;
    type SerializeTupleVariant = JsCompound<'a, 'b>;
    type SerializeMap = JsCompound<'a, 'b>;
    type SerializeStruct = JsCompound<'a, 'b>;
    type SerializeStructVariant = JsCompound<'a, 'b>;

    fn serialize_bool(self, value: bool) -> Result<(), WireError> {
        self.out.push_str(if value { "true" } else { "false" });
        Ok(())
    }

    fn serialize_i8(self, value: i8) -> Result<(), WireError> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i16(self, value: i16) -> Result<(), WireError> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i32(self, value: i32) -> Result<(), WireError> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i64(self, value: i64) -> Result<(), WireError> {
        self.integer(value < 0, u128::from(value.unsigned_abs()))
    }

    fn serialize_i128(self, value: i128) -> Result<(), WireError> {
        self.integer(value < 0, value.unsigned_abs())
    }

    fn serialize_u8(self, value: u8) -> Result<(), WireError> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u16(self, value: u16) -> Result<(), WireError> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u32(self, value: u32) -> Result<(), WireError> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u64(self, value: u64) -> Result<(), WireError> {
        self.integer(false, u128::from(value))
    }

    fn serialize_u128(self, value: u128) -> Result<(), WireError> {
        self.integer(false, value)
    }

    fn serialize_f32(self, value: f32) -> Result<(), WireError> {
        self.serialize_f64(f64::from(value))
    }

    fn serialize_f64(self, value: f64) -> Result<(), WireError> {
        if value.is_finite() {
            self.out.push_str(&js_number_to_string(value));
        } else {
            self.out.push_str("null");
        }
        Ok(())
    }

    fn serialize_char(self, value: char) -> Result<(), WireError> {
        write_js_string(self.out, value.encode_utf8(&mut [0; 4]));
        Ok(())
    }

    fn serialize_str(self, value: &str) -> Result<(), WireError> {
        write_js_string(self.out, value);
        Ok(())
    }

    fn serialize_bytes(self, value: &[u8]) -> Result<(), WireError> {
        use serde::ser::SerializeSeq;
        let mut seq = self.serialize_seq(Some(value.len()))?;
        for byte in value {
            seq.serialize_element(byte)?;
        }
        seq.end()
    }

    fn serialize_none(self) -> Result<(), WireError> {
        self.serialize_unit()
    }

    fn serialize_some<T: serde::Serialize + ?Sized>(self, value: &T) -> Result<(), WireError> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), WireError> {
        self.out.push_str("null");
        Ok(())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), WireError> {
        self.serialize_unit()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<(), WireError> {
        self.serialize_str(variant)
    }

    fn serialize_newtype_struct<T: serde::Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), WireError> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: serde::Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<(), WireError> {
        self.open_variant(variant);
        value.serialize(&mut *self)?;
        self.out.push('}');
        Ok(())
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<JsCompound<'a, 'b>, WireError> {
        self.out.push('[');
        Ok(JsCompound {
            json: self,
            first: true,
            close: "]",
        })
    }

    fn serialize_tuple(self, len: usize) -> Result<JsCompound<'a, 'b>, WireError> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<JsCompound<'a, 'b>, WireError> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<JsCompound<'a, 'b>, WireError> {
        self.open_variant(variant);
        self.out.push('[');
        Ok(JsCompound {
            json: self,
            first: true,
            close: "]}",
        })
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<JsCompound<'a, 'b>, WireError> {
        self.out.push('{');
        Ok(JsCompound {
            json: self,
            first: true,
            close: "}",
        })
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<JsCompound<'a, 'b>, WireError> {
        self.serialize_map(Some(len))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<JsCompound<'a, 'b>, WireError> {
        self.open_variant(variant);
        self.out.push('{');
        Ok(JsCompound {
            json: self,
            first: true,
            close: "}}",
        })
    }
}

impl serde::ser::SerializeSeq for JsCompound<'_, '_> {
    type Ok = ();
    type Error = WireError;

    fn serialize_element<T: serde::Serialize + ?Sized>(
        &mut self,
        value: &T,
    ) -> Result<(), WireError> {
        self.separator();
        value.serialize(&mut *self.json)
    }

    fn end(self) -> Result<(), WireError> {
        self.json.out.push_str(self.close);
        Ok(())
    }
}

impl serde::ser::SerializeTuple for JsCompound<'_, '_> {
    type Ok = ();
    type Error = WireError;

    fn serialize_element<T: serde::Serialize + ?Sized>(
        &mut self,
        value: &T,
    ) -> Result<(), WireError> {
        serde::ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<(), WireError> {
        serde::ser::SerializeSeq::end(self)
    }
}

impl serde::ser::SerializeTupleStruct for JsCompound<'_, '_> {
    type Ok = ();
    type Error = WireError;

    fn serialize_field<T: serde::Serialize + ?Sized>(
        &mut self,
        value: &T,
    ) -> Result<(), WireError> {
        serde::ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<(), WireError> {
        serde::ser::SerializeSeq::end(self)
    }
}

impl serde::ser::SerializeTupleVariant for JsCompound<'_, '_> {
    type Ok = ();
    type Error = WireError;

    fn serialize_field<T: serde::Serialize + ?Sized>(
        &mut self,
        value: &T,
    ) -> Result<(), WireError> {
        serde::ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<(), WireError> {
        serde::ser::SerializeSeq::end(self)
    }
}

impl serde::ser::SerializeMap for JsCompound<'_, '_> {
    type Ok = ();
    type Error = WireError;

    fn serialize_key<T: serde::Serialize + ?Sized>(&mut self, key: &T) -> Result<(), WireError> {
        // As serde_json (and a JS object): string keys, other scalars as their text.
        let key = match serde_json::to_value(key) {
            Ok(serde_json::Value::String(text)) => text,
            Ok(serde_json::Value::Number(number)) => number.to_string(),
            Ok(serde_json::Value::Bool(value)) => value.to_string(),
            _ => return Err(WireError("key must be a string".to_string())),
        };
        self.key(&key);
        Ok(())
    }

    fn serialize_value<T: serde::Serialize + ?Sized>(
        &mut self,
        value: &T,
    ) -> Result<(), WireError> {
        value.serialize(&mut *self.json)
    }

    fn end(self) -> Result<(), WireError> {
        self.json.out.push_str(self.close);
        Ok(())
    }
}

impl serde::ser::SerializeStruct for JsCompound<'_, '_> {
    type Ok = ();
    type Error = WireError;

    fn serialize_field<T: serde::Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), WireError> {
        self.key(key);
        value.serialize(&mut *self.json)
    }

    fn end(self) -> Result<(), WireError> {
        self.json.out.push_str(self.close);
        Ok(())
    }
}

impl serde::ser::SerializeStructVariant for JsCompound<'_, '_> {
    type Ok = ();
    type Error = WireError;

    fn serialize_field<T: serde::Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), WireError> {
        serde::ser::SerializeStruct::serialize_field(self, key, value)
    }

    fn end(self) -> Result<(), WireError> {
        serde::ser::SerializeStruct::end(self)
    }
}

/// The JSON text of a server call's arguments as the JS frontend sends it
/// (`JSON.stringify(rt.record([[name, encoded value], …]))`): one field per parameter, in order.
#[derive(Debug)]
pub struct WireRequest {
    text: String,
}

impl Default for WireRequest {
    fn default() -> Self {
        WireRequest::new()
    }
}

impl WireRequest {
    pub fn new() -> WireRequest {
        WireRequest {
            text: String::from("{"),
        }
    }

    /// Adds the parameter `name` with `value`. A value the JS frontend cannot encode (a `Long`
    /// beyond ±(2^53 − 1): its `encodeLong` throws a `TypeError`) is an internal error, whose
    /// screen is the one the JS frontend shows for that exception.
    pub fn field<T: serde::Serialize + ?Sized>(
        &mut self,
        name: &str,
        value: &T,
    ) -> Result<(), AppError> {
        if self.text.len() > 1 {
            self.text.push(',');
        }
        write_js_string(&mut self.text, name);
        self.text.push(':');
        value
            .serialize(&mut JsJson {
                out: &mut self.text,
            })
            .map_err(|error| AppError::Internal(error.to_string()))
    }

    /// The request body.
    pub fn finish(mut self) -> String {
        self.text.push('}');
        self.text
    }
}

/// The JSON text `JSON.stringify` gives for a value (`WireRequest`'s writer), or why the JS
/// frontend could not encode it.
pub fn js_json<T: serde::Serialize + ?Sized>(value: &T) -> Result<String, WireError> {
    let mut out = String::new();
    value.serialize(&mut JsJson { out: &mut out })?;
    Ok(out)
}

/// Decodes a request body as the server's RPC route does: `BadRequest` with serde's message
/// (status 400, the message shown) when it does not fit the endpoint's input.
pub fn wire_decode_request<I: serde::de::DeserializeOwned>(body: &str) -> Result<I, AppError> {
    serde_json::from_str(body).map_err(|error| AppError::BadRequest(error.to_string()))
}

/// The result of a server call as the JS frontend receives it: serialized as the RPC route
/// does (`Serialization` when serde cannot, the route's 500), then read back as strictly as
/// the JS decoders read it: `RequestFailed` for a non-finite number (serde writes `null`), an
/// `Int` of more than `MAX_INT_DIGITS` digits or an integer beyond ±(2^53 − 1). The value read
/// back is what the page then uses (a `Decimal` `-0` reads as `0`, as in JS).
pub fn wire_response<T: serde::Serialize + serde::de::DeserializeOwned>(
    value: T,
) -> Result<T, AppError> {
    let json = serde_json::to_value(&value)?;
    drop(value);
    if !js_decodable_numbers(&json) {
        return Err(AppError::RequestFailed);
    }
    serde_json::from_value(json).map_err(|_| AppError::RequestFailed)
}

/// Whether every integer in `json` is one a JS number holds exactly (the JS `decodeLong`).
fn js_decodable_numbers(json: &serde_json::Value) -> bool {
    let mut pending = vec![json];
    while let Some(value) = pending.pop() {
        match value {
            serde_json::Value::Number(number) => {
                let safe = match (number.as_i64(), number.as_u64()) {
                    (Some(value), _) => value.unsigned_abs() <= MAX_SAFE_INTEGER,
                    (None, Some(value)) => value <= MAX_SAFE_INTEGER,
                    (None, None) => true,
                };
                if !safe {
                    return false;
                }
            }
            serde_json::Value::Array(items) => pending.extend(items),
            serde_json::Value::Object(fields) => pending.extend(fields.values()),
            _ => {}
        }
    }
    true
}

// ---------------------------------------------------------------------------------------------
// Page parameters (SSR): the query string of `GET /`, decoded like the JS runtime's `readParams`.
// ---------------------------------------------------------------------------------------------

/// The public message of a page parameter that is absent, repeated or invalid; the JS runtime's
/// `errorBody("BadParameter", name)` shows the same text.
pub fn bad_parameter_message(name: &str) -> String {
    format!("invalid parameter: {name}")
}

/// The error of a page parameter that is absent, repeated or invalid (status 400).
pub fn bad_parameter(name: &str) -> AppError {
    AppError::BadRequest(bad_parameter_message(name))
}

/// The pairs of a query string (`application/x-www-form-urlencoded`, an optional leading `?`):
/// split on `&` (empty pieces skipped), then on the first `=` (a piece without one has an
/// empty value); `+` is a space and `%XX` a byte, and the bytes must be UTF-8. Unlike
/// `URLSearchParams`, decoding is strict: a malformed `%` escape or invalid UTF-8 makes a value
/// invalid (the parameter is then rejected) and a name undecodable (the pair is ignored).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryParams {
    pairs: Vec<(String, Option<String>)>,
}

impl QueryParams {
    pub fn parse(query: &str) -> QueryParams {
        let query = query.strip_prefix('?').unwrap_or(query);
        let pairs = query
            .split('&')
            .filter(|piece| !piece.is_empty())
            .filter_map(|piece| {
                let (name, value) = piece.split_once('=').unwrap_or((piece, ""));
                Some((form_decode(name)?, form_decode(value)))
            })
            .collect();
        QueryParams { pairs }
    }

    /// The decoded values of every pair named `name`, in order (`None` for an invalid one).
    fn values(&self, name: &str) -> Vec<Option<&str>> {
        self.pairs
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_deref())
            .collect()
    }
}

/// `+` as a space, then percent-decoding; `None` on a malformed escape or invalid UTF-8.
fn form_decode(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut input = text.bytes();
    while let Some(byte) = input.next() {
        match byte {
            b'+' => bytes.push(b' '),
            b'%' => {
                let high = (input.next()? as char).to_digit(16)?;
                let low = (input.next()? as char).to_digit(16)?;
                bytes.push((high * 16 + low) as u8);
            }
            other => bytes.push(other),
        }
    }
    String::from_utf8(bytes).ok()
}

/// A page parameter type: decoded from its text, which must be canonical (the text the value
/// shows and serializes as), so the SSR and the JS frontend accept the same texts.
pub trait FromParam: Sized {
    fn from_param(text: &str) -> Option<Self>;
}

/// `String`, `Email`, `Url`: the text as is.
impl FromParam for String {
    fn from_param(text: &str) -> Option<Self> {
        Some(text.to_string())
    }
}

/// `Int`: canonical decimal digits (at most `MAX_INT_DIGITS`), as on the wire.
impl FromParam for Int {
    fn from_param(text: &str) -> Option<Self> {
        parse_canonical_int(text).map(Int)
    }
}

/// Decodes `text` as the JSON string of a `T`, accepted only when `T` serializes back to the
/// same text.
fn canonical_wire<T: serde::Serialize + serde::de::DeserializeOwned>(text: &str) -> Option<T> {
    let value: T = serde_json::from_value(serde_json::Value::String(text.to_string())).ok()?;
    let back = display_text(&value).ok()?;
    (back == text).then_some(value)
}

/// Whether the second of a `YYYY-MM-DDTHH:MM:SS…` text is below 60 (chrono reads a leap
/// second `:60`, which the JS frontend does not).
fn no_leap_second(text: &str) -> bool {
    text.get(17..19).is_some_and(|second| second < "60")
}

/// `UUID`: lowercase, hyphenated.
impl FromParam for uuid::Uuid {
    fn from_param(text: &str) -> Option<Self> {
        canonical_wire(text)
    }
}

/// `Date`: `YYYY-MM-DD`, a valid date of the years 0000 to 9999.
impl FromParam for chrono::NaiveDate {
    fn from_param(text: &str) -> Option<Self> {
        let unsigned = text.bytes().next().is_some_and(|b| b.is_ascii_digit());
        canonical_wire(text).filter(|_| unsigned)
    }
}

/// `DateTime`: `YYYY-MM-DDTHH:MM:SS`, then `.` and 3, 6 or 9 fraction digits when the
/// fraction is not zero (the shortest of those), years 0000 to 9999, no leap second.
impl FromParam for chrono::NaiveDateTime {
    fn from_param(text: &str) -> Option<Self> {
        let unsigned = text.bytes().next().is_some_and(|b| b.is_ascii_digit());
        canonical_wire(text).filter(|_| unsigned && no_leap_second(text))
    }
}

/// `Instant`: a `DateTime` text followed by `Z`.
impl FromParam for chrono::DateTime<chrono::Utc> {
    fn from_param(text: &str) -> Option<Self> {
        let unsigned = text.bytes().next().is_some_and(|b| b.is_ascii_digit());
        canonical_wire(text).filter(|_| unsigned && no_leap_second(text))
    }
}

/// The page parameter `name`: exactly one pair with that name, whose value decodes as a `T`;
/// otherwise `bad_parameter(name)`. Other pairs are ignored.
pub fn param<T: FromParam>(params: &QueryParams, name: &str) -> Result<T, AppError> {
    match params.values(name).as_slice() {
        [Some(text)] => T::from_param(text).ok_or_else(|| bad_parameter(name)),
        _ => Err(bad_parameter(name)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn int(text: &str) -> Int {
        serde_json::from_str(&format!("\"{text}\"")).unwrap()
    }

    #[test]
    fn script_text_is_escaped() {
        let html = render_html(&Node::Text("<script>alert(1)</script>".into()));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("<script"));
    }

    #[test]
    fn attributes_and_headings() {
        let link = Node::Element {
            tag: NodeTag::Link,
            args: vec![
                NodeArg::Str(" JavaScript:alert(1)".into()),
                NodeArg::Str("a\"b".into()),
                NodeArg::Str("_blank".into()),
            ],
            children: Arc::new(vec![Node::Text("x".into())]),
        };
        assert_eq!(
            render_html(&link),
            "<a href=\"\" class=\"a&quot;b\" target=\"_blank\" rel=\"noopener noreferrer\">x</a>"
        );
        let heading = |level: Option<NodeArg>| Node::Element {
            tag: NodeTag::Heading,
            args: level.into_iter().collect(),
            children: Arc::new(vec![]),
        };
        assert_eq!(
            render_html(&heading(Some(NodeArg::Int(int("0"))))),
            "<h1></h1>"
        );
        assert_eq!(
            render_html(&heading(Some(NodeArg::Int(int("-9"))))),
            "<h1></h1>"
        );
        assert_eq!(
            render_html(&heading(Some(NodeArg::Int(int("4"))))),
            "<h4></h4>"
        );
        assert_eq!(
            render_html(&heading(Some(NodeArg::Int(int("99999999999999999999"))))),
            "<h6></h6>"
        );
        assert_eq!(
            render_html(&heading(Some(NodeArg::Str("2".into())))),
            "<h1></h1>"
        );
        assert_eq!(render_html(&heading(None)), "<h1></h1>");
    }

    #[test]
    fn deep_trees_render_and_drop_without_growing_the_stack() {
        // 200,000 nested `Div`s, on a test thread's small stack.
        let depth = 200_000;
        let mut tree = Node::Text("x".into());
        for _ in 0..depth {
            tree = Node::Element {
                tag: NodeTag::Div,
                args: Vec::new(),
                children: Arc::new(vec![tree]),
            };
        }
        let html = render_html(&tree);
        assert_eq!(html.len(), depth * "<div></div>".len() + 1);
        assert!(html.starts_with("<div><div>") && html.ends_with("</div></div>"));
        // A copy shares the children; dropping both frees the tree once, iteratively.
        let copy = tree.clone();
        drop(tree);
        assert_eq!(render_html(&copy).len(), html.len());
        drop(copy);
    }

    #[test]
    fn requests_are_written_as_json_stringify_writes_them() {
        #[derive(serde::Serialize)]
        struct Record {
            #[serde(rename = "__proto__")]
            proto: String,
            maybe: Option<Option<i64>>,
            amount: rust_decimal::Decimal,
            number: Int,
            ratio: f32,
            list: Vec<f64>,
        }
        #[derive(serde::Serialize)]
        enum Shape {
            #[serde(rename = "circle")]
            Circle { radius: Int },
            #[serde(rename = "none")]
            Nothing,
        }
        let mut request = WireRequest::new();
        request
            .field("text", "a\"b\\c\n\u{8}\u{c}\u{1}\u{1f}é😀</script>")
            .unwrap();
        request.field("x", &f64::NAN).unwrap();
        request
            .field("y", &[1e21, 1e-7, -0.0, 0.1, 100.0, f64::INFINITY, 123.456])
            .unwrap();
        request
            .field(
                "r",
                &Record {
                    proto: "p".into(),
                    maybe: Some(None),
                    amount: rust_decimal::Decimal::new(-150, 2),
                    number: int("-12345678901234567890"),
                    ratio: 0.1,
                    list: vec![],
                },
            )
            .unwrap();
        request
            .field("s", &[Shape::Circle { radius: int("2") }, Shape::Nothing])
            .unwrap();
        request
            .field("ok", &BindResult::<i64, String>::Ok(9_007_199_254_740_991))
            .unwrap();
        request.field("unit", &()).unwrap();
        assert_eq!(
            request.finish(),
            concat!(
                r#"{"text":"a\"b\\c\n\b\f\u0001\u001fé😀</script>","x":null,"#,
                r#""y":[1e+21,1e-7,0,0.1,100,null,123.456],"#,
                r#""r":{"__proto__":"p","maybe":null,"amount":"-1.50","number":"-12345678901234567890","ratio":0.10000000149011612,"list":[]},"#,
                r#""s":[{"circle":{"radius":"2"}},"none"],"ok":{"Ok":9007199254740991},"unit":null}"#
            )
        );
        // A `Long` the JS frontend cannot encode: its `encodeLong` throws (an internal error).
        let mut request = WireRequest::new();
        assert!(matches!(
            request.field("n", &9_007_199_254_740_992_i64),
            Err(AppError::Internal(_))
        ));
        assert!(matches!(
            WireRequest::new().field("n", &i64::MIN),
            Err(AppError::Internal(_))
        ));
        assert_eq!(
            js_json(&-9_007_199_254_740_991_i64).unwrap(),
            "-9007199254740991"
        );
        assert_eq!(WireRequest::new().finish(), "{}");
    }

    #[test]
    fn requests_are_decoded_as_the_rpc_route_decodes_them() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct Input {
            x: f64,
            n: Int,
        }
        let mut request = WireRequest::new();
        request.field("x", &f64::NAN).unwrap();
        request.field("n", &int("1")).unwrap();
        match wire_decode_request::<Input>(&request.finish()) {
            Err(AppError::BadRequest(message)) => {
                assert_eq!(
                    message,
                    "invalid type: null, expected f64 at line 1 column 9"
                )
            }
            other => panic!("{other:?}"),
        }
        let digits = format!("1{}", "0".repeat(MAX_INT_DIGITS));
        let mut request = WireRequest::new();
        request.field("x", &1.5).unwrap();
        request.field("n", &int(&digits[..MAX_INT_DIGITS])).unwrap();
        assert!(wire_decode_request::<Input>(&request.finish()).is_ok());
        let body = format!("{{\"x\":1.5,\"n\":\"{digits}\"}}");
        assert!(matches!(
            wire_decode_request::<Input>(&body),
            Err(AppError::BadRequest(message)) if message.starts_with("invalid Int")
        ));
    }

    #[test]
    fn responses_are_read_back_as_the_js_decoders_read_them() {
        assert!(matches!(
            wire_response(f64::NAN),
            Err(AppError::RequestFailed)
        ));
        assert!(matches!(
            wire_response(f64::NEG_INFINITY),
            Err(AppError::RequestFailed)
        ));
        assert!(matches!(
            wire_response(vec![1.0_f32, f32::INFINITY]),
            Err(AppError::RequestFailed)
        ));
        assert_eq!(wire_response(0.1_f32).unwrap(), 0.1_f32);
        assert_eq!(
            wire_response(-0.0_f64).unwrap().to_bits(),
            (-0.0_f64).to_bits()
        );
        let big = Int(num_bigint::BigInt::from(10).pow(MAX_INT_DIGITS as u32));
        assert!(matches!(wire_response(big), Err(AppError::RequestFailed)));
        let fits = Int(num_bigint::BigInt::from(10).pow(MAX_INT_DIGITS as u32 - 1));
        assert_eq!(wire_response(fits.clone()).unwrap(), fits);
        assert!(matches!(
            wire_response(9_007_199_254_740_992_i64),
            Err(AppError::RequestFailed)
        ));
        assert!(matches!(
            wire_response(Some(-9_007_199_254_740_992_i64)),
            Err(AppError::RequestFailed)
        ));
        assert_eq!(
            wire_response(-9_007_199_254_740_991_i64).unwrap(),
            -9_007_199_254_740_991
        );
        assert_eq!(wire_response(Some(None::<i64>)).unwrap(), None);
        let zero = wire_response(-rust_decimal::Decimal::new(0, 2)).unwrap();
        assert_eq!(
            display_decimal(&zero),
            "0.00",
            "a negative zero reads back as zero"
        );
        let error = AppError::RequestFailed;
        assert_eq!(
            (error.status(), error.public_message().as_str()),
            (500, TEXT_REQUEST_FAILED)
        );
        let (status, body) = error_body(&error);
        assert_eq!(status, 500);
        assert_eq!(
            body,
            "<main class=\"bind-error\"><h1>Something went wrong</h1><p>Request failed</p></main>"
        );
    }

    #[test]
    fn page_and_error_pages() {
        let meta = PageMeta {
            title: "A & B",
            description: None,
            language: "en",
            stylesheet: "/s.css?x=\"1\"",
        };
        let page = page_html(&meta, "<p>hi</p>");
        assert!(page.starts_with("<!doctype html>\n<html lang=\"en\">"));
        assert!(page.contains("<title>A &amp; B</title>"));
        assert!(page.contains("<link rel=\"stylesheet\" href=\"/s.css?x=&quot;1&quot;\">"));
        assert!(page.contains("<div class=\"bind-site\"><p>hi</p></div>"));
        assert!(!page.contains("description"));

        let (status, html) = error_page(&AppError::NotFound("user 42 secret".into()), &meta);
        assert_eq!(status, 404);
        assert!(html.contains(TITLE_NOT_FOUND));
        assert!(html.contains("not found"));
        assert!(!html.contains("secret"));
        let (status, html) = error_page(&AppError::BadRequest("bad <id>".into()), &meta);
        assert_eq!(status, 400);
        assert!(html.contains(TITLE_BAD_REQUEST));
        assert!(html.contains("bad &lt;id&gt;"));
        let (status, html) = error_page(&AppError::Internal("db password".into()), &meta);
        assert_eq!(status, 500);
        assert!(html.contains(TITLE_ERROR));
        assert!(html.contains("internal error"));
        assert!(!html.contains("password"));
    }

    #[test]
    fn display_of_decimal_and_duration() {
        assert_eq!(display_decimal(&-Decimal::new(0, 2)), "-0.00");
        assert_eq!(display_decimal(&Decimal::new(150, 2)), "1.50");
        assert_eq!(
            display_duration(&chrono::TimeDelta::milliseconds(-500)),
            "-PT0.5S"
        );
        assert_eq!(display_duration(&chrono::TimeDelta::zero()), "P0D");
    }

    #[test]
    fn int_json_round_trip() {
        let value = int("12345678901234567890123");
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            "\"12345678901234567890123\""
        );
        assert_eq!(int("-42"), Int::from_i64(-42));
        assert_eq!(int("0"), Int::from_i64(0));
    }

    #[test]
    fn int_json_rejects_non_canonical() {
        for bad in [
            "\"1.5\"", "\" 1\"", "\"1 \"", "\"+1\"", "\"01\"", "\"1e3\"", "\"-0\"", "\"\"",
            "\"-\"", "1", "1.0", "null",
        ] {
            assert!(serde_json::from_str::<Int>(bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn int_digit_cap_and_negative_round_trip() {
        let ok = format!("\"1{}\"", "0".repeat(MAX_INT_DIGITS - 1));
        assert!(serde_json::from_str::<Int>(&ok).is_ok());
        let too_long = format!("\"1{}\"", "0".repeat(MAX_INT_DIGITS));
        assert!(serde_json::from_str::<Int>(&too_long).is_err());
        let neg_ok = format!("\"-1{}\"", "0".repeat(MAX_INT_DIGITS - 1));
        assert!(serde_json::from_str::<Int>(&neg_ok).is_ok());
        let neg_big = int("-98765432109876543210987654321");
        let json = serde_json::to_string(&neg_big).unwrap();
        assert_eq!(json, "\"-98765432109876543210987654321\"");
        assert_eq!(serde_json::from_str::<Int>(&json).unwrap(), neg_big);
    }

    #[test]
    fn int_from_parts() {
        let two_pow_32 = Int::from_parts(false, &[0, 1]);
        assert_eq!(two_pow_32, int("4294967296"));
        assert_eq!(Int::from_parts(true, &[5]), Int::from_i64(-5));
        assert_eq!(Int::from_parts(true, &[]), Int::from_i64(0));
    }

    #[test]
    fn int_arithmetic() {
        let a = Int::from_i64(-7);
        let b = Int::from_i64(2);
        assert_eq!(a.div_checked(&b), Some(Int::from_i64(-3)));
        assert_eq!(a.rem_checked(&b), Some(Int::from_i64(-1)));
        assert_eq!(a.div_checked(&Int::from_i64(0)), None);
        assert_eq!(a.rem_checked(&Int::from_i64(0)), None);
        assert_eq!(a.add(&b), Int::from_i64(-5));
        assert_eq!(a.sub(&b), Int::from_i64(-9));
        assert_eq!(a.mul(&b), Int::from_i64(-14));
        assert_eq!(a.neg(), Int::from_i64(7));
        assert_eq!(Int::from_i64(3).to_index(), Some(3));
        assert_eq!(a.to_index(), None);
        assert_eq!(int("99999999999999999999999999").to_index(), None);
    }

    #[test]
    fn decimal_overflow_and_division() {
        let one = Decimal::ONE;
        assert!(matches!(
            decimal_add(&Decimal::MAX, &one),
            Err(AppError::Overflow(_))
        ));
        assert!(matches!(
            decimal_mul(&Decimal::MAX, &Decimal::TWO),
            Err(AppError::Overflow(_))
        ));
        assert!(matches!(
            decimal_sub(&Decimal::MIN, &one),
            Err(AppError::Overflow(_))
        ));
        assert_eq!(decimal_add(&one, &one).unwrap(), Decimal::TWO);
        assert_eq!(decimal_neg(&one).unwrap(), -one);
        let tiny = Decimal::new(1, 28);
        assert!(matches!(
            decimal_div_checked(&Decimal::MAX, &tiny),
            Err(AppError::Overflow(_))
        ));
        assert!(matches!(
            decimal_div_checked(&one, &Decimal::ZERO),
            Ok(None)
        ));
        assert_eq!(
            decimal_div_checked(&Decimal::TWO, &one).unwrap(),
            Some(Decimal::TWO)
        );
    }

    #[test]
    fn duration_overflow() {
        let one = chrono::TimeDelta::seconds(1);
        assert_eq!(
            duration_add(&one, &one).unwrap(),
            chrono::TimeDelta::seconds(2)
        );
        assert_eq!(duration_sub(&one, &one).unwrap(), chrono::TimeDelta::zero());
        assert_eq!(duration_neg(&one).unwrap(), chrono::TimeDelta::seconds(-1));
        assert!(matches!(
            duration_add(&chrono::TimeDelta::MAX, &one),
            Err(AppError::Overflow(_))
        ));
        let neg_max = chrono::TimeDelta::MIN;
        assert!(matches!(
            duration_sub(&neg_max, &one),
            Err(AppError::Overflow(_))
        ));
        assert_eq!(
            duration_neg(&chrono::TimeDelta::MAX).unwrap(),
            chrono::TimeDelta::MIN
        );
        // Negating MIN either succeeds or reports Overflow; it must never panic.
        assert!(matches!(
            duration_neg(&neg_max),
            Ok(_) | Err(AppError::Overflow(_))
        ));
    }

    #[test]
    fn take_transaction_without_database_is_internal() {
        let rt = runtime();
        let ctx = Ctx::pure();
        assert!(matches!(
            rt.block_on(ctx.take_transaction()),
            Err(AppError::Internal(_))
        ));
        assert!(matches!(
            rt.block_on(ctx.transaction()),
            Err(AppError::Internal(_))
        ));
    }

    /// Needs a live PostgreSQL: runs only when `BIND_TEST_DATABASE_URL` is set.
    #[test]
    fn take_transaction_with_live_clone_then_again() {
        let Ok(url) = std::env::var("BIND_TEST_DATABASE_URL") else {
            return;
        };
        let rt = runtime();
        rt.block_on(async {
            let pool = sqlx::PgPool::connect(&url).await.unwrap();
            let ctx = Ctx::with_transaction(pool.begin().await.unwrap());
            let clone = ctx.enter().unwrap();
            assert!(clone.transaction().await.is_ok());
            let tx = ctx.take_transaction().await.unwrap();
            tx.rollback().await.unwrap();
            assert!(matches!(
                ctx.take_transaction().await,
                Err(AppError::Internal(_))
            ));
            assert!(matches!(
                clone.transaction().await,
                Err(AppError::Internal(_))
            ));
        });
    }

    #[test]
    fn server_calls_start_at_depth_zero() {
        let mut ctx = Ctx::pure();
        for _ in 0..MAX_DEPTH {
            ctx = ctx.enter().unwrap();
        }
        assert!(ctx.enter().is_err());
        assert!(ctx.server_call().enter().is_ok());
    }

    #[test]
    fn depth_limit() {
        let mut ctx = Ctx::pure();
        for _ in 0..MAX_DEPTH {
            ctx = ctx.enter().unwrap();
        }
        assert!(matches!(ctx.enter(), Err(AppError::RecursionLimit)));
    }

    #[test]
    fn statuses_and_public_messages() {
        assert_eq!(AppError::BadRequest("x".into()).status(), 400);
        assert_eq!(AppError::NotFound("x".into()).status(), 404);
        assert_eq!(AppError::RecursionLimit.status(), 500);
        assert_eq!(AppError::Overflow("x".into()).status(), 500);
        let secret = "password=hunter2 host=db.internal";
        let db = AppError::Database(sqlx::Error::Protocol(secret.to_string()));
        let internal = AppError::Internal(secret.to_string());
        let serde_err = serde_json::from_str::<Int>("1").unwrap_err();
        let ser = AppError::Serialization(serde_err);
        for error in [db, internal, ser] {
            assert_eq!(error.status(), 500);
            let message = error.public_message();
            assert!(!message.contains("hunter2") && !message.contains("db.internal"));
            assert!(!message.contains("invalid type"));
        }
        let not_found = AppError::NotFound("user 42 secret".into()).public_message();
        assert!(!not_found.contains("secret") && !not_found.contains("42"));
        assert_eq!(
            AppError::BadRequest("bad id".into()).public_message(),
            "bad id"
        );
    }

    #[test]
    fn lists() {
        let xs = list_of(vec![10, 20]);
        assert_eq!(first(&xs), Some(10));
        assert_eq!(get(&xs, &Int::from_i64(1)), Some(20));
        assert_eq!(get(&xs, &Int::from_i64(2)), None);
        assert_eq!(get(&xs, &Int::from_i64(-1)), None);
        assert_eq!(length(&xs), Int::from_i64(2));
        assert_eq!(first(&empty_list::<i32>()), None);
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_stack_size(64 * 1024 * 1024)
            .build()
            .unwrap()
    }

    #[test]
    fn map_and_fold() {
        let rt = runtime();
        let xs = list_of(vec![1_i64, 2, 3]);
        let double: Arc<dyn Fn(Ctx, i64) -> BoxFuture<i64> + Send + Sync> =
            Arc::new(|_ctx, x| Box::pin(async move { Ok(x * 2) }));
        let mapped = rt.block_on(map(Ctx::pure(), xs.clone(), double)).unwrap();
        assert_eq!(*mapped, vec![2, 4, 6]);

        let sum: Arc<dyn Fn(Ctx, i64, i64) -> BoxFuture<i64> + Send + Sync> =
            Arc::new(|_ctx, acc, x| Box::pin(async move { Ok(acc + x) }));
        assert_eq!(
            rt.block_on(fold(Ctx::pure(), xs.clone(), 100, sum))
                .unwrap(),
            106
        );

        let failing: Arc<dyn Fn(Ctx, i64) -> BoxFuture<i64> + Send + Sync> = Arc::new(|_ctx, x| {
            Box::pin(async move {
                if x == 2 {
                    Err(AppError::BadRequest("two".into()))
                } else {
                    Ok(x)
                }
            })
        });
        assert!(matches!(
            rt.block_on(map(Ctx::pure(), xs.clone(), failing)),
            Err(AppError::BadRequest(_))
        ));

        let failing_fold: Arc<dyn Fn(Ctx, i64, i64) -> BoxFuture<i64> + Send + Sync> =
            Arc::new(|_ctx, _acc, _x| Box::pin(async { Err(AppError::NotFound("n".into())) }));
        assert!(matches!(
            rt.block_on(fold(Ctx::pure(), xs, 0, failing_fold)),
            Err(AppError::NotFound(_))
        ));
    }

    fn recurse(ctx: Ctx, n: Int) -> BoxFuture<Int> {
        Box::pin(async move {
            let ctx = ctx.enter()?;
            if n == Int::from_i64(0) {
                Ok(n)
            } else {
                recurse(ctx, n.sub(&Int::from_i64(1))).await
            }
        })
    }

    #[test]
    fn deep_recursion_hits_limit_without_stack_overflow() {
        let rt = runtime();
        let joined =
            rt.block_on(async { tokio::spawn(recurse(Ctx::pure(), Int::from_i64(100_000))).await });
        assert!(matches!(joined.unwrap(), Err(AppError::RecursionLimit)));
        let ok =
            rt.block_on(async { tokio::spawn(recurse(Ctx::pure(), Int::from_i64(5_000))).await });
        assert_eq!(ok.unwrap().unwrap(), Int::from_i64(0));
    }
}
