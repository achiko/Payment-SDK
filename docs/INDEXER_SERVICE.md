# Ethereum Indexer Service v1

Status: the Bitcoin and Ethereum IX runtime, generic HTTP surface, remote HTTP
client, and RocksDB repository are implemented and covered by deterministic
loopback end-to-end tests. Opt-in real-node execution and production deployment
evidence remain pending.

This document remains the Ethereum-specific runtime contract. Bitcoin uses the
same generic repository/reorg/rebuild machinery through the chain-specific
`indexer-worker bitcoin ...` commands, with a canonical UTXO projection and
explicit deployment policy. See
[`BITCOIN_SERVICES.md`](./BITCOIN_SERVICES.md) for the Bitcoin Core 31
prerequisites, configuration, API, and acceptance boundary.

This document fixes the operational and persistence decisions for the first
real Indexer Service (IX) implementation. The general ownership and accounting
requirements remain in [`SYSTEM_REQUIREMENTS.md`](./SYSTEM_REQUIREMENTS.md).

## Scope

One IX process owns one Ethereum chain/network scope and one RocksDB path. The
process downloads every canonical block from a mandatory bootstrap height and
filters facts against durable address and transaction watches.

The process may be the `indexer-worker` binary or an application embedding the
`EthereumService` library facade. Both placements run the same runtime and obey
the same single-scope, exclusive-database ownership rules.

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

The CLI and library runtime require these values on first boot:

| Setting | Requirement |
|---|---|
| Network slug | Canonical, non-empty scope name |
| Expected chain ID | Must equal `eth_chainId` |
| Expected genesis hash | Must equal block zero |
| HTTP RPC URL | The single authoritative canonical source |
| Bootstrap height | Earliest supported watch birthday |
| IX database path | Must be exclusive to this scope/process |
| `STRICT_AUTHENTICATION_MODE` | Exact required `true`/`false`; strict requires `IX_BEARER_TOKEN` even on loopback |

Defaults are confirmation depth `12`, rollback retention `50`, HTTP polling
every five seconds, RPC timeout 15 seconds, ready lag at most two blocks, and a
maximum ready reconciliation age of 30 seconds. A WebSocket URL is optional and
disabled by default. A `newHeads` message only wakes HTTP reconciliation; it is
never itself canonical evidence.

The persisted chain identity, bootstrap height, confirmation policy, and
retention policy must match configured values on restart. A mismatch fails
closed and requires an offline rebuild under the new configuration. Ordinary
indexing does not mutate the meaning of already published facts in place.

One process owns one database. RocksDB's path lock and the storage adapter's
serialized writer provide the v1 single-owner guarantee. This is not a
distributed lease or high-availability design. Operators must not run copied
databases for the same scope as independent writers.

### In-process composition

`EthereumConfig::new` requires the database path, logical network slug,
bootstrap height, expected chain ID, expected genesis hash, and authoritative
HTTP RPC URL. It selects the documented v1 defaults for confirmation,
retention, polling, loopback listeners, and readiness. Callers may override
those public configuration fields before validation:

```rust
use http::server::AuthenticationMode;
use indexer_worker::{EthereumConfig, EthereumService};

// Replace this with the actual block-zero hash reported by the target node.
let genesis_hash =
    "0x0000000000000000000000000000000000000000000000000000000000000000";
let mut config = EthereumConfig::new(
    "./indexer.db",
    "anvil",
    0,
    31_337,
    genesis_hash,
    "http://127.0.0.1:8545",
    AuthenticationMode::GlobalTrusted,
);
// One confirmation is suitable only for this disposable local test.
config.confirmation_depth = 1;

let service = EthereumService::new(config)?;
service.run().await?;
```

`run()` owns Ctrl+C handling. A larger application should normally call
`run_until(shutdown)` with its existing shutdown future. Startup opens the
exclusive RocksDB path, verifies RPC chain identity, and binds the configured
HTTP listener. The facade does not turn IX into a stateless object like WS.
Configuration validation and synchronous RocksDB opening finish before the
shutdown future is polled; asynchronous RPC preflight and the supervised
runtime are cancellation-aware.

The current runtime does not install a metrics recorder or expose a metrics
listener. Operational status and readiness are available through the HTTP API;
telemetry can be added later by an application-owned adapter without adding
chain or storage semantics to the indexing contracts. Offline backup, rebuild,
and cleanup remain explicit `indexer-worker` maintenance commands and must not
run concurrently with the embedded service.

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

Physical record versions belong to the RocksDB adapter. A binary rejects
missing, older, newer, or corrupt schema metadata. The adapter provides no
in-place schema conversion; operators create a new database and rebuild it.
Published revision/event journals are never deleted by rebuild, cleanup, or
unwatch.

Confirmation depth and rollback retention have no semantic migration command.
Changing either value requires a new offline rebuild under the target policy.
The rebuilt generation is validated before activation, so already published
facts are never relabeled in place. Scope and bootstrap height remain immutable;
chain-finality cannot be enabled in Ethereum v1.

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
GET    /v1/scopes/{chain}/{network}/status
POST   /v1/scopes/{chain}/{network}/watches
DELETE /v1/scopes/{chain}/{network}/watches/{watch_id}
GET    /v1/scopes/{chain}/{network}/transactions/{transaction}
GET    /v1/scopes/{chain}/{network}/addresses/{address}/transactions
GET    /v1/scopes/{chain}/{network}/events?after_cursor=...&limit=...
GET    /v1/scopes/{chain}/{network}/addresses/{address}/outputs
GET    /health/live
GET    /health/ready
```

Pagination is exclusive-after, defaults to 100, and is capped at 1,000. JSON
cursors and other large integers are decimal strings. Monetary values are
exact `Decimal` strings; each concrete chain validates its native-unit scale.
Errors use:

```json
{
  "code": "stable_code",
  "message": "safe contextual message",
  "retryable": false,
  "request_id": "opaque request identifier"
}
```

`STRICT_AUTHENTICATION_MODE` is required and accepts exactly lowercase `true`
or `false`. Strict mode requires the IX bearer for every `/v1` route, including
loopback. Global-trusted mode omits repo-owned bearer authorization and grants
every reachable caller the same authority; it is not identity isolation.
Liveness remains detail-free, while public readiness and `/v1` status expose
only the sanitized mode. Built-in TLS and token hot reload are not in v1;
non-loopback traffic requires a trusted TLS-terminating proxy in either mode,
and strict token rotation requires restart.

Readiness requires phase `Ready`, lag no greater than two blocks, and a
successful canonical reconciliation in the preceding 30 seconds. Liveness only
reports that the process and supervisor are running.

Direct watch registration keeps repository idempotency mandatory. Every HTTP
request must provide a non-empty caller-owned idempotency key in both strict and
global-trusted modes. IX never invents one: callers must persist and reuse the
same key so a lost response can be retried safely.

## Payment Service integration

PS owns the cross-service retry window. A deposit address is exposed only after
the durable deposit record exists and IX has acknowledged its caller-owned,
idempotent address watch. For outgoing payments and sweeps, PS persists the
exact signed transaction, registers the transaction watch, and only then
broadcasts. It consumes IX revision events with a durable per-scope cursor;
confirmation may advance a payment while a later reorg revision may move it
back to submitted.

`apps/api` provides protocol-neutral orchestration plus a configured payment
binary. It composes concrete Bitcoin/Ethereum wallets, the remote Indexer
adapter, payment RocksDB, bearer-authenticated HTTP, and reconciliation. One
optional finite deposit scope can additionally compose address/watch,
observation, balance/history, and server-derived collection planning/execution
for Bitcoin native, Ethereum native, or ERC-20. UTXO planning consumes IX
outputs only through a stable canonical snapshot. The concrete contract and
exclusions are documented in [`PAYMENT_SERVICE.md`](./PAYMENT_SERVICE.md). IX
remains independent of `sdk/deposits`.

## Operations and validation

The Indexer binary provides `serve`, `backup`, `rebuild`, `rebuild-abort`, and
old-generation cleanup commands for Ethereum, plus the corresponding nested
`bitcoin` command family. Logs must not contain bearer tokens, RPC
authorization, raw signed transactions, or private keys.

Deterministic tests own exact reorg depths 1, 12, 49, 50, and 51, response-loss
retry, confirmation on empty blocks, and crash boundaries. A pinned opt-in
Kurtosis/Disruptoor profile and runbook are checked in for a real pre-finality
fork, but that resource-intensive scenario is not part of `cargo test` and has
not yet been executed. It does not assert an exact depth because proposer
timing makes the fork nondeterministic.

The offline runtime acceptance tests start real loopback Indexer HTTP services,
deterministic Bitcoin Core and Ethereum JSON-RPC doubles, and a real temporary
RocksDB repository. They exercise the chain-neutral HTTP adapter through watch
registration, block ingestion, confirmation, transaction/history/event reads,
and Bitcoin output reads. Each test then shuts down and reopens the service on
the same RocksDB path to prove that caller idempotency, history, the event
cursor, and outputs survive a process restart:

```bash
mac cargo test --locked -p indexer-worker --test runtime_e2e
```
