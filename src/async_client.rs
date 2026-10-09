//! RPC wrapper around [`crate::sdk::PumpSdk`].

use std::{collections::HashMap, sync::Arc};

use solana_client::{
    nonblocking::rpc_client::RpcClient, rpc_client::SerializableTransaction,
    rpc_response::RpcSimulateTransactionResult,
};
use solana_sdk::{
    account::Account,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Signature,
    signer::Signer,
    transaction::Transaction,
};

use crate::{
    accounts::{
        decode, decode_bonding_curve, decode_fee_config, decode_global,
        decode_global_volume_accumulator, decode_quote_control,
        decode_user_volume_accumulator_nullable,
        pump_amm::{decode_global_config, decode_pool},
    },
    errors::{PumpClientError, Result},
    math::{QuoteError, QuoteResult},
    pda,
    sdk::{MultiHopHop, PumpSdk, Quote, RouteHop},
    state::{
        pump_amm::{GlobalConfig, Pool},
        BondingCurve, FeeConfig, Global, GlobalVolumeAccumulator, QuoteControl,
        UserVolumeAccumulator,
    },
};

#[derive(Clone)]
pub struct AsyncPumpClient {
    rpc: Arc<RpcClient>,
    sdk: PumpSdk,
}

/// Account snapshot for building a buy.
#[derive(Debug)]
pub struct BuyState {
    pub bonding_curve_account: Account,
    pub bonding_curve: BondingCurve,
    pub associated_user_account: Option<Account>,
}

/// Account snapshot for building a sell.
#[derive(Debug)]
pub struct SellState {
    pub bonding_curve_account: Account,
    pub bonding_curve: BondingCurve,
}

/// Optional compute-budget instructions prepended to built transactions (limit, then price).
#[derive(Clone, Copy, Debug, Default)]
pub struct ComputeBudget {
    pub units: Option<u32>,
    pub micro_lamports_per_unit: Option<u64>,
}

impl ComputeBudget {
    fn prepend_into(&self, base: &[Instruction]) -> Vec<Instruction> {
        let extra = self.units.is_some() as usize + self.micro_lamports_per_unit.is_some() as usize;
        let mut out = Vec::with_capacity(base.len() + extra);
        if let Some(units) = self.units {
            out.push(ComputeBudgetInstruction::set_compute_unit_limit(units));
        }
        if let Some(price) = self.micro_lamports_per_unit {
            out.push(ComputeBudgetInstruction::set_compute_unit_price(price));
        }
        out.extend_from_slice(base);
        out
    }
}

impl AsyncPumpClient {
    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self {
            rpc,
            sdk: PumpSdk::new(),
        }
    }

    pub fn rpc(&self) -> &Arc<RpcClient> {
        &self.rpc
    }

    pub fn sdk(&self) -> &PumpSdk {
        &self.sdk
    }

    pub async fn fetch_global(&self) -> Result<Global> {
        let address = pda::pump::global().0;
        let account = self.get_account(&address, "global").await?;
        decode_global(&account.data)
    }

    pub async fn fetch_fee_config(&self) -> Result<FeeConfig> {
        let address = pda::pump::fee_config().0;
        let account = self.get_account(&address, "fee_config").await?;
        decode_fee_config(&account.data)
    }

    /// The `quote-control` PDA listing quote mints admitted for `create_v2`
    /// beyond `Global.whitelisted_quote_mints`. Errors until it is initialized.
    pub async fn fetch_quote_control(&self) -> Result<QuoteControl> {
        let address = pda::pump::quote_control().0;
        let account = self.get_account(&address, "quote_control").await?;
        decode_quote_control(&account.data)
    }

    pub async fn fetch_bonding_curve(&self, mint: &Pubkey) -> Result<BondingCurve> {
        let address = pda::pump::bonding_curve(mint).0;
        let account = self.get_account(&address, "bonding_curve").await?;
        decode_bonding_curve(&account.data)
    }

    pub async fn fetch_global_volume_accumulator(&self) -> Result<GlobalVolumeAccumulator> {
        let address = pda::pump::global_volume_accumulator().0;
        let account = self
            .get_account(&address, "global_volume_accumulator")
            .await?;
        decode_global_volume_accumulator(&account.data)
    }

    pub async fn fetch_user_volume_accumulator(
        &self,
        user: &Pubkey,
    ) -> Result<Option<UserVolumeAccumulator>> {
        let address = pda::pump::user_volume_accumulator(user).0;
        match self
            .rpc
            .get_account_with_commitment(&address, self.rpc.commitment())
            .await
        {
            Ok(response) => match response.value {
                Some(account) => Ok(decode_user_volume_accumulator_nullable(&account.data)),
                None => Ok(None),
            },
            Err(e) => Err(PumpClientError::from(e)),
        }
    }

    pub async fn fetch_buy_state(
        &self,
        mint: &Pubkey,
        user: &Pubkey,
        token_program: &Pubkey,
    ) -> Result<BuyState> {
        let (bonding_curve_account, bonding_curve, associated_user_account) = self
            .fetch_bonding_curve_with_user_token_account(mint, user, token_program)
            .await?;
        Ok(BuyState {
            bonding_curve_account,
            bonding_curve,
            associated_user_account,
        })
    }

    pub async fn fetch_sell_state(
        &self,
        mint: &Pubkey,
        user: &Pubkey,
        token_program: &Pubkey,
    ) -> Result<SellState> {
        let (bonding_curve_account, bonding_curve, associated_user_account) = self
            .fetch_bonding_curve_with_user_token_account(mint, user, token_program)
            .await?;
        if associated_user_account.is_none() {
            return Err(PumpClientError::AccountNotFound {
                name: "associated_user",
                address: pda::associated_token(user, token_program, mint).0,
            });
        }
        Ok(SellState {
            bonding_curve_account,
            bonding_curve,
        })
    }

    async fn fetch_bonding_curve_with_user_token_account(
        &self,
        mint: &Pubkey,
        user: &Pubkey,
        token_program: &Pubkey,
    ) -> Result<(Account, BondingCurve, Option<Account>)> {
        let bonding_curve_address = pda::pump::bonding_curve(mint).0;
        let associated_user = pda::associated_token(user, token_program, mint).0;

        let mut accounts = self
            .rpc
            .get_multiple_accounts(&[bonding_curve_address, associated_user])
            .await
            .map_err(PumpClientError::from)?
            .into_iter();

        let bonding_curve_account =
            accounts
                .next()
                .flatten()
                .ok_or(PumpClientError::AccountNotFound {
                    name: "bonding_curve",
                    address: bonding_curve_address,
                })?;
        let associated_user_account = accounts.next().flatten();
        let bonding_curve = decode_bonding_curve(&bonding_curve_account.data)?;
        Ok((
            bonding_curve_account,
            bonding_curve,
            associated_user_account,
        ))
    }

    /// Spendable lamports in the creator vault (above rent); 0 if missing or rent-only.
    pub async fn get_creator_vault_balance(&self, creator: &Pubkey) -> Result<u64> {
        let creator_vault = pda::pump::creator_vault(creator).0;

        let account = match self
            .rpc
            .get_account_with_commitment(&creator_vault, self.rpc.commitment())
            .await
            .map_err(PumpClientError::from)?
            .value
        {
            Some(account) => account,
            None => return Ok(0),
        };

        let rent_exempt = self
            .rpc
            .get_minimum_balance_for_rent_exemption(account.data.len())
            .await
            .map_err(PumpClientError::from)?;

        if account.lamports <= rent_exempt {
            return Ok(0);
        }
        Ok(account.lamports - rent_exempt)
    }

    /// Recent blockhash at the client's commitment.
    pub async fn latest_blockhash(&self) -> Result<Hash> {
        self.rpc
            .get_latest_blockhash()
            .await
            .map_err(PumpClientError::from)
    }

    /// Fetches a blockhash then signs. Prefer [`Self::build_transaction_with_blockhash`] if you already have a hash.
    pub async fn build_transaction(
        &self,
        ixs: &[Instruction],
        payer: &Pubkey,
        signers: &[&dyn Signer],
        compute_budget: Option<ComputeBudget>,
    ) -> Result<Transaction> {
        let recent_blockhash = self.latest_blockhash().await?;
        Ok(self.build_transaction_with_blockhash(
            ixs,
            payer,
            signers,
            recent_blockhash,
            compute_budget,
        ))
    }

    /// Prepends compute-budget instructions when set, then signs.
    pub fn build_transaction_with_blockhash(
        &self,
        ixs: &[Instruction],
        payer: &Pubkey,
        signers: &[&dyn Signer],
        recent_blockhash: Hash,
        compute_budget: Option<ComputeBudget>,
    ) -> Transaction {
        let full_ixs = match compute_budget {
            Some(cb) => cb.prepend_into(ixs),
            None => ixs.to_vec(),
        };
        Transaction::new_signed_with_payer(&full_ixs, Some(payer), signers, recent_blockhash)
    }

    /// Simulate a transaction.
    pub async fn simulate_transaction<T: SerializableTransaction>(
        &self,
        tx: &T,
    ) -> Result<RpcSimulateTransactionResult> {
        let response = self
            .rpc
            .simulate_transaction(tx)
            .await
            .map_err(PumpClientError::from)?;
        Ok(response.value)
    }

    /// Send without waiting for confirmation.
    pub async fn send_transaction<T: SerializableTransaction>(&self, tx: &T) -> Result<Signature> {
        self.rpc
            .send_transaction(tx)
            .await
            .map_err(PumpClientError::from)
    }

    /// Send and confirm at the client's commitment.
    pub async fn send_and_confirm_transaction<T: SerializableTransaction>(
        &self,
        tx: &T,
    ) -> Result<Signature> {
        self.rpc
            .send_and_confirm_transaction(tx)
            .await
            .map_err(PumpClientError::from)
    }

    pub async fn fetch_pool(&self, pool: &Pubkey) -> Result<Pool> {
        let account = self.get_account(pool, "pool").await?;
        decode_pool(&account.data)
    }

    pub async fn fetch_amm_global_config(&self) -> Result<GlobalConfig> {
        let address = pda::pump_amm::global_config().0;
        let account = self.get_account(&address, "amm_global_config").await?;
        decode_global_config(&account.data)
    }

    /// pump-amm's pump-fees `FeeConfig` (a different PDA from pump's).
    pub async fn fetch_amm_fee_config(&self) -> Result<FeeConfig> {
        let address = pda::pump_amm::fee_config().0;
        let account = self.get_account(&address, "amm_fee_config").await?;
        decode_fee_config(&account.data)
    }

    /// `(supply, owning token program)` of `mint` from one account fetch.
    pub async fn fetch_mint(&self, mint: &Pubkey) -> Result<(u64, Pubkey)> {
        let account = self.get_account(mint, "mint").await?;
        Self::mint_info(&account)
    }

    fn mint_info(account: &Account) -> Result<(u64, Pubkey)> {
        Ok((
            decode::<anchor_spl::token_interface::Mint>(&account.data)?.supply,
            account.owner,
        ))
    }

    /// Everything the bonding-curve quoters and the v2 / v3 trade builders
    /// need for `mint`: `Global`, pump's `FeeConfig`, the curve, the live mint
    /// supply, both token programs and a buyback fee recipient drawn from
    /// `Global`. Two fetches (the quote mint and the curve's base ATA are only
    /// known once the curve and mint are decoded).
    pub async fn fetch_curve_quote_state(&self, mint: &Pubkey) -> Result<CurveQuoteState> {
        let keys = [
            pda::pump::global().0,
            pda::pump::fee_config().0,
            pda::pump::bonding_curve(mint).0,
            *mint,
        ];
        let accounts = self.get_many(&keys).await?;
        let get = |i: usize, name: &'static str| -> Result<&Account> {
            accounts[i]
                .as_ref()
                .ok_or(PumpClientError::AccountNotFound {
                    name,
                    address: keys[i],
                })
        };
        let bonding_curve = decode_bonding_curve(&get(2, "bonding_curve")?.data)?;
        let (mint_supply, base_token_program) = Self::mint_info(get(3, "mint")?)?;
        let quote_mint = PumpSdk::resolve_quote_mint(bonding_curve.quote_mint);
        let base_ata = pda::associated_token(&keys[2], &base_token_program, mint).0;
        let second = [quote_mint, base_ata];
        let more = self.get_many(&second).await?;
        let quote_token_program = required(&more, &second, 0, "quote_mint")?.owner;
        let base_ata_amount = more[1].as_ref().map_or(Ok(0), token_amount)?;
        let global = decode_global(&get(0, "global")?.data)?;
        let buyback_fee_recipient = PumpSdk::buyback_fee_recipient_from_pump_global(&global)
            .ok_or(PumpClientError::AccountNotFound {
                name: "buyback_fee_recipient",
                address: keys[0],
            })?;
        Ok(CurveQuoteState {
            global,
            fee_config: decode_fee_config(&get(1, "fee_config")?.data)?,
            bonding_curve,
            mint: *mint,
            mint_supply,
            base_token_program,
            quote_mint,
            quote_token_program,
            base_ata_amount,
            buyback_fee_recipient,
        })
    }

    /// `buy_v3` of `amount` tokens of `mint` for `user`: fetches the state,
    /// quotes with `slippage_bps` and returns the quote with
    /// [`PumpSdk::buy_v3_instructions`] at the quoted `max_input`. Past the
    /// curve's remaining supply the quote includes the post-completion leg
    /// (a mayhem curve fills only the remainder).
    pub async fn build_buy_v3(
        &self,
        user: &Pubkey,
        mint: &Pubkey,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<(Quote, Vec<Instruction>)> {
        let s = self.fetch_curve_quote_state(mint).await?;
        let quote = self.sdk.buy_quote_bonding_curve_v3_token_out(
            &s.global,
            &s.fee_config,
            &s.bonding_curve,
            s.mint_supply,
            s.base_ata_amount,
            amount,
            slippage_bps,
        )?;
        let ixs = self.sdk.buy_v3_instructions(
            s.mint,
            s.quote_mint,
            s.base_token_program,
            s.quote_token_program,
            *user,
            s.buyback_fee_recipient,
            amount,
            quote.max_input,
        );
        Ok((quote, ixs))
    }

    /// `buy_exact_quote_in_v3` spending `spendable_quote_in` of `mint`'s
    /// quote: quotes the tokens out and builds
    /// [`PumpSdk::buy_exact_quote_in_v3_instructions`] at the quoted `min_out`.
    pub async fn build_buy_exact_quote_in_v3(
        &self,
        user: &Pubkey,
        mint: &Pubkey,
        spendable_quote_in: u64,
        slippage_bps: u16,
    ) -> Result<(Quote, Vec<Instruction>)> {
        let s = self.fetch_curve_quote_state(mint).await?;
        let quote = self.sdk.buy_quote_bonding_curve_v3_sol_in(
            &s.global,
            &s.fee_config,
            &s.bonding_curve,
            s.mint_supply,
            s.base_ata_amount,
            spendable_quote_in,
            slippage_bps,
        )?;
        let ixs = self.sdk.buy_exact_quote_in_v3_instructions(
            s.mint,
            s.quote_mint,
            s.base_token_program,
            s.quote_token_program,
            *user,
            s.buyback_fee_recipient,
            spendable_quote_in,
            quote.min_out,
        );
        Ok((quote, ixs))
    }

    /// `sell_v3` of `amount` tokens of `mint`: quotes the proceeds and builds
    /// [`PumpSdk::sell_v3_instructions`] at the quoted `min_out`.
    pub async fn build_sell_v3(
        &self,
        user: &Pubkey,
        mint: &Pubkey,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<(Quote, Vec<Instruction>)> {
        let s = self.fetch_curve_quote_state(mint).await?;
        let quote = self.sdk.sell_quote_bonding_curve(
            &s.global,
            &s.fee_config,
            &s.bonding_curve,
            s.mint_supply,
            amount,
            slippage_bps,
        )?;
        let ixs = self.sdk.sell_v3_instructions(
            s.mint,
            s.quote_mint,
            s.base_token_program,
            s.quote_token_program,
            *user,
            s.buyback_fee_recipient,
            amount,
            quote.min_out,
        );
        Ok((quote, ixs))
    }

    /// `create_v2` inputs for a coin quoted in pump coin `quote_mint` (Q): the
    /// derived reserves and depth, Q's token program and the remaining accounts
    /// to append. The `quote-control` PDA must exist (the program requires it).
    pub async fn fetch_pump_quote_create(&self, quote_mint: &Pubkey) -> Result<PumpQuoteCreate> {
        let keys = [
            pda::pump::global().0,
            pda::pump::quote_control().0,
            pda::pump::bonding_curve(quote_mint).0,
            *quote_mint,
        ];
        let accounts = self.get_many(&keys).await?;
        let global = decode_global(&required(&accounts, &keys, 0, "global")?.data)?;
        let quote_control =
            decode_quote_control(&required(&accounts, &keys, 1, "quote_control")?.data)?;
        let quote_curve =
            decode_bonding_curve(&required(&accounts, &keys, 2, "bonding_curve")?.data)?;
        let (_, quote_token_program) =
            Self::mint_info(required(&accounts, &keys, 3, "quote_mint")?)?;

        // Once Q completes, its canonical pool (if migrated) prices it.
        let pool_key = pda::pump_amm::canonical_pool(
            quote_mint,
            &PumpSdk::resolve_quote_mint(quote_curve.quote_mint),
        )
        .0;
        let pool = if quote_curve.complete {
            self.get_many(&[pool_key])
                .await?
                .pop()
                .flatten()
                .map(|a| decode_pool(&a.data))
                .transpose()?
        } else {
            None
        };
        let vaults = match &pool {
            Some(pool) => {
                let keys = [pool.pool_base_token_account, pool.pool_quote_token_account];
                let accounts = self.get_many(&keys).await?;
                Some((
                    token_amount(required(&accounts, &keys, 0, "pool_base_token_account")?)?,
                    token_amount(required(&accounts, &keys, 1, "pool_quote_token_account")?)?,
                ))
            }
            None => None,
        };
        let (virtual_quote_reserves, depth) = PumpSdk::pump_quote_initial_virtual_quote_reserves(
            &global,
            Some(&quote_control),
            &quote_curve,
            pool.as_ref().zip(vaults).map(|(p, (b, q))| (p, b, q)),
        )?;
        Ok(PumpQuoteCreate {
            virtual_quote_reserves,
            depth,
            quote_token_program,
            remaining_accounts: PumpSdk::create_v2_pump_quote_accounts(
                quote_mint,
                pool.as_ref().map(|p| (pool_key, p)),
            ),
        })
    }

    /// Walk the quote chain back from `out_mint`: each mint's bonding curve
    /// names its quote; a complete curve becomes its canonical pool, an open
    /// one a curve venue. Stops at the first mint without a pump bonding curve
    /// (the root currency, e.g. USDC or wSOL; a SOL curve's quote is wSOL) or
    /// at `stop_at`. Venues come back in buy order (`in_mint` → `out_mint`).
    /// One curve fetch per hop.
    pub async fn discover_route(
        &self,
        out_mint: &Pubkey,
        stop_at: Option<&Pubkey>,
    ) -> Result<MultiHopRoute> {
        // ponytail: 8 hops is well past what a transaction fits; guards a cyclic chain.
        const MAX_HOPS: usize = 8;
        let mut venues = Vec::new();
        let mut mint = *out_mint;
        while Some(&mint) != stop_at && venues.len() < MAX_HOPS {
            let curve_address = pda::pump::bonding_curve(&mint).0;
            let Some(account) = self
                .rpc
                .get_account_with_commitment(&curve_address, self.rpc.commitment())
                .await
                .map_err(PumpClientError::from)?
                .value
                // Anyone can fund a system account at a curve address; only a
                // pump-owned one is a curve (as `create_v2` checks).
                .filter(|a| a.owner == crate::pump::ID)
            else {
                break;
            };
            let curve = decode_bonding_curve(&account.data)?;
            let quote = PumpSdk::resolve_quote_mint(curve.quote_mint);
            venues.push(if curve.complete {
                RouteVenue::Pool(pda::pump_amm::canonical_pool(&mint, &quote).0)
            } else {
                RouteVenue::Curve(mint)
            });
            mint = quote;
        }
        if venues.is_empty() {
            return Err(PumpClientError::AccountNotFound {
                name: "bonding_curve",
                address: pda::pump::bonding_curve(out_mint).0,
            });
        }
        venues.reverse();
        Ok(MultiHopRoute {
            in_mint: mint,
            out_mint: *out_mint,
            venues,
        })
    }

    /// Everything a `multi_hop_swap` of `in_mint` through `venues` (path
    /// order) needs to quote and build, in two batched fetches: both
    /// programs' globals and fee configs, each venue, vault / curve ATA
    /// balances, mint supplies and token programs.
    pub async fn fetch_multi_hop_quote_state(
        &self,
        in_mint: Pubkey,
        venues: &[RouteVenue],
    ) -> Result<MultiHopQuoteState> {
        // Round 1: configs, venues and curve base mints.
        let mut keys = vec![
            pda::pump::global().0,
            pda::pump::fee_config().0,
            pda::pump_amm::global_config().0,
            pda::pump_amm::fee_config().0,
        ];
        for venue in venues {
            match *venue {
                RouteVenue::Pool(pool) => keys.push(pool),
                RouteVenue::Curve(mint) => keys.extend([pda::pump::bonding_curve(&mint).0, mint]),
            }
        }
        let accounts = self.get_many(&keys).await?;
        let need = |i: usize, name: &'static str| required(&accounts, &keys, i, name);
        let global = decode_global(&need(0, "global")?.data)?;
        let pump_fee_config = decode_fee_config(&need(1, "fee_config")?.data)?;
        let global_config = decode_global_config(&need(2, "amm_global_config")?.data)?;
        let amm_fee_config = decode_fee_config(&need(3, "amm_fee_config")?.data)?;

        let mut token_programs = HashMap::new();
        let mut states = Vec::with_capacity(venues.len());
        let mut i = 4;
        for venue in venues {
            match *venue {
                RouteVenue::Pool(_) => {
                    states.push(VenueState::Pool(decode_pool(&need(i, "pool")?.data)?));
                    i += 1;
                }
                RouteVenue::Curve(mint) => {
                    let curve = decode_bonding_curve(&need(i, "bonding_curve")?.data)?;
                    let (supply, base_token_program) = Self::mint_info(need(i + 1, "mint")?)?;
                    token_programs.insert(mint, base_token_program);
                    states.push(VenueState::Curve(mint, curve, supply, base_token_program));
                    i += 2;
                }
            }
        }
        let route: Vec<MultiHopHop> = states
            .iter()
            .map(|s| {
                let (base_mint, quote_mint) = s.mints();
                MultiHopHop {
                    base_mint,
                    quote_mint,
                    venue: Pubkey::default(),
                    base_vault: Pubkey::default(),
                    quote_vault: Pubkey::default(),
                }
            })
            .collect();
        let (is_buy, out_mint) = PumpSdk::multi_hop_route(in_mint, &route)?;

        // Round 2: pool vaults and mints, curve base ATAs and quote mints.
        let mut keys = Vec::new();
        for state in &states {
            match state {
                VenueState::Pool(p) => keys.extend([
                    p.pool_base_token_account,
                    p.pool_quote_token_account,
                    p.base_mint,
                    p.quote_mint,
                ]),
                VenueState::Curve(mint, curve, _, base_token_program) => {
                    let venue = pda::pump::bonding_curve(mint).0;
                    keys.extend([
                        pda::associated_token(&venue, base_token_program, mint).0,
                        PumpSdk::resolve_quote_mint(curve.quote_mint),
                    ]);
                }
            }
        }
        let accounts = self.get_many(&keys).await?;
        let need = |i: usize, name: &'static str| required(&accounts, &keys, i, name);
        let mut hops = Vec::with_capacity(states.len());
        let mut i = 0;
        for state in states {
            match state {
                VenueState::Pool(pool) => {
                    let (base_mint_supply, base_token_program) =
                        Self::mint_info(need(i + 2, "base_mint")?)?;
                    token_programs.insert(pool.base_mint, base_token_program);
                    token_programs.insert(pool.quote_mint, need(i + 3, "quote_mint")?.owner);
                    hops.push(HopState::Pool {
                        base_reserve: token_amount(need(i, "pool_base_token_account")?)?,
                        quote_vault_balance: token_amount(need(
                            i + 1,
                            "pool_quote_token_account",
                        )?)?,
                        base_mint_supply,
                        pool,
                    });
                    i += 4;
                }
                VenueState::Curve(mint, curve, mint_supply, base_token_program) => {
                    let quote_token_program = need(i + 1, "quote_mint")?.owner;
                    token_programs.insert(
                        PumpSdk::resolve_quote_mint(curve.quote_mint),
                        quote_token_program,
                    );
                    hops.push(HopState::Curve {
                        mint,
                        base_token_program,
                        quote_token_program,
                        mint_supply,
                        base_ata_amount: token_amount(need(i, "associated_bonding_curve")?)?,
                        bonding_curve: curve,
                    });
                    i += 2;
                }
            }
        }
        let token_program = |mint: Pubkey| {
            token_programs
                .get(&mint)
                .copied()
                .ok_or(PumpClientError::AccountNotFound {
                    name: "mint",
                    address: mint,
                })
        };
        // The protocol leg is the first hop of a buy route and the last of a sell route.
        let (recipient, config) = match venues[if is_buy { 0 } else { venues.len() - 1 }] {
            RouteVenue::Pool(_) => (
                PumpSdk::buyback_fee_recipient_from_amm_global(&global_config),
                pda::pump_amm::global_config().0,
            ),
            RouteVenue::Curve(_) => (
                PumpSdk::buyback_fee_recipient_from_pump_global(&global),
                pda::pump::global().0,
            ),
        };
        Ok(MultiHopQuoteState {
            in_token_program: token_program(in_mint)?,
            out_token_program: token_program(out_mint)?,
            buyback_fee_recipient: recipient.ok_or(PumpClientError::AccountNotFound {
                name: "buyback_fee_recipient",
                address: config,
            })?,
            in_mint,
            out_mint,
            global,
            pump_fee_config,
            global_config,
            amm_fee_config,
            hops,
        })
    }

    /// Discover the route to `out_mint` from its root currency, quote
    /// `amount_in` of that currency (buy) or of `out_mint` (sell) with
    /// `slippage_bps`, and build [`PumpSdk::multi_hop_swap_instructions`] at
    /// the quoted `min_out`. Attach
    /// [`crate::constants::pump_amm::MULTI_HOP_COMPUTE_UNITS`]. A route rooted on
    /// a SOL curve buys with, and sells into, the wallet's native SOL (see
    /// [`PumpSdk::multi_hop_swap_instruction`]).
    pub async fn build_multi_hop_swap(
        &self,
        user: &Pubkey,
        out_mint: &Pubkey,
        is_buy: bool,
        amount_in: u64,
        slippage_bps: u16,
    ) -> Result<(Quote, Vec<Instruction>)> {
        let route = self.discover_route(out_mint, None).await?;
        self.build_multi_hop_swap_for_route(user, &route, is_buy, amount_in, slippage_bps)
            .await
    }

    /// [`Self::build_multi_hop_swap`] paying with (or ending in) `in_mint`
    /// instead of the root currency: the route stops at `in_mint`, so a coin
    /// quoted in one the user already holds trades in one hop.
    pub async fn build_multi_hop_swap_from(
        &self,
        user: &Pubkey,
        in_mint: &Pubkey,
        out_mint: &Pubkey,
        is_buy: bool,
        amount_in: u64,
        slippage_bps: u16,
    ) -> Result<(Quote, Vec<Instruction>)> {
        let route = self.discover_route(out_mint, Some(in_mint)).await?;
        if route.in_mint != *in_mint {
            // `in_mint` is not on `out_mint`'s quote chain.
            return Err(QuoteError::MultiHopDiscontinuousPath.into());
        }
        self.build_multi_hop_swap_for_route(user, &route, is_buy, amount_in, slippage_bps)
            .await
    }

    /// Quote and build a swap over `route` (discovered, or built by hand in
    /// buy order): `is_buy` trades `route.in_mint` → `route.out_mint`, a sell
    /// the reverse path.
    pub async fn build_multi_hop_swap_for_route(
        &self,
        user: &Pubkey,
        route: &MultiHopRoute,
        is_buy: bool,
        amount_in: u64,
        slippage_bps: u16,
    ) -> Result<(Quote, Vec<Instruction>)> {
        let (in_mint, venues) = route.path(is_buy);
        let state = self.fetch_multi_hop_quote_state(in_mint, &venues).await?;
        let quote = state.quote(&self.sdk, user, amount_in, slippage_bps)?;
        let ixs = state.instructions(&self.sdk, user, amount_in, quote.min_out)?;
        Ok((quote, ixs))
    }

    async fn get_many(&self, keys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        self.rpc
            .get_multiple_accounts(keys)
            .await
            .map_err(PumpClientError::from)
    }

    async fn get_account(&self, address: &Pubkey, name: &'static str) -> Result<Account> {
        let value = self
            .rpc
            .get_account_with_commitment(address, self.rpc.commitment())
            .await
            .map_err(PumpClientError::from)?
            .value;
        value.ok_or(PumpClientError::AccountNotFound {
            name,
            address: *address,
        })
    }
}

/// `accounts[i]` (fetched for `keys[i]`), or `AccountNotFound`.
fn required<'a>(
    accounts: &'a [Option<Account>],
    keys: &[Pubkey],
    i: usize,
    name: &'static str,
) -> Result<&'a Account> {
    accounts[i]
        .as_ref()
        .ok_or(PumpClientError::AccountNotFound {
            name,
            address: keys[i],
        })
}

/// Balance of an SPL Token / Token-2022 account.
fn token_amount(account: &Account) -> Result<u64> {
    Ok(decode::<anchor_spl::token_interface::TokenAccount>(&account.data)?.amount)
}

/// One venue of a `multi_hop_swap` route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteVenue {
    /// A pump-amm pool, by address.
    Pool(Pubkey),
    /// A bonding curve (SOL or token quoted), by its base mint.
    Curve(Pubkey),
}

/// A `multi_hop_swap` route in buy order (`in_mint` → `out_mint`), from
/// [`AsyncPumpClient::discover_route`] or built by hand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultiHopRoute {
    /// The root currency (or the `stop_at` coin).
    pub in_mint: Pubkey,
    pub out_mint: Pubkey,
    /// One venue per hop, buy order.
    pub venues: Vec<RouteVenue>,
}

impl MultiHopRoute {
    /// `(input mint, venues in path order)` for a buy (`in_mint` →
    /// `out_mint`) or a sell (the reverse).
    pub fn path(&self, is_buy: bool) -> (Pubkey, Vec<RouteVenue>) {
        if is_buy {
            (self.in_mint, self.venues.clone())
        } else {
            (self.out_mint, self.venues.iter().rev().copied().collect())
        }
    }
}

/// Round-1 state of a route venue: a pool, or a curve with
/// `(mint, curve, mint supply, base token program)`.
enum VenueState {
    Pool(Pool),
    Curve(Pubkey, BondingCurve, u64, Pubkey),
}

impl VenueState {
    fn mints(&self) -> (Pubkey, Pubkey) {
        match self {
            Self::Pool(p) => (p.base_mint, p.quote_mint),
            Self::Curve(mint, curve, ..) => (*mint, PumpSdk::resolve_quote_mint(curve.quote_mint)),
        }
    }
}

/// Owned state of one hop; [`RouteHop`] borrows it.
#[derive(Clone, Debug)]
pub enum HopState {
    Pool {
        pool: Pool,
        base_reserve: u64,
        quote_vault_balance: u64,
        base_mint_supply: u64,
    },
    Curve {
        mint: Pubkey,
        bonding_curve: BondingCurve,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        mint_supply: u64,
        base_ata_amount: u64,
    },
}

impl HopState {
    pub fn as_route_hop(&self) -> RouteHop<'_> {
        match self {
            Self::Pool {
                pool,
                base_reserve,
                quote_vault_balance,
                base_mint_supply,
            } => RouteHop::Pool {
                pool,
                base_reserve: *base_reserve,
                quote_vault_balance: *quote_vault_balance,
                base_mint_supply: *base_mint_supply,
            },
            Self::Curve {
                mint,
                bonding_curve,
                base_token_program,
                quote_token_program,
                mint_supply,
                base_ata_amount,
            } => RouteHop::Curve {
                mint: *mint,
                bonding_curve,
                base_token_program: *base_token_program,
                quote_token_program: *quote_token_program,
                mint_supply: *mint_supply,
                base_ata_amount: *base_ata_amount,
            },
        }
    }
}

/// A route's fetched state, from [`AsyncPumpClient::fetch_multi_hop_quote_state`]:
/// quote it any number of times, then build the swap.
#[derive(Clone, Debug)]
pub struct MultiHopQuoteState {
    pub in_mint: Pubkey,
    pub in_token_program: Pubkey,
    pub out_mint: Pubkey,
    pub out_token_program: Pubkey,
    /// Listed by the protocol-leg venue's config (first hop of a buy, last of a sell).
    pub buyback_fee_recipient: Pubkey,
    pub global: Global,
    pub pump_fee_config: FeeConfig,
    pub global_config: GlobalConfig,
    pub amm_fee_config: FeeConfig,
    /// Path order.
    pub hops: Vec<HopState>,
}

impl MultiHopQuoteState {
    fn route_hops(&self) -> Vec<RouteHop<'_>> {
        self.hops.iter().map(HopState::as_route_hop).collect()
    }

    /// [`PumpSdk::quote_multi_hop_swap`] over this state.
    pub fn quote(
        &self,
        sdk: &PumpSdk,
        user: &Pubkey,
        amount_in: u64,
        slippage_bps: u16,
    ) -> QuoteResult<Quote> {
        sdk.quote_multi_hop_swap(
            &self.global,
            &self.pump_fee_config,
            &self.global_config,
            &self.amm_fee_config,
            user,
            self.in_mint,
            &self.route_hops(),
            amount_in,
            slippage_bps,
        )
    }

    /// [`PumpSdk::multi_hop_swap_instructions`] over this state.
    pub fn instructions(
        &self,
        sdk: &PumpSdk,
        user: &Pubkey,
        amount_in: u64,
        min_amount_out: u64,
    ) -> QuoteResult<Vec<Instruction>> {
        let hops: Vec<MultiHopHop> = self.route_hops().iter().map(RouteHop::accounts).collect();
        sdk.multi_hop_swap_instructions(
            *user,
            self.in_mint,
            self.in_token_program,
            self.out_token_program,
            &hops,
            self.buyback_fee_recipient,
            amount_in,
            min_amount_out,
        )
    }
}

/// `create_v2` inputs for a pump-coin quote, from
/// [`AsyncPumpClient::fetch_pump_quote_create`].
#[derive(Clone, Debug)]
pub struct PumpQuoteCreate {
    /// The new curve's derived `virtual_quote_reserves` (for [`PumpSdk::initial_bonding_curve`]).
    pub virtual_quote_reserves: u64,
    /// The new curve's depth (Q's depth + 1).
    pub depth: u8,
    /// Q's token program: `create_v2`'s `quote_token_program`.
    pub quote_token_program: Pubkey,
    /// [`PumpSdk::create_v2_pump_quote_accounts`]: append to the `create_v2` instruction.
    pub remaining_accounts: Vec<AccountMeta>,
}

/// Snapshot for quoting and building one bonding-curve trade, from
/// [`AsyncPumpClient::fetch_curve_quote_state`].
#[derive(Clone, Debug)]
pub struct CurveQuoteState {
    pub global: Global,
    pub fee_config: FeeConfig,
    pub bonding_curve: BondingCurve,
    pub mint: Pubkey,
    pub mint_supply: u64,
    /// Owner of `mint`: Token-2022 for `create_v2` coins, SPL Token for legacy ones.
    pub base_token_program: Pubkey,
    /// Resolved quote (`Pubkey::default()` → wSOL).
    pub quote_mint: Pubkey,
    pub quote_token_program: Pubkey,
    /// The curve's base ATA balance (prices the v3 post-completion leg).
    pub base_ata_amount: u64,
    /// A listed `Global.buyback_fee_recipients` wallet, drawn at random.
    pub buyback_fee_recipient: Pubkey,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(deprecated)]
    use solana_sdk::{signature::Keypair, system_instruction};

    #[test]
    fn client_is_send_sync_clone() {
        fn assert_traits<T: Send + Sync + Clone>() {}
        assert_traits::<AsyncPumpClient>();
    }

    #[test]
    fn constructible_against_localhost() {
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let client = AsyncPumpClient::new(rpc);
        let _: &PumpSdk = client.sdk();
    }

    fn local_client() -> AsyncPumpClient {
        AsyncPumpClient::new(Arc::new(RpcClient::new(
            "http://localhost:8899".to_string(),
        )))
    }

    fn discriminator(ix: &Instruction) -> Option<u8> {
        ix.data.first().copied()
    }

    #[test]
    fn build_transaction_with_blockhash_prepends_compute_budget_ixs() {
        let client = local_client();
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let transfer = system_instruction::transfer(&payer.pubkey(), &recipient, 1);
        let blockhash = Hash::new_unique();

        let tx = client.build_transaction_with_blockhash(
            &[transfer],
            &payer.pubkey(),
            &[&payer],
            blockhash,
            Some(ComputeBudget {
                units: Some(200_000),
                micro_lamports_per_unit: Some(1_000),
            }),
        );

        assert_eq!(tx.message.instructions.len(), 3);
        let cb_program = solana_sdk::compute_budget::id();
        assert_eq!(
            tx.message.account_keys[tx.message.instructions[0].program_id_index as usize],
            cb_program,
        );
        assert_eq!(tx.message.instructions[0].data.first(), Some(&2u8));
        assert_eq!(
            tx.message.account_keys[tx.message.instructions[1].program_id_index as usize],
            cb_program,
        );
        assert_eq!(tx.message.instructions[1].data.first(), Some(&3u8));
    }

    #[test]
    fn build_transaction_with_blockhash_emits_only_requested_compute_budget_ixs() {
        let client = local_client();
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let transfer = system_instruction::transfer(&payer.pubkey(), &recipient, 1);

        let tx = client.build_transaction_with_blockhash(
            std::slice::from_ref(&transfer),
            &payer.pubkey(),
            &[&payer],
            Hash::new_unique(),
            Some(ComputeBudget {
                units: Some(50_000),
                micro_lamports_per_unit: None,
            }),
        );
        assert_eq!(tx.message.instructions.len(), 2);
        assert_eq!(tx.message.instructions[0].data.first(), Some(&2u8));

        let tx = client.build_transaction_with_blockhash(
            &[transfer],
            &payer.pubkey(),
            &[&payer],
            Hash::new_unique(),
            None,
        );
        assert_eq!(tx.message.instructions.len(), 1);
    }

    #[test]
    fn build_transaction_with_blockhash_signs_with_payer() {
        let client = local_client();
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let transfer = system_instruction::transfer(&payer.pubkey(), &recipient, 1);
        let blockhash = Hash::new_unique();

        let tx = client.build_transaction_with_blockhash(
            &[transfer],
            &payer.pubkey(),
            &[&payer],
            blockhash,
            None,
        );

        assert!(tx.is_signed());
        assert_eq!(tx.message.recent_blockhash, blockhash);
        assert_eq!(tx.message.account_keys[0], payer.pubkey());
    }

    #[test]
    fn compute_budget_helper_emits_known_discriminators() {
        let limit = ComputeBudgetInstruction::set_compute_unit_limit(0);
        let price = ComputeBudgetInstruction::set_compute_unit_price(0);
        assert_eq!(discriminator(&limit), Some(2));
        assert_eq!(discriminator(&price), Some(3));
    }
}
