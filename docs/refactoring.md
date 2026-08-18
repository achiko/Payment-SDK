# Payment SDK refactoring contract

## Status

This document records the approved architecture and the requirements that drove
the refactor. The current public contract is summarized in `docs/CONTRACTS.md`;
code and tests remain the evidence for implemented behavior.

The refactor is intentionally destructive. Existing source and persistent indexing data do not
require compatibility adapters unless this document explicitly says otherwise. Public types added
to `sdk/chains/base` require explicit approval; concrete-chain details must not be moved there merely
to make a shared interface compile.

## Observed starting point

The following inventory was taken from the current workspace before continuing the implementation.
It records migration inputs, not approved architecture:

- The removed `sdk/transactions/*` crates and the former combined broadcaster
  were migration inputs, not retained architecture. Shared transaction
  contracts now live in `sdk/chains/base`, while model-specific construction
  remains in each chain.
- `CoreClient` combines readiness, fee estimation, block reads, preflight, submission, and
  receipts in one large RPC object.
- Ethereum account, transaction, and block adapters now share one low-level client while retaining
  separate chain-owned method surfaces. Request IDs, framing, response correlation, and batch
  ordering are implemented once; applications inject the focused adapters into wallets and IX.
- Bitcoin indexing reaches through the large Core client rather than receiving a focused block RPC
  service.
- Bitcoin and Ethereum emit the shared `IndexChanges` and `IndexUndo` contracts and expose no
  persistence format. `sdk/indexing/rocksdb` alone translates those chain-neutral values into its
  physical records. `apps/indexer` only initializes the selected repository and workers.
- `WorkerObserver` is block/reorg worker instrumentation; it is not a transaction-delivery
  abstraction.
- Several indexer and address modules still import removed identifier aliases from base, showing the
  workspace is between architectures and cannot be treated as a clean compiling baseline.
- Canonical documents still describe removed signing, custody, chain-contract, transaction, and
  telemetry packages. They must be updated with the implementation.
- The worktree contains extensive in-progress user changes and deletions. Migration edits must
  preserve unrelated work and must not restore removed source from `old/` or `reference/`.

## Goals

The SDK must let an application composition root select concrete chains, RPC nodes, storage, and
keys once. Code below that composition root must then be able to:

1. construct a wallet from a stored asset key and private key;
2. read its address, balance, and transaction history;
3. prepare, persist, and broadcast a transfer without importing a concrete chain, then observe it
   through indexing;
4. index Bitcoin and Ethereum through one indexing contract;
5. compose several indexers without introducing cross-chain ordering claims;
6. support both account and UTXO transaction models without pretending their native transactions
   are the same; and
7. delete one concrete chain crate without leaving its vocabulary or protocol types elsewhere.

The SDK is an abstraction over common workflows, not a universal blockchain model. Bitcoin keeps
outpoints, scripts, witnesses, fee weight, and UTXO selection. Ethereum keeps nonce, gas, EIP-1559,
contract calldata, logs, and receipts. Shared contracts describe lifecycle and capabilities only.

## Non-goals

- Hardware wallets, remote signing, signer capability discovery, and user-interaction protocols.
- Metrics and telemetry.
- A universal native transaction representation.
- A cross-chain RPC trait.
- A global total order across chains.
- A generic explorer that indexes every address by default.
- Hiding concrete chain selection from the application composition root.
- Compatibility aliases for removed packages and old prefixed internal names.
- In-place migration of the current indexing record format.

## Dependency and ownership rules

The dependency direction is:

```text
packages/*
    ^
sdk/chains/base, sdk/indexing
    ^
sdk/chains/bitcoin, sdk/chains/ethereum
    ^
sdk/wallets
    ^
apps/*
```

The arrow points from a dependency toward its consumer. Cargo dependencies point in the opposite
visual direction: a concrete chain depends on base and indexing, never the reverse.

### Packages

`packages/*` contains only infrastructure reusable outside this repository.

- A package may depend on another package or an external library.
- A package must not depend on `sdk/*` or `apps/*`.
- `packages/crypto` owns generic cryptographic values and operations. It must not contain chain
  names, addresses, wallets, assets, RPC methods, transaction formats, or custody policy.
- `packages/http` owns low-level HTTP transport and generally useful extensions such as bounded
  bodies, timeouts, headers, and retry mechanics. It must not know blockchain method semantics.
- `packages/json-rpc` owns JSON-RPC framing and transport-independent request/response behavior. It
  must not contain Bitcoin or Ethereum methods.
- `packages/storage` owns backend-independent atomic storage mechanics.
- `packages/telemetry` and `packages/transport` are removed.

### SDK

- `sdk/chains/base` owns only approved protocol-neutral primitives and lifecycle contracts.
- `sdk/chains/bitcoin` owns every Bitcoin-specific type and rule.
- `sdk/chains/ethereum` owns every Ethereum-specific type and rule.
- `sdk/indexing` owns normalized indexing, checkpoint, watch, revision, stream, and repository
  contracts. It has no storage dependency. Base does not contain indexing.
- `sdk/indexing/rocksdb` owns the current durable record layout and implements those repository
  contracts over transactional RocksDB-backed storage. A future PostgreSQL adapter implements the
  same contracts directly and does not emulate the key/value layout.
- `sdk/wallets` is an explicit composition package. It may import concrete chain crates because its
  purpose is runtime wallet selection.
- `sdk/deposits` owns deposit, accounting, collection workflow, and other payment-service meanings.
- `sdk/chains/base::Decimal` is the only generic monetary value. Indexing and deposits use it
  directly; concrete chains convert it to native integer units after validating sign, precision,
  and range.
- `sdk/transactions`, `sdk/signing`, and `sdk/storage` are removed. Their valid responsibilities
  belong respectively in chain base, chain implementations, and `packages/storage`.
- Sibling SDK domains should remain independent. A concrete chain may depend on base, indexing,
  packages, and a narrowly required SDK contract. Indexing must never import a concrete chain.

### Applications

- An application may depend on SDK packages, infrastructure packages, and external libraries.
- Applications are composition roots and may import multiple concrete chains.
- Applications must not depend on one another.
- Chain selection, RPC endpoints, persistent backends, keys, and worker configuration happen here.

### Chain deletion invariant

Deleting `sdk/chains/bitcoin` must remove every Bitcoin type, RPC method, address parser, transaction
format, block parser, and interpreter. Generic UTXO effects, indexing, repository adapters,
packages, Ethereum, and generic transaction lifecycle contracts must remain valid. The same rule
applies to every concrete chain.

## Naming rules

- Names should normally contain one or two semantic words.
- Do not add ceremonial qualifiers such as `Business`, `Production`, `Compatibility`, or
  `Registration` when the type has a precise role without them.
- The durable transaction-feed trait is `Observer`, not `BusinessObserver` or
  `TransactionObserver`. Worker commit/reorg instrumentation is separately named
  `WorkerObserver`.
- The wallet composition type is `Wallets`, not `Registry`.
- A chain factory is `bitcoin::WalletProvider`, not `BitcoinRegistration`.
- Inside a concrete package, names do not repeat that package: use `bitcoin::Address`, not
  `bitcoin::BitcoinAddress`. A compatibility re-export is not retained during this migration.
- Chain vocabulary such as `bitcoin`, `btc`, `ethereum`, and `eth` is forbidden outside `apps/*`
  and the owning concrete chain directory.
- A method returning `Self` or `Result<Self, _>` without consuming instance state is an associated
  constructor, not an instance method.
- Every trait declares at most three functions, including provided functions.

## Base API

`sdk/chains/base` contains the following approved modules:

```text
address
asset
block
chain
decimal
derivation
error
key_pair
network
signer
transaction
```

`BlockHeight`, `BlockHash`, and `BlockRef` are protocol-neutral canonical-chain
references. Indexing re-exports them for source compatibility, but `base` owns
their definitions; concrete RPC adapters validate protocol-specific hash sizes
and byte order.

New modules or public types require review. Base must not acquire chain RPC, indexing persistence,
wallet factories, generated-address wrappers, hardware-wallet capabilities, or business workflow
state.

### Address

`base::Address` is opaque address bytes. It does not carry a textual `0x` prefix or validate a
concrete chain encoding.

```rust
pub struct Address(Vec<u8>);

pub trait Addresser {
    fn address(&self) -> Address;
}

pub trait AddressValidator {
    fn validate(&self) -> Result<(), AddressError>;
}
```

Concrete address types wrap the base address and own parsing, textual formatting, checksums, and
network validation:

```rust
pub struct bitcoin::Address(base::Address);
pub struct ethereum::Address(base::Address);
```

Both concrete address types implement `FromStr`, `Display`, `Addresser`, and `AddressValidator`.
Functions that only need identity accept `&dyn Addresser` or an appropriate generic bound rather
than importing a concrete address.

Address validation errors live in `address.rs`, not in a detached general error module.

### Networks and chains

Network identifier storage is generic; chain identity is not encoded as a generic marker parameter.

```rust
pub enum NetworkKind {
    Mainnet,
    Testnet,
}

pub struct NetworkId<R = &'static str> {
    value: R,
    kind: NetworkKind,
}
```

The storage value may be a string, integer, fixed byte array, or another serializable identifier.
Do not assume every known chain uses an integer network ID.

```rust
pub struct Chain<R = &'static str> {
    pub network_id: NetworkId<R>,
    pub name: &'static str,
    pub ticker: &'static str,
}
```

Each concrete chain exports its networks as `CHAINS`. The collection exposes direct `mainnet` and
named testnet access; it does not add speculative iterator search helpers.

### Assets

An asset exposes its chain, network ID, name, and ticker through a small capability. Both `Chain`
and `Asset` implement it. Contract assets are intentionally deferred until their requirements are
approved.

### Decimal

`Decimal` is an arbitrary-precision, base-10 value represented by an integer coefficient and scale.
It never uses floating point.

```text
human value = coefficient × 10^-scale
```

The business API always uses human asset units:

```rust
Decimal::from_str("1.25")
```

Each concrete asset owns the conversion between human decimal values and atomic chain units:

- Bitcoin converts BTC to satoshis using 8 decimal places.
- Ethereum converts ETH to wei using 18 decimal places.
- Token contracts provide their own decimal precision later.

Conversions reject negative values where invalid, excess fractional precision, overflow, and
non-integral atomic results. Native transaction code uses integer atomic units after conversion.

### Cryptography and signing

Derivation paths, key pairs, signatures, signing payloads, and the minimal signer contract live in
base because they are common to concrete chains.

```rust
pub struct DerivationPath(pub Vec<ChildIndex>);

pub struct ChildIndex {
    pub index: u32,
    pub hardened: bool,
}

pub trait Signer: Send + Sync {
    fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a>;
}
```

`KeyPair` is in its own file and is the only local signer for now. It must not implement secret-
revealing `Debug`, log its private key, or serialize private key material. Hardware-wallet status,
capabilities, and user interaction do not belong in base.

## Transaction abstraction

There is no separate `sdk/transactions` domain. Shared transaction lifecycle contracts and the
narrow UTXO capability live under `base::transaction`. Chain-native construction and signing remain
in each concrete chain.

### Common lifecycle

```text
TransactionBuilder
    -> SignedTransaction
    -> Broadcaster
    -> Submission
    -> Observer / transaction history
```

The lifecycle separates pure/persistable preparation from the external broadcast effect.

```rust
pub trait TransactionBuilder: BuilderCast + Send {
    fn transfer(
        &mut self,
        destination: Address,
        amount: Decimal,
    ) -> Result<(), TransactionError>;

    fn snapshot(&self) -> Result<TransactionSnapshot, TransactionError>;

    fn prepare<'a>(
        &'a mut self,
    ) -> TransactionFuture<'a, Result<SignedTransaction, TransactionError>>;
}
```

Calling `transfer` repeatedly expresses several outputs. A Bitcoin implementation may create one
native transaction with all outputs. An implementation that cannot represent the requested shape
returns a structured `Unsupported` error; it must not silently emit several native transactions.

`snapshot()` returns versioned JSON describing the configured builder for auditing and application
workflows. Durable retry does not restore this snapshot: it stores the exact `SignedTransaction`
value before the external broadcast effect.

```rust
pub struct SignedTransaction {
    version: u16,
    kind: String,
    id: TransactionId,
    envelope: TransactionEnvelope,
}

pub trait Broadcaster: Send + Sync {
    fn broadcast<'a>(&'a self, transaction: &'a SignedTransaction)
        -> TransactionFuture<'a, Result<Submission, TransactionError>>;
}
```

`prepare()` performs required reads, builds the native transaction, computes its signing payload,
invokes the injected signer, validates/inserts the signature, computes the native transaction ID,
and returns exact signed bytes. It never broadcasts.

The application serializes and persists the complete `SignedTransaction` before calling
`broadcast()`: version, kind discriminator, canonical textual ID, and exact opaque envelope.
Broadcast validates and submits that value without rebuilding or signing, then returns
`Submission { id }`. Confirmation is an indexer observation, not a wallet RPC loop. Recovery
deserializes and rebroadcasts the persisted value unchanged.

### UTXO extension

Ordinary code uses `TransactionBuilder`. Code that requires UTXO-specific controls asks for a typed
capability without downcasting to Bitcoin:

```rust
pub trait BuilderCast {
    fn utxo(&mut self) -> Option<&mut dyn UtxoBuilder>;
}

pub trait UtxoBuilder {
    fn inputs(&mut self, policy: InputPolicy) -> Result<(), TransactionError>;
    fn change(&mut self, address: Address) -> Result<(), TransactionError>;
}

pub enum InputPolicy {
    Automatic,
    SpendAll,
}
```

Bitcoin implements the capability. Ethereum returns `None`. Do not create a marker or empty account
builder. A future account capability requires a demonstrated shared operation.

Bitcoin collection example:

```rust
let mut transaction = wallet.transaction();
transaction.transfer(master, amount)?;

transaction
    .utxo()
    .ok_or(TransactionError::Unsupported)?
    .inputs(InputPolicy::SpendAll)?;

let prepared = transaction.prepare().await?;
database.save(&prepared).await?;
let submission = wallet.broadcaster().broadcast(&prepared).await?;
let watch = WatchRequest {
    scope,
    selector: WatchSelector::Transaction(TransactionRef {
        chain,
        value: submission.id.to_string(),
    }),
    start_height,
    idempotency_key,
};
watcher.watch(watch).await?;
```

Coin selection, outpoints, satisfaction weight, scripts, dust, fee rate, change calculation, RBF,
sighash, witness construction, and serialization remain Bitcoin-owned. Advanced exact coin control
may use the concrete Bitcoin builder.

## RPC abstraction

There is no cross-chain RPC trait and no connection object acting as a service locator. Each chain
has one cloneable `rpc::Client` responsible for node transport coordination. Focused structs accept
the client explicitly and own method semantics.

### Construction

```rust
let client = bitcoin::rpc::Client::from_urls(
    [first_node, second_node, third_node],
    config,
).await?;

let blocks = bitcoin::rpc::Blocks::new(client.clone());
let utxos = bitcoin::rpc::Utxos::new(client.clone());
let transactions = bitcoin::rpc::Transactions::new(client.clone());
```

Supported client constructors are:

```rust
Client::from_rpc(existing_json_rpc)
Client::from_url(url, config)
Client::from_urls(urls, config)
```

`from_rpc` is the injection point for deterministic tests, vendor clients, proxies, or callers that
already own transport. `from_urls` creates a node pool.

### Client responsibilities

The client owns:

- endpoints and authentication;
- request identifiers;
- request timeouts and response-size bounds;
- health state and cooldown;
- concurrency, hedging, and node selection;
- sanitized transport errors; and
- the configured read quorum and broadcast policy.

It does not own Bitcoin or Ethereum method names, DTO conversion, fee policy, block parsing,
transaction validation, or indexing semantics.

Every node is verified before becoming eligible. Bitcoin validates expected network and genesis
hash. Ethereum validates expected chain ID. A wrong-chain node is permanently excluded. Temporary
transport failure triggers bounded cooldown. A lagging node is not declared corrupt merely because
its tip is behind.

### Focused RPC services

Bitcoin provides:

- `Blocks`: tip, canonical hash, block retrieval, and index-source behavior;
- `Utxos`: outpoint and address UTXO reads; and
- `Transactions`: fee estimates, preflight, submission, and receipts.

Ethereum provides:

- `Blocks`: canonical block, receipt/log, and index-source reads;
- `Accounts`: native/token balance and nonce; and
- `Transactions`: gas/fee context, submission, and receipts.

No single public RPC struct combines every method. Raw JSON requests are private to the relevant
service.

### Multi-node reads and indexing

Ordinary wallet reads use first-valid response with bounded hedging. Receipt lookup may query more
than one healthy node until it obtains a definitive matching receipt.

Indexing is stricter. For the next height, `Blocks` requests the canonical hash from eligible nodes
and advances only when the configured quorum agrees. It fetches the full block from an agreeing node
and verifies the returned block hash before interpretation.

The default quorum is a strict majority:

```text
floor(configured_nodes / 2) + 1
```

Three production nodes with quorum two tolerate one unavailable or divergent node. Without quorum,
the indexer returns a retryable `Unavailable` or `Divergent` source error and does not advance its
checkpoint or emit a batch.

### Multi-node broadcast

The chain implementation computes the transaction ID locally before RPC submission. With
`BroadcastPolicy::All`, it sends the identical signed envelope concurrently to every healthy node.

```rust
pub enum BroadcastPolicy {
    One,
    All,
}
```

Classification rules are:

- any matching acceptance means `Submitted`;
- an exact already-known response counts as acceptance only after matching the locally computed ID;
- all definitive rejections mean `Rejected`;
- no acceptance plus any ambiguous transport result means `Unknown`;
- every result retains the locally computed transaction ID.

`All` is the selected production default for availability. Bitcoin operators must understand that
multi-node rebroadcast can reveal the transaction source to more nodes. `One` remains available for
privacy-sensitive deployments.

An `Unknown` result is not a rejection. The caller reconciles through transaction lookup, receipt,
mempool evidence, or explicit rebroadcast of the persisted bytes.

## Wallet abstraction and composition

`sdk/wallets` is the protocol-neutral runtime composition package. `Wallets` stores explicitly
registered providers but does not import concrete chain crates. Each concrete chain implements the
wallet and provider contracts; an application registers those providers. There is no global mutable
registry or automatic plugin discovery.

### Startup

```rust
enum WalletKey { Bitcoin, Ethereum }
let mut wallets = Wallets::new();
wallets.register(WalletKey::Bitcoin, bitcoin_provider)?;
wallets.register(WalletKey::Ethereum, ethereum_provider)?;

let wallet = wallets
    .new_wallet(&WalletKey::Bitcoin, SecretBytes::new(record.private_key))
    .await?;

if wallet.address() != record.address {
    return Err(WalletError::AddressMismatch);
}
```

Concrete selection is allowed here and in application startup. Later business logic receives only
`Arc<dyn Wallet>`.

### Provider

```rust
pub trait Provider: Send + Sync {
    fn create<'a>(
        &'a self,
        secret: SecretBytes,
    ) -> WalletFuture<'a, Result<Arc<dyn Wallet>, WalletError>>;
}
```

`Wallets<K>` rejects an unregistered key and rejects duplicate registration at startup. The SDK
does not define the key: each application uses a finite enum whose variants identify its configured
providers. Provider configuration remains authoritative for protocol, network, and asset values.

### Wallet

```rust
pub trait BalanceReader: Send + Sync {
    fn balance<'a>(&'a self) -> WalletFuture<'a, Result<WalletBalance, WalletError>>;
}
pub trait TransactionFactory: Send + Sync {
    fn transaction(&self) -> Box<dyn TransactionBuilder>;
    fn broadcaster(&self) -> &dyn Broadcaster;
}
pub trait TransactionRestore: Send + Sync {
    fn restore(&self, snapshot: &TransactionSnapshot) -> Result<Box<dyn TransactionBuilder>, TransactionError>;
}
pub trait HistoryReader: Send + Sync {
    fn history<'a>(&'a self, request: HistoryRequest) -> WalletFuture<'a, Result<TransactionHistory, WalletError>>;
}
pub trait Wallet: Addresser + BalanceReader + TransactionFactory + TransactionRestore + HistoryReader + Send + Sync {}
```

The wallet holds the signer/key material and focused RPC/indexing dependencies needed by that
concrete implementation. It does not own durable workflow state.

`WalletBalance` uses human `Decimal` and may include the canonical block at which the balance was
observed. Bitcoin balance is derived from canonical indexed UTXOs. Ethereum balance comes from its
account RPC service. History comes from indexing, not ad hoc wallet-owned persistence.

Secrets are redacted, are zeroized on drop where possible, and never implement revealing `Debug`,
`Display`, or serialization.

## Indexing abstraction

Indexing is watched-only. It emits normalized chain facts, never deposit, sweep, user-credit, or
other payment meanings.

### Public interface

```rust
pub trait Checkpoint: Send + Sync {
    fn checkpoint<'a>(&'a self, scope: &'a IndexScope)
        -> IndexFuture<'a, Result<Option<BlockRef>, IndexError>>;
}
pub trait Watcher: Send + Sync {
    fn watch<'a>(&'a self, request: WatchRequest) -> IndexFuture<'a, Result<WatchReceipt, IndexError>>;
    fn unwatch<'a>(&'a self, request: UnwatchRequest) -> IndexFuture<'a, Result<UnwatchOutcome, IndexError>>;
}
pub trait History: Send + Sync {
    fn transaction<'a>(&'a self, request: TransactionQuery)
        -> IndexFuture<'a, Result<Option<ObservedTransaction>, IndexError>>;
    fn history<'a>(&'a self, request: HistoryQuery)
        -> IndexFuture<'a, Result<TransactionPage, IndexError>>;
}
pub trait Observer: Send + Sync {
    fn events<'a>(&'a self, request: EventQuery)
        -> IndexFuture<'a, Result<EventPage, IndexError>>;
}
pub trait Indexer: Checkpoint + Watcher + History + Observer {}
```

`Indexer` is only the blanket marker for values implementing the four focused consumer traits. It
has no `index()` synchronization method. `indexing_rocksdb::Repository` directly implements
`Checkpoint`, `Watcher`, `History`, and `Observer`; chain sources and the sync worker write into the repository
through separate internal repository traits. `Observer::events` returns one durable cursor page per
call. Polling, cursor persistence, and retry timing belong to the consuming application.

### Composer

`Composer` is only an exact-scope router. It is constructed with `Composer::new().with(scope,
indexer)?` and delegates `Checkpoint`, `Watcher`, `History`, and `Observer` calls to that registered child.
Registering an occupied scope returns `IndexErrorKind::Conflict`; it never silently replaces a
child.

- Ordering is preserved within each chain/network scope.
- No global ordering is claimed between chains.
- An error identifies its scope.
- It does not create workers, follow chains, merge event streams, or own cursors.

The runnable chain-specific composition is `EthereumService::new(EthereumConfig)` or
`BitcoinService::new(BitcoinConfig)`, followed by `run()` or `run_until(shutdown)`. These facades
own one configured source, worker, RocksDB repository, and HTTP server.
`indexing-http::Remote::connect(config)` provides the external adapter and may
be stored directly as `Arc<dyn Indexer>`. `Config` accepts one or more base
URLs, an optional bearer token, request timeout, bounded response size, and a
generic HTTP retry policy. The adapter contains no concrete-chain parsing and
validates that every response belongs to the requested scope.
`Remote` also implements the separate `OutputQuery` capability through
`/v1/scopes/{chain}/{network}/addresses/{address}/outputs`; output amount,
asset, evidence, and snapshot fields remain chain-neutral on the wire.
The in-process Payment Service does implement the durable consumer side: each
wallet registration includes an `IndexScope`, and `Payments` receives an
`Arc<dyn Indexer>`. It persists the signed envelope, registers the transaction
watch, persists the receipt, broadcasts, and later reconciles cursor pages
without waiting inside the request.

### Persistence ownership

`sdk/indexing` owns the typed commands and small repository traits for watches, canonical blocks,
transactions, events, status, backfills, and rebuilds. It neither stores nor encodes records.
`sdk/indexing/rocksdb` owns versioned watch, rollback, observation, event, checkpoint, and cursor
records for the RocksDB adapter. Concrete chains own block parsing, protocol validation, and
conversion into typed indexing effects. They never construct storage keys, byte prefixes, record
headers, schema versions, or encoded values.

One adapter may share an internal transactional engine because a canonical block commit must update
raw block data, undo, observations, revisions, events, typed effects, confirmations, and checkpoint
atomically. That internal atomicity does not collapse the public API: workers and read services use
the narrow repository capabilities they need. PostgreSQL may map those capabilities to normalized
tables and native transactions without importing RocksDB keys or record DTOs.

`InterpretedBlock` carries a chain-owned semantic effect rather than byte-key mutations. Bitcoin's
first effect is its typed UTXO transition:

```rust
pub struct OutputChanges {
    pub created: Vec<IndexedOutput>,
    pub spent: Vec<OutputKey>,
    pub tracked_spends: Vec<OutputKey>,
}
```

`spent` contains inputs selected by an active watch. `tracked_spends` contains inputs
discovered while indexing another watch and is applied only when the repository already contains
the matching creation fact. These are chain-neutral output-index semantics, not RocksDB operations.
The opaque `evidence` bytes retain whatever a concrete transaction builder needs without teaching
indexing about scripts or another chain's native transaction model.

The corresponding read boundary is `indexing::OutputQuery`, expressed as an
`OutputRequest`, `OutputPage`, and snapshot-bound `OutputCursor`. The cursor's
position is opaque. `indexing-rocksdb::OutputReader` is one implementation;
another database can implement the same contract without reproducing RocksDB
keys. Bitcoin owns `IndexUtxos`, which validates and converts these facts into
the chain-native `UtxoSet` consumed by its wallet builder.

The RocksDB adapter projects these values into private versioned key/value records. A future
PostgreSQL adapter can implement the same indexing repository capabilities using rows and
constraints without importing the RocksDB representation. Applications query typed outputs and do
not access projection keys or values.

The chain interpreter supplies shared forward effects and semantic undo identities. The selected
repository converts both into private mutations inside its atomic block or reorg commit. Chain code
never provides storage rollback mutations.

Current indexing data is rebuilt after the schema change. There is no compatibility decoder.
Indexing exposes no semantic-policy migration command. Policy or physical-schema incompatibility
selects the offline rebuild path; adapter-private record versions do not leak into SDK contracts.

The consumer `Watcher` keeps `watch` and `unwatch` together. Its removal request
contains only the scope and watch ID. The persistence `WatchStore` keeps
`register_watch` and `deactivate` together; its `DeactivateWatch` command adds
the exact checkpoint and inactive height required for an atomic storage fence.
Both are deliberately cohesive two-method traits: callers
that control a watch lifecycle need both operations, while history and event readers remain
independent.

### Observer

`Observer` is the transaction-confirmation boundary. A wallet broadcasts exact signed bytes and
returns submission evidence; it does not poll receipts or await confirmations. Applications persist
the prepared transaction and register its watch before broadcasting, then use the indexer-backed
observer for inclusion, confirmation, replacement, failure, and reorg revisions.

```rust
pub trait Observer: Send + Sync {
    fn events<'a>(
        &'a self,
        request: EventQuery,
    ) -> BoxFuture<'a, Result<EventPage, IndexError>>;
}
```

The request identifies one scope and resumes after an event cursor. Pages contain absolute
transaction revisions, including reorg corrections; callers persist the cursor alongside their
own state. Polling, push notification, or a stream adapter may drive the same durable feed. A
lightweight embedded indexer and the durable Indexer Service
implement the same one-method contract.

## Concrete-chain requirements

### Bitcoin

Bitcoin owns:

- network-aware address parsing and formatting;
- satoshi conversion;
- outpoints, inputs, outputs, transaction IDs, unsigned/signed transactions, and receipts;
- automatic and spend-all selection;
- multiple inputs and outputs;
- fee rate, fee/weight calculation, dust, change, and checked arithmetic;
- P2WPKH and Taproot key-path ownership verification and signing;
- consensus serialization and witness assembly;
- Core RPC DTOs and response interpretation;
- block parsing, previous-output resolution, and typed UTXO effects.

Remove `BitcoinInputSigningContext` and vague transaction codecs. Signing should be expressed as
private, named stages: construct, validate ownership, compute sighash, sign, assemble witness, and
validate final transaction.

### Ethereum

Ethereum owns:

- address parsing, checksum formatting, and 20-byte validation;
- wei conversion;
- native/token transfer requests;
- chain ID, nonce, gas, EIP-1559 fees and envelopes;
- contract calldata;
- signature recovery and sender validation;
- transaction IDs, receipts, logs, and block interpretation;
- Ethereum RPC DTOs and response interpretation.

Remove vague transaction codecs and duplicated raw RPC stacks between wallet and index source. Both
focused services receive the same cloneable client.

## Error contracts

Errors have a structured kind and sanitized contextual message. Chain/provider messages, URLs,
headers, keys, signed envelopes, and credentials must not escape in `Display` or `Debug`.

The common lifecycle distinguishes at least:

- invalid address/network/value;
- unsupported capability or transaction shape;
- insufficient funds, dust, overflow, and fee-policy violation;
- invalid signature or signer mismatch;
- invalid persisted snapshot/envelope;
- RPC unavailable/divergent;
- transaction rejected;
- submission unknown; and
- timeout/finality failure.

Retryability is explicit. RPC outage or missing evidence is not proof that a transaction was
rejected, dropped, or absent from the chain.

## Linter requirements

The repository design linter must enforce:

1. `apps/*` may depend on SDK/packages/external libraries but not another app.
2. `packages/*` may depend only on packages and external libraries.
3. Generic SDK crates may not depend on concrete chain crates.
4. Concrete chains may depend toward base/indexing/packages, never the reverse.
5. Chain vocabulary is restricted to apps and its concrete chain directory.
6. Traits contain no more than three methods.
7. In-package type names do not repeat package/module names.
8. Constructors returning `Self` are associated functions.
9. Oversized files, god objects, duplicate models, catch-all modules, and ceremonial wrappers are
   diagnosed.

Suppression uses a reasoned source comment:

```rust
// design-lint: allow rule-id -- concrete reason this boundary is exceptional
```

Blanket, unreasoned, or configuration-wide suppression is not acceptable. Diagnostic mode prints
active errors. Cases mode writes persistent examples under `lint/errors` and passing review cases
under `lint/check`; ordinary diagnostic mode does not update those directories.

## Migration order

1. Freeze this document as the approved target.
2. Remove stale workspace members and update canonical architecture documents.
3. Move transaction contracts into base and migrate both chains.
4. Introduce the injectable multi-node RPC client and focused chain RPC services.
5. Remove duplicate chain RPC traits, codecs, and context objects.
6. Implement the wallet abstraction and `sdk/wallets` composition package.
7. Implement the indexing stream and composer over the existing durable repository semantics.
8. Replace opaque projections with typed effects and move all encoding into repository adapters.
9. Migrate applications to the new abstractions.
10. Remove every compatibility alias and dead source file.
11. Rebuild index data and run full validation.

Each stage must keep secrets redacted and must not weaken chain-native transaction validation merely
to satisfy a generic interface.

## Acceptance criteria

The refactor is complete when:

- a composition root can construct Bitcoin and Ethereum providers from one or several RPC nodes;
- later code sends funds using only `dyn Wallet` and the base transaction lifecycle;
- Bitcoin collection spends several inputs into one output through the typed UTXO capability;
- builders and prepared transactions round-trip through versioned JSON;
- exact signed bytes are durably available before broadcast;
- multi-node broadcasts classify accepted, duplicate, rejected, and unknown outcomes correctly;
- indexing refuses to advance without its configured canonical quorum;
- Bitcoin and Ethereum indexers satisfy the same `Indexer` contract tests;
- `Composer` preserves per-scope ordering and at-least-once cursor semantics;
- chain crates and applications never construct or decode persistence keys or values;
- deleting either concrete chain leaves generic packages and SDK abstractions buildable;
- the design linter reports no unsuppressed architecture errors;
- formatting, workspace check, tests, Clippy, and documentation pass with locked dependencies; and
- no telemetry, remote signer, hardware-wallet capability, custody application, old signing domain,
  or transaction subpackage is reintroduced.

## Required tests

### Base and transactions

- Decimal parsing, precision conversion, overflow, and invalid fractional atomic values.
- Builder snapshot version and JSON round-trip.
- Restore rejects another wallet, chain, network, or malformed envelope.
- Preparation does not broadcast.
- UTXO capability is present for Bitcoin and absent for Ethereum.

### Bitcoin

- Wrong-network and malformed addresses.
- Multiple inputs, multiple outputs, spend-all, change, dust, and insufficient funds.
- Duplicate inputs and checked fee arithmetic.
- P2WPKH/Taproot ownership and signature failures.
- Exact transaction ID and byte preservation.

### Ethereum

- Wrong chain ID, malformed address, nonce, gas, fee ceiling, and insufficient funds.
- Native and token transfers.
- Signature recovery and sender mismatch.
- Exact envelope and transaction ID preservation.

### RPC

- Injected single client and URL-created pools.
- Wrong-chain node exclusion.
- Temporary failure, cooldown, and recovery.
- Hedged reads and quorum agreement.
- Lagging and divergent nodes.
- Broadcast fanout with mixed success, duplicate, rejection, and timeout results.
- Secret/endpoint redaction.

### Wallets

- Missing provider keys and duplicate startup registration.
- Address/private-key mismatch.
- Secret redaction and zeroization behavior.
- Bitcoin and Ethereum balance/history/transaction use entirely through `dyn Wallet`.

### Indexing

- Watch idempotency and scope routing.
- Commit-before-yield.
- Cursor replay, restart, and at-least-once delivery.
- Reorg revisions and inverse projection correctness.
- Per-scope ordering and absence of cross-chain ordering assumptions.
- Slow-consumer backpressure and independent child failure.

## Validation commands

```bash
cargo fmt --all -- --check
git diff --check
cargo run --locked -p design-lint -- --policy lint.toml .
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked --workspace --no-deps
```

Network-backed examples, real transaction broadcast, and funded keys are excluded from routine
validation.
