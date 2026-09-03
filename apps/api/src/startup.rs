//! Enforces the startup boundary between external identity checks and storage.

use std::future::Future;

/// Opens storage only after every supplied identity check has succeeded.
pub(crate) async fn verify_then_open<V, T, E, O, S>(verification: V, open: O) -> Result<(T, S), E>
where
    V: Future<Output = Result<T, E>>,
    O: FnOnce() -> Result<S, E>,
{
    let verified = verification.await?;
    let storage = open()?;
    Ok((verified, storage))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    #[tokio::test]
    async fn every_identity_failure_observes_zero_storage_calls() {
        for failure in [
            "wrong Bitcoin identity",
            "wrong Ethereum identity",
            "wrong Solana genesis",
            "missing Memo",
            "non-executable Memo",
            "malformed Memo",
            "Memo below finalized floor",
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let opened = Arc::clone(&calls);
            let result = verify_then_open(async { Err::<(), _>(failure) }, move || {
                opened.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
            .await;

            assert_eq!(result, Err(failure));
            assert_eq!(calls.load(Ordering::Relaxed), 0, "failure: {failure}");
        }
    }

    #[tokio::test]
    async fn storage_opens_once_after_success() {
        let calls = AtomicUsize::new(0);
        let result = verify_then_open(async { Ok::<_, &str>("verified") }, || {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok("pool")
        })
        .await;

        assert_eq!(result, Ok(("verified", "pool")));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
