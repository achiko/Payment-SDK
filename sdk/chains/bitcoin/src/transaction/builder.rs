use crate::{ChainError, ChainErrorKind};
use base::{Decimal, DecimalError, TransactionFuture};
use bitcoin::ScriptBuf;

use crate::{Address, Network, Satoshi, TransactionId};

const P2WPKH_SATISFACTION_WEIGHT: u64 = 109;
const P2TR_SATISFACTION_WEIGHT: u64 = 67;

/// A Bitcoin fee rate expressed as satoshis per 1,000 virtual bytes.
///
/// Weight units and virtual bytes are deliberately not interchangeable; fee
/// calculation converts transaction weight to virtual size before applying
/// this rate.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct FeeRate(u64);

impl FeeRate {
    #[must_use]
    pub const fn new(satoshis: u64) -> Self {
        Self(satoshis)
    }

    #[must_use]
    pub const fn satoshis_per_kvb(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpendSource {
    pub transaction_id: [u8; 32],
    pub output_index: u32,
    pub value: Satoshi,
    pub script_pubkey: Vec<u8>,
    pub satisfaction_weight: u64,
}

impl SpendSource {
    /// Accepts one exact PS-reserved/IX-sourced outpoint while deriving all
    /// signing weight from the verified chain-native script.
    pub fn from_exact_selection(
        network: Network,
        address: &Address,
        transaction_id: TransactionId,
        output_index: u32,
        value: Satoshi,
        script_pubkey: Vec<u8>,
    ) -> Result<Self, ChainError> {
        let expected = address.script_pubkey_for_network(network)?;
        let script = ScriptBuf::from_bytes(script_pubkey);
        if script != expected {
            return Err(invalid_selection(
                "Bitcoin selected output script does not match its address",
            ));
        }
        let satisfaction_weight = if script.is_p2wpkh() {
            P2WPKH_SATISFACTION_WEIGHT
        } else if script.is_p2tr() {
            P2TR_SATISFACTION_WEIGHT
        } else {
            return Err(invalid_selection(
                "Bitcoin selected output must be P2WPKH or P2TR",
            ));
        };
        Ok(Self {
            transaction_id: transaction_id.0,
            output_index,
            value,
            script_pubkey: script.into_bytes(),
            satisfaction_weight,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Input {
    pub utxo: SpendSource,
    pub sequence: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Output {
    pub address: Address,
    pub value: Satoshi,
}

impl Output {
    pub fn new(address: Address, value: Decimal) -> Result<Self, DecimalError> {
        Ok(Self {
            address,
            value: Satoshi::from_decimal(&value)?,
        })
    }

    #[must_use]
    pub const fn from_atomic(address: Address, value: Satoshi) -> Self {
        Self { address, value }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BuildRequest {
    pub available: Vec<SpendSource>,
    pub recipients: Vec<Output>,
    pub change_address: Address,
    pub fee_rate: FeeRate,
    pub drain_wallet: bool,
}

#[derive(Clone)]
pub(crate) struct Funding {
    pub available: Vec<SpendSource>,
    pub recipients: Vec<Output>,
    pub change_address: Address,
}

pub(crate) struct BatchBuilder {
    network: Network,
    groups: Vec<Funding>,
    fee_rate: FeeRate,
}

impl BatchBuilder {
    pub(crate) const fn new(network: Network, groups: Vec<Funding>, fee_rate: FeeRate) -> Self {
        Self {
            network,
            groups,
            fee_rate,
        }
    }

    pub(crate) fn sign_each<'a, S: base::Signer + ?Sized>(
        &'a self,
        signers: &'a [&'a S],
    ) -> TransactionFuture<'a, Result<super::SignedTransaction, ChainError>> {
        Box::pin(async move {
            let unsigned =
                super::operations::build_grouped(self.network, self.groups.clone(), self.fee_rate)?;
            super::operations::sign_each(self.network, unsigned, signers).await
        })
    }
}

/// Fully specified Bitcoin transaction construction ready for signing.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Builder {
    network: Network,
    request: BuildRequest,
}

impl Builder {
    #[must_use]
    pub const fn new(network: Network, request: BuildRequest) -> Self {
        Self { network, request }
    }

    pub fn build<'a>(
        &'a self,
    ) -> TransactionFuture<'a, Result<super::UnsignedTransaction, ChainError>> {
        Box::pin(async move { super::operations::build(self.network, self.request.clone()) })
    }

    pub fn sign<'a>(
        &'a self,
        signer: &'a dyn base::Signer,
    ) -> TransactionFuture<'a, Result<super::SignedTransaction, ChainError>> {
        Box::pin(async move {
            let unsigned = super::operations::build(self.network, self.request.clone())?;
            super::operations::sign(self.network, unsigned, signer).await
        })
    }

    /// Signs each input with the corresponding owner in transaction-input order.
    ///
    /// This is the chain-native primitive for a single transaction funded by
    /// several independently owned UTXOs. Application code decides which
    /// wallets participate; Bitcoin keeps input ordering and sighash rules.
    pub fn sign_each<'a, S: base::Signer + ?Sized>(
        &'a self,
        signers: &'a [&'a S],
    ) -> TransactionFuture<'a, Result<super::SignedTransaction, ChainError>> {
        Box::pin(async move {
            let unsigned = super::operations::build(self.network, self.request.clone())?;
            super::operations::sign_each(self.network, unsigned, signers).await
        })
    }
}

fn invalid_selection(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::InvalidTransaction,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{Address as NativeAddress, CompressedPublicKey, PublicKey, secp256k1::Secp256k1};

    use super::*;

    fn address_and_script() -> (Address, Vec<u8>) {
        let public_key = PublicKey::from_slice(&[
            0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test public key must parse");
        let address = NativeAddress::p2wpkh(
            &CompressedPublicKey::try_from(public_key).expect("test public key must be compressed"),
            bitcoin::Network::Regtest,
        );
        (
            Address::from_encoded(address.to_string()),
            address.script_pubkey().into_bytes(),
        )
    }

    fn seeded_address(seed: u8) -> (Address, Vec<u8>) {
        let secp = Secp256k1::new();
        let secret = bitcoin::secp256k1::SecretKey::from_slice(&[seed; 32])
            .expect("fixture secret must be valid");
        let public = bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &secret);
        let public = CompressedPublicKey::try_from(PublicKey::new(public))
            .expect("fixture public key must be compressed");
        let address = NativeAddress::p2wpkh(&public, bitcoin::Network::Regtest);
        (
            Address::from_encoded(address.to_string()),
            address.script_pubkey().into_bytes(),
        )
    }

    #[test]
    fn exact_selection_derives_weight_from_verified_script() {
        let (address, script) = address_and_script();

        let utxo = SpendSource::from_exact_selection(
            Network::Regtest,
            &address,
            TransactionId([7; 32]),
            2,
            Satoshi(42_000),
            script,
        )
        .expect("matching P2WPKH selection must be accepted");

        assert_eq!(utxo.satisfaction_weight, P2WPKH_SATISFACTION_WEIGHT);
    }

    #[test]
    fn exact_selection_rejects_client_script_mismatch() {
        let (address, _) = address_and_script();

        let error = SpendSource::from_exact_selection(
            Network::Regtest,
            &address,
            TransactionId([7; 32]),
            2,
            Satoshi(42_000),
            vec![0x51],
        )
        .expect_err("mismatched selection script must fail");

        assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);
    }

    #[test]
    fn grouped_funding_preserves_each_sources_change() {
        let (alice, alice_script) = seeded_address(1);
        let (bob, bob_script) = seeded_address(2);
        let (recipient, _) = seeded_address(3);
        let groups = vec![
            Funding {
                available: vec![
                    SpendSource::from_exact_selection(
                        Network::Regtest,
                        &alice,
                        TransactionId([1; 32]),
                        0,
                        Satoshi(100_000),
                        alice_script,
                    )
                    .expect("Alice input must be valid"),
                ],
                recipients: vec![Output::from_atomic(recipient.clone(), Satoshi(40_000))],
                change_address: alice.clone(),
            },
            Funding {
                available: vec![
                    SpendSource::from_exact_selection(
                        Network::Regtest,
                        &bob,
                        TransactionId([2; 32]),
                        0,
                        Satoshi(100_000),
                        bob_script,
                    )
                    .expect("Bob input must be valid"),
                ],
                recipients: vec![Output::from_atomic(recipient, Satoshi(30_000))],
                change_address: bob.clone(),
            },
        ];

        let transaction = crate::transaction::operations::build_grouped(
            Network::Regtest,
            groups,
            FeeRate::new(1_000),
        )
        .expect("both sources must fund one transaction");

        assert_eq!(transaction.inputs.len(), 2);
        assert_eq!(transaction.outputs.len(), 4);
        assert!(
            transaction
                .outputs
                .iter()
                .any(|output| output.address == alice)
        );
        assert!(
            transaction
                .outputs
                .iter()
                .any(|output| output.address == bob)
        );
    }

    #[test]
    fn grouped_funding_does_not_cross_subsidize_sources() {
        let (alice, alice_script) = seeded_address(1);
        let (bob, bob_script) = seeded_address(2);
        let (recipient, _) = seeded_address(3);
        let groups = vec![
            Funding {
                available: vec![
                    SpendSource::from_exact_selection(
                        Network::Regtest,
                        &alice,
                        TransactionId([1; 32]),
                        0,
                        Satoshi(10_000),
                        alice_script,
                    )
                    .expect("Alice input must be valid"),
                ],
                recipients: vec![Output::from_atomic(recipient.clone(), Satoshi(20_000))],
                change_address: alice,
            },
            Funding {
                available: vec![
                    SpendSource::from_exact_selection(
                        Network::Regtest,
                        &bob,
                        TransactionId([2; 32]),
                        0,
                        Satoshi(1_000_000),
                        bob_script,
                    )
                    .expect("Bob input must be valid"),
                ],
                recipients: vec![Output::from_atomic(recipient, Satoshi(1_000))],
                change_address: bob,
            },
        ];

        let error = crate::transaction::operations::build_grouped(
            Network::Regtest,
            groups,
            FeeRate::new(1_000),
        )
        .expect_err("Bob's funds must not pay Alice's requested output");

        assert_eq!(error.kind, ChainErrorKind::InsufficientFunds);
    }
}
