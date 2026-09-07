use super::super::{SignedTransaction, TransactionId, UnsignedTransaction};
use crate::{ChainError, ChainErrorKind};
use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature as AlloySignature, TxKind, U256, keccak256};
use base::{
    Digest, PublicKeyFormat, SignRequest, SignablePayload, SignatureEncoding, SignatureScheme,
    Signer,
};

impl UnsignedTransaction {
    pub(in crate::transaction) async fn sign(
        self,
        signer: &dyn Signer,
    ) -> Result<SignedTransaction, ChainError> {
        let native = self.eip1559()?;
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
            .map_err(|error| {
                ChainError::new(
                    ChainErrorKind::Signer,
                    format!("Ethereum signing failed: {error}"),
                )
            })?;
        let signature = signed.signature;
        if signature.scheme != SignatureScheme::EcdsaSecp256k1
            || signature.encoding != SignatureEncoding::Recoverable
        {
            return Err(ChainError::new(
                ChainErrorKind::Signer,
                "Ethereum signer returned an incompatible signature",
            ));
        }
        let signature = AlloySignature::try_from(signature.bytes.as_slice()).map_err(|error| {
            ChainError::new(
                ChainErrorKind::Signer,
                format!("invalid recoverable Ethereum signature: {error}"),
            )
        })?;
        let signed = native.into_signed(signature);
        let recovered = signed.recover_signer().map_err(|error| {
            ChainError::new(
                ChainErrorKind::Signer,
                format!("could not recover Ethereum signer: {error}"),
            )
        })?;
        if recovered.into_array() != self.from.0 {
            return Err(ChainError::new(
                ChainErrorKind::Signer,
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
            let error = futures_executor::block_on(transaction.sign(&MustNotSign))
                .expect_err("oversized fees must not reach the signer");
            assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);
            assert_eq!(error.message, message);
        }
    }

    struct ReplySigner {
        expected: SignRequest,
        result: Result<base::SignedPayload, base::SignerError>,
    }

    impl Signer for ReplySigner {
        fn sign<'a>(&'a self, request: SignRequest) -> base::SignFuture<'a> {
            assert_eq!(request, self.expected);
            Box::pin(async { self.result.clone() })
        }
    }

    fn signing_request() -> (UnsignedTransaction, SignRequest) {
        let transaction = UnsignedTransaction {
            chain_id: 1,
            nonce: 42,
            from: crate::Address([1; 20]),
            to: Some(crate::Address([2; 20])),
            value: Wei::from_u128(7),
            input: vec![0, 127, 255],
            gas_limit: 21_000,
            max_fee_per_gas: Wei::from_u128(10),
            max_priority_fee_per_gas: Wei::from_u128(3),
        };
        let request = SignRequest {
            payload: SignablePayload::Digest(Digest {
                bytes: transaction.eip1559().unwrap().signature_hash().to_vec(),
            }),
            scheme: SignatureScheme::EcdsaSecp256k1,
            encoding: SignatureEncoding::Recoverable,
            public_key_format: PublicKeyFormat::Raw,
            key_tweak: None,
        };
        (transaction, request)
    }

    #[test]
    fn signer_failure_preserves_digest_request_kind_and_context() {
        let (transaction, expected) = signing_request();
        let signer = ReplySigner {
            expected,
            result: Err(base::SignerError {
                kind: base::SignerErrorKind::UnsupportedOperation,
                message: "fixture rejected signing".to_owned(),
            }),
        };
        let error = futures_executor::block_on(transaction.sign(&signer)).unwrap_err();
        assert_eq!(error.kind, ChainErrorKind::Signer);
        assert_eq!(
            error.message,
            "Ethereum signing failed: fixture rejected signing"
        );
    }

    #[test]
    fn signature_metadata_validation_precedes_recoverable_byte_validation() {
        for (scheme, encoding) in [
            (SignatureScheme::Ed25519, SignatureEncoding::Recoverable),
            (SignatureScheme::EcdsaSecp256k1, SignatureEncoding::Compact),
        ] {
            let (transaction, expected) = signing_request();
            let signer = ReplySigner {
                expected,
                result: Ok(base::SignedPayload {
                    signature: base::Signature {
                        scheme,
                        encoding,
                        bytes: Vec::new(),
                    },
                    public_key: base::PublicKey {
                        curve: base::Curve::Secp256k1,
                        format: PublicKeyFormat::Raw,
                        bytes: Vec::new(),
                    },
                }),
            };
            let error = futures_executor::block_on(transaction.sign(&signer)).unwrap_err();
            assert_eq!(error.kind, ChainErrorKind::Signer);
            assert_eq!(
                error.message,
                "Ethereum signer returned an incompatible signature"
            );
        }
    }
}
