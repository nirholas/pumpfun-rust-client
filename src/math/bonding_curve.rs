//! Bonding-curve quoting. Ports `getBuyTokenAmountFromSolAmount`,
//! `getBuySolAmountFromTokenAmount`, and `getSellSolAmountFromTokenAmount`
//! from `@pump-fun/pump-sdk-internal/src/bondingCurve.ts`.

use solana_program::pubkey::Pubkey;

use crate::math::fees::{compute_bonding_curve_fee_bps, fee_amount, BondingCurveFeeBps};
use crate::math::utils::slippage_bounds;
use crate::math::{QuoteError, QuoteResult};
use crate::state::{BondingCurve, FeeConfig, Global};

/// Total bonding-curve token issuance: 1B tokens with 6 decimals (10^15).
/// Mirrors the on-chain pump program's fixed supply and is used by
/// [`validate_market_cap`] (mcap = `TOKEN_SUPPLY * vSol / vTokens`).
pub const TOKEN_SUPPLY: u128 = 1_000_000_000_000_000;

/// Tokens received for a SOL input on the bonding curve, after fees.
/// Caps at `bonding_curve.real_token_reserves` like the on-chain program.
///
/// Returns `Ok(0)` for zero input or for a migrated curve
/// (`virtual_token_reserves == 0`).
pub fn buy_token_amount_from_sol_amount(
    global: &Global,
    fee_config: Option<&FeeConfig>,
    bonding_curve: &BondingCurve,
    mint_supply: u64,
    sol_amount: u64,
) -> QuoteResult<u64> {
    if sol_amount == 0 || bonding_curve.virtual_token_reserves == 0 {
        return Ok(0);
    }
    if bonding_curve.virtual_quote_reserves == 0 {
        return Err(QuoteError::EmptyReserves);
    }

    let BondingCurveFeeBps {
        protocol_fee_bps,
        creator_fee_bps,
    } = compute_bonding_curve_fee_bps(
        global,
        fee_config,
        mint_supply,
        bonding_curve.virtual_quote_reserves,
        bonding_curve.virtual_token_reserves,
    );

    // Creator fees only kick in when the curve has a real creator.
    let total_fee_bps = protocol_fee_bps
        + if bonding_curve.creator != Pubkey::default() {
            creator_fee_bps
        } else {
            0
        };

    // Back the trade amount out of the fee-inclusive input. Matches:
    //   inputAmount = (amount - 1) * 10_000 / (totalFeeBasisPoints + 10_000)
    let input_amount = (sol_amount as u128 - 1) * 10_000 / (total_fee_bps as u128 + 10_000);

    // Constant-product: tokens = inputAmount * vTokens / (vSol + inputAmount)
    let tokens = input_amount * (bonding_curve.virtual_token_reserves as u128)
        / (bonding_curve.virtual_quote_reserves as u128 + input_amount);

    let capped = tokens.min(bonding_curve.real_token_reserves as u128);
    Ok(capped as u64)
}

/// SOL needed to buy a desired token amount on the bonding curve, including
/// fees. Caller-side amount is capped at `real_token_reserves` like the
/// on-chain program.
///
/// Returns `Ok(0)` for zero input or for a migrated curve. Returns
/// `Err(DepletedBondingCurve)` if the capped trade size would empty the
/// curve.
pub fn buy_sol_amount_from_token_amount(
    global: &Global,
    fee_config: Option<&FeeConfig>,
    bonding_curve: &BondingCurve,
    mint_supply: u64,
    token_amount: u64,
) -> QuoteResult<u64> {
    if token_amount == 0 || bonding_curve.virtual_token_reserves == 0 {
        return Ok(0);
    }

    let min_amount = (token_amount as u128).min(bonding_curve.real_token_reserves as u128);
    if min_amount >= bonding_curve.virtual_token_reserves as u128 {
        return Err(QuoteError::DepletedBondingCurve);
    }

    // solCost = minAmount * vSol / (vTokens - minAmount) + 1
    let sol_cost = min_amount * (bonding_curve.virtual_quote_reserves as u128)
        / ((bonding_curve.virtual_token_reserves as u128) - min_amount)
        + 1;

    let fee = fee_for_quote(global, fee_config, bonding_curve, mint_supply, sol_cost);
    Ok((sol_cost + fee) as u64)
}

/// SOL received for selling a token amount on the bonding curve, after
/// subtracting fees.
///
/// Returns `Ok(0)` for zero input or for a migrated curve.
pub fn sell_sol_amount_from_token_amount(
    global: &Global,
    fee_config: Option<&FeeConfig>,
    bonding_curve: &BondingCurve,
    mint_supply: u64,
    token_amount: u64,
) -> QuoteResult<u64> {
    if token_amount == 0 || bonding_curve.virtual_token_reserves == 0 {
        return Ok(0);
    }

    // solOut = inputAmount * vSol / (vTokens + inputAmount)
    let sol_out = (token_amount as u128) * (bonding_curve.virtual_quote_reserves as u128)
        / ((bonding_curve.virtual_token_reserves as u128) + (token_amount as u128));

    let fee = fee_for_quote(global, fee_config, bonding_curve, mint_supply, sol_out);
    // Defensive: a degenerate flat fee table could exceed 100% bps.
    if (sol_out as i128) - (fee as i128) < 0 {
        return Err(QuoteError::FeesExceedOutput);
    }
    Ok((sol_out - fee) as u64)
}

/// Fee in lamports for a given on-curve quote amount. Mirrors `getFee` in TS:
/// outside of mayhem mode the fee tier is computed against the fixed 1B
/// bonding-curve supply, not the real mint supply.
fn fee_for_quote(
    global: &Global,
    fee_config: Option<&FeeConfig>,
    bonding_curve: &BondingCurve,
    mint_supply: u64,
    amount: u128,
) -> u128 {
    let supply_for_tier = if bonding_curve.is_mayhem_mode {
        mint_supply
    } else {
        TOKEN_SUPPLY as u64
    };

    let BondingCurveFeeBps {
        protocol_fee_bps,
        creator_fee_bps,
    } = compute_bonding_curve_fee_bps(
        global,
        fee_config,
        supply_for_tier,
        bonding_curve.virtual_quote_reserves,
        bonding_curve.virtual_token_reserves,
    );

    let protocol = fee_amount(amount, protocol_fee_bps);
    let creator = if bonding_curve.creator != Pubkey::default() {
        fee_amount(amount, creator_fee_bps)
    } else {
        0
    };
    protocol + creator
}

// ---------------------------------------------------------------------------
// Fee-less constant-product primitives.
//
// These mirror `mayhem-program::math::pump_math` exactly: the on-chain pump
// program applies fees as a separate step, so callers (notably the mayhem
// CPI helpers) need the raw curve output without any fee adjustment. The
// fee-aware functions above wrap these same formulas with a fee step layered
// on top.
// ---------------------------------------------------------------------------

/// Pure constant-product sell quote on the bonding curve, no fees applied.
/// `out = amount * vSol / (vTokens + amount)`.
///
/// Returns [`QuoteError::MathOverflow`] on `u128` overflow.
pub fn sell_quote(
    virtual_sol_reserves: u64,
    virtual_token_reserves: u64,
    amount: u64,
) -> QuoteResult<u128> {
    let amount = u128::from(amount);
    let v_sol = u128::from(virtual_sol_reserves);
    let v_tokens = u128::from(virtual_token_reserves);

    amount
        .checked_mul(v_sol)
        .ok_or(QuoteError::MathOverflow)?
        .checked_div(v_tokens.checked_add(amount).ok_or(QuoteError::MathOverflow)?)
        .ok_or(QuoteError::MathOverflow)
}

/// Pure constant-product buy quote on the bonding curve, no fees applied.
/// `out = sol_amount * vTokens / (vSol + sol_amount)`.
///
/// Returns [`QuoteError::MathOverflow`] on `u128` overflow.
pub fn buy_token_quote_with_sol(
    virtual_sol_reserves: u64,
    virtual_token_reserves: u64,
    sol_amount: u64,
) -> QuoteResult<u128> {
    let sol_amount = u128::from(sol_amount);
    let v_sol = u128::from(virtual_sol_reserves);
    let v_tokens = u128::from(virtual_token_reserves);

    sol_amount
        .checked_mul(v_tokens)
        .ok_or(QuoteError::MathOverflow)?
        .checked_div(v_sol.checked_add(sol_amount).ok_or(QuoteError::MathOverflow)?)
        .ok_or(QuoteError::MathOverflow)
}

/// Inverse of [`sell_quote`]: given a desired SOL output, how many tokens
/// must be sold. `out = sol_amount * vTokens / (vSol - sol_amount)`.
///
/// Returns [`QuoteError::MathOverflow`] if `sol_amount >= virtual_sol_reserves`
/// (the denominator would be zero or underflow).
pub fn sell_token_quote_with_sol(
    virtual_sol_reserves: u64,
    virtual_token_reserves: u64,
    sol_amount: u64,
) -> QuoteResult<u128> {
    let sol_amount = u128::from(sol_amount);
    let v_sol = u128::from(virtual_sol_reserves);
    let v_tokens = u128::from(virtual_token_reserves);

    sol_amount
        .checked_mul(v_tokens)
        .ok_or(QuoteError::MathOverflow)?
        .checked_div(v_sol.checked_sub(sol_amount).ok_or(QuoteError::MathOverflow)?)
        .ok_or(QuoteError::MathOverflow)
}

/// Validate that the bonding curve's current market cap is within
/// `target_market_cap ± slippage_bps`. Uses the fixed [`TOKEN_SUPPLY`] for
/// market-cap derivation: `mcap = TOKEN_SUPPLY * vSol / vTokens`.
///
/// Returns [`QuoteError::SlippageExceeded`] if the observed market cap falls
/// outside the envelope, or [`QuoteError::MathOverflow`] on intermediate
/// overflow.
pub fn validate_market_cap(
    virtual_sol_reserves: u64,
    virtual_token_reserves: u64,
    target_market_cap: u128,
    slippage_bps: u16,
) -> QuoteResult<()> {
    let v_sol = u128::from(virtual_sol_reserves);
    let v_tokens = u128::from(virtual_token_reserves);

    let current = TOKEN_SUPPLY
        .checked_mul(v_sol)
        .ok_or(QuoteError::MathOverflow)?
        .checked_div(v_tokens)
        .ok_or(QuoteError::MathOverflow)?;

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

    // Fresh-curve fixture: pump's launch reserves of 30 SOL virtual quote
    // (in lamports) and ~1.073B virtual tokens (in 6-dec base units).
    const V_SOL: u64 = 30_000_000_000;
    const V_TOKENS: u64 = 1_073_000_000_000_000;

    #[test]
    fn sell_quote_matches_constant_product() {
        // Selling 1M whole tokens = 1_000_000 * 1e6 base units.
        let amount: u64 = 1_000_000_000_000;
        let out = sell_quote(V_SOL, V_TOKENS, amount).unwrap();
        // amount * vSol / (vTokens + amount)
        let expected = (amount as u128) * (V_SOL as u128) / ((V_TOKENS as u128) + amount as u128);
        assert_eq!(out, expected);
    }

    #[test]
    fn buy_token_quote_round_trips_with_sell_token_quote() {
        // Spending 1 SOL of input.
        let sol_in: u64 = 1_000_000_000;
        let bought = buy_token_quote_with_sol(V_SOL, V_TOKENS, sol_in).unwrap();
        // amount * vTokens / (vSol + amount)
        let expected =
            (sol_in as u128) * (V_TOKENS as u128) / ((V_SOL as u128) + sol_in as u128);
        assert_eq!(bought, expected);

        // sell_token_quote_with_sol is the structural inverse: same inputs,
        // denominator is (vSol - sol_amount) instead of (vSol + sol_amount).
        let inv = sell_token_quote_with_sol(V_SOL, V_TOKENS, sol_in).unwrap();
        let expected_inv =
            (sol_in as u128) * (V_TOKENS as u128) / ((V_SOL as u128) - sol_in as u128);
        assert_eq!(inv, expected_inv);
    }

    #[test]
    fn sell_token_quote_overflow_when_sol_exceeds_reserve() {
        // sol_amount == vSol → denominator zero → MathOverflow.
        assert_eq!(
            sell_token_quote_with_sol(V_SOL, V_TOKENS, V_SOL),
            Err(QuoteError::MathOverflow)
        );
        // sol_amount > vSol → underflow → MathOverflow.
        assert_eq!(
            sell_token_quote_with_sol(V_SOL, V_TOKENS, V_SOL + 1),
            Err(QuoteError::MathOverflow)
        );
    }

    #[test]
    fn validate_market_cap_passes_within_envelope() {
        // Current mcap with launch fixture: 1e15 * 30e9 / 1.073e15 ≈ 27.96B lamports.
        let current = TOKEN_SUPPLY * (V_SOL as u128) / (V_TOKENS as u128);
        // Hit it exactly with zero slippage.
        validate_market_cap(V_SOL, V_TOKENS, current, 0).unwrap();
        // Off by 1% with 200 bps tolerance still passes.
        validate_market_cap(V_SOL, V_TOKENS, current * 99 / 100, 200).unwrap();
    }

    #[test]
    fn validate_market_cap_fails_outside_envelope() {
        let current = TOKEN_SUPPLY * (V_SOL as u128) / (V_TOKENS as u128);
        // 5% off with 100 bps tolerance must fail.
        assert_eq!(
            validate_market_cap(V_SOL, V_TOKENS, current * 95 / 100, 100),
            Err(QuoteError::SlippageExceeded)
        );
    }
}
