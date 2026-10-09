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

- Bonding-curve builders: [`src/sdk/pump_v2.rs`](src/sdk/pump_v2.rs),
  v3 trades + creator sweep: [`src/sdk/pump_v3.rs`](src/sdk/pump_v3.rs)
- AMM builders (v1 + v2 + pool sweep): [`src/sdk/pump_amm_ix.rs`](src/sdk/pump_amm_ix.rs)
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
    &fee_config,
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
    pump_fee_config: &fee_config,
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
| `buy_quote_bonding_curve_v3_sol_in` / `buy_quote_bonding_curve_v3_token_out` | `src/sdk/mod.rs` | v3 buys, post-completion leg included (take the curve's base ATA balance) |
| `quote_multi_hop_swap` | `src/sdk/mod.rs` | `multi_hop_swap` route output from per-hop `RouteHop` state |
| `quote_trade` | `src/sdk/trade_tx.rs` | Auto-routed via `bonding_curve.complete` |

Quotes follow the on-chain fee resolution and take the pump-fees `FeeConfig`
(`fetch_fee_config` for curves, `fetch_amm_fee_config` for pools; the two are
different PDAs): the schedule is picked by quote mint, and a coin's own
`creator_fee_bps` (set at `create_v2`) replaces the schedule's creator rate
while `Global.creator_fee_configurable` is on. `Global`'s own bps fields play
no part in pricing. A v2 buy that would take more than a curve's remaining
tokens fills only the remainder and completes the curve (`partial_fill`), and
the v2 `*_bonding_curve_*` quotes clamp to match. A v3 buy on a non-mayhem
curve instead completes it and buys the rest from the pool the migration will
create, in the same trade; quote v3 buys with the `*_v3_*` quoters (v2 quotes
under-price them past the remaining supply). Pool quotes price against the vault plus
`Pool.virtual_quote_reserves` and pay sells out of the vault less the un-swept
fee buckets, as pump-amm does after v2 trades. A pump-amm disable flag
surfaces as `TradingDisabled`. To quote the first buy of a coin that does not exist yet, build the
curve with `PumpSdk::initial_bonding_curve` using the reserves from
`PumpSdk::initial_virtual_quote_reserves` (`Global` whitelist, then the
`quote-control` PDA via `fetch_quote_control`), or
`PumpSdk::pump_quote_initial_virtual_quote_reserves` for a coin quoted in
another pump coin.

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
| `create_v2_instruction` | [`examples/create_v2.rs`](examples/create_v2.rs), [`examples/create_v2_token2022_quote.rs`](examples/create_v2_token2022_quote.rs) |
| `create_v2_and_buy_instruction` | [`examples/create_v2_and_buy.rs`](examples/create_v2_and_buy.rs) |
| `create_coin_instructions` | — |

**Bonding curve v3 (`pump_v3`)** — [`src/sdk/pump_v3.rs`](src/sdk/pump_v3.rs).
One 17-account set, no fee recipients: the protocol fee (less its buyback
slice) and the creator fee accrue in the curve's `protocol_fees` /
`creator_fee` buckets until a permissionless sweep pays them out;
the buyback slice is paid in the trade to `buyback_fee_recipient`. Pricing
equals v2 up to the remaining supply; past it a buy takes the post-completion
leg, so use the v3 quoters. Cashback coins stay on v2.

| Builder | Example |
| --- | --- |
| `buy_v3_instruction` / `buy_v3_instructions` | [`examples/buy_v3.rs`](examples/buy_v3.rs) |
| `buy_exact_quote_in_v3_instruction[s]` | — |
| `sell_v3_instruction` / `sell_v3_instructions` | — |
| `sweep_creator_fee_instruction` | [`examples/sweep_creator_fee.rs`](examples/sweep_creator_fee.rs) |
| `AsyncPumpClient::build_buy_v3` / `build_buy_exact_quote_in_v3` / `build_sell_v3` | One call: fetch, quote with slippage, instructions |

The v3 builders work from keys alone: `(base_mint, quote_mint,
base_token_program, quote_token_program, user, buyback_fee_recipient, amount,
limit)`. Pass `Pubkey::default()` or wSOL as the quote of a SOL curve,
Token-2022 as the base program of a `create_v2` coin and SPL Token for a
legacy `create` coin (the mint account's owner). `buyback_fee_recipient` is a
listed `Global.buyback_fee_recipients` wallet
(`PumpSdk::buyback_fee_recipient_from_pump_global` draws one); the builder
passes the wallet itself on a SOL curve and its quote ATA on a token quote.
The program never creates that ATA: the plural `*_v3_instructions` prepend an
idempotent create (the user pays), the singular builders need it to exist. `fetch_curve_quote_state` returns all of that plus
the state the quoters need; the `build_*_v3` helpers use it.

Prepend `sweep_creator_fee_instruction` in the same transaction as any
`distribute_creator_fees*`, `update_fee_shares*` or `admin_cto` on a coin
that has seen a v3 trade; those fail with `CreatorFeesNotSwept` otherwise.
`distribute_creator_fees_v2_instructions` does this for you.

**AMM (`pump_amm`, post-graduation)** — [`src/sdk/pump_amm_ix.rs`](src/sdk/pump_amm_ix.rs):

| Builder | Example |
| --- | --- |
| `buy_amm_instruction` / `buy_amm_instructions` | [`examples/buy_amm.rs`](examples/buy_amm.rs) |
| `sell_amm_instruction` / `sell_amm_instructions` | [`examples/sell_amm.rs`](examples/sell_amm.rs) |
| `buy_amm_v2_instruction[s]` / `buy_exact_quote_in_amm_v2_instruction` / `sell_amm_v2_instruction[s]` | 17-account pool trades from a fetched `Pool` (the address is derived from its seeds) and a listed `GlobalConfig.buyback_fee_recipients` wallet, whose quote ATA (which must exist) takes the buyback slice in the trade; the rest of the protocol fee and the creator fee accrue in `Pool.protocol_fees` / `creator_fees`. Any pool but a cashback coin's (a mayhem pool takes no buyback slice, a non-pump pool pays the flat fees) |
| `multi_hop_swap_instruction[s]` | One exact-in route through pump pools and bonding curves (`MultiHopHop::pool` / `::curve`, all hops one direction; a buy starting on a SOL curve pays native SOL and a sell ending on one pays lamports to the wallet, the user's wSOL ATA there being a placeholder that must exist; mayhem curves are refused). The protocol fee is paid on the hop in the user's currency, creator / LP fees on the target hop; `buyback_fee_recipient` is listed by that protocol-leg venue and its ATA is in the user's currency. ~30k CU per hop (`MULTI_HOP_COMPUTE_UNITS`); routes over 3 hops need a v0 transaction plus a lookup table |
| `sweep_pool_creator_fee_instruction` | Pays `Pool.creator_fees` into the coin-creator vault authority's ATA; prepend before `admin_cto_pool`, fee-sharing setup and pump-fees `update_fee_shares(_v2)` (which fail with `CreatorFeesNotSwept` / `PoolCreatorFeesNotSwept` otherwise) |

**Auto-routed trade** — [`src/sdk/trade_tx.rs`](src/sdk/trade_tx.rs):

| Builder | Notes |
| --- | --- |
| `trade_tx_instructions` | Routes on `bonding_curve.complete`; wraps/unwraps wSOL for native-quote AMM trades |
| `trade_tx_instructions_with_venue` | Same, but the caller pins the `TradeVenue` |

**pump-fees** — [`src/sdk/pump_fees_ix.rs`](src/sdk/pump_fees_ix.rs):

| Builder | Notes |
| --- | --- |
| `update_fee_shares_instruction` / `update_fee_shares_v2_instruction` | Replace a coin's shareholders; the current shareholders (plus their quote ATAs on v2) are remaining accounts, and a complete curve's canonical pool goes last. Sweep first (`sweep_creator_fee_instruction` / `sweep_pool_creator_fee_instruction`) |

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

The `create_v2` builders take the `quote_token_program` owning the quote
mint (SPL Token or Token-2022; xStock mints are Token-2022) and a
`creator_fee_bps` (`0` = pump-fees schedule rate). For a non-SOL quote the
builder appends four remaining accounts: quote mint, the curve's quote ATA,
the quote token program, and the `quote-control` PDA, which the program
reads only when `Global` does not whitelist the mint. v2 trades create the
buyback recipient's quote ATA on-chain (the user pays).

To quote a coin in another pump coin Q, append
`PumpSdk::create_v2_pump_quote_accounts` (or `fetch_pump_quote_create`'s
`remaining_accounts`) and budget ~250k CU; the program derives the reserves
from Q's price (`Global.max_curve_depth` gates it, 0 disables).

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
| `fetch_pool` / `fetch_amm_global_config` / `fetch_amm_fee_config` | The pump-amm side of the same |
| `fetch_mint` / `fetch_curve_quote_state` | Mint supply + owning token program; everything a curve quote or v3 trade needs in one snapshot |
| `build_buy_v3` / `build_buy_exact_quote_in_v3` / `build_sell_v3` | Fetch, quote with slippage and build a v3 trade in one call |
| `discover_route` | Walk `out_mint`'s quote chain back to its root currency (or a `stop_at` coin) into a `MultiHopRoute`: open curves become curve hops, graduated ones their canonical pool |
| `fetch_multi_hop_quote_state` | One two-round batched fetch of a route's state (`MultiHopQuoteState`); `quote` / `instructions` on it reuse the snapshot |
| `build_multi_hop_swap` / `build_multi_hop_swap_from` / `build_multi_hop_swap_for_route` | Discover (from the root currency, or from a coin you hold), quote with slippage and build `multi_hop_swap` in one call; `_for_route` takes a known route |
| `fetch_pump_quote_create` | Derived reserves, depth and remaining accounts for a `create_v2` quoted in a pump coin |
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
| [`examples/create_v2_token2022_quote.rs`](examples/create_v2_token2022_quote.rs) | Standalone (env-configured) create + first buy with a Token-2022 / xStock quote mint, quote-control reserves, and a per-coin creator fee |
| [`examples/buy_amm.rs`](examples/buy_amm.rs) | AMM buy on a graduated coin via `buy_amm_instructions` |
| [`examples/sell_amm.rs`](examples/sell_amm.rs) | AMM buy → sell via `buy_amm_instructions` + `sell_amm_instructions` |
| [`examples/buy_v3.rs`](examples/buy_v3.rs) | v3 bonding-curve buy, fees left in the curve's buckets, buyback paid in the trade |
| [`examples/sweep_creator_fee.rs`](examples/sweep_creator_fee.rs) | Paying a curve's creator bucket into the creator vault |
| [`examples/multi_hop_swap.rs`](examples/multi_hop_swap.rs) | A coin quoted in another pump coin (`fetch_pump_quote_create`), then USDC → Q → C with `build_multi_hop_swap` |

## Features

- `client` — enables `AsyncPumpClient` and the `solana-sdk` / `solana-client`
  dependencies. Required to call `fetch_global` / `fetch_bonding_curve` and
  the transaction-building helpers.
- `local-validator` — implies `client`; builds the `local-validator`,
  `clone_devnet_accounts` and `airdropusdc` binaries, the examples and the
  integration tests (`just test`). Building it needs libclang for RocksDB
  (`LIBCLANG_PATH` and `DYLD_FALLBACK_LIBRARY_PATH` set to
  `/Library/Developer/CommandLineTools/usr/lib` on macOS). If another local
  node holds 8899 / 9000, start the validator with `PUMP_LOCAL_RPC_PORT` /
  `PUMP_LOCAL_FAUCET_PORT` and point the tests and examples at it with
  `PUMP_LOCAL_RPC=http://127.0.0.1:<port>`. `just idls` refreshes `idls/` and
  `artifacts/` from a `pump-programs-monorepo` checkout.

Without `client`, the SDK still exposes every `*_instruction` /
`*_instructions` builder and every quoter — only the RPC wrapper is gated.
The base crate (no features) only needs `anchor-lang` and `solana-program`,
so it stays buildable inside on-chain programs that depend on the SDK for
PDA / account-meta derivation.

## CPI from another program

If you want to CPI into pump's `buy_v2` / `sell_v2` from your own Anchor
program and reuse this SDK to derive the account metas, see
[`CPI_README.md`](CPI_README.md).
