# ADR-0003: Separate native block position from produced-block height

## Status

Accepted

## Date

2026-08-27

## Context

The indexing model currently uses `BlockHeight` for several different jobs:

- the coordinate passed to a chain RPC;
- the checkpoint and canonical-chain lookup coordinate;
- the next-block calculation;
- an address birthday;
- history ordering, rollback retention, and confirmation depth.

Those meanings coincide for Bitcoin block height and Ethereum block number.
They do not coincide for native Solana. Solana RPC addresses blocks by slot,
slots may be skipped without producing a block, and a skipped slot does not
increment Solana block height. A returned block therefore has both a native
slot and a produced-block height.

Treating a Solana slot as the existing `BlockHeight` would make the generic
synchronizer request skipped slots, overstate confirmation depth, measure
rollback retention in elapsed slots instead of produced blocks, and attempt to
find parents by subtracting one from a coordinate that is not contiguous.

The generic indexing lifecycle should still own checkpoints, birthdays,
catch-up, canonicality checks, retained reorgs, and persistence. Solana-specific
RPC fields and skipped-slot enumeration must remain in the Solana chain crate.

## Decision

Introduce two chain-neutral numeric concepts with distinct meanings:

| Concept | Meaning | Bitcoin | Ethereum | Solana |
|---|---|---|---|---|
| `BlockPosition` | Native monotonic RPC traversal coordinate | block height | block number | slot |
| `BlockHeight` | Number of produced blocks through this block | block height | block number | RPC `blockHeight` |

`BlockRef` will carry both values. Its optional parent will be one atomic value
that pairs the parent position and parent hash, rather than two independent
optional fields:

```rust,ignore
struct BlockRef {
    position: BlockPosition,
    height: BlockHeight,
    hash: BlockHash,
    parent: Option<BlockParent>,
    timestamp: Option<u64>,
}

struct BlockParent {
    position: BlockPosition,
    hash: BlockHash,
}
```

The exact field and type names remain an implementation detail, but the atomic
parent invariant does not. Only a genesis block may have no parent.

`BlockHeight` remains required for every block offered to indexing. Solana RPC
may return a null `blockHeight`; the Solana source must treat that as an
incomplete, retryable source response. It must not substitute the slot, derive
a local counter, or advance the checkpoint.

### Source contract

`BlockSource` will retain no more than three tightly coupled operations. The
contract will provide:

1. the latest complete produced-block reference;
2. a bounded fetch of actual produced blocks within an inclusive native
   position range; and
3. the current canonical reference at one native position.

The bounded fetch returns blocks in strictly increasing, unique position order
and omits positions at which no block was produced. Its limit counts returned
blocks, not the numeric distance between positions. A concrete source may
chunk stricter provider range limits internally. Exact Rust method names and
return containers are deferred to the implementation step.

Bitcoin and Ethereum sources return contiguous positions. A Solana source
enumerates produced slots using native RPC behavior and fetches blocks by slot.
The generic synchronizer never synthesizes a block for a skipped slot.

### Synchronization and birthdays

Native position will drive:

- source traversal and catch-up targets;
- readiness comparisons;
- canonical lookups and restart resumption;
- address-filter activation and wallet birthdays; and
- the native coordinate recorded in public block references.

For every non-genesis commit, the synchronizer must validate all of these
conditions before interpreting or persisting the block:

1. the block position is strictly greater than the checkpoint position;
2. the block height equals the checkpoint height plus one; and
3. the block's parent position and hash equal the checkpoint position and
   hash.

A generated wallet starts at the checked successor of the current checkpoint
position. This is a lower bound: if that position is skipped, the wallet becomes
active at the first produced block after it. Imported wallets use an explicit
`start_position` under the same rule.

Fresh indexing must not manufacture `start_position - 1` as an anchor. For a
birthday within the observed chain, the source locates the first produced block
at or after that birthday and indexing uses that block's actual parent as the
empty-address anchor before processing the block. Birthday zero starts at
genesis. If there are no filters, or every birthday is beyond the current tip,
the scope may anchor at the actual produced tip without inspecting addresses.

### History, confirmations, retention, and reorgs

Produced-block height will continue to drive:

- confirmation arithmetic;
- canonical history ordering and history positions;
- live-output creation height;
- rollback journal keys; and
- retention measured as a count of committed blocks.

This is safe because every committed child must increment its parent's height
by exactly one even when native positions have gaps.

Reconciliation may walk retained local records by produced height, but it must
query the remote chain using each stored block's native position. Rollback uses
the previous checkpoint stored in the journal. It must never subtract one from
a native position to invent a parent slot.

### Persistence and public boundaries

Every persisted `BlockRef`, including checkpoints, journal entries,
transaction status, and previous checkpoints, will store position and the
parent position in addition to the existing height and hashes. Existing
height-based keys and ordering may remain.

The redb record shape and PostgreSQL schema will be replaced directly. Existing
pre-release index data must be recreated and rescanned; no legacy decoder,
migration reader, compatibility alias, or inferred `position = height` fallback
will be added. The PostgreSQL repository tests currently reference a missing
`migrations/0001_init.sql`; that schema source must be restored or replaced
before PostgreSQL implementation validation can be complete.

HTTP block representations and opaque checkpoint-bearing cursors will carry
both position and height, including parent position where a parent exists.
Pre-release cursors encoded with the old height-only shape will be rejected
rather than guessed. Exact endpoint field changes are deferred to application
integration planning.

## Consequences

### Positive

- Solana slots remain native RPC coordinates without corrupting confirmation
  or retention semantics.
- Skipped slots do not become fictional blocks, checkpoints, or history.
- Bitcoin and Ethereum preserve their existing behavior with
  `position == height`.
- One generic synchronization and reorg lifecycle continues to serve every
  concrete chain.
- Stored checkpoints contain the native coordinate required for deterministic
  restart and canonical lookup.

### Negative

- The change is coordinated and breaking across base values, source adapters,
  synchronization, wallet birthdays, both repositories, and HTTP block data.
- Existing pre-release index databases and checkpoint cursors become invalid
  and require recreation or rejection.
- The Solana source must enumerate sparse produced slots and respect provider
  range limits while presenting one bounded generic result.
- A Solana block whose RPC response omits `blockHeight` cannot be committed
  until a complete response is available.

### Neutral

- History and journal keys remain height-based; block records gain enough
  native identity for remote lookup.
- `batch_size` continues to count actual blocks applied in one pass.
- No watch registry, raw-block archive, pending-confirmation state, or
  chain-specific synchronizer is introduced.

## Alternatives considered

### Treat Solana slot as `BlockHeight`

Rejected. Slot gaps would break contiguous traversal, inflate confirmations
and retention, and make numeric predecessor lookup disagree with Solana
ancestry.

### Use Solana block height as its RPC lookup coordinate

Rejected. Solana block RPC is slot-addressed and exposes `parentSlot` as the
native parent coordinate. Persisting only block height would not provide the
coordinate required to fetch or revalidate the same block after restart.

### Synthesize empty blocks for skipped slots

Rejected. A skipped slot produced no block and is not an ancestor. Synthetic
entries would create false checkpoints, confirmations, and rollback state.

### Hide slot-to-height mapping inside only the Solana source

Rejected. Restart, canonical lookup, parent validation, birthdays, and reorg
reconciliation all require the durable native slot, so the distinction cannot
remain an adapter-local implementation detail.

### Add a Solana-specific synchronizer

Rejected. It would duplicate generic checkpoint, address-selection,
persistence, reorg, readiness, and task-lifecycle policy.

### Key every history and persistence record by native position

Rejected for current scope. Native position is necessary on block records and
for RPC traversal, but produced height already supplies contiguous ordering,
confirmation depth, and retention counts. Re-keying every projection would
broaden the change without improving those semantics.

### Make produced-block height optional

Rejected. Exact confirmation and retention behavior require it. An incomplete
RPC response is a source failure, not permission to persist ambiguous state.

### Use a chain enum or opaque string for native position

Rejected. Every supported native coordinate is a checked `u64`. A chain enum
would couple the generic base crate to concrete chains, while an opaque string
would weaken ordering and boundary validation.

## Failure modes and required validation

- Sparse positions such as slots `100`, `103`, and `107` must synchronize in
  order without fetching or storing skipped slots.
- A block at slot `103`, height `51`, whose parent is slot `100`, height `50`,
  must connect successfully.
- A wrong parent position, wrong parent hash, repeated height, skipped produced
  height, repeated position, or decreasing position must fail before commit.
- A null Solana `blockHeight` must return a retryable source error and preserve
  the checkpoint.
- Source batches must be bounded, ordered, unique, and contain only actual
  produced blocks within the requested range.
- A birthday on a skipped slot must activate at the next produced slot, using
  that block's actual parent as the initial anchor.
- Restart must resume after the persisted native position without replaying an
  already committed block.
- Canonical mismatch and retained reorg tests must query stored positions and
  restore stored previous checkpoints rather than decrementing slots.
- Rollback retention and `batch_size` must count produced blocks, not position
  distance.
- Confirmation tests must use produced height, so slot gaps add exactly one
  confirmation per produced descendant.
- redb and PostgreSQL contract tests must round-trip both coordinates and
  reject the old storage shape clearly.
- HTTP block and cursor tests must round-trip position and height and reject
  old height-only cursors.
- Bitcoin and Ethereum regression tests must prove `position == height`,
  contiguous behavior, unchanged confirmation results, and unchanged history
  ordering.

## Approval boundary

Decision `S1.4` was explicitly approved on 2026-08-27. Acceptance records the
architecture decision only; it does not authorize source, synchronization,
persistence, schema, cursor, or API implementation changes.

## References

- [Solana `getBlock`](https://solana.com/docs/rpc/http/getblock)
- [Solana `getBlocks`](https://solana.com/docs/rpc/http/getblocks)
- [Solana terminology](https://github.com/solana-foundation/solana-com/blob/main/apps/docs/content/docs/en/references/terminology.mdx)
- `sdk/chains/base/src/block.rs`
- `sdk/indexing/src/source.rs`
- `sdk/indexing/src/synchronizer.rs`
- `sdk/indexing/src/block.rs`
- `sdk/indexing/src/indexer.rs`
- `sdk/indexing/src/service.rs`
- `sdk/indexing/redb/src/record.rs`
- `sdk/indexing/postgres/src/row.rs`
- `sdk/indexing/postgres/src/write.rs`
- `sdk/indexing/postgres/src/registry.rs`
- `sdk/indexing/postgres/tests/repository_contract.rs`
- `sdk/wallets/src/wallets.rs`
- `apps/api/src/api/transaction.rs`
- `apps/api/src/api/cursor.rs`
- `docs/SYSTEM_REQUIREMENTS.md`
- `docs/INDEXING.md`
