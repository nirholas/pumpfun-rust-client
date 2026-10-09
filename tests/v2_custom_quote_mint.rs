//! End-to-end `create_v2` + `buy_v2` + `sell_v2` flow against a non-wSOL
//! custom quote mint. Uses the SPL Token mint synthesized at
//! [`common::fixtures::USDC_QUOTE_MINT`] by `clone_devnet_accounts` and
//! the matching authority keypair to fund the user's quote ATA before the
//! trades.
//!
//! Pre-requisite (run in a separate shell, in this order):
//!   1. `cargo run --features local-validator --bin clone_devnet_accounts`
//!   2. `cargo run --features local-validator --bin local-validator`
//!
//! Then in this shell:
//!   `cargo test --features local-validator --test v2_custom_quote_mint \
//!        -- --ignored --nocapture`

#![cfg(feature = "local-validator")]

mod common;

use anchor_spl::associated_token::spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use anchor_spl::token::spl_token;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Keypair, Signer};

use pump_rust_client::{constants, pda, PumpSdk};

use common::fixtures::{
    USDC_QUOTE_MINT, USDC_QUOTE_MINT_AUTHORITY, USDC_QUOTE_MINT_AUTHORITY_KEYPAIR_PATH,
};
use common::{
    airdrop_blocking, load_alt, make_client, make_rpc, send_v0_tx, token_balance,
    DEFAULT_USER_LAMPORTS,
};

/// Generous balance pre-funded onto the user's quote ATA so slippage and fees
/// never cause the test to trip on `TooMuchSolRequired`. Quote mint is 6 dec,
/// so this is 1_000_000 token units.
const QUOTE_MINT_AMOUNT: u64 = 1_000_000_000_000;

/// Base-token amount to buy on the freshly-created v2 bonding curve. Sized
/// large enough that the round-trip `sell_v2` proceeds (after protocol +
/// creator + buyback fees) remain strictly positive — at smaller sizes the
/// quote-side `sell_quote` rounds to 0 and the program fails with
/// `TooLittleSolReceived`.
const BUY_BASE_AMOUNT: u64 = 100_000_000_000;

/// Slippage ceiling for `buy_v2`, denominated in the custom quote mint's
/// smallest unit. Generous to keep the test robust against on-curve drift.
const MAX_QUOTE_COST: u64 = 500_000_000_000;

#[tokio::test]
#[ignore = "requires `cargo run --features local-validator --bin local-validator` running"]
async fn v2_custom_quote_mint_create_buy_sell() {
    let rpc = make_rpc();
    let client = make_client();
    let sdk = PumpSdk::new();

    let global = client.fetch_global().await.expect(
        "fetch_global on local validator — did you run clone_devnet_accounts and start the validator?",
    );
    // All non-zero fee + buyback recipients from `Global`. The SDK picks one at
    // random per trade (see `fee_recipient_from_pump_global` /
    // `buyback_fee_recipient_from_pump_global`), so every entry here is a
    // possible target on a given buy_v2/sell_v2 — and the program does not
    // create the recipient ATAs, so we pre-create them all up-front.
    let mut recipient_owners: Vec<Pubkey> = Vec::new();
    if global.fee_recipient != Pubkey::default() {
        recipient_owners.push(global.fee_recipient);
    }
    recipient_owners.extend(
        global
            .fee_recipients
            .iter()
            .copied()
            .filter(|p| *p != Pubkey::default()),
    );
    recipient_owners.extend(
        global
            .buyback_fee_recipients
            .iter()
            .copied()
            .filter(|p| *p != Pubkey::default()),
    );
    recipient_owners.sort();
    recipient_owners.dedup();
    assert!(
        !recipient_owners.is_empty(),
        "Global has no fee_recipient / buyback_fee_recipient entries set"
    );

    let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
    let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;

    // The mint authority keypair on disk MUST match the constant we ship as
    // the synthetic mint's authority — otherwise `mint_to` below silently
    // signs with the wrong key and fails inside spl-token.
    let quote_mint_authority = read_keypair_file(USDC_QUOTE_MINT_AUTHORITY_KEYPAIR_PATH)
        .expect("read USDC_QUOTE_MINT_AUTHORITY keypair");
    assert_eq!(
        quote_mint_authority.pubkey(),
        USDC_QUOTE_MINT_AUTHORITY,
        "USDC_QUOTE_MINT_AUTHORITY constant must match the on-disk keypair file"
    );

    let user = Keypair::new();
    let mint = Keypair::new();
    println!(
        "[v2-quote] user = {} base_mint = {} quote_mint = {USDC_QUOTE_MINT}",
        user.pubkey(),
        mint.pubkey()
    );
    airdrop_blocking(&rpc, &user.pubkey(), DEFAULT_USER_LAMPORTS).await;

    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;

    let user_quote_ata =
        pda::associated_token(&user.pubkey(), &quote_token_program, &USDC_QUOTE_MINT).0;
    let user_base_ata =
        pda::associated_token(&user.pubkey(), &base_token_program, &mint.pubkey()).0;

    // ---- Tx 1a: chunks of 3 idempotent ATA creates for the fee + buyback
    //             recipients. Batched 3-per-tx because the full recipient
    //             list does not fit in a single tx (account list overflows
    //             the 1232-byte limit even with the ALT). The other
    //             quote-side ATAs (creator_vault, volume_accumulator,
    //             bonding_curve) are created on-demand by the program
    //             inside buy_v2/sell_v2. ----
    for chunk in recipient_owners.chunks(3) {
        let mut ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(200_000)];
        for owner in chunk {
            ixs.push(create_associated_token_account_idempotent(
                &user.pubkey(),
                owner,
                &USDC_QUOTE_MINT,
                &quote_token_program,
            ));
        }
        send_v0_tx(&rpc, &ixs, &user, &[&user], &alt).await;
    }

    // ---- Tx 1b: user's quote ATA + mint_to to fund the trades. Split out
    //             so this is the only tx that needs the quote-mint authority
    //             as a co-signer. ----
    let user_setup_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(200_000),
        create_associated_token_account_idempotent(
            &user.pubkey(),
            &user.pubkey(),
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
        .expect("mint_to ix"),
    ];
    send_v0_tx(
        &rpc,
        &user_setup_ixs,
        &user,
        &[&user, &quote_mint_authority],
        &alt,
    )
    .await;
    let user_quote_after_setup = token_balance(&rpc, &user_quote_ata).await;
    assert_eq!(
        user_quote_after_setup, QUOTE_MINT_AMOUNT,
        "mint_to should fund the user's quote ATA exactly"
    );

    // ---- Tx 2: create_v2 with the custom quote mint. The non-wSOL branch
    //            in create_v2_instruction appends the 3 remaining accounts
    //            (quote_mint, associated_quote_bonding_curve, spl_token program)
    //            and the program creates associated_quote_bonding_curve via
    //            CPI. The bonding_curve will record `quote_mint = USDC_QUOTE_MINT`. ----
    let create_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        sdk.create_v2_instruction(
            mint.pubkey(),
            user.pubkey(),
            "TestQuote",
            "TQ",
            "https://example.com/tq.json",
            user.pubkey(),
            USDC_QUOTE_MINT,
            constants::SPL_TOKEN_PROGRAM_ID,
            false,
            false,
            0,
        ),
    ];
    let create_sig = send_v0_tx(&rpc, &create_ixs, &user, &[&user, &mint], &alt).await;
    println!("[v2-quote] create sig: {create_sig} Mint created {{mint.pubkey()}}");

    let bc_after_create = client
        .fetch_bonding_curve(&mint.pubkey())
        .await
        .expect("fetch bonding_curve after create_v2");
    assert_eq!(
        bc_after_create.quote_mint, USDC_QUOTE_MINT,
        "create_v2 must persist the custom quote mint on the bonding curve"
    );

    // ---- Tx 3: buy_v2_instructions = idempotent base/quote user ATAs +
    //            buy_v2. Quote_mint is read off bonding_curve so no extra
    //            plumbing on top of the existing wSOL-flow signature. ----
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
    println!("[v2-quote] buy sig: {buy_sig}");

    let bc_after_buy = client
        .fetch_bonding_curve(&mint.pubkey())
        .await
        .expect("fetch bonding_curve after v2 buy");
    let user_base_after_buy = token_balance(&rpc, &user_base_ata).await;
    let user_quote_after_buy = token_balance(&rpc, &user_quote_ata).await;
    assert_eq!(
        user_base_after_buy, BUY_BASE_AMOUNT,
        "buy_v2 should credit exactly BUY_BASE_AMOUNT base units"
    );
    assert!(
        user_quote_after_buy < user_quote_after_setup,
        "buy_v2 should debit some quote balance (pre={user_quote_after_setup} post={user_quote_after_buy})"
    );
    assert!(
        bc_after_buy.real_quote_reserves > bc_after_create.real_quote_reserves,
        "buy_v2 should grow real_quote_reserves (pre={} post={})",
        bc_after_create.real_quote_reserves,
        bc_after_buy.real_quote_reserves,
    );

    // ---- Tx 4: sell_v2_instructions for the entire base balance we just
    //            bought. Proceeds land in the user's quote ATA. ----
    let sell_amount = user_base_after_buy;
    let mut sell_ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(400_000)];
    sell_ixs.extend(
        sdk.sell_v2_instructions(
            &global,
            &bc_after_buy,
            mint.pubkey(),
            quote_token_program,
            user.pubkey(),
            sell_amount,
            1,
        )
        .expect("sell_v2_instructions"),
    );
    let sell_sig = send_v0_tx(&rpc, &sell_ixs, &user, &[&user], &alt).await;
    println!("[v2-quote] sell sig: {sell_sig}");

    let bc_after_sell = client
        .fetch_bonding_curve(&mint.pubkey())
        .await
        .expect("fetch bonding_curve after v2 sell");
    let user_base_after_sell = token_balance(&rpc, &user_base_ata).await;
    let user_quote_after_sell = token_balance(&rpc, &user_quote_ata).await;
    assert_eq!(
        user_base_after_sell, 0,
        "sell_v2 should drain the user's base balance"
    );
    assert!(
        user_quote_after_sell > user_quote_after_buy,
        "sell_v2 should credit quote proceeds (pre={user_quote_after_buy} post={user_quote_after_sell})"
    );
    assert!(
        bc_after_sell.real_quote_reserves < bc_after_buy.real_quote_reserves,
        "sell_v2 should shrink real_quote_reserves (pre={} post={})",
        bc_after_buy.real_quote_reserves,
        bc_after_sell.real_quote_reserves,
    );
}
