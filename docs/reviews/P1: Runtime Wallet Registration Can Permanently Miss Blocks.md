# P1: Runtime Wallet Registration Can Permanently Miss Blocks

## Purpose

This document is an implementation brief for the coding agent fixing the
runtime-wallet synchronization race discussed during review of PR #8 at commit
`1c6db91f5161f8f8b194362d7c43bc52482df489`.

The required outcome is stronger than "read the filters again." A wallet must
never become visible to an API caller with a birthday whose blocks can be
committed without that wallet's address being inspected.

## Executive summary

The current proposal reads the wallet filter selection twice:

1. once before source I/O, for validation;
2. once after observing the source tip, for synchronization.

That fixes only registrations occurring between those two reads. After the
second read, the synchronizer stores the filters in one `Vec<AddressFilter>` and
reuses it for the complete batch. A wallet can still be created after this
snapshot while the synchronizer continues advancing the checkpoint.

The recommended fix is an application-owned synchronization barrier:

- runtime wallet creation requests a pause;
- the indexing runtime finishes its current `Indexer::sync` call;
- the runtime acknowledges the pause only when no synchronization call is in
  flight;
- wallet creation reads the now-stable checkpoint and inserts the wallet;
- wallet creation completes before releasing the pause;
- synchronization resumes and reads a selection containing the new wallet.

This is a logical pause protocol implemented with channels and acknowledgements,
not a mutex held across `.await`.

## Relevant ownership and source

The important components are:

- `sdk/wallets/src/wallets.rs`
  - `Wallets::generate`
  - `Wallets::adopt`
  - `Wallets::store`
  - `Wallets::activate`
  - `Wallets::filters`
- `sdk/indexing/src/synchronizer.rs`
  - `Synchronizer::sync`
  - the bounded block-application loop
- `sdk/indexing/src/service.rs`
  - `FilterSource`
  - `Service`'s `Indexer` implementation
- `sdk/indexing/src/composer.rs`
  - scope validation and filter partitioning
- `sdk/indexing/runtime/src/lib.rs`
  - the long-running synchronization loop
  - readiness, retry, shutdown, and the proposed pause protocol
- `apps/api/src/main.rs`
  - construction of `Wallets`, `Composer`, and the synchronization task
- `apps/api/src/api/wallet.rs`
  - the runtime wallet-creation endpoint

The live wallet set is currently stored in memory in:

```rust,ignore
RwLock<BTreeMap<I, Entry<I, F>>>
```

`Wallets::filters()` reads that map and produces a new filter vector. The
vector is a snapshot; later insertions into the map do not change it.

## Required invariant

Use this as the acceptance invariant:

> If a runtime wallet is assigned birthday `B`, every canonical block at height
> `B` or later that the indexer commits must have been interpreted with that
> wallet's canonical address active.

For the generated-wallet HTTP flow, the externally observable version is:

> Before the new address is returned to the caller, either synchronization is
> paused at a stable checkpoint or the system has another atomic protocol that
> proves no block at or above the assigned birthday can commit without the new
> address.

The implementation must establish a linearization point between:

1. reading the checkpoint used to calculate the birthday;
2. inserting the new wallet/filter into `Wallets.values`; and
3. starting the next synchronization call that can advance the checkpoint.

## Failure timeline

Assume:

- persisted checkpoint: block `100`;
- observed source tip: block `105`;
- batch size permits blocks `101..=105` in one sync call.

The unsafe execution is:

```text
Indexer                                 Wallet API
-------                                 ----------
reads filters: [existing wallets]
observes source tip 105
reads filters again: [existing wallets]
                                        reads checkpoint 100
                                        assigns birthday 101
                                        inserts and returns new wallet
commits block 101 without new wallet
commits block 102 without new wallet
commits block 103 without new wallet
commits block 104 without new wallet
commits block 105 without new wallet
checkpoint is now 105
```

The next synchronization pass sees the new wallet, but it starts at block 106.
Blocks 101 through 105 are never interpreted again for that address. A payment
to the returned address in those blocks is permanently missing from indexed
history and balance, even though synchronization can report `Ready`.

## Why the new PR test is insufficient

The PR test registers the address from inside `source.tip()`:

```text
first selection read
source.tip() starts
wallet is registered
source.tip() returns
second selection read sees the wallet
blocks are processed
```

That test proves only that the second read catches registration during the tip
request. It does not cover registration after the second read:

```text
second selection read
wallet is registered
block commit starts
```

Moving or repeating a snapshot read changes the size of the race window; it
does not remove the race.

## Recommended solution: pause and acknowledge at sync boundaries

### Decision

Add a small control plane to `sdk/indexing/runtime`. Runtime wallet creation
must obtain an exclusive pause permit before it calls the code that reads the
checkpoint and inserts the wallet.

The indexing runtime acknowledges a pause only between complete
`Indexer::sync` calls. It must never cancel a synchronization future merely to
service the pause request: the current call may be committing storage or
reconciling a reorg.

### Resulting sequence

```text
Wallet creation                         Indexing runtime
---------------                         ----------------
request pause ------------------------> current sync continues safely
                                        current sync returns
                           <------------ pause acknowledged
read stable checkpoint H
assign birthday H + 1
insert wallet/filter
finish durable registration if used
return successful wallet result
release pause ------------------------> resume synchronization
                                        read filters including new wallet
                                        process H + 1 and later
```

If a pause request arrives during a batch, wallet creation waits for that batch
to finish. The wallet address is not returned during the unsafe interval. When
the pause is acknowledged, the wallet reads the final checkpoint produced by
the batch, so its birthday is after that checkpoint.

### Why this closes the race

There are only two valid orderings:

1. Synchronization wins first.
   - The current batch completes.
   - The pause is acknowledged.
   - Wallet creation reads the new checkpoint and uses `checkpoint + 1`.
2. Wallet creation wins first.
   - The runtime is already paused.
   - The wallet is inserted.
   - Synchronization resumes and obtains a filter snapshot containing it.

There is no ordering in which wallet creation reads checkpoint `H`, becomes
visible, and synchronization then commits `H + 1` from an older filter
snapshot.

## Suggested Rust design

Names are illustrative. Keep final names aligned with the repository's precise
noun-based style.

### Runtime control handle

`sdk/indexing/runtime` should expose a cloneable handle backed by a bounded
`tokio::sync::mpsc` command channel and `oneshot` acknowledgements.

```rust,ignore
pub struct SynchronizationControl {
    commands: tokio::sync::mpsc::Sender<Command>,
}

enum Command {
    Pause {
        acknowledged: tokio::sync::oneshot::Sender<PauseToken>,
    },
    Resume {
        token: PauseToken,
    },
}

pub struct PausePermit {
    token: PauseToken,
    resume: tokio::sync::mpsc::UnboundedSender<Command>,
}
```

Required behavior:

- `pause().await` returns only after the current `Indexer::sync` call finishes.
- While a permit is active, the loop starts no new synchronization call.
- `PausePermit::drop` must request resume so cancellation of the HTTP request
  cannot leave indexing permanently paused.
- Shutdown closes pending pause requests with a typed error.
- Concurrent pause requests must be serialized or reference-counted with a
  documented policy. Exclusive FIFO permits are the simplest initial design.
- Control messages carry no wallet secret, private key, or wallet instance.
- Use a bounded request channel for backpressure. An unbounded, non-blocking
  sender may be used narrowly for `Drop`-based resume if necessary.

Do not hold `std::sync::MutexGuard`, `RwLockGuard`, or
`tokio::sync::MutexGuard` across `.await` to implement this protocol.

### Runtime loop behavior

The loop should service pause requests only at safe boundaries:

```rust,ignore
loop {
    // Process queued control messages before starting another sync call.
    if paused {
        wait_for_resume_or_shutdown().await?;
        continue;
    }

    // Do not select a pause request against this future and cancel it halfway.
    let result = run_one_complete_sync_call().await;
    publish_state(result)?;

    // A queued pause is acknowledged here, before the next sync call.
}
```

Shutdown may still use the repository's existing cancellation policy, but the
coding agent must verify that adding pause handling does not introduce a second
way to abandon an in-progress durable operation.

### Application orchestration

The API handler should continue to perform one application operation. Do not
put pause/resume mechanics directly in the Axum handler.

Introduce a narrowly named application capability in `apps/api` that composes:

- `Arc<Wallets<...>>`;
- `SynchronizationControl`.

Its runtime generation flow is:

```rust,ignore
pub async fn generate(&self, id: WalletId, family: &Family) -> Result<WalletInfo, Error> {
    let permit = self.synchronization.pause().await?;
    let result = self.wallets.generate(id, family).await;
    drop(permit);
    result
}
```

The actual implementation should ensure resume is attempted on every return
path. Prefer an RAII permit whose `Drop` is non-blocking.

This application capability coordinates two owners; it must not move wallet
domain behavior into HTTP and must not make `sdk/wallets` depend on
`sdk/indexing/runtime`.

Initially pausing around the complete `Wallets::generate` call is acceptable
for correctness. If measurement later shows key generation makes the pause too
long, split wallet preparation from activation carefully so the permit still
covers checkpoint read, filter insertion, and durable registration as one
logical operation.

### `FilterSource` after this change

`FilterSource` may remain useful for obtaining the latest authoritative
selection when a sync call begins. It must not be documented as the mechanism
that makes concurrent runtime registration safe.

Once runtime mutations occur only while synchronization is paused, one validated
snapshot per child sync is sufficient for that pause protocol. If the live
`FilterSource` API is retained, independently fix these concerns:

- do not silently discard an unconfigured scope that appears after Composer's
  initial validation;
- preserve retryable error classification instead of converting every filter
  read failure into terminal `InvalidRequest`;
- document exactly how many times one sync may read the selection.

## Required tests

### 1. Regression: pause requested during a batch

Build a deterministic source/repository double with barriers:

1. checkpoint begins at `100`;
2. source tip is `105`;
3. synchronization starts with only the original filters;
4. block processing is paused inside the test after the sync call begins;
5. runtime wallet creation requests a pause;
6. assert that wallet creation has not completed and its address has not been
   returned;
7. release the sync call and allow it to finish at checkpoint `105`;
8. assert that the pause is then acknowledged;
9. create the wallet and assert its birthday is `106`;
10. resume synchronization;
11. provide block `106` with a movement for the new wallet;
12. assert block 106 was interpreted with the new address and history contains
    the movement.

This test must fail under the PR's double-read-only implementation.

### 2. Registration already waiting before the next sync

Queue a pause while the runtime is between passes. Assert:

- the pause is acknowledged before another source-tip call;
- wallet insertion completes;
- the next sync reads the new filter;
- the first block at the wallet birthday is inspected for it.

### 3. Cancellation safety

Cancel or drop the wallet-creation future after obtaining the permit. Assert
that synchronization resumes and can reach `Ready`.

### 4. Wallet-generation failure

Make the provider fail after pause acknowledgement. Assert:

- the error reaches the caller;
- no partial wallet remains in `Wallets.values`;
- the runtime resumes;
- readiness can recover.

### 5. Shutdown while waiting

Request a pause while a sync call is in flight, then shut the runtime down.
Assert the waiting caller receives a typed terminal error rather than waiting
forever.

### 6. Concurrent wallet creation

Start two runtime generations concurrently. Assert the chosen permit policy is
deterministic, both wallets receive safe birthdays, and the runtime cannot
resume while either active permit still requires the pause.

### 7. Existing behavior

Retain coverage for:

- invalid filter selection rejected before source I/O;
- empty selection anchoring;
- birthday anchoring;
- retained reorg reconciliation;
- observer callbacks only after `BlockOutcome::Applied`;
- runtime readiness, retry, and shutdown behavior.

## Acceptance criteria

The change is complete only when all of the following are true:

- A wallet address cannot be returned while an older filter snapshot can still
  advance the checkpoint across that wallet's birthday.
- Pause acknowledgement happens only with no `Indexer::sync` call in flight.
- No synchronization future is cancelled merely to service a pause request.
- Every error or cancellation path releases the pause.
- Shutdown wakes every pending registration request.
- The Axum wallet handler remains a thin adapter invoking one application
  capability.
- `sdk/wallets` does not depend on `sdk/indexing/runtime`.
- No secret material is sent through synchronization-control messages or
  included in logs or `Debug` output.
- Focused deterministic tests reproduce the old race and pass with the fix.
- Relevant public-contract documents no longer claim that a second filter read
  alone provides atomic safety.

## Approaches that are not sufficient

### Read the wallet list a third time

There is always another interval after the last read and before commit. This is
still check-then-act without atomic coordination.

### Read filters before every block

A wallet can be inserted after that block's read and before its commit. The
window becomes smaller but remains real.

### Set `batch_size` to one

The wallet can be inserted after the only selection read and before that one
block commits. The following pass starts after the missed block.

### Check a selection revision before commit

A plain revision check is also vulnerable to a time-of-check/time-of-use race:
the revision can change after the check and before checkpoint commit. A revision
scheme is valid only if the revision and checkpoint participate in one atomic
commit protocol, or if a detected post-commit change safely rolls back and
replays the block while wallet activation itself is coordinated.

### Hold the wallet `RwLock` for the whole sync batch

This blocks wallet operations across network and storage awaits, violates the
repository's concurrency guidance, and creates a broad lock-order/deadlock
risk. Use an explicit pause protocol instead.

### Assign the birthday from the persisted checkpoint without pausing

That is the current race. The checkpoint may advance immediately after it is
read while the indexer still uses an older selection.

## Separate known issue: reorg-safe activation

The pause protocol fixes registration racing with an in-flight synchronization
batch. It does not by itself make a height-only birthday safe across a later
reorg.

Example:

1. wallet is activated after checkpoint `7A` and receives birthday `8`;
2. block `7A` is orphaned;
3. replacement block `7B` contains a payment to the wallet;
4. reconciliation returns to block 6;
5. the height-only filter is inactive for replacement block 7;
6. the payment is missed.

Do not claim the broader runtime-wallet lifecycle is fully reorg-safe after
implementing only this document. A separate design must bind activation to a
`BlockRef` or define conservative rescan semantics when the activation anchor is
orphaned, followed by a replacement-branch acceptance test.

## Implementation order

1. Write the failing deterministic pause-during-batch regression test.
2. Add runtime control commands and pause acknowledgement at sync boundaries.
3. Add cancellation-safe `PausePermit` behavior.
4. Add the narrow application orchestration capability.
5. Route runtime wallet generation through that capability.
6. Add failure, shutdown, and concurrent-generation tests.
7. Update `ARCHITECTURE.md`, `docs/CONTRACTS.md`, `docs/INDEXING.md`,
   `docs/SYSTEM_REQUIREMENTS.md`, and `docs/FEATURE_VALIDATION.md` consistently.
8. Run focused crate tests, then the workspace validation gates.

## Validation commands

At minimum:

```bash
cargo fmt --all -- --check
cargo test --locked -p indexing
cargo test --locked -p indexing-runtime
cargo test --locked -p wallets
cargo test --locked -p payment-api
cargo check --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked --workspace --no-deps
cargo run --locked -p design-lint -- --policy lint.toml check .
git diff --check
```

Report PostgreSQL-backed, RPC-backed, or acceptance tests separately if their
required local services are not available. Do not present a compiled or skipped
test target as runtime-race proof.
