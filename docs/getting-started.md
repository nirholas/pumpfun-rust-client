# Getting started with pumpfun-rust-client

`pump-rust-client` is a Rust SDK for the pump bonding-curve program and the
PumpSwap (`pump_amm`) program on Solana. This repository tracks the official
`pump-rust-client` crate (currently 0.4.0) and the IDLs published in
pump-public-docs. One crate gives you:

1. **Instruction builders** for every trade generation on both venues: v2 and
   v3 bonding-curve buys and sells, PumpSwap v1 and v2 trades,
   `multi_hop_swap` routes, `create_v2` (SOL, USDC, Token-2022 and pump-coin
   quote mints) and the creator / pool fee sweeps.
2. **Quoting** that follows the on-chain math exactly, including the v3
   synthetic migration and the signed `Pool.virtual_quote_reserves`.
3. **An RPC wrapper** (`AsyncPumpClient`, `client` feature) that fetches the
   state a trade needs, quotes it and returns ready-to-sign instructions.

## Requirements

- Rust 1.89 or newer (the repository pins it in `rust-toolchain.toml`).
- OpenSSL headers for the `client` feature (`libssl-dev` on Debian or
  Ubuntu). Point `OPENSSL_DIR` at a local copy if you cannot install packages
  system-wide.
- libclang only if you build the `local-validator` feature (RocksDB).

## Install

Add the crate as a git dependency with the `client` feature, which enables
`AsyncPumpClient`:

```toml
[dependencies]
pump-rust-client = { git = "https://github.com/nirholas/pumpfun-rust-client", branch = "main", features = ["client"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
solana-client = "2.1.21"
solana-sdk = "2.1.21"
```

No features is the right choice for on-chain programs and for code that only
needs instruction builders, quoters and PDAs: the base crate depends on
`anchor-lang` and `solana-program` alone.

```toml
pump-rust-client = { git = "https://github.com/nirholas/pumpfun-rust-client", branch = "main" }
```

## Build and test the repository

```bash
git clone https://github.com/nirholas/pumpfun-rust-client.git
cd pumpfun-rust-client
cargo build --locked
cargo test --locked
```

`cargo test` runs the unit tests, which cover every quote path (including
`src/math/synthetic_migration_tests.rs`, the v3 synthetic-migration and signed
`virtual_quote_reserves` checks against the published formulas). The
integration tests under `tests/` need a local validator loaded with the pump
programs; run them with `just local-validator` in one terminal and `just test`
in another.

## Your first quote

Quote a v3 buy that spends 1 SOL on a bonding-curve coin. `fetch_curve_quote_state`
pulls the `Global`, the pump-fees `FeeConfig`, the curve, the mint supply and the
curve's base ATA balance in one round trip:

```rust
use std::sync::Arc;

use pump_rust_client::AsyncPumpClient;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{native_token::LAMPORTS_PER_SOL, pubkey::Pubkey};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc = Arc::new(RpcClient::new("https://api.mainnet-beta.solana.com".to_string()));
    let client = AsyncPumpClient::new(rpc);
    let mint: Pubkey = std::env::args()
        .nth(1)
        .ok_or("usage: first_quote <mint>")?
        .parse()?;

    let s = client.fetch_curve_quote_state(&mint).await?;
    let quote = client.sdk().buy_quote_bonding_curve_v3_sol_in(
        &s.global,
        &s.fee_config,
        &s.bonding_curve,
        s.mint_supply,
        s.base_ata_amount,
        LAMPORTS_PER_SOL,
        100, // 1% slippage
    )?;
    println!(
        "1 SOL buys {} base units (min {}), charging {} lamports",
        quote.amount, quote.min_out, quote.input_amount_used
    );
    Ok(())
}
```

If the buy is large enough to empty the curve, the quote already includes
the part bought from the pool the migration will create. See the README's
"Synthetic migration" section for the formulas.

## Next steps

- [Examples](./examples.md) has snippets for every trade and quote path.
- The [README](https://github.com/nirholas/pumpfun-rust-client#readme) is the complete reference.
- [`CPI_README.md`](https://github.com/nirholas/pumpfun-rust-client/blob/main/CPI_README.md) covers CPI from your own Anchor program.
- Found a problem? [Open an issue](https://github.com/nirholas/pumpfun-rust-client/issues).
