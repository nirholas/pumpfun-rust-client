//! Create a coin quoted in a Token-2022 (e.g. xStock) or SPL Token mint and
//! make the first buy in the same transaction.
//!
//! Standalone: runs against any cluster with a funded keypair that already
//! holds the quote token. Environment:
//!
//! ```text
//! RPC_URL          default https://api.devnet.solana.com
//! KEYPAIR          path to the payer/creator keypair (default ~/.config/solana/id.json)
//! QUOTE_MINT       required; the quote mint (xStock, USDC, ...)
//! TOKEN_AMOUNT     base tokens to buy, raw units (default 1_000_000_000 = 1 token at 6 decimals)
//! SLIPPAGE_BPS     default 100
//! CREATOR_FEE_BPS  per-coin creator fee; 0 = pump-fees schedule rate (default 0)
//! ALT              address lookup table (default: mainnet ALT if RPC_URL contains "mainnet", else devnet)
//! ```
//!
//! ```sh
//! QUOTE_MINT=<mint> KEYPAIR=~/.config/solana/id.json \
//!   cargo run --example create_v2_token2022_quote --features local-validator
//! ```
//!
//! Flow: the mint account's owner picks the quote token program (SPL Token or
//! Token-2022), `Global` + the `quote-control` PDA give the curve's initial
//! virtual quote reserves, a preview curve quotes the first buy, then
//! `create_v2` (with the quote-control PDA as 4th remaining account) + `buy_v2`
//! go out as one v0 transaction.

#[path = "../tests/common/mod.rs"]
mod common;

use std::env;
use std::str::FromStr;
use std::sync::Arc;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Keypair, Signer};

use pump_rust_client::{constants, AsyncPumpClient, PumpSdk};

use common::{load_alt, send_v0_tx};

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() {
    let rpc_url = env_or("RPC_URL", "https://api.devnet.solana.com");
    let keypair_path = env_or(
        "KEYPAIR",
        &format!("{}/.config/solana/id.json", env_or("HOME", ".")),
    );
    let quote_mint = Pubkey::from_str(&env::var("QUOTE_MINT").expect("QUOTE_MINT is required"))
        .expect("QUOTE_MINT is not a valid pubkey");
    let token_amount: u64 = env_or("TOKEN_AMOUNT", "1000000000")
        .parse()
        .expect("TOKEN_AMOUNT");
    let slippage_bps: u16 = env_or("SLIPPAGE_BPS", "100").parse().expect("SLIPPAGE_BPS");
    let creator_fee_bps: u64 = env_or("CREATOR_FEE_BPS", "0")
        .parse()
        .expect("CREATOR_FEE_BPS");
    let default_alt = if rpc_url.contains("mainnet") {
        constants::MAINNET_ALT
    } else {
        constants::DEVNET_ALT
    };
    let alt_key = env::var("ALT")
        .ok()
        .map(|s| Pubkey::from_str(&s).expect("ALT pubkey"))
        .unwrap_or(default_alt);

    let rpc = Arc::new(RpcClient::new_with_commitment(
        rpc_url,
        CommitmentConfig::confirmed(),
    ));
    let client = AsyncPumpClient::new(rpc.clone());
    let sdk = PumpSdk::new();
    let user = read_keypair_file(&keypair_path).expect("read KEYPAIR");
    let mint = Keypair::new();

    // 1. The mint's owner is the quote token program (SPL Token or Token-2022).
    let quote_mint_account = rpc
        .get_account(&quote_mint)
        .await
        .expect("fetch quote mint");
    let quote_token_program = quote_mint_account.owner;
    assert!(
        quote_token_program == constants::SPL_TOKEN_PROGRAM_ID
            || quote_token_program == constants::SPL_TOKEN_2022_PROGRAM_ID,
        "QUOTE_MINT is not owned by SPL Token or Token-2022"
    );

    // 2. Global + quote-control decide the curve's initial virtual quote reserves.
    let global = client.fetch_global().await.expect("fetch_global");
    let fee_config = client.fetch_fee_config().await.expect("fetch_fee_config");
    let quote_control = client.fetch_quote_control().await.ok();
    let virtual_quote_reserves =
        PumpSdk::initial_virtual_quote_reserves(&global, quote_control.as_ref(), &quote_mint)
            .expect("QUOTE_MINT is neither whitelisted on Global nor listed in quote-control");

    // 3. Quote the first buy against the curve create_v2 will initialize.
    let preview = PumpSdk::initial_bonding_curve(
        &global,
        user.pubkey(),
        quote_mint,
        virtual_quote_reserves,
        false,
        false,
        creator_fee_bps,
    );
    let quote = sdk
        .buy_quote_bonding_curve_token_out(
            &global,
            &fee_config,
            &preview,
            global.token_total_supply,
            token_amount,
            slippage_bps,
        )
        .expect("quote first buy");
    println!(
        "quote mint {quote_mint} (program {quote_token_program}); initial virtual quote reserves {virtual_quote_reserves}"
    );
    println!(
        "buying {token_amount} base units costs {} quote units (max {} with {slippage_bps} bps slippage); the payer's quote ATA must hold at least the max",
        quote.amount, quote.max_input
    );

    // 4. create_v2 (+ quote-control PDA) and buy_v2 in one transaction.
    let alt = load_alt(&rpc, alt_key).await;
    let mut ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(400_000)];
    ixs.extend(
        sdk.create_v2_and_buy_instruction(
            mint.pubkey(),
            user.pubkey(),
            "Example",
            "EX",
            "https://example.com/ex.json",
            user.pubkey(),
            quote_mint,
            quote_token_program,
            false,
            false,
            creator_fee_bps,
            None,
            &global,
            token_amount,
            quote.max_input,
        )
        .expect("create_v2_and_buy_instruction"),
    );
    let sig = send_v0_tx(&rpc, &ixs, &user, &[&user, &mint], &alt).await;
    println!("create_v2 + buy_v2 sig: {sig}");

    let curve = client
        .fetch_bonding_curve(&mint.pubkey())
        .await
        .expect("fetch_bonding_curve");
    println!(
        "mint {} quote_mint {} virtual_quote_reserves {} creator_fee_bps {}",
        mint.pubkey(),
        curve.quote_mint,
        curve.virtual_quote_reserves,
        curve.creator_fee_bps
    );
}
