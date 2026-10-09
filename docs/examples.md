# pumpfun-rust-client examples

Every snippet assumes the `client` feature (see [Getting started](./getting-started.md)) and this setup:

```rust
use std::sync::Arc;

use pump_rust_client::AsyncPumpClient;
use solana_client::nonblocking::rpc_client::RpcClient;

let rpc = Arc::new(RpcClient::new(rpc_url));
let client = AsyncPumpClient::new(rpc);
let sdk = client.sdk();
```

`user` is the wallet `Pubkey`, `mint` the coin's mint. Every `build_*` helper
returns `(Quote, Vec<Instruction>)`: sign the instructions with
`client.build_transaction(&ixs, &user, &[&keypair], None)` and send with
`client.send_and_confirm_transaction(&tx)`.

## v3 bonding-curve trades

v3 trades leave the protocol and creator fees in the curve's buckets and pay
the buyback slice in the trade.

```rust
// Buy an exact token amount. Past the remaining supply the quote includes
// the post-completion leg, so max_input covers both parts.
let (quote, ixs) = client.build_buy_v3(&user, &mint, 1_000_000_000, 100).await?;

// Spend an exact quote amount. quote.input_amount_used is what the program charges.
let (quote, ixs) = client
    .build_buy_exact_quote_in_v3(&user, &mint, 500_000_000, 100)
    .await?;

// Sell.
let (quote, ixs) = client.build_sell_v3(&user, &mint, 1_000_000_000, 100).await?;
```

## Quote a v3 buy without sending it

```rust
let s = client.fetch_curve_quote_state(&mint).await?;

let by_tokens = sdk.buy_quote_bonding_curve_v3_token_out(
    &s.global, &s.fee_config, &s.bonding_curve,
    s.mint_supply, s.base_ata_amount,
    900_000_000_000_000, // more than the curve has left
    100,
)?;
// by_tokens.amount: total quote cost, curve part plus pool-to-be part
// by_tokens.max_input: pass as max_sol_cost

let by_quote = sdk.buy_quote_bonding_curve_v3_sol_in(
    &s.global, &s.fee_config, &s.bonding_curve,
    s.mint_supply, s.base_ata_amount,
    100_000_000_000, // 100 SOL
    100,
)?;
// by_quote.amount: tokens out; by_quote.min_out: pass as min_tokens_out
```

Mayhem curves keep the v2 partial fill: the quote stops at
`bonding_curve.real_token_reserves`.

## Multi-hop swaps

`build_multi_hop_swap` discovers the route to `out_mint` (for example USDC to a
pump coin Q to a coin quoted in Q), quotes every hop and returns one
`multi_hop_swap` instruction. Give it the multi-hop compute budget:

```rust
use pump_rust_client::{constants, ComputeBudget};

let (quote, ixs) = client
    .build_multi_hop_swap(&user, &out_mint, true, 10_000_000, 100)
    .await?;
let tx = client
    .build_transaction(
        &ixs,
        &user,
        &[&keypair],
        Some(ComputeBudget {
            units: Some(constants::pump_amm::MULTI_HOP_COMPUTE_UNITS),
            micro_lamports_per_unit: None,
        }),
    )
    .await?;
```

A mayhem-mode curve cannot be a hop: the quote fails with
`QuoteError::VenueNotSupported`, as the program would with
`MultiHopMayhemCurveNotSupported` (error 6108).

## Create a coin quoted in another pump coin

```rust
// Q's curve (and its pool, once migrated) price the new coin's initial reserves.
let q = client.fetch_pump_quote_create(&quote_mint).await?;
```

See `examples/multi_hop_swap.rs` for the full create-then-route flow and
`examples/create_v2_token2022_quote.rs` for Token-2022 quote mints.

## Fee sweeps

v3 and PumpSwap v2 trades accrue fees in buckets. Sweep them before any
instruction that refuses a full bucket:

```rust
// Bonding curve: pay the creator bucket into the creator vault.
let ix = sdk.sweep_creator_fee_instruction(user, mint, creator, quote_mint, quote_token_program);

// PumpSwap: pay the pool's creator bucket out.
let pool_state = client.fetch_pool(&pool).await?;
let ix = sdk.sweep_pool_creator_fee_instruction(user, &pool_state, quote_token_program);
```

`distribute_creator_fees_v2_instructions` prepends the curve sweep for you.

## PumpSwap quotes with signed virtual reserves

Pass the raw vault balances; the quoter adds the signed
`Pool.virtual_quote_reserves` and pays sells out of the vault less the fee buckets:

```rust
use pump_rust_client::AmmQuoteSource;

let pool_state = client.fetch_pool(&pool).await?;
let quote = sdk.buy_quote_amm_sol_in(
    &client.fetch_amm_global_config().await?,
    &client.fetch_amm_fee_config().await?,
    AmmQuoteSource::Pool {
        pool: &pool_state,
        base_reserve,      // raw base vault balance
        quote_reserve,     // RAW quote vault balance
        base_mint_supply,
    },
    1_000_000_000,
    100,
)?;
```

## Runnable programs

The repository's `examples/` directory holds end-to-end programs that run
against the local validator (`just local-validator`, then
`just example-one <name>`): `create_v2`, `create_v2_and_buy`, `buy_v2`,
`sell_v2`, `buy_amm`, `sell_amm`, `buy_v3`, `sweep_creator_fee`,
`multi_hop_swap`, `claim_cashback_v2`, `collect_creator_fee_v2`,
`collect_coin_creator_fee`, `distribute_creator_fees_v2`,
`transfer_creator_fees_to_pump_v2` and the standalone
`create_v2_token2022_quote`.
