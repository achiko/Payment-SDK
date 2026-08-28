# Indexing

## Purpose

Indexing turns chain-native blocks into canonical transaction history and the
live output projection needed by UTXO wallets. It runs inside `apps/api`; it is
not a service, HTTP API, wallet registry, or storage engine.

The public indexing domain answers:

1. which complete block reference a chain/network scope has processed;
2. which complete canonical transactions affect an address;
3. what confirmation state those transactions have at a checkpoint; and
4. which indexed outputs are currently live when a chain needs that projection.

It does not provide watches, event feeds, raw-block archives, migration or
rebuild commands, or durable pending-transaction state.

This is the target indexing design. The current `apps/api` still composes one
redb repository per chain and the current `BlockRef` is height-only. The shared
PostgreSQL topology and two-coordinate block model below are not yet claimed as
implemented. No Solana crate, source, interpreter, or service exists yet; the
Solana object below is a target peer of the existing Bitcoin and Ethereum
services. ADR-0027 accepts its runtime composition, but that acceptance is an
implementation contract rather than evidence that the target exists.

## Object graph

```text
chain RPC client
    -> chain BlockSource
    -> chain-native block
    -> chain BlockInterpreter + active addresses
    -> InterpretedBlock
    -> chain Service implementing Indexer
    -> Blocks / Transactions / Outputs
    -> scope-bound indexing PostgreSQL Repository
    -> one process-wide PostgreSQL pool and shared schema

Bitcoin Service ----\
Ethereum Service ----+-> Composer implementing the same Indexer contract
Solana Service ------/

Wallets::filters() -> FilterSource -> Indexer::sync(selection)
HTTP/wallet reads  -> Checkpoint / History / optional Outputs
```

`Service` binds one chain source, interpreter, repository, scope, and
synchronization policy. `Composer` combines disjoint scopes and routes by
`IndexScope`. A caller uses the same `Indexer` contract for either object.

The target process opens one database/schema and one pool. It clones the pool
into one repository handle per exact `(chain, network)`; the handle rejects a
different scope. Native and token assets are history facts and share their
chain/network repository. redb remains an embedded implementation and test
backend, but it is not the target `apps/api` persistence composition.

## Caller contract

`Indexer` combines the two general query capabilities and synchronization:

```rust,ignore
pub trait Indexer: Checkpoint + History {
    fn scopes(&self) -> &[IndexScope];

    fn sync(&self, selection: &dyn FilterSource)
        -> BoxFuture<Result<Vec<SyncStatus>, IndexError>>;
}

pub trait FilterSource: Send + Sync {
    fn filters(&self) -> Result<Vec<AddressFilter>, IndexError>;
}
```

`AddressFilter` is one canonical address plus its native-position birthday.
`Wallets` owns and deduplicates the authoritative address set. Every filter
read returns a complete snapshot; indexing stores no watch, wallet identity,
secret, or internal address registry. A sync reads once before source I/O for
fail-fast validation and again after observing the tip so a newly admitted
forward-only wallet cannot be passed by the checkpoint.

`Composer` validates all filters before any child sync, partitions them by
scope, and invokes every configured child. An empty partition is meaningful:
that scope still follows its tip and remains ready.

The composer itself requires at least one child. Filter addresses must be
non-empty, unique, and belong to a configured scope; validation finishes before
any source call. `Wallets::filters()` satisfies the uniqueness rule by retaining
the earliest birthday for a repeated canonical address.

`Outputs` is deliberately separate from `Indexer`. Bitcoin needs live outputs
for balance and transaction construction; Ethereum does not need UTXO
semantics. Composition injects the narrow capability each consumer requires.

Every `BlockRef` contains:

- `BlockPosition`, the native RPC coordinate used for traversal, canonical
  lookup, restart, readiness, and birthdays;
- `BlockHeight`, the produced-block count used for confirmations, history and
  output ordering, rollback journal keys, and retention;
- the canonical block hash; and
- one atomic optional parent value pairing parent position with parent hash.

Only genesis has no parent. Bitcoin and Ethereum have dense positions equal to
their produced heights. Solana positions are slots and may be sparse, while
its produced height still increments exactly once per committed child.

## Chain producer contracts

`BlockSource` exposes only canonical block reads needed by synchronization: the
latest complete produced-block reference, a bounded ordered fetch of actual
produced blocks in an inclusive native-position range, and canonical reference
lookup at one native position. The range omits positions with no produced
block, and its limit counts returned blocks rather than numeric position
distance. Concrete RPC methods and DTOs stay in the chain crate.

`BlockInterpreter` receives one native block and the canonical addresses active
at that native position. It returns `InterpretedBlock` with:

- the unchanged `BlockRef`;
- complete transaction drafts and stable movements; and
- `OutputChanges` for live UTXO state.

Bitcoin represents each input and output separately. Ethereum represents
native value and token log movements with distinct assets. Solana represents
native SOL System Program movements without UTXO output changes. Interpreters
do not create persistence keys, journals, rows, batches, or codecs. An outbound
Memo token supplies transaction uniqueness only; it is not a value movement and
does not alter canonical payment history.

## Persistence collections

Storage-independent persistence has three clear nouns:

| Collection | Operations | Meaning |
|---|---|---|
| `Blocks` | `get`, `add`, `remove` | canonical checkpoint/block lookup and atomic block lifecycle |
| `Transactions` | `list` | address-primary canonical history |
| `Outputs` | `list` | current live outputs for one address |

`Blocks::get` selects either the scope tip or a retained block by produced
height. Non-tip lookup is limited to canonical journal coverage used during
reorg reconciliation. The returned complete block reference supplies the
native position for remote canonical lookup; produced height is never passed to
a chain RPC as a substitute coordinate.

`BlockAddition::new` receives the scope, expected checkpoint, retention, and
`InterpretedBlock`. It validates the parent connection, duplicate transaction
IDs, scoped movements, amounts, and output changes before storage is called.
For every non-genesis child, its position must increase, its produced height
must equal its parent's height plus one, and its atomic parent position/hash
must match the checkpoint.

`Blocks::add` atomically:

1. compares the expected checkpoint;
2. reads required current outputs and derives private undo data;
3. writes every affected address's complete canonical transaction;
4. updates live outputs;
5. writes one rollback-journal entry;
6. moves the checkpoint; and
7. removes journal entries beyond retention.

The storage adapter derives undo from its own state. There is no public commit
context, plan, or caller-authored rollback payload.

`Blocks::remove(scope, expected_tip)` verifies that the expected tip is still
current, reads its own journal, removes orphan canonical history, restores live
outputs, deletes the journal entry, and moves the checkpoint to the recorded
parent in one atomic write. Missing or inconsistent journal state is an error,
not a reason to move the checkpoint anyway.

`Transactions::list` and `Outputs::list` are read projections. They do not
write independent state or contain synchronization policy.

The target PostgreSQL adapter owns only the checkpoint, history/movement,
live-output, journal, and journal-output tables. One schema holds every scope;
each repository handle is bound to one exact `(chain, network)`. An asset is a
fact in a movement row, not a reason to create another repository, schema, or
pool. Solana writes no UTXO output rows.

Deployment-owned central schema creation and migration scripts live physically
under `sdk/indexing/postgres/migrations/`. Their physical location does not
make an application-owned table part of the indexing repository contract.

Application-owned custody tables are outside indexing even when physically
colocated in the central database. In particular, the indexing runtime adapter
must not read, write, truncate, delete, or issue DDL for `payment_wallets`.
Shared-schema evolution is preservation-first: add and backfill generic fields
under validation, then enforce final constraints. A scope-local rescan may
replace only indexing-owned rows for an explicitly approved scope and must
preserve all other scopes and application-owned rows. No runtime compatibility
reader, versioned storage DTO, or inferred coordinate fallback is introduced.

## Durable state

For each exact chain/network scope, the repository stores only:

- one canonical checkpoint with native position, produced height, hash, and
  atomic optional parent position/hash;
- complete canonical transactions keyed beneath each affected selected
  address, including their complete inclusion block references;
- current live outputs; and
- one bounded rollback-journal entry per retained canonical block, including
  the complete current and previous checkpoint references needed for rollback.

Copying a transaction beneath each affected address makes history a primary
ordered scan. It intentionally avoids a second address index. Journal records
are physical storage details and never cross the repository boundary.

Ethereum address history retains every canonical native and Transfer-shaped
token fact for the watched address. Asset-specific wallet presentation is a
concrete Ethereum-wallet projection over that checkpoint-bound page; it does
not add an asset index, token catalog, or alternate persistence record.

The repository does not store address filters, wallet identities or secrets,
watch IDs, synchronizer phase, confirmations, observation revisions, pending-
confirmation records, spent markers, an event log, or raw blocks.

## Initial coverage and birthdays

Composition registers all configured imported wallets before the first sync
and supplies a caller-owned `FilterSource` thereafter. Each source read returns
one complete filter snapshot; synchronization controls when those reads occur.

For a fresh scope:

- no filters: establish the observed tip as an empty anchor;
- earliest birthday `B > 0`: locate the first produced block at or after `B`,
  establish that block's actual parent as the anchor, then interpret forward;
  and
- birthday position `0`: interpret from genesis.

The anchor gives the first interpreted block a verified parent without reading
irrelevant history or manufacturing `B - 1`, which may be a skipped native
position. At position `P`, only filters with `start_position <= P` are active.
A generated wallet starts at the checked successor of the current checkpoint
position; if that position is skipped, it activates at the first later produced
block. Existing history therefore remains complete without backfill.

One scope checkpoint describes coverage for the complete filter snapshot that
produced it. Adding a historical address, or lowering a birthday, beneath an
existing checkpoint would create incomplete history. Runtime import is
unavailable after the wallet collection is shared, while generation is
forward-only. Across process restarts, the embedding application must load the
same authoritative historical wallet set before synchronization; because
indexing deliberately stores no filters, it cannot detect selection drift. If
the set changed, composition must recreate and rescan that scope.

## Forward synchronization

Each service receives a `SyncConfig` built as
`SyncConfig::new(scope, minimum_confirmations, reorg_retention, batch_size)`.
Minimum confirmations, rollback retention, and batch size must all be greater
than zero. The configured `u64` confirmation depth is used directly when
history derives transaction status.

For each bounded sync invocation:

1. read the repository tip and source tip;
2. verify the stored `(position, hash)` remains canonical;
3. reconcile a retained reorg when it does not;
4. request actual produced blocks after the checkpoint position, bounded by
   the requested returned-block count;
5. select filters active at each returned block's native position;
6. verify and interpret each native block in position order;
7. verify its canonical hash again immediately before persistence; and
8. call `Blocks::add` with the expected checkpoint.

A restart resumes from the checked successor of `checkpoint.position`; skipped
positions are omitted rather than synthesized. An RPC failure is retryable or
unknown; it does not alter canonical state or prove a submitted transaction was
dropped.

## Reorg reconciliation

When the stored tip is no longer canonical, the synchronizer walks retained
local block references by produced height and queries remote canonical state at
each stored native position until it finds the common ancestor. It then calls
`Blocks::remove` for each orphan tip. Each removal atomically deletes orphan
history, restores output state, and moves the checkpoint to the complete
previous reference stored in the journal. Reconciliation never subtracts one
from a native position to invent a parent. The replacement branch is added
through the ordinary forward path.

If no common ancestor exists within journal retention, synchronization returns
`ReorgTooDeep`. Only that exact scope's indexing-owned rows may be recreated and
rescanned from the authoritative birthdays; other scopes and application tables
remain untouched. There is no partial rollback or public migration/rebuild
command.

## Submission reconciliation reads

The accepted-but-unimplemented native SOL submission coordinator receives the
same scope's existing `Checkpoint` and `History` read capabilities plus an
application-published checkpoint-advance notification. These are narrow reads
over canonical indexing state. Indexing stores no outgoing operation, request
identity, source lease, signed envelope, submission attempt, or reconciliation
status for the coordinator.

A valid non-null signature status or canonical history containing the locally
derived signature proves that the exact transaction was observed/submitted,
including when it carries an execution error. Ordinary confirmation, execution
failure, and reorg presentation still come only from canonical indexed history;
an RPC status does not create a parallel confirmation system.

Absence is terminal only after the recent blockhash has expired and the finalized
index has complete, unpruned coverage through its last valid block height. The
coordinator must then exhaust the active fee-payer's history in pages of at most
100 against one unchanged checkpoint and find no matching signature. A cursor
conflict, checkpoint movement, reorg, page error, pruning, indexing gap, or
incomplete traversal discards that proof. An indexing or history outage never
releases the source; it remains guarded until valid evidence becomes available.

## Transactions and pagination

Canonical storage records inclusion or failure at one complete block reference.
Confirmation is a depth-only query result derived from inclusion produced
height and the returned page checkpoint's produced height. `Confirmed` reports
that observed count; the current surface does not claim chain-finality proof.
Confirmation is not persisted as evolving state.

Every history cursor contains its checkpoint and last transaction position. A
later page is valid only against that same canonical checkpoint; otherwise the
consumer receives a conflict and restarts at page one. Output cursors follow
the same snapshot rule.

A Bitcoin transaction with three inputs and two outputs exposes five
movements. It is never reduced to a fictional sender/recipient pair. All values
use exact `Decimal`/atomic integer semantics and never floating point.

## Required evidence

Deterministic tests cover:

- one-chain `Service` and multi-chain `Composer` through `dyn Indexer`;
- filter validation, partitioning, birthdays, empty scopes, and bounded sync;
- no-address tip anchoring and birthday anchoring without a genesis scan;
- sparse native positions without synthesized blocks and restart from a
  complete position/height/hash/parent checkpoint;
- dense Bitcoin and Ethereum regression behavior with `position == height`;
- parent-position/hash atomicity and exact produced-height increments;
- duplicate add and compare-and-swap conflict;
- atomic history/output/checkpoint/journal commit;
- one- and multi-block retained reorg;
- orphan history deletion and spent-output restoration;
- `ReorgTooDeep`;
- checkpoint-bound history and output pagination;
- Bitcoin multi-input/multi-output movements;
- Ethereum native and token movements;
- one shared PostgreSQL pool/schema with Bitcoin, Ethereum, and Solana scope
  isolation and native/token asset coexistence;
- PostgreSQL migration/backfill from the height-only baseline while preserving
  existing scope facts and a sentinel `payment_wallets` row byte-for-byte;
- an exact-scope rescan that leaves every unrelated scope and application-owned
  row unchanged;
- positive native SOL reconciliation from canonical signature history;
- blockhash expiry plus exhaustive checkpoint-stable absence proof;
- invalidation of an absence scan by checkpoint movement, cursor conflict, or
  reorg;
- indefinite source guarding while status or history evidence is unavailable;
- no outgoing-operation, source-lease, or exact-envelope persistence in either
  PostgreSQL or redb; and
- RPC outage without false terminal state.

Repository contract tests exercise both PostgreSQL and redb. Target application
system tests compose PostgreSQL through one process-wide pool; current redb
system coverage does not by itself satisfy that composition requirement.
