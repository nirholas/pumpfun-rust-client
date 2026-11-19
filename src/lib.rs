//! Rust SDK for the pump (bonding curve) and pump_amm (PumpSwap) Solana programs.
//!
//! - [`PumpSdk`] builds every trade, create and fee instruction from keys and
//!   decoded account state, and quotes trades. It does no I/O.
//! - [`math`] holds the quote math, integer-exact with the programs (`u128`
//!   intermediates, the programs' rounding directions).
//! - [`pda`], [`constants`] and [`accounts`] derive addresses, name program ids
//!   and seeds, and decode account data.
//! - `AsyncPumpClient` (feature `client`) fetches that state over RPC and
//!   builds, simulates and sends transactions.
//!
//! No feature is enabled by default, so the crate builds with only
//! `anchor-lang` / `anchor-spl` and can be used for CPI account derivation.
//!
//! # Quoting a buy
//!
//! ```
//! use pump_rust_client::{
//!     constants,
//!     pump::types::{FeeTier, Fees},
//!     state::{BondingCurve, FeeConfig, Global},
//!     PumpSdk,
//! };
//! # use anchor_lang::prelude::Pubkey;
//!
//! // Normally fetched and decoded with `accounts::decode_*`.
//! let fees = Fees { lp_fee_bps: 0, protocol_fee_bps: 100, creator_fee_bps: 50 };
//! let mut fee_config = FeeConfig::default();
//! fee_config.fee_tiers.push(FeeTier { market_cap_lamports_threshold: 0, fees });
//! let mut curve = BondingCurve::default();
//! curve.virtual_token_reserves = 1_073_000_000_000_000;
//! curve.virtual_quote_reserves = 30_000_000_000;
//! curve.real_token_reserves = 793_100_000_000_000;
//! curve.creator = Pubkey::new_unique();
//! curve.quote_mint = constants::NATIVE_MINT;
//!
//! let quote = PumpSdk::new().buy_quote_bonding_curve_sol_in(
//!     &Global::default(),
//!     &fee_config,
//!     &curve,
//!     1_000_000_000_000_000, // mint supply
//!     1_000_000_000,         // spend 1 SOL, fees included
//!     100,                   // 1% slippage
//! )?;
//! assert_eq!(quote.amount, 34_117_646_995_895);
//! assert_eq!(quote.min_out, 33_776_470_525_936);
//! # Ok::<(), pump_rust_client::math::QuoteError>(())
//! ```

pub mod pump {
    use anchor_lang::{self, declare_program};
    declare_program!(pump);
    // Account structs are re-exported from `state`, wrapped in `AccountWrapper`.
    pub(crate) use pump::accounts;
    pub use pump::{client, constants, cpi, events, program, types, ID, ID_CONST};
}
pub mod pump_amm {
    use anchor_lang::{self, declare_program};
    declare_program!(pump_amm);
    // Account structs are re-exported from `state`, wrapped in `AccountWrapper`.
    pub(crate) use pump_amm::accounts;
    pub use pump_amm::{client, constants, cpi, events, program, types, ID, ID_CONST};
}
pub mod pump_fees {
    use anchor_lang::{self, declare_program};
    declare_program!(pump_fees);
    pub use pump_fees::{client, cpi, events, program, types, ID, ID_CONST};
}
pub mod pump_agent_payments {
    use anchor_lang::{self, declare_program};
    declare_program!(pump_agent_payments);
    // Account structs are re-exported from `state`, wrapped in `AccountWrapper`.
    pub(crate) use pump_agent_payments::accounts;
    pub use pump_agent_payments::{client, constants, cpi, events, program, types, ID, ID_CONST};
}

pub mod account_wrapper;
pub mod accounts;
#[cfg(feature = "client")]
pub mod async_client;
pub mod constants;
pub mod errors;
pub mod math;
pub mod pda;
pub mod sdk;
pub mod state;
pub mod token;

pub use account_wrapper::AccountWrapper;
pub use accounts::decode;
#[cfg(feature = "client")]
pub use async_client::{
    AsyncPumpClient, ComputeBudget, CurveQuoteState, HopState, MultiHopQuoteState, MultiHopRoute,
    PumpQuoteCreate, RouteVenue,
};
pub use errors::{PumpClientError, Result};
pub use sdk::{
    AmmQuoteSource, CreateCoinParams, MultiHopHop, PumpPoolCtx, PumpPoolQuoteCtx, PumpSdk, Quote,
    RouteHop, TradeQuoteParams, TradeTxParams, TradeTxWithVenueParams, TradeVenue,
};
