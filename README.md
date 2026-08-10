# Payment SDK services and chain-native execution

This workspace combines contract-first boundaries with concrete Ethereum v1
and Bitcoin block-only v1 service slices. It contains a stateless
Bitcoin/Ethereum Wallet Service library, authenticated chain-specific Wallet
HTTP runtimes, durable Ethereum and Bitcoin Indexer Service modes, and a
single-network Payment Service mode for Ethereum or native Bitcoin, each backed
by its own RocksDB database. A loopback-only ephemeral custody process supports
disposable local development. PS and IX never open or write each other's
storage.

The implemented wallet capabilities are:

- Ethereum EOA generation, balances, EIP-1559 build/sign/broadcast/receipt,
  native collection, and ERC-20 gas requirements plus one-transfer collection;
- Bitcoin native SegWit v0 and Taproot address generation, exact caller-selected
  UTXO validation, chain-native signing, canonical IX-backed balances, Core
  preflight/broadcast/receipts, and batched collection attribution;
- a generic `wallet_worker::WalletService` facade that shares one injected
  custody backend between provisioning and signing without owning persistence;
- chain-specific Ethereum and Bitcoin Wallet HTTP modes using concrete node/IX
  adapters and an authenticated remote key-provisioning/signing adapter;
- Ethereum and Bitcoin IX process modes with durable watches, canonical
  checkpoints, observations, revisions, reorg recovery, and a cursor feed;
- a Bitcoin IX canonical UTXO projection with generation/revision/checkpoint
  snapshot pagination and atomic connect, spend, rollback, rebuild, and cleanup
  behavior;
- an Ethereum PS process with authenticated APIs, durable jobs, deposit/watch
  recovery, IX mirroring, business projection, absolute ledgers, typed
  reconciliation, and native/ERC-20 collection workflows;
- a nested Bitcoin PS mode with strict policy binding, P2WPKH/P2TR deposits,
  same-principal multi-deposit jobs, atomic exact-outpoint reservations,
  deterministic shared-fee allocation, retained signed bytes, and block-only
  confirmation/reorg projection. V1 permits only one Bitcoin collection
  aggregate per deposit; later receipts remain watched/accounted but cannot
  start another collection.

`signer-local` and `apps/custody` are deliberately ephemeral and not production
custody. `signer-remote` supplies the hardened client contract, but durable
custody and secret storage remain external deployment responsibilities. The
checked-in code does not by itself prove a live production deployment:
production custody integration, operational TLS configuration, monitoring,
automated Anvil acceptance, and real-node evidence remain separate work. The
Bitcoin Core 31 disposable-regtest acceptance scenario has not been run on this
checkout because the required Core binary is unavailable; source and
deterministic tests are not a substitute for that operational evidence. The
checked-in [`manual procedure`](./docs/manual-bitcoin-regtest/README.md) remains
unexecuted and does not change that status.
Both PS modes deliberately exclude HA and multi-network ownership in one
process/database. Bitcoin PS source and deterministic coverage do not prove a
live Core-backed deployment, and no live/funded Bitcoin broadcast was performed
as part of this implementation.

The previous experimental workspace is preserved under [`old/`](./old). The
current workspace is organized by architectural ownership:

```text
apps/                             executable composition roots
sdk/
├── chains/                       concrete chains, identities, shared capabilities
├── deposits/                     PS deposits, event mirror, ledger, collection state
├── transactions/                 reusable UTXO and account construction
├── signing/                      chain-independent signing
├── indexing/                     block synchronization and reorg contracts
└── storage/                      backend-independent atomic storage
packages/                         non-blockchain HTTP, JSON-RPC, transport, telemetry
reference/                        ignored shallow upstream research checkouts
```

The principal documents are:

- [`docs/SYSTEM_REQUIREMENTS.md`](./docs/SYSTEM_REQUIREMENTS.md): canonical consolidated requirements and Mermaid flows;
- [`ARCHITECTURE.md`](./ARCHITECTURE.md): ownership and dependency rules;
- [`docs/CONTRACTS.md`](./docs/CONTRACTS.md): how the current traits compose;
- [`docs/INDEXING.md`](./docs/INDEXING.md): implemented indexing and reorg model;
- [`docs/INDEXER_SERVICE.md`](./docs/INDEXER_SERVICE.md): concrete Ethereum IX v1 runtime;
- [`docs/WALLET_SERVICE.md`](./docs/WALLET_SERVICE.md): concrete stateless Ethereum and Bitcoin WS runtimes;
- [`docs/BITCOIN_SERVICES.md`](./docs/BITCOIN_SERVICES.md): Bitcoin Core 31 prerequisites, chain-specific IX/WS/PS configuration, APIs, ownership, policy, and acceptance status;
- [`docs/manual-bitcoin-regtest/README.md`](./docs/manual-bitcoin-regtest/README.md): unexecuted, opt-in Core 31 regtest acceptance commands and evidence matrix;
- [`docs/WALLET_SERVICE_USAGE.md`](./docs/WALLET_SERVICE_USAGE.md): step-by-step native ETH and ERC-20 Rust library usage;
- [`docs/PAYMENT_SERVICE.md`](./docs/PAYMENT_SERVICE.md): concrete Ethereum PS v1 runtime and API;
- [`docs/manual-local-stack/README.md`](./docs/manual-local-stack/README.md): manual Anvil, IX, custody, WS, and PS startup using one private `.env` file;
- [`docs/PAYMENT_SERVICE_USAGE.md`](./docs/PAYMENT_SERVICE_USAGE.md): step-by-step PS startup, complete curl request catalog, recovery, and maintenance usage;
- [`docs/PAYMENT_SERVICE_POSTMAN.md`](./docs/PAYMENT_SERVICE_POSTMAN.md): Postman import, variables, safe manual smoke flow, and mutation gates;
- [`docs/PAYMENT_SERVICE.postman_collection.json`](./docs/PAYMENT_SERVICE.postman_collection.json): importable Payment Service Postman collection;
- [`docs/PAYMENT_SERVICE_AGENT_RUNBOOK.md`](./docs/PAYMENT_SERVICE_AGENT_RUNBOOK.md): safe local PS startup and evidence checklist for agent handoffs;
- [`payment_sdk_demo/README.md`](./payment_sdk_demo/README.md): offline Bitcoin wallet and Payment Service samples plus the environment-backed Bitcoin indexer sample;
- [`docs/FEATURE_VALIDATION.md`](./docs/FEATURE_VALIDATION.md): original PS/WS/IX feature traceability and corrections;
- [`docs/RESEARCH.md`](./docs/RESEARCH.md): upstream type and architecture findings;
- [`docs/REQUIREMENTS.md`](./docs/REQUIREMENTS.md): decisions still required before implementation;
- [`reference/README.md`](./reference/README.md): local reference repositories and revisions.

To start Anvil, IX, local ephemeral custody, WS, and PS one process at a time
with a single private `.env` file, follow the
[`manual local stack guide`](./docs/manual-local-stack/README.md).

As an optional one-command alternative, start IX, custody, WS, and PS against
an already-running loopback Ethereum RPC with:

```bash
./scripts/run-local-payment-services.sh --disposable-policy
```

The launcher does not start or stop Anvil. See the
[`Payment Service usage guide`](./docs/PAYMENT_SERVICE_USAGE.md#one-command-local-service-launcher)
for configuration, generated credentials, logs, and shutdown behavior.

Run the structural compile check with:

```bash
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
```

Generate ephemeral test keys and chain-native addresses with:

```bash
cargo run --locked -p chain-ethereum --example ethereum_test_wallet
cargo run --locked -p chain-bitcoin --example bitcoin_test_wallet
```

These examples are offline and print only public information. Their private
keys remain in process memory and are discarded when each example exits.

Run the complete three-asset Wallet Service composition example with:

```bash
cargo run --locked -p wallet-worker --example three_asset_wallet_service
```

The example configures Bitcoin Testnet4, native ETH on Ethereum mainnet, and
Ethereum-mainnet USDC. It demonstrates Bitcoin collection, an explicit ETH
build/sign/broadcast flow, and USDC gas-requirement plus collection handling.
Its deterministic RPC implementations are offline test doubles: replace
`DemoBitcoinRpc`, `DemoEthereumRpc`, and the ephemeral `LocalSigner` with
authenticated production adapters before connecting the composition to funds.

The offline tests exercise Bitcoin SegWit/Taproot signing, Ethereum EIP-1559
signing, balance reads, collection requirements, collection submission, and the
shared-custody Wallet Service composition.

### Live native-ETH transaction example

[`apps/wallet/examples/live_ethereum_transaction.rs`](./apps/wallet/examples/live_ethereum_transaction.rs)
is an opt-in executable that builds a real EIP-1559 transfer from live RPC
state. Start with a funded development/testnet key, keep the key out of shell
history and source control, and set these environment variables:

- `ETH_RPC_URL`: an authenticated HTTPS Ethereum JSON-RPC endpoint (plain HTTP
  is accepted only for a loopback development node);
- `ETH_PRIVATE_KEY`: the sender's 32-byte secp256k1 private key;
- `ETH_TO`: the `0x`-prefixed recipient address;
- `ETH_VALUE_WEI`: the base-10 native-ETH amount in wei;
- `ETH_CHAIN_ID`: the expected chain ID, checked against the RPC before build.

With those variables configured, review the freshly queried nonce, balance,
gas estimate, fee caps, and maximum debit without signing:

```bash
cargo run --locked -p wallet-worker --example live_ethereum_transaction
```

Signing and broadcasting are independent approvals. Sign without broadcasting
with:

```bash
ETH_SIGN_TRANSACTION=true \
  cargo run --locked -p wallet-worker --example live_ethereum_transaction
```

After reviewing every field, explicitly enable both operations to send it:

```bash
ETH_SIGN_TRANSACTION=true \
ETH_BROADCAST_TRANSACTION=I_UNDERSTAND \
  cargo run --locked -p wallet-worker --example live_ethereum_transaction
```

The example never prints the private key or raw signed envelope. It verifies
that the node's returned transaction hash equals the locally computed hash.
RPC acceptance is not on-chain confirmation, so production code must monitor
the receipt and serialize builds for each sender to avoid pending-nonce races.
The environment-backed signer and transaction-only RPC adapter are sample
wiring; production custody, authenticated transport policy, and durable receipt
tracking remain deployment responsibilities.
