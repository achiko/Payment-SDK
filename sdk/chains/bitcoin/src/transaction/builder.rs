use std::cmp::Ordering;

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

    pub(super) fn for_vsize(self, virtual_size: u64) -> Result<u64, ChainError> {
        let numerator = u128::from(self.satoshis_per_kvb())
            .checked_mul(u128::from(virtual_size))
            .and_then(|value| value.checked_add(999))
            .ok_or_else(|| {
                super::operations::invalid_transaction("Bitcoin transaction fee overflowed u128")
            })?;
        u64::try_from(numerator / 1_000).map_err(|_| {
            super::operations::invalid_transaction("Bitcoin transaction fee overflowed u64")
        })
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
    /// Orders only outpoint identity, using displayed transaction IDs before output indices.
    pub(super) fn compare_outpoint(&self, other: &Self) -> Ordering {
        TransactionId(self.transaction_id)
            .to_string()
            .cmp(&TransactionId(other.transaction_id).to_string())
            .then_with(|| self.output_index.cmp(&other.output_index))
    }

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
        Box::pin(async move { self.request.clone().build(self.network) })
    }

    pub fn sign<'a>(
        &'a self,
        signer: &'a dyn base::Signer,
    ) -> TransactionFuture<'a, Result<super::SignedTransaction, ChainError>> {
        Box::pin(async move {
            let unsigned = self.request.clone().build(self.network)?;
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
            let unsigned = self.request.clone().build(self.network)?;
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

    #[test]
    fn fee_rate_applies_exact_and_fractional_virtual_sizes() {
        for (rate, size, expected) in [
            (2_000, 250, 500),
            (1_250, 101, 127),
            (1, 1, 1),
            (1, 1_001, 2),
        ] {
            assert_eq!(
                FeeRate::new(rate)
                    .for_vsize(size)
                    .expect("representable fee"),
                expected
            );
        }
    }

    #[test]
    fn zero_fee_rate_or_virtual_size_produces_zero_fee() {
        for (rate, size) in [(0, 0), (0, u64::MAX), (u64::MAX, 0)] {
            assert_eq!(FeeRate::new(rate).for_vsize(size).expect("zero fee"), 0);
        }
    }

    #[test]
    fn fee_rate_accepts_the_maximum_representable_fee() {
        assert_eq!(
            FeeRate::new(u64::MAX)
                .for_vsize(1_000)
                .expect("maximum fee"),
            u64::MAX
        );
        assert_eq!(
            FeeRate::new(1_000)
                .for_vsize(u64::MAX)
                .expect("maximum size"),
            u64::MAX
        );
    }

    #[test]
    fn fee_rate_rejects_fees_above_u64() {
        for size in [1_001, u64::MAX] {
            let error = FeeRate::new(u64::MAX)
                .for_vsize(size)
                .expect_err("fee exceeds u64");

            assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);
            assert_eq!(error.message, "Bitcoin transaction fee overflowed u64");
        }
    }

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
    fn drain_preserves_displayed_outpoint_order_and_exact_fee() {
        let (address, script) = address_and_script();
        let mut first = [0; 32];
        first[0] = 1;
        let mut last = [0; 32];
        last[31] = 1;
        let source = |transaction_id, output_index| SpendSource {
            transaction_id,
            output_index,
            value: Satoshi(100_000),
            script_pubkey: script.clone(),
            satisfaction_weight: P2WPKH_SATISFACTION_WEIGHT,
        };
        let request = BuildRequest {
            available: vec![source(last, 0), source(first, 10), source(first, 2)],
            recipients: vec![Output::from_atomic(address.clone(), Satoshi(0))],
            change_address: address,
            fee_rate: FeeRate::new(1_000),
            drain_wallet: true,
        };

        let grouped = crate::transaction::operations::build_grouped(
            Network::Regtest,
            vec![Funding {
                available: request.available.clone(),
                recipients: vec![Output::from_atomic(
                    request.change_address.clone(),
                    Satoshi(50_000),
                )],
                change_address: request.change_address.clone(),
            }],
            request.fee_rate,
        )
        .expect("grouped funding must retain canonical outpoint order");
        let transaction =
            futures_executor::block_on(Builder::new(Network::Regtest, request).build())
                .expect("drain must retain every selected outpoint");

        assert_eq!(
            transaction
                .inputs
                .iter()
                .map(|input| (input.utxo.transaction_id, input.utxo.output_index))
                .collect::<Vec<_>>(),
            vec![(first, 2), (first, 10), (last, 0)]
        );
        assert_eq!(grouped.inputs, transaction.inputs);
        assert_eq!(transaction.outputs.len(), 1);
        // Three P2WPKH inputs and one output predict 247 virtual bytes.
        assert_eq!(transaction.outputs[0].value, Satoshi(299_753));
    }

    #[test]
    fn normal_selection_keeps_amount_then_raw_outpoint_order() {
        let (address, script) = address_and_script();
        let mut first = [0; 32];
        first[0] = 1;
        let mut last = [0; 32];
        last[31] = 1;
        let source = |transaction_id, value| SpendSource {
            transaction_id,
            output_index: 0,
            value: Satoshi(value),
            script_pubkey: script.clone(),
            satisfaction_weight: P2WPKH_SATISFACTION_WEIGHT,
        };
        let request = BuildRequest {
            available: vec![
                source(first, 100_000),
                source([0; 32], 40_000),
                source(last, 100_000),
            ],
            recipients: vec![Output::from_atomic(address.clone(), Satoshi(150_000))],
            change_address: address.clone(),
            fee_rate: FeeRate::new(1_000),
            drain_wallet: false,
        };

        let transaction =
            futures_executor::block_on(Builder::new(Network::Regtest, request).build())
                .expect("two largest inputs must fund the transfer");

        assert_eq!(
            transaction
                .inputs
                .iter()
                .map(|input| input.utxo.transaction_id)
                .collect::<Vec<_>>(),
            vec![last, first]
        );
        assert_eq!(transaction.outputs.len(), 2);
        assert_eq!(transaction.outputs[0].value, Satoshi(150_000));
        assert_eq!(transaction.outputs[1].address, address);
        // Two P2WPKH inputs and two outputs predict 209 virtual bytes.
        assert_eq!(transaction.outputs[1].value, Satoshi(49_791));
    }

    #[test]
    fn transaction_construction_rejects_invalid_and_wrong_network_addresses() {
        let (regtest, script) = address_and_script();
        let mainnet = Address::from_script_for_network(
            &ScriptBuf::from_bytes(script.clone()),
            Network::Mainnet,
        )
        .expect("fixture script must encode for mainnet");
        for (recipient, change_address, network, expected_message) in [
            (
                Address::from_encoded("invalid"),
                regtest.clone(),
                Network::Regtest,
                "invalid Bitcoin address:",
            ),
            (
                regtest.clone(),
                Address::from_encoded("invalid"),
                Network::Regtest,
                "invalid Bitcoin address:",
            ),
            (
                regtest.clone(),
                mainnet.clone(),
                Network::Mainnet,
                "Bitcoin address is for the wrong network:",
            ),
            (
                mainnet,
                regtest,
                Network::Mainnet,
                "Bitcoin address is for the wrong network:",
            ),
        ] {
            let request = BuildRequest {
                available: vec![SpendSource {
                    transaction_id: [1; 32],
                    output_index: 0,
                    value: Satoshi(100_000),
                    script_pubkey: script.clone(),
                    satisfaction_weight: P2WPKH_SATISFACTION_WEIGHT,
                }],
                recipients: vec![Output::from_atomic(recipient, Satoshi(50_000))],
                change_address,
                fee_rate: FeeRate::new(1_000),
                drain_wallet: false,
            };
            let error = futures_executor::block_on(Builder::new(network, request).build())
                .expect_err("address must be valid for the configured network");
            assert_eq!(error.kind, ChainErrorKind::InvalidAddress);
            assert!(
                error.message.starts_with(expected_message),
                "{}",
                error.message
            );
        }
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

    #[test]
    fn no_available_inputs_retains_insufficient_funds_before_recipient_validation() {
        let (address, _) = address_and_script();
        let request = BuildRequest {
            available: Vec::new(),
            recipients: Vec::new(),
            change_address: address,
            fee_rate: FeeRate::new(0),
            drain_wallet: false,
        };
        let error = futures_executor::block_on(Builder::new(Network::Regtest, request).build())
            .unwrap_err();
        assert_eq!(error.kind, ChainErrorKind::InsufficientFunds);
        assert_eq!(error.message, "Bitcoin transfer has no available UTXOs");
    }
}
