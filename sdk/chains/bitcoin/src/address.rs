use std::{fmt, str::FromStr};

use base::{Address as BaseAddress, AddressError, AddressErrorKind, AddressValidator, Addresser};
use bitcoin::{Address as NativeAddress, Script, ScriptBuf, address::NetworkUnchecked};

use crate::{ChainError, ChainErrorKind, Network};

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Address(pub BaseAddress);

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.encoded())
    }
}

impl FromStr for Address {
    type Err = bitcoin::address::ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        input
            .parse::<NativeAddress<NetworkUnchecked>>()
            .map(|address| Self::from_encoded(address.assume_checked().to_string()))
    }
}

impl Address {
    #[must_use]
    pub fn from_encoded(value: impl AsRef<str>) -> Self {
        Self(BaseAddress::new(value.as_ref().as_bytes()))
    }

    #[must_use]
    pub fn encoded(&self) -> &str {
        std::str::from_utf8(self.0.as_bytes())
            .expect("concrete address constructors store UTF-8 encoded text")
    }

    /// Parses, network-checks, and returns Bitcoin's canonical address display.
    pub fn parse_for_network(value: &str, network: Network) -> Result<Self, ChainError> {
        let address = value
            .parse::<NativeAddress<NetworkUnchecked>>()
            .map_err(|error| ChainError {
                kind: ChainErrorKind::InvalidAddress,
                message: format!("invalid Bitcoin address: {error}"),
            })?
            .require_network(network.native())
            .map_err(|error| ChainError {
                kind: ChainErrorKind::InvalidAddress,
                message: format!("Bitcoin address is for the wrong network: {error}"),
            })?;
        Ok(Self::from_encoded(address.to_string()))
    }

    /// Returns a network-encoded address when the output script has one.
    pub(crate) fn from_script_for_network(script: &Script, network: Network) -> Option<Self> {
        NativeAddress::from_script(script, network.native())
            .ok()
            .map(|address| Self::from_encoded(address.to_string()))
    }

    /// Returns the script for a canonical, network-checked address.
    pub fn script_pubkey_for_network(&self, network: Network) -> Result<ScriptBuf, ChainError> {
        Self::parse_for_network(self.encoded(), network).and_then(|canonical| {
            canonical
                .encoded()
                .parse::<NativeAddress<NetworkUnchecked>>()
                .map_err(|error| ChainError {
                    kind: ChainErrorKind::InvalidAddress,
                    message: format!("invalid canonical Bitcoin address: {error}"),
                })?
                .require_network(network.native())
                .map(|address| address.script_pubkey())
                .map_err(|error| ChainError {
                    kind: ChainErrorKind::InvalidAddress,
                    message: format!("Bitcoin address is for the wrong network: {error}"),
                })
        })
    }
}

impl Addresser for Address {
    fn address(&self) -> BaseAddress {
        self.0.clone()
    }
}

impl AddressValidator for Address {
    fn validate(&self) -> Result<(), AddressError> {
        let encoded = std::str::from_utf8(self.0.as_bytes()).map_err(|_| {
            AddressError::new(
                AddressErrorKind::InvalidEncoding,
                "address is not valid UTF-8",
            )
        })?;
        encoded
            .parse::<NativeAddress<NetworkUnchecked>>()
            .map(|_| ())
            .map_err(|_| {
                AddressError::new(
                    AddressErrorKind::InvalidFormat,
                    "address does not use a recognized encoding",
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{CompressedPublicKey, PublicKey};

    fn address(network: Network) -> String {
        let public_key = PublicKey::from_slice(&[
            0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test public key must parse");
        NativeAddress::p2wpkh(
            &CompressedPublicKey::try_from(public_key).expect("test public key must be compressed"),
            network.native(),
        )
        .to_string()
    }

    #[test]
    fn parser_checks_network_and_returns_canonical_display() {
        let regtest = address(Network::Regtest);

        let parsed = Address::parse_for_network(&regtest, Network::Regtest)
            .expect("regtest address must parse");

        assert_eq!(parsed.encoded(), regtest);
        assert!(Address::parse_for_network(&regtest, Network::Mainnet).is_err());
        assert!(
            parsed
                .script_pubkey_for_network(Network::Regtest)
                .expect("network-checked address must produce a script")
                .is_p2wpkh()
        );
    }

    #[test]
    fn from_str_parses_without_imposing_a_network() {
        let mainnet = address(Network::Mainnet);
        let regtest = address(Network::Regtest);

        assert_eq!(
            mainnet
                .parse::<Address>()
                .expect("valid mainnet address must parse")
                .to_string(),
            mainnet
        );
        assert_eq!(
            regtest
                .parse::<Address>()
                .expect("valid regtest address must parse")
                .to_string(),
            regtest
        );
    }

    #[test]
    fn from_str_rejects_unrecognized_address_encoding() {
        assert!("not-a-bitcoin-address".parse::<Address>().is_err());
    }

    #[test]
    fn script_constructor_preserves_legacy_address_encodings() {
        for (script, expected) in [
            (
                "76a914162c5ea71c0b23f5b9022ef047c4a86470a5b07088ac",
                "132F25rTsvBdp9JzLLBHP5mvGY66i1xdiM",
            ),
            (
                "a914162c5ea71c0b23f5b9022ef047c4a86470a5b07087",
                "33iFwdLuRpW1uK1RTRqsoi8rR4NpDzk66k",
            ),
        ] {
            let script = ScriptBuf::from_hex(script).expect("fixture script must decode");
            let converted = Address::from_script_for_network(&script, Network::Mainnet)
                .expect("legacy payment script must have an address");

            assert_eq!(converted.encoded(), expected);
        }
    }

    #[test]
    fn script_constructor_uses_requested_witness_network() {
        let script = Address::parse_for_network(&address(Network::Mainnet), Network::Mainnet)
            .expect("fixture address must parse")
            .script_pubkey_for_network(Network::Mainnet)
            .expect("fixture address must have a script");

        for network in [
            Network::Mainnet,
            Network::Testnet3,
            Network::Testnet4,
            Network::Signet,
            Network::Regtest,
        ] {
            let converted = Address::from_script_for_network(&script, network)
                .expect("witness payment script must have an address");

            assert_eq!(converted.encoded(), address(network));
            assert_eq!(
                converted
                    .script_pubkey_for_network(network)
                    .expect("converted address must retain its script"),
                script
            );
        }
    }

    #[test]
    fn script_constructor_preserves_future_witness_programs() {
        // Witness version 13 with a 40-byte program has an address even though
        // the SDK has no corresponding spending implementation.
        let script = ScriptBuf::from_bytes([vec![0x5d, 0x28], vec![0x11; 40]].concat());
        let converted = Address::from_script_for_network(&script, Network::Regtest)
            .expect("future witness program must retain its address");

        assert!(converted.encoded().starts_with("bcrt1"));
        assert_eq!(
            converted
                .script_pubkey_for_network(Network::Regtest)
                .expect("future witness address must round-trip"),
            script
        );
    }

    #[test]
    fn script_constructor_returns_none_when_no_address_exists() {
        for bytes in [
            vec![],
            vec![0x6a, 0x01, 0x01],
            vec![0x00, 0x01, 0x2a],
            vec![0x51],
        ] {
            let script = ScriptBuf::from_bytes(bytes);

            assert_eq!(
                Address::from_script_for_network(&script, Network::Regtest),
                None
            );
        }
    }
}
