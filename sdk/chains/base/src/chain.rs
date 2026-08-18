use std::fmt;

use crate::NetworkId;

/// Common metadata describing one concrete blockchain network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chain<R = &'static str> {
    pub network_id: NetworkId<R>,
    pub name: &'static str,
    pub ticker: &'static str,
}

impl<R> Chain<R> {
    #[must_use]
    pub const fn new(network_id: NetworkId<R>, name: &'static str, ticker: &'static str) -> Self {
        Self {
            network_id,
            name,
            ticker,
        }
    }
}

impl<R: fmt::Display> fmt::Display for Chain<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.name, self.network_id)
    }
}

/// Static string map of named test networks.
#[derive(Clone, Copy, Debug)]
pub struct TestnetMap<R: 'static = &'static str>(&'static [(&'static str, Chain<R>)]);

impl<R: 'static> TestnetMap<R> {
    #[must_use]
    pub const fn new(networks: &'static [(&'static str, Chain<R>)]) -> Self {
        Self(networks)
    }

    #[must_use]
    pub const fn as_slice(self) -> &'static [(&'static str, Chain<R>)] {
        self.0
    }

    #[must_use]
    pub fn get(self, name: &str) -> Option<&'static Chain<R>> {
        self.0
            .iter()
            .find_map(|(key, chain)| key.eq_ignore_ascii_case(name).then_some(chain))
    }
}

/// Direct access to one mainnet and its named test networks.
#[derive(Clone, Copy, Debug)]
pub struct ChainCollection<R: 'static = &'static str> {
    pub mainnet: Chain<R>,
    pub testnet: TestnetMap<R>,
}

impl<R: 'static> ChainCollection<R> {
    #[must_use]
    pub const fn new(mainnet: Chain<R>, testnet: &'static [(&'static str, Chain<R>)]) -> Self {
        Self {
            mainnet,
            testnet: TestnetMap::new(testnet),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetworkKind;

    const MAINNET: Chain = Chain::new(NetworkId::new("one", NetworkKind::Mainnet), "one", "ONE");
    const TESTNET: &[(&str, Chain)] = &[(
        "local",
        Chain::new(
            NetworkId::new("one-test", NetworkKind::Testnet),
            "one",
            "ONE",
        ),
    )];

    #[test]
    fn collection_exposes_mainnet_and_named_testnets_directly() {
        let chains = ChainCollection::new(MAINNET, TESTNET);
        assert_eq!(chains.mainnet, MAINNET);
        assert_eq!(chains.testnet.get("LOCAL"), Some(&TESTNET[0].1));
        assert_eq!(chains.testnet.get("missing"), None);
    }
}
