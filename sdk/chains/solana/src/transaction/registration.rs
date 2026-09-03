use std::{future::Future, pin::Pin};

pub type RegistrationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), RegistrationError>> + Send + 'a>>;

/// Application-supervised work that must be inserted before dispatch begins.
pub struct SubmissionTask(Pin<Box<dyn Future<Output = ()> + Send + 'static>>);

pub(super) struct Activation(tokio::sync::oneshot::Sender<()>);

impl SubmissionTask {
    pub(super) fn dormant(future: impl Future<Output = ()> + Send + 'static) -> (Self, Activation) {
        let (start, waiting) = tokio::sync::oneshot::channel();
        (
            Self(Box::pin(async move {
                if waiting.await.is_ok() {
                    future.await;
                }
            })),
            Activation(start),
        )
    }

    pub async fn run(self) {
        self.0.await;
    }
}

impl Activation {
    pub(super) fn start(self) {
        let _ = self.0.send(());
    }
}

/// Inserts one Solana submission task into application-owned supervision.
///
/// `Ok(())` is an acknowledgement that `task` is already visible to the
/// supervisor. An implementation must return an error without insertion when
/// registration is closed or that acknowledgement cannot be delivered.
pub trait SubmissionRegistrar: Send + Sync {
    fn register<'a>(&'a self, task: SubmissionTask) -> RegistrationFuture<'a>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationError {
    Closed,
    AcknowledgementLost,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    struct Registrar {
        outcome: Result<(), RegistrationError>,
        inserted: Arc<Mutex<Vec<SubmissionTask>>>,
    }

    impl SubmissionRegistrar for Registrar {
        fn register<'a>(&'a self, task: SubmissionTask) -> RegistrationFuture<'a> {
            Box::pin(async move {
                self.outcome?;
                self.inserted
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(task);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn acknowledges_only_an_inserted_task() {
        let inserted = Arc::new(Mutex::new(Vec::new()));
        let registrar = Registrar {
            outcome: Ok(()),
            inserted: Arc::clone(&inserted),
        };
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_completed = Arc::clone(&completed);

        let (task, activation) = SubmissionTask::dormant(async move {
            task_completed.store(true, std::sync::atomic::Ordering::Release);
        });
        registrar
            .register(task)
            .await
            .expect("inserted task acknowledgement");

        let task = inserted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .expect("registered task");
        assert!(!completed.load(std::sync::atomic::Ordering::Acquire));
        activation.start();
        task.run().await;
        assert!(completed.load(std::sync::atomic::Ordering::Acquire));
    }

    #[tokio::test]
    async fn closed_and_lost_acknowledgement_do_not_insert_or_run() {
        for outcome in [
            RegistrationError::Closed,
            RegistrationError::AcknowledgementLost,
        ] {
            let inserted = Arc::new(Mutex::new(Vec::new()));
            let registrar = Registrar {
                outcome: Err(outcome),
                inserted: Arc::clone(&inserted),
            };
            let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let task_completed = Arc::clone(&completed);

            let (task, activation) = SubmissionTask::dormant(async move {
                task_completed.store(true, std::sync::atomic::Ordering::Release);
            });
            let error = registrar
                .register(task)
                .await
                .expect_err("registration must fail");

            assert_eq!(error, outcome);
            assert!(
                inserted
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_empty()
            );
            drop(activation);
            assert!(!completed.load(std::sync::atomic::Ordering::Acquire));
        }
    }
}
