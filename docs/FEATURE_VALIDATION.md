# Original design feature extraction and validation

## Verdict

The repository now contains more than the original contract scaffold. The
stateless Bitcoin/Ethereum Wallet Service execution paths and both chain-specific
Indexer Service modes have concrete implementations. Ethereum retains its
HTTP JSON-RPC parser and durable IX vertical slice. Bitcoin adds a direct Core
31 adapter, authenticated stateless WS runtime, block interpreter, canonical
UTXO projection, generation/revision/checkpoint-fenced query surface, and durable IX runtime over
the same ordered synchronization, RocksDB, reorg, replay, and staged-rebuild
machinery. The Payment Service source includes authenticated
public/admin APIs, durable users/jobs and command idempotency, deposit/watch
recovery, IX mirroring, storage-aware classification, absolute ledger
projection, a per-deposit observation index, typed reconciliation, and
native/ERC-20 collection execution. Its nested Bitcoin mode adds strict native-
BTC policy binding, multi-source jobs, atomic exact-outpoint reservations,
full-drain/no-change execution, deterministic fee allocation, retained exact
bytes, and all-participant projection. Neither Wallet HTTP mode owns a database.

This is still not evidence of a production deployment. A loopback-only
ephemeral custody adapter now exists, but external durable custody, automated
Anvil acceptance, HA, and multi-network ownership in a single PS store remain
absent or deliberately excluded. The Bitcoin Core 31 disposable-regtest
acceptance scenario is also pending because the required binary is unavailable
in the current environment; deterministic fixtures do not prove composed
real-node behavior. Its checked-in
[`manual acceptance procedure`](./manual-bitcoin-regtest/README.md) has not been
executed and is not pass evidence. The complete composed failure-window matrix
remains acceptance work even though the repository command applies a Bitcoin
batch's collection/ledger/reconciliation/cursor effects atomically.

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

- `signer::KeyProvisioner` provisions an opaque key handle and public key;
- `chain_contract::DepositAddressGenerator<C>` derives the chain-native address;
- `deposits::DepositStore` persists an `AwaitingWatch` deposit;
- `indexing::ObservationRegistry::watch` registers the address;
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

- `IndexRepository` owns atomic block effects, undo data, observations, feed
  rows, checkpoint movement, watches, backfills, and staged generations;
- `OrderedSyncWorker` owns synchronization for an
  `IndexScope { chain, network }`;
- `WatchSelector` is `Address` or `Transaction`;
- `ObservationRegistry`, `ObservationQuery`, and `ObservationEventSource` are
  the semantic public IX surface;
- `ObservedTransaction` contains movements, fee, status, and revision;
- `PersistentIndexRepository` persists IX facts and its durable event feed
  atomically over the injected `Storage` contract; and
- `ProjectionQuery` exposes active-generation opaque chain projections;
  Bitcoin uses it for canonical watched UTXOs while Ethereum supplies an empty
  projection batch.

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

- small capabilities in `chain-contract` replace a single oversized wallet
  interface;
- `TransferBuilder -> TransactionSigner -> Broadcaster` preserves the unsigned
  and signed states;
- `Collector<C>` reports factual requirements and retains the compatible
  prepare-then-broadcast one-shot operation;
- `EthereumWallet::prepare_collection` returns the exact signed envelope and
  attribution before the broadcast side effect;
- `WalletFactory<C>` selects the stateless per-asset adapter exposed by WS;
- concrete `CollectionRequest`, `CollectionRequirement`, and attribution types
  live in the Bitcoin or Ethereum crate.

The `apps/wallet` composition root deliberately selects no storage or DB
backend. It serves authenticated ETH/ERC-20 address, balance, signing,
collection-requirement, collection-preparation, exact-envelope broadcast, and
receipt endpoints over bounded JSON. Its nested Bitcoin mode adds authenticated
P2WPKH/P2TR address generation, IX-backed balances, exact-selected-input
transfer/collection signing, Core preflight/broadcast, and receipt endpoints.
WS revalidates selected outputs against one generation/revision/checkpoint IX
snapshot, verifies that checkpoint against Core immediately before signing,
but never reserves the outputs.

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

| Original requirement | Scaffold location | Structural status |
|---|---|---|
| PS runtime composition | `apps/api` | Authenticated Ethereum and nested Bitcoin modes with durable jobs, watch reconciliation, IX ingestion/projection, expiration, collection, readiness, backup, and migration |
| Stateless WS composition | `apps/wallet` | Authenticated chain-specific Ethereum and Bitcoin HTTP runtimes with concrete RPC/IX and remote-custody adapters; no direct storage/backend |
| Independent IX composition | `apps/indexer` | Runnable Ethereum and nested Bitcoin workers, APIs, health, metrics, maintenance commands, and embeddable lifecycle facades implemented |
| Per-chain checkpoint height/hash | `IndexScope`, `IndexRepository`, `SyncStatus` | Implemented for Ethereum and Bitcoin scopes |
| Wait for provable depth | `ConfirmationPolicy`, `Included`, `ConfirmationProof` | Persisted depth transitions; Ethereum v1 defaults to 12 while Bitcoin requires an explicit deployment value |
| `watch(address)` / `watch(txid)` | `ObservationRegistry`, `WatchSelector` | Implemented through both chain-specific IX HTTP APIs |
| `txs(address)` / `tx(txid)` | `ObservationQuery` | Implemented through both chain-specific IX HTTP APIs |
| Replayable state events | `ObservationEventSource`, `EventCursor` | Persistent cursor feed implemented |
| IX facts only | `ObservedTransaction`, `ValueMovement` | Implemented without PS semantics |
| IX-owned persistence | `PersistentIndexRepository`, `storage-rocksdb` | Implemented with atomic batches, explicit RecordV1 formats, and generation-scoped chain projections |
| Bitcoin Core identity/readiness | `BitcoinCoreClient` | Core 31.x, network/genesis, unpruned state, IBD, block/header sync, and current txindex checks implemented |
| Bitcoin canonical UTXOs | `BitcoinIndexRecordCodec`, `ProjectionQuery`, Bitcoin IX API | Watched output create/spend, atomic rollback, full projection-snapshot pagination, Core checkpoint binding, and coinbase metadata implemented |
| PS append-only event mirror | `PersistentPaymentRepository` | Implemented with atomic ingestion cursor advancement and durable deposit-to-observation indexing |
| PS classification | `ObservationClassifier`, `apps/api::runtime` | Storage-aware precedence and unresolved-fact projection stop implemented |
| Absolute deposit balance journal | `LedgerEntry`, `DepositBalances` | Checked absolute projection, network-fee handling, reorg correction, and accounting isolation implemented |
| Included vs deep-confirmed amount | `received`, `confirmed` snapshot fields | Present |
| Internal user credit | `AccountingCommand` | Administrator-only absolute command with expected-head and idempotency checks implemented |
| Post-credit reconciliation | `ReconciliationStore` | Typed reverse-credit, accepted-liability, and external-debt decisions implemented |
| Generic address generation flow | `KeyProvisioner`, `DepositAddressGenerator<C>` | Present |
| Balance read | `BalanceReader<C>` | Implemented for Ethereum/ERC-20 RPC and Bitcoin IX-backed canonical UTXOs |
| Build/sign/broadcast | chain transaction capabilities | Bitcoin SegWit/Taproot and Ethereum EIP-1559 implemented with separate non-broadcasting sign and exact-byte broadcast APIs |
| Wallet/account collection | `Collector<C>`, PS collection executor | Native Ethereum durable reservation/sign/broadcast/watch workflow implemented |
| BTC batched collection | Bitcoin collection request + attribution | Same-principal multi-deposit job, fenced complete-UTXO selection, atomic exact-outpoint reservation, stateless signing, fee allocation, exact-byte persistence/broadcast, watch, and block-only projection implemented |
| Bitcoin PS v1 decision record | `TODO/BITCOIN_PAYMENT_SERVICE_IMPLEMENTATION_PLAN.md` | Implemented: one native-BTC scope, same-principal full-drain/no-change batches, mandatory ceilings, retained exact bytes, same-txid re-inclusion, and block-only exclusions; live Core 31 acceptance pending |
| ERC-20 prefund then sweep | PS collection legs + Ethereum requirement | PS persists planned gas funding, waits for its IX confirmation, then advances the token sweep |
| Retry pending broadcasts | signed-envelope record + `CollectionLegState` | Exact envelope persists before broadcast; replay verifies hash and attaches an idempotent IX watch |
| Fee and chain policy | Chain-native signed-transaction inspection + PS policy | Ethereum chain/gas/fee ceilings and Bitcoin exact-input/output/vsize/rate/absolute-fee bounds enforced before collection broadcast |
| Local/remote/Trezor substitution | `Signer` and separate signer crates | Ephemeral local signer and authenticated remote adapter implemented; Trezor remains a placeholder |
| Concrete storage engine | `sdk/storage/rocksdb` | Implemented for IX and PS as separate database paths |
| PS schema migration | `PaymentDatabaseMetadataStore`, `payment-api migrate` | Verified physical backup, semantic validation/index rebuild, and fail-closed schema-v2 binding implemented |

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

### “Keys live in PS” means custody authority, not raw bytes in every call

A Trezor/HSM/KMS key cannot be returned as exportable private material. PS
persists the deposit-to-`KeyLocator` relationship and owns authorization; the
selected `KeyProvisioner`/`Signer` owns the actual secret. WS receives a public
key or opaque locator and never chooses a concrete signer. If PS and WS are
separate processes, the same semantic contract requires an authenticated remote
signer adapter rather than serializing a Rust trait object or plaintext key.

## Dependency and state ownership validation

```text
apps/api
  -> deposits
       -> indexing -> storage
       -> chain-identity
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
- security and durability of a production custody service (the local server is
  explicitly ephemeral and loopback-only);
- webhook/event delivery authentication;
- correctness of reorg inverses not covered by deterministic or real-node
  tests;
- the complete PS crash-window, restart, and collection workflow test matrix;
- automated, repeatable PS/WS/IX Anvil end-to-end acceptance;
- the composed IX service against a live Ethereum node;
- the composed Bitcoin IX/WS/PS services against the pinned Bitcoin Core 31
  regtest node, including restart, controlled reorg, UTXO restoration, and
  re-inclusion; the unexecuted procedure is documented in
  [`manual-bitcoin-regtest/README.md`](./manual-bitcoin-regtest/README.md); and
- the checked-in Kurtosis/Disruptoor scenario, which remains opt-in and has not
  been executed as part of ordinary Rust validation.

Ethereum v1 fixes depth 12, rollback retention 50, a RocksDB atomic repository,
and staged rebuild as testable acceptance criteria. It intentionally does not
claim Ethereum finality, mempool coverage, traces, internal transfers, or all
token behavior. Bitcoin block-only v1 requires explicit confirmation and reorg
policies and fixes Core 31, unpruned history, synchronized txindex, P2WPKH/P2TR,
raw finalized transactions, and an IX-owned canonical UTXO projection. It does
not claim mempool/RBF/drop/replacement coverage. Bitcoin PS block-only v1 now
has a concrete source/runtime and deterministic repository/HTTP/workflow
coverage, but no Core 31 operational acceptance evidence. Its behavior is one
native-BTC scope per database, same-principal cross-user full-drain/no-change
batches,
atomic exact-outpoint reservations, proportional largest-remainder fee
allocation, mandatory financial limits, retained same bytes across broadcast/
reorg, and same-txid re-inclusion without replacement signing. That retained
ownership limits v1 to one Bitcoin collection aggregate per deposit; late
payments remain watched/accounted but require a new deposit address to be
collectable.

Each Payment Service v1 mode fixes one exclusive RocksDB owner, one chain
scope/feed, explicit business commands, and polling-only delivery; it
intentionally does not claim HA, one database spanning multiple networks,
webhooks, automatic credit/collection, fee replacement, or production custody.
See
[`INDEXER_SERVICE.md`](./INDEXER_SERVICE.md),
[`BITCOIN_SERVICES.md`](./BITCOIN_SERVICES.md), and
[`PAYMENT_SERVICE.md`](./PAYMENT_SERVICE.md). The Bitcoin PS implementation and
remaining acceptance record is
[`BITCOIN_PAYMENT_SERVICE_IMPLEMENTATION_PLAN.md`](../TODO/BITCOIN_PAYMENT_SERVICE_IMPLEMENTATION_PLAN.md).
