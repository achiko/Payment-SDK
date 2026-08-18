use base::{
    Digest, KeyTweak, PublicKeyFormat, SignRequest, SignablePayload, SignatureEncoding,
    SignatureScheme, Signer,
};
use bitcoin::{
    Address as NativeAddress, Amount, CompressedPublicKey, OutPoint, ScriptBuf, Sequence,
    Transaction, TxIn, TxOut, Txid, Witness, absolute, consensus,
    hashes::Hash,
    key::{TapTweak, XOnlyPublicKey},
    secp256k1::{Message, Secp256k1, ecdsa, schnorr},
    sighash::{Prevouts, SighashCache},
    transaction::Version,
};

use crate::{ChainError, Network};

use super::{
    Input, SighashType, SignedTransaction, TransactionId, UnsignedTransaction, ecdsa_sighash_type,
    invalid_transaction, native_network, signer_error, signer_error_message, taproot_sighash_type,
};

struct InputSigner<'a, S: ?Sized> {
    transaction: &'a Transaction,
    prevouts: &'a [TxOut],
    sighash_type: SighashType,
    network: Network,
    signer: &'a S,
}

pub(in crate::transaction) async fn sign(
    network: Network,
    transaction: UnsignedTransaction,
    signer: &dyn Signer,
) -> Result<SignedTransaction, ChainError> {
    let signers = transaction
        .inputs
        .iter()
        .map(|_| signer)
        .collect::<Vec<_>>();
    sign_each(network, transaction, &signers).await
}

pub(in crate::transaction) async fn sign_each<S: Signer + ?Sized>(
    network: Network,
    transaction: UnsignedTransaction,
    signers: &[&S],
) -> Result<SignedTransaction, ChainError> {
    if transaction.inputs.len() != signers.len() {
        return Err(invalid_transaction(
            "Bitcoin transaction needs exactly one signer per input",
        ));
    }
    let mut native = native_transaction(network, &transaction)?;
    let prevouts = transaction
        .inputs
        .iter()
        .map(|input| {
            Ok(TxOut {
                value: Amount::from_sat(input.utxo.value.0),
                script_pubkey: ScriptBuf::from_bytes(input.utxo.script_pubkey.clone()),
            })
        })
        .collect::<Result<Vec<_>, ChainError>>()?;

    for (input_index, input) in transaction.inputs.iter().enumerate() {
        let script = &prevouts[input_index].script_pubkey;
        let signing = InputSigner {
            transaction: &native,
            prevouts: &prevouts,
            sighash_type: transaction.sighash_type,
            network,
            signer: signers[input_index],
        };
        let witness = if script.is_p2wpkh() {
            signing.sign_p2wpkh_input(input_index, input).await?
        } else if script.is_p2tr() {
            signing.sign_p2tr_input(input_index).await?
        } else {
            return Err(invalid_transaction(format!(
                "Bitcoin input {input_index} is neither P2WPKH nor P2TR"
            )));
        };
        native.input[input_index].witness = witness;
    }

    let id = TransactionId::from(native.compute_txid());
    SignedTransaction::from_consensus_bytes(id, consensus::serialize(&native))
}

fn native_transaction(
    network: Network,
    transaction: &UnsignedTransaction,
) -> Result<Transaction, ChainError> {
    let input = transaction
        .inputs
        .iter()
        .map(|input| TxIn {
            previous_output: OutPoint::new(
                Txid::from_byte_array(input.utxo.transaction_id),
                input.utxo.output_index,
            ),
            script_sig: ScriptBuf::new(),
            sequence: Sequence(input.sequence),
            witness: Witness::new(),
        })
        .collect();
    let output = transaction
        .outputs
        .iter()
        .map(|output| {
            Ok(TxOut {
                value: Amount::from_sat(output.value.0),
                script_pubkey: super::checked_address(network, &output.address)?.script_pubkey(),
            })
        })
        .collect::<Result<Vec<_>, ChainError>>()?;
    Ok(Transaction {
        version: Version(transaction.version),
        lock_time: absolute::LockTime::from_consensus(transaction.lock_time),
        input,
        output,
    })
}

impl<S: Signer + ?Sized> InputSigner<'_, S> {
    async fn sign_p2wpkh_input(
        &self,
        input_index: usize,
        input: &Input,
    ) -> Result<Witness, ChainError> {
        let script = &self.prevouts[input_index].script_pubkey;
        let sighash_type = ecdsa_sighash_type(self.sighash_type)?;
        let sighash = SighashCache::new(self.transaction)
            .p2wpkh_signature_hash(
                input_index,
                script,
                Amount::from_sat(input.utxo.value.0),
                sighash_type,
            )
            .map_err(|error| {
                invalid_transaction(format!("could not compute Bitcoin input sighash: {error}"))
            })?;
        let signed = self
            .signer
            .sign(SignRequest {
                payload: SignablePayload::Digest(Digest {
                    bytes: sighash.to_byte_array().to_vec(),
                }),
                scheme: SignatureScheme::EcdsaSecp256k1,
                encoding: SignatureEncoding::Der,
                public_key_format: PublicKeyFormat::Compressed,
                key_tweak: None,
            })
            .await
            .map_err(signer_error)?;
        let public_key =
            CompressedPublicKey::from_slice(&signed.public_key.bytes).map_err(|error| {
                signer_error_message(format!("invalid compressed Bitcoin public key: {error}"))
            })?;
        if NativeAddress::p2wpkh(&public_key, native_network(self.network)).script_pubkey()
            != *script
        {
            return Err(signer_error_message(format!(
                "Bitcoin input {input_index} does not belong to its signing key"
            )));
        }
        let signature = signed.signature;
        if signature.scheme != SignatureScheme::EcdsaSecp256k1
            || signature.encoding != SignatureEncoding::Der
        {
            return Err(signer_error_message(
                "Bitcoin signer returned an incompatible ECDSA signature",
            ));
        }
        let signature = ecdsa::Signature::from_der(&signature.bytes).map_err(|error| {
            signer_error_message(format!("invalid DER Bitcoin signature: {error}"))
        })?;
        let message = Message::from_digest(sighash.to_byte_array());
        Secp256k1::verification_only()
            .verify_ecdsa(&message, &signature, &public_key.0)
            .map_err(|_| {
                signer_error_message(format!(
                    "Bitcoin signer returned an ECDSA signature that failed cryptographic verification for input {input_index}"
                ))
            })?;
        let signature = bitcoin::ecdsa::Signature {
            signature,
            sighash_type,
        };
        let signature = signature.serialize();
        let public_key = public_key.to_bytes();
        let signature_bytes: &[u8] = signature.as_ref();
        Ok(Witness::from_slice(&[
            signature_bytes.to_vec(),
            public_key.to_vec(),
        ]))
    }

    async fn sign_p2tr_input(&self, input_index: usize) -> Result<Witness, ChainError> {
        let sighash_type = taproot_sighash_type(self.sighash_type)?;
        let sighash = SighashCache::new(self.transaction)
            .taproot_key_spend_signature_hash(
                input_index,
                &Prevouts::All(self.prevouts),
                sighash_type,
            )
            .map_err(|error| {
                invalid_transaction(format!("could not compute Taproot input sighash: {error}"))
            })?;
        let signed = self
            .signer
            .sign(SignRequest {
                payload: SignablePayload::Digest(Digest {
                    bytes: sighash.to_byte_array().to_vec(),
                }),
                scheme: SignatureScheme::SchnorrSecp256k1,
                encoding: SignatureEncoding::Raw,
                public_key_format: PublicKeyFormat::XOnly,
                key_tweak: Some(KeyTweak::TaggedHashAdd {
                    tag: b"TapTweak".to_vec(),
                    suffix: Vec::new(),
                }),
            })
            .await
            .map_err(signer_error)?;
        let signature = signed.signature;
        let public_key = XOnlyPublicKey::from_slice(&signed.public_key.bytes).map_err(|error| {
            signer_error_message(format!("invalid x-only Bitcoin public key: {error}"))
        })?;
        let secp = Secp256k1::verification_only();
        let expected = NativeAddress::p2tr(&secp, public_key, None, native_network(self.network))
            .script_pubkey();
        if expected != self.prevouts[input_index].script_pubkey {
            return Err(signer_error_message(format!(
                "Bitcoin Taproot input {input_index} does not belong to its signing key"
            )));
        }
        if signature.scheme != SignatureScheme::SchnorrSecp256k1
            || signature.encoding != SignatureEncoding::Raw
        {
            return Err(signer_error_message(
                "Bitcoin signer returned an incompatible Schnorr signature",
            ));
        }
        let signature = schnorr::Signature::from_slice(&signature.bytes).map_err(|error| {
            signer_error_message(format!("invalid raw Bitcoin Schnorr signature: {error}"))
        })?;
        let (output_key, _) = public_key.tap_tweak(&secp, None);
        let message = Message::from_digest(sighash.to_byte_array());
        secp.verify_schnorr(&signature, &message, output_key.as_x_only_public_key())
            .map_err(|_| {
                signer_error_message(format!(
                    "Bitcoin signer returned a Schnorr signature that failed cryptographic verification for input {input_index}"
                ))
            })?;
        let signature = bitcoin::taproot::Signature {
            signature,
            sighash_type,
        };
        Ok(Witness::from_slice(&[signature.to_vec()]))
    }
}
