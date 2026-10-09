//! Synthetic-migration v3 buy quoting and signed `Pool.virtual_quote_reserves`
//! pricing, checked against an independent transcription of the off-chain
//! formulas published in pump-public-docs `docs/SYNTHETIC_MIGRATION.md`:
//!
//! ```text
//! pool_base  = curve base ATA balance - remaining
//! pool_quote = real_quote_reserves + curve_quote - pool_migration_fee   (SOL-paired)
//!            = real_quote_reserves + curve_quote                        (token-paired)
//! buy_v3 extra `out`:               quote_in = ceil(pool_quote * out / (pool_base - out))
//! buy_exact_quote_in_v3 net `in`:   out      = floor((in - 1) * pool_base / (pool_quote + in - 1))
//! ```
//!
//! The bonding-curve fee schedule is charged on each part separately.

use anchor_lang::solana_program::pubkey::Pubkey;

use crate::math::amm::{effective_quote_reserve, real_quote_reserve};
use crate::math::bonding_curve::{
    buy_sol_amount_from_token_amount, buy_v3_sol_amount_from_token_amount,
    buy_v3_token_amount_from_sol_amount,
};
use crate::math::QuoteError;
use crate::pump::types::{FeeTier, Fees};
use crate::sdk::{AmmQuoteSource, PumpSdk, Quote};
use crate::state::pump_amm::{GlobalConfig, Pool, PoolFromIdl};
use crate::state::{
    BondingCurve, BondingCurveFromIdl, FeeConfig, FeeConfigFromIdl, Global, GlobalFromIdl,
};

/// Mainnet launch constants: 30 SOL / 1.073B virtual, 793.1M sellable.
const V_QUOTE: u64 = 30_000_000_000;
const V_TOKENS: u64 = 1_073_000_000_000_000;
const REMAINING: u64 = 793_100_000_000_000;
const TOTAL_SUPPLY: u64 = 1_000_000_000_000_000;
/// The curve's base ATA at the crossing buy: the sellable remainder plus the
/// 206.9M reserved for the pool.
const BASE_ATA: u64 = TOTAL_SUPPLY;
const PROTOCOL_BPS: u64 = 100;
const CREATOR_BPS: u64 = 50;
const MIGRATION_FEE: u64 = 15_000_001;

fn fee_config(lp: u64) -> FeeConfig {
    let fees = Fees {
        lp_fee_bps: lp,
        protocol_fee_bps: PROTOCOL_BPS,
        creator_fee_bps: CREATOR_BPS,
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

fn global() -> Global {
    let mut global = Global::new(GlobalFromIdl::default());
    global.pool_migration_fee = MIGRATION_FEE;
    global.initial_real_token_reserves = REMAINING;
    global
}

/// A fresh curve paired with `quote_mint` (wrapped SOL, or a pump coin / exotic token).
fn curve(quote_mint: Pubkey) -> BondingCurve {
    BondingCurve::new(BondingCurveFromIdl {
        virtual_token_reserves: V_TOKENS,
        virtual_quote_reserves: V_QUOTE,
        real_token_reserves: REMAINING,
        real_quote_reserves: 0,
        token_total_supply: TOTAL_SUPPLY,
        creator: Pubkey::new_unique(),
        quote_mint,
        ..Default::default()
    })
}

fn sol_curve() -> BondingCurve {
    curve(crate::constants::NATIVE_MINT)
}

fn token_paired_curve() -> BondingCurve {
    curve(Pubkey::new_unique())
}

fn ceil_fee(amount: u128, bps: u64) -> u128 {
    (amount * u128::from(bps)).div_ceil(10_000)
}

fn with_curve_fees(amount: u128) -> u128 {
    amount + ceil_fee(amount, PROTOCOL_BPS) + ceil_fee(amount, CREATOR_BPS)
}

/// Bonding-curve cost of the whole remainder: `floor(n * vq / (vt - n)) + 1`.
fn ref_curve_quote() -> u128 {
    u128::from(REMAINING) * u128::from(V_QUOTE) / u128::from(V_TOKENS - REMAINING) + 1
}

/// `(pool_base, pool_quote)` the crossing buy prices its pool part on.
fn ref_pool_to_be(base_ata: u64, sol_paired: bool) -> (u128, u128) {
    let migration_fee = if sol_paired { MIGRATION_FEE } else { 0 };
    (
        u128::from(base_ata - REMAINING),
        ref_curve_quote() - u128::from(migration_fee),
    )
}

/// `buy_v3` total cost for `tokens` past the remaining supply.
fn ref_buy_v3_cost(tokens: u64, sol_paired: bool) -> u128 {
    let (pool_base, pool_quote) = ref_pool_to_be(BASE_ATA, sol_paired);
    let out = u128::from(tokens - REMAINING);
    let leg_quote = (pool_quote * out).div_ceil(pool_base - out);
    with_curve_fees(ref_curve_quote()) + with_curve_fees(leg_quote)
}

/// Exact-quote-in fee split: `net = floor(budget * 10_000 / (10_000 + bps))`,
/// each fee ceil'd on that net, and any overshoot shaved off the net.
/// Returns `(net, charged)`.
fn ref_exact_in(budget: u128, bps: &[u64]) -> (u128, u128) {
    let total_bps: u128 = bps.iter().map(|b| u128::from(*b)).sum();
    let net = budget * 10_000 / (10_000 + total_bps);
    let fees: u128 = bps.iter().map(|b| ceil_fee(net, *b)).sum();
    let shaved = net - (net + fees).saturating_sub(budget);
    (shaved, shaved + fees)
}

/// Constant-product exact-in swap of `net - 1`.
fn ref_swap(net: u128, in_reserve: u128, out_reserve: u128) -> u128 {
    (net - 1) * out_reserve / (in_reserve + net - 1)
}

/// `buy_exact_quote_in_v3` past the remaining supply: `(tokens, charged)`.
fn ref_buy_exact_quote_in_v3(budget: u64, base_ata: u64, sol_paired: bool) -> (u128, u128) {
    let curve_total = with_curve_fees(ref_curve_quote());
    let (pool_base, pool_quote) = ref_pool_to_be(base_ata, sol_paired);
    let (net, leg_charged) = ref_exact_in(
        u128::from(budget) - curve_total,
        &[PROTOCOL_BPS, CREATOR_BPS],
    );
    let out = ref_swap(net, pool_quote, pool_base);
    if out == 0 {
        return (u128::from(REMAINING), curve_total);
    }
    (u128::from(REMAINING) + out, curve_total + leg_charged)
}

#[test]
fn v3_token_out_past_the_curve_matches_the_published_formula() {
    let global = global();
    let fees = fee_config(0);
    let past = [
        1,
        1_000_000,
        1_000_000_000_000,
        50_000_000_000_000,
        150_000_000_000_000,
        206_000_000_000_000,
    ];
    for (curve, sol_paired) in [(sol_curve(), true), (token_paired_curve(), false)] {
        for extra in past {
            let tokens = REMAINING + extra;
            assert_eq!(
                buy_v3_sol_amount_from_token_amount(&global, &fees, &curve, 0, BASE_ATA, tokens)
                    .map(u128::from),
                Ok(ref_buy_v3_cost(tokens, sol_paired)),
                "sol_paired={sol_paired} extra={extra}"
            );
        }
    }
}

#[test]
fn only_sol_paired_curves_lose_the_migration_fee_from_the_pool_to_be() {
    let global = global();
    let fees = fee_config(0);
    let tokens = REMAINING + 150_000_000_000_000;
    let sol =
        buy_v3_sol_amount_from_token_amount(&global, &fees, &sol_curve(), 0, BASE_ATA, tokens)
            .unwrap();
    let token_paired = buy_v3_sol_amount_from_token_amount(
        &global,
        &fees,
        &token_paired_curve(),
        0,
        BASE_ATA,
        tokens,
    )
    .unwrap();
    // A thinner quote side prices the pool part cheaper.
    assert!(sol < token_paired);

    // With no migration fee the two pairings price identically.
    let mut no_fee = global.clone();
    no_fee.pool_migration_fee = 0;
    assert_eq!(
        buy_v3_sol_amount_from_token_amount(&no_fee, &fees, &sol_curve(), 0, BASE_ATA, tokens),
        Ok(token_paired)
    );
}

#[test]
fn v3_token_out_cannot_take_the_whole_pool_to_be() {
    let global = global();
    let fees = fee_config(0);
    let curve = sol_curve();
    let pool_base = BASE_ATA - REMAINING;
    assert_eq!(
        buy_v3_sol_amount_from_token_amount(
            &global,
            &fees,
            &curve,
            0,
            BASE_ATA,
            REMAINING + pool_base
        ),
        Err(QuoteError::BaseOutExceedsReserve)
    );
    // One unit short of draining it, the constant-product price is no longer
    // a u64 amount of quote.
    assert_eq!(
        buy_v3_sol_amount_from_token_amount(
            &global,
            &fees,
            &curve,
            0,
            BASE_ATA,
            REMAINING + pool_base - 1
        ),
        Err(QuoteError::MathOverflow)
    );
    // Taking 99% of it is steep but priced, and by the published formula.
    let tokens = REMAINING + pool_base / 100 * 99;
    assert_eq!(
        buy_v3_sol_amount_from_token_amount(&global, &fees, &curve, 0, BASE_ATA, tokens)
            .map(u128::from),
        Ok(ref_buy_v3_cost(tokens, true))
    );
}

#[test]
fn v3_sol_in_spends_the_leftover_budget_in_the_pool_to_be() {
    let global = global();
    let fees = fee_config(0);
    let budgets = [
        100_000_000_000,
        250_000_000_000,
        1_000_000_000_000,
        10_000_000_000_000,
    ];
    for (curve, sol_paired) in [(sol_curve(), true), (token_paired_curve(), false)] {
        for budget in budgets {
            let (tokens, charged) =
                buy_v3_token_amount_from_sol_amount(&global, &fees, &curve, 0, BASE_ATA, budget)
                    .unwrap();
            assert_eq!(
                (u128::from(tokens), u128::from(charged)),
                ref_buy_exact_quote_in_v3(budget, BASE_ATA, sol_paired),
                "sol_paired={sol_paired} budget={budget}"
            );
            assert!(tokens > REMAINING);
            assert!(charged <= budget);
            // The token-out quoter never asks more for those tokens than the
            // exact-in buy charged for them.
            let cost =
                buy_v3_sol_amount_from_token_amount(&global, &fees, &curve, 0, BASE_ATA, tokens)
                    .unwrap();
            assert!(cost <= charged, "cost {cost} > charged {charged}");
        }
    }
}

#[test]
fn v3_sol_in_stops_at_the_completed_curve_when_the_leftover_buys_nothing() {
    let global = global();
    let fees = fee_config(0);
    let curve = sol_curve();
    // A single base unit left for the pool: no affordable leftover buys it.
    let base_ata = REMAINING + 1;
    let budget = 100_000_000_000;
    let curve_total = with_curve_fees(ref_curve_quote());
    assert_eq!(
        ref_buy_exact_quote_in_v3(budget, base_ata, true),
        (u128::from(REMAINING), curve_total)
    );
    assert_eq!(
        buy_v3_token_amount_from_sol_amount(&global, &fees, &curve, 0, base_ata, budget),
        Ok((REMAINING, curve_total as u64))
    );
}

#[test]
fn sdk_v3_quotes_put_slippage_on_the_caller_side() {
    let sdk = PumpSdk::new();
    let global = global();
    let fees = fee_config(0);
    let curve = sol_curve();
    let slippage_bps = 250;

    let tokens = REMAINING + 100_000_000_000_000;
    let cost = ref_buy_v3_cost(tokens, true) as u64;
    assert_eq!(
        sdk.buy_quote_bonding_curve_v3_token_out(
            &global,
            &fees,
            &curve,
            0,
            BASE_ATA,
            tokens,
            slippage_bps
        ),
        Ok(Quote {
            amount: cost,
            min_out: tokens,
            input_amount_used: tokens,
            max_input: (u128::from(cost) * 10_250 / 10_000) as u64,
        })
    );

    let budget = 120_000_000_000;
    let (out, charged) = ref_buy_exact_quote_in_v3(budget, BASE_ATA, true);
    assert_eq!(
        sdk.buy_quote_bonding_curve_v3_sol_in(
            &global,
            &fees,
            &curve,
            0,
            BASE_ATA,
            budget,
            slippage_bps
        ),
        Ok(Quote {
            amount: out as u64,
            min_out: (out * 9_750 / 10_000) as u64,
            input_amount_used: charged as u64,
            max_input: budget,
        })
    );
}

#[test]
fn sdk_v3_quotes_partial_fill_mayhem_curves() {
    let sdk = PumpSdk::new();
    let global = global();
    let fees = fee_config(0);
    let mut mayhem = sol_curve();
    mayhem.is_mayhem_mode = true;
    let remainder_cost =
        buy_sol_amount_from_token_amount(&global, &fees, &mayhem, 0, REMAINING).unwrap();

    let quote = sdk
        .buy_quote_bonding_curve_v3_token_out(
            &global,
            &fees,
            &mayhem,
            0,
            BASE_ATA,
            REMAINING + 100_000_000_000_000,
            0,
        )
        .unwrap();
    assert_eq!(quote.amount, remainder_cost);
    assert_eq!(quote.min_out, REMAINING);

    let quote = sdk
        .buy_quote_bonding_curve_v3_sol_in(&global, &fees, &mayhem, 0, BASE_ATA, 500_000_000_000, 0)
        .unwrap();
    assert_eq!(quote.amount, REMAINING);
    assert_eq!(quote.input_amount_used, remainder_cost);
}

/// After a synthetic migration `migrate_v2` opens the pool on the reserves the
/// crossing buy left behind, so the first PumpSwap quote (estimated from the
/// completed curve's `post_complete_*` fields) prices on exactly those.
#[test]
fn completed_curve_estimate_opens_where_the_synthetic_leg_stopped() {
    let sdk = PumpSdk::new();
    let global = global();
    let fees = fee_config(0);
    let budget: u64 = 150_000_000_000;
    let (pool_base, pool_quote) = ref_pool_to_be(BASE_ATA, true);
    let curve_quote = ref_curve_quote();
    let (leg_net, _) = ref_exact_in(
        u128::from(budget) - with_curve_fees(curve_quote),
        &[PROTOCOL_BPS, CREATOR_BPS],
    );
    let leg_out = ref_swap(leg_net, pool_quote, pool_base);

    let mut completed = sol_curve();
    completed.complete = true;
    completed.real_token_reserves = 0;
    completed.real_quote_reserves = curve_quote as u64;
    completed.post_complete_base_out = leg_out as u64;
    completed.post_complete_quote_in = leg_net as u64;
    let base_mint = Pubkey::new_unique();

    let opened_base = pool_base - leg_out;
    let opened_quote = pool_quote + leg_net;
    let sol_in = 1_000_000_000u64;
    let (net, _) = ref_exact_in(u128::from(sol_in), &[0, PROTOCOL_BPS, CREATOR_BPS]);
    let quote = sdk
        .buy_quote_amm_sol_in(
            &GlobalConfig::default(),
            &fees,
            AmmQuoteSource::BondingCurveComplete {
                global: &global,
                bonding_curve: &completed,
                base_mint: &base_mint,
                base_mint_supply: TOTAL_SUPPLY,
            },
            sol_in,
            0,
        )
        .unwrap();
    assert_eq!(
        u128::from(quote.amount),
        ref_swap(net, opened_quote, opened_base)
    );
}

const VAULT: u64 = 80_000_000_000;
const POOL_BASE: u64 = 200_000_000_000_000;

fn pump_pool(virtual_quote_reserves: i128, protocol_fees: u64, creator_fees: u64) -> Pool {
    let base_mint = Pubkey::new_unique();
    Pool::new(PoolFromIdl {
        base_mint,
        creator: crate::pda::pump::pool_authority(&base_mint).0,
        coin_creator: Pubkey::new_unique(),
        quote_mint: crate::constants::NATIVE_MINT,
        protocol_fees,
        creator_fees,
        virtual_quote_reserves,
        ..Default::default()
    })
}

#[test]
fn signed_virtual_quote_reserves_shift_the_pricing_reserve_both_ways() {
    // Un-swept fee buckets with no boost: the field is minus the buckets, so
    // the pool prices on its liquidity alone.
    let buckets = pump_pool(-10_000, 7_000, 3_000);
    assert_eq!(effective_quote_reserve(&buckets, VAULT), Ok(VAULT - 10_000));
    assert_eq!(
        effective_quote_reserve(&buckets, VAULT),
        real_quote_reserve(&buckets, VAULT)
    );
    // A boost larger than the buckets lifts the pricing reserve above the vault.
    let boosted = pump_pool(5_000_000_000 - 10_000, 7_000, 3_000);
    assert_eq!(
        effective_quote_reserve(&boosted, VAULT),
        Ok(VAULT + 5_000_000_000 - 10_000)
    );
    // Negative past the vault, or past u64, is not a price.
    let underwater = pump_pool(-(i128::from(VAULT) + 1), 0, 0);
    assert_eq!(
        effective_quote_reserve(&underwater, VAULT),
        Err(QuoteError::MathOverflow)
    );
    let huge = pump_pool(i128::from(u64::MAX), 0, 0);
    assert_eq!(
        effective_quote_reserve(&huge, 1),
        Err(QuoteError::MathOverflow)
    );
}

#[test]
fn amm_buy_prices_on_the_signed_effective_reserve() {
    let sdk = PumpSdk::new();
    let gc = GlobalConfig::default();
    let fees = fee_config(20);
    let sol_in = 2_000_000_000u64;
    let (net, _) = ref_exact_in(u128::from(sol_in), &[20, PROTOCOL_BPS, CREATOR_BPS]);
    let quote_out = |pool: &Pool| {
        sdk.buy_quote_amm_sol_in(
            &gc,
            &fees,
            AmmQuoteSource::Pool {
                pool,
                base_reserve: POOL_BASE,
                quote_reserve: VAULT,
                base_mint_supply: TOTAL_SUPPLY,
            },
            sol_in,
            0,
        )
        .unwrap()
        .amount
    };

    let flat = pump_pool(0, 0, 0);
    let buckets = pump_pool(-10_000_000, 6_000_000, 4_000_000);
    let flat_out = quote_out(&flat);
    let buckets_out = quote_out(&buckets);
    assert_eq!(
        u128::from(flat_out),
        ref_swap(net, VAULT.into(), POOL_BASE.into())
    );
    assert_eq!(
        u128::from(buckets_out),
        ref_swap(net, u128::from(VAULT - 10_000_000), POOL_BASE.into())
    );
    // Treating the field as unsigned (or ignoring it) would overprice the buy.
    assert!(buckets_out > flat_out);
}

#[test]
fn pump_idl_carries_the_new_error_codes() {
    let idl = include_str!("../../idls/pump.json");
    let entry = |code: u32, name: &str| {
        let at = idl
            .find(&format!("\"name\": \"{name}\""))
            .unwrap_or_else(|| panic!("{name} missing from idls/pump.json"));
        let before = &idl[at.saturating_sub(48)..at];
        assert!(
            before.contains(&format!("\"code\": {code},")),
            "{name} is not error {code}"
        );
        idl[at..].lines().nth(1).unwrap_or_default().to_owned()
    };
    assert!(entry(6108, "MultiHopMayhemCurveNotSupported").contains("mayhem-mode curves"));
    let msg = entry(6104, "QuoteReservesOutOfRange");
    assert!(msg.contains("Derived quote reserves are zero"));
    // The client's QuoteError reads the same as the program's message.
    assert!(msg
        .to_ascii_lowercase()
        .contains(&QuoteError::QuoteReservesOutOfRange.to_string()));
}
