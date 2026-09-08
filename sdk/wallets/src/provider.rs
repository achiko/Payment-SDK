use std::sync::Arc;

use crypto::SecretBytes;

use crate::{FutureResult, Wallet};

/// Constructs one concrete wallet family selected during application startup.
///
/// Every provider must select its native key-generation policy explicitly. An
/// implementation that supplies only the import path must not compile:
///
/// ```compile_fail,E0046
/// use std::sync::Arc;
/// use wallets::{FutureResult, Provider, SecretBytes, Wallet};
///
/// struct IncompleteProvider;
///
/// impl Provider for IncompleteProvider {
///     fn create<'a>(
///         &'a self,
///         _secret: SecretBytes,
///     ) -> FutureResult<'a, Arc<dyn Wallet>> {
///         unimplemented!()
///     }
/// }
/// ```
pub trait Provider: Send + Sync {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>>;

    /// Generates a wallet without exposing its private key to the caller.
    fn generate(&self) -> FutureResult<'_, Arc<dyn Wallet>>;
}

impl<T: Provider + ?Sized> Provider for Arc<T> {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>> {
        (**self).create(secret)
    }

    fn generate(&self) -> FutureResult<'_, Arc<dyn Wallet>> {
        (**self).generate()
    }
}
