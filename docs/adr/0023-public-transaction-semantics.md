# ADR-0023: Public Transaction Semantics

## Status

Proposed

## Date

2026-08-27

## Context

The public API already has one single-send route and one batch-send route. The
accepted Solana design keeps those routes chain-neutral, requires exact JSON
objects, and rejects an empty batch without inventing item zero. It does not
yet define the maximum batch size, occurrence identity, duplicate behavior,
error precedence, or non-body request controls.

Those rules must be fixed before a Solana sender groups account reads. A grouped
RPC call is an implementation detail and must not reorder, merge, or renumber
the payments the caller submitted.

## Decision

### Public shape and bounds

The existing routes and DTOs remain the only transaction interface. Solana adds
`sol`, `solana`, and canonical `base58` variants to the existing closed public
enums; it does not add a Solana-specific route or request wrapper.

`TransferRequest.transfers` contains from 1 through 50 items. The accepted
empty-batch contract remains unchanged. More than 50 items is the index-free
collection error `InvalidBatch`, rendered as `400 Bad Request` with message
`at most 50 transfers are allowed`, no accepted IDs, no failed index, and no
sender or RPC call. OpenAPI publishes `minItems: 1`, `maxItems: 50`, and no
`uniqueItems` rule.

`Wallets::send_all` is the authoritative minimum-and-maximum guard for HTTP and
direct SDK callers. The HTTP adapter also applies the same exported maximum
before converting any item, so a 51-item body cannot make an invalid amount win
error precedence. The accepted empty-array path still reaches the authoritative
`Wallets` minimum guard. Every concrete sender defensively rejects an impossible
out-of-contract count before chain I/O.

Fifty is one shared product bound, not a Solana-only hidden limit. It lets
native SOL preparation inspect at most 100 unique source/destination addresses
in one protocol-supported account request and prevents a direct SDK caller from
bypassing that bound.

### Occurrence identity, order, and duplicates

For a batch, an item's identity is its zero-based position in the authored JSON
array. API conversion, wallet lookup, common validation, and the registered
`Sender` handoff preserve the exact length, order, and multiplicity of that
list. They must not sort, group, deduplicate, or renumber it.

Repeated wallet IDs, destinations, amounts, and even identical items are
separate requested payment occurrences. Internal RPC deduplication may reuse
one observation, but every result and error maps back to the original
occurrence. Solana must produce distinct signed transaction identities for
distinct occurrences; the Native SOL Submission ADR owns that mechanism.

### Error precedence

The fixed precedence is:

1. existing transport and authentication rejection;
2. JSON shape and exact-schema rejection;
3. collection cardinality, first minimum and then maximum;
4. conversion of every wire item in original order, stopping at the first
   conversion error;
5. positive-amount validation, wallet resolution, and family compatibility in
   original order;
6. chain-specific full-batch preparation in the stage order defined by that
   chain; and
7. ordered broadcast, stopping at the first failed or ambiguous occurrence.

A pre-broadcast item error reports the first original occurrence that fails the
current deterministic stage and has no accepted prefix. A batch-wide RPC,
coherence, or resource failure that cannot truthfully be assigned to one item
has no failed index. Once broadcasting begins, accepted IDs describe only the
definitely acknowledged prefix, `failed_index` remains the original item
position, and no later item is attempted.

The generic contract does not equate accepted-ID count with `failed_index`
because a grouped chain may represent several public occurrences with one
transaction. For a Solana broadcast-stage failed or ambiguous outcome, the
one-to-one mapping means `accepted.len() == failed_index`. A Solana
pre-broadcast item error has no accepted prefix even when its original index is
greater than zero. Success means submitted, not confirmed. Confirmation and
terminal failure continue to come from indexing.

### Error representation and ambiguous identity

The chain-neutral transaction and wallet errors gain an optional canonical
`ambiguous_transaction_id: Id`. `SendError.failed_index` becomes optional and
`SendError` also carries that optional ID. The HTTP `ErrorBody` projects the
same optional string field. It is present only when exact signed bytes may have
been submitted but acceptance cannot yet be proved. Definite failures and
chains without that condition omit it.

One-wallet and batch errors remain distinct:

- a single-send ambiguity is `503 Service Unavailable` with
  `ambiguous_transaction_id`, no `transaction_ids`, and no `failed_index`;
- a batch ambiguity is `503` with only the definitely acknowledged
  `transaction_ids`, the original ambiguous item `failed_index`, and its
  `ambiguous_transaction_id`; and
- a pre-broadcast item failure has no transaction IDs or ambiguous ID; its
  batch form carries only the original `failed_index`, while its single-send
  form has no index; and
- a pre-broadcast collection or operation failure has no accepted IDs, failed
  index, or ambiguous ID.

An ambiguous ID is reconciliation metadata, not proof of submission and not an
idempotency key. Its presence always maps to `503`, regardless of provider
prose.

### Query parameters and headers

Both transaction POST routes reject any non-empty URI query string with
`400 Bad Request` and `transaction query parameters are not supported` before
request conversion or wallet delegation.

There is no transaction-control header contract. Normal HTTP, proxy,
authentication, content-negotiation, and tracing headers remain permitted and
are not interpreted as lag, reference, commitment, retry, or priority-fee
controls. An arbitrary header therefore cannot change send semantics, and a
caller must not treat it as an accepted safety option. Any future application
header requires an explicit public-contract decision.

## Consequences

- Callers receive stable original indices even when concrete chains optimize
  reads internally.
- Intentional duplicate payments remain expressible.
- The shared bound controls RPC, signing, simulation, and memory fan-out.
- Unknown query parameters fail closed, while ordinary HTTP infrastructure can
  continue to add headers.
- `SendError` and the HTTP error body must support both indexed item failures
  and index-free collection or operation failures.

## Alternatives considered

### Preserve no maximum

Rejected. Body-byte limits do not bound account reads, fee calls, signatures,
simulations, or coordinator state.

### Deduplicate identical payment items

Rejected. Two identical payments may be intentional, and silently collapsing
them changes value flow.

### Reject all unknown HTTP headers

Rejected. Standard clients and infrastructure add headers outside the domain
schema. Those headers are transport metadata, not a hidden option channel.

## Validation requirements

Tests must cover 0, 1, 50, and 51 items; repeated and identical items; aliasing
wallet IDs; stable original indices through every common validation failure;
index-free collection and operation errors; accepted-prefix behavior; query
rejection; ordinary unknown-header tolerance; and OpenAPI bounds without
`uniqueItems`. They must also cover a 51-item body with an earlier malformed
amount, direct-SDK cardinality enforcement, single and batch ambiguity bodies,
field omission for definite failures, and Solana's one-to-one prefix invariant.

## Approval boundary

This proposal consolidates the former order, index, duplicate, maximum-size,
error-precedence, query, and header questions. Approval records the contract
but does not authorize Rust implementation. Acceptance also requires replacing
the current no-maximum language in `docs/API.md` and
`docs/SYSTEM_REQUIREMENTS.md`; those canonical documents remain unchanged while
this ADR is Proposed.

## References

- `apps/api/src/api/transaction.rs`
- `apps/api/src/api/error.rs`
- `sdk/wallets/src/wallets.rs`
- `sdk/wallets/src/sender.rs`
- `docs/API.md`
- `docs/SYSTEM_REQUIREMENTS.md`
- [Solana `getMultipleAccounts`](https://solana.com/docs/rpc/http/getmultipleaccounts)
