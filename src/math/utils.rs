//! Shared numeric helpers for quote math.

use crate::math::{QuoteError, QuoteResult};

/// `a * b / c` in `u128`, surfacing overflow as [`QuoteError::MathOverflow`].
#[inline]
pub fn mul_div_u128(a: u128, b: u128, c: u128) -> QuoteResult<u128> {
    a.checked_mul(b)
        .ok_or(QuoteError::MathOverflow)?
        .checked_div(c)
        .ok_or(QuoteError::MathOverflow)
}

/// Constant-product exact-in swap, `floor(amount * out_reserve / (in_reserve + amount))`
/// (`swap_exact_input` in both programs).
#[inline]
pub fn swap_exact_input(amount: u128, in_reserve: u128, out_reserve: u128) -> QuoteResult<u128> {
    let denom = in_reserve
        .checked_add(amount)
        .ok_or(QuoteError::MathOverflow)?;
    mul_div_u128(amount, out_reserve, denom)
}

/// `value * (10_000 + bps) / 10_000`, saturating at `u64::MAX`.
#[inline]
pub fn add_slippage(value: u64, bps: u16) -> u64 {
    let scaled = u128::from(value) * (10_000 + u128::from(bps)) / 10_000;
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// `value * (10_000 - bps) / 10_000`. `bps` larger than 10_000 saturates to 0.
#[inline]
pub fn sub_slippage(value: u64, bps: u16) -> u64 {
    let bps = bps.min(10_000) as u128;
    (((value as u128) * (10_000 - bps)) / 10_000) as u64
}

/// `(value - delta, value + delta)` with `delta = value * bps / 10_000`; `None` on overflow.
#[inline]
pub fn slippage_bounds(value: u128, bps: u16) -> Option<(u128, u128)> {
    let delta = value.checked_mul(u128::from(bps))?.checked_div(10_000)?;
    Some((value.checked_sub(delta)?, value.checked_add(delta)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slippage_helpers_apply_correct_direction() {
        assert_eq!(sub_slippage(100, 250), 97);
        assert_eq!(add_slippage(100, 250), 102);
        assert_eq!(sub_slippage(1_000_000, 10_000), 0);
        assert_eq!(sub_slippage(1_000_000, 20_000), 0);
    }

    // The result used to be truncated with `as u64`, wrapping a large max-input
    // bound around to a small one.
    #[test]
    fn add_slippage_saturates_instead_of_wrapping() {
        assert_eq!(add_slippage(u64::MAX, 1), u64::MAX);
        assert_eq!(add_slippage(u64::MAX / 2 + 1, u16::MAX), u64::MAX);
        assert_eq!(add_slippage(u64::MAX, 0), u64::MAX);
    }

    #[test]
    fn slippage_bounds_brackets_value() {
        assert_eq!(slippage_bounds(1_000_000, 250), Some((975_000, 1_025_000)));
        assert_eq!(slippage_bounds(42, 0), Some((42, 42)));
    }

    #[test]
    fn slippage_bounds_overflow_returns_none() {
        assert_eq!(slippage_bounds(u128::MAX, 1), None);
    }
}
