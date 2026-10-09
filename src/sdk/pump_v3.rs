//! pump v3 trades and the creator fee sweep.
//!
//! `buy_v3` / `buy_exact_quote_in_v3` / `sell_v3` take one shared 17-account
//! set: no fee recipients, no creator vault, no volume-accumulator ATAs. The
//! protocol fee (less its buyback slice) and the creator fee stay on the
//! curve (`BondingCurve.protocol_fees` / `creator_fee`) until a
//! permissionless sweep pays them out; the buyback slice is paid in the trade
//! to `buyback_fee_recipient` (index 13). The fee schedule is v2's, and so is
//! the price up to the curve's remaining supply. Past it a non-mayhem buy
//! completes the curve and buys the rest from the pool the migration will
//! create (the post-completion leg), so quote v3 buys with
//! [`PumpSdk::buy_quote_bonding_curve_v3_token_out`] /
//! [`PumpSdk::buy_quote_bonding_curve_v3_sol_in`]. Cashback coins are refused
//! by the program; keep them on v2.
//!
//! Every builder here works from keys alone (no fetched state): the curve's
//! `quote_mint` (`Pubkey::default()` or wSOL for a SOL curve), the base token
//! program (Token-2022 for `create_v2` coins, SPL Token for legacy `create`
//! coins), the quote token program and a listed `Global.buyback_fee_recipients`
//! wallet (see [`PumpSdk::buyback_fee_recipient_from_pump_global`]).
//! [`crate::AsyncPumpClient`] derives all of them from chain state in
//! `build_buy_v3` / `build_sell_v3`.

use anchor_lang::solana_program::{instruction::Instruction, pubkey::Pubkey};
use anchor_lang::system_program;
use anchor_lang::{InstructionData, ToAccountMetas};

use crate::token::create_associated_token_account_idempotent;
use crate::{constants, pda, pump::client, pump::types::OptionBool};

use super::PumpSdk;

/// Keys of one v3 trade, shared by the three instructions.
#[derive(Clone, Copy)]
struct V3Trade {
    base_mint: Pubkey,
    /// Resolved quote (`Pubkey::default()` → wSOL).
    quote_mint: Pubkey,
    base_token_program: Pubkey,
    quote_token_program: Pubkey,
    user: Pubkey,
    /// The listed recipient wallet (not its ATA).
    buyback_fee_recipient: Pubkey,
}

impl V3Trade {
    fn new(
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        buyback_fee_recipient: Pubkey,
    ) -> Self {
        Self {
            base_mint,
            quote_mint: PumpSdk::resolve_quote_mint(quote_mint),
            base_token_program,
            quote_token_program,
            user,
            buyback_fee_recipient,
        }
    }

    fn is_token_quoted(&self) -> bool {
        self.quote_mint != constants::NATIVE_MINT
    }

    /// The shared account set as the generated `BuyV3` struct; the other two
    /// instructions have the same fields in the same order.
    fn accounts(&self) -> client::accounts::BuyV3 {
        let bonding_curve = pda::pump::bonding_curve(&self.base_mint).0;
        let ata = |owner: &Pubkey, token_program: &Pubkey, mint: &Pubkey| {
            pda::associated_token(owner, token_program, mint).0
        };
        client::accounts::BuyV3 {
            global: pda::pump::global().0,
            base_mint: self.base_mint,
            quote_mint: self.quote_mint,
            base_token_program: self.base_token_program,
            quote_token_program: self.quote_token_program,
            bonding_curve,
            associated_base_bonding_curve: ata(
                &bonding_curve,
                &self.base_token_program,
                &self.base_mint,
            ),
            associated_quote_bonding_curve: ata(
                &bonding_curve,
                &self.quote_token_program,
                &self.quote_mint,
            ),
            user: self.user,
            associated_base_user: ata(&self.user, &self.base_token_program, &self.base_mint),
            associated_quote_user: ata(&self.user, &self.quote_token_program, &self.quote_mint),
            user_volume_accumulator: pda::pump::user_volume_accumulator(&self.user).0,
            fee_config: pda::pump::fee_config().0,
            // The wallet itself on a SOL curve, its canonical quote ATA on a
            // token quote. The program never creates that ATA: it must exist.
            buyback_fee_recipient: if self.is_token_quoted() {
                ata(
                    &self.buyback_fee_recipient,
                    &self.quote_token_program,
                    &self.quote_mint,
                )
            } else {
                self.buyback_fee_recipient
            },
            system_program: system_program::ID,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
        }
    }

    fn instruction(&self, data: Vec<u8>) -> Instruction {
        Instruction {
            program_id: crate::pump::ID,
            accounts: self.accounts().to_account_metas(None),
            data,
        }
    }

    /// Idempotent creates for the accounts the program reads as-is: the
    /// user's base ATA on buys and, when the quote is a token (a SOL curve
    /// moves lamports), the user's and the buyback recipient's quote ATAs.
    /// The curve's own ATAs exist since `create_v2`.
    fn with_user_atas(&self, include_base: bool, trade: Instruction) -> Vec<Instruction> {
        let mut ixs = Vec::with_capacity(4);
        if include_base {
            ixs.push(create_associated_token_account_idempotent(
                &self.user,
                &self.user,
                &self.base_mint,
                &self.base_token_program,
            ));
        }
        if self.is_token_quoted() {
            ixs.push(create_associated_token_account_idempotent(
                &self.user,
                &self.user,
                &self.quote_mint,
                &self.quote_token_program,
            ));
            ixs.push(create_associated_token_account_idempotent(
                &self.user,
                &self.buyback_fee_recipient,
                &self.quote_mint,
                &self.quote_token_program,
            ));
        }
        ixs.push(trade);
        ixs
    }
}

// Buys request a partial fill. It only matters on a mayhem curve (or for the
// whitelisted agent): past the remaining supply such a buy fills the rest and
// completes the curve instead of failing (the exact-in form then skips
// `min_tokens_out`). Any other buy continues into the post-completion leg
// regardless, and `max_quote_cost` / `min_tokens_out` cover both legs.
fn buy_v3_data(amount: u64, max_quote_cost: u64) -> Vec<u8> {
    client::args::BuyV3 {
        amount,
        max_sol_cost: max_quote_cost,
        partial_fill: OptionBool(true),
    }
    .data()
}

fn buy_exact_quote_in_v3_data(spendable_quote_in: u64, min_tokens_out: u64) -> Vec<u8> {
    client::args::BuyExactQuoteInV3 {
        spendable_quote_in,
        min_tokens_out,
        partial_fill: OptionBool(true),
    }
    .data()
}

fn sell_v3_data(amount: u64, min_quote_out: u64) -> Vec<u8> {
    client::args::SellV3 {
        amount,
        min_sol_output: min_quote_out,
    }
    .data()
}

impl PumpSdk {
    /// `buy_v3`: `amount` tokens out for at most `max_quote_cost` of the quote
    /// (fees included, post-completion leg included). `buyback_fee_recipient`
    /// is a listed `Global.buyback_fee_recipients` wallet; on a token quote its
    /// ATA for the quote mint must already exist ([`Self::buy_v3_instructions`]
    /// creates it).
    pub fn buy_v3_instruction(
        &self,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        buyback_fee_recipient: Pubkey,
        amount: u64,
        max_quote_cost: u64,
    ) -> Instruction {
        V3Trade::new(
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            buyback_fee_recipient,
        )
        .instruction(buy_v3_data(amount, max_quote_cost))
    }

    /// `buy_exact_quote_in_v3`: spend `spendable_quote_in` for at least
    /// `min_tokens_out` tokens; past the remaining supply the rest of the
    /// budget goes to the post-completion leg (a mayhem curve fills only the
    /// remainder). Accounts as [`Self::buy_v3_instruction`].
    pub fn buy_exact_quote_in_v3_instruction(
        &self,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        buyback_fee_recipient: Pubkey,
        spendable_quote_in: u64,
        min_tokens_out: u64,
    ) -> Instruction {
        V3Trade::new(
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            buyback_fee_recipient,
        )
        .instruction(buy_exact_quote_in_v3_data(
            spendable_quote_in,
            min_tokens_out,
        ))
    }

    /// `sell_v3`: `amount` tokens in for at least `min_quote_out` (after
    /// fees). Accounts as [`Self::buy_v3_instruction`].
    pub fn sell_v3_instruction(
        &self,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        buyback_fee_recipient: Pubkey,
        amount: u64,
        min_quote_out: u64,
    ) -> Instruction {
        V3Trade::new(
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            buyback_fee_recipient,
        )
        .instruction(sell_v3_data(amount, min_quote_out))
    }

    /// [`Self::buy_v3_instruction`] behind idempotent creates of the user's
    /// base ATA and, for a token quote, the user's and the buyback recipient's
    /// quote ATAs.
    pub fn buy_v3_instructions(
        &self,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        buyback_fee_recipient: Pubkey,
        amount: u64,
        max_quote_cost: u64,
    ) -> Vec<Instruction> {
        let trade = V3Trade::new(
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            buyback_fee_recipient,
        );
        trade.with_user_atas(true, trade.instruction(buy_v3_data(amount, max_quote_cost)))
    }

    /// [`Self::buy_exact_quote_in_v3_instruction`] with the same ATA prelude as
    /// [`Self::buy_v3_instructions`].
    pub fn buy_exact_quote_in_v3_instructions(
        &self,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        buyback_fee_recipient: Pubkey,
        spendable_quote_in: u64,
        min_tokens_out: u64,
    ) -> Vec<Instruction> {
        let trade = V3Trade::new(
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            buyback_fee_recipient,
        );
        trade.with_user_atas(
            true,
            trade.instruction(buy_exact_quote_in_v3_data(
                spendable_quote_in,
                min_tokens_out,
            )),
        )
    }

    /// [`Self::sell_v3_instruction`] behind idempotent creates of the user's
    /// and the buyback recipient's quote ATAs when the quote is a token.
    pub fn sell_v3_instructions(
        &self,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        user: Pubkey,
        buyback_fee_recipient: Pubkey,
        amount: u64,
        min_quote_out: u64,
    ) -> Vec<Instruction> {
        let trade = V3Trade::new(
            base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            user,
            buyback_fee_recipient,
        );
        trade.with_user_atas(
            false,
            trade.instruction(sell_v3_data(amount, min_quote_out)),
        )
    }

    /// `sweep_creator_fee` (permissionless): pays the curve's
    /// `creator_fee` bucket into the pump creator vault
    /// (`creator-vault` PDA of the curve's `creator`; lamports on a SOL curve,
    /// the vault's quote ATA otherwise). `payer` funds the curve realloc and
    /// any missing ATA; an empty bucket is a no-op. The program refuses
    /// `distribute_creator_fees*`, `admin_cto` and fee-sharing creation while
    /// the bucket holds fees (`CreatorFeesNotSwept`), so prepend this in the
    /// same transaction on any coin with a v3 trade
    /// ([`Self::distribute_creator_fees_v2_instructions`] does).
    pub fn sweep_creator_fee_instruction(
        &self,
        payer: Pubkey,
        base_mint: Pubkey,
        creator: Pubkey,
        quote_mint: Pubkey,
        quote_token_program: Pubkey,
    ) -> Instruction {
        let quote_mint = Self::resolve_quote_mint(quote_mint);
        let bonding_curve = pda::pump::bonding_curve(&base_mint).0;
        let recipient = pda::pump::creator_vault(&creator).0;
        let accounts = client::accounts::SweepCreatorFee {
            payer,
            global: pda::pump::global().0,
            base_mint,
            quote_mint,
            quote_token_program,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            system_program: system_program::ID,
            bonding_curve,
            associated_quote_bonding_curve: pda::associated_token(
                &bonding_curve,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            recipient,
            associated_quote_recipient: pda::associated_token(
                &recipient,
                &quote_token_program,
                &quote_mint,
            )
            .0,
            event_authority: pda::pump::event_authority().0,
            program: crate::pump::ID,
        };
        Instruction {
            program_id: crate::pump::ID,
            accounts: accounts.to_account_metas(None),
            data: client::args::SweepCreatorFee.data(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::Discriminator;

    const T22: Pubkey = constants::SPL_TOKEN_2022_PROGRAM_ID;
    const TP: Pubkey = constants::SPL_TOKEN_PROGRAM_ID;

    fn key(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    // Byte-for-byte account order of the shared `TradeV3` struct.
    #[test]
    fn v3_trades_share_the_seventeen_account_layout() {
        let sdk = PumpSdk::new();
        let (mint, quote, user, buyback) = (key(1), key(2), key(3), key(4));
        let buy = sdk.buy_v3_instruction(mint, quote, T22, TP, user, buyback, 10, 20);
        let exact =
            sdk.buy_exact_quote_in_v3_instruction(mint, quote, T22, TP, user, buyback, 10, 20);
        let sell = sdk.sell_v3_instruction(mint, quote, T22, TP, user, buyback, 10, 20);
        assert_eq!(buy.accounts, exact.accounts);
        assert_eq!(buy.accounts, sell.accounts);

        let curve = pda::pump::bonding_curve(&mint).0;
        let expected = [
            pda::pump::global().0,
            mint,
            quote,
            T22,
            TP,
            curve,
            pda::associated_token(&curve, &T22, &mint).0,
            pda::associated_token(&curve, &TP, &quote).0,
            user,
            pda::associated_token(&user, &T22, &mint).0,
            pda::associated_token(&user, &TP, &quote).0,
            pda::pump::user_volume_accumulator(&user).0,
            pda::pump::fee_config().0,
            // A token quote pays the buyback slice into the recipient's quote ATA.
            pda::associated_token(&buyback, &TP, &quote).0,
            system_program::ID,
            pda::pump::event_authority().0,
            crate::pump::ID,
        ];
        let keys: Vec<Pubkey> = buy.accounts.iter().map(|m| m.pubkey).collect();
        assert_eq!(keys, expected);
        let writable: Vec<bool> = buy.accounts.iter().map(|m| m.is_writable).collect();
        assert_eq!(
            writable,
            [
                false, false, false, false, false, true, true, true, true, true, true, true, false,
                true, false, false, false
            ]
        );
        assert!(buy.accounts[8].is_signer);
        assert_eq!(buy.accounts.iter().filter(|m| m.is_signer).count(), 1);

        assert_eq!(&buy.data[..8], client::args::BuyV3::DISCRIMINATOR);
        assert_eq!(
            &exact.data[..8],
            client::args::BuyExactQuoteInV3::DISCRIMINATOR
        );
        assert_eq!(&sell.data[..8], client::args::SellV3::DISCRIMINATOR);
        // Both buys end in `partial_fill = Some(true)`; the sell takes no flag.
        let args = [10u64.to_le_bytes(), 20u64.to_le_bytes()].concat();
        assert_eq!(&buy.data[8..], [args.as_slice(), &[1]].concat());
        assert_eq!(&exact.data[8..], &buy.data[8..]);
        assert_eq!(&sell.data[8..], args);
    }

    // A legacy `create` coin has an SPL Token base; the ATAs must follow. A
    // SOL curve pays the buyback slice to the recipient wallet itself.
    #[test]
    fn sol_curve_uses_the_wallet_and_base_program_drives_the_base_atas() {
        let (mint, user, buyback) = (key(1), key(3), key(4));
        let ix =
            PumpSdk::new().buy_v3_instruction(mint, Pubkey::default(), TP, TP, user, buyback, 1, 1);
        let curve = pda::pump::bonding_curve(&mint).0;
        assert_eq!(ix.accounts[2].pubkey, constants::NATIVE_MINT);
        assert_eq!(ix.accounts[3].pubkey, TP);
        assert_eq!(
            ix.accounts[6].pubkey,
            pda::associated_token(&curve, &TP, &mint).0
        );
        assert_eq!(
            ix.accounts[9].pubkey,
            pda::associated_token(&user, &TP, &mint).0
        );
        assert_eq!(ix.accounts[13].pubkey, buyback);
        assert!(ix.accounts[13].is_writable);
    }

    #[test]
    fn ata_prelude_depends_on_the_quote() {
        let sdk = PumpSdk::new();
        let (mint, user, buyback, usdc) = (key(1), key(3), key(4), key(9));
        let sol = Pubkey::default();
        assert_eq!(
            sdk.buy_v3_instructions(mint, sol, T22, TP, user, buyback, 1, 1)
                .len(),
            2
        );
        assert_eq!(
            sdk.sell_v3_instructions(mint, sol, T22, TP, user, buyback, 1, 1)
                .len(),
            1
        );
        let usdc_buy = sdk.buy_v3_instructions(mint, usdc, T22, TP, user, buyback, 1, 1);
        assert_eq!(usdc_buy.len(), 4);
        // A token quote also creates the buyback recipient's quote ATA (user pays).
        let user_quote_ata = pda::associated_token(&user, &TP, &usdc).0;
        let buyback_ata = pda::associated_token(&buyback, &TP, &usdc).0;
        assert_eq!(usdc_buy[1].accounts[1].pubkey, user_quote_ata);
        assert_eq!(usdc_buy[2].accounts[0].pubkey, user);
        assert_eq!(usdc_buy[2].accounts[1].pubkey, buyback_ata);
        assert_eq!(usdc_buy[2].accounts[2].pubkey, buyback);
        assert_eq!(usdc_buy[3].accounts[13].pubkey, buyback_ata);
        let usdc_exact =
            sdk.buy_exact_quote_in_v3_instructions(mint, usdc, T22, TP, user, buyback, 1, 1);
        assert_eq!(usdc_exact.len(), 4);
        assert_eq!(usdc_exact[..3], usdc_buy[..3]);
        let usdc_sell = sdk.sell_v3_instructions(mint, usdc, T22, TP, user, buyback, 1, 1);
        assert_eq!(usdc_sell.len(), 3);
        assert_eq!(usdc_sell[1].accounts[1].pubkey, buyback_ata);
    }

    #[test]
    fn sweep_creator_fee_pays_the_creator_vault() {
        let (creator, quote) = (key(7), key(9));
        let ix = PumpSdk::new().sweep_creator_fee_instruction(key(1), key(2), creator, quote, TP);
        assert_eq!(ix.accounts.len(), 13);
        assert_eq!(&ix.data, client::args::SweepCreatorFee::DISCRIMINATOR);
        let vault = pda::pump::creator_vault(&creator).0;
        assert_eq!(ix.accounts[9].pubkey, vault);
        assert_eq!(
            ix.accounts[10].pubkey,
            pda::associated_token(&vault, &TP, &quote).0
        );
        assert!(ix.accounts[0].is_signer && ix.accounts[0].is_writable);
        assert!(ix.accounts[7].is_writable && ix.accounts[9].is_writable);
    }
}
