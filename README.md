# Payment SDK

This workspace is a design-stage Rust SDK and one executable API for Bitcoin
and Ethereum wallets. The API embeds indexing synchronizers and RocksDB storage in
the same process. There are no internal HTTP hops and no separately deployed
wallet or indexer services.

The current scope is deliberately small:

- create Bitcoin and Ethereum wallets through one chain-neutral wallet API;
- watch addresses and index blocks with reorg-safe checkpoints;
- read wallet balances and complete transaction history; and
- send one or several transfers through the same wallet abstraction; and
- compose concrete RPC clients, indexers, storage, and wallet providers once
  in `apps/api`.

Deposit accounting, collections, payment workflows, custody services,
hardware wallets, remote signers, and service-to-service transports are not in
the workspace. They may be designed later on top of the wallet and indexing
contracts, but are not retained as dormant V1/V2 code.

## Workspace

```text
apps/api                 one process and composition root
sdk/chains/base          approved chain-neutral values and small capabilities
sdk/chains/bitcoin       Bitcoin-native RPC, transactions, wallets, indexing
sdk/chains/ethereum      Ethereum-native RPC, transactions, wallets, indexing
sdk/wallets              chain-neutral wallet capabilities and provider map
sdk/indexing             chain-neutral indexing contracts and synchronizer
sdk/indexing/rocksdb     indexing repository implementation
packages/*               generic HTTP, JSON-RPC, crypto, and storage mechanics
apps/api/tests           deterministic composed-binary tests
```

Dependencies point inward: `packages/*` are generic, SDK crates implement
reusable domain behavior, and `apps/api` chooses concrete implementations.
Concrete chain crates never leak into generic indexing, wallets, or packages.
They retain chain-native transaction/signing implementations; the wallet/API
surface exposes one small send capability without inventing one universal
Bitcoin/Ethereum transaction representation.

## Validation

Use locked dependencies:

```bash
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked --workspace --no-deps
cargo run --locked -p design-lint -- check .
git diff --check
```

The deterministic tests use loopback RPC doubles. Do not point tests or
examples at a funded key or live broadcast endpoint.

See [`docs/API.md`](docs/API.md) for the current single-process configuration
and public wallet routes.
