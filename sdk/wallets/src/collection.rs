use std::{collections::BTreeMap, sync::Arc};

use crypto::SecretBytes;

use crate::{Error, ErrorKind, FutureResult, Provider, Wallet};

#[derive(Default)]
// design-lint: allow package-name-prefix -- Wallets is the approved public composition noun
pub struct Wallets<K: Ord> {
    providers: BTreeMap<K, Arc<dyn Provider>>,
}

impl<K: Ord> Wallets<K> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            providers: BTreeMap::new(),
        }
    }

    /// Registers exactly one provider for a typed application key.
    ///
    /// Duplicate keys are rejected during composition rather than discovered
    /// when a wallet is requested.
    pub fn register(&mut self, key: K, provider: impl Provider + 'static) -> Result<(), Error> {
        if self.providers.contains_key(&key) {
            return Err(Error::new(
                ErrorKind::Duplicate,
                "a wallet provider is already registered for this key",
            ));
        }
        self.providers.insert(key, Arc::new(provider));
        Ok(())
    }

    pub fn new_wallet<'a>(
        &'a self,
        key: &'a K,
        secret: SecretBytes,
    ) -> FutureResult<'a, Arc<dyn Wallet>> {
        let provider = self.providers.get(key).cloned();

        Box::pin(async move {
            match provider {
                None => Err(Error::new(
                    ErrorKind::Unsupported,
                    "no wallet provider is registered for this key",
                )),
                Some(provider) => provider.create(secret).await,
            }
        })
    }
}
