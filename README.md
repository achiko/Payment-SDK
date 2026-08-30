# Payment SDK

Payment-SDK is a design-stage Rust workspace whose current implementation is one
Bitcoin/Ethereum/Solana wallet API process. It composes native chain clients,
wallet implementations, embedded indexers, and one central PostgreSQL
database/schema/pool in one process. There are no internal HTTP hops or
separately deployed wallet/indexer services.

Native SOL submission and runtime composition are implemented through the same
chain-neutral wallet and indexing contracts. The checksum-pinned owned Agave
system target is explicit but remains manual evidence; see
`docs/FEATURE_VALIDATION.md` for the tested and unavailable boundary.

The current scope is deliberately small:

- generate or import wallets through one chain-neutral `Wallets` collection;
- index the authoritative wallet address/birthday set with one reusable
  `Indexer` contract;
- read exact selected-asset balances and complete checkpoint-bound history for
  each wallet's configured payment asset;
- submit one transfer or a non-empty ordered batch; and
- survive restarts and retained reorgs without serving orphan history.

Deposit accounting, ledgers, collections/sweeps, payment workflows, custody
services, hardware wallets, raw-block archives, and indexing command APIs are
not part of the workspace.

## Current workspace

```text
apps/api                 explicit composition root and public HTTP process
sdk/chains/base          approved chain-neutral values and capabilities
sdk/chains/bitcoin       Bitcoin RPC, transactions, wallets, and interpretation
sdk/chains/ethereum      Ethereum RPC, transactions, wallets, and interpretation
sdk/chains/solana        Solana RPC, transactions, wallets, and interpretation
sdk/wallets              wallet families, instances, birthdays, and sending
sdk/indexing             Indexer, Composer, synchronization contracts, and collections
sdk/indexing/runtime     reusable async synchronization and readiness loop
sdk/indexing/postgres    scope-bound PostgreSQL indexing repository
sdk/indexing/redb        indexing Blocks/Transactions/Outputs persistence
packages/*               generic HTTP, JSON-RPC, crypto, and storage mechanics
```

`apps/api/src/main.rs` is deliberately the visible object graph. It creates one
long-lived RPC client per configured chain, the chain sources/interpreters and
repositories, one chain index service per scope, a multi-chain `Composer`, and
one `Wallets` collection. The sync task passes `Wallets::filters()` to the same
composed indexer used by query consumers. HTTP receives only the wallet
abstraction and readiness state.

Index persistence is expressed by three nouns: `Blocks` atomically adds and
removes canonical blocks, `Transactions` lists address-primary history, and
`Outputs` lists current live UTXOs. The repository stores only checkpoint,
canonical history, live outputs, and a bounded rollback journal. Address
filters remain caller-owned.

Dependencies point toward generic contracts. Packages import no SDK/application
crate; indexing imports no concrete chain or redb record; chain-native
transaction and RPC semantics stay in each chain crate. After composition,
business and endpoint code do not branch on a concrete chain.

## Documentation

- [`docs/SYSTEM_REQUIREMENTS.md`](docs/SYSTEM_REQUIREMENTS.md) defines canonical
  scope and acceptance criteria.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) defines ownership and dependency
  direction.
- [`docs/CONTRACTS.md`](docs/CONTRACTS.md) describes reusable Rust boundaries.
- [`docs/FEATURE_VALIDATION.md`](docs/FEATURE_VALIDATION.md) separates current
  evidence from accepted implementation gaps.
- [`docs/INDEXING.md`](docs/INDEXING.md) defines synchronization and persistence
  semantics.
- [`docs/refactoring.md`](docs/refactoring.md) shows the target API, composition,
  business usage, and refactoring acceptance evidence.
- [`docs/API.md`](docs/API.md) documents the public HTTP surface.
- [`docs/INDEXING_CENTRAL_DATABASE_PLAN.md`](docs/INDEXING_CENTRAL_DATABASE_PLAN.md)
  is the flat execution plan for shared PostgreSQL and generic indexing work.
- [`docs/SOLANA_IMPLEMENTATION_PLAN.md`](docs/SOLANA_IMPLEMENTATION_PLAN.md) is
  the flat, maximum-small implementation plan for native Solana support.

## Validation

Use locked dependencies:

```bash
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked --workspace --no-deps
cargo run --locked -p design-lint -- --policy lint.toml check .
git diff --check
```

Tests use loopback RPC doubles and temporary databases. Never point tests or
examples at a funded key or public broadcast endpoint.
