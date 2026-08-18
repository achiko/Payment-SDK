use super::{
    BuildRequest, Input, Output, SighashType, SignedTransaction, SpendSource, TransactionId,
    UnsignedTransaction,
};
use crate::{ChainError, ChainErrorKind};
use crate::{Network, Satoshi};
use base::{
    Digest, KeyTweak, PublicKeyFormat, SignRequest, SignablePayload, SignatureEncoding,
    SignatureScheme, Signer,
};
use bitcoin::{
    Address as NativeAddress, Amount, CompressedPublicKey, Network as NativeNetwork, OutPoint,
    ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness, absolute, consensus,
    hashes::Hash,
    key::{TapTweak, XOnlyPublicKey},
    secp256k1::{Message, Secp256k1, ecdsa, schnorr},
    sighash::{Prevouts, SighashCache},
    transaction::Version,
};
const SEGWIT_MARKER_FLAG_WEIGHT: u64 = 2;

struct InputSigner<'a, S: ?Sized> {
    transaction: &'a Transaction,
    prevouts: &'a [TxOut],
    sighash_type: SighashType,
    network: Network,
    signer: &'a S,
}

pub(super) async fn sign(
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

pub(super) async fn sign_each<S: Signer + ?Sized>(
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

pub(super) fn build(
    network: Network,
    mut request: BuildRequest,
) -> Result<UnsignedTransaction, ChainError> {
    if request.available.is_empty() {
        return Err(insufficient_funds(
            "Bitcoin transfer has no available UTXOs",
        ));
    }
    if request.recipients.is_empty() {
        return Err(invalid_transaction("Bitcoin transfer has no recipients"));
    }
    if request.fee_rate.satoshis_per_kvb() == 0 {
        return Err(ChainError {
            kind: ChainErrorKind::FeeUnavailable,
            message: "Bitcoin fee rate must be greater than zero".to_owned(),
        });
    }
    validate_unique_utxos(&request.available)?;
    for utxo in &request.available {
        let script = ScriptBuf::from_bytes(utxo.script_pubkey.clone());
        if !script.is_p2wpkh() && !script.is_p2tr() {
            return Err(invalid_transaction(
                "Bitcoin wallet supports P2WPKH and P2TR inputs only",
            ));
        }
    }

    let recipient_scripts = request
        .recipients
        .iter()
        .map(|output| checked_output(network, output, request.drain_wallet))
        .collect::<Result<Vec<_>, ChainError>>()?;
    let change_script = checked_address(network, &request.change_address)?.script_pubkey();

    if request.drain_wallet {
        if request.recipients.len() != 1 {
            return Err(invalid_transaction(
                "Bitcoin drain transfer requires exactly one recipient",
            ));
        }
        request.available.sort_by(canonical_outpoint_order);
        let selected_total = sum_utxos(&request.available)?;
        let fee = predicted_fee(&request.available, &recipient_scripts, request.fee_rate)?;
        let value = selected_total.checked_sub(fee).ok_or_else(|| {
            insufficient_funds("Bitcoin UTXOs cannot cover the drain transaction fee")
        })?;
        let minimum = recipient_scripts[0].minimal_non_dust().to_sat();
        if value < minimum {
            return Err(insufficient_funds(format!(
                "Bitcoin drain output is dust: minimum is {minimum} satoshis"
            )));
        }
        request.recipients[0].value = Satoshi(value);
        return Ok(unsigned(request.available, request.recipients));
    }

    request.available.sort_by(|left, right| {
        right
            .value
            .cmp(&left.value)
            .then_with(|| left.transaction_id.cmp(&right.transaction_id))
            .then_with(|| left.output_index.cmp(&right.output_index))
    });

    let recipient_total = request.recipients.iter().try_fold(0_u64, |total, output| {
        total
            .checked_add(output.value.0)
            .ok_or_else(|| invalid_transaction("Bitcoin recipient amount overflowed u64"))
    })?;
    let mut selected = Vec::new();
    let mut selected_total = 0_u64;
    let mut final_outputs = None;

    for utxo in request.available {
        selected_total = selected_total
            .checked_add(utxo.value.0)
            .ok_or_else(|| invalid_transaction("Bitcoin selected input amount overflowed u64"))?;
        selected.push(utxo);

        let fee_without_change = predicted_fee(&selected, &recipient_scripts, request.fee_rate)?;
        let required = recipient_total
            .checked_add(fee_without_change)
            .ok_or_else(|| invalid_transaction("Bitcoin amount and fee overflowed u64"))?;
        if selected_total < required {
            continue;
        }

        let mut with_change_scripts = recipient_scripts.clone();
        with_change_scripts.push(change_script.clone());
        let fee_with_change = predicted_fee(&selected, &with_change_scripts, request.fee_rate)?;
        let change = selected_total
            .checked_sub(recipient_total)
            .and_then(|value| value.checked_sub(fee_with_change));
        let mut outputs = request.recipients.clone();
        if let Some(change) =
            change.filter(|value| *value >= change_script.minimal_non_dust().to_sat())
        {
            outputs.push(Output {
                address: request.change_address.clone(),
                value: Satoshi(change),
            });
        }
        final_outputs = Some(outputs);
        break;
    }

    let outputs = final_outputs.ok_or_else(|| {
        insufficient_funds(format!(
            "insufficient Bitcoin funds for {recipient_total} satoshis plus network fee"
        ))
    })?;
    Ok(unsigned(selected, outputs))
}

fn canonical_outpoint_order(left: &SpendSource, right: &SpendSource) -> std::cmp::Ordering {
    TransactionId(left.transaction_id)
        .to_string()
        .cmp(&TransactionId(right.transaction_id).to_string())
        .then_with(|| left.output_index.cmp(&right.output_index))
}

fn unsigned(utxos: Vec<SpendSource>, outputs: Vec<Output>) -> UnsignedTransaction {
    UnsignedTransaction {
        version: 2,
        lock_time: 0,
        inputs: utxos
            .into_iter()
            .map(|utxo| Input {
                utxo,
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME.to_consensus_u32(),
            })
            .collect(),
        outputs,
        sighash_type: SighashType::All,
    }
}

fn predicted_fee(
    inputs: &[SpendSource],
    output_scripts: &[ScriptBuf],
    fee_rate: crate::FeeRate,
) -> Result<u64, ChainError> {
    let transaction = Transaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: inputs
            .iter()
            .map(|utxo| TxIn {
                previous_output: OutPoint::new(
                    Txid::from_byte_array(utxo.transaction_id),
                    utxo.output_index,
                ),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            })
            .collect(),
        output: output_scripts
            .iter()
            .map(|script_pubkey| TxOut {
                value: Amount::ZERO,
                script_pubkey: script_pubkey.clone(),
            })
            .collect(),
    };
    let satisfaction_weight = inputs.iter().try_fold(0_u64, |total, utxo| {
        total
            .checked_add(utxo.satisfaction_weight)
            .ok_or_else(|| invalid_transaction("Bitcoin satisfaction weight overflowed u64"))
    })?;
    let weight = transaction
        .weight()
        .to_wu()
        .checked_add(SEGWIT_MARKER_FLAG_WEIGHT)
        .and_then(|weight| weight.checked_add(satisfaction_weight))
        .ok_or_else(|| invalid_transaction("Bitcoin transaction weight overflowed u64"))?;
    let virtual_size = weight.div_ceil(4);
    fee_for_vsize(fee_rate, virtual_size)
}

fn fee_for_vsize(fee_rate: crate::FeeRate, virtual_size: u64) -> Result<u64, ChainError> {
    let numerator = u128::from(fee_rate.satoshis_per_kvb())
        .checked_mul(u128::from(virtual_size))
        .and_then(|value| value.checked_add(999))
        .ok_or_else(|| invalid_transaction("Bitcoin transaction fee overflowed u128"))?;
    u64::try_from(numerator / 1_000)
        .map_err(|_| invalid_transaction("Bitcoin transaction fee overflowed u64"))
}

fn native_transaction(
    network: Network,
    transaction: &UnsignedTransaction,
) -> Result<Transaction, ChainError> {
    let version = Version(transaction.version);
    let lock_time = absolute::LockTime::from_consensus(transaction.lock_time);
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
                script_pubkey: checked_address(network, &output.address)?.script_pubkey(),
            })
        })
        .collect::<Result<Vec<_>, ChainError>>()?;
    Ok(Transaction {
        version,
        lock_time,
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
        let witness = vec![signature_bytes.to_vec(), public_key.to_vec()];
        Ok(Witness::from_slice(&witness))
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
        let secp = bitcoin::secp256k1::Secp256k1::verification_only();
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

mod rules;

use rules::{
    checked_address, checked_output, ecdsa_sighash_type, sum_utxos, taproot_sighash_type,
    validate_unique_utxos,
};
pub(crate) const fn native_network(network: Network) -> NativeNetwork {
    match network {
        Network::Mainnet => NativeNetwork::Bitcoin,
        Network::Testnet3 => NativeNetwork::Testnet,
        Network::Testnet4 => NativeNetwork::Testnet4,
        Network::Signet => NativeNetwork::Signet,
        Network::Regtest => NativeNetwork::Regtest,
    }
}

pub(super) fn invalid_transaction(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::InvalidTransaction,
        message: message.into(),
    }
}

fn insufficient_funds(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::InsufficientFunds,
        message: message.into(),
    }
}

fn signer_error(error: base::SignerError) -> ChainError {
    signer_error_message(format!("Bitcoin signing failed: {error}"))
}

fn signer_error_message(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::Signer,
        message: message.into(),
    }
}
