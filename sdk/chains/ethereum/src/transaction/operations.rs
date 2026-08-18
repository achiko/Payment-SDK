use super::{BuildContext, SignedTransaction, TransactionId, TransferRequest, UnsignedTransaction};
use crate::{ChainError, ChainErrorKind};
use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature as AlloySignature, TxKind, U256, keccak256};
use base::{
    Digest, PublicKeyFormat, SignRequest, SignablePayload, SignatureEncoding, SignatureScheme,
    Signer,
};

pub(super) fn build(
    request: TransferRequest,
    context: BuildContext,
) -> Result<UnsignedTransaction, ChainError> {
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

    Ok(UnsignedTransaction {
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

pub(super) async fn sign(
    transaction: UnsignedTransaction,
    signer: &dyn Signer,
) -> Result<SignedTransaction, ChainError> {
    let native = native_transaction(&transaction)?;
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

fn native_transaction(transaction: &UnsignedTransaction) -> Result<TxEip1559, ChainError> {
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

fn signer_error(error: base::SignerError) -> ChainError {
    signer_error_message(format!("Ethereum signing failed: {error}"))
}

fn signer_error_message(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::Signer,
        message: message.into(),
    }
}
