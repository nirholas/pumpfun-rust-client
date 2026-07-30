# pump-rust-client

Rust SDK for the `pump` and `pump_amm` Solana programs. Three things in one
crate:

1. **Instruction builders** for buy / sell / create on both the bonding
   curve (`pump`) and the AMM (`pump_amm`), with auto-routing across the
   two.
2. **Quoting** for both venues, with slippage applied — drives UI prices
   and the `*_threshold` arguments that the trade builders need.
3. **Transaction building** ([`AsyncPumpClient`](src/async_client.rs),
   behind the `client` feature): fetches on-chain state, prepends
   compute-budget, signs, simulates, and sends.

Canonical sources:

- Bonding-curve builders: [`src/sdk/pump_v2.rs`](src/sdk/pump_v2.rs)
- AMM builders: [`src/sdk/pump_amm_ix.rs`](src/sdk/pump_amm_ix.rs)
- Auto-routed trade + quote: [`src/sdk/trade_tx.rs`](src/sdk/trade_tx.rs)
- Quote helpers: [`src/sdk/mod.rs`](src/sdk/mod.rs)
- RPC wrapper: [`src/async_client.rs`](src/async_client.rs)
- Runnable end-to-end code: [`examples/`](examples/)

## Quickstart — buy

Adapted from [`examples/buy_v2.rs`](examples/buy_v2.rs):

```rust
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::signature::{Keypair, Signer};

use pump_rust_client::{constants, AsyncPumpClient, PumpSdk};

let sdk = PumpSdk::new();
let client = AsyncPumpClient::new(rpc.clone()); // requires the `client` feature

let global = client.fetch_global().await?;
let bonding_curve = client.fetch_bonding_curve(&mint).await?;

// `buy_v2_instructions` returns idempotent ATA creates + the trade ix.
let mut ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(400_000)];
ixs.extend(
    sdk.buy_v2_instructions(
        &global,
        &bonding_curve,
        mint,
        constants::SPL_TOKEN_PROGRAM_ID, // quote token program
        user.pubkey(),
        300_000_000,        // base tokens to buy
        LAMPORTS_PER_SOL,   // max quote spent
    )
    .expect("buy_v2_instructions"),
);
```

The other examples follow the same shape:
[`examples/sell_v2.rs`](examples/sell_v2.rs),
[`examples/create_v2_and_buy.rs`](examples/create_v2_and_buy.rs),
[`examples/buy_amm.rs`](examples/buy_amm.rs), and
[`examples/sell_amm.rs`](examples/sell_amm.rs).

## Quoting

Every trade builder takes a `*_threshold` (max quote spent on a buy, min
quote out on a sell). The quote helpers compute that threshold from a
slippage in basis points:

```rust
use pump_rust_client::PumpSdk;

let sdk = PumpSdk::new();
let global = client.fetch_global().await?;
let fee_config = client.fetch_fee_config().await?;
let bonding_curve = client.fetch_bonding_curve(&mint).await?;

let quote = sdk.buy_quote_bonding_curve_sol_in(
    &global,
    Some(&fee_config),
    &bonding_curve,
    mint_supply,        // base mint supply
    LAMPORTS_PER_SOL,   // sol_amount in
    100,                // 1% slippage
)?;
// quote.amount    — tokens out at the current curve
// quote.min_out   — slippage-protected floor; pass to sell builders
// quote.max_input — slippage-protected ceiling; pass to buy builders
```

For code that doesn't know yet whether the curve has graduated, use the
auto-routed `quote_trade` — it dispatches to bonding-curve or AMM math
based on `bonding_curve.complete`, mirroring `trade_tx_instructions`:

```rust
use pump_rust_client::{PumpSdk, TradeQuoteParams};

let quote = sdk.quote_trade(TradeQuoteParams {
    is_buy: true,
    base_amount: 300_000_000,
    slippage_bps: 100,
    base_mint_supply: mint_supply,
    pump_global: &global,
    pump_fee_config: Some(&fee_config),
    bonding_curve: &bonding_curve,
    pump_pool: None, // Some(PumpPoolQuoteCtx { … }) required when curve.complete
}); // returns None if the curve is complete and pump_pool was not supplied
```

| Quote builder | Source | Notes |
| --- | --- | --- |
| `buy_quote_bonding_curve_sol_in` | `src/sdk/mod.rs` | SOL in → tokens out |
| `buy_quote_bonding_curve_token_out` | `src/sdk/mod.rs` | Tokens out → SOL needed |
| `sell_quote_bonding_curve` | `src/sdk/mod.rs` | Tokens in → SOL out |
| `buy_quote_amm_sol_in` / `buy_quote_amm_token_out` | `src/sdk/mod.rs` | AMM equivalents |
| `sell_quote_amm` | `src/sdk/mod.rs` | AMM sell |
| `quote_trade` | `src/sdk/trade_tx.rs` | Auto-routed via `bonding_curve.complete` |

## AMM pricing: `virtual_quote_reserves`

The PumpSwap `Pool` account carries a `virtual_quote_reserves` field. Every
AMM quote, spot price, price-impact figure, and market cap prices against the
**effective** quote reserve:

```text
effective_quote_reserve = pool_quote_token_account.amount + pool.virtual_quote_reserves
```

The base side is unchanged: base reserves are still the raw
`pool_base_token_account.amount`.

Boost pools carry a non-zero value from 2026-07-20. Non-boost pools, and every
pool written before the field shipped, carry `0`, where `effective == raw` and
quotes are byte-identical to earlier releases.
`AccountWrapper` zero-pads short buffers, so a pool account that predates the
field decodes as `0` rather than failing.

**Pass the raw balance and the virtual figure separately.** The math sums them
internally. Pre-summing double-counts:

```rust
use pump_rust_client::{AmmQuoteSource, PumpSdk};

let quote = sdk.sell_quote_amm(
    &global_config,
    Some(&fee_config),
    AmmQuoteSource::Pool {
        pool: &pool,
        base_reserve,                                       // raw base vault balance
        quote_reserve,                                      // RAW quote vault balance
        virtual_quote_reserves: pool.virtual_quote_reserves, // added internally
        base_mint_supply,
    },
    token_amount,
    100,
)?;
```

```rust
// WRONG: double-counts the virtual figure.
quote_reserve: quote_reserve + pool.virtual_quote_reserves as u64,
virtual_quote_reserves: pool.virtual_quote_reserves,

// WRONG: prices a boost pool off the raw vault balance.
virtual_quote_reserves: 0,
```

`AmmQuoteSource::Pool` and `PumpPoolQuoteCtx` both require the field, so the
compiler rejects a call site that has not been updated rather than letting it
silently misprice. The fee-less primitives (`math::amm::sell_quote`,
`buy_token_quote_with_sol`, `sell_token_quote_with_sol`, `validate_market_cap`)
likewise take it as a distinct argument. `math::amm::effective_quote_reserve`
exposes the sum directly for hand-rolled math, and returns
`QuoteError::EmptyReserves` when the effective reserve is zero or negative.

A pool whose raw quote vault is empty but whose virtual reserve is non-zero is
tradable. Liquidity and depth checks must be judged on effective reserves, or
a live pool gets rejected as empty.

### The field is `i128` and signed

`virtual_quote_reserves` is `i128` (16 bytes), matching the on-chain IDL, and
it is **signed**. Two consequences:

- Declaring it `u64` misreads the field width and shifts every byte after it.
- A negative value is legitimate and makes the pool **shallower** than its raw
  vault balance suggests. Never `as u64` it and never subtract it with
  unsigned arithmetic: in debug that panics, and in release it wraps to a
  near-`u64::MAX` reserve, which a sizing routine reads as bottomless
  liquidity. That is the worst available failure mode.

`effective_quote_reserve` promotes the raw `u64` balance to `i128`, adds with
`checked_add`, and converts back with `u64::try_from`, so a non-positive
result surfaces as `QuoteError::EmptyReserves` (an untradable pool) rather
than as a plausible-looking number.

The `BuyEvent` and `SellEvent` logs carry an appended `virtual_quote_reserves`
too (plus `can_boost` and `base_supply`). Existing Borsh decoders still read
the earlier fields correctly, but an indexer reconstructing reserves from the
event stream must read the new field to price correctly.

Note that `BondingCurve::virtual_quote_reserves` (pre-graduation, pump program)
and `Pool::virtual_quote_reserves` (post-graduation AMM) are different values
that share a name. The bonding-curve field is part of the curve's own
constant-product formula and is unrelated to effective AMM reserves.

## Instruction reference

All builders live on `PumpSdk`. Prefer the `*_instructions` (plural)
variants — they prepend the idempotent ATA creates the user needs. The
singular `*_instruction` returns only the trade ix, for callers that manage
ATAs themselves.

**Bonding curve (`pump_v2`)** — [`src/sdk/pump_v2.rs`](src/sdk/pump_v2.rs):

| Builder | Example |
| --- | --- |
| `buy_v2_instruction` / `buy_v2_instructions` | [`examples/buy_v2.rs`](examples/buy_v2.rs) |
| `sell_v2_instruction` / `sell_v2_instructions` | [`examples/sell_v2.rs`](examples/sell_v2.rs) |
| `buy_exact_quote_in_v2_instruction[s]` | — |
| `create_v2_instruction` | [`examples/create_v2.rs`](examples/create_v2.rs) |
| `create_v2_and_buy_instruction` | [`examples/create_v2_and_buy.rs`](examples/create_v2_and_buy.rs) |
| `create_coin_instructions` | — |

**AMM (`pump_amm`, post-graduation)** — [`src/sdk/pump_amm_ix.rs`](src/sdk/pump_amm_ix.rs):

| Builder | Example |
| --- | --- |
| `buy_amm_instruction` / `buy_amm_instructions` | [`examples/buy_amm.rs`](examples/buy_amm.rs) |
| `sell_amm_instruction` / `sell_amm_instructions` | [`examples/sell_amm.rs`](examples/sell_amm.rs) |

**Auto-routed trade** — [`src/sdk/trade_tx.rs`](src/sdk/trade_tx.rs):

| Builder | Notes |
| --- | --- |
| `trade_tx_instructions` | Routes on `bonding_curve.complete`; wraps/unwraps wSOL for native-quote AMM trades |
| `trade_tx_instructions_with_venue` | Same, but the caller pins the `TradeVenue` |

The auto-routed builders take `TradeTxParams` / `TradeTxWithVenueParams`
(both re-exported at the crate root, see [`src/lib.rs`](src/lib.rs)) and
handle ATA creation and wSOL wrap/unwrap themselves — the caller only
assembles compute-budget plus signers.

The bonding-curve `*_instructions` plural builders take a fetched
[`Global`](src/state.rs) and [`BondingCurve`](src/state.rs) so the SDK can
pick the correct fee recipients and quote layout.
`create_v2_and_buy_instruction` synthesises a bonding-curve preview
internally from the supplied `quote_mint` (`Pubkey::default()` → wSOL),
so a fetch is not required before the curve exists.

## Building, signing, and sending — `AsyncPumpClient`

With the `client` feature, [`AsyncPumpClient`](src/async_client.rs) wraps
`solana_client::nonblocking::rpc_client::RpcClient` and handles the full
buy lifecycle:

```rust
use std::sync::Arc;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{native_token::LAMPORTS_PER_SOL, signature::{Keypair, Signer}};
use pump_rust_client::{constants, AsyncPumpClient, ComputeBudget};

let rpc = Arc::new(RpcClient::new(rpc_url));
let client = AsyncPumpClient::new(rpc);
let user = Keypair::new();

// 1. Fetch live state.
let global = client.fetch_global().await?;
let bonding_curve = client.fetch_bonding_curve(&mint).await?;

// 2. Build instructions via the inner SDK.
let ixs = client.sdk().buy_v2_instructions(
    &global,
    &bonding_curve,
    mint,
    constants::SPL_TOKEN_PROGRAM_ID,
    user.pubkey(),
    300_000_000,
    LAMPORTS_PER_SOL,
).expect("buy_v2_instructions");

// 3. Sign — compute-budget instructions are prepended automatically.
let tx = client.build_transaction(
    &ixs,
    &user.pubkey(),
    &[&user],
    Some(ComputeBudget {
        units: Some(400_000),
        micro_lamports_per_unit: Some(1_000),
    }),
).await?;

// 4. Simulate, then send.
let _sim = client.simulate_transaction(&tx).await?;
let sig = client.send_and_confirm_transaction(&tx).await?;
```

| Helper | Purpose |
| --- | --- |
| `fetch_global` / `fetch_fee_config` / `fetch_bonding_curve` | Load the on-chain state needed by builders and quoters |
| `fetch_buy_state` / `fetch_sell_state` | One round-trip fetch of bonding curve + user ATA |
| `fetch_global_volume_accumulator` / `fetch_user_volume_accumulator` | Volume accumulators (cashback) |
| `get_creator_vault_balance` | Spendable lamports above rent |
| `latest_blockhash` | Recent blockhash at the client's commitment |
| `build_transaction` / `build_transaction_with_blockhash` | Sign with optional `ComputeBudget` prepended |
| `simulate_transaction` | Preflight against the RPC |
| `send_transaction` / `send_and_confirm_transaction` | Submit to the cluster |
| `sdk()` / `rpc()` | Borrow the underlying `PumpSdk` / `RpcClient` |

## Account initialization

The trade instructions pull in 25+ accounts. **You do not need to pre-create
most of them** — the SDK derives every PDA and ATA inside
`V2TradeAccounts::derive` (see `src/sdk/pump_v2.rs`). The only ATAs the
caller is responsible for are the user's own base/quote ATAs, and those are
auto-prepended by the `*_instructions` variants.

The exact rules for which user ATAs are created when are encoded in
`fn user_trade_atas` in [`src/sdk/pump_v2.rs`](src/sdk/pump_v2.rs).
Summary:

- **Base ATA**: created on buy, skipped on sell (the user already holds the
  base balance to spend).
- **Quote ATA**: created only when the curve's `quote_mint` is non-default
  (i.e. non-wSOL curves). Legacy SOL-only curves skip the quote ATA.

`trade_tx_instructions` additionally handles wSOL wrap/unwrap for
native-quote AMM trades, so callers using the auto-routed path do not need
a separate setup transaction.

If you need different behavior (e.g. you manage user ATAs upstream), call
the singular `*_instruction` builders directly.

## Examples

| Example | Demonstrates |
| --- | --- |
| [`examples/create_v2.rs`](examples/create_v2.rs) | Creating a coin with `create_v2_instruction` (mint signer required) |
| [`examples/buy_v2.rs`](examples/buy_v2.rs) | Bonding-curve buy with `buy_v2_instructions` |
| [`examples/sell_v2.rs`](examples/sell_v2.rs) | Buy → sell cycle with `buy_v2_instructions` + `sell_v2_instructions` |
| [`examples/create_v2_and_buy.rs`](examples/create_v2_and_buy.rs) | Atomic create + buy via `create_v2_and_buy_instruction` |
| [`examples/buy_amm.rs`](examples/buy_amm.rs) | AMM buy on a graduated coin via `buy_amm_instructions` |
| [`examples/sell_amm.rs`](examples/sell_amm.rs) | AMM buy → sell via `buy_amm_instructions` + `sell_amm_instructions` |

## Features

- `client` — enables `AsyncPumpClient` and the `solana-sdk` / `solana-client`
  dependencies. Required to call `fetch_global` / `fetch_bonding_curve` and
  the transaction-building helpers.

Without `client`, the SDK still exposes every `*_instruction` /
`*_instructions` builder and every quoter — only the RPC wrapper is gated.
The base crate (no features) only needs `anchor-lang` and `solana-program`,
so it stays buildable inside on-chain programs that depend on the SDK for
PDA / account-meta derivation.

## CPI from another program

If you want to CPI into pump's `buy_v2` / `sell_v2` from your own Anchor
program and reuse this SDK to derive the account metas, see
[`CPI_README.md`](CPI_README.md).

## Documentation

Full documentation site: **https://nirholas.github.io/pumpfun-rust-client/**

- [Getting started](docs/getting-started.md) covers install and first run.
- [Examples](docs/examples.md) has copy-paste snippets.
