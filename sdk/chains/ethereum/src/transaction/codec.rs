use super::{
    EthereumBuildContext, EthereumSignedTransaction, EthereumTransactionBuilder,
    EthereumTransactionId, EthereumTransactionSigning, EthereumTransferRequest,
    UnsignedEthereumTransaction,
};
use crate::BoxFuture;
use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature as AlloySignature, TxKind, U256, keccak256};
use chain_contract::{ChainError, ChainErrorKind};
use signer::{
    Digest, SignRequest, SignablePayload, SignatureEncoding, SignatureScheme, Signer,
    UserInteraction,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct EthereumTransactionCodec;

impl EthereumTransactionBuilder for EthereumTransactionCodec {
    fn build(
        &self,
        request: EthereumTransferRequest,
        context: EthereumBuildContext,
    ) -> Result<UnsignedEthereumTransaction, ChainError> {
        if context.chain_id == 0 {
            return Err(invalid_transaction("Ethereum chain ID must be non-zero"));
        }
        if context.gas_limit == 0 {
            return Err(invalid_transaction("Ethereum gas limit must be non-zero"));
        }
        if context.max_fee_per_gas < context.max_priority_fee_per_gas {
            return Err(invalid_transaction(
                "Ethereum max fee per gas is below the priority fee",
            ));
        }
        if request.to.is_none() && request.data.is_empty() {
            return Err(invalid_transaction(
                "Ethereum contract creation requires init code",
            ));
        }

        Ok(UnsignedEthereumTransaction {
            key: request.key,
            chain_id: context.chain_id,
            nonce: context.nonce,
            from: request.from,
            to: request.to,
            value: request.value,
            input: request.data,
            gas_limit: context.gas_limit,
            max_fee_per_gas: context.max_fee_per_gas,
            max_priority_fee_per_gas: context.max_priority_fee_per_gas,
        })
    }
}

impl EthereumTransactionSigning for EthereumTransactionCodec {
    fn sign<'a>(
        &'a self,
        transaction: UnsignedEthereumTransaction,
        signer: &'a dyn Signer,
    ) -> BoxFuture<'a, Result<EthereumSignedTransaction, ChainError>> {
        Box::pin(async move {
            let native = native_transaction(&transaction)?;
            let signature_hash = native.signature_hash();
            let signature = signer
                .sign(SignRequest {
                    key: transaction.key,
                    payload: SignablePayload::Digest(Digest {
                        bytes: signature_hash.to_vec(),
                    }),
                    scheme: SignatureScheme::EcdsaSecp256k1,
                    encoding: SignatureEncoding::Recoverable,
                    key_tweak: None,
                    user_interaction: UserInteraction::Allowed,
                })
                .await
                .map_err(signer_error)?;
            if signature.scheme != SignatureScheme::EcdsaSecp256k1
                || signature.encoding != SignatureEncoding::Recoverable
            {
                return Err(signer_error_message(
                    "Ethereum signer returned an incompatible signature",
                ));
            }
            let signature =
                AlloySignature::try_from(signature.bytes.as_slice()).map_err(|error| {
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
            let id = EthereumTransactionId(keccak256(&encoded).0);

            Ok(EthereumSignedTransaction {
                id,
                envelope: encoded,
            })
        })
    }
}

fn native_transaction(transaction: &UnsignedEthereumTransaction) -> Result<TxEip1559, ChainError> {
    let max_fee_per_gas = transaction
        .max_fee_per_gas
        .checked_to_u128()
        .ok_or_else(|| invalid_transaction("Ethereum max fee per gas exceeds u128"))?;
    let max_priority_fee_per_gas = transaction
        .max_priority_fee_per_gas
        .checked_to_u128()
        .ok_or_else(|| invalid_transaction("Ethereum priority fee exceeds u128"))?;

    Ok(TxEip1559 {
        chain_id: transaction.chain_id,
        nonce: transaction.nonce,
        gas_limit: transaction.gas_limit,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        to: transaction.to.as_ref().map_or(TxKind::Create, |address| {
            TxKind::Call(Address::from(address.0))
        }),
        value: U256::from_be_bytes(transaction.value.0),
        access_list: Default::default(),
        input: Bytes::from(transaction.input.clone()),
    })
}

fn invalid_transaction(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::InvalidTransaction,
        message: message.into(),
    }
}

fn signer_error(error: signer::SignerError) -> ChainError {
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
    use crate::{EthereumAddressGenerator, EthereumGenerateAddress, Wei};
    use chain_contract::DepositAddressGenerator;
    use futures_executor::block_on;
    use signer_local::LocalSigner;

    #[test]
    fn builds_and_signs_eip1559_transfer() {
        let signer = LocalSigner::ephemeral_for_testing();
        let generated = block_on(
            EthereumAddressGenerator
                .generate_address(EthereumGenerateAddress::new(31_337, "sender"), &signer),
        )
        .expect("Ethereum sender should be generated");
        let codec = EthereumTransactionCodec;
        let unsigned = codec
            .build(
                EthereumTransferRequest {
                    key: generated.key,
                    from: generated.address,
                    to: Some(crate::EthereumAddress([9; 20])),
                    value: Wei::from_u128(1_000_000),
                    data: Vec::new(),
                },
                EthereumBuildContext {
                    chain_id: 31_337,
                    nonce: 4,
                    gas_limit: 21_000,
                    max_fee_per_gas: Wei::from_u128(2_000_000_000),
                    max_priority_fee_per_gas: Wei::from_u128(1_000_000_000),
                },
            )
            .expect("Ethereum transfer should build");
        let signed =
            block_on(codec.sign(unsigned, &signer)).expect("Ethereum transfer should sign");

        assert_eq!(signed.envelope[0], 0x02);
        assert_eq!(signed.id.0, keccak256(&signed.envelope).0);
    }

    #[test]
    fn rejects_signature_from_the_wrong_sender() {
        let signer = LocalSigner::ephemeral_for_testing();
        let generated = block_on(
            EthereumAddressGenerator
                .generate_address(EthereumGenerateAddress::new(1, "actual-signer"), &signer),
        )
        .expect("Ethereum signer should be generated");
        let codec = EthereumTransactionCodec;
        let unsigned = codec
            .build(
                EthereumTransferRequest {
                    key: generated.key,
                    from: crate::EthereumAddress([7; 20]),
                    to: Some(crate::EthereumAddress([8; 20])),
                    value: Wei::from_u128(1),
                    data: Vec::new(),
                },
                EthereumBuildContext {
                    chain_id: 1,
                    nonce: 0,
                    gas_limit: 21_000,
                    max_fee_per_gas: Wei::from_u128(2),
                    max_priority_fee_per_gas: Wei::from_u128(1),
                },
            )
            .expect("Ethereum transfer should build");
        let error =
            block_on(codec.sign(unsigned, &signer)).expect_err("wrong Ethereum sender must fail");

        assert_eq!(error.kind, ChainErrorKind::Signer);
    }
}
