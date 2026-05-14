//! Generic numeric helpers shared by the quote builders. Pure functions —
//! no domain types, no I/O.

/// `value * (10_000 + bps) / 10_000`. Saturates on the rare case the
/// multiplication itself overflows `u64`.
#[inline]
pub fn add_slippage(value: u64, bps: u16) -> u64 {
    (((value as u128) * (10_000 + bps as u128)) / 10_000) as u64
}

/// `value * (10_000 - bps) / 10_000`. `bps` larger than 10_000 saturates to 0.
#[inline]
pub fn sub_slippage(value: u64, bps: u16) -> u64 {
    let bps = bps.min(10_000) as u128;
    (((value as u128) * (10_000 - bps)) / 10_000) as u64
}

/// `(value - delta, value + delta)` where `delta = value * bps / 10_000`.
/// Returns `None` if any intermediate overflows `u128` — callers can map that
/// to [`crate::math::QuoteError::MathOverflow`]. Mirrors `handle_bps` from
/// `mayhem-program::math` so both `validate_market_cap` paths can share a
/// slippage envelope.
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
        // Sub: 100 * (10_000 - 250) / 10_000 = 97
        assert_eq!(sub_slippage(100, 250), 97);
        // Add: 100 * (10_000 + 250) / 10_000 = 102
        assert_eq!(add_slippage(100, 250), 102);
        // Saturates at full slippage
        assert_eq!(sub_slippage(1_000_000, 10_000), 0);
        assert_eq!(sub_slippage(1_000_000, 20_000), 0);
    }

    #[test]
    fn slippage_bounds_brackets_value() {
        // 1_000_000 * 250 / 10_000 = 25_000
        assert_eq!(slippage_bounds(1_000_000, 250), Some((975_000, 1_025_000)));
        // bps=0 collapses to a point envelope
        assert_eq!(slippage_bounds(42, 0), Some((42, 42)));
    }

    #[test]
    fn slippage_bounds_overflow_returns_none() {
        // Multiplication overflows u128.
        assert_eq!(slippage_bounds(u128::MAX, 1), None);
    }
}
