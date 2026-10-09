#[path = "../tests/common/mod.rs"]
mod common;

use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::message::{v0, VersionedMessage};
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::VersionedTransaction;

use pump_rust_client::{constants, PumpSdk};

use common::fixtures::USDC_QUOTE_MINT;
use common::{airdrop_blocking, load_alt, make_rpc, DEFAULT_USER_LAMPORTS};

#[tokio::main]
async fn main() {
    let rpc = make_rpc();
    let sdk = PumpSdk::new();

    let user = Keypair::new();
    airdrop_blocking(&rpc, &user.pubkey(), DEFAULT_USER_LAMPORTS).await;
    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;

    let mut collect_ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(200_000)];
    collect_ixs.extend(sdk.collect_creator_fee_v2_instructions(
        user.pubkey(),
        user.pubkey(),
        USDC_QUOTE_MINT,
        constants::SPL_TOKEN_PROGRAM_ID,
        true,
    ));

    let blockhash = rpc.get_latest_blockhash().await.expect("latest_blockhash");
    let msg = v0::Message::try_compile(&user.pubkey(), &collect_ixs, &[alt], blockhash)
        .expect("compile v0 message");
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&user])
        .expect("sign versioned tx");
    match rpc.send_and_confirm_transaction(&tx).await {
        Ok(sig) => println!("collect_creator_fee_v2 sig: {sig}"),
        Err(err) => println!("collect_creator_fee_v2 send failed (expected when creator_vault ATA does not exist): {err}"),
    }
}
