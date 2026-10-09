//! Airdrop the test USDC quote mint to a wallet on the local validator.
//!
//! Usage: `airdropusdc <WALLET> <AMOUNT>`
//!   - WALLET: base58 pubkey of the recipient
//!   - AMOUNT: USDC amount in human units (the mint has 6 decimals, so
//!     `100` = 100 USDC = 100_000_000 raw token units). Accepts floats.
//!
//! Pre-requisites (in separate shells, in this order):
//!   1. `cargo run --features local-validator --bin clone_devnet_accounts`
//!      (synthesizes the test quote mint at `fixtures::USDC_QUOTE_MINT`)
//!   2. `cargo run --features local-validator --bin local-validator`
//!
//! Then:
//!   `cargo run --features local-validator --bin airdropusdc -- <WALLET> <AMOUNT>`
//!
//! The script uses `USDC_QUOTE_MINT_AUTHORITY` as both fee payer and
//! mint authority, requesting a SOL airdrop from the local faucet first
//! if it doesn't already have a balance.

#![cfg(feature = "local-validator")]

use std::str::FromStr;
use std::time::Duration;

use anchor_spl::associated_token::spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use anchor_spl::token::spl_token;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Signer};
use solana_sdk::transaction::Transaction;

use pump_rust_client::{constants, pda};

#[path = "../../../tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;
use fixtures::{
    USDC_QUOTE_MINT, USDC_QUOTE_MINT_AUTHORITY, USDC_QUOTE_MINT_AUTHORITY_KEYPAIR_PATH,
};

const LOCAL_RPC: &str = "http://127.0.0.1:8899";
const QUOTE_DECIMALS: u32 = 6;

fn parse_args() -> (Pubkey, u64) {
    let mut args = std::env::args().skip(1);
    let wallet_str = args
        .next()
        .unwrap_or_else(|| panic!("usage: airdropusdc <WALLET> <AMOUNT>"));
    let amount_str = args
        .next()
        .unwrap_or_else(|| panic!("usage: airdropusdc <WALLET> <AMOUNT>"));
    let wallet = Pubkey::from_str(&wallet_str).expect("WALLET must be a valid base58 pubkey");
    let usdc: f64 = amount_str
        .parse()
        .expect("AMOUNT must be a number (e.g. 100 or 1.5)");
    assert!(
        usdc.is_finite() && usdc >= 0.0,
        "AMOUNT must be finite and non-negative (got {usdc})"
    );
    let scale = 10f64.powi(QUOTE_DECIMALS as i32);
    let raw = (usdc * scale).round();
    assert!(
        raw <= u64::MAX as f64,
        "AMOUNT overflows u64 token units (got {raw})"
    );
    (wallet, raw as u64)
}

async fn ensure_authority_funded(rpc: &RpcClient, authority: &Pubkey) {
    if rpc.get_balance(authority).await.unwrap_or(0) >= LAMPORTS_PER_SOL {
        return;
    }
    let sig = rpc
        .request_airdrop(authority, 10 * LAMPORTS_PER_SOL)
        .await
        .expect("request_airdrop for mint authority");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if rpc.confirm_transaction(&sig).await.unwrap_or(false) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "authority airdrop did not confirm within 30s"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::main]
async fn main() {
    let (wallet, raw_amount) = parse_args();

    let url = std::env::var("PUMP_LOCAL_RPC").unwrap_or_else(|_| LOCAL_RPC.to_string());
    let rpc = RpcClient::new_with_commitment(url, CommitmentConfig::confirmed());
    let authority = read_keypair_file(USDC_QUOTE_MINT_AUTHORITY_KEYPAIR_PATH)
        .expect("read USDC_QUOTE_MINT_AUTHORITY keypair");
    assert_eq!(
        authority.pubkey(),
        USDC_QUOTE_MINT_AUTHORITY,
        "USDC_QUOTE_MINT_AUTHORITY constant must match the on-disk keypair"
    );

    ensure_authority_funded(&rpc, &authority.pubkey()).await;

    let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;
    let wallet_quote_ata = pda::associated_token(&wallet, &quote_token_program, &USDC_QUOTE_MINT).0;

    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(200_000),
        create_associated_token_account_idempotent(
            &authority.pubkey(),
            &wallet,
            &USDC_QUOTE_MINT,
            &quote_token_program,
        ),
        spl_token::instruction::mint_to(
            &quote_token_program,
            &USDC_QUOTE_MINT,
            &wallet_quote_ata,
            &USDC_QUOTE_MINT_AUTHORITY,
            &[],
            raw_amount,
        )
        .expect("mint_to ix"),
    ];
    let blockhash = rpc.get_latest_blockhash().await.expect("latest_blockhash");
    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&authority.pubkey()),
        &[&authority],
        blockhash,
    );
    let sig = rpc
        .send_and_confirm_transaction(&tx)
        .await
        .expect("send_and_confirm_transaction (airdrop USDC)");
    println!(
        "✅ Airdropped {raw_amount} USDC token units to {wallet} (ata: {wallet_quote_ata}) sig: {sig}"
    );
}
