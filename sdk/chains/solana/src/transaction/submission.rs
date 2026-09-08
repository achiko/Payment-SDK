use std::sync::Arc;

use base::{TransactionError, TransactionErrorKind, TransactionId};
use wallets::{Error as WalletError, ErrorKind as WalletErrorKind, SendError};

use crate::RpcClient;

use super::{
    Cancellation, Envelope, PreparedBatch, Reconciler, SubmissionRegistrar, SubmissionTask,
    reconciliation::Resolver, registration::Activation, source::GuardedSources,
};

pub struct Submitter<C> {
    rpc: RpcClient<C>,
    registrar: Arc<dyn SubmissionRegistrar>,
    resolver: Arc<dyn Resolver>,
}

impl<C> Submitter<C>
where
    C: json_rpc::Client + 'static,
{
    #[must_use]
    pub fn new(
        rpc: RpcClient<C>,
        registrar: Arc<dyn SubmissionRegistrar>,
        reconciler: Reconciler<C>,
    ) -> Self {
        Self {
            rpc,
            registrar,
            resolver: Arc::new(reconciler),
        }
    }

    #[cfg(test)]
    fn fixture(rpc: RpcClient<C>, registrar: Arc<dyn SubmissionRegistrar>) -> Self {
        Self::with_resolver(rpc, registrar, Arc::new(InactiveResolver))
    }

    #[cfg(test)]
    fn with_resolver(
        rpc: RpcClient<C>,
        registrar: Arc<dyn SubmissionRegistrar>,
        resolver: Arc<dyn Resolver>,
    ) -> Self {
        Self {
            rpc,
            registrar,
            resolver,
        }
    }

    pub async fn submit(
        &self,
        prepared: PreparedBatch,
        cancellation: &Cancellation,
    ) -> Result<Vec<TransactionId>, SendError> {
        cancellation.ensure()?;
        let (floor, envelopes, leases) = prepared.into_parts();
        let first = envelopes.first().ok_or_else(|| {
            SendError::operation(
                WalletErrorKind::InvalidBatch,
                "Solana submission requires at least one envelope",
            )
        })?;
        let height = tokio::select! {
            result = self.rpc.block_height(floor) => result.map_err(|_| definite(
                first.index(),
                Vec::new(),
                "Solana block height is unavailable before registration",
            ))?,
            () = cancellation.cancelled() => return Err(SendError::operation(
                WalletErrorKind::Unavailable,
                "Solana submission registration was cancelled",
            )),
        };
        if height > first.lifetime().last_valid_block_height() {
            return Err(definite(
                first.index(),
                Vec::new(),
                "Solana transaction lifetime expired before registration",
            ));
        }
        let guarded = leases.guard();
        let (result_send, result_wait) = tokio::sync::oneshot::channel();
        let rpc = self.rpc.clone();
        let resolver = Arc::clone(&self.resolver);
        let (task, activation) = SubmissionTask::dormant(async move {
            match run(rpc, floor, envelopes, guarded).await {
                Outcome::Complete(result) => {
                    let _ = result_send.send(result);
                }
                Outcome::Ambiguous { error, envelope } => {
                    let _ = result_send.send(Err(error));
                    resolver.resolve(*envelope).await;
                }
            }
        });

        self.register(task, activation, cancellation).await?;

        tokio::select! {
            result = result_wait => result.unwrap_or_else(|_| {
                Err(SendError::operation(
                    WalletErrorKind::Unavailable,
                    "Solana submission task ended without a result",
                ))
            }),
            () = cancellation.cancelled() => Err(SendError::operation(
                WalletErrorKind::Unavailable,
                "Solana submission result waiter was cancelled",
            )),
        }
    }

    async fn register(
        &self,
        task: SubmissionTask,
        activation: Activation,
        cancellation: &Cancellation,
    ) -> Result<(), SendError> {
        tokio::select! {
            result = self.registrar.register(task) => {
                result.map_err(|_| SendError::operation(
                    WalletErrorKind::Unavailable,
                    "Solana submission registration failed",
                ))?;
                activation.start();
                Ok(())
            },
            () = cancellation.cancelled() => Err(SendError::operation(
                WalletErrorKind::Unavailable,
                "Solana submission registration was cancelled",
            )),
        }
    }
}

async fn run<C>(
    rpc: RpcClient<C>,
    floor: u64,
    envelopes: Vec<Envelope>,
    guarded: GuardedSources,
) -> Outcome
where
    C: json_rpc::Client,
{
    let mut accepted = Vec::with_capacity(envelopes.len());
    for (position, envelope) in envelopes.into_iter().enumerate() {
        let height = match position {
            0 => Ok(None),
            _ => rpc.block_height(floor).await.map(Some),
        };
        match height {
            Err(_) => {
                return Outcome::Complete(Err(definite(
                    envelope.index(),
                    accepted,
                    "Solana block height is unavailable before dispatch",
                )));
            }
            Ok(Some(height)) if height > envelope.lifetime().last_valid_block_height() => {
                return Outcome::Complete(Err(definite(
                    envelope.index(),
                    accepted,
                    "Solana transaction lifetime expired before dispatch",
                )));
            }
            Ok(_) => {}
        }

        if !envelope.submit_and_observe(&rpc, floor).await {
            guarded.retain_ambiguity(envelope.source());
            let error = TransactionError::new(
                TransactionErrorKind::Unknown,
                "Solana submission outcome is unknown",
            )
            .with_ambiguous_transaction_id(envelope.id().clone());
            return Outcome::Ambiguous {
                error: SendError::item(envelope.index(), accepted, WalletError::from(error)),
                envelope: Box::new(envelope),
            };
        }
        accepted.push(envelope.id().clone());
    }
    Outcome::Complete(Ok(accepted))
}

impl Envelope {
    async fn submit_and_observe<C>(&self, rpc: &RpcClient<C>, floor: u64) -> bool
    where
        C: json_rpc::Client,
    {
        for attempt in 0..3 {
            if rpc
                .send_transaction(self.signed_bytes(), floor, self.id().clone())
                .await
                .is_ok()
            {
                return true;
            }
            match rpc.signature_status(self.id(), floor).await {
                Ok(status) if status.value.is_some() => return true,
                Ok(_) if attempt < 2 => {}
                _ => return false,
            }
            match rpc.block_height(floor).await {
                Ok(height) if height <= self.lifetime().last_valid_block_height() => {}
                _ => return false,
            }
        }
        false
    }
}

#[cfg(test)]
struct InactiveResolver;

#[cfg(test)]
impl Resolver for InactiveResolver {
    fn resolve<'a>(
        &'a self,
        _envelope: Envelope,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
}

enum Outcome {
    Complete(Result<Vec<TransactionId>, SendError>),
    Ambiguous {
        error: SendError,
        envelope: Box<Envelope>,
    },
}

fn definite(index: usize, accepted: Vec<TransactionId>, message: &'static str) -> SendError {
    SendError::item(
        index,
        accepted,
        WalletError::new(WalletErrorKind::Unavailable, message),
    )
}

#[cfg(test)]
mod tests {
    use std::{future::Future, pin::Pin, sync::Mutex};

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;
    use solana_hash::Hash;
    use solana_signature::Signature;

    use crate::{
        Address, BlockhashLifetime, Key, Lamport, Memo, Message, ResolvedTransfer, Seed,
        SourceCoordinator, rpc::test_support::Scripted,
    };

    use super::*;

    struct Registrar {
        outcome: Result<(), super::super::RegistrationError>,
        tasks: Arc<Mutex<Vec<SubmissionTask>>>,
    }

    impl SubmissionRegistrar for Registrar {
        fn register<'a>(
            &'a self,
            task: SubmissionTask,
        ) -> super::super::registration::RegistrationFuture<'a> {
            Box::pin(async move {
                self.outcome?;
                self.tasks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(task);
                Ok(())
            })
        }
    }

    struct PendingRegistrar;

    impl SubmissionRegistrar for PendingRegistrar {
        fn register<'a>(
            &'a self,
            task: SubmissionTask,
        ) -> super::super::registration::RegistrationFuture<'a> {
            Box::pin(async move {
                task.run().await;
                panic!("unacknowledged registration must not activate the task")
            })
        }
    }

    #[derive(Default)]
    struct Resolution {
        envelopes: Mutex<Vec<Envelope>>,
    }

    impl Resolver for Resolution {
        fn resolve<'a>(
            &'a self,
            envelope: Envelope,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            Box::pin(async move {
                self.envelopes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(envelope);
            })
        }
    }

    fn key() -> Arc<Key> {
        Arc::new(
            Key::from_seed(
                "0707070707070707070707070707070707070707070707070707070707070707"
                    .parse::<Seed>()
                    .expect("seed"),
            )
            .expect("key"),
        )
    }

    fn prepared(coordinator: &SourceCoordinator) -> PreparedBatch {
        let key = key();
        let source = key.address().clone();
        let transfer = ResolvedTransfer::new(
            0,
            source.clone(),
            Address::from_bytes([8; 32]).to_string(),
            Lamport::from_atomic(3),
        );
        let leases = coordinator
            .lease(std::slice::from_ref(&transfer), false)
            .expect("source lease");
        let lifetime = BlockhashLifetime::new(Hash::new_from_array([9; 32]), 44);
        let message = Message::native_transfer(
            &source,
            &Address::from_bytes([8; 32]),
            transfer.amount(),
            Memo::from_bytes([3; Memo::LENGTH]),
            &lifetime,
        )
        .expect("message");
        let envelope = Envelope::sign(source, 0, message, 11, lifetime, &key).expect("envelope");
        PreparedBatch::fixture(11, vec![envelope], leases)
    }

    #[tokio::test]
    async fn closed_registration_executes_no_wire_call_and_releases_guard() {
        let coordinator = SourceCoordinator::default();
        let registrar = Arc::new(Registrar {
            outcome: Err(super::super::RegistrationError::Closed),
            tasks: Arc::new(Mutex::new(Vec::new())),
        });
        let rpc = Scripted::one(
            "getBlockHeight",
            json!([{"commitment":"confirmed", "minContextSlot":11}]),
            json!(44),
        );
        let submitter = Submitter::fixture(RpcClient::new(rpc.clone()), registrar);

        let error = submitter
            .submit(prepared(&coordinator), &Cancellation::default())
            .await
            .expect_err("closed registration");
        assert_eq!(error.source.kind, WalletErrorKind::Unavailable);
        assert_eq!(
            error.source.message,
            "Solana submission registration failed"
        );
        assert!(error.accepted.is_empty());
        assert_eq!(error.failed_index, None);
        assert!(error.ambiguous_transaction_id.is_none());
        rpc.assert_finished();
        assert!(
            coordinator
                .lease(
                    &[ResolvedTransfer::new(
                        0,
                        key().address().clone(),
                        String::new(),
                        Lamport::from_atomic(1),
                    )],
                    false,
                )
                .is_ok()
        );
    }

    #[tokio::test]
    async fn cancellation_before_acknowledgement_never_activates_and_releases_guard() {
        use std::task::{Context, Poll, Waker};

        let coordinator = SourceCoordinator::default();
        let prepared = prepared(&coordinator);
        let source = prepared.envelopes()[0].source().clone();
        let transfer = ResolvedTransfer::new(0, source, String::new(), Lamport::from_atomic(1));
        let rpc = Scripted::one(
            "getBlockHeight",
            json!([{"commitment":"confirmed", "minContextSlot":11}]),
            json!(44),
        );
        let submitter = Submitter::fixture(RpcClient::new(rpc.clone()), Arc::new(PendingRegistrar));
        let cancellation = Cancellation::default();
        let mut waiting = Box::pin(submitter.submit(prepared, &cancellation));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(waiting.as_mut().poll(&mut context), Poll::Pending));
        assert!(
            coordinator
                .lease(std::slice::from_ref(&transfer), false)
                .is_err()
        );

        cancellation.cancel();
        let error = waiting.await.expect_err("registration cancellation");
        assert_eq!(error.source.kind, WalletErrorKind::Unavailable);
        assert_eq!(
            error.source.message,
            "Solana submission registration was cancelled"
        );
        assert!(error.accepted.is_empty());
        assert_eq!(error.failed_index, None);
        assert_eq!(error.ambiguous_transaction_id, None);
        assert_eq!(error.source.ambiguous_transaction_id, None);
        coordinator
            .lease(&[transfer], false)
            .expect("cancelled registration releases source");
        rpc.assert_finished();
    }

    #[tokio::test]
    async fn registered_task_broadcasts_exact_bytes_and_returns_local_id() {
        let coordinator = SourceCoordinator::default();
        let prepared = prepared(&coordinator);
        let envelope = prepared.envelopes()[0].clone();
        let rpc = Scripted::new([
            (
                "getBlockHeight",
                json!([{"commitment":"confirmed", "minContextSlot":11}]),
                json!(44),
            ),
            (
                "sendTransaction",
                json!([STANDARD.encode(envelope.signed_bytes()), {"encoding":"base64","skipPreflight":false,"preflightCommitment":"confirmed","minContextSlot":11,"maxRetries":0}]),
                json!(envelope.id().as_str()),
            ),
        ]);
        let tasks = Arc::new(Mutex::new(Vec::new()));
        let registrar = Arc::new(Registrar {
            outcome: Ok(()),
            tasks: Arc::clone(&tasks),
        });
        let submitter = Arc::new(Submitter::fixture(RpcClient::new(rpc.clone()), registrar));
        let task_submitter = Arc::clone(&submitter);
        let waiter = tokio::spawn(async move {
            task_submitter
                .submit(prepared, &Cancellation::default())
                .await
        });
        tokio::task::yield_now().await;
        let task = tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .expect("inserted task");
        task.run().await;
        let accepted = waiter.await.expect("waiter").expect("accepted");
        assert_eq!(accepted, [envelope.id().clone()]);
        rpc.assert_finished();
    }

    #[tokio::test]
    async fn bounds_identical_replay_and_hands_ambiguity_to_reconciliation() {
        let coordinator = SourceCoordinator::default();
        let (floor, mut envelopes, leases) = prepared(&coordinator).into_parts();
        let first = envelopes[0].clone();
        let message = Message::native_transfer(
            first.source(),
            &Address::from_bytes([8; 32]),
            Lamport::from_atomic(3),
            Memo::from_bytes([4; Memo::LENGTH]),
            first.lifetime(),
        )
        .expect("second distinct message");
        let envelope = Envelope::sign(
            first.source().clone(),
            7,
            message,
            floor,
            first.lifetime().clone(),
            &key(),
        )
        .expect("second envelope retains its original occurrence index");
        envelopes.push(envelope.clone());
        let prepared = PreparedBatch::fixture(floor, envelopes, leases);
        let local = envelope.id().clone();
        let mismatch = Signature::from([8; 64]).to_string();
        let send = || {
            (
                "sendTransaction",
                json!([STANDARD.encode(envelope.signed_bytes()), {"encoding":"base64","skipPreflight":false,"preflightCommitment":"confirmed","minContextSlot":11,"maxRetries":0}]),
                json!(mismatch.clone()),
            )
        };
        let status = || {
            (
                "getSignatureStatuses",
                json!([[local.as_str()], {"searchTransactionHistory":true}]),
                json!({"context":{"slot":15},"value":[null]}),
            )
        };
        let height = || {
            (
                "getBlockHeight",
                json!([{"commitment":"confirmed", "minContextSlot":11}]),
                json!(44),
            )
        };
        let rpc = Scripted::new([
            height(),
            (
                "sendTransaction",
                json!([STANDARD.encode(first.signed_bytes()), {"encoding":"base64","skipPreflight":false,"preflightCommitment":"confirmed","minContextSlot":11,"maxRetries":0}]),
                json!(first.id().as_str()),
            ),
            height(),
            send(),
            status(),
            height(),
            send(),
            status(),
            height(),
            send(),
            status(),
        ]);
        let tasks = Arc::new(Mutex::new(Vec::new()));
        let registrar = Arc::new(Registrar {
            outcome: Ok(()),
            tasks: Arc::clone(&tasks),
        });
        let resolution = Arc::new(Resolution::default());
        let submitter = Arc::new(Submitter::with_resolver(
            RpcClient::new(rpc.clone()),
            registrar,
            resolution.clone(),
        ));
        let waiter =
            tokio::spawn(async move { submitter.submit(prepared, &Cancellation::default()).await });
        tokio::task::yield_now().await;
        let task = tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .expect("registered task");
        task.run().await;
        let error = waiter.await.expect("waiter").expect_err("ambiguous");
        assert_eq!(error.failed_index, Some(7));
        assert_eq!(error.accepted, [first.id().clone()]);
        assert_eq!(error.ambiguous_transaction_id, Some(local.clone()));
        assert_eq!(error.source.kind, WalletErrorKind::Unavailable);
        assert_eq!(error.source.message, "Solana submission outcome is unknown");
        assert_eq!(error.source.ambiguous_transaction_id, None);
        assert_eq!(
            *resolution
                .envelopes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            [envelope]
        );
        let retained = coordinator
            .lease(
                &[ResolvedTransfer::new(
                    7,
                    first.source().clone(),
                    String::new(),
                    Lamport::from_atomic(1),
                )],
                false,
            )
            .err()
            .expect("ambiguous source must remain guarded for reconciliation");
        assert_eq!(retained.source.kind, WalletErrorKind::SourceBusy);
        rpc.assert_finished();
    }

    #[tokio::test]
    async fn failed_dispatch_requires_valid_status_and_live_height_before_replay() {
        let absent = json!({"context":{"slot":15},"value":[null]});
        for (status, height, observed) in [
            (
                json!({"context":{"slot":15},"value":[{"slot":12,"confirmations":null,"err":{"InstructionError":[0,"Custom"]},"confirmationStatus":"finalized"}]}),
                None,
                true,
            ),
            (json!({"context":{"slot":10},"value":[null]}), None, false),
            (absent.clone(), Some(json!(45)), false),
            (absent, Some(json!("unavailable")), false),
        ] {
            let coordinator = SourceCoordinator::default();
            let (floor, envelopes, leases) = prepared(&coordinator).into_parts();
            let envelope = envelopes[0].clone();
            let send = (
                "sendTransaction",
                json!([STANDARD.encode(envelope.signed_bytes()), {"encoding":"base64","skipPreflight":false,"preflightCommitment":"confirmed","minContextSlot":floor,"maxRetries":0}]),
                json!(Signature::from([8; 64]).to_string()),
            );
            let status = (
                "getSignatureStatuses",
                json!([[envelope.id().as_str()], {"searchTransactionHistory":true}]),
                status,
            );
            let sentinel = ("getHealth", json!([]), json!("ok"));
            let rpc = match height {
                Some(height) => Scripted::new([
                    send,
                    status,
                    (
                        "getBlockHeight",
                        json!([{"commitment":"confirmed", "minContextSlot":floor}]),
                        height,
                    ),
                    sentinel,
                ]),
                None => Scripted::new([send, status, sentinel]),
            };
            let client = RpcClient::new(rpc.clone());
            let outcome = run(client.clone(), floor, envelopes, leases.guard()).await;
            match outcome {
                Outcome::Complete(result) => {
                    assert!(observed);
                    assert_eq!(
                        result.expect("observed execution failure"),
                        [envelope.id().clone()]
                    );
                }
                Outcome::Ambiguous {
                    error,
                    envelope: retained,
                } => {
                    assert!(!observed);
                    assert_eq!(*retained, envelope);
                    assert_eq!(error.failed_index, Some(envelope.index()));
                    assert!(error.accepted.is_empty());
                    assert_eq!(error.ambiguous_transaction_id, Some(envelope.id().clone()));
                    assert_eq!(error.source.kind, WalletErrorKind::Unavailable);
                    assert_eq!(error.source.message, "Solana submission outcome is unknown");
                }
            }
            let lease = coordinator.lease(
                &[ResolvedTransfer::new(
                    0,
                    envelope.source().clone(),
                    String::new(),
                    Lamport::from_atomic(1),
                )],
                false,
            );
            assert_eq!(lease.is_ok(), observed);
            client
                .health()
                .await
                .expect("no extra status, height, or replay calls");
            rpc.assert_finished();
        }
    }

    #[tokio::test]
    async fn later_item_lifetime_failure_preserves_the_accepted_prefix_and_releases_sources() {
        for (height, message) in [
            (
                json!(45),
                "Solana transaction lifetime expired before dispatch",
            ),
            (
                json!("unavailable"),
                "Solana block height is unavailable before dispatch",
            ),
        ] {
            let coordinator = SourceCoordinator::default();
            let (floor, mut envelopes, leases) = prepared(&coordinator).into_parts();
            let first = envelopes[0].clone();
            let second_message = Message::native_transfer(
                first.source(),
                &Address::from_bytes([8; 32]),
                Lamport::from_atomic(3),
                Memo::from_bytes([4; Memo::LENGTH]),
                first.lifetime(),
            )
            .expect("second message");
            envelopes.push(
                Envelope::sign(
                    first.source().clone(),
                    7,
                    second_message,
                    floor,
                    first.lifetime().clone(),
                    &key(),
                )
                .expect("second envelope"),
            );
            let rpc = Scripted::new([
                (
                    "sendTransaction",
                    json!([STANDARD.encode(first.signed_bytes()), {"encoding":"base64","skipPreflight":false,"preflightCommitment":"confirmed","minContextSlot":floor,"maxRetries":0}]),
                    json!(first.id().as_str()),
                ),
                (
                    "getBlockHeight",
                    json!([{"commitment":"confirmed", "minContextSlot":floor}]),
                    height,
                ),
                ("getHealth", json!([]), json!("ok")),
            ]);
            let client = RpcClient::new(rpc.clone());
            let Outcome::Complete(result) =
                run(client.clone(), floor, envelopes, leases.guard()).await
            else {
                panic!("an undispatched second item has no ambiguous outcome");
            };
            let error = result.expect_err("second item lifetime unavailable");
            assert_eq!(error.failed_index, Some(7));
            assert_eq!(error.accepted, [first.id().clone()]);
            assert_eq!(error.source.kind, WalletErrorKind::Unavailable);
            assert_eq!(error.source.message, message);
            assert!(error.ambiguous_transaction_id.is_none());
            coordinator
                .lease(
                    &[ResolvedTransfer::new(
                        0,
                        first.source().clone(),
                        String::new(),
                        Lamport::from_atomic(1),
                    )],
                    false,
                )
                .expect("completed dispatch releases sources");
            client
                .health()
                .await
                .expect("second envelope was never dispatched");
            rpc.assert_finished();
        }
    }

    #[tokio::test]
    async fn cancellation_after_registration_detaches_only_the_waiter() {
        let coordinator = SourceCoordinator::default();
        let prepared = prepared(&coordinator);
        let envelope = prepared.envelopes()[0].clone();
        let rpc = Scripted::new([
            (
                "getBlockHeight",
                json!([{"commitment":"confirmed", "minContextSlot":11}]),
                json!(44),
            ),
            (
                "sendTransaction",
                json!([STANDARD.encode(envelope.signed_bytes()), {"encoding":"base64","skipPreflight":false,"preflightCommitment":"confirmed","minContextSlot":11,"maxRetries":0}]),
                json!(envelope.id().as_str()),
            ),
        ]);
        let tasks = Arc::new(Mutex::new(Vec::new()));
        let registrar = Arc::new(Registrar {
            outcome: Ok(()),
            tasks: Arc::clone(&tasks),
        });
        let submitter = Arc::new(Submitter::fixture(RpcClient::new(rpc.clone()), registrar));
        let cancellation = Cancellation::default();
        let waiter_cancellation = cancellation.clone();
        let waiter =
            tokio::spawn(async move { submitter.submit(prepared, &waiter_cancellation).await });
        tokio::task::yield_now().await;
        cancellation.cancel();
        assert!(waiter.await.expect("waiter").is_err());
        let task = tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .expect("registered task");
        task.run().await;
        rpc.assert_finished();
    }
}
