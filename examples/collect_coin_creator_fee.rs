#[path = "../tests/common/mod.rs"]
mod common;

use pump_rust_client::constants::NATIVE_MINT;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::signature::{Keypair, Signer};

use pump_rust_client::accounts::pump_amm::decode_pool;
use pump_rust_client::{constants, pda, PumpSdk};

use common::fixtures::GRADUATED_DEVNET_MINT;
use common::{airdrop_blocking, load_alt, make_rpc, send_v0_tx, DEFAULT_USER_LAMPORTS};

#[tokio::main]
async fn main() {
    let rpc = make_rpc();
    let sdk = PumpSdk::new();

    let mint = GRADUATED_DEVNET_MINT;
    let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;
    let quote_mint = NATIVE_MINT;

    let pool_creator = pda::pump::pool_authority(&mint).0;
    let pool_address = pda::pump_amm::pool(0, &pool_creator, &mint, &quote_mint).0;
    let pool = decode_pool(
        &rpc.get_account(&pool_address)
            .await
            .expect("pool account")
            .data,
    )
    .expect("decode_pool");

    let user = Keypair::new();
    airdrop_blocking(&rpc, &user.pubkey(), DEFAULT_USER_LAMPORTS).await;
    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;

    let mut collect_ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(200_000)];
    collect_ixs.extend(sdk.collect_coin_creator_fee_instructions(
        user.pubkey(),
        pool.coin_creator,
        quote_mint,
        quote_token_program,
        true,
    ));
    let sig = send_v0_tx(&rpc, &collect_ixs, &user, &[&user], &alt).await;
    println!("collect_coin_creator_fee sig: {sig}");
}
