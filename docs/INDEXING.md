# Indexing model

The goal is a watched-address payment index, not a full general-purpose block
explorer. Generic indexing controls synchronization; each chain interprets its
own blocks and transactions.

## Contracts

- [`BlockSource`](../sdk/indexing/src/source.rs) exposes the observed tip,
  canonical block at a height, and canonical hash lookup.
- [`IndexedBlock`](../sdk/indexing/src/block.rs) exposes height, hash, parent
  hash, and optional timestamp through `BlockRef`.
- [`BlockInterpreter`](../sdk/indexing/src/source.rs) converts a chain-native
  block and active watch targets into events plus chain-owned undo data.
- [`WatchStore`](../sdk/indexing/src/store.rs) persists targets and their first
  relevant height.
- [`IndexStore`](../sdk/indexing/src/store.rs) atomically commits a block or
  reverts the current tip.
- [`IndexingWorker`](../sdk/indexing/src/service.rs) exposes sync and checkpoint
  status to the IX worker.
- [`ObservationRegistry`](../sdk/indexing/src/service.rs) registers address and
  transaction-ID watches.
- [`ObservationQuery`](../sdk/indexing/src/service.rs) returns normalized
  transaction facts by transaction ID or address.
- [`ObservationEventSource`](../sdk/indexing/src/service.rs) provides durable,
  cursor-based replay independent of its push/poll transport.
- [`ObservationStore`](../sdk/indexing/src/store.rs) is IX-owned and atomically
  persists transaction revisions plus their outgoing events.

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
9. Atomically persist block identity, chain-native effects, undo data,
   normalized observations, and the new checkpoint.
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

Ethereum has several distinct sources of value movement:

- top-level native transfers from block transactions;
- transaction success/failure and fees from receipts;
- token transfers from logs;
- internal native transfers from execution traces.

Standard Ethereum JSON-RPC does not guarantee historical traces. The chain RPC
contract therefore reports indexing capabilities so the application cannot
silently claim complete transaction history when its node cannot provide it.
