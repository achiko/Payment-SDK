use std::sync::Arc;

use crypto::SecretBytes;

use crate::{FutureResult, Wallet};

/// Constructs one concrete wallet family selected during application startup.
pub trait Provider: Send + Sync {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>>;

    /// Generates a wallet without exposing its private key to the caller.
    fn generate(&self) -> FutureResult<'_, Arc<dyn Wallet>> {
        let secret = match SecretBytes::generate_secp256k1() {
            Ok(secret) => secret,
            Err(error) => {
                return Box::pin(async move {
                    Err(crate::Error::new(
                        crate::ErrorKind::Generation,
                        error.to_string(),
                    ))
                });
            }
        };
        self.create(secret)
    }
}

impl<T: Provider + ?Sized> Provider for Arc<T> {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>> {
        (**self).create(secret)
    }

    fn generate(&self) -> FutureResult<'_, Arc<dyn Wallet>> {
        (**self).generate()
    }
}
