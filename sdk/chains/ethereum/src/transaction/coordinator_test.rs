use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use alloy_primitives::keccak256;
use indexing::{BlockRef, BoxFuture, SourceError};

use super::*;
use crate::{AssetKind, BuildContext, Wei};

const CHAIN_ID: u64 = 31_337;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum BalanceKey {
    Native(Address),
    Token(Address, Address),
}

#[derive(Default)]
struct AccountStub {
    nonces: Mutex<BTreeMap<Address, u64>>,
    balances: Mutex<BTreeMap<BalanceKey, Wei>>,
}

impl AccountStub {
    fn set_nonce(&self, address: Address, nonce: u64) {
        self.nonces
            .lock()
            .expect("nonce lock must be healthy")
            .insert(address, nonce);
    }

    fn set_native_balance(&self, address: Address, balance: u128) {
        self.balances
            .lock()
            .expect("balance lock must be healthy")
            .insert(BalanceKey::Native(address), Wei::from_u128(balance));
    }

    fn set_token_balance(&self, address: Address, token: Address, balance: u128) {
        self.balances
            .lock()
            .expect("balance lock must be healthy")
            .insert(BalanceKey::Token(address, token), Wei::from_u128(balance));
    }
}

impl Accounts for AccountStub {
    fn balance<'a>(
        &'a self,
        address: Address,
        asset: &'a AssetKind,
        _at: Option<BlockRef>,
    ) -> BoxFuture<'a, Result<Wei, SourceError>> {
        Box::pin(async move {
            let key = match asset {
                AssetKind::Native => BalanceKey::Native(address),
                AssetKind::Erc20(token) => BalanceKey::Token(address, token.clone()),
            };
            Ok(self
                .balances
                .lock()
                .expect("balance lock must be healthy")
                .get(&key)
                .cloned()
                .unwrap_or_else(|| Wei::from_u128(1_000_000)))
        })
    }

    fn nonce<'a>(&'a self, address: Address) -> BoxFuture<'a, Result<u64, SourceError>> {
        Box::pin(async move {
            Ok(self
                .nonces
                .lock()
                .expect("nonce lock must be healthy")
                .get(&address)
                .copied()
                .unwrap_or(0))
        })
    }
}

enum BroadcastAction {
    Accept,
    Reject,
    Ambiguous,
    Pending,
}

#[derive(Default)]
struct TransactionStub {
    contexts: Mutex<Vec<(Address, u64)>>,
    broadcasts: Mutex<Vec<SignedTransaction>>,
    actions: Mutex<VecDeque<BroadcastAction>>,
    known_results: Mutex<VecDeque<Result<bool, SourceError>>>,
    known_ids: Mutex<Vec<TransactionId>>,
}

impl TransactionStub {
    fn actions(&self, actions: impl IntoIterator<Item = BroadcastAction>) {
        self.actions
            .lock()
            .expect("action lock must be healthy")
            .extend(actions);
    }

    fn known(&self, results: impl IntoIterator<Item = Result<bool, SourceError>>) {
        self.known_results
            .lock()
            .expect("known-result lock must be healthy")
            .extend(results);
    }
}

impl Transactions for TransactionStub {
    fn build_context<'a>(
        &'a self,
        request: &'a TransferRequest,
        nonce: u64,
    ) -> BoxFuture<'a, Result<BuildContext, ChainError>> {
        Box::pin(async move {
            self.contexts
                .lock()
                .expect("context lock must be healthy")
                .push((request.from().clone(), nonce));
            Ok(BuildContext {
                chain_id: CHAIN_ID,
                nonce,
                gas_limit: 1,
                max_fee_per_gas: Wei::from_u128(1),
                max_priority_fee_per_gas: Wei::from_u128(1),
            })
        })
    }

    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
    ) -> BoxFuture<'a, Result<TransactionId, base::TransactionError>> {
        Box::pin(async move {
            let id = transaction.id.clone();
            self.broadcasts
                .lock()
                .expect("broadcast lock must be healthy")
                .push(transaction);
            let action = self
                .actions
                .lock()
                .expect("action lock must be healthy")
                .pop_front()
                .unwrap_or(BroadcastAction::Accept);
            match action {
                BroadcastAction::Accept => Ok(id),
                BroadcastAction::Reject => Err(base::TransactionError::new(
                    base::TransactionErrorKind::Rejected,
                    "terminal rejection",
                )),
                BroadcastAction::Ambiguous => Err(base::TransactionError::new(
                    base::TransactionErrorKind::Unavailable,
                    "ambiguous submission",
                )
                .with_ambiguous_transaction_id(base::TransactionId::new(id.to_string()))),
                BroadcastAction::Pending => std::future::pending().await,
            }
        })
    }

    fn known<'a>(
        &'a self,
        transaction: &'a TransactionId,
    ) -> BoxFuture<'a, Result<bool, SourceError>> {
        Box::pin(async move {
            self.known_ids
                .lock()
                .expect("known-ID lock must be healthy")
                .push(transaction.clone());
            self.known_results
                .lock()
                .expect("known-result lock must be healthy")
                .pop_front()
                .unwrap_or(Ok(false))
        })
    }
}

fn signer(seed: u8) -> base::KeyPair<Address> {
    let secret = vec![seed; 32];
    let key = crypto::SecretKey::new(secret.clone()).expect("test key must be valid");
    let public = key
        .public_key(crypto::PublicKeyFormat::Raw)
        .expect("test public key must derive");
    let hash = keccak256(&public.bytes);
    let mut bytes = [0_u8; 20];
    bytes.copy_from_slice(&hash[12..]);
    base::KeyPair::new(Address(bytes), secret).expect("test signer must construct")
}

fn transfer(from: &Address, value: u128) -> TransferRequest {
    TransferRequest::native_atomic(from.clone(), Address([0x55; 20]), Wei::from_u128(value))
}

fn token_transfer(from: &Address, token: &Address, amount: u128) -> TransferRequest {
    TransferRequest::erc20(
        from.clone(),
        token.clone(),
        Address([0x55; 20]),
        Wei::from_u128(amount),
    )
}

fn coordinator(
    accounts: Arc<AccountStub>,
    transactions: Arc<TransactionStub>,
) -> TransactionCoordinator {
    TransactionCoordinator::new(accounts, transactions)
}

#[tokio::test]
async fn prepares_a_b_a_with_consecutive_per_sender_nonces_before_broadcast() {
    let signer_a = signer(1);
    let signer_b = signer(2);
    let accounts = Arc::new(AccountStub::default());
    accounts.set_nonce(signer_a.address.clone(), 5);
    accounts.set_nonce(signer_b.address.clone(), 9);
    let transactions = Arc::new(TransactionStub::default());
    let coordinator = coordinator(accounts, transactions.clone());

    let mut batch = coordinator
        .prepare_batch(vec![
            Preparation::signer(transfer(&signer_a.address, 1), CHAIN_ID, &signer_a),
            Preparation::signer(transfer(&signer_b.address, 2), CHAIN_ID, &signer_b),
            Preparation::signer(transfer(&signer_a.address, 3), CHAIN_ID, &signer_a),
        ])
        .await
        .expect("valid batch must prepare");

    assert_eq!(
        *transactions
            .contexts
            .lock()
            .expect("context lock must be healthy"),
        [
            (signer_a.address.clone(), 5),
            (signer_b.address.clone(), 9),
            (signer_a.address.clone(), 6),
        ]
    );
    assert!(
        transactions
            .broadcasts
            .lock()
            .expect("broadcast lock must be healthy")
            .is_empty()
    );
    for _ in 0..3 {
        assert!(
            batch
                .next()
                .await
                .expect("submission must succeed")
                .is_some()
        );
    }
    assert!(batch.next().await.expect("batch must finish").is_none());
}

#[tokio::test]
async fn aggregate_overspend_reports_first_threshold_crossing_without_broadcast() {
    let signer = signer(3);
    let accounts = Arc::new(AccountStub::default());
    accounts.set_native_balance(signer.address.clone(), 10);
    let transactions = Arc::new(TransactionStub::default());
    let coordinator = coordinator(accounts, transactions.clone());

    let result = coordinator
        .prepare_batch(vec![
            Preparation::signer(transfer(&signer.address, 4), CHAIN_ID, &signer),
            Preparation::signer(transfer(&signer.address, 5), CHAIN_ID, &signer),
        ])
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("aggregate value plus fees must exceed the balance"),
    };

    assert_eq!(error.index, 1);
    assert_eq!(error.source.kind, ChainErrorKind::InsufficientFunds);
    assert!(
        transactions
            .broadcasts
            .lock()
            .expect("broadcast lock must be healthy")
            .is_empty()
    );
}

#[tokio::test]
async fn cumulative_token_amount_and_native_gas_fail_before_any_broadcast() {
    let signer = signer(7);
    let token = Address([0x77; 20]);
    let accounts = Arc::new(AccountStub::default());
    accounts.set_native_balance(signer.address.clone(), 2);
    accounts.set_token_balance(signer.address.clone(), token.clone(), 10);
    let transactions = Arc::new(TransactionStub::default());
    let coordinator = coordinator(accounts.clone(), transactions.clone());
    let preparations = || {
        vec![
            Preparation::signer(
                token_transfer(&signer.address, &token, 6),
                CHAIN_ID,
                &signer,
            ),
            Preparation::signer(
                token_transfer(&signer.address, &token, 6),
                CHAIN_ID,
                &signer,
            ),
        ]
    };

    let token_error = match coordinator.prepare_batch(preparations()).await {
        Err(error) => error,
        Ok(_) => panic!("cumulative token amount must exceed the token balance"),
    };
    assert_eq!(token_error.index, 1);
    assert_eq!(token_error.source.kind, ChainErrorKind::InsufficientFunds);
    assert!(
        transactions
            .broadcasts
            .lock()
            .expect("broadcast lock must be healthy")
            .is_empty()
    );

    accounts.set_token_balance(signer.address.clone(), token.clone(), 12);
    accounts.set_native_balance(signer.address.clone(), 1);
    let gas_error = match coordinator.prepare_batch(preparations()).await {
        Err(error) => error,
        Ok(_) => panic!("cumulative maximum gas must exceed the native balance"),
    };
    assert_eq!(gas_error.index, 1);
    assert_eq!(gas_error.source.kind, ChainErrorKind::InsufficientFunds);
    assert!(
        transactions
            .broadcasts
            .lock()
            .expect("broadcast lock must be healthy")
            .is_empty()
    );
}

#[tokio::test]
async fn ambiguous_submission_reconciles_and_replays_the_exact_envelope() {
    let signer = signer(4);
    let accounts = Arc::new(AccountStub::default());
    accounts.set_nonce(signer.address.clone(), 5);
    let transactions = Arc::new(TransactionStub::default());
    transactions.actions([BroadcastAction::Ambiguous, BroadcastAction::Accept]);
    transactions.known([Ok(false)]);
    let coordinator = coordinator(accounts, transactions.clone());
    let signed = coordinator
        .prepare_one(Preparation::signer(
            transfer(&signer.address, 1),
            CHAIN_ID,
            &signer,
        ))
        .await
        .expect("transaction must prepare");

    let first = coordinator
        .broadcast(signed.clone())
        .await
        .expect_err("first submission must be ambiguous");
    assert_eq!(first.kind, base::TransactionErrorKind::Unavailable);
    assert_eq!(
        first.ambiguous_transaction_id,
        Some(base::TransactionId::new(signed.id.to_string()))
    );
    assert_eq!(
        coordinator
            .broadcast(signed.clone())
            .await
            .expect("exact replay must resolve"),
        signed.id
    );
    let broadcasts = transactions
        .broadcasts
        .lock()
        .expect("broadcast lock must be healthy");
    assert_eq!(broadcasts.as_slice(), [signed.clone(), signed]);
    assert_eq!(
        transactions
            .known_ids
            .lock()
            .expect("known-ID lock must be healthy")
            .len(),
        1
    );
}

#[tokio::test]
async fn new_same_sender_preparation_recovers_old_envelope_before_nonce_plus_one() {
    let signer = signer(8);
    let accounts = Arc::new(AccountStub::default());
    accounts.set_nonce(signer.address.clone(), 5);
    let transactions = Arc::new(TransactionStub::default());
    transactions.actions([
        BroadcastAction::Ambiguous,
        BroadcastAction::Accept,
        BroadcastAction::Accept,
    ]);
    transactions.known([Ok(false)]);
    let coordinator = coordinator(accounts, transactions.clone());
    let old = coordinator
        .prepare_one(Preparation::signer(
            transfer(&signer.address, 1),
            CHAIN_ID,
            &signer,
        ))
        .await
        .expect("old transaction must prepare");

    let error = coordinator
        .broadcast(old.clone())
        .await
        .expect_err("old submission must be ambiguous");
    assert_eq!(
        error.ambiguous_transaction_id,
        Some(base::TransactionId::new(old.id.to_string()))
    );
    let new = coordinator
        .prepare_one(Preparation::signer(
            transfer(&signer.address, 2),
            CHAIN_ID,
            &signer,
        ))
        .await
        .expect("new preparation must recover the old transaction first");
    coordinator
        .broadcast(new.clone())
        .await
        .expect("new transaction must submit");

    assert_eq!(
        transactions
            .contexts
            .lock()
            .expect("context lock must be healthy")
            .iter()
            .map(|(_, nonce)| *nonce)
            .collect::<Vec<_>>(),
        [5, 6]
    );
    let broadcasts = transactions
        .broadcasts
        .lock()
        .expect("broadcast lock must be healthy");
    assert_eq!(broadcasts.as_slice(), [old.clone(), old.clone(), new]);
    assert_eq!(
        *transactions
            .known_ids
            .lock()
            .expect("known-ID lock must be healthy"),
        [old.id]
    );
}

#[tokio::test]
async fn terminal_initial_rejection_releases_the_nonce_for_reuse() {
    let signer = signer(9);
    let accounts = Arc::new(AccountStub::default());
    accounts.set_nonce(signer.address.clone(), 5);
    let transactions = Arc::new(TransactionStub::default());
    transactions.actions([BroadcastAction::Reject]);
    let coordinator = coordinator(accounts, transactions.clone());
    let rejected = coordinator
        .prepare_one(Preparation::signer(
            transfer(&signer.address, 1),
            CHAIN_ID,
            &signer,
        ))
        .await
        .expect("rejected transaction must prepare");
    let error = coordinator
        .broadcast(rejected)
        .await
        .expect_err("scripted submission must be rejected");
    assert_eq!(error.kind, base::TransactionErrorKind::Rejected);
    assert_eq!(error.ambiguous_transaction_id, None);

    let replacement = coordinator
        .prepare_one(Preparation::signer(
            transfer(&signer.address, 2),
            CHAIN_ID,
            &signer,
        ))
        .await
        .expect("terminal rejection must release the nonce");
    coordinator
        .broadcast(replacement)
        .await
        .expect("replacement must submit");
    assert_eq!(
        transactions
            .contexts
            .lock()
            .expect("context lock must be healthy")
            .iter()
            .map(|(_, nonce)| *nonce)
            .collect::<Vec<_>>(),
        [5, 5]
    );
}

#[tokio::test]
async fn cancellation_after_submission_starts_retains_unknown_for_exact_recovery() {
    let signer = signer(5);
    let accounts = Arc::new(AccountStub::default());
    let transactions = Arc::new(TransactionStub::default());
    transactions.actions([BroadcastAction::Pending, BroadcastAction::Accept]);
    transactions.known([Ok(false)]);
    let coordinator = coordinator(accounts, transactions.clone());
    let signed = coordinator
        .prepare_one(Preparation::signer(
            transfer(&signer.address, 1),
            CHAIN_ID,
            &signer,
        ))
        .await
        .expect("transaction must prepare");

    let mut submission = Box::pin(coordinator.broadcast(signed.clone()));
    let waker = futures_util::task::noop_waker_ref();
    let mut context = Context::from_waker(waker);
    assert_eq!(submission.as_mut().poll(&mut context), Poll::Pending);
    drop(submission);

    assert_eq!(
        coordinator
            .broadcast(signed.clone())
            .await
            .expect("cancelled submission must recover exact bytes"),
        signed.id
    );
    let broadcasts = transactions
        .broadcasts
        .lock()
        .expect("broadcast lock must be healthy");
    assert_eq!(broadcasts.as_slice(), [signed.clone(), signed]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_same_sender_operations_never_reuse_a_nonce() {
    let signer = Arc::new(signer(6));
    let accounts = Arc::new(AccountStub::default());
    accounts.set_nonce(signer.address.clone(), 5);
    let transactions = Arc::new(TransactionStub::default());
    let coordinator = coordinator(accounts, transactions.clone());
    let (prepared, prepared_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());

    let first = {
        let coordinator = coordinator.clone();
        let signer = signer.clone();
        let release = release.clone();
        tokio::spawn(async move {
            let signed = coordinator
                .prepare_one(Preparation::signer(
                    transfer(&signer.address, 1),
                    CHAIN_ID,
                    signer.as_ref(),
                ))
                .await
                .expect("first transaction must prepare");
            prepared.send(()).expect("test receiver must remain");
            release.notified().await;
            coordinator.broadcast(signed).await
        })
    };
    prepared_rx.await.expect("first transaction must prepare");
    let second = {
        let coordinator = coordinator.clone();
        let signer = signer.clone();
        tokio::spawn(async move {
            let signed = coordinator
                .prepare_one(Preparation::signer(
                    transfer(&signer.address, 2),
                    CHAIN_ID,
                    signer.as_ref(),
                ))
                .await
                .expect("second transaction must prepare");
            coordinator.broadcast(signed).await
        })
    };
    tokio::task::yield_now().await;
    release.notify_one();

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        first
            .await
            .expect("first task must join")
            .expect("first submission must succeed");
        second
            .await
            .expect("second task must join")
            .expect("second submission must succeed");
    })
    .await
    .expect("same-sender coordination must not hang");
    assert_eq!(
        transactions
            .contexts
            .lock()
            .expect("context lock must be healthy")
            .iter()
            .map(|(_, nonce)| *nonce)
            .collect::<Vec<_>>(),
        [5, 6]
    );
}
