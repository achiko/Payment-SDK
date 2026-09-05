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

        register(self.registrar.as_ref(), task, activation, cancellation).await?;

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
}

async fn register(
    registrar: &dyn SubmissionRegistrar,
    task: SubmissionTask,
    activation: Activation,
    cancellation: &Cancellation,
) -> Result<(), SendError> {
    tokio::select! {
        result = registrar.register(task) => {
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
        if position != 0 {
            let height = match rpc.block_height(floor).await {
                Ok(height) => height,
                Err(_) => {
                    return Outcome::Complete(Err(definite(
                        envelope.index(),
                        accepted.clone(),
                        "Solana block height is unavailable before dispatch",
                    )));
                }
            };
            if height > envelope.lifetime().last_valid_block_height() {
                return Outcome::Complete(Err(definite(
                    envelope.index(),
                    accepted,
                    "Solana transaction lifetime expired before dispatch",
                )));
            }
        }

        let mut submitted = false;
        for attempt in 0..3 {
            match rpc
                .send_transaction(envelope.signed_bytes(), floor, envelope.id().clone())
                .await
            {
                Ok(()) => {
                    submitted = true;
                    break;
                }
                Err(_) => match rpc.signature_status(envelope.id(), floor).await {
                    Ok(status) if status.value.is_some() => {
                        submitted = true;
                        break;
                    }
                    Ok(_) if attempt < 2 => match rpc.block_height(floor).await {
                        Ok(height) if height <= envelope.lifetime().last_valid_block_height() => {}
                        _ => {
                            return ambiguous_outcome(envelope, accepted, guarded);
                        }
                    },
                    _ => {
                        return ambiguous_outcome(envelope, accepted, guarded);
                    }
                },
            }
        }
        if !submitted {
            return ambiguous_outcome(envelope, accepted, guarded);
        }
        accepted.push(envelope.id().clone());
    }
    Outcome::Complete(Ok(accepted))
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

fn ambiguous_outcome(
    envelope: Envelope,
    accepted: Vec<TransactionId>,
    guarded: GuardedSources,
) -> Outcome {
    guarded.retain_ambiguity(envelope.source());
    let error = TransactionError::new(
        TransactionErrorKind::Unknown,
        "Solana submission outcome is unknown",
    )
    .with_ambiguous_transaction_id(envelope.id().clone());
    Outcome::Ambiguous {
        error: SendError::item(envelope.index(), accepted, WalletError::from(error)),
        envelope: Box::new(envelope),
    }
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
