//! v3 curve trades, v2 pool trades and creator fee sweeps against the local
//! validator, each with zero-slippage quote parity: the on-chain limit is set
//! to the quoted amount, so the transaction itself fails if the SDK math
//! drifts from the program by a single unit. Every trade also checks the
//! buyback slice landed on the recipient in the trade, not on the venue.
//!
//! The curve tests create their own USDC-quoted coin (devnet fixtures
//! graduate over time); the pool test uses the graduated USDC fixture.
//!
//! Pre-requisite (run in a separate shell, in this order):
//!   1. `cargo run --features local-validator --bin clone_devnet_accounts`
//!   2. `cargo run --features local-validator --bin local-validator`
//!
//! Then: `cargo test --features local-validator --test v3_trades -- --ignored --nocapture`

#![cfg(feature = "local-validator")]

mod common;

use std::sync::Arc;

use anchor_spl::associated_token::spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::address_lookup_table::AddressLookupTableAccount;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

use pump_rust_client::math::amm::effective_quote_reserve;
use pump_rust_client::state::BondingCurve;
use pump_rust_client::{constants, pda, AmmQuoteSource, AsyncPumpClient, PumpSdk};

use common::fixtures::{NOT_CASHBACK_SOL_CURVE_MINT, NOT_CASHBACK_USDC_POOL_MINT, USDC_QUOTE_MINT};
use common::{
    airdrop_blocking, fund_test_usdc, load_alt, make_client, make_rpc, send_v0_tx, token_balance,
    DEFAULT_USER_LAMPORTS,
};

const USDC: u64 = 1_000_000;
const T22: Pubkey = constants::SPL_TOKEN_2022_PROGRAM_ID;
const TP: Pubkey = constants::SPL_TOKEN_PROGRAM_ID;

struct Env {
    rpc: Arc<RpcClient>,
    client: AsyncPumpClient,
    sdk: PumpSdk,
    user: Keypair,
    alt: AddressLookupTableAccount,
    usdc_ata: Pubkey,
}

impl Env {
    async fn new(usdc: u64) -> Self {
        let rpc = make_rpc();
        let user = Keypair::new();
        airdrop_blocking(&rpc, &user.pubkey(), DEFAULT_USER_LAMPORTS).await;
        let usdc_ata = fund_test_usdc(&rpc, &user, usdc).await;
        let alt = load_alt(&rpc, constants::DEVNET_ALT).await;
        Self {
            rpc,
            client: make_client(),
            sdk: PumpSdk::new(),
            user,
            alt,
            usdc_ata,
        }
    }

    async fn send(&self, cu: u32, ixs: Vec<Instruction>, extra_signers: &[&Keypair]) {
        let mut all = vec![ComputeBudgetInstruction::set_compute_unit_limit(cu)];
        all.extend(ixs);
        let mut signers = vec![&self.user];
        signers.extend(extra_signers);
        send_v0_tx(&self.rpc, &all, &self.user, &signers, &self.alt).await;
    }

    /// Test USDC is synthesized locally, so no recipient holds a USDC ATA
    /// yet; create it for every listed recipient (the SDK draws one at random
    /// and the plain single-instruction builders don't create it).
    async fn create_buyback_usdc_atas(&self, recipients: &[Pubkey]) {
        let ixs = recipients
            .iter()
            .filter(|r| **r != Pubkey::default())
            .map(|r| {
                create_associated_token_account_idempotent(
                    &self.user.pubkey(),
                    r,
                    &USDC_QUOTE_MINT,
                    &TP,
                )
            })
            .collect();
        self.send(400_000, ixs, &[]).await;
    }

    async fn balance(&self, mint: &Pubkey, token_program: &Pubkey) -> u64 {
        token_balance(
            &self.rpc,
            &pda::associated_token(&self.user.pubkey(), token_program, mint).0,
        )
        .await
    }

    async fn usdc(&self) -> u64 {
        token_balance(&self.rpc, &self.usdc_ata).await
    }

    async fn usdc_of(&self, owner: &Pubkey) -> u64 {
        token_balance(
            &self.rpc,
            &pda::associated_token(owner, &TP, &USDC_QUOTE_MINT).0,
        )
        .await
    }

    /// Summed lamports of `keys` (the SOL-curve buyback recipients).
    async fn lamports_of(&self, keys: &[Pubkey]) -> u64 {
        let accounts = self.rpc.get_multiple_accounts(keys).await.unwrap();
        accounts.iter().flatten().map(|a| a.lamports).sum()
    }

    async fn supply(&self, mint: &Pubkey) -> u64 {
        self.rpc
            .get_token_supply(mint)
            .await
            .expect("get_token_supply")
            .amount
            .parse()
            .expect("supply is u64")
    }

    async fn curve(&self, mint: &Pubkey) -> BondingCurve {
        self.client.fetch_bonding_curve(mint).await.unwrap()
    }

    /// The curve's base ATA balance (the v3 quoters price the post-completion leg on it).
    async fn curve_base_ata(&self, mint: &Pubkey, token_program: &Pubkey) -> u64 {
        let curve = pda::pump::bonding_curve(mint).0;
        token_balance(
            &self.rpc,
            &pda::associated_token(&curve, token_program, mint).0,
        )
        .await
    }

    /// The curve's quote ATA holds exactly its reserves, the post-completion
    /// leg's quote and the two fee buckets: the buyback slice left in the trade.
    async fn assert_curve_holds_reserves_and_buckets(&self, mint: &Pubkey) {
        let bc = self.curve(mint).await;
        let curve_ata =
            pda::associated_token(&pda::pump::bonding_curve(mint).0, &TP, &USDC_QUOTE_MINT).0;
        assert_eq!(
            token_balance(&self.rpc, &curve_ata).await,
            bc.real_quote_reserves + bc.post_complete_quote_in + bc.protocol_fees + bc.creator_fee,
            "curve quote ATA vs reserves + buckets: {bc:?}"
        );
    }

    /// `create_v2` a coin quoted in test USDC with the user as creator.
    async fn create_usdc_coin(&self) -> Pubkey {
        let mint = Keypair::new();
        let create = self.sdk.create_v2_instruction(
            mint.pubkey(),
            self.user.pubkey(),
            "Vthree",
            "VTHREE",
            "https://example.com/v3.json",
            self.user.pubkey(),
            USDC_QUOTE_MINT,
            TP,
            false,
            false,
            0,
        );
        self.send(400_000, vec![create], &[&mint]).await;
        let bc = self.curve(&mint.pubkey()).await;
        assert_eq!(bc.quote_mint, USDC_QUOTE_MINT);
        assert_eq!(bc.creator, self.user.pubkey());
        mint.pubkey()
    }
}

#[tokio::test]
#[ignore = "requires `cargo run --features local-validator --bin local-validator` running"]
async fn v3_trades_match_quotes_and_sweep_pays_the_creator_vault() {
    let env = Env::new(10_000 * USDC).await;
    let mint = env.create_usdc_coin().await;
    let global = env.client.fetch_global().await.unwrap();
    env.create_buyback_usdc_atas(&global.buyback_fee_recipients)
        .await;
    let buyback = PumpSdk::buyback_fee_recipient_from_pump_global(&global).unwrap();
    let fee_config = env.client.fetch_fee_config().await.unwrap();
    let bc = env.curve(&mint).await;
    let supply = env.supply(&mint).await;

    // buy_v3 at exactly the quoted cost (zero slippage).
    let amount = 500_000 * USDC;
    let base_ata = env.curve_base_ata(&mint, &T22).await;
    let quote = env
        .sdk
        .buy_quote_bonding_curve_v3_token_out(
            &global,
            &fee_config,
            &bc,
            supply,
            base_ata,
            amount,
            0,
        )
        .unwrap();
    let usdc_before = env.usdc().await;
    let buyback_before = env.usdc_of(&buyback).await;
    env.send(
        400_000,
        env.sdk.buy_v3_instructions(
            mint,
            bc.quote_mint,
            T22,
            TP,
            env.user.pubkey(),
            buyback,
            amount,
            quote.amount,
        ),
        &[],
    )
    .await;
    assert_eq!(usdc_before - env.usdc().await, quote.amount, "buy_v3 cost");
    assert_eq!(env.balance(&mint, &T22).await, amount);
    let bc = env.curve(&mint).await;
    let bucket_after_buy = bc.creator_fee;
    assert!(
        bucket_after_buy > 0 && bc.protocol_fees > 0,
        "v3 accrues on the curve: {bc:?}"
    );
    if global.buyback_basis_points > 0 {
        assert!(env.usdc_of(&buyback).await > buyback_before, "buyback paid");
    }
    env.assert_curve_holds_reserves_and_buckets(&mint).await;

    // sell_v3 at exactly the quoted proceeds.
    let quote = env
        .sdk
        .sell_quote_bonding_curve(&global, &fee_config, &bc, supply, amount, 0)
        .unwrap();
    let usdc_before = env.usdc().await;
    let buyback_before = env.usdc_of(&buyback).await;
    env.send(
        400_000,
        env.sdk.sell_v3_instructions(
            mint,
            bc.quote_mint,
            T22,
            TP,
            env.user.pubkey(),
            buyback,
            amount,
            quote.amount,
        ),
        &[],
    )
    .await;
    assert_eq!(
        env.usdc().await - usdc_before,
        quote.amount,
        "sell_v3 proceeds"
    );
    assert_eq!(env.balance(&mint, &T22).await, 0);
    if global.buyback_basis_points > 0 {
        assert!(env.usdc_of(&buyback).await > buyback_before, "buyback paid");
    }
    env.assert_curve_holds_reserves_and_buckets(&mint).await;

    // Sweep the creator bucket into the creator vault's quote ATA.
    let bc = env.curve(&mint).await;
    let bucket = bc.creator_fee;
    assert!(bucket > bucket_after_buy);
    let vault = pda::pump::creator_vault(&bc.creator).0;
    let vault_before = env.usdc_of(&vault).await;
    env.send(
        200_000,
        vec![env.sdk.sweep_creator_fee_instruction(
            env.user.pubkey(),
            mint,
            bc.creator,
            bc.quote_mint,
            TP,
        )],
        &[],
    )
    .await;
    assert_eq!(env.usdc_of(&vault).await - vault_before, bucket);
    assert_eq!(env.curve(&mint).await.creator_fee, 0);
}

#[tokio::test]
#[ignore = "requires `cargo run --features local-validator --bin local-validator` running"]
async fn pool_v2_trades_match_quotes_and_pool_sweep() {
    let env = Env::new(10_000 * USDC).await;
    let mint = NOT_CASHBACK_USDC_POOL_MINT;
    let pool = pda::pump_amm::canonical_pool(&mint, &USDC_QUOTE_MINT).0;
    let pool_state = env.client.fetch_pool(&pool).await.expect("fetch_pool");
    assert!(
        !pool_state.is_cashback_coin,
        "v2 trades refuse cashback coins"
    );
    assert_ne!(pool_state.coin_creator, Pubkey::default());
    let amm_global = env.client.fetch_amm_global_config().await.unwrap();
    env.create_buyback_usdc_atas(&amm_global.buyback_fee_recipients)
        .await;
    let buyback = PumpSdk::buyback_fee_recipient_from_amm_global(&amm_global).unwrap();
    let amm_fee_config = env.client.fetch_amm_fee_config().await.unwrap();
    let supply = env.supply(&mint).await;
    let base_reserve = token_balance(&env.rpc, &pool_state.pool_base_token_account).await;
    let quote_reserve = token_balance(&env.rpc, &pool_state.pool_quote_token_account).await;

    // buy_v2 (pool) of 0.1% of the pool at exactly the quoted cost.
    let amount = base_reserve / 1_000;
    let quote = env
        .sdk
        .buy_quote_amm_token_out(
            &amm_global,
            &amm_fee_config,
            AmmQuoteSource::Pool {
                pool: &pool_state,
                base_reserve,
                quote_reserve,
                base_mint_supply: supply,
            },
            amount,
            0,
        )
        .unwrap();
    assert!(
        quote.amount <= 5_000 * USDC,
        "pool too pricey for the test wallet"
    );
    let usdc_before = env.usdc().await;
    let buyback_before = env.usdc_of(&buyback).await;
    env.send(
        400_000,
        env.sdk.buy_amm_v2_instructions(
            &pool_state,
            T22,
            TP,
            env.user.pubkey(),
            buyback,
            amount,
            quote.amount,
        ),
        &[],
    )
    .await;
    assert_eq!(usdc_before - env.usdc().await, quote.amount, "buy_v2 cost");
    if amm_global.buyback_basis_points > 0 {
        assert!(env.usdc_of(&buyback).await > buyback_before, "buyback paid");
    }
    let pool_state = env.client.fetch_pool(&pool).await.unwrap();
    assert!(pool_state.creator_fees > 0 && pool_state.virtual_quote_reserves < 0);

    // sell_v2 of the same amount at exactly the quoted proceeds: the quote
    // must price against vault + (negative) virtual reserves now.
    let base_reserve = token_balance(&env.rpc, &pool_state.pool_base_token_account).await;
    let quote_reserve = token_balance(&env.rpc, &pool_state.pool_quote_token_account).await;
    let sell = env
        .sdk
        .sell_quote_amm(
            &amm_global,
            &amm_fee_config,
            AmmQuoteSource::Pool {
                pool: &pool_state,
                base_reserve,
                quote_reserve,
                base_mint_supply: supply,
            },
            amount,
            0,
        )
        .unwrap();
    let usdc_before = env.usdc().await;
    env.send(
        400_000,
        env.sdk.sell_amm_v2_instructions(
            &pool_state,
            T22,
            TP,
            env.user.pubkey(),
            buyback,
            amount,
            sell.amount,
        ),
        &[],
    )
    .await;
    assert_eq!(
        env.usdc().await - usdc_before,
        sell.amount,
        "sell_v2 proceeds"
    );
    assert_eq!(env.balance(&mint, &T22).await, 0);

    // Sweep the pool's creator bucket into the coin-creator vault authority's ATA.
    let pool_state = env.client.fetch_pool(&pool).await.unwrap();
    let bucket = pool_state.creator_fees;
    assert!(bucket > 0);
    let authority = pda::pump_amm::coin_creator_vault_authority(&pool_state.coin_creator).0;
    let before = env.usdc_of(&authority).await;
    env.send(
        200_000,
        vec![env
            .sdk
            .sweep_pool_creator_fee_instruction(env.user.pubkey(), &pool_state, TP)],
        &[],
    )
    .await;
    assert_eq!(env.usdc_of(&authority).await - before, bucket);
    assert_eq!(env.client.fetch_pool(&pool).await.unwrap().creator_fees, 0);
}

#[tokio::test]
#[ignore = "requires `cargo run --features local-validator --bin local-validator` running"]
async fn buy_exact_quote_in_v3_matches_quote_and_buys_past_completion() {
    let env = Env::new(1_000_000 * USDC).await;
    let mint = env.create_usdc_coin().await;
    let global = env.client.fetch_global().await.unwrap();
    env.create_buyback_usdc_atas(&global.buyback_fee_recipients)
        .await;
    let buyback = PumpSdk::buyback_fee_recipient_from_pump_global(&global).unwrap();
    let fee_config = env.client.fetch_fee_config().await.unwrap();
    let bc = env.curve(&mint).await;
    let supply = env.supply(&mint).await;

    // Spend 1% of the curve's virtual quote; the on-chain `min_tokens_out` is
    // the quoted token amount, so the send itself proves the exact-in math.
    let spend = bc.virtual_quote_reserves / 100;
    let base_ata = env.curve_base_ata(&mint, &T22).await;
    let quote = env
        .sdk
        .buy_quote_bonding_curve_v3_sol_in(&global, &fee_config, &bc, supply, base_ata, spend, 0)
        .unwrap();
    assert!(quote.amount > 0);
    // The program charges the shaved net plus ceil'd fees, which can undershoot
    // the budget by a unit per fee component; the v3 quote reports that charge.
    assert!(quote.input_amount_used <= spend && spend - quote.input_amount_used <= 2);
    let usdc_before = env.usdc().await;
    env.send(
        400_000,
        env.sdk.buy_exact_quote_in_v3_instructions(
            mint,
            bc.quote_mint,
            T22,
            TP,
            env.user.pubkey(),
            buyback,
            spend,
            quote.amount,
        ),
        &[],
    )
    .await;
    assert_eq!(env.balance(&mint, &T22).await, quote.amount);
    assert_eq!(usdc_before - env.usdc().await, quote.input_amount_used);
    env.assert_curve_holds_reserves_and_buckets(&mint).await;

    // Past the curve: on a non-mayhem curve a budget beyond the remaining
    // supply completes the curve and spends the rest on the post-completion
    // leg (priced on the pool the migration will create), in the same trade.
    let bc = env.curve(&mint).await;
    let remaining = bc.real_token_reserves;
    let spend = bc.virtual_quote_reserves * 4;
    let (quote, ixs) = env
        .client
        .build_buy_exact_quote_in_v3(&env.user.pubkey(), &mint, spend, 0)
        .await
        .unwrap();
    assert!(quote.amount > remaining, "quote {quote:?}");
    let held = env.balance(&mint, &T22).await;
    let usdc_before = env.usdc().await;
    env.send(400_000, ixs, &[]).await;
    assert_eq!(env.balance(&mint, &T22).await - held, quote.amount);
    assert_eq!(usdc_before - env.usdc().await, quote.input_amount_used);
    let bc = env.curve(&mint).await;
    assert!(bc.complete && bc.real_token_reserves == 0);
    assert_eq!(bc.post_complete_base_out, quote.amount - remaining);
    assert!(bc.post_complete_quote_in > 0, "{bc:?}");
    env.assert_curve_holds_reserves_and_buckets(&mint).await;
}

#[tokio::test]
#[ignore = "requires `cargo run --features local-validator --bin local-validator` running"]
async fn v3_trades_on_a_sol_curve_pay_lamports() {
    let env = Env::new(USDC).await;
    let mint = NOT_CASHBACK_SOL_CURVE_MINT;
    let global = env.client.fetch_global().await.unwrap();
    let fee_config = env.client.fetch_fee_config().await.unwrap();
    let bc = env.curve(&mint).await;
    assert_eq!(bc.quote_mint, Pubkey::default(), "legacy SOL curve");
    assert!(!bc.complete && !bc.is_cashback_coin);
    let supply = env.supply(&mint).await;

    // No wSOL setup: v3 moves lamports on a SOL curve and pays the buyback
    // slice to the recipient wallet itself. Zero slippage makes
    // `max_sol_cost` the exact quote, so the buy only lands if the math agrees
    // to the lamport. `build_buy_v3` fetches everything itself, including the
    // base token program from the mint's owner.
    let amount = 5_000_000 * USDC;
    let (supply_again, base_token_program) = env.client.fetch_mint(&mint).await.unwrap();
    assert_eq!(supply_again, supply);
    let (quote, ixs) = env
        .client
        .build_buy_v3(&env.user.pubkey(), &mint, amount, 0)
        .await
        .unwrap();
    let base_ata = env.curve_base_ata(&mint, &base_token_program).await;
    assert_eq!(
        quote,
        env.sdk
            .buy_quote_bonding_curve_v3_token_out(
                &global,
                &fee_config,
                &bc,
                supply,
                base_ata,
                amount,
                0
            )
            .unwrap()
    );
    let lamports_before = env.rpc.get_balance(&env.user.pubkey()).await.unwrap();
    let buyback_before = env.lamports_of(&global.buyback_fee_recipients).await;
    env.send(400_000, ixs, &[]).await;
    assert_eq!(env.balance(&mint, &base_token_program).await, amount);
    let spent = lamports_before - env.rpc.get_balance(&env.user.pubkey()).await.unwrap();
    // Cost plus the tx fee, the user's base ATA rent and the curve's realloc
    // rent (the first v3 trade grows an older curve to 166 bytes).
    assert!(
        spent >= quote.amount && spent < quote.amount + 10_000_000,
        "spent {spent}"
    );
    if global.buyback_basis_points > 0 {
        assert!(
            env.lamports_of(&global.buyback_fee_recipients).await > buyback_before,
            "buyback paid"
        );
    }
    let bc = env.curve(&mint).await;
    assert!(bc.protocol_fees > 0);

    let (sell, ixs) = env
        .client
        .build_sell_v3(&env.user.pubkey(), &mint, amount, 0)
        .await
        .unwrap();
    let lamports_before = env.rpc.get_balance(&env.user.pubkey()).await.unwrap();
    env.send(400_000, ixs, &[]).await;
    assert_eq!(env.balance(&mint, &base_token_program).await, 0);
    let received = env.rpc.get_balance(&env.user.pubkey()).await.unwrap() - lamports_before;
    // Proceeds minus the 5_000-lamport tx fee.
    assert_eq!(received + 5_000, sell.amount, "received {received}");
}

#[tokio::test]
#[ignore = "requires `cargo run --features local-validator --bin local-validator` running"]
async fn migrate_v2_after_a_post_completion_buy() {
    let env = Env::new(1_000_000 * USDC).await;
    let mint = env.create_usdc_coin().await;
    let global = env.client.fetch_global().await.unwrap();
    env.create_buyback_usdc_atas(&global.buyback_fee_recipients)
        .await;

    // Complete the curve with a buy past its remaining supply (no clamp).
    let bc = env.curve(&mint).await;
    let (quote, ixs) = env
        .client
        .build_buy_exact_quote_in_v3(&env.user.pubkey(), &mint, bc.virtual_quote_reserves * 4, 0)
        .await
        .unwrap();
    assert!(quote.amount > bc.real_token_reserves);
    env.send(400_000, ixs, &[]).await;
    let bc = env.curve(&mint).await;
    assert!(bc.complete && bc.post_complete_base_out > 0);
    let base_left = env.curve_base_ata(&mint, &T22).await;

    // Migration deposits what the leg left: the base ATA balance and the raise
    // plus the leg's quote (no migration fee on a token-quoted curve).
    // migrate_v2 creates the pool, its ATAs and the boost vault: ~400k+ CU.
    env.send(
        1_000_000,
        vec![env.sdk.migrate_v2_instruction(
            mint,
            USDC_QUOTE_MINT,
            env.user.pubkey(),
            global.withdraw_authority,
            T22,
            TP,
        )],
        &[],
    )
    .await;
    let pool_key = pda::pump_amm::canonical_pool(&mint, &USDC_QUOTE_MINT).0;
    let pool = env.client.fetch_pool(&pool_key).await.unwrap();
    assert_eq!(
        token_balance(&env.rpc, &pool.pool_base_token_account).await,
        base_left
    );
    // `init_boost` moves part of the quote into the boost vault and books it as
    // `virtual_quote_reserves`, so the effective quote is the full deposit.
    let quote_vault = token_balance(&env.rpc, &pool.pool_quote_token_account).await;
    assert_eq!(
        effective_quote_reserve(&pool, quote_vault).unwrap(),
        bc.real_quote_reserves + bc.post_complete_quote_in
    );
}
