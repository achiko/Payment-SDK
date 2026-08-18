use std::{borrow::Borrow, collections::BTreeMap, sync::Arc};

use crypto::SecretBytes;

use crate::{Error, ErrorKind, FutureResult, Provider, Wallet};

/// Wallet constructors selected during application startup.
#[derive(Default)]
pub struct Providers<K: Ord> {
    values: BTreeMap<K, Arc<dyn Provider>>,
}

impl<K: Ord> Providers<K> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            values: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, key: K, provider: impl Provider + 'static) -> Result<(), Error> {
        if self.values.contains_key(&key) {
            return Err(duplicate(
                "a wallet provider is already registered for this key",
            ));
        }
        self.values.insert(key, Arc::new(provider));
        Ok(())
    }

    pub fn create<'a>(
        &'a self,
        key: &'a K,
        secret: SecretBytes,
    ) -> FutureResult<'a, Arc<dyn Wallet>> {
        let provider = self.values.get(key).cloned();
        Box::pin(async move {
            provider
                .ok_or_else(|| unsupported("no wallet provider is registered for this key"))?
                .create(secret)
                .await
        })
    }

    pub fn generate<'a>(&'a self, key: &'a K) -> FutureResult<'a, Arc<dyn Wallet>> {
        let provider = self.values.get(key).cloned();
        Box::pin(async move {
            provider
                .ok_or_else(|| unsupported("no wallet provider is registered for this key"))?
                .generate()
                .await
        })
    }
}

/// Wallet instances available to application handlers.
#[derive(Default)]
// design-lint: allow package-name-prefix -- Wallets is the approved public composition noun
pub struct Wallets<K: Ord> {
    values: BTreeMap<K, Arc<dyn Wallet>>,
}

impl<K: Ord> Wallets<K> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            values: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, key: K, wallet: Arc<dyn Wallet>) -> Result<(), Error> {
        if self.values.contains_key(&key) {
            return Err(duplicate("a wallet is already registered for this key"));
        }
        self.values.insert(key, wallet);
        Ok(())
    }

    pub fn get<Q>(&self, key: &Q) -> Result<Arc<dyn Wallet>, Error>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.values
            .get(key)
            .cloned()
            .ok_or_else(|| unsupported("no wallet is registered for this key"))
    }
}

fn duplicate(message: &'static str) -> Error {
    Error::new(ErrorKind::Duplicate, message)
}

fn unsupported(message: &'static str) -> Error {
    Error::new(ErrorKind::Unsupported, message)
}
