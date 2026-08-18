use std::fmt;

/// Coarse environment classification shared by every supported protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NetworkKind {
    Mainnet,
    Testnet,
}

/// A protocol-neutral network identifier with chain-owned storage.
///
/// `R` is the native identifier representation: for example a string, an
/// integer chain ID, or a genesis hash. Generic consumers may inspect the
/// environment kind without knowing that representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NetworkId<R = &'static str> {
    value: R,
    kind: NetworkKind,
}

impl<R> NetworkId<R> {
    #[must_use]
    pub const fn new(value: R, kind: NetworkKind) -> Self {
        Self { value, kind }
    }

    #[must_use]
    pub const fn value(&self) -> &R {
        &self.value
    }

    #[must_use]
    pub fn into_value(self) -> R {
        self.value
    }

    #[must_use]
    pub const fn kind(&self) -> NetworkKind {
        self.kind
    }

    #[must_use]
    pub const fn is_mainnet(&self) -> bool {
        matches!(self.kind, NetworkKind::Mainnet)
    }

    #[must_use]
    pub const fn is_testnet(&self) -> bool {
        matches!(self.kind, NetworkKind::Testnet)
    }
}

impl<R: fmt::Display> fmt::Display for NetworkId<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt(formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_storage_is_generic_while_kind_is_common() {
        let named = NetworkId::new("mainnet", NetworkKind::Mainnet);
        let numeric = NetworkId::new(1_u64, NetworkKind::Mainnet);

        assert_eq!(named.value(), &"mainnet");
        assert_eq!(numeric.value(), &1);
        assert!(named.is_mainnet());
        assert!(!numeric.is_testnet());
    }
}
