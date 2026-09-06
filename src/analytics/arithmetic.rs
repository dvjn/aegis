#![allow(dead_code)]
//! Engine-independent exact allocation arithmetic.
//!
//! SQLite stores scaled bounds as decimal text and never performs arithmetic on
//! those values.
use anyhow::{Context, Result, bail, ensure};
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{Signed, ToPrimitive, Zero};
use std::time::{Duration, Instant};

pub const SCALE: i64 = 1_000_000_000_000_000_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Interval {
    /// Sum of mathematical floors at `SCALE` subunits per nanodollar.
    pub lower: BigInt,
    /// Count of terms with a nonzero remainder. The upper endpoint is exclusive.
    pub remainders: BigInt,
}

impl Interval {
    pub fn add(&mut self, other: &Self) {
        self.lower += &other.lower;
        self.remainders += &other.remainders;
    }

    pub fn from_rational(value: &BigRational) -> Self {
        let scaled = value * BigRational::from_integer(BigInt::from(SCALE));
        let lower = floor(&scaled);
        let remainders = BigInt::from(scaled != BigRational::from_integer(lower.clone()));
        Self { lower, remainders }
    }

    pub fn from_decimal(lower: &str, remainders: &str) -> Result<Self> {
        let lower = lower
            .parse::<BigInt>()
            .context("invalid interval lower bound")?;
        let remainders = remainders
            .parse::<BigInt>()
            .context("invalid interval remainder count")?;
        ensure!(
            !remainders.is_negative(),
            "negative interval remainder count"
        );
        Ok(Self { lower, remainders })
    }

    pub fn decimal_parts(&self) -> (String, String) {
        (self.lower.to_string(), self.remainders.to_string())
    }

    /// Returns the only integer to which every value in
    /// `[lower, lower + remainders)` truncates toward zero.
    pub fn proven_truncation(&self) -> Option<BigInt> {
        let low = trunc_scaled(&self.lower);
        if self.remainders.is_zero() {
            return Some(low);
        }
        let upper = &self.lower + &self.remainders;
        // At a positive exclusive endpoint, inspect one scaled integer below
        // it. At zero or a negative endpoint, truncation's left-hand limit is
        // the endpoint's toward-zero quotient.
        let high = if upper > BigInt::zero() {
            trunc_scaled(&(upper - 1))
        } else {
            trunc_scaled(&upper)
        };
        (low == high).then_some(low)
    }

    pub fn proven_i64(&self) -> Result<Option<i64>> {
        self.proven_truncation()
            .map(|value| checked_i64(&value))
            .transpose()
    }
}

pub fn parse_rational(numerator: &str, denominator: &str) -> Result<BigRational> {
    let numerator = numerator
        .parse::<BigInt>()
        .context("invalid allocation numerator")?;
    let denominator = denominator
        .parse::<BigInt>()
        .context("invalid allocation denominator")?;
    ensure!(
        denominator > BigInt::zero(),
        "nonpositive allocation denominator"
    );
    Ok(BigRational::new(numerator, denominator))
}

pub fn allocate(cost_nanos: i64, numerator: &str, denominator: &str) -> Result<BigRational> {
    Ok(BigRational::from_integer(BigInt::from(cost_nanos))
        * parse_rational(numerator, denominator)?)
}

/// Allocates a byte weight against a request's total context bytes. A request
/// with zero total bytes contributes no known allocated cost.
pub fn allocate_weight(
    cost_nanos: Option<i64>,
    weight_numerator: &str,
    weight_denominator: &str,
    total_bytes: i64,
) -> Result<Option<BigRational>> {
    let Some(cost_nanos) = cost_nanos else {
        return Ok(None);
    };
    ensure!(total_bytes >= 0, "negative request context byte total");
    if total_bytes == 0 {
        return Ok(Some(BigRational::zero()));
    }
    Ok(Some(
        allocate(cost_nanos, weight_numerator, weight_denominator)? / BigInt::from(total_bytes),
    ))
}

pub fn tool_allocations(
    cost_nanos: Option<i64>,
    total_bytes: i64,
    definition_num: &str,
    definition_den: &str,
    transmission_num: &str,
    transmission_den: &str,
    budget: &mut ReportBudget,
) -> Result<(Option<BigRational>, Option<BigRational>)> {
    budget.consume_exact_terms(2)?;
    Ok((
        allocate_weight(cost_nanos, definition_num, definition_den, total_bytes)?,
        allocate_weight(cost_nanos, transmission_num, transmission_den, total_bytes)?,
    ))
}

pub fn checked_i64(value: &BigInt) -> Result<i64> {
    value
        .to_i64()
        .ok_or_else(|| anyhow::anyhow!("reported value exceeds signed 64-bit range"))
}

pub fn truncate_to_i64(value: &BigRational) -> Result<i64> {
    checked_i64(&(value.numer() / value.denom()))
}

fn floor(value: &BigRational) -> BigInt {
    let n = value.numer();
    let d = value.denom();
    let quotient = n / d;
    if n.is_negative() && n % d != BigInt::zero() {
        quotient - 1
    } else {
        quotient
    }
}

fn trunc_scaled(value: &BigInt) -> BigInt {
    value / SCALE
}

#[derive(Debug)]
pub struct ReportBudget {
    deadline: Instant,
    rows_left: usize,
    groups_left: usize,
    exact_terms_left: usize,
}

impl Default for ReportBudget {
    fn default() -> Self {
        Self::new(Duration::from_secs(2), 250_000, 25_000, 500_000)
    }
}

impl ReportBudget {
    pub fn new(deadline: Duration, rows: usize, groups: usize, exact_terms: usize) -> Self {
        Self {
            deadline: Instant::now() + deadline,
            rows_left: rows,
            groups_left: groups,
            exact_terms_left: exact_terms,
        }
    }

    pub fn consume_rows(&mut self, count: usize) -> Result<()> {
        Self::consume(&mut self.rows_left, count, "row")?;
        self.check_deadline()
    }

    pub fn consume_groups(&mut self, count: usize) -> Result<()> {
        Self::consume(&mut self.groups_left, count, "group")?;
        self.check_deadline()
    }

    pub fn consume_exact_terms(&mut self, count: usize) -> Result<()> {
        Self::consume(&mut self.exact_terms_left, count, "exact-term")?;
        self.check_deadline()
    }

    pub fn check_deadline(&self) -> Result<()> {
        if Instant::now() > self.deadline {
            bail!("report deadline exceeded");
        }
        Ok(())
    }

    fn consume(remaining: &mut usize, count: usize, resource: &str) -> Result<()> {
        *remaining = remaining
            .checked_sub(count)
            .ok_or_else(|| anyhow::anyhow!("report {resource} budget exceeded"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thirds_across_hours_require_one_exact_sum() {
        let third = allocate(1, "1", "3").unwrap();
        let mut interval = Interval::default();
        for _ in 0..3 {
            interval.add(&Interval::from_rational(&third));
        }
        assert_eq!(interval.proven_truncation(), None);
        assert_eq!(
            truncate_to_i64(&(third.clone() * BigInt::from(3))).unwrap(),
            1
        );
    }

    #[test]
    fn exact_point_and_open_bound_are_distinct() {
        let point = Interval::from_rational(&allocate(7, "1", "1").unwrap());
        assert_eq!(point.remainders, BigInt::zero());
        assert_eq!(point.proven_i64().unwrap(), Some(7));
        let open = Interval {
            lower: BigInt::from(SCALE - 1),
            remainders: BigInt::from(1),
        };
        assert_eq!(open.proven_i64().unwrap(), Some(0));
    }

    #[test]
    fn negatives_on_both_sides_of_zero_use_floor_then_truncate() {
        let negative = Interval::from_rational(&allocate(-1, "1", "3").unwrap());
        assert_eq!(negative.lower, BigInt::from(-333_333_333_333_333_334_i64));
        assert_eq!(negative.proven_i64().unwrap(), Some(0));
        let crossing = Interval {
            lower: BigInt::from(-1),
            remainders: BigInt::from(2),
        };
        assert_eq!(crossing.proven_i64().unwrap(), Some(0));
    }

    #[test]
    fn zero_total_bytes_and_unknown_price_are_distinct() {
        assert_eq!(
            allocate_weight(Some(9), "4", "1", 0).unwrap(),
            Some(BigRational::zero())
        );
        assert_eq!(allocate_weight(None, "4", "1", 8).unwrap(), None);
    }

    #[test]
    fn huge_rational_is_exact_and_final_overflow_is_reported() {
        let huge = allocate(i64::MAX, "999999999999999999999999", "1").unwrap();
        assert!(truncate_to_i64(&huge).is_err());
    }

    #[test]
    fn budget_exhaustion_is_explicit() {
        let mut budget = ReportBudget::new(Duration::from_secs(1), 1, 1, 1);
        budget.consume_rows(1).unwrap();
        assert!(
            budget
                .consume_rows(1)
                .unwrap_err()
                .to_string()
                .contains("row budget")
        );
    }
}
