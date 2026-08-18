//! Runtime wallet composition without concrete protocol dependencies.

mod address;
mod error;
mod history_serde;
mod provider;
mod registries;
mod sender;
mod wallet;

pub use address::{AddressEncoding, AddressFormat, AddressText};
pub use crypto::SecretBytes;
pub use error::{Error, ErrorKind};
pub use provider::Provider;
pub use registries::{Providers, Wallets};
pub use sender::{SendError, SendFuture, Sender, Transfer};
pub use wallet::{
    Balance, BalanceReader, FutureResult, History, HistoryAsset, HistoryEntry, HistoryFee,
    HistoryMovement, HistoryReader, HistoryRequest, HistoryStatus, TransactionFactory, Wallet,
};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base::{Address, Addresser, Broadcaster, TransactionBuilder};

    use super::*;

    enum NeverWallet {
        Value,
    }

    impl Addresser for NeverWallet {
        fn address(&self) -> Address {
            Address::from([1])
        }
    }

    impl base::Signer for NeverWallet {
        fn sign<'a>(&'a self, _request: base::SignRequest) -> base::SignFuture<'a> {
            Box::pin(async { unreachable!("fixture operation must not run") })
        }
    }

    impl AddressFormat for NeverWallet {
        fn address_text(&self, address: &Address) -> Result<AddressText, Error> {
            Ok(AddressText::new(AddressEncoding::Hex, address.to_string()))
        }

        fn parse_address(&self, address: &AddressText) -> Result<Address, Error> {
            Ok(Address::new(address.text.as_bytes()))
        }
    }

    impl BalanceReader for NeverWallet {
        fn balance<'a>(&'a self) -> FutureResult<'a, Balance> {
            Box::pin(async { unreachable!("fixture operation must not run") })
        }
    }

    impl TransactionFactory for NeverWallet {
        fn transaction(&self) -> Box<dyn TransactionBuilder> {
            unreachable!("fixture operation must not run")
        }

        fn restore(
            &self,
            _snapshot: &base::TransactionSnapshot,
        ) -> Result<Box<dyn TransactionBuilder>, base::TransactionError> {
            unreachable!("fixture operation must not run")
        }

        fn broadcaster(&self) -> &dyn Broadcaster {
            unreachable!("fixture operation must not run")
        }
    }

    impl HistoryReader for NeverWallet {
        fn history<'a>(&'a self, _request: HistoryRequest) -> FutureResult<'a, History> {
            Box::pin(async { unreachable!("fixture operation must not run") })
        }
    }

    enum FixtureProvider {
        Value,
    }

    impl Provider for FixtureProvider {
        fn create<'a>(&'a self, _secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>> {
            Box::pin(async { Ok(Arc::new(NeverWallet::Value) as Arc<dyn Wallet>) })
        }
    }

    #[test]
    fn creates_wallet_through_the_only_matching_provider() {
        let mut providers = Providers::new();
        providers
            .register("fixture", FixtureProvider::Value)
            .expect("fixture key must be unique");

        let wallet =
            futures_executor::block_on(providers.create(&"fixture", SecretBytes::new([7; 32])))
                .expect("the fixture provider must create a wallet");

        assert_eq!(wallet.address(), Address::from([1]));
    }

    #[test]
    fn generates_wallet_without_returning_secret_material() {
        let mut providers = Providers::new();
        providers
            .register("fixture", FixtureProvider::Value)
            .expect("fixture key must be unique");

        let wallet = futures_executor::block_on(providers.generate(&"fixture"))
            .expect("OS-backed generation must create a wallet");

        assert_eq!(wallet.address(), Address::from([1]));
    }

    #[test]
    fn rejects_missing_and_duplicate_providers() {
        let missing = futures_executor::block_on(
            Providers::<&str>::new().create(&"fixture", SecretBytes::new([7; 32])),
        )
        .err()
        .expect("missing provider must fail");
        assert_eq!(missing.kind, ErrorKind::Unsupported);

        let mut providers = Providers::new();
        providers
            .register("fixture", FixtureProvider::Value)
            .expect("first provider must register");
        let duplicate = providers
            .register("fixture", FixtureProvider::Value)
            .expect_err("duplicate provider must fail during startup");
        assert_eq!(duplicate.kind, ErrorKind::Duplicate);
    }

    #[test]
    fn stores_created_wallets_separately_from_providers() {
        let wallet = Arc::new(NeverWallet::Value) as Arc<dyn Wallet>;
        let mut wallets = Wallets::new();
        wallets
            .insert("alice", wallet.clone())
            .expect("wallet key must be unique");

        assert_eq!(
            wallets.get(&"alice").expect("wallet must exist").address(),
            wallet.address()
        );
        assert_eq!(
            wallets
                .insert("alice", wallet)
                .expect_err("duplicate wallet must fail")
                .kind,
            ErrorKind::Duplicate
        );
    }

    #[test]
    fn send_error_preserves_the_accepted_prefix() {
        let error = SendError::at(
            2,
            vec![base::Id::new("first"), base::Id::new("second")],
            Error::new(ErrorKind::Transaction, "third transfer failed"),
        );

        assert_eq!(error.failed_index, 2);
        assert_eq!(error.accepted[0].as_str(), "first");
        assert_eq!(error.accepted[1].as_str(), "second");
        assert!(error.to_string().contains("third transfer failed"));
    }
}
