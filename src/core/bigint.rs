//! The arbitrary-precision integer of `core::eval` (Bind's `Int`): what the evaluator needs and
//! nothing more, so bindc keeps no big-integer dependency in production (the static page is
//! pre-rendered by the evaluator). Its results equal `num-bigint`'s, checked by the tests below.
//!
//! Sign and magnitude: `limbs` holds the magnitude in base 2^32, least significant first, with
//! no trailing zero limb; zero has no limbs and is never negative. `/` truncates toward zero
//! and `%` has the sign of the dividend (both panic on a zero divisor, like `num-bigint`: the
//! evaluator checks the divisor first).

use std::cmp::Ordering;
use std::fmt;
use std::ops::{Add, Div, Mul, Neg, Rem, Sub};

/// Largest magnitude of an `Int` produced by arithmetic, in 32-bit limbs (32768 bits): a sum,
/// difference or product with more limbs is an overflow. The same bound as the Rust runtime's
/// `MAX_INT_BITS` (`32 * MAX_INT_LIMBS`) and the JS runtime's `MAX_INT_BITS`.
pub const MAX_INT_LIMBS: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BigInt {
    negative: bool,
    limbs: Vec<u32>,
}

impl BigInt {
    fn new(negative: bool, mut limbs: Vec<u32>) -> BigInt {
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
        let negative = negative && !limbs.is_empty();
        BigInt { negative, limbs }
    }

    /// Parses an optional `+` or `-` and at least one digit of `radix` (2 to 16); `None`
    /// otherwise.
    pub fn parse_bytes(bytes: &[u8], radix: u32) -> Option<BigInt> {
        if !(2..=16).contains(&radix) {
            return None;
        }
        let (negative, digits) = match bytes.split_first() {
            Some((b'-', rest)) => (true, rest),
            Some((b'+', rest)) => (false, rest),
            _ => (false, bytes),
        };
        if digits.is_empty() {
            return None;
        }
        let mut limbs: Vec<u32> = Vec::new();
        for &byte in digits {
            let digit = (byte as char).to_digit(radix)?;
            mul_small_add(&mut limbs, radix, digit);
        }
        Some(BigInt::new(negative, limbs))
    }

    /// The number of 32-bit limbs of the magnitude (0 for zero).
    pub fn limb_count(&self) -> usize {
        self.limbs.len()
    }

    /// Whether this value is beyond the arithmetic bound [`MAX_INT_LIMBS`].
    pub fn exceeds_int_bound(&self) -> bool {
        self.limbs.len() > MAX_INT_LIMBS
    }

    fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }
}

impl From<usize> for BigInt {
    fn from(value: usize) -> BigInt {
        let value = value as u64;
        BigInt::new(false, vec![value as u32, (value >> 32) as u32])
    }
}

impl TryFrom<&BigInt> for usize {
    type Error = ();

    fn try_from(value: &BigInt) -> Result<usize, ()> {
        if value.negative || value.limbs.len() > 2 {
            return Err(());
        }
        let mut result: u64 = 0;
        for (index, limb) in value.limbs.iter().enumerate() {
            result |= u64::from(*limb) << (32 * index);
        }
        usize::try_from(result).map_err(|_| ())
    }
}

/// `limbs = limbs * factor + addend`.
fn mul_small_add(limbs: &mut Vec<u32>, factor: u32, addend: u32) {
    let mut carry = u64::from(addend);
    for limb in limbs.iter_mut() {
        let value = u64::from(*limb) * u64::from(factor) + carry;
        *limb = value as u32;
        carry = value >> 32;
    }
    if carry > 0 {
        limbs.push(carry as u32);
    }
}

/// `limbs / divisor` in place; gives the remainder.
fn div_small(limbs: &mut [u32], divisor: u32) -> u32 {
    let mut remainder: u64 = 0;
    for limb in limbs.iter_mut().rev() {
        let value = (remainder << 32) | u64::from(*limb);
        *limb = (value / u64::from(divisor)) as u32;
        remainder = value % u64::from(divisor);
    }
    remainder as u32
}

fn compare_magnitude(a: &[u32], b: &[u32]) -> Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

fn add_magnitude(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u64;
    for index in 0..a.len().max(b.len()) {
        let sum = u64::from(a.get(index).copied().unwrap_or(0))
            + u64::from(b.get(index).copied().unwrap_or(0))
            + carry;
        result.push(sum as u32);
        carry = sum >> 32;
    }
    result.push(carry as u32);
    result
}

/// `a - b` for `|a| >= |b|`.
fn sub_magnitude(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (index, limb) in a.iter().enumerate() {
        let mut difference =
            i64::from(*limb) - i64::from(b.get(index).copied().unwrap_or(0)) - borrow;
        borrow = 0;
        if difference < 0 {
            difference += 1 << 32;
            borrow = 1;
        }
        result.push(difference as u32);
    }
    result
}

fn mul_magnitude(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = vec![0u32; a.len() + b.len()];
    for (i, x) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, y) in b.iter().enumerate() {
            let value = u64::from(*x) * u64::from(*y) + u64::from(result[i + j]) + carry;
            result[i + j] = value as u32;
            carry = value >> 32;
        }
        let mut k = i + b.len();
        while carry > 0 {
            let value = u64::from(result[k]) + carry;
            result[k] = value as u32;
            carry = value >> 32;
            k += 1;
        }
    }
    result
}

/// Quotient and remainder of the magnitudes (`b` not zero), by binary long division.
fn divmod_magnitude(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
    if compare_magnitude(a, b) == Ordering::Less {
        return (Vec::new(), a.to_vec());
    }
    if let [divisor] = b {
        let mut quotient = a.to_vec();
        let remainder = div_small(&mut quotient, *divisor);
        return (quotient, vec![remainder]);
    }
    let mut quotient = vec![0u32; a.len()];
    let mut remainder: Vec<u32> = Vec::new();
    for bit in (0..a.len() * 32).rev() {
        // remainder = remainder * 2 + bit
        let incoming = (a[bit / 32] >> (bit % 32)) & 1;
        mul_small_add(&mut remainder, 2, incoming);
        if compare_magnitude(&remainder, b) != Ordering::Less {
            remainder = sub_magnitude(&remainder, b);
            while remainder.last() == Some(&0) {
                remainder.pop();
            }
            quotient[bit / 32] |= 1 << (bit % 32);
        }
    }
    (quotient, remainder)
}

impl Add for BigInt {
    type Output = BigInt;

    fn add(self, other: BigInt) -> BigInt {
        if self.negative == other.negative {
            return BigInt::new(self.negative, add_magnitude(&self.limbs, &other.limbs));
        }
        match compare_magnitude(&self.limbs, &other.limbs) {
            Ordering::Less => BigInt::new(other.negative, sub_magnitude(&other.limbs, &self.limbs)),
            _ => BigInt::new(self.negative, sub_magnitude(&self.limbs, &other.limbs)),
        }
    }
}

impl Neg for BigInt {
    type Output = BigInt;

    fn neg(self) -> BigInt {
        BigInt::new(!self.negative, self.limbs)
    }
}

impl Sub for BigInt {
    type Output = BigInt;

    #[allow(clippy::suspicious_arithmetic_impl)]
    fn sub(self, other: BigInt) -> BigInt {
        self + -other
    }
}

impl Mul for BigInt {
    type Output = BigInt;

    fn mul(self, other: BigInt) -> BigInt {
        BigInt::new(
            self.negative != other.negative,
            mul_magnitude(&self.limbs, &other.limbs),
        )
    }
}

impl Div for BigInt {
    type Output = BigInt;

    fn div(self, other: BigInt) -> BigInt {
        assert!(!other.is_zero(), "division by zero");
        let (quotient, _) = divmod_magnitude(&self.limbs, &other.limbs);
        BigInt::new(self.negative != other.negative, quotient)
    }
}

impl Rem for BigInt {
    type Output = BigInt;

    fn rem(self, other: BigInt) -> BigInt {
        assert!(!other.is_zero(), "division by zero");
        let (_, remainder) = divmod_magnitude(&self.limbs, &other.limbs);
        BigInt::new(self.negative, remainder)
    }
}

impl Ord for BigInt {
    fn cmp(&self, other: &BigInt) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => compare_magnitude(&self.limbs, &other.limbs),
            (true, true) => compare_magnitude(&other.limbs, &self.limbs),
        }
    }
}

impl PartialOrd for BigInt {
    fn partial_cmp(&self, other: &BigInt) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for BigInt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_zero() {
            return formatter.write_str("0");
        }
        // Base 10^9 chunks, least significant first.
        let mut chunks = Vec::new();
        let mut limbs = self.limbs.clone();
        while !limbs.is_empty() {
            chunks.push(div_small(&mut limbs, 1_000_000_000));
            while limbs.last() == Some(&0) {
                limbs.pop();
            }
        }
        let mut text = String::new();
        if self.negative {
            text.push('-');
        }
        let mut rest = chunks.iter().rev();
        if let Some(first) = rest.next() {
            text.push_str(&first.to_string());
        }
        for chunk in rest {
            text.push_str(&format!("{chunk:09}"));
        }
        formatter.write_str(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn ours(text: &str) -> BigInt {
        BigInt::parse_bytes(text.as_bytes(), 10).unwrap()
    }

    fn theirs(text: &str) -> num_bigint::BigInt {
        num_bigint::BigInt::parse_bytes(text.as_bytes(), 10).unwrap()
    }

    /// Decimal integers of up to 60 digits, both signs, zero and powers of 2^32 near limbs.
    fn integer() -> impl Strategy<Value = String> {
        prop_oneof![
            "-?[1-9][0-9]{0,59}",
            Just("0".to_string()),
            Just("4294967296".to_string()),
            Just("-4294967295".to_string()),
            Just("18446744073709551616".to_string()),
            (0u32..200).prop_map(|n| (num_bigint::BigInt::from(1) << n).to_string()),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn arithmetic_and_order_agree_with_num_bigint(a in integer(), b in integer()) {
            let (x, y) = (ours(&a), ours(&b));
            let (p, q) = (theirs(&a), theirs(&b));
            prop_assert_eq!(x.to_string(), p.to_string());
            prop_assert_eq!((x.clone() + y.clone()).to_string(), (&p + &q).to_string());
            prop_assert_eq!((x.clone() - y.clone()).to_string(), (&p - &q).to_string());
            prop_assert_eq!((x.clone() * y.clone()).to_string(), (&p * &q).to_string());
            prop_assert_eq!((-x.clone()).to_string(), (-&p).to_string());
            prop_assert_eq!(x.cmp(&y), p.cmp(&q));
            prop_assert_eq!(x == y, p == q);
            if b != "0" {
                prop_assert_eq!((x.clone() / y.clone()).to_string(), (&p / &q).to_string());
                prop_assert_eq!((x.clone() % y.clone()).to_string(), (&p % &q).to_string());
            }
        }
    }

    #[test]
    fn parsing_radixes_and_conversions() {
        assert_eq!(BigInt::parse_bytes(b"ff", 16).unwrap().to_string(), "255");
        assert_eq!(BigInt::parse_bytes(b"-101", 2).unwrap().to_string(), "-5");
        assert_eq!(BigInt::parse_bytes(b"+7", 10).unwrap().to_string(), "7");
        assert_eq!(ours("-0").to_string(), "0");
        assert_eq!(ours("-0"), ours("0"));
        for bad in ["", "-", "+", "1a", "1_000", " 1"] {
            assert!(BigInt::parse_bytes(bad.as_bytes(), 10).is_none(), "{bad}");
        }
        assert_eq!(BigInt::from(usize::MAX).to_string(), usize::MAX.to_string());
        assert_eq!(usize::try_from(&ours("12345678901")), Ok(12345678901));
        assert_eq!(usize::try_from(&ours("-1")), Err(()));
        assert_eq!(usize::try_from(&ours("18446744073709551616")), Err(()));
        let thousand = "9".repeat(1000);
        assert_eq!(ours(&thousand).to_string(), thousand);
        assert_eq!(
            (ours(&thousand) / ours("99999999999999999999")).to_string(),
            (theirs(&thousand) / theirs("99999999999999999999")).to_string()
        );
    }
}
