use indexing::{ChainId, IndexError, IndexErrorKind, IndexScope};

/// Scope-bound owner for Solana block interpretation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interpreter {
    scope: IndexScope,
}

impl Interpreter {
    pub fn new(scope: IndexScope) -> Result<Self, IndexError> {
        if scope.chain != ChainId(crate::CHAIN.to_owned()) {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Solana interpreter scope must use the solana chain ID",
                false,
            ));
        }
        if scope.network.trim().is_empty() {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Solana interpreter network slug must not be empty",
                false,
            ));
        }
        Ok(Self { scope })
    }

    #[must_use]
    pub fn scope(&self) -> &IndexScope {
        &self.scope
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_one_non_empty_solana_scope() {
        let scope = IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: "localnet".to_owned(),
        };
        assert_eq!(Interpreter::new(scope.clone()).unwrap().scope(), &scope);

        let wrong_chain = IndexScope {
            chain: ChainId("ethereum".to_owned()),
            network: "localnet".to_owned(),
        };
        assert!(Interpreter::new(wrong_chain).is_err());

        let empty_network = IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: String::new(),
        };
        assert!(Interpreter::new(empty_network).is_err());
    }
}
