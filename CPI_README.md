# CPI into pump from your own Anchor program

When your program needs to CPI into pump's `buy_v2` or `sell_v2`, you can
reuse `pump-rust-client` to derive every account meta. The pattern is:

1. Build pump's instruction list with `sdk.buy_v2_instructions(...)`.
2. **Re-target** the trailing trade ix at your wrapper: swap `program_id`
   to your program's id and replace `data` with your wrapper's instruction
   data.
3. Send. The wrapper's account-order matches pump's, so the SDK's metas
   pass through unchanged — including the trailing `program` meta, which
   doubles as the wrapper's CPI target.

The worked example below comes from `devnet-v2-test`, an Anchor program
whose only job is to CPI `buy_v2` / `sell_v2` straight through.

## 1. The wrapper program

The wrapper declares pump via `declare_program!(pump)` (which reads
`idls/pump.json`) and exposes a `buy_v2` instruction whose `Accounts`
struct **mirrors pump's `BuyV2` account order exactly**. From
`devnet-v2-test/programs/devnet-v2-test/src/lib.rs`:

```rust
use anchor_lang::prelude::*;

declare_id!("7enkT8uLQrwKYSRMrazjr1YZPT7unToAtwdLMWa2QLYY");
declare_program!(pump);

#[program]
pub mod test_program {
    use super::*;

    pub fn buy_v2(ctx: Context<BuyV2>, amount: u64, max_sol_cost: u64) -> Result<()> {
        pump::cpi::buy_v2(
            CpiContext::new(
                ctx.accounts.program.to_account_info(),
                pump::cpi::accounts::BuyV2 {
                    global: ctx.accounts.global.to_account_info(),
                    base_mint: ctx.accounts.base_mint.to_account_info(),
                    quote_mint: ctx.accounts.quote_mint.to_account_info(),
                    base_token_program: ctx.accounts.base_token_program.to_account_info(),
                    quote_token_program: ctx.accounts.quote_token_program.to_account_info(),
                    associated_token_program: ctx.accounts.associated_token_program.to_account_info(),
                    fee_recipient: ctx.accounts.fee_recipient.to_account_info(),
                    associated_quote_fee_recipient: ctx.accounts.associated_quote_fee_recipient.to_account_info(),
                    buyback_fee_recipient: ctx.accounts.buyback_fee_recipient.to_account_info(),
                    associated_quote_buyback_fee_recipient: ctx.accounts.associated_quote_buyback_fee_recipient.to_account_info(),
                    bonding_curve: ctx.accounts.bonding_curve.to_account_info(),
                    associated_base_bonding_curve: ctx.accounts.associated_base_bonding_curve.to_account_info(),
                    associated_quote_bonding_curve: ctx.accounts.associated_quote_bonding_curve.to_account_info(),
                    user: ctx.accounts.user.to_account_info(),
                    associated_base_user: ctx.accounts.associated_base_user.to_account_info(),
                    associated_quote_user: ctx.accounts.associated_quote_user.to_account_info(),
                    creator_vault: ctx.accounts.creator_vault.to_account_info(),
                    associated_creator_vault: ctx.accounts.associated_creator_vault.to_account_info(),
                    sharing_config: ctx.accounts.sharing_config.to_account_info(),
                    global_volume_accumulator: ctx.accounts.global_volume_accumulator.to_account_info(),
                    user_volume_accumulator: ctx.accounts.user_volume_accumulator.to_account_info(),
                    associated_user_volume_accumulator: ctx.accounts.associated_user_volume_accumulator.to_account_info(),
                    fee_config: ctx.accounts.fee_config.to_account_info(),
                    fee_program: ctx.accounts.fee_program.to_account_info(),
                    system_program: ctx.accounts.system_program.to_account_info(),
                    event_authority: ctx.accounts.event_authority.to_account_info(),
                    program: ctx.accounts.program.to_account_info(),
                },
            ),
            amount,
            max_sol_cost,
        )?;
        Ok(())
    }
}

#[derive(Accounts)]
pub struct BuyV2<'info> {
    /// CHECK: passed straight to pump
    pub global: UncheckedAccount<'info>,
    /// CHECK: passed straight to pump
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: passed straight to pump
    pub quote_mint: UncheckedAccount<'info>,
    // ... fields in the same order as pump::cpi::accounts::BuyV2 ...
    #[account(mut)]
    pub user: Signer<'info>,
    // ...
    /// CHECK: pump program; CPI target
    pub program: UncheckedAccount<'info>,
}
```

The full file is at
`devnet-v2-test/programs/devnet-v2-test/src/lib.rs`.

## 2. Client-side: re-target the SDK's instruction

From `devnet-v2-test/programs/devnet-v2-test/tests/buy_sell_v2_cpi.rs`:

```rust
use anchor_lang::InstructionData;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;

use pump_rust_client::{constants, PumpSdk};

/// Re-target the trailing pump ix produced by `pump-rust-client` so it hits
/// the wrapper. The account-meta order is identical between pump's `BuyV2`
/// and the wrapper, and the trailing `program` meta already points at pump
/// (it doubles as the CPI target), so we only swap program id and data.
fn retarget_to_wrapper(ix: &mut Instruction, new_program_id: Pubkey, new_data: Vec<u8>) {
    ix.program_id = new_program_id;
    ix.data = new_data;
}

let sdk = PumpSdk::new();
let mut pump_buy_ixs = sdk
    .buy_v2_instructions(
        &global,
        &bonding_curve,
        mint,
        constants::SPL_TOKEN_PROGRAM_ID,
        user.pubkey(),
        buy_amount,
        max_sol_cost,
    )
    .expect("buy_v2_instructions");

let buy_args = devnet_v2_test::instruction::BuyV2 {
    amount: buy_amount,
    max_sol_cost,
};
let last = pump_buy_ixs.last_mut().expect("trailing buy_v2 ix");
retarget_to_wrapper(last, devnet_v2_test::ID, buy_args.data());

// `pump_buy_ixs` now starts with idempotent ATA creates and ends with the
// re-targeted wrapper call. Send as-is.
```

`sell_v2` follows the identical pattern with
`sdk.sell_v2_instructions(...)` and `devnet_v2_test::instruction::SellV2`.

## 3. Why this works

- The SDK builds the trade ix with **pump's account layout** (see
  `V2TradeAccounts::derive` in `src/sdk/pump_v2.rs`).
- The wrapper's `Accounts` struct lists the same accounts in the same
  order, so Anchor accepts the meta list verbatim.
- The trailing meta is the pump program id — the wrapper picks it up via
  `ctx.accounts.program` and uses it as the CPI target. No meta surgery is
  needed beyond the program-id and data swap.

## Caveats

- **Account order matters.** If your wrapper reorders, renames, or adds
  accounts, the drop-in re-target breaks. In that case build the meta list
  manually using `pda::pump::*` helpers (see `src/pda.rs`) and the public
  `PumpSdk` PDAs.
- **IDL.** Your wrapper crate must ship the pump IDL alongside it (e.g.
  `devnet-v2-test/programs/devnet-v2-test/idls/pump.json`) so
  `declare_program!(pump)` can resolve `pump::cpi::accounts::BuyV2`.
- **Testing.** Build and deploy the wrapper to devnet (or any cluster
  where the pump program is live), then point your client RPC at that
  cluster and exercise it the same way as the SDK examples.
