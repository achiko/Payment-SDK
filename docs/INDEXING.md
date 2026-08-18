# Indexing model

The goal is a watched-address payment index, not a full general-purpose block
explorer. Generic indexing controls synchronization; each chain interprets its
own blocks and transactions.

## Contracts

- [`BlockSource`](../sdk/indexing/src/source.rs) exposes the observed tip,
  canonical block at a height, and canonical hash lookup.
- [`IndexedBlock`](../sdk/indexing/src/source.rs) exposes height, hash, parent
  hash, and optional timestamp through `BlockRef`.
- [`BlockInterpreter`](../sdk/indexing/src/source.rs) converts a chain-native
  block and active watch targets into events plus chain-owned undo data.
- The focused repository traits in [`store.rs`](../sdk/indexing/src/store.rs)
  join block/undo, observation, confirmation, checkpoint, and feed mutations in
  one atomic semantic command without a god repository marker.
- [`Worker`](../sdk/indexing/src/service.rs) exposes sync and checkpoint
  status to the IX worker.
- [`Watcher`](../sdk/indexing/src/indexer.rs) registers and removes address and
  transaction-ID watches.
- [`Checkpoint`](../sdk/indexing/src/indexer.rs) reads the current canonical
  boundary used as the birthday for a watch created before an external action.
- [`History`](../sdk/indexing/src/indexer.rs) returns normalized transaction
  facts by transaction ID or address.
- [`Observer`](../sdk/indexing/src/indexer.rs) returns durable cursor-based event
  pages. It does not create a follow stream.
- [`OutputQuery`](../sdk/indexing/src/output.rs) is the backend-neutral typed
  output read contract. Its cursor carries a consistency snapshot and an opaque
  position; consumers never receive a storage key.
- `indexing-rocksdb::OutputReader` implements that contract over private
  generation/revision/checkpoint-fenced records. Generic indexing and concrete
  chains do not expose byte-key projection contracts.
- [`ObservationDraft`](../sdk/indexing/src/observation.rs) carries interpreted
  facts before the repository assigns revisions, IDs, and feed cursors.

The concise consumer `Watcher` contract keeps `watch` and `unwatch` together.
The repository `WatchStore` keeps durable registration and removal together.
The operations are lifecycle complements; splitting them into independent
traits would make every real consumer reconstruct the same pair.

Amounts in movements, fees, outputs, and payment projections use
`base::Decimal`. The value is exact and never floating point. Concrete chain
interpreters convert native atomic integers at their boundary; indexing does
not define or persist a competing `AtomicAmount` type.

`indexing_rocksdb::Handle` implements `Checkpoint`, `Watcher`, `History`, and
`Observer`; `Indexer` is their blanket marker. Its corresponding
`indexing_rocksdb::Runtime` owns one RocksDB database, the generic ordered sync
worker, and historical-watch backfills. An embedded composition supplies a
chain-owned `Source` and `BlockInterpreter`, retains the runtime to call
`sync(max_blocks)`, and gives only the cloneable handle to business code. This
keeps record codecs, database keys, and repository mechanics out of concrete
chain crates and external applications. Bitcoin additionally obtains an
`OutputReader` through `handle.outputs()` for typed spendable-output queries.

```rust,ignore
let source = chain_bitcoin::Source::connect(rpc, source_config).await?;
let interpreter = chain_bitcoin::BlockInterpreter::new(scope.clone(), network)?;
let runtime = indexing_rocksdb::Runtime::open(
    "index.db",
    index_config,
    source,
    interpreter,
)?;
let indexer: std::sync::Arc<dyn indexing::Indexer> =
    std::sync::Arc::new(runtime.handle());

// A process-owned loop calls this; business code receives only `indexer`.
runtime.sync(256).await?;
```

`Composer::new().with(scope, child)?` performs exact-scope delegation only; duplicate registration returns
`IndexErrorKind::Conflict` and never replaces the existing child. It does not
synchronize chains or merge child streams. `EthereumService` and `BitcoinService` in `apps/indexer`
are the current chain-specific runtime facades. `indexing-http::Remote`
implements the same consumer traits over the generic router, and payment
reconciliation connects persisted broadcasts to index events.

The approved Ethereum v1 configuration is detailed in
[`INDEXER_SERVICE.md`](./INDEXER_SERVICE.md). It processes no mempool state.
Bitcoin v1 is also block-only. It consumes an unpruned Bitcoin Core 31 source,
requires a synchronized transaction index, resolves every input previous
output, and materializes watched canonical UTXOs in the same atomic commit as
the block, undo, observations, feed rows, and checkpoint.

Bitcoin wallets consume indexed spendable outputs through
`chain-bitcoin::IndexUtxos`. This chain-owned adapter validates generic facts
and converts them into native outpoints, satoshis, scripts, confirmation depth,
and coinbase maturity. The indexing contract therefore remains independent of
Bitcoin while wallet transaction construction does not depend on RocksDB.

## Address registration

An exchange must never return a deposit address before its IX watch is durable.
PS and IX have independent databases, so they cannot share a local transaction.
The intended recoverable sequence is:

```text
1. Read IX's current canonical checkpoint.
2. Provision a key handle and public key through the injected key provider.
3. Build the chain-native address in WS.
4. Persist the PS deposit as `AwaitingWatch`, with its birthday height.
5. Call IX `watch(address)` with a stable idempotency key.
6. Persist the returned IX `watch_id` and mark the deposit `Active`.
7. Return the address to the caller.
```

The watch birthday is the earliest block that could contain activity. For a
newly generated address this is normally the observed tip or the immediately
following height. Imported wallets require an explicit historical birthday or
a deliberate full rescan.

A reconciler retries every `AwaitingWatch` deposit. This closes both crash
windows: PS can retry after step 4, and IX returns the same registration for a
repeated idempotency key after step 5.

## Forward synchronization

```text
1. Load the local checkpoint for one chain/network scope.
2. Read the remote observed tip.
3. Compare the local checkpoint hash with the remote canonical hash at the
   same height.
4. If they differ, execute the reorg procedure below.
5. Fetch the next block. Fetching may be parallel, but commits remain ordered.
6. Verify height and parent hash connect to the persisted checkpoint.
7. Load watches active at that height.
8. Ask the concrete chain interpreter for events and undo information.
9. Atomically persist block identity, chain-native projection effects, undo
   data, normalized observations, and the new checkpoint.
10. Recompute confirmation depth for previously included watched transactions.
11. Append every changed transaction revision to IX's event feed in the same
    atomic operation as its current state.
12. Deliver notifications only after the commit succeeds.
13. Continue until the requested height or newly observed tip is reached.
```

Height alone is never a checkpoint. A checkpoint contains at least height,
block hash, and parent hash.

## Confirmation and finality

`Included` means a transaction is in the current canonical branch but has not
yet met accounting policy. `Confirmed` means IX has attached a
`ConfirmationProof` satisfying the configured policy for that chain/network.

```text
observed depth = canonical tip height - inclusion height + 1
```

IX stores the inclusion block and advances this value from its persisted tip.
The policy may require a minimum depth, a chain-native finalized checkpoint, or
both. A caller cannot weaken the policy in `watch(address)`.

PS logs every transition. `Included` may change the latest absolute `received`
and address `balance` snapshot so operators can see not-yet-deep funds, but it
cannot change `confirmed`, `collected`, or user `accounted`. `Confirmed`
advances the confirmation-qualified balances. An orphan produces a new
`Reorged` revision and a new absolute ledger row that reverses the current
snapshot without deleting history.

## Reorganizations

```text
1. Detect that the persisted tip hash is not canonical at its height.
2. Walk backward comparing local block hashes with source canonical hashes.
3. Stop at the highest common ancestor.
4. Revert orphaned blocks from newest to oldest using persisted undo data.
5. Each revert atomically removes/orphans current effects, restores
   balances/UTXOs, and moves the checkpoint backward.
6. Persist a new `Reorged` observation revision for every affected watched
   transaction; never delete its earlier event from IX's replay history.
7. Connect the replacement canonical blocks in ascending order.
```

Undo retention and the maximum supported reorg depth must be explicit. A reorg
beyond retained undo data is not a normal retry; it requires a controlled
rebuild from an earlier checkpoint.

Changing confirmation policy or an incompatible physical record schema also
requires an explicit offline rebuild. It is not an ordinary indexing mutation,
and there is no public policy-migration command in the indexing contract.

Ethereum v1 retains 50 complete reversible bundles plus one predecessor anchor.
The parent check is only a sequential fast path: startup, polling, reconnect,
and sequence gaps compare the persisted hash against the HTTP canonical hash.
An exact 50-block fork can roll back to the anchor; failure to find an ancestor
there enters `RebuildRequired` and requires the offline staged rebuild. This
retention window is recovery capacity, not finality.

## Mempool

Mempool observations are intentionally separate from canonical block changes:

- they have a `last_seen` notion rather than a block anchor;
- they may be replaced, conflicted, or evicted;
- their balances are pending, never confirmed;
- block inclusion alone is still not deep confirmation;
- confirmation links the observation to a canonical block;
- a dropped transaction must not remain indefinitely pending.

UTXO chains additionally require conflict handling for multiple transactions
spending the same outpoint. Account chains require pending nonce conflict and
replacement handling.

## Chain interpretation

Bitcoin watches scripts/addresses, indexes created outputs and spent
outpoints, and derives balance from canonical unspent outputs. Input relevance
requires resolving the previous outputs being spent.

Bitcoin v1 assigns stable movements independently: `txid:vin:index` for each
input and `txid:vout:index` for each output. Fees require every spent output and
are calculated as the checked input sum minus output sum. A transaction is
never collapsed into a fabricated single sender-to-recipient movement. The
projection retains immutable creation values and records spends as separate
markers. Reorg undo removes orphaned markers to expose the retained creation
again, while orphaned creations are removed.

Ethereum has several distinct sources of value movement:

- top-level native transfers from block transactions;
- transaction success/failure and fees from receipts;
- token transfers from logs;
- internal native transfers from execution traces.

Standard Ethereum JSON-RPC does not guarantee historical traces. The chain RPC
contract therefore reports indexing capabilities so the application cannot
silently claim complete transaction history when its node cannot provide it.

Ethereum v1 reports traces and internal transfers as unsupported. It indexes
successful top-level ETH value, contract-creation value, actual receipt fees,
fee-only failed receipts, and valid ERC-20 `Transfer` logs. Optional WebSocket
heads are hints; one HTTP provider remains the canonical source.
