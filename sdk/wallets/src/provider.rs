use std::sync::Arc;

use crypto::SecretBytes;

use crate::{FutureResult, Wallet};

/// Constructs one concrete wallet family selected during application startup.
pub trait Provider: Send + Sync {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>>;
}

impl<T: Provider + ?Sized> Provider for Arc<T> {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>> {
        (**self).create(secret)
    }
}
