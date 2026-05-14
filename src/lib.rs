//! Rust SDK for the pump and pump_amm Solana programs.
//!
//! Enable the default `client` feature for [`AsyncPumpClient`] and the full
//! `solana-sdk` / `solana-client` dependency stack. For on-chain CPI usage only,
//! depend with `default-features = false` — instruction helpers use
//! `solana-program` and avoid RPC crates.

use anchor_lang::declare_program;

declare_program!(pump);
declare_program!(pump_amm);

pub mod accounts;
#[cfg(feature = "client")]
pub mod async_client;
pub mod constants;
pub mod errors;
pub mod fixtures;
pub mod math;
pub mod pda;
pub mod sdk;
pub mod state;
pub mod token;

pub use accounts::{decode, decode_padded};
#[cfg(feature = "client")]
pub use async_client::{AsyncPumpClient, ComputeBudget};
pub use errors::{PumpClientError, Result};
pub use sdk::{AmmQuoteSource, CreateCoinParams, PumpSdk, TradeTxParams, TradeVenue};
