//! pump-fees `update_fee_shares` / `update_fee_shares_v2`.
//!
//! Both distribute the pending creator fees to the current shareholders
//! (pulling the coin's AMM creator fees first once it graduated), then
//! replace the shares and revoke the admin. On a graduated coin the pool's
//! `creator_fees` bucket must be swept first
//! ([`PumpSdk::sweep_pool_creator_fee_instruction`]; else `PoolCreatorFeesNotSwept`).

use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use anchor_lang::system_program;
use anchor_lang::{InstructionData, ToAccountMetas};

use crate::pump_fees::{client, types::Shareholder};
use crate::state::BondingCurve;
use crate::{constants, pda};

use super::PumpSdk;

/// Keys shared by both instructions.
struct FeeSharesKeys {
    sharing_config: Pubkey,
    pump_creator_vault: Pubkey,
    coin_creator_vault_authority: Pubkey,
}

impl FeeSharesKeys {
    fn new(mint: &Pubkey) -> Self {
        let sharing_config = pda::pump::sharing_config(mint).0;
        Self {
            sharing_config,
            pump_creator_vault: pda::pump::creator_vault(&sharing_config).0,
            coin_creator_vault_authority: pda::pump_amm::coin_creator_vault_authority(
                &sharing_config,
            )
            .0,
        }
    }
}

/// Appends the canonical pool as the last remaining account when the curve is
/// complete (the program reads it only then).
fn push_pool_if_complete(
    metas: &mut Vec<AccountMeta>,
    mint: &Pubkey,
    bonding_curve: &BondingCurve,
) {
    if bonding_curve.complete {
        let quote_mint = PumpSdk::resolve_quote_mint(bonding_curve.quote_mint);
        metas.push(AccountMeta::new_readonly(
            pda::pump_amm::canonical_pool(mint, &quote_mint).0,
            false,
        ));
    }
}

impl PumpSdk {
    /// pump-fees `update_fee_shares` (SOL-quoted coins). `current_shareholders`
    /// are the sharing config's existing shareholder wallets, in order (paid
    /// before the rewrite); `shareholders` is the new set (bps summing to 10_000).
    pub fn update_fee_shares_instruction(
        &self,
        authority: Pubkey,
        mint: Pubkey,
        bonding_curve: &BondingCurve,
        current_shareholders: &[Pubkey],
        shareholders: Vec<Shareholder>,
    ) -> Instruction {
        let keys = FeeSharesKeys::new(&mint);
        let accounts = client::accounts::UpdateFeeShares {
            event_authority: pda::pump_fees::event_authority().0,
            program: crate::pump_fees::ID,
            authority,
            global: pda::pump::global().0,
            mint,
            sharing_config: keys.sharing_config,
            bonding_curve: pda::pump::bonding_curve(&mint).0,
            pump_creator_vault: keys.pump_creator_vault,
            system_program: system_program::ID,
            pump_program: crate::pump::ID,
            pump_event_authority: pda::pump::event_authority().0,
            pump_amm_program: crate::pump_amm::ID,
            amm_event_authority: pda::pump_amm::event_authority().0,
            wsol_mint: constants::NATIVE_MINT,
            token_program: constants::SPL_TOKEN_PROGRAM_ID,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            coin_creator_vault_authority: keys.coin_creator_vault_authority,
            coin_creator_vault_ata: pda::associated_token(
                &keys.coin_creator_vault_authority,
                &constants::SPL_TOKEN_PROGRAM_ID,
                &constants::NATIVE_MINT,
            )
            .0,
        };
        let mut metas = accounts.to_account_metas(None);
        metas.extend(
            current_shareholders
                .iter()
                .map(|sh| AccountMeta::new(*sh, false)),
        );
        push_pool_if_complete(&mut metas, &mint, bonding_curve);
        Instruction {
            program_id: crate::pump_fees::ID,
            accounts: metas,
            data: client::args::UpdateFeeShares { shareholders }.data(),
        }
    }

    /// pump-fees `update_fee_shares_v2` (any quote; the quote mint is the
    /// curve's). Remaining accounts are the current shareholder wallets, then,
    /// on a token quote, their quote ATAs. Args as
    /// [`Self::update_fee_shares_instruction`].
    pub fn update_fee_shares_v2_instruction(
        &self,
        authority: Pubkey,
        mint: Pubkey,
        bonding_curve: &BondingCurve,
        quote_token_program: Pubkey,
        current_shareholders: &[Pubkey],
        shareholders: Vec<Shareholder>,
    ) -> Instruction {
        let keys = FeeSharesKeys::new(&mint);
        let quote_mint = Self::resolve_quote_mint(bonding_curve.quote_mint);
        let ata =
            |owner: &Pubkey| pda::associated_token(owner, &quote_token_program, &quote_mint).0;
        let accounts = client::accounts::UpdateFeeSharesV2 {
            event_authority: pda::pump_fees::event_authority().0,
            program: crate::pump_fees::ID,
            authority,
            global: pda::pump::global().0,
            mint,
            sharing_config: keys.sharing_config,
            bonding_curve: pda::pump::bonding_curve(&mint).0,
            pump_creator_vault: keys.pump_creator_vault,
            pump_creator_vault_ata: ata(&keys.pump_creator_vault),
            system_program: system_program::ID,
            pump_program: crate::pump::ID,
            pump_event_authority: pda::pump::event_authority().0,
            pump_amm_program: crate::pump_amm::ID,
            amm_event_authority: pda::pump_amm::event_authority().0,
            quote_mint,
            token_program: quote_token_program,
            associated_token_program: constants::SPL_ATA_PROGRAM_ID,
            coin_creator_vault_authority: keys.coin_creator_vault_authority,
            coin_creator_vault_ata: ata(&keys.coin_creator_vault_authority),
        };
        let mut metas = accounts.to_account_metas(None);
        metas.extend(
            current_shareholders
                .iter()
                .map(|sh| AccountMeta::new(*sh, false)),
        );
        if quote_mint != constants::NATIVE_MINT {
            metas.extend(
                current_shareholders
                    .iter()
                    .map(|sh| AccountMeta::new(ata(sh), false)),
            );
        }
        push_pool_if_complete(&mut metas, &mint, bonding_curve);
        Instruction {
            program_id: crate::pump_fees::ID,
            accounts: metas,
            data: client::args::UpdateFeeSharesV2 { shareholders }.data(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::BondingCurveFromIdl;
    use anchor_lang::Discriminator;

    const TP: Pubkey = constants::SPL_TOKEN_PROGRAM_ID;

    fn key(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    fn curve(quote_mint: Pubkey, complete: bool) -> BondingCurve {
        BondingCurve::new(BondingCurveFromIdl {
            quote_mint,
            complete,
            ..Default::default()
        })
    }

    fn new_shares() -> Vec<Shareholder> {
        vec![Shareholder {
            address: key(5),
            share_bps: 10_000,
        }]
    }

    #[test]
    fn update_fee_shares_appends_the_pool_only_when_complete() {
        let sdk = PumpSdk::new();
        let (authority, mint, holders) = (key(1), key(2), [key(3), key(4)]);
        let open = sdk.update_fee_shares_instruction(
            authority,
            mint,
            &curve(Pubkey::default(), false),
            &holders,
            new_shares(),
        );
        assert_eq!(open.accounts.len(), 18 + 2);
        assert_eq!(
            &open.data[..8],
            client::args::UpdateFeeShares::DISCRIMINATOR
        );
        assert_eq!(open.accounts[0].pubkey, pda::pump_fees::event_authority().0);
        assert!(open.accounts[2].is_signer && !open.accounts[2].is_writable);
        assert_eq!(open.accounts[5].pubkey, pda::pump::sharing_config(&mint).0);
        assert_eq!(open.accounts[18].pubkey, holders[0]);
        assert!(open.accounts[18].is_writable && open.accounts[19].is_writable);

        let done = sdk.update_fee_shares_instruction(
            authority,
            mint,
            &curve(Pubkey::default(), true),
            &holders,
            new_shares(),
        );
        assert_eq!(done.accounts[..20], open.accounts[..]);
        let pool = done.accounts.last().unwrap();
        assert_eq!(
            pool.pubkey,
            pda::pump_amm::canonical_pool(&mint, &constants::NATIVE_MINT).0
        );
        assert!(!pool.is_writable && done.accounts.len() == 21);
    }

    #[test]
    fn update_fee_shares_v2_adds_holder_atas_on_a_token_quote() {
        let sdk = PumpSdk::new();
        let (authority, mint, usdc, holders) = (key(1), key(2), key(9), [key(3), key(4)]);
        let open = sdk.update_fee_shares_v2_instruction(
            authority,
            mint,
            &curve(usdc, false),
            TP,
            &holders,
            new_shares(),
        );
        assert_eq!(open.accounts.len(), 19 + 4);
        assert_eq!(
            &open.data[..8],
            client::args::UpdateFeeSharesV2::DISCRIMINATOR
        );
        assert!(open.accounts[2].is_signer && open.accounts[2].is_writable);
        assert_eq!(open.accounts[14].pubkey, usdc);
        assert_eq!(open.accounts[15].pubkey, TP);
        let vault = pda::pump::creator_vault(&pda::pump::sharing_config(&mint).0).0;
        assert_eq!(
            open.accounts[8].pubkey,
            pda::associated_token(&vault, &TP, &usdc).0
        );
        assert_eq!(open.accounts[19].pubkey, holders[0]);
        assert_eq!(
            open.accounts[21].pubkey,
            pda::associated_token(&holders[0], &TP, &usdc).0
        );

        let done = sdk.update_fee_shares_v2_instruction(
            authority,
            mint,
            &curve(usdc, true),
            TP,
            &holders,
            new_shares(),
        );
        assert_eq!(done.accounts.len(), 24);
        assert_eq!(
            done.accounts[23].pubkey,
            pda::pump_amm::canonical_pool(&mint, &usdc).0
        );

        // A SOL curve takes no holder ATAs and its pool is the wSOL one.
        let sol = sdk.update_fee_shares_v2_instruction(
            authority,
            mint,
            &curve(Pubkey::default(), true),
            TP,
            &holders,
            new_shares(),
        );
        assert_eq!(sol.accounts.len(), 19 + 2 + 1);
        assert_eq!(sol.accounts[14].pubkey, constants::NATIVE_MINT);
        assert_eq!(
            sol.accounts[21].pubkey,
            pda::pump_amm::canonical_pool(&mint, &constants::NATIVE_MINT).0
        );
    }
}
