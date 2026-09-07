use bitcoin::EcdsaSighashType;

use crate::ChainError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SighashType {
    All,
    None,
    Single,
    AllAnyoneCanPay,
    NoneAnyoneCanPay,
    SingleAnyoneCanPay,
    TaprootDefault,
}

impl SighashType {
    pub(super) fn ecdsa(self) -> Result<EcdsaSighashType, ChainError> {
        match self {
            SighashType::All => Ok(EcdsaSighashType::All),
            SighashType::None => Ok(EcdsaSighashType::None),
            SighashType::Single => Ok(EcdsaSighashType::Single),
            SighashType::AllAnyoneCanPay => Ok(EcdsaSighashType::AllPlusAnyoneCanPay),
            SighashType::NoneAnyoneCanPay => Ok(EcdsaSighashType::NonePlusAnyoneCanPay),
            SighashType::SingleAnyoneCanPay => Ok(EcdsaSighashType::SinglePlusAnyoneCanPay),
            SighashType::TaprootDefault => Err(ChainError::invalid_transaction(
                "Taproot default sighash cannot sign a P2WPKH input",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChainErrorKind;

    #[test]
    fn ecdsa_modes_preserve_their_consensus_flags() {
        for (mode, flag) in [
            (SighashType::All, 0x01),
            (SighashType::None, 0x02),
            (SighashType::Single, 0x03),
            (SighashType::AllAnyoneCanPay, 0x81),
            (SighashType::NoneAnyoneCanPay, 0x82),
            (SighashType::SingleAnyoneCanPay, 0x83),
        ] {
            assert_eq!(mode.ecdsa().unwrap().to_u32(), flag);
        }
    }

    #[test]
    fn ecdsa_rejects_taproot_default() {
        let error = SighashType::TaprootDefault.ecdsa().unwrap_err();
        assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);
        assert_eq!(
            error.message,
            "Taproot default sighash cannot sign a P2WPKH input"
        );
    }
}
