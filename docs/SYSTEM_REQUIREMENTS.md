# System requirements

This is the canonical scope for the design-stage workspace.

## Product boundary

The system MUST provide one API process that can:

1. initialize Bitcoin and Ethereum RPC clients and synchronizers;
2. generate or import a wallet through a chain-neutral provider API;
3. return a wallet's canonical address, exact balance, and transaction history;
4. send one transfer or a non-empty batch of transfers from a wallet;
5. register watches and continuously index relevant chain activity;
6. survive restarts without losing watches, canonical checkpoints, history, or
   the ability to correct a reorg; and
7. expose these capabilities without business code importing a concrete chain.

The current system MUST NOT contain deposit accounting, ledgers, collection
jobs, payment state machines, hardware-wallet workflows, remote custody, or
separate wallet/indexer microservices. There are no compatibility versions: a
type is renamed or replaced directly while the project remains pre-release.

## Layering

### `packages/*`

- MUST remain useful outside this repository's blockchain domain.
- MAY depend on external crates and other packages.
- MUST NOT import SDK or application crates.
- HTTP helpers MUST be transport mechanics, not wallet/indexing response DTOs.
- JSON-RPC MUST delegate protocol framing and correlation to `jsonrpsee` and own
  only bounded transport, retry, and ordered endpoint failover.
- Crypto MUST contain no chain names, addresses, transactions, or wallet policy.

### `sdk/chains/base`

- MUST remain small and explicitly approved.
- MUST contain only semantics common across substantially different chains.
- MUST NOT import a concrete chain, RPC implementation, indexing, or wallets.
- MUST NOT define a universal chain transaction or universal RPC interface.

### Concrete chain crates

- MUST own their native address, RPC, block, transaction, signing, and indexing
  translation semantics.
- MUST contain `src/address.rs`, `src/batch.rs`, `src/error.rs`, `src/lib.rs`,
  and directory modules `src/indexer/mod.rs`, `src/rpc/mod.rs`,
  `src/transaction/mod.rs`, and `src/wallet/mod.rs`; design lint MUST enforce
  this skeleton.
- MUST use mature external protocol libraries when they correctly implement a
  standard; local code SHOULD be limited to the repository's abstraction and
  policy gaps.
- MUST keep chain names out of exported type names when the crate namespace is
  already sufficient (`bitcoin::Address`, not `BitcoinAddress` internally).
- MUST be independently deletable without breaking generic crates.

### `sdk/indexing`

- MUST define storage-independent synchronization, address-watch, history,
  checkpoint, and repository contracts.
- MUST NOT know RocksDB physical keys, HTTP routes, wallets, or business labels.
- Traits SHOULD contain one to three tightly coupled operations.
- A checkpoint MUST include height and hash.
- A transaction MUST preserve all stable movements, including multiple UTXO
  inputs and outputs.
- A block commit MUST atomically persist canonical identity, effects, undo,
  observation revisions, and checkpoint movement.
- A reorg MUST produce new revisions and preserve previous facts.
- MUST NOT expose an event feed, raw-block archive, backfill command, rebuild
  command, watch deactivation, or migration surface in the current design.
- Persistence MUST be expressed by `CanonicalStore`, `WatchStore`,
  `BlockStore`, `HistoryStore`, and `StatusStore`. Each trait MUST have at most
  three cohesive load/save/query methods. `Index<R>` MUST be the consumer
  facade and semantic planning MUST remain outside storage adapters.
- RPC failures MUST remain unknown/retryable and MUST NOT imply a dropped tx.

### `sdk/indexing/rocksdb`

- MUST implement indexing repository contracts only.
- MUST own all RocksDB key/value encoding for indexing records.
- MUST NOT leak RocksDB record types through generic indexing APIs.
- MUST expose `Repository`; synchronizer construction belongs to `apps/api`, not
  to a storage-owned runtime or handle.

### `sdk/wallets`

- MUST expose small capabilities usable through `dyn Wallet`.
- MUST support provider-selected generation/import without returning secrets.
- MUST read balance and history through indexing abstractions.
- MUST preserve exact asset precision and full movement history.
- MUST expose one small sending capability without exposing concrete chain
  transaction types to API/business callers.
- MUST NOT own indexing persistence or background synchronization.

### `apps/api`

- MUST be the only executable and composition root.
- MUST directly compose wallet providers, chain RPC clients, synchronizers,
  RocksDB repositories, and public HTTP routes.
- MUST supervise indexing and HTTP tasks in one process.
- MUST pass abstractions into handlers rather than construct dependencies per
  request.
- MUST keep transport DTOs at the edge and secrets out of responses.

## Bitcoin requirements

- Address parsing MUST use the standard Bitcoin library and enforce network.
- Transactions MUST retain exact outpoints and multiple inputs/outputs.
- Signing MUST support the explicitly implemented address/script kinds and
  verify each input belongs to the signer before signing.
- Fee calculations MUST use checked integer satoshi units.
- Indexed history MUST expose input and output movements separately.
- Reorg rollback MUST restore spent-output state exactly.

## Ethereum requirements

- Addresses MUST parse canonical 20-byte values without storing an `0x` prefix
  in base address bytes.
- Transaction building MUST validate chain ID, nonce, gas, and EIP-1559 fees.
- The recovered signer MUST match the requested sender before accepting a
  signed envelope.
- Native and token movements MUST be represented as distinct assets.
- Indexed receipts/logs and reorg corrections MUST preserve exact U256 values.

## API requirements

The public API MUST have chain-neutral wallet routes for:

- creating a wallet for a configured chain/network;
- reading wallet metadata and address;
- reading current indexed balance; and
- reading paginated full transaction history; and
- sending one or several exact transfers.

Route paths MUST remain chain-neutral rather than duplicate one router for
Bitcoin and another for Ethereum. The create request selects a registered
chain; its network is fixed by startup configuration and returned in wallet
metadata. Authentication and request limits belong to the public server
boundary. Health MUST distinguish process liveness from indexing readiness.

The batch send input MUST be one non-empty ordered list of
`(wallet, destination, amount)` transfers. Every destination, amount, fee bound,
wallet/chain compatibility rule, and chain invariant MUST validate before the
first broadcast. One request MUST target one chain; mixed-chain batches are
rejected rather than reordered into several unrelated partial operations.

Bitcoin MUST build one chain-native transaction for the batch. It MAY consume
several UTXOs from several source wallets and MUST create one requested output
per transfer. Every source is read against the same canonical output checkpoint
and receives its own change output when needed. Fee allocation MUST be
deterministic, and each input MUST be signed by its owning wallet. A successful
batch returns one submitted transaction ID; any pre-submit error returns no
accepted ID.

Ethereum MUST build one chain-native transaction per transfer and broadcast
strictly in input order. Nonces MUST be consecutive for transfers from the same
source wallet. Several broadcasts are not atomic. On failure the result MUST
identify the accepted prefix and the first failed transfer; it MUST NOT imply
later transfers were attempted.

The sender MUST preserve exact signed bytes across ambiguous broadcast outcomes
where retries are supported, and MUST treat indexing, not RPC acceptance, as
confirmation. Sending is a wallet operation; it MUST NOT introduce deposits,
accounting, collections, reservations, jobs, or sweep terminology.

## Quality gates

Completion requires all of the following against the current workspace:

```bash
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked --workspace --no-deps
cargo run --locked -p design-lint -- check .
git diff --check
```

System tests MUST compose the real API facade, wallet providers, chain RPC
doubles, synchronizer, and RocksDB repository in one process. Tests MUST cover
restart and reorg behavior for both chains and must not contact public networks.
