//! Helpers shared across the local-validator integration tests.
//!
//! Cargo treats every top-level `tests/*.rs` as its own crate, so the
//! standard way to share test code is `mod common;` per file with the
//! helpers living here. Everything is `pub` because each test file is a
//! separate compilation unit (the `#[allow(dead_code)]` on the module
//! avoids "unused" warnings in files that don't touch every helper).
//!
//! All helpers assume the local validator at `LOCAL_RPC` is up and was
//! booted with the snapshot from `clone_devnet_accounts`.

#![allow(dead_code)]

pub mod fixtures;

use std::sync::Arc;
use std::time::Duration;

use anchor_spl::associated_token::spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use anchor_spl::token::spl_token;
use anchor_spl::token::spl_token::instruction::{close_account, sync_native};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::address_lookup_table::state::AddressLookupTable;
use solana_sdk::address_lookup_table::AddressLookupTableAccount;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::{v0, VersionedMessage};
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Keypair, Signature, Signer};
#[allow(deprecated)]
use solana_sdk::system_instruction;
use solana_sdk::transaction::{Transaction, VersionedTransaction};

use pump_rust_client::{constants, pda, AsyncPumpClient};

/// Default local validator RPC URL; `PUMP_LOCAL_RPC` overrides it (pair with
/// `PUMP_LOCAL_RPC_PORT` on the validator). Tests panic if it isn't
/// reachable — `cargo run --features local-validator --bin local-validator`
/// must be running in another shell.
pub const LOCAL_RPC: &str = "http://127.0.0.1:8899";

/// The RPC URL the tests and examples use: `PUMP_LOCAL_RPC` or [`LOCAL_RPC`].
pub fn local_rpc_url() -> String {
    std::env::var("PUMP_LOCAL_RPC").unwrap_or_else(|_| LOCAL_RPC.to_string())
}

/// Fresh `RpcClient` pointed at the local validator with `confirmed`
/// commitment. Cheap to call once per test.
pub fn make_rpc() -> Arc<RpcClient> {
    Arc::new(RpcClient::new_with_commitment(
        local_rpc_url(),
        CommitmentConfig::confirmed(),
    ))
}

/// `AsyncPumpClient` wired to the same `RpcClient` returned by
/// [`make_rpc`]. Use this when the test needs the higher-level fetch /
/// build / send convenience around the SDK.
pub fn make_client() -> AsyncPumpClient {
    AsyncPumpClient::new(make_rpc())
}

/// First non-zero `Pubkey` in `xs`, or `None` if every entry is the
/// zero pubkey. Lets tests pull the live `fee_recipient` /
/// `buyback_fee_recipient` from `Global` without caring whether the
/// admin used the singular or list field on a given cluster.
pub fn pick_first_nonzero(xs: &[Pubkey]) -> Option<Pubkey> {
    xs.iter().copied().find(|p| *p != Pubkey::default())
}

/// `(fee_recipient, buyback_fee_recipient)` pulled from the cloned
/// `Global`. Both are required: tests panic with an actionable message
/// if the snapshot's admin state has them unset.
pub async fn fee_recipients(client: &AsyncPumpClient) -> (Pubkey, Pubkey) {
    let global = client.fetch_global().await.expect(
        "fetch_global on local validator — did you run clone_devnet_accounts and start the validator?",
    );
    let fee_recipient = pick_first_nonzero(&[global.fee_recipient])
        .or_else(|| pick_first_nonzero(&global.fee_recipients))
        .expect("Global has no fee_recipient set");
    let buyback_fee_recipient = pick_first_nonzero(&global.buyback_fee_recipients)
        .expect("Global has no buyback_fee_recipients[] entry set");
    (fee_recipient, buyback_fee_recipient)
}

/// Airdrop `lamports` to `to` and block until the local validator
/// confirms. Times out after 30s with a clear panic — the local faucet
/// is fast in practice (sub-second), anything slower is a setup bug.
pub async fn airdrop_blocking(rpc: &RpcClient, to: &Pubkey, lamports: u64) {
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

/// Generate a fresh `Keypair` and airdrop `lamports` to it. Convenient
/// for tests that need a clean user with predictable balance.
pub async fn funded_user(rpc: &RpcClient, lamports: u64) -> Keypair {
    let user = Keypair::new();
    airdrop_blocking(rpc, &user.pubkey(), lamports).await;
    user
}

/// Address lookup table account loaded straight off the local
/// validator. The cloning script rewrites the meta so it's active from
/// slot 0; if that step is missing the validator will reject any v0 tx
/// referencing the ALT.
pub async fn load_alt(rpc: &RpcClient, key: Pubkey) -> AddressLookupTableAccount {
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

/// Pack `ixs` into a v0 versioned tx using `alt` and sign with `signers`
/// (1232-byte limit; failures here are usually a missing ALT entry or an
/// oversize instruction list). Simulate it, or send it via [`send_v0_tx`].
pub async fn build_v0_tx(
    rpc: &RpcClient,
    ixs: &[Instruction],
    payer: &Keypair,
    signers: &[&Keypair],
    alt: &AddressLookupTableAccount,
) -> VersionedTransaction {
    let blockhash = rpc.get_latest_blockhash().await.expect("latest_blockhash");
    let msg = v0::Message::try_compile(&payer.pubkey(), ixs, std::slice::from_ref(alt), blockhash)
        .expect("compile v0 message");
    let signers_dyn: Vec<&dyn Signer> = signers.iter().map(|kp| *kp as &dyn Signer).collect();
    VersionedTransaction::try_new(VersionedMessage::V0(msg), &signers_dyn)
        .expect("sign versioned tx")
}

/// [`build_v0_tx`] then send + confirm.
pub async fn send_v0_tx(
    rpc: &RpcClient,
    ixs: &[Instruction],
    payer: &Keypair,
    signers: &[&Keypair],
    alt: &AddressLookupTableAccount,
) -> Signature {
    let tx = build_v0_tx(rpc, ixs, payer, signers, alt).await;
    rpc.send_and_confirm_transaction(&tx)
        .await
        .expect("send_and_confirm_transaction — check local-validator logs for program error")
}

/// Create the user's test-USDC ATA and mint `amount` raw units (6 decimals)
/// into it; the synthesized mint's authority keypair from `keys/` co-signs.
/// Returns the ATA.
pub async fn fund_test_usdc(rpc: &RpcClient, user: &Keypair, amount: u64) -> Pubkey {
    let authority = read_keypair_file(fixtures::USDC_QUOTE_MINT_AUTHORITY_KEYPAIR_PATH)
        .expect("read USDC_QUOTE_MINT_AUTHORITY keypair");
    assert_eq!(
        authority.pubkey(),
        fixtures::USDC_QUOTE_MINT_AUTHORITY,
        "USDC_QUOTE_MINT_AUTHORITY constant must match the on-disk keypair file"
    );
    let token_program = constants::SPL_TOKEN_PROGRAM_ID;
    let ata = pda::associated_token(&user.pubkey(), &token_program, &fixtures::USDC_QUOTE_MINT).0;
    let ixs = [
        create_associated_token_account_idempotent(
            &user.pubkey(),
            &user.pubkey(),
            &fixtures::USDC_QUOTE_MINT,
            &token_program,
        ),
        spl_token::instruction::mint_to(
            &token_program,
            &fixtures::USDC_QUOTE_MINT,
            &ata,
            &authority.pubkey(),
            &[],
            amount,
        )
        .expect("mint_to ix"),
    ];
    let blockhash = rpc.get_latest_blockhash().await.expect("latest_blockhash");
    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&user.pubkey()),
        &[user, &authority],
        blockhash,
    );
    rpc.send_and_confirm_transaction(&tx)
        .await
        .expect("fund_test_usdc");
    ata
}

/// Convenience wrapper around `pda::associated_token` for the user's
/// wSOL ATA, which every wSOL-quoted trade needs both before and after
/// the swap.
pub fn user_wsol_ata(user: &Pubkey) -> Pubkey {
    pda::associated_token(
        user,
        &constants::SPL_TOKEN_PROGRAM_ID,
        &constants::NATIVE_MINT,
    )
    .0
}

/// `close_account(user_wsol_ata)` with destination + owner = user.
/// Returns any wSOL balance to the user as native SOL. Always safe to
/// append after a trade.
pub fn unwrap_sol_ix(user: &Pubkey) -> Instruction {
    close_account(
        &constants::SPL_TOKEN_PROGRAM_ID,
        &user_wsol_ata(user),
        user,
        user,
        &[],
    )
    .expect("close_account: token program id is constant")
}

/// Build the wSOL-ATA-setup transaction the v2-style trade flows need
/// before the buy: idempotent ATA creates for `bonding_curve`, `user`,
/// and `buyback_fee_recipient`, then a `system_transfer` + `sync_native`
/// to wrap `wrap_lamports` SOL on the user's wSOL ATA.
///
/// Returned as a legacy `Transaction` because the wSOL flow fits inside
/// the 1232-byte limit without an ALT and tests of this module don't
/// need the complexity of a v0 build.
pub async fn build_wsol_setup_tx(
    rpc: &RpcClient,
    user: &Keypair,
    bonding_curve: Pubkey,
    buyback_fee_recipient: Pubkey,
    wrap_lamports: u64,
) -> Transaction {
    let quote_mint = constants::NATIVE_MINT;
    let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;
    let user_ata = user_wsol_ata(&user.pubkey());

    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(200_000),
        create_associated_token_account_idempotent(
            &user.pubkey(),
            &bonding_curve,
            &quote_mint,
            &quote_token_program,
        ),
        create_associated_token_account_idempotent(
            &user.pubkey(),
            &user.pubkey(),
            &quote_mint,
            &quote_token_program,
        ),
        create_associated_token_account_idempotent(
            &user.pubkey(),
            &buyback_fee_recipient,
            &quote_mint,
            &quote_token_program,
        ),
        system_instruction::transfer(&user.pubkey(), &user_ata, wrap_lamports),
        sync_native(&quote_token_program, &user_ata).expect("sync_native"),
    ];
    let blockhash = rpc.get_latest_blockhash().await.expect("latest_blockhash");
    Transaction::new_signed_with_payer(&ixs, Some(&user.pubkey()), &[user], blockhash)
}

/// Conservative default funding amount for tests: 50 SOL. Enough to
/// cover any single SDK trade flow plus rent/fees several times over.
pub const DEFAULT_USER_LAMPORTS: u64 = 50 * LAMPORTS_PER_SOL;

/// Read the parsed token amount on `ata`, defaulting to 0 if the
/// account does not exist yet (handy before the first buy creates it).
pub async fn token_balance(rpc: &RpcClient, ata: &Pubkey) -> u64 {
    match rpc.get_token_account_balance(ata).await {
        Ok(amount) => amount.amount.parse().expect("token balance is u64"),
        // Account-not-found → no ATA → balance 0.
        Err(_) => 0,
    }
}
