use std::{collections::BTreeMap, sync::Arc};

use wallets::{Error as WalletError, ErrorKind as WalletErrorKind, SendError};

use crate::{Address, Error, ErrorKind, Key, Lamport, Memo, Message, RpcClient};

use super::source::SourceLeases;
use super::{AcquiredAccounts, Cancellation, Envelope};

type MemoFactory = dyn Fn() -> Result<Memo, Error> + Send + Sync;

pub struct Preparer<C> {
    rpc: RpcClient<C>,
    memo: Arc<MemoFactory>,
}

pub struct PreparedBatch {
    floor: u64,
    envelopes: Vec<Envelope>,
    leases: SourceLeases,
}

impl PreparedBatch {
    #[must_use]
    pub const fn floor(&self) -> u64 {
        self.floor
    }

    #[must_use]
    pub fn envelopes(&self) -> &[Envelope] {
        &self.envelopes
    }

    pub(super) fn into_parts(self) -> (u64, Vec<Envelope>, SourceLeases) {
        (self.floor, self.envelopes, self.leases)
    }

    #[cfg(test)]
    pub(super) fn fixture(floor: u64, envelopes: Vec<Envelope>, leases: SourceLeases) -> Self {
        Self {
            floor,
            envelopes,
            leases,
        }
    }
}

impl<C> Preparer<C>
where
    C: json_rpc::Client,
{
    #[must_use]
    pub fn new(rpc: RpcClient<C>) -> Self {
        Self {
            rpc,
            memo: Arc::new(Memo::generate),
        }
    }

    pub async fn prepare(
        &self,
        acquired: AcquiredAccounts,
        keys: &BTreeMap<Address, Arc<Key>>,
        cancellation: &Cancellation,
    ) -> Result<PreparedBatch, SendError> {
        let (mut floor, transfers, balances, destinations, leases) = acquired.into_parts();
        cancellation.ensure()?;
        let lifetime = race(cancellation, self.rpc.latest_blockhash(floor)).await?;
        floor = lifetime.slot;

        let mut messages = Vec::with_capacity(transfers.len());
        let mut memo_values = std::collections::BTreeSet::new();
        let mut message_values = std::collections::BTreeSet::new();
        for (transfer, destination) in transfers.iter().zip(&destinations) {
            cancellation.ensure()?;
            let memo = (self.memo)().map_err(|_| {
                item(
                    transfer.index(),
                    WalletErrorKind::Generation,
                    "Solana Memo generation failed",
                )
            })?;
            if !memo_values.insert(memo) {
                return Err(item(
                    transfer.index(),
                    WalletErrorKind::Transaction,
                    "Solana Memo token must be unique",
                ));
            }
            let message = Message::native_transfer(
                transfer.source(),
                destination,
                transfer.amount(),
                memo,
                &lifetime.value,
            )
            .map_err(|_| {
                item(
                    transfer.index(),
                    WalletErrorKind::Transaction,
                    "Solana message construction failed",
                )
            })?;
            let bytes = message.wire_bytes().map_err(|_| {
                item(
                    transfer.index(),
                    WalletErrorKind::Transaction,
                    "Solana message encoding failed",
                )
            })?;
            if !message_values.insert(bytes.clone()) {
                return Err(item(
                    transfer.index(),
                    WalletErrorKind::Transaction,
                    "Solana messages must be distinct",
                ));
            }
            messages.push((message, bytes));
        }

        let mut fees = Vec::with_capacity(messages.len());
        for (_, bytes) in &messages {
            cancellation.ensure()?;
            let fee = race(cancellation, self.rpc.fee_for_message(bytes, floor)).await?;
            floor = fee.slot;
            fees.push(fee.value);
        }
        check_sufficiency(&transfers, &balances, &fees)?;

        let mut envelopes = Vec::with_capacity(messages.len());
        let mut signatures = std::collections::BTreeSet::new();
        let mut signed_values = std::collections::BTreeSet::new();
        for (transfer, (message, _)) in transfers.iter().zip(messages) {
            cancellation.ensure()?;
            let key = keys.get(transfer.source()).ok_or_else(|| {
                item(
                    transfer.index(),
                    WalletErrorKind::Transaction,
                    "Solana source signing key is unavailable",
                )
            })?;
            let envelope = Envelope::sign(
                transfer.source().clone(),
                transfer.index(),
                message,
                floor,
                lifetime.value.clone(),
                key,
            )
            .map_err(|_| {
                item(
                    transfer.index(),
                    WalletErrorKind::Transaction,
                    "Solana envelope signing failed",
                )
            })?;
            if !signatures.insert(envelope.id().as_str().to_owned())
                || !signed_values.insert(envelope.signed_bytes().to_vec())
            {
                return Err(item(
                    transfer.index(),
                    WalletErrorKind::Transaction,
                    "Solana signed envelopes must be distinct",
                ));
            }
            envelopes.push(envelope);
        }

        for envelope in &envelopes {
            cancellation.ensure()?;
            floor = simulate(cancellation, &self.rpc, envelope, floor).await?;
        }
        cancellation.ensure()?;
        Ok(PreparedBatch {
            floor,
            envelopes,
            leases,
        })
    }

    #[cfg(test)]
    fn with_memos(rpc: RpcClient<C>, memos: impl IntoIterator<Item = Memo>) -> Self {
        let values = Arc::new(std::sync::Mutex::new(
            memos.into_iter().collect::<std::collections::VecDeque<_>>(),
        ));
        Self {
            rpc,
            memo: Arc::new(move || {
                values
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop_front()
                    .ok_or_else(|| Error::new(ErrorKind::Generation, "fixture Memo exhausted"))
            }),
        }
    }
}

fn check_sufficiency(
    transfers: &[super::ResolvedTransfer],
    balances: &[Lamport],
    fees: &[Lamport],
) -> Result<(), SendError> {
    let mut available = BTreeMap::<Address, Lamport>::new();
    let mut required = BTreeMap::<Address, Lamport>::new();
    for ((transfer, balance), fee) in transfers.iter().zip(balances).zip(fees) {
        if available
            .insert(transfer.source().clone(), *balance)
            .is_some_and(|previous| previous != *balance)
        {
            return Err(operation("Solana source balance witness is inconsistent"));
        }
        let needed = transfer.amount().checked_add(*fee).ok_or_else(|| {
            item(
                transfer.index(),
                WalletErrorKind::Transaction,
                "Solana source requirement overflowed",
            )
        })?;
        let total = required
            .get(transfer.source())
            .copied()
            .unwrap_or(Lamport::ZERO)
            .checked_add(needed)
            .ok_or_else(|| {
                item(
                    transfer.index(),
                    WalletErrorKind::Transaction,
                    "Solana source requirement overflowed",
                )
            })?;
        required.insert(transfer.source().clone(), total);
        if total > *balance {
            return Err(item(
                transfer.index(),
                WalletErrorKind::Transaction,
                "Solana source balance is insufficient",
            ));
        }
    }
    Ok(())
}

async fn race<T>(
    cancellation: &Cancellation,
    future: impl std::future::Future<Output = Result<T, Error>>,
) -> Result<T, SendError> {
    tokio::select! {
        result = future => result.map_err(|_| operation("Solana transaction preparation failed")),
        () = cancellation.cancelled() => Err(operation("Solana transaction preparation was cancelled")),
    }
}

async fn simulate<C>(
    cancellation: &Cancellation,
    rpc: &RpcClient<C>,
    envelope: &Envelope,
    floor: u64,
) -> Result<u64, SendError>
where
    C: json_rpc::Client,
{
    tokio::select! {
        result = rpc.simulate(envelope.signed_bytes(), floor) => match result {
            Ok(slot) => Ok(slot),
            Err(error) if error.kind() == ErrorKind::Simulation => Err(item(
                envelope.index(),
                WalletErrorKind::Transaction,
                "Solana transaction simulation failed",
            )),
            Err(_) => Err(operation("Solana transaction simulation is unavailable")),
        },
        () = cancellation.cancelled() => Err(operation("Solana transaction preparation was cancelled")),
    }
}

fn item(index: usize, kind: WalletErrorKind, message: &'static str) -> SendError {
    SendError::item(index, Vec::new(), WalletError::new(kind, message))
}

fn operation(message: &'static str) -> SendError {
    SendError::operation(WalletErrorKind::Unavailable, message)
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;
    use solana_hash::Hash;

    use crate::{
        Acquirer, Batch, ResolvedTransfer, Seed, SourceCoordinator, rpc::test_support::Scripted,
    };

    use super::*;

    fn key(value: u8) -> Arc<Key> {
        Arc::new(
            Key::from_seed(hex::encode([value; 32]).parse::<Seed>().expect("seed")).expect("key"),
        )
    }

    #[test]
    fn cumulative_sufficiency_never_credits_incoming_transfers() {
        let source = Address::from_bytes([1; 32]);
        let transfers = [
            super::super::ResolvedTransfer::new(
                0,
                source.clone(),
                Address::from_bytes([2; 32]).to_string(),
                Lamport::from_atomic(4),
            ),
            super::super::ResolvedTransfer::new(
                1,
                source,
                Address::from_bytes([3; 32]).to_string(),
                Lamport::from_atomic(5),
            ),
        ];
        let error = check_sufficiency(
            &transfers,
            &[Lamport::from_atomic(10), Lamport::from_atomic(10)],
            &[Lamport::from_atomic(1), Lamport::from_atomic(1)],
        )
        .expect_err("second occurrence crosses the threshold");
        assert_eq!(error.failed_index, Some(1));

        check_sufficiency(
            &transfers,
            &[Lamport::from_atomic(11), Lamport::from_atomic(11)],
            &[Lamport::from_atomic(1), Lamport::from_atomic(1)],
        )
        .expect("exact balance");
    }

    #[tokio::test]
    async fn prepares_one_complete_envelope_in_strict_stage_order() {
        let signer = key(7);
        let source = signer.address().clone();
        let destination = key(8).address().clone();
        let system = Address::from_bytes([0; 32]);
        let transfer = ResolvedTransfer::new(
            0,
            source.clone(),
            destination.to_string(),
            Lamport::from_atomic(10),
        );
        let memo = Memo::from_bytes([3; Memo::LENGTH]);
        let lifetime = crate::BlockhashLifetime::new(Hash::new_from_array([9; 32]), 44);
        let message =
            Message::native_transfer(&source, &destination, transfer.amount(), memo, &lifetime)
                .expect("message fixture");
        let message_bytes = message.wire_bytes().expect("message bytes");
        let expected = Envelope::sign(source.clone(), 0, message, 8, lifetime.clone(), &signer)
            .expect("signed fixture");
        let account = json!({
            "lamports":100,
            "owner":system.to_string(),
            "executable":false,
            "data":["","base64"],
            "space":0
        });
        let rpc = Scripted::new([
            ("getHealth", json!([]), json!("ok")),
            ("getSlot", json!([{"commitment":"confirmed"}]), json!(5)),
            (
                "getMultipleAccounts",
                json!([[source.to_string(),destination.to_string()], {"encoding":"base64","commitment":"confirmed","minContextSlot":5}]),
                json!({"context":{"slot":5},"value":[account,null]}),
            ),
            (
                "getSlot",
                json!([{"commitment":"confirmed","minContextSlot":5}]),
                json!(6),
            ),
            (
                "getLatestBlockhash",
                json!([{"commitment":"confirmed","minContextSlot":6}]),
                json!({"context":{"slot":7},"value":{"blockhash":lifetime.blockhash().to_string(),"lastValidBlockHeight":44}}),
            ),
            (
                "getFeeForMessage",
                json!([STANDARD.encode(&message_bytes), {"commitment":"confirmed","minContextSlot":7}]),
                json!({"context":{"slot":8},"value":5}),
            ),
            (
                "simulateTransaction",
                json!([STANDARD.encode(expected.signed_bytes()), {"encoding":"base64","commitment":"confirmed","sigVerify":true,"replaceRecentBlockhash":false,"minContextSlot":8}]),
                json!({"context":{"slot":9},"value":{"err":null}}),
            ),
        ]);
        let client = RpcClient::new(rpc.clone());
        let acquired = Acquirer::new(client.clone(), SourceCoordinator::default())
            .acquire(
                Batch::new(vec![transfer]).expect("batch"),
                &Cancellation::default(),
            )
            .await
            .expect("account witness");
        let mut keys = BTreeMap::new();
        keys.insert(source, signer);
        let prepared = Preparer::with_memos(client, [memo])
            .prepare(acquired, &keys, &Cancellation::default())
            .await
            .expect("complete preparation");

        assert_eq!(prepared.floor(), 9);
        assert_eq!(prepared.envelopes().len(), 1);
        assert_eq!(
            prepared.envelopes()[0].signed_bytes(),
            expected.signed_bytes()
        );
        rpc.assert_finished();
    }
}
