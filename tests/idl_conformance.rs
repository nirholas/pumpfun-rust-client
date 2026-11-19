//! Offline conformance of every public instruction builder against the IDLs
//! in `idls/`.
//!
//! For each built instruction this checks, against the instruction of the
//! same name in the program's IDL:
//!
//! * the program id and the 8-byte discriminator;
//! * that every named IDL account is present, in order, with the IDL's
//!   signer / writable flags and fixed `address`;
//! * every PDA the IDL declares, re-derived from the IDL's own seed list
//!   (constant seeds, the other accounts of the instruction, and account
//!   fields such as `bonding_curve.creator` supplied by the test), so a wrong
//!   seed, seed order or owning program in `pda.rs` fails here.
//!
//! Remaining accounts (past the IDL's named accounts) are not described by
//! the IDLs and are covered by the unit tests next to each builder.

use std::collections::HashMap;

use anchor_lang::solana_program::{instruction::Instruction, pubkey::Pubkey};
use pump_rust_client::pump::pump::accounts as idl_accounts;
use pump_rust_client::{
    constants, pda,
    pump_fees::types::Shareholder,
    state::{pump_amm::GlobalConfig, pump_amm::Pool, BondingCurve, Global},
    MultiHopHop, PumpSdk,
};
use serde_json::Value;

const PUMP_IDL: &str = include_str!("../idls/pump.json");
const PUMP_AMM_IDL: &str = include_str!("../idls/pump_amm.json");
const PUMP_FEES_IDL: &str = include_str!("../idls/pump_fees.json");
const AGENT_IDL: &str = include_str!("../idls/pump_agent_payments.json");

struct Idls(HashMap<Pubkey, Value>);

impl Idls {
    fn load() -> Self {
        let map = [PUMP_IDL, PUMP_AMM_IDL, PUMP_FEES_IDL, AGENT_IDL]
            .into_iter()
            .map(|src| {
                let idl: Value = serde_json::from_str(src).expect("IDL is valid JSON");
                let address = idl["address"].as_str().expect("IDL has an address");
                (Pubkey::from_str_const(address), idl)
            })
            .collect();
        Self(map)
    }

    /// Check `ix` against the IDL instruction `name` of its program and return
    /// the number of PDAs re-derived.
    fn check(&self, name: &str, ix: &Instruction, fields: &[(&str, Pubkey)]) -> usize {
        let idl = self
            .0
            .get(&ix.program_id)
            .unwrap_or_else(|| panic!("{name}: no IDL for program {}", ix.program_id));
        let idl_ix = idl["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == name)
            .unwrap_or_else(|| panic!("{name}: not in the IDL"));

        let discriminator: Vec<u8> = idl_ix["discriminator"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b.as_u64().unwrap() as u8)
            .collect();
        assert_eq!(&ix.data[..8], discriminator, "{name}: discriminator");

        let accounts = idl_ix["accounts"].as_array().unwrap();
        assert!(
            ix.accounts.len() >= accounts.len(),
            "{name}: {} metas for {} IDL accounts",
            ix.accounts.len(),
            accounts.len()
        );
        let index: HashMap<&str, usize> = accounts
            .iter()
            .enumerate()
            .map(|(i, a)| (a["name"].as_str().unwrap(), i))
            .collect();
        let fields: HashMap<&str, Pubkey> = fields.iter().copied().collect();

        let mut derived = 0;
        for (i, account) in accounts.iter().enumerate() {
            let account_name = account["name"].as_str().unwrap();
            let meta = &ix.accounts[i];
            let flag = |key: &str| account[key].as_bool().unwrap_or(false);
            assert_eq!(
                meta.is_signer,
                flag("signer"),
                "{name}.{account_name}: signer"
            );
            assert_eq!(
                meta.is_writable,
                flag("writable"),
                "{name}.{account_name}: writable"
            );
            if let Some(address) = account["address"].as_str() {
                assert_eq!(
                    meta.pubkey,
                    Pubkey::from_str_const(address),
                    "{name}.{account_name}: fixed address"
                );
            }
            let Some(pda_spec) = account.get("pda") else {
                continue;
            };
            let resolve = |seed: &Value| -> Vec<u8> {
                match seed["kind"].as_str().unwrap() {
                    "const" => seed["value"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|b| b.as_u64().unwrap() as u8)
                        .collect(),
                    "account" => {
                        let path = seed["path"].as_str().unwrap();
                        let key = if path.contains('.') {
                            *fields.get(path).unwrap_or_else(|| {
                                panic!("{name}.{account_name}: supply field `{path}`")
                            })
                        } else {
                            ix.accounts[index[path]].pubkey
                        };
                        key.to_bytes().to_vec()
                    }
                    other => panic!("{name}.{account_name}: unsupported seed kind {other}"),
                }
            };
            let seeds: Vec<Vec<u8>> = pda_spec["seeds"]
                .as_array()
                .unwrap()
                .iter()
                .map(resolve)
                .collect();
            let owner = match pda_spec.get("program") {
                Some(program) => Pubkey::try_from(resolve(program).as_slice()).unwrap(),
                None => ix.program_id,
            };
            let seed_refs: Vec<&[u8]> = seeds.iter().map(Vec::as_slice).collect();
            let (expected, _) = Pubkey::find_program_address(&seed_refs, &owner);
            assert_eq!(meta.pubkey, expected, "{name}.{account_name}: PDA");
            derived += 1;
        }
        derived
    }
}

fn key(seed: u8) -> Pubkey {
    Pubkey::new_from_array([seed; 32])
}

const MINT: Pubkey = Pubkey::new_from_array([1; 32]);
const USER: Pubkey = Pubkey::new_from_array([2; 32]);
const CREATOR: Pubkey = Pubkey::new_from_array([3; 32]);
const USDC: Pubkey = Pubkey::new_from_array([4; 32]);
const FEE_RECIPIENT: Pubkey = Pubkey::new_from_array([5; 32]);
const BUYBACK: Pubkey = Pubkey::new_from_array([6; 32]);

fn global() -> Global {
    let mut buyback_fee_recipients = [Pubkey::default(); 8];
    buyback_fee_recipients[0] = BUYBACK;
    Global::new(idl_accounts::Global {
        fee_recipient: FEE_RECIPIENT,
        buyback_fee_recipients,
        ..Default::default()
    })
}

fn curve(quote_mint: Pubkey) -> BondingCurve {
    BondingCurve::new(idl_accounts::BondingCurve {
        creator: CREATOR,
        quote_mint,
        ..Default::default()
    })
}

fn pump_pool(quote_mint: Pubkey) -> (Pubkey, Pool) {
    let mut p = Pool::default();
    p.base_mint = MINT;
    p.quote_mint = quote_mint;
    p.creator = pda::pump::pool_authority(&MINT).0;
    p.coin_creator = CREATOR;
    p.pool_base_token_account =
        pda::associated_token(&p.creator, &constants::SPL_TOKEN_2022_PROGRAM_ID, &MINT).0;
    p.pool_quote_token_account =
        pda::associated_token(&p.creator, &constants::SPL_TOKEN_PROGRAM_ID, &quote_mint).0;
    (pda::pump_amm::canonical_pool(&MINT, &quote_mint).0, p)
}

fn amm_global() -> GlobalConfig {
    let mut g = GlobalConfig::default();
    g.protocol_fee_recipients[0] = FEE_RECIPIENT;
    g.buyback_fee_recipients[0] = BUYBACK;
    g
}

const T22: Pubkey = constants::SPL_TOKEN_2022_PROGRAM_ID;
const SPL: Pubkey = constants::SPL_TOKEN_PROGRAM_ID;

#[test]
fn pump_bonding_curve_builders_match_the_idl() {
    let idls = Idls::load();
    let sdk = PumpSdk::new();
    let g = global();
    let mut derived = 0;
    for quote in [Pubkey::default(), USDC] {
        let c = curve(quote);
        let creator_field = [("bonding_curve.creator", CREATOR)];

        let ix = sdk.create_v2_instruction(
            MINT, USER, "n", "s", "u", CREATOR, quote, SPL, false, false, 0,
        );
        derived += idls.check("create_v2", &ix, &[]);

        let ix = sdk
            .buy_v2_instruction(&g, &c, MINT, SPL, USER, 1, 2)
            .unwrap();
        derived += idls.check("buy_v2", &ix, &creator_field);
        let ix = sdk
            .buy_exact_quote_in_v2_instruction(&g, &c, MINT, SPL, USER, 1, 2)
            .unwrap();
        derived += idls.check("buy_exact_quote_in_v2", &ix, &creator_field);
        let ix = sdk
            .sell_v2_instruction(&g, &c, MINT, SPL, USER, 1, 2)
            .unwrap();
        derived += idls.check("sell_v2", &ix, &creator_field);

        let ix = sdk.buy_v3_instruction(MINT, quote, T22, SPL, USER, BUYBACK, 1, 2);
        derived += idls.check("buy_v3", &ix, &[]);
        let ix = sdk.buy_exact_quote_in_v3_instruction(MINT, quote, T22, SPL, USER, BUYBACK, 1, 2);
        derived += idls.check("buy_exact_quote_in_v3", &ix, &[]);
        let ix = sdk.sell_v3_instruction(MINT, quote, T22, SPL, USER, BUYBACK, 1, 2);
        derived += idls.check("sell_v3", &ix, &[]);
        let ix = sdk.sweep_creator_fee_instruction(USER, MINT, CREATOR, quote, SPL);
        derived += idls.check("sweep_creator_fee", &ix, &[]);

        let ix = sdk.claim_cashback_v2_instruction(USER, quote, SPL);
        derived += idls.check("claim_cashback_v2", &ix, &[]);
        let ix = sdk.collect_creator_fee_v2_instruction(CREATOR, quote, SPL);
        derived += idls.check("collect_creator_fee_v2", &ix, &[]);
        let ix = sdk.distribute_creator_fees_v2_instruction(
            USER,
            MINT,
            CREATOR,
            quote,
            SPL,
            true,
            &[key(9)],
        );
        derived += idls.check("distribute_creator_fees_v2", &ix, &creator_field);
        let ix = sdk.migrate_v2_instruction(
            MINT,
            if quote == Pubkey::default() {
                constants::NATIVE_MINT
            } else {
                quote
            },
            USER,
            key(8),
            T22,
            SPL,
        );
        derived += idls.check("migrate_v2", &ix, &[]);
    }
    let ix = sdk.extend_account_ix(MINT, USER);
    derived += idls.check("extend_account", &ix, &[]);
    assert!(derived > 150, "only {derived} PDAs checked");
}

#[test]
#[allow(deprecated)]
fn pump_legacy_builders_match_the_idl() {
    let idls = Idls::load();
    let sdk = PumpSdk::new();
    let creator_field = [("bonding_curve.creator", CREATOR)];
    let ix = sdk.create_instruction(MINT, USER, "n", "s", "u", CREATOR);
    idls.check("create", &ix, &[]);
    let ix = sdk.buy_instruction(MINT, USER, CREATOR, FEE_RECIPIENT, BUYBACK, 1, 2, SPL);
    idls.check("buy", &ix, &creator_field);
    for cashback in [false, true] {
        let ix = sdk.sell_instruction(
            MINT,
            USER,
            CREATOR,
            FEE_RECIPIENT,
            BUYBACK,
            1,
            2,
            SPL,
            cashback,
        );
        idls.check("sell", &ix, &creator_field);
    }
}

#[test]
fn pump_amm_builders_match_the_idl() {
    let idls = Idls::load();
    let sdk = PumpSdk::new();
    let gc = amm_global();
    let mut derived = 0;
    for quote in [constants::NATIVE_MINT, USDC] {
        let (pool, state) = pump_pool(quote);
        let coin_creator = [("pool.coin_creator", CREATOR)];

        let ix = sdk
            .buy_amm_instruction(pool, &gc, &state, T22, SPL, USER, 1, 2)
            .unwrap();
        derived += idls.check("buy", &ix, &coin_creator);
        let ix = sdk
            .sell_amm_instruction(pool, &gc, &state, T22, SPL, USER, 1, 2)
            .unwrap();
        derived += idls.check("sell", &ix, &coin_creator);

        let ix = sdk.buy_amm_v2_instruction(&state, T22, SPL, USER, BUYBACK, 1, 2);
        derived += idls.check("buy_v2", &ix, &[]);
        let ix = sdk.buy_exact_quote_in_amm_v2_instruction(&state, T22, SPL, USER, BUYBACK, 1, 2);
        derived += idls.check("buy_exact_quote_in_v2", &ix, &[]);
        let ix = sdk.sell_amm_v2_instruction(&state, T22, SPL, USER, BUYBACK, 1, 2);
        derived += idls.check("sell_v2", &ix, &[]);

        let ix = sdk.collect_coin_creator_fee_instruction(CREATOR, quote, SPL);
        derived += idls.check("collect_coin_creator_fee", &ix, &[]);
        let ix = sdk.sweep_pool_creator_fee_instruction(USER, &state, SPL);
        derived += idls.check("sweep_creator_fee", &ix, &[]);
        let ix = sdk.transfer_creator_fees_to_pump_v2_instruction(USER, CREATOR, quote, SPL);
        derived += idls.check("transfer_creator_fees_to_pump_v2", &ix, &[]);
    }

    let (_, state) = pump_pool(constants::NATIVE_MINT);
    let hops = [MultiHopHop::pool(&state)];
    let ix = sdk
        .multi_hop_swap_instruction(USER, constants::NATIVE_MINT, SPL, T22, &hops, BUYBACK, 1, 1)
        .unwrap();
    derived += idls.check("multi_hop_swap", &ix, &[]);
    assert!(derived >= 60, "only {derived} PDAs checked");
}

#[test]
fn pump_fees_and_agent_builders_match_the_idl() {
    let idls = Idls::load();
    let sdk = PumpSdk::new();
    let shares = || {
        vec![Shareholder {
            address: CREATOR,
            share_bps: 10_000,
        }]
    };
    for complete in [false, true] {
        let mut c = curve(USDC);
        c.complete = complete;
        let ix = sdk.update_fee_shares_instruction(USER, MINT, &c, &[key(9)], shares());
        idls.check("update_fee_shares", &ix, &[]);
        let ix = sdk.update_fee_shares_v2_instruction(USER, MINT, &c, SPL, &[key(9)], shares());
        idls.check("update_fee_shares_v2", &ix, &[]);
    }
    let ix = sdk.agent_initialize_instruction(MINT, USER, CREATOR, 100);
    idls.check("agent_initialize", &ix, &[]);
}
