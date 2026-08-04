# Ethereum Indexer Service v1

Status: Ethereum IX implementation baseline complete; production PS business
composition and opt-in real-node execution remain pending.

This document fixes the operational and persistence decisions for the first
real Indexer Service (IX) implementation. The general ownership and accounting
requirements remain in [`SYSTEM_REQUIREMENTS.md`](./SYSTEM_REQUIREMENTS.md).

## Scope

One IX process owns one Ethereum chain/network scope and one RocksDB path. The
process downloads every canonical block from a mandatory bootstrap height and
filters facts against durable address and transaction watches.

V1 indexes:

- successful top-level native ETH transfers;
- native value sent by successful contract-creation transactions;
- actual transaction fees from receipts;
- failed receipts as fee-only failed facts; and
- structurally valid ERC-20 `Transfer` logs, including mint and burn endpoints.

V1 does not read the mempool, subscribe to pending transactions, call trace
methods, claim internal-native-transfer completeness, use `safe`/`finalized`
tags, or interpret token metadata and balances. Mempool contracts remain
separate and unwired.

## Runtime configuration

The `serve` command requires these values on first boot:

| Setting | Requirement |
|---|---|
| Network slug | Canonical, non-empty scope name |
| Expected chain ID | Must equal `eth_chainId` |
| Expected genesis hash | Must equal block zero |
| HTTP RPC URL | The single authoritative canonical source |
| Bootstrap height | Earliest supported watch birthday |
| IX database path | Must be exclusive to this scope/process |

Defaults are confirmation depth `12`, rollback retention `50`, HTTP polling
every five seconds, RPC timeout 15 seconds, ready lag at most two blocks, and a
maximum ready reconciliation age of 30 seconds. A WebSocket URL is optional and
disabled by default. A `newHeads` message only wakes HTTP reconciliation; it is
never itself canonical evidence.

The persisted chain identity, bootstrap height, confirmation policy, and
retention policy must match configured values on restart. A mismatch fails
closed until an explicit migration is run.

One process owns one database. RocksDB's path lock and the storage adapter's
serialized writer provide the v1 single-owner guarantee. This is not a
distributed lease or high-availability design. Operators must not run copied
databases for the same scope as independent writers.

## Confirmation policy

Depth is computed from persisted canonical state:

```text
depth = checkpoint height - inclusion height + 1
```

Each depth change from 1 through 11 appends a new `Included` revision. Depth 12
appends `Confirmed` with a depth proof. Every connected block re-evaluates
included transactions, including blocks containing no watched movements.

Depth 12 is an application confirmation policy, not a claim of Ethereum
consensus finality. Retaining 50 reversible bundles is rollback capacity, not a
finality threshold.

## Canonical synchronization

On startup, after an RPC error, after WebSocket reconnect, after a sequence
gap, and on each polling cycle, IX compares its persisted checkpoint hash with
the HTTP source's canonical hash at the same height. Parent-hash equality is a
fast path for a candidate next block, not the only reorg check.

For each block, IX:

1. fetches the full block and receipts from one HTTP provider;
2. validates height, parent, transaction order, receipt transaction/block
   identity, and configured chain identity;
3. loads watches active at the height;
4. interprets Ethereum facts into drafts without repository-assigned IDs;
5. re-queries the candidate block hash immediately before commit; and
6. commits the raw block/receipt bytes, undo, facts, revisions, events,
   confirmation changes, retention pruning, and checkpoint in one atomic batch.

`eth_getBlockReceipts` may be used when supported. Otherwise receipts are read
with batched `eth_getTransactionReceipt`. An unavailable or inconsistent RPC
response leaves canonical state unchanged and is retryable; it is never proof
that a transaction was dropped or failed.

## RocksDB layout and durability

IX and Payment Service (PS) use physically separate RocksDB paths. The generic
storage adapter uses fixed `meta` and `data` column families and a single
bounded command channel feeding one OS thread that exclusively owns the DB.
Condition checks and mutations are therefore serialized.

A physical data key contains a format byte, a length-prefixed namespace, and
the ordered logical key. Every value has a magic value, frame version, logical
storage version, payload length, and payload. Semantic repositories use
explicit immutable `RecordV1` DTOs; public domain enum layout is not a storage
format.

Critical commits use WAL with synchronous durability. One logical commit
evaluates all `Missing`/`Version` conditions and applies all operations in one
RocksDB `WriteBatch`. A caller cancellation after enqueue cannot cancel an
accepted write, so a timeout has an unknown outcome and semantic callers retry
using expected checkpoints and idempotency keys.

IX records are grouped into stable logical namespaces for:

- schema, policy, active generation, worker phase, and feed counters;
- scopes, checkpoints, canonical block hashes, retained raw bundles, and undo;
- watches, watch idempotency, watch activation, and historical backfill jobs;
- current observations, immutable revisions, confirmation indexes, and
  address/transaction indexes;
- append-only event rows and event-ID indexes; and
- reorg and staged-rebuild progress.

Schema upgrades are ordered and resumable. A binary rejects a newer schema and
corrupt frames. `migrate schema` handles this physical format registry.
Maintenance commands create and verify a backup before a mutation. Published
revision/event journals are never deleted by migration, rebuild, cleanup, or
unwatch.

Confirmation depth and rollback retention use the separate `migrate policy`
path. The operator supplies the exact old values, target values, an idempotency
key, and an audit reason. One compare-and-swap batch updates repository policy,
appends an immutable versioned audit row, and moves a checkpointed index to
`RebuildRequired`. Scope and bootstrap height remain immutable; chain-finality
cannot be enabled in Ethereum v1. A retry with the same key and payload returns
the recorded version, while changed payload or stale old values fail closed.

Run policy migration only while the service is stopped, with no staged rebuild
manifest. A checkpointed repository must be `Ready`; an active revert, replay,
halt, or rebuild is rejected rather than relabeled. After migration, a
checkpointed repository requires staged rebuild activation under the target
configuration before semantic watch/query/feed operations resume. An empty,
uncheckpointed repository remains `Starting` and can initialize directly under
the new policy.

## Watch semantics

Every request includes its `IndexScope`, required start height, selector, and
caller idempotency key. The key is unique within a scope. Repeating an identical
request returns the existing receipt; reusing the key with a different
selector or height returns a conflict.

The start height cannot precede the configured bootstrap height. A watch whose
birthday is behind the current checkpoint creates durable backfill work. A
soft-unwatched record remains for idempotency and audit history.

## Reorganization recovery

At tip `H`, IX retains complete reversible bundles `[H-49, H]` and the hash-only
predecessor anchor `H-50`. This permits an exact 50-block rollback.

When the checkpoint hash is no longer canonical, IX finds the highest common
ancestor within that window. It persists reorg progress, reverts orphan tips
newest-first in separate atomic commits, then replays replacement blocks in
ascending order. Every affected inclusion appends a new `Reorged` revision;
earlier facts and feed rows remain immutable. Restart resumes from the durable
checkpoint and phase without allocating duplicate revisions or cursors.

If no common ancestor exists through the predecessor anchor, IX enters
`RebuildRequired`, becomes unready, blocks normal semantic watch/query/feed
operations, and stops canonical writes. It does not delete the invalid state or
retry indefinitely.

## Staged rebuild

The offline `rebuild` command acquires the same exclusive database ownership,
allocates a generation-prefixed shadow state, and rescans from the persisted
bootstrap height. Shadow facts and a shadow checkpoint are durable but
unpublished. The command validates chain identity, parent links, receipt
associations, depth-12 projections, watches, and the retained rollback window.

It then deterministically diffs old current facts against the rebuilt branch
and prepares hidden correction revisions above the existing published cursor.
One synchronous batch switches the active generation, checkpoint, published
event high-water, and manifest state. A crash exposes either the complete old
generation or the complete new generation, never a mixture.

`rebuild-abort` may remove only unpublished staging data and returns to
`RebuildRequired`. Old published journals remain forever. Old raw/projection
generations are removed only by explicit cleanup after operational verification.

## HTTP API

The versioned semantic surface is:

```text
GET    /v1/scopes/ethereum/{network}/status
POST   /v1/scopes/ethereum/{network}/watches
DELETE /v1/scopes/ethereum/{network}/watches/{watch_id}
GET    /v1/scopes/ethereum/{network}/transactions/{tx_hash}
GET    /v1/scopes/ethereum/{network}/addresses/{address}/transactions
GET    /v1/events?after_cursor=...&limit=...
GET    /health/live
GET    /health/ready
GET    /metrics                    loopback listener only
```

Pagination is exclusive-after, defaults to 100, and is capped at 1,000. JSON
cursors and other large integers are decimal strings. Atomic amounts are
32-byte hexadecimal strings. Errors use:

```json
{
  "code": "stable_code",
  "message": "safe contextual message",
  "retryable": false,
  "request_id": "opaque request identifier"
}
```

If configured, bearer authentication protects every `/v1` route. A non-loopback
bind refuses startup without a bearer token. Health endpoints are unauthenticated
and sanitized. Built-in TLS and token hot reload are not in v1; non-loopback
traffic must stay private or terminate TLS at a trusted proxy, and token
rotation requires restart.

Readiness requires phase `Ready`, lag no greater than two blocks, and a
successful canonical reconciliation in the preceding 30 seconds. Liveness only
reports that the process and supervisor are running.

The loopback Prometheus listener reports checkpoint, remote tip, lag, worker
phase, published event-feed head, reconciliation/backfill outcomes, source-call
outcomes and durations, block-commit outcomes and latency, exact or
beyond-retention reorg depth, and WebSocket enabled/connected/reconnect/failure
state. Metric labels contain the network slug only; RPC URLs, authorization,
request bodies, raw block data, and bearer values are never labels.

## Payment Service integration

PS uses a separate database and owns the cross-service retry window:

1. require IX `Ready` and capture its checkpoint as the birthday;
2. request a new address through the stateless Wallet Service boundary;
3. atomically persist the deposit as `AwaitingWatch` plus a zero ledger row;
4. register the IX watch with a stable idempotency key;
5. persist `Active { watch_id }`; and
6. only then return the address.

A worker retries every durable `AwaitingWatch` record. PS mirrors IX events and
advances an ingestion cursor atomically. A separate projection cursor advances
only after classification and all affected absolute ledger rows are committed.

If a reorg lowers canonical confirmation below an already credited `accounted`
amount, PS preserves `accounted`, appends corrected canonical ledger snapshots,
creates a durable open `PostCreditReorg` reconciliation case, and blocks further
automatic credit and collection for that deposit. Only an explicit operator
resolution may close the case.

The backend-independent coordinator and the persistent PS repository implement
and test these atomic transitions. `apps/api` currently exposes bounded
maintenance commands to resume `AwaitingWatch` records, mirror IX events, and
inspect the independent projection backlog. It does not yet expose public
deposit creation or run a production projection supervisor: those composition
steps require the approved Wallet Service transport/address adapter and
concrete PS business classification rules. IX remains independent of
`sdk/deposits` in either case.

## Operations and validation

The Indexer binary provides `serve`, `backup`, `migrate schema`, `migrate
policy`, `rebuild`, `rebuild-abort`, and old-generation cleanup commands. Both
migration modes create and verify the requested backup before mutation. Logs
and metrics must not contain bearer tokens, RPC authorization, custody values,
raw signed transactions, or private keys.

Deterministic tests own exact reorg depths 1, 12, 49, 50, and 51, response-loss
retry, confirmation on empty blocks, and crash boundaries. A pinned opt-in
Kurtosis/Disruptoor profile and runbook are checked in for a real pre-finality
fork, but that resource-intensive scenario is not part of `cargo test` and has
not yet been executed. It does not assert an exact depth because proposer
timing makes the fork nondeterministic.
