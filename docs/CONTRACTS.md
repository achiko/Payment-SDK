# Contract walkthrough

This document describes the public contracts that exist in the current
workspace. Applications compose concrete implementations; SDK crates own
reusable blockchain behavior; packages remain transferable infrastructure.

## Shared chain values

[`sdk/chains/base`](../sdk/chains/base/src/lib.rs) contains the deliberately
small set of values and capabilities shared by concrete chains:

- `Address` is opaque bytes. Encoding, checksum, and network validation remain
  in a concrete chain. `Addresser::address` is the small capability for values
  that expose an address.
- `NetworkId<R>` stores a chain-selected identifier and `NetworkKind`
  distinguishes mainnet from a named test network.
- `Chain<R>` describes one network with its ID, name, and ticker. It is runtime
  metadata, not a universal protocol implementation.
- `Asset<R>` describes a chain asset. `Asseter` exposes its chain, network,
  name, and ticker.
- `BlockHeight`, `BlockHash`, and `BlockRef` are canonical block primitives.
- `Decimal` is the only protocol-neutral amount. It stores an arbitrary-size
  signed coefficient and an explicit decimal scale, performs checked exact
  arithmetic, and serializes through stable parts. Floating point is never
  used for money.
- `DerivationPath`, `KeyPair`, signing requests, signatures, and the one-method
  `Signer` are the minimal cryptographic boundary needed by chains.

Concrete chains validate conversions between `Decimal` and native integer
units such as satoshis or wei. Indexing and payment code do not define a second
atomic-amount type.

Indexing identities are scoped values rather than bare chain/text pairs.
`CanonicalAddress` and `TransactionRef` contain the complete `IndexScope`
(`chain + network`) in which their text is canonical. Query, watch, HTTP, and
persistence boundaries require that embedded scope to equal the request scope,
so identical address or transaction text on two networks cannot collide.

## Signing and transaction construction

[`Signer`](../sdk/chains/base/src/signer.rs) receives a precomputed
cryptographic payload plus its scheme, encoding, public-key format, and
optional key tweak. It returns a signature and public key. It does not receive
RPC clients, chain transactions, wallet policy, custody capabilities, or user
interaction requests. `KeyPair<A>` is currently the only signer
implementation; private bytes are zeroized and are neither cloned nor debug
printed.

[`TransactionBuilder`](../sdk/chains/base/src/transaction.rs) is the common
construction surface:

```text
transfer(destination, Decimal)
snapshot() -> serializable, versioned TransactionSnapshot
prepare() -> serializable SignedTransaction
```

`BuilderCast::utxo` optionally exposes the UTXO-only controls `inputs` and
`change`; account-model builders return no UTXO capability. Concrete builders
own RPC population, fee rules, chain encoding, and signing.

`SignedTransaction` is durable data containing a version, chain-owned kind,
canonical textual transaction ID, and exact signed envelope. Its debug output
redacts envelope bytes. Applications persist this value before invoking the
one-method `Broadcaster`. A retry therefore submits identical bytes without
rebuilding or signing. Submission is not confirmation: indexing reports
confirmation, failure, replacement, and reorg state.

## Wallet abstraction

[`sdk/wallets`](../sdk/wallets/src/lib.rs) is the protocol-neutral wallet
composition layer. Its small capabilities are:

- `BalanceReader::balance` returns an exact `Decimal` and optional observed
  block;
- `AmountFormat::display_amount` converts an exact scale-zero indexing or
  reservation value into the configured wallet asset's display precision;
- `TransactionFactory` creates a builder and exposes its broadcaster;
- `CollectionFactory` optionally exposes a selected-output `Collector`;
- `Sweeper` optionally prepares an account-model full-balance drain;
- `TransactionRestore::restore` reconstructs a builder from a durable snapshot
  only after the concrete wallet verifies its wallet ID, scope, source, network,
  and asset configuration;
- `HistoryReader::history` returns wallet-owned transaction, movement, fee,
  status, and asset DTOs with a cursor. Indexing persists exact scale-zero
  atomic units; `AmountFormat` and history conversion turn those values into display-unit
  `Decimal` amounts using the asset's own precision. An Ethereum token wallet
  therefore uses token decimals for token movements and 18 decimals for its
  native network fee;
- `Addresser` exposes the wallet address;
- `AddressFormat` converts opaque address bytes to and from chain-native text;
- the one-method `Signer` lets a builder request signatures from the source
  wallet without learning how its key was created.

`Wallet` is the marker combining those capabilities. It deliberately does not
contain key creation, mnemonic derivation, RPC configuration, or polling for a
receipt.

`CollectionFactory::collector` defaults to `None`. A supported
[`Collector`](../sdk/wallets/src/collector.rs) receives one or more source
wallets with exact `SelectedOutput` reservations, one destination, and returns
`PreparedCollection`. For this UTXO capability, the result contains the exact
`SignedTransaction` plus its factual network fee. Both selected amounts and
the returned fee are scale-zero chain-native atomic `Decimal` values. The
application—not the chain crate—owns fee allocation policy.

A `SelectedOutput` carries only indexing's stable `OutputId` and the amount
observed when it was reserved. The concrete implementation must reload and
validate current chain evidence; locking scripts never enter the generic
wallet contract. Bitcoin's collector reloads all sources at one checkpoint,
rejects duplicate/missing/wrong-owner/amount-drifted selections, orders inputs
canonically, and signs each input with its source wallet. Ethereum returns the
default `None` because selected-output collection does not describe its account
model.

The one-method `Sweeper::sweep(destination)` is the account-model drain
capability. Ethereum native wallets estimate their maximum fee first and sign
for `balance - fee`; token wallets transfer the full token balance only after
proving their separate native balance can pay that fee. The returned
`PreparedCollection` records the signed transaction and scale-zero fee
ceiling as `PreparedFee::Limit`. PS persists the limit before broadcast and
rejects a receipt fee above it. A lower effective price or gas use leaves a
factual residual balance; IX receipt facts, never the limit, drive accounting.
Bitcoin leaves this capability unsupported because its exact UTXO
drain belongs to `Collector`.

Transaction snapshots are versioned JSON data. They contain construction
intent, never RPC or signer handles. An application may persist a snapshot and
later pass it back to the same abstract wallet through `TransactionRestore`.
Bitcoin additionally restores its input policy and change address; Ethereum
restores its single native or token transfer. A different wallet, network, or
asset is an `InvalidSnapshot` error.

`Provider` constructs a concrete `Arc<dyn Wallet>` from secret bytes.
`Wallets<K>` is the composition-time provider collection; `K` is a finite,
application-owned key:

```rust,ignore
enum WalletKey { Bitcoin, Ethereum }
let mut wallets = Wallets::new();
wallets.register(WalletKey::Bitcoin, bitcoin_provider)?;
wallets.register(WalletKey::Ethereum, ethereum_provider)?;

let wallet = wallets.new_wallet(&WalletKey::Bitcoin, secret).await?;
let balance = wallet.balance().await?;
let mut transaction = wallet.transaction();
transaction.transfer(destination, amount)?;
let signed = transaction.prepare().await?;
persist(&signed)?;
wallet.broadcaster().broadcast(&signed).await?;
```

Each key maps to exactly one provider. Missing keys fail at lookup and duplicate
keys fail during startup registration instead of relying on registration order.

[`apps/wallet`](../apps/wallet/src/lib.rs) exposes authenticated wallet summary,
balance, history, transaction preparation, and exact-envelope broadcast routes.
Preparation returns the complete serializable `SignedTransaction`; a separate
transaction-addressed `PUT` accepts that same value for retry-safe broadcast.
The caller persists it and owns durable command idempotency because WS remains
stateless. Its executable can compose native
Bitcoin and/or Ethereum wallets, focused RPC capabilities, and remote Indexer
readers from explicit `WS_*` environment variables. Complete configuration is
required per enabled chain; with neither chain configured it is live and not
ready. Embedding hosts may still use `Service::with`. The workspace contains
no production custody service; environment-backed local keys are an explicit
initial composition, not an invented generic wallet policy.

## Concrete chains and RPC

[`sdk/chains/bitcoin`](../sdk/chains/bitcoin/src/lib.rs) and
[`sdk/chains/ethereum`](../sdk/chains/ethereum/src/lib.rs) own all protocol
specific types, validation, builders, signed-envelope inspection, index
interpretation, and RPC methods. Each crate has its own `rpc/` module.
Chain-specific DTOs and method semantics stay there; generic JSON-RPC framing
and HTTP execution stay in `packages/`.

Each chain-local RPC client owns request correlation over an injected generic
transport, while focused adapters own method families. Bitcoin exposes node,
fee, and transaction adapters from one shared client. Ethereum composes its
account and transaction adapters over one shared client. Endpoint ordering,
retryable failover, timeouts, authentication, and response bounds remain
transport concerns; neither chain exposes a service-locator connection object.

Bitcoin owns UTXOs, scripts, dust and fee rules, sighashes, witnesses,
consensus encoding, native SegWit v0 P2WPKH, and Taproot key-path signing.
Ethereum owns chain ID, nonce, EIP-1559 fees and envelopes, ERC-20 calldata,
receipt/log interpretation, and sender recovery. Neither chain is represented
by a universal transaction struct.

The chain-deletion invariant remains strict: deleting one concrete chain crate
must remove all of that chain's vocabulary without breaking base, wallets,
indexing, storage, HTTP, JSON-RPC, or the other concrete chain.

## Indexing consumer API

[`sdk/indexing`](../sdk/indexing/src/lib.rs) separates chain-neutral consumer
contracts from block synchronization and persistence contracts:

- `Watcher` registers and removes durable address or transaction watches;
- `Checkpoint` reads the current canonical block boundary for watch birthdays;
- `History` reads one transaction or an address transaction page;
- `Observer` reads durable revision events using a cursor;
- `Indexer` is only the marker `Checkpoint + Watcher + History + Observer`;
- `OutputQuery` separately reads snapshot-consistent spendable outputs.

Every request carries an exact `IndexScope` containing chain and network.
Watch creation requires a caller-owned idempotency key. `UnwatchRequest`
contains only scope and watch ID; the repository derives its deactivation
checkpoint. `ObservedTransaction` records chain facts, multiple independent
value movements, fee, status, and revision. This correctly represents UTXO
transactions without inventing one `from -> to -> amount` transfer.

`Composer::new().with(scope, indexer)?` routes calls to independently composed
children by exact scope. Duplicate registration is a conflict and never
replaces the existing child. The composer does not own workers, merge streams,
or allocate cursors.

[`sdk/indexing/http`](../sdk/indexing/http/src/lib.rs) implements `Checkpoint`,
`Watcher`, `History`, and `Observer` over the chain-neutral HTTP resources:

```text
/v1/scopes/{chain}/{network}/watches
/v1/scopes/{chain}/{network}/checkpoint
/v1/scopes/{chain}/{network}/transactions/{transaction}
/v1/scopes/{chain}/{network}/addresses/{address}/transactions
/v1/scopes/{chain}/{network}/events
/v1/scopes/{chain}/{network}/addresses/{address}/outputs
```

The remote accepts one or more endpoints and uses bounded responses, timeouts,
retry policy, and safe failover. It implements `OutputQuery` separately because
outputs are needed by UTXO wallet construction but are not part of the
observation-lifecycle marker.

## Index synchronization and persistence

`BlockSource` and `BlockInterpreter` are chain-facing synchronization
boundaries. A concrete interpreter turns a native block into
`ObservationDraft`, `IndexChanges`, and `IndexUndo`; it never creates storage
keys, record versions, event cursors, or repository revisions.

Persistence-facing traits in `sdk/indexing::store` describe semantic operations
for checkpoints, watches, observations, outputs, rebuilds, and reorgs.
[`sdk/indexing/rocksdb`](../sdk/indexing/rocksdb/src/lib.rs) implements those
traits and the consumer API over `packages/storage/rocksdb`. Its record keys and
encodings are private. A future PostgreSQL adapter can implement the same
semantic contracts without exposing raw key/value operations to indexers or
applications.

A canonical block commit atomically covers block effects, inverse undo,
checkpoint movement, observation revisions, event rows, and output changes.
Reorgs append revisions instead of deleting history. Mempool absence and RPC
failure are not proof that a transaction was dropped.

Ordinary indexing APIs expose no schema or policy migration command. The
current adapter opens the schema it supports and rejects incompatible data;
operators rebuild or restore explicitly outside the consumer contract.

[`apps/indexer`](../apps/indexer/src/lib.rs) is the concrete indexing
composition root. `EthereumService` and `BitcoinService` combine one chain
source, interpreter, RocksDB repository, worker, and generic router. The
`indexer-worker` binary delegates to the same runtime. Routes are parameterized
by chain and network; there are no separate Bitcoin and Ethereum routers.

## Payment orchestration

[`apps/api`](../apps/api/src/lib.rs) exposes durable payment orchestration as a
library and as a configured payment executable. `Payments` composes an exact wallet with
`Arc<dyn Indexer>` and persists this sequence:

```text
Requested -> Prepared -> Watched -> Submitted -> Confirmed
```

The exact `SignedTransaction` is stored before the external effect. A
transaction watch is registered and its receipt persisted before broadcast.

`payment_api::Service` is the reusable operational host boundary. It receives
already-composed `Payments` and supervises HTTP plus periodic event
reconciliation for every configured `IndexScope`; readiness remains
unavailable until all scopes complete a reconciliation pass and is cleared by
any later failure. `run_until` and `run_on` provide graceful shutdown for
embedding hosts and tests.

`payment_api::Runtime` is the concrete executable composition. It opens
RocksDB, connects the remote Indexer and chain RPC clients, and creates
Bitcoin/Ethereum wallets from keys named by environment variables. An optional
finite key-purpose map selects one Bitcoin-native, Ethereum-native, or ERC-20
deposit scope and composes its address/watch facade, observer, balance/history,
and collection execution over the same durable database. ERC-20 may resolve a
same-scope native gas wallet. The HTTP surface requires bearer authentication;
TLS and production custody are external.

The key-purpose map is allocation policy, not HTTP input. For a new deposit,
PS passes a server-owned operation ID and zero-based candidate to the one-method
`DepositAddressSource`. The configured resolver sorts purposes canonically and
returns a deterministic address, opaque key identity, and purpose. The durable
repository enforces one deposit per canonical address atomically; PS advances
past occupied candidates and reports finite-pool exhaustion. Once persisted,
retries reuse the exact record and perform only the watch handshake.
Retries reuse the same bytes and idempotency identity. `Payments::reconcile`
consumes revision events and atomically commits payment evidence with the
per-scope cursor. Sufficient confirmation/finality advances a payment;
reorg-corrected evidence can return it to `Submitted`. There is no synchronous
receipt polling loop.

[`sdk/deposits`](../sdk/deposits/src/lib.rs) owns reusable payment-domain
records for deposits, accounting, jobs, observation classification, and
collection workflows. It may consume indexing facts but indexing never imports
deposit meanings such as deposit, sweep, gas funding, or user credit.

[`apps/api::Sweeps`](../apps/api/src/collection.rs) executes durable account,
token-with-gas, and UTXO collections without concrete chain types.
`DepositWallets` resolves each durable
deposit to an already composed `Arc<dyn Wallet>`; private bytes and key
locators do not cross this boundary. `CollectionStore` combines the existing
collection and deposit repository capabilities. The one-method `GasWallet`
resolves the application-owned native funding wallet.

For a required leg, `Sweeps` supplies every participant's exact reserved
outputs to the collector, prepares once, and durably records the serialized
`SignedTransaction`, ID, factual fee allocation, and transition guard. It then
registers the transaction watch in IX before broadcasting. If the broadcast
response is lost, the leg remains `Signed`; retry deserializes and submits the
same envelope after repeating the same idempotent watch. It never prepares or
signs a replacement. After node acceptance, the store records `Broadcast` and
attaches the already acknowledged watch.

For token collection, the gas leg transfers its fixed `planned_amount` to the
deposit before the token leg can be prepared. `DepositObserver` alone advances
confirmation and reorg state and validates the observed native credit against
that planned amount. The confirmed allocation keeps token debit/master credit
separate from its native network fee.

The fee is allocated proportionally to each participant's gross reserved
value. Integer remainders are assigned largest-first with canonical deposit ID
as the tie-breaker. Each durable allocation records gross deposit debit, net
master credit, fee asset, and allocated fee; checked arithmetic rejects zero,
overflow, or a fee consuming the complete batch.

The present `apps/api` exports both a supervised service library and a payment
executable. The executable composes Bitcoin/Ethereum wallets, remote indexing,
RocksDB, bearer-authenticated HTTP, reconciliation, and one optional finite
deposit scope. `POST /v1/collections` accepts stable collection/job/deposit IDs
and a timestamp; the planner loads ledger state and canonical outputs and uses
configured policy and destination rather than trusting caller-supplied money or
chain evidence. UTXO pages must share one checkpointed snapshot; eligibility,
confirmation/maturity, input limits, ledger heads, and resource uniqueness are
validated before atomic reservation. Identical requests replay the durable
collection while a changed request with the same identity conflicts. It
supports no second simultaneous deposit scope and is not a complete
exchange/payment-gateway deployment.

The Ethereum system acceptance test composes a concrete Ethereum wallet and
signer, mock JSON-RPC node, Indexer HTTP service, Payment HTTP service, and
separate real temporary RocksDB databases. It proves broadcast, confirmation,
restart recovery, and canonical reorg correction across those checked-in
boundaries; it is not live-network or production-custody evidence.

## Generic infrastructure

- [`packages/crypto`](../packages/crypto/src/lib.rs) owns reusable key and
  signature mechanisms without chain or wallet policy.
- [`packages/http`](../packages/http/src/lib.rs) owns small generic client and
  server extensions.
- [`packages/json-rpc`](../packages/json-rpc/src/lib.rs) owns JSON-RPC framing,
  not chain methods.
- [`packages/storage`](../packages/storage/src/lib.rs) owns backend-neutral
  atomic key/value mechanics; its RocksDB child supplies the concrete engine.

No `packages/*` crate may import `sdk/*` or `apps/*`.
