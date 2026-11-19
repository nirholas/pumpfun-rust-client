//! Property tests for the quote math: no input panics, and the round-trip
//! and bound invariants the programs' rounding guarantees hold.

use anchor_lang::solana_program::pubkey::Pubkey;
use proptest::prelude::*;

use crate::math::amm::{self, AmmContext, HopFees};
use crate::math::bonding_curve as bc;
use crate::math::fees::{exact_in_fee_amounts, fee_amount, USDC_MINT};
use crate::math::utils::{add_slippage, sub_slippage};
use crate::math::QuoteError;
use crate::pump::types::{FeeTier, Fees};
use crate::sdk::{AmmQuoteSource, PumpSdk};
use crate::state::pump_amm::{GlobalConfig, GlobalConfigFromIdl, Pool, PoolFromIdl};
use crate::state::{
    BondingCurve, BondingCurveFromIdl, FeeConfig, FeeConfigFromIdl, Global, GlobalFromIdl,
};

fn fee_config(lp: u64, protocol: u64, creator: u64) -> FeeConfig {
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

fn quote_mint() -> impl Strategy<Value = Pubkey> {
    prop_oneof![
        Just(Pubkey::default()),
        Just(crate::constants::NATIVE_MINT),
        Just(USDC_MINT),
        Just(Pubkey::new_from_array([9; 32])),
    ]
}

/// Any `u64`, with the boundary values drawn far more often than uniform
/// sampling would (zero reserves and `u64::MAX` amounts are where overflow and
/// division-by-zero bugs live).
fn edge_u64() -> impl Strategy<Value = u64> {
    prop_oneof![
        1 => Just(0),
        1 => Just(1),
        1 => Just(u64::MAX),
        1 => Just(u64::MAX - 1),
        6 => any::<u64>(),
    ]
}

fn edge_i128() -> impl Strategy<Value = i128> {
    prop_oneof![
        1 => Just(0),
        1 => Just(i128::MIN),
        1 => Just(i128::MAX),
        1 => edge_u64().prop_map(|v| -i128::from(v)),
        6 => any::<i128>(),
    ]
}

prop_compose! {
    /// Any curve the account layout can hold, including degenerate ones.
    fn any_curve()(
        virtual_token_reserves in edge_u64(),
        virtual_quote_reserves in edge_u64(),
        real_token_reserves in edge_u64(),
        real_quote_reserves in edge_u64(),
        token_total_supply in edge_u64(),
        complete in any::<bool>(),
        has_creator in any::<bool>(),
        is_mayhem_mode in any::<bool>(),
        is_cashback_coin in any::<bool>(),
        quote_mint in quote_mint(),
        creator_fee_bps in edge_u64(),
    ) -> BondingCurve {
        BondingCurve::new(BondingCurveFromIdl {
            virtual_token_reserves,
            virtual_quote_reserves,
            real_token_reserves,
            real_quote_reserves,
            token_total_supply,
            complete,
            creator: if has_creator { Pubkey::new_from_array([3; 32]) } else { Pubkey::default() },
            is_mayhem_mode,
            is_cashback_coin,
            quote_mint,
            creator_fee_bps,
            ..Default::default()
        })
    }
}

prop_compose! {
    /// An open SOL curve with launch-like reserves, sellable supply left.
    fn open_curve()(
        virtual_token_reserves in 10_000_000u64..=2_000_000_000_000_000,
        virtual_quote_reserves in 1_000_000u64..=1_000_000_000_000_000,
        sold_ratio in 0u64..=999,
    ) -> BondingCurve {
        BondingCurve::new(BondingCurveFromIdl {
            virtual_token_reserves,
            virtual_quote_reserves,
            real_token_reserves: virtual_token_reserves / 1_000 * (1_000 - sold_ratio) / 2,
            token_total_supply: 1_000_000_000_000_000,
            creator: Pubkey::new_from_array([3; 32]),
            quote_mint: crate::constants::NATIVE_MINT,
            ..Default::default()
        })
    }
}

fn any_global() -> impl Strategy<Value = Global> {
    (edge_u64(), edge_u64(), any::<bool>()).prop_map(
        |(pool_migration_fee, initial_real_token_reserves, creator_fee_configurable)| {
            Global::new(GlobalFromIdl {
                pool_migration_fee,
                initial_real_token_reserves,
                creator_fee_configurable,
                ..Default::default()
            })
        },
    )
}

fn pump_pool(pool: PoolFromIdl) -> Pool {
    let base_mint = Pubkey::new_from_array([1; 32]);
    Pool::new(PoolFromIdl {
        base_mint,
        creator: crate::pda::pump::pool_authority(&base_mint).0,
        ..pool
    })
}

prop_compose! {
    fn any_pool()(
        virtual_quote_reserves in edge_i128(),
        protocol_fees in edge_u64(),
        creator_fees in edge_u64(),
        is_mayhem_mode in any::<bool>(),
        is_cashback_coin in any::<bool>(),
        has_creator in any::<bool>(),
        creator_fee_bps in edge_u64(),
        quote_mint in quote_mint(),
    ) -> Pool {
        pump_pool(PoolFromIdl {
            quote_mint,
            coin_creator: if has_creator { Pubkey::new_from_array([3; 32]) } else { Pubkey::default() },
            virtual_quote_reserves,
            protocol_fees,
            creator_fees,
            is_mayhem_mode,
            is_cashback_coin,
            creator_fee_bps,
            ..Default::default()
        })
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn exact_in_fees_never_overspend(
        amount in any::<u64>(),
        bps in prop::array::uniform3(0u64..=20_000),
    ) {
        match exact_in_fee_amounts(amount, bps) {
            Ok((net, fees)) => {
                prop_assert!(net >= 1);
                let spent = u128::from(net) + fees.iter().map(|f| u128::from(*f)).sum::<u128>();
                prop_assert!(spent <= u128::from(amount));
                // Fees are charged on the net before any overshoot is shaved.
                let total_bps: u128 = bps.iter().map(|b| u128::from(*b)).sum();
                let unshaved = u128::from(amount) * 10_000 / (10_000 + total_bps);
                prop_assert!(u128::from(net) <= unshaved);
                for (fee, b) in fees.iter().zip(bps) {
                    prop_assert_eq!(u128::from(*fee), fee_amount(unshaved, b));
                }
            }
            Err(e) => prop_assert_eq!(e, QuoteError::ZeroAmount),
        }
    }

    #[test]
    fn fee_amount_is_the_ceiling(amount in any::<u64>(), bps in any::<u64>()) {
        let exact = u128::from(amount) * u128::from(bps);
        prop_assert_eq!(fee_amount(amount.into(), bps), exact.div_ceil(10_000));
    }

    #[test]
    fn slippage_brackets_the_value(value in any::<u64>(), bps in any::<u16>()) {
        prop_assert!(sub_slippage(value, bps) <= value);
        prop_assert!(add_slippage(value, bps) >= value);
    }

    // Every public curve quoter returns a result for any decodable state.
    #[test]
    fn curve_quoters_never_panic(
        global in any_global(),
        curve in any_curve(),
        bps in prop::array::uniform3(edge_u64()),
        mint_supply in edge_u64(),
        base_ata_amount in edge_u64(),
        amount in edge_u64(),
        flags in 0u8..4,
        is_buy in any::<bool>(),
    ) {
        let fc = fee_config(bps[0], bps[1], bps[2]);
        let user = Pubkey::new_from_array([2; 32]);
        let _ = bc::buy_token_amount_from_sol_amount(&global, &fc, &curve, mint_supply, amount);
        let _ = bc::buy_sol_amount_from_token_amount(&global, &fc, &curve, mint_supply, amount);
        let _ = bc::sell_sol_amount_from_token_amount(&global, &fc, &curve, mint_supply, amount);
        let _ = bc::buy_v3_sol_amount_from_token_amount(
            &global, &fc, &curve, mint_supply, base_ata_amount, amount,
        );
        let _ = bc::buy_v3_token_amount_from_sol_amount(
            &global, &fc, &curve, mint_supply, base_ata_amount, amount,
        );
        let _ = bc::multi_hop_curve_hop(
            &global, &fc, &curve, mint_supply, base_ata_amount, &user, is_buy, amount, flags,
        );
        let _ = bc::validate_market_cap(
            curve.virtual_quote_reserves, curve.virtual_token_reserves, amount.into(), 100,
        );
        let sdk = PumpSdk::new();
        let _ = sdk.buy_quote_bonding_curve_sol_in(&global, &fc, &curve, mint_supply, amount, 100);
        let _ = sdk.buy_quote_bonding_curve_token_out(&global, &fc, &curve, mint_supply, amount, 100);
        let _ = sdk.sell_quote_bonding_curve(&global, &fc, &curve, mint_supply, amount, 100);
        let source = AmmQuoteSource::BondingCurveComplete {
            global: &global,
            bonding_curve: &curve,
            base_mint: &user,
            base_mint_supply: mint_supply,
        };
        let gc = GlobalConfig::default();
        let _ = sdk.buy_quote_amm_sol_in(&gc, &fc, source, amount, 100);
        let _ = sdk.buy_quote_amm_token_out(&gc, &fc, source, amount, 100);
        let _ = sdk.sell_quote_amm(&gc, &fc, source, amount, 100);
    }

    // Every public pool quoter returns a result for any decodable state.
    #[test]
    fn pool_quoters_never_panic(
        pool in any_pool(),
        bps in prop::array::uniform3(edge_u64()),
        base_reserve in edge_u64(),
        quote_vault_balance in edge_u64(),
        base_mint_supply in edge_u64(),
        amount in edge_u64(),
        is_buy in any::<bool>(),
        legs in (any::<bool>(), any::<bool>(), any::<bool>()),
        disable_flags in any::<u8>(),
    ) {
        let fc = fee_config(bps[0], bps[1], bps[2]);
        let gc = GlobalConfig::new(GlobalConfigFromIdl { disable_flags, ..Default::default() });
        let legs = HopFees { protocol: legs.0, creator: legs.1, lp: legs.2 };
        let _ = amm::multi_hop_pool_hop(
            &gc, &fc, &pool, base_reserve, quote_vault_balance, base_mint_supply, is_buy, amount, legs,
        );
        let sdk = PumpSdk::new();
        let source = AmmQuoteSource::Pool {
            pool: &pool,
            base_reserve,
            quote_reserve: quote_vault_balance,
            base_mint_supply,
        };
        let _ = sdk.buy_quote_amm_sol_in(&gc, &fc, source, amount, 100);
        let _ = sdk.buy_quote_amm_token_out(&gc, &fc, source, amount, 100);
        let _ = sdk.sell_quote_amm(&gc, &fc, source, amount, 100);
        let _ = amm::validate_market_cap(base_reserve, quote_vault_balance, amount.into(), 100);
    }

    // Buying the tokens a budget buys never costs more than the budget: the
    // exact-out inverse rounds up and the exact-in path rounds down.
    #[test]
    fn curve_buy_round_trip_stays_within_budget(
        curve in open_curve(),
        protocol in 0u64..=1_000,
        creator in 0u64..=1_000,
        sol in 1u64..=100_000_000_000_000,
    ) {
        let global = Global::default();
        let fc = fee_config(0, protocol, creator);
        let tokens = bc::buy_token_amount_from_sol_amount(&global, &fc, &curve, 0, sol).unwrap();
        prop_assume!(tokens > 0 && tokens < curve.real_token_reserves);
        let cost = bc::buy_sol_amount_from_token_amount(&global, &fc, &curve, 0, tokens).unwrap();
        prop_assert!(cost <= sol, "{} tokens cost {} > budget {}", tokens, cost, sol);
    }

    // A sell never pays more than the fee-less constant product or the reserve.
    #[test]
    fn curve_sell_is_bounded_by_the_constant_product(
        curve in open_curve(),
        protocol in 0u64..=1_000,
        creator in 0u64..=1_000,
        tokens in 1u64..=1_000_000_000_000_000,
    ) {
        let global = Global::default();
        let fc = fee_config(0, protocol, creator);
        let out = bc::sell_sol_amount_from_token_amount(&global, &fc, &curve, 0, tokens).unwrap();
        let gross = bc::sell_quote(curve.virtual_quote_reserves, curve.virtual_token_reserves, tokens)
            .unwrap();
        prop_assert!(u128::from(out) <= gross);
        prop_assert!(out < curve.virtual_quote_reserves);
    }

    #[test]
    fn amm_buy_round_trip_stays_within_budget(
        base_reserve in 1_000u64..=1_000_000_000_000_000,
        quote_reserve in 1_000u64..=1_000_000_000_000_000,
        bps in prop::array::uniform3(0u64..=1_000),
        quote_in in 1u64..=1_000_000_000_000_000,
    ) {
        let gc = GlobalConfig::default();
        let fc = fee_config(bps[0], bps[1], bps[2]);
        let pool = pump_pool(PoolFromIdl {
            coin_creator: Pubkey::new_from_array([3; 32]),
            quote_mint: crate::constants::NATIVE_MINT,
            ..Default::default()
        });
        let ctx = AmmContext {
            global_config: &gc,
            fee_config: &fc,
            base_mint: &pool.base_mint,
            pool_creator: &pool.creator,
            coin_creator: &pool.coin_creator,
            quote_mint: &pool.quote_mint,
            creator_fee_bps: 0,
            base_reserve,
            quote_reserve,
            base_mint_supply: 0,
            real_quote_reserve: quote_reserve,
        };
        let Ok(bought) = amm::buy_quote_input(&ctx, quote_in) else {
            return Ok(());
        };
        prop_assert!(bought.effective_quote <= quote_in);
        let cost = amm::buy_base_input(&ctx, bought.base_amount_out).unwrap();
        prop_assert!(
            cost.total_quote_in <= quote_in,
            "{} base cost {} > budget {}", bought.base_amount_out, cost.total_quote_in, quote_in
        );
    }
}
