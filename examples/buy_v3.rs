//! `buy_v3` in one call: `build_buy_v3` fetches the curve, fee schedule and
//! mint, quotes with slippage and returns the instructions (17 accounts, no
//! fee recipients; the protocol and creator fees accrue on the curve, the
//! buyback slice goes to a `Global` buyback recipient in the trade). On a SOL
//! curve the user pays lamports directly, so no wSOL setup.
#[path = "../tests/common/mod.rs"]
mod common;

use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::signature::{Keypair, Signer};

use pump_rust_client::{constants, pda};

use common::fixtures::NOT_CASHBACK_SOL_CURVE_MINT;
use common::{
    airdrop_blocking, load_alt, make_client, make_rpc, send_v0_tx, token_balance,
    DEFAULT_USER_LAMPORTS,
};

#[tokio::main]
async fn main() {
    let rpc = make_rpc();
    let client = make_client();
    let user = Keypair::new();
    let mint = NOT_CASHBACK_SOL_CURVE_MINT;
    let amount = 1_000_000_000u64; // 1_000 tokens (6 decimals)

    airdrop_blocking(&rpc, &user.pubkey(), DEFAULT_USER_LAMPORTS).await;
    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;

    let (quote, mut ixs) = client
        .build_buy_v3(&user.pubkey(), &mint, amount, 100)
        .await
        .expect("build_buy_v3");
    println!(
        "buy_v3 quote: cost={} max_input={} (1% slippage)",
        quote.amount, quote.max_input
    );
    ixs.insert(0, ComputeBudgetInstruction::set_compute_unit_limit(400_000));
    let sig = send_v0_tx(&rpc, &ixs, &user, &[&user], &alt).await;

    let (_, base_token_program) = client.fetch_mint(&mint).await.expect("fetch_mint");
    let ata = pda::associated_token(&user.pubkey(), &base_token_program, &mint).0;
    let after = client
        .fetch_bonding_curve(&mint)
        .await
        .expect("fetch_bonding_curve");
    println!(
        "buy_v3 sig: {sig}\n  user base balance = {}\n  curve buckets: protocol={} creator={}",
        token_balance(&rpc, &ata).await,
        after.protocol_fees,
        after.creator_fee
    );
}
