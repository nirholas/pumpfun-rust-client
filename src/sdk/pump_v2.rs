use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use anchor_lang::system_program;
use anchor_lang::{InstructionData, ToAccountMetas};

use crate::math::utils::{mul_div_u128, swap_exact_input};
use crate::math::{QuoteError, QuoteResult};
use crate::state::pump_amm::Pool;
use crate::state::{BondingCurve, BondingCurveFromIdl, Global, QuoteControl};
use crate::token::create_associated_token_account_idempotent;
use crate::{
    constants, pda,
    pump::client,
    pump::types::{OptionBool, OptionU64},
    pump_agent_payments::client as agent_client,
};

use super::{CreateCoinParams, PumpSdk};

/// Pubkeys derived once and threaded into both `BuyV2` and `SellV2` (the two
/// instructions share the same account set apart from `BuyV2`'s extra
/// `global_volume_accumulator`). `quote_mint` is the resolved value
/// (default → wSOL); `base_token_program` is fixed for v2 (Token-2022).
struct V2TradeAccounts {
    quote_mint: Pubkey,
    base_token_program: Pubkey,
    bonding_curve: Pubkey,
    creator_vault: Pubkey,
    user_volume_accumulator: Pubkey,
    associated_quote_fee_recipient: Pubkey,
    associated_quote_buyback_fee_recipient: Pubkey,
    associated_base_bonding_curve: Pubkey,
    associated_quote_bonding_curve: Pubkey,
    associated_base_user: Pubkey,
    associated_quote_user: Pubkey,
    associated_creator_vault: Pubkey,
    associated_user_volume_accumulator: Pubkey,
}

impl V2TradeAccounts {
    fn derive(
        base_mint: Pubkey,
        quote_mint: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        creator: Pubkey,
        fee_recipient: Pubkey,
        buyback_fee_recipient: Pubkey,
    ) -> Self {
        let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
        let quote_mint = if quote_mint == Pubkey::default() {
            constants::NATIVE_MINT
        } else {
            quote_mint
        };
        let bonding_curve = pda::pump::bonding_curve(&base_mint).0;
        let creator_vault = pda::pump::creator_vault(&creator).0;
        let user_volume_accumulator = pda::pump::user_volume_accumulator(&user).0;
        let ata = |owner: &Pubkey, token_program: &Pubkey, mint: &Pubkey| {
            pda::associated_token(owner, token_program, mint).0
        };
        Self {
            quote_mint,
            base_token_program,
            bonding_curve,
            creator_vault,
            user_volume_accumulator,
            associated_quote_fee_recipient: ata(&fee_recipient, &quote_token_program, &quote_mint),
            associated_quote_buyback_fee_recipient: ata(
                &buyback_fee_recipient,
                &quote_token_program,
                &quote_mint,
            ),
            associated_base_bonding_curve: ata(&bonding_curve, &base_token_program, &base_mint),
            associated_quote_bonding_curve: ata(&bonding_curve, &quote_token_program, &quote_mint),
            associated_base_user: ata(&user, &base_token_program, &base_mint),
            associated_quote_user: ata(&user, &quote_token_program, &quote_mint),
            associated_creator_vault: ata(&creator_vault, &quote_token_program, &quote_mint),
            associated_user_volume_accumulator: ata(
                &user_volume_accumulator,
                &quote_token_program,
                &quote_mint,
            ),
        }
    }
}

impl PumpSdk {
    pub(crate) fn resolve_quote_mint(quote_mint: Pubkey) -> Pubkey {
        if quote_mint == Pubkey::default() {
            constants::NATIVE_MINT
        } else {
            quote_mint
        }
    }

    /// Initial `virtual_quote_reserves` `create_v2` would give a curve quoted
    /// in `quote_mint`, mirroring the program's precedence: SOL/default →
    /// `Global.initial_virtual_sol_reserves`; a `Global`-whitelisted mint →
    /// `Global.initial_virtual_quote_reserves`; otherwise the mint's
    /// `quote-control` entry. `None` means the mint is admitted nowhere and
    /// `create_v2` would fail with `UnsupportedQuoteMint`.
    pub fn initial_virtual_quote_reserves(
        global: &Global,
        quote_control: Option<&QuoteControl>,
        quote_mint: &Pubkey,
    ) -> Option<u64> {
        if *quote_mint == Pubkey::default() || *quote_mint == constants::NATIVE_MINT {
            return Some(global.initial_virtual_sol_reserves);
        }
        if global.whitelisted_quote_mints.contains(quote_mint) {
            return Some(global.initial_virtual_quote_reserves);
        }
        quote_control?
            .mints
            .iter()
            .find(|m| m.mint == *quote_mint)
            .map(|m| m.initial_virtual_quote_reserves)
    }

    /// `create_v2` `(virtual_quote_reserves, depth)` for a coin quoted in pump coin Q
    /// (`quote_curve`; `quote_pool` = `(pool, base vault, quote vault)` balances once Q migrated).
    /// Target's graduation raise is bought on Q's constant product; the Q it returns is the new
    /// curve's raise, and the seed is that raise scaled back by the raise ratio. No supply bound.
    pub fn pump_quote_initial_virtual_quote_reserves(
        global: &Global,
        quote_control: Option<&QuoteControl>,
        quote_curve: &BondingCurve,
        quote_pool: Option<(&Pool, u64, u64)>,
    ) -> QuoteResult<(u64, u8)> {
        let depth = quote_curve
            .depth
            .checked_add(1)
            .ok_or(QuoteError::MathOverflow)?;
        if depth > global.max_curve_depth {
            return Err(QuoteError::CurveDepthExceeded);
        }
        if quote_curve.is_mayhem_mode {
            return Err(QuoteError::QuoteCurveNotEligible);
        }
        // Only the zero key takes the SOL seed on-chain; a stored wSOL is a `Global` quote.
        let q = &quote_curve.quote_mint;
        let target = if quote_curve.depth > 0 {
            quote_curve.initial_virtual_quote_reserves
        } else if *q == Pubkey::default() {
            global.initial_virtual_sol_reserves
        } else if *q == constants::NATIVE_MINT || global.whitelisted_quote_mints.contains(q) {
            global.initial_virtual_quote_reserves
        } else {
            quote_control
                .and_then(|qc| qc.mints.iter().find(|m| m.mint == *q))
                .map(|m| m.initial_virtual_quote_reserves)
                .ok_or(QuoteError::QuoteCurveNotEligible)?
        };

        let migrated = quote_curve.real_quote_reserves == 0
            && quote_curve.virtual_quote_reserves == 0
            && quote_curve.real_token_reserves == 0
            && quote_curve.virtual_token_reserves == 0;
        let (base, quote) = if migrated {
            let (pool, base_vault, quote_vault) =
                quote_pool.ok_or(QuoteError::QuotePoolAccountsRequired)?;
            let quote = i128::from(quote_vault)
                .checked_add(pool.virtual_quote_reserves)
                .and_then(|q| u128::try_from(q).ok())
                .ok_or(QuoteError::MathOverflow)?;
            (base_vault, quote)
        } else if quote_curve.complete {
            return Err(QuoteError::QuoteCurveAwaitingMigration);
        } else {
            (
                quote_curve.virtual_token_reserves,
                u128::from(quote_curve.virtual_quote_reserves),
            )
        };

        // Target's graduation raise bought on Q's constant product is the new curve's raise;
        // the seed is that raise scaled back by the raise ratio (`seed_for_raise`). An emptied
        // quote side prices nothing on-chain (`DivisionByZero`).
        if quote == 0 {
            return Err(QuoteError::MathOverflow);
        }
        let real = u128::from(global.initial_real_token_reserves);
        let unsold = u128::from(global.initial_virtual_token_reserves)
            .checked_sub(real)
            .ok_or(QuoteError::MathOverflow)?;
        let raise = swap_exact_input(
            mul_div_u128(target.into(), real, unsold)?,
            quote,
            base.into(),
        )?;
        let derived = u64::try_from(mul_div_u128(raise, unsold, real)?)
            .map_err(|_| QuoteError::MathOverflow)?;
        if derived == 0 {
            return Err(QuoteError::QuoteReservesOutOfRange);
        }
        Ok((derived, depth))
    }

    /// Remaining accounts `[4..]` of a pump-quoted `create_v2`: Q's curve, then Q's
    /// canonical pool `(key, state)` and its vaults once Q migrated.
    pub fn create_v2_pump_quote_accounts(
        quote_mint: &Pubkey,
        quote_pool: Option<(Pubkey, &Pool)>,
    ) -> Vec<AccountMeta> {
        let mut metas = vec![AccountMeta::new_readonly(
            pda::pump::bonding_curve(quote_mint).0,
            false,
        )];
        if let Some((key, pool)) = quote_pool {
            metas.extend(
                [
                    key,
                    pool.pool_base_token_account,
                    pool.pool_quote_token_account,
                ]
                .map(|k| AccountMeta::new_readonly(k, false)),
            );
        }
        metas
    }

    /// The [`BondingCurve`] `create_v2` would initialize (mirrors on-chain
    /// `BondingCurve::init`), for quoting the first buy of a coin that does not
    /// exist yet. `virtual_quote_reserves` comes from
    /// [`Self::initial_virtual_quote_reserves`] (or
    /// [`Self::pump_quote_initial_virtual_quote_reserves`] for a pump-coin quote).
    #[allow(clippy::too_many_arguments)]
    pub fn initial_bonding_curve(
        global: &Global,
        creator: Pubkey,
        quote_mint: Pubkey,
        virtual_quote_reserves: u64,
        mayhem_mode: bool,
        cashback: bool,
        creator_fee_bps: u64,
    ) -> BondingCurve {
        BondingCurve::new(BondingCurveFromIdl {
            virtual_token_reserves: global.initial_virtual_token_reserves,
            virtual_quote_reserves,
            real_token_reserves: global.initial_real_token_reserves,
            real_quote_reserves: 0,
            token_total_supply: global.token_total_supply,
            complete: false,
            creator,
            is_mayhem_mode: mayhem_mode,
            is_cashback_coin: cashback,
            quote_mint: Self::resolve_quote_mint(quote_mint),
            creator_fee_bps,
            can_edit_creator_fee: false,
            initial_virtual_quote_reserves: virtual_quote_reserves,
            // Holder reward and the fee buckets start zeroed.
            ..Default::default()
        })
    }

    /// `create_v2` (Token-2022, Mayhem PDAs).
    ///
    /// `quote_mint` selects the curve's quote asset. Pass `Pubkey::default()`
    /// or [`constants::NATIVE_MINT`] for the standard wSOL curve. Any other
    /// mint (USDC, an xStock, …) is a non-SOL quote and four remaining
    /// accounts are appended: the quote mint, the bonding curve's quote ATA
    /// (derived with `quote_token_program`), `quote_token_program` (SPL Token
    /// or Token-2022, whichever owns the mint), and the `quote-control` PDA.
    /// The program reads the PDA only when `Global` does not whitelist the
    /// mint, so it is always safe to pass. Quote-control-only mints cannot be
    /// combined with `mayhem_mode`.
    ///
    /// For a pump-coin quote append [`Self::create_v2_pump_quote_accounts`]; such a coin
    /// cannot be mayhem, and budget ~250k CU.
    ///
    /// `creator_fee_bps` is the coin's own creator fee rate; `0` keeps the
    /// pump-fees schedule rate. It is honoured for quote-control and pump-quote
    /// coins only (ignored on SOL / `Global`-whitelisted quotes). A nonzero value requires
    /// `Global.creator_fee_configurable`, a non-cashback coin, and
    /// `1..=Global.max_configurable_creator_fee_bps`.
    #[allow(clippy::too_many_arguments)]
    pub fn create_v2_instruction(
        &self,
        mint: Pubkey,
        user: Pubkey,
        name: impl Into<String>,
        symbol: impl Into<String>,
        uri: impl Into<String>,
        creator: Pubkey,
        quote_mint: Pubkey,
        quote_token_program: Pubkey,
        mayhem_mode: bool,
        cashback: bool,
        creator_fee_bps: u64,
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
            creator_fee_bps: OptionU64(creator_fee_bps),
            is_holder_reward: OptionBool(false),
        };
        let mut metas = accounts.to_account_metas(None);
        let resolved_quote_mint = if quote_mint == Pubkey::default() {
            constants::NATIVE_MINT
        } else {
            quote_mint
        };
        if resolved_quote_mint != constants::NATIVE_MINT {
            let associated_quote_bonding_curve =
                pda::associated_token(&bonding_curve, &quote_token_program, &resolved_quote_mint).0;
            metas.push(AccountMeta::new_readonly(resolved_quote_mint, false));
            metas.push(AccountMeta::new(associated_quote_bonding_curve, false));
            metas.push(AccountMeta::new_readonly(quote_token_program, false));
            metas.push(AccountMeta::new_readonly(
                pda::pump::quote_control().0,
                false,
            ));
        }
        Instruction {
            program_id: crate::pump::ID,
            accounts: metas,
            data: args.data(),
        }
    }

    /// `buy_v2` with fee recipients from [`Global`] and quote layout from [`BondingCurve`].
    pub fn buy_v2_instruction(
        &self,
        global: &Global,
        bonding_curve: &BondingCurve,
        base_mint: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        amount: u64,
        max_quote_tokens: u64,
    ) -> Option<Instruction> {
        let (fee_recipient, buyback_fee_recipient) =
            Self::pump_fee_recipients_pair(global, bonding_curve.is_mayhem_mode)?;
        Some(self.buy_v2_instruction_with_recipients(
            base_mint,
            bonding_curve.quote_mint,
            quote_token_program,
            user,
            bonding_curve.creator,
            fee_recipient,
            buyback_fee_recipient,
            amount,
            max_quote_tokens,
        ))
    }

    pub(crate) fn buy_v2_instruction_with_recipients(
        &self,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        creator: Pubkey,
        fee_recipient: Pubkey,
        buyback_fee_recipient: Pubkey,
        amount: u64,
        max_sol_cost: u64,
    ) -> Instruction {
        let a = V2TradeAccounts::derive(
            base_mint,
            quote_mint,
            quote_token_program,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
        );
        let accounts = client::accounts::BuyV2 {
            global: pda::pump::global().0,
            base_mint,
            quote_mint: a.quote_mint,
            base_token_program: a.base_token_program,
            quote_token_program,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            fee_recipient,
            associated_quote_fee_recipient: a.associated_quote_fee_recipient,
            buyback_fee_recipient,
            associated_quote_buyback_fee_recipient: a.associated_quote_buyback_fee_recipient,
            bonding_curve: a.bonding_curve,
            associated_base_bonding_curve: a.associated_base_bonding_curve,
            associated_quote_bonding_curve: a.associated_quote_bonding_curve,
            user,
            associated_base_user: a.associated_base_user,
            associated_quote_user: a.associated_quote_user,
            creator_vault: a.creator_vault,
            associated_creator_vault: a.associated_creator_vault,
            sharing_config: pda::pump::sharing_config(&base_mint).0,
            global_volume_accumulator: pda::pump::global_volume_accumulator().0,
            user_volume_accumulator: a.user_volume_accumulator,
            associated_user_volume_accumulator: a.associated_user_volume_accumulator,
            fee_config: pda::pump::fee_config().0,
            fee_program: constants::FEE_PROGRAM_ID,
            system_program: system_program::ID,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
        };
        let args = client::args::BuyV2 {
            amount,
            max_sol_cost,
            // Fill what the curve still holds instead of failing, as the quote
            // helpers assume.
            partial_fill: OptionBool(true),
        };
        Instruction {
            program_id: crate::pump::ID,
            accounts: accounts.to_account_metas(None),
            data: args.data(),
        }
    }

    /// `buy_v2` with idempotent ATA creates for base/quote user accounts (skipped when `quote_mint` is the default).
    pub fn buy_v2_instructions(
        &self,
        global: &Global,
        bonding_curve: &BondingCurve,
        base_mint: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        amount: u64,
        max_quote_tokens: u64,
    ) -> Option<Vec<Instruction>> {
        let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
        let (fee_recipient, buyback_fee_recipient) =
            Self::pump_fee_recipients_pair(global, bonding_curve.is_mayhem_mode)?;
        let quote_mint = bonding_curve.quote_mint;
        let creator = bonding_curve.creator;
        let mut instructions = Self::user_trade_atas(
            user,
            base_mint,
            bonding_curve,
            base_token_program,
            quote_token_program,
            true,
        );
        instructions.push(self.buy_v2_instruction_with_recipients(
            base_mint,
            quote_mint,
            quote_token_program,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
            amount,
            max_quote_tokens,
        ));
        Some(instructions)
    }

    /// `buy_exact_quote_in_v2`: spend exactly `spendable_quote_in`, requiring at least `min_tokens_out` base tokens out.
    pub fn buy_exact_quote_in_v2_instruction(
        &self,
        global: &Global,
        bonding_curve: &BondingCurve,
        base_mint: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        spendable_quote_in: u64,
        min_tokens_out: u64,
    ) -> Option<Instruction> {
        let (fee_recipient, buyback_fee_recipient) =
            Self::pump_fee_recipients_pair(global, bonding_curve.is_mayhem_mode)?;
        Some(self.buy_exact_quote_in_v2_instruction_with_recipients(
            base_mint,
            bonding_curve.quote_mint,
            quote_token_program,
            user,
            bonding_curve.creator,
            fee_recipient,
            buyback_fee_recipient,
            spendable_quote_in,
            min_tokens_out,
        ))
    }

    pub(crate) fn buy_exact_quote_in_v2_instruction_with_recipients(
        &self,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        creator: Pubkey,
        fee_recipient: Pubkey,
        buyback_fee_recipient: Pubkey,
        spendable_quote_in: u64,
        min_tokens_out: u64,
    ) -> Instruction {
        let a = V2TradeAccounts::derive(
            base_mint,
            quote_mint,
            quote_token_program,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
        );
        let accounts = client::accounts::BuyExactQuoteInV2 {
            global: pda::pump::global().0,
            base_mint,
            quote_mint: a.quote_mint,
            base_token_program: a.base_token_program,
            quote_token_program,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            fee_recipient,
            associated_quote_fee_recipient: a.associated_quote_fee_recipient,
            buyback_fee_recipient,
            associated_quote_buyback_fee_recipient: a.associated_quote_buyback_fee_recipient,
            bonding_curve: a.bonding_curve,
            associated_base_bonding_curve: a.associated_base_bonding_curve,
            associated_quote_bonding_curve: a.associated_quote_bonding_curve,
            user,
            associated_base_user: a.associated_base_user,
            associated_quote_user: a.associated_quote_user,
            creator_vault: a.creator_vault,
            associated_creator_vault: a.associated_creator_vault,
            sharing_config: pda::pump::sharing_config(&base_mint).0,
            global_volume_accumulator: pda::pump::global_volume_accumulator().0,
            user_volume_accumulator: a.user_volume_accumulator,
            associated_user_volume_accumulator: a.associated_user_volume_accumulator,
            fee_config: pda::pump::fee_config().0,
            fee_program: constants::FEE_PROGRAM_ID,
            system_program: system_program::ID,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
        };
        let args = client::args::BuyExactQuoteInV2 {
            spendable_quote_in,
            min_tokens_out,
            // Fill what the curve still holds instead of failing, as the quote
            // helpers assume.
            partial_fill: OptionBool(true),
        };
        Instruction {
            program_id: crate::pump::ID,
            accounts: accounts.to_account_metas(None),
            data: args.data(),
        }
    }

    /// `buy_exact_quote_in_v2` with idempotent ATA creates for base/quote user accounts (skipped when `quote_mint` is the default).
    pub fn buy_exact_quote_in_v2_instructions(
        &self,
        global: &Global,
        bonding_curve: &BondingCurve,
        base_mint: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        spendable_quote_in: u64,
        min_tokens_out: u64,
    ) -> Option<Vec<Instruction>> {
        let base_token_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
        let (fee_recipient, buyback_fee_recipient) =
            Self::pump_fee_recipients_pair(global, bonding_curve.is_mayhem_mode)?;
        let quote_mint = bonding_curve.quote_mint;
        let creator = bonding_curve.creator;
        let mut instructions = Self::user_trade_atas(
            user,
            base_mint,
            bonding_curve,
            base_token_program,
            quote_token_program,
            true,
        );
        instructions.push(self.buy_exact_quote_in_v2_instruction_with_recipients(
            base_mint,
            quote_mint,
            quote_token_program,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
            spendable_quote_in,
            min_tokens_out,
        ));
        Some(instructions)
    }

    /// `sell_v2`; fee recipients from [`Global`], quote accounts from [`BondingCurve`].
    pub fn sell_v2_instruction(
        &self,
        global: &Global,
        bonding_curve: &BondingCurve,
        base_mint: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        amount: u64,
        min_sol_output: u64,
    ) -> Option<Instruction> {
        let (fee_recipient, buyback_fee_recipient) =
            Self::pump_fee_recipients_pair(global, bonding_curve.is_mayhem_mode)?;
        Some(self.sell_v2_instruction_with_recipients(
            base_mint,
            bonding_curve.quote_mint,
            quote_token_program,
            user,
            bonding_curve.creator,
            fee_recipient,
            buyback_fee_recipient,
            amount,
            min_sol_output,
        ))
    }

    pub(crate) fn sell_v2_instruction_with_recipients(
        &self,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        creator: Pubkey,
        fee_recipient: Pubkey,
        buyback_fee_recipient: Pubkey,
        amount: u64,
        min_sol_output: u64,
    ) -> Instruction {
        let a = V2TradeAccounts::derive(
            base_mint,
            quote_mint,
            quote_token_program,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
        );
        let accounts = client::accounts::SellV2 {
            global: pda::pump::global().0,
            base_mint,
            quote_mint: a.quote_mint,
            base_token_program: a.base_token_program,
            quote_token_program,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            fee_recipient,
            associated_quote_fee_recipient: a.associated_quote_fee_recipient,
            buyback_fee_recipient,
            associated_quote_buyback_fee_recipient: a.associated_quote_buyback_fee_recipient,
            bonding_curve: a.bonding_curve,
            associated_base_bonding_curve: a.associated_base_bonding_curve,
            associated_quote_bonding_curve: a.associated_quote_bonding_curve,
            user,
            associated_base_user: a.associated_base_user,
            associated_quote_user: a.associated_quote_user,
            creator_vault: a.creator_vault,
            associated_creator_vault: a.associated_creator_vault,
            sharing_config: pda::pump::sharing_config(&base_mint).0,
            user_volume_accumulator: a.user_volume_accumulator,
            associated_user_volume_accumulator: a.associated_user_volume_accumulator,
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

    /// `sell_v2` with idempotent ATA creates for base/quote user accounts (skipped when `quote_mint` is the default).
    pub fn sell_v2_instructions(
        &self,
        global: &Global,
        bonding_curve: &BondingCurve,
        base_mint: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        amount: u64,
        min_sol_output: u64,
    ) -> Option<Vec<Instruction>> {
        let (fee_recipient, buyback_fee_recipient) =
            Self::pump_fee_recipients_pair(global, bonding_curve.is_mayhem_mode)?;
        let quote_mint = bonding_curve.quote_mint;
        let creator = bonding_curve.creator;
        let mut instructions = Self::user_trade_atas(
            user,
            base_mint,
            bonding_curve,
            constants::SPL_TOKEN_2022_PROGRAM_ID,
            quote_token_program,
            false,
        );
        instructions.push(self.sell_v2_instruction_with_recipients(
            base_mint,
            quote_mint,
            quote_token_program,
            user,
            creator,
            fee_recipient,
            buyback_fee_recipient,
            amount,
            min_sol_output,
        ));
        Some(instructions)
    }

    /// `create_v2` then [`Self::buy_v2_instructions`]. Pass
    /// `quote_mint = Pubkey::default()` for a wSOL-quoted coin, or a
    /// supported quote mint (e.g. USDC) for a non-native quote, with
    /// `quote_token_program` the program owning that mint (see
    /// [`Self::create_v2_instruction`]). `creator_fee_bps = 0` keeps the
    /// schedule rate. Pass `tokenized_agent_buyback_bps = Some(bps)` to also
    /// append [`Self::agent_initialize_instruction`] in the same tx (mirrors
    /// the frontend's tokenized-agent flow). On a pump-coin quote, extend the
    /// first instruction with [`Self::create_v2_pump_quote_accounts`].
    #[allow(clippy::too_many_arguments)]
    pub fn create_v2_and_buy_instruction(
        &self,
        mint: Pubkey,
        user: Pubkey,
        name: impl Into<String>,
        symbol: impl Into<String>,
        uri: impl Into<String>,
        creator: Pubkey,
        quote_mint: Pubkey,
        quote_token_program: Pubkey,
        mayhem_mode: bool,
        cashback: bool,
        creator_fee_bps: u64,
        tokenized_agent_buyback_bps: Option<u16>,
        global: &Global,
        amount: u64,
        max_quote_tokens: u64,
    ) -> Option<Vec<Instruction>> {
        let bonding_curve_preview = BondingCurve::new(BondingCurveFromIdl {
            creator,
            is_cashback_coin: cashback,
            is_mayhem_mode: mayhem_mode,
            quote_mint: Self::resolve_quote_mint(quote_mint),
            creator_fee_bps,
            ..Default::default()
        });
        let buy_v2_instructions = self.buy_v2_instructions(
            global,
            &bonding_curve_preview,
            mint,
            quote_token_program,
            user,
            amount,
            max_quote_tokens,
        )?;
        let mut instructions = vec![self.create_v2_instruction(
            mint,
            user,
            name,
            symbol,
            uri,
            creator,
            quote_mint,
            quote_token_program,
            mayhem_mode,
            cashback,
            creator_fee_bps,
        )];

        instructions.extend(buy_v2_instructions);

        if let Some(buyback_bps) = tokenized_agent_buyback_bps {
            instructions.push(self.agent_initialize_instruction(mint, user, creator, buyback_bps));
        }

        Some(instructions)
    }

    /// `agent_initialize` (pump_agent_payments). Initializes the
    /// `TokenAgentPayments` PDA for `mint` with `creator` as the agent
    /// payment authority. `user` signs and pays for the new account.
    pub fn agent_initialize_instruction(
        &self,
        mint: Pubkey,
        user: Pubkey,
        creator: Pubkey,
        buyback_bps: u16,
    ) -> Instruction {
        let accounts = agent_client::accounts::AgentInitialize {
            authority: user,
            bonding_curve: pda::pump::bonding_curve(&mint).0,
            global_config: pda::pump_agent_payments::global_config().0,
            mint,
            token_agent_payments: pda::pump_agent_payments::token_agent_payments(&mint).0,
            system_program: system_program::ID,
            event_authority: pda::pump_agent_payments::event_authority().0,
            program: crate::pump_agent_payments::ID,
        };
        // The current pump-agent-payments program refuses this instruction
        // (`AgentInitializationNotSupported`, 6015); kept for deployments that
        // still admit new agents.
        let args = agent_client::args::AgentInitialize {
            _authority: creator,
            _buyback_bps: buyback_bps,
        };
        let mut metas = accounts.to_account_metas(None);
        metas.push(AccountMeta::new_readonly(
            pda::pump::sharing_config(&mint).0,
            false,
        ));
        Instruction {
            program_id: crate::pump_agent_payments::ID,
            accounts: metas,
            data: args.data(),
        }
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

    /// `migrate_v2`: migrates a completed bonding curve into a pump_amm pool
    /// with an arbitrary quote mint. `user` signs and pays;
    /// `withdraw_authority` is the migration authority. `base_token_program` /
    /// `quote_token_program` own the respective mints.
    /// The two boost PDAs are mandatory to be passed as remaining accounts.
    pub fn migrate_v2_instruction(
        &self,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        user: Pubkey,
        withdraw_authority: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
    ) -> Instruction {
        let token_2022_program = constants::SPL_TOKEN_2022_PROGRAM_ID;
        let bonding_curve = pda::pump::bonding_curve(&base_mint).0;
        let pool_authority = pda::pump::pool_authority(&base_mint).0;
        let pool = pda::pump_amm::pool(0, &pool_authority, &base_mint, &quote_mint).0;
        let lp_mint = pda::pump_amm::lp_mint(&pool).0;
        let accounts = client::accounts::MigrateV2 {
            global: pda::pump::global().0,
            withdraw_authority,
            base_mint,
            quote_mint,
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
            system_program: system_program::ID,
            pump_amm: crate::pump_amm::ID,
            pool,
            pool_authority,
            pool_authority_mint_account: pda::associated_token(
                &pool_authority,
                &base_token_program,
                &base_mint,
            )
            .0,
            pool_authority_quote_account: pda::associated_token(
                &pool_authority,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            amm_global_config: pda::pump_amm::global_config().0,
            lp_mint,
            user_pool_token_account: pda::associated_token(
                &pool_authority,
                &token_2022_program,
                &lp_mint,
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
            base_token_program,
            quote_token_program,
            token_2022_program,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            pump_amm_event_authority: pda::pump_amm::event_authority().0,
            rent: constants::RENT_SYSVAR_ID,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
        };
        let mut metas = accounts.to_account_metas(None);
        const MIGRATE_V2_NAMED_ACCOUNTS: usize = 27;
        assert_eq!(
            metas.len(),
            MIGRATE_V2_NAMED_ACCOUNTS,
            "MigrateV2 named-account layout changed; check latest IDL",
        );
        metas.extend([
            AccountMeta::new_readonly(pda::pump_amm::boost_vault_authority(&pool).0, false),
            AccountMeta::new(
                pda::pump_amm::boost_vault(&pool, &quote_mint, &quote_token_program).0,
                false,
            ),
        ]);
        Instruction {
            program_id: crate::pump::ID,
            accounts: metas,
            data: client::args::MigrateV2.data(),
        }
    }

    pub fn claim_cashback_v2_instruction(
        &self,
        user: Pubkey,
        quote_mint: Pubkey,
        quote_token_program: Pubkey,
    ) -> Instruction {
        let resolved_quote_mint = if quote_mint == Pubkey::default() {
            constants::NATIVE_MINT
        } else {
            quote_mint
        };
        let user_volume_accumulator = pda::pump::user_volume_accumulator(&user).0;
        let associated_user_volume_accumulator = pda::associated_token(
            &user_volume_accumulator,
            &quote_token_program,
            &resolved_quote_mint,
        )
        .0;
        let associated_quote_user =
            pda::associated_token(&user, &quote_token_program, &resolved_quote_mint).0;
        let accounts = client::accounts::ClaimCashbackV2 {
            user,
            user_volume_accumulator,
            quote_mint: resolved_quote_mint,
            quote_token_program,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            associated_user_volume_accumulator,
            associated_quote_user,
            system_program: system_program::ID,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
        };
        Instruction {
            program_id: crate::pump::ID,
            accounts: accounts.to_account_metas(None),
            data: client::args::ClaimCashbackV2.data(),
        }
    }

    pub fn collect_creator_fee_v2_instructions(
        &self,
        payer: Pubkey,
        creator: Pubkey,
        quote_mint: Pubkey,
        quote_token_program: Pubkey,
        create_creator_ata: bool,
    ) -> Vec<Instruction> {
        let resolved_quote_mint = if quote_mint == Pubkey::default() {
            constants::NATIVE_MINT
        } else {
            quote_mint
        };
        let mut ixs = Vec::with_capacity(2);
        if create_creator_ata {
            ixs.push(create_associated_token_account_idempotent(
                &payer,
                &creator,
                &resolved_quote_mint,
                &quote_token_program,
            ));
        }
        ixs.push(self.collect_creator_fee_v2_instruction(
            creator,
            resolved_quote_mint,
            quote_token_program,
        ));
        ixs
    }

    pub fn collect_creator_fee_v2_instruction(
        &self,
        creator: Pubkey,
        quote_mint: Pubkey,
        quote_token_program: Pubkey,
    ) -> Instruction {
        let resolved_quote_mint = if quote_mint == Pubkey::default() {
            constants::NATIVE_MINT
        } else {
            quote_mint
        };
        let creator_vault = pda::pump::creator_vault(&creator).0;
        let creator_token_account =
            pda::associated_token(&creator, &quote_token_program, &resolved_quote_mint).0;
        let creator_vault_token_account =
            pda::associated_token(&creator_vault, &quote_token_program, &resolved_quote_mint).0;
        let accounts = client::accounts::CollectCreatorFeeV2 {
            creator,
            creator_token_account,
            creator_vault,
            creator_vault_token_account,
            quote_mint: resolved_quote_mint,
            quote_token_program,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            system_program: system_program::ID,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
        };
        Instruction {
            program_id: crate::pump::ID,
            accounts: accounts.to_account_metas(None),
            data: client::args::CollectCreatorFeeV2.data(),
        }
    }

    /// [`Self::distribute_creator_fees_v2_instruction`] behind
    /// [`Self::sweep_creator_fee_instruction`] and the optional ATA creates.
    pub fn distribute_creator_fees_v2_instructions(
        &self,
        payer: Pubkey,
        mint: Pubkey,
        creator: Pubkey,
        quote_mint: Pubkey,
        quote_token_program: Pubkey,
        initialize_ata: bool,
        create_creator_vault_ata: bool,
        shareholders: &[Pubkey],
    ) -> Vec<Instruction> {
        let resolved_quote_mint = if quote_mint == Pubkey::default() {
            constants::NATIVE_MINT
        } else {
            quote_mint
        };
        let mut ixs = Vec::with_capacity(3 + shareholders.len());
        // The program refuses to distribute while v3 trades' creator fees sit
        // un-swept on the curve (`CreatorFeesNotSwept`); a no-op otherwise.
        ixs.push(self.sweep_creator_fee_instruction(
            payer,
            mint,
            creator,
            resolved_quote_mint,
            quote_token_program,
        ));
        if create_creator_vault_ata {
            let creator_vault = pda::pump::creator_vault(&creator).0;
            ixs.push(create_associated_token_account_idempotent(
                &payer,
                &creator_vault,
                &resolved_quote_mint,
                &quote_token_program,
            ));
        }
        for sh in shareholders {
            ixs.push(create_associated_token_account_idempotent(
                &payer,
                sh,
                &resolved_quote_mint,
                &quote_token_program,
            ));
        }
        ixs.push(self.distribute_creator_fees_v2_instruction(
            payer,
            mint,
            creator,
            resolved_quote_mint,
            quote_token_program,
            initialize_ata,
            shareholders,
        ));
        ixs
    }

    pub fn distribute_creator_fees_v2_instruction(
        &self,
        payer: Pubkey,
        mint: Pubkey,
        creator: Pubkey,
        quote_mint: Pubkey,
        quote_token_program: Pubkey,
        initialize_ata: bool,
        shareholders: &[Pubkey],
    ) -> Instruction {
        let resolved_quote_mint = if quote_mint == Pubkey::default() {
            constants::NATIVE_MINT
        } else {
            quote_mint
        };
        let bonding_curve = pda::pump::bonding_curve(&mint).0;
        let sharing_config = pda::pump::sharing_config(&mint).0;
        let creator_vault = pda::pump::creator_vault(&creator).0;
        let creator_vault_quote_token_account =
            pda::associated_token(&creator_vault, &quote_token_program, &resolved_quote_mint).0;
        let accounts = client::accounts::DistributeCreatorFeesV2 {
            payer,
            mint,
            bonding_curve,
            sharing_config,
            creator_vault,
            system_program: system_program::ID,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
            creator_vault_quote_token_account,
            quote_mint: resolved_quote_mint,
            quote_token_program,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
        };
        let mut metas = accounts.to_account_metas(None);
        for sh in shareholders {
            metas.push(AccountMeta::new(*sh, false));
        }
        if resolved_quote_mint != constants::NATIVE_MINT {
            for sh in shareholders {
                let sh_ata =
                    pda::associated_token(sh, &quote_token_program, &resolved_quote_mint).0;
                metas.push(AccountMeta::new(sh_ata, false));
            }
        }
        Instruction {
            program_id: crate::pump::ID,
            accounts: metas,
            data: client::args::DistributeCreatorFeesV2 { initialize_ata }.data(),
        }
    }

    /// `create_v2` then [`Self::buy_v2_instructions`]. Set
    /// `params.quote_mint = Pubkey::default()` for a wSOL-quoted coin, or a
    /// supported quote mint (e.g. USDC) for a non-native quote.
    /// Setting `params.tokenized_agent_buyback_bps = Some(bps)` also appends
    /// [`Self::agent_initialize_instruction`].
    /// On a pump-coin quote, extend the first instruction with
    /// [`Self::create_v2_pump_quote_accounts`].
    pub fn create_coin_instructions(
        &self,
        params: CreateCoinParams<'_>,
    ) -> Option<Vec<Instruction>> {
        let CreateCoinParams {
            mint,
            user,
            creator,
            name,
            symbol,
            uri,
            mayhem_mode,
            cashback,
            quote_mint,
            quote_token_program,
            creator_fee_bps,
            global,
            token_amount,
            max_quote_tokens,
            tokenized_agent_buyback_bps,
        } = params;
        let bonding_curve_preview = BondingCurve::new(BondingCurveFromIdl {
            creator,
            is_cashback_coin: cashback,
            is_mayhem_mode: mayhem_mode,
            quote_mint: Self::resolve_quote_mint(quote_mint),
            creator_fee_bps,
            ..Default::default()
        });

        let mut ixs: Vec<Instruction> = vec![self.create_v2_instruction(
            mint,
            user,
            name,
            symbol,
            uri,
            creator,
            quote_mint,
            quote_token_program,
            mayhem_mode,
            cashback,
            creator_fee_bps,
        )];
        ixs.extend(self.buy_v2_instructions(
            global,
            &bonding_curve_preview,
            mint,
            quote_token_program,
            user,
            token_amount,
            max_quote_tokens,
        )?);
        if let Some(buyback_bps) = tokenized_agent_buyback_bps {
            ixs.push(self.agent_initialize_instruction(mint, user, creator, buyback_bps));
        }
        Some(ixs)
    }

    /// Idempotent ATA creates for the user's base + quote token accounts plus
    /// the two PDA-owned quote ATAs (`associated_creator_vault` and
    /// `associated_user_volume_accumulator`) that v2 buy/sell reference. The
    /// base ATA is skipped when `include_base` is `false` (sell-side flows
    /// where the user already holds the base balance to spend). A default
    /// `bonding_curve.quote_mint` resolves to wSOL to match
    /// [`V2TradeAccounts::derive`].
    fn user_trade_atas(
        user: Pubkey,
        base_mint: Pubkey,
        bonding_curve: &BondingCurve,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        include_base: bool,
    ) -> Vec<Instruction> {
        let mut ixs = vec![];
        if include_base {
            ixs.push(create_associated_token_account_idempotent(
                &user,
                &user,
                &base_mint,
                &base_token_program,
            ));
        }
        let bonding_curve_has_non_sol_quote = bonding_curve.quote_mint != Pubkey::default()
            && bonding_curve.quote_mint != constants::NATIVE_MINT;
        // associated_quote_fee_recipient: Has alraedy been initialized, we will initalize this during initial testing.
        // associated_quote_buyback_fee_recipient: Has alraedy been initialized, we will initalize this during initial testing.
        if bonding_curve_has_non_sol_quote {
            ixs.push(create_associated_token_account_idempotent(
                &user,
                &user,
                &bonding_curve.quote_mint,
                &quote_token_program,
            ));
            ixs.push(create_associated_token_account_idempotent(
                &user,
                &bonding_curve.creator,
                &bonding_curve.quote_mint,
                &quote_token_program,
            ));
            // associated_quote_bonding_curve
            let bonding_curve_pda = pda::pump::bonding_curve(&base_mint).0;
            ixs.push(create_associated_token_account_idempotent(
                &user,
                &bonding_curve_pda,
                &bonding_curve.quote_mint,
                &quote_token_program,
            ));
            if bonding_curve.is_cashback_coin {
                let user_volume_accumulator = pda::pump::user_volume_accumulator(&user).0;
                ixs.push(create_associated_token_account_idempotent(
                    &user,
                    &user_volume_accumulator,
                    &bonding_curve.quote_mint,
                    &quote_token_program,
                ));
            }
        }
        ixs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::pump_amm::PoolFromIdl;
    use crate::state::{GlobalFromIdl, QuoteControlMint};

    // Mainnet `Global` constants, as pump's `pump_quote` tests.
    const VT0: u64 = 1_073_000_000_000_000;
    const REAL0: u64 = 793_100_000_000_000;
    const SUPPLY: u64 = 1_000_000_000_000_000;
    const SOL_SEED: u64 = 30_000_000_000;
    const USDC_SEED: u64 = 4_292_000_000;

    fn key(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    fn usdc() -> Pubkey {
        key(7)
    }

    fn global(max_curve_depth: u8) -> Global {
        Global::new(GlobalFromIdl {
            initial_virtual_token_reserves: VT0,
            initial_real_token_reserves: REAL0,
            token_total_supply: SUPPLY,
            initial_virtual_sol_reserves: SOL_SEED,
            initial_virtual_quote_reserves: USDC_SEED,
            whitelisted_quote_mints: [usdc()],
            max_curve_depth,
            ..Default::default()
        })
    }

    /// Q trading on its curve at `vt` / `vq`.
    fn on_curve(quote_mint: Pubkey, vt: u64, vq: u64) -> BondingCurve {
        BondingCurve::new(BondingCurveFromIdl {
            virtual_token_reserves: vt,
            virtual_quote_reserves: vq,
            real_token_reserves: REAL0,
            token_total_supply: SUPPLY,
            creator: key(9),
            quote_mint,
            ..Default::default()
        })
    }

    /// Q after `migrate_v2` reset its reserves.
    fn migrated(quote_mint: Pubkey) -> BondingCurve {
        BondingCurve::new(BondingCurveFromIdl {
            complete: true,
            quote_mint,
            ..Default::default()
        })
    }

    fn pool(virtual_quote_reserves: i128) -> Pool {
        Pool::new(PoolFromIdl {
            pool_base_token_account: key(3),
            pool_quote_token_account: key(4),
            virtual_quote_reserves,
            ..Default::default()
        })
    }

    fn derive(
        global: &Global,
        quote_control: Option<&QuoteControl>,
        curve: &BondingCurve,
        pool: Option<(&Pool, u64, u64)>,
    ) -> QuoteResult<(u64, u8)> {
        PumpSdk::pump_quote_initial_virtual_quote_reserves(global, quote_control, curve, pool)
    }

    #[test]
    fn pump_quote_reproduces_the_program_acceptance_examples() {
        let g = global(1);
        let p = pool(0);
        // AE1: SOL-quoted Q, migrated, pool at graduation state.
        assert_eq!(
            derive(
                &g,
                None,
                &migrated(Pubkey::default()),
                Some((&p, 206_900_000_000_000, 84_990_359_055))
            ),
            Ok((36_512_684_371_780, 1))
        );
        // AE4: USDC-quoted Q, migrated.
        assert_eq!(
            derive(
                &g,
                None,
                &migrated(usdc()),
                Some((&p, 206_900_000_000_000, 12_161_433_369))
            ),
            Ok((36_509_462_867_229, 1))
        );
        // AE15: AE1's pool pushed 100x.
        assert_eq!(
            derive(
                &g,
                None,
                &migrated(Pubkey::default()),
                Some((&p, 20_690_000_000_000, 849_903_590_550))
            ),
            Ok((663_914_919_474, 1))
        );
        // AE1 with the vault holding fees a negative virtual offsets.
        let accrued = pool(-15_009_640_945);
        assert_eq!(
            derive(
                &g,
                None,
                &migrated(Pubkey::default()),
                Some((&accrued, 206_900_000_000_000, 100_000_000_000))
            ),
            Ok((36_512_684_371_780, 1))
        );
        // Only a negative sum is refused.
        assert_eq!(
            derive(
                &g,
                None,
                &migrated(Pubkey::default()),
                Some((&accrued, 206_900_000_000_000, 15_009_640_944))
            ),
            Err(QuoteError::MathOverflow)
        );
        assert_eq!(
            derive(&g, None, &migrated(Pubkey::default()), None),
            Err(QuoteError::QuotePoolAccountsRequired)
        );
    }

    #[test]
    fn pump_quote_has_no_supply_bound() {
        let g = global(2);
        // A fresh SOL-quoted Q: Target's raise buys Q's whole tradable reserve (to rounding),
        // which the old spot-ratio bound refused.
        let fresh = on_curve(Pubkey::default(), VT0, SOL_SEED);
        assert_eq!(derive(&g, None, &fresh, None), Ok((279_899_999_999_307, 1)));
        // AE3 / AE6: priced at spot these asked for more than Q's supply; now admitted.
        let p = pool(0);
        assert_eq!(
            derive(
                &g,
                None,
                &migrated(Pubkey::default()),
                Some((&p, 654_275_247_900_000, 26_880_000_000))
            ),
            Ok((175_431_867_068_413, 1))
        );
        assert_eq!(
            derive(
                &g,
                None,
                &on_curve(Pubkey::default(), 676_450_000_000_000, 47_586_730_000),
                None
            ),
            Ok((153_052_117_545_539, 1))
        );
        // The only bound left: a seed of at least 1.
        let seeded = |seed: u64| {
            let mut q = on_curve(key(8), 1_000, 1_000);
            q.depth = 1;
            q.initial_virtual_quote_reserves = seed;
            q
        };
        assert_eq!(
            derive(&g, None, &seeded(0), None),
            Err(QuoteError::QuoteReservesOutOfRange)
        );
        // An emptied quote side prices nothing.
        assert_eq!(
            derive(&g, None, &on_curve(Pubkey::default(), VT0, 0), None),
            Err(QuoteError::MathOverflow)
        );
    }

    #[test]
    fn pump_quote_eligibility_and_target() {
        // Reserves deep enough that Target's raise barely moves the price: derived ~= Target.
        let at_par = |quote_mint| on_curve(quote_mint, 10u64.pow(18), 10u64.pow(18));
        // Depth gate first: 0 disables the path, and a depth-1 Q needs 2.
        assert_eq!(
            derive(&global(0), None, &at_par(Pubkey::default()), None),
            Err(QuoteError::CurveDepthExceeded)
        );
        let mut deep = at_par(key(8));
        deep.depth = 1;
        deep.initial_virtual_quote_reserves = 5_000;
        assert_eq!(
            derive(&global(1), None, &deep, None),
            Err(QuoteError::CurveDepthExceeded)
        );
        // Depth 1: Q's own stored seed, whatever its quote.
        assert_eq!(derive(&global(2), None, &deep, None), Ok((4_999, 2)));

        let g = global(1);
        let mut mayhem = at_par(Pubkey::default());
        mayhem.is_mayhem_mode = true;
        assert_eq!(
            derive(&g, None, &mayhem, None),
            Err(QuoteError::QuoteCurveNotEligible)
        );
        // Depth 0: SOL, then `Global`, then quote-control, else not eligible.
        assert_eq!(
            derive(&g, None, &at_par(Pubkey::default()), None),
            Ok((29_999_997_449, 1))
        );
        assert_eq!(
            derive(&g, None, &at_par(usdc()), None),
            Ok((4_291_999_947, 1))
        );
        // A stored wSOL is not the SOL arm on-chain: it is a `Global` quote.
        assert_eq!(
            derive(&g, None, &at_par(constants::NATIVE_MINT), None),
            Ok((4_291_999_947, 1))
        );
        let listed = QuoteControl {
            admin: key(1),
            reserves_admin: key(2),
            _reserved: [0; 32],
            mints: vec![QuoteControlMint {
                mint: key(8),
                initial_virtual_quote_reserves: 1_234_000_000,
            }],
        };
        assert_eq!(
            derive(&g, Some(&listed), &at_par(key(8)), None),
            Ok((1_233_999_995, 1))
        );
        assert_eq!(
            derive(&g, Some(&listed), &at_par(key(6)), None),
            Err(QuoteError::QuoteCurveNotEligible)
        );
        assert_eq!(
            derive(&g, None, &at_par(key(8)), None),
            Err(QuoteError::QuoteCurveNotEligible)
        );

        // Graduated but not migrated: no price until `migrate_v2`.
        let mut awaiting = at_par(Pubkey::default());
        awaiting.complete = true;
        assert_eq!(
            derive(&g, None, &awaiting, Some((&pool(0), 1, 1))),
            Err(QuoteError::QuoteCurveAwaitingMigration)
        );
    }

    #[test]
    fn create_v2_pump_quote_accounts_follow_migration() {
        let q = key(8);
        let on_curve = PumpSdk::create_v2_pump_quote_accounts(&q, None);
        assert_eq!(
            on_curve,
            [AccountMeta::new_readonly(
                pda::pump::bonding_curve(&q).0,
                false
            )]
        );
        let state = pool(0);
        let pool_key = pda::pump_amm::canonical_pool(&q, &usdc()).0;
        let migrated = PumpSdk::create_v2_pump_quote_accounts(&q, Some((pool_key, &state)));
        let keys: Vec<Pubkey> = migrated.iter().map(|m| m.pubkey).collect();
        assert_eq!(
            keys,
            [pda::pump::bonding_curve(&q).0, pool_key, key(3), key(4)]
        );
        assert!(migrated.iter().all(|m| !m.is_writable && !m.is_signer));
    }

    #[test]
    fn initial_bonding_curve_stores_its_seed() {
        let curve =
            PumpSdk::initial_bonding_curve(&global(1), key(1), usdc(), 777, false, false, 0);
        assert_eq!(curve.initial_virtual_quote_reserves, 777);
    }
}
