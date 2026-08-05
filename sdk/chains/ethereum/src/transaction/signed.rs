use std::{error::Error, fmt, str::FromStr};

use alloy_consensus::TxEnvelope;
use alloy_eips::Decodable2718;
use alloy_primitives::keccak256;
use indexing::BlockRef;

use crate::Wei;

#[derive(Clone, PartialEq, Eq)]
pub struct EthereumTransactionId(pub [u8; 32]);

impl fmt::Display for EthereumTransactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("0x")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for EthereumTransactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl FromStr for EthereumTransactionId {
    type Err = EthereumTransactionIdParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let hexadecimal = input
            .strip_prefix("0x")
            .ok_or(EthereumTransactionIdParseError::MissingPrefix)?;
        if hexadecimal.len() != 64 {
            return Err(EthereumTransactionIdParseError::InvalidLength);
        }
        let mut bytes = [0_u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hexadecimal[index * 2..index * 2 + 2], 16)
                .map_err(|_| EthereumTransactionIdParseError::InvalidHexadecimal)?;
        }
        let parsed = Self(bytes);
        if parsed.to_string() != input {
            return Err(EthereumTransactionIdParseError::NonCanonical);
        }
        Ok(parsed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EthereumTransactionIdParseError {
    MissingPrefix,
    InvalidLength,
    InvalidHexadecimal,
    NonCanonical,
}

impl fmt::Display for EthereumTransactionIdParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingPrefix => "Ethereum transaction ID is missing its 0x prefix",
            Self::InvalidLength => "Ethereum transaction ID must contain exactly 32 bytes",
            Self::InvalidHexadecimal => "Ethereum transaction ID contains non-hex characters",
            Self::NonCanonical => {
                "Ethereum transaction ID must use canonical lowercase hexadecimal"
            }
        })
    }
}

impl Error for EthereumTransactionIdParseError {}

#[derive(Clone, PartialEq, Eq)]
pub struct EthereumSignedTransaction {
    pub id: EthereumTransactionId,
    pub envelope: Vec<u8>,
}

impl EthereumSignedTransaction {
    /// Reconstructs a signed transaction only when the caller-provided ID is
    /// the canonical Keccak-256 hash of the exact opaque envelope bytes.
    pub fn from_envelope(
        id: EthereumTransactionId,
        envelope: Vec<u8>,
    ) -> Result<Self, EthereumSignedTransactionError> {
        if envelope.is_empty() {
            return Err(EthereumSignedTransactionError::EmptyEnvelope);
        }
        if keccak256(&envelope).0 != id.0 {
            return Err(EthereumSignedTransactionError::HashMismatch);
        }
        Ok(Self { id, envelope })
    }

    /// Decodes the exact EIP-2718 envelope and exposes only the EIP-1559 fee
    /// authorization fields needed by a caller-owned policy boundary.
    ///
    /// The opaque envelope and signature are deliberately absent from the
    /// returned value and from all inspection errors.
    pub fn inspect_eip1559_fees(
        &self,
    ) -> Result<EthereumEip1559FeeInspection, EthereumEip1559InspectionError> {
        let envelope = TxEnvelope::decode_2718_exact(&self.envelope)
            .map_err(|_| EthereumEip1559InspectionError::MalformedEnvelope)?;
        let signed = envelope
            .as_eip1559()
            .ok_or(EthereumEip1559InspectionError::UnsupportedTransactionType)?;
        let transaction = signed.tx();
        if transaction.chain_id == 0 {
            return Err(EthereumEip1559InspectionError::ZeroChainId);
        }
        if transaction.gas_limit == 0 {
            return Err(EthereumEip1559InspectionError::ZeroGasLimit);
        }
        if transaction.max_priority_fee_per_gas > transaction.max_fee_per_gas {
            return Err(EthereumEip1559InspectionError::PriorityFeeExceedsMaximumFee);
        }

        let max_fee_per_gas = Wei::from_u128(transaction.max_fee_per_gas);
        let maximum_total_fee = max_fee_per_gas
            .checked_mul_u64(transaction.gas_limit)
            .ok_or(EthereumEip1559InspectionError::MaximumTotalFeeOverflow)?;
        Ok(EthereumEip1559FeeInspection {
            chain_id: transaction.chain_id,
            gas_limit: transaction.gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas: Wei::from_u128(transaction.max_priority_fee_per_gas),
            maximum_total_fee,
        })
    }
}

impl fmt::Debug for EthereumSignedTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EthereumSignedTransaction")
            .field("id", &self.id)
            .field("envelope", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EthereumSignedTransactionError {
    EmptyEnvelope,
    HashMismatch,
}

impl fmt::Display for EthereumSignedTransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyEnvelope => "signed Ethereum envelope must not be empty",
            Self::HashMismatch => "signed Ethereum envelope hash does not match its transaction ID",
        })
    }
}

impl Error for EthereumSignedTransactionError {}

/// Policy-relevant fields decoded from one exact signed EIP-1559 envelope.
///
/// This boundary intentionally carries neither the opaque envelope nor its
/// signature and does not implement `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct EthereumEip1559FeeInspection {
    pub chain_id: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: Wei,
    pub max_priority_fee_per_gas: Wei,
    pub maximum_total_fee: Wei,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EthereumEip1559InspectionError {
    MalformedEnvelope,
    UnsupportedTransactionType,
    ZeroChainId,
    ZeroGasLimit,
    PriorityFeeExceedsMaximumFee,
    MaximumTotalFeeOverflow,
}

impl fmt::Display for EthereumEip1559InspectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MalformedEnvelope => "signed Ethereum envelope is malformed",
            Self::UnsupportedTransactionType => {
                "signed Ethereum envelope is not an EIP-1559 transaction"
            }
            Self::ZeroChainId => "signed EIP-1559 transaction has a zero chain ID",
            Self::ZeroGasLimit => "signed EIP-1559 transaction has a zero gas limit",
            Self::PriorityFeeExceedsMaximumFee => {
                "signed EIP-1559 priority fee exceeds its maximum fee per gas"
            }
            Self::MaximumTotalFeeOverflow => {
                "signed EIP-1559 maximum total fee overflows the Ethereum amount range"
            }
        })
    }
}

impl Error for EthereumEip1559InspectionError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumReceipt {
    pub id: EthereumTransactionId,
    pub included_in: Option<BlockRef>,
    pub succeeded: Option<bool>,
    pub confirmations: u64,
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction, TxEnvelope, TxLegacy};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, Bytes, Signature, U256};

    use super::*;

    #[test]
    fn transaction_ids_require_canonical_lowercase_hexadecimal() {
        let id = EthereumTransactionId([0xab; 32]);
        assert_eq!(id.to_string().parse(), Ok(id.clone()));
        assert_eq!(
            id.to_string()
                .to_ascii_uppercase()
                .parse::<EthereumTransactionId>(),
            Err(EthereumTransactionIdParseError::MissingPrefix)
        );
    }

    #[test]
    fn envelope_construction_validates_hash_and_redacts_debug() {
        let envelope = vec![0x02, 0x01, 0x02];
        let id = EthereumTransactionId(keccak256(&envelope).0);
        let signed = EthereumSignedTransaction::from_envelope(id, envelope.clone())
            .expect("matching envelope must be accepted");
        assert!(!format!("{signed:?}").contains("020102"));
        assert_eq!(
            EthereumSignedTransaction::from_envelope(EthereumTransactionId([0; 32]), envelope),
            Err(EthereumSignedTransactionError::HashMismatch)
        );
    }

    #[test]
    fn fee_inspection_rejects_malformed_and_non_eip1559_envelopes() {
        let malformed_bytes = b"hello".to_vec();
        let malformed = EthereumSignedTransaction::from_envelope(
            EthereumTransactionId(keccak256(&malformed_bytes).0),
            malformed_bytes,
        )
        .expect("matching transaction ID should preserve the malformed test bytes");
        assert!(matches!(
            malformed.inspect_eip1559_fees(),
            Err(EthereumEip1559InspectionError::MalformedEnvelope)
        ));

        let legacy = TxLegacy {
            chain_id: Some(1),
            nonce: 1,
            gas_price: 2,
            gas_limit: 21_000,
            to: Address::ZERO.into(),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let signature =
            Signature::from_scalars_and_parity([1_u8; 32].into(), [2_u8; 32].into(), false);
        let envelope: TxEnvelope = legacy.into_signed(signature).into();
        let mut legacy_bytes = Vec::with_capacity(envelope.encode_2718_len());
        envelope.encode_2718(&mut legacy_bytes);
        let legacy = EthereumSignedTransaction::from_envelope(
            EthereumTransactionId(keccak256(&legacy_bytes).0),
            legacy_bytes,
        )
        .expect("matching transaction ID should accept the synthetic legacy envelope");
        assert!(matches!(
            legacy.inspect_eip1559_fees(),
            Err(EthereumEip1559InspectionError::UnsupportedTransactionType)
        ));
    }
}
