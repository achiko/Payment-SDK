use std::{future::Future, io, pin::Pin};

use chain_solana::{RegistrationError, SubmissionRegistrar, SubmissionTask};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
};

enum Command {
    Insert {
        task: Task,
        acknowledgement: oneshot::Sender<()>,
    },
    Close {
        acknowledgement: oneshot::Sender<()>,
    },
}

enum Task {
    Submission(SubmissionTask),
    #[cfg(test)]
    Fixture(Pin<Box<dyn Future<Output = ()> + Send + 'static>>),
}

impl Task {
    async fn run(self) {
        match self {
            Self::Submission(task) => task.run().await,
            #[cfg(test)]
            Self::Fixture(task) => task.await,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Registrar {
    commands: mpsc::Sender<Command>,
}

pub(crate) struct Control {
    commands: mpsc::Sender<Command>,
}

pub(crate) struct Supervisor {
    commands: mpsc::Receiver<Command>,
    tasks: JoinSet<()>,
}

impl Supervisor {
    pub(crate) fn new(capacity: usize) -> Result<(Self, Registrar, Control), io::Error> {
        if capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "submission queue capacity must be positive",
            ));
        }
        let (commands, receiver) = mpsc::channel(capacity);
        Ok((
            Self {
                commands: receiver,
                tasks: JoinSet::new(),
            },
            Registrar {
                commands: commands.clone(),
            },
            Control { commands },
        ))
    }

    pub(crate) async fn run(mut self) -> Result<(), tokio::task::JoinError> {
        loop {
            tokio::select! {
                biased;
                command = self.commands.recv() => {
                    match command {
                        Some(Command::Insert { task, acknowledgement }) => {
                            self.tasks.spawn(task.run());
                            let _ = acknowledgement.send(());
                        }
                        Some(Command::Close { acknowledgement }) => {
                            self.commands.close();
                            let _ = acknowledgement.send(());
                            break;
                        }
                        None => break,
                    }
                }
                result = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    if let Some(result) = result {
                        result?;
                    }
                }
            }
        }
        while let Ok(command) = self.commands.try_recv() {
            drop(command);
        }
        while let Some(result) = self.tasks.join_next().await {
            result?;
        }
        Ok(())
    }
}

impl Registrar {
    async fn insert(&self, task: Task) -> Result<(), RegistrationError> {
        let (acknowledgement, inserted) = oneshot::channel();
        self.commands
            .send(Command::Insert {
                task,
                acknowledgement,
            })
            .await
            .map_err(|_| RegistrationError::Closed)?;
        inserted
            .await
            .map_err(|_| RegistrationError::AcknowledgementLost)
    }
}

impl SubmissionRegistrar for Registrar {
    fn register<'a>(
        &'a self,
        task: SubmissionTask,
    ) -> Pin<Box<dyn Future<Output = Result<(), RegistrationError>> + Send + 'a>> {
        Box::pin(self.insert(Task::Submission(task)))
    }
}

impl Control {
    pub(crate) async fn close(&self) -> Result<(), RegistrationError> {
        let (acknowledgement, closed) = oneshot::channel();
        self.commands
            .send(Command::Close { acknowledgement })
            .await
            .map_err(|_| RegistrationError::Closed)?;
        closed
            .await
            .map_err(|_| RegistrationError::AcknowledgementLost)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    #[tokio::test]
    async fn insertion_is_acknowledged_only_after_the_supervisor_owns_the_task() {
        let (supervisor, registrar, control) = Supervisor::new(1).expect("bounded supervisor");
        let running = tokio::spawn(supervisor.run());
        let completed = Arc::new(AtomicBool::new(false));
        let task_completed = Arc::clone(&completed);

        registrar
            .insert(Task::Fixture(Box::pin(async move {
                task_completed.store(true, Ordering::Release);
            })))
            .await
            .expect("inserted acknowledgement");
        control.close().await.expect("serialized close");
        running.await.expect("supervisor task").expect("task set");
        assert!(completed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn close_winning_rejects_later_registration() {
        let (supervisor, registrar, control) = Supervisor::new(1).expect("bounded supervisor");
        let running = tokio::spawn(supervisor.run());

        control.close().await.expect("serialized close");
        assert_eq!(
            registrar
                .insert(Task::Fixture(Box::pin(async {})))
                .await
                .expect_err("closed registration"),
            RegistrationError::Closed
        );
        running.await.expect("supervisor task").expect("task set");
    }

    #[tokio::test]
    async fn insertion_winning_is_tracked_before_a_later_close() {
        let (supervisor, registrar, control) = Supervisor::new(2).expect("bounded supervisor");
        let completed = Arc::new(AtomicBool::new(false));
        let task_completed = Arc::clone(&completed);
        let insertion = tokio::spawn(async move {
            registrar
                .insert(Task::Fixture(Box::pin(async move {
                    task_completed.store(true, Ordering::Release);
                })))
                .await
        });
        tokio::task::yield_now().await;
        let closing = tokio::spawn(async move { control.close().await });
        tokio::task::yield_now().await;
        let running = tokio::spawn(supervisor.run());

        insertion
            .await
            .expect("insertion requester")
            .expect("inserted before close");
        closing
            .await
            .expect("close requester")
            .expect("serialized close");
        running.await.expect("supervisor task").expect("task set");
        assert!(completed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn abandoned_acknowledgement_does_not_detach_inserted_work() {
        let (supervisor, registrar, control) = Supervisor::new(2).expect("bounded supervisor");
        let completed = Arc::new(AtomicBool::new(false));
        let task_completed = Arc::clone(&completed);
        let insertion = tokio::spawn(async move {
            registrar
                .insert(Task::Fixture(Box::pin(async move {
                    task_completed.store(true, Ordering::Release);
                })))
                .await
        });
        tokio::task::yield_now().await;
        insertion.abort();

        let running = tokio::spawn(supervisor.run());
        tokio::task::yield_now().await;
        control.close().await.expect("serialized close");
        running.await.expect("supervisor task").expect("task set");
        assert!(completed.load(Ordering::Acquire));
    }

    #[test]
    fn rejects_an_unbounded_zero_capacity() {
        assert!(Supervisor::new(0).is_err());
    }
}
