# Ethereum-first Indexer Service implementation plan

Status: Approved on 2026-08-04. The Ethereum IX runtime, persistent IX/PS
repositories, reorg/rebuild/backfill recovery, HTTP API, semantic policy
migration, telemetry, and deterministic validation are implemented. Production
PS address creation/projection composition and the opt-in Kurtosis execution
remain explicit review gates. Current behavior and operations are tracked in
[`docs/INDEXER_SERVICE.md`](../docs/INDEXER_SERVICE.md).

Implementation note: `apps/wallet` was touched only to remove obsolete IX RPC
methods from the stateless Wallet Service boundary; custody and transaction
behavior were not changed.

## Summary

Build a real, separately runnable Ethereum Indexer Service backed by RocksDB, followed by the durable Payment Service watch/replay integration.

Locked v1 behavior:

- One Ethereum chain/network scope per Indexer Service process and RocksDB path.
- Every canonical block is processed; only the latest 50 raw reversible block bundles plus one predecessor anchor are retained.
- No mempool access, pending-transaction subscription, traces, or internal native-transfer claims.
- Index successful top-level ETH transfers, transaction fees, receipt failures, contract-creation value, and valid ERC-20 `Transfer` logs.
- Use depth-only confirmation: `depth = tip - inclusion + 1`; mark a transaction `Confirmed` at depth 12. Do not use Ethereum `safe` or `finalized` as a v1 policy.
- Treat HTTP polling as authoritative. Optional `newHeads` only wakes reconciliation because Geth subscriptions can skip heads, emit multiple same-height heads, and do not replay after reconnect. See [Geth subscription documentation](https://geth.ethereum.org/docs/interacting-with-geth/rpc/pubsub).
- Use RocksDB 0.24.0, which is compatible with Rust 1.85 and supplies atomic batches and process locking. See the [RocksDB manifest](https://raw.githubusercontent.com/rust-rocksdb/rust-rocksdb/v0.24.0/Cargo.toml) and [RocksDB operations](https://github.com/facebook/rocksdb/wiki/Basic-Operations).

The proposed parent-hash check is valid during sequential fetching. It must be supplemented by comparing the persisted checkpoint hash with the HTTP RPC canonical hash on startup, reconnect, polling cycles, and sequence gaps.

## Public contracts

### Indexing contracts

Update [`sdk/indexing`](../sdk/indexing/src/lib.rs) so:

- `IndexScope` is present on watch receipts, observations, transaction queries, and status.
- `WatchRequest.start_height` is required.
- `(scope, idempotency_key)` uniquely identifies a watch. Repeating the same payload returns the existing receipt; repeating the key with a changed payload returns a conflict.
- `ObservationDraft` contains interpreted facts but no revision, cursor, event ID, or previous state.
- `InterpretedBlock` combines block effects, undo, drafts, and persisted raw payloads.
- One composite `IndexRepository` replaces separate block and observation mutations. Its atomic commands cover:
  - canonical block and checkpoint lookup;
  - idempotent watch registration and soft unwatch;
  - `commit_block(expected_checkpoint, expected_watch_version, block)`;
  - `revert_tip(expected_tip)`;
  - scoped transaction and address queries;
  - cursor-based event replay;
  - rebuild-generation operations.
- Revisions are monotonic per `(scope, transaction_id)` and event cursors are global per Indexer Service database.
- Every depth change from 1 through 11 appends a new `Included` revision; depth 12 appends `Confirmed`.
- A failed receipt produces a fee-only `Failed` fact with no value or token movements.
- Public status uses explicit phases: `Starting`, `Reconciling`, `CatchingUp`, `Ready`, `Reverting`, `Replaying`, `RebuildRequired`, and `Halted`.

Keep the existing object-safe `BoxFuture` style. Leave `MempoolSource` untouched and entirely unwired.

### Ethereum boundary

Split Indexer-specific RPC behavior from the wallet-facing `EthereumRpc` in [`sdk/chains/ethereum`](../sdk/chains/ethereum/src/rpc.rs):

- Validate configured chain ID and genesis hash.
- Fetch canonical headers and full transactions by number and hash.
- Fetch receipts with `eth_getBlockReceipts` when supported, falling back to batched `eth_getTransactionReceipt`.
- Validate every receipt's block hash, height, transaction hash, and order before interpretation.
- Re-query the candidate block hash immediately before commit.
- Persist exact successful block and receipt JSON result bytes alongside decoded fields.
- Parse only ERC-20 logs with the precise signature, topic, and data shape; recognize zero-address mint and burn movements.
- Use stable movement IDs based on transaction hash plus `"value"` or log index.
- Report `traces = false` and `internal_transfers = false` in capabilities.

Use the official block and receipt shapes from the [Ethereum Execution APIs](https://ethereum.github.io/execution-apis/api/methods/eth_getBlockByNumber/).

### Indexer HTTP API

Implement these versioned JSON endpoints:

- `GET /v1/scopes/ethereum/{network}/status`
- `POST /v1/scopes/ethereum/{network}/watches`
- `DELETE /v1/scopes/ethereum/{network}/watches/{watch_id}`
- `GET /v1/scopes/ethereum/{network}/transactions/{tx_hash}`
- `GET /v1/scopes/ethereum/{network}/addresses/{address}/transactions`
- `GET /v1/events?after_cursor=...&limit=...`
- `GET /health/live`
- `GET /health/ready`
- A separate loopback `/metrics` endpoint

Use exclusive-after opaque pagination, a default page size of 100, and a maximum page size of 1,000. Encode large integers and cursors as decimal strings and amounts as 32-byte hexadecimal values. Return structured `{ code, message, retryable, request_id }` errors.

If a bearer token is configured, protect every `/v1` endpoint. Non-loopback binding must refuse startup without a token. Health responses remain unauthenticated and sanitized.

## Parts of the project touched

| Area | Main impact |
|---|---|
| [`sdk/indexing`](../sdk/indexing/src/lib.rs) | Composite repository, ordered worker, observations, confirmation, watches, replay, reorg, and rebuild contracts |
| [`sdk/chains/ethereum`](../sdk/chains/ethereum/src/indexer.rs) | Ethereum block source, receipt/log decoding, stable fact creation, and optional `newHeads` |
| [`sdk/storage`](../sdk/storage/src/lib.rs) | New `sdk/storage/rocksdb` backend package implementing the existing atomic `Storage` contract |
| [`packages/http`](../packages/http/src/lib.rs), [`packages/json-rpc`](../packages/json-rpc/src/lib.rs), and [`packages/telemetry`](../packages/telemetry/src/lib.rs) | Reqwest transport, JSON-RPC client/framing, JSON logs, and Prometheus implementation |
| [`apps/indexer`](../apps/indexer/src/main.rs) | Configuration, worker supervision, HTTP API, health, telemetry, and maintenance CLI |
| [`sdk/deposits`](../sdk/deposits/src/lib.rs) and [`apps/api`](../apps/api/src/main.rs) | Durable `AwaitingWatch` recovery, Indexer client, event mirror, projection cursor, and reconciliation cases |
| Canonical documentation and workspace manifests | Record selected decisions, dependencies, validation, and operations |

`apps/wallet`, signing crates, Bitcoin transaction/indexing code, and custody behavior remain unchanged. Remove the unused Bitcoin dependency from the Ethereum-only `apps/indexer` composition.

## Recommended implementation order

### 1. Record the approved design

- Add a canonical `docs/INDEXER_SERVICE.md` containing runtime configuration, HTTP DTOs, RocksDB schema, confirmation policy, recovery runbook, and limitations.
- Update `SYSTEM_REQUIREMENTS.md`, `ARCHITECTURE.md`, `CONTRACTS.md`, `FEATURE_VALIDATION.md`, `INDEXING.md`, `REQUIREMENTS.md`, and `RESEARCH.md` together.
- Explicitly record: no mempool or traces, depth 12, retention 50, single-process RocksDB, optional WebSocket wake-up, staged rebuild, and manual post-credit reconciliation.

### 2. Implement the RocksDB backend

- Add package `storage-rocksdb` under `sdk/storage/rocksdb` using `rocksdb = 0.24.0`, LZ4, and runtime bindgen.
- Feed one bounded asynchronous channel into a dedicated OS thread that exclusively owns the database. Serialize all reads, condition checks, and writes on that thread.
- Implement `Storage::commit` by evaluating `Missing` and `Version` conditions and applying one WAL-enabled, `sync = true` RocksDB `WriteBatch`.
- Use fixed `meta` and `data` column families. Encode keys as a format byte, namespace length, namespace, and ordered logical key.
- Store versioned envelopes and immutable Bincode `RecordV1` DTOs; never serialize public Rust domain enums directly.
- Add physical-format validation, a schema migration registry, BackupEngine support, corruption limits, and fail-closed opening.
- Use physically separate Indexer and Payment Service databases.

### 3. Correct the Indexer repository and generic worker

- Implement backend-independent persistent repository behavior in `sdk/indexing` over `Storage`, keeping application composition thin.
- Add watch activation history and durable backfill jobs for watches registered behind the current checkpoint. Reject birthdays before the configured bootstrap height.
- Make one block commit atomically write raw block and receipts, undo, current facts, immutable revisions, events, confirmation-depth transitions, checkpoint, and retention pruning.
- Make a retry after an unknown commit outcome return `AlreadyApplied` without duplicating revisions or cursors.
- Persist operational and reorg progress so restart resumes safely.

### 4. Implement the Ethereum vertical slice

- Add typed Alloy RPC structures using `alloy-rpc-types-eth = 1.0.22`.
- Implement the authoritative HTTP source using the existing transport and JSON-RPC boundaries.
- Add optional `tokio-tungstenite` `newHeads`. Duplicate, old, jumping, and same-height replacement notifications only trigger HTTP reconciliation.
- Poll every five seconds even while WebSocket is healthy. Use one authoritative HTTP provider, a 15-second request timeout, and bounded exponential retry.
- Never issue mempool, pending-transaction, trace, `safe`, or `finalized` RPC calls.

### 5. Compose the runnable Indexer Service

- Add `serve`, `backup`, `migrate`, `rebuild`, `rebuild-abort`, and old-generation cleanup CLI commands.
- Require on first boot: network slug, expected chain ID, expected genesis hash, HTTP RPC URL, bootstrap height, and database path.
- Make the WebSocket URL optional and disabled by default. Default confirmation depth to 12 and reorg retention to 50.
- Persist the initial confirmation policy. A later configuration mismatch halts with `PolicyMismatch` until explicitly migrated.
- Add JSON tracing logs and Prometheus metrics for checkpoint, remote tip, lag, RPC outcomes, commit latency, reorg depth, feed head, WebSocket state, and worker phase.
- Readiness requires `Ready`, lag of at most two blocks, and a successful reconciliation within 30 seconds.

### 6. Implement bounded reorg recovery and staged rebuild

- At tip `H`, retain reversible bundles `[H-49, H]` plus the hash-only anchor `H-50`.
- On mismatch, find the highest common ancestor, revert only the orphaned suffix newest-first, and replay replacement blocks in ascending order.
- Make every revert atomic and append-only. An orphaned inclusion receives `Reorged`; a surviving inclusion whose depth falls receives a corrected `Included` revision.
- If no ancestor exists within 50 blocks, publish `RebuildRequired`, disable semantic watch/query/feed operations, and stop normal writes.
- Make the offline rebuild create generation-prefixed shadow state in the same RocksDB, rescan from the persisted bootstrap height, validate it, diff current facts, and prepare hidden correction events above the published cursor.
- Use one synchronous batch to atomically switch `active_generation`, checkpoint, and published event high-water. A crash exposes either the complete old generation or the complete new generation.
- Keep the previous generation until explicit cleanup. Never delete published revisions or event rows.

### 7. Add the durable Payment Service slice

- Reuse `storage-rocksdb` with a separate Payment Service path.
- Add atomic `create AwaitingWatch + zero ledger`, `AwaitingWatch` scanning, and idempotent activation.
- Have Payment Service read Indexer `Ready`, capture its checkpoint as the birthday, obtain an address through an injected Wallet Service boundary, persist locally, register the watch, and return the address only after Indexer acknowledgement.
- Add two durable cursors:
  - ingestion atomically mirrors Indexer events and advances its cursor;
  - projection classifies a mirrored event, appends all absolute ledger corrections, and advances independently.
- On a post-credit reorg, preserve `accounted`, correct canonical balances, create a durable open `PostCreditReorg` reconciliation case, and block automatic credit or collection for that deposit.
- Do not add any Indexer dependency on `sdk/deposits`.

### 8. Add opt-in real-node validation

- Check in a pinned Kurtosis `ethereum-package` profile with two branches, Geth/Lighthouse participants, Dora, OTel, and Disruptoor.
- Start the OTel stack first and require Docker plus explicit `--privileged`; Disruptoor needs Docker socket and host-PID access. See the [Ethereum-package Disruptoor example](https://github.com/ethpandaops/ethereum-package#disruptoor-example) and [Kurtosis privileged-mode guidance](https://docs.kurtosis.com/running-privileged-containers/).
- Index a watched transaction on the branch intended to lose, heal the partition, and verify `Reorged`, replacement replay, cursor uniqueness, and convergence.
- Keep this outside ordinary `cargo test`. Deterministic doubles cover exact depths because Disruptoor cannot guarantee an exact fork depth.

## Test and acceptance plan

- Storage contract suite: atomic conditions, ordered scans, synchronous persistence, second-process rejection, cancellation after enqueue, lost acknowledgement, corrupt envelopes, backup/restore, and migration fixtures.
- Repository suite: watch idempotency/conflict, historical backfill, cursor/revision allocation, response-loss retry, and injected failures at every semantic boundary.
- Ethereum suite: ETH transfer, contract creation, reverted receipt, checked fee multiplication, ERC-20 transfer/mint/burn, malformed logs, receipt mismatch, and unsupported traces.
- Worker suite: sequential blocks; WebSocket jump/duplicate/out-of-order/same-height events; RPC outage; source behind; restart reconciliation; confirmations on empty blocks; reorg depths 1, 12, 49, and 50; depth 51 entering rebuild.
- Rebuild suite: crash/reopen in every manifest phase, invisible staged events, atomic activation, abort safety, correction ordering, and journal preservation.
- HTTP suite: authentication, non-loopback guard, status codes, request/body limits, pagination, and sanitized health.
- Payment Service suite: both deposit/watch crash windows, duplicate delivery, ingestion/projection restart gaps, reorg ledger corrections, and durable manual-reconciliation cases.
- Final validation: formatting, targeted package checks/tests/Clippy, full locked workspace check/test/Clippy/docs, `git diff --check`, chain-deletion dependency check, and then the explicit Kurtosis scenario.

## Assumptions and fixed defaults

- The bootstrap height is mandatory and may be `1` on a local devnet. Public Ethereum never silently starts from block 1.
- Event/revision and Payment Service ledger history are retained indefinitely in v1; only raw block and undo material is bounded to 50.
- One process owns one scope and database path. RocksDB locking plus the serialized writer replaces distributed leasing. High availability and multi-replica deployment require a future PostgreSQL backend.
- Built-in TLS and token hot reload are out of scope. Non-loopback HTTP must stay on a private network or behind TLS termination; token rotation requires restart.
- Imported watch birthdays are supported only at or after the configured bootstrap height.
- ERC-20 logs are atomic facts only. Token decimals, fee-on-transfer reconciliation, rebasing, ERC-721, and ERC-1155 are out of scope.
- Pin dependencies around RocksDB 0.24.0, Bincode 2.0.1, Axum 0.8.4,
  Tokio-Tungstenite 0.28.0, Clap 4.5.50, and the workspace's existing Reqwest,
  Tokio, and Alloy versions. Axum 0.8.5 and newer require a Serde release newer
  than the workspace's custody-compatible 1.0.219 pin, so v1 uses the latest
  compatible Axum patch rather than widening the signing dependency surface.
