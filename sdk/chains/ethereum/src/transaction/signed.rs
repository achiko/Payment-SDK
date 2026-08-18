use std::{error::Error, fmt, str::FromStr};

use alloy_consensus::TxEnvelope;
use alloy_eips::Decodable2718;
use alloy_primitives::keccak256;
use indexing::BlockRef;

use crate::Wei;

#[derive(Clone, PartialEq, Eq)]
pub struct Id(pub [u8; 32]);

impl fmt::Display for Id {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("0x")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl FromStr for Id {
    type Err = IdError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let hexadecimal = input.strip_prefix("0x").ok_or(IdError::MissingPrefix)?;
        if hexadecimal.len() != 64 {
            return Err(IdError::InvalidLength);
        }
        let mut bytes = [0_u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hexadecimal[index * 2..index * 2 + 2], 16)
                .map_err(|_| IdError::InvalidHexadecimal)?;
        }
        let parsed = Self(bytes);
        if parsed.to_string() != input {
            return Err(IdError::NonCanonical);
        }
        Ok(parsed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdError {
    MissingPrefix,
    InvalidLength,
    InvalidHexadecimal,
    NonCanonical,
}

impl fmt::Display for IdError {
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

impl Error for IdError {}

#[derive(Clone, PartialEq, Eq)]
pub struct SignedTransaction {
    pub id: Id,
    pub envelope: Vec<u8>,
}

impl SignedTransaction {
    /// Reconstructs a signed transaction only when the caller-provided ID is
    /// the canonical Keccak-256 hash of the exact opaque envelope bytes.
    pub fn from_envelope(id: Id, envelope: Vec<u8>) -> Result<Self, SignedError> {
        if envelope.is_empty() {
            return Err(SignedError::EmptyEnvelope);
        }
        if keccak256(&envelope).0 != id.0 {
            return Err(SignedError::HashMismatch);
        }
        Ok(Self { id, envelope })
    }

    /// Decodes the exact EIP-2718 envelope and exposes only the EIP-1559 fee
    /// authorization fields needed by a caller-owned policy boundary.
    ///
    /// The opaque envelope and signature are deliberately absent from the
    /// returned value and from all inspection errors.
    pub fn inspect_eip1559_fees(&self) -> Result<FeeInspection, InspectionError> {
        let envelope = TxEnvelope::decode_2718_exact(&self.envelope)
            .map_err(|_| InspectionError::MalformedEnvelope)?;
        let signed = envelope
            .as_eip1559()
            .ok_or(InspectionError::UnsupportedTransactionType)?;
        let transaction = signed.tx();
        if transaction.chain_id == 0 {
            return Err(InspectionError::ZeroChainId);
        }
        if transaction.gas_limit == 0 {
            return Err(InspectionError::ZeroGasLimit);
        }
        if transaction.max_priority_fee_per_gas > transaction.max_fee_per_gas {
            return Err(InspectionError::PriorityFeeExceedsMaximumFee);
        }

        let max_fee_per_gas = Wei::from_u128(transaction.max_fee_per_gas);
        let maximum_total_fee = max_fee_per_gas
            .checked_mul_u64(transaction.gas_limit)
            .ok_or(InspectionError::MaximumTotalFeeOverflow)?;
        Ok(FeeInspection {
            chain_id: transaction.chain_id,
            gas_limit: transaction.gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas: Wei::from_u128(transaction.max_priority_fee_per_gas),
            maximum_total_fee,
        })
    }
}

impl fmt::Debug for SignedTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignedTransaction")
            .field("id", &self.id)
            .field("envelope", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignedError {
    EmptyEnvelope,
    HashMismatch,
}

impl fmt::Display for SignedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyEnvelope => "signed Ethereum envelope must not be empty",
            Self::HashMismatch => "signed Ethereum envelope hash does not match its transaction ID",
        })
    }
}

impl Error for SignedError {}

/// Policy-relevant fields decoded from one exact signed EIP-1559 envelope.
///
/// This boundary intentionally carries neither the opaque envelope nor its
/// signature and does not implement `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct FeeInspection {
    pub chain_id: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: Wei,
    pub max_priority_fee_per_gas: Wei,
    pub maximum_total_fee: Wei,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectionError {
    MalformedEnvelope,
    UnsupportedTransactionType,
    ZeroChainId,
    ZeroGasLimit,
    PriorityFeeExceedsMaximumFee,
    MaximumTotalFeeOverflow,
}

impl fmt::Display for InspectionError {
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

impl Error for InspectionError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub id: Id,
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
        let id = Id([0xab; 32]);
        assert_eq!(id.to_string().parse(), Ok(id.clone()));
        assert_eq!(
            id.to_string().to_ascii_uppercase().parse::<Id>(),
            Err(IdError::MissingPrefix)
        );
    }

    #[test]
    fn envelope_construction_validates_hash_and_redacts_debug() {
        let envelope = vec![0x02, 0x01, 0x02];
        let id = Id(keccak256(&envelope).0);
        let signed = SignedTransaction::from_envelope(id, envelope.clone())
            .expect("matching envelope must be accepted");
        assert!(!format!("{signed:?}").contains("020102"));
        assert_eq!(
            SignedTransaction::from_envelope(Id([0; 32]), envelope),
            Err(SignedError::HashMismatch)
        );
    }

    #[test]
    fn fee_inspection_rejects_malformed_and_non_eip1559_envelopes() {
        let malformed_bytes = b"hello".to_vec();
        let malformed =
            SignedTransaction::from_envelope(Id(keccak256(&malformed_bytes).0), malformed_bytes)
                .expect("matching transaction ID should preserve the malformed test bytes");
        assert!(matches!(
            malformed.inspect_eip1559_fees(),
            Err(InspectionError::MalformedEnvelope)
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
        let legacy = SignedTransaction::from_envelope(Id(keccak256(&legacy_bytes).0), legacy_bytes)
            .expect("matching transaction ID should accept the synthetic legacy envelope");
        assert!(matches!(
            legacy.inspect_eip1559_fees(),
            Err(InspectionError::UnsupportedTransactionType)
        ));
    }
}
