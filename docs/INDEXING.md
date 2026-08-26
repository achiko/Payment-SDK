# Indexing

## Purpose

Indexing turns chain-native blocks into canonical transaction history and the
live output projection needed by UTXO wallets. It runs inside `apps/api`; it is
not a service, HTTP API, wallet registry, or storage engine.

The public indexing domain answers:

1. which `(height, hash)` a chain/network scope has processed;
2. which complete canonical transactions affect an address;
3. what confirmation state those transactions have at a checkpoint; and
4. which indexed outputs are currently live when a chain needs that projection.

It does not provide watches, event feeds, raw-block archives, migration or
rebuild commands, or durable pending-transaction state.

## Object graph

```text
chain RPC client
    -> chain BlockSource
    -> chain-native block
    -> chain BlockInterpreter + active addresses
    -> InterpretedBlock
    -> chain Service implementing Indexer
    -> Blocks / Transactions / Outputs
    -> indexing redb Repository

Bitcoin Service ----\
                     -> Composer implementing the same Indexer contract
Ethereum Service ---/

Wallets::filters() -> Indexer::sync(filters)
HTTP/wallet reads  -> Checkpoint / History / optional Outputs
```

`Service` binds one chain source, interpreter, repository, scope, and
synchronization policy. `Composer` combines disjoint scopes and routes by
`IndexScope`. A caller uses the same `Indexer` contract for either object.

## Caller contract

`Indexer` combines the two general query capabilities and synchronization:

```rust,ignore
pub trait Indexer: Checkpoint + History {
    fn scopes(&self) -> &[IndexScope];

    fn sync<'a>(
        &'a self,
        filters: Vec<AddressFilter>,
    ) -> BoxFuture<'a, Result<Vec<SyncStatus>, IndexError>>;
}
```

`AddressFilter` is one canonical address plus its birthday height. `Wallets`
owns and deduplicates the authoritative address set. Every sync receives a
complete snapshot; indexing stores no watch or internal address registry.

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

## Chain producer contracts

`BlockSource` exposes only canonical block reads needed by synchronization:
source tip, a native block at a height, and canonical hash lookup. Concrete RPC
methods and DTOs stay in the chain crate.

`BlockInterpreter` receives one native block and the canonical addresses active
at that height. It returns `InterpretedBlock` with:

- the unchanged `BlockRef`;
- complete transaction drafts and stable movements; and
- `OutputChanges` for live UTXO state.

Bitcoin represents each input and output separately. Ethereum represents
native value and token log movements with distinct assets. Interpreters do not
create persistence keys, journals, rows, batches, or codecs.

## Persistence collections

Storage-independent persistence has three clear nouns:

| Collection | Operations | Meaning |
|---|---|---|
| `Blocks` | `get`, `add`, `remove` | canonical checkpoint/block lookup and atomic block lifecycle |
| `Transactions` | `list` | address-primary canonical history |
| `Outputs` | `list` | current live outputs for one address |

`Blocks::get` accepts `BlockSelector::Tip(scope)` or
`BlockSelector::Height { scope, height }`. Height lookup is limited to retained
canonical journal coverage used during reorg reconciliation.

`BlockAddition::new` receives the scope, expected checkpoint, retention, and
`InterpretedBlock`. It validates the parent connection, duplicate transaction
IDs, scoped movements, amounts, and output changes before storage is called.

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

## Durable state

For each exact chain/network scope, the repository stores only:

- one canonical checkpoint with height and hash;
- complete canonical transactions keyed beneath each affected selected
  address;
- current live outputs; and
- one bounded rollback-journal entry per retained canonical block.

Copying a transaction beneath each affected address makes history a primary
ordered scan. It intentionally avoids a second address index. Journal records
are physical storage details and never cross the repository boundary.

Ethereum address history retains every canonical native and Transfer-shaped
token fact for the watched address. Asset-specific wallet presentation is a
concrete Ethereum-wallet projection over that checkpoint-bound page; it does
not add an asset index, token catalog, or alternate persistence record.

The repository does not store address filters, watch IDs, synchronizer phase,
confirmations, observation revisions, pending-confirmation records, spent
markers, an event log, or raw blocks.

## Initial coverage and birthdays

Composition registers all configured imported wallets before the first sync
and passes the complete filter snapshot on every later sync.

For a fresh scope:

- no filters: establish the observed tip as an empty anchor;
- earliest birthday `B > 0`: establish `B - 1` as the parent anchor, then
  interpret `B..=tip`; and
- birthday `0`: interpret from block zero.

The anchor gives the first interpreted block a verified parent without reading
irrelevant history. At height `H`, only filters with `start_height <= H` are
active. A generated wallet starts after the current checkpoint, so existing
history remains complete without backfill.

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
2. verify the stored `(height, hash)` remains canonical;
3. reconcile a retained reorg when it does not;
4. choose the next height, bounded by the requested batch size;
5. select filters active at that height;
6. fetch and interpret the native block;
7. verify its canonical hash again immediately before persistence; and
8. call `Blocks::add` with the expected checkpoint.

A restart resumes at `checkpoint + 1`. An RPC failure is retryable/unknown; it
does not alter canonical state or prove a submitted transaction was dropped.

## Reorg reconciliation

When the stored tip is no longer canonical, the synchronizer walks retained
local block references and remote canonical hashes toward their common
ancestor. It then calls `Blocks::remove` for each orphan tip. Each removal
atomically deletes orphan history, restores output state, and moves the
checkpoint. The replacement branch is added through the ordinary forward path.

If no common ancestor exists within journal retention, synchronization returns
`ReorgTooDeep`. The scope must be recreated and rescanned from the authoritative
birthdays. There is no partial rollback or public migration/rebuild command.

## Transactions and pagination

Canonical storage records inclusion or failure at one block. Confirmation is a
depth-only query result derived from inclusion height and the returned page
checkpoint. `Confirmed` reports that observed count; the current surface does
not claim chain-finality proof. Confirmation is not persisted as evolving
state.

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
- restart from a height-and-hash checkpoint;
- duplicate add and compare-and-swap conflict;
- atomic history/output/checkpoint/journal commit;
- one- and multi-block retained reorg;
- orphan history deletion and spent-output restoration;
- `ReorgTooDeep`;
- checkpoint-bound history and output pagination;
- Bitcoin multi-input/multi-output movements;
- Ethereum native and token movements; and
- RPC outage without false terminal state.
