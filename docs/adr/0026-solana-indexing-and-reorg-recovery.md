# ADR-0026: Solana Indexing and Reorg Recovery

## Status

Proposed

## Date

2026-08-27

## Context

The current generic synchronizer assumes a dense height coordinate. Solana RPC
uses sparse slots, while its produced block height remains the contiguous value
needed for confirmation and retention. The accepted block-position ADR defines
the generic replacement but is not implemented. The remaining decision is the
exact finalized RPC flow and its checkpoint, pruning, restart, and reorg rules.

## Decision

Implement the accepted `BlockPosition`/`BlockHeight` separation across generic
indexing and both repositories before adding the Solana source. There is no
Solana-specific synchronizer.

### Selection ownership prerequisite

Remove the out-of-contract `indexing::Registry` and `RegisteredAddress`
surface, the PostgreSQL registry module/queries, and the optional registry
parameter plus `Wallets::adopt`/`restore` coupling. Indexing persists no watch,
address selection, wallet identity, or secret bytes. Runtime-generated wallets
remain process-local; configured historical imports remain startup-only. Any
future durable wallet registry belongs to the wallet/application boundary and
requires a separately approved encrypted-custody design.

`sdk/indexing` owns one concrete checkpoint/revision coordinator and its commit
and publication permits because the protected commit occurs in the generic
synchronizer. `apps/api` constructs one coordinator per `IndexScope` after
loading the persisted checkpoint, injects its commit side into that scope's
`Service`, and gives the matching publication handle to each registered wallet
family in the scope. `sdk/wallets` consumes that handle without indexing
knowing a wallet type. `sdk/indexing/runtime` remains only the loop driver and
owns no coordination state. The public `Indexer`/`FilterSource` shape remains
chain-neutral; the synchronizer captures the filter vector and revision
atomically through its coordinator before planning.

The Solana source uses one configured endpoint and finalized commitment only.
`getSlot(finalized) = T` is a traversal upper bound, not a promise that slot
`T` is a retrievable produced block. Every tip or batch attempt also samples
`getFirstAvailableBlock() = A0` before enumeration and `A1` after all selected
blocks are fetched but before a plan may commit.

Every tip or range attempt has three private resource bounds: a 30-second
monotonic deadline, at most 64 enumeration RPC calls, and at most 500,000
numeric positions in one enumeration call. The ordinary private window target
is 10,000 positions. Every RPC future is raced against both the cancellation
signal and the remaining monotonic attempt deadline, even when the configured
per-call timeout is longer; cancellation is also checked on every loop edge.
Deadline, call-budget, arithmetic, or cancellation exhaustion discards all
fetched facts, publishes no tip, and permits no checkpoint commit; deadline or
call-budget exhaustion is a retryable source error. These are SDK resource
guards, not public configuration or chain-freshness controls.

### Complete produced tip

The source derives the latest complete produced reference at or below `T` only
with explicit closed `getBlocks` windows. Starting with `end = T`, it computes
`start = max(A0, end.saturating_sub(9_999))` and calls:

```text
getBlocks(start, end, finalized, minContextSlot = T)
```

The response must be strictly increasing, unique, and wholly inside the sent
inclusive range. If it is non-empty, its last slot is the greatest produced
slot in the highest remaining window. If it is empty and `start > A0`, the
next lower window ends at checked `start - 1`. An empty response with
`start == A0` proves there is no retained candidate and fails the attempt.
`A0 > T` is inconsistent and also fails the attempt. The greatest retained
candidate is fetched with:

```text
getBlock(candidate, finalized/full/json/version 0/no rewards)
```

The source returns a tip only after that complete block passes every native
check. The checked window step is traversal only; no numeric slot is treated as
a parent, and `T` itself is never fabricated as a produced reference. If no
candidate can be proved inside the retained range, or the enumerated candidate
becomes unavailable, the attempt fails without publishing a tip.

### Bounded sparse range fetch

For a generic inclusive range `[start, tip.position]` and positive returned-
block limit `L`, a request with `start > tip.position` returns an empty generic
range without enumeration. Otherwise the source keeps `collected`, a numeric
`cursor`, and `remaining = L - collected.len()`. It chooses
`window = min(max(remaining, 10_000), 500_000)` and an inclusive end that
covers at most that many numeric positions without exceeding `tip.position`,
then calls:

```text
getBlocks(cursor, end, finalized, minContextSlot = tip.position)
```

Each response can contain no more produced slots than numeric positions in its
window. It must be strictly increasing, unique, and wholly inside the exact
sent range. The source appends only the earliest `remaining` returned slots in
order; any later returned slots are validated but left for the next sync. If
fewer than `L` have been collected and `end < tip.position`, the numeric cursor
advances to checked `end + 1` even when the response was empty or short; the
explicit closed range has proved those omitted numbers are skipped slots.
Enumeration stops at `L` slots or after covering the tip. If the final window
covers the tip before `L` slots have been collected, its response must end with
the independently proved `tip.position`; omission of that known produced slot
is inconsistent. The next sync resumes from the checked successor of the last
committed native position. Skipped slots are omitted and never synthesized.

`getBlocksWithLimit` is intentionally not used. In pinned Agave v3.1.14 its
BigTable branch can return before enforcing `minContextSlot`, while finalized
`getBlocks` enforces that floor before selecting local blockstore or BigTable.
The singular URL may still hide a load balancer, so canonical agreement among
its physical backends remains the explicit operator trust assumption recorded
by the runtime-composition proposal.

Every selected slot is then fetched with the exact full `getBlock` configuration
above. The response is explicitly paired with its requested slot and must
contain canonical blockhash, previousBlockhash, actual `parentSlot`, and a
non-null RPC `blockHeight`. Null block height is valid in the RPC schema but is
an incomplete provider response for this SDK. `blockTime` remains optional.
For a non-genesis child, the source/generic boundary requires:

- child slot greater than checkpoint slot;
- child block height exactly checkpoint block height plus one; and
- exact parent slot and hash equality with the checkpoint.

An enumerated slot whose block becomes unavailable is retryable and advances no
checkpoint. Cleaned-up history, unavailable transaction history, or a birthday,
anchor, or checkpoint below the retained lower bound is an explicit provider-
capability failure; the service does not skip, fabricate, or silently reset.
Wrong genesis is a startup identity failure.

Pruning may advance between `A0` and enumeration. Before returning a plan, the
closing `A1` must still be no greater than the oldest requested start, actual
anchor, or checkpoint required to prove that plan. Otherwise the entire plan is
discarded as a provider-capability failure. This prevents a newly pruned slot
from being mistaken for an ordinary skipped slot.

### Canonical lookup and reorg evidence

Canonical lookup and reconciliation use the stored native slot `S`. A complete
different hash or parent is a canonical mismatch and enters the generic bounded
rollback path. Null, pruned, incomplete, or temporarily unavailable `getBlock`
data alone is not reorg evidence.

A replacement finalized fork may omit `S` entirely. The proof order is exact:

```text
getSlot(finalized) = T where T > S
getFirstAvailableBlock() = A0 where A0 <= S
getBlocks(S, S, finalized, minContextSlot = T) = []
getFirstAvailableBlock() = A1 where A1 <= S
```

Only that complete pruning sandwich makes an empty exact-range result positive
canonical-omission evidence and therefore a mismatch. A result containing `S`
followed by unavailable `getBlock(S)`, either lower bound above `S`, any failed
witness, or an attempt with `T <= S` remains unavailable rather than false
reorg evidence. Rollback restores the journaled previous checkpoint atomically
and never subtracts one from a slot.

The accepted Solana interpreter remains all-or-nothing for the block: legacy
and version-0 transactions, loaded addresses, top-level and inner System
transfers, balance completeness, actual fee, and failed-transaction behavior.
`maxSupportedTransactionVersion: 0` is fail-closed. An unsupported version
fails the entire block with no partial history or checkpoint movement. Support
for transaction version 1 is a release gate before cluster activation; the
value must not be raised without decoder and fixture coverage.

Produced block height continues to own confirmations, journal keys, history
ordering, and retention. Slot owns RPC traversal, birthdays, canonical lookup,
and restart. Redb and PostgreSQL records store both coordinates and atomic
parent position/hash; old pre-release records and cursors are rejected and
rescanned without compatibility readers.

### Dynamic wallet correctness

The generic runtime must close the existing wallet-registration race before
Solana can claim complete history. One coordinator per `IndexScope` keeps an
in-memory persisted-checkpoint snapshot, filter revision, and commit-in-progress
flag. It owns no database records or watch registry.

RPC fetching and interpretation happen without a coordination lock. A sync plan
captures the checkpoint and filter revision. Immediately before `Blocks::add`,
it acquires a short lock, verifies both still match, marks one commit in
progress, and releases the lock. The resulting commit permit—not a held lock—
survives the asynchronous repository call. On success it publishes the new
persisted checkpoint snapshot and clears the flag. An error or dropped permit
after I/O starts marks the snapshot recovery-required; publication and another
commit remain closed until an out-of-lock repository reload reestablishes the
persisted checkpoint. A changed revision discards the plan and retries from the
unchanged checkpoint.

Runtime generation may create key/address material before admission, but
publication waits asynchronously while a commit is in progress without holding
a lock. Under one short critical section it reads the latest persisted
checkpoint snapshot, assigns the checked successor native position as birthday
(or zero with no checkpoint), inserts the wallet/filter, and increments the
revision. A sync plan therefore either includes the new filter or commits first
and forces the wallet to start after that commit. Startup-only historical import
remains exclusive and publishes its explicit position before sync begins;
lowering a birthday after progress requires scope recreation and rescan.

## Consequences

- Skipped slots cannot corrupt checkpoints, confirmations, or retention.
- Restart and reorg lookup retain the native slot required by RPC.
- Provider pruning and future transaction versions fail visibly instead of
  creating incomplete wallet history.
- The prerequisite touches Bitcoin, Ethereum, both repositories, cursors, and
  public block representations before Solana source work begins.
- Same-slot replacements and replacement forks that omit an old produced slot
  both enter bounded rollback, even though finalized reorgs should be rare.

## Alternatives considered

### Use slot as generic block height

Rejected by the accepted block-position decision; gaps would inflate
confirmation and break parent traversal.

### Scan every numeric slot with `getBlock`

Rejected. Skipped slots are normal, and null/unavailable has different meaning
from an independently enumerated omission.

### Use `getSignaturesForAddress` as canonical history

Rejected. It cannot supply one block-atomic, complete multi-address history and
would bypass generic checkpoint/reorg persistence.

## Validation requirements

Generic tests must cover sparse positions, actual parent jumps, produced-height
continuity, skipped-slot birthdays, the checkpoint-read/commit/filter-publish
race in both possible orders, commit cancellation recovery, startup import,
restart, retained reorgs, deep reorg failure, confirmations, both repository
round trips, removal of the indexing registry/secret persistence surface, and
old record/cursor rejection. The missing
`sdk/indexing/postgres/migrations/0001_init.sql` fixture must be restored or
replaced before PostgreSQL can satisfy this gate. Solana tests must cover
backward tip windows, count-bounded forward windows, the 500,000-position
boundary, empty and short skipped-slot windows, prefix resume, strict in-range
slot mapping, known-tip omission, false-high `T`, a huge empty gap, enumeration
call-budget and deadline exhaustion, in-flight deadline and cancellation races,
cancellation at every loop edge, pruning between the two lower-bound witnesses,
null/cleaned/unavailable blocks, wrong parent, null block height, legacy/v0
completeness, unsupported versions, no partial commit, same-slot canonical
mismatch, replacement-fork omission of a stored slot, and rejection of a
`getBlocksWithLimit` implementation shortcut.

## Approval boundary

This proposal consolidates finalized traversal, pruning, checkpoint,
persistence, restart, reorg, transaction-version, and dynamic-filter rules. It
does not authorize implementation. Acceptance requires reconciling the current
height-only and stale registry descriptions in `ARCHITECTURE.md`,
`docs/CONTRACTS.md`, `docs/INDEXING.md`, and `docs/SYSTEM_REQUIREMENTS.md` with
accepted ADR-0003 and this coordination contract before source changes begin.
The source-level registry removal and PostgreSQL schema repair are explicit
prerequisites, not documentation-only cleanup.

## References

- [Solana `getSlot`](https://solana.com/docs/rpc/http/getslot)
- [Solana `getFirstAvailableBlock`](https://solana.com/docs/rpc/http/getfirstavailableblock)
- [Solana `getBlocks`](https://solana.com/docs/rpc/http/getblocks)
- [Solana `getBlocksWithLimit`](https://solana.com/docs/rpc/http/getblockswithlimit)
- [Solana `getBlock`](https://solana.com/docs/rpc/http/getblock)
- [Solana versioned transactions](https://solana.com/docs/core/transactions/versioned-transactions)
- [Pinned Agave `getBlocks` floor enforcement](https://github.com/anza-xyz/agave/blob/3134055b562e95902233be308453fffa1c4a8902/rpc/src/rpc.rs#L1403-L1467)
- [Pinned Agave `getBlocksWithLimit` BigTable early return](https://github.com/anza-xyz/agave/blob/3134055b562e95902233be308453fffa1c4a8902/rpc/src/rpc.rs#L1498-L1545)
- `docs/adr/0003-separate-native-block-position-from-produced-block-height.md`
- `docs/adr/0004-derive-native-sol-history-from-system-transfers.md`
- `sdk/indexing/src/synchronizer.rs`
- `sdk/indexing/redb/src/record.rs`
- `sdk/indexing/postgres/src/row.rs`
