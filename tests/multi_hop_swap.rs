//! pump-quote `create_v2` and pump-amm `multi_hop_swap` against the local
//! validator: a coin C quoted in pump coin Q (itself quoted in test USDC),
//! then USDC -> Q -> C and back through both curves in one instruction, each
//! at zero slippage so the swap only lands if the SDK route quote is exact.
//!
//! Pre-requisite (run in a separate shell, in this order):
//!   1. `cargo run --features local-validator --bin clone_devnet_accounts`
//!   2. `cargo run --features local-validator --bin local-validator`
//!
//! Then: `cargo test --features local-validator --test multi_hop_swap -- --ignored --nocapture`

#![cfg(feature = "local-validator")]

mod common;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::address_lookup_table::AddressLookupTableAccount;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};

use pump_rust_client::{constants, pda, MultiHopRoute, PumpSdk, RouteVenue};

use common::fixtures::{NOT_CASHBACK_SOL_CURVE_MINT, NOT_CASHBACK_USDC_POOL_MINT, USDC_QUOTE_MINT};
use common::{
    airdrop_blocking, fund_test_usdc, load_alt, make_client, make_rpc, send_v0_tx, token_balance,
    DEFAULT_USER_LAMPORTS,
};

const USDC: u64 = 1_000_000;
const T22: Pubkey = constants::SPL_TOKEN_2022_PROGRAM_ID;
const TP: Pubkey = constants::SPL_TOKEN_PROGRAM_ID;

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

#[tokio::test]
#[ignore = "requires `cargo run --features local-validator --bin local-validator` running"]
async fn pump_quote_create_and_multi_hop_round_trip() {
    let rpc = make_rpc();
    let client = make_client();
    let sdk = PumpSdk::new();
    let user = Keypair::new();
    airdrop_blocking(&rpc, &user.pubkey(), DEFAULT_USER_LAMPORTS).await;
    let usdc_ata = fund_test_usdc(&rpc, &user, 100_000 * USDC).await;
    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;
    let balance = |mint: Pubkey, token_program: Pubkey| {
        let ata = pda::associated_token(&user.pubkey(), &token_program, &mint).0;
        let rpc = &rpc;
        async move { token_balance(rpc, &ata).await }
    };
    let global = client.fetch_global().await.unwrap();
    assert!(
        global.max_curve_depth >= 1,
        "pump-quote create_v2 disabled; re-run clone_devnet_accounts"
    );

    // Q: a USDC-quoted coin, bought down to a price that admits a child curve
    // (at launch price a child's graduation raise exceeds Q's supply).
    let q = Keypair::new();
    let create_q = sdk.create_v2_instruction(
        q.pubkey(),
        user.pubkey(),
        "Quote",
        "QUOTE",
        "https://example.com/q.json",
        user.pubkey(),
        USDC_QUOTE_MINT,
        TP,
        false,
        false,
        0,
    );
    send(&rpc, &alt, &user, 400_000, vec![create_q], &[&q]).await;
    let q = q.pubkey();
    let (_, ixs) = client
        .build_buy_v3(&user.pubkey(), &q, 500_000_000 * USDC, 0)
        .await
        .unwrap();
    send(&rpc, &alt, &user, 400_000, ixs, &[]).await;

    // C: quoted in Q; the program derives its reserves from Q's price.
    let created = client.fetch_pump_quote_create(&q).await.unwrap();
    let q_curve = client.fetch_bonding_curve(&q).await.unwrap();
    let (_, q_token_program) = client.fetch_mint(&q).await.unwrap();
    let expected = PumpSdk::pump_quote_initial_virtual_quote_reserves(
        &global,
        Some(&client.fetch_quote_control().await.unwrap()),
        &q_curve,
        None,
    )
    .unwrap();
    assert_eq!((created.virtual_quote_reserves, created.depth), expected);
    assert_eq!(created.quote_token_program, q_token_program);
    assert_eq!(
        created.remaining_accounts.len(),
        1,
        "unmigrated Q: its curve only"
    );
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
    send(&rpc, &alt, &user, 250_000, vec![create_c], &[&c]).await;
    let c = c.pubkey();
    let c_curve = client.fetch_bonding_curve(&c).await.unwrap();
    assert_eq!(c_curve.quote_mint, q);
    assert_eq!(c_curve.depth, 1);
    assert_eq!(
        c_curve.virtual_quote_reserves,
        created.virtual_quote_reserves
    );
    assert_eq!(
        c_curve.initial_virtual_quote_reserves,
        c_curve.virtual_quote_reserves
    );

    // USDC -> Q -> C: the output must equal the quote to the unit. Q's whole
    // remaining raise is a few USDC, so a larger input would complete Q and
    // the sell back through it would be refused.
    let route = client.discover_route(&c, None).await.unwrap();
    assert_eq!(
        route,
        MultiHopRoute {
            in_mint: USDC_QUOTE_MINT,
            out_mint: c,
            venues: vec![RouteVenue::Curve(q), RouteVenue::Curve(c)],
        }
    );
    let amount_in = USDC;
    let (quote, ixs) = client
        .build_multi_hop_swap(&user.pubkey(), &c, true, amount_in, 0)
        .await
        .unwrap();
    assert!(quote.amount > 0 && quote.min_out == quote.amount);
    let usdc_before = token_balance(&rpc, &usdc_ata).await;
    let q_before = balance(q, T22).await;
    send(
        &rpc,
        &alt,
        &user,
        constants::pump_amm::MULTI_HOP_COMPUTE_UNITS,
        ixs,
        &[],
    )
    .await;
    assert_eq!(
        usdc_before - token_balance(&rpc, &usdc_ata).await,
        amount_in
    );
    assert_eq!(balance(c, T22).await, quote.amount, "multi-hop buy output");
    assert_eq!(balance(q, T22).await, q_before, "Q only passes through");

    // C -> Q -> USDC: sell everything back.
    let (quote, ixs) = client
        .build_multi_hop_swap(&user.pubkey(), &c, false, quote.amount, 0)
        .await
        .unwrap();
    let usdc_before = token_balance(&rpc, &usdc_ata).await;
    send(
        &rpc,
        &alt,
        &user,
        constants::pump_amm::MULTI_HOP_COMPUTE_UNITS,
        ixs,
        &[],
    )
    .await;
    assert_eq!(balance(c, T22).await, 0);
    assert_eq!(
        token_balance(&rpc, &usdc_ata).await - usdc_before,
        quote.amount,
        "multi-hop sell output"
    );

    // Q -> C from a coin the user already holds: the route stops at Q (one hop).
    let q_in = q_before / 10;
    let (quote, ixs) = client
        .build_multi_hop_swap_from(&user.pubkey(), &q, &c, true, q_in, 0)
        .await
        .unwrap();
    let q_before = balance(q, T22).await;
    send(
        &rpc,
        &alt,
        &user,
        constants::pump_amm::MULTI_HOP_COMPUTE_UNITS,
        ixs,
        &[],
    )
    .await;
    assert_eq!(q_before - balance(q, T22).await, q_in);
    assert_eq!(balance(c, T22).await, quote.amount, "from-Q buy output");
}

#[tokio::test]
#[ignore = "requires `cargo run --features local-validator --bin local-validator` running"]
async fn discovered_pool_route_round_trip() {
    let rpc = make_rpc();
    let client = make_client();
    let user = Keypair::new();
    airdrop_blocking(&rpc, &user.pubkey(), DEFAULT_USER_LAMPORTS).await;
    let usdc_ata = fund_test_usdc(&rpc, &user, 100_000 * USDC).await;
    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;

    // A graduated USDC coin: discovery maps its curve to the canonical pool.
    let x = NOT_CASHBACK_USDC_POOL_MINT;
    let route = client.discover_route(&x, None).await.unwrap();
    let pool = pda::pump_amm::canonical_pool(&x, &USDC_QUOTE_MINT).0;
    assert_eq!(
        route,
        MultiHopRoute {
            in_mint: USDC_QUOTE_MINT,
            out_mint: x,
            venues: vec![RouteVenue::Pool(pool)],
        }
    );
    let base_token_program = client.fetch_mint(&x).await.unwrap().1;
    let x_ata = pda::associated_token(&user.pubkey(), &base_token_program, &x).0;

    // Buy through the pool (protocol leg on the pool: AMM buyback recipient).
    let amount_in = USDC;
    let (quote, ixs) = client
        .build_multi_hop_swap(&user.pubkey(), &x, true, amount_in, 0)
        .await
        .unwrap();
    let usdc_before = token_balance(&rpc, &usdc_ata).await;
    send(
        &rpc,
        &alt,
        &user,
        constants::pump_amm::MULTI_HOP_COMPUTE_UNITS,
        ixs,
        &[],
    )
    .await;
    assert_eq!(
        usdc_before - token_balance(&rpc, &usdc_ata).await,
        amount_in
    );
    assert_eq!(
        token_balance(&rpc, &x_ata).await,
        quote.amount,
        "pool buy output"
    );

    // And sell it all back.
    let (quote, ixs) = client
        .build_multi_hop_swap(&user.pubkey(), &x, false, quote.amount, 0)
        .await
        .unwrap();
    let usdc_before = token_balance(&rpc, &usdc_ata).await;
    send(
        &rpc,
        &alt,
        &user,
        constants::pump_amm::MULTI_HOP_COMPUTE_UNITS,
        ixs,
        &[],
    )
    .await;
    assert_eq!(token_balance(&rpc, &x_ata).await, 0);
    assert_eq!(
        token_balance(&rpc, &usdc_ata).await - usdc_before,
        quote.amount,
        "pool sell output"
    );
}

/// An open SOL curve, its route (rooted on wSOL) and the user's ATAs.
async fn sol_curve_setup() -> (
    std::sync::Arc<RpcClient>,
    pump_rust_client::AsyncPumpClient,
    Keypair,
    AddressLookupTableAccount,
    Pubkey,
    Pubkey,
    Pubkey,
) {
    let rpc = make_rpc();
    let client = make_client();
    let user = Keypair::new();
    airdrop_blocking(&rpc, &user.pubkey(), DEFAULT_USER_LAMPORTS).await;
    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;
    let s = NOT_CASHBACK_SOL_CURVE_MINT;
    let route = client.discover_route(&s, None).await.unwrap();
    assert_eq!(
        route,
        MultiHopRoute {
            in_mint: constants::NATIVE_MINT,
            out_mint: s,
            venues: vec![RouteVenue::Curve(s)],
        }
    );
    let s_program = client.fetch_mint(&s).await.unwrap().1;
    let s_ata = pda::associated_token(&user.pubkey(), &s_program, &s).0;
    let wsol_ata = pda::associated_token(&user.pubkey(), &TP, &constants::NATIVE_MINT).0;
    (rpc, client, user, alt, s, s_ata, wsol_ata)
}

#[tokio::test]
#[ignore = "requires the local validator running pump-programs-monorepo#63 binaries (SOL-curve hops)"]
async fn sol_curve_route_sells_into_native_sol() {
    let (rpc, client, user, alt, s, s_ata, wsol_ata) = sol_curve_setup().await;
    // Hold some S first (a plain v3 buy).
    let (_, ixs) = client
        .build_buy_v3(&user.pubkey(), &s, 1_000_000_000_000, 500)
        .await
        .unwrap();
    send(&rpc, &alt, &user, 400_000, ixs, &[]).await;
    let held = token_balance(&rpc, &s_ata).await;
    assert!(held > 0);

    // Sell through the route: lamports to the wallet, the wSOL ATA only a placeholder.
    let (quote, ixs) = client
        .build_multi_hop_swap(&user.pubkey(), &s, false, held, 0)
        .await
        .unwrap();
    // Rent the builder's idempotent creates may charge: the buyback recipient's
    // and the user's wSOL ATAs.
    let mut rent = 0;
    for ata in [ixs[0].accounts[1].pubkey, wsol_ata] {
        if rpc.get_account(&ata).await.is_err() {
            rent += rpc
                .get_minimum_balance_for_rent_exemption(165)
                .await
                .unwrap();
        }
    }
    let lamports_before = rpc.get_balance(&user.pubkey()).await.unwrap();
    send(
        &rpc,
        &alt,
        &user,
        constants::pump_amm::MULTI_HOP_COMPUTE_UNITS,
        ixs,
        &[],
    )
    .await;
    assert_eq!(token_balance(&rpc, &s_ata).await, 0);
    assert_eq!(
        token_balance(&rpc, &wsol_ata).await,
        0,
        "placeholder untouched"
    );
    // One signature (5_000 lamports), no priority fee.
    assert_eq!(
        rpc.get_balance(&user.pubkey()).await.unwrap() + 5_000 + rent,
        lamports_before + quote.amount,
        "SOL sell output in lamports"
    );
}

#[tokio::test]
#[ignore = "requires the local validator running pump-programs-monorepo#63 binaries (SOL-curve hops)"]
async fn sol_curve_route_buys_with_native_sol() {
    let (rpc, client, user, alt, s, s_ata, wsol_ata) = sol_curve_setup().await;
    // Buy with native SOL: the wSOL ATA is only a placeholder and stays empty.
    let (quote, ixs) = client
        .build_multi_hop_swap(&user.pubkey(), &s, true, 10_000_000, 0)
        .await
        .unwrap();
    send(
        &rpc,
        &alt,
        &user,
        constants::pump_amm::MULTI_HOP_COMPUTE_UNITS,
        ixs,
        &[],
    )
    .await;
    assert_eq!(
        token_balance(&rpc, &s_ata).await,
        quote.amount,
        "SOL buy output"
    );
    assert_eq!(
        token_balance(&rpc, &wsol_ata).await,
        0,
        "placeholder untouched"
    );
}
