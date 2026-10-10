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
            | AppError::Internal(_) => 500,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn int(text: &str) -> Int {
        serde_json::from_str(&format!("\"{text}\"")).unwrap()
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
