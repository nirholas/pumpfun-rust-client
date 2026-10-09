//! Bonding-curve buy/sell quotes.

use anchor_lang::solana_program::pubkey::Pubkey;

use crate::math::fees::{
    ceil_div, compute_bonding_curve_fee_bps, exact_in_fee_amounts, exact_in_fees, fee_amount,
    is_sol_like_quote_mint, BondingCurveFeeBps,
};
use crate::math::utils::{mul_div_u128, slippage_bounds, swap_exact_input};
use crate::math::{QuoteError, QuoteResult};
use crate::state::{BondingCurve, FeeConfig, Global};

/// Fixed curve supply: 1B tokens × 10^6 decimals (used by [`validate_market_cap`]).
pub const TOKEN_SUPPLY: u128 = 1_000_000_000_000_000;

/// Tokens received for a quote input on the bonding curve, after fees
/// (`buy_exact_quote_in_v2`): the program's `exact_quote_in_fees`
/// derivation (floor net, per-component ceil fees, shave), then
/// `swap_exact_input(net - 1)`. Caps at `bonding_curve.real_token_reserves`
/// like the program's partial fill (v3: [`buy_v3_token_amount_from_sol_amount`]).
///
/// Returns `Ok(0)` for zero input, an input too small to cover the fees, or
/// a migrated curve (`virtual_token_reserves == 0`).
pub fn buy_token_amount_from_sol_amount(
    global: &Global,
    fee_config: &FeeConfig,
    bonding_curve: &BondingCurve,
    mint_supply: u64,
    sol_amount: u64,
) -> QuoteResult<u64> {
    let tokens = buy_tokens_exact_in(global, fee_config, bonding_curve, mint_supply, sol_amount)?;
    Ok(tokens.min(bonding_curve.real_token_reserves as u128) as u64)
}

/// [`buy_token_amount_from_sol_amount`] before the `real_token_reserves` cap.
fn buy_tokens_exact_in(
    global: &Global,
    fee_config: &FeeConfig,
    bonding_curve: &BondingCurve,
    mint_supply: u64,
    sol_amount: u64,
) -> QuoteResult<u128> {
    if sol_amount == 0 || bonding_curve.virtual_token_reserves == 0 {
        return Ok(0);
    }
    if bonding_curve.virtual_quote_reserves == 0 {
        return Err(QuoteError::EmptyReserves);
    }

    let [protocol_fee_bps, creator_fee_bps] =
        curve_fee_bps(global, fee_config, bonding_curve, mint_supply)?;
    let net = match exact_in_fees(sol_amount, [protocol_fee_bps, creator_fee_bps, 0]) {
        Ok(net) => net,
        Err(QuoteError::ZeroAmount) => return Ok(0),
        Err(e) => return Err(e),
    };
    swap_exact_input(
        u128::from(net) - 1,
        u128::from(bonding_curve.virtual_quote_reserves),
        u128::from(bonding_curve.virtual_token_reserves),
    )
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
    fee_config: &FeeConfig,
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

    let sol_cost = min_amount * (bonding_curve.virtual_quote_reserves as u128)
        / ((bonding_curve.virtual_token_reserves as u128) - min_amount)
        + 1;

    let fee = fee_for_quote(global, fee_config, bonding_curve, mint_supply, sol_cost)?;
    Ok((sol_cost + fee) as u64)
}

/// SOL received for selling a token amount on the bonding curve, after
/// subtracting fees.
///
/// Returns `Ok(0)` for zero input or for a migrated curve.
pub fn sell_sol_amount_from_token_amount(
    global: &Global,
    fee_config: &FeeConfig,
    bonding_curve: &BondingCurve,
    mint_supply: u64,
    token_amount: u64,
) -> QuoteResult<u64> {
    if token_amount == 0 || bonding_curve.virtual_token_reserves == 0 {
        return Ok(0);
    }

    let sol_out = (token_amount as u128) * (bonding_curve.virtual_quote_reserves as u128)
        / ((bonding_curve.virtual_token_reserves as u128) + (token_amount as u128));

    let fee = fee_for_quote(global, fee_config, bonding_curve, mint_supply, sol_out)?;
    if (sol_out as i128) - (fee as i128) < 0 {
        return Err(QuoteError::FeesExceedOutput);
    }
    Ok((sol_out - fee) as u64)
}

/// Quote cost of a `buy_v3` of `token_amount`, fees included. Past the remaining
/// supply a non-mayhem curve completes and buys the rest from the pool the
/// migration will create (the post-completion leg, sized by `base_ata_amount`,
/// the curve's base ATA balance); a mayhem curve partial-fills as v2.
pub fn buy_v3_sol_amount_from_token_amount(
    global: &Global,
    fee_config: &FeeConfig,
    bonding_curve: &BondingCurve,
    mint_supply: u64,
    base_ata_amount: u64,
    token_amount: u64,
) -> QuoteResult<u64> {
    check_curve_open(bonding_curve, false)?;
    let remaining = bonding_curve.real_token_reserves;
    if token_amount <= remaining || bonding_curve.is_mayhem_mode {
        return buy_sol_amount_from_token_amount(
            global,
            fee_config,
            bonding_curve,
            mint_supply,
            token_amount,
        );
    }
    let bps = curve_fee_bps(global, fee_config, bonding_curve, mint_supply)?;
    let sol_cost = buy_quote(bonding_curve, remaining)?;
    let is_legacy = is_sol_like_quote_mint(&bonding_curve.quote_mint);
    let (pool_base, pool_quote) = pool_to_be(
        global,
        bonding_curve,
        base_ata_amount,
        is_legacy,
        remaining,
        sol_cost,
    )?;
    let past = token_amount - remaining;
    if pool_base <= past {
        return Err(QuoteError::BaseOutExceedsReserve);
    }
    let quote_in = u64::try_from(ceil_div(
        u128::from(pool_quote) * u128::from(past),
        u128::from(pool_base - past),
    ))
    .map_err(|_| QuoteError::MathOverflow)?;
    u64::try_from(with_fees(sol_cost, bps) + with_fees(quote_in, bps))
        .map_err(|_| QuoteError::MathOverflow)
}

/// `(tokens, quote charged)` of a `buy_exact_quote_in_v3` spending `sol_amount`.
/// Past the remaining supply a non-mayhem curve completes and the leftover budget
/// buys from the pool-to-be (see [`buy_v3_sol_amount_from_token_amount`]); a mayhem
/// curve partial-fills as v2.
pub fn buy_v3_token_amount_from_sol_amount(
    global: &Global,
    fee_config: &FeeConfig,
    bonding_curve: &BondingCurve,
    mint_supply: u64,
    base_ata_amount: u64,
    sol_amount: u64,
) -> QuoteResult<(u64, u64)> {
    check_curve_open(bonding_curve, false)?;
    let tokens = buy_tokens_exact_in(global, fee_config, bonding_curve, mint_supply, sol_amount)?;
    if tokens == 0 {
        return Ok((0, 0));
    }
    let bps = curve_fee_bps(global, fee_config, bonding_curve, mint_supply)?;
    let remaining = bonding_curve.real_token_reserves;
    if tokens <= u128::from(remaining) {
        let (net, [fee, creator_fee, _]) = exact_in_fee_amounts(sol_amount, [bps[0], bps[1], 0])?;
        return Ok((tokens as u64, net + fee + creator_fee));
    }
    let net = buy_quote(bonding_curve, remaining)?;
    let curve_total = u64::try_from(with_fees(net, bps)).map_err(|_| QuoteError::MathOverflow)?;
    let budget_left = sol_amount
        .checked_sub(curve_total)
        .ok_or(QuoteError::MathOverflow)?;
    if budget_left == 0 || bonding_curve.is_mayhem_mode {
        return Ok((remaining, curve_total));
    }
    let is_legacy = is_sol_like_quote_mint(&bonding_curve.quote_mint);
    let pool = pool_to_be(
        global,
        bonding_curve,
        base_ata_amount,
        is_legacy,
        remaining,
        net,
    )?;
    Ok(
        match post_complete_leg_exact_quote(pool, bps, budget_left, false)? {
            Some((base_out, leg_total)) => (remaining + base_out, curve_total + leg_total),
            None => (remaining, curve_total),
        },
    )
}

/// Output of one curve hop of pump-amm `multi_hop_swap` (pump `multi_hop_curve_swap`),
/// charging only the fee components in `flags` ([`crate::math::amm::HopFees::curve_flags`]).
/// A buy spends the whole input; past the remaining supply it takes the post-completion leg.
/// Mayhem curves are refused (pump `MultiHopMayhemCurveNotSupported`).
#[allow(clippy::too_many_arguments)]
pub fn multi_hop_curve_hop(
    global: &Global,
    fee_config: &FeeConfig,
    bonding_curve: &BondingCurve,
    mint_supply: u64,
    base_ata_amount: u64,
    user: &Pubkey,
    is_buy: bool,
    amount_in: u64,
    flags: u8,
) -> QuoteResult<u64> {
    let charges_creator = flags & HOP_FEE_CREATOR != 0;
    check_curve_open(bonding_curve, !charges_creator)?;
    if bonding_curve.is_mayhem_mode {
        return Err(QuoteError::VenueNotSupported);
    }
    let [protocol, creator] = curve_fee_bps(global, fee_config, bonding_curve, mint_supply)?;
    let bps = [
        if flags & HOP_FEE_PROTOCOL != 0 {
            protocol
        } else {
            0
        },
        if charges_creator { creator } else { 0 },
    ];

    if !is_buy {
        let gross = sell_quote(
            bonding_curve.virtual_quote_reserves,
            bonding_curve.virtual_token_reserves,
            amount_in,
        )?;
        let out = gross
            .checked_sub(fee_amount(gross, bps[0]) + fee_amount(gross, bps[1]))
            .filter(|out| *out > 0)
            .ok_or(QuoteError::ZeroAmount)?;
        return u64::try_from(out).map_err(|_| QuoteError::MathOverflow);
    }

    let (_, [fee, creator_fee, _]) = exact_in_fee_amounts(amount_in, [bps[0], bps[1], 0])?;
    let net = amount_in
        .checked_sub(fee + creator_fee)
        .filter(|net| *net > 0)
        .ok_or(QuoteError::ZeroAmount)?;
    let tokens = swap_exact_input(
        u128::from(net) - 1,
        u128::from(bonding_curve.virtual_quote_reserves),
        u128::from(bonding_curve.virtual_token_reserves),
    )?;
    if tokens == 0 {
        return Err(QuoteError::ZeroAmount);
    }
    let remaining = bonding_curve.real_token_reserves;
    if tokens <= u128::from(remaining) {
        return Ok(tokens as u64);
    }
    if *user == global.whitelist_pda {
        return Err(QuoteError::PostCompleteLegNotAllowed);
    }
    let curve_net = buy_quote(bonding_curve, remaining)?;
    let budget_left = match u128::from(amount_in).checked_sub(with_fees(curve_net, bps)) {
        Some(left) if left > 0 => left as u64,
        _ => return Ok(remaining),
    };
    // The migration fee comes off a SOL curve's raise, as in `buy_v3`.
    let pool = pool_to_be(
        global,
        bonding_curve,
        base_ata_amount,
        is_sol_like_quote_mint(&bonding_curve.quote_mint),
        remaining,
        curve_net,
    )?;
    let leg = post_complete_leg_exact_quote(pool, bps, budget_left, true)?;
    Ok(remaining + leg.map_or(0, |(base_out, _)| base_out))
}

/// pump `multi_hop_curve_swap` `hop_fees` bits.
const HOP_FEE_PROTOCOL: u8 = 1 << 0;
const HOP_FEE_CREATOR: u8 = 1 << 1;

/// pump `check_curve_open_allowing_cashback`: a complete curve, or a cashback
/// curve where a creator fee is charged, is refused.
fn check_curve_open(bonding_curve: &BondingCurve, allow_cashback: bool) -> QuoteResult<()> {
    if bonding_curve.complete || (bonding_curve.is_cashback_coin && !allow_cashback) {
        return Err(QuoteError::VenueNotSupported);
    }
    Ok(())
}

/// `[protocol, creator]` bps of a curve trade; creator 0 on a creator-less curve.
fn curve_fee_bps(
    global: &Global,
    fee_config: &FeeConfig,
    bonding_curve: &BondingCurve,
    mint_supply: u64,
) -> QuoteResult<[u64; 2]> {
    let BondingCurveFeeBps {
        protocol_fee_bps,
        creator_fee_bps,
    } = compute_bonding_curve_fee_bps(
        global,
        fee_config,
        &bonding_curve.quote_mint,
        bonding_curve.creator_fee_bps,
        curve_tier_supply(bonding_curve, mint_supply),
        bonding_curve.virtual_quote_reserves,
        bonding_curve.virtual_token_reserves,
    )?;
    let creator_fee_bps = if bonding_curve.creator != Pubkey::default() {
        creator_fee_bps
    } else {
        0
    };
    Ok([protocol_fee_bps, creator_fee_bps])
}

/// `amount` plus its ceil'd protocol and creator fees.
fn with_fees(amount: u64, [protocol, creator]: [u64; 2]) -> u128 {
    let amount = u128::from(amount);
    amount + fee_amount(amount, protocol) + fee_amount(amount, creator)
}

/// pump `BondingCurve::buy_quote`: `amount * vq / (vt - amount) + 1`.
fn buy_quote(bonding_curve: &BondingCurve, amount: u64) -> QuoteResult<u64> {
    if amount == 0 {
        return Err(QuoteError::ZeroAmount);
    }
    let denom = u128::from(bonding_curve.virtual_token_reserves)
        .checked_sub(amount.into())
        .filter(|d| *d > 0)
        .ok_or(QuoteError::DepletedBondingCurve)?;
    let cost = mul_div_u128(
        amount.into(),
        bonding_curve.virtual_quote_reserves.into(),
        denom,
    )? + 1;
    u64::try_from(cost).map_err(|_| QuoteError::MathOverflow)
}

/// pump `pool_to_be`: the `(base, quote)` reserves `migrate_v2` would deposit once
/// the curve leg (`curve_tokens` for `curve_net_quote`) completes the curve.
fn pool_to_be(
    global: &Global,
    bonding_curve: &BondingCurve,
    base_ata_amount: u64,
    is_legacy: bool,
    curve_tokens: u64,
    curve_net_quote: u64,
) -> QuoteResult<(u64, u64)> {
    let base = base_ata_amount
        .checked_sub(curve_tokens)
        .ok_or(QuoteError::MathOverflow)?;
    let raised = bonding_curve
        .real_quote_reserves
        .checked_add(curve_net_quote)
        .ok_or(QuoteError::MathOverflow)?;
    let quote = if is_legacy {
        raised
            .checked_sub(global.pool_migration_fee)
            .ok_or(QuoteError::MathOverflow)?
    } else {
        raised
    };
    if base == 0 || quote == 0 {
        return Err(QuoteError::EmptyReserves);
    }
    Ok((base, quote))
}

/// pump `post_complete_leg_exact_quote`: `(base_out, quote charged)` that `budget`
/// buys from the pool-to-be, or `None` when it buys nothing. `whole_budget` (a
/// multi-hop hop) puts the shaved rounding remainder into the pool too.
fn post_complete_leg_exact_quote(
    (pool_base, pool_quote): (u64, u64),
    [protocol, creator]: [u64; 2],
    budget: u64,
    whole_budget: bool,
) -> QuoteResult<Option<(u64, u64)>> {
    let total_bps = u128::from(protocol) + u128::from(creator);
    if u128::from(budget) * 10_000 / (10_000 + total_bps) < 2 {
        return Ok(None);
    }
    let (net, [fee, creator_fee, _]) = exact_in_fee_amounts(budget, [protocol, creator, 0])?;
    let quote_in = if whole_budget {
        budget
            .checked_sub(fee + creator_fee)
            .ok_or(QuoteError::MathOverflow)?
    } else {
        net
    };
    let base_out = swap_exact_input(
        u128::from(quote_in - 1),
        u128::from(pool_quote),
        u128::from(pool_base),
    )?;
    if base_out == 0 {
        return Ok(None);
    }
    Ok(Some((base_out as u64, quote_in + fee + creator_fee)))
}

/// Fee in lamports for a quote amount. The tier market cap uses the stored
/// total supply on mayhem coins and the live mint supply otherwise
/// (`BondingCurve::market_cap`).
fn fee_for_quote(
    global: &Global,
    fee_config: &FeeConfig,
    bonding_curve: &BondingCurve,
    mint_supply: u64,
    amount: u128,
) -> QuoteResult<u128> {
    let [protocol_fee_bps, creator_fee_bps] =
        curve_fee_bps(global, fee_config, bonding_curve, mint_supply)?;
    Ok(fee_amount(amount, protocol_fee_bps) + fee_amount(amount, creator_fee_bps))
}

/// Supply the curve's fee-tier market cap uses (`BondingCurve::market_cap`):
/// the stored total supply on mayhem coins, the live mint supply otherwise.
pub fn curve_tier_supply(curve: &BondingCurve, mint_supply: u64) -> u64 {
    if curve.is_mayhem_mode {
        curve.token_total_supply
    } else {
        mint_supply
    }
}

/// Constant-product sell on the bonding curve, fees not applied.
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
    let denom = v_tokens
        .checked_add(amount)
        .ok_or(QuoteError::MathOverflow)?;
    mul_div_u128(amount, v_sol, denom)
}

/// Constant-product buy, fees not applied.
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
    let denom = v_sol
        .checked_add(sol_amount)
        .ok_or(QuoteError::MathOverflow)?;
    mul_div_u128(sol_amount, v_tokens, denom)
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
    let denom = v_sol
        .checked_sub(sol_amount)
        .ok_or(QuoteError::MathOverflow)?;
    mul_div_u128(sol_amount, v_tokens, denom)
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

    let current = mul_div_u128(TOKEN_SUPPLY, v_sol, v_tokens)?;

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

    const V_SOL: u64 = 30_000_000_000;
    const V_TOKENS: u64 = 1_073_000_000_000_000;

    fn fixture_fee_config(protocol: u64, creator: u64) -> FeeConfig {
        use crate::pump::types::{FeeTier, Fees};
        use crate::state::FeeConfigFromIdl;
        let fees = Fees {
            lp_fee_bps: 0,
            protocol_fee_bps: protocol,
            creator_fee_bps: creator,
        };
        FeeConfig::new(FeeConfigFromIdl {
            bump: 0,
            admin: Pubkey::default(),
            flat_fees: fees,
            fee_tiers: vec![FeeTier {
                market_cap_lamports_threshold: 0,
                fees,
            }],
            stable_fee_tiers: vec![],
            exotic_flat_fees: fees,
        })
    }

    // Fresh SOL curve at the mainnet constants, protocol 100 / creator 50 bps.
    fn fresh_sol_curve() -> (Global, FeeConfig, BondingCurve) {
        use crate::state::{BondingCurveFromIdl, GlobalFromIdl};
        let global = Global::new(GlobalFromIdl::default());
        let curve = BondingCurve::new(BondingCurveFromIdl {
            virtual_token_reserves: V_TOKENS,
            virtual_quote_reserves: V_SOL,
            real_token_reserves: 793_100_000_000_000,
            real_quote_reserves: 0,
            token_total_supply: 1_000_000_000_000_000,
            creator: Pubkey::new_unique(),
            quote_mint: crate::constants::NATIVE_MINT,
            ..Default::default()
        });
        (global, fixture_fee_config(100, 50), curve)
    }

    // Partial fill, as the v2 / v3 buys with `partial_fill` set: past the
    // remaining supply both quotes stop at the remainder (793.1e12), which
    // costs buy_quote = 85_005_359_057 plus ceil'd 1% + 0.5% fees.
    #[test]
    fn buys_past_the_curve_fill_the_remainder() {
        let (global, fee_config, curve) = fresh_sol_curve();
        assert_eq!(
            buy_sol_amount_from_token_amount(&global, &fee_config, &curve, 0, 800_000_000_000_000),
            Ok(85_005_359_057 + 850_053_591 + 425_026_796)
        );
        assert_eq!(
            buy_token_amount_from_sol_amount(&global, &fee_config, &curve, 0, 100_000_000_000),
            Ok(793_100_000_000_000)
        );
    }

    const BASE_ATA: u64 = 1_000_000_000_000_000;

    // v3 past the remaining supply: the remainder at the curve price, then the
    // rest from the pool-to-be (B = 1e15 - 793.1e12, Q = the remainder's net).
    #[test]
    fn v3_buys_past_the_curve_take_the_post_completion_leg() {
        let (global, fee_config, curve) = fresh_sol_curve();
        let v3_cost = |c: &BondingCurve, tokens| {
            buy_v3_sol_amount_from_token_amount(&global, &fee_config, c, 0, BASE_ATA, tokens)
        };
        let v3_tokens = |c: &BondingCurve, sol| {
            buy_v3_token_amount_from_sol_amount(&global, &fee_config, c, 0, BASE_ATA, sol)
        };
        assert_eq!(v3_cost(&curve, 800_000_000_000_000), Ok(89_257_114_606));
        assert_eq!(
            v3_tokens(&curve, 100_000_000_000),
            Ok((821_485_770_786_056, 100_000_000_000))
        );
        // Within the remaining supply v3 prices as v2.
        assert_eq!(
            v3_cost(&curve, 1_000_000_000_000),
            buy_sol_amount_from_token_amount(&global, &fee_config, &curve, 0, 1_000_000_000_000)
        );
        let (tokens, charged) = v3_tokens(&curve, 1_000_000_000).unwrap();
        assert_eq!(
            Ok(tokens),
            buy_token_amount_from_sol_amount(&global, &fee_config, &curve, 0, 1_000_000_000)
        );
        assert!(charged <= 1_000_000_000);

        // Mayhem keeps the v2 partial fill.
        let mut mayhem = curve.clone();
        mayhem.is_mayhem_mode = true;
        assert_eq!(
            v3_cost(&mayhem, 800_000_000_000_000),
            Ok(85_005_359_057 + 850_053_591 + 425_026_796)
        );
        assert_eq!(
            v3_tokens(&mayhem, 100_000_000_000),
            Ok((
                793_100_000_000_000,
                85_005_359_057 + 850_053_591 + 425_026_796
            ))
        );

        // Complete and cashback curves are refused.
        let mut complete = curve.clone();
        complete.complete = true;
        assert!(v3_cost(&complete, 1).is_err());
        assert!(v3_tokens(&complete, 1).is_err());
        let mut cashback = curve.clone();
        cashback.is_cashback_coin = true;
        assert!(v3_cost(&cashback, 1).is_err());
    }

    #[test]
    fn multi_hop_curve_hop_matches_the_v3_trades() {
        let (global, fee_config, curve) = fresh_sol_curve();
        let user = Pubkey::new_unique();
        let hop = |c: &BondingCurve, user: &Pubkey, is_buy, amount, flags| {
            multi_hop_curve_hop(
                &global,
                &fee_config,
                c,
                0,
                BASE_ATA,
                user,
                is_buy,
                amount,
                flags,
            )
        };
        // Both fees, no leg: the exact-in v3 buy.
        assert_eq!(
            hop(&curve, &user, true, 1_000_000_000, 3),
            buy_token_amount_from_sol_amount(&global, &fee_config, &curve, 0, 1_000_000_000)
        );
        // Past the remainder the whole leftover budget enters the pool-to-be.
        assert_eq!(
            hop(&curve, &user, true, 100_000_000_000, 3),
            Ok(821_485_770_786_056)
        );
        // A SOL curve's pool-to-be loses the migration fee, as in `buy_v3`.
        let mut fee_global = global.clone();
        fee_global.pool_migration_fee = 1_000_000_000;
        let mut sol_curve = curve.clone();
        sol_curve.quote_mint = Pubkey::default();
        let (v3_tokens, _) = buy_v3_token_amount_from_sol_amount(
            &fee_global,
            &fee_config,
            &sol_curve,
            0,
            BASE_ATA,
            100_000_000_000,
        )
        .unwrap();
        assert_eq!(
            multi_hop_curve_hop(
                &fee_global,
                &fee_config,
                &sol_curve,
                0,
                BASE_ATA,
                &user,
                true,
                100_000_000_000,
                3
            ),
            Ok(v3_tokens)
        );
        // No fees: the bare curve.
        assert_eq!(
            hop(&curve, &user, true, 1_000_000_000, 0),
            Ok(swap_exact_input(999_999_999, V_SOL.into(), V_TOKENS.into()).unwrap() as u64)
        );
        assert_eq!(
            hop(&curve, &user, false, 1_000_000_000_000, 3),
            sell_sol_amount_from_token_amount(&global, &fee_config, &curve, 0, 1_000_000_000_000)
        );

        // Mayhem curves never route; the whitelisted agent can't continue past the curve.
        let mut mayhem = curve.clone();
        mayhem.is_mayhem_mode = true;
        assert_eq!(
            hop(&mayhem, &user, true, 1_000_000_000, 3),
            Err(QuoteError::VenueNotSupported)
        );
        assert_eq!(
            hop(&mayhem, &user, false, 1_000_000_000_000, 3),
            Err(QuoteError::VenueNotSupported)
        );
        assert_eq!(
            hop(&curve, &global.whitelist_pda, true, 100_000_000_000, 3),
            Err(QuoteError::PostCompleteLegNotAllowed)
        );
        assert!(hop(&curve, &global.whitelist_pda, true, 1_000_000_000, 3).is_ok());

        // Cashback curves route only without the creator component; complete never.
        let mut cashback = curve.clone();
        cashback.is_cashback_coin = true;
        assert_eq!(
            hop(&cashback, &user, true, 1_000_000_000, 3),
            Err(QuoteError::VenueNotSupported)
        );
        assert!(hop(&cashback, &user, true, 1_000_000_000, 1).is_ok());
        let mut complete = curve.clone();
        complete.complete = true;
        assert_eq!(
            hop(&complete, &user, false, 1_000_000_000, 0),
            Err(QuoteError::VenueNotSupported)
        );
    }

    // `exact_quote_in_fees` + `quote_buy_exact_sol_in`: the fee is ceil'd on
    // the floor'd net, then `net - 1` is swapped. The old `(sol - 1) * 10_000
    // / (10_000 + bps)` shortcut gave 71_533_328 here.
    #[test]
    fn buy_exact_in_matches_program_derivation() {
        use crate::pump::types::{FeeTier, Fees};
        use crate::state::{BondingCurveFromIdl, FeeConfigFromIdl, GlobalFromIdl};
        let global = Global::new(GlobalFromIdl::default());
        let fees = Fees {
            lp_fee_bps: 0,
            protocol_fee_bps: 5,
            creator_fee_bps: 0,
        };
        let fee_config = FeeConfig::new(FeeConfigFromIdl {
            bump: 0,
            admin: Pubkey::default(),
            flat_fees: fees,
            fee_tiers: vec![FeeTier {
                market_cap_lamports_threshold: 0,
                fees,
            }],
            stable_fee_tiers: vec![],
            exotic_flat_fees: fees,
        });
        let curve = BondingCurve::new(BondingCurveFromIdl {
            virtual_token_reserves: V_TOKENS,
            virtual_quote_reserves: V_SOL,
            real_token_reserves: V_TOKENS,
            ..Default::default()
        });
        assert_eq!(
            buy_token_amount_from_sol_amount(&global, &fee_config, &curve, 0, 2002),
            Ok(71_497_561)
        );
        // Too small to cover fees: the program refuses, the quote says 0 tokens.
        assert_eq!(
            buy_token_amount_from_sol_amount(&global, &fee_config, &curve, 0, 1),
            Ok(0)
        );
    }

    #[test]
    fn sell_quote_matches_constant_product() {
        let amount: u64 = 1_000_000_000_000;
        let out = sell_quote(V_SOL, V_TOKENS, amount).unwrap();
        let expected = (amount as u128) * (V_SOL as u128) / ((V_TOKENS as u128) + amount as u128);
        assert_eq!(out, expected);
    }

    #[test]
    fn buy_token_quote_round_trips_with_sell_token_quote() {
        let sol_in: u64 = 1_000_000_000;
        let bought = buy_token_quote_with_sol(V_SOL, V_TOKENS, sol_in).unwrap();
        let expected = (sol_in as u128) * (V_TOKENS as u128) / ((V_SOL as u128) + sol_in as u128);
        assert_eq!(bought, expected);

        let inv = sell_token_quote_with_sol(V_SOL, V_TOKENS, sol_in).unwrap();
        let expected_inv =
            (sol_in as u128) * (V_TOKENS as u128) / ((V_SOL as u128) - sol_in as u128);
        assert_eq!(inv, expected_inv);
    }

    #[test]
    fn sell_token_quote_overflow_when_sol_exceeds_reserve() {
        assert_eq!(
            sell_token_quote_with_sol(V_SOL, V_TOKENS, V_SOL),
            Err(QuoteError::MathOverflow)
        );
        assert_eq!(
            sell_token_quote_with_sol(V_SOL, V_TOKENS, V_SOL + 1),
            Err(QuoteError::MathOverflow)
        );
    }

    #[test]
    fn validate_market_cap_passes_within_envelope() {
        let current = TOKEN_SUPPLY * (V_SOL as u128) / (V_TOKENS as u128);
        validate_market_cap(V_SOL, V_TOKENS, current, 0).unwrap();
        validate_market_cap(V_SOL, V_TOKENS, current * 99 / 100, 200).unwrap();
    }

    #[test]
    fn validate_market_cap_fails_outside_envelope() {
        let current = TOKEN_SUPPLY * (V_SOL as u128) / (V_TOKENS as u128);
        assert_eq!(
            validate_market_cap(V_SOL, V_TOKENS, current * 95 / 100, 100),
            Err(QuoteError::SlippageExceeded)
        );
    }
}
