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

pub const GRADUATED_DEVNET_MINT_WITH_QUOTE_MINT: Pubkey =
    pubkey!("2FLTYhPLpiBpjANBgp9zq5XcX1yCZrVdBv5vZiDdpump");
// When the account is cloned from devnet we change the bondingCurve.quoteMint to USDC_QUOTE_MINT
pub const NOT_GRADUATED_DEVNET_MINT_WITH_QUOTE_MINT: Pubkey =
    pubkey!("5bztZmMVYUaoucJKmbGbL4bBqVREi9KC13aCWqEDpump");
pub const NON_GRADUATED_WITH_SHARING_FEE_CONFIG_AND_QUOTE_MINT: Pubkey =
    pubkey!("FRkJN46usFvkCaMUMnPqNQmXwkXNEbW4hqKRYbzXpump");
pub const GRADUATED_WITH_SHARING_FEE_CONFIG_AND_QUOTE_MINT: Pubkey =
    pubkey!("5LMXdz8i1NPhVMNgR71maKb3gWLXWHz3s5rJ3sqJpump");

/// Non-cashback coins by quote. The USDC ones are re-keyed to
/// [`USDC_QUOTE_MINT`] by `clone_devnet_accounts`. v3 / pool v2 trades
/// refuse cashback coins, so these are the fixtures those flows use.
/// Devnet coins graduate over time ([`NOT_CASHBACK_USDC_MINT`] already has),
/// so tests that need an open curve create their own coin instead.
pub const NOT_CASHBACK_SOL_CURVE_MINT: Pubkey =
    pubkey!("87xWBwS3ZDxLkgjvjJXqXMfjRCXdk9hstuFiXeC9pump");
pub const NOT_CASHBACK_USDC_MINT: Pubkey = pubkey!("AKipFUB8JfACJEhzBzQPUmBpJDVvN7Cp9r5RLEKJpump");
pub const NOT_CASHBACK_USDC_POOL_MINT: Pubkey =
    pubkey!("8XrqT9yLg3Bef1epT1a1J9VqoNN7GKXmy3Wxc1u4pump");

/// Test-only quote mint injected into the cloned `Global.whitelisted_quote_mints`
/// and synthesized as a legacy SPL Token mint by `clone_devnet_accounts` so
/// custom-quote `create_v2` / `buy_v2` / `sell_v2` flows can run on the local
/// validator. Paired with [`USDC_QUOTE_MINT_AUTHORITY`] so tests can mint freely.
pub const USDC_QUOTE_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

/// Mint authority for [`USDC_QUOTE_MINT`]. The matching `Keypair` lives at
/// [`USDC_QUOTE_MINT`] and must sign `mint_to` ixs in tests.
pub const USDC_QUOTE_MINT_AUTHORITY: Pubkey =
    pubkey!("6ycYUmdKav4kycBvZXSHfQVNzpEbt8SCVdxUgTLcuTF3");

/// Path to the `Keypair` JSON file for [`USDC_QUOTE_MINT`].
/// Used by tests; `#[allow(dead_code)]` because the clone-script binary
/// `#[path]`-imports this module and never reads the path constants.
#[allow(dead_code)]
pub const USDC_QUOTE_MINT_KEYPAIR_PATH: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/keys/quote_mint.json");

/// Path to the `Keypair` JSON file for [`USDC_QUOTE_MINT_AUTHORITY`].
/// Used by tests; see note on [`USDC_QUOTE_MINT_KEYPAIR_PATH`].
#[allow(dead_code)]
pub const USDC_QUOTE_MINT_AUTHORITY_KEYPAIR_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/keys/quote_mint_authority.json"
);

/// Stable label paired with each fixture mint. Cloning logs and test
/// failure messages use the label so it's obvious which fixture is
/// involved when something breaks.
#[derive(Clone, Copy, Debug)]
pub struct FixtureMint {
    pub label: &'static str,
    pub mint: Pubkey,
    /// When true, the cloned `bonding_curve`'s `quote_mint` field is
    /// rewritten to [`USDC_QUOTE_MINT`] so local-validator tests can exercise
    /// the non-SOL-quote (USDC) trade path without needing a real
    /// USDC-quoted curve on devnet. The bonding curve is otherwise cloned
    /// from the selected network as-is.
    pub patch_quote_mint_to_test: bool,
}

pub const FIXTURE_MINTS: &[FixtureMint] = &[
    FixtureMint {
        label: "graduated:devnet",
        mint: GRADUATED_DEVNET_MINT,
        patch_quote_mint_to_test: false,
    },
    FixtureMint {
        label: "not_graduated:devnet",
        mint: NOT_GRADUATED_DEVNET_MINT,
        patch_quote_mint_to_test: false,
    },
    FixtureMint {
        label: "not_graduated_with_quote_mint:devnet",
        mint: NOT_GRADUATED_DEVNET_MINT_WITH_QUOTE_MINT,
        patch_quote_mint_to_test: true,
    },
    FixtureMint {
        label: "graduated_with_quote_mint:devnet",
        mint: GRADUATED_DEVNET_MINT_WITH_QUOTE_MINT,
        patch_quote_mint_to_test: true,
    },
    FixtureMint {
        label: "not_graduated_with_sharing_fee_config_and_quote_mint:devnet",
        mint: NON_GRADUATED_WITH_SHARING_FEE_CONFIG_AND_QUOTE_MINT,
        patch_quote_mint_to_test: true,
    },
    FixtureMint {
        label: "graduated_with_sharing_fee_config_and_quote_mint:devnet",
        mint: GRADUATED_WITH_SHARING_FEE_CONFIG_AND_QUOTE_MINT,
        patch_quote_mint_to_test: true,
    },
    FixtureMint {
        label: "graduated_with_sharing_fee_config_and_quote_mint_cashback:devnet",
        mint: pubkey!("AaAL1HYhVbnBXxfEM3dwvUGnBFhzs5FWvtxFYboSpump"),
        patch_quote_mint_to_test: true,
    },
    // FOR USDC + SOL Mobile testing:
    FixtureMint {
        label: "cashback:sol:non-migrated:devnet",
        mint: pubkey!("Cz6uMdGiizBGpPhzShGKXMpw2QyH9zjFPR8m737Ypump"),
        patch_quote_mint_to_test: false,
    },
    FixtureMint {
        label: "cashback:usdc:non-migrated:devnet",
        mint: pubkey!("8oijL5GFQnkN78gK9DAJmGM9gteeBz8QvyvbPoZHpump"),
        patch_quote_mint_to_test: true,
    },
    FixtureMint {
        label: "cashback:sol:migrated:devnet",
        mint: pubkey!("HUMUMRFtzZNJnbbgmKhhjQW6PwEQbcCG6DrM2vYDpump"),
        patch_quote_mint_to_test: false,
    },
    FixtureMint {
        label: "cashback:usdc:migrated:devnet",
        mint: pubkey!("3depnRX8R42EEmDn9JoNuuXcXoNhHaBRQBVzGNcepump"),
        patch_quote_mint_to_test: true,
    },
    // Non cashback
    FixtureMint {
        label: "not-cashback:sol:non-migrated:devnet",
        mint: NOT_CASHBACK_SOL_CURVE_MINT,
        patch_quote_mint_to_test: false,
    },
    FixtureMint {
        label: "not-cashback:usdc:non-migrated:devnet",
        mint: NOT_CASHBACK_USDC_MINT,
        patch_quote_mint_to_test: true,
    },
    FixtureMint {
        label: "not-cashback:sol:migrated:devnet",
        mint: pubkey!("Bc1FHN8ZdybEw2XeoJrc5WsEg1CMAaJbXbn5yaHgpump"),
        patch_quote_mint_to_test: false,
    },
    FixtureMint {
        label: "not-cashback:usdc:migrated:devnet",
        mint: NOT_CASHBACK_USDC_POOL_MINT,
        patch_quote_mint_to_test: true,
    },
];
