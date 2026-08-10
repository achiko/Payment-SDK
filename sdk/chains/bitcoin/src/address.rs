use bitcoin::{Address, ScriptBuf, address::NetworkUnchecked};
use chain_contract::{ChainError, ChainErrorKind};

use crate::BitcoinNetwork;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BitcoinAddress(pub String);

impl BitcoinAddress {
    /// Parses, network-checks, and returns Bitcoin's canonical address display.
    pub fn parse_for_network(value: &str, network: BitcoinNetwork) -> Result<Self, ChainError> {
        let address = value
            .parse::<Address<NetworkUnchecked>>()
            .map_err(|error| ChainError {
                kind: ChainErrorKind::InvalidAddress,
                message: format!("invalid Bitcoin address: {error}"),
            })?
            .require_network(network.native())
            .map_err(|error| ChainError {
                kind: ChainErrorKind::InvalidAddress,
                message: format!("Bitcoin address is for the wrong network: {error}"),
            })?;
        Ok(Self(address.to_string()))
    }

    /// Returns the script for a canonical, network-checked address.
    pub fn script_pubkey_for_network(
        &self,
        network: BitcoinNetwork,
    ) -> Result<ScriptBuf, ChainError> {
        Self::parse_for_network(&self.0, network).and_then(|canonical| {
            canonical
                .0
                .parse::<Address<NetworkUnchecked>>()
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

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{CompressedPublicKey, PublicKey};

    fn address(network: BitcoinNetwork) -> String {
        let public_key = PublicKey::from_slice(&[
            0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test public key must parse");
        Address::p2wpkh(
            &CompressedPublicKey::try_from(public_key).expect("test public key must be compressed"),
            network.native(),
        )
        .to_string()
    }

    #[test]
    fn parser_checks_network_and_returns_canonical_display() {
        let regtest = address(BitcoinNetwork::Regtest);

        let parsed = BitcoinAddress::parse_for_network(&regtest, BitcoinNetwork::Regtest)
            .expect("regtest address must parse");

        assert_eq!(parsed.0, regtest);
        assert!(BitcoinAddress::parse_for_network(&regtest, BitcoinNetwork::Mainnet).is_err());
        assert!(
            parsed
                .script_pubkey_for_network(BitcoinNetwork::Regtest)
                .expect("network-checked address must produce a script")
                .is_p2wpkh()
        );
    }
}
