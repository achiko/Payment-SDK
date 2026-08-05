use super::{
    BitcoinBuildRequest, BitcoinInput, BitcoinOutput, BitcoinSignedTransaction,
    BitcoinTransactionBuilder, BitcoinTransactionId, BitcoinTransactionSigning, BitcoinUtxo,
    SighashType, UnsignedBitcoinTransaction,
};
use crate::{BitcoinAddress, BitcoinNetwork, BoxFuture, Satoshi};
use bitcoin::{
    Address, Amount, CompressedPublicKey, EcdsaSighashType, Network, OutPoint, ScriptBuf, Sequence,
    TapSighashType, Transaction, TxIn, TxOut, Txid, Witness, absolute,
    address::NetworkUnchecked,
    consensus,
    hashes::Hash,
    key::XOnlyPublicKey,
    secp256k1::{ecdsa, schnorr},
    sighash::{Prevouts, SighashCache},
    taproot::TapTweakHash,
    transaction::Version,
};
use chain_contract::{ChainError, ChainErrorKind};
use signer::{
    Curve, Digest, KeyTweak, OperationId, PublicKeyFormat, SignRequest, SignablePayload,
    SignatureEncoding, SignatureScheme, Signer, UserInteraction,
};
use std::collections::BTreeSet;

const SEGWIT_MARKER_FLAG_WEIGHT: u64 = 2;

#[derive(Clone, Copy, Debug)]
pub struct BitcoinTransactionCodec {
    network: BitcoinNetwork,
}

impl BitcoinTransactionCodec {
    #[must_use]
    pub const fn new(network: BitcoinNetwork) -> Self {
        Self { network }
    }
}

struct BitcoinInputSigningContext<'a> {
    transaction: &'a Transaction,
    prevouts: &'a [TxOut],
    sighash_type: SighashType,
    network: BitcoinNetwork,
    signer: &'a dyn Signer,
}

impl BitcoinTransactionBuilder for BitcoinTransactionCodec {
    fn build(
        &self,
        request: BitcoinBuildRequest,
    ) -> Result<UnsignedBitcoinTransaction, ChainError> {
        build_transaction(self.network, request)
    }
}

impl BitcoinTransactionSigning for BitcoinTransactionCodec {
    fn sign<'a>(
        &'a self,
        transaction: UnsignedBitcoinTransaction,
        signer: &'a dyn Signer,
    ) -> BoxFuture<'a, Result<BitcoinSignedTransaction, ChainError>> {
        Box::pin(async move {
            let mut native = native_transaction(self.network, &transaction)?;
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
                let operation_id = transaction
                    .signing_operation_id
                    .child(format!("input-{input_index}"))
                    .map_err(signer_error)?;
                let signing = BitcoinInputSigningContext {
                    transaction: &native,
                    prevouts: &prevouts,
                    sighash_type: transaction.sighash_type,
                    network: self.network,
                    signer,
                };
                let witness = if script.is_p2wpkh() {
                    signing
                        .sign_p2wpkh_input(operation_id, input_index, input)
                        .await?
                } else if script.is_p2tr() {
                    signing
                        .sign_p2tr_input(operation_id, input_index, input)
                        .await?
                } else {
                    return Err(invalid_transaction(format!(
                        "Bitcoin input {input_index} is neither P2WPKH nor P2TR"
                    )));
                };
                native.input[input_index].witness = witness;
            }

            let id = BitcoinTransactionId(native.compute_txid().to_byte_array());
            Ok(BitcoinSignedTransaction {
                id,
                consensus_bytes: consensus::serialize(&native),
            })
        })
    }
}

fn build_transaction(
    network: BitcoinNetwork,
    mut request: BitcoinBuildRequest,
) -> Result<UnsignedBitcoinTransaction, ChainError> {
    if request.available.is_empty() {
        return Err(insufficient_funds(
            "Bitcoin transfer has no available UTXOs",
        ));
    }
    if request.recipients.is_empty() {
        return Err(invalid_transaction("Bitcoin transfer has no recipients"));
    }
    if request.fee_rate.units_per_weight == 0 {
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

    request.available.sort_by(|left, right| {
        right
            .value
            .cmp(&left.value)
            .then_with(|| left.transaction_id.cmp(&right.transaction_id))
            .then_with(|| left.output_index.cmp(&right.output_index))
    });

    if request.drain_wallet {
        if request.recipients.len() != 1 {
            return Err(invalid_transaction(
                "Bitcoin drain transfer requires exactly one recipient",
            ));
        }
        let selected_total = sum_utxos(&request.available)?;
        let fee = predicted_fee(
            &request.available,
            &recipient_scripts,
            request.fee_rate.units_per_weight,
        )?;
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
        return Ok(unsigned(
            request.signing_operation_id,
            request.available,
            request.recipients,
        ));
    }

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

        let fee_without_change = predicted_fee(
            &selected,
            &recipient_scripts,
            request.fee_rate.units_per_weight,
        )?;
        let required = recipient_total
            .checked_add(fee_without_change)
            .ok_or_else(|| invalid_transaction("Bitcoin amount and fee overflowed u64"))?;
        if selected_total < required {
            continue;
        }

        let mut with_change_scripts = recipient_scripts.clone();
        with_change_scripts.push(change_script.clone());
        let fee_with_change = predicted_fee(
            &selected,
            &with_change_scripts,
            request.fee_rate.units_per_weight,
        )?;
        let change = selected_total
            .checked_sub(recipient_total)
            .and_then(|value| value.checked_sub(fee_with_change));
        let mut outputs = request.recipients.clone();
        if let Some(change) =
            change.filter(|value| *value >= change_script.minimal_non_dust().to_sat())
        {
            outputs.push(BitcoinOutput {
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
    Ok(unsigned(request.signing_operation_id, selected, outputs))
}

fn unsigned(
    signing_operation_id: OperationId,
    utxos: Vec<BitcoinUtxo>,
    outputs: Vec<BitcoinOutput>,
) -> UnsignedBitcoinTransaction {
    UnsignedBitcoinTransaction {
        signing_operation_id,
        version: 2,
        lock_time: 0,
        inputs: utxos
            .into_iter()
            .map(|utxo| BitcoinInput {
                utxo,
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME.to_consensus_u32(),
            })
            .collect(),
        outputs,
        sighash_type: SighashType::All,
    }
}

fn predicted_fee(
    inputs: &[BitcoinUtxo],
    output_scripts: &[ScriptBuf],
    units_per_weight: u64,
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
    weight
        .checked_mul(units_per_weight)
        .ok_or_else(|| invalid_transaction("Bitcoin transaction fee overflowed u64"))
}

fn native_transaction(
    network: BitcoinNetwork,
    transaction: &UnsignedBitcoinTransaction,
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

impl BitcoinInputSigningContext<'_> {
    async fn sign_p2wpkh_input(
        &self,
        operation_id: OperationId,
        input_index: usize,
        input: &BitcoinInput,
    ) -> Result<Witness, ChainError> {
        let script = &self.prevouts[input_index].script_pubkey;
        let public_key = self
            .signer
            .public_key(
                &input.utxo.key,
                Curve::Secp256k1,
                PublicKeyFormat::Compressed,
            )
            .await
            .map_err(signer_error)?;
        let public_key = CompressedPublicKey::from_slice(&public_key.bytes).map_err(|error| {
            signer_error_message(format!("invalid compressed Bitcoin public key: {error}"))
        })?;
        if Address::p2wpkh(&public_key, native_network(self.network)).script_pubkey() != *script {
            return Err(signer_error_message(format!(
                "Bitcoin input {input_index} does not belong to its signing key"
            )));
        }
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
        let signature = self
            .signer
            .sign(SignRequest {
                operation_id,
                key: input.utxo.key.clone(),
                payload: SignablePayload::Digest(Digest {
                    bytes: sighash.to_byte_array().to_vec(),
                }),
                scheme: SignatureScheme::EcdsaSecp256k1,
                encoding: SignatureEncoding::Der,
                key_tweak: None,
                user_interaction: UserInteraction::Allowed,
            })
            .await
            .map_err(signer_error)?;
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

    async fn sign_p2tr_input(
        &self,
        operation_id: OperationId,
        input_index: usize,
        input: &BitcoinInput,
    ) -> Result<Witness, ChainError> {
        let public_key = self
            .signer
            .public_key(&input.utxo.key, Curve::Secp256k1, PublicKeyFormat::XOnly)
            .await
            .map_err(signer_error)?;
        let public_key = XOnlyPublicKey::from_slice(&public_key.bytes).map_err(|error| {
            signer_error_message(format!("invalid x-only Bitcoin public key: {error}"))
        })?;
        let secp = bitcoin::secp256k1::Secp256k1::verification_only();
        let expected =
            Address::p2tr(&secp, public_key, None, native_network(self.network)).script_pubkey();
        if expected != self.prevouts[input_index].script_pubkey {
            return Err(signer_error_message(format!(
                "Bitcoin Taproot input {input_index} does not belong to its signing key"
            )));
        }
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
        let tweak = TapTweakHash::from_key_and_tweak(public_key, None).to_byte_array();
        let signature = self
            .signer
            .sign(SignRequest {
                operation_id,
                key: input.utxo.key.clone(),
                payload: SignablePayload::Digest(Digest {
                    bytes: sighash.to_byte_array().to_vec(),
                }),
                scheme: SignatureScheme::SchnorrSecp256k1,
                encoding: SignatureEncoding::Raw,
                key_tweak: Some(KeyTweak::Secp256k1Add(tweak)),
                user_interaction: UserInteraction::Allowed,
            })
            .await
            .map_err(signer_error)?;
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
        let signature = bitcoin::taproot::Signature {
            signature,
            sighash_type,
        };
        Ok(Witness::from_slice(&[signature.to_vec()]))
    }
}

fn checked_output(
    network: BitcoinNetwork,
    output: &BitcoinOutput,
    allow_zero: bool,
) -> Result<ScriptBuf, ChainError> {
    let address = checked_address(network, &output.address)?;
    let minimum = address.script_pubkey().minimal_non_dust().to_sat();
    if !allow_zero && output.value.0 < minimum {
        return Err(invalid_transaction(format!(
            "Bitcoin recipient output is dust: minimum is {minimum} satoshis"
        )));
    }
    Ok(address.script_pubkey())
}

fn checked_address(
    network: BitcoinNetwork,
    address: &BitcoinAddress,
) -> Result<Address, ChainError> {
    address
        .0
        .parse::<Address<NetworkUnchecked>>()
        .map_err(|error| ChainError {
            kind: ChainErrorKind::InvalidAddress,
            message: format!("invalid Bitcoin address: {error}"),
        })?
        .require_network(native_network(network))
        .map_err(|error| ChainError {
            kind: ChainErrorKind::InvalidAddress,
            message: format!("Bitcoin address is for the wrong network: {error}"),
        })
}

fn validate_unique_utxos(utxos: &[BitcoinUtxo]) -> Result<(), ChainError> {
    let mut seen = BTreeSet::new();
    for utxo in utxos {
        if !seen.insert((utxo.transaction_id, utxo.output_index)) {
            return Err(invalid_transaction(
                "Bitcoin transfer contains a duplicate UTXO",
            ));
        }
    }
    Ok(())
}

fn sum_utxos(utxos: &[BitcoinUtxo]) -> Result<u64, ChainError> {
    utxos.iter().try_fold(0_u64, |total, utxo| {
        total
            .checked_add(utxo.value.0)
            .ok_or_else(|| invalid_transaction("Bitcoin selected input amount overflowed u64"))
    })
}

fn ecdsa_sighash_type(sighash_type: SighashType) -> Result<EcdsaSighashType, ChainError> {
    match sighash_type {
        SighashType::All => Ok(EcdsaSighashType::All),
        SighashType::None => Ok(EcdsaSighashType::None),
        SighashType::Single => Ok(EcdsaSighashType::Single),
        SighashType::AllAnyoneCanPay => Ok(EcdsaSighashType::AllPlusAnyoneCanPay),
        SighashType::NoneAnyoneCanPay => Ok(EcdsaSighashType::NonePlusAnyoneCanPay),
        SighashType::SingleAnyoneCanPay => Ok(EcdsaSighashType::SinglePlusAnyoneCanPay),
        SighashType::TaprootDefault => Err(invalid_transaction(
            "Taproot default sighash cannot sign a P2WPKH input",
        )),
    }
}

fn taproot_sighash_type(sighash_type: SighashType) -> Result<TapSighashType, ChainError> {
    match sighash_type {
        SighashType::All => Ok(TapSighashType::All),
        SighashType::None => Ok(TapSighashType::None),
        SighashType::Single => Ok(TapSighashType::Single),
        SighashType::AllAnyoneCanPay => Ok(TapSighashType::AllPlusAnyoneCanPay),
        SighashType::NoneAnyoneCanPay => Ok(TapSighashType::NonePlusAnyoneCanPay),
        SighashType::SingleAnyoneCanPay => Ok(TapSighashType::SinglePlusAnyoneCanPay),
        SighashType::TaprootDefault => Ok(TapSighashType::Default),
    }
}

pub(crate) const fn native_network(network: BitcoinNetwork) -> Network {
    match network {
        BitcoinNetwork::Mainnet => Network::Bitcoin,
        BitcoinNetwork::Testnet3 => Network::Testnet,
        BitcoinNetwork::Testnet4 => Network::Testnet4,
        BitcoinNetwork::Signet => Network::Signet,
        BitcoinNetwork::Regtest => Network::Regtest,
    }
}

fn invalid_transaction(message: impl Into<String>) -> ChainError {
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

fn signer_error(error: signer::SignerError) -> ChainError {
    signer_error_message(format!("Bitcoin signing failed: {error}"))
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
    use crate::{BitcoinAddressGenerator, BitcoinAddressKind, BitcoinGenerateAddress};
    use chain_contract::DepositAddressGenerator;
    use futures_executor::block_on;
    use signer_local::LocalSigner;
    use transaction_utxo::FeeRate;

    fn operation(value: impl Into<String>) -> OperationId {
        OperationId::new(value).expect("test operation ID must be valid")
    }

    fn generated_address(
        signer: &LocalSigner,
        kind: BitcoinAddressKind,
        purpose: &str,
    ) -> chain_contract::GeneratedAddress<BitcoinAddress> {
        block_on(BitcoinAddressGenerator.generate_address(
            BitcoinGenerateAddress::new(
                BitcoinNetwork::Regtest,
                kind,
                operation(format!("provision-{purpose}")),
                purpose,
            ),
            signer,
        ))
        .expect("test Bitcoin address should be generated")
    }

    fn build_and_sign(
        kind: BitcoinAddressKind,
    ) -> (UnsignedBitcoinTransaction, BitcoinSignedTransaction) {
        let signer = LocalSigner::ephemeral_for_testing();
        let source = generated_address(&signer, kind, "source");
        let recipient = generated_address(&signer, BitcoinAddressKind::SegwitV0, "recipient");
        let source_script = checked_address(BitcoinNetwork::Regtest, &source.address)
            .expect("source address should parse")
            .script_pubkey();
        let satisfaction_weight = match kind {
            BitcoinAddressKind::SegwitV0 => 109,
            BitcoinAddressKind::Taproot => 67,
        };
        let codec = BitcoinTransactionCodec::new(BitcoinNetwork::Regtest);
        let unsigned = codec
            .build(BitcoinBuildRequest {
                signing_operation_id: operation("sign-bitcoin-transfer"),
                available: vec![BitcoinUtxo {
                    transaction_id: [11; 32],
                    output_index: 1,
                    value: Satoshi(50_000),
                    script_pubkey: source_script.into_bytes(),
                    satisfaction_weight,
                    key: source.key,
                }],
                recipients: vec![BitcoinOutput {
                    address: recipient.address,
                    value: Satoshi(20_000),
                }],
                change_address: source.address,
                fee_rate: FeeRate {
                    units_per_weight: 1,
                },
                drain_wallet: false,
            })
            .expect("Bitcoin transfer should build");
        let signed =
            block_on(codec.sign(unsigned.clone(), &signer)).expect("Bitcoin transfer should sign");
        (unsigned, signed)
    }

    #[test]
    fn builds_and_signs_native_segwit_transfer() {
        let (unsigned, signed) = build_and_sign(BitcoinAddressKind::SegwitV0);
        let native: Transaction = consensus::deserialize(&signed.consensus_bytes)
            .expect("signed Bitcoin transaction should decode");

        assert_eq!(native.input.len(), 1);
        assert_eq!(native.input[0].witness.len(), 2);
        assert_eq!(native.output.len(), 2);
        assert_eq!(signed.id.0, native.compute_txid().to_byte_array());
        assert_eq!(unsigned.sighash_type, SighashType::All);
    }

    #[test]
    fn builds_and_signs_taproot_key_path_transfer() {
        let (_, signed) = build_and_sign(BitcoinAddressKind::Taproot);
        let native: Transaction = consensus::deserialize(&signed.consensus_bytes)
            .expect("signed Taproot transaction should decode");

        assert_eq!(native.input.len(), 1);
        assert_eq!(native.input[0].witness.len(), 1);
        assert_eq!(native.input[0].witness[0].len(), 65);
        assert_eq!(signed.id.0, native.compute_txid().to_byte_array());
    }

    #[test]
    fn rejects_duplicate_utxos() {
        let signer = LocalSigner::ephemeral_for_testing();
        let source = generated_address(&signer, BitcoinAddressKind::SegwitV0, "source");
        let recipient = generated_address(&signer, BitcoinAddressKind::SegwitV0, "recipient");
        let script = checked_address(BitcoinNetwork::Regtest, &source.address)
            .expect("source address should parse")
            .script_pubkey();
        let utxo = BitcoinUtxo {
            transaction_id: [17; 32],
            output_index: 0,
            value: Satoshi(20_000),
            script_pubkey: script.into_bytes(),
            satisfaction_weight: 109,
            key: source.key,
        };
        let error = BitcoinTransactionCodec::new(BitcoinNetwork::Regtest)
            .build(BitcoinBuildRequest {
                signing_operation_id: operation("sign-duplicate-utxos"),
                available: vec![utxo.clone(), utxo],
                recipients: vec![BitcoinOutput {
                    address: recipient.address,
                    value: Satoshi(10_000),
                }],
                change_address: source.address,
                fee_rate: FeeRate {
                    units_per_weight: 1,
                },
                drain_wallet: false,
            })
            .expect_err("duplicate UTXOs must fail");

        assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);
    }
}
