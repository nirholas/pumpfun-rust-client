//! Account decoders for the pump and pump_amm programs.

use anchor_lang::{AccountDeserialize, AccountSerialize};

use crate::{
    errors::{PumpClientError, Result},
    state::{
        BondingCurve, FeeConfig, Global, GlobalVolumeAccumulator, SharingConfig,
        UserVolumeAccumulator,
    },
};

/// Anchor-deserialize an account buffer. Errors on bad discriminator or
/// truncation. Works for any `T` from `crate::pump::accounts::*` or
/// `crate::pump_amm::accounts::*`.
pub fn decode<T: AccountDeserialize>(data: &[u8]) -> Result<T> {
    T::try_deserialize(&mut &data[..]).map_err(PumpClientError::from)
}

/// Decode `T`, zero-padding short buffers to the full Anchor-serialized size
/// of `T` first (computed by serializing `T::default()`). Returns `None` on
/// any decode failure.
pub fn decode_padded<T>(data: &[u8]) -> Option<T>
where
    T: AccountDeserialize + AccountSerialize + Default,
{
    let mut serialized = Vec::new();
    T::default().try_serialize(&mut serialized).ok()?;
    let expected_size = serialized.len();

    let owned;
    let buf: &[u8] = if data.len() < expected_size {
        owned = {
            let mut padded = vec![0u8; expected_size];
            padded[..data.len()].copy_from_slice(data);
            padded
        };
        &owned
    } else {
        data
    };
    decode::<T>(buf).ok()
}

// ----------------------------------------------------------------------
// Named pump-account decoders
// ----------------------------------------------------------------------

pub fn decode_global(data: &[u8]) -> Result<Global> {
    decode::<Global>(data)
}

pub fn decode_fee_config(data: &[u8]) -> Result<FeeConfig> {
    decode::<FeeConfig>(data)
}

pub fn decode_bonding_curve(data: &[u8]) -> Result<BondingCurve> {
    decode::<BondingCurve>(data)
}

pub fn decode_bonding_curve_nullable(data: &[u8]) -> Option<BondingCurve> {
    decode_padded::<BondingCurve>(data)
}

pub fn decode_global_volume_accumulator(data: &[u8]) -> Result<GlobalVolumeAccumulator> {
    decode::<GlobalVolumeAccumulator>(data)
}

pub fn decode_user_volume_accumulator(data: &[u8]) -> Result<UserVolumeAccumulator> {
    decode::<UserVolumeAccumulator>(data)
}

pub fn decode_user_volume_accumulator_nullable(data: &[u8]) -> Option<UserVolumeAccumulator> {
    decode::<UserVolumeAccumulator>(data).ok()
}

pub fn decode_sharing_config(data: &[u8]) -> Result<SharingConfig> {
    decode::<SharingConfig>(data)
}

// ----------------------------------------------------------------------
// Named pump_amm-account decoders
// ----------------------------------------------------------------------

pub mod pump_amm {
    use super::{decode, Result};
    use crate::state::pump_amm::{
        BondingCurve, FeeConfig, GlobalConfig, GlobalVolumeAccumulator, Pool, SharingConfig,
        UserVolumeAccumulator,
    };

    pub fn decode_bonding_curve(data: &[u8]) -> Result<BondingCurve> {
        decode::<BondingCurve>(data)
    }

    pub fn decode_fee_config(data: &[u8]) -> Result<FeeConfig> {
        decode::<FeeConfig>(data)
    }

    pub fn decode_global_config(data: &[u8]) -> Result<GlobalConfig> {
        decode::<GlobalConfig>(data)
    }

    pub fn decode_global_volume_accumulator(data: &[u8]) -> Result<GlobalVolumeAccumulator> {
        decode::<GlobalVolumeAccumulator>(data)
    }

    pub fn decode_pool(data: &[u8]) -> Result<Pool> {
        decode::<Pool>(data)
    }

    pub fn decode_sharing_config(data: &[u8]) -> Result<SharingConfig> {
        decode::<SharingConfig>(data)
    }

    pub fn decode_user_volume_accumulator(data: &[u8]) -> Result<UserVolumeAccumulator> {
        decode::<UserVolumeAccumulator>(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::AccountSerialize;
    use solana_program::pubkey::Pubkey;

    use crate::{constants, state::BondingCurve};

    fn fake_pubkey(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    #[test]
    fn bonding_curve_round_trip() {
        let original = BondingCurve {
            virtual_token_reserves: 1_000_000_000,
            virtual_quote_reserves: 30_000_000_000,
            real_token_reserves: 800_000_000,
            real_quote_reserves: 0,
            token_total_supply: 1_000_000_000_000,
            complete: false,
            creator: fake_pubkey(7),
            is_mayhem_mode: true,
            is_cashback_coin: false,
            quote_mint: constants::NATIVE_MINT,
        };

        let mut buf = Vec::new();
        original.try_serialize(&mut buf).expect("serialize");

        let decoded: BondingCurve = decode(&buf).expect("decode");
        assert_eq!(
            decoded.virtual_token_reserves,
            original.virtual_token_reserves
        );
        assert_eq!(decoded.creator, original.creator);
        assert_eq!(decoded.is_mayhem_mode, original.is_mayhem_mode);
        assert_eq!(decoded.quote_mint, original.quote_mint);
    }

    #[test]
    fn decode_padded_pads_short_buffer() {
        let original = BondingCurve {
            virtual_token_reserves: 42,
            virtual_quote_reserves: 0,
            real_token_reserves: 0,
            real_quote_reserves: 0,
            token_total_supply: 0,
            complete: false,
            creator: Pubkey::default(),
            is_mayhem_mode: false,
            is_cashback_coin: false,
            quote_mint: Pubkey::default(),
        };
        let mut buf = Vec::new();
        original.try_serialize(&mut buf).expect("serialize");

        // Strip the new-layout suffix: simulates a not-yet-extended account.
        let truncated_len = buf.len() - 16;
        buf.truncate(truncated_len);

        let decoded: BondingCurve = decode_padded(&buf).expect("decode some");
        assert_eq!(decoded.virtual_token_reserves, 42);
    }
}
