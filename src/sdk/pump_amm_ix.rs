use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use anchor_lang::system_program;
use anchor_lang::{InstructionData, ToAccountMetas};

use crate::math::{QuoteError, QuoteResult};
use crate::pump_amm::{
    client as amm_client, types::OptionBool as AmmOptionBool, ID as PUMP_AMM_PROGRAM_ID,
};
use crate::state::pump_amm::{GlobalConfig, Pool};
use crate::token::create_associated_token_account_idempotent;
use crate::{constants, pda};

use super::PumpSdk;

/// Pubkeys derived once and threaded into both `Buy` and `Sell` AMM
/// instructions (which share the same account set apart from buy's extra
/// volume-accumulator slots).
struct AmmTradeAccounts {
    coin_creator_vault_authority: Pubkey,
    user_volume_accumulator: Pubkey,
    user_base_token_account: Pubkey,
    user_quote_token_account: Pubkey,
    pool_base_token_account: Pubkey,
    pool_quote_token_account: Pubkey,
    protocol_fee_recipient_token_account: Pubkey,
    coin_creator_vault_ata: Pubkey,
}

impl AmmTradeAccounts {
    fn derive(
        pool: Pubkey,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        coin_creator: Pubkey,
        protocol_fee_recipient: Pubkey,
    ) -> Self {
        let coin_creator_vault_authority =
            pda::pump_amm::coin_creator_vault_authority(&coin_creator).0;
        let user_volume_accumulator = pda::pump_amm::user_volume_accumulator(&user).0;
        let ata = |owner: &Pubkey, token_program: &Pubkey, mint: &Pubkey| {
            pda::associated_token(owner, token_program, mint).0
        };
        Self {
            coin_creator_vault_authority,
            user_volume_accumulator,
            user_base_token_account: ata(&user, &base_token_program, &base_mint),
            user_quote_token_account: ata(&user, &quote_token_program, &quote_mint),
            pool_base_token_account: ata(&pool, &base_token_program, &base_mint),
            pool_quote_token_account: ata(&pool, &quote_token_program, &quote_mint),
            protocol_fee_recipient_token_account: ata(
                &protocol_fee_recipient,
                &quote_token_program,
                &quote_mint,
            ),
            coin_creator_vault_ata: ata(
                &coin_creator_vault_authority,
                &quote_token_program,
                &quote_mint,
            ),
        }
    }
}

impl PumpSdk {
    /// `pump_amm` buy. Fees from [`GlobalConfig`], pool layout from [`Pool`]. Use default `coin_creator` to omit `pool_v2`.
    ///
    /// The program reads the cashback and `pool_v2` remaining accounts only
    /// while the trade's creator fee is non-zero and otherwise expects the
    /// buyback recipient first. These builders append them whenever the pool
    /// has a coin creator, so a trade whose creator fee is zero (a schedule
    /// with a zero creator rate, or a trade so small the fee rounds to zero)
    /// fails with `BuybackFeeRecipientNotAuthorized`. Size trades
    /// accordingly, or use the v2 builders (no remaining accounts).
    pub fn buy_amm_instruction(
        &self,
        pool: Pubkey,
        amm_global: &GlobalConfig,
        pool_state: &Pool,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        base_amount_out: u64,
        max_quote_amount_in: u64,
    ) -> Option<Instruction> {
        let (protocol_fee_recipient, buyback_fee_recipient) =
            Self::amm_fee_recipients_pair(amm_global)?;
        Some(self.buy_amm_instruction_with_recipients(
            pool,
            pool_state.base_mint,
            pool_state.quote_mint,
            base_token_program,
            quote_token_program,
            user,
            pool_state.coin_creator,
            protocol_fee_recipient,
            buyback_fee_recipient,
            pool_state.is_cashback_coin,
            base_amount_out,
            max_quote_amount_in,
        ))
    }

    pub(crate) fn buy_amm_instruction_with_recipients(
        &self,
        pool: Pubkey,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        coin_creator: Pubkey,
        protocol_fee_recipient: Pubkey,
        buyback_fee_recipient: Pubkey,
        is_cashback_coin: bool,
        base_amount_out: u64,
        max_quote_amount_in: u64,
    ) -> Instruction {
        let a = AmmTradeAccounts::derive(
            pool,
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            coin_creator,
            protocol_fee_recipient,
        );
        let accounts = amm_client::accounts::Buy {
            pool,
            user,
            global_config: pda::pump_amm::global_config().0,
            base_mint,
            quote_mint,
            user_base_token_account: a.user_base_token_account,
            user_quote_token_account: a.user_quote_token_account,
            pool_base_token_account: a.pool_base_token_account,
            pool_quote_token_account: a.pool_quote_token_account,
            protocol_fee_recipient,
            protocol_fee_recipient_token_account: a.protocol_fee_recipient_token_account,
            base_token_program,
            quote_token_program,
            system_program: system_program::ID,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            event_authority: pda::pump_amm::event_authority().0,
            program: PUMP_AMM_PROGRAM_ID,
            coin_creator_vault_ata: a.coin_creator_vault_ata,
            coin_creator_vault_authority: a.coin_creator_vault_authority,
            global_volume_accumulator: pda::pump_amm::global_volume_accumulator().0,
            user_volume_accumulator: a.user_volume_accumulator,
            fee_config: pda::pump_amm::fee_config().0,
            fee_program: constants::FEE_PROGRAM_ID,
        };
        let args = amm_client::args::Buy {
            base_amount_out,
            max_quote_amount_in,
            track_volume: AmmOptionBool(true),
        };
        let mut metas = accounts.to_account_metas(None);
        if is_cashback_coin {
            metas.push(AccountMeta::new(
                pda::associated_token(
                    &a.user_volume_accumulator,
                    &quote_token_program,
                    &quote_mint,
                )
                .0,
                false,
            ));
        }
        if coin_creator != Pubkey::default() {
            metas.push(AccountMeta::new_readonly(
                pda::pump_amm::pool_v2(&base_mint).0,
                false,
            ));
        }
        metas.push(AccountMeta::new(buyback_fee_recipient, false));
        metas.push(AccountMeta::new(
            pda::associated_token(&buyback_fee_recipient, &quote_token_program, &quote_mint).0,
            false,
        ));
        Instruction {
            program_id: PUMP_AMM_PROGRAM_ID,
            accounts: metas,
            data: args.data(),
        }
    }

    /// [`Self::buy_amm_instruction`] plus idempotent user base ATA create.
    pub fn buy_amm_instructions(
        &self,
        pool: Pubkey,
        amm_global: &GlobalConfig,
        pool_state: &Pool,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        base_amount_out: u64,
        max_quote_amount_in: u64,
    ) -> Option<Vec<Instruction>> {
        let buy = self.buy_amm_instruction(
            pool,
            amm_global,
            pool_state,
            base_token_program,
            quote_token_program,
            user,
            base_amount_out,
            max_quote_amount_in,
        )?;
        Some(vec![
            create_associated_token_account_idempotent(
                &user,
                &user,
                &pool_state.base_mint,
                &base_token_program,
            ),
            buy,
        ])
    }

    /// `pump_amm` sell (remaining accounts differ from buy for cashback / buyback).
    /// Same creator-fee caveat as [`Self::buy_amm_instruction`].
    pub fn sell_amm_instruction(
        &self,
        pool: Pubkey,
        amm_global: &GlobalConfig,
        pool_state: &Pool,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        base_amount_in: u64,
        min_quote_amount_out: u64,
    ) -> Option<Instruction> {
        let (protocol_fee_recipient, buyback_fee_recipient) =
            Self::amm_fee_recipients_pair(amm_global)?;
        Some(self.sell_amm_instruction_with_recipients(
            pool,
            pool_state.base_mint,
            pool_state.quote_mint,
            base_token_program,
            quote_token_program,
            user,
            pool_state.coin_creator,
            protocol_fee_recipient,
            buyback_fee_recipient,
            pool_state.is_cashback_coin,
            base_amount_in,
            min_quote_amount_out,
        ))
    }

    pub(crate) fn sell_amm_instruction_with_recipients(
        &self,
        pool: Pubkey,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        coin_creator: Pubkey,
        protocol_fee_recipient: Pubkey,
        buyback_fee_recipient: Pubkey,
        is_cashback_coin: bool,
        base_amount_in: u64,
        min_quote_amount_out: u64,
    ) -> Instruction {
        let a = AmmTradeAccounts::derive(
            pool,
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            coin_creator,
            protocol_fee_recipient,
        );
        let accounts = amm_client::accounts::Sell {
            pool,
            user,
            global_config: pda::pump_amm::global_config().0,
            base_mint,
            quote_mint,
            user_base_token_account: a.user_base_token_account,
            user_quote_token_account: a.user_quote_token_account,
            pool_base_token_account: a.pool_base_token_account,
            pool_quote_token_account: a.pool_quote_token_account,
            protocol_fee_recipient,
            protocol_fee_recipient_token_account: a.protocol_fee_recipient_token_account,
            base_token_program,
            quote_token_program,
            system_program: system_program::ID,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            event_authority: pda::pump_amm::event_authority().0,
            program: PUMP_AMM_PROGRAM_ID,
            coin_creator_vault_ata: a.coin_creator_vault_ata,
            coin_creator_vault_authority: a.coin_creator_vault_authority,
            fee_config: pda::pump_amm::fee_config().0,
            fee_program: constants::FEE_PROGRAM_ID,
        };
        let args = amm_client::args::Sell {
            base_amount_in,
            min_quote_amount_out,
        };
        let mut metas = accounts.to_account_metas(None);
        if is_cashback_coin {
            metas.push(AccountMeta::new(
                pda::associated_token(
                    &a.user_volume_accumulator,
                    &quote_token_program,
                    &quote_mint,
                )
                .0,
                false,
            ));
            metas.push(AccountMeta::new(a.user_volume_accumulator, false));
        }
        if coin_creator != Pubkey::default() {
            metas.push(AccountMeta::new_readonly(
                pda::pump_amm::pool_v2(&base_mint).0,
                false,
            ));
        }
        metas.push(AccountMeta::new_readonly(buyback_fee_recipient, false));
        metas.push(AccountMeta::new(
            pda::associated_token(&buyback_fee_recipient, &quote_token_program, &quote_mint).0,
            false,
        ));
        Instruction {
            program_id: PUMP_AMM_PROGRAM_ID,
            accounts: metas,
            data: args.data(),
        }
    }

    /// [`Self::sell_amm_instruction`] plus idempotent user quote ATA create.
    pub fn sell_amm_instructions(
        &self,
        pool: Pubkey,
        amm_global: &GlobalConfig,
        pool_state: &Pool,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        base_amount_in: u64,
        min_quote_amount_out: u64,
    ) -> Option<Vec<Instruction>> {
        let sell = self.sell_amm_instruction(
            pool,
            amm_global,
            pool_state,
            base_token_program,
            quote_token_program,
            user,
            base_amount_in,
            min_quote_amount_out,
        )?;
        Some(vec![
            create_associated_token_account_idempotent(
                &user,
                &user,
                &pool_state.quote_mint,
                &quote_token_program,
            ),
            sell,
        ])
    }

    /// `collect_coin_creator_fee`. Sweeps the AMM coin-creator vault's
    /// `quote_mint` balance into the coin creator's own quote ATA.
    /// Works for any supported quote mint (wSOL or non-SOL).
    pub fn collect_coin_creator_fee_instruction(
        &self,
        coin_creator: Pubkey,
        quote_mint: Pubkey,
        quote_token_program: Pubkey,
    ) -> Instruction {
        let coin_creator_vault_authority =
            pda::pump_amm::coin_creator_vault_authority(&coin_creator).0;
        let coin_creator_vault_ata = pda::associated_token(
            &coin_creator_vault_authority,
            &quote_token_program,
            &quote_mint,
        )
        .0;
        let coin_creator_token_account =
            pda::associated_token(&coin_creator, &quote_token_program, &quote_mint).0;
        let accounts = amm_client::accounts::CollectCoinCreatorFee {
            quote_mint,
            quote_token_program,
            coin_creator,
            coin_creator_vault_authority,
            coin_creator_vault_ata,
            coin_creator_token_account,
            event_authority: pda::pump_amm::event_authority().0,
            program: PUMP_AMM_PROGRAM_ID,
        };
        Instruction {
            program_id: PUMP_AMM_PROGRAM_ID,
            accounts: accounts.to_account_metas(None),
            data: amm_client::args::CollectCoinCreatorFee.data(),
        }
    }

    /// [`Self::collect_coin_creator_fee_instruction`] with an optional
    /// idempotent ATA create for the coin creator's quote ATA prepended.
    /// `payer` only matters when the ATA create runs.
    pub fn collect_coin_creator_fee_instructions(
        &self,
        payer: Pubkey,
        coin_creator: Pubkey,
        quote_mint: Pubkey,
        quote_token_program: Pubkey,
        create_coin_creator_ata: bool,
    ) -> Vec<Instruction> {
        let mut ixs = Vec::with_capacity(2);
        if create_coin_creator_ata {
            ixs.push(create_associated_token_account_idempotent(
                &payer,
                &coin_creator,
                &quote_mint,
                &quote_token_program,
            ));
        }
        ixs.push(self.collect_coin_creator_fee_instruction(
            coin_creator,
            quote_mint,
            quote_token_program,
        ));
        ixs
    }

    /// The pool's own address from its stored seeds (`create_pool`).
    fn pool_address(pool_state: &Pool) -> Pubkey {
        pda::pump_amm::pool(
            pool_state.index,
            &pool_state.creator,
            &pool_state.base_mint,
            &pool_state.quote_mint,
        )
        .0
    }

    /// `pump_amm` v2 trades share one 17-account set (`TradeV2`): no fee
    /// recipients, no creator vault. The protocol fee (less its buyback
    /// slice) and the coin-creator fee stay in the pool's quote vault until
    /// `sweep_*_fee` pays them out; the buyback slice is paid in the trade to
    /// `buyback_fee_recipient`'s quote ATA (index 14), which must already
    /// exist. Any pool but a cashback coin's (a mayhem pool takes no buyback
    /// slice, a non-pump pool pays the flat fees); the quote helpers
    /// (`buy_quote_amm_*`, `sell_quote_amm`) apply.
    fn amm_v2_instruction(
        pool_state: &Pool,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        buyback_fee_recipient: Pubkey,
        data: Vec<u8>,
    ) -> Instruction {
        let ata = |owner: &Pubkey, token_program: &Pubkey, mint: &Pubkey| {
            pda::associated_token(owner, token_program, mint).0
        };
        let accounts = amm_client::accounts::BuyV2 {
            pool: Self::pool_address(pool_state),
            user,
            global_config: pda::pump_amm::global_config().0,
            base_mint: pool_state.base_mint,
            quote_mint: pool_state.quote_mint,
            user_base_token_account: ata(&user, &base_token_program, &pool_state.base_mint),
            user_quote_token_account: ata(&user, &quote_token_program, &pool_state.quote_mint),
            pool_base_token_account: pool_state.pool_base_token_account,
            pool_quote_token_account: pool_state.pool_quote_token_account,
            base_token_program,
            quote_token_program,
            system_program: system_program::ID,
            user_volume_accumulator: pda::pump_amm::user_volume_accumulator(&user).0,
            fee_config: pda::pump_amm::fee_config().0,
            buyback_fee_recipient: ata(
                &buyback_fee_recipient,
                &quote_token_program,
                &pool_state.quote_mint,
            ),
            event_authority: pda::pump_amm::event_authority().0,
            program: PUMP_AMM_PROGRAM_ID,
        };
        Instruction {
            program_id: PUMP_AMM_PROGRAM_ID,
            accounts: accounts.to_account_metas(None),
            data,
        }
    }

    /// `buy_v2`: `base_amount_out` tokens for at most `max_quote_amount_in`.
    /// `buyback_fee_recipient` is a listed `GlobalConfig.buyback_fee_recipients`
    /// wallet (see [`Self::buyback_fee_recipient_from_amm_global`]).
    pub fn buy_amm_v2_instruction(
        &self,
        pool_state: &Pool,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        buyback_fee_recipient: Pubkey,
        base_amount_out: u64,
        max_quote_amount_in: u64,
    ) -> Instruction {
        Self::amm_v2_instruction(
            pool_state,
            base_token_program,
            quote_token_program,
            user,
            buyback_fee_recipient,
            amm_client::args::BuyV2 {
                base_amount_out,
                max_quote_amount_in,
            }
            .data(),
        )
    }

    /// `buy_exact_quote_in_v2`: spend `spendable_quote_in` for at least
    /// `min_base_amount_out`. Accounts as [`Self::buy_amm_v2_instruction`].
    pub fn buy_exact_quote_in_amm_v2_instruction(
        &self,
        pool_state: &Pool,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        buyback_fee_recipient: Pubkey,
        spendable_quote_in: u64,
        min_base_amount_out: u64,
    ) -> Instruction {
        Self::amm_v2_instruction(
            pool_state,
            base_token_program,
            quote_token_program,
            user,
            buyback_fee_recipient,
            amm_client::args::BuyExactQuoteInV2 {
                spendable_quote_in,
                min_base_amount_out,
            }
            .data(),
        )
    }

    /// `sell_v2`: `base_amount_in` tokens for at least `min_quote_amount_out`.
    /// Accounts as [`Self::buy_amm_v2_instruction`].
    pub fn sell_amm_v2_instruction(
        &self,
        pool_state: &Pool,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        buyback_fee_recipient: Pubkey,
        base_amount_in: u64,
        min_quote_amount_out: u64,
    ) -> Instruction {
        Self::amm_v2_instruction(
            pool_state,
            base_token_program,
            quote_token_program,
            user,
            buyback_fee_recipient,
            amm_client::args::SellV2 {
                base_amount_in,
                min_quote_amount_out,
            }
            .data(),
        )
    }

    /// [`Self::buy_amm_v2_instruction`] plus idempotent user base ATA create.
    pub fn buy_amm_v2_instructions(
        &self,
        pool_state: &Pool,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        buyback_fee_recipient: Pubkey,
        base_amount_out: u64,
        max_quote_amount_in: u64,
    ) -> Vec<Instruction> {
        vec![
            create_associated_token_account_idempotent(
                &user,
                &user,
                &pool_state.base_mint,
                &base_token_program,
            ),
            self.buy_amm_v2_instruction(
                pool_state,
                base_token_program,
                quote_token_program,
                user,
                buyback_fee_recipient,
                base_amount_out,
                max_quote_amount_in,
            ),
        ]
    }

    /// [`Self::sell_amm_v2_instruction`] plus idempotent user quote ATA create.
    pub fn sell_amm_v2_instructions(
        &self,
        pool_state: &Pool,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        buyback_fee_recipient: Pubkey,
        base_amount_in: u64,
        min_quote_amount_out: u64,
    ) -> Vec<Instruction> {
        vec![
            create_associated_token_account_idempotent(
                &user,
                &user,
                &pool_state.quote_mint,
                &quote_token_program,
            ),
            self.sell_amm_v2_instruction(
                pool_state,
                base_token_program,
                quote_token_program,
                user,
                buyback_fee_recipient,
                base_amount_in,
                min_quote_amount_out,
            ),
        ]
    }

    /// `sweep_creator_fee` (permissionless): pays `Pool.creator_fees` from the
    /// pool's quote vault into the coin-creator vault authority's quote ATA
    /// (`creator_vault` PDA of `pool.coin_creator`, the same account
    /// `collect_coin_creator_fee` drains). `payer` funds the realloc and any
    /// missing ATA. The programs refuse `admin_cto_pool`, fee-sharing setup and
    /// pump-fees `update_fee_shares(_v2)` while this bucket holds fees, so
    /// prepend it in the same transaction on a graduated coin with a v2 trade.
    /// Sweep before [`Self::update_fee_shares_instruction`] on a graduated coin
    /// (else `PoolCreatorFeesNotSwept`).
    pub fn sweep_pool_creator_fee_instruction(
        &self,
        payer: Pubkey,
        pool_state: &Pool,
        quote_token_program: Pubkey,
    ) -> Instruction {
        let pool = Self::pool_address(pool_state);
        let recipient = pda::pump_amm::coin_creator_vault_authority(&pool_state.coin_creator).0;
        let accounts = amm_client::accounts::SweepCreatorFee {
            payer,
            global_config: pda::pump_amm::global_config().0,
            pool,
            quote_mint: pool_state.quote_mint,
            quote_token_program,
            pool_quote_token_account: pool_state.pool_quote_token_account,
            recipient,
            recipient_token_account: pda::associated_token(
                &recipient,
                &quote_token_program,
                &pool_state.quote_mint,
            )
            .0,
            system_program: system_program::ID,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            event_authority: pda::pump_amm::event_authority().0,
            program: PUMP_AMM_PROGRAM_ID,
        };
        Instruction {
            program_id: PUMP_AMM_PROGRAM_ID,
            accounts: accounts.to_account_metas(None),
            data: amm_client::args::SweepCreatorFee.data(),
        }
    }

    pub fn transfer_creator_fees_to_pump_v2_instruction(
        &self,
        payer: Pubkey,
        coin_creator: Pubkey,
        quote_mint: Pubkey,
        token_program: Pubkey,
    ) -> Instruction {
        let coin_creator_vault_authority =
            pda::pump_amm::coin_creator_vault_authority(&coin_creator).0;
        let coin_creator_vault_ata =
            pda::associated_token(&coin_creator_vault_authority, &token_program, &quote_mint).0;
        let pump_creator_vault = pda::pump::creator_vault(&coin_creator).0;
        let pump_creator_vault_ata =
            pda::associated_token(&pump_creator_vault, &token_program, &quote_mint).0;
        let accounts = amm_client::accounts::TransferCreatorFeesToPumpV2 {
            payer,
            quote_mint,
            token_program,
            system_program: system_program::ID,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            coin_creator,
            coin_creator_vault_authority,
            coin_creator_vault_ata,
            pump_creator_vault,
            pump_creator_vault_ata,
            event_authority: pda::pump_amm::event_authority().0,
            program: PUMP_AMM_PROGRAM_ID,
        };
        Instruction {
            program_id: PUMP_AMM_PROGRAM_ID,
            accounts: accounts.to_account_metas(None),
            data: amm_client::args::TransferCreatorFeesToPumpV2.data(),
        }
    }
}

/// One 5-account hop group of `multi_hop_swap`, in path order: base mint,
/// quote mint, venue (pool or bonding curve), base vault, quote vault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MultiHopHop {
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub venue: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
}

impl MultiHopHop {
    /// A canonical pump pool hop.
    pub fn pool(pool: &Pool) -> Self {
        Self {
            base_mint: pool.base_mint,
            quote_mint: pool.quote_mint,
            venue: PumpSdk::pool_address(pool),
            base_vault: pool.pool_base_token_account,
            quote_vault: pool.pool_quote_token_account,
        }
    }

    /// A bonding-curve hop; `quote_mint` is the curve's stored quote (the zero
    /// key of a SOL curve resolves to wSOL, as in the v3 trades).
    pub fn curve(
        mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
    ) -> Self {
        let venue = pda::pump::bonding_curve(&mint).0;
        let quote_mint = PumpSdk::resolve_quote_mint(quote_mint);
        Self {
            base_mint: mint,
            quote_mint,
            venue,
            base_vault: pda::associated_token(&venue, &base_token_program, &mint).0,
            quote_vault: pda::associated_token(&venue, &quote_token_program, &quote_mint).0,
        }
    }

    fn metas(&self) -> [AccountMeta; 5] {
        [
            AccountMeta::new_readonly(self.base_mint, false),
            AccountMeta::new_readonly(self.quote_mint, false),
            AccountMeta::new(self.venue, false),
            AccountMeta::new(self.base_vault, false),
            AccountMeta::new(self.quote_vault, false),
        ]
    }
}

impl PumpSdk {
    /// `(route_is_buy, out_mint)` of a route starting at `in_mint`: each hop
    /// buys when fed its quote and sells when fed its base, and every hop must
    /// trade in the first hop's direction (the program's route checks).
    pub fn multi_hop_route(in_mint: Pubkey, hops: &[MultiHopHop]) -> QuoteResult<(bool, Pubkey)> {
        let mut mint = in_mint;
        let mut direction = None;
        for hop in hops {
            let is_buy = if mint == hop.quote_mint {
                true
            } else if mint == hop.base_mint {
                false
            } else {
                return Err(QuoteError::MultiHopDiscontinuousPath);
            };
            if *direction.get_or_insert(is_buy) != is_buy {
                return Err(QuoteError::MultiHopMixedDirection);
            }
            mint = if is_buy {
                hop.base_mint
            } else {
                hop.quote_mint
            };
        }
        let is_buy = direction.ok_or(QuoteError::MultiHopDiscontinuousPath)?;
        Ok((is_buy, mint))
    }

    /// pump-amm `multi_hop_swap`: exact-in `amount_in` of `in_mint` through
    /// pump pools and bonding curves (SOL curves trade wSOL), slippage checked once on
    /// the final amount. `in_token_program` / `out_token_program` own the
    /// route's first and last mints.
    ///
    /// `buyback_fee_recipient` is a wallet listed by the protocol leg's venue
    /// (the first hop of a buy route, the last of a sell route): pump-amm
    /// `GlobalConfig` for a pool, pump `Global` for a curve. Its ATA for the
    /// user's currency must exist ([`Self::multi_hop_swap_instructions`]
    /// creates it). Every touched pool and curve is auto-extended at the
    /// user's expense. Budget ~30k CU per hop
    /// ([`constants::pump_amm::MULTI_HOP_COMPUTE_UNITS`]); routes over 3 hops
    /// need a v0 transaction with a lookup table.
    ///
    /// SOL curve legs: a buy route starting on a SOL curve pays native SOL
    /// from `user`'s wallet and a sell route ending on one pays lamports to it;
    /// the user's wSOL ATA at that end is a placeholder read for its mint
    /// (must exist, never debited or credited, so no wrap / `sync_native`).
    /// Budget `amount_in` plus rent (volume accumulator, curve extension) on a
    /// SOL buy. A route starting or ending on a wSOL pool still moves wSOL.
    #[allow(clippy::too_many_arguments)]
    pub fn multi_hop_swap_instruction(
        &self,
        user: Pubkey,
        in_mint: Pubkey,
        in_token_program: Pubkey,
        out_token_program: Pubkey,
        hops: &[MultiHopHop],
        buyback_fee_recipient: Pubkey,
        amount_in: u64,
        min_amount_out: u64,
    ) -> QuoteResult<Instruction> {
        if amount_in == 0 || min_amount_out == 0 {
            return Err(QuoteError::ZeroAmount);
        }
        let (is_buy, out_mint) = Self::multi_hop_route(in_mint, hops)?;
        // The buyback slice is paid in the user's currency.
        let (fee_mint, fee_token_program) = if is_buy {
            (in_mint, in_token_program)
        } else {
            (out_mint, out_token_program)
        };
        let ata = |owner: &Pubkey, token_program: &Pubkey, mint: &Pubkey| {
            pda::associated_token(owner, token_program, mint).0
        };
        let mut metas = amm_client::accounts::MultiHopSwap {
            user,
            user_in_token_account: ata(&user, &in_token_program, &in_mint),
            user_out_token_account: ata(&user, &out_token_program, &out_mint),
            global_config: pda::pump_amm::global_config().0,
            fee_config: pda::pump_amm::fee_config().0,
            user_volume_accumulator: pda::pump_amm::user_volume_accumulator(&user).0,
            buyback_fee_recipient: ata(&buyback_fee_recipient, &fee_token_program, &fee_mint),
            token_program: constants::SPL_TOKEN_PROGRAM_ID,
            token_2022_program: constants::SPL_TOKEN_2022_PROGRAM_ID,
            system_program: system_program::ID,
            event_authority: pda::pump_amm::event_authority().0,
            program: PUMP_AMM_PROGRAM_ID,
            pump_program: crate::pump::ID,
            pump_global: pda::pump::global().0,
            pump_fee_config: pda::pump::fee_config().0,
            pump_event_authority: pda::pump::event_authority().0,
        }
        .to_account_metas(None);
        metas.extend(hops.iter().flat_map(MultiHopHop::metas));
        Ok(Instruction {
            program_id: PUMP_AMM_PROGRAM_ID,
            accounts: metas,
            data: amm_client::args::MultiHopSwap {
                amount_in,
                min_amount_out,
            }
            .data(),
        })
    }

    /// [`Self::multi_hop_swap_instruction`] behind idempotent creates of the
    /// buyback recipient's ATA, the user's output ATA and, on a buy route
    /// starting on a SOL curve, the user's (undebited) wSOL ATA (the user pays).
    #[allow(clippy::too_many_arguments)]
    pub fn multi_hop_swap_instructions(
        &self,
        user: Pubkey,
        in_mint: Pubkey,
        in_token_program: Pubkey,
        out_token_program: Pubkey,
        hops: &[MultiHopHop],
        buyback_fee_recipient: Pubkey,
        amount_in: u64,
        min_amount_out: u64,
    ) -> QuoteResult<Vec<Instruction>> {
        let swap = self.multi_hop_swap_instruction(
            user,
            in_mint,
            in_token_program,
            out_token_program,
            hops,
            buyback_fee_recipient,
            amount_in,
            min_amount_out,
        )?;
        let (is_buy, out_mint) = Self::multi_hop_route(in_mint, hops)?;
        let (fee_mint, fee_token_program) = if is_buy {
            (in_mint, in_token_program)
        } else {
            (out_mint, out_token_program)
        };
        let mut ixs = vec![create_associated_token_account_idempotent(
            &user,
            &buyback_fee_recipient,
            &fee_mint,
            &fee_token_program,
        )];
        // A SOL curve first leg takes lamports, but the wSOL ATA must still exist.
        let starts_on_sol_curve = is_buy
            && in_mint == constants::NATIVE_MINT
            && hops[0].venue == pda::pump::bonding_curve(&hops[0].base_mint).0;
        if starts_on_sol_curve {
            ixs.push(create_associated_token_account_idempotent(
                &user,
                &user,
                &in_mint,
                &in_token_program,
            ));
        }
        ixs.push(create_associated_token_account_idempotent(
            &user,
            &user,
            &out_mint,
            &out_token_program,
        ));
        ixs.push(swap);
        Ok(ixs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::pump_amm::PoolFromIdl;
    use anchor_lang::Discriminator;

    fn key(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    fn pool_state() -> Pool {
        Pool::new(PoolFromIdl {
            index: 0,
            creator: pda::pump::pool_authority(&key(1)).0,
            base_mint: key(1),
            quote_mint: key(2),
            pool_base_token_account: key(3),
            pool_quote_token_account: key(4),
            coin_creator: key(7),
            ..Default::default()
        })
    }

    fn pool_key() -> Pubkey {
        pda::pump_amm::canonical_pool(&key(1), &key(2)).0
    }

    #[test]
    fn amm_v2_trades_share_the_seventeen_account_layout() {
        let sdk = PumpSdk::new();
        let (pool, user, buyback) = (pool_key(), key(5), key(6));
        let (t22, tp) = (
            constants::SPL_TOKEN_2022_PROGRAM_ID,
            constants::SPL_TOKEN_PROGRAM_ID,
        );
        let state = pool_state();
        let buy = sdk.buy_amm_v2_instruction(&state, t22, tp, user, buyback, 10, 20);
        let exact =
            sdk.buy_exact_quote_in_amm_v2_instruction(&state, t22, tp, user, buyback, 10, 20);
        let sell = sdk.sell_amm_v2_instruction(&state, t22, tp, user, buyback, 10, 20);
        assert_eq!(buy.accounts, exact.accounts);
        assert_eq!(buy.accounts, sell.accounts);
        let keys: Vec<Pubkey> = buy.accounts.iter().map(|m| m.pubkey).collect();
        assert_eq!(
            keys,
            [
                pool,
                user,
                pda::pump_amm::global_config().0,
                key(1),
                key(2),
                pda::associated_token(&user, &t22, &key(1)).0,
                pda::associated_token(&user, &tp, &key(2)).0,
                key(3),
                key(4),
                t22,
                tp,
                system_program::ID,
                pda::pump_amm::user_volume_accumulator(&user).0,
                pda::pump_amm::fee_config().0,
                pda::associated_token(&buyback, &tp, &key(2)).0,
                pda::pump_amm::event_authority().0,
                PUMP_AMM_PROGRAM_ID,
            ]
        );
        let writable: Vec<bool> = buy.accounts.iter().map(|m| m.is_writable).collect();
        assert_eq!(
            writable,
            [
                true, true, false, false, false, true, true, true, true, false, false, false, true,
                false, true, false, false
            ]
        );
        assert!(buy.accounts[1].is_signer);
        assert_eq!(&buy.data[..8], amm_client::args::BuyV2::DISCRIMINATOR);
        assert_eq!(
            &exact.data[..8],
            amm_client::args::BuyExactQuoteInV2::DISCRIMINATOR
        );
        assert_eq!(&sell.data[..8], amm_client::args::SellV2::DISCRIMINATOR);
        assert_eq!(
            sdk.buy_amm_v2_instructions(&state, t22, tp, user, buyback, 1, 1)
                .len(),
            2
        );
    }

    #[test]
    fn sweep_pool_creator_fee_pays_the_vault_authority_ata() {
        let state = pool_state();
        let ix = PumpSdk::new().sweep_pool_creator_fee_instruction(
            key(8),
            &state,
            constants::SPL_TOKEN_PROGRAM_ID,
        );
        assert_eq!(ix.accounts[2].pubkey, pool_key());
        assert_eq!(ix.accounts.len(), 12);
        assert_eq!(&ix.data, amm_client::args::SweepCreatorFee::DISCRIMINATOR);
        let authority = pda::pump_amm::coin_creator_vault_authority(&key(7)).0;
        assert_eq!(ix.accounts[6].pubkey, authority);
        assert!(!ix.accounts[6].is_writable);
        assert_eq!(
            ix.accounts[7].pubkey,
            pda::associated_token(&authority, &constants::SPL_TOKEN_PROGRAM_ID, &key(2)).0
        );
        assert!(ix.accounts[7].is_writable);
        assert_eq!(ix.accounts[5].pubkey, key(4));
        assert!(ix.accounts[0].is_signer && ix.accounts[2].is_writable);
    }

    fn hop(base: u8, quote: u8) -> MultiHopHop {
        MultiHopHop {
            base_mint: key(base),
            quote_mint: key(quote),
            venue: key(base + 100),
            base_vault: key(base + 110),
            quote_vault: key(base + 120),
        }
    }

    #[test]
    fn multi_hop_swap_layout() {
        let sdk = PumpSdk::new();
        let (user, buyback) = (key(5), key(6));
        let (t22, tp) = (
            constants::SPL_TOKEN_2022_PROGRAM_ID,
            constants::SPL_TOKEN_PROGRAM_ID,
        );
        // Buy route USDC(2) -> Q(10) -> C(11); the buyback ATA is in USDC.
        let hops = [hop(10, 2), hop(11, 10)];
        let ix = sdk
            .multi_hop_swap_instruction(user, key(2), tp, t22, &hops, buyback, 7, 3)
            .unwrap();
        assert_eq!(ix.program_id, PUMP_AMM_PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 16 + 5 * hops.len());
        let keys: Vec<Pubkey> = ix.accounts.iter().map(|m| m.pubkey).collect();
        assert_eq!(
            keys[..16],
            [
                user,
                pda::associated_token(&user, &tp, &key(2)).0,
                pda::associated_token(&user, &t22, &key(11)).0,
                pda::pump_amm::global_config().0,
                pda::pump_amm::fee_config().0,
                pda::pump_amm::user_volume_accumulator(&user).0,
                pda::associated_token(&buyback, &tp, &key(2)).0,
                tp,
                t22,
                system_program::ID,
                pda::pump_amm::event_authority().0,
                PUMP_AMM_PROGRAM_ID,
                crate::pump::ID,
                pda::pump::global().0,
                pda::pump::fee_config().0,
                pda::pump::event_authority().0,
            ]
        );
        let writable: Vec<bool> = ix.accounts.iter().map(|m| m.is_writable).collect();
        assert_eq!(
            writable[..16],
            [
                true, true, true, false, false, true, true, false, false, false, false, false,
                false, false, false, false
            ]
        );
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts.iter().filter(|m| m.is_signer).count(), 1);
        for (i, h) in hops.iter().enumerate() {
            let group = &ix.accounts[16 + 5 * i..21 + 5 * i];
            let keys: Vec<Pubkey> = group.iter().map(|m| m.pubkey).collect();
            assert_eq!(
                keys,
                [
                    h.base_mint,
                    h.quote_mint,
                    h.venue,
                    h.base_vault,
                    h.quote_vault
                ]
            );
            let writable: Vec<bool> = group.iter().map(|m| m.is_writable).collect();
            assert_eq!(writable, [false, false, true, true, true]);
        }
        assert_eq!(ix.data[..8], [43, 100, 73, 19, 233, 246, 111, 148]);
        assert_eq!(&ix.data[..8], amm_client::args::MultiHopSwap::DISCRIMINATOR);
        assert_eq!(
            ix.data[8..],
            [7u64.to_le_bytes(), 3u64.to_le_bytes()].concat()
        );

        // Sell route C(11) -> Q(10) -> USDC(2): the buyback ATA is in the out mint.
        let sell_hops = [hop(11, 10), hop(10, 2)];
        let sell = sdk
            .multi_hop_swap_instruction(user, key(11), t22, tp, &sell_hops, buyback, 7, 3)
            .unwrap();
        assert_eq!(
            sell.accounts[6].pubkey,
            pda::associated_token(&buyback, &tp, &key(2)).0
        );
        assert_eq!(
            sell.accounts[2].pubkey,
            pda::associated_token(&user, &tp, &key(2)).0
        );

        let ixs = sdk
            .multi_hop_swap_instructions(user, key(2), tp, t22, &hops, buyback, 7, 3)
            .unwrap();
        assert_eq!(ixs.len(), 3);
        assert_eq!(
            ixs[0].accounts[1].pubkey,
            pda::associated_token(&buyback, &tp, &key(2)).0
        );
        assert_eq!(
            ixs[1].accounts[1].pubkey,
            pda::associated_token(&user, &t22, &key(11)).0
        );
        assert_eq!(ixs[2], ix);
        assert_eq!(
            sdk.multi_hop_swap_instruction(user, key(2), tp, t22, &hops, buyback, 7, 0),
            Err(QuoteError::ZeroAmount)
        );

        // A buy starting on a SOL curve also creates the user's wSOL ATA (not
        // debited: the curve takes lamports); the buyback ATA is wSOL.
        let wsol = constants::NATIVE_MINT;
        let sol_hops = [
            MultiHopHop::curve(key(10), Pubkey::default(), t22, tp),
            hop(11, 10),
        ];
        let ixs = sdk
            .multi_hop_swap_instructions(user, wsol, tp, t22, &sol_hops, buyback, 7, 3)
            .unwrap();
        assert_eq!(ixs.len(), 4);
        assert_eq!(
            ixs[0].accounts[1].pubkey,
            pda::associated_token(&buyback, &tp, &wsol).0
        );
        assert_eq!(
            ixs[1].accounts[1].pubkey,
            pda::associated_token(&user, &tp, &wsol).0
        );
        assert_eq!(
            ixs[3].accounts[1].pubkey,
            pda::associated_token(&user, &tp, &wsol).0
        );
        // Selling back into SOL: lamports go to the wallet; the wSOL out ATA is
        // the (created) placeholder, no extra create.
        let sol_sell = [
            hop(11, 10),
            MultiHopHop::curve(key(10), Pubkey::default(), t22, tp),
        ];
        let ixs = sdk
            .multi_hop_swap_instructions(user, key(11), t22, tp, &sol_sell, buyback, 7, 3)
            .unwrap();
        assert_eq!(ixs.len(), 3);
        assert_eq!(
            ixs[1].accounts[1].pubkey,
            pda::associated_token(&user, &tp, &wsol).0
        );
    }

    #[test]
    fn multi_hop_hop_constructors() {
        let state = pool_state();
        assert_eq!(
            MultiHopHop::pool(&state),
            MultiHopHop {
                base_mint: key(1),
                quote_mint: key(2),
                venue: pool_key(),
                base_vault: key(3),
                quote_vault: key(4),
            }
        );
        let (t22, tp) = (
            constants::SPL_TOKEN_2022_PROGRAM_ID,
            constants::SPL_TOKEN_PROGRAM_ID,
        );
        let curve = pda::pump::bonding_curve(&key(1)).0;
        assert_eq!(
            MultiHopHop::curve(key(1), key(2), t22, tp),
            MultiHopHop {
                base_mint: key(1),
                quote_mint: key(2),
                venue: curve,
                base_vault: pda::associated_token(&curve, &t22, &key(1)).0,
                quote_vault: pda::associated_token(&curve, &tp, &key(2)).0,
            }
        );
    }

    #[test]
    fn multi_hop_route_checks() {
        assert_eq!(
            PumpSdk::multi_hop_route(key(2), &[hop(10, 2), hop(11, 10)]),
            Ok((true, key(11)))
        );
        assert_eq!(
            PumpSdk::multi_hop_route(key(11), &[hop(11, 10), hop(10, 2)]),
            Ok((false, key(2)))
        );
        assert_eq!(
            PumpSdk::multi_hop_route(key(2), &[]),
            Err(QuoteError::MultiHopDiscontinuousPath)
        );
        assert_eq!(
            PumpSdk::multi_hop_route(key(2), &[hop(10, 2), hop(11, 12)]),
            Err(QuoteError::MultiHopDiscontinuousPath)
        );
        // Buy into Q, then sell Q back into USDC on a second venue.
        assert_eq!(
            PumpSdk::multi_hop_route(key(2), &[hop(10, 2), hop(10, 2)]),
            Err(QuoteError::MultiHopMixedDirection)
        );
        // A SOL curve hop trades wSOL: its zero quote key resolves to wSOL.
        let tp = constants::SPL_TOKEN_PROGRAM_ID;
        let sol_curve = MultiHopHop::curve(key(10), Pubkey::default(), tp, tp);
        assert_eq!(sol_curve.quote_mint, constants::NATIVE_MINT);
        assert_eq!(
            PumpSdk::multi_hop_route(constants::NATIVE_MINT, &[sol_curve, hop(11, 10)]),
            Ok((true, key(11)))
        );
    }
}
