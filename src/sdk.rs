use anchor_lang::{InstructionData, ToAccountMetas};
use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program, sysvar,
};

use crate::math::amm::{
    self as amm_math, AmmContext, BuyBaseInputResult, BuyQuoteInputResult, SellBaseInputResult,
};
use crate::math::bonding_curve as bc_math;
use crate::math::utils::{add_slippage, sub_slippage};
pub use crate::math::QuoteError;
use crate::math::QuoteResult;
use crate::pump_amm::{
    client as amm_client, types::OptionBool as AmmOptionBool, ID as PUMP_AMM_PROGRAM_ID,
};
use crate::state::pump_amm::{FeeConfig as AmmFeeConfig, GlobalConfig, Pool};
use crate::state::{BondingCurve, FeeConfig, Global};
use crate::token::{
    create_associated_token_account_idempotent, unwrap_sol_instruction, wrap_sol_instructions,
};
use crate::{constants, pda, pump::client, pump::types::OptionBool};

/// Result of a quote computation. `amount` is the primary output (tokens for
/// SOL-input buys; lamports for token-input buys and for sells).
/// `min_out` and `max_input` are slippage-adjusted bounds where applicable.
/// For **AMM** quote-in buys (`buy_quote_amm_sol_in`), `max_input` is the
/// slippage-inflated quote ceiling (mirrors `maxQuote` in
/// `@pump-fun/pump-swap-sdk` `buyQuoteInput`). For sells, `min_out` is the
/// quote floor. For token-out buys, `max_input` is the max quote you may spend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
    pub amount: u64,
    pub min_out: u64,
    pub input_amount_used: u64,
    pub max_input: u64,
}

/// Routing for [`PumpSdk::trade_tx_instructions`]. The bonding-curve path
/// uses `buy_v2` / `sell_v2`; the AMM path targets the supplied pool with
/// `buy_amm` / `sell_amm`.
#[derive(Clone, Copy, Debug)]
pub enum TradeVenue {
    BondingCurve,
    Amm { pool: Pubkey },
}

/// How [`PumpSdk::buy_quote_amm_sol_in`], [`PumpSdk::buy_quote_amm_token_out`], and
/// [`PumpSdk::sell_quote_amm`] obtain reserves and pool identity.
///
/// Prefer [`AmmQuoteSource::Pool`] with vault balances from RPC (same contract as
/// `@pump-fun/pump-swap-sdk` `OnlinePumpAmmSdk.swapSolanaState`). Use
/// [`AmmQuoteSource::BondingCurveComplete`] only when the curve is finished and the pool
/// is not available yet — reserves are a **heuristic** from pump `Global` +
/// `BondingCurve`, not live AMM state.
#[derive(Clone, Copy, Debug)]
pub enum AmmQuoteSource<'a> {
    Pool {
        pool: &'a Pool,
        base_reserve: u64,
        quote_reserve: u64,
        base_mint_supply: u64,
    },
    BondingCurveComplete {
        global: &'a Global,
        bonding_curve: &'a BondingCurve,
        base_mint: &'a Pubkey,
        base_mint_supply: u64,
    },
}

/// Inputs to [`PumpSdk::trade_tx_instructions`]. Quote mint is hard-wired to
/// wSOL: `sol_amount_threshold` is the SOL ceiling on buy and the SOL floor
/// on sell. Caller pre-computes amounts via the existing `*_quote_*` helpers.
#[derive(Clone, Copy, Debug)]
pub struct TradeTxParams {
    pub mint: Pubkey,
    pub base_token_program: Pubkey,
    pub user: Pubkey,
    pub creator: Pubkey,
    pub fee_recipient: Pubkey,
    pub buyback_fee_recipient: Pubkey,
    pub is_buy: bool,
    pub venue: TradeVenue,
    /// Only consumed by the AMM venue; ignored for `BondingCurve`.
    pub is_cashback_coin: bool,
    pub base_amount: u64,
    pub sol_amount_threshold: u64,
}

/// Inputs to [`PumpSdk::create_coin_instructions`]. Mint is the freshly
/// generated keypair pubkey; the caller signs the resulting tx with that
/// keypair plus the user.
#[derive(Clone, Debug)]
pub struct CreateCoinParams {
    pub mint: Pubkey,
    pub user: Pubkey,
    pub creator: Pubkey,
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub mayhem_mode: bool,
    pub cashback: bool,
    pub fee_recipient: Pubkey,
    pub buyback_fee_recipient: Pubkey,
    pub token_amount: u64,
    pub max_sol_cost: u64,
}

/// Offline instruction builders. No RPC calls are made and does not require RPC connection url.
#[derive(Clone, Copy, Debug, Default)]
pub struct PumpSdk;

impl PumpSdk {
    pub const fn new() -> Self {
        Self
    }

    fn map_amm_quote_source<R>(
        global_config: &GlobalConfig,
        fee_config: Option<&AmmFeeConfig>,
        source: AmmQuoteSource<'_>,
        with: impl FnOnce(AmmContext<'_>) -> QuoteResult<R>,
    ) -> QuoteResult<R> {
        match source {
            AmmQuoteSource::Pool {
                pool,
                base_reserve,
                quote_reserve,
                base_mint_supply,
            } => {
                let ctx = AmmContext {
                    global_config,
                    fee_config,
                    base_mint: &pool.base_mint,
                    pool_creator: &pool.creator,
                    coin_creator: &pool.coin_creator,
                    base_reserve,
                    quote_reserve,
                    base_mint_supply,
                };
                with(ctx)
            }
            AmmQuoteSource::BondingCurveComplete {
                global,
                bonding_curve,
                base_mint,
                base_mint_supply,
            } => {
                let (base_reserve, quote_reserve) = estimated_reserves(global, bonding_curve)?;
                let pool_creator_pk = pda::pump::pool_authority(base_mint).0;
                let ctx = AmmContext {
                    global_config,
                    fee_config,
                    base_mint,
                    pool_creator: &pool_creator_pk,
                    coin_creator: &bonding_curve.creator,
                    base_reserve,
                    quote_reserve,
                    base_mint_supply,
                };
                with(ctx)
            }
        }
    }

    /// v1 create: SPL Token + Metaplex metadata. The TS SDK marks the
    /// matching helper `@deprecated` in favor of `create_v2_instruction`,
    /// but it remains supported on-chain.
    #[deprecated(note = "Use create_v2_instruction instead")]
    pub fn create_instruction(
        &self,
        mint: Pubkey,
        user: Pubkey,
        name: impl Into<String>,
        symbol: impl Into<String>,
        uri: impl Into<String>,
        creator: Pubkey,
    ) -> Instruction {
        let token_program = constants::SPL_TOKEN_PROGRAM_ID;
        let bonding_curve = pda::pump::bonding_curve(&mint).0;
        let accounts = client::accounts::Create {
            mint,
            mint_authority: pda::pump::mint_authority().0,
            bonding_curve,
            associated_bonding_curve: pda::associated_token(&bonding_curve, &token_program, &mint)
                .0,
            global: pda::pump::global().0,
            mpl_token_metadata: constants::MPL_TOKEN_METADATA_PROGRAM_ID,
            metadata: pda::pump::metadata(&mint).0,
            user,
            system_program: system_program::ID,
            token_program,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            rent: sysvar::rent::ID,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
        };
        let args = client::args::Create {
            name: name.into(),
            symbol: symbol.into(),
            uri: uri.into(),
            creator,
        };
        Instruction {
            program_id: crate::pump::ID,
            accounts: accounts.to_account_metas(None),
            data: args.data(),
        }
    }

    /// v2 create: Token-2022 + Mayhem PDAs.
    pub fn create_v2_instruction(
        &self,
        mint: Pubkey,
        user: Pubkey,
        name: impl Into<String>,
        symbol: impl Into<String>,
        uri: impl Into<String>,
        creator: Pubkey,
        mayhem_mode: bool,
        cashback: bool,
    ) -> Instruction {
        let token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
        let bonding_curve = pda::pump::bonding_curve(&mint).0;
        let accounts = client::accounts::CreateV2 {
            mint,
            mint_authority: pda::pump::mint_authority().0,
            bonding_curve,
            associated_bonding_curve: pda::associated_token(&bonding_curve, &token_program, &mint)
                .0,
            global: pda::pump::global().0,
            user,
            system_program: system_program::ID,
            token_program,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            mayhem_program_id: constants::MAYHEM_PROGRAM_ID,
            global_params: pda::mayhem::global_params().0,
            sol_vault: pda::mayhem::sol_vault().0,
            mayhem_state: pda::mayhem::mayhem_state(&mint).0,
            mayhem_token_vault: pda::mayhem::mayhem_token_vault(&mint).0,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
        };
        let args = client::args::CreateV2 {
            name: name.into(),
            symbol: symbol.into(),
            uri: uri.into(),
            creator,
            is_mayhem_mode: mayhem_mode,
            is_cashback_enabled: OptionBool(cashback),
        };
        Instruction {
            program_id: crate::pump::ID,
            accounts: accounts.to_account_metas(None),
            data: args.data(),
        }
    }

    /// Buy tokens from a bonding curve. The caller picks `fee_recipient` and
    /// `buyback_fee_recipient` from the on-chain `Global` state. `max_sol_cost`
    /// is the slippage-adjusted ceiling on SOL spent. `token_program` is the
    /// SPL Token program for the mint (Token-2022 for v2 coins, classic SPL
    /// Token otherwise).
    ///
    /// Remaining accounts (in order, mirrors `pump-sdk-internal`'s
    /// `getBuyInstructionInternal`): `bonding_curve_v2` (readonly),
    /// `buyback_fee_recipient` (writable).
    #[deprecated(note = "Use buy_v2_instruction instead")]
    pub fn buy_instruction(
        &self,
        mint: Pubkey,
        user: Pubkey,
        creator: Pubkey,
        fee_recipient: Pubkey,
        buyback_fee_recipient: Pubkey,
        amount: u64,
        max_sol_cost: u64,
        token_program: Pubkey,
    ) -> Instruction {
        let bonding_curve = pda::pump::bonding_curve(&mint).0;
        let accounts = client::accounts::Buy {
            global: pda::pump::global().0,
            fee_recipient,
            mint,
            bonding_curve,
            associated_bonding_curve: pda::associated_token(&bonding_curve, &token_program, &mint)
                .0,
            associated_user: pda::associated_token(&user, &token_program, &mint).0,
            user,
            system_program: system_program::ID,
            token_program,
            creator_vault: pda::pump::creator_vault(&creator).0,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
            global_volume_accumulator: pda::pump::global_volume_accumulator().0,
            user_volume_accumulator: pda::pump::user_volume_accumulator(&user).0,
            fee_config: pda::pump::fee_config().0,
            fee_program: constants::FEE_PROGRAM_ID,
        };
        let args = client::args::Buy {
            amount,
            max_sol_cost,
            track_volume: OptionBool(true),
        };
        let mut metas = accounts.to_account_metas(None);
        metas.push(AccountMeta::new_readonly(
            pda::pump::bonding_curve_v2(&mint).0,
            false,
        ));
        metas.push(AccountMeta::new(buyback_fee_recipient, false));
        Instruction {
            program_id: crate::pump::ID,
            accounts: metas,
            data: args.data(),
        }
    }

    /// Buy tokens with an arbitrary quote mint. Unlike `buy_instruction`, the
    /// program takes the quote asset on its own token program and routes a
    /// share of fees through the per-mint `sharing_config`.
    /// Caller picks `fee_recipient` and `buyback_fee_recipient` from `Global`.
    pub fn buy_v2_instruction(
        &self,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        creator: Pubkey,
        fee_recipient: Pubkey,
        buyback_fee_recipient: Pubkey,
        amount: u64,
        max_sol_cost: u64,
    ) -> Instruction {
        let bonding_curve = pda::pump::bonding_curve(&base_mint).0;
        let creator_vault = pda::pump::creator_vault(&creator).0;
        let user_volume_accumulator = pda::pump::user_volume_accumulator(&user).0;
        let accounts = client::accounts::BuyV2 {
            global: pda::pump::global().0,
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            fee_recipient,
            associated_quote_fee_recipient: pda::associated_token(
                &fee_recipient,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            buyback_fee_recipient,
            associated_quote_buyback_fee_recipient: pda::associated_token(
                &buyback_fee_recipient,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            bonding_curve,
            associated_base_bonding_curve: pda::associated_token(
                &bonding_curve,
                &base_token_program,
                &base_mint,
            )
            .0,
            associated_quote_bonding_curve: pda::associated_token(
                &bonding_curve,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            user,
            associated_base_user: pda::associated_token(&user, &base_token_program, &base_mint).0,
            associated_quote_user: pda::associated_token(&user, &quote_token_program, &quote_mint)
                .0,
            creator_vault,
            associated_creator_vault: pda::associated_token(
                &creator_vault,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            sharing_config: pda::pump::sharing_config(&base_mint).0,
            global_volume_accumulator: pda::pump::global_volume_accumulator().0,
            user_volume_accumulator,
            associated_user_volume_accumulator: pda::associated_token(
                &user_volume_accumulator,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            fee_config: pda::pump::fee_config().0,
            fee_program: constants::FEE_PROGRAM_ID,
            system_program: system_program::ID,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
        };
        let args = client::args::BuyV2 {
            amount,
            max_sol_cost,
        };
        Instruction {
            program_id: crate::pump::ID,
            accounts: accounts.to_account_metas(None),
            data: args.data(),
        }
    }

    /// `buy_v2` preceded by idempotent ATA creates for every account the
    /// program does NOT lazy-initialize. From `programs/pump/src/buy_v2.rs`,
    /// `validate_quote_token_account` only initializes the creator-vault,
    /// user-volume-accumulator, and quote-fee-recipient quote ATAs
    /// (`should_initialize = true`); everything else must already exist.
    /// We emit 4 prefixes:
    ///   1. `associated_base_user` (always required)
    ///   2. `associated_quote_bonding_curve`
    ///   3. `associated_quote_user`
    ///   4. `associated_quote_buyback_fee_recipient`
    /// (2-4) are only enforced for non-legacy curves but are idempotent and
    /// cheap when already present.
    pub fn buy_v2_instructions(
        &self,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        creator: Pubkey,
        fee_recipient: Pubkey,
        buyback_fee_recipient: Pubkey,
        amount: u64,
        max_sol_cost: u64,
    ) -> Vec<Instruction> {
        let bonding_curve = pda::pump::bonding_curve(&base_mint).0;
        vec![
            create_associated_token_account_idempotent(
                &user,
                &user,
                &base_mint,
                &base_token_program,
            ),
            create_associated_token_account_idempotent(
                &user,
                &bonding_curve,
                &quote_mint,
                &quote_token_program,
            ),
            create_associated_token_account_idempotent(
                &user,
                &user,
                &quote_mint,
                &quote_token_program,
            ),
            create_associated_token_account_idempotent(
                &user,
                &buyback_fee_recipient,
                &quote_mint,
                &quote_token_program,
            ),
            self.buy_v2_instruction(
                base_mint,
                quote_mint,
                base_token_program,
                quote_token_program,
                user,
                creator,
                fee_recipient,
                buyback_fee_recipient,
                amount,
                max_sol_cost,
            ),
        ]
    }

    /// Sell tokens back to a bonding curve. The caller picks `fee_recipient`
    /// from the on-chain `Global` state. `min_sol_output` is the
    /// slippage-adjusted floor on SOL received. `token_program` is the SPL
    /// Token program for the mint (Token-2022 for v2 coins, classic SPL Token
    /// otherwise).
    ///
    /// Remaining accounts (in order, mirrors the on-chain handler's cashback
    /// path): `user_volume_accumulator` (writable), `bonding_curve_v2`
    /// (readonly). Always appended; the program falls back to
    /// `creator_vault` when these are absent or invalid.
    #[deprecated(note = "Use sell_v2_instruction instead")]
    pub fn sell_instruction(
        &self,
        mint: Pubkey,
        user: Pubkey,
        creator: Pubkey,
        fee_recipient: Pubkey,
        amount: u64,
        min_sol_output: u64,
        token_program: Pubkey,
    ) -> Instruction {
        let bonding_curve = pda::pump::bonding_curve(&mint).0;
        let accounts = client::accounts::Sell {
            global: pda::pump::global().0,
            fee_recipient,
            mint,
            bonding_curve,
            associated_bonding_curve: pda::associated_token(&bonding_curve, &token_program, &mint)
                .0,
            associated_user: pda::associated_token(&user, &token_program, &mint).0,
            user,
            system_program: system_program::ID,
            creator_vault: pda::pump::creator_vault(&creator).0,
            token_program,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
            fee_config: pda::pump::fee_config().0,
            fee_program: constants::FEE_PROGRAM_ID,
        };
        let args = client::args::Sell {
            amount,
            min_sol_output,
        };
        let mut metas = accounts.to_account_metas(None);
        metas.push(AccountMeta::new(
            pda::pump::user_volume_accumulator(&user).0,
            false,
        ));
        metas.push(AccountMeta::new_readonly(
            pda::pump::bonding_curve_v2(&mint).0,
            false,
        ));
        Instruction {
            program_id: crate::pump::ID,
            accounts: metas,
            data: args.data(),
        }
    }

    /// Sell tokens for an arbitrary quote mint. Mirror of `buy_v2_instruction`
    /// on the sell side. Routes a share of fees through the per-mint
    /// `sharing_config`. Caller picks `fee_recipient` and
    /// `buyback_fee_recipient` from `Global`.
    pub fn sell_v2_instruction(
        &self,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        creator: Pubkey,
        fee_recipient: Pubkey,
        buyback_fee_recipient: Pubkey,
        amount: u64,
        min_sol_output: u64,
    ) -> Instruction {
        let bonding_curve = pda::pump::bonding_curve(&base_mint).0;
        let creator_vault = pda::pump::creator_vault(&creator).0;
        let user_volume_accumulator = pda::pump::user_volume_accumulator(&user).0;
        let accounts = client::accounts::SellV2 {
            global: pda::pump::global().0,
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            fee_recipient,
            associated_quote_fee_recipient: pda::associated_token(
                &fee_recipient,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            buyback_fee_recipient,
            associated_quote_buyback_fee_recipient: pda::associated_token(
                &buyback_fee_recipient,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            bonding_curve,
            associated_base_bonding_curve: pda::associated_token(
                &bonding_curve,
                &base_token_program,
                &base_mint,
            )
            .0,
            associated_quote_bonding_curve: pda::associated_token(
                &bonding_curve,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            user,
            associated_base_user: pda::associated_token(&user, &base_token_program, &base_mint).0,
            associated_quote_user: pda::associated_token(&user, &quote_token_program, &quote_mint)
                .0,
            creator_vault,
            associated_creator_vault: pda::associated_token(
                &creator_vault,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            sharing_config: pda::pump::sharing_config(&base_mint).0,
            user_volume_accumulator,
            associated_user_volume_accumulator: pda::associated_token(
                &user_volume_accumulator,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            fee_config: pda::pump::fee_config().0,
            fee_program: constants::FEE_PROGRAM_ID,
            system_program: system_program::ID,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
        };
        let args = client::args::SellV2 {
            amount,
            min_sol_output,
        };
        Instruction {
            program_id: crate::pump::ID,
            accounts: accounts.to_account_metas(None),
            data: args.data(),
        }
    }

    /// `sell_v2` preceded by idempotent ATA creates for every account the
    /// program does NOT lazy-initialize. Mirrors `buy_v2_instructions`. We
    /// emit 4 prefixes:
    ///   1. `associated_base_user` (user must hold the base token)
    ///   2. `associated_quote_bonding_curve`
    ///   3. `associated_quote_user` (user receives quote)
    ///   4. `associated_quote_buyback_fee_recipient`
    pub fn sell_v2_instructions(
        &self,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        creator: Pubkey,
        fee_recipient: Pubkey,
        buyback_fee_recipient: Pubkey,
        amount: u64,
        min_sol_output: u64,
    ) -> Vec<Instruction> {
        let bonding_curve = pda::pump::bonding_curve(&base_mint).0;
        vec![
            create_associated_token_account_idempotent(
                &user,
                &user,
                &base_mint,
                &base_token_program,
            ),
            create_associated_token_account_idempotent(
                &user,
                &bonding_curve,
                &quote_mint,
                &quote_token_program,
            ),
            create_associated_token_account_idempotent(
                &user,
                &user,
                &quote_mint,
                &quote_token_program,
            ),
            create_associated_token_account_idempotent(
                &user,
                &buyback_fee_recipient,
                &quote_mint,
                &quote_token_program,
            ),
            self.sell_v2_instruction(
                base_mint,
                quote_mint,
                base_token_program,
                quote_token_program,
                user,
                creator,
                fee_recipient,
                buyback_fee_recipient,
                amount,
                min_sol_output,
            ),
        ]
    }

    /// `create_v2` + idempotent ATA + `buy`, in that order.
    /// Mirrors `createV2AndBuyInstructions` in `pump-sdk/src/sdk.ts`.
    /// `fee_recipient` is supplied by the caller (typically picked from
    /// `Global`). Quote is wSOL on the classic SPL Token program — the
    /// only sensible quote at creation time. For other quotes, call
    /// `buy_v2_instruction` directly.
    pub fn create_v2_and_buy_instruction(
        &self,
        mint: Pubkey,
        user: Pubkey,
        name: impl Into<String>,
        symbol: impl Into<String>,
        uri: impl Into<String>,
        creator: Pubkey,
        mayhem_mode: bool,
        cashback: bool,
        fee_recipient: Pubkey,
        buyback_fee_recipient: Pubkey,
        amount: u64,
        max_sol_cost: u64,
    ) -> Vec<Instruction> {
        let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
        let quote_mint = constants::NATIVE_MINT;
        let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;
        let buy_v2_instructions = self.buy_v2_instructions(
            mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
            amount,
            max_sol_cost,
        );
        let mut instructions = vec![self.create_v2_instruction(
            mint,
            user,
            name,
            symbol,
            uri,
            creator,
            mayhem_mode,
            cashback,
        )];

        instructions.extend(buy_v2_instructions);

        instructions
    }

    pub fn extend_account_ix(&self, mint: Pubkey, user: Pubkey) -> Instruction {
        let accounts = client::accounts::ExtendAccount {
            account: pda::pump::bonding_curve(&mint).0,
            user,
            system_program: system_program::ID,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
        };
        Instruction {
            program_id: crate::pump::ID,
            accounts: accounts.to_account_metas(None),
            data: client::args::ExtendAccount.data(),
        }
    }

    /// Buy on a graduated `pump_amm` pool. Mirrors `PUMP_AMM_SDK.buyInstructions`'
    /// inner IX from `pump-swap-sdk/src/sdk/offlinePumpAmm.ts`. Caller picks
    /// `protocol_fee_recipient` from `GlobalConfig` (via `getFeeRecipient`) and
    /// `buyback_fee_recipient` likewise (via `getBuybackFeeRecipient`).
    /// `coin_creator` and `is_cashback_coin` come from the on-chain `Pool`.
    /// Pass `Pubkey::default()` for `coin_creator` to skip the
    /// `bonding_curve_v2` remaining account on default-creator pools.
    /// `track_volume` is hardcoded to true (matches the TS SDK).
    #[allow(clippy::too_many_arguments)]
    pub fn buy_amm_instruction(
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
        let coin_creator_vault_authority =
            pda::pump_amm::coin_creator_vault_authority(&coin_creator).0;
        let user_volume_accumulator = pda::pump_amm::user_volume_accumulator(&user).0;
        let accounts = amm_client::accounts::Buy {
            pool,
            user,
            global_config: pda::pump_amm::global_config().0,
            base_mint,
            quote_mint,
            user_base_token_account: pda::associated_token(&user, &base_token_program, &base_mint)
                .0,
            user_quote_token_account: pda::associated_token(
                &user,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            pool_base_token_account: pda::associated_token(&pool, &base_token_program, &base_mint)
                .0,
            pool_quote_token_account: pda::associated_token(
                &pool,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            protocol_fee_recipient,
            protocol_fee_recipient_token_account: pda::associated_token(
                &protocol_fee_recipient,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            base_token_program,
            quote_token_program,
            system_program: system_program::ID,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            event_authority: pda::pump_amm::event_authority().0,
            program: PUMP_AMM_PROGRAM_ID,
            coin_creator_vault_ata: pda::associated_token(
                &coin_creator_vault_authority,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            coin_creator_vault_authority,
            global_volume_accumulator: pda::pump_amm::global_volume_accumulator().0,
            user_volume_accumulator,
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
                    &user_volume_accumulator,
                    &quote_token_program,
                    &constants::NATIVE_MINT,
                )
                .0,
                false,
            ));
        }
        if coin_creator != Pubkey::default() {
            metas.push(AccountMeta::new_readonly(
                pda::pump::bonding_curve_v2(&base_mint).0,
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

    /// `buy_amm` preceded by an idempotent ATA-create for the user's base ATA
    /// (the asset being received). Wsol management for the quote side is the
    /// caller's responsibility; the existing `buy_v2_instructions` wrapper
    /// follows the same convention.
    #[allow(clippy::too_many_arguments)]
    pub fn buy_amm_instructions(
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
    ) -> Vec<Instruction> {
        vec![
            create_associated_token_account_idempotent(
                &user,
                &user,
                &base_mint,
                &base_token_program,
            ),
            self.buy_amm_instruction(
                pool,
                base_mint,
                quote_mint,
                base_token_program,
                quote_token_program,
                user,
                coin_creator,
                protocol_fee_recipient,
                buyback_fee_recipient,
                is_cashback_coin,
                base_amount_out,
                max_quote_amount_in,
            ),
        ]
    }

    /// Sell on a graduated `pump_amm` pool. Mirrors `PUMP_AMM_SDK.sellInstructions`'
    /// inner IX from `pump-swap-sdk/src/sdk/offlinePumpAmm.ts`. The remaining-account
    /// layout differs from buy in three places (cashback adds two accounts not one,
    /// the cashback ATA uses `quote_mint` not `NATIVE_MINT`, and
    /// `buyback_fee_recipient` is read-only). See `buy_amm_instruction` for the
    /// rest of the parameter contract.
    #[allow(clippy::too_many_arguments)]
    pub fn sell_amm_instruction(
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
        let coin_creator_vault_authority =
            pda::pump_amm::coin_creator_vault_authority(&coin_creator).0;
        let user_volume_accumulator = pda::pump_amm::user_volume_accumulator(&user).0;
        let accounts = amm_client::accounts::Sell {
            pool,
            user,
            global_config: pda::pump_amm::global_config().0,
            base_mint,
            quote_mint,
            user_base_token_account: pda::associated_token(&user, &base_token_program, &base_mint)
                .0,
            user_quote_token_account: pda::associated_token(
                &user,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            pool_base_token_account: pda::associated_token(&pool, &base_token_program, &base_mint)
                .0,
            pool_quote_token_account: pda::associated_token(
                &pool,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            protocol_fee_recipient,
            protocol_fee_recipient_token_account: pda::associated_token(
                &protocol_fee_recipient,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            base_token_program,
            quote_token_program,
            system_program: system_program::ID,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            event_authority: pda::pump_amm::event_authority().0,
            program: PUMP_AMM_PROGRAM_ID,
            coin_creator_vault_ata: pda::associated_token(
                &coin_creator_vault_authority,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            coin_creator_vault_authority,
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
                pda::associated_token(&user_volume_accumulator, &quote_token_program, &quote_mint)
                    .0,
                false,
            ));
            metas.push(AccountMeta::new(user_volume_accumulator, false));
        }
        if coin_creator != Pubkey::default() {
            metas.push(AccountMeta::new_readonly(
                pda::pump::bonding_curve_v2(&base_mint).0,
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

    /// `sell_amm` preceded by an idempotent ATA-create for the user's quote ATA
    /// (the asset being received).
    #[allow(clippy::too_many_arguments)]
    pub fn sell_amm_instructions(
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
    ) -> Vec<Instruction> {
        vec![
            create_associated_token_account_idempotent(
                &user,
                &user,
                &quote_mint,
                &quote_token_program,
            ),
            self.sell_amm_instruction(
                pool,
                base_mint,
                quote_mint,
                base_token_program,
                quote_token_program,
                user,
                coin_creator,
                protocol_fee_recipient,
                buyback_fee_recipient,
                is_cashback_coin,
                base_amount_in,
                min_quote_amount_out,
            ),
        ]
    }

    // ------------------------------------------------------------------
    // Complete trade flows (mirror `TradeTxService` in
    // `backends/blockchain-client/src/trade-tx/trade-tx.service.ts`).
    //
    // Quote is hard-wired to wSOL. SOL is wrapped via system_transfer +
    // sync_native before the swap (buy only) and the user's wSOL ATA is
    // closed after the swap (always), so any residual or proceeds become
    // lamports without a second tx.
    // ------------------------------------------------------------------

    /// Build a complete buy/sell against either the bonding curve (`buy_v2`/
    /// `sell_v2`) or a graduated `pump_amm` pool. Mirrors `TradeTxService.tradeTx`.
    pub fn trade_tx_instructions(&self, params: TradeTxParams) -> Vec<Instruction> {
        let TradeTxParams {
            mint,
            base_token_program,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
            is_buy,
            venue,
            is_cashback_coin,
            base_amount,
            sol_amount_threshold,
        } = params;
        let quote_mint = constants::NATIVE_MINT;
        let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;

        match venue {
            TradeVenue::BondingCurve => {
                let bonding_curve = pda::pump::bonding_curve(&mint).0;
                let mut ixs = vec![
                    create_associated_token_account_idempotent(
                        &user,
                        &user,
                        &mint,
                        &base_token_program,
                    ),
                    create_associated_token_account_idempotent(
                        &user,
                        &bonding_curve,
                        &quote_mint,
                        &quote_token_program,
                    ),
                    create_associated_token_account_idempotent(
                        &user,
                        &user,
                        &quote_mint,
                        &quote_token_program,
                    ),
                    create_associated_token_account_idempotent(
                        &user,
                        &buyback_fee_recipient,
                        &quote_mint,
                        &quote_token_program,
                    ),
                ];
                if is_buy {
                    ixs.extend(wrap_sol_instructions(&user, sol_amount_threshold));
                    ixs.push(self.buy_v2_instruction(
                        mint,
                        quote_mint,
                        base_token_program,
                        quote_token_program,
                        user,
                        creator,
                        fee_recipient,
                        buyback_fee_recipient,
                        base_amount,
                        sol_amount_threshold,
                    ));
                } else {
                    ixs.push(self.sell_v2_instruction(
                        mint,
                        quote_mint,
                        base_token_program,
                        quote_token_program,
                        user,
                        creator,
                        fee_recipient,
                        buyback_fee_recipient,
                        base_amount,
                        sol_amount_threshold,
                    ));
                }
                ixs.push(unwrap_sol_instruction(&user));
                ixs
            }
            TradeVenue::Amm { pool } => {
                let mut ixs: Vec<Instruction> = Vec::with_capacity(6);
                if is_buy {
                    ixs.push(create_associated_token_account_idempotent(
                        &user,
                        &user,
                        &mint,
                        &base_token_program,
                    ));
                    ixs.push(create_associated_token_account_idempotent(
                        &user,
                        &user,
                        &quote_mint,
                        &quote_token_program,
                    ));
                    ixs.extend(wrap_sol_instructions(&user, sol_amount_threshold));
                    ixs.push(self.buy_amm_instruction(
                        pool,
                        mint,
                        quote_mint,
                        base_token_program,
                        quote_token_program,
                        user,
                        creator,
                        fee_recipient,
                        buyback_fee_recipient,
                        is_cashback_coin,
                        base_amount,
                        sol_amount_threshold,
                    ));
                } else {
                    ixs.push(create_associated_token_account_idempotent(
                        &user,
                        &user,
                        &quote_mint,
                        &quote_token_program,
                    ));
                    ixs.push(self.sell_amm_instruction(
                        pool,
                        mint,
                        quote_mint,
                        base_token_program,
                        quote_token_program,
                        user,
                        creator,
                        fee_recipient,
                        buyback_fee_recipient,
                        is_cashback_coin,
                        base_amount,
                        sol_amount_threshold,
                    ));
                }
                ixs.push(unwrap_sol_instruction(&user));
                ixs
            }
        }
    }

    /// Build a complete `create_v2` + initial-buy with full SOL wrap/unwrap.
    /// Mirrors `TradeTxService.createCoin` minus the optional tokenized-agent
    /// step (no Rust `pump-agent-payments` SDK in this repo).
    pub fn create_coin_instructions(&self, params: CreateCoinParams) -> Vec<Instruction> {
        let CreateCoinParams {
            mint,
            user,
            creator,
            name,
            symbol,
            uri,
            mayhem_mode,
            cashback,
            fee_recipient,
            buyback_fee_recipient,
            token_amount,
            max_sol_cost,
        } = params;
        let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
        let quote_mint = constants::NATIVE_MINT;
        let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;
        let bonding_curve = pda::pump::bonding_curve(&mint).0;

        let mut ixs: Vec<Instruction> = Vec::with_capacity(9);
        ixs.push(self.create_v2_instruction(
            mint,
            user,
            name,
            symbol,
            uri,
            creator,
            mayhem_mode,
            cashback,
        ));
        ixs.push(create_associated_token_account_idempotent(
            &user,
            &user,
            &mint,
            &base_token_program,
        ));
        ixs.push(create_associated_token_account_idempotent(
            &user,
            &bonding_curve,
            &quote_mint,
            &quote_token_program,
        ));
        ixs.push(create_associated_token_account_idempotent(
            &user,
            &user,
            &quote_mint,
            &quote_token_program,
        ));
        ixs.push(create_associated_token_account_idempotent(
            &user,
            &buyback_fee_recipient,
            &quote_mint,
            &quote_token_program,
        ));
        ixs.extend(wrap_sol_instructions(&user, max_sol_cost));
        ixs.push(self.buy_v2_instruction(
            mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
            token_amount,
            max_sol_cost,
        ));
        ixs.push(unwrap_sol_instruction(&user));
        ixs
    }

    // ------------------------------------------------------------------
    // Quoting
    //
    // Bonding curve: mirrors `getQuote` / bonding-curve branches in the
    // frontend store (`stores/blockchainStore/store.ts`).
    //
    // **Pump AMM** (canonical): match `@pump-fun/pump-swap-sdk` — load pool
    // `GlobalConfig` + optional `FeeConfig`, decode `Pool`, then set
    // `base_reserve` / `quote_reserve` to the **live SPL token amounts** on
    // `pool.pool_base_token_account` and `pool.pool_quote_token_account`
    // (same as `OnlinePumpAmmSdk.swapSolanaState` in TS). Pass
    // `base_mint_supply` from the base mint account (`supply` field), same as
    // the SDK. Pass [`AmmQuoteSource::Pool`] with those balances. For a
    // completed curve before the pool exists, [`AmmQuoteSource::BondingCurveComplete`]
    // synthesizes reserves (heuristic only).
    //
    // `slippage_bps` is in basis points (1% = 100). All inputs and outputs
    // are atomic units (lamports / token base units).
    // ------------------------------------------------------------------

    /// Bonding curve buy quoting where the user specifies SOL in.
    pub fn buy_quote_bonding_curve_sol_in(
        &self,
        global: &Global,
        fee_config: Option<&FeeConfig>,
        bonding_curve: &BondingCurve,
        mint_supply: u64,
        sol_amount: u64,
        slippage_bps: u16,
    ) -> QuoteResult<Quote> {
        let tokens_out = bc_math::buy_token_amount_from_sol_amount(
            global,
            fee_config,
            bonding_curve,
            mint_supply,
            sol_amount,
        )?;
        Ok(Quote {
            amount: tokens_out,
            min_out: sub_slippage(tokens_out, slippage_bps),
            input_amount_used: sol_amount,
            max_input: sol_amount,
        })
    }

    /// Bonding curve buy quoting where the user specifies token amount out.
    pub fn buy_quote_bonding_curve_token_out(
        &self,
        global: &Global,
        fee_config: Option<&FeeConfig>,
        bonding_curve: &BondingCurve,
        mint_supply: u64,
        token_amount: u64,
        slippage_bps: u16,
    ) -> QuoteResult<Quote> {
        let sol_cost = bc_math::buy_sol_amount_from_token_amount(
            global,
            fee_config,
            bonding_curve,
            mint_supply,
            token_amount,
        )?;
        Ok(Quote {
            amount: sol_cost,
            // Desired tokens out is fixed; slippage is on the SOL side.
            min_out: token_amount,
            input_amount_used: token_amount,
            max_input: add_slippage(sol_cost, slippage_bps),
        })
    }

    /// Bonding curve sell quoting.
    pub fn sell_quote_bonding_curve(
        &self,
        global: &Global,
        fee_config: Option<&FeeConfig>,
        bonding_curve: &BondingCurve,
        mint_supply: u64,
        token_amount: u64,
        slippage_bps: u16,
    ) -> QuoteResult<Quote> {
        let sol_out = bc_math::sell_sol_amount_from_token_amount(
            global,
            fee_config,
            bonding_curve,
            mint_supply,
            token_amount,
        )?;
        Ok(Quote {
            amount: sol_out,
            min_out: sub_slippage(sol_out, slippage_bps),
            input_amount_used: token_amount,
            max_input: token_amount,
        })
    }

    /// AMM buy quoting where the user specifies quote (e.g. wSOL) in.
    pub fn buy_quote_amm_sol_in(
        &self,
        global_config: &GlobalConfig,
        fee_config: Option<&AmmFeeConfig>,
        source: AmmQuoteSource<'_>,
        sol_amount: u64,
        slippage_bps: u16,
    ) -> QuoteResult<Quote> {
        Self::map_amm_quote_source(global_config, fee_config, source, |ctx| {
            let BuyQuoteInputResult {
                base_amount_out, ..
            } = amm_math::buy_quote_input(&ctx, sol_amount)?;
            Ok(Quote {
                amount: base_amount_out,
                min_out: sub_slippage(base_amount_out, slippage_bps),
                input_amount_used: sol_amount,
                max_input: add_slippage(sol_amount, slippage_bps),
            })
        })
    }

    /// AMM buy quoting where the user specifies token amount out.
    pub fn buy_quote_amm_token_out(
        &self,
        global_config: &GlobalConfig,
        fee_config: Option<&AmmFeeConfig>,
        source: AmmQuoteSource<'_>,
        token_amount: u64,
        slippage_bps: u16,
    ) -> QuoteResult<Quote> {
        Self::map_amm_quote_source(global_config, fee_config, source, |ctx| {
            let BuyBaseInputResult { total_quote_in, .. } =
                amm_math::buy_base_input(&ctx, token_amount)?;
            Ok(Quote {
                amount: total_quote_in,
                min_out: token_amount,
                input_amount_used: token_amount,
                max_input: add_slippage(total_quote_in, slippage_bps),
            })
        })
    }

    /// AMM sell quoting.
    pub fn sell_quote_amm(
        &self,
        global_config: &GlobalConfig,
        fee_config: Option<&AmmFeeConfig>,
        source: AmmQuoteSource<'_>,
        token_amount: u64,
        slippage_bps: u16,
    ) -> QuoteResult<Quote> {
        Self::map_amm_quote_source(global_config, fee_config, source, |ctx| {
            let SellBaseInputResult {
                final_quote_out, ..
            } = amm_math::sell_base_input(&ctx, token_amount)?;
            Ok(Quote {
                amount: final_quote_out,
                min_out: sub_slippage(final_quote_out, slippage_bps),
                input_amount_used: token_amount,
                max_input: token_amount,
            })
        })
    }
}

/// Synthesized reserves when using [`AmmQuoteSource::BondingCurveComplete`].
/// Mirrors the legacy app formula (`store.ts`):
/// `token_total_supply - initial_real_token_reserves` and
/// `real_quote_reserves - pool_migration_fee`.
fn estimated_reserves(global: &Global, bonding_curve: &BondingCurve) -> QuoteResult<(u64, u64)> {
    let base_reserve = global
        .token_total_supply
        .checked_sub(global.initial_real_token_reserves)
        .ok_or(QuoteError::EmptyReserves)?;
    let quote_reserve = bonding_curve
        .real_quote_reserves
        .checked_sub(global.pool_migration_fee)
        .ok_or(QuoteError::EmptyReserves)?;
    if base_reserve == 0 || quote_reserve == 0 {
        return Err(QuoteError::EmptyReserves);
    }
    Ok((base_reserve, quote_reserve))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::Discriminator;
    use solana_program::{pubkey, pubkey::Pubkey};

    fn fake_pubkey(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    #[test]
    #[allow(deprecated)]
    fn create_instruction_wires_metadata_pdas() {
        let sdk = PumpSdk::new();
        let mint = fake_pubkey(51);
        let user = fake_pubkey(52);
        let creator = fake_pubkey(53);

        let ix = sdk.create_instruction(
            mint,
            user,
            "Test",
            "TST",
            "https://example.com/metadata.json",
            creator,
        );

        assert_eq!(ix.program_id, crate::pump::ID);
        assert_eq!(
            &ix.data[..8],
            <client::args::Create as Discriminator>::DISCRIMINATOR,
        );
        let metas: Vec<Pubkey> = ix.accounts.iter().map(|m| m.pubkey).collect();
        assert!(metas.contains(&pda::pump::metadata(&mint).0));
        assert!(metas.contains(&constants::MPL_TOKEN_METADATA_PROGRAM_ID));
        assert!(metas.contains(&constants::SPL_TOKEN_PROGRAM_ID));
        assert!(metas.contains(&pda::pump::bonding_curve(&mint).0));
        assert!(metas.contains(&sysvar::rent::ID));
    }

    #[test]
    fn create_v2_instruction_wires_mayhem_pdas() {
        let sdk = PumpSdk::new();
        let mint = fake_pubkey(21);
        let user = fake_pubkey(22);
        let creator = fake_pubkey(23);

        let ix = sdk.create_v2_instruction(
            mint,
            user,
            "Test",
            "TST",
            "https://example.com/metadata.json",
            creator,
            false,
            false,
        );

        assert_eq!(ix.program_id, crate::pump::ID);
        assert_eq!(
            &ix.data[..8],
            <client::args::CreateV2 as Discriminator>::DISCRIMINATOR,
        );
        let metas: Vec<Pubkey> = ix.accounts.iter().map(|m| m.pubkey).collect();
        assert!(metas.contains(&pda::mayhem::global_params().0));
        assert!(metas.contains(&pda::mayhem::sol_vault().0));
        assert!(metas.contains(&pda::mayhem::mayhem_state(&mint).0));
        assert!(metas.contains(&pda::mayhem::mayhem_token_vault(&mint).0));
        assert!(metas.contains(&constants::MAYHEM_PROGRAM_ID));
        assert!(metas.contains(&constants::SPL_TOKEN_2022_PROGRAM_ID));
    }

    #[test]
    #[allow(deprecated)]
    fn buy_instruction_appends_bonding_curve_v2_then_buyback_fee_recipient() {
        let sdk = PumpSdk::new();
        let mint = fake_pubkey(61);
        let user = fake_pubkey(62);
        let creator = fake_pubkey(63);
        let fee_recipient = fake_pubkey(64);
        let buyback_fee_recipient = fake_pubkey(65);
        let token_program = constants::SPL_TOKEN_PROGRAM_ID;

        let ix = sdk.buy_instruction(
            mint,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
            100,
            5_000_000,
            token_program,
        );

        assert_eq!(ix.program_id, crate::pump::ID);
        assert_eq!(
            &ix.data[..8],
            <client::args::Buy as Discriminator>::DISCRIMINATOR,
        );

        let metas: Vec<Pubkey> = ix.accounts.iter().map(|m| m.pubkey).collect();
        assert!(metas.contains(&pda::pump::global().0));
        assert!(metas.contains(&pda::pump::bonding_curve(&mint).0));
        assert!(metas.contains(&pda::pump::creator_vault(&creator).0));
        assert!(metas.contains(&pda::pump::user_volume_accumulator(&user).0));
        assert!(metas.contains(&pda::pump::fee_config().0));
        assert!(metas.contains(&constants::FEE_PROGRAM_ID));
        assert!(metas.contains(&fee_recipient));

        // Tail of the account list is exactly [bonding_curve_v2 ro, buyback_fee_recipient rw].
        let n = ix.accounts.len();
        let bcv2 = &ix.accounts[n - 2];
        let buyback = &ix.accounts[n - 1];
        assert_eq!(bcv2.pubkey, pda::pump::bonding_curve_v2(&mint).0);
        assert!(!bcv2.is_writable);
        assert!(!bcv2.is_signer);
        assert_eq!(buyback.pubkey, buyback_fee_recipient);
        assert!(buyback.is_writable);
        assert!(!buyback.is_signer);
    }

    #[test]
    fn create_v2_and_buy_instruction_returns_create_then_buy_v2_prefix_in_order() {
        let sdk = PumpSdk::new();
        let mint = fake_pubkey(71);
        let user = fake_pubkey(72);
        let creator = fake_pubkey(73);
        let fee_recipient = fake_pubkey(74);
        let buyback_fee_recipient = fake_pubkey(75);

        let ixs = sdk.create_v2_and_buy_instruction(
            mint,
            user,
            "Test",
            "TST",
            "https://example.com/metadata.json",
            creator,
            false,
            false,
            fee_recipient,
            buyback_fee_recipient,
            1_000,
            500_000,
        );

        // create_v2 + buy_v2_instructions (4 idempotent ATA creates + buy_v2).
        assert_eq!(ixs.len(), 6);
        assert_eq!(
            &ixs[0].data[..8],
            <client::args::CreateV2 as Discriminator>::DISCRIMINATOR,
        );
        for i in 1..5 {
            assert_eq!(ixs[i].data, vec![1], "expected SPL ATA idempotent ix");
        }
        assert_eq!(
            &ixs[5].data[..8],
            <client::args::BuyV2 as Discriminator>::DISCRIMINATOR,
        );

        // Buy should be a v2 buy with base=Token-2022, quote=wSOL/SPL Token.
        let buy_metas: Vec<Pubkey> = ixs[5].accounts.iter().map(|m| m.pubkey).collect();
        assert!(buy_metas.contains(&constants::SPL_TOKEN_2022_PROGRAM_ID));
        assert!(buy_metas.contains(&constants::SPL_TOKEN_PROGRAM_ID));
        assert!(buy_metas.contains(&constants::NATIVE_MINT));
        assert!(buy_metas.contains(&pda::pump::sharing_config(&mint).0));
    }

    #[test]
    fn buy_v2_instruction_wires_quote_and_sharing_config_pdas() {
        let sdk = PumpSdk::new();
        let base_mint = fake_pubkey(81);
        let quote_mint = fake_pubkey(82);
        let user = fake_pubkey(83);
        let creator = fake_pubkey(84);
        let fee_recipient = fake_pubkey(85);
        let buyback_fee_recipient = fake_pubkey(86);
        let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
        let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;

        let ix = sdk.buy_v2_instruction(
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
            1_000,
            5_000_000,
        );

        assert_eq!(ix.program_id, crate::pump::ID);
        assert_eq!(
            &ix.data[..8],
            <client::args::BuyV2 as Discriminator>::DISCRIMINATOR,
        );

        let metas: Vec<Pubkey> = ix.accounts.iter().map(|m| m.pubkey).collect();
        assert!(metas.contains(&pda::pump::global().0));
        assert!(metas.contains(&base_mint));
        assert!(metas.contains(&quote_mint));
        assert!(metas.contains(&fee_recipient));
        assert!(metas.contains(&buyback_fee_recipient));
        assert!(metas.contains(&pda::pump::bonding_curve(&base_mint).0));
        assert!(metas.contains(&pda::pump::creator_vault(&creator).0));
        assert!(metas.contains(&pda::pump::sharing_config(&base_mint).0));
        assert!(metas.contains(&pda::pump::user_volume_accumulator(&user).0));
        assert!(metas.contains(&pda::pump::fee_config().0));
        assert!(metas.contains(&constants::FEE_PROGRAM_ID));

        // Quote-side ATAs derive against quote_token_program + quote_mint.
        assert!(metas
            .contains(&pda::associated_token(&fee_recipient, &quote_token_program, &quote_mint).0));
        assert!(metas.contains(
            &pda::associated_token(&buyback_fee_recipient, &quote_token_program, &quote_mint).0
        ));

        // Caller is the only signer.
        let signers: Vec<Pubkey> = ix
            .accounts
            .iter()
            .filter(|m| m.is_signer)
            .map(|m| m.pubkey)
            .collect();
        assert_eq!(signers, vec![user]);
    }

    #[test]
    fn buy_v2_instructions_prepends_four_ata_creates() {
        let sdk = PumpSdk::new();
        let base_mint = fake_pubkey(91);
        let quote_mint = fake_pubkey(92);
        let user = fake_pubkey(93);
        let creator = fake_pubkey(94);
        let fee_recipient = fake_pubkey(95);
        let buyback_fee_recipient = fake_pubkey(96);
        let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
        let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;

        let ixs = sdk.buy_v2_instructions(
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
            1_000,
            5_000_000,
        );

        // [base_user_ata, quote_bonding_curve_ata, quote_user_ata, quote_buyback_ata, buy_v2]
        assert_eq!(ixs.len(), 5);
        for ata_ix in &ixs[..4] {
            assert_eq!(ata_ix.program_id, constants::SPL_ATA_PROGRAM_ID);
            // ATA program's idempotent-create discriminator is `1`.
            assert_eq!(ata_ix.data, vec![1]);
        }
        assert_eq!(
            &ixs[4].data[..8],
            <client::args::BuyV2 as Discriminator>::DISCRIMINATOR,
        );

        // The four ATA-create ixs must derive against the same (mint, token_program)
        // pairs the on-chain program expects.
        let bonding_curve = pda::pump::bonding_curve(&base_mint).0;
        let expected_atas: [Pubkey; 4] = [
            pda::associated_token(&user, &base_token_program, &base_mint).0,
            pda::associated_token(&bonding_curve, &quote_token_program, &quote_mint).0,
            pda::associated_token(&user, &quote_token_program, &quote_mint).0,
            pda::associated_token(&buyback_fee_recipient, &quote_token_program, &quote_mint).0,
        ];
        for (i, expected) in expected_atas.iter().enumerate() {
            let metas: Vec<Pubkey> = ixs[i].accounts.iter().map(|m| m.pubkey).collect();
            assert!(metas.contains(expected), "ix {i} missing expected ATA");
        }
    }

    #[test]
    #[allow(deprecated)]
    fn sell_instruction_appends_user_volume_accumulator_then_bonding_curve_v2() {
        let sdk = PumpSdk::new();
        let mint = fake_pubkey(101);
        let user = fake_pubkey(102);
        let creator = fake_pubkey(103);
        let fee_recipient = fake_pubkey(104);
        let token_program = constants::SPL_TOKEN_PROGRAM_ID;

        let ix = sdk.sell_instruction(
            mint,
            user,
            creator,
            fee_recipient,
            100,
            1_000_000,
            token_program,
        );

        assert_eq!(ix.program_id, crate::pump::ID);
        assert_eq!(
            &ix.data[..8],
            <client::args::Sell as Discriminator>::DISCRIMINATOR,
        );

        let metas: Vec<Pubkey> = ix.accounts.iter().map(|m| m.pubkey).collect();
        assert!(metas.contains(&pda::pump::global().0));
        assert!(metas.contains(&pda::pump::bonding_curve(&mint).0));
        assert!(metas.contains(&pda::pump::creator_vault(&creator).0));
        assert!(metas.contains(&pda::pump::fee_config().0));
        assert!(metas.contains(&constants::FEE_PROGRAM_ID));
        assert!(metas.contains(&fee_recipient));
        assert!(metas.contains(&pda::associated_token(&user, &token_program, &mint).0));

        // Tail of the account list is exactly [user_volume_accumulator rw, bonding_curve_v2 ro].
        let n = ix.accounts.len();
        let uva = &ix.accounts[n - 2];
        let bcv2 = &ix.accounts[n - 1];
        assert_eq!(uva.pubkey, pda::pump::user_volume_accumulator(&user).0);
        assert!(uva.is_writable);
        assert!(!uva.is_signer);
        assert_eq!(bcv2.pubkey, pda::pump::bonding_curve_v2(&mint).0);
        assert!(!bcv2.is_writable);
        assert!(!bcv2.is_signer);
    }

    #[test]
    fn sell_v2_instruction_wires_quote_and_sharing_config_pdas() {
        let sdk = PumpSdk::new();
        let base_mint = fake_pubkey(111);
        let quote_mint = fake_pubkey(112);
        let user = fake_pubkey(113);
        let creator = fake_pubkey(114);
        let fee_recipient = fake_pubkey(115);
        let buyback_fee_recipient = fake_pubkey(116);
        let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
        let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;

        let ix = sdk.sell_v2_instruction(
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
            1_000,
            1_000_000,
        );

        assert_eq!(ix.program_id, crate::pump::ID);
        assert_eq!(
            &ix.data[..8],
            <client::args::SellV2 as Discriminator>::DISCRIMINATOR,
        );

        let metas: Vec<Pubkey> = ix.accounts.iter().map(|m| m.pubkey).collect();
        assert!(metas.contains(&pda::pump::global().0));
        assert!(metas.contains(&base_mint));
        assert!(metas.contains(&quote_mint));
        assert!(metas.contains(&fee_recipient));
        assert!(metas.contains(&buyback_fee_recipient));
        assert!(metas.contains(&pda::pump::bonding_curve(&base_mint).0));
        assert!(metas.contains(&pda::pump::creator_vault(&creator).0));
        assert!(metas.contains(&pda::pump::sharing_config(&base_mint).0));
        assert!(metas.contains(&pda::pump::user_volume_accumulator(&user).0));
        assert!(metas.contains(&pda::pump::fee_config().0));
        assert!(metas.contains(&constants::FEE_PROGRAM_ID));

        // sell_v2 has no global_volume_accumulator (unlike buy_v2).
        assert!(!metas.contains(&pda::pump::global_volume_accumulator().0));

        // Quote-side ATAs derive against quote_token_program + quote_mint.
        assert!(metas
            .contains(&pda::associated_token(&fee_recipient, &quote_token_program, &quote_mint).0));
        assert!(metas.contains(
            &pda::associated_token(&buyback_fee_recipient, &quote_token_program, &quote_mint).0
        ));

        // Caller is the only signer.
        let signers: Vec<Pubkey> = ix
            .accounts
            .iter()
            .filter(|m| m.is_signer)
            .map(|m| m.pubkey)
            .collect();
        assert_eq!(signers, vec![user]);
    }

    #[test]
    fn sell_v2_instructions_prepends_four_ata_creates() {
        let sdk = PumpSdk::new();
        let base_mint = fake_pubkey(121);
        let quote_mint = fake_pubkey(122);
        let user = fake_pubkey(123);
        let creator = fake_pubkey(124);
        let fee_recipient = fake_pubkey(125);
        let buyback_fee_recipient = fake_pubkey(126);
        let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
        let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;

        let ixs = sdk.sell_v2_instructions(
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
            1_000,
            1_000_000,
        );

        // [base_user_ata, quote_bonding_curve_ata, quote_user_ata, quote_buyback_ata, sell_v2]
        assert_eq!(ixs.len(), 5);
        for ata_ix in &ixs[..4] {
            assert_eq!(ata_ix.program_id, constants::SPL_ATA_PROGRAM_ID);
            // ATA program's idempotent-create discriminator is `1`.
            assert_eq!(ata_ix.data, vec![1]);
        }
        assert_eq!(
            &ixs[4].data[..8],
            <client::args::SellV2 as Discriminator>::DISCRIMINATOR,
        );

        // The four ATA-create ixs must derive against the same (mint, token_program)
        // pairs the on-chain program expects.
        let bonding_curve = pda::pump::bonding_curve(&base_mint).0;
        let expected_atas: [Pubkey; 4] = [
            pda::associated_token(&user, &base_token_program, &base_mint).0,
            pda::associated_token(&bonding_curve, &quote_token_program, &quote_mint).0,
            pda::associated_token(&user, &quote_token_program, &quote_mint).0,
            pda::associated_token(&buyback_fee_recipient, &quote_token_program, &quote_mint).0,
        ];
        for (i, expected) in expected_atas.iter().enumerate() {
            let metas: Vec<Pubkey> = ixs[i].accounts.iter().map(|m| m.pubkey).collect();
            assert!(metas.contains(expected), "ix {i} missing expected ATA");
        }
    }

    // ------------------------------------------------------------------
    // pump_amm buy / sell instruction tests.
    //
    // Mirror the IDL ordering at `idls/pump_amm.json` (buy: 23 base accounts,
    // sell: 21) and the remaining-account layout from
    // `pump-swap-sdk/src/sdk/offlinePumpAmm.ts:638-882`.
    // ------------------------------------------------------------------

    fn fake_amm_inputs() -> (
        Pubkey,
        Pubkey,
        Pubkey,
        Pubkey,
        Pubkey,
        Pubkey,
        Pubkey,
        Pubkey,
    ) {
        (
            fake_pubkey(0xA1),               // pool
            fake_pubkey(0xA2),               // base_mint
            fake_pubkey(0xA3),               // quote_mint
            fake_pubkey(0xA4),               // user
            fake_pubkey(0xA5),               // coin_creator
            fake_pubkey(0xA6),               // protocol_fee_recipient
            fake_pubkey(0xA7),               // buyback_fee_recipient
            constants::SPL_TOKEN_PROGRAM_ID, // quote_token_program (placeholder)
        )
    }

    #[test]
    fn buy_amm_instruction_wires_pump_amm_pdas() {
        let sdk = PumpSdk::new();
        let (pool, base_mint, quote_mint, user, coin_creator, fee_rec, buyback, _) =
            fake_amm_inputs();
        let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
        let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;

        let ix = sdk.buy_amm_instruction(
            pool,
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            coin_creator,
            fee_rec,
            buyback,
            true, // is_cashback_coin
            1_000,
            500_000,
        );

        assert_eq!(ix.program_id, crate::pump_amm::ID);
        assert_eq!(
            &ix.data[..8],
            <amm_client::args::Buy as Discriminator>::DISCRIMINATOR,
        );
        // 23 IDL accounts + 1 cashback ATA + 1 bonding_curve_v2 + 2 buyback (recipient + ATA).
        assert_eq!(ix.accounts.len(), 27);

        let pubkeys: Vec<Pubkey> = ix.accounts.iter().map(|m| m.pubkey).collect();
        // Spot-check IDL-derived PDAs are present.
        assert!(pubkeys.contains(&pda::pump_amm::global_config().0));
        assert!(pubkeys.contains(&pda::pump_amm::event_authority().0));
        assert!(pubkeys.contains(&pda::pump_amm::global_volume_accumulator().0));
        assert!(pubkeys.contains(&pda::pump_amm::user_volume_accumulator(&user).0));
        assert!(pubkeys.contains(&pda::pump_amm::fee_config().0));
        assert!(pubkeys.contains(&pda::pump_amm::coin_creator_vault_authority(&coin_creator).0));

        // First account is the pool (writable, not signer); user is signer.
        assert_eq!(ix.accounts[0].pubkey, pool);
        assert!(ix.accounts[0].is_writable);
        assert!(!ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[1].pubkey, user);
        assert!(ix.accounts[1].is_signer);

        // Tail layout: [cashback_ata rw, bonding_curve_v2 ro, buyback rw, buyback_ata rw].
        let n = ix.accounts.len();
        let cashback = &ix.accounts[n - 4];
        let bcv2 = &ix.accounts[n - 3];
        let buyback_meta = &ix.accounts[n - 2];
        let buyback_ata = &ix.accounts[n - 1];

        // Cashback ATA on buy uses NATIVE_MINT, not quote_mint.
        let user_vol_accum = pda::pump_amm::user_volume_accumulator(&user).0;
        assert_eq!(
            cashback.pubkey,
            pda::associated_token(
                &user_vol_accum,
                &quote_token_program,
                &constants::NATIVE_MINT,
            )
            .0
        );
        assert!(cashback.is_writable);

        assert_eq!(bcv2.pubkey, pda::pump::bonding_curve_v2(&base_mint).0);
        assert!(!bcv2.is_writable);

        // buyback_fee_recipient is WRITABLE on buy.
        assert_eq!(buyback_meta.pubkey, buyback);
        assert!(buyback_meta.is_writable);

        assert_eq!(
            buyback_ata.pubkey,
            pda::associated_token(&buyback, &quote_token_program, &quote_mint).0
        );
        assert!(buyback_ata.is_writable);
    }

    #[test]
    fn buy_amm_instruction_skips_remaining_when_no_cashback_no_creator() {
        let sdk = PumpSdk::new();
        let (pool, base_mint, quote_mint, user, _, fee_rec, buyback, _) = fake_amm_inputs();

        let ix = sdk.buy_amm_instruction(
            pool,
            base_mint,
            quote_mint,
            constants::SPL_TOKEN_2022_PROGRAM_ID,
            constants::SPL_TOKEN_PROGRAM_ID,
            user,
            Pubkey::default(), // default coin_creator -> no bonding_curve_v2 remaining
            fee_rec,
            buyback,
            false, // no cashback
            1_000,
            500_000,
        );

        // 23 IDL accounts + 0 + 0 + 2 (always-on buyback pair) = 25.
        assert_eq!(ix.accounts.len(), 25);
        let n = ix.accounts.len();
        assert_eq!(ix.accounts[n - 2].pubkey, buyback);
        assert!(ix.accounts[n - 2].is_writable);
    }

    #[test]
    fn sell_amm_instruction_wires_pump_amm_pdas() {
        let sdk = PumpSdk::new();
        let (pool, base_mint, quote_mint, user, coin_creator, fee_rec, buyback, _) =
            fake_amm_inputs();
        let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
        let quote_token_program = constants::SPL_TOKEN_PROGRAM_ID;

        let ix = sdk.sell_amm_instruction(
            pool,
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            coin_creator,
            fee_rec,
            buyback,
            true, // is_cashback_coin
            1_000_000,
            500,
        );

        assert_eq!(ix.program_id, crate::pump_amm::ID);
        assert_eq!(
            &ix.data[..8],
            <amm_client::args::Sell as Discriminator>::DISCRIMINATOR,
        );
        // 21 IDL accounts + 2 cashback + 1 bonding_curve_v2 + 2 buyback = 26.
        assert_eq!(ix.accounts.len(), 26);

        // Sell does NOT include global_volume_accumulator (unlike buy).
        let pubkeys: Vec<Pubkey> = ix.accounts.iter().map(|m| m.pubkey).collect();
        assert!(!pubkeys.contains(&pda::pump_amm::global_volume_accumulator().0));

        let user_vol_accum = pda::pump_amm::user_volume_accumulator(&user).0;
        let n = ix.accounts.len();
        // Tail: [cashback_ata rw, user_vol_accum rw, bonding_curve_v2 ro, buyback ro, buyback_ata rw]
        let cashback_ata = &ix.accounts[n - 5];
        let cashback_uva = &ix.accounts[n - 4];
        let bcv2 = &ix.accounts[n - 3];
        let buyback_meta = &ix.accounts[n - 2];
        let buyback_ata = &ix.accounts[n - 1];

        // Cashback ATA on sell uses quote_mint (not NATIVE_MINT — the buy side does that).
        assert_eq!(
            cashback_ata.pubkey,
            pda::associated_token(&user_vol_accum, &quote_token_program, &quote_mint).0
        );
        assert!(cashback_ata.is_writable);

        assert_eq!(cashback_uva.pubkey, user_vol_accum);
        assert!(cashback_uva.is_writable);

        assert_eq!(bcv2.pubkey, pda::pump::bonding_curve_v2(&base_mint).0);
        assert!(!bcv2.is_writable);

        // buyback_fee_recipient is READ-ONLY on sell (writable on buy).
        assert_eq!(buyback_meta.pubkey, buyback);
        assert!(!buyback_meta.is_writable);

        assert_eq!(
            buyback_ata.pubkey,
            pda::associated_token(&buyback, &quote_token_program, &quote_mint).0
        );
        assert!(buyback_ata.is_writable);
    }

    #[test]
    fn sell_amm_instruction_skips_remaining_when_no_cashback_no_creator() {
        let sdk = PumpSdk::new();
        let (pool, base_mint, quote_mint, user, _, fee_rec, buyback, _) = fake_amm_inputs();

        let ix = sdk.sell_amm_instruction(
            pool,
            base_mint,
            quote_mint,
            constants::SPL_TOKEN_2022_PROGRAM_ID,
            constants::SPL_TOKEN_PROGRAM_ID,
            user,
            Pubkey::default(),
            fee_rec,
            buyback,
            false,
            1_000_000,
            500,
        );

        // 21 IDL accounts + 0 + 0 + 2 (always-on buyback pair) = 23.
        assert_eq!(ix.accounts.len(), 23);
        let n = ix.accounts.len();
        // buyback is read-only on sell.
        assert_eq!(ix.accounts[n - 2].pubkey, buyback);
        assert!(!ix.accounts[n - 2].is_writable);
    }

    #[test]
    fn buy_amm_instructions_prepends_one_ata_create() {
        let sdk = PumpSdk::new();
        let (pool, base_mint, quote_mint, user, coin_creator, fee_rec, buyback, _) =
            fake_amm_inputs();

        let ixs = sdk.buy_amm_instructions(
            pool,
            base_mint,
            quote_mint,
            constants::SPL_TOKEN_2022_PROGRAM_ID,
            constants::SPL_TOKEN_PROGRAM_ID,
            user,
            coin_creator,
            fee_rec,
            buyback,
            false,
            1_000,
            500_000,
        );

        assert_eq!(ixs.len(), 2);
        // First is the SPL ATA program's idempotent instruction (single byte 1).
        assert_eq!(ixs[0].program_id, constants::SPL_ATA_PROGRAM_ID);
        assert_eq!(ixs[0].data, vec![1]);
        // Second is the AMM buy.
        assert_eq!(ixs[1].program_id, crate::pump_amm::ID);
        assert_eq!(
            &ixs[1].data[..8],
            <amm_client::args::Buy as Discriminator>::DISCRIMINATOR,
        );
    }

    #[test]
    fn sell_amm_instructions_prepends_one_ata_create() {
        let sdk = PumpSdk::new();
        let (pool, base_mint, quote_mint, user, coin_creator, fee_rec, buyback, _) =
            fake_amm_inputs();

        let ixs = sdk.sell_amm_instructions(
            pool,
            base_mint,
            quote_mint,
            constants::SPL_TOKEN_2022_PROGRAM_ID,
            constants::SPL_TOKEN_PROGRAM_ID,
            user,
            coin_creator,
            fee_rec,
            buyback,
            false,
            1_000_000,
            500,
        );

        assert_eq!(ixs.len(), 2);
        assert_eq!(ixs[0].program_id, constants::SPL_ATA_PROGRAM_ID);
        assert_eq!(ixs[0].data, vec![1]);
        assert_eq!(ixs[1].program_id, crate::pump_amm::ID);
        assert_eq!(
            &ixs[1].data[..8],
            <amm_client::args::Sell as Discriminator>::DISCRIMINATOR,
        );
    }

    // ------------------------------------------------------------------
    // Complete trade-flow tests (trade_tx_instructions / create_coin_instructions).
    // ------------------------------------------------------------------

    fn user_wsol_ata(user: &Pubkey) -> Pubkey {
        pda::associated_token(
            user,
            &constants::SPL_TOKEN_PROGRAM_ID,
            &constants::NATIVE_MINT,
        )
        .0
    }

    /// `system_instruction::transfer` is a system-program instruction whose
    /// data is `[2u32 LE, lamports u64 LE]`. Pulls out the lamports for assertions.
    fn parse_system_transfer_lamports(ix: &Instruction) -> u64 {
        assert_eq!(ix.program_id, system_program::ID);
        assert_eq!(ix.data.len(), 12);
        assert_eq!(&ix.data[..4], &2u32.to_le_bytes());
        u64::from_le_bytes(ix.data[4..12].try_into().unwrap())
    }

    #[test]
    fn trade_tx_buy_bc_wraps_sol_and_unwraps_residual() {
        let sdk = PumpSdk::new();
        let mint = fake_pubkey(0xB1);
        let user = fake_pubkey(0xB2);
        let creator = fake_pubkey(0xB3);
        let fee_recipient = fake_pubkey(0xB4);
        let buyback_fee_recipient = fake_pubkey(0xB5);
        let max_sol = 7_500_000u64;

        let ixs = sdk.trade_tx_instructions(TradeTxParams {
            mint,
            base_token_program: constants::SPL_TOKEN_2022_PROGRAM_ID,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
            is_buy: true,
            venue: TradeVenue::BondingCurve,
            is_cashback_coin: false,
            base_amount: 1_000,
            sol_amount_threshold: max_sol,
        });

        // [ata×4, system_transfer, sync_native, buy_v2, close_account]
        assert_eq!(ixs.len(), 8);
        for ata_ix in &ixs[..4] {
            assert_eq!(ata_ix.program_id, constants::SPL_ATA_PROGRAM_ID);
            assert_eq!(ata_ix.data, vec![1]);
        }
        assert_eq!(parse_system_transfer_lamports(&ixs[4]), max_sol);
        assert_eq!(ixs[5].program_id, constants::SPL_TOKEN_PROGRAM_ID);
        assert_eq!(ixs[6].program_id, crate::pump::ID);
        assert_eq!(
            &ixs[6].data[..8],
            <client::args::BuyV2 as Discriminator>::DISCRIMINATOR,
        );
        assert_eq!(ixs[7].program_id, constants::SPL_TOKEN_PROGRAM_ID);

        // sync_native and close_account both target the user's wSOL ATA.
        let wsol_ata = user_wsol_ata(&user);
        assert!(ixs[5].accounts.iter().any(|m| m.pubkey == wsol_ata));
        assert!(ixs[7].accounts.iter().any(|m| m.pubkey == wsol_ata));

        // ATA prefixes match the four (mint, token_program) pairs the program expects.
        let bonding_curve = pda::pump::bonding_curve(&mint).0;
        let expected_atas: [Pubkey; 4] = [
            pda::associated_token(&user, &constants::SPL_TOKEN_2022_PROGRAM_ID, &mint).0,
            pda::associated_token(
                &bonding_curve,
                &constants::SPL_TOKEN_PROGRAM_ID,
                &constants::NATIVE_MINT,
            )
            .0,
            wsol_ata,
            pda::associated_token(
                &buyback_fee_recipient,
                &constants::SPL_TOKEN_PROGRAM_ID,
                &constants::NATIVE_MINT,
            )
            .0,
        ];
        for (i, expected) in expected_atas.iter().enumerate() {
            let metas: Vec<Pubkey> = ixs[i].accounts.iter().map(|m| m.pubkey).collect();
            assert!(metas.contains(expected), "ata ix {i} missing expected ATA");
        }
    }

    #[test]
    fn trade_tx_sell_bc_unwraps_proceeds_no_wrap() {
        let sdk = PumpSdk::new();
        let mint = fake_pubkey(0xC1);
        let user = fake_pubkey(0xC2);
        let creator = fake_pubkey(0xC3);
        let fee_recipient = fake_pubkey(0xC4);
        let buyback_fee_recipient = fake_pubkey(0xC5);

        let ixs = sdk.trade_tx_instructions(TradeTxParams {
            mint,
            base_token_program: constants::SPL_TOKEN_2022_PROGRAM_ID,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
            is_buy: false,
            venue: TradeVenue::BondingCurve,
            is_cashback_coin: false,
            base_amount: 1_000,
            sol_amount_threshold: 1_000_000,
        });

        // [ata×4, sell_v2, close_account] — no system_transfer or sync_native on sell.
        assert_eq!(ixs.len(), 6);
        for ata_ix in &ixs[..4] {
            assert_eq!(ata_ix.program_id, constants::SPL_ATA_PROGRAM_ID);
            assert_eq!(ata_ix.data, vec![1]);
        }
        assert_eq!(ixs[4].program_id, crate::pump::ID);
        assert_eq!(
            &ixs[4].data[..8],
            <client::args::SellV2 as Discriminator>::DISCRIMINATOR,
        );
        assert_eq!(ixs[5].program_id, constants::SPL_TOKEN_PROGRAM_ID);
        assert!(ixs[5]
            .accounts
            .iter()
            .any(|m| m.pubkey == user_wsol_ata(&user)));
    }

    #[test]
    fn trade_tx_buy_amm_wraps_and_unwraps_with_cashback() {
        let sdk = PumpSdk::new();
        let mint = fake_pubkey(0xD1);
        let user = fake_pubkey(0xD2);
        let coin_creator = fake_pubkey(0xD3);
        let fee_recipient = fake_pubkey(0xD4);
        let buyback_fee_recipient = fake_pubkey(0xD5);
        let pool = fake_pubkey(0xD6);
        let max_sol = 2_500_000u64;

        let ixs = sdk.trade_tx_instructions(TradeTxParams {
            mint,
            base_token_program: constants::SPL_TOKEN_2022_PROGRAM_ID,
            user,
            creator: coin_creator,
            fee_recipient,
            buyback_fee_recipient,
            is_buy: true,
            venue: TradeVenue::Amm { pool },
            is_cashback_coin: true,
            base_amount: 1_000,
            sol_amount_threshold: max_sol,
        });

        // [ata_base, ata_wsol, system_transfer, sync_native, buy_amm, close_account]
        assert_eq!(ixs.len(), 6);
        assert_eq!(ixs[0].program_id, constants::SPL_ATA_PROGRAM_ID);
        assert_eq!(ixs[1].program_id, constants::SPL_ATA_PROGRAM_ID);
        assert_eq!(parse_system_transfer_lamports(&ixs[2]), max_sol);
        assert_eq!(ixs[3].program_id, constants::SPL_TOKEN_PROGRAM_ID);
        assert_eq!(ixs[4].program_id, crate::pump_amm::ID);
        assert_eq!(
            &ixs[4].data[..8],
            <amm_client::args::Buy as Discriminator>::DISCRIMINATOR,
        );
        assert_eq!(ixs[5].program_id, constants::SPL_TOKEN_PROGRAM_ID);

        // The buy_amm instruction must include both the cashback remaining account
        // (NATIVE_MINT-flavoured) and the bonding_curve_v2 readonly tail (non-default creator).
        let pubkeys: Vec<Pubkey> = ixs[4].accounts.iter().map(|m| m.pubkey).collect();
        let user_vol_accum = pda::pump_amm::user_volume_accumulator(&user).0;
        assert!(pubkeys.contains(
            &pda::associated_token(
                &user_vol_accum,
                &constants::SPL_TOKEN_PROGRAM_ID,
                &constants::NATIVE_MINT,
            )
            .0
        ));
        assert!(pubkeys.contains(&pda::pump::bonding_curve_v2(&mint).0));
    }

    #[test]
    fn trade_tx_sell_amm_unwraps_proceeds_no_wrap() {
        let sdk = PumpSdk::new();
        let mint = fake_pubkey(0xE1);
        let user = fake_pubkey(0xE2);
        let coin_creator = fake_pubkey(0xE3);
        let fee_recipient = fake_pubkey(0xE4);
        let buyback_fee_recipient = fake_pubkey(0xE5);
        let pool = fake_pubkey(0xE6);

        let ixs = sdk.trade_tx_instructions(TradeTxParams {
            mint,
            base_token_program: constants::SPL_TOKEN_2022_PROGRAM_ID,
            user,
            creator: coin_creator,
            fee_recipient,
            buyback_fee_recipient,
            is_buy: false,
            venue: TradeVenue::Amm { pool },
            is_cashback_coin: false,
            base_amount: 5_000,
            sol_amount_threshold: 100,
        });

        // [ata_wsol, sell_amm, close_account]
        assert_eq!(ixs.len(), 3);
        assert_eq!(ixs[0].program_id, constants::SPL_ATA_PROGRAM_ID);
        let wsol_ata = user_wsol_ata(&user);
        assert!(ixs[0].accounts.iter().any(|m| m.pubkey == wsol_ata));
        assert_eq!(ixs[1].program_id, crate::pump_amm::ID);
        assert_eq!(
            &ixs[1].data[..8],
            <amm_client::args::Sell as Discriminator>::DISCRIMINATOR,
        );
        assert_eq!(ixs[2].program_id, constants::SPL_TOKEN_PROGRAM_ID);
        assert!(ixs[2].accounts.iter().any(|m| m.pubkey == wsol_ata));
    }

    #[test]
    fn create_coin_instructions_emits_full_create_buy_with_wrap() {
        let sdk = PumpSdk::new();
        let mint = fake_pubkey(0xF1);
        let user = fake_pubkey(0xF2);
        let creator = fake_pubkey(0xF3);
        let fee_recipient = fake_pubkey(0xF4);
        let buyback_fee_recipient = fake_pubkey(0xF5);
        let max_sol = 1_234_567u64;

        let ixs = sdk.create_coin_instructions(CreateCoinParams {
            mint,
            user,
            creator,
            name: "Test".into(),
            symbol: "TST".into(),
            uri: "https://example.com/metadata.json".into(),
            mayhem_mode: false,
            cashback: false,
            fee_recipient,
            buyback_fee_recipient,
            token_amount: 1_000,
            max_sol_cost: max_sol,
        });

        // [create_v2, ata×4, system_transfer, sync_native, buy_v2, close_account]
        assert_eq!(ixs.len(), 9);
        assert_eq!(
            &ixs[0].data[..8],
            <client::args::CreateV2 as Discriminator>::DISCRIMINATOR,
        );
        for ata_ix in &ixs[1..5] {
            assert_eq!(ata_ix.program_id, constants::SPL_ATA_PROGRAM_ID);
            assert_eq!(ata_ix.data, vec![1]);
        }
        assert_eq!(parse_system_transfer_lamports(&ixs[5]), max_sol);
        assert_eq!(ixs[6].program_id, constants::SPL_TOKEN_PROGRAM_ID);
        assert_eq!(ixs[7].program_id, crate::pump::ID);
        assert_eq!(
            &ixs[7].data[..8],
            <client::args::BuyV2 as Discriminator>::DISCRIMINATOR,
        );
        // Buy is v2 with base=Token-2022, quote=wSOL/SPL Token.
        let buy_metas: Vec<Pubkey> = ixs[7].accounts.iter().map(|m| m.pubkey).collect();
        assert!(buy_metas.contains(&constants::SPL_TOKEN_2022_PROGRAM_ID));
        assert!(buy_metas.contains(&constants::NATIVE_MINT));
        assert!(buy_metas.contains(&pda::pump::sharing_config(&mint).0));

        assert_eq!(ixs[8].program_id, constants::SPL_TOKEN_PROGRAM_ID);
        let wsol_ata = user_wsol_ata(&user);
        assert!(ixs[8].accounts.iter().any(|m| m.pubkey == wsol_ata));
    }

    /// Sanity: ensure the IDL-derived program IDs match the canonical addresses
    /// from the TS SDK so we don't silently regress against a future IDL change.
    #[test]
    fn program_ids_match_idl() {
        assert_eq!(
            crate::pump::ID,
            pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P")
        );
        assert_eq!(
            crate::pump_amm::ID,
            pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA")
        );
    }

    // ------------------------------------------------------------------
    // Quote parity tests.
    //
    // Reference values were generated by a Python port of the TS algorithm
    // and cross-verified against `@pump-fun/pump-sdk-internal@1.0.0-devnet.29`
    // and `@pump-fun/pump-swap-sdk@1.15.0` directly (see commit history /
    // sibling repro script `tools/quote_parity_check.cjs`). If the on-chain
    // formulas change, regenerate the constants.
    // ------------------------------------------------------------------

    use crate::state::pump_amm::{GlobalConfig, Pool};
    use crate::state::{BondingCurve, Global};

    /// Standard pump global with flat fees (1% protocol, 0.5% creator) and
    /// realistic supply / migration fee. All other fields zero/default.
    fn fixture_global() -> Global {
        Global {
            fee_basis_points: 100,
            creator_fee_basis_points: 50,
            token_total_supply: 1_000_000_000_000_000,
            initial_real_token_reserves: 793_100_000_000_000,
            pool_migration_fee: 6_900_000_000,
            ..Default::default()
        }
    }

    /// Pre-trade bonding curve, real creator (so creator fee applies).
    fn fixture_bonding_curve(creator: Pubkey) -> BondingCurve {
        BondingCurve {
            virtual_token_reserves: 1_073_000_000_000_000,
            virtual_quote_reserves: 30_000_000_000,
            real_token_reserves: 793_100_000_000_000,
            real_quote_reserves: 0,
            token_total_supply: 1_000_000_000_000_000,
            complete: false,
            creator,
            is_mayhem_mode: false,
            is_cashback_coin: false,
            quote_mint: constants::NATIVE_MINT,
        }
    }

    fn fixture_amm_global_config() -> GlobalConfig {
        GlobalConfig {
            lp_fee_basis_points: 20,
            protocol_fee_basis_points: 5,
            coin_creator_fee_basis_points: 5,
            ..Default::default()
        }
    }

    /// Non-pump pool: `creator` deliberately not the pool-authority PDA so
    /// the no-fee-config path is exercised purely against `GlobalConfig`.
    fn fixture_pool(coin_creator: Pubkey) -> Pool {
        Pool {
            base_mint: fake_pubkey(0xAA),
            creator: fake_pubkey(0xBB),
            coin_creator,
            quote_mint: constants::NATIVE_MINT,
            ..Default::default()
        }
    }

    // -------- Bonding curve parity --------

    #[test]
    fn bc_buy_sol_in_matches_ts() {
        let sdk = PumpSdk::new();
        let g = fixture_global();
        let bc = fixture_bonding_curve(fake_pubkey(7));
        let q = sdk
            .buy_quote_bonding_curve_sol_in(
                &g,
                None,
                &bc,
                g.token_total_supply,
                1_000_000_000, // 1 SOL
                100,           // 1% slippage
            )
            .unwrap();
        assert_eq!(q.amount, 34_117_646_995_895);
        assert_eq!(q.min_out, 33_776_470_525_936);
        assert_eq!(q.input_amount_used, 1_000_000_000);
        assert_eq!(q.max_input, 1_000_000_000);
    }

    #[test]
    fn bc_buy_token_out_matches_ts() {
        let sdk = PumpSdk::new();
        let g = fixture_global();
        let bc = fixture_bonding_curve(fake_pubkey(7));
        let q = sdk
            .buy_quote_bonding_curve_token_out(
                &g,
                None,
                &bc,
                g.token_total_supply,
                100_000_000_000_000,
                200, // 2% slippage
            )
            .unwrap();
        assert_eq!(q.amount, 3_129_496_404);
        assert_eq!(q.max_input, 3_192_086_332);
        assert_eq!(q.min_out, 100_000_000_000_000);
        assert_eq!(q.input_amount_used, 100_000_000_000_000);
    }

    #[test]
    fn bc_sell_with_creator_matches_ts() {
        let sdk = PumpSdk::new();
        let g = fixture_global();
        let bc = fixture_bonding_curve(fake_pubkey(7));
        let q = sdk
            .sell_quote_bonding_curve(
                &g,
                None,
                &bc,
                g.token_total_supply,
                50_000_000_000_000,
                50, // 0.5% slippage
            )
            .unwrap();
        assert_eq!(q.amount, 1_315_672_305);
        assert_eq!(q.min_out, 1_309_093_943);
    }

    #[test]
    fn bc_sell_default_creator_skips_creator_fee() {
        let sdk = PumpSdk::new();
        let g = fixture_global();
        let bc = fixture_bonding_curve(Pubkey::default());
        let q = sdk
            .sell_quote_bonding_curve(&g, None, &bc, g.token_total_supply, 50_000_000_000_000, 0)
            .unwrap();
        // Output is strictly larger than the with-creator case because the
        // 0.5% creator slice is not subtracted.
        assert_eq!(q.amount, 1_322_350_845);
    }

    // -------- AMM parity --------

    #[test]
    fn amm_buy_sol_in_matches_ts() {
        let sdk = PumpSdk::new();
        let gc = fixture_amm_global_config();
        let pool = fixture_pool(fake_pubkey(0xCC));
        let q = sdk
            .buy_quote_amm_sol_in(
                &gc,
                None,
                AmmQuoteSource::Pool {
                    pool: &pool,
                    base_reserve: 200_000_000_000_000,
                    quote_reserve: 30_000_000_000,
                    base_mint_supply: 1_000_000_000_000_000,
                },
                1_000_000_000,
                100,
            )
            .unwrap();
        assert_eq!(q.amount, 6_432_936_635_069);
        assert_eq!(q.min_out, 6_368_607_268_718);
        assert_eq!(q.max_input, 1_010_000_000); // pump-swap-sdk buyQuoteInput maxQuote @ 1% bps
    }

    #[test]
    fn amm_buy_token_out_matches_ts() {
        let sdk = PumpSdk::new();
        let gc = fixture_amm_global_config();
        let pool = fixture_pool(fake_pubkey(0xCC));
        let q = sdk
            .buy_quote_amm_token_out(
                &gc,
                None,
                AmmQuoteSource::Pool {
                    pool: &pool,
                    base_reserve: 200_000_000_000_000,
                    quote_reserve: 30_000_000_000,
                    base_mint_supply: 1_000_000_000_000_000,
                },
                1_000_000_000_000,
                100,
            )
            .unwrap();
        assert_eq!(q.amount, 151_206_031);
        assert_eq!(q.max_input, 152_718_091);
    }

    #[test]
    fn amm_sell_with_coin_creator_matches_ts() {
        let sdk = PumpSdk::new();
        let gc = fixture_amm_global_config();
        let pool = fixture_pool(fake_pubkey(0xCC));
        let q = sdk
            .sell_quote_amm(
                &gc,
                None,
                AmmQuoteSource::Pool {
                    pool: &pool,
                    base_reserve: 200_000_000_000_000,
                    quote_reserve: 30_000_000_000,
                    base_mint_supply: 1_000_000_000_000_000,
                },
                1_000_000_000_000,
                100,
            )
            .unwrap();
        assert_eq!(q.amount, 148_805_969);
        assert_eq!(q.min_out, 147_317_909);
    }

    #[test]
    fn amm_sell_default_coin_creator_skips_creator_fee() {
        let sdk = PumpSdk::new();
        let gc = fixture_amm_global_config();
        let pool = fixture_pool(Pubkey::default());
        let q = sdk
            .sell_quote_amm(
                &gc,
                None,
                AmmQuoteSource::Pool {
                    pool: &pool,
                    base_reserve: 200_000_000_000_000,
                    quote_reserve: 30_000_000_000,
                    base_mint_supply: 1_000_000_000_000_000,
                },
                1_000_000_000_000,
                0,
            )
            .unwrap();
        assert_eq!(q.amount, 148_880_596);
    }

    // -------- Edge cases --------

    #[test]
    fn zero_input_returns_zero() {
        let sdk = PumpSdk::new();
        let g = fixture_global();
        let bc = fixture_bonding_curve(fake_pubkey(7));
        let q = sdk
            .buy_quote_bonding_curve_sol_in(&g, None, &bc, g.token_total_supply, 0, 100)
            .unwrap();
        assert_eq!(q.amount, 0);
        let q = sdk
            .sell_quote_bonding_curve(&g, None, &bc, g.token_total_supply, 0, 100)
            .unwrap();
        assert_eq!(q.amount, 0);
    }

    #[test]
    fn migrated_curve_returns_zero() {
        let sdk = PumpSdk::new();
        let g = fixture_global();
        let mut bc = fixture_bonding_curve(fake_pubkey(7));
        bc.virtual_token_reserves = 0; // signals already-migrated curve
        let q = sdk
            .buy_quote_bonding_curve_sol_in(&g, None, &bc, g.token_total_supply, 1_000_000_000, 0)
            .unwrap();
        assert_eq!(q.amount, 0);
    }

    // -------- Error paths --------

    #[test]
    fn amm_empty_reserves_errors() {
        let sdk = PumpSdk::new();
        let gc = fixture_amm_global_config();
        let pool = fixture_pool(fake_pubkey(0xCC));
        let err = sdk
            .buy_quote_amm_sol_in(
                &gc,
                None,
                AmmQuoteSource::Pool {
                    pool: &pool,
                    base_reserve: 0,
                    quote_reserve: 1_000,
                    base_mint_supply: 1_000,
                },
                1,
                0,
            )
            .unwrap_err();
        assert_eq!(err, QuoteError::EmptyReserves);
        let err = sdk
            .sell_quote_amm(
                &gc,
                None,
                AmmQuoteSource::Pool {
                    pool: &pool,
                    base_reserve: 1_000,
                    quote_reserve: 0,
                    base_mint_supply: 1_000,
                },
                1,
                0,
            )
            .unwrap_err();
        assert_eq!(err, QuoteError::EmptyReserves);
    }

    #[test]
    fn amm_buy_token_out_oversize_errors() {
        let sdk = PumpSdk::new();
        let gc = fixture_amm_global_config();
        let pool = fixture_pool(fake_pubkey(0xCC));
        // base_out == base_reserve (would deplete pool)
        let err = sdk
            .buy_quote_amm_token_out(
                &gc,
                None,
                AmmQuoteSource::Pool {
                    pool: &pool,
                    base_reserve: 1_000,
                    quote_reserve: 1_000,
                    base_mint_supply: 1_000_000,
                },
                1_000,
                0,
            )
            .unwrap_err();
        assert_eq!(err, QuoteError::BaseOutExceedsReserve);
        // base_out > base_reserve
        let err = sdk
            .buy_quote_amm_token_out(
                &gc,
                None,
                AmmQuoteSource::Pool {
                    pool: &pool,
                    base_reserve: 1_000,
                    quote_reserve: 1_000,
                    base_mint_supply: 1_000_000,
                },
                5_000,
                0,
            )
            .unwrap_err();
        assert_eq!(err, QuoteError::BaseOutExceedsReserve);
    }

    #[test]
    fn bc_token_out_buy_at_real_reserves_errors() {
        let sdk = PumpSdk::new();
        let g = fixture_global();
        let mut bc = fixture_bonding_curve(fake_pubkey(7));
        // Force degenerate state: real_token_reserves = virtual_token_reserves.
        bc.real_token_reserves = bc.virtual_token_reserves;
        let err = sdk
            .buy_quote_bonding_curve_token_out(
                &g,
                None,
                &bc,
                g.token_total_supply,
                bc.virtual_token_reserves,
                0,
            )
            .unwrap_err();
        assert_eq!(err, QuoteError::DepletedBondingCurve);
    }

    #[test]
    fn amm_quote_bonding_curve_complete_underflow_errors() {
        let sdk = PumpSdk::new();
        let g = fixture_global();
        let gc = fixture_amm_global_config();
        let bc = fixture_bonding_curve(fake_pubkey(7)); // real_quote_reserves = 0
        let base_mint = fake_pubkey(0xAA);
        let err = sdk
            .buy_quote_amm_sol_in(
                &gc,
                None,
                AmmQuoteSource::BondingCurveComplete {
                    global: &g,
                    bonding_curve: &bc,
                    base_mint: &base_mint,
                    base_mint_supply: g.token_total_supply,
                },
                1_000_000_000,
                0,
            )
            .unwrap_err();
        assert_eq!(err, QuoteError::EmptyReserves);
    }

    #[test]
    fn amm_quote_bonding_curve_complete_matches_formula() {
        let sdk = PumpSdk::new();
        let g = fixture_global();
        let gc = fixture_amm_global_config();
        let mut bc = fixture_bonding_curve(fake_pubkey(7));
        bc.real_quote_reserves = 100_000_000_000;
        let base_mint = fake_pubkey(0xAA);

        let q = sdk
            .buy_quote_amm_sol_in(
                &gc,
                None,
                AmmQuoteSource::BondingCurveComplete {
                    global: &g,
                    bonding_curve: &bc,
                    base_mint: &base_mint,
                    base_mint_supply: g.token_total_supply,
                },
                1_000_000_000,
                0,
            )
            .unwrap();

        let base_reserve = g.token_total_supply - g.initial_real_token_reserves;
        let quote_reserve = bc.real_quote_reserves - g.pool_migration_fee;
        let pool_creator = pda::pump::pool_authority(&base_mint).0;
        let ctx = AmmContext {
            global_config: &gc,
            fee_config: None,
            base_mint: &base_mint,
            pool_creator: &pool_creator,
            coin_creator: &bc.creator,
            base_reserve,
            quote_reserve,
            base_mint_supply: g.token_total_supply,
        };
        let expected = amm_math::buy_quote_input(&ctx, 1_000_000_000)
            .unwrap()
            .base_amount_out;
        assert_eq!(q.amount, expected);
    }
}
