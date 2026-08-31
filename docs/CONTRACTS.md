# Public contracts

This document describes the reusable Rust boundaries. Current source is
authoritative for exact lifetimes, generic bounds, and error types that are
already implemented. Public Transaction Semantics, Destination Account
Acquisition, and the shared-PostgreSQL composition are integrated: the Solana
crate implements its native values, wallet/provider, one-shot RPC, submission
coordinator, sparse source, and block interpreter, while `apps/api` registers
native SOL through the central pool and reusable SDK surface.
Complete native block coordinates,
the position-aware redb/PostgreSQL repositories, sparse synchronization,
bounded transaction admission, exact ambiguity propagation, provider-owned
Bitcoin/Ethereum generation, and per-scope wallet/filter admission are now
implemented. Canonical plain Base58, witnessed account handoff, Native SOL
Submission, the reusable Solana wallet/indexing adapters, and Solana Runtime
Composition are implemented.

## Wallet collection

`wallets::Wallets<I, F>` is the chain-neutral application surface. `I` is the
embedding application's wallet identity. `F` is its configured family key,
such as the public API's asset selector. The collection does not prescribe
whether an application keys providers by chain, asset, or another closed
domain identity.

```rust,ignore
let mut wallets = wallets::Wallets::new(checkpoints.clone());

wallets.register(
    WalletAsset::Btc,
    bitcoin_scope,
    bitcoin_provider,
    bitcoin_sender,
)?;
wallets.register(
    WalletAsset::Eth,
    ethereum_scope,
    ethereum_native_provider,
    ethereum_native_sender,
)?;
wallets.register(
    WalletAsset::Usdc,
    ethereum_scope,
    ethereum_usdc_provider,
    ethereum_usdc_sender,
)?;
wallets.register(
    WalletAsset::Sol,
    solana_scope,
    solana_provider,
    solana_sender,
)?;

let imported = wallets
    .import(id, &WalletAsset::Btc, secret, BlockPosition(birthday))
    .await?;
let generated = wallets.generate(other_id, &WalletAsset::Usdc).await?;
```

Each family registration contains exactly one `IndexScope`, concrete
`Provider`, and chain batch `Sender`. Duplicate family keys fail during
composition. A separate provider registry would duplicate that same family map
and is not part of the public API.

`Provider::create` and `Provider::generate` are both mandatory methods.
Each concrete provider owns its native secret generation and validation policy,
then reuses its own `create` path. Bitcoin and Ethereum explicitly generate
secp256k1 material; Solana generates one Ed25519 seed. Both operations return an
abstract wallet and never return secret material. There is no generic
secp256k1 generation default; each provider must own its native policy.

A generated Solana wallet exists only for the current process and is not
restart-recoverable. A configured Solana import is reconstructed from exactly
one lowercase 64-character hexadecimal seed held in its named environment
variable and is registered before synchronization. Accepted bytes pass through
the shared `SecretBytes` boundary into a Solana-private key owner whose owned
secret bytes are zeroized on drop and never exposed through ordinary formatting,
serialization, cloning, or errors. Environment strings and rejected decode
temporaries follow the same existing handling as Bitcoin/Ethereum imports; this
contract adds no stricter Solana-only custody guarantee, no Solana row to
`payment_wallets`, and no claim of durable production custody.

Import requires a native-position birthday. Generation assigns the checked
successor of the current checkpoint position, or position zero when the scope
has no checkpoint. If the successor is not a produced block, the address
becomes active at the first produced block after it. `Wallets`
stores the abstract instance, public identity/scope/address metadata, and the
deduplicated `AddressFilter` used for synchronization.

`import` requires exclusive mutable access and is a startup-only operation.
After imports finish, composition wraps the collection in `Arc`; runtime code
can generate forward-only wallets but cannot add historical coverage.
Wallet lookup state uses a standard `RwLock`; operations clone the required
entry and release the lock before every `.await`.

The collection owns application operations:

```rust,ignore
let wallet = wallets.get(&id)?;
let balance = wallets.balance(&id).await?;
let page = wallets.history(&id, HistoryRequest::first(100)).await?;
let id = wallets.send(&id, destination, amount).await?;
let ids = wallets.send_all(transfers).await?;
let filters = wallets.filters()?;
```

`Wallets::send_all` is also the compatibility boundary for a chain `Sender`:
it resolves every wallet from one registered family and then selects that
family's sender. Directly pairing a public `Transfer` with a sender from a
different provider or family is outside the reusable contract.

Business and HTTP code use these methods without matching on the concrete
chain. The reusable SDK owns the registry/restoration capability used by
`Wallets::adopt`/`restore`; the embedding application selects custody policy
and composes that capability. PostgreSQL continues implementing the SDK
`Registry` over `payment_wallets`, while synchronizer repository operations do
not query or mutate it. Preserving this path does not certify the current
opaque secret bytes as encrypted production custody; that custody design
requires a separate decision.

## Wallet capabilities

`Wallet` composes only capabilities that every constructed wallet actually
supports:

- `Addresser` and `AddressFormat` for canonical and external address forms;
- `BalanceReader` for an exact selected-asset balance, with an optional
  canonical observation `BlockRef` when the concrete source supplies one;
- `HistoryReader` for complete checkpoint-bound history of the wallet's
  configured payment asset;
- `SingleSender` for one guarded chain-native submission operation; and
- `Signer` for the minimal signing request.

Bitcoin and Ethereum concrete wallets additionally implement
`TransactionFactory` for their native prepare/broadcast workflows. Solana does
not expose that split because its source lease, preparation, registration,
ordered dispatch, and ambiguity reconciliation form one indivisible operation.

Code needing one capability accepts that capability rather than `dyn Wallet`.
`Provider` is construction and does not become a post-construction wallet
capability.

`AddressEncoding::Base58` is distinct from `Base58Check`. The accepted Solana
address boundary decodes exactly 32 bytes and requires canonical plain-Base58
round trips; Bitcoin encodings keep their existing network and checksum rules.

`chain_solana::SOL` is the canonical native asset metadata: name `Solana`,
ticker `SOL`, and nine decimal places. `chain_solana::AssetKind` currently
contains only `Native`; `chain_solana::WalletConfig` binds that kind to one
configured runtime network scope. The expected genesis hash remains separate
runtime identity and is verified before storage opens.

A native SOL balance is an exact finalized `u64` lamport value converted through
`SOL` to a nine-decimal `Decimal` without floating point. Its `observed_at`
remains optional under the shared contract; a concrete implementation must not
invent a checkpoint when its balance source did not supply one.

One-wallet sending belongs to the wallet abstraction. It validates the
destination and exact positive amount, constructs the chain-native-backed
builder, prepares/signs, broadcasts the exact signed bytes, and verifies the
returned transaction ID. `Wallets::send` owns lookup and delegates this
operation.

A batch contains from one through 50 ordered occurrences and belongs to the
registered family's `Sender`. `Wallets::send_all` is the authoritative
minimum-and-maximum guard for HTTP and direct SDK callers; a concrete sender
also rejects an out-of-contract count before chain I/O. Each occurrence keeps
the identity of its zero-based input position. Conversion, wallet lookup,
common validation, sender handoff, and result mapping preserve exact length,
order, and multiplicity; duplicate items remain distinct requested payments.

The chain behaviors are:

- Bitcoin may fund one transaction from several abstract wallets, creates the
  requested outputs and per-source change, signs every input with its owner,
  and returns one submitted ID; and
- Ethereum reserves consecutive nonces per sender, prepares and signs the
  entire batch, broadcasts exact envelopes in input order, stops on the first
  failure, and returns the accepted prefix with the failed index; and
- Solana builds one distinct legacy System-transfer-plus-Memo transaction per
  occurrence, prepares and simulates the complete batch, broadcasts exact
  signed bytes in input order, and returns only the definitely acknowledged
  prefix before the first failed or ambiguous occurrence.

### Native SOL account acquisition ownership

The concrete Solana chain owns one private acquisition per public single or
batch invocation. After syntax and on-curve validation, it walks original
occurrences in order and stably deduplicates canonical 32-byte source-then-
destination addresses. At most 100 unique addresses enter one unchunked
`getMultipleAccounts` request. The same endpoint executes health admission,
opening confirmed slot `F`, the full Base64 account observation at context `C`,
and closing confirmed witness `U`, with `C >= F` and `U >= C`.

The acquisition has one complete-or-empty handoff. Exact cardinality,
positional mapping, strict account decoding, decoded-data/space agreement, and
every source and destination classification complete before the snapshot and
source balances leave the chain. Only then is witnessed `U` published
atomically as operation floor `P`. No chain-neutral RPC or transaction trait
gains Solana account DTOs, slot variables, or partial-acquisition state;
`packages/json-rpc` retains only generic framing, correlation, bounded
transport, and response execution.

Timeout, cancellation, response-size rejection, malformed response, below-
floor context, or failed closing witness is operation-wide and index-free. It
discards all account facts and floor candidates, releases every pre-envelope
lexical source lease already held by the containing send operation, and has no
transaction ID, accepted IDs, failed index, ambiguous ID, or downstream
transaction effect. It never releases a coordinator-owned submitted or
ambiguous envelope guard. A structurally coherent but unsupported account
shape instead identifies the earliest original occurrence using that account;
because it prevents the complete handoff, it still publishes no floor and
releases the containing operation's lexical leases.

### Native SOL submission ownership

The concrete Solana chain owns a process-local coordinator keyed by resolved
source public key. It acquires all involved sources in canonical byte order
before account RPC. A busy or ambiguously guarded source rejects the complete
new invocation as `SourceBusy`, releases its provisional leases, and performs no
new RPC, construction, signing, simulation, or broadcast.

`Wallets::send` and the chain-neutral `Sender` batch path reach that same
private coordinator; they do not create independent source guards. Solana
message, account, blockhash-lifetime, Memo, exact-envelope, and source-lease
types remain private to `sdk/chains/solana`. They do not add a universal RPC,
transaction, retry, or coordinator trait to `sdk/chains/base` or
`packages/json-rpc`.

The Solana crate owns one narrow submission-task registration capability;
`apps/api` implements it with its application-owned queue and tracked task set.
Registration succeeds only after the supervisor inserts the task. A closed
registrar or acknowledgement loss before insertion fails before dispatch, so no
handler or SDK object can detach an untracked send. Complete destination account
acquisition precedes registration; once a registered task crosses dispatch, its
submitted or ambiguous guard is coordinator-owned.

After the acquisition handoff publishes operation floor `P`, the coordinator
obtains one confirmed recent-blockhash lifetime; constructs one exact System
transfer plus opaque random Memo-v3 token per occurrence; obtains exact fees;
checks cumulative source `amount + fee`; signs, verifies, and serializes each
distinct message; and simulates every exact signed transaction. Any preparation
failure produces zero broadcasts. The Memo makes intentional identical
occurrences distinct but is not a request-idempotency key.

Broadcast is sequential and requires the provider result to match the locally
derived first signature. An unknown result may cause at most two additional
byte-identical submissions, for three wire calls total, after signature-status
and block-height checks. Once the first wire call begins, every transport,
provider, malformed-response, returned-signature, or cancellation outcome that
does not prove observation remains ambiguous and exposes only that locally
derived signature as reconciliation identity.

The guarded source remains unavailable until signature status or canonical
finalized indexed history proves observation, or blockhash expiry plus one
complete checkpoint-stable history traversal proves absence. Missing evidence
may block it indefinitely. State is not durable: one process must be the only
writer per source, callers must not automatically retry an unknown logical
payment, and response loss, restart, failover, active-active writers, or a new
invocation can double-pay.

Reconciliation reuses only the same scope's narrow chain-neutral `Checkpoint`
and `History` capabilities plus an application-published checkpoint-advance
notification. Indexing does not learn about source leases or exact envelopes
and stores no outgoing-operation state for the coordinator.

`SendError` carries the definitely acknowledged IDs, an optional original
`failed_index`, the source error, and an optional canonical
`ambiguous_transaction_id`. An item-scoped failure has its original index. An
empty/oversized collection, operation-wide preparation/resource failure, or
grouped-transaction broadcast failure has no synthetic index. A grouped
ambiguity may still carry its locally derived transaction ID.

The concrete chain transaction error is the sole origin of an ambiguous ID: it
derives the canonical, chain-validated value from the exact locally signed
envelope. Wallet and send-error conversion preserve that value unchanged; the
HTTP layer only projects it. Provider prose or a returned identifier that does
not match the local envelope has no authority. The ID is reconciliation
metadata, not proof of submission or an idempotency key.

Request shape and mixed-family compatibility validate before the first external
effect. All three concrete chains complete their chain-level batch preparation
before broadcast. Solana account acquisition and submission implement
ADR-0024 and ADR-0025. Ethereum uses one
coordinator shared by native and ERC-20 providers,
keyed by sender address rather than wallet family. It checks cumulative native
value, maximum fees, and token amounts before signing, and retains an exact
envelope when a retryable submission outcome is ambiguous. That sender remains
blocked until exact-hash lookup or exact-envelope replay resolves acceptance.
RPC acceptance means submitted; indexed history establishes canonical
inclusion and confirmation.

The Ethereum coordinator is process-local and is not a durable payment-operation
store. It assumes one active application writer per managed EOA and does not
claim crash-safe recovery of an in-flight submission.

An Ethereum provider is configured for exactly one `AssetKind`. Native and
ERC-20 providers may share account, transaction, and indexing handles, but each
generated wallet clones only its provider's fixed asset configuration. A token
wallet resolves movements from its configured contract, keeps native ETH fees
as fee metadata, and ignores unrelated assets in presentation without changing
the canonical address-history store.

## Indexer

A one-chain service and multi-chain composer share one object-safe surface:

```rust,ignore
pub trait Checkpoint {
    fn checkpoint(&self, scope: &IndexScope)
        -> BoxFuture<Result<Option<BlockRef>, IndexError>>;
}

pub trait History {
    fn history(&self, query: HistoryQuery)
        -> BoxFuture<Result<TransactionPage, IndexError>>;
}

pub trait Indexer: Checkpoint + History {
    fn scopes(&self) -> &[IndexScope];

    fn sync(&self, selection: &dyn FilterSource)
        -> BoxFuture<Result<Vec<SyncStatus>, IndexError>>;
}

pub trait FilterSource: Send + Sync {
    fn filters(&self) -> Result<Vec<AddressFilter>, IndexError>;
    fn plan(&self, scope: &IndexScope, checkpoint: Option<BlockRef>)
        -> Result<SyncPlan, IndexError>;
}
```

The `BlockRef` returned by `Checkpoint` contains a native `BlockPosition`, a
produced `BlockHeight`, the block hash, and an atomic optional parent pairing
parent position with parent hash. `BlockPosition` drives RPC traversal,
canonical lookup, restart, readiness, and birthdays. `BlockHeight` drives
confirmation arithmetic, history/output ordering, journal keys, and retained-
block counts. Only genesis may omit the parent pair.

`sync` asks the selection for one immutable checkpoint/filter-revision plan.
Before each canonical repository transition it obtains a commit permit that
rechecks both values. Runtime wallet publication waits for an in-flight commit,
uses the resulting persisted checkpoint's checked successor as its birthday,
inserts the wallet/filter, and only then increments the revision. If publication
wins, an older plan cannot commit. If cancellation occurs after repository I/O
starts, the next plan must reload the authoritative repository checkpoint.
Coordinator mutex guards never cross `.await`; source and repository I/O happen
outside the short critical section.

`Service<S, I, R>` implements `Indexer` for one exact scope. `Composer` requires
at least one child, rejects duplicate scopes, validates a complete filter
snapshot, partitions it by scope, routes checkpoint/history calls, and
synchronizes every child.

Synchronization policy stays concrete and small:

```rust,ignore
SyncConfig::new(scope, minimum_confirmations, reorg_retention, batch_size)?;
```

The three numeric inputs must be greater than zero. Confirmation depth is the
`u64` value itself; it does not need a one-field policy wrapper.

`Wallets` owns the address selection, one admission coordinator per registered
scope, and the composed `Checkpoint` capability used to choose safe runtime
birthdays. Indexing owns no wallet identity, secret, or watch lifecycle. The
sync task hands the composed indexer `Wallets` as its `FilterSource`, so plan
capture and runtime publication share the same scope boundary.

Filter addresses are non-empty, unique, and scoped to a configured child.
Composer validates the whole selection before any source I/O and narrows it per
child on each read; `Wallets` keeps the earliest birthday when several wallets
have the same canonical address.

`Outputs` is an independent capability. It is injected only into consumers
that need live UTXOs; it is not a supertrait of `Indexer`.

## Chain indexing contracts

Each chain implements:

- `BlockSource`, wrapping native RPC produced-tip, bounded produced-block range,
  and canonical reference-at-position reads; and
- `BlockInterpreter`, converting one native block and the active canonical
  addresses into `InterpretedBlock`.

The source range is ordered by native position and omits coordinates where no
block was produced. Its bound counts returned blocks, not numeric position
distance. Bitcoin and Ethereum positions are dense; Solana slots may be sparse.

`InterpretedBlock` contains a complete `BlockRef`, transaction drafts, and
`OutputChanges`. It contains no storage key, record, journal, or backend type.
Bitcoin, Ethereum, and Solana remain free to use different native RPC and
transaction models.

The accepted Solana interpreter admits finalized legacy and version-0
transactions, resolves loaded addresses, and decodes supported top-level and
inner System Program `Transfer` and `TransferWithSeed` instructions. It retains
the first signature as transaction identity, authoritative execution status and
fee, emits no movements for failed transactions, and emits no UTXO output
changes. Missing or inconsistent meaningful metadata invalidates the complete
source block rather than advancing a checkpoint with partial history.

## Persistence collections

```rust,ignore
pub trait Blocks {
    fn get(&self, selector: BlockSelector)
        -> BoxFuture<Result<Option<BlockRef>, IndexError>>;
    fn add(&self, addition: BlockAddition)
        -> BoxFuture<Result<BlockOutcome, IndexError>>;
    fn remove(&self, scope: IndexScope, expected_tip: BlockRef)
        -> BoxFuture<Result<Option<BlockRef>, IndexError>>;
}

pub trait Transactions {
    fn list(&self, query: HistoryQuery)
        -> BoxFuture<Result<CanonicalPage, IndexError>>;
}

pub trait Outputs {
    fn list(&self, request: OutputRequest)
        -> BoxFuture<Result<OutputPage, IndexError>>;
}
```

`Blocks::add` compares the expected checkpoint and atomically writes canonical
address history, live output changes, a storage-derived bounded journal entry,
and the new checkpoint. `Blocks::remove` verifies the current tip and derives
the entire inverse from that private journal. No caller can author commit or
rollback state.

`Transactions` and `Outputs` are read projections over that atomic block
lifecycle. A persistence implementation may use redb, PostgreSQL, or another
transactional backend, but backend records never cross these contracts.

The application composition opens one PostgreSQL database/schema and
one process-wide pool. It clones that pool into one
`indexing_postgres::Repository` handle per exact `(chain, network)` scope. A
handle refuses another scope. An asset is a history fact, so native and token
assets on the same chain share the same repository rather than acquiring an
asset repository or schema.

The PostgreSQL `Repository` owns only checkpoint, canonical history/movement,
live-output, and bounded-journal collections. The same SDK adapter preserves
its separate `Registry` capability over `payment_wallets`; that table remains
outside synchronization collections even when physically colocated. redb
remains an embedded implementation and test backend; `apps/api` uses
PostgreSQL.

Shared PostgreSQL evolution is preservation-first. Generic columns and
constraints are added and validated without destroying existing scopes. A
scope-local rescan may replace only indexing-owned rows for its explicitly
approved `(chain, network)` and must preserve every other scope and
SDK registry row. No legacy runtime reader, inferred coordinate fallback,
compatibility alias, or versioned storage DTO is part of the contract.

The deployment-owned canonical creation and ordered migration scripts live
physically under `sdk/indexing/postgres/migrations/`. This one history may
describe the central schema, but a change to the SDK registry table still
requires separate SDK-level custody approval; synchronization repository
operations remain limited to indexing tables and issue no registry-table DDL.

## History model

`CanonicalTransaction` stores:

- scoped transaction identity;
- included or failed state tied to a canonical `BlockRef`;
- every stable `ValueMovement`; and
- optional exact network fee.

The `History` implementation reads canonical transactions and derives
`ObservedTransaction` confirmation from inclusion produced height and the page
checkpoint's produced height. `Confirmed` carries the observed depth. The
current API neither stores confirmation transitions nor claims chain-finality
proof.

Bitcoin inputs and outputs are separate movements, so multi-input and
multi-output history remains truthful. Transfer, input, output, mint, and burn
are the current movement variants. Native and token assets have different
`AssetId` values. Wallet history resolves trusted asset metadata and converts
exact atomic values into exact display decimals.

History and output cursors carry the checkpoint snapshot from their first
page. A changed checkpoint produces a conflict and requires pagination to
restart.

## Address coverage contract

All imported wallets and native-position birthdays for an existing scope form
one authoritative startup set. They are registered before the first sync. A
fresh scope locates the first produced block at or after the earliest birthday
and anchors at that block's actual parent; it never invents `birthday - 1`. An
empty scope anchors at the actual produced tip.

A wallet created at runtime starts at the checked successor of
`checkpoint.position`; a skipped successor activates at the first later
produced block. A historical address
cannot be added after `Wallets` becomes shared because import is startup-only.
If the authoritative startup set changes below the persisted checkpoint, the
embedding application may recreate and rescan only that scope's indexing-owned
rows. It must not drop the shared database, alter another scope, or touch SDK
registry wallet rows. Synchronization stores no durable filter selection and
cannot infer selection drift across restarts; the separate SDK `Registry`
persists the wallet restoration facts used to rebuild that selection.

## Composition contract

The process is assembled directly in `apps/api/src/main.rs`:

1. validate the complete closed configuration;
2. construct one long-lived client per configured chain, using one singular
   no-retry Solana endpoint and redacted configuration;
3. verify all chain identities before database mutation, including one-shot
   Solana genesis and finalized executable Memo-v3 checks;
4. construct one process-wide PostgreSQL pool and call
   `indexing_postgres::validate_schema(&pool, configured_schema)` to validate
   the already-applied pinned schema in one read-only transaction without DDL;
5. clone the pool into one scope-bound repository per `(chain, network)`, load
   checkpoints, and initialize filter/commit coordination;
6. construct services and one `Arc<Composer>`, then expose narrow `Indexer`,
   `Checkpoint`, `History`, and Bitcoin-only `Outputs` views;
7. inject the Solana service's checkpoint/history views and checkpoint
   notification into its coordinator and inject the application task registrar;
8. construct `Wallets`, register only native SOL for Solana, and import every
   configured wallet at `start_position` before the first sync snapshot;
9. start supervised synchronization, readiness, and submission tasks;
10. wait for persisted ready checkpoints before binding HTTP; and
11. supervise HTTP, sync, fatal exits, cancellation, ambiguity reconciliation,
    and graceful shutdown.

No step creates a database, schema, pool, or repository for an asset. The
current `apps/api` composition implements this PostgreSQL contract and performs
no startup DDL.

There is no process facade or app-local service facade. HTTP state contains the
abstract wallet collection and readiness state only. Concrete handles and
chain selection remain in `main`.

A runtime-fatal Solana indexer error publishes not-ready and closes admission.
If no exact envelope is guarded, the supervisor joins and returns the error. If
one is submitted or ambiguous, shutdown waits without an automatic deadline
while the reconciliation paths required for safety remain active. Registrar
closure, handler drain, guarded-envelope drain, synchronization cancellation,
and storage joins occur in that order. After a fatal indexer exit, only positive
historical status can clear the guard in-process; force-kill accepts the known
duplicate-payment risk.

## Contract evidence

Repository contract tests must exercise both redb and PostgreSQL with complete
block references, sparse native positions, dense Bitcoin/Ethereum positions,
atomic parent presence, scope rejection, commit/rollback atomicity, and
checkpoint-bound pagination. Shared-schema tests must use one pool to prove
Bitcoin, Ethereum, and Solana scope isolation, native/token asset coexistence,
and byte-for-byte preservation of sentinel SDK registry wallet rows.

Application system tests must compose the shared PostgreSQL topology,
prove restart/readiness from native position while confirmations remain
produced-height based, and verify that a scope-local rescan leaves unrelated
scopes and `payment_wallets` unchanged. Solana system evidence must additionally
prove singular endpoint configuration and redaction, genesis/Memo probes,
tracked registration before dispatch, shutdown races and indefinite ambiguity,
the pinned/checksummed owned validator, and the explicit `solana_stack` target.

## HTTP contract

`apps/api` owns all public wire models, Utoipa schema derivation, extraction,
authentication, request limits, error/status mapping, and encoding. SDK crates
know no Axum route or response shape.

Following transport and authentication handling, both transaction POST routes
reject a non-empty URI query before JSON shape extraction. An empty query
component has no semantic effect. Ordinary infrastructure headers are allowed
but are not interpreted as transaction-control inputs. HTTP applies the shared
50-item maximum before converting a batch item, while the SDK collection still
owns the authoritative guard.

The fixed transaction precedence is query rejection, JSON schema, collection
cardinality, wire conversion in original order, then itemwise common
validation. For each occurrence that itemwise stage checks positive amount,
wallet resolution, and family compatibility before advancing. Chain-specific
complete preparation and ordered broadcast follow. HTTP error projection
includes optional accepted IDs, failed index, and ambiguous ID only when the
underlying domain error truthfully supplies them; any ambiguous ID maps to
`503`. A native SOL acquisition-wide failure supplies none of those optional
fields; a structurally valid unsupported account shape may supply only its
truthful original item index.

Each endpoint-specific input and output struct is declared immediately above
its handler. A handler performs extraction, one `Wallets` call, mapping, and
encoding. Catch-all DTO modules are not part of the structure. A wire
type is shared only when several endpoints use the exact same contract;
reusable domain values remain in their SDK owner.

Public routes remain chain-neutral for wallet generation, metadata, balance,
paginated transactions, one send, and ordered batch send. Health distinguishes
liveness from index readiness. There is no public indexing command surface.
