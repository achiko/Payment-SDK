use base::{
    Digest, KeyTweak, PublicKeyFormat, SignRequest, SignablePayload, SignatureEncoding,
    SignatureScheme, Signer,
};
use bitcoin::{
    Address as NativeAddress, Amount, CompressedPublicKey, ScriptBuf, Transaction, TxOut, Witness,
    consensus,
    hashes::Hash,
    key::{TapTweak, XOnlyPublicKey},
    secp256k1::{Message, Secp256k1, ecdsa, schnorr},
    sighash::{Prevouts, SighashCache},
};

use crate::{ChainError, Network};

use super::{
    Input, SighashType, SignedTransaction, TransactionId, UnsignedTransaction, taproot_sighash_type,
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
        return Err(ChainError::invalid_transaction(
            "Bitcoin transaction needs exactly one signer per input",
        ));
    }
    let mut native = transaction.native(network)?;
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
            return Err(ChainError::invalid_transaction(format!(
                "Bitcoin input {input_index} is neither P2WPKH nor P2TR"
            )));
        };
        native.input[input_index].witness = witness;
    }

    let id = TransactionId::from(native.compute_txid());
    SignedTransaction::from_consensus_bytes(id, consensus::serialize(&native))
}

impl<S: Signer + ?Sized> InputSigner<'_, S> {
    async fn sign_p2wpkh_input(
        &self,
        input_index: usize,
        input: &Input,
    ) -> Result<Witness, ChainError> {
        let script = &self.prevouts[input_index].script_pubkey;
        let sighash_type = self.sighash_type.ecdsa()?;
        let sighash = SighashCache::new(self.transaction)
            .p2wpkh_signature_hash(
                input_index,
                script,
                Amount::from_sat(input.utxo.value.0),
                sighash_type,
            )
            .map_err(|error| {
                ChainError::invalid_transaction(format!(
                    "could not compute Bitcoin input sighash: {error}"
                ))
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
            .map_err(|error| ChainError::signing(format!("Bitcoin signing failed: {error}")))?;
        let public_key =
            CompressedPublicKey::from_slice(&signed.public_key.bytes).map_err(|error| {
                ChainError::signing(format!("invalid compressed Bitcoin public key: {error}"))
            })?;
        if NativeAddress::p2wpkh(&public_key, self.network.native()).script_pubkey() != *script {
            return Err(ChainError::signing(format!(
                "Bitcoin input {input_index} does not belong to its signing key"
            )));
        }
        let signature = signed.signature;
        if signature.scheme != SignatureScheme::EcdsaSecp256k1
            || signature.encoding != SignatureEncoding::Der
        {
            return Err(ChainError::signing(
                "Bitcoin signer returned an incompatible ECDSA signature",
            ));
        }
        let signature = ecdsa::Signature::from_der(&signature.bytes).map_err(|error| {
            ChainError::signing(format!("invalid DER Bitcoin signature: {error}"))
        })?;
        let message = Message::from_digest(sighash.to_byte_array());
        Secp256k1::verification_only()
            .verify_ecdsa(&message, &signature, &public_key.0)
            .map_err(|_| {
                ChainError::signing(format!(
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
                ChainError::invalid_transaction(format!(
                    "could not compute Taproot input sighash: {error}"
                ))
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
            .map_err(|error| ChainError::signing(format!("Bitcoin signing failed: {error}")))?;
        let signature = signed.signature;
        let public_key = XOnlyPublicKey::from_slice(&signed.public_key.bytes).map_err(|error| {
            ChainError::signing(format!("invalid x-only Bitcoin public key: {error}"))
        })?;
        let secp = Secp256k1::verification_only();
        let expected =
            NativeAddress::p2tr(&secp, public_key, None, self.network.native()).script_pubkey();
        if expected != self.prevouts[input_index].script_pubkey {
            return Err(ChainError::signing(format!(
                "Bitcoin Taproot input {input_index} does not belong to its signing key"
            )));
        }
        if signature.scheme != SignatureScheme::SchnorrSecp256k1
            || signature.encoding != SignatureEncoding::Raw
        {
            return Err(ChainError::signing(
                "Bitcoin signer returned an incompatible Schnorr signature",
            ));
        }
        let signature = schnorr::Signature::from_slice(&signature.bytes).map_err(|error| {
            ChainError::signing(format!("invalid raw Bitcoin Schnorr signature: {error}"))
        })?;
        let (output_key, _) = public_key.tap_tweak(&secp, None);
        let message = Message::from_digest(sighash.to_byte_array());
        secp.verify_schnorr(&signature, &message, output_key.as_x_only_public_key())
            .map_err(|_| {
                ChainError::signing(format!(
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use base::{SignFuture, SignerError, SignerErrorKind};
    use futures_executor::block_on;

    use super::*;
    use crate::{ChainErrorKind, Satoshi, SpendSource};

    struct FailingSigner {
        requests: Mutex<Vec<SignRequest>>,
        error: SignerError,
    }

    impl Signer for FailingSigner {
        fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a> {
            self.requests.lock().unwrap().push(request);
            Box::pin(async { Err(self.error.clone()) })
        }
    }

    #[test]
    fn both_input_kinds_preserve_signer_failure_context_and_signing_request() {
        for (script, scheme, encoding, public_key_format, key_tweak) in [
            (
                ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([7; 20])),
                SignatureScheme::EcdsaSecp256k1,
                SignatureEncoding::Der,
                PublicKeyFormat::Compressed,
                None,
            ),
            (
                ScriptBuf::from_bytes([vec![0x51, 0x20], vec![7; 32]].concat()),
                SignatureScheme::SchnorrSecp256k1,
                SignatureEncoding::Raw,
                PublicKeyFormat::XOnly,
                Some(KeyTweak::TaggedHashAdd {
                    tag: b"TapTweak".to_vec(),
                    suffix: Vec::new(),
                }),
            ),
        ] {
            let signer = FailingSigner {
                requests: Mutex::new(Vec::new()),
                error: SignerError {
                    kind: SignerErrorKind::UnsupportedOperation,
                    message: " signer unavailable\nrequest context ".to_owned(),
                },
            };
            let unsigned = UnsignedTransaction {
                version: 2,
                lock_time: 0,
                inputs: vec![Input {
                    utxo: SpendSource {
                        transaction_id: [3; 32],
                        output_index: 0,
                        value: Satoshi(10_000),
                        script_pubkey: script.into_bytes(),
                        satisfaction_weight: 0,
                    },
                    sequence: 0,
                }],
                outputs: Vec::new(),
                sighash_type: SighashType::All,
            };
            let error = block_on(sign(Network::Regtest, unsigned, &signer)).unwrap_err();
            assert_eq!(error.kind, ChainErrorKind::Signer);
            assert_eq!(
                error.message,
                "Bitcoin signing failed:  signer unavailable\nrequest context "
            );
            let requests = signer.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].scheme, scheme);
            assert_eq!(requests[0].encoding, encoding);
            assert_eq!(requests[0].public_key_format, public_key_format);
            assert_eq!(requests[0].key_tweak, key_tweak);
            assert!(
                matches!(&requests[0].payload, SignablePayload::Digest(digest) if digest.bytes.len() == 32)
            );
        }
    }
}
