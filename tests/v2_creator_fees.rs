//! Coverage for the v2 creator-fee instructions.
//!
//! `collect_creator_fee_v2` is exercised on a fresh `USDC_QUOTE_MINT`
//! coin (same pattern as `v2_custom_quote_mint`) — the only quote mint
//! we can whitelist on the local validator (single-slot array) and the
//! only one we can mint to without an upstream authority.
//!
//! `transfer_creator_fees_to_pump_v2` is exercised against the cloned
//! graduated wSOL pool with a buy/sell large enough that creator fees
//! exceed the rent minimum (otherwise the program skips the transfer).
//!
//! `distribute_creator_fees_v2` is exercised against the
//! `GRADUATED_WITH_SHARING_FEE_CONFIG` fixture, which ships with a
//! populated `sharing_config` and a non-zero `creator_vault_quote_ata`
//! balance from prior devnet activity. The test fetches the on-chain
//! `SharingConfig` to thread shareholder pubkeys into the plural
//! builder (program reads them from `remaining_accounts`).
//! The non-graduated fixture is skipped because its
//! `creator_vault_quote_ata` is empty on devnet — nothing to distribute.
//!
//! Setup:
//!   1. `cargo run --features local-validator --bin clone_devnet_accounts`
//!   2. `cargo run --features local-validator --bin local-validator`  (other shell)
//!   3. `cargo test --features local-validator --test v2_creator_fees -- --ignored --nocapture`

#![cfg(feature = "local-validator")]

mod common;

use anchor_spl::associated_token::spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use anchor_spl::token::spl_token;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::signature::{read_keypair_file, Keypair, Signer};
use solana_sdk::transaction::Transaction;

use pump_rust_client::accounts::pump_amm::{decode_global_config, decode_pool};
use pump_rust_client::{constants, pda, PumpSdk};

use common::fixtures::{
    GRADUATED_DEVNET_MINT, GRADUATED_WITH_SHARING_FEE_CONFIG_AND_QUOTE_MINT, USDC_QUOTE_MINT,
    USDC_QUOTE_MINT_AUTHORITY, USDC_QUOTE_MINT_AUTHORITY_KEYPAIR_PATH,
};
use common::{
    airdrop_blocking, build_wsol_setup_tx, fee_recipients, load_alt, make_client, make_rpc,
    send_v0_tx, token_balance, DEFAULT_USER_LAMPORTS,
};

const QUOTE_MINT_AMOUNT: u64 = 1_000_000_000_000;
const BUY_BASE_AMOUNT: u64 = 100_000_000_000;
const MAX_QUOTE_COST: u64 = 500_000_000_000;

#[tokio::test]
#[ignore = "requires `cargo run --features local-validator --bin local-validator` running"]
async fn v2_collect_creator_fee() {
    let rpc = make_rpc();
    let client = make_client();
    let sdk = PumpSdk::new();
    let (_fee_recipient, buyback_fee_recipient) = fee_recipients(&client).await;

    let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
    let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;

    let quote_mint_authority = read_keypair_file(USDC_QUOTE_MINT_AUTHORITY_KEYPAIR_PATH)
        .expect("read USDC_QUOTE_MINT_AUTHORITY keypair");
    assert_eq!(quote_mint_authority.pubkey(), USDC_QUOTE_MINT_AUTHORITY);

    // user = creator → creator fees route to a creator_vault keyed by `user`.
    let user = Keypair::new();
    let mint = Keypair::new();
    println!(
        "[v2-collect] user/creator={} mint={} quote_mint={USDC_QUOTE_MINT}",
        user.pubkey(),
        mint.pubkey()
    );
    airdrop_blocking(&rpc, &user.pubkey(), DEFAULT_USER_LAMPORTS).await;

    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;
    let user_quote_ata =
        pda::associated_token(&user.pubkey(), &quote_token_program, &USDC_QUOTE_MINT).0;
    let user_base_ata =
        pda::associated_token(&user.pubkey(), &base_token_program, &mint.pubkey()).0;
    let creator_vault = pda::pump::creator_vault(&user.pubkey()).0;
    let creator_vault_quote_ata =
        pda::associated_token(&creator_vault, &quote_token_program, &USDC_QUOTE_MINT).0;

    // ---- Tx 1: user/buyback quote ATAs + mint_to user. ----
    let setup_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(200_000),
        create_associated_token_account_idempotent(
            &user.pubkey(),
            &user.pubkey(),
            &USDC_QUOTE_MINT,
            &quote_token_program,
        ),
        create_associated_token_account_idempotent(
            &user.pubkey(),
            &buyback_fee_recipient,
            &USDC_QUOTE_MINT,
            &quote_token_program,
        ),
        spl_token::instruction::mint_to(
            &quote_token_program,
            &USDC_QUOTE_MINT,
            &user_quote_ata,
            &USDC_QUOTE_MINT_AUTHORITY,
            &[],
            QUOTE_MINT_AMOUNT,
        )
        .expect("mint_to"),
    ];
    let blockhash = rpc.get_latest_blockhash().await.expect("blockhash");
    let setup_tx = Transaction::new_signed_with_payer(
        &setup_ixs,
        Some(&user.pubkey()),
        &[&user, &quote_mint_authority],
        blockhash,
    );
    let setup_sig = rpc
        .send_and_confirm_transaction(&setup_tx)
        .await
        .expect("setup tx");
    println!("[v2-collect] setup sig: {setup_sig}");

    // ---- Tx 2: create_v2 with USDC_QUOTE_MINT. ----
    let create_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        sdk.create_v2_instruction(
            mint.pubkey(),
            user.pubkey(),
            "CreatorFees",
            "CF",
            "https://example.com/cf.json",
            user.pubkey(),
            USDC_QUOTE_MINT,
            constants::SPL_TOKEN_PROGRAM_ID,
            false,
            false,
            0,
        ),
    ];
    let create_sig = send_v0_tx(&rpc, &create_ixs, &user, &[&user, &mint], &alt).await;
    println!("[v2-collect] create sig: {create_sig}");

    let global = client.fetch_global().await.expect("fetch_global");
    let bc_after_create = client
        .fetch_bonding_curve(&mint.pubkey())
        .await
        .expect("fetch bc after create");

    // ---- Tx 3: buy_v2 → routes creator fee into creator_vault_quote_ata. ----
    let mut buy_ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(400_000)];
    buy_ixs.extend(
        sdk.buy_v2_instructions(
            &global,
            &bc_after_create,
            mint.pubkey(),
            quote_token_program,
            user.pubkey(),
            BUY_BASE_AMOUNT,
            MAX_QUOTE_COST,
        )
        .expect("buy_v2_instructions"),
    );
    let buy_sig = send_v0_tx(&rpc, &buy_ixs, &user, &[&user], &alt).await;
    println!("[v2-collect] buy sig: {buy_sig}");

    let bc_after_buy = client
        .fetch_bonding_curve(&mint.pubkey())
        .await
        .expect("fetch bc after buy");
    let user_base_after_buy = token_balance(&rpc, &user_base_ata).await;

    // ---- Tx 4: sell_v2 → additional creator fee. ----
    let mut sell_ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(400_000)];
    sell_ixs.extend(
        sdk.sell_v2_instructions(
            &global,
            &bc_after_buy,
            mint.pubkey(),
            quote_token_program,
            user.pubkey(),
            user_base_after_buy,
            1,
        )
        .expect("sell_v2_instructions"),
    );
    let sell_sig = send_v0_tx(&rpc, &sell_ixs, &user, &[&user], &alt).await;
    println!("[v2-collect] sell sig: {sell_sig}");

    let vault_balance = token_balance(&rpc, &creator_vault_quote_ata).await;
    println!("[v2-collect] creator_vault_quote_ata after buy+sell = {vault_balance}");
    assert!(vault_balance > 0, "buy+sell must accrue creator fees");

    // ---- Tx 5: collect_creator_fee_v2 (plural — prepends idempotent ATA create). ----
    let user_quote_pre_collect = token_balance(&rpc, &user_quote_ata).await;
    let mut collect_ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(200_000)];
    collect_ixs.extend(sdk.collect_creator_fee_v2_instructions(
        user.pubkey(),
        user.pubkey(),
        USDC_QUOTE_MINT,
        quote_token_program,
        true,
    ));
    let collect_sig = send_v0_tx(&rpc, &collect_ixs, &user, &[&user], &alt).await;
    println!("[v2-collect] collect sig: {collect_sig}");

    let user_quote_post_collect = token_balance(&rpc, &user_quote_ata).await;
    let vault_post_collect = token_balance(&rpc, &creator_vault_quote_ata).await;
    let collected = user_quote_post_collect - user_quote_pre_collect;
    println!("[v2-collect] collected={collected} vault: {vault_balance} -> {vault_post_collect}");
    assert!(
        collected > 0,
        "collect should credit the creator's quote ATA"
    );
    assert!(
        vault_post_collect < vault_balance,
        "collect should drain creator_vault_quote_ata"
    );
}

#[tokio::test]
#[ignore = "requires `cargo run --features local-validator --bin local-validator` running"]
async fn v2_transfer_creator_fees_to_pump_wsol() {
    let rpc = make_rpc();
    let client = make_client();
    let sdk = PumpSdk::new();
    let (_fee_recipient, buyback_fee_recipient) = fee_recipients(&client).await;

    let mint = GRADUATED_DEVNET_MINT;
    let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
    let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;
    let quote_mint = constants::NATIVE_MINT;

    let pool_creator = pda::pump::pool_authority(&mint).0;
    let pool_address = pda::pump_amm::pool(0, &pool_creator, &mint, &quote_mint).0;
    let pool = decode_pool(
        &rpc.get_account(&pool_address)
            .await
            .expect("pool account missing — re-run clone_devnet_accounts")
            .data,
    )
    .expect("decode_pool");
    let global_config = decode_global_config(
        &rpc.get_account(&pda::pump_amm::global_config().0)
            .await
            .expect("pump_amm global_config")
            .data,
    )
    .expect("decode_global_config");
    println!(
        "[v2-transfer] pool={pool_address} coin_creator={}",
        pool.coin_creator
    );

    let user = Keypair::new();
    println!("[v2-transfer] user={} mint={mint}", user.pubkey());
    airdrop_blocking(&rpc, &user.pubkey(), DEFAULT_USER_LAMPORTS).await;

    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;
    let user_base_ata = pda::associated_token(&user.pubkey(), &base_token_program, &mint).0;
    // Each ~5 SOL round-trip accrues ~89k lamports of creator fee on the
    // cloned pool; rent minimum for the destination ATA is ~2_039_280
    // lamports, so we loop until we clear it. Wrap once upfront with a
    // big wSOL buffer — every sell deposits back to the same ATA, so we
    // only lose fees per round, not the wrap amount.
    let max_sol_cost = 5 * LAMPORTS_PER_SOL;
    // Sized so ~50 buy/sell rounds clear the 2.1M-lamport ATA rent floor at the
    // cloned pool's price (about 80k lamports of creator fee per round).
    let base_amount_out = 20_000_000_000_000u64;
    let max_rounds = 50;
    let wrap_amount = 40 * LAMPORTS_PER_SOL;

    let cc_authority = pda::pump_amm::coin_creator_vault_authority(&pool.coin_creator).0;
    let cc_vault_ata = pda::associated_token(&cc_authority, &quote_token_program, &quote_mint).0;
    let pump_creator_vault = pda::pump::creator_vault(&pool.coin_creator).0;
    let pump_creator_vault_ata =
        pda::associated_token(&pump_creator_vault, &quote_token_program, &quote_mint).0;

    let setup_tx = build_wsol_setup_tx(
        &rpc,
        &user,
        pda::pump::bonding_curve(&mint).0,
        buyback_fee_recipient,
        wrap_amount,
    )
    .await;
    let setup_sig = rpc
        .send_and_confirm_transaction(&setup_tx)
        .await
        .expect("wSOL setup");
    println!("[v2-transfer] setup sig: {setup_sig}");

    for round in 0..max_rounds {
        let mut buy_ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(400_000)];
        buy_ixs.extend(
            sdk.buy_amm_instructions(
                pool_address,
                &global_config,
                &pool,
                base_token_program,
                quote_token_program,
                user.pubkey(),
                base_amount_out,
                max_sol_cost,
            )
            .expect("buy_amm_instructions"),
        );
        send_v0_tx(&rpc, &buy_ixs, &user, &[&user], &alt).await;

        let sell_amount = token_balance(&rpc, &user_base_ata).await;
        let mut sell_ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(400_000)];
        sell_ixs.extend(
            sdk.sell_amm_instructions(
                pool_address,
                &global_config,
                &pool,
                base_token_program,
                quote_token_program,
                user.pubkey(),
                sell_amount,
                0,
            )
            .expect("sell_amm_instructions"),
        );
        send_v0_tx(&rpc, &sell_ixs, &user, &[&user], &alt).await;

        let cc_balance = token_balance(&rpc, &cc_vault_ata).await;
        println!("[v2-transfer] round {round}: cc_vault_ata = {cc_balance}");
        if cc_balance > 2_100_000 {
            break;
        }
    }

    // For wSOL the program may unwrap and credit pump_creator_vault as
    // native SOL instead of tokens in pump_creator_vault_ata. Track both.
    let amm_pre = token_balance(&rpc, &cc_vault_ata).await;
    let pump_ata_pre = token_balance(&rpc, &pump_creator_vault_ata).await;
    let pump_lamports_pre = rpc
        .get_balance(&pump_creator_vault)
        .await
        .expect("get_balance pump_creator_vault");
    println!(
        "[v2-transfer] pre: amm={amm_pre} pump_ata={pump_ata_pre} pump_lamports={pump_lamports_pre}"
    );
    assert!(
        amm_pre > 2_039_280,
        "AMM creator fees ({amm_pre}) below ATA rent minimum after {max_rounds} rounds — bump trade size"
    );

    let transfer_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(200_000),
        sdk.transfer_creator_fees_to_pump_v2_instruction(
            user.pubkey(),
            pool.coin_creator,
            quote_mint,
            quote_token_program,
        ),
    ];
    let transfer_sig = send_v0_tx(&rpc, &transfer_ixs, &user, &[&user], &alt).await;
    println!("[v2-transfer] transfer sig: {transfer_sig}");

    let amm_post = token_balance(&rpc, &cc_vault_ata).await;
    let pump_ata_post = token_balance(&rpc, &pump_creator_vault_ata).await;
    let pump_lamports_post = rpc
        .get_balance(&pump_creator_vault)
        .await
        .expect("get_balance pump_creator_vault");
    println!(
        "[v2-transfer] post: amm={amm_post} pump_ata={pump_ata_post} pump_lamports={pump_lamports_post}"
    );
    assert!(amm_post < amm_pre, "transfer should drain AMM vault");
    let pump_ata_delta = pump_ata_post.saturating_sub(pump_ata_pre);
    let pump_lamports_delta = pump_lamports_post.saturating_sub(pump_lamports_pre);
    assert!(
        pump_ata_delta > 0 || pump_lamports_delta > 0,
        "transfer should credit pump vault — either ATA tokens (delta={pump_ata_delta}) or native SOL (delta={pump_lamports_delta})"
    );
}

#[tokio::test]
#[ignore = "requires `cargo run --features local-validator --bin local-validator` running"]
async fn v2_collect_coin_creator_fee_wsol() {
    let rpc = make_rpc();
    let client = make_client();
    let sdk = PumpSdk::new();
    let (_fee_recipient, buyback_fee_recipient) = fee_recipients(&client).await;

    let mint = GRADUATED_DEVNET_MINT;
    let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
    let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;
    let quote_mint = constants::NATIVE_MINT;

    let pool_creator = pda::pump::pool_authority(&mint).0;
    let pool_address = pda::pump_amm::pool(0, &pool_creator, &mint, &quote_mint).0;
    let pool = decode_pool(
        &rpc.get_account(&pool_address)
            .await
            .expect("pool account missing — re-run clone_devnet_accounts")
            .data,
    )
    .expect("decode_pool");
    let global_config = decode_global_config(
        &rpc.get_account(&pda::pump_amm::global_config().0)
            .await
            .expect("pump_amm global_config")
            .data,
    )
    .expect("decode_global_config");
    println!(
        "[v2-collect-cc] pool={pool_address} coin_creator={}",
        pool.coin_creator
    );

    let user = Keypair::new();
    println!("[v2-collect-cc] user={} mint={mint}", user.pubkey());
    airdrop_blocking(&rpc, &user.pubkey(), DEFAULT_USER_LAMPORTS).await;

    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;
    let user_base_ata = pda::associated_token(&user.pubkey(), &base_token_program, &mint).0;
    let max_sol_cost = 5 * LAMPORTS_PER_SOL;
    // Sized so ~50 buy/sell rounds clear the 2.1M-lamport ATA rent floor at the
    // cloned pool's price (about 80k lamports of creator fee per round).
    let base_amount_out = 20_000_000_000_000u64;
    let max_rounds = 50;
    let wrap_amount = 40 * LAMPORTS_PER_SOL;

    let cc_authority = pda::pump_amm::coin_creator_vault_authority(&pool.coin_creator).0;
    let cc_vault_ata = pda::associated_token(&cc_authority, &quote_token_program, &quote_mint).0;
    let cc_token_account =
        pda::associated_token(&pool.coin_creator, &quote_token_program, &quote_mint).0;

    let setup_tx = build_wsol_setup_tx(
        &rpc,
        &user,
        pda::pump::bonding_curve(&mint).0,
        buyback_fee_recipient,
        wrap_amount,
    )
    .await;
    let setup_sig = rpc
        .send_and_confirm_transaction(&setup_tx)
        .await
        .expect("wSOL setup");
    println!("[v2-collect-cc] setup sig: {setup_sig}");

    for round in 0..max_rounds {
        let mut buy_ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(400_000)];
        buy_ixs.extend(
            sdk.buy_amm_instructions(
                pool_address,
                &global_config,
                &pool,
                base_token_program,
                quote_token_program,
                user.pubkey(),
                base_amount_out,
                max_sol_cost,
            )
            .expect("buy_amm_instructions"),
        );
        send_v0_tx(&rpc, &buy_ixs, &user, &[&user], &alt).await;

        let sell_amount = token_balance(&rpc, &user_base_ata).await;
        let mut sell_ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(400_000)];
        sell_ixs.extend(
            sdk.sell_amm_instructions(
                pool_address,
                &global_config,
                &pool,
                base_token_program,
                quote_token_program,
                user.pubkey(),
                sell_amount,
                0,
            )
            .expect("sell_amm_instructions"),
        );
        send_v0_tx(&rpc, &sell_ixs, &user, &[&user], &alt).await;

        let cc_balance = token_balance(&rpc, &cc_vault_ata).await;
        println!("[v2-collect-cc] round {round}: cc_vault_ata = {cc_balance}");
        if cc_balance > 2_100_000 {
            break;
        }
    }

    let amm_pre = token_balance(&rpc, &cc_vault_ata).await;
    let cc_ata_pre = token_balance(&rpc, &cc_token_account).await;
    let cc_lamports_pre = rpc
        .get_balance(&pool.coin_creator)
        .await
        .expect("get_balance coin_creator");
    println!(
        "[v2-collect-cc] pre: amm={amm_pre} cc_ata={cc_ata_pre} cc_lamports={cc_lamports_pre}"
    );
    assert!(
        amm_pre > 2_039_280,
        "AMM creator fees ({amm_pre}) below ATA rent minimum after {max_rounds} rounds"
    );

    let mut collect_ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(200_000)];
    collect_ixs.extend(sdk.collect_coin_creator_fee_instructions(
        user.pubkey(),
        pool.coin_creator,
        quote_mint,
        quote_token_program,
        true,
    ));
    let collect_sig = send_v0_tx(&rpc, &collect_ixs, &user, &[&user], &alt).await;
    println!("[v2-collect-cc] collect sig: {collect_sig}");

    let amm_post = token_balance(&rpc, &cc_vault_ata).await;
    let cc_ata_post = token_balance(&rpc, &cc_token_account).await;
    let cc_lamports_post = rpc
        .get_balance(&pool.coin_creator)
        .await
        .expect("get_balance coin_creator");
    println!(
        "[v2-collect-cc] post: amm={amm_post} cc_ata={cc_ata_post} cc_lamports={cc_lamports_post}"
    );
    assert!(amm_post < amm_pre, "collect should drain AMM vault");
    let cc_ata_delta = cc_ata_post.saturating_sub(cc_ata_pre);
    let cc_lamports_delta = cc_lamports_post.saturating_sub(cc_lamports_pre);
    assert!(
        cc_ata_delta > 0 || cc_lamports_delta > 0,
        "collect should credit coin_creator — ATA tokens (delta={cc_ata_delta}) or native SOL (delta={cc_lamports_delta})"
    );
}

#[tokio::test]
#[ignore = "requires `cargo run --features local-validator --bin local-validator` running"]
async fn v2_distribute_creator_fees_graduated() {
    use pump_rust_client::accounts::decode_sharing_config;

    let rpc = make_rpc();
    let client = make_client();
    let sdk = PumpSdk::new();
    let _ = fee_recipients(&client).await;

    let mint = GRADUATED_WITH_SHARING_FEE_CONFIG_AND_QUOTE_MINT;
    let bc = client
        .fetch_bonding_curve(&mint)
        .await
        .expect("fetch bonding_curve — did clone_devnet_accounts run?");
    let creator = bc.creator;
    let quote_mint = if bc.quote_mint == solana_sdk::pubkey::Pubkey::default() {
        constants::NATIVE_MINT
    } else {
        bc.quote_mint
    };
    let quote_token_program = rpc
        .get_account(&quote_mint)
        .await
        .expect("quote_mint on local validator")
        .owner;
    println!("[v2-distribute] mint={mint} creator={creator} quote_mint={quote_mint} qtp={quote_token_program}");

    // Fetch SharingConfig to learn the shareholder list.
    let sharing_config_key = pda::pump::sharing_config(&mint).0;
    let sharing_config_data = rpc
        .get_account(&sharing_config_key)
        .await
        .expect("sharing_config missing — did clone_devnet_accounts run?")
        .data;
    let sharing_config = decode_sharing_config(&sharing_config_data).expect("decode SharingConfig");
    let shareholders: Vec<_> = sharing_config
        .shareholders
        .iter()
        .map(|s| s.address)
        .collect();
    println!("[v2-distribute] {} shareholder(s)", shareholders.len());

    let creator_vault = pda::pump::creator_vault(&creator).0;
    let creator_vault_quote_ata =
        pda::associated_token(&creator_vault, &quote_token_program, &quote_mint).0;
    let vault_pre = token_balance(&rpc, &creator_vault_quote_ata).await;
    println!("[v2-distribute] creator_vault_quote_ata pre = {vault_pre}");
    assert!(
        vault_pre > 0,
        "fixture {mint} has 0 in creator_vault_quote_ata — re-run clone_devnet_accounts \
         when devnet has accumulated creator fees on this curve"
    );

    let payer = Keypair::new();
    println!("[v2-distribute] payer={}", payer.pubkey());
    airdrop_blocking(&rpc, &payer.pubkey(), DEFAULT_USER_LAMPORTS).await;
    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;

    let shareholder_balances_pre: Vec<u64> = {
        let mut v = Vec::with_capacity(shareholders.len());
        for sh in &shareholders {
            let ata = pda::associated_token(sh, &quote_token_program, &quote_mint).0;
            v.push(token_balance(&rpc, &ata).await);
        }
        v
    };

    let mut ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(400_000)];
    ixs.extend(sdk.distribute_creator_fees_v2_instructions(
        payer.pubkey(),
        mint,
        creator,
        quote_mint,
        quote_token_program,
        false,
        true,
        &shareholders,
    ));
    let sig = send_v0_tx(&rpc, &ixs, &payer, &[&payer], &alt).await;
    println!("[v2-distribute] distribute sig: {sig}");

    let vault_post = token_balance(&rpc, &creator_vault_quote_ata).await;
    println!("[v2-distribute] creator_vault_quote_ata post = {vault_post}");
    assert!(
        vault_post < vault_pre,
        "distribute should drain shareholders' cut (pre={vault_pre} post={vault_post})"
    );

    let mut total_credited = 0u64;
    for (sh, pre) in shareholders.iter().zip(shareholder_balances_pre.iter()) {
        let ata = pda::associated_token(sh, &quote_token_program, &quote_mint).0;
        let post = token_balance(&rpc, &ata).await;
        let delta = post.saturating_sub(*pre);
        println!("[v2-distribute] shareholder {sh}: {pre} -> {post} (+{delta})");
        total_credited += delta;
    }
    assert!(
        total_credited > 0,
        "at least one shareholder's quote ATA should have been credited"
    );
}
