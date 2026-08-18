use base::Decimal;

use crate::Error;

/// Converts exact scale-zero index amounts into this wallet's display unit.
///
/// The wallet owns the configured asset precision. Application orchestration
/// must use this boundary before passing index or reservation amounts to a
/// transaction builder, whose public amounts are expressed in display units.
pub trait AmountFormat: Send + Sync {
    fn display_amount(&self, atomic: &Decimal) -> Result<Decimal, Error>;
}
