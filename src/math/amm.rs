//! AMM (pump-swap) quote helpers.

use solana_program::pubkey::Pubkey;

use crate::math::bonding_curve::TOKEN_SUPPLY;
use crate::math::fees::{ceil_div, compute_amm_fee_bps, creator_fee_amount, fee_amount, AmmFeeBps};
use crate::math::utils::{mul_div_u128, slippage_bounds};
use crate::math::{QuoteError, QuoteResult};
use crate::state::FeeConfig;
use crate::state::pump_amm::{ GlobalConfig};

pub struct BuyQuoteInputResult {
    pub base_amount_out: u64,
    pub effective_quote: u64,
}

pub struct BuyBaseInputResult {
    pub total_quote_in: u64,
    pub raw_quote_in: u64,
}

pub struct SellBaseInputResult {
    pub final_quote_out: u64,
    pub raw_quote_out: u64,
}

/// Common AMM trade context. `pool_creator` is the pool's anchor `creator`
/// field; `coin_creator` is the per-coin creator that receives the
/// coin-creator fee slice (set to `Pubkey::default()` to skip the slice).
///
/// # Quote reserves
///
/// `quote_reserve` is the **raw** `pool_quote_token_account.amount`, exactly as
/// read from the vault. `virtual_quote_reserves` is the pool's
/// `Pool::virtual_quote_reserves` field, passed separately and **never**
/// pre-added by the caller. All pricing runs against
///
/// ```text
/// effective_quote_reserve = quote_reserve + virtual_quote_reserves
/// ```
///
/// which [`AmmContext::effective_quote_reserve`] computes. Summing the two
/// before constructing the context double-counts the virtual figure and
/// silently misprices every quote.
///
/// The base side is unchanged: `base_reserve` stays the raw
/// `pool_base_token_account.amount`.
///
/// `virtual_quote_reserves` is `i128` to match the on-chain IDL. Pools that
/// predate the field, and every non-boost pool, carry `0`, where
/// `effective == raw` and quotes are byte-identical to the pre-change SDK.
pub struct AmmContext<'a> {
    pub global_config: &'a GlobalConfig,
    pub fee_config: Option<&'a FeeConfig>,
    pub base_mint: &'a Pubkey,
    pub pool_creator: &'a Pubkey,
    pub coin_creator: &'a Pubkey,
    pub base_reserve: u64,
    /// Raw quote-vault balance. Not the effective reserve.
    pub quote_reserve: u64,
    /// `Pool::virtual_quote_reserves`. Added to `quote_reserve` internally.
    pub virtual_quote_reserves: i128,
    pub base_mint_supply: u64,
}

impl AmmContext<'_> {
    /// `quote_reserve + virtual_quote_reserves`, the reserve every quote,
    /// spot price, and market-cap figure must price against.
    ///
    /// Returns [`QuoteError::EmptyReserves`] when the sum is zero or negative:
    /// such a pool cannot price a trade, and clamping to zero or wrapping into
    /// a huge `u64` would surface as a wildly wrong quote instead of an error.
    pub fn effective_quote_reserve(&self) -> QuoteResult<u64> {
        effective_quote_reserve(self.quote_reserve, self.virtual_quote_reserves)
    }

    fn check_reserves(&self) -> QuoteResult<u64> {
        if self.base_reserve == 0 {
            return Err(QuoteError::EmptyReserves);
        }
        self.effective_quote_reserve()
    }
}

/// `quote_reserve + virtual_quote_reserves`, range-checked into `u64`.
///
/// `quote_reserve` is the raw vault balance and `virtual_quote_reserves` is
/// the pool field; passing an already-summed value here double-counts.
///
/// Returns [`QuoteError::EmptyReserves`] if the sum is `<= 0`, and
/// [`QuoteError::MathOverflow`] if it exceeds `u64::MAX` (unreachable for real
/// pools, but never silently truncated).
pub fn effective_quote_reserve(
    quote_reserve: u64,
    virtual_quote_reserves: i128,
) -> QuoteResult<u64> {
    let sum = i128::from(quote_reserve)
        .checked_add(virtual_quote_reserves)
        .ok_or(QuoteError::MathOverflow)?;
    if sum <= 0 {
        return Err(QuoteError::EmptyReserves);
    }
    u64::try_from(sum).map_err(|_| QuoteError::MathOverflow)
}

/// AMM buy: caller specifies SOL input, gets tokens out.
pub fn buy_quote_input(ctx: &AmmContext<'_>, quote_in: u64) -> QuoteResult<BuyQuoteInputResult> {
    let effective_quote_reserve = ctx.check_reserves()?;

    let AmmFeeBps {
        lp_fee_bps,
        protocol_fee_bps,
        creator_fee_bps,
    } = compute_amm_fee_bps(
        ctx.global_config,
        ctx.fee_config,
        ctx.base_mint,
        ctx.pool_creator,
        ctx.base_mint_supply,
        ctx.base_reserve,
        effective_quote_reserve,
    );
    let coin_creator_bps = if *ctx.coin_creator == Pubkey::default() {
        0
    } else {
        creator_fee_bps
    };

    let total_fee_bps = lp_fee_bps + protocol_fee_bps + coin_creator_bps;
    let denom = 10_000u128 + total_fee_bps as u128;

    let effective_quote = (quote_in as u128) * 10_000 / denom;
    let base_out = (ctx.base_reserve as u128) * effective_quote
        / ((effective_quote_reserve as u128) + effective_quote);

    Ok(BuyQuoteInputResult {
        base_amount_out: base_out as u64,
        effective_quote: effective_quote as u64,
    })
}

/// AMM buy: caller specifies desired tokens out, gets total SOL cost.
pub fn buy_base_input(ctx: &AmmContext<'_>, base_out: u64) -> QuoteResult<BuyBaseInputResult> {
    let effective_quote_reserve = ctx.check_reserves()?;
    if base_out >= ctx.base_reserve {
        return Err(QuoteError::BaseOutExceedsReserve);
    }

    let numerator = (effective_quote_reserve as u128) * (base_out as u128);
    let denominator = (ctx.base_reserve as u128) - (base_out as u128);
    let raw_quote = ceil_div(numerator, denominator);

    let AmmFeeBps {
        lp_fee_bps,
        protocol_fee_bps,
        creator_fee_bps,
    } = compute_amm_fee_bps(
        ctx.global_config,
        ctx.fee_config,
        ctx.base_mint,
        ctx.pool_creator,
        ctx.base_mint_supply,
        ctx.base_reserve,
        effective_quote_reserve,
    );

    let lp = fee_amount(raw_quote, lp_fee_bps);
    let protocol = fee_amount(raw_quote, protocol_fee_bps);
    let coin_creator = creator_fee_amount(ctx.coin_creator, raw_quote, creator_fee_bps);
    let total = raw_quote + lp + protocol + coin_creator;

    Ok(BuyBaseInputResult {
        total_quote_in: total as u64,
        raw_quote_in: raw_quote as u64,
    })
}

/// AMM sell: caller specifies tokens in, gets net SOL out.
pub fn sell_base_input(ctx: &AmmContext<'_>, base_in: u64) -> QuoteResult<SellBaseInputResult> {
    let effective_quote_reserve = ctx.check_reserves()?;

    let raw_quote = (effective_quote_reserve as u128) * (base_in as u128)
        / ((ctx.base_reserve as u128) + (base_in as u128));

    let AmmFeeBps {
        lp_fee_bps,
        protocol_fee_bps,
        creator_fee_bps,
    } = compute_amm_fee_bps(
        ctx.global_config,
        ctx.fee_config,
        ctx.base_mint,
        ctx.pool_creator,
        ctx.base_mint_supply,
        ctx.base_reserve,
        effective_quote_reserve,
    );

    let lp = fee_amount(raw_quote, lp_fee_bps);
    let protocol = fee_amount(raw_quote, protocol_fee_bps);
    let coin_creator = creator_fee_amount(ctx.coin_creator, raw_quote, creator_fee_bps);
    let total_fee = lp + protocol + coin_creator;
    if raw_quote < total_fee {
        return Err(QuoteError::FeesExceedOutput);
    }
    let final_quote = raw_quote - total_fee;

    Ok(SellBaseInputResult {
        final_quote_out: final_quote as u64,
        raw_quote_out: raw_quote as u64,
    })
}

/// Constant-product sell quote, fees not applied.
/// `out = amount * effective_quote / (pool_base + amount)`.
///
/// `pool_quote_token_reserves` is the RAW quote-vault balance and
/// `virtual_quote_reserves` is `Pool::virtual_quote_reserves`; the effective
/// reserve is summed internally. Passing a pre-summed value double-counts.
pub fn sell_quote(
    pool_base_token_reserves: u64,
    pool_quote_token_reserves: u64,
    virtual_quote_reserves: i128,
    amount: u64,
) -> QuoteResult<u128> {
    let amount = u128::from(amount);
    let v_quote = u128::from(effective_quote_reserve(
        pool_quote_token_reserves,
        virtual_quote_reserves,
    )?);
    let v_base = u128::from(pool_base_token_reserves);
    let denom = v_base.checked_add(amount).ok_or(QuoteError::MathOverflow)?;
    mul_div_u128(amount, v_quote, denom)
}

/// Pure constant-product buy quote on an AMM pool, no fees applied.
/// `out = sol_amount * pool_base / (effective_quote + sol_amount)`.
///
/// `pool_quote_token_reserves` is the RAW quote-vault balance and
/// `virtual_quote_reserves` is `Pool::virtual_quote_reserves`; the effective
/// reserve is summed internally. Passing a pre-summed value double-counts.
pub fn buy_token_quote_with_sol(
    pool_base_token_reserves: u64,
    pool_quote_token_reserves: u64,
    virtual_quote_reserves: i128,
    sol_amount: u64,
) -> QuoteResult<u128> {
    let sol_amount = u128::from(sol_amount);
    let v_quote = u128::from(effective_quote_reserve(
        pool_quote_token_reserves,
        virtual_quote_reserves,
    )?);
    let v_base = u128::from(pool_base_token_reserves);
    let denom = v_quote
        .checked_add(sol_amount)
        .ok_or(QuoteError::MathOverflow)?;
    mul_div_u128(sol_amount, v_base, denom)
}

/// Inverse of [`sell_quote`]: given a desired SOL output, how many tokens
/// must be sold. `out = sol_amount * pool_base / (effective_quote - sol_amount)`.
///
/// `pool_quote_token_reserves` is the RAW quote-vault balance and
/// `virtual_quote_reserves` is `Pool::virtual_quote_reserves`; the effective
/// reserve is summed internally. Passing a pre-summed value double-counts.
///
/// Returns [`QuoteError::MathOverflow`] if `sol_amount` is at or above the
/// effective quote reserve.
pub fn sell_token_quote_with_sol(
    pool_base_token_reserves: u64,
    pool_quote_token_reserves: u64,
    virtual_quote_reserves: i128,
    sol_amount: u64,
) -> QuoteResult<u128> {
    let sol_amount = u128::from(sol_amount);
    let v_quote = u128::from(effective_quote_reserve(
        pool_quote_token_reserves,
        virtual_quote_reserves,
    )?);
    let v_base = u128::from(pool_base_token_reserves);
    let denom = v_quote
        .checked_sub(sol_amount)
        .ok_or(QuoteError::MathOverflow)?;
    mul_div_u128(sol_amount, v_base, denom)
}

/// Validate that the AMM pool's current market cap is within
/// `target_market_cap ± slippage_bps`. Uses the fixed [`TOKEN_SUPPLY`] for
/// market-cap derivation: `mcap = TOKEN_SUPPLY * effective_quote / pool_base`.
///
/// `pool_quote_token_reserves` is the RAW quote-vault balance and
/// `virtual_quote_reserves` is `Pool::virtual_quote_reserves`; the effective
/// reserve is summed internally. Passing a pre-summed value double-counts and
/// will reject pools that are inside the envelope.
pub fn validate_market_cap(
    pool_base_token_reserves: u64,
    pool_quote_token_reserves: u64,
    virtual_quote_reserves: i128,
    target_market_cap: u128,
    slippage_bps: u16,
) -> QuoteResult<()> {
    let v_quote = u128::from(effective_quote_reserve(
        pool_quote_token_reserves,
        virtual_quote_reserves,
    )?);
    let v_base = u128::from(pool_base_token_reserves);

    let current = mul_div_u128(TOKEN_SUPPLY, v_quote, v_base)?;

    let (min, max) =
        slippage_bounds(target_market_cap, slippage_bps).ok_or(QuoteError::MathOverflow)?;

    if current < min || current > max {
        return Err(QuoteError::SlippageExceeded);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const POOL_QUOTE: u64 = 100_000_000_000;
    const POOL_BASE: u64 = 800_000_000_000_000;
    /// Non-zero virtual figure, as a boost pool carries from
    /// 2026-07-20 onward.
    const VIRTUAL_QUOTE: i128 = 25_000_000_000;
    const EFFECTIVE_QUOTE: u64 = POOL_QUOTE + VIRTUAL_QUOTE as u64;

    #[test]
    fn sell_quote_matches_constant_product() {
        let amount: u64 = 1_000_000_000_000;
        let out = sell_quote(POOL_BASE, POOL_QUOTE, 0, amount).unwrap();
        let expected =
            (amount as u128) * (POOL_QUOTE as u128) / ((POOL_BASE as u128) + amount as u128);
        assert_eq!(out, expected);
    }

    #[test]
    fn buy_and_sell_token_quote_with_sol_use_correct_denominators() {
        let sol_in: u64 = 1_000_000_000;
        let bought = buy_token_quote_with_sol(POOL_BASE, POOL_QUOTE, 0, sol_in).unwrap();
        let expected =
            (sol_in as u128) * (POOL_BASE as u128) / ((POOL_QUOTE as u128) + sol_in as u128);
        assert_eq!(bought, expected);

        let inv = sell_token_quote_with_sol(POOL_BASE, POOL_QUOTE, 0, sol_in).unwrap();
        let expected_inv =
            (sol_in as u128) * (POOL_BASE as u128) / ((POOL_QUOTE as u128) - sol_in as u128);
        assert_eq!(inv, expected_inv);
    }

    #[test]
    fn sell_token_quote_overflow_when_sol_exceeds_reserve() {
        assert_eq!(
            sell_token_quote_with_sol(POOL_BASE, POOL_QUOTE, 0, POOL_QUOTE),
            Err(QuoteError::MathOverflow)
        );
        assert_eq!(
            sell_token_quote_with_sol(POOL_BASE, POOL_QUOTE, 0, POOL_QUOTE + 1),
            Err(QuoteError::MathOverflow)
        );
    }

    #[test]
    fn validate_market_cap_passes_within_envelope() {
        let current = TOKEN_SUPPLY * (POOL_QUOTE as u128) / (POOL_BASE as u128);
        validate_market_cap(POOL_BASE, POOL_QUOTE, 0, current, 0).unwrap();
        validate_market_cap(POOL_BASE, POOL_QUOTE, 0, current * 99 / 100, 200).unwrap();
    }

    #[test]
    fn validate_market_cap_fails_outside_envelope() {
        let current = TOKEN_SUPPLY * (POOL_QUOTE as u128) / (POOL_BASE as u128);
        assert_eq!(
            validate_market_cap(POOL_BASE, POOL_QUOTE, 0, current * 95 / 100, 100),
            Err(QuoteError::SlippageExceeded)
        );
    }

    // ------------------------------------------------------------------
    // virtual_quote_reserves (PumpSwap change, phase 2 from 2026-07-20)
    // ------------------------------------------------------------------

    #[test]
    fn effective_quote_reserve_sums_raw_and_virtual() {
        assert_eq!(
            effective_quote_reserve(POOL_QUOTE, VIRTUAL_QUOTE).unwrap(),
            EFFECTIVE_QUOTE
        );
        // A pool that predates the field, and every non-boost pool.
        assert_eq!(effective_quote_reserve(POOL_QUOTE, 0).unwrap(), POOL_QUOTE);
    }

    /// The field is signed and may legitimately be negative, which makes the
    /// pool SHALLOWER than its raw vault balance suggests. Unsigned
    /// arithmetic here would panic in debug and wrap to near-infinite depth
    /// in release, so a sizing routine would read a shallow pool as bottomless.
    #[test]
    fn negative_virtual_reserves_reduce_effective_depth() {
        let negative: i128 = -40_000_000_000;
        let effective = effective_quote_reserve(POOL_QUOTE, negative).unwrap();
        assert_eq!(effective, POOL_QUOTE - 40_000_000_000);
        assert!(effective < POOL_QUOTE);

        // A sell against the shallower pool must return LESS quote, never a
        // wrapped huge number.
        let amount: u64 = 1_000_000_000_000;
        let with_negative = sell_quote(POOL_BASE, POOL_QUOTE, negative, amount).unwrap();
        let raw_only = sell_quote(POOL_BASE, POOL_QUOTE, 0, amount).unwrap();
        assert!(
            with_negative < raw_only,
            "negative virtual reserves must lower the quote ({with_negative} vs {raw_only})"
        );
        assert_eq!(
            with_negative,
            sell_quote(POOL_BASE, effective, 0, amount).unwrap()
        );
    }

    #[test]
    fn effective_quote_reserve_rejects_non_positive_and_overflow() {
        assert_eq!(
            effective_quote_reserve(POOL_QUOTE, -(POOL_QUOTE as i128)),
            Err(QuoteError::EmptyReserves)
        );
        assert_eq!(
            effective_quote_reserve(POOL_QUOTE, -(POOL_QUOTE as i128) - 1),
            Err(QuoteError::EmptyReserves)
        );
        assert_eq!(
            effective_quote_reserve(u64::MAX, 1),
            Err(QuoteError::MathOverflow)
        );
    }

    /// A raw quote vault of zero is still tradable when the virtual figure
    /// carries the liquidity. Gating on the raw balance would fail a live
    /// pool as empty.
    #[test]
    fn zero_raw_vault_with_virtual_reserves_is_tradable() {
        assert_eq!(
            effective_quote_reserve(0, VIRTUAL_QUOTE).unwrap(),
            VIRTUAL_QUOTE as u64
        );
        let out = sell_quote(POOL_BASE, 0, VIRTUAL_QUOTE, 1_000_000_000_000).unwrap();
        assert!(out > 0, "pool with only virtual quote reserves must price");
    }

    /// The standalone helpers take the RAW vault balance plus the virtual
    /// figure and sum internally. Pre-summing double-counts, and the result
    /// must not silently match.
    #[test]
    fn passing_effective_reserve_as_raw_double_counts() {
        let amount: u64 = 1_000_000_000_000;
        let correct = sell_quote(POOL_BASE, POOL_QUOTE, VIRTUAL_QUOTE, amount).unwrap();
        let double_counted = sell_quote(POOL_BASE, EFFECTIVE_QUOTE, VIRTUAL_QUOTE, amount).unwrap();
        assert_ne!(
            correct, double_counted,
            "pre-summing the virtual figure must change the quote"
        );

        // The correct call equals pricing directly off the effective reserve.
        let direct = sell_quote(POOL_BASE, EFFECTIVE_QUOTE, 0, amount).unwrap();
        assert_eq!(correct, direct);
    }

    /// Omitting the virtual figure (the pre-change call shape) prices off the
    /// raw vault balance and understates a sell. This is the silent
    /// mispricing the appended field introduces.
    #[test]
    fn ignoring_virtual_reserves_underprices_a_sell() {
        let amount: u64 = 1_000_000_000_000;
        let correct = sell_quote(POOL_BASE, POOL_QUOTE, VIRTUAL_QUOTE, amount).unwrap();
        let raw_only = sell_quote(POOL_BASE, POOL_QUOTE, 0, amount).unwrap();
        assert!(
            correct > raw_only,
            "effective reserves must yield more quote out than the raw vault ({correct} vs {raw_only})"
        );
    }

    #[test]
    fn buy_and_market_cap_helpers_use_effective_reserves() {
        let sol_in: u64 = 1_000_000_000;
        let with_virtual = buy_token_quote_with_sol(POOL_BASE, POOL_QUOTE, VIRTUAL_QUOTE, sol_in)
            .unwrap();
        let direct = buy_token_quote_with_sol(POOL_BASE, EFFECTIVE_QUOTE, 0, sol_in).unwrap();
        assert_eq!(with_virtual, direct);
        // Deeper effective reserves mean less slippage, so fewer tokens per SOL.
        let raw_only = buy_token_quote_with_sol(POOL_BASE, POOL_QUOTE, 0, sol_in).unwrap();
        assert!(with_virtual < raw_only);

        // Market cap is measured on effective reserves, so the envelope that
        // the raw balance would satisfy must now be judged against the higher
        // figure.
        let effective_mcap = TOKEN_SUPPLY * (EFFECTIVE_QUOTE as u128) / (POOL_BASE as u128);
        validate_market_cap(POOL_BASE, POOL_QUOTE, VIRTUAL_QUOTE, effective_mcap, 0).unwrap();

        let raw_mcap = TOKEN_SUPPLY * (POOL_QUOTE as u128) / (POOL_BASE as u128);
        assert_eq!(
            validate_market_cap(POOL_BASE, POOL_QUOTE, VIRTUAL_QUOTE, raw_mcap, 100),
            Err(QuoteError::SlippageExceeded)
        );
    }
}
