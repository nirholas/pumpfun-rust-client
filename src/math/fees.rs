//! Fee math for bonding-curve and AMM quotes (basis points as `u64`).

use anchor_lang::solana_program::pubkey::Pubkey;

use crate::math::{QuoteError, QuoteResult};
use crate::pda;
use crate::pump::types::{FeeTier, Fees};
use crate::state::pump_amm::GlobalConfig;
use crate::state::{FeeConfig, Global};

/// USDC: the quote mint routed to `stable_fee_tiers`.
pub const USDC_MINT: Pubkey = anchor_lang::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

/// SOL-like quotes: the zero key legacy curves store, legacy WSOL, Token-2022 native.
pub fn is_sol_like_quote_mint(quote_mint: &Pubkey) -> bool {
    *quote_mint == Pubkey::default()
        || *quote_mint == anchor_spl::token::spl_token::native_mint::ID
        || *quote_mint == anchor_spl::token_2022::spl_token_2022::native_mint::ID
}

/// `ceil(a / b)` for non-zero `b`.
#[inline]
pub fn ceil_div(a: u128, b: u128) -> u128 {
    a.div_ceil(b)
}

/// `ceil(amount * basis_points / 10_000)`.
#[inline]
pub fn fee_amount(amount: u128, basis_points: u64) -> u128 {
    ceil_div(amount * basis_points as u128, 10_000)
}

/// `fee_amount(amount, basis_points)`, or `0` when `creator` is `Pubkey::default()`
/// (the convention for "no creator fee on this trade").
#[inline]
pub fn creator_fee_amount(creator: &Pubkey, amount: u128, basis_points: u64) -> u128 {
    if *creator == Pubkey::default() {
        0
    } else {
        fee_amount(amount, basis_points)
    }
}

/// Net quote of an exact-quote-in buy (`exact_quote_in_fees`, identical in
/// pump's v2 / v3 buys and pump-amm's `buy_exact_quote_in` / `_v2`):
/// `net = floor(in * 10_000 / (10_000 + Σbps))`, each fee is
/// `ceil(net * bps / 10_000)`, and when `net + Σfees` overshoots the input
/// the excess is shaved off `net`. [`QuoteError::ZeroAmount`] when nothing is
/// left to swap, as the programs refuse it.
pub(crate) fn exact_in_fees(amount_in: u64, bps: [u64; 3]) -> QuoteResult<u64> {
    exact_in_fee_amounts(amount_in, bps).map(|(net, _)| net)
}

/// [`exact_in_fees`] plus each fee, charged on the unshaved net as the programs do.
pub(crate) fn exact_in_fee_amounts(amount_in: u64, bps: [u64; 3]) -> QuoteResult<(u64, [u64; 3])> {
    let total_bps: u128 = bps.iter().map(|b| *b as u128).sum();
    let input = amount_in as u128;
    let mut net = input * 10_000 / (10_000 + total_bps);
    if net == 0 {
        return Err(QuoteError::ZeroAmount);
    }
    let fees = bps.map(|b| fee_amount(net, b));
    let cost = net + fees.iter().sum::<u128>();
    if cost > input {
        net = net
            .checked_sub(cost - input)
            .filter(|n| *n > 0)
            .ok_or(QuoteError::ZeroAmount)?;
    }
    Ok((net as u64, fees.map(|f| f as u64)))
}

/// Bonding-curve market cap in lamports.
/// `marketCap = virtualQuoteReserves * mintSupply / virtualTokenReserves`.
#[inline]
pub fn bonding_curve_market_cap(
    mint_supply: u64,
    virtual_quote_reserves: u64,
    virtual_token_reserves: u64,
) -> u128 {
    debug_assert!(virtual_token_reserves != 0);
    (virtual_quote_reserves as u128) * (mint_supply as u128) / (virtual_token_reserves as u128)
}

/// AMM pool market cap in lamports.
/// `marketCap = quoteReserve * baseMintSupply / baseReserve`.
#[inline]
pub fn pool_market_cap(base_mint_supply: u64, base_reserve: u64, quote_reserve: u64) -> u128 {
    debug_assert!(base_reserve != 0);
    (quote_reserve as u128) * (base_mint_supply as u128) / (base_reserve as u128)
}

/// `true` iff `pool_creator` matches the canonical pump-program-derived pool
/// authority for `base_mint`. Used to decide whether a pool gets tiered fees
/// (pump pools) or flat fees (third-party pools).
pub fn is_pump_pool(base_mint: &Pubkey, pool_creator: &Pubkey) -> bool {
    &pda::pump::pool_authority(base_mint).0 == pool_creator
}

/// Bonding-curve fees split into protocol and creator components. The
/// AMM-style LP fee is not part of the bonding-curve fee model.
#[derive(Clone, Copy, Debug)]
pub struct BondingCurveFeeBps {
    pub protocol_fee_bps: u64,
    pub creator_fee_bps: u64,
}

/// AMM fees split into LP, protocol, and coin-creator components.
#[derive(Clone, Copy, Debug)]
pub struct AmmFeeBps {
    pub lp_fee_bps: u64,
    pub protocol_fee_bps: u64,
    pub creator_fee_bps: u64,
}

/// Highest tier with threshold `<= market_cap`, else first tier; `None` on
/// an empty table.
fn calculate_fee_tier(tiers: &[FeeTier], market_cap: u128) -> Option<&Fees> {
    let first = tiers.first()?;
    if market_cap < first.market_cap_lamports_threshold {
        return Some(&first.fees);
    }
    Some(
        tiers
            .iter()
            .rev()
            .find(|tier| market_cap >= tier.market_cap_lamports_threshold)
            .map_or(&first.fees, |tier| &tier.fees),
    )
}

/// On-chain rule (pump `util/fee.rs`, pump-amm `curve/fees.rs`): a configured
/// per-coin creator rate wins over the schedule while the global gate is on.
fn configured_creator_fee_bps(gate_on: bool, stored_bps: u64, schedule_bps: u64) -> u64 {
    if gate_on && stored_bps != 0 {
        stored_bps
    } else {
        schedule_bps
    }
}

/// Fee schedule for a trade: non-pump pools pay `flat_fees`; pump pools pay
/// `fee_tiers` for SOL-like quotes, `stable_fee_tiers` for USDC, and
/// `exotic_flat_fees` for any other quote mint, falling back to `flat_fees`
/// while that schedule is unset (all-zero). An empty tier table is
/// [`QuoteError::EmptyFeeTiers`] (the program's `FeeTiersEmpty`).
pub fn fees_for_quote_mint<'a>(
    cfg: &'a FeeConfig,
    is_pump_pool: bool,
    market_cap: u128,
    quote_mint: &Pubkey,
) -> QuoteResult<&'a Fees> {
    if !is_pump_pool {
        return Ok(&cfg.flat_fees);
    }
    if is_sol_like_quote_mint(quote_mint) {
        return calculate_fee_tier(&cfg.fee_tiers, market_cap).ok_or(QuoteError::EmptyFeeTiers);
    }
    if *quote_mint == USDC_MINT {
        return calculate_fee_tier(&cfg.stable_fee_tiers, market_cap)
            .ok_or(QuoteError::EmptyFeeTiers);
    }
    let exotic = &cfg.exotic_flat_fees;
    if exotic.lp_fee_bps == 0 && exotic.protocol_fee_bps == 0 && exotic.creator_fee_bps == 0 {
        Ok(&cfg.flat_fees)
    } else {
        Ok(exotic)
    }
}

/// Resolve the bps for a bonding-curve trade the way every pump trade path
/// does: select the pump-fees schedule by `quote_mint` (see
/// [`fees_for_quote_mint`]) indexed by current market cap, then replace the
/// schedule's creator rate with the coin's own `coin_creator_fee_bps` while
/// `Global.creator_fee_configurable` is on and the stored rate is nonzero
/// (0 = "not configured, use the schedule"). `Global`'s own bps fields play
/// no part in on-chain pricing.
pub fn compute_bonding_curve_fee_bps(
    global: &Global,
    fee_config: &FeeConfig,
    quote_mint: &Pubkey,
    coin_creator_fee_bps: u64,
    mint_supply: u64,
    virtual_quote_reserves: u64,
    virtual_token_reserves: u64,
) -> QuoteResult<BondingCurveFeeBps> {
    let market_cap =
        bonding_curve_market_cap(mint_supply, virtual_quote_reserves, virtual_token_reserves);
    let fees = fees_for_quote_mint(fee_config, true, market_cap, quote_mint)?;
    Ok(BondingCurveFeeBps {
        protocol_fee_bps: fees.protocol_fee_bps,
        creator_fee_bps: configured_creator_fee_bps(
            global.creator_fee_configurable,
            coin_creator_fee_bps,
            fees.creator_fee_bps,
        ),
    })
}

/// AMM trade fee bps: the pump-fees schedule selected by pool origin and
/// `quote_mint` (see [`fees_for_quote_mint`]). The pool's own
/// `coin_creator_fee_bps` replaces the schedule's creator rate while
/// `GlobalConfig.creator_fee_configurable` is on and the stored rate is nonzero.
/// `GlobalConfig`'s own bps fields play no part in on-chain pricing.
pub fn compute_amm_fee_bps(
    global_config: &GlobalConfig,
    fee_config: &FeeConfig,
    base_mint: &Pubkey,
    pool_creator: &Pubkey,
    quote_mint: &Pubkey,
    coin_creator_fee_bps: u64,
    base_mint_supply: u64,
    base_reserve: u64,
    quote_reserve: u64,
) -> QuoteResult<AmmFeeBps> {
    let market_cap = pool_market_cap(base_mint_supply, base_reserve, quote_reserve);
    let fees = fees_for_quote_mint(
        fee_config,
        is_pump_pool(base_mint, pool_creator),
        market_cap,
        quote_mint,
    )?;
    Ok(AmmFeeBps {
        lp_fee_bps: fees.lp_fee_bps,
        protocol_fee_bps: fees.protocol_fee_bps,
        creator_fee_bps: configured_creator_fee_bps(
            global_config.creator_fee_configurable,
            coin_creator_fee_bps,
            fees.creator_fee_bps,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::FeeConfigFromIdl;

    fn tier(threshold: u128, lp: u64, protocol: u64, creator: u64) -> FeeTier {
        FeeTier {
            market_cap_lamports_threshold: threshold,
            fees: fees(lp, protocol, creator),
        }
    }

    fn fees(lp: u64, protocol: u64, creator: u64) -> Fees {
        Fees {
            lp_fee_bps: lp,
            protocol_fee_bps: protocol,
            creator_fee_bps: creator,
        }
    }

    // `Fees` (IDL-generated) has no `PartialEq`; compare by field.
    fn bps(f: &Fees) -> (u64, u64, u64) {
        (f.lp_fee_bps, f.protocol_fee_bps, f.creator_fee_bps)
    }

    fn config(exotic: Fees) -> FeeConfig {
        FeeConfig::new(FeeConfigFromIdl {
            bump: 0,
            admin: Pubkey::default(),
            flat_fees: fees(25, 5, 0),
            fee_tiers: vec![tier(0, 1, 90, 30), tier(1_000, 2, 80, 20)],
            stable_fee_tiers: vec![tier(0, 3, 70, 10), tier(1_000, 4, 60, 5)],
            exotic_flat_fees: exotic,
        })
    }

    #[test]
    fn selection_matrix() {
        let cfg = config(fees(5, 300, 25));
        let sel =
            |pump: bool, mc: u128, q: &Pubkey| bps(fees_for_quote_mint(&cfg, pump, mc, q).unwrap());
        let other = Pubkey::new_unique();
        assert_eq!(sel(false, 1_000, &other), (25, 5, 0));
        for sol in [
            Pubkey::default(),
            anchor_spl::token::spl_token::native_mint::ID,
            anchor_spl::token_2022::spl_token_2022::native_mint::ID,
        ] {
            assert_eq!(sel(true, 0, &sol), (1, 90, 30));
            assert_eq!(sel(true, 1_000, &sol), (2, 80, 20));
        }
        assert_eq!(sel(true, 999, &USDC_MINT), (3, 70, 10));
        assert_eq!(sel(true, 1_000, &USDC_MINT), (4, 60, 5));
        assert_eq!(sel(true, u128::MAX, &other), (5, 300, 25));

        // Unset (all-zero) exotic fees fall back to flat fees.
        let unset = config(fees(0, 0, 0));
        assert_eq!(
            bps(fees_for_quote_mint(&unset, true, 0, &other).unwrap()),
            (25, 5, 0)
        );

        // An empty tier table is an error, never a panic (program: FeeTiersEmpty).
        let mut no_stable = config(fees(0, 0, 0));
        no_stable.stable_fee_tiers.clear();
        assert_eq!(
            fees_for_quote_mint(&no_stable, true, 0, &USDC_MINT).err(),
            Some(QuoteError::EmptyFeeTiers)
        );
        assert!(fees_for_quote_mint(&no_stable, true, 0, &other).is_ok());
    }

    // A coin's own creator rate wins over the schedule only while the gate is
    // on and the stored rate is nonzero; protocol/LP never move.
    #[test]
    fn creator_fee_override() {
        use crate::state::pump_amm::GlobalConfigFromIdl;
        use crate::state::GlobalFromIdl;

        let cfg = config(fees(5, 300, 25));
        let sol = Pubkey::default();
        let global = |gate: bool| {
            Global::new(GlobalFromIdl {
                fee_basis_points: 100,
                creator_fee_basis_points: 50,
                creator_fee_configurable: gate,
                ..Default::default()
            })
        };
        let bc = |gate: bool, stored: u64| {
            let f =
                compute_bonding_curve_fee_bps(&global(gate), &cfg, &sol, stored, 1, 1, 1).unwrap();
            (f.protocol_fee_bps, f.creator_fee_bps)
        };
        // Schedule at market cap 0 is tier (1, 90, 30).
        assert_eq!(bc(true, 250), (90, 250));
        assert_eq!(bc(true, 0), (90, 30));
        assert_eq!(bc(false, 250), (90, 30));

        let global_config = |gate: bool| {
            GlobalConfig::new(GlobalConfigFromIdl {
                lp_fee_basis_points: 20,
                protocol_fee_basis_points: 5,
                coin_creator_fee_basis_points: 5,
                creator_fee_configurable: gate,
                ..Default::default()
            })
        };
        let base_mint = Pubkey::new_unique();
        let pool_creator = pda::pump::pool_authority(&base_mint).0;
        let amm = |gate: bool, stored: u64| {
            let f = compute_amm_fee_bps(
                &global_config(gate),
                &cfg,
                &base_mint,
                &pool_creator,
                &sol,
                stored,
                1,
                1,
                1,
            )
            .unwrap();
            (f.lp_fee_bps, f.protocol_fee_bps, f.creator_fee_bps)
        };
        assert_eq!(amm(true, 250), (1, 90, 250));
        assert_eq!(amm(true, 0), (1, 90, 30));
        assert_eq!(amm(false, 250), (1, 90, 30));
    }
}

#[cfg(test)]
mod pre_extension_layout_tests {
    use super::*;
    use crate::accounts::decode_fee_config;
    use crate::state::FeeConfigFromIdl;
    use anchor_lang::AccountSerialize;

    // Pre-extension FeeConfig (4073 bytes, `exotic_flat_fees` absent) must still decode,
    // reading the missing field as zero and thus routing other quotes to `flat_fees`.
    #[test]
    fn decodes_4073_byte_account_without_exotic_field() {
        let tiers: Vec<FeeTier> = (0..25)
            .map(|i| FeeTier {
                market_cap_lamports_threshold: i as u128 * 1_000,
                fees: Fees {
                    lp_fee_bps: 20,
                    protocol_fee_bps: 100 - i,
                    creator_fee_bps: 30,
                },
            })
            .collect();
        let cfg = FeeConfigFromIdl {
            bump: 250,
            admin: Pubkey::new_unique(),
            flat_fees: Fees {
                lp_fee_bps: 25,
                protocol_fee_bps: 5,
                creator_fee_bps: 0,
            },
            fee_tiers: tiers.clone(),
            stable_fee_tiers: tiers,
            exotic_flat_fees: Fees {
                lp_fee_bps: 0,
                protocol_fee_bps: 0,
                creator_fee_bps: 0,
            },
        };
        let mut data = Vec::new();
        cfg.try_serialize(&mut data).unwrap();
        data.truncate(data.len() - 24); // drop exotic_flat_fees, as on chain today
        data.resize(4073, 0);
        let decoded = decode_fee_config(&data).unwrap();
        assert_eq!(decoded.fee_tiers.len(), 25);
        assert_eq!(decoded.stable_fee_tiers.len(), 25);
        assert_eq!(decoded.exotic_flat_fees.protocol_fee_bps, 0);
        let other = Pubkey::new_unique();
        let f = fees_for_quote_mint(&decoded, true, 0, &other).unwrap();
        assert_eq!((f.lp_fee_bps, f.protocol_fee_bps), (25, 5));
    }

    // Serialize `T` (with discriminator), drop the trailing `drop` bytes, and
    // zero-pad to `len` — the on-chain account at a historical length.
    fn at_len<T: AccountSerialize>(v: &T, drop: usize, len: usize) -> Vec<u8> {
        let mut data = Vec::new();
        v.try_serialize(&mut data).unwrap();
        data.truncate(data.len() - drop);
        data.resize(len, 0);
        data
    }

    // Pre-creator-fee accounts (115-byte curve, 261-byte pool, 1045-byte
    // Global, 940-byte GlobalConfig) decode with the new fields unset; the
    // extended sizes (151 / 301) keep the stored values.
    #[test]
    fn decodes_pre_creator_fee_account_lengths() {
        use crate::accounts::pump_amm::{decode_global_config, decode_pool};
        use crate::accounts::{decode_bonding_curve, decode_global};
        use crate::state::pump_amm::{GlobalConfigFromIdl, PoolFromIdl};
        use crate::state::{BondingCurveFromIdl, GlobalFromIdl};

        let quote_mint = Pubkey::new_unique();
        let curve = BondingCurveFromIdl {
            virtual_quote_reserves: 30_000_000_000,
            quote_mint,
            creator_fee_bps: 250,
            can_edit_creator_fee: true,
            ..Default::default()
        };
        let old = decode_bonding_curve(&at_len(&curve, 9, 115)).unwrap();
        assert_eq!(old.quote_mint, quote_mint);
        assert_eq!(old.creator_fee_bps, 0);
        assert!(!old.can_edit_creator_fee);
        let extended = decode_bonding_curve(&at_len(&curve, 0, 151)).unwrap();
        assert_eq!(extended.creator_fee_bps, 250);
        assert!(extended.can_edit_creator_fee);

        let pool = PoolFromIdl {
            virtual_quote_reserves: -42,
            is_cashback_coin: true,
            creator_fee_bps: 250,
            can_edit_creator_fee: true,
            ..Default::default()
        };
        let old = decode_pool(&at_len(&pool, 9, 261)).unwrap();
        assert_eq!(old.virtual_quote_reserves, -42);
        assert!(old.is_cashback_coin);
        assert_eq!(old.creator_fee_bps, 0);
        assert!(!old.can_edit_creator_fee);
        let extended = decode_pool(&at_len(&pool, 0, 301)).unwrap();
        assert_eq!(extended.creator_fee_bps, 250);

        let global = GlobalFromIdl {
            initial_virtual_quote_reserves: 7,
            creator_fee_configurable: true,
            max_configurable_creator_fee_bps: 5_000,
            ..Default::default()
        };
        let old = decode_global(&at_len(&global, 9, 1045)).unwrap();
        assert_eq!(old.initial_virtual_quote_reserves, 7);
        assert!(!old.creator_fee_configurable);
        assert_eq!(old.max_configurable_creator_fee_bps, 0);
        assert!(
            decode_global(&at_len(&global, 0, 1054))
                .unwrap()
                .creator_fee_configurable
        );

        let global_config = GlobalConfigFromIdl {
            boost_enabled: true,
            creator_fee_configurable: true,
            max_configurable_creator_fee_bps: 5_000,
            ..Default::default()
        };
        let old = decode_global_config(&at_len(&global_config, 9, 940)).unwrap();
        assert!(old.boost_enabled);
        assert!(!old.creator_fee_configurable);
        assert_eq!(old.max_configurable_creator_fee_bps, 0);
        assert!(
            decode_global_config(&at_len(&global_config, 0, 949))
                .unwrap()
                .creator_fee_configurable
        );
    }

    // Fee-bucket rungs: a 125-byte curve and a 271-byte pool (pre-buckets)
    // read zero buckets; the 141-byte curve and 287-byte pool keep them.
    // Depth rung: a 141/150/151-byte curve keeps its buckets and reads the
    // depth / post-completion fields as 0; the 166-byte curve keeps them all.
    #[test]
    fn decodes_fee_bucket_era_account_lengths() {
        use crate::accounts::pump_amm::decode_pool;
        use crate::accounts::{decode_bonding_curve, decode_global};
        use crate::state::pump_amm::PoolFromIdl;
        use crate::state::{BondingCurveFromIdl, GlobalFromIdl};

        let curve = BondingCurveFromIdl {
            creator_fee_bps: 250,
            is_holder_reward: true,
            creator_fee: 11,
            protocol_fees: 33,
            depth: 1,
            initial_virtual_quote_reserves: 9,
            post_complete_base_out: 10,
            post_complete_quote_in: 12,
            ..Default::default()
        };
        let mut full = Vec::new();
        curve.try_serialize(&mut full).unwrap();
        assert_eq!(full.len(), 166);
        assert_eq!(full[141], 1);
        assert_eq!(full[142..150], 9u64.to_le_bytes());
        let pre_buckets = decode_bonding_curve(&at_len(&curve, 25, 125)).unwrap();
        assert_eq!(pre_buckets.creator_fee_bps, 250);
        assert!(pre_buckets.is_holder_reward);
        assert_eq!((pre_buckets.creator_fee, pre_buckets.protocol_fees), (0, 0));
        for len in [141, 150, 151] {
            let pre_depth = decode_bonding_curve(&at_len(&curve, 25, len)).unwrap();
            assert_eq!((pre_depth.creator_fee, pre_depth.protocol_fees), (11, 33));
            assert_eq!(pre_depth.depth, 0);
            assert_eq!(
                (
                    pre_depth.initial_virtual_quote_reserves,
                    pre_depth.post_complete_base_out,
                    pre_depth.post_complete_quote_in
                ),
                (0, 0, 0)
            );
        }
        let current = decode_bonding_curve(&full).unwrap();
        assert_eq!((current.creator_fee, current.protocol_fees), (11, 33));
        assert_eq!(current.depth, 1);
        assert_eq!(
            (
                current.initial_virtual_quote_reserves,
                current.post_complete_base_out,
                current.post_complete_quote_in
            ),
            (9, 10, 12)
        );

        let global = GlobalFromIdl {
            max_curve_depth: 2,
            ..Default::default()
        };
        let mut full = Vec::new();
        global.try_serialize(&mut full).unwrap();
        assert_eq!(full.len(), 1088);
        assert_eq!(decode_global(&full).unwrap().max_curve_depth, 2);
        assert_eq!(
            decode_global(&at_len(&global, 1, 1087))
                .unwrap()
                .max_curve_depth,
            0
        );

        let pool = PoolFromIdl {
            virtual_quote_reserves: -7,
            protocol_fees: 1,
            creator_fees: 3,
            ..Default::default()
        };
        let mut full = Vec::new();
        pool.try_serialize(&mut full).unwrap();
        assert_eq!(full.len(), 287);
        let pre_buckets = decode_pool(&at_len(&pool, 0, 271)).unwrap();
        assert_eq!(pre_buckets.virtual_quote_reserves, -7);
        assert_eq!(
            (pre_buckets.protocol_fees, pre_buckets.creator_fees),
            (0, 0)
        );
        let current = decode_pool(&full).unwrap();
        assert_eq!((current.protocol_fees, current.creator_fees), (1, 3));
    }

    // QuoteControl is a 2108-byte account with the entry Vec last and zero
    // padding after it; the padding must not disturb the list.
    #[test]
    fn decodes_zero_padded_quote_control() {
        use crate::accounts::decode_quote_control;
        use crate::state::{QuoteControl, QuoteControlMint};

        let mint = Pubkey::new_unique();
        let qc = QuoteControl {
            admin: Pubkey::new_unique(),
            reserves_admin: Pubkey::default(),
            _reserved: [0; 32],
            mints: vec![
                QuoteControlMint {
                    mint,
                    initial_virtual_quote_reserves: 123,
                },
                QuoteControlMint {
                    mint: Pubkey::new_unique(),
                    initial_virtual_quote_reserves: 456,
                },
            ],
        };
        let decoded = decode_quote_control(&at_len(&qc, 0, 2108)).unwrap();
        assert_eq!(decoded.mints.len(), 2);
        assert_eq!(decoded.mints[0].mint, mint);
        assert_eq!(decoded.mints[0].initial_virtual_quote_reserves, 123);
    }
}
