# Indexing

## Purpose

Indexing turns chain-native blocks into durable transaction history for watched
addresses. It runs inside `apps/api`; it is neither a service nor a transport.

The current surface answers three questions:

1. What canonical `(height, hash)` has a chain/network scope processed?
2. Which complete transactions affect a watched address?
3. How did an observation change after confirmation or a reorg?

There is deliberately no event feed, raw-block archive, transaction watch,
watch removal, backfill command, rebuild command, or migration API.

## Composition

```text
chain RPC client
    -> chain BlockSource
    -> chain-native block
    -> chain BlockInterpreter + address WatchSnapshot
    -> InterpretedBlock
    -> Synchronizer
    -> Repository

HTTP/wallet consumer
    -> Index<Repository>
         -> Checkpoint
         -> Watcher
         -> History
```

`apps/api` constructs `Repository`, gives a clone to `Synchronizer`, and
wraps another clone in `Index<R>`. RocksDB does not own a runtime or public
handle. Application startup owns synchronizer scheduling, readiness, cancellation,
and shutdown.

## Consumer facade

`Index<R>` is a small generic facade over one repository. Depending on the
repository bounds it implements:

- `Checkpoint::checkpoint(scope)`;
- `Watcher::watch(request)` for a canonical address; and
- `History::transaction(query)` and `History::history(query)`.

Watch registration is durable and idempotent. A watch records its first
relevant height and remains active; the current design has no `unwatch` or
inactive state. `IndexScope` identifies one exact chain and network, and every
watch and query is scope-checked.

## Persistence contracts

The storage-independent boundary consists of five small traits:

| Trait | Methods | Invariant |
|---|---|---|
| `CanonicalStore` | `checkpoint`, `canonical_block`, `load_commit` | canonical identity is height plus hash; planning reads a stable semantic context |
| `WatchStore` | `watches_at`, `load_watch`, `save_watch` | indexing decides idempotency and registration; storage applies the supplied plan |
| `BlockStore` | `commit_block`, `load_revert`, `save_revert` | indexing decides rollback transitions; storage applies them atomically |
| `HistoryStore` | `transaction`, `transactions_by_address` | complete observations remain queryable |
| `StatusStore` | `status`, `set_status` | synchronizer readiness/failure is durable per scope |

`sdk/indexing/rocksdb::Repository` implements these semantic contracts.
Its record structs, byte keys, codecs, column layout, and compare-and-swap
mechanics are physical implementation details. Watch, commit, and rollback
decisions are pure plans in `sdk/indexing`; RocksDB only loads their required
context and atomically persists supplied plans. Chain interpreters emit
semantic effects and must never create RocksDB keys. This boundary permits a
future PostgreSQL repository without changing indexing consumers or chains.

## Synchronization and reorgs

For a bounded synchronization step, `Synchronizer`:

1. reads the persisted checkpoint and source tip;
2. verifies the persisted tip remains canonical;
3. reverts retained tips until it finds a common ancestor when necessary;
4. loads the address-watch snapshot for the next height;
5. fetches the chain-native block and asks the chain interpreter for facts;
6. verifies canonical identity immediately before persistence; and
7. commits effects, undo, observation revisions, and the checkpoint atomically.

A crash exposes either the old state or the complete new state. RPC failures
remain retryable/unknown and never imply that a transaction was dropped.

Reorg rollback uses retained undo. Corrected observations receive later
revisions; previous revisions remain history rather than being rewritten to
make the replacement branch look original. If a common ancestor is older than
retention, synchronization halts with `ReorgBeyondRetention`; the operator
must replace the database and resynchronize. That failure mode is not a public
rebuild API.

## Transactions and movements

An observation contains a scoped transaction ID, revision, status, stable
movements, optional network fee, and ordering metadata. Movements represent
account transfers, UTXO inputs/outputs, minting, and burning without assigning
business meaning.

A Bitcoin transaction with three inputs and two outputs therefore has five
movements. It is never collapsed into a fictional single sender, recipient,
and amount. Values use exact `Decimal` representation with no floating point.
Wallet presentation applies trusted asset precision.

## Validation

Deterministic tests must cover address-watch idempotency, initial catch-up,
restart from a height-and-hash checkpoint, duplicate commit, one- and
multi-block reorg, revision preservation, Bitcoin spent-output restoration,
Ethereum native/token movements, and RPC outage without false terminal state.
