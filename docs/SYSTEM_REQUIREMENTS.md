# System requirements

This file defines the canonical scope for the design-stage workspace.

## Product boundary

The system MUST provide one API process that can:

1. initialize Bitcoin and Ethereum RPC clients and embedded indexers;
2. generate or import a wallet through a chain-neutral wallet collection;
3. return a wallet's canonical address, exact balance, and complete paginated
   transaction history;
4. send one transfer or a non-empty ordered batch;
5. continuously index the authoritative wallet address/birthday set;
6. survive restarts without losing canonical checkpoints, history, live
   outputs, or retained reorg recovery; and
7. expose business and HTTP behavior without importing a concrete chain after
   composition.

The current system MUST NOT contain deposit accounting, ledgers, collection or
sweep jobs, payment state machines, reservation systems, hardware-wallet
workflows, remote custody, separate wallet/indexer processes, or internal
wallet/indexer transports.

The project is pre-release. Obsolete internal types and persisted formats MUST
be replaced directly rather than retained behind compatibility versions or
migrations. External Bitcoin, Ethereum, HTTP, and JSON-RPC standards remain
compatibility contracts.

## Layering

### `packages/*`

- MUST remain useful outside blockchain projects.
- MAY depend on external libraries and other packages.
- MUST NOT import SDK or application crates.
- HTTP helpers MUST remain transport mechanics, not wallet/indexing DTOs.
- JSON-RPC MUST delegate framing/correlation to `jsonrpsee` and own only
  bounded transport, retry, and ordered endpoint failover.
- Crypto MUST contain no chain names, addresses, transactions, assets, or
  wallet policy.
- Generic RocksDB mechanics MUST remain separate from indexing records.

### `sdk/chains/base`

- MUST remain small and explicitly approved.
- MUST contain only semantics shared across substantially different chains.
- MUST NOT import a concrete chain, RPC implementation, indexing, or wallets.
- MUST NOT define a universal chain RPC or native transaction representation.

### Concrete chain crates

- MUST own native address, network validation, RPC, blocks, transactions,
  signing, fees, wallets, and indexing interpretation.
- MUST use mature external protocol libraries where they correctly implement a
  standard; local code SHOULD cover only abstraction or policy gaps.
- MUST keep redundant chain prefixes out of types within the crate namespace.
- MUST satisfy the common directory skeleton enforced by design lint.
- MUST be independently deletable without breaking generic crates or the other
  concrete chain.

### `sdk/indexing`

- MUST define storage-independent block synchronization, scoped address
  filters, canonical history, checkpoints, output projections, and persistence
  collections.
- MUST NOT know RocksDB keys/records, HTTP routes, wallets, or business labels.
- A checkpoint MUST contain height and hash.
- `Service` for one scope and `Composer` for several scopes MUST implement the
  same `Indexer` contract.
- `Indexer::sync` MUST accept the complete caller-owned `AddressFilter`
  snapshot for that invocation. Indexing MUST NOT own or persist watches or an
  address registry.
- `Composer` MUST validate the complete snapshot before effects, partition it
  by scope, synchronize every configured child, and reject empty composition
  or duplicate scopes.
- Filter addresses MUST be non-empty, unique, and belong to a configured scope;
  invalid snapshots MUST fail before source I/O.
- `Outputs` MUST remain an independent optional capability rather than an
  `Indexer` requirement.
- A transaction MUST preserve every stable movement, including separate
  Bitcoin inputs and outputs and distinct native/token assets.
- Confirmation MUST be derived from inclusion height and the page checkpoint,
  not stored as a transition.
- Confirmation policy MUST be depth-only. `Confirmed` MUST report the observed
  depth and MUST NOT claim chain-finality proof.
- History and output pagination MUST be checkpoint-bound and reject a changed
  snapshot.
- Persistence MUST be expressed by `Blocks`, `Transactions`, and `Outputs`.
- `Blocks::add` MUST atomically compare/move the checkpoint, persist canonical
  address-primary history, apply live output changes, and write/prune the
  storage-derived rollback journal.
- `Blocks::remove` MUST verify the expected current tip and derive its entire
  inverse from the repository's private journal. Callers MUST NOT supply undo
  state.
- A retained reorg MUST remove orphan history and restore live outputs.
  `ReorgTooDeep` MUST require recreation/rescan when the common ancestor is
  outside retention.
- RPC failures MUST remain retryable/unknown and MUST NOT imply a dropped
  transaction.
- Synchronization errors MUST be typed errors. `SyncStatus` MUST describe only
  active progress (`CatchingUp` or `Ready`), not cache failure variants.
- MUST NOT expose a public event feed, raw-block archive, backfill/rebuild
  command, watch lifecycle, or migration surface.

### `sdk/indexing/rocksdb`

- MUST implement indexing persistence collections only.
- MUST own all indexing keys, records, codecs, ordered scans,
  compare-and-swap conditions, atomic batches, and journal encoding.
- MUST expose no storage record or undo type through generic indexing APIs.
- MUST persist only canonical checkpoint, address-primary history, current
  live outputs, and bounded rollback journal.
- MUST NOT own a synchronizer task, runtime handle, filter registry, or public
  service.

### `sdk/wallets`

- MUST expose one `Wallets<I, F>` collection for chain-neutral application
  behavior.
- MUST own the family map `(IndexScope, Provider, Sender)`, constructed wallet
  instances/public metadata, and authoritative address birthdays.
- MUST inject the composed `Arc<dyn Checkpoint>` needed for safe runtime
  birthdays and expose the complete deduplicated filters needed by the sync
  task.
- MUST support provider-selected generation/import without returning secrets.
- Import MUST require exclusive startup access and an explicit birthday.
  Runtime generation MUST start after the current checkpoint, or at zero when
  no checkpoint exists.
- MUST expose get, exact balance, full history, one send, and ordered batch
  send without leaking concrete chain transaction types.
- One-wallet build/prepare/broadcast/ID verification MUST live on the wallet
  abstraction, not in HTTP.
- MUST NOT own indexing persistence, a background runtime, or durable custody.

### `apps/api`

- MUST be the only executable and composition root; no crate may depend on it.
- `main.rs` MUST explicitly construct and connect RPC clients, repositories,
  chain services, `Composer`, wallet families, sync, readiness, and HTTP.
- MUST NOT hide this object graph behind a process or service facade.
- MUST share one concrete composed indexing object through narrow trait views.
- MUST load/import the complete authoritative startup wallet set before the
  first sync.
- MUST supervise synchronization and HTTP in one process and own cancellation,
  fatal-task handling, and graceful shutdown.
- MUST pass `Wallets` and readiness into handlers rather than construct
  dependencies per request.
- MUST keep endpoint-specific wire input/output structs immediately above
  their handler and generate one Utoipa contract from those routes.
- MUST keep secrets out of JSON, schemas, logs, and ordinary `Debug`.

## Address coverage requirements

- A fresh scope with earliest birthday `B > 0` MUST establish `B - 1` as its
  parent anchor and interpret from `B` forward.
- Birthday zero MUST interpret from genesis. A fresh scope with no addresses
  MUST establish the current source tip as an empty anchor.
- A restart MUST resume at the persisted checkpoint plus one after verifying
  its hash remains canonical.
- A generated runtime wallet MUST begin at the next block and require no
  historical backfill.
- Historical import MUST require exclusive startup access and MUST be
  unavailable after the wallet collection is shared.
- On restart, the embedding application MUST supply the same complete
  historical address/birthday set that produced the checkpoint. Because filters
  are deliberately not persisted, a changed set requires explicit scope
  recreation/rescan and cannot be auto-detected safely.

## Bitcoin requirements

- Address parsing MUST use the standard Bitcoin library and enforce the
  configured network.
- Transactions MUST retain exact outpoints and all inputs/outputs.
- Signing MUST support only the explicitly implemented script/address kinds and
  verify each input belongs to its signer.
- Fee calculations MUST use checked integer satoshis.
- Indexed history MUST expose input and output movements separately.
- Reorg rollback MUST restore spent output state exactly.

## Ethereum requirements

- Addresses MUST parse canonical 20-byte values without storing an `0x` prefix
  in base address bytes.
- Transaction building MUST validate chain ID, nonce, gas, and EIP-1559 fees.
- The recovered signer MUST match the requested sender before a signed envelope
  is accepted.
- Native and token movements MUST use distinct assets.
- Receipts/logs and reorg correction MUST preserve exact `U256` values.

## Sending requirements

The public send input MUST be one non-empty ordered list of wallet,
destination, and exact amount. Destination syntax, positive amounts, fee
bounds, wallet/family compatibility, and chain invariants MUST validate before
the first broadcast. A request MUST target one family; mixed-chain batches are
rejected rather than split.

Bitcoin MUST build one native transaction for a compatible batch. It MAY
consume UTXOs from several source wallets, MUST read them at one output
checkpoint, create one requested output per transfer, preserve per-source
change, allocate fees deterministically, and sign each input with its owner. A
successful batch returns one submitted ID; a pre-submit failure returns none.

Ethereum MUST build one native transaction per transfer and broadcast in input
order with consecutive nonces per source. On failure it MUST report the
accepted prefix and first failed input and MUST NOT imply later inputs were
attempted.

The sender MUST preserve exact signed bytes across retryable ambiguous outcomes
and verify the returned ID against those bytes. Submission MUST NOT be called
confirmation; indexing provides canonical confirmation.

## HTTP requirements

The public API MUST provide chain-neutral routes for:

- generating a wallet for a configured family;
- reading wallet metadata/address;
- reading current indexed balance;
- reading paginated complete transactions;
- sending one exact transfer; and
- sending a non-empty ordered batch.

Network is selected at startup and returned as wallet metadata, not accepted as
untrusted route policy. Authentication and request limits belong to the server
boundary. Liveness and indexing readiness MUST be distinct.

Every endpoint has one handler. Its endpoint-specific request and response
objects MUST be directly above it, simple serde/Utoipa wire structs. Handlers
MUST do extraction, one `Wallets` call, error/status mapping, and encoding only.

## Quality gates

Completion requires:

```bash
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked --workspace --no-deps
cargo run --locked -p design-lint -- --policy lint.toml check .
git diff --check
```

System tests MUST compose the public router, wallet families, one composed
indexer, chain RPC doubles, synchronizer, and temporary RocksDB in one process.
They MUST cover birthdays, restart, retained reorg, orphan removal, output
restoration, one and batch sends, readiness, and shutdown without contacting a
public network.
