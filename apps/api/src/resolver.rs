use std::{collections::BTreeMap, sync::Arc};

use deposits::{
    AddressRequest, BoxFuture, Deposit, DepositAddressSource, DepositError, DepositErrorKind,
    KeyId, ProvisionedAddress,
};
use indexing::{AssetId, CanonicalAddress, IndexScope};
use wallets::Wallet;

/// One eagerly composed deposit key selected by application configuration.
pub(crate) struct DepositKey {
    pub purpose: String,
    pub wallet_id: String,
    pub scope: IndexScope,
    pub asset: AssetId,
    pub wallet: Arc<dyn Wallet>,
}

struct ResolvedKey {
    purpose: String,
    wallet: Arc<dyn Wallet>,
    scope: IndexScope,
    asset: AssetId,
    address: CanonicalAddress,
    key: KeyId,
}

/// App-owned key-purpose resolver shared by address issuance and collection.
///
/// It stores composed wallets, never private bytes. Durable deposits retain an
/// opaque key ID that resolves to the same wallet after process restart.
pub(crate) struct DepositResolver {
    entries: Vec<ResolvedKey>,
    keys: BTreeMap<String, usize>,
}

pub(crate) struct GasResolver {
    wallet: Arc<dyn Wallet>,
    scope: IndexScope,
}

impl GasResolver {
    pub(crate) fn new(wallet: Arc<dyn Wallet>, scope: IndexScope) -> Self {
        Self { wallet, scope }
    }
}

impl crate::GasWallet for GasResolver {
    fn wallet<'a>(
        &'a self,
        collection: &'a deposits::Collection,
    ) -> wallets::FutureResult<'a, Arc<dyn Wallet>> {
        Box::pin(async move {
            if collection.asset.chain != self.scope.chain
                || collection.destination.scope != self.scope
            {
                return Err(wallets::Error::new(
                    wallets::ErrorKind::Unsupported,
                    "gas wallet does not belong to the collection scope",
                ));
            }
            Ok(self.wallet.clone())
        })
    }
}

impl DepositResolver {
    pub fn new(entries: Vec<DepositKey>) -> Result<Self, DepositError> {
        let mut configured = entries;
        configured.sort_by(|left, right| left.purpose.cmp(&right.purpose));
        let mut entries = Vec::with_capacity(configured.len());
        let mut keys = BTreeMap::new();
        let mut purposes = BTreeMap::new();
        let mut addresses = BTreeMap::new();
        for entry in configured {
            if entry.purpose.trim().is_empty() || entry.wallet_id.trim().is_empty() {
                return Err(invalid(
                    "deposit key purpose and wallet ID must not be empty",
                ));
            }
            if entry.asset.chain != entry.scope.chain {
                return Err(invalid("deposit key asset and scope must share a chain"));
            }
            let text = entry
                .wallet
                .address_text(&entry.wallet.address())
                .map_err(|error| {
                    invalid(format!("deposit address could not be formatted: {error}"))
                })?
                .text;
            let address = CanonicalAddress {
                scope: entry.scope.clone(),
                value: text,
            };
            let key_value = format!("{}:{}", entry.wallet_id, entry.purpose);
            if purposes.contains_key(&entry.purpose) {
                return Err(invalid("deposit key purposes must be unique"));
            }
            if addresses
                .insert(address.clone(), entry.purpose.clone())
                .is_some()
            {
                return Err(invalid("deposit keys must derive unique addresses"));
            }
            let key = KeyId::Identifier(key_value.clone());
            let position = entries.len();
            keys.insert(key_value, position);
            purposes.insert(entry.purpose.clone(), position);
            entries.push(ResolvedKey {
                purpose: entry.purpose,
                wallet: entry.wallet,
                scope: entry.scope,
                asset: entry.asset,
                address,
                key,
            });
        }
        if purposes.is_empty() {
            return Err(invalid("at least one deposit key must be configured"));
        }
        Ok(Self { entries, keys })
    }
}

impl DepositAddressSource for DepositResolver {
    fn address<'a>(
        &'a self,
        request: AddressRequest,
    ) -> BoxFuture<'a, Result<ProvisionedAddress, DepositError>> {
        Box::pin(async move {
            if request.operation_id.trim().is_empty() {
                return Err(invalid("deposit address operation ID must not be empty"));
            }
            let position = usize::try_from(request.candidate)
                .map_err(|_| invalid("deposit address candidate is out of range"))?;
            let entry = self.entries.get(position).ok_or_else(|| DepositError {
                kind: DepositErrorKind::Conflict,
                message: "configured deposit address pool is exhausted".to_owned(),
            })?;
            if entry.scope != request.scope || entry.asset != request.asset {
                return Err(invalid(
                    "deposit key purpose is not configured for the requested scope and asset",
                ));
            }
            Ok(ProvisionedAddress {
                address: entry.address.clone(),
                key: entry.key.clone(),
                key_purpose: entry.purpose.clone(),
            })
        })
    }
}

impl crate::DepositWallets for DepositResolver {
    fn wallet<'a>(&'a self, deposit: &'a Deposit) -> wallets::FutureResult<'a, Arc<dyn Wallet>> {
        Box::pin(async move {
            let KeyId::Identifier(key) = &deposit.key else {
                return Err(wallets::Error::new(
                    wallets::ErrorKind::InvalidSecret,
                    "deposit key is not managed by the configured local resolver",
                ));
            };
            let position = self.keys.get(key).ok_or_else(|| {
                wallets::Error::new(
                    wallets::ErrorKind::Unsupported,
                    "deposit key is not configured in this process",
                )
            })?;
            let entry = self.entries.get(*position).ok_or_else(|| {
                wallets::Error::new(
                    wallets::ErrorKind::Unsupported,
                    "deposit key purpose is not configured in this process",
                )
            })?;
            if deposit.key_purpose != entry.purpose
                || deposit.asset != entry.asset
                || deposit.address != entry.address
                || deposit.address.scope != entry.scope
            {
                return Err(wallets::Error::new(
                    wallets::ErrorKind::AddressMismatch,
                    "deposit metadata does not match its configured wallet",
                ));
            }
            Ok(entry.wallet.clone())
        })
    }
}

fn invalid(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvariantViolation,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use base::{
        Address, Addresser, Broadcaster, Signer, TransactionBuilder, TransactionError,
        TransactionRestore, TransactionSnapshot,
    };
    use wallets::{
        AddressEncoding, AddressFormat, AddressText, AmountFormat, Balance, BalanceReader, Error,
        FutureResult, History, HistoryReader, HistoryRequest, TransactionFactory,
    };

    use super::*;

    struct Fixture(&'static str);

    impl Addresser for Fixture {
        fn address(&self) -> Address {
            Address::new(self.0.as_bytes())
        }
    }

    impl AddressFormat for Fixture {
        fn address_text(&self, address: &Address) -> Result<AddressText, Error> {
            Ok(AddressText::new(
                AddressEncoding::Bech32,
                address.to_string(),
            ))
        }

        fn parse_address(&self, address: &AddressText) -> Result<Address, Error> {
            Ok(Address::new(address.text.as_bytes()))
        }
    }

    impl Signer for Fixture {
        fn sign<'a>(&'a self, _: base::SignRequest) -> base::SignFuture<'a> {
            Box::pin(async { unreachable!("resolver does not sign") })
        }
    }

    impl BalanceReader for Fixture {
        fn balance<'a>(&'a self) -> FutureResult<'a, Balance> {
            Box::pin(async { unreachable!("resolver does not read balances") })
        }
    }

    impl AmountFormat for Fixture {
        fn display_amount(&self, atomic: &base::Decimal) -> Result<base::Decimal, Error> {
            Ok(atomic.clone())
        }
    }

    impl TransactionFactory for Fixture {
        fn transaction(&self) -> Box<dyn TransactionBuilder> {
            unreachable!("resolver does not build transactions")
        }

        fn broadcaster(&self) -> &dyn Broadcaster {
            unreachable!("resolver does not broadcast")
        }
    }

    impl wallets::CollectionFactory for Fixture {}

    impl wallets::Sweeper for Fixture {}

    impl TransactionRestore for Fixture {
        fn restore(
            &self,
            _: &TransactionSnapshot,
        ) -> Result<Box<dyn TransactionBuilder>, TransactionError> {
            unreachable!("resolver does not restore transactions")
        }
    }

    impl HistoryReader for Fixture {
        fn history<'a>(&'a self, _: HistoryRequest) -> FutureResult<'a, History> {
            Box::pin(async { unreachable!("resolver does not read history") })
        }
    }

    fn scope() -> IndexScope {
        IndexScope {
            chain: indexing::ChainId(chain_bitcoin::CHAIN.to_owned()),
            network: "regtest".to_owned(),
        }
    }

    fn asset() -> AssetId {
        AssetId {
            chain: scope().chain,
            asset: "native".to_owned(),
        }
    }

    #[tokio::test]
    async fn binds_purpose_key_address_and_wallet() {
        let resolver = DepositResolver::new(vec![
            DepositKey {
                purpose: "merchant-2".to_owned(),
                wallet_id: "btc".to_owned(),
                scope: scope(),
                asset: asset(),
                wallet: Arc::new(Fixture("bcrt1second")),
            },
            DepositKey {
                purpose: "merchant-1".to_owned(),
                wallet_id: "btc".to_owned(),
                scope: scope(),
                asset: asset(),
                wallet: Arc::new(Fixture("bcrt1fixture")),
            },
        ])
        .expect("valid resolver");
        let provisioned = resolver
            .address(AddressRequest {
                scope: scope(),
                asset: asset(),
                operation_id: "deposit-1".to_owned(),
                candidate: 0,
                idempotency_key: deposits::IdempotencyKey("open-1".to_owned()),
            })
            .await
            .expect("configured address");
        let replayed = resolver
            .address(AddressRequest {
                scope: scope(),
                asset: asset(),
                operation_id: "deposit-1".to_owned(),
                candidate: 0,
                idempotency_key: deposits::IdempotencyKey("open-1".to_owned()),
            })
            .await
            .expect("same operation candidate");
        assert_eq!(replayed, provisioned);
        let deposit = Deposit {
            id: deposits::DepositId("deposit-1".to_owned()),
            idempotency_key: deposits::IdempotencyKey("open-1".to_owned()),
            user_id: deposits::UserId("user-1".to_owned()),
            asset: asset(),
            address: provisioned.address,
            key: provisioned.key,
            key_purpose: "merchant-1".to_owned(),
            expected: base::Decimal::from(1_u64),
            birthday: indexing::BlockHeight(1),
            expires_at: 10,
            state: deposits::DepositState::AwaitingWatch,
            created_at: 1,
        };
        let wallet = crate::DepositWallets::wallet(&resolver, &deposit)
            .await
            .expect("matching wallet");
        assert_eq!(wallet.address(), Address::new(b"bcrt1fixture"));

        let second = resolver
            .address(AddressRequest {
                scope: scope(),
                asset: asset(),
                operation_id: "deposit-2".to_owned(),
                candidate: 1,
                idempotency_key: deposits::IdempotencyKey("open-2".to_owned()),
            })
            .await
            .expect("second configured address");
        assert_eq!(second.address.value, "62637274317365636f6e64");
        assert_eq!(second.key_purpose, "merchant-2");

        let exhausted = resolver
            .address(AddressRequest {
                scope: scope(),
                asset: asset(),
                operation_id: "deposit-3".to_owned(),
                candidate: 2,
                idempotency_key: deposits::IdempotencyKey("open-3".to_owned()),
            })
            .await
            .expect_err("finite configured address pool must report exhaustion");
        assert_eq!(exhausted.kind, DepositErrorKind::Conflict);
    }
}
