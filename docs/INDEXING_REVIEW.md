# Indexing design review

## Decision

Keep indexing as one storage-independent SDK domain. A chain `Service` and the
multi-chain `Composer` implement the same `Indexer` contract. The caller owns
the complete address/birthday snapshot and supplies it to every sync.

Use `Blocks`, `Transactions`, and `Outputs` as the persistence contracts.
`Blocks::add` and `Blocks::remove` own the atomic canonical block lifecycle;
transaction history and live outputs are read projections. The redb adapter
owns all physical records, keys, journals, compare-and-swap checks, and atomic
batches.

`apps/api` embeds the indexers, owns their tasks, and shares one composed object
with wallet and HTTP consumers through narrow trait views.

## Vocabulary

| Name | Meaning |
|---|---|
| `AddressFilter` | canonical address and first relevant block height supplied by the caller |
| `BlockSource` | reads canonical chain-native blocks from one chain's RPC client |
| `BlockInterpreter` | turns one native block and active addresses into domain facts |
| `InterpretedBlock` | block identity, complete transaction drafts, and output changes |
| `Blocks` | canonical block lookup and atomic add/remove persistence |
| `Transactions` | canonical address-primary history reads |
| `Outputs` | current live-output reads needed by UTXO consumers |
| `Service` | one scope's source, interpreter, repository, and sync policy |
| `Indexer` | reusable checkpoint, history, and synchronization surface |
| `Composer` | scope router implementing the same surface for several indexers |
| `Repository` | physical indexing persistence implementation |

## Why collections replace persistence phases

Loading context, constructing a public plan, and applying that plan exposed
storage-owned state and allowed callers to author invalid commit or undo data.
Those phases did not describe independent domain capabilities.

The collection boundary makes the invariant direct:

- `Blocks::get` reads the current tip or a retained block;
- `Blocks::add(BlockAddition)` validates the expected checkpoint and commits
  history, outputs, journal, and checkpoint together;
- `Blocks::remove(scope, expected_tip)` derives its inverse from the private
  stored journal and applies it atomically;
- `Transactions::list` reads canonical history; and
- `Outputs::list` reads the current live output projection.

No public context, plan, undo, or storage record is necessary. Another
persistence backend implements these domain operations without changing a
chain interpreter or consumer.

## Address ownership

Indexing does not persist watches or own an address registry. `Wallets` owns
and deduplicates wallet addresses and birthdays. The sync task passes
`Wallets::filters()` to `Indexer::sync`, and `Composer` partitions that complete
snapshot by scope before invoking children.

This design has an explicit coverage rule: a checkpoint is valid for the
historical filter set used to produce it. A late filter at or below that
checkpoint requires recreating and rescanning the scope. Import requires
exclusive startup access, and runtime generation is forward-only. Across
restarts, the embedding application must reload the same authoritative
imported-wallet set before synchronization; selection drift cannot be detected
because filters are intentionally not stored.

## Durable minimum

Persist only:

- canonical checkpoint `(height, hash)`;
- address-primary canonical transaction history;
- current live outputs; and
- bounded rollback journal.

Derive confirmation at read time. Keep synchronization phase and readiness in
memory. Do not persist filters, confirmations, observation revisions, pending
state, spent markers, secondary address indexes, raw blocks, or event feeds.

## Rejected structures

- a separate indexer process, HTTP client, or SDK transport;
- a storage-owned runtime or synchronizer handle;
- independent load/plan/apply persistence traits;
- caller-supplied commit or rollback state;
- chain code producing physical storage keys;
- an internal address/watch registry in indexing;
- forcing UTXO `Outputs` onto every `Indexer`;
- a universal chain RPC or native transaction representation;
- mutable confirmation records or revision history;
- raw-block archives, backfill/rebuild commands, and migrations.

## Review checks

- The same behavior is available through a one-chain `Service` and `Composer`.
- Every filter and query is validated against its exact chain/network scope.
- A fresh scan begins at the earliest birthday, not genesis by default.
- Commit exposes either all history/output/journal/checkpoint changes or none.
- Remove uses only storage-owned journal data.
- Retained reorgs remove orphan truth and restore live outputs.
- `ReorgTooDeep` fails clearly and requires a full scope rescan.
- Physical encoding stays private to the persistence adapter.
- HTTP and wallets receive narrow abstractions, not repositories or
  synchronizers.
- Tests prove restart, reorg, pagination, and chain deletion behavior.
