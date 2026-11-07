//! Numbers as the database wrote them, compared and added without floating
//! point ([golden rule 9](../docs/README.md#golden-rules)).
//!
//! [`compare_numbers`] orders two numbers digit by digit; [`Sum`] adds any
//! amount of them exactly and divides the total by a count, which is what
//! the grid's sort and the selection statistics need.
use std::cmp::Ordering;

/// A number as the database wrote it (`-12.50`, `1.5e-05`), read without
/// floating point: `0.<digits> × 10^exponent`, where the digits are those
/// of `integer` then `fraction` after their leading zeros.
#[derive(Clone, Copy)]
pub(crate) struct Decimal<'a> {
    pub(crate) negative: bool,
    integer: &'a str,
    fraction: &'a str,
    /// Where the decimal point falls before the first significant digit.
    exponent: i64,
}

impl<'a> Decimal<'a> {
    /// `None` for anything that is not a decimal, with or without an
    /// exponent (`NaN`, `Infinity`, text).
    pub(crate) fn parse(text: &'a str) -> Option<Self> {
        let text = text.trim();
        let (negative, rest) = match text.as_bytes().first()? {
            b'-' => (true, &text[1..]),
            b'+' => (false, &text[1..]),
            _ => (false, text),
        };
        let (mantissa, exponent) = match rest.split_once(['e', 'E']) {
            Some((mantissa, exponent)) => {
                let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
                if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                (mantissa, exponent.parse::<i64>().ok()?)
            }
            None => (rest, 0),
        };
        let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        if integer.is_empty() && fraction.is_empty()
            || !integer
                .bytes()
                .chain(fraction.bytes())
                .all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let integer = integer.trim_start_matches('0');
        let point = if integer.is_empty() {
            -((fraction.len() - fraction.trim_start_matches('0').len()) as i64)
        } else {
            integer.len() as i64
        };
        Some(Self {
            negative,
            integer,
            fraction,
            exponent: point.checked_add(exponent)?,
        })
    }

    fn digits(&self) -> impl Iterator<Item = u8> + '_ {
        self.integer
            .bytes()
            .chain(self.fraction.bytes())
            .skip_while(|digit| *digit == b'0')
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.digits().next().is_none()
    }

    /// Orders the absolute values of two nonzero numbers.
    fn magnitude(&self, other: &Self) -> Ordering {
        self.exponent.cmp(&other.exponent).then_with(|| {
            // Missing digits are zeros, so `1.50` and `1.5` are equal.
            let (mut left, mut right) = (self.digits(), other.digits());
            loop {
                match (left.next(), right.next()) {
                    (None, None) => return Ordering::Equal,
                    (left, right) => {
                        let order = left.unwrap_or(b'0').cmp(&right.unwrap_or(b'0'));
                        if order.is_ne() {
                            return order;
                        }
                    }
                }
            }
        })
    }

    /// The significant digits as values 0–9 and how many of them fall after
    /// the decimal point (negative when zeros follow them: `12e3` is `12`
    /// with a scale of -3). Trailing zeros of the fraction are kept, so
    /// `1.50` has a scale of 2, as the database wrote it.
    fn coefficient(&self) -> (Vec<u8>, i64) {
        let digits: Vec<u8> = self
            .integer
            .bytes()
            .chain(self.fraction.bytes())
            .skip_while(|digit| *digit == b'0')
            .map(|digit| digit - b'0')
            .collect();
        let scale = digits.len() as i64 - self.exponent;
        (digits, scale)
    }
}

/// Orders two numbers exactly, as the database wrote them: decimals (also
/// in exponent form, as `float8` prints `1.5e-05`) are compared digit by
/// digit, never through floating point. Values that are not decimals
/// (`NaN`, `Infinity`) sort after them, by their text.
pub fn compare_numbers(left: &str, right: &str) -> Ordering {
    Number::new(left).cmp(&Number::new(right))
}

/// A value read once for [`compare_numbers`]' order, so that ordering many
/// values (a minimum, a median) does not parse each one at every
/// comparison.
#[derive(Clone, Copy)]
pub(crate) struct Number<'a> {
    pub(crate) text: &'a str,
    pub(crate) decimal: Option<Decimal<'a>>,
}

impl<'a> Number<'a> {
    pub(crate) fn new(text: &'a str) -> Self {
        Self {
            text,
            decimal: Decimal::parse(text),
        }
    }

    /// The order of [`compare_numbers`].
    pub(crate) fn cmp(&self, other: &Self) -> Ordering {
        // Negative, zero (with or without a sign), positive.
        let class = |number: &Decimal| match (number.is_zero(), number.negative) {
            (true, _) => 1,
            (false, true) => 0,
            (false, false) => 2,
        };
        match (&self.decimal, &other.decimal) {
            (Some(left), Some(right)) => match (class(left), class(right)) {
                (1, 1) => Ordering::Equal,
                (0, 0) => left.magnitude(right).reverse(),
                (2, 2) => left.magnitude(right),
                (left, right) => left.cmp(&right),
            },
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => self.text.cmp(other.text),
        }
    }
}

/// The smallest and largest of `values` in [`compare_numbers`]' order, each
/// value read once: the first of equal smallest values and the last of equal
/// largest ones, as `Iterator::min_by` and `max_by` choose.
pub fn min_max_numbers<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> Option<(&'a str, &'a str)> {
    let mut values = values.into_iter().map(Number::new);
    let first = values.next()?;
    let (min, max) = values.fold((first, first), |(min, max), value| {
        (
            if value.cmp(&min).is_lt() { value } else { min },
            if value.cmp(&max).is_ge() { value } else { max },
        )
    });
    Some((min.text, max.text))
}

/// Whether `text` is a decimal [`compare_numbers`] orders by value.
pub fn is_number(text: &str) -> bool {
    Decimal::parse(text).is_some()
}

/// Numbers whose digits reach further than this from the decimal point
/// (`1e400`, `1e-400`) are compared but not added: their sum would need
/// hundreds of digits per value.
pub const MAX_SCALE: i64 = 400;

/// Unsigned digits, least significant first, at the scale of the sum that
/// holds them.
type Digits = Vec<u8>;

/// An exact sum of decimals. Positive and negative values are added apart
/// and subtracted once at the end, so adding never borrows.
#[derive(Clone, Debug, Default)]
pub struct Sum {
    positive: Digits,
    negative: Digits,
    /// Digits after the decimal point, the largest scale added so far.
    scale: u32,
    /// How many values were added.
    count: usize,
    /// A value too far from the decimal point was skipped.
    overflow: bool,
}

impl Sum {
    /// Adds `text` when it is a decimal and returns whether it was one.
    pub fn add(&mut self, text: &str) -> bool {
        let Some(number) = Decimal::parse(text) else {
            return false;
        };
        self.add_decimal(&number);
        true
    }

    /// Adds a value already read.
    pub(crate) fn add_decimal(&mut self, number: &Decimal) {
        self.count += 1;
        if number.is_zero() {
            return;
        }
        let (digits, scale) = number.coefficient();
        if scale.abs() > MAX_SCALE {
            self.overflow = true;
            return;
        }
        // A value with more decimals widens the sum: its digits move up.
        let scale_u = scale.max(0) as u32;
        if scale_u > self.scale {
            let shift = (scale_u - self.scale) as usize;
            for total in [&mut self.positive, &mut self.negative] {
                if !total.is_empty() {
                    total.splice(0..0, std::iter::repeat_n(0, shift));
                }
            }
            self.scale = scale_u;
        }
        // Where the value's last digit lands in the sum.
        let offset = (self.scale as i64 - scale) as usize;
        let total = if number.negative {
            &mut self.negative
        } else {
            &mut self.positive
        };
        add_at(total, &digits, offset);
    }

    /// How many decimals were added (zeros included, other text not).
    pub fn count(&self) -> usize {
        self.count
    }

    /// The total as plain decimal text with the largest scale added
    /// (`1.50 + 2` is `3.50`), or `None` when nothing was added or a value
    /// was too large to add.
    pub fn total(&self) -> Option<String> {
        let (negative, digits) = self.signed()?;
        Some(format_digits(negative, &digits, self.scale))
    }

    /// The total divided by `divisor`, rounded half away from zero to
    /// `extra` decimals beyond the largest scale added, without trailing
    /// zeros past that scale.
    pub fn divided(&self, divisor: usize, extra: u32) -> Option<String> {
        if divisor == 0 {
            return None;
        }
        let (negative, mut digits) = self.signed()?;
        // One more digit than kept, to round on.
        digits.splice(0..0, std::iter::repeat_n(0, extra as usize + 1));
        let (mut quotient, _) = divide(&digits, divisor as u64);
        let round_up = quotient.first().is_some_and(|digit| *digit >= 5);
        quotient.remove(0);
        if round_up {
            add_at(&mut quotient, &[1], 0);
        }
        trim(&mut quotient);
        let scale = self.scale + extra;
        let mut text = format_digits(negative && !quotient.is_empty(), &quotient, scale);
        // Trailing zeros beyond the values' own scale say nothing.
        if extra > 0 && text.contains('.') {
            let keep = text
                .find('.')
                .map_or(0, |point| point + 1 + self.scale as usize);
            while text.len() > keep && text.ends_with('0') {
                text.pop();
            }
            if text.ends_with('.') {
                text.pop();
            }
        }
        Some(text)
    }

    /// The total as a sign and its digits (least significant first).
    fn signed(&self) -> Option<(bool, Digits)> {
        if self.count == 0 || self.overflow {
            return None;
        }
        let (mut positive, mut negative) = (self.positive.clone(), self.negative.clone());
        trim(&mut positive);
        trim(&mut negative);
        Some(match compare_digits(&positive, &negative) {
            Ordering::Less => (true, subtract(&negative, &positive)),
            _ => (false, subtract(&positive, &negative)),
        })
    }
}

/// Adds `digits` (most significant first) to `total` (least significant
/// first), with the last of `digits` at position `offset`.
fn add_at(total: &mut Digits, digits: &[u8], offset: usize) {
    let needed = offset + digits.len() + 1;
    if total.len() < needed {
        total.resize(needed, 0);
    }
    let mut carry = 0;
    let mut position = offset;
    for digit in digits.iter().rev() {
        let value = total[position] + digit + carry;
        total[position] = value % 10;
        carry = value / 10;
        position += 1;
    }
    while carry > 0 {
        if position == total.len() {
            total.push(0);
        }
        let value = total[position] + carry;
        total[position] = value % 10;
        carry = value / 10;
        position += 1;
    }
}

/// Drops the leading zeros (at the end, least significant first).
fn trim(digits: &mut Digits) {
    while digits.last() == Some(&0) {
        digits.pop();
    }
}

fn compare_digits(left: &[u8], right: &[u8]) -> Ordering {
    left.len()
        .cmp(&right.len())
        .then_with(|| left.iter().rev().cmp(right.iter().rev()))
}

/// `left - right` for `left >= right`, both trimmed.
fn subtract(left: &[u8], right: &[u8]) -> Digits {
    let mut result = Vec::with_capacity(left.len());
    let mut borrow = 0;
    for (position, digit) in left.iter().enumerate() {
        let take = right.get(position).copied().unwrap_or(0) + borrow;
        if *digit >= take {
            result.push(digit - take);
            borrow = 0;
        } else {
            result.push(digit + 10 - take);
            borrow = 1;
        }
    }
    trim(&mut result);
    result
}

/// Long division of `digits` (least significant first) by a small divisor.
fn divide(digits: &[u8], divisor: u64) -> (Digits, u64) {
    let mut quotient = vec![0; digits.len()];
    let mut remainder: u64 = 0;
    for (position, digit) in digits.iter().enumerate().rev() {
        let value = remainder * 10 + u64::from(*digit);
        quotient[position] = (value / divisor) as u8;
        remainder = value % divisor;
    }
    (quotient, remainder)
}

/// Plain decimal text for digits (least significant first) at `scale`.
fn format_digits(negative: bool, digits: &[u8], scale: u32) -> String {
    let scale = scale as usize;
    let mut text = String::with_capacity(digits.len().max(scale + 1) + 2);
    if negative && digits.iter().any(|digit| *digit != 0) {
        text.push('-');
    }
    let integer = digits.len().saturating_sub(scale);
    if integer == 0 {
        text.push('0');
    }
    for position in (scale..digits.len()).rev() {
        text.push((b'0' + digits[position]) as char);
    }
    if scale > 0 {
        text.push('.');
        for position in (0..scale).rev() {
            text.push((b'0' + digits.get(position).copied().unwrap_or(0)) as char);
        }
    }
    text
}

/// The same number written one way only (`1.50`, `1.5` and `15e-1` are all
/// `1.5`; `-0` is `0`), to count distinct values. Text that is not a
/// decimal, or too far from the decimal point to rewrite, stays as written.
pub fn canonical(text: &str) -> String {
    let Some(number) = Decimal::parse(text) else {
        return text.to_owned();
    };
    if number.is_zero() {
        return "0".into();
    }
    let (mut digits, mut scale) = number.coefficient();
    if scale.abs() > MAX_SCALE {
        return text.to_owned();
    }
    while scale > 0 && digits.last() == Some(&0) {
        digits.pop();
        scale -= 1;
    }
    let mut little: Digits = digits.into_iter().rev().collect();
    if scale < 0 {
        little.splice(0..0, std::iter::repeat_n(0, (-scale) as usize));
        scale = 0;
    }
    format_digits(number.negative, &little, scale as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(values: &[&str]) -> Sum {
        let mut sum = Sum::default();
        for value in values {
            sum.add(value);
        }
        sum
    }

    #[test]
    fn numbers_compare_exactly_in_any_written_form() {
        use Ordering::{Equal, Greater, Less};
        assert_eq!(compare_numbers("0", "-0"), Equal);
        assert_eq!(compare_numbers("1e5", "2"), Greater);
        assert_eq!(compare_numbers("1.5e-05", "0.0001"), Less);
        assert_eq!(compare_numbers("1.5e-05", "0.000015"), Equal);
        assert_eq!(compare_numbers("1e+20", "99999999999999999999"), Greater);
        assert_eq!(compare_numbers("10", "1E1"), Equal);
        assert_eq!(compare_numbers("1.50", "1.5"), Equal);
        assert_eq!(compare_numbers("-1e-3", "-0.01"), Greater);
        assert_eq!(compare_numbers("NaN", "1e300"), Greater);
        assert!(is_number("-12.50") && is_number("1e-5") && !is_number("1e") && !is_number(""));
    }

    #[test]
    fn sums_are_exact_and_keep_the_largest_scale() {
        assert_eq!(sum(&["1.50", "2"]).total().as_deref(), Some("3.50"));
        // 0.1 + 0.2 is 0.3, not 0.30000000000000004.
        assert_eq!(sum(&["0.1", "0.2"]).total().as_deref(), Some("0.3"));
        assert_eq!(sum(&["-5", "3.25"]).total().as_deref(), Some("-1.75"));
        assert_eq!(sum(&["-5", "5"]).total().as_deref(), Some("0"));
        assert_eq!(
            sum(&["1.5e-05", "1e3"]).total().as_deref(),
            Some("1000.000015")
        );
        // Past what a 128-bit integer or a float holds exactly.
        assert_eq!(
            sum(&[
                "99999999999999999999999999999999999999.9999999999",
                "0.0000000001"
            ])
            .total()
            .as_deref(),
            Some("100000000000000000000000000000000000000.0000000000")
        );
        // Text is not a number and is not counted.
        let mixed = sum(&["1", "NaN", "abc", "2"]);
        assert_eq!((mixed.count(), mixed.total().as_deref()), (2, Some("3")));
        assert_eq!(Sum::default().total(), None);
        assert_eq!(sum(&["1e500", "1"]).total(), None);
    }

    #[test]
    fn averages_round_half_away_from_zero_past_the_values_scale() {
        let values = sum(&["1", "2", "2"]);
        assert_eq!(values.divided(3, 6).as_deref(), Some("1.666667"));
        assert_eq!(
            sum(&["1.50", "2.50"]).divided(2, 6).as_deref(),
            Some("2.00")
        );
        assert_eq!(sum(&["-1", "-2"]).divided(2, 6).as_deref(), Some("-1.5"));
        assert_eq!(sum(&["10"]).divided(4, 0).as_deref(), Some("3"));
        assert_eq!(sum(&["-10"]).divided(4, 0).as_deref(), Some("-3"));
        assert_eq!(sum(&["1"]).divided(0, 6), None);
    }

    #[test]
    fn canonical_text_names_one_value_once() {
        assert_eq!(canonical("1.50"), "1.5");
        assert_eq!(canonical("15e-1"), "1.5");
        assert_eq!(canonical("-0.0"), "0");
        assert_eq!(canonical("1.2e3"), "1200");
        assert_eq!(canonical("007"), "7");
        assert_eq!(canonical("NaN"), "NaN");
    }
}
