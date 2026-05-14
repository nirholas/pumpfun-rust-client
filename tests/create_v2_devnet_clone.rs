//! End-to-end test: create a Token-2022 coin and immediately buy on the
//! local validator using `PumpSdk::create_v2_and_buy_instruction` against
//! state cloned from a Solana cluster.
//!
//! The full create+buy flow is packed into a single v0 versioned tx using
//! the cloned pump trade ALT. Around the SDK helper we still need the
//! wSOL setup the helper does NOT cover: the bonding-curve and
//! buyback-recipient wSOL ATAs, plus wrap/unwrap on the user's wSOL ATA.
//!
//! Pre-requisite (run in a separate shell, in this order):
//!   1. `cargo run --features local-validator --bin clone_devnet_accounts`
//!   2. `cargo run --features local-validator --bin local-validator`
//!
//! Then in this shell:
//!   `cargo test --features local-validator --test create_v2_devnet_clone -- --ignored --nocapture`

#![cfg(feature = "local-validator")]

use std::sync::Arc;
use std::time::Duration;

use anchor_spl::token::spl_token::instruction::close_account;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::address_lookup_table::state::AddressLookupTable;
use solana_sdk::address_lookup_table::AddressLookupTableAccount;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::{v0, VersionedMessage};
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::VersionedTransaction;

use pump_rust_client::{constants, pda, AsyncPumpClient, PumpSdk};

const LOCAL_RPC: &str = "http://127.0.0.1:8899";

#[tokio::test]
#[ignore = "requires `cargo run --features local-validator --bin local-validator` running"]
async fn create_v2_devnet_clone_full_flow() {
    let rpc = Arc::new(RpcClient::new_with_commitment(
        LOCAL_RPC.to_string(),
        CommitmentConfig::confirmed(),
    ));
    let client = AsyncPumpClient::new(rpc.clone());
    let sdk = PumpSdk::new();

    // ---- 1. Sanity-check the cloned program state. ----
    let global = client.fetch_global().await.expect(
        "Global on local validator — did you run clone_devnet_accounts and start the validator?",
    );
    assert!(
        global.create_v2_enabled,
        "cloned snapshot has create_v2 disabled — re-clone after admin enables it"
    );
    let fee_recipient = pick_first_nonzero(&[global.fee_recipient])
        .or_else(|| pick_first_nonzero(&global.fee_recipients))
        .expect("Global has no fee_recipient set");
    let buyback_fee_recipient = pick_first_nonzero(&global.buyback_fee_recipients)
        .expect("Global has no buyback_fee_recipients[] entry set");
    println!("fee_recipient         = {fee_recipient}");
    println!("buyback_fee_recipient = {buyback_fee_recipient}");

    // ---- 2. Fund a fresh user via the validator's local faucet. ----
    let user = Keypair::new();
    let mint = Keypair::new();
    println!("user = {}, mint = {}", user.pubkey(), mint.pubkey());
    airdrop_blocking(&rpc, &user.pubkey(), 50 * LAMPORTS_PER_SOL).await;

    // ---- 3. Build the single tx: wSOL setup + SDK helper + unwrap. ----
    let alt = load_alt(&rpc, constants::DEVNET_ALT).await;
    println!("loaded ALT with {} addresses", alt.addresses.len());

    let token_amount = 1_000_000_000;
    let max_sol_cost = LAMPORTS_PER_SOL;
    let quote_mint = constants::NATIVE_MINT;
    let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;
    let user_wsol_ata = pda::associated_token(&user.pubkey(), &quote_token_program, &quote_mint).0;

    // Stress-test: max-length name/symbol/uri per `create_v2.rs`
    // (CREATE_V2_MAX_NAME_LENGTH=32, _SYMBOL_=13, _URI_=200).
    let name = "n".repeat(32);
    let symbol = "s".repeat(13);
    let uri = "u".repeat(200);

    let mut ixs: Vec<Instruction> = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        ComputeBudgetInstruction::set_compute_unit_price(1_000_000),
    ];
    ixs.extend(sdk.create_v2_and_buy_instruction(
        mint.pubkey(),
        user.pubkey(),
        name,
        symbol,
        uri,
        user.pubkey(),
        false,
        false,
        fee_recipient,
        buyback_fee_recipient,
        token_amount,
        max_sol_cost,
    ));
    ixs.push(
        close_account(
            &quote_token_program,
            &user_wsol_ata,
            &user.pubkey(),
            &user.pubkey(),
            &[],
        )
        .expect("close_account"),
    );

    println!("submitting {} instructions in v0 tx", ixs.len());
    let sig = send_v0_tx(&rpc, &ixs, &user, &[&user, &mint], &alt).await;
    println!("create_v2_and_buy sig: {sig}");

    // ---- 4. Verify post-buy program state. ----
    let bonding_curve_pda = pda::pump::bonding_curve(&mint.pubkey()).0;
    let bc_account = rpc
        .get_account(&bonding_curve_pda)
        .await
        .expect("bonding_curve account");
    assert_eq!(
        bc_account.owner,
        pump_rust_client::pump::ID,
        "bonding_curve owner"
    );

    let bc = client
        .fetch_bonding_curve(&mint.pubkey())
        .await
        .expect("decode bonding_curve");
    assert_eq!(bc.creator, user.pubkey(), "bonding_curve.creator");
    assert!(!bc.complete, "bonding_curve should not be complete");
    assert!(!bc.is_mayhem_mode, "bonding_curve.is_mayhem_mode");
    assert_eq!(
        bc.token_total_supply, global.token_total_supply,
        "bonding_curve.token_total_supply"
    );
    assert!(
        bc.real_token_reserves < global.initial_real_token_reserves,
        "buy_v2 should have shrunk real_token_reserves below initial ({} >= {})",
        bc.real_token_reserves,
        global.initial_real_token_reserves
    );
    assert!(
        bc.real_quote_reserves > 0,
        "buy_v2 should have credited real_quote_reserves (got 0)"
    );
    println!(
        "bonding_curve: creator={} real_token_reserves={} real_quote_reserves={} complete={}",
        bc.creator, bc.real_token_reserves, bc.real_quote_reserves, bc.complete
    );
}

fn pick_first_nonzero(xs: &[Pubkey]) -> Option<Pubkey> {
    xs.iter().copied().find(|p| *p != Pubkey::default())
}

async fn airdrop_blocking(rpc: &RpcClient, to: &Pubkey, lamports: u64) {
    let sig = rpc
        .request_airdrop(to, lamports)
        .await
        .expect("request_airdrop");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if rpc.confirm_transaction(&sig).await.unwrap_or(false) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "airdrop did not confirm within 30s"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let bal = rpc.get_balance(to).await.expect("get_balance");
    assert!(bal >= lamports, "airdrop confirmed but balance={bal}");
}

async fn load_alt(rpc: &RpcClient, key: Pubkey) -> AddressLookupTableAccount {
    let acct = rpc
        .get_account(&key)
        .await
        .expect("ALT account missing on local validator — re-run clone_devnet_accounts");
    let parsed = AddressLookupTable::deserialize(&acct.data)
        .expect("deserialize ALT — was the cloned ALT properly re-serialized for tests?");
    AddressLookupTableAccount {
        key,
        addresses: parsed.addresses.into_owned(),
    }
}

async fn send_v0_tx(
    rpc: &RpcClient,
    ixs: &[Instruction],
    payer: &Keypair,
    signers: &[&Keypair],
    alt: &AddressLookupTableAccount,
) -> Signature {
    let blockhash = rpc.get_latest_blockhash().await.expect("latest_blockhash");
    let signers_dyn: Vec<&dyn Signer> = signers.iter().map(|kp| *kp as &dyn Signer).collect();

    let msg = v0::Message::try_compile(&payer.pubkey(), ixs, std::slice::from_ref(alt), blockhash)
        .expect("compile v0 message");
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &signers_dyn)
        .expect("sign versioned tx");

    rpc.send_and_confirm_transaction(&tx)
        .await
        .expect("send_and_confirm_transaction — check local-validator logs for program error")
}
