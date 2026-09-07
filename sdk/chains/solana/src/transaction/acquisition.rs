use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::Notify;
use wallets::{Error as WalletError, ErrorKind as WalletErrorKind, SendError};

use crate::{
    AccountSnapshot, Address, Batch, Lamport, NativeDestination, RpcClient, RpcCommitment,
};

use super::SourceCoordinator;
use super::source::SourceLeases;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedTransfer {
    index: usize,
    source: Address,
    destination: String,
    amount: Lamport,
}

impl ResolvedTransfer {
    #[must_use]
    pub fn new(index: usize, source: Address, destination: String, amount: Lamport) -> Self {
        Self {
            index,
            source,
            destination,
            amount,
        }
    }

    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    #[must_use]
    pub const fn source(&self) -> &Address {
        &self.source
    }

    #[must_use]
    pub const fn amount(&self) -> Lamport {
        self.amount
    }

    fn destination(&self) -> Result<Address, SendError> {
        let parsed = self.destination.parse::<Address>().map_err(|_| {
            SendError::item(
                self.index,
                Vec::new(),
                WalletError::new(
                    WalletErrorKind::InvalidAddress,
                    "invalid Solana destination",
                ),
            )
        })?;
        let destination = NativeDestination::try_from(parsed).map_err(|_| {
            SendError::item(
                self.index,
                Vec::new(),
                WalletError::new(
                    WalletErrorKind::Unsupported,
                    "unsupported Solana native destination",
                ),
            )
        })?;
        if destination.address() == &self.source {
            return Err(SendError::item(
                self.index,
                Vec::new(),
                WalletError::new(
                    WalletErrorKind::AddressMismatch,
                    "Solana source and destination must differ",
                ),
            ));
        }
        Ok(destination.address().clone())
    }
}

#[derive(Clone, Default)]
pub struct Cancellation {
    state: Arc<CancelState>,
}

#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl Cancellation {
    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Release);
        self.state.notify.notify_waiters();
    }

    pub(super) async fn cancelled(&self) {
        if self.state.cancelled.load(Ordering::Acquire) {
            return;
        }
        self.state.notify.notified().await;
    }

    pub(super) fn ensure(&self) -> Result<(), SendError> {
        if self.state.cancelled.load(Ordering::Acquire) {
            return Err(SendError::operation(
                WalletErrorKind::Unavailable,
                "Solana account acquisition was cancelled",
            ));
        }
        Ok(())
    }

    async fn race_acquisition<T>(
        &self,
        future: impl Future<Output = Result<T, crate::Error>>,
    ) -> Result<T, SendError> {
        tokio::select! {
            result = future => result.map_err(|_| SendError::operation(WalletErrorKind::Unavailable, "Solana account acquisition failed")),
            () = self.cancelled() => Err(SendError::operation(WalletErrorKind::Unavailable, "Solana account acquisition was cancelled")),
        }
    }
}

pub struct AcquiredAccounts {
    floor: u64,
    transfers: Vec<ResolvedTransfer>,
    balances: Vec<Lamport>,
    destinations: Vec<Address>,
    leases: SourceLeases,
}

impl AcquiredAccounts {
    #[must_use]
    pub const fn floor(&self) -> u64 {
        self.floor
    }

    #[must_use]
    pub fn balances(&self) -> &[Lamport] {
        &self.balances
    }

    #[must_use]
    pub fn transfers(&self) -> &[ResolvedTransfer] {
        &self.transfers
    }

    #[must_use]
    pub fn destinations(&self) -> &[Address] {
        &self.destinations
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        u64,
        Vec<ResolvedTransfer>,
        Vec<Lamport>,
        Vec<Address>,
        SourceLeases,
    ) {
        (
            self.floor,
            self.transfers,
            self.balances,
            self.destinations,
            self.leases,
        )
    }
}

pub struct Acquirer<C> {
    rpc: RpcClient<C>,
    sources: SourceCoordinator,
}

impl<C> Acquirer<C>
where
    C: json_rpc::Client,
{
    #[must_use]
    pub fn new(rpc: RpcClient<C>, sources: SourceCoordinator) -> Self {
        Self { rpc, sources }
    }

    pub async fn acquire(
        &self,
        transfers: Batch<ResolvedTransfer>,
        cancellation: &Cancellation,
    ) -> Result<AcquiredAccounts, SendError> {
        let items = transfers.as_slice();
        if items
            .iter()
            .enumerate()
            .any(|(index, transfer)| transfer.index != index)
        {
            return Err(SendError::collection(
                WalletErrorKind::InvalidBatch,
                "Solana transfer indices must preserve authored order",
            ));
        }
        let leases = self.sources.lease(items, items.len() > 1)?;
        cancellation.ensure()?;
        let destinations = items
            .iter()
            .map(ResolvedTransfer::destination)
            .collect::<Result<Vec<_>, _>>()?;
        cancellation.ensure()?;
        let mut seen = BTreeSet::new();
        let query = items
            .iter()
            .zip(&destinations)
            .flat_map(|(item, destination)| [&item.source, destination])
            .filter(|address| seen.insert(*address))
            .cloned()
            .collect::<Vec<_>>();

        cancellation.race_acquisition(self.rpc.health()).await?;
        cancellation.ensure()?;
        let opening = cancellation
            .race_acquisition(self.rpc.slot(RpcCommitment::Confirmed, None))
            .await?;
        cancellation.ensure()?;
        let context = cancellation
            .race_acquisition(
                self.rpc
                    .accounts(&query, RpcCommitment::Confirmed, Some(opening)),
            )
            .await?;
        cancellation.ensure()?;
        let closing = cancellation
            .race_acquisition(self.rpc.slot(RpcCommitment::Confirmed, Some(context.slot)))
            .await?;
        cancellation.ensure()?;

        let observed = query
            .into_iter()
            .zip(context.value)
            .collect::<BTreeMap<_, _>>();
        let balances = classify(items, &destinations, &observed)?;
        cancellation.ensure()?;
        Ok(AcquiredAccounts {
            floor: closing,
            transfers: items.to_vec(),
            balances,
            destinations,
            leases,
        })
    }
}

fn classify(
    items: &[ResolvedTransfer],
    destinations: &[Address],
    observed: &BTreeMap<Address, Option<AccountSnapshot>>,
) -> Result<Vec<Lamport>, SendError> {
    let mut balances = Vec::with_capacity(items.len());
    for (item, destination) in items.iter().zip(destinations) {
        let source = observed.get(&item.source).ok_or_else(|| {
            SendError::operation(
                WalletErrorKind::Unavailable,
                "Solana source observation is missing",
            )
        })?;
        if source
            .as_ref()
            .is_some_and(|account| !account.supports_native_transfer())
        {
            return Err(SendError::item(
                item.index,
                Vec::new(),
                WalletError::new(
                    WalletErrorKind::Unsupported,
                    "unsupported Solana source account",
                ),
            ));
        }
        let destination = observed.get(destination).ok_or_else(|| {
            SendError::operation(
                WalletErrorKind::Unavailable,
                "Solana destination observation is missing",
            )
        })?;
        if destination
            .as_ref()
            .is_some_and(|account| !account.supports_native_transfer())
        {
            return Err(SendError::item(
                item.index,
                Vec::new(),
                WalletError::new(
                    WalletErrorKind::Unsupported,
                    "unsupported Solana destination account",
                ),
            ));
        }
        balances.push(
            source
                .as_ref()
                .map_or(Lamport::ZERO, AccountSnapshot::lamports),
        );
    }
    Ok(balances)
}

impl AccountSnapshot {
    fn supports_native_transfer(&self) -> bool {
        !self.executable()
            && self.owner() == &Address::from_bytes([0; 32])
            && self.data().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use json_rpc::{BoxFuture, Call, CallResult, Error as RpcError, RawJson};
    use serde_json::json;
    use solana_keypair::{Keypair, Signer as _};

    use crate::rpc::test_support::Scripted;

    use super::*;

    #[tokio::test]
    async fn acquisition_failures_preserve_operation_scope_and_redacted_messages() {
        let source = Address::from_bytes([1; 32]);
        let destination = Address::from_bytes([2; 32]);
        let items = [ResolvedTransfer::new(
            0,
            source.clone(),
            destination.to_string(),
            Lamport::from_atomic(1),
        )];
        let missing_source =
            classify(&items, std::slice::from_ref(&destination), &BTreeMap::new()).unwrap_err();
        let missing_destination =
            classify(&items, &[destination], &BTreeMap::from([(source, None)])).unwrap_err();
        let transport = Cancellation::default()
            .race_acquisition::<()>(async {
                Err(crate::Error::new(
                    crate::ErrorKind::RpcTimeout,
                    "provider secret",
                ))
            })
            .await
            .unwrap_err();
        for (failure, message) in [
            (missing_source, "Solana source observation is missing"),
            (
                missing_destination,
                "Solana destination observation is missing",
            ),
            (transport, "Solana account acquisition failed"),
        ] {
            assert_eq!(failure.source.kind, WalletErrorKind::Unavailable);
            assert_eq!(failure.to_string(), message);
            assert_eq!(failure.failed_index, None);
            assert!(failure.accepted.is_empty());
            assert_eq!(failure.ambiguous_transaction_id, None);
        }
    }

    #[derive(Clone)]
    struct Blocking {
        block_at: usize,
        calls: Arc<AtomicUsize>,
        entered: Arc<Notify>,
    }

    impl json_rpc::Client for Blocking {
        fn request<'a>(
            &'a self,
            method: &'a str,
            params: serde_json::Value,
        ) -> BoxFuture<'a, Result<CallResult, RpcError>> {
            self.request_once(method, params)
        }

        fn request_once<'a>(
            &'a self,
            method: &'a str,
            _params: serde_json::Value,
        ) -> BoxFuture<'a, Result<CallResult, RpcError>> {
            Box::pin(async move {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call == self.block_at {
                    self.entered.notify_one();
                    std::future::pending::<()>().await;
                }
                let system = Address::from_bytes([0; 32]);
                let value = match call {
                    0 => {
                        assert_eq!(method, "getHealth");
                        json!("ok")
                    }
                    1 => {
                        assert_eq!(method, "getSlot");
                        json!(1)
                    }
                    2 => {
                        assert_eq!(method, "getMultipleAccounts");
                        json!({"context":{"slot":1},"value":[account(&system,10,false,"",0),null]})
                    }
                    3 => {
                        assert_eq!(method, "getSlot");
                        json!(1)
                    }
                    _ => panic!("unexpected downstream call"),
                };
                Ok(Ok(RawJson::from_serializable(&value).unwrap()))
            })
        }

        fn batch<'a>(
            &'a self,
            _calls: Vec<Call>,
        ) -> BoxFuture<'a, Result<Vec<CallResult>, RpcError>> {
            Box::pin(async { unreachable!("account acquisition never batches JSON-RPC envelopes") })
        }
    }

    fn address(seed: u8) -> Address {
        Address::from_bytes(Keypair::new_from_array([seed; 32]).pubkey().to_bytes())
    }

    fn transfer(index: usize, source: &Address, destination: &Address) -> ResolvedTransfer {
        ResolvedTransfer::new(
            index,
            source.clone(),
            destination.to_string(),
            Lamport::from_atomic(1),
        )
    }

    fn account(
        owner: &Address,
        lamports: u64,
        executable: bool,
        data: &str,
        space: u64,
    ) -> serde_json::Value {
        json!({"lamports":lamports,"owner":owner.to_string(),"executable":executable,"data":[data,"base64"],"space":space})
    }

    #[tokio::test]
    async fn performs_one_witnessed_stable_query_and_publishes_atomically() {
        let source = address(7);
        let destination = address(8);
        let other = address(9);
        let system = Address::from_bytes([0; 32]);
        let rpc = Scripted::new([
            ("getHealth", json!([]), json!("ok")),
            ("getSlot", json!([{"commitment":"confirmed"}]), json!(10)),
            (
                "getMultipleAccounts",
                json!([[source.to_string(),destination.to_string(),other.to_string()], {"encoding":"base64","commitment":"confirmed","minContextSlot":10}]),
                json!({"context":{"slot":11},"value":[account(&system,20,false,"",0),null,account(&system,30,false,"",0)]}),
            ),
            (
                "getSlot",
                json!([{"commitment":"confirmed","minContextSlot":11}]),
                json!(12),
            ),
        ]);
        let client = RpcClient::new(rpc.clone());
        let sources = SourceCoordinator::default();
        let acquirer = Acquirer::new(client, sources.clone());
        let batch = Batch::new(vec![
            transfer(0, &source, &destination),
            transfer(1, &other, &destination),
            transfer(2, &source, &other),
            transfer(3, &destination, &source),
        ])
        .unwrap();
        let acquired = acquirer
            .acquire(batch, &Cancellation::default())
            .await
            .unwrap();
        assert_eq!(acquired.floor(), 12);
        assert_eq!(
            acquired.destinations(),
            &[destination.clone(), destination, other, source.clone()]
        );
        assert_eq!(
            acquired
                .balances()
                .iter()
                .map(|value| value.atomic())
                .collect::<Vec<_>>(),
            [20, 30, 20, 0]
        );
        rpc.assert_finished();
        assert!(
            sources
                .lease(&[transfer(0, &source, &address(10))], false)
                .is_err()
        );
        drop(acquired);
        assert!(
            sources
                .lease(&[transfer(0, &source, &address(10))], false)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn rejects_destination_and_self_transfer_before_rpc_and_releases_leases() {
        let source = address(7);
        let rpc = Scripted::new([]);
        let sources = SourceCoordinator::default();
        let acquirer = Acquirer::new(RpcClient::new(rpc.clone()), sources.clone());
        for (destination, kind, message) in [
            (
                "bad".to_owned(),
                WalletErrorKind::InvalidAddress,
                "invalid Solana destination",
            ),
            (
                "2fnQrngrQT4SeLcdToJAD96phoEjNL2man2kfRLCASVk".to_owned(),
                WalletErrorKind::Unsupported,
                "unsupported Solana native destination",
            ),
            (
                source.to_string(),
                WalletErrorKind::AddressMismatch,
                "Solana source and destination must differ",
            ),
        ] {
            let failure = acquirer
                .acquire(
                    Batch::new(vec![ResolvedTransfer::new(
                        0,
                        source.clone(),
                        destination,
                        Lamport::from_atomic(1),
                    )])
                    .unwrap(),
                    &Cancellation::default(),
                )
                .await
                .err()
                .expect("destination rejection");
            assert_eq!(failure.failed_index, Some(0));
            assert_eq!(failure.source.kind, kind);
            assert_eq!(failure.source.message, message);
            assert!(failure.accepted.is_empty());
            assert_eq!(failure.ambiguous_transaction_id, None);
            assert!(
                sources
                    .lease(&[transfer(0, &source, &address(8))], false)
                    .is_ok()
            );
        }
        rpc.assert_finished();
    }

    #[tokio::test]
    async fn rejects_non_authored_internal_indices_before_leasing_or_rpc() {
        let source = address(7);
        let destination = address(8);
        let sources = SourceCoordinator::default();
        let failure = Acquirer::new(RpcClient::new(Scripted::new([])), sources.clone())
            .acquire(
                Batch::new(vec![ResolvedTransfer::new(
                    4,
                    source.clone(),
                    destination.to_string(),
                    Lamport::from_atomic(1),
                )])
                .unwrap(),
                &Cancellation::default(),
            )
            .await
            .err()
            .expect("invalid occurrence index");
        assert_eq!(failure.source.kind, WalletErrorKind::InvalidBatch);
        assert!(
            sources
                .lease(&[transfer(0, &source, &destination)], false)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn semantic_failure_uses_earliest_item_and_publishes_no_floor() {
        let source = address(7);
        let destination = address(8);
        let second_destination = address(10);
        let unsupported_owner = address(9);
        let rpc = Scripted::new([
            ("getHealth", json!([]), json!("ok")),
            ("getSlot", json!([{"commitment":"confirmed"}]), json!(1)),
            (
                "getMultipleAccounts",
                json!([[source.to_string(),destination.to_string(),second_destination.to_string()], {"encoding":"base64","commitment":"confirmed","minContextSlot":1}]),
                json!({"context":{"slot":1},"value":[account(&unsupported_owner,1,false,"",0),null,null]}),
            ),
            (
                "getSlot",
                json!([{"commitment":"confirmed","minContextSlot":1}]),
                json!(1),
            ),
        ]);
        let sources = SourceCoordinator::default();
        let failure = Acquirer::new(RpcClient::new(rpc.clone()), sources.clone())
            .acquire(
                Batch::new(vec![
                    transfer(0, &source, &destination),
                    transfer(1, &source, &second_destination),
                ])
                .unwrap(),
                &Cancellation::default(),
            )
            .await
            .err()
            .expect("semantic rejection");
        assert_eq!(failure.failed_index, Some(0));
        assert_eq!(failure.source.kind, WalletErrorKind::Unsupported);
        assert!(
            sources
                .lease(&[transfer(0, &source, &address(11))], false)
                .is_ok()
        );
        rpc.assert_finished();
    }

    #[tokio::test]
    async fn cancellation_before_rpc_is_index_free_and_releases_sources() {
        let source = address(7);
        let destination = address(8);
        let cancellation = Cancellation::default();
        cancellation.cancel();
        let sources = SourceCoordinator::default();
        let failure = Acquirer::new(RpcClient::new(Scripted::new([])), sources.clone())
            .acquire(
                Batch::new(vec![transfer(0, &source, &destination)]).unwrap(),
                &cancellation,
            )
            .await
            .err()
            .expect("cancelled acquisition");
        assert_eq!(failure.failed_index, None);
        assert_eq!(failure.source.kind, WalletErrorKind::Unavailable);
        assert_eq!(
            failure.to_string(),
            "Solana account acquisition was cancelled"
        );
        assert!(failure.accepted.is_empty());
        assert_eq!(failure.ambiguous_transaction_id, None);
        assert!(
            sources
                .lease(&[transfer(0, &source, &destination)], false)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn acquisition_preserves_first_appearance_through_public_maximum() {
        let mut items = Vec::new();
        let mut expected = Vec::new();
        for index in 0..wallets::MAX_TRANSFERS {
            let source = address(u8::try_from(index + 1).unwrap());
            let destination = address(u8::try_from(index + 101).unwrap());
            items.push(transfer(index, &source, &destination));
            expected.push(source.to_string());
            expected.push(destination.to_string());
        }
        assert_eq!(expected.len(), 100);
        let rpc = Scripted::new([
            ("getHealth", json!([]), json!("ok")),
            ("getSlot", json!([{"commitment":"confirmed"}]), json!(10)),
            (
                "getMultipleAccounts",
                json!([expected, {"encoding":"base64","commitment":"confirmed","minContextSlot":10}]),
                json!({"context":{"slot":11},"value":vec![serde_json::Value::Null;100]}),
            ),
            (
                "getSlot",
                json!([{"commitment":"confirmed","minContextSlot":11}]),
                json!(12),
            ),
        ]);
        let acquired = Acquirer::new(RpcClient::new(rpc.clone()), SourceCoordinator::default())
            .acquire(Batch::new(items).unwrap(), &Cancellation::default())
            .await
            .expect("one complete maximum-size query");
        assert_eq!(acquired.floor(), 12);
        assert_eq!(
            acquired.balances(),
            vec![Lamport::ZERO; wallets::MAX_TRANSFERS]
        );
        rpc.assert_finished();
    }

    #[test]
    fn classification_preserves_earliest_occurrence_and_source_before_destination_errors() {
        let source = address(7);
        let destination = address(8);
        let other = address(9);
        let items = [
            transfer(0, &source, &destination),
            transfer(1, &destination, &other),
        ];
        let unsupported = AccountSnapshot::new(other.clone(), Lamport::ZERO, false, Vec::new());
        for (first_source, message) in [
            (None, "unsupported Solana destination account"),
            (
                Some(unsupported.clone()),
                "unsupported Solana source account",
            ),
        ] {
            let observed = BTreeMap::from([
                (source.clone(), first_source),
                (destination.clone(), Some(unsupported.clone())),
                (other.clone(), Some(unsupported.clone())),
            ]);
            let failure =
                classify(&items, &[destination.clone(), other.clone()], &observed).unwrap_err();
            assert_eq!(failure.failed_index, Some(0));
            assert_eq!(failure.source.kind, WalletErrorKind::Unsupported);
            assert_eq!(failure.source.message, message);
            assert!(failure.accepted.is_empty());
            assert_eq!(failure.ambiguous_transaction_id, None);
        }
    }

    #[test]
    fn classification_rejects_every_unsupported_native_account_shape() {
        let source = address(7);
        let destination = address(8);
        let system = Address::from_bytes([0; 32]);
        let other = address(9);
        let item = transfer(0, &source, &destination);
        for (owner, executable, data, failed) in [
            (system.clone(), false, Vec::new(), false),
            (other, false, Vec::new(), true),
            (system.clone(), true, Vec::new(), true),
            (system.clone(), false, vec![1], true),
        ] {
            let snapshot = AccountSnapshot::new(owner, Lamport::from_atomic(1), executable, data);
            for failing_address in [&source, &destination] {
                let mut observed =
                    BTreeMap::from([(source.clone(), None), (destination.clone(), None)]);
                observed.insert(failing_address.clone(), Some(snapshot.clone()));
                let result = classify(
                    std::slice::from_ref(&item),
                    std::slice::from_ref(&destination),
                    &observed,
                );
                assert_eq!(result.is_err(), failed);
                if !failed {
                    let balances = result.unwrap();
                    let expected = if failing_address == &source { 1 } else { 0 };
                    assert_eq!(balances[0].atomic(), expected);
                }
            }
        }
    }

    #[tokio::test]
    async fn cancellation_at_every_rpc_await_stops_downstream_work_and_releases_lease() {
        for block_at in 0..4 {
            let source = address(7);
            let destination = address(8);
            let calls = Arc::new(AtomicUsize::new(0));
            let entered = Arc::new(Notify::new());
            let transport = Blocking {
                block_at,
                calls: Arc::clone(&calls),
                entered: Arc::clone(&entered),
            };
            let sources = SourceCoordinator::default();
            let acquirer = Acquirer::new(RpcClient::new(transport), sources.clone());
            let cancellation = Cancellation::default();
            let task_cancellation = cancellation.clone();
            let task_source = source.clone();
            let task_destination = destination.clone();
            let task = tokio::spawn(async move {
                acquirer
                    .acquire(
                        Batch::new(vec![transfer(0, &task_source, &task_destination)]).unwrap(),
                        &task_cancellation,
                    )
                    .await
            });
            entered.notified().await;
            cancellation.cancel();
            let failure = task.await.unwrap().err().expect("cancelled RPC await");
            assert_eq!(failure.failed_index, None);
            assert_eq!(
                failure.to_string(),
                "Solana account acquisition was cancelled"
            );
            assert!(failure.accepted.is_empty());
            assert_eq!(failure.ambiguous_transaction_id, None);
            assert_eq!(calls.load(Ordering::SeqCst), block_at + 1);
            assert!(
                sources
                    .lease(&[transfer(0, &source, &destination)], false)
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn maximum_slot_is_published_only_after_a_matching_closing_witness() {
        let source = address(7);
        let destination = address(8);
        let system = Address::from_bytes([0; 32]);
        let successful = Scripted::new([
            ("getHealth", json!([]), json!("ok")),
            (
                "getSlot",
                json!([{"commitment":"confirmed"}]),
                json!(u64::MAX),
            ),
            (
                "getMultipleAccounts",
                json!([[source.to_string(),destination.to_string()], {"encoding":"base64","commitment":"confirmed","minContextSlot":u64::MAX}]),
                json!({"context":{"slot":u64::MAX},"value":[account(&system,1,false,"",0),null]}),
            ),
            (
                "getSlot",
                json!([{"commitment":"confirmed","minContextSlot":u64::MAX}]),
                json!(u64::MAX),
            ),
        ]);
        let acquired = Acquirer::new(RpcClient::new(successful), SourceCoordinator::default())
            .acquire(
                Batch::new(vec![transfer(0, &source, &destination)]).unwrap(),
                &Cancellation::default(),
            )
            .await
            .expect("self-consistent maximum witness");
        assert_eq!(acquired.floor(), u64::MAX);
        drop(acquired);

        let failed = Scripted::new([
            ("getHealth", json!([]), json!("ok")),
            (
                "getSlot",
                json!([{"commitment":"confirmed"}]),
                json!(u64::MAX),
            ),
            (
                "getMultipleAccounts",
                json!([[source.to_string(),destination.to_string()], {"encoding":"base64","commitment":"confirmed","minContextSlot":u64::MAX}]),
                json!({"context":{"slot":u64::MAX},"value":[account(&system,1,false,"",0),null]}),
            ),
            (
                "getSlot",
                json!([{"commitment":"confirmed","minContextSlot":u64::MAX}]),
                json!(u64::MAX - 1),
            ),
        ]);
        let sources = SourceCoordinator::default();
        let failure = Acquirer::new(RpcClient::new(failed), sources.clone())
            .acquire(
                Batch::new(vec![transfer(0, &source, &destination)]).unwrap(),
                &Cancellation::default(),
            )
            .await
            .err()
            .expect("unclosed maximum claim");
        assert_eq!(failure.failed_index, None);
        assert!(
            sources
                .lease(&[transfer(0, &source, &destination)], false)
                .is_ok()
        );
    }
}
