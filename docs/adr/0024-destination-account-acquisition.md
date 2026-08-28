# ADR-0024: Destination Account Acquisition

## Status

Accepted

## Date

2026-08-27

## Accepted

Accepted by the user on 2026-08-28 under the simplified decision name
**Destination Account Acquisition**. Acceptance includes the all-or-nothing
rule that timeout, cancellation, oversized response, or malformed account data
terminates the complete acquisition, publishes no slot floor, and releases
every pre-envelope lexical source lease already held by the invocation. It
records a target contract and does not claim that a Solana implementation
exists.

## Context

Accepted decisions require confirmed destination observations, native RPC
health admission, exact context-slot floors, all-or-nothing attempts, on-curve
native destinations, and absent or zero-data System-owned recipient accounts.
The now-resolved question was how one bounded request acquires, deduplicates,
validates, and maps source and destination accounts without allowing a false
context slot to poison later work.

## Decision

One native SOL send invocation performs exactly one destination/account
observation attempt initially. It does not automatically retry or fail over.
The attempt uses the same configured endpoint for every call:

```text
getHealth()
  -> getSlot(confirmed) = F
  -> getMultipleAccounts(confirmed, base64, minContextSlot = F) = (C, values)
  -> getSlot(confirmed, minContextSlot = C) = U
  -> atomic eligibility and balance handoff at operation floor P = U
```

Only exact `"ok"` admits `getHealth`, and that one-shot call has no transparent
transport retry. The opening and closing `getSlot` calls and the account call
also have no transparent retry or endpoint switch.

### Grouping and mapping

For each original transfer in order, append its resolved source address and
then its parsed destination address to a private query list if that address has
not already appeared. Deduplication is stable by first occurrence and uses the
canonical 32-byte address, not request text or wallet ID. At 50 transfers the
list contains at most the protocol maximum of 100 addresses, so the initial
implementation sends one `getMultipleAccounts` call and never chunks.

The response must contain one valid context slot and exactly one value for each
requested address. Values map by request position only after exact cardinality
validation. A short, extra, malformed, or positionally uncorrelatable response
invalidates the complete attempt. One deduplicated observation is reused for
all occurrences of that address, but every later classification maps to the
earliest failing original item.

The request asks for full Base64 account data and sends no `dataSlice`.
Explicit JSON `null` alone means absent. Existing accounts require valid
lamports, owner, executable, data, and total-space fields. Account data must be
the exact `[string, "base64"]` tuple and decode under strict Base64 rules. Owner
text must be canonical Base58 for exactly 32 bytes. The decoded data must agree
with the reported total length; zero-data policy requires both to be zero.

Exact cardinality, context, and complete payload structure/encoding validate
before the closing `getSlot` is issued. A semantically well-formed account that
is executable, has non-zero data, or has an unsupported owner reaches the later
item-scoped account classification; malformed wire or account representation
does not.

### Slot plausibility and promotion

The account context must satisfy `C >= F`. It advances the causal request floor
inside this attempt, so the closing request must use it, but it remains
provisional outside the attempt. Only a successful closing confirmed
`getSlot(minContextSlot = C)` returning `U >= C` closes the attempt. Failure of
that witness discards every account fact and publishes no operation floor.
Only the witnessed `U` can become the floor passed to later preparation, and
it does so atomically with the complete successful eligibility and source-
balance handoff. Cancellation or semantic classification failure before that
handoff discards `F`, `C`, and `U` rather than publishing a floor alone.

This is endpoint-local consistency evidence, not an independent cluster tip,
fork proof, same-bank guarantee, or numeric maximum-lag guarantee. A dishonest
or faulty endpoint can return the self-consistent extreme `F = C = U =
u64::MAX`, which passes this witness and may fail later preparation. The witness
only rejects a claimed context that the same endpoint cannot continue to
satisfy. Nothing is retained across a separate caller invocation.

This accepted narrower publication rule partially supersedes the following
accepted clauses:

- ADR-0013 still governs causal, nondecreasing floors between requests inside
  an attempt, but an accepted contextual response is provisional until the
  closing witness succeeds; and
- ADR-0014 may carry a floor into a successor attempt only from a fully closed,
  witnessed predecessor. A failed closing witness publishes neither `C` nor
  the opening base.

Initial support has no automatic successor attempt. A separate caller
invocation begins without a retained floor. Any future in-operation retry may
inherit only the last fully witnessed floor from a complete atomic handoff and
must reacquire every account.

### Failure atomicity, cancellation, and lease release

The acquisition is one all-or-nothing operation. A timeout or cancellation at
any await; transport, HTTP, or JSON-RPC failure; response larger than the
configured `max_response_bytes`; invalid JSON or Base64; invalid owner text;
missing or malformed context, lamports, executable, data, or space; a data
value other than the exact `[string, "base64"]` tuple; an invalid Base64
alphabet or padding; owner text that is non-string, invalid Base58, non-
canonical, or not exactly 32 decoded bytes; cardinality mismatch; data/space
disagreement; below-floor response; or failed closing witness terminates the
complete acquisition.

Such termination discards every observed account value and explicit absence,
publishes no `F`, `C`, `U`, or derived operation floor outside the attempt,
releases every pre-envelope lexical source lease held for the invocation, and
performs no fee call, transaction construction, signing, simulation, or
broadcast. It is an operation-wide, index-free failure and supplies no
transaction ID, accepted IDs, failed index, or ambiguous ID. Cancellation
cannot leave a background acquisition, pending floor publication, or lexical
source lease behind. This decision never releases a coordinator-owned
submitted or ambiguous envelope guard.

A fully decoded, structurally coherent account with a valid but unsupported
semantic shape remains different: after the complete acquisition witness
closes, classification assigns that error to the earliest original occurrence
using the account. Because classification prevented the atomic handoff, it
publishes no floor. Initial support still performs no automatic reacquisition;
every terminal item error releases the invocation's pre-envelope lexical
leases.

### Account classification

Destination syntax and on-curve checks happen before the RPC call. After the
complete snapshot is validated, original items are classified in order:

- an absent destination is eligible;
- an existing destination is eligible only when it is non-executable,
  System-owned, and zero-data;
- an existing source must satisfy the same System-account shape; and
- an absent source has balance zero and fails later checked sufficiency.

The snapshot's source lamports are the balance input for batch preparation.
They are not a reservation and do not prove the account will remain unchanged.

An acquisition/response failure is operation-wide and index-free. An account
shape failure is assigned to the earliest original item using that source or
destination. No failed attempt reaches fee calculation, signing, simulation,
or broadcast.

## Consequences

- One contextual request observes all account facts used for eligibility and
  initial source balances.
- Duplicate addresses cannot receive contradictory classifications inside one
  attempt.
- The public 50-item limit avoids provider-specific chunk semantics.
- The closing witness catches only internally uncorroborated context claims; it
  does not limit a self-consistent false-high claim or establish freshness.
- A transient failure or cancellation ends the invocation without retaining
  account facts, a floor, or pre-envelope lexical source leases; callers may
  start a new invocation, subject to submission-idempotency guidance.

## Alternatives considered

### Sequential `getAccountInfo`

Rejected for the bounded initial contract. It is easy to map but creates up to
100 sequential observations and permits state differences between addresses.

### Chunked `getMultipleAccounts`

Rejected initially. The shared limit fits one protocol-supported request, so
chunk ordering and partial-attempt policy add no value.

### Accept every numeric context slot immediately

Rejected. An inconsistent `u64::MAX` response could poison the live operation
floor even when the endpoint cannot satisfy it on the next call.

## Validation requirements

Tests must cover stable deduplication and reverse mapping; exactly 100 unique
addresses; malformed cardinality; duplicate sources and destinations; null
accounts; full-data/space disagreement; every unsupported owner/executable/data
shape; `C < F`; closing-witness failure and `U < C`; false `u64::MAX` without
floor publication; self-consistent `F = C = U = u64::MAX` publication only
with a complete atomic handoff inside the live invocation; one-shot call
counts; no endpoint switch; earliest
original item mapping; and zero downstream effects on any failed attempt. The
suite must separately cover timeout and cancellation at `getHealth`, both
`getSlot` calls, `getMultipleAccounts`, and immediately before atomic handoff;
enforcement of the configured response cap before over-buffering; invalid JSON;
the exact Base64 tuple/label plus invalid alphabet and padding; non-string,
invalid, wrong-length, or non-canonical owner text; malformed account fields;
release of all lexical leases; absence of background work; and no floor
publication. It must distinguish those operation-wide failures from a valid
32-byte non-System owner or other structurally valid but semantically
unsupported account shape assigned to one original item.

## Approval boundary

This accepted decision consolidates account method selection, batching,
deduplication, response mapping, failure precedence, and apparently-future-slot
handling. Acceptance records the contract but does not authorize
implementation. `docs/SYSTEM_REQUIREMENTS.md`, `docs/API.md`,
`docs/CONTRACTS.md`, `docs/FEATURE_VALIDATION.md`, and the partially superseded
floor-publication clauses in ADR-0013 and ADR-0014 must be reconciled in the
same documentation change.

## References

- [Solana `getHealth`](https://solana.com/docs/rpc/http/gethealth)
- [Solana `getSlot`](https://solana.com/docs/rpc/http/getslot)
- [Solana `getMultipleAccounts`](https://solana.com/docs/rpc/http/getmultipleaccounts)
- `packages/json-rpc/src/http.rs`
- `docs/adr/0006-require-on-curve-native-sol-destinations.md`
- `docs/adr/0007-require-zero-data-system-wallet-destinations.md`
- `docs/adr/0008-treat-sol-destination-observations-as-one-attempt.md`
- `docs/adr/0013-chain-sol-destination-reads-through-a-monotonic-floor.md`
- `docs/adr/0014-carry-sol-destination-progress-across-operation-retries.md`
- `docs/API.md`
- `docs/SYSTEM_REQUIREMENTS.md`
- `docs/CONTRACTS.md`
- `docs/FEATURE_VALIDATION.md`
