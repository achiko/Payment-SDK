# Feature extraction and validation

## Verdict

The repository contains concrete Bitcoin and Ethereum wallets, durable chain
indexers, a chain-neutral wallet composition layer, an indexer HTTP client, and
durable payment orchestration. The runnable indexer composes chain-native block
interpretation with a generic RocksDB repository and one parameterized HTTP
router. The wallet and payment applications also provide runnable binaries;
the payment binary supports outgoing Bitcoin and Ethereum payments and at most
one configured finite deposit scope. There is no repository-owned production
custody process.

Deterministic coverage exercises transaction construction/signing, indexing,
HTTP contracts, RocksDB restart, watch-before-broadcast ordering, exact-envelope
retry, confirmation, and reorg correction. This evidence does not prove live
node compatibility, production custody, public-network behavior, HA, or a
complete exchange/payment-gateway deployment.

The original document is accepted with five corrections required for safety:

1. four mutable counters cannot show included-but-not-deep value; the ledger
   needs an explicit `confirmed` balance and immutable absolute snapshot rows;
2. a confirmed amount cannot also be permanently monotonic when reorgs are
   supported;
3. collection readiness cannot subtract `accounted`, because a user credit
   does not spend on-chain funds;
4. `collected` must mean the gross deposit debit if the proposed balance
   invariant is retained; and
5. a stateless WS can own each chain operation, but PS must persist and resume
   a multi-transaction ERC-20 workflow.

These are accounting corrections, not changes to the PS/WS/IX boundary.

## Extracted capabilities

### 1. Deposit address generation

- one asset-independent PS orchestration flow;
- one chain-specific address derivation implementation per asset/network;
- chain-independent key provisioning and signing selected by an application;
- persist the deposit before returning it;
- register the address with IX from the deposit birthday height;
- idempotency across retries;
- recover the failure window between PS persistence and IX registration;
- return the address only after IX has acknowledged a durable watch.

Contract mapping:

- key provisioning has been removed; callers explicitly supply a `base::KeyPair`;
- a concrete `wallets::Provider` constructs the chain-native wallet/address;
- `deposits::DepositStore` persists an `AwaitingWatch` deposit;
- `indexing::Watcher::watch` registers the address;
- the deposit becomes `Active { watch_id }` only after IX responds.

The flow is common; key derivation and address encoding are not. Bitcoin and
Ethereum generation request types remain in their own chain crates.

### 2. Observation and indexing

- one independent IX database, never the PS database;
- a canonical checkpoint containing height and block hash per chain/network;
- start historical scanning at the earliest watched deposit birthday;
- watch either an address or a transaction ID;
- query one transaction or all transactions observed for an address;
- parse multiple movements and actual atomic amounts per transaction;
- record network fee and block identity;
- track pending, included, confirmed, failed, replaced, dropped, and reorged states;
- dispatch at-least-once state-transition events through a replay cursor;
- keep IX facts free of deposit/user/incoming/sweep semantics;
- reverse orphaned blocks and reconnect the canonical branch.

Contract and implementation mapping:

- focused repository traits own atomic block effects, undo data, observations,
  feed rows, checkpoint movement, watches, backfills, and staged generations;
- `SyncWorker` owns synchronization for an
  `IndexScope { chain, network }`;
- `WatchSelector` is `Address` or `Transaction`;
- `Watcher`, `History`, and `Observer` are the semantic in-process IX surface;
- `ObservedTransaction` contains movements, fee, status, and revision;
- `indexing_rocksdb::Repository` persists IX facts and its durable event feed
  atomically over the injected `Store` contract and implements the three
  consumer traits; and
- `OutputQuery` exposes snapshot-consistent chain-neutral outputs;
  Bitcoin validates them into canonical wallet UTXOs while Ethereum does not
  require the optional output capability.

An observation is not constrained to a fake single `from/to/amount` tuple.
UTXO inputs and outputs are independent movements, while EVM native, internal,
and token transfers can each contribute movements to the same transaction.

### 3. Confirmation-depth accounting gate

Each IX chain/network scope has a `ConfirmationPolicy`. A transaction moves
through these states:

```text
Pending
  -> Included { block, confirmations }       observation snapshot only
  -> Confirmed { block, proof }              confirmation-qualified balance
  -> Reorged { previous_block }              append corrected absolute snapshot
```

`ConfirmationProof` records whether the threshold was depth, a chain-native
finalized checkpoint, or both. IX persists its canonical height/hash first and
re-evaluates included transactions as the tip advances. This matters even in a
block containing no watched transaction: that block can make an older watched
transaction deep enough to confirm.

PS mirrors every state transition. An `Included` event may append an absolute
ledger row that changes `received` and current `balance`, while `confirmed`,
`collected`, and user `accounted` remain unchanged. A `Confirmed` event advances
the confirmation-qualified balances. Thresholds belong to IX chain/network
configuration, not to an untrusted caller's `watch` request.

### 4. PS event log, classification, and ledger

- append-only mirror of relevant IX observation revisions;
- idempotency by IX event/revision, not merely `(txid, status)`;
- classification by consulting PS deposits and collection records;
- an immutable per-deposit balance journal, each row containing a complete
  absolute snapshot;
- a separate internal command for user accounting credits;
- one event may affect many deposits, as in a batched Bitcoin collection;
- PS remains able to reconcile events received after a restart.

Contract mapping:

- `ObservationEventLog` is the append-only PS mirror;
- `ObservationClassifier` produces `Incoming`, `Collection`, `GasFunding`, or
  unclassified results without changing IX facts;
- `DepositLedger` appends one idempotent absolute-balance row per
  event/deposit;
- `AccountingCommand` is the only route to `accounted`.

Each `LedgerEntry` contains:

- `received`: canonically included incoming asset value;
- `confirmed`: the subset deep/final enough for business use;
- `balance`: current canonical asset value at the deposit address;
- `collected`: confirmed gross value removed by owned collection transactions;
- `accounted`: value credited to the user by PS.

It also stores the previous row ID, classification, stable movement IDs, and
the exact IX event/revision or business idempotency key that caused the
transition. Current balances are the latest row; audit and rebuild read the
complete sequence and dereference movement details from the PS event mirror.

Example for a 100 USDT deposit (display units shown only for readability):

| Row cause | received | confirmed | balance | collected | accounted |
|---|---:|---:|---:|---:|---:|
| Deposit created | 0 | 0 | 0 | 0 | 0 |
| Incoming transfer included | 100 | 0 | 100 | 0 | 0 |
| Incoming reaches IX proof depth | 100 | 100 | 100 | 0 | 0 |
| PS credits the user | 100 | 100 | 100 | 0 | 100 |
| Token sweep included | 100 | 100 | 0 | 0 | 100 |
| Token sweep reaches proof depth | 100 | 100 | 0 | 100 | 100 |

Every row contains all five absolute values. The ERC-20 gas-funding and sweep
transaction IDs live in the collection legs; the ledger row cause points to
their IX movement IDs. If the sweep is reorged, a later row restores `balance`
and lowers `collected` without modifying any earlier row.

### 5. Stateless wallet operations

- generate an address;
- read a balance;
- build a chain-native unsigned transaction;
- inject a generic signer;
- produce a chain-native signed transaction;
- prepare a signed Ethereum collection envelope without broadcasting it;
- broadcast it;
- read its receipt;
- perform one stateless collection attempt.

Contract mapping:

- `BalanceReader`, `TransactionFactory`, `HistoryReader`, and `Addresser`
  combine through the protocol-neutral `Wallet` marker;
- `TransactionBuilder::prepare` preserves the chain-owned build/sign boundary
  and returns an exact serializable `SignedTransaction`;
- the separate one-method `Broadcaster` performs the only submission effect;
- `Wallets` selects exactly one concrete `Provider` during composition;
- model-specific controls and transaction types remain in Bitcoin or Ethereum.

The `apps/wallet` composition root deliberately selects no storage or DB
backend. Its runtime serves configured wallets' addresses, balances, full
indexed histories, and transaction submission. The checked-in binary can
compose Bitcoin and/or Ethereum wallets from explicit `WS_*` variables and is
ready when at least one complete chain configuration succeeds. With no chain
variables it remains live and truthfully not-ready; embedding hosts can instead
register wallets through `Service::with`.

### 6. Collection modes

#### Account transfer (ETH/SOL-style native asset)

One collection contains one `Sweep` leg. Broadcast records a pending leg; IX
watches the returned transaction ID; only a deep `Confirmed` fact increments
`collected`.

#### UTXO batch

`BitcoinBatchCollectionRequest` contains many deposit sources and one master
destination. `CollectionSubmission` returns per-deposit attribution. PS stores
one sweep transaction leg plus N deposit allocations, so one IX transition can
confirm or fail the whole batch without losing input ownership.

#### Token with gas

`EthereumCollectionRequirement::NativeGasBalance` reports the current,
required, and deficit amounts. PS persists two legs:

```text
GasFunding: Required -> Broadcast -> Confirmed
Sweep:      Required -> Broadcast -> Confirmed
```

WS performs each leg without durable state. PS advances the workflow only from
IX observations, so restart, retry, dropped transaction, and reorg behavior are
recoverable.

## Traceability matrix

| Requirement | Implementation | Status and evidence |
|---|---|---|
| Payment orchestration | `apps/api::{Payments,Service,Runtime,Storage}` | Configured binary composes concrete Bitcoin/Ethereum wallets, remote indexing, RocksDB, bearer-authenticated HTTP, reconciliation, and one optional finite native/ERC-20 deposit scope; no runtime schema-migration command is exposed |
| Stateless wallet HTTP composition | `apps/wallet::{compose,Service}` | Runnable binary composes configured Bitcoin/Ethereum wallets and remote indexing; zero chain configuration is live/not-ready and no production custody is claimed |
| Independent IX composition | `apps/indexer` | Runnable Ethereum and nested Bitcoin workers, mode-aware APIs/clients, health, maintenance commands, and embeddable lifecycle facades implemented; no metrics endpoint is currently claimed |
| Per-chain checkpoint height/hash | `IndexScope`, focused repository traits, `SyncStatus` | Implemented for Ethereum and Bitcoin scopes |
| Wait for provable depth | `ConfirmationPolicy`, `Included`, `ConfirmationProof` | Persisted depth transitions; Ethereum v1 defaults to 12 while Bitcoin requires an explicit deployment value |
| `watch(address)` / `watch(txid)` | `Watcher`, `WatchSelector` | Implemented by the RocksDB repository and exposed by the generic IX routes |
| `txs(address)` / `tx(txid)` | `History` | Implemented by the RocksDB repository and exposed by the generic IX routes |
| Replayable state events | `Observer`, `EventCursor`, `indexing_http::Remote` | Persistent paged cursor feed and generic remote trait implementation are implemented |
| IX facts only | `ObservedTransaction`, `ValueMovement` | Implemented without PS semantics |
| IX-owned persistence | `indexing_rocksdb::Repository`, `storage-rocksdb` | Implemented with atomic batches and adapter-private versioned records |
| Bitcoin Core identity/readiness | `chain_bitcoin::rpc::{Client,Node,FeeClient,TransactionClient}` | One shared RPC client supplies focused node, fee, and transaction capabilities; Core 31.x, network/genesis, unpruned state, IBD, block/header sync, and txindex checks are implemented |
| Bitcoin canonical UTXOs | `OutputQuery`, `chain_bitcoin::IndexUtxos` | Generic snapshot pages are validated into Bitcoin outpoints/scripts without storage vocabulary in the chain crate |
| Payment event reconciliation | `Payments::reconcile`, `ReconcileStore` | Payment evidence and the per-scope event cursor commit atomically |
| Deposit classification | `sdk/deposits::ObservationClassifier` | Payment-domain classification remains outside indexing |
| Absolute deposit balance journal | `LedgerEntry`, `DepositBalances` | Checked absolute projection, network-fee handling, reorg correction, and accounting isolation implemented |
| Included vs deep-confirmed amount | `received`, `confirmed` snapshot fields | Present |
| Internal user credit | `AccountingCommand` | Administrator-only absolute command with expected-head and idempotency checks implemented |
| Post-credit reconciliation | `ReconciliationStore` | Typed reverse-credit, accepted-liability, and external-debt decisions implemented |
| Generic address issuance flow | `payment_api::Deposits`, `deposits::WatchCoordinator` | The deposit and zero ledger row are persisted before the address is returned, and the address is returned only after IX durably acknowledges its idempotent watch |
| Balance read | `wallets::BalanceReader` | Exact `Decimal` plus an optional observation block is protocol-neutral |
| Build/sign/broadcast | chain transaction capabilities | Bitcoin SegWit/Taproot and Ethereum EIP-1559 implemented with separate non-broadcasting sign and exact-byte broadcast APIs |
| Transaction submission | `TransactionBuilder`, `SignedTransaction`, `Broadcaster` | Persistable prepare output and a separate one-method external effect are implemented |
| Server-derived collection planning | `payment_api::Planner`, `POST /v1/collections` | Caller supplies stable IDs and time only; configured policy/destination plus durable ledger and snapshot-consistent IX outputs determine amount/resources, with deterministic replay and changed-request conflict |
| BTC batched collection | `wallets::{Collector,PreparedCollection}`, `payment_api::Sweeps` | Multi-owner exact-output collection, atomic reservation fences, per-owner signing, largest-remainder fee allocation, exact-byte persistence, watch-before-broadcast, and identical retry are implemented |
| Bitcoin payment constraints | `docs/SYSTEM_REQUIREMENTS.md` | Exact-envelope persistence, exact-outpoint uniqueness, and block-only exclusions remain requirements; payment and one finite deposit scope are composed, but no funded live Core acceptance is claimed |
| ERC-20 prefund then sweep | PS collection legs + Ethereum requirement | PS persists planned gas funding, waits for its IX confirmation, then advances the token sweep |
| Retry pending broadcasts | signed-envelope record + `CollectionLegState` | Exact envelope persists before broadcast; replay verifies hash and attaches an idempotent IX watch |
| Fee and chain policy | Chain-native signed-transaction inspection + PS policy | Ethereum chain/gas/fee ceilings and Bitcoin exact-input/output/vsize/rate/absolute-fee bounds enforced before collection broadcast |
| Signing substitution | `base::Signer`, `base::KeyPair` | The one-method protocol-neutral boundary and in-process key-pair implementation exist; hardware and remote custody are outside the current workspace |
| Concrete storage engine | `packages/storage/rocksdb` | Implemented for IX and PS as separate database paths |
| Payment persistence compatibility | `apps/api::Storage` | No migration command exists; future conversion requires an explicitly approved recoverable operator workflow |
| Indexer authentication | `packages/http`, `apps/indexer`, `sdk/indexing/http` | The indexer server and matching remote adapter implement the configured mode; wallet/payment libraries leave deployment authentication to their embedding application |

## Corrections to the original accounting statements

### `received` cannot be both monotonic and canonical

If an incoming transaction is later orphaned, a monotonic `received` balance
would continue showing value that is no longer on-chain. The current contract
stores new lower absolute balances in a later ledger row; it never edits or
deletes the earlier row. Both `received` and `confirmed` can therefore fall in
the current snapshot while the complete historical journal remains monotonic
in rows.

If the business later needs a gross-lifetime metric, add a reporting metric
named `gross_received`; do not overload the canonical `received` counter.

### `collected` needs gross and net attribution

For a Bitcoin batch, the master output is less than the summed deposit inputs
because the transaction has a shared fee. Therefore:

- `collected` means gross value removed from a deposit;
- `master_credit` means value received by the master;
- `allocated_fee` records the chosen allocation of the shared fee.

Only with that definition can `received ≈ balance + collected` be a useful
reconciliation relation. It is not an exact invariant until fee allocation,
dust, unsolicited spending, token mechanics, and reorg state are specified.

### `accounted` is not collection readiness

The expression `received - collected - accounted` is not a sweepable balance.
Crediting a user's business account does not remove coins from the deposit
address; when `accounted == received`, that formula incorrectly says nothing
should be collected.

Collection eligibility must instead be derived from canonical spendable
balance minus pending reservations, network fee reserve, and dust/minimum
transfer rules. `accounted` remains a business-liability measure.

### `accounted <= confirmed` needs a reorg policy

The stronger inequality holds at the moment a credit is authorized, but a
later reorg can reduce canonical `confirmed`. The business must choose one of:

- wait for a sufficiently strong confirmation policy before crediting;
- post an explicit accounting reversal/debt entry after reorg;
- accept reorg loss as a business risk.

The implementation appends explicit `AccountingCommand` rows and does not
silently decrement user balances. After a post-credit reorg it opens a blocking
case; an administrator must record a typed reverse-credit, accepted-liability,
or external-debt decision.

### Production custody remains a composition responsibility

The checked-in concrete wallets currently compose an in-process
`base::KeyPair`; this proves the small signing boundary, not production
custody. The workspace has no `KeyLocator`, remote signer adapter, HSM/KMS
integration, or hardware-wallet implementation. A deployment that adds one
must keep authorization and non-exportable secret ownership outside the
protocol-neutral wallet traits and must not serialize a Rust trait object or
plaintext key between processes.

## Dependency and state ownership validation

```text
apps/api
  -> deposits
       -> indexing -> storage
       -> chains
       -> signer

apps/indexer
  -> chain-ethereum
       -> indexing -> storage
       -> transaction model + signer contract

apps/wallet                         (no direct storage or DB backend)
  -> chain-bitcoin / chain-ethereum
  -> signer contract

packages/* -> packages/* only
```

State ownership is consequently enforceable:

- IX facts/checkpoints/watches are behind indexing contracts;
- PS deposits/event log/accounting/collections are behind deposit contracts;
- WS has no store contract in its dependency manifest;
- no chain imports deposit/user/accounting semantics;
- no generic signer imports a chain.

## Deliberately not validated, excluded, or still open

Source structure and deterministic tests cannot establish:

- chain parser completeness;
- finality behavior for every network beyond the explicit Ethereum v1 depth
  policy;
- production filesystem, process, and crash behavior beyond the real-store
  deterministic tests that have run;
- nonce reservations and distributed multi-writer UTXO ownership beyond the
  selected single-owner PS database model;
- repeated Bitcoin collections for one deposit, or archival/space reclamation
  of its indefinitely retained signed aggregate;
- a generic UTXO-batch cancellation/failure transition or release of its
  unsigned required reservation;
- nonstandard token-tax/rebasing behavior or unsupported token policies;
- security and durability of any production custody service;
- webhook/event delivery authentication;
- correctness of reorg inverses not covered by deterministic or real-node
  tests;
- broadcast-response loss, crashes around envelope persistence/watch
  registration, dependency outages, duplicate replay delivery, lease recovery,
  and reorgs beyond retained undo depth;
- live-node PS/WS/IX acceptance (the checked-in cross-service Ethereum test uses
  a deterministic mock JSON-RPC node);
- the composed IX service against a live Ethereum node;
- the checked-in Kurtosis/Disruptoor scenario, which remains opt-in and has not
  been executed as part of ordinary Rust validation.

Ethereum v1 fixes depth 12, rollback retention 50, a RocksDB atomic repository,
and staged rebuild as testable acceptance criteria. It intentionally does not
claim Ethereum finality, mempool coverage, traces, internal transfers, or all
token behavior. Bitcoin block-only v1 requires explicit confirmation and reorg
policies and fixes Core 31, unpruned history, synchronized txindex, P2WPKH/P2TR,
raw finalized transactions, and an IX-owned canonical UTXO projection. It does
not claim mempool/RBF/drop/replacement coverage. Bitcoin payment orchestration
has deterministic repository and HTTP coverage. The
`system-tests::collection_runtime` acceptance additionally composes the
concrete runtime, Bitcoin wallet/RPC, Indexer/RocksDB, authenticated planner
replay/conflict, restart, two-input signing, and exact broadcast capture. This
remains loopback mock-node evidence, not live-Core acceptance.

Each Payment Service v1 mode fixes one exclusive RocksDB owner, one chain
scope/feed, explicit business commands, and polling-only delivery; it
intentionally does not claim HA, one database spanning multiple networks,
webhooks, automatic credit/collection, fee replacement, or production custody.
See
[`INDEXER_SERVICE.md`](./INDEXER_SERVICE.md),
[`BITCOIN_SERVICES.md`](./BITCOIN_SERVICES.md), and
[`PAYMENT_SERVICE.md`](./PAYMENT_SERVICE.md). Current Bitcoin payment
composition constraints live in `SYSTEM_REQUIREMENTS.md`.
