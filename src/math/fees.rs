//! Fee math shared by the bonding-curve and AMM quote paths.
//!
//! Ports `@pump-fun/pump-sdk-internal/src/fees.ts` and
//! `@pump-fun/pump-swap-sdk/src/sdk/fees.ts`. All values are basis points
//! (`u64`) the way the on-chain programs store them.

use solana_program::pubkey::Pubkey;

use crate::pda;
use crate::pump::types::{FeeTier as PumpFeeTier, Fees as PumpFees};
use crate::pump_amm::types::{FeeTier as AmmFeeTier, Fees as AmmFees};
use crate::state::pump_amm::{FeeConfig as AmmFeeConfig, GlobalConfig};
use crate::state::{FeeConfig as PumpFeeConfig, Global};

/// `ceil(a / b)` for non-zero `b`. Mirrors `ceilDiv` in both TS SDKs.
#[inline]
pub fn ceil_div(a: u128, b: u128) -> u128 {
    a.div_ceil(b)
}

/// `ceil(amount * basis_points / 10_000)`. Mirrors `fee()` in both TS SDKs.
#[inline]
pub fn fee_amount(amount: u128, basis_points: u64) -> u128 {
    ceil_div(amount * basis_points as u128, 10_000)
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

/// Pick a tier whose threshold is the highest one `<=` `market_cap`. If
/// `market_cap` is below every threshold, fall back to the first tier.
/// Mirrors `calculateFeeTier` in both TS SDKs.
fn calculate_pump_fee_tier(tiers: &[PumpFeeTier], market_cap: u128) -> &PumpFees {
    let first = &tiers[0].fees;
    if market_cap < tiers[0].market_cap_lamports_threshold {
        return first;
    }
    for tier in tiers.iter().rev() {
        if market_cap >= tier.market_cap_lamports_threshold {
            return &tier.fees;
        }
    }
    first
}

fn calculate_amm_fee_tier(tiers: &[AmmFeeTier], market_cap: u128) -> &AmmFees {
    let first = &tiers[0].fees;
    if market_cap < tiers[0].market_cap_lamports_threshold {
        return first;
    }
    for tier in tiers.iter().rev() {
        if market_cap >= tier.market_cap_lamports_threshold {
            return &tier.fees;
        }
    }
    first
}

/// Resolve the bps for a bonding-curve trade. When `fee_config` is `None`,
/// fall back to the flat fees on `Global`. When provided, use the tiered
/// fees indexed by current market cap.
pub fn compute_bonding_curve_fee_bps(
    global: &Global,
    fee_config: Option<&PumpFeeConfig>,
    mint_supply: u64,
    virtual_quote_reserves: u64,
    virtual_token_reserves: u64,
) -> BondingCurveFeeBps {
    if let Some(cfg) = fee_config {
        let market_cap =
            bonding_curve_market_cap(mint_supply, virtual_quote_reserves, virtual_token_reserves);
        let fees = calculate_pump_fee_tier(&cfg.fee_tiers, market_cap);
        BondingCurveFeeBps {
            protocol_fee_bps: fees.protocol_fee_bps,
            creator_fee_bps: fees.creator_fee_bps,
        }
    } else {
        BondingCurveFeeBps {
            protocol_fee_bps: global.fee_basis_points,
            creator_fee_bps: global.creator_fee_basis_points,
        }
    }
}

/// Resolve the bps for an AMM trade. Mirrors `pump-swap-sdk`'s
/// `computeFeesBps`: pump pools (creator == pool-authority PDA) get tiered
/// fees; third-party pools get the `flat_fees` table; with no `fee_config`,
/// fall back to flat globals on `GlobalConfig`.
pub fn compute_amm_fee_bps(
    global_config: &GlobalConfig,
    fee_config: Option<&AmmFeeConfig>,
    base_mint: &Pubkey,
    pool_creator: &Pubkey,
    base_mint_supply: u64,
    base_reserve: u64,
    quote_reserve: u64,
) -> AmmFeeBps {
    if let Some(cfg) = fee_config {
        let market_cap = pool_market_cap(base_mint_supply, base_reserve, quote_reserve);
        let fees = if is_pump_pool(base_mint, pool_creator) {
            calculate_amm_fee_tier(&cfg.fee_tiers, market_cap)
        } else {
            &cfg.flat_fees
        };
        AmmFeeBps {
            lp_fee_bps: fees.lp_fee_bps,
            protocol_fee_bps: fees.protocol_fee_bps,
            creator_fee_bps: fees.creator_fee_bps,
        }
    } else {
        AmmFeeBps {
            lp_fee_bps: global_config.lp_fee_basis_points,
            protocol_fee_bps: global_config.protocol_fee_basis_points,
            creator_fee_bps: global_config.coin_creator_fee_basis_points,
        }
    }
}
