# P1 : Runtime Wallet Birthday Is Unsafe Across Reorgs

This document is an implementation brief for the coding agent fixing the
runtime-wallet reorg issue identified while reviewing PR #8 at commit
`1c6db91f5161f8f8b194362d7c43bc52482df489`.

The selected solution is deliberately conservative: once a runtime wallet is
registered, its address must be active for every block the synchronizer
processes or reprocesses. The persisted checkpoint, not the filter birthday,
continues to prevent ordinary historical backfill.

This issue is separate from `docs/reviews/P1.md`. P1 prevents wallet creation
from racing with a synchronization batch. P1.1 prevents a later reorg from
moving block processing below the runtime wallet's numeric birthday. Both fixes
are required.

## 1. Problem

### 1.1 Required invariant

The implementation must guarantee:

> After a runtime wallet becomes visible to the caller, every canonical block
> processed or reprocessed from that point onward is interpreted with the
> wallet's address active.

The current implementation guarantees only:

> The wallet is active when the processed block height is greater than or equal
> to the numeric birthday recorded when the wallet was created.

Those statements are not equivalent when a blockchain reorg replaces an
already indexed height with a newly mined block.

### 1.2 Current model

`sdk/indexing/src/service.rs` models one watched address as:

```rust,ignore
pub struct AddressFilter {
    pub address: CanonicalAddress,
    pub start_height: BlockHeight,
}
```

For runtime generation and adoption, `Wallets::activate` receives no explicit
start height. It reads the current persisted checkpoint and calculates:

```rust,ignore
start_height = checkpoint.height + 1
```

Both of these paths use that calculation:

- `Wallets::generate`
- `Wallets::adopt`

Explicit startup `Wallets::import` is different: its caller deliberately
supplies a historical start height. Do not change that semantic accidentally.

During synchronization, the address is active only when:

```rust,ignore
filter.start_height <= block_height
```

The filter therefore remembers only a height. It does not remember that the
wallet was created after one particular block hash at that height.

### 1.3 Concrete failure timeline

Assume the canonical chain is:

```text
block 6 ──> block 7A
                ↑
          persisted checkpoint
```

A runtime wallet `W` is created after the indexer has committed `7A`:

```text
checkpoint:       (height 7, hash 7A)
wallet address:   W
stored birthday:  8
```

The API returns address `W`, and a payment is broadcast to it.

The chain then reorganizes:

```text
Old canonical branch: block 6 ──> block 7A  (orphaned)
New canonical branch: block 6 ──> block 7B  (contains payment to W)
```

`7B` has a lower height than the wallet's numeric birthday, but it was mined
after the wallet existed and after its address was returned. It can therefore
legitimately contain the wallet's payment.

The synchronizer performs these steps:

1. It reads persisted checkpoint `7A`.
2. It asks the source for the canonical hash at height 7 and receives `7B`.
3. It searches backward and finds block 6 as the common ancestor.
4. It removes orphaned block `7A`, returning the checkpoint to block 6.
5. It starts the normal forward path at replacement block `7B`.
6. It evaluates wallet `W` for height 7:

   ```text
   start_height = 8
   block_height = 7

   8 <= 7 is false
   ```

7. It interprets and commits `7B` without address `W`.
8. At height 8, wallet `W` becomes active, but block `7B` is never revisited.

The payment is permanently absent from canonical history. For a UTXO chain,
the corresponding live output can also be absent, so balance and later
transaction construction may be wrong.

### 1.4 Why the system still reports `Ready`

Readiness is derived from checkpoint progress:

```text
local checkpoint reached observed canonical tip → Ready
```

It does not prove that every replacement block was interpreted with the correct
address set. The index can therefore be internally consistent at the block and
storage level while being incomplete for wallet `W`.

This is why the issue is P1: it produces a silent false-success state rather
than a visible synchronization failure.

### 1.5 The conceptual error

The wallet was not created "after every possible block at height 7." It was
created after one exact canonical block:

```text
BlockRef {
    height: 7,
    hash: 7A,
}
```

When `7A` is orphaned, the statement "the wallet starts after height 7" loses
its intended meaning. Replacement block `7B` is new chain history even though
its height is still 7.

The current numeric birthday is being used for two different purposes:

1. to avoid scanning old blocks when a wallet is created;
2. to decide whether an address is active when any block is processed later.

The first purpose is controlled safely by the persisted checkpoint. Using the
same height for the second purpose creates the reorg hole.

### 1.6 When the bug is reachable

The bug requires all of the following:

- a wallet is created or adopted at runtime after checkpoint `H`;
- its filter receives `start_height = H + 1`;
- a later retained reorg finds a common ancestor below `H`;
- a replacement block at height `<= H` contains activity for the wallet.

The transaction can appear in the lower replacement block because replacement
blocks are mined later in real time. Block height is chain position, not wall
clock creation order.

The problem applies to any chain whose synchronizer uses the shared
height-filtered reorg path. It is not limited to Bitcoin, although missing a
Bitcoin output makes the impact especially visible.

### 1.7 Cases that are not affected

- A reorg whose common ancestor remains at or above the wallet's activation
  boundary does not cross the unsafe height.
- A fresh scope with birthday zero already activates the address for every
  processed block.
- A reorg deeper than retained rollback data produces `ReorgTooDeep`; that is a
  visible rescan requirement rather than this silent miss.
- A startup import with an explicit historical birthday has caller-selected
  coverage semantics and must retain that explicit boundary.

### 1.8 Existing tests do not prove this invariant

The runtime-selection test added by PR #8 registers a wallet during
`source.tip()`. It tests the in-flight selection race from P1, not a later
canonical replacement below the wallet birthday.

A valid P1.1 regression must:

- create the wallet after committing an activation block;
- return or expose the address;
- replace that activation block;
- put a payment in the replacement block;
- prove the replacement block was interpreted with the wallet address;
- prove canonical history and live outputs contain the payment;
- prove the runtime reaches `Ready` only with complete data.

## 2. Solution

### 2.1 Decision

For wallets created at runtime, use a conservative filter floor:

```rust,ignore
start_height = BlockHeight(0)
```

Do not rewind the checkpoint when the wallet is registered.

The meanings become separate and explicit:

- the persisted checkpoint decides which block synchronization processes next;
- the filter floor decides whether the wallet is active for a block that the
  synchronizer processes or reprocesses.

With an existing checkpoint `H`, a newly registered runtime wallet does not
cause blocks `0..=H` to be read again. The next ordinary block remains `H + 1`.
If a later reorg rolls the checkpoint back, however, the wallet is active for
every replacement block because zero is below every valid block height.

This is a correctness-first choice. A full rebuild may scan more history, but
it cannot silently omit wallet activity.

### 2.2 Dependency on P1

This solution assumes the synchronization barrier from `docs/reviews/P1.md` is
also implemented.

`start_height = 0` cannot help a sync call that already copied a filter list
which does not contain the wallet at all. The required registration sequence is:

```text
pause requested
current sync call finishes
pause acknowledged
wallet with start_height 0 is inserted
pause released
next sync reads a selection containing the wallet
```

P1 provides safe insertion ordering. P1.1 provides safe behavior during later
reorgs. Neither replaces the other.

### 2.3 Why the conservative floor works

Normal forward processing:

```text
checkpoint when wallet is inserted: H
filter start height:                 0
next block selected by checkpoint:  H + 1

0 <= H + 1 → wallet active
```

No historical block is automatically rescanned because synchronization resumes
from the persisted checkpoint.

Replacement processing:

```text
activation checkpoint:              H
reorg common ancestor:              A, where A < H
first replacement block:            A + 1
filter start height:                 0

0 <= A + 1 → wallet active
```

The wallet is inspected in replacement blocks regardless of how far the
retained reorg rolls back.

### 2.4 Required code changes

#### `sdk/wallets/src/wallets.rs`

Change the runtime path in `Wallets::activate`:

```rust,ignore
let start_height = match start_height {
    Some(height) => height,
    None => BlockHeight(0),
};
```

Rules:

- `Wallets::import(..., Some(height))` keeps the caller's explicit height.
- `Wallets::generate(..., None)` becomes active for every subsequently
  processed block.
- `Wallets::adopt(..., None)` may use the same policy only when the adopted key
  is newly generated/unpublished and is guaranteed not to have earlier
  history. An existing externally used key must go through startup import with
  an explicit birthday and, when necessary, a rescan.

After changing this logic, search all uses of the injected `Checkpoint`
capability. If `Wallets` no longer needs it anywhere, remove that dependency
cleanly from:

- `Wallets` fields and constructor;
- application composition;
- tests and documentation.

Do not keep an unused checkpoint dependency or a compatibility constructor in
this pre-release codebase.

#### `sdk/indexing`

The synchronizer's active-address predicate can remain:

```rust,ignore
filter.start_height <= height
```

Do not add special reorg-only address lists inside storage. Chain interpreters
still receive facts, and persistence remains unaware of wallet lifecycle.

Clarify `AddressFilter::start_height` as a conservative coverage floor: the
earliest height at which the address must be active whenever that height is
processed. It is not necessarily the wall-clock creation height.

#### `sdk/indexing/runtime`

Implement the P1 pause/acknowledgement protocol first or in the same coherent
change. Runtime synchronization must not begin another pass between wallet
activation and filter visibility.

#### Registry implementations

When runtime adoption uses a durable registry, persist the conservative zero
floor so restart restores the same selection semantics.

The currently shipped API passes `None` for family registries, so ordinary
generated wallets remain memory-only across restart. Do not claim that P1.1
adds durable custody or restart persistence.

#### Documentation

Update all higher-authority documents consistently:

- `docs/SYSTEM_REQUIREMENTS.md`
- `ARCHITECTURE.md`
- `docs/CONTRACTS.md`
- `docs/INDEXING.md`
- `docs/FEATURE_VALIDATION.md`

The documents must distinguish:

- explicit import birthday;
- conservative runtime filter floor;
- checkpoint-controlled forward progress;
- replacement-block coverage during retained reorgs;
- the separate P1 registration barrier.

Do not continue saying that `checkpoint + 1` alone makes runtime generation
safe.

### 2.5 Resulting behavior

| Scenario | Expected behavior |
|---|---|
| Ordinary runtime generation at checkpoint `H` | No rewind; next processed block is `H + 1`, with the wallet active |
| Retained reorg to ancestor `A < H` | Wallet is active for every replacement block from `A + 1` |
| Restart with the same checkpoint and restored filter | Resume at checkpoint plus one; wallet remains active |
| Repository recreation/full rescan | Runtime wallet may be inspected from genesis; safe but potentially more expensive |
| Explicit startup import at height `B` | Preserve caller-supplied `B`; scan according to import contract |
| Reorg deeper than retained journal | Return `ReorgTooDeep`; require explicit scope recreation/rescan |

### 2.6 Required regression tests

#### Replacement at the activation height

1. Commit canonical block `7A`.
2. Register runtime wallet `W` under the P1 pause barrier.
3. Assert its filter floor is zero.
4. Replace `7A` with `7B` containing a payment to `W`.
5. Run synchronization.
6. Assert `7A` is removed.
7. Assert `7B` is interpreted with address `W`.
8. Assert canonical history contains the payment.
9. For Bitcoin, assert the replacement output exists in the live output set.
10. Assert synchronization reaches `Ready` only after those facts commit.

This test must fail with `start_height = 8` and pass with the conservative
runtime filter floor.

#### Multi-block retained reorg below activation

1. Activate a wallet after checkpoint `10A`.
2. Reorganize to a common ancestor below 10, within retention.
3. Put wallet activity in the earliest replacement block.
4. Assert every replacement height is interpreted with the wallet active.

This proves that changing `H + 1` to `H` would not be sufficient.

#### No ordinary historical backfill

1. Begin with checkpoint `100`.
2. Register a runtime wallet with filter floor zero.
3. Resume synchronization with tip `101`.
4. Assert the source is asked for block 101, not blocks 0 through 100.

This proves that the checkpoint, not the filter floor, prevents ordinary
backfill.

#### Explicit import remains unchanged

Import a wallet with birthday 50 into a fresh scope. Assert anchoring and
interpretation still begin according to the explicit import contract rather
than forcing zero.

#### Restart and durable registry

For a configured registry:

1. adopt a newly generated runtime key;
2. persist and restore it;
3. assert the restored filter retains the zero floor;
4. perform a retained reorg below its original activation checkpoint;
5. assert replacement activity is indexed.

If no registry is configured, state explicitly that restart persistence was not
tested or provided.

#### Deep reorg boundary

Exercise the exact retained boundary and the first depth beyond it. Assert:

- retained replacement blocks are indexed with the wallet active;
- a deeper reorg returns `ReorgTooDeep`;
- readiness is not published after that terminal coverage failure.

### 2.7 Acceptance criteria

The implementation is complete only when:

- Runtime `generate` no longer derives its filter floor from
  `checkpoint.height + 1`.
- Runtime-created wallet addresses are active for every block processed after
  registration, including lower-height replacement blocks.
- The existing checkpoint prevents an immediate historical scan during normal
  runtime generation.
- Explicit startup-import birthdays retain their existing semantics.
- P1's pause barrier guarantees the wallet is present before another sync call
  starts.
- A retained replacement block containing the wallet's payment is reflected in
  canonical history and applicable live-output state.
- `Ready` is not used as a substitute for coverage assertions in tests.
- Deep reorgs still fail visibly with `ReorgTooDeep` rather than silently
  advancing incomplete state.
- Registry-backed restoration preserves the same conservative filter floor.
- No new wallet/indexer dependency direction is introduced.
- No secret material enters logs, synchronization messages, snapshots, or
  ordinary `Debug` output.
- Public requirements and architecture documents describe the new semantics
  consistently.

### 2.8 Alternatives considered

#### Store `AfterCheckpoint(BlockRef)`

A more precise model could distinguish:

```rust,ignore
enum CoverageStart {
    FromHeight(BlockHeight),
    AfterCheckpoint(BlockRef),
}
```

When the anchor hash remains canonical, processing begins after its height. If
the anchor becomes orphaned, the wallet becomes active from the reorg's common
ancestor.

This model is semantically exact but requires:

- new filter and registry representations;
- durable anchor-hash storage;
- restart behavior for orphaned anchors;
- reorg-aware activation calculations;
- conservative behavior when anchor canonicality cannot be queried.

It may be adopted later if full-rescan cost becomes material. It is not required
for the simpler correctness-first fix.

#### Change birthday from `H + 1` to `H`

Reject this. A deeper retained reorg can replace `H - 1`, and a newly mined
replacement block there can contain the wallet payment. The same hole remains
one height lower.

#### Stop whenever a reorg crosses the birthday

This avoids silent corruption but turns retained reorgs into manual rescans and
does not satisfy the product's retained-reorg survival goal. It is acceptable
only as a temporary safety guard if the full fix cannot ship immediately.

### 2.9 Implementation order

1. Implement or land the P1 pause/acknowledgement barrier.
2. Add the failing equal-height replacement regression.
3. Change runtime-generated filter floors to zero while preserving explicit
   import heights.
4. Remove the wallet checkpoint dependency if it has no remaining callers.
5. Add multi-block reorg, no-backfill, restart, and deep-boundary tests.
6. Update the complete documentation hierarchy.
7. Run focused tests followed by workspace validation.

### 2.10 Validation commands

At minimum:

```bash
cargo fmt --all -- --check
cargo test --locked -p indexing
cargo test --locked -p indexing-runtime
cargo test --locked -p indexing-redb
cargo test --locked -p wallets
cargo test --locked -p payment-api
cargo check --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked --workspace --no-deps
cargo run --locked -p design-lint -- --policy lint.toml check .
git diff --check
```

Report unavailable RPC, PostgreSQL, or acceptance environments separately. A
unit test that checks only filter construction is not sufficient evidence; the
required proof is a payment-bearing replacement branch processed through the
public indexing behavior.
