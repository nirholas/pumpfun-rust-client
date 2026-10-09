//! AMM (pump-swap) quote helpers.

use anchor_lang::solana_program::pubkey::Pubkey;

use crate::math::bonding_curve::TOKEN_SUPPLY;
use crate::math::fees::{
    ceil_div, compute_amm_fee_bps, creator_fee_amount, exact_in_fee_amounts, exact_in_fees,
    fee_amount, is_pump_pool, AmmFeeBps,
};
use crate::math::utils::{mul_div_u128, slippage_bounds, swap_exact_input};
use crate::math::{QuoteError, QuoteResult};
use crate::state::pump_amm::{GlobalConfig, Pool};
use crate::state::FeeConfig;

/// Quote reserves the pool prices against: vault balance +
/// `pool.virtual_quote_reserves`. That field is the boost sigma minus the
/// un-swept fee buckets, so this stays the pricing reserve after v2 trades
/// (mirrors `Pool::effective_quote_reserves`).
pub fn effective_quote_reserve(pool: &Pool, quote_vault_balance: u64) -> QuoteResult<u64> {
    i128::from(quote_vault_balance)
        .checked_add(pool.virtual_quote_reserves)
        .and_then(|v| u64::try_from(v).ok())
        .ok_or(QuoteError::MathOverflow)
}

/// pump-amm `GlobalConfig.disable_flags`: bit 3 turns buys off, bit 4 sells
/// (`DisableFlag::Buy` / `Sell`); the program fails `DisabledBuy` /
/// `DisabledSell` before pricing.
pub fn check_trading_enabled(global_config: &GlobalConfig, is_buy: bool) -> QuoteResult<()> {
    let bit = if is_buy { 3 } else { 4 };
    if global_config.disable_flags & (1 << bit) != 0 {
        return Err(QuoteError::TradingDisabled);
    }
    Ok(())
}

/// Sum of the pool's un-swept fee buckets (`Pool::fee_buckets_total`).
pub fn fee_buckets_total(pool: &Pool) -> QuoteResult<u64> {
    pool.protocol_fees
        .checked_add(pool.creator_fees)
        .ok_or(QuoteError::MathOverflow)
}

/// Quote vault balance that is liquidity: balance minus the fee buckets
/// (mirrors `Pool::real_quote_reserves`). Sells are paid out of this.
pub fn real_quote_reserve(pool: &Pool, quote_vault_balance: u64) -> QuoteResult<u64> {
    quote_vault_balance
        .checked_sub(fee_buckets_total(pool)?)
        .ok_or(QuoteError::MathOverflow)
}

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
pub struct AmmContext<'a> {
    pub global_config: &'a GlobalConfig,
    pub fee_config: &'a FeeConfig,
    pub base_mint: &'a Pubkey,
    pub pool_creator: &'a Pubkey,
    pub coin_creator: &'a Pubkey,
    /// Pool's quote mint; selects the fee schedule (SOL tiers, USDC tiers, exotic flat).
    pub quote_mint: &'a Pubkey,
    /// Pool's configured creator fee rate; 0 = the pump-fees schedule rate.
    pub creator_fee_bps: u64,
    pub base_reserve: u64,
    /// Quote reserves the curve prices against: vault balance + the pool's
    /// `virtual_quote_reserves` (boost). Equals `real_quote_reserve` when the
    /// pool has no boost.
    pub quote_reserve: u64,
    pub base_mint_supply: u64,
    /// Actual quote vault balance. Boosted pools can quote more than they can
    /// pay out; pump-amm rejects such sells with `InsufficientRealQuoteReserves`.
    pub real_quote_reserve: u64,
}

impl AmmContext<'_> {
    fn check_reserves(&self) -> QuoteResult<()> {
        if self.base_reserve == 0 || self.quote_reserve == 0 {
            return Err(QuoteError::EmptyReserves);
        }
        Ok(())
    }
}

/// AMM buy: caller specifies quote input, gets tokens out
/// (`buy_exact_quote_in` / `_v2`): the program's exact-in derivation, then
/// `swap_exact_input(net - 1)`. Dust that leaves nothing to swap or buys
/// nothing is [`QuoteError::ZeroAmount`], as the program rejects it.
pub fn buy_quote_input(ctx: &AmmContext<'_>, quote_in: u64) -> QuoteResult<BuyQuoteInputResult> {
    check_trading_enabled(ctx.global_config, true)?;
    ctx.check_reserves()?;

    let AmmFeeBps {
        lp_fee_bps,
        protocol_fee_bps,
        creator_fee_bps,
    } = compute_amm_fee_bps(
        ctx.global_config,
        ctx.fee_config,
        ctx.base_mint,
        ctx.pool_creator,
        ctx.quote_mint,
        ctx.creator_fee_bps,
        ctx.base_mint_supply,
        ctx.base_reserve,
        ctx.quote_reserve,
    )?;
    let coin_creator_bps = if *ctx.coin_creator == Pubkey::default() {
        0
    } else {
        creator_fee_bps
    };

    let net = exact_in_fees(quote_in, [lp_fee_bps, protocol_fee_bps, coin_creator_bps])?;
    let base_out = swap_exact_input(
        u128::from(net) - 1,
        u128::from(ctx.quote_reserve),
        u128::from(ctx.base_reserve),
    )?;
    if base_out == 0 {
        return Err(QuoteError::ZeroAmount);
    }

    Ok(BuyQuoteInputResult {
        base_amount_out: base_out as u64,
        effective_quote: net,
    })
}

/// AMM buy: caller specifies desired tokens out, gets total SOL cost.
pub fn buy_base_input(ctx: &AmmContext<'_>, base_out: u64) -> QuoteResult<BuyBaseInputResult> {
    check_trading_enabled(ctx.global_config, true)?;
    ctx.check_reserves()?;
    if base_out >= ctx.base_reserve {
        return Err(QuoteError::BaseOutExceedsReserve);
    }

    let numerator = (ctx.quote_reserve as u128) * (base_out as u128);
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
        ctx.quote_mint,
        ctx.creator_fee_bps,
        ctx.base_mint_supply,
        ctx.base_reserve,
        ctx.quote_reserve,
    )?;

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
    check_trading_enabled(ctx.global_config, false)?;
    ctx.check_reserves()?;

    let raw_quote = (ctx.quote_reserve as u128) * (base_in as u128)
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
        ctx.quote_mint,
        ctx.creator_fee_bps,
        ctx.base_mint_supply,
        ctx.base_reserve,
        ctx.quote_reserve,
    )?;

    let lp = fee_amount(raw_quote, lp_fee_bps);
    let protocol = fee_amount(raw_quote, protocol_fee_bps);
    let coin_creator = creator_fee_amount(ctx.coin_creator, raw_quote, creator_fee_bps);
    let total_fee = lp + protocol + coin_creator;
    if raw_quote < total_fee {
        return Err(QuoteError::FeesExceedOutput);
    }
    let final_quote = raw_quote - total_fee;

    // pump-amm pays the seller out of the real vault: it requires
    // `real_quote_reserve >= raw_quote - lp_fee` (lp fee stays in the pool).
    if (ctx.real_quote_reserve as u128) < raw_quote - lp {
        return Err(QuoteError::InsufficientRealQuoteReserves);
    }

    Ok(SellBaseInputResult {
        final_quote_out: final_quote as u64,
        raw_quote_out: raw_quote as u64,
    })
}

/// Fee components one `multi_hop_swap` hop charges (pump-amm `HopFees`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HopFees {
    pub protocol: bool,
    pub creator: bool,
    pub lp: bool,
}

impl HopFees {
    /// The leg trading the user's currency pays the protocol fee, the leg trading
    /// the far coin the creator and LP fees: first / last hop on a buy route, last
    /// / first on a sell route; a single hop pays all.
    pub fn for_hop(route_is_buy: bool, index: usize, hops: usize) -> Self {
        let (first, last) = (index == 0, index + 1 == hops);
        let (protocol, target) = if route_is_buy {
            (first, last)
        } else {
            (last, first)
        };
        Self {
            protocol,
            creator: target,
            lp: target,
        }
    }

    /// pump `multi_hop_curve_swap` flag byte: bit 0 protocol, bit 1 creator.
    pub fn curve_flags(self) -> u8 {
        u8::from(self.protocol) | (u8::from(self.creator) << 1)
    }
}

/// Output of one pool hop of `multi_hop_swap`, priced on the vaults before the
/// hop and charging only the components in `legs`. A buy spends the whole input.
/// Non-pump and mayhem pools, and a cashback pool on a creator leg, are
/// [`QuoteError::VenueNotSupported`].
#[allow(clippy::too_many_arguments)]
pub fn multi_hop_pool_hop(
    global_config: &GlobalConfig,
    fee_config: &FeeConfig,
    pool: &Pool,
    base_reserve: u64,
    quote_vault_balance: u64,
    base_mint_supply: u64,
    is_buy: bool,
    amount_in: u64,
    legs: HopFees,
) -> QuoteResult<u64> {
    check_trading_enabled(global_config, is_buy)?;
    if !is_pump_pool(&pool.base_mint, &pool.creator)
        || (legs.creator && pool.is_cashback_coin)
        || pool.is_mayhem_mode
    {
        return Err(QuoteError::VenueNotSupported);
    }
    let real_quote = real_quote_reserve(pool, quote_vault_balance)?;
    let quote_reserve = effective_quote_reserve(pool, quote_vault_balance)?;
    if base_reserve == 0 || quote_reserve == 0 {
        return Err(QuoteError::EmptyReserves);
    }
    let AmmFeeBps {
        lp_fee_bps,
        protocol_fee_bps,
        creator_fee_bps,
    } = compute_amm_fee_bps(
        global_config,
        fee_config,
        &pool.base_mint,
        &pool.creator,
        &pool.quote_mint,
        pool.creator_fee_bps,
        base_mint_supply,
        base_reserve,
        quote_reserve,
    )?;
    let has_creator = pool.coin_creator != Pubkey::default();
    let bps = [
        if legs.lp { lp_fee_bps } else { 0 },
        if legs.protocol { protocol_fee_bps } else { 0 },
        if legs.creator && has_creator {
            creator_fee_bps
        } else {
            0
        },
    ];

    if is_buy {
        let (_, fees) = exact_in_fee_amounts(amount_in, bps)?;
        let net = amount_in
            .checked_sub(fees.iter().sum())
            .filter(|net| *net > 0)
            .ok_or(QuoteError::ZeroAmount)?;
        let out = swap_exact_input(
            u128::from(net) - 1,
            u128::from(quote_reserve),
            u128::from(base_reserve),
        )?;
        if out == 0 {
            return Err(QuoteError::ZeroAmount);
        }
        return Ok(out as u64);
    }

    let gross = sell_quote(base_reserve, quote_reserve, amount_in)?;
    let [lp, protocol, creator] = bps.map(|b| fee_amount(gross, b));
    let without_lp = gross.checked_sub(lp).ok_or(QuoteError::FeesExceedOutput)?;
    if u128::from(real_quote) < without_lp {
        return Err(QuoteError::InsufficientRealQuoteReserves);
    }
    let out = without_lp
        .checked_sub(protocol + creator)
        .ok_or(QuoteError::FeesExceedOutput)?;
    if out == 0 {
        return Err(QuoteError::ZeroAmount);
    }
    Ok(out as u64)
}

/// Constant-product sell quote, fees not applied.
/// `out = amount * pool_quote / (pool_base + amount)`.
pub fn sell_quote(
    pool_base_token_reserves: u64,
    pool_quote_token_reserves: u64,
    amount: u64,
) -> QuoteResult<u128> {
    let amount = u128::from(amount);
    let v_quote = u128::from(pool_quote_token_reserves);
    let v_base = u128::from(pool_base_token_reserves);
    let denom = v_base.checked_add(amount).ok_or(QuoteError::MathOverflow)?;
    mul_div_u128(amount, v_quote, denom)
}

/// Pure constant-product buy quote on an AMM pool, no fees applied.
/// `out = sol_amount * pool_base / (pool_quote + sol_amount)`.
pub fn buy_token_quote_with_sol(
    pool_base_token_reserves: u64,
    pool_quote_token_reserves: u64,
    sol_amount: u64,
) -> QuoteResult<u128> {
    let sol_amount = u128::from(sol_amount);
    let v_quote = u128::from(pool_quote_token_reserves);
    let v_base = u128::from(pool_base_token_reserves);
    let denom = v_quote
        .checked_add(sol_amount)
        .ok_or(QuoteError::MathOverflow)?;
    mul_div_u128(sol_amount, v_base, denom)
}

/// Inverse of [`sell_quote`]: given a desired SOL output, how many tokens
/// must be sold. `out = sol_amount * pool_base / (pool_quote - sol_amount)`.
///
/// Returns [`QuoteError::MathOverflow`] if `sol_amount >= pool_quote_token_reserves`.
pub fn sell_token_quote_with_sol(
    pool_base_token_reserves: u64,
    pool_quote_token_reserves: u64,
    sol_amount: u64,
) -> QuoteResult<u128> {
    let sol_amount = u128::from(sol_amount);
    let v_quote = u128::from(pool_quote_token_reserves);
    let v_base = u128::from(pool_base_token_reserves);
    let denom = v_quote
        .checked_sub(sol_amount)
        .ok_or(QuoteError::MathOverflow)?;
    mul_div_u128(sol_amount, v_base, denom)
}

/// Validate that the AMM pool's current market cap is within
/// `target_market_cap ± slippage_bps`. Uses the fixed [`TOKEN_SUPPLY`] for
/// market-cap derivation: `mcap = TOKEN_SUPPLY * pool_quote / pool_base`.
pub fn validate_market_cap(
    pool_base_token_reserves: u64,
    pool_quote_token_reserves: u64,
    target_market_cap: u128,
    slippage_bps: u16,
) -> QuoteResult<()> {
    let v_quote = u128::from(pool_quote_token_reserves);
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

    #[test]
    fn hop_fees_follow_the_route_ends() {
        let f = |protocol, creator| HopFees {
            protocol,
            creator,
            lp: creator,
        };
        for is_buy in [true, false] {
            assert_eq!(HopFees::for_hop(is_buy, 0, 1), f(true, true));
        }
        let buy: Vec<_> = (0..3).map(|i| HopFees::for_hop(true, i, 3)).collect();
        assert_eq!(buy, [f(true, false), f(false, false), f(false, true)]);
        let sell: Vec<_> = (0..3).map(|i| HopFees::for_hop(false, i, 3)).collect();
        assert_eq!(sell, [f(false, true), f(false, false), f(true, false)]);
        assert_eq!(
            [
                f(true, false),
                f(false, false),
                f(false, true),
                f(true, true)
            ]
            .map(HopFees::curve_flags),
            [1, 0, 2, 3]
        );
    }

    fn fee_config(lp: u64, protocol: u64, creator: u64) -> FeeConfig {
        use crate::pump::types::{FeeTier, Fees};
        use crate::state::FeeConfigFromIdl;
        let fees = Fees {
            lp_fee_bps: lp,
            protocol_fee_bps: protocol,
            creator_fee_bps: creator,
        };
        let tiers = vec![FeeTier {
            market_cap_lamports_threshold: 0,
            fees,
        }];
        FeeConfig::new(FeeConfigFromIdl {
            bump: 0,
            admin: Pubkey::default(),
            flat_fees: fees,
            fee_tiers: tiers.clone(),
            stable_fee_tiers: tiers,
            exotic_flat_fees: fees,
        })
    }

    // A pump pool on SOL with a 10-unit fee bucket in the quote vault.
    fn pump_pool() -> Pool {
        use crate::state::pump_amm::PoolFromIdl;
        let base_mint = Pubkey::new_unique();
        Pool::new(PoolFromIdl {
            base_mint,
            creator: crate::pda::pump::pool_authority(&base_mint).0,
            coin_creator: Pubkey::new_unique(),
            quote_mint: crate::constants::NATIVE_MINT,
            protocol_fees: 10,
            ..Default::default()
        })
    }

    #[test]
    fn multi_hop_pool_hop_matches_the_v2_trades() {
        let gc = GlobalConfig::default();
        let fc = fee_config(20, 5, 5);
        let pool = pump_pool();
        let all = HopFees::for_hop(true, 0, 1);
        let none = HopFees::for_hop(true, 1, 3);
        let hop = |pool: &Pool, is_buy, amount, legs| {
            multi_hop_pool_hop(
                &gc, &fc, pool, POOL_BASE, POOL_QUOTE, 0, is_buy, amount, legs,
            )
        };
        let ctx = AmmContext {
            global_config: &gc,
            fee_config: &fc,
            base_mint: &pool.base_mint,
            pool_creator: &pool.creator,
            coin_creator: &pool.coin_creator,
            quote_mint: &pool.quote_mint,
            creator_fee_bps: 0,
            base_reserve: POOL_BASE,
            quote_reserve: POOL_QUOTE,
            base_mint_supply: 0,
            real_quote_reserve: POOL_QUOTE - 10,
        };

        // A free middle hop is the bare pool.
        assert_eq!(
            hop(&pool, true, 1_000_000_000, none),
            Ok(swap_exact_input(999_999_999, POOL_QUOTE.into(), POOL_BASE.into()).unwrap() as u64)
        );
        // A single hop sells as sell_v2; it buys with the whole budget, so at least buy_v2.
        assert_eq!(
            hop(&pool, false, 1_000_000_000_000, all),
            Ok(sell_base_input(&ctx, 1_000_000_000_000)
                .unwrap()
                .final_quote_out)
        );
        let bought = hop(&pool, true, 1_000_000_003, all).unwrap();
        assert!(
            bought
                >= buy_quote_input(&ctx, 1_000_000_003)
                    .unwrap()
                    .base_amount_out
        );

        // Sells are paid from liquidity, never from the buckets.
        let mut drained = pump_pool();
        drained.protocol_fees = POOL_QUOTE;
        assert_eq!(
            hop(&drained, false, 1_000_000_000_000, none),
            Err(QuoteError::InsufficientRealQuoteReserves)
        );

        let mut third_party = pump_pool();
        third_party.creator = Pubkey::new_unique();
        let mut mayhem = pump_pool();
        mayhem.is_mayhem_mode = true;
        let mut cashback = pump_pool();
        cashback.is_cashback_coin = true;
        for (pool, legs) in [(&third_party, none), (&mayhem, none), (&cashback, all)] {
            assert_eq!(
                hop(pool, true, 1_000_000_000, legs),
                Err(QuoteError::VenueNotSupported)
            );
        }
        assert!(hop(&cashback, true, 1_000_000_000, HopFees::for_hop(true, 0, 2)).is_ok());
    }

    #[test]
    fn sell_quote_matches_constant_product() {
        let amount: u64 = 1_000_000_000_000;
        let out = sell_quote(POOL_BASE, POOL_QUOTE, amount).unwrap();
        let expected =
            (amount as u128) * (POOL_QUOTE as u128) / ((POOL_BASE as u128) + amount as u128);
        assert_eq!(out, expected);
    }

    #[test]
    fn buy_and_sell_token_quote_with_sol_use_correct_denominators() {
        let sol_in: u64 = 1_000_000_000;
        let bought = buy_token_quote_with_sol(POOL_BASE, POOL_QUOTE, sol_in).unwrap();
        let expected =
            (sol_in as u128) * (POOL_BASE as u128) / ((POOL_QUOTE as u128) + sol_in as u128);
        assert_eq!(bought, expected);

        let inv = sell_token_quote_with_sol(POOL_BASE, POOL_QUOTE, sol_in).unwrap();
        let expected_inv =
            (sol_in as u128) * (POOL_BASE as u128) / ((POOL_QUOTE as u128) - sol_in as u128);
        assert_eq!(inv, expected_inv);
    }

    #[test]
    fn sell_token_quote_overflow_when_sol_exceeds_reserve() {
        assert_eq!(
            sell_token_quote_with_sol(POOL_BASE, POOL_QUOTE, POOL_QUOTE),
            Err(QuoteError::MathOverflow)
        );
        assert_eq!(
            sell_token_quote_with_sol(POOL_BASE, POOL_QUOTE, POOL_QUOTE + 1),
            Err(QuoteError::MathOverflow)
        );
    }

    #[test]
    fn validate_market_cap_passes_within_envelope() {
        let current = TOKEN_SUPPLY * (POOL_QUOTE as u128) / (POOL_BASE as u128);
        validate_market_cap(POOL_BASE, POOL_QUOTE, current, 0).unwrap();
        validate_market_cap(POOL_BASE, POOL_QUOTE, current * 99 / 100, 200).unwrap();
    }

    #[test]
    fn validate_market_cap_fails_outside_envelope() {
        let current = TOKEN_SUPPLY * (POOL_QUOTE as u128) / (POOL_BASE as u128);
        assert_eq!(
            validate_market_cap(POOL_BASE, POOL_QUOTE, current * 95 / 100, 100),
            Err(QuoteError::SlippageExceeded)
        );
    }
}
