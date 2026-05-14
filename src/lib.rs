//! Rust SDK for the pump and pump_amm programs.
//!
//! Default features pull in `solana-client` for [`AsyncPumpClient`]. For CPI-only
//! use, set `default-features = false` (instruction helpers use `solana-program`).

use anchor_lang::declare_program;

declare_program!(pump);
declare_program!(pump_amm);
declare_program!(pump_agent_payments);

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

pub use accounts::{decode, decode_padded};
#[cfg(feature = "client")]
pub use async_client::{AsyncPumpClient, ComputeBudget};
pub use errors::{PumpClientError, Result};
pub use sdk::{
    AmmQuoteSource, CreateCoinParams, PumpPoolCtx, PumpPoolQuoteCtx, PumpSdk, Quote,
    TradeQuoteParams, TradeTxParams, TradeTxWithVenueParams, TradeVenue,
};
