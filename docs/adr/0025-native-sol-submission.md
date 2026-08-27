# ADR-0025: Native SOL Submission

## Status

Proposed

## Date

2026-08-27

## Context

Native SOL submission needs an exact transaction identity, fee and balance
policy, full-batch preparation boundary, recent-blockhash lifetime, simulation
configuration, ordered broadcast, and truthful handling when RPC acceptance is
unknown. Identical System transfers built with the same recent blockhash would
otherwise have identical messages and signatures, collapsing two requested
payments into one on-chain transaction.

## Decision

The concrete Solana coordinator is process-local and keyed by resolved source
public key. It acquires every involved source in canonical byte order, prepares
the whole batch, and prevents another local send from those sources until the
batch finishes. Immediately before a first potentially submitting call, its
lexical source leases transition atomically into coordinator-owned envelope
state. Registration never releases an ambiguous source for another send.

Source acquisition is fail-fast and all-or-nothing. If any requested source is
already preparing, submitting, or guarded by unresolved ambiguity, the
coordinator releases every lease acquired for this invocation and performs no
account RPC, construction, signing, simulation, or broadcast. It reports the
earliest original occurrence using that source as `SourceBusy`: `503 Service
Unavailable`, no transaction IDs, and no ambiguous ID because the new
invocation has not crossed its wire boundary. A batch carries that occurrence's
original `failed_index`; a single send has no index. The prior guarded envelope
and its reconciliation continue unchanged. A caller may retry this definite
pre-broadcast failure later, but must not infer anything new about the earlier
ambiguous transaction.

Every self-transfer is rejected before RPC. Every other item becomes one
legacy transaction with exactly these top-level instructions:

1. one System Program native transfer, with the source as fee payer and only
   signer; and
2. one zero-account Memo instruction containing a new opaque 256-bit token
   from the operating-system CSPRNG, encoded as canonical Base58.

The token contains no wallet ID, customer data, timestamp, amount, destination,
or caller idempotency value. Every item receives a different token, including
non-duplicate items. A token remains part of its immutable signed envelope for
every replay. Construction uses exactly `spl_memo_interface::v3::ID`,
`MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr`. That exact account must be
executable at startup and in the owned validator fixture; v1, v4, an override,
and a memo-less fallback are unsupported.

The Memo supplies transaction uniqueness, not HTTP request idempotency.

### Full-batch preparation

After the account handoff, preparation is strictly:

1. acquire `getLatestBlockhash` with `confirmed` commitment and the current
   `minContextSlot`; retain blockhash, context slot, and
   `lastValidBlockHeight` as one lifetime value;
2. construct every exact System-transfer-plus-Memo legacy message;
3. call `getFeeForMessage` sequentially in original order for every exact
   message, sending the Base64 encoding of its exact bincode wire bytes with
   `confirmed` and the nondecreasing context floor; `null` is a failure, never
   a zero fee;
4. use checked arithmetic to accumulate `amount + exact fee` per source from
   the account snapshot, without crediting incoming batch transfers;
5. sign each message once with its source Ed25519 signer, verify every local
   signature, serialize exact bytes, and require all messages and first
   signatures to be distinct; and
6. simulate every exact signed transaction in original order with Base64,
   `confirmed`, `sigVerify: true`, `replaceRecentBlockhash: false`, and the
   current `minContextSlot`; require RPC success and `value.err == null`.

Each contextual response must meet the exact sent floor before advancing it.
Any amount, address, RNG, blockhash, fee, arithmetic, signing, encoding, or
simulation failure occurs before the first broadcast. The first original item
failing the current stage is reported; operation-wide RPC/coherence failures
remain index-free.

`lastValidBlockHeight` is compared only with confirmed `getBlockHeight`, never
with a slot. Every lifetime query sends the current operation floor as
`minContextSlot`; its bare `u64` result cannot advance that slot floor. The
envelope is valid while `currentBlockHeight <= lastValidBlockHeight` and expires
only when the current height is greater. The coordinator checks immediately
before the first broadcast and before every later item. It never rebuilds or
re-signs an envelope after any batch item may have been submitted.

### Broadcast and exact replay

Transactions are broadcast in original order with:

```text
encoding: base64
skipPreflight: false
preflightCommitment: confirmed
minContextSlot: current operation floor
maxRetries: 0
```

The SDK uses a one-shot HTTP execution and requires the returned Base58
signature to equal the locally derived first signature. Matching success means
submitted, not confirmed. The immutable envelope and its source guard are
registered immediately before this first `sendTransaction`, then ownership is
transferred to an application-supervised coordinator task before dispatch.
Dropping or cancelling the HTTP handler drops only its result waiter; it cannot
cancel the submission or reconciliation task.

For a retryable transport or unknown response, the coordinator may perform at
most two additional wire submissions during the invocation, for three
`sendTransaction` calls total. Before each replay, and once after the final
unknown response, it calls `getSignatureStatuses` with exactly the one local
signature and `{ "searchTransactionHistory": true }`. That method has no
commitment or `minContextSlot`; the coordinator therefore requires one response
entry in request position, a valid context slot no lower than the operation
floor, and a valid status shape whose reported transaction slot lies in the
inclusive range from the operation floor through that response context. A
lower slot is inconsistent with this SDK's construction timeline, but the RPC
protocol does not guarantee either bound; this is a defensive SDK coherence
rule. Because the request could not carry the floor, its response context is
corroboration only and never advances the operation floor. Any non-null entry,
including one with `err != null`, proves that the exact signature was observed
and is returned as submitted; it does not prove successful execution or
finality. `null` remains unknown. An unavailable, malformed, incoherent,
low-context, short, or extra status result also remains unknown and permits no
replay.

After a valid null status, the coordinator queries confirmed `getBlockHeight`
with the current `minContextSlot`. Failure of that query remains ambiguous and
permits no replay. Height greater than `lastValidBlockHeight` forbids replay but
does not prove absence. At or below the last valid height, only the identical
bytes may be replayed. Memo, blockhash, message, signature, endpoint, and item
order never change.

Under the initial endpoint model, no `sendTransaction` error proves no relay.
The configured URL may front a load balancer or proxy whose hedging and retry
behavior the SDK cannot inspect. Once the client begins the first wire call,
every timeout, disconnect, cancellation, JSON-RPC error code, malformed or
uncorrelated response, internal error, provider message, and returned-signature
mismatch is therefore ambiguous. Agave custom codes and standard JSON-RPC
codes remain diagnostics only; none releases the source or changes replay
safety. A definite item failure can occur only before that wire boundary.

Every ambiguous or definite broadcast result is still attached to the exact
original occurrence currently being attempted. In a batch it retains that
`failed_index` and the definitely acknowledged prefix; an item-scoped wire
call never becomes an index-free operation error merely because its cause is
provider-wide.

If the bounded replay window ends unresolved, the API uses the single or batch
`503` shape fixed by Public Transaction Semantics. The ambiguous item is not
part of a batch's accepted prefix and no later item is attempted. Sources used
only by unattempted later items are released when the batch task terminates;
the ambiguous item's resolved source remains guarded.

The coordinator receives the same scope's injected chain-neutral `Checkpoint`
and `History` capabilities plus a checkpoint-advance notification published by
the application-installed index observer. Background reconciliation repeats
the exact one-signature historical status query when that notification fires
and on a deterministic capped backoff starting at 500 milliseconds and
doubling to 10 seconds. Checkpoint progress resets the backoff. It never spins
an un-delayed RPC or history loop.

A valid non-null status or canonical indexed history containing the signature
resolves it as submitted, regardless of execution error. Absence is terminal
only when all of these are true:

1. confirmed block height is greater than `lastValidBlockHeight`;
2. the finalized indexer has complete, unpruned canonical coverage through
   that produced height; and
3. the active fee-payer/source address history for the complete possible
   inclusion window contains no matching signature.

The history proof reads pages of at most 100 entries against one
checkpoint-bound cursor until exhaustion. It first requires checkpoint height
at least `lastValidBlockHeight`; any cursor conflict, checkpoint change, reorg,
page error, or incomplete traversal discards the scan and waits for the next
notification/backoff. Only one exhausted traversal at one unchanged checkpoint
may classify the envelope as expired without landing and release the source.
Null status, unavailable transaction history, an indexer gap, pruning, or a
fatal source failure leaves the source blocked. Canonical confirmation or
failure presented to ordinary callers still comes only from indexing.

No durable outgoing-operation store is introduced. One running API process
must be the only writer for each managed source. A caller or proxy can lose a
successful HTTP response, a handler can be cancelled after the task accepts
the transaction, another active API instance can race, or a process can fail
over/restart and lose the guard. A new logical invocation creates a new Memo
and can therefore double-pay in every one of those cases, not only after a
crash. Callers must not automatically retry an unknown result. Cross-process or
logical-payment duplicate prevention requires durable request identity plus
exact-envelope recovery and is outside current product scope.

Priority-fee and Compute Budget instructions remain unsupported.

## Consequences

- Repeated identical payments within one blockhash have distinct signatures.
- Full preparation guarantees zero broadcasts on any preparation failure.
- Exact-byte replay improves delivery without creating a second transaction.
- Ambiguous outcomes expose the only safe reconciliation identity.
- Standard Memo program availability becomes a startup and test requirement.
- Client-retry, cancellation, failover, active-active, and crash-safe
  idempotency remain explicit limitations.

## Alternatives considered

### Reject duplicate intents

Rejected. It does not protect two sequential identical invocations that obtain
the same recent blockhash, and it forbids intentional repeated payments.

### Wait for a different recent blockhash

Rejected. It adds progress-dependent latency and still needs durable prior
state across restart.

### Durable nonce accounts

Rejected initially. They require funded nonce accounts, nonce authority
custody, serialization, recovery, and a different batch-preparation model.

## Validation requirements

Tests must cover same-blockhash identical items and sequential sends; RNG
failure; missing Memo program; exact fee and simulation including Memo; checked
cumulative balances; source locking and aliases; fail-fast `SourceBusy` with
all provisional leases released; zero broadcasts on every preparation failure;
block-height expiry; returned-signature mismatch; exact byte replay; three-call
bound; status mapping; cancellation after dispatch;
ambiguous public metadata; no later-item attempt; indexed Memo transactions
counting only the System transfer; every exact JSON-RPC allowlist branch and
malformed-data fallback; status context/cardinality/null/error behavior;
equality at the last valid height; final indexed absence proof; indefinite
guard on unavailable history; response-loss duplicate risk; active-writer
exclusion; checkpoint-triggered/capped reconciliation without a busy loop; and
the documented restart limitation. Every returned JSON-RPC error code must
exercise the same ambiguous wire-boundary path.

## Approval boundary

This proposal consolidates construction, uniqueness, blockhash, fee, balance,
signing, simulation, broadcast, retry, ambiguity, and confirmation policy. It
does not authorize implementation or live submission. Acceptance requires
reconciling the transaction, ambiguity, and error contracts in
`docs/SYSTEM_REQUIREMENTS.md`, `docs/API.md`, and `docs/CONTRACTS.md` before
source changes begin.

## References

- [Solana `getLatestBlockhash`](https://solana.com/docs/rpc/http/getlatestblockhash)
- [Solana `getFeeForMessage`](https://solana.com/docs/rpc/http/getfeeformessage)
- [Solana `simulateTransaction`](https://solana.com/docs/rpc/http/simulatetransaction)
- [Solana `sendTransaction`](https://solana.com/docs/rpc/http/sendtransaction)
- [Solana `getSignatureStatuses`](https://solana.com/docs/rpc/http/getsignaturestatuses)
- [Solana `getBlockHeight`](https://solana.com/docs/rpc/http/getblockheight)
- [Agave RPC custom error codes](https://github.com/anza-xyz/agave/blob/master/rpc-client-api/src/custom_error.rs)
- [Solana Memo interface IDs](https://github.com/solana-program/memo/blob/main/interface/src/lib.rs)
- `sdk/chains/base/src/transaction.rs`
- `sdk/chains/base/src/signer.rs`
- `sdk/wallets/src/sender.rs`
