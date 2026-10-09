//! Fetches the admin-initialized PDAs the SDK needs (`Global`, `FeeConfig`,
//! mayhem `global-params`/`sol-vault`, the pump trade ALT, etc.) from a
//! Solana cluster and dumps them to `artifacts/accounts_to_load.zst` — the
//! file the local validator boots with via
//! `src/bin/local_validator/main.rs`.
//!
//! Network selection (which ALT / fixture layout to use):
//!   - `--network devnet|mainnet` (CLI), or `PUMP_NETWORK` — defaults to devnet.
//!
//! RPC endpoint precedence:
//!   1. `--rpc-url <URL>` or a single positional `<RPC_URL>` (same meaning)
//!   2. `PUMP_CLONE_RPC`
//!   3. Public Solana cluster RPC for the selected network
//!
//! Per-PDA mainnet override: entries in `fixed_pdas` carry a
//! `fetch_from_mainnet` bool. When set, that PDA is fetched from mainnet even
//! on a devnet run via a separate `RpcClient`. The mainnet endpoint comes from
//! `PUMP_CLONE_MAINNET_RPC` (if set) or the public mainnet-beta URL.
//!
//! A `.env` in the current directory is loaded when present (`dotenvy`), so
//! `PUMP_CLONE_RPC` / `PUMP_CLONE_MAINNET_RPC` / `PUMP_NETWORK` can live there.
//!
//! Run once before `cargo run --features local-validator --bin local-validator`:
//!   `cargo run --features local-validator --bin clone_devnet_accounts -- --help`

use std::collections::HashMap;
use std::fs;

use anchor_lang::system_program;
use anchor_lang::AccountSerialize;
use anchor_spl::token::spl_token;
use solana_client::rpc_client::RpcClient;
use solana_program::program_option::COption;
use solana_program::program_pack::Pack;
use solana_sdk::account::Account;
use solana_sdk::address_lookup_table::state::{AddressLookupTable, LookupTableMeta};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::rent::Rent;
use spl_token_2022::extension::PodStateWithExtensionsMut;
use spl_token_2022::pod::{PodAccount, PodCOption, PodMint};

use pump_rust_client::accounts::pump_amm::decode_pool;
use pump_rust_client::accounts::{decode_bonding_curve, decode_global, decode_sharing_config};
use pump_rust_client::constants;
use pump_rust_client::pda;
use pump_rust_client::state::pump_amm::Pool;
use pump_rust_client::state::{Global, QuoteControl};

#[path = "../../../tests/common/fixtures.rs"]
mod fixtures;
use fixtures::{FixtureMint, FIXTURE_MINTS};

const OUT_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/artifacts/accounts_to_load.zst"
);

/// Canonical mainnet Raydium AMM v4 SOL/USDC pool and every sibling
/// account the swap CPI may touch.
///
/// The `pump_stables_router` program (see `bonding_curve_v2.rs` /
/// `pump_swap_v2.rs`) CPIs into this pool for the SOL <-> USDC leg of any
/// USDC-quoted trade. We always fetch from mainnet because (a) devnet has
/// no equivalent pool and (b) the router pins the v4 mainnet authority,
/// so a devnet pool would fail on-chain anyway.
///
/// The Raydium V4 `SwapBaseIn` instruction takes 17 accounts (open-orders
/// variant, no target-orders slot) covering the AMM side, the OpenBook
/// market, and the user. To keep the local validator self-contained we
/// clone every one of them so the CPI works whether it falls into the
/// AMM-only or hybrid-orderbook path. Values come from Raydium's public
/// liquidity-pool list for pool `58oQChx4…`.
#[allow(unused_imports, dead_code)] // entries kept for the commented-out ALT extras
mod raydium_sol_usdc {
    use super::Pubkey;
    use pump_rust_client::constants::{
        pump::PROGRAM_ID as PUMP_PROGRAM_ID,
        pump_agent_payments::PROGRAM_ID as PUMP_AGENT_PAYMENTS_PROGRAM_ID,
        pump_amm::PROGRAM_ID as PUMP_AMM_PROGRAM_ID, FEE_PROGRAM_ID, MAYHEM_PROGRAM_ID,
        MPL_TOKEN_METADATA_PROGRAM_ID,
    };
    use pump_rust_client::pda;
    use solana_program::pubkey;

    // AMM side
    pub const POOL: Pubkey = pubkey!("58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2");
    pub const AMM_AUTHORITY: Pubkey = pubkey!("5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1");
    pub const OPEN_ORDERS: Pubkey = pubkey!("HRk9CMrpq7Jn9sh7mzxE8CChHG8dneX9p475QKz4Fsfc");
    pub const TARGET_ORDERS: Pubkey = pubkey!("CZza3Ej4Mc58MnxWA385itCC9jCo3L1D7zc3LKy1bZMR");
    pub const WSOL_VAULT: Pubkey = pubkey!("DQyrAcCrDXQ7NeoqGgDCZwBvWDcYmFCjSb9JtteuvPpz");
    pub const USDC_VAULT: Pubkey = pubkey!("HLmqeL62xR1QoZ1HKKbXRrdN1p3phKpxRMb2VVopvBBz");

    pub const USDC_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
    pub const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

    pub fn alt_extras() -> Vec<Pubkey> {
        vec![
            // POOL,
            // AMM_AUTHORITY,
            // OPEN_ORDERS,
            // TARGET_ORDERS,
            // WSOL_VAULT,
            // USDC_VAULT,
            // WSOL_MINT,
            // USDC_MINT,
            // PUMP_PROGRAM_ID,
            // PUMP_AMM_PROGRAM_ID,
            // FEE_PROGRAM_ID,
            // MAYHEM_PROGRAM_ID,
            // MPL_TOKEN_METADATA_PROGRAM_ID,
            // PUMP_AGENT_PAYMENTS_PROGRAM_ID,
            // solana_program::pubkey!("6Vo3245eszAb5wuqEMw8mGdbfRUdKbHhDHP5LcaGuTAB"),
            // solana_program::pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"),
            // // event_authorities
            // pda::pump::event_authority().0,
            // pda::pump_amm::event_authority().0,
            // pda::pump_agent_payments::event_authority().0,
            // // globals
            // pda::pump::global().0,
            // pda::pump::global_volume_accumulator().0,
            // pda::pump::mint_authority().0,
            // pda::pump_amm::global_config().0,
            // pda::pump_amm::global_volume_accumulator().0,
            // pda::pump_agent_payments::global_config().0,
            // pda::mayhem::global_params().0,
            // pda::mayhem::sol_vault().0,
            // // fee_configs
            // pda::pump::fee_config().0,
            // pda::pump_amm::fee_config().0,
        ]
    }

    // OpenBook market side. `MARKET_PROGRAM_ID` is the OpenBook DEX program
    // address — it must be loaded as a *program* on the local validator
    // (via `dump-programs`), not snapshotted as an account, so it's not in
    // the fetch list below.
    #[allow(dead_code)]
    pub const MARKET_PROGRAM_ID: Pubkey = pubkey!("srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX");
    pub const MARKET: Pubkey = pubkey!("8BnEgHoWFysVcuFFX7QztDmzuH8r5ZFvyP3sYwn1XTh6");
    pub const MARKET_AUTHORITY: Pubkey = pubkey!("CTz5UMLQm2SRWHzQnU62Pi4yJqbNGjgRBHqqp6oDHfF7");
    pub const MARKET_BASE_VAULT: Pubkey = pubkey!("CKxTHwM9fPMRRvZmFnFoqKNd9pQR21c5Aq9bh5h9oghX");
    pub const MARKET_QUOTE_VAULT: Pubkey = pubkey!("6A5NHCj1yF6urc9wZNe6Bcjj4LVszQNj5DwAWG97yzMu");
    pub const MARKET_BIDS: Pubkey = pubkey!("5jWUncPNBMZJ3sTHKmMLszypVkoRK6bfEQMQUHweeQnh");
    pub const MARKET_ASKS: Pubkey = pubkey!("EaXdHx7x3mdGA38j5RSmKYSXMzAFzzUXCLNBEDXDn1d5");
    // `MARKET_EVENT_QUEUE` was previously `8CvwxZ9Dg1LRxpgHsvQwYDqEYFA6jSdNG6N4qbSCC4SD`
    // but that address does not exist on mainnet. Left out of the clone
    // list — fill in once we have a confirmed pubkey.
    // pub const MARKET_EVENT_QUEUE: Pubkey = pubkey!("…");
}

#[derive(Clone, Copy, Debug)]
enum Network {
    Devnet,
    Mainnet,
}

impl Network {
    fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mainnet" | "mainnet-beta" => Ok(Network::Mainnet),
            "devnet" => Ok(Network::Devnet),
            other => Err(format!(
                "network must be devnet or mainnet (or mainnet-beta), got `{other}`"
            )),
        }
    }

    fn from_env() -> Self {
        let s = std::env::var("PUMP_NETWORK").unwrap_or_else(|_| "devnet".into());
        Self::parse(&s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn default_rpc(self) -> &'static str {
        match self {
            // Public cluster RPCs (rate-limited). For dedicated providers, set `PUMP_CLONE_RPC`.
            Network::Devnet => "https://api.devnet.solana.com",
            Network::Mainnet => "https://api.mainnet-beta.solana.com",
        }
    }
}

struct Cli {
    network: Option<Network>,
    /// From `-r` / `--rpc-url` or a single positional argument.
    rpc_url: Option<String>,
}

fn print_usage() {
    println!(
        "\
clone_devnet_accounts — snapshot on-chain accounts for the local validator

Usage:
  clone_devnet_accounts [OPTIONS] [RPC_URL]

Options:
  -n, --network <NETWORK>   devnet | mainnet (sets which ALT / fixtures apply).
                              Overrides PUMP_NETWORK.
  -r, --rpc-url <URL>         RPC HTTP endpoint. Overrides PUMP_CLONE_RPC.
  -h, --help                  Print this help.

If RPC_URL is given as the first non-option argument, it is treated like --rpc-url.
Do not pass both --rpc-url and a positional RPC_URL.

RPC resolution: CLI (-r or positional) → PUMP_CLONE_RPC → public cluster RPC.

A .env file in the current directory is loaded when present.

Examples:
  clone_devnet_accounts --network devnet
  clone_devnet_accounts -r https://api.devnet.solana.com
  clone_devnet_accounts https://api.mainnet-beta.solana.com --network mainnet
"
    );
}

fn parse_cli() -> Result<Cli, String> {
    let mut network = None;
    let mut rpc_flag = None;
    let mut positional = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "-n" | "--network" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--network requires a value (devnet or mainnet)".to_string())?;
                network = Some(Network::parse(&v)?);
            }
            "-r" | "--rpc-url" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--rpc-url requires a URL".to_string())?;
                rpc_flag = Some(v);
            }
            s if s.starts_with('-') => {
                return Err(format!("unknown option `{s}` (try --help)"));
            }
            s => {
                if positional.is_some() {
                    return Err(format!("unexpected extra argument `{s}`"));
                }
                positional = Some(s.to_string());
            }
        }
    }

    if rpc_flag.is_some() && positional.is_some() {
        return Err("use either --rpc-url or one positional RPC URL, not both".into());
    }

    Ok(Cli {
        network,
        rpc_url: rpc_flag.or(positional),
    })
}

/// `(label, address, required, fetch_from_mainnet)`. Required entries panic
/// if absent on the chosen cluster; optional entries are skipped when not
/// initialized (e.g. signer-only PDAs or program-state that's lazily
/// created). When `fetch_from_mainnet` is `true` the entry is fetched from
/// mainnet regardless of `--network`; flip back to `false` to revert that
/// PDA to the selected-network fetch.
fn fixed_pdas() -> Vec<(&'static str, Pubkey, bool, bool)> {
    vec![
        ("pump:global", pda::pump::global().0, true, false),
        (
            "pump:event_authority",
            pda::pump::event_authority().0,
            false,
            false,
        ),
        (
            "pump:mint_authority",
            pda::pump::mint_authority().0,
            false,
            false,
        ),
        (
            "pump:global_volume_accumulator",
            pda::pump::global_volume_accumulator().0,
            true,
            false,
        ),
        ("pump:fee_config", pda::pump::fee_config().0, true, false),
        // Consulted by `create_v2` for any quote mint `Global` does not
        // whitelist. Optional: absent until the admin initializes it.
        (
            "pump:quote_control",
            pda::pump::quote_control().0,
            false,
            false,
        ),
        (
            "pump_amm:global_config",
            pda::pump_amm::global_config().0,
            false,
            false,
        ),
        (
            "pump_amm:event_authority",
            pda::pump_amm::event_authority().0,
            false,
            false,
        ),
        (
            "pump_amm:global_volume_accumulator",
            pda::pump_amm::global_volume_accumulator().0,
            false,
            false,
        ),
        (
            "pump_amm:fee_config",
            pda::pump_amm::fee_config().0,
            true,
            false,
        ),
        (
            "pump_agent_payments:global_config",
            pda::pump_agent_payments::global_config().0,
            false,
            false,
        ),
        (
            "mayhem:global_params",
            pda::mayhem::global_params().0,
            true,
            false,
        ),
        ("mayhem:sol_vault", pda::mayhem::sol_vault().0, true, false),
        // ALTs — required so the test's full create_coin versioned tx can
        // compress shared accounts under the 1232-byte limit. Fetch each from
        // its own cluster so both are available regardless of `--network`.
        ("alt:devnet", constants::DEVNET_ALT, true, false),
        ("alt:mainnet", constants::MAINNET_ALT, true, true),
        // Raydium AMM v4 SOL/USDC pool — every sibling account the swap CPI
        // may touch, always fetched from mainnet (see `raydium_sol_usdc`
        // doc-comment for why). All entries are marked `required` because a
        // partial clone will fail the swap unpredictably at runtime; if any
        // mainnet fetch returns `None`, we'd rather hear about it now.
        (
            "raydium_amm_v4:sol_usdc:pool",
            raydium_sol_usdc::POOL,
            true,
            true,
        ),
        (
            "raydium_amm_v4:sol_usdc:amm_authority",
            raydium_sol_usdc::AMM_AUTHORITY,
            false,
            true,
        ),
        (
            "raydium_amm_v4:sol_usdc:open_orders",
            raydium_sol_usdc::OPEN_ORDERS,
            true,
            true,
        ),
        (
            "raydium_amm_v4:sol_usdc:target_orders",
            raydium_sol_usdc::TARGET_ORDERS,
            true,
            true,
        ),
        (
            "raydium_amm_v4:sol_usdc:wsol_vault",
            raydium_sol_usdc::WSOL_VAULT,
            true,
            true,
        ),
        (
            "raydium_amm_v4:sol_usdc:usdc_vault",
            raydium_sol_usdc::USDC_VAULT,
            true,
            true,
        ),
        (
            "openbook:sol_usdc:market",
            raydium_sol_usdc::MARKET,
            true,
            true,
        ),
        (
            "openbook:sol_usdc:market_authority",
            raydium_sol_usdc::MARKET_AUTHORITY,
            false,
            true,
        ),
        (
            "openbook:sol_usdc:market_base_vault",
            raydium_sol_usdc::MARKET_BASE_VAULT,
            true,
            true,
        ),
        (
            "openbook:sol_usdc:market_quote_vault",
            raydium_sol_usdc::MARKET_QUOTE_VAULT,
            true,
            true,
        ),
        (
            "openbook:sol_usdc:market_bids",
            raydium_sol_usdc::MARKET_BIDS,
            true,
            true,
        ),
        (
            "openbook:sol_usdc:market_asks",
            raydium_sol_usdc::MARKET_ASKS,
            true,
            true,
        ),
        // openbook:sol_usdc:market_event_queue — the address we had
        // (8CvwxZ9Dg…) does not exist on mainnet; left commented until we
        // confirm a real one. Raydium can swap without it via the AMM-only
        // path since the router passes a placeholder for this slot anyway.
        // (
        //     "openbook:sol_usdc:market_event_queue",
        //     raydium_sol_usdc::MARKET_EVENT_QUEUE,
        //     true,
        //     true,
        // ),
        // OpenBook fee-discount mints. Not strictly part of the SOL/USDC
        // pool, but OpenBook references them in fee accounting paths; clone
        // them from mainnet so the local validator sees real mint state.
        // Optional — markets without fee-discount logic don't need them.
        (
            "openbook:srm_mint",
            solana_program::pubkey!("SRMuApVNdxXokk5GT7XD5cUUgXMBCoAz2LHeuAoKWRt"),
            false,
            true,
        ),
        (
            "openbook:msrm_mint",
            solana_program::pubkey!("MSRMcoVyrFxnSgo5uXwone5SKcGhT1KEJMFEkMEWf9L"),
            false,
            true,
        ),
    ]
}

fn print_account(label: &str, key: &Pubkey, acct: &Account) {
    println!(
        "  {:<40} {} lamports={} owner={} data={}B",
        label,
        key,
        acct.lamports,
        acct.owner,
        acct.data.len()
    );
}

/// Pull `key` and stash it under `label`. Required entries panic when
/// missing (the test depending on the fixture would fail more
/// confusingly downstream); optional entries log a skip line.
fn clone_one(
    rpc: &RpcClient,
    out: &mut HashMap<Pubkey, Account>,
    label: &str,
    key: Pubkey,
    required: bool,
) -> Result<Option<Account>, Box<dyn std::error::Error>> {
    if let Some(existing) = out.get(&key) {
        print_account(label, &key, existing);
        return Ok(Some(existing.clone()));
    }
    let acct = rpc
        .get_account_with_commitment(&key, rpc.commitment())?
        .value;
    match acct {
        Some(acct) => {
            print_account(label, &key, &acct);
            out.insert(key, acct.clone());
            Ok(Some(acct))
        }
        None if required => {
            panic!("required fixture account `{label}` missing on cluster at {key}")
        }
        None => {
            println!("  {:<40} {} (not on cluster — skipped)", label, key);
            Ok(None)
        }
    }
}

/// Reset ALT meta (active from slot 0) and replace the address list.
fn rewrite_alt_active_from_slot_0(
    acct: &mut Account,
    addresses: Vec<Pubkey>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut deserialized = AddressLookupTable::deserialize(&acct.data)?;
    deserialized.meta = LookupTableMeta::default();
    deserialized.addresses = addresses.into();
    acct.data = AddressLookupTable::serialize_for_tests(deserialized)?;
    Ok(())
}

fn rewrite_token_account(
    acct: &mut Account,
    new_mint: Option<Pubkey>,
    new_owner: Option<Pubkey>,
) -> Result<(), Box<dyn std::error::Error>> {
    if acct.owner == constants::SPL_TOKEN_2022_PROGRAM_ID {
        let state = PodStateWithExtensionsMut::<PodAccount>::unpack(&mut acct.data)?;
        if let Some(mint) = new_mint {
            state.base.mint = mint;
        }
        if let Some(owner) = new_owner {
            state.base.owner = owner;
        }
    } else {
        let mut token = spl_token::state::Account::unpack(&acct.data)?;
        if let Some(mint) = new_mint {
            token.mint = mint;
        }
        if let Some(owner) = new_owner {
            token.owner = owner;
        }
        let mut new_data = vec![0u8; spl_token::state::Account::LEN];
        spl_token::state::Account::pack(token, &mut new_data)?;
        acct.data = new_data;
    }
    Ok(())
}

fn rewrite_mint_authority(
    acct: &mut Account,
    new_authority: Pubkey,
) -> Result<(), Box<dyn std::error::Error>> {
    if acct.owner == constants::SPL_TOKEN_2022_PROGRAM_ID {
        let state = PodStateWithExtensionsMut::<PodMint>::unpack(&mut acct.data)?;
        state.base.mint_authority = PodCOption::some(new_authority);
    } else {
        let mut mint_state = spl_token::state::Mint::unpack(&acct.data)?;
        mint_state.mint_authority = COption::Some(new_authority);
        let mut new_data = vec![0u8; spl_token::state::Mint::LEN];
        spl_token::state::Mint::pack(mint_state, &mut new_data)?;
        acct.data = new_data;
    }
    Ok(())
}

/// Re-key a graduated pool (and every PDA that derives from its address)
/// so the cloned snapshot looks identical to a pool natively created with
/// [`fixtures::USDC_QUOTE_MINT`]. Operates entirely in-memory on `out` —
/// every account it touches has already been cloned by the caller. The
/// pool PDA seeds include `quote_mint`, so changing the quote mint forces
/// the pool to a new address; `lp_mint` is seeded with the pool, so it
/// moves too, and its `mint_authority` must be retargeted at the new pool.
/// The vaults are ATAs of the pool, so their addresses change and their
/// `Token.owner` (and the quote vault's `Token.mint`) must be rewritten.
fn patch_graduated_pool_to_usdc_quote_mint(
    out: &mut HashMap<Pubkey, Account>,
    fixture: &FixtureMint,
    base_mint: &Pubkey,
    pool_creator: &Pubkey,
    pool: &Pool,
    original_pool_pda: Pubkey,
    original_quote_mint: Pubkey,
) -> Result<(), Box<dyn std::error::Error>> {
    let original_lp_mint = pool.lp_mint;
    let original_base_vault = pool.pool_base_token_account;
    let original_quote_vault = pool.pool_quote_token_account;
    let cc_vault_authority = pda::pump_amm::coin_creator_vault_authority(&pool.coin_creator).0;
    let original_cc_quote_ata = pda::associated_token(
        &cc_vault_authority,
        &constants::SPL_TOKEN_PROGRAM_ID,
        &original_quote_mint,
    )
    .0;

    let (new_pool_pda, new_pool_bump) =
        pda::pump_amm::pool(0, pool_creator, base_mint, &fixtures::USDC_QUOTE_MINT);
    let (new_lp_mint_pda, _) = pda::pump_amm::lp_mint(&new_pool_pda);

    let base_token_program = out
        .get(&original_base_vault)
        .expect("original base vault must be cloned before patch")
        .owner;
    let quote_token_program = out
        .get(&original_quote_vault)
        .expect("original quote vault must be cloned before patch")
        .owner;
    let new_base_vault = pda::associated_token(&new_pool_pda, &base_token_program, base_mint).0;
    let new_quote_vault = pda::associated_token(
        &new_pool_pda,
        &quote_token_program,
        &fixtures::USDC_QUOTE_MINT,
    )
    .0;

    let mut patched_pool = pool.clone();
    patched_pool.pool_bump = new_pool_bump;
    patched_pool.quote_mint = fixtures::USDC_QUOTE_MINT;
    patched_pool.lp_mint = new_lp_mint_pda;
    patched_pool.pool_base_token_account = new_base_vault;
    patched_pool.pool_quote_token_account = new_quote_vault;
    let mut new_pool_data = Vec::new();
    patched_pool.try_serialize(&mut new_pool_data)?;
    let mut pool_account = out
        .remove(&original_pool_pda)
        .expect("pool must be present in `out`");
    if new_pool_data.len() < pool_account.data.len() {
        new_pool_data.resize(pool_account.data.len(), 0);
    }
    pool_account.data = new_pool_data;
    out.insert(new_pool_pda, pool_account);

    let mut base_acct = out
        .remove(&original_base_vault)
        .expect("base vault must be cloned before patch");
    rewrite_token_account(&mut base_acct, None, Some(new_pool_pda))?;
    out.insert(new_base_vault, base_acct);

    let mut quote_acct = out
        .remove(&original_quote_vault)
        .expect("quote vault must be cloned before patch");
    rewrite_token_account(
        &mut quote_acct,
        Some(fixtures::USDC_QUOTE_MINT),
        Some(new_pool_pda),
    )?;
    out.insert(new_quote_vault, quote_acct);

    let mut lp_acct = out
        .remove(&original_lp_mint)
        .expect("lp_mint must be cloned before patch");
    rewrite_mint_authority(&mut lp_acct, new_pool_pda)?;
    out.insert(new_lp_mint_pda, lp_acct);

    let new_cc_quote_ata = pda::associated_token(
        &cc_vault_authority,
        &constants::SPL_TOKEN_PROGRAM_ID,
        &fixtures::USDC_QUOTE_MINT,
    )
    .0;
    if let Some(mut cc_acct) = out.remove(&original_cc_quote_ata) {
        rewrite_token_account(&mut cc_acct, Some(fixtures::USDC_QUOTE_MINT), None)?;
        out.insert(new_cc_quote_ata, cc_acct);
    }

    println!(
        "🛠  Re-keyed pool {} -> {} (quote_mint -> {}) for {}",
        original_pool_pda,
        new_pool_pda,
        fixtures::USDC_QUOTE_MINT,
        fixture.label
    );
    Ok(())
}

/// Snapshot every PDA the SDK touches for `fixture.mint`. For graduated
/// mints, also snapshots the post-migration `pump_amm` `Pool` and its
/// base/quote/lp/creator-vault accounts so AMM tests have a working pool
/// on the local validator without needing to run a migration.
///
/// Pool derivation matches canonical pump AMM `pool` PDA layout (see
/// `pump-swap-sdk` / `PumpSdk::buy_quote_amm_*` with live vault balances):
///   `pool_creator = pda::pump::pool_authority(mint)` and `index = 0` —
/// the pump migration always uses index 0 against the deterministic
/// pool-authority PDA.
fn clone_fixture_mint(
    rpc: &RpcClient,
    out: &mut HashMap<Pubkey, Account>,
    fixture: &FixtureMint,
) -> Result<(), Box<dyn std::error::Error>> {
    let mint = fixture.mint;
    println!("📥 Fixture {} ({}):", fixture.label, mint);

    // The mint account itself (Token-2022 program owner for v2 coins).
    clone_one(rpc, out, "  mint", mint, true)?;

    // Bonding curve always exists; everything else hangs off its `creator`.
    let bonding_curve_key = pda::pump::bonding_curve(&mint).0;
    let bc_account = clone_one(rpc, out, "  bonding_curve", bonding_curve_key, true)?
        .expect("bonding_curve required entry returned None");
    let mut bc = decode_bonding_curve(&bc_account.data)?;
    // Remember the quote mint as it lives on-chain before any local patch:
    // the AMM pool and its quote-reserve ATA exist on the cluster at addresses
    // derived from THIS mint, regardless of any patch we apply below.
    let original_bc_quote_mint = bc.quote_mint;

    // Always clone the bonding curve's quote-reserve ATA at the on-chain
    // derived address, regardless of any quote-mint patching below.
    let original_quote_ata_key = pda::associated_token(
        &bonding_curve_key,
        &constants::SPL_TOKEN_PROGRAM_ID,
        &original_bc_quote_mint,
    )
    .0;
    let source_quote_ata = clone_one(
        rpc,
        out,
        "  bonding_curve_quote_ata",
        original_quote_ata_key,
        false,
    )?;

    // Devnet curves are SOL-quoted; rewrite their `quote_mint` to
    // [`USDC_QUOTE_MINT`] so the local validator can exercise the
    // non-SOL-quote (USDC) trade path end-to-end. The Global has already been
    // patched in `patch_global` so the on-chain
    // `is_quote_mint_supported` check accepts this mint.
    if fixture.patch_quote_mint_to_test && bc.quote_mint != fixtures::USDC_QUOTE_MINT {
        bc.quote_mint = fixtures::USDC_QUOTE_MINT;
        let mut new_data = Vec::new();
        bc.try_serialize(&mut new_data)?;
        let entry = out
            .get_mut(&bonding_curve_key)
            .expect("bonding_curve must be present in `out` before patching");
        if new_data.len() < entry.data.len() {
            new_data.resize(entry.data.len(), 0);
        }
        entry.data = new_data;
        println!(
            "🛠  Patched bonding_curve.quote_mint -> {} for {}",
            fixtures::USDC_QUOTE_MINT,
            fixture.label
        );

        // Re-key the quote ATA: program will now derive from USDC_QUOTE_MINT.
        let new_quote_ata_key = pda::associated_token(
            &bonding_curve_key,
            &constants::SPL_TOKEN_PROGRAM_ID,
            &fixtures::USDC_QUOTE_MINT,
        )
        .0;
        match source_quote_ata {
            Some(mut acct) => {
                let mut token = spl_token::state::Account::unpack(&acct.data)?;
                token.mint = fixtures::USDC_QUOTE_MINT;
                let mut new_data = vec![0u8; spl_token::state::Account::LEN];
                spl_token::state::Account::pack(token, &mut new_data)?;
                acct.data = new_data;
                out.insert(new_quote_ata_key, acct);
                out.remove(&original_quote_ata_key);
                println!(
                    "🛠  Re-keyed bonding curve quote ATA {} -> {} (Token.mint -> {})",
                    original_quote_ata_key,
                    new_quote_ata_key,
                    fixtures::USDC_QUOTE_MINT
                );
            }
            None => {
                println!(
                    "ℹ️  No quote ATA on cluster at {} — skipping re-key",
                    original_quote_ata_key
                );
            }
        }
    }

    // Per-mint PDAs the program does NOT lazy-init.
    let creator_vault = pda::pump::creator_vault(&bc.creator).0;
    clone_one(rpc, out, "  creator_vault", creator_vault, false)?;

    // creator_vault's quote-mint ATA — where v2 creator fees accumulate.
    // distribute_creator_fees_v2 / transfer_creator_fees_to_pump_v2 read
    // from this account, so it must be present and non-empty in the
    // snapshot. Clone from the on-chain (pre-patch) quote-mint derived
    // address, then re-key under the USDC_QUOTE_MINT derived address with
    // `Token.mint` rewritten, matching the bonding-curve quote ATA logic.
    {
        let on_chain_quote_mint = if original_bc_quote_mint == Pubkey::default() {
            constants::NATIVE_MINT
        } else {
            original_bc_quote_mint
        };
        let patched_quote_mint = if bc.quote_mint == Pubkey::default() {
            constants::NATIVE_MINT
        } else {
            bc.quote_mint
        };
        let original_cv_quote_ata = pda::associated_token(
            &creator_vault,
            &constants::SPL_TOKEN_PROGRAM_ID,
            &on_chain_quote_mint,
        )
        .0;
        let new_cv_quote_ata = pda::associated_token(
            &creator_vault,
            &constants::SPL_TOKEN_PROGRAM_ID,
            &patched_quote_mint,
        )
        .0;
        let source = clone_one(
            rpc,
            out,
            "  creator_vault_quote_ata (source)",
            original_cv_quote_ata,
            false,
        )?;
        if patched_quote_mint != on_chain_quote_mint {
            if let Some(mut acct) = source {
                let mut token = spl_token::state::Account::unpack(&acct.data)?;
                token.mint = patched_quote_mint;
                let mut new_data = vec![0u8; spl_token::state::Account::LEN];
                spl_token::state::Account::pack(token, &mut new_data)?;
                acct.data = new_data;
                out.insert(new_cv_quote_ata, acct);
                out.remove(&original_cv_quote_ata);
                println!(
                    "🛠  Re-keyed creator_vault_quote_ata {} -> {} (Token.mint -> {})",
                    original_cv_quote_ata, new_cv_quote_ata, patched_quote_mint
                );
            } else {
                println!(
                    "ℹ️  creator_vault_quote_ata not on cluster at {} — skipped re-key",
                    original_cv_quote_ata
                );
            }
        }
    }

    // sharing_config + shareholders (when present): each shareholder's
    // wallet plus their quote ATA so distribute_creator_fees_v2 can pay them.
    let sharing_config_key = pda::pump::sharing_config(&mint).0;
    if let Some(sharing_acct) = clone_one(rpc, out, "  sharing_config", sharing_config_key, false)?
    {
        let quote_mint = if bc.quote_mint == Pubkey::default() {
            constants::NATIVE_MINT
        } else {
            bc.quote_mint
        };
        let quote_token_program = out
            .get(&quote_mint)
            .map(|a| a.owner)
            .unwrap_or(constants::SPL_TOKEN_PROGRAM_ID);
        match decode_sharing_config(&sharing_acct.data) {
            Ok(sc) => {
                println!("    {} shareholder(s)", sc.shareholders.len());
                for (i, sh) in sc.shareholders.iter().enumerate() {
                    clone_one(
                        rpc,
                        out,
                        &format!("    shareholder[{i}]"),
                        sh.address,
                        false,
                    )?;
                    clone_one(
                        rpc,
                        out,
                        &format!("    shareholder[{i}]_quote_ata"),
                        pda::associated_token(&sh.address, &quote_token_program, &quote_mint).0,
                        false,
                    )?;
                }
            }
            Err(e) => println!("    sharing_config decode failed: {e}"),
        }
    }
    clone_one(
        rpc,
        out,
        "  bonding_curve_v2",
        pda::pump::bonding_curve_v2(&mint).0,
        false,
    )?;
    // Bonding curve's base + quote ATAs (Token-2022 base, classic-SPL wSOL quote).
    clone_one(
        rpc,
        out,
        "  bonding_curve_base_ata",
        pda::associated_token(
            &bonding_curve_key,
            &constants::SPL_TOKEN_2022_PROGRAM_ID,
            &mint,
        )
        .0,
        false,
    )?;
    clone_one(
        rpc,
        out,
        "  bonding_curve_wsol_ata",
        pda::associated_token(
            &bonding_curve_key,
            &constants::SPL_TOKEN_PROGRAM_ID,
            &constants::NATIVE_MINT,
        )
        .0,
        false,
    )?;

    // Post-migration AMM pool, only when the curve has graduated. The
    // pool_authority + index=0 derivation matches what the program emits
    // on migration; if the snapshot pre-dates migration, the pool will
    // simply be missing and the AMM tests will be skipped at runtime.
    if bc.complete {
        let pool_creator = pda::pump::pool_authority(&mint).0;
        let pool_quote_mint = if original_bc_quote_mint == Pubkey::default() {
            constants::NATIVE_MINT
        } else {
            original_bc_quote_mint
        };
        let original_pool_pda = pda::pump_amm::pool(0, &pool_creator, &mint, &pool_quote_mint).0;
        let pool_account = clone_one(rpc, out, "  pool", original_pool_pda, false)?;
        if let Some(pool_account) = pool_account {
            let pool = decode_pool(&pool_account.data)?;
            clone_one(rpc, out, "    lp_mint", pool.lp_mint, true)?;
            clone_one(
                rpc,
                out,
                "    pool_base_token_account",
                pool.pool_base_token_account,
                true,
            )?;
            clone_one(
                rpc,
                out,
                "    pool_quote_token_account",
                pool.pool_quote_token_account,
                true,
            )?;
            let cc_vault_authority =
                pda::pump_amm::coin_creator_vault_authority(&pool.coin_creator).0;
            clone_one(
                rpc,
                out,
                "    coin_creator_vault_authority",
                cc_vault_authority,
                false,
            )?;
            let original_cc_quote_ata = pda::associated_token(
                &cc_vault_authority,
                &constants::SPL_TOKEN_PROGRAM_ID,
                &original_bc_quote_mint,
            )
            .0;
            clone_one(
                rpc,
                out,
                "    coin_creator_vault_quote_ata",
                original_cc_quote_ata,
                false,
            )?;

            if fixture.patch_quote_mint_to_test {
                patch_graduated_pool_to_usdc_quote_mint(
                    out,
                    fixture,
                    &mint,
                    &pool_creator,
                    &pool,
                    original_pool_pda,
                    original_bc_quote_mint,
                )?;
            }
        }
    }

    Ok(())
}

/// Patch the cloned `Global`: whitelist [`fixtures::USDC_QUOTE_MINT`] so
/// `create_v2` / `buy_v2` / `sell_v2` accept it. Re-serializing through the
/// current IDL also grows the account to the layout the shipped `pump.so`
/// requires (`Global::SIZE`; an older clone is shorter and every pump
/// instruction fails with Anchor 3003 until it is extended). Trailing fields
/// the clone did not have start zeroed, exactly as `extend_account` leaves
/// them; a longer clone keeps its extra tail bytes. `max_curve_depth` is
/// raised to at least 2 so the pump-quote `create_v2` path is open locally.
fn patch_global(
    out: &mut HashMap<Pubkey, Account>,
    global: &Global,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut patched = global.clone();
    if !patched
        .whitelisted_quote_mints
        .contains(&fixtures::USDC_QUOTE_MINT)
    {
        patched.whitelisted_quote_mints[0] = fixtures::USDC_QUOTE_MINT;
    }
    patched.max_curve_depth = patched.max_curve_depth.max(2);
    let mut new_data = Vec::new();
    patched.try_serialize(&mut new_data)?;
    let key = pda::pump::global().0;
    let entry = out
        .get_mut(&key)
        .expect("pump:global must be present in `out` before patching");
    if let Some(tail) = entry.data.get(new_data.len()..) {
        new_data.extend_from_slice(tail);
    }
    println!(
        "🛠  Global: whitelisted quote mint {}, {} -> {} bytes",
        fixtures::USDC_QUOTE_MINT,
        entry.data.len(),
        new_data.len()
    );
    entry.data = new_data;
    Ok(())
}

/// Insert a synthetic legacy SPL Token mint at [`fixtures::USDC_QUOTE_MINT`]
/// owned by [`fixtures::USDC_QUOTE_MINT_AUTHORITY`] so tests can
/// `mint_to` arbitrary balances on the local validator. Always overwrites:
/// re-running the clone script regenerates a fresh mint with supply=0.
fn synthesize_quote_mint(out: &mut HashMap<Pubkey, Account>) {
    let mint = spl_token::state::Mint {
        mint_authority: solana_program::program_option::COption::Some(
            fixtures::USDC_QUOTE_MINT_AUTHORITY,
        ),
        supply: 0,
        decimals: 6,
        is_initialized: true,
        freeze_authority: solana_program::program_option::COption::None,
    };
    let mut data = vec![0u8; spl_token::state::Mint::LEN];
    spl_token::state::Mint::pack(mint, &mut data).expect("pack test quote Mint");
    let acct = Account {
        lamports: Rent::default().minimum_balance(spl_token::state::Mint::LEN),
        data,
        owner: spl_token::ID,
        executable: false,
        rent_epoch: 0,
    };
    out.insert(fixtures::USDC_QUOTE_MINT, acct);
    println!(
        "🛠  Synthesized test quote mint at {} (authority {})",
        fixtures::USDC_QUOTE_MINT,
        fixtures::USDC_QUOTE_MINT_AUTHORITY
    );
}

/// Insert an empty pump-owned `QuoteControl` when the clone has none:
/// pump-quote `create_v2` requires the PDA to exist even when it lists nothing.
fn synthesize_quote_control(
    out: &mut HashMap<Pubkey, Account>,
) -> Result<(), Box<dyn std::error::Error>> {
    let key = pda::pump::quote_control().0;
    if out.contains_key(&key) {
        return Ok(());
    }
    let mut data = Vec::new();
    QuoteControl {
        admin: Pubkey::default(),
        reserves_admin: Pubkey::default(),
        _reserved: [0; 32],
        mints: vec![],
    }
    .try_serialize(&mut data)?;
    out.insert(
        key,
        Account {
            lamports: Rent::default().minimum_balance(data.len()),
            data,
            owner: pump_rust_client::pump::ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    println!("🛠  Synthesized empty quote_control at {key}");
    Ok(())
}

/// Top every pump / pump_amm account up to rent exemption: re-serializing
/// through the current IDL grows older layouts (`Global`, curves, pools).
fn top_up_rent(out: &mut HashMap<Pubkey, Account>) {
    let rent = Rent::default();
    for acct in out.values_mut().filter(|a| {
        a.owner == pump_rust_client::pump::ID || a.owner == pump_rust_client::pump_amm::ID
    }) {
        acct.lamports = acct.lamports.max(rent.minimum_balance(acct.data.len()));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let cli =
        parse_cli().map_err(|msg| std::io::Error::new(std::io::ErrorKind::InvalidInput, msg))?;

    let network = cli.network.unwrap_or_else(Network::from_env);
    let rpc_url = cli
        .rpc_url
        .or_else(|| std::env::var("PUMP_CLONE_RPC").ok())
        .unwrap_or_else(|| network.default_rpc().to_string());
    let rpc = RpcClient::new(rpc_url.clone());
    println!("🌐 Cloning {network:?} from: {rpc_url}");

    let labeled = fixed_pdas();

    let needs_mainnet = labeled.iter().any(|(_, _, _, from_mainnet)| *from_mainnet);
    let mainnet_rpc: Option<RpcClient> = if needs_mainnet && !matches!(network, Network::Mainnet) {
        let mainnet_url = std::env::var("PUMP_CLONE_MAINNET_RPC")
            .unwrap_or_else(|_| Network::Mainnet.default_rpc().to_string());
        println!("🌐 Mainnet override RPC: {mainnet_url}");
        Some(RpcClient::new(mainnet_url))
    } else {
        None
    };

    let default_keys: Vec<Pubkey> = labeled
        .iter()
        .filter(|(_, _, _, from_mainnet)| !*from_mainnet)
        .map(|(_, k, _, _)| *k)
        .collect();
    let mainnet_keys: Vec<Pubkey> = labeled
        .iter()
        .filter(|(_, _, _, from_mainnet)| *from_mainnet)
        .map(|(_, k, _, _)| *k)
        .collect();

    let mut by_key: HashMap<Pubkey, Option<Account>> = HashMap::new();
    if !default_keys.is_empty() {
        println!(
            "📡 Fetching {} key(s) from {network:?} ({rpc_url})",
            default_keys.len()
        );
        for (k, a) in default_keys
            .iter()
            .copied()
            .zip(rpc.get_multiple_accounts(&default_keys)?.into_iter())
        {
            by_key.insert(k, a);
        }
    }
    if !mainnet_keys.is_empty() {
        let client = mainnet_rpc.as_ref().unwrap_or(&rpc);
        println!(
            "📡 Fetching {} key(s) from mainnet override",
            mainnet_keys.len()
        );
        for (k, a) in mainnet_keys
            .iter()
            .copied()
            .zip(client.get_multiple_accounts(&mainnet_keys)?.into_iter())
        {
            by_key.insert(k, a);
        }
    }

    let mut out: HashMap<Pubkey, Account> = HashMap::new();
    let mut global_account: Option<Account> = None;
    let mut alt_keys: Vec<Pubkey> = Vec::new();
    println!("📥 Fixed PDAs:");
    for (label, key, required, from_mainnet) in labeled.iter() {
        let suffix = if *from_mainnet { " (mainnet)" } else { "" };
        match by_key.remove(key).flatten() {
            Some(acct) => {
                print_account(&format!("{label}{suffix}"), key, &acct);
                if *label == "pump:global" {
                    global_account = Some(acct.clone());
                }
                if label.starts_with("alt:") {
                    alt_keys.push(*key);
                }
                out.insert(*key, acct);
            }
            None if *required => {
                let source = if *from_mainnet {
                    "mainnet".to_string()
                } else {
                    format!("{network:?}")
                };
                panic!("required account `{label}` missing on {source} at {key}");
            }
            None => {
                println!(
                    "  {:<40} {} (not on cluster — skipped)",
                    format!("{label}{suffix}"),
                    key
                );
            }
        }
    }

    // Merge both ALTs: each one ends up holding the union of addresses from
    // devnet + mainnet ALTs plus the Raydium extras, with meta reset to
    // active-from-slot-0. Lets a single snapshot serve either ALT pubkey.
    if !alt_keys.is_empty() {
        let mut seen: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
        let mut merged: Vec<Pubkey> = Vec::new();
        for key in &alt_keys {
            let acct = out.get(key).expect("alt account must be in out");
            let table = AddressLookupTable::deserialize(&acct.data)?;
            for addr in table.addresses.iter() {
                if seen.insert(*addr) {
                    merged.push(*addr);
                }
            }
        }
        for addr in raydium_sol_usdc::alt_extras().iter() {
            if seen.insert(*addr) {
                merged.push(*addr);
            }
        }
        for key in &alt_keys {
            let acct = out.get_mut(key).expect("alt account must be in out");
            rewrite_alt_active_from_slot_0(acct, merged.clone())?;
        }
        println!(
            "🛠  Merged {} ALT(s) → {} addresses each",
            alt_keys.len(),
            merged.len()
        );
    }

    let global = decode_global(&global_account.expect("pump:global must be set above").data)?;
    patch_global(&mut out, &global)?;
    let mut recipients: Vec<Pubkey> = Vec::new();
    recipients.push(global.fee_recipient);
    recipients.extend(global.fee_recipients.iter().copied());
    recipients.push(global.reserved_fee_recipient);
    recipients.extend(global.reserved_fee_recipients.iter().copied());
    recipients.extend(global.buyback_fee_recipients.iter().copied());
    recipients.retain(|p| *p != Pubkey::default());
    recipients.sort();
    recipients.dedup();
    println!(
        "🔎 Global yielded {} distinct fee/buyback recipient(s)",
        recipients.len()
    );

    if !recipients.is_empty() {
        let recipient_accounts = rpc.get_multiple_accounts(&recipients)?;
        println!("📥 Recipients:");
        for (key, maybe_acct) in recipients.iter().zip(recipient_accounts.into_iter()) {
            let acct = maybe_acct.unwrap_or_else(|| Account {
                lamports: 0,
                data: vec![],
                owner: system_program::ID,
                executable: false,
                rent_epoch: 0,
            });
            print_account("recipient", key, &acct);
            out.entry(*key).or_insert(acct);
        }
    }
    for fixture in FIXTURE_MINTS {
        clone_fixture_mint(&rpc, &mut out, fixture)?;
    }

    synthesize_quote_mint(&mut out);
    synthesize_quote_control(&mut out)?;
    top_up_rent(&mut out);

    let bytes = bincode::serialize(&out)?;
    let compressed = zstd::stream::encode_all(&bytes[..], 3)?;
    if let Some(parent) = std::path::Path::new(OUT_PATH).parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(OUT_PATH, &compressed)?;
    println!(
        "✅ Wrote {} accounts to {} ({} bytes raw, {} bytes zstd)",
        out.len(),
        OUT_PATH,
        bytes.len(),
        compressed.len()
    );
    Ok(())
}
