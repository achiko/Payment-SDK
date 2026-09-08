//! Enforces the startup boundary between external identity checks and storage.

use std::future::Future;

// design-lint: allow unclassified-free-function -- application startup sequences an injected verification future before a one-shot storage opener, preserving a testable zero-open boundary on verification failure or cancellation
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

    #[test]
    fn pending_and_cancelled_verification_never_opens_storage() {
        let calls = AtomicUsize::new(0);
        let verification = std::future::pending::<Result<(), &str>>();
        let mut startup = Box::pin(verify_then_open(verification, || {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());

        assert!(startup.as_mut().poll(&mut context).is_pending());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        drop(startup);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn storage_failure_propagates_after_successful_verification() {
        let order = AtomicUsize::new(0);
        let result = verify_then_open(
            async {
                assert_eq!(order.fetch_add(1, Ordering::Relaxed), 0);
                Ok::<_, &str>("verified")
            },
            || {
                assert_eq!(order.fetch_add(1, Ordering::Relaxed), 1);
                Err::<(), _>("storage open failed")
            },
        )
        .await;

        assert_eq!(result, Err("storage open failed"));
        assert_eq!(order.load(Ordering::Relaxed), 2);
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
