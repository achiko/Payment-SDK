//! Runtime wallet composition without concrete protocol dependencies.

mod address;
mod amount;
mod collection;
mod collector;
mod error;
mod history_serde;
mod provider;
mod wallet;

pub use address::{AddressEncoding, AddressFormat, AddressText};
pub use amount::AmountFormat;
pub use collection::Wallets;
pub use collector::{Collector, PreparedCollection, PreparedFee, SelectedOutput, Sweeper};
pub use crypto::SecretBytes;
pub use error::{Error, ErrorKind};
pub use provider::Provider;
pub use wallet::{
    Balance, BalanceReader, CollectionFactory, FutureResult, History, HistoryAsset, HistoryEntry,
    HistoryFee, HistoryMovement, HistoryReader, HistoryRequest, HistoryStatus, TransactionFactory,
    Wallet,
};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base::{
        Address, Addresser, Broadcaster, Decimal, TransactionBuilder, TransactionError,
        TransactionRestore, TransactionSnapshot,
    };

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

    impl AmountFormat for NeverWallet {
        fn display_amount(&self, atomic: &Decimal) -> Result<Decimal, Error> {
            Ok(atomic.clone())
        }
    }

    impl TransactionFactory for NeverWallet {
        fn transaction(&self) -> Box<dyn TransactionBuilder> {
            unreachable!("fixture operation must not run")
        }

        fn broadcaster(&self) -> &dyn Broadcaster {
            unreachable!("fixture operation must not run")
        }
    }

    impl CollectionFactory for NeverWallet {}

    impl Sweeper for NeverWallet {}

    impl TransactionRestore for NeverWallet {
        fn restore(
            &self,
            _snapshot: &TransactionSnapshot,
        ) -> Result<Box<dyn TransactionBuilder>, TransactionError> {
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
        let mut wallets = Wallets::new();
        wallets
            .register("fixture", FixtureProvider::Value)
            .expect("fixture key must be unique");

        let wallet =
            futures_executor::block_on(wallets.new_wallet(&"fixture", SecretBytes::new([7; 32])))
                .expect("the fixture provider must create a wallet");

        assert_eq!(wallet.address(), Address::from([1]));
    }

    #[test]
    fn rejects_missing_and_duplicate_providers() {
        let missing = futures_executor::block_on(
            Wallets::<&str>::new().new_wallet(&"fixture", SecretBytes::new([7; 32])),
        )
        .err()
        .expect("missing provider must fail");
        assert_eq!(missing.kind, ErrorKind::Unsupported);

        let mut wallets = Wallets::new();
        wallets
            .register("fixture", FixtureProvider::Value)
            .expect("first provider must register");
        let duplicate = wallets
            .register("fixture", FixtureProvider::Value)
            .expect_err("duplicate provider must fail during startup");
        assert_eq!(duplicate.kind, ErrorKind::Duplicate);
    }
}
