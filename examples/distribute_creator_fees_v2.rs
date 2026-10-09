#[path = "../tests/common/mod.rs"]
mod common;

use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

use pump_rust_client::accounts::decode_sharing_config;
use pump_rust_client::{constants, pda, PumpSdk};

use common::fixtures::{GRADUATED_WITH_SHARING_FEE_CONFIG_AND_QUOTE_MINT, USDC_QUOTE_MINT};
use common::{
    airdrop_blocking, load_alt, make_client, make_rpc, send_v0_tx, DEFAULT_USER_LAMPORTS,
};

#[tokio::main]
async fn main() {
    let rpc = make_rpc();
    let client = make_client();
    let sdk = PumpSdk::new();

    let mint = GRADUATED_WITH_SHARING_FEE_CONFIG_AND_QUOTE_MINT;
    let bc = client
        .fetch_bonding_curve(&mint)
        .await
        .expect("fetch_bonding_curve");

    let sharing_config = decode_sharing_config(
        &rpc.get_account(&pda::pump::sharing_config(&mint).0)
            .await
            .expect("sharing_config")
            .data,
    )
    .expect("decode SharingConfig");
    let shareholders: Vec<Pubkey> = sharing_config
        .shareholders
        .iter()
        .map(|s| s.address)
        .collect();

    let payer = Keypair::new();
    airdrop_blocking(&rpc, &payer.pubkey(), DEFAULT_USER_LAMPORTS).await;
    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;

    let mut ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(400_000)];
    ixs.extend(sdk.distribute_creator_fees_v2_instructions(
        payer.pubkey(),
        mint,
        bc.creator,
        USDC_QUOTE_MINT,
        constants::SPL_TOKEN_PROGRAM_ID,
        false,
        true,
        &shareholders,
    ));
    let sig = send_v0_tx(&rpc, &ixs, &payer, &[&payer], &alt).await;
    println!("distribute_creator_fees_v2 sig: {sig}");
}
