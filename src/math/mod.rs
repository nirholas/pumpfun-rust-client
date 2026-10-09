//! Bonding-curve and AMM quote math (`u128` intermediates, `u64` at the boundary).

pub mod amm;
pub mod bonding_curve;
pub mod fees;
pub mod utils;

pub use bonding_curve::TOKEN_SUPPLY;

/// Quote failure modes (empty reserves, bad inputs, fee overflow, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteError {
    /// `base_reserve` or `quote_reserve` is zero, so the pool cannot price
    /// any trade.
    EmptyReserves,
    /// Caller asked for `base_out` >= `base_reserve` on a buy. Equivalent
    /// to draining the pool; the constant-product denominator would be
    /// `<= 0`.
    BaseOutExceedsReserve,
    /// Sum of LP, protocol, and coin-creator fees exceeds the raw quote
    /// output — only reachable with a degenerate fee table whose total bps
    /// > 10_000.
    FeesExceedOutput,
    /// Bonding curve degeneracy where `real_token_reserves ==
    /// virtual_token_reserves`, which would zero the constant-product
    /// denominator on a token-out buy.
    DepletedBondingCurve,
    /// A `u128` intermediate overflowed, or a subtraction would underflow
    /// (e.g. `sol_out >= virtual_sol_reserves` on the `sell_token_quote_with_sol`
    /// inverse). Surfaced from the fee-less primitive quote helpers so callers
    /// don't need to wrap primitive arithmetic.
    MathOverflow,
    /// Boosted AMM pool: the curve prices the sell against virtual reserves,
    /// but the quote vault cannot cover the payout. pump-amm rejects the trade
    /// with `InsufficientRealQuoteReserves`.
    InsufficientRealQuoteReserves,
    /// Observed market cap fell outside the caller's `target ± slippage_bps`
    /// envelope. Returned by `validate_market_cap` on both quote paths.
    SlippageExceeded,
    /// Zero input, or fees leave nothing to swap / nothing to pay out. The
    /// programs reject such trades (`BuyNotEnoughSolToCoverFees`,
    /// `ZeroBaseAmount`, `ZeroQuoteAmount`).
    ZeroAmount,
    /// The fee schedule the quote mint selects has no tiers (a `FeeConfig`
    /// shorter than the stable-tier layout). pump errors `FeeTiersEmpty`.
    EmptyFeeTiers,
    /// pump-amm `GlobalConfig.disable_flags` turns this side off
    /// (`DisabledBuy` / `DisabledSell`).
    TradingDisabled,
    /// pump-quote `create_v2`: `quote curve depth + 1 > Global.max_curve_depth`
    /// (pump `CurveDepthExceeded`; a depth of 0 disables the path).
    CurveDepthExceeded,
    /// pump-quote `create_v2`: the quote coin's curve can't seed a new curve
    /// (mayhem, or an unlisted depth-0 quote; pump `QuoteBondingCurveNotEligible`).
    QuoteCurveNotEligible,
    /// pump-quote `create_v2`: derived reserves are zero (pump `QuoteReservesOutOfRange`).
    QuoteReservesOutOfRange,
    /// pump-quote `create_v2`: the quote coin's curve is complete but not migrated
    /// (pump `QuoteCurveAwaitingMigration`).
    QuoteCurveAwaitingMigration,
    /// pump-quote `create_v2`: the quote coin migrated, so its pool and vaults are
    /// needed (pump `QuotePoolAccountsRequired`).
    QuotePoolAccountsRequired,
    /// multi-hop: the route is empty or a hop doesn't connect to the running mint
    /// (`MultiHopDiscontinuousPath`).
    MultiHopDiscontinuousPath,
    /// multi-hop: hops trade in different directions (`MultiHopMixedDirection`).
    MultiHopMixedDirection,
    /// multi-hop: venue refused on this hop (non-pump/mayhem pool, complete curve,
    /// or cashback venue on a creator-fee hop).
    VenueNotSupported,
    /// A buy past the curve's remaining supply where the post-completion leg isn't
    /// allowed (mayhem curve or whitelisted agent; pump `NotEnoughTokensToBuy`).
    PostCompleteLegNotAllowed,
}

impl std::fmt::Display for QuoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyReserves => write!(f, "pool reserves are zero"),
            Self::BaseOutExceedsReserve => {
                write!(f, "base_out exceeds the pool's base reserve")
            }
            Self::FeesExceedOutput => write!(f, "fees exceed the pool's quote output"),
            Self::DepletedBondingCurve => {
                write!(f, "bonding curve is depleted (real == virtual reserves)")
            }
            Self::MathOverflow => write!(f, "checked arithmetic overflowed"),
            Self::InsufficientRealQuoteReserves => {
                write!(f, "pool quote vault cannot cover the payout")
            }
            Self::SlippageExceeded => {
                write!(f, "market cap fell outside the slippage envelope")
            }
            Self::ZeroAmount => write!(f, "amount is zero after fees"),
            Self::EmptyFeeTiers => write!(f, "fee schedule has no tiers"),
            Self::TradingDisabled => write!(f, "trading is disabled on pump-amm"),
            Self::CurveDepthExceeded => write!(f, "curve depth exceeds Global.max_curve_depth"),
            Self::QuoteCurveNotEligible => {
                write!(f, "quote coin's bonding curve is not eligible as a quote")
            }
            Self::QuoteReservesOutOfRange => write!(f, "derived quote reserves are zero"),
            Self::QuoteCurveAwaitingMigration => {
                write!(f, "quote coin's curve is complete but not migrated")
            }
            Self::QuotePoolAccountsRequired => {
                write!(f, "quote coin migrated; its pool accounts are required")
            }
            Self::MultiHopDiscontinuousPath => write!(f, "multi-hop route does not connect"),
            Self::MultiHopMixedDirection => {
                write!(f, "multi-hop hops trade in different directions")
            }
            Self::VenueNotSupported => write!(f, "venue not supported on this hop"),
            Self::PostCompleteLegNotAllowed => {
                write!(
                    f,
                    "buy exceeds the curve and the post-completion leg is not allowed"
                )
            }
        }
    }
}

impl std::error::Error for QuoteError {}

pub type QuoteResult<T> = std::result::Result<T, QuoteError>;
