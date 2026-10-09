//! pump-quote `create_v2` plus a two-hop `multi_hop_swap`: create coin Q
//! quoted in test USDC, buy some Q (a child's reserves derive from Q's price),
//! create coin C quoted in Q via `fetch_pump_quote_create`, then swap
//! USDC -> Q -> C in one instruction with `build_multi_hop_swap` (route discovered
//! from C's quote chain).
#[path = "../tests/common/mod.rs"]
mod common;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::address_lookup_table::AddressLookupTableAccount;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::instruction::Instruction;
use solana_sdk::signature::{Keypair, Signature, Signer};

use pump_rust_client::{constants, pda, PumpSdk};

use common::fixtures::USDC_QUOTE_MINT;
use common::{
    airdrop_blocking, fund_test_usdc, load_alt, make_client, make_rpc, send_v0_tx, token_balance,
    DEFAULT_USER_LAMPORTS,
};

/// `ixs` behind a compute-unit limit, as a v0 transaction paid by `user`.
async fn send(
    rpc: &RpcClient,
    alt: &AddressLookupTableAccount,
    user: &Keypair,
    cu: u32,
    ixs: Vec<Instruction>,
    extra_signers: &[&Keypair],
) -> Signature {
    let mut all = vec![ComputeBudgetInstruction::set_compute_unit_limit(cu)];
    all.extend(ixs);
    let mut signers = vec![user];
    signers.extend(extra_signers);
    send_v0_tx(rpc, &all, user, &signers, alt).await
}

#[tokio::main]
async fn main() {
    let rpc = make_rpc();
    let client = make_client();
    let sdk = PumpSdk::new();
    let user = Keypair::new();
    airdrop_blocking(&rpc, &user.pubkey(), DEFAULT_USER_LAMPORTS).await;
    fund_test_usdc(&rpc, &user, 100_000_000_000).await; // 100k USDC
    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;

    let q = Keypair::new();
    let create_q = sdk.create_v2_instruction(
        q.pubkey(),
        user.pubkey(),
        "Quote",
        "QUOTE",
        "https://example.com/q.json",
        user.pubkey(),
        USDC_QUOTE_MINT,
        constants::SPL_TOKEN_PROGRAM_ID,
        false,
        false,
        0,
    );
    send(&rpc, &alt, &user, 400_000, vec![create_q], &[&q]).await;
    let q = q.pubkey();
    let (_, ixs) = client
        .build_buy_v3(&user.pubkey(), &q, 500_000_000_000_000, 100)
        .await
        .expect("build_buy_v3");
    send(&rpc, &alt, &user, 400_000, ixs, &[]).await;

    let created = client
        .fetch_pump_quote_create(&q)
        .await
        .expect("fetch_pump_quote_create");
    let c = Keypair::new();
    let mut create_c = sdk.create_v2_instruction(
        c.pubkey(),
        user.pubkey(),
        "Child",
        "CHILD",
        "https://example.com/c.json",
        user.pubkey(),
        q,
        created.quote_token_program,
        false,
        false,
        0,
    );
    create_c.accounts.extend(created.remaining_accounts);
    let sig = send(&rpc, &alt, &user, 250_000, vec![create_c], &[&c]).await;
    let c = c.pubkey();
    println!(
        "create_v2 (quoted in {q}) sig: {sig}\n  depth={} virtual_quote_reserves={}",
        created.depth, created.virtual_quote_reserves
    );

    // The route (USDC -> Q -> C) is discovered from C's quote chain.
    let route = client
        .discover_route(&c, None)
        .await
        .expect("discover_route");
    println!("route: {route:?}");
    let (quote, ixs) = client
        .build_multi_hop_swap(&user.pubkey(), &c, true, 1_000_000 /* 1 USDC */, 100)
        .await
        .expect("build_multi_hop_swap");
    let sig = send(
        &rpc,
        &alt,
        &user,
        constants::pump_amm::MULTI_HOP_COMPUTE_UNITS,
        ixs,
        &[],
    )
    .await;
    let ata = pda::associated_token(&user.pubkey(), &constants::SPL_TOKEN_2022_PROGRAM_ID, &c).0;
    println!(
        "multi_hop_swap sig: {sig}\n  quoted out={} min_out={} (1% slippage)\n  user C balance = {}",
        quote.amount,
        quote.min_out,
        token_balance(&rpc, &ata).await
    );
}
