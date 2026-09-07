use super::super::{SignedTransaction, TransactionId, UnsignedTransaction};
use crate::{ChainError, ChainErrorKind};
use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature as AlloySignature, TxKind, U256, keccak256};
use base::{
    Digest, PublicKeyFormat, SignRequest, SignablePayload, SignatureEncoding, SignatureScheme,
    Signer,
};

pub(in crate::transaction) async fn sign(
    transaction: UnsignedTransaction,
    signer: &dyn Signer,
) -> Result<SignedTransaction, ChainError> {
    let native = transaction.eip1559()?;
    let signature_hash = native.signature_hash();
    let signed = signer
        .sign(SignRequest {
            payload: SignablePayload::Digest(Digest {
                bytes: signature_hash.to_vec(),
            }),
            scheme: SignatureScheme::EcdsaSecp256k1,
            encoding: SignatureEncoding::Recoverable,
            public_key_format: PublicKeyFormat::Raw,
            key_tweak: None,
        })
        .await
        .map_err(signer_error)?;
    let signature = signed.signature;
    if signature.scheme != SignatureScheme::EcdsaSecp256k1
        || signature.encoding != SignatureEncoding::Recoverable
    {
        return Err(signer_error_message(
            "Ethereum signer returned an incompatible signature",
        ));
    }
    let signature = AlloySignature::try_from(signature.bytes.as_slice()).map_err(|error| {
        signer_error_message(format!("invalid recoverable Ethereum signature: {error}"))
    })?;
    let signed = native.into_signed(signature);
    let recovered = signed.recover_signer().map_err(|error| {
        signer_error_message(format!("could not recover Ethereum signer: {error}"))
    })?;
    if recovered.into_array() != transaction.from.0 {
        return Err(signer_error_message(
            "Ethereum signature does not match the transaction sender",
        ));
    }

    let envelope: TxEnvelope = signed.into();
    let mut encoded = Vec::with_capacity(envelope.encode_2718_len());
    envelope.encode_2718(&mut encoded);
    let id = TransactionId(keccak256(&encoded).0);

    Ok(SignedTransaction {
        id,
        envelope: encoded,
    })
}

impl UnsignedTransaction {
    fn eip1559(&self) -> Result<TxEip1559, ChainError> {
        let max_fee_per_gas = self.max_fee_per_gas.checked_to_u128().ok_or_else(|| {
            ChainError::new(
                ChainErrorKind::InvalidTransaction,
                "Ethereum max fee per gas exceeds u128",
            )
        })?;
        let max_priority_fee_per_gas =
            self.max_priority_fee_per_gas
                .checked_to_u128()
                .ok_or_else(|| {
                    ChainError::new(
                        ChainErrorKind::InvalidTransaction,
                        "Ethereum priority fee exceeds u128",
                    )
                })?;

        Ok(TxEip1559 {
            chain_id: self.chain_id,
            nonce: self.nonce,
            gas_limit: self.gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to: self.to.as_ref().map_or(TxKind::Create, |address| {
                TxKind::Call(Address::from(address.0))
            }),
            value: U256::from_be_bytes(self.value.0),
            access_list: Default::default(),
            input: Bytes::from(self.input.clone()),
        })
    }
}

fn signer_error(error: base::SignerError) -> ChainError {
    signer_error_message(format!("Ethereum signing failed: {error}"))
}

fn signer_error_message(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::Signer,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Wei;

    struct MustNotSign;

    impl Signer for MustNotSign {
        fn sign<'a>(&'a self, _: SignRequest) -> base::SignFuture<'a> {
            panic!("out-of-range fees must fail before signing")
        }
    }

    #[test]
    fn eip1559_preserves_call_creation_and_full_width_value() {
        for to in [None, Some(crate::Address([2; 20]))] {
            let transaction = UnsignedTransaction {
                chain_id: u64::MAX,
                nonce: 42,
                from: crate::Address([1; 20]),
                to,
                value: Wei([255; 32]),
                input: vec![0, 127, 255],
                gas_limit: 123_456,
                max_fee_per_gas: Wei::from_u128(u128::MAX),
                max_priority_fee_per_gas: Wei::from_u128(u128::MAX - 1),
            };
            let native = transaction.eip1559().expect("representable fees");
            assert_eq!(native.chain_id, u64::MAX);
            assert_eq!(native.nonce, 42);
            assert_eq!(native.gas_limit, 123_456);
            assert_eq!(native.max_fee_per_gas, u128::MAX);
            assert_eq!(native.max_priority_fee_per_gas, u128::MAX - 1);
            assert_eq!(native.value, U256::MAX);
            assert_eq!(native.input.as_ref(), [0, 127, 255]);
            assert!(native.access_list.0.is_empty());
            assert_eq!(
                native.to,
                match transaction.to {
                    Some(address) => TxKind::Call(Address::from(address.0)),
                    None => TxKind::Create,
                }
            );
        }
    }

    #[test]
    fn fee_width_errors_precede_signing_and_preserve_validation_order() {
        for (max_fee, priority, message) in [
            (
                Wei([255; 32]),
                Wei([255; 32]),
                "Ethereum max fee per gas exceeds u128",
            ),
            (
                Wei::from_u128(u128::MAX),
                Wei([255; 32]),
                "Ethereum priority fee exceeds u128",
            ),
        ] {
            let transaction = UnsignedTransaction {
                chain_id: 1,
                nonce: 0,
                from: crate::Address([1; 20]),
                to: Some(crate::Address([2; 20])),
                value: Wei::ZERO,
                input: Vec::new(),
                gas_limit: 21_000,
                max_fee_per_gas: max_fee,
                max_priority_fee_per_gas: priority,
            };
            let error = futures_executor::block_on(sign(transaction, &MustNotSign))
                .expect_err("oversized fees must not reach the signer");
            assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);
            assert_eq!(error.message, message);
        }
    }
}
