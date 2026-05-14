//! Pump base mints we ship as cloned fixtures for local-validator tests.
//!
//! `clone_devnet_accounts` walks this list and pulls each mint plus every
//! per-mint PDA the SDK touches (bonding curve, sharing/creator/v2 PDAs,
//! and — for graduated mints — the post-migration `pump_amm` `Pool` and
//! its base/quote/lp accounts). Tests under `tests/` reference these by
//! name so the fixture set is the single source of truth.
//!
//! Adding a new fixture: append a `(label, base58_mint)` entry below, run
//! `cargo run --features local-validator --bin clone_devnet_accounts` to
//! refresh `artifacts/accounts_to_load.zst`, then restart the local
//! validator.

use solana_program::{pubkey, pubkey::Pubkey};

/// Devnet mint that has graduated from the bonding curve onto `pump_amm`.
/// Used by AMM buy/sell tests.
pub const GRADUATED_DEVNET_MINT: Pubkey = pubkey!("4bYhSvpXvDQ1dXTELLtf6p9fPV3Th1PKBu4A1yN7pump");

/// Devnet mint still on the bonding curve. Used by `buy_v2` / `sell_v2`
/// tests and by the `AsyncPumpClient` fetch tests.
pub const NOT_GRADUATED_DEVNET_MINT: Pubkey =
    pubkey!("6Uw9RM7bxQYeGH9drfbEwTMqWJLaAsVe8wJJpq29pump");

/// Stable label paired with each fixture mint. Cloning logs and test
/// failure messages use the label so it's obvious which fixture is
/// involved when something breaks.
#[derive(Clone, Copy, Debug)]
pub struct FixtureMint {
    pub label: &'static str,
    pub mint: Pubkey,
}

/// All fixture mints in cloning order. New entries go at the end so the
/// indices in any test that pins by index stay stable.
pub const FIXTURE_MINTS: &[FixtureMint] = &[
    FixtureMint {
        label: "graduated:devnet",
        mint: GRADUATED_DEVNET_MINT,
    },
    FixtureMint {
        label: "not_graduated:devnet",
        mint: NOT_GRADUATED_DEVNET_MINT,
    },
];
