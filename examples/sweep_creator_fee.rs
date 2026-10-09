//! `sweep_creator_fee`: pay a curve's accrued creator fee bucket (v3 trades
//! leave it on the curve) into the creator vault. Permissionless; run after
//! `examples/buy_v3.rs` (same SOL curve, so the bucket is non-zero) and
//! before any distribute / CTO flow. On a SOL curve the vault receives
//! lamports; on a token-quoted curve its quote ATA receives tokens.
#[path = "../tests/common/mod.rs"]
mod common;

use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::signature::{Keypair, Signer};

use pump_rust_client::{constants, pda, PumpSdk};

use common::fixtures::NOT_CASHBACK_SOL_CURVE_MINT;
use common::{
    airdrop_blocking, load_alt, make_client, make_rpc, send_v0_tx, DEFAULT_USER_LAMPORTS,
};

#[tokio::main]
async fn main() {
    let rpc = make_rpc();
    let client = make_client();
    let sdk = PumpSdk::new();
    let payer = Keypair::new();
    let mint = NOT_CASHBACK_SOL_CURVE_MINT;
    let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;

    airdrop_blocking(&rpc, &payer.pubkey(), DEFAULT_USER_LAMPORTS).await;
    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;

    let bc = client
        .fetch_bonding_curve(&mint)
        .await
        .expect("fetch_bonding_curve");
    let vault = pda::pump::creator_vault(&bc.creator).0;
    println!(
        "creator {} bucket = {} vault lamports = {}",
        bc.creator,
        bc.creator_fee,
        rpc.get_balance(&vault).await.expect("get_balance")
    );

    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(200_000),
        sdk.sweep_creator_fee_instruction(
            payer.pubkey(),
            mint,
            bc.creator,
            bc.quote_mint,
            quote_token_program,
        ),
    ];
    let sig = send_v0_tx(&rpc, &ixs, &payer, &[&payer], &alt).await;
    let after = client
        .fetch_bonding_curve(&mint)
        .await
        .expect("fetch_bonding_curve");
    println!(
        "sweep_creator_fee sig: {sig}\n  bucket = {} vault lamports = {}",
        after.creator_fee,
        rpc.get_balance(&vault).await.expect("get_balance")
    );
}
