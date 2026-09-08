# ADR-0015: Gate SOL destination reads on native RPC health

## Status

Accepted

## Date

2026-08-27

## Context

ADR-0011 establishes one confirmed `getSlot` base `F` for each native SOL
destination-observation attempt. ADR-0013 advances an attempt-local high-water
floor `H`, and ADR-0014 carries only the greatest accepted numeric floor `P`
into a separately authorized successor attempt inside the same live send
operation.

Those guards prevent declared slot regression. They do not prove that the
selected RPC node is close to the cluster's current progress. A uniformly
stale node can return internally consistent confirmed slots and account
contexts that satisfy every `F`, `H`, `P`, and `minContextSlot` rule.

Solana exposes the native no-parameter `getHealth` method for node health.
Its successful result is the exact JSON string `"ok"`. Current Agave compares
the node's latest replayed optimistically confirmed bank with a cluster-progress
reference using an operator-configured slot distance. Before Alpenglow that
reference is the latest optimistic-confirmed slot learned through gossip and
blockstore; with Alpenglow enabled it is the highest finalized certificate.
An unhealthy or unknown state is represented as a JSON-RPC error rather than a
successful result.

That signal is deliberately weaker than an SDK-owned freshness guarantee:

- the health distance is selected by the node operator rather than the SDK;
- a successful response exposes neither the actual distance nor the configured
  threshold;
- Agave has internal override paths that can force a healthy result; disabling
  its health evaluation produces `"ok"`, and the client cannot detect that
  bypass;
- an eclipsed or dishonest endpoint can claim `"ok"` against an incomplete or
  fabricated view; and
- one load-balanced URL does not prove that health and account requests reach
  the same backend process.

Other same-node signals do not repair this boundary. A processed-versus-
confirmed gap, maximum shred-insert slot, maximum retransmit slot, finalized
slot, block time, or local indexing checkpoint describes some part of the
selected node or SDK's own progress. None independently proves proximity to
the current cluster head.

There are therefore two separate questions:

1. whether every destination-observation attempt should require the chain's
   native coarse health admission before acquiring state; and
2. whether initial support can enforce an SDK-selected numeric maximum lag
   against an independent reference.

The historical split kept native health admission separate from a numeric
freshness claim. ADR-0016 records that initial support has no independent
reference or SDK-enforced numeric maximum-lag guarantee. ADR-0017 through
ADR-0022 record the accepted startup, exact-input, and empty-batch boundaries.
The simplified named ADR set now consolidates this behavior according to each
ADR's recorded status; it does not alter this accepted health decision.

## Decision

Every native SOL destination-observation attempt must begin with one standard
Solana `getHealth` request through the same endpoint-affine RPC context selected
for that attempt's opening confirmed `getSlot` and destination account reads.
The method has no semantic parameters. Only a structurally valid successful
result containing the exact JSON string `"ok"` admits the attempt.

"One request" means one endpoint-bound wire execution. The health admission
must use a path that performs no transparent transport retry or endpoint
failover. A failed execution closes the attempt; this decision does not
authorize a second health wire call inside that attempt. A later attempt may
repeat health only when separately approved retry or reacquisition behavior
creates that successor.

```text
select one endpoint-affine attempt context
  -> getHealth() == "ok"
  -> confirmed getSlot(minContextSlot = P when P exists) -> F candidate
  -> validate every approved candidate guard
  -> establish F and H
  -> acquire destination accounts through the approved floor chain
```

A transport failure, JSON-RPC error, unsupported method, malformed envelope,
malformed result, or any successful result other than exact `"ok"` must close
that attempt before its opening `getSlot`. Such a failure establishes no `F`
or `H`, issues no destination account request, produces no eligibility
handoff, and reaches no signing, simulation, or broadcast.

The health result is an ephemeral admission bit for exactly one attempt. It
must not be cached for another attempt, inherited from a predecessor, persisted,
configured, exposed as wallet or transaction state, stored in an indexing
checkpoint, or used as a transaction snapshot field. Every separately
authorized successor attempt repeats `getHealth`, including a successor inside
the same live operation.

For a successor governed by ADR-0014, `getHealth` runs before the constrained
opening `getSlot`. A health failure neither consumes nor changes the operation's
existing `P`. If another successor is separately authorized, it repeats health
and still sends the exact applicable opening floor. A health success supplies
no slot and cannot create, lower, raise, replace, or validate `F`, `H`, or `P`.

Current Agave maps both `Behind` and `Unknown` to JSON-RPC error `-32005`.
Its `data.numSlotsBehind` is a JSON number for `Behind` and `null` for
`Unknown`. Neither form is an accepted cluster-reference slot, request floor,
current slot, or threshold. Error data must not modify destination-validation
state or permit continuation or recovery through another request inside the
failed attempt. It cannot consume or modify `P`; a later successor remains
possible only under separately approved retry or reacquisition behavior. Exact
private and public error mapping remains a later boundary decision.

Health and destination acquisition must remain in the same logical
endpoint-affine attempt context. The implementation must not check one
configured endpoint and silently obtain `getSlot` or account state through
another endpoint. This requirement cannot verify physical backend identity
behind a provider-controlled load balancer; backend affinity and truthful
health reporting remain explicit deployment trust assumptions.

The health check is not commitment-specific and carries no `minContextSlot`.
Its success must not substitute for ADR-0011's confirmed `getSlot`, ADR-0012's
response-floor validation, ADR-0013's causal floor chain, ADR-0014's operation
floor, or any later accepted lag or future-slot guard. A node can become stale
immediately after responding, so `"ok"` is admission evidence only at that
point in the attempt.

This decision intentionally makes an unavailable, unsupported, rejected, or
failed `getHealth` request fail closed for native SOL destination validation.
It does not add an SDK lag threshold, reference endpoint, provider quorum,
wall-clock policy, readiness probe, background monitor, retry, or failover
rule. It also does not claim resistance to a malicious node, eclipse,
provider-wide stale view, cluster halt, or backend switch hidden behind one
URL.

The future private concrete Solana destination coordinator owns the ordering
between attempt-local health admission, the opening base request, and account
acquisition. The concrete Solana RPC adapter owns native request construction
and exact result decoding. `packages/json-rpc` continues to own generic
framing, IDs, response correlation, and transport behavior; it gains no
Solana health meaning. `sdk/chains/base`, `sdk/wallets`, `sdk/indexing`, and
persistence gain no health abstraction or state.

Acceptance adds the matching canonical requirement: before each native SOL
destination-observation attempt obtains its confirmed `getSlot` base, it must
receive exact `"ok"` from the no-parameter `getHealth` method through the same
endpoint-affine RPC context. Any failed, unsupported, malformed, or non-`"ok"`
response stops the attempt before base or account acquisition. The result is
ephemeral, repeated for every attempt, and supplies no slot or SDK-enforced
numeric lag guarantee.

Acceptance also narrowly clarifies ADR-0011 and ADR-0014. `getHealth` becomes
the attempt's first RPC admission call, while the opening confirmed `getSlot`
remains its first slot-bearing acquisition and the only call that can establish
the base `F`. ADR-0014's placement of an inherited `P` on the successor's
opening `getSlot` remains unchanged, but that request is no longer the
successor's first RPC call after health admission is required.

## Scope boundary

S1.6.3.5.3.2.1 decides only the mandatory attempt-local native health call,
its exact success condition, placement before the opening base request,
failure boundary, lifetime, and ownership. It does not decide:

- an SDK-enforced numeric maximum node lag, reference, threshold, or units;
- whether an independent reference endpoint or provider quorum will exist;
- reference-endpoint identity, independence, genesis validation, sampling
  window, aggregation, failure policy, or configuration;
- processed-versus-confirmed, shred, retransmit, finalized, block-time,
  indexing-checkpoint, or wall-clock thresholds;
- apparently future base or account-response slots;
- physical backend identity or affinity behind a load-balanced URL;
- malicious RPC, eclipse, fork ancestry, bank identity, cluster halt, or
  authenticated account-state proofs;
- startup readiness, liveness monitoring, metrics, alerts, or public health
  endpoints;
- retries, retry count, backoff, cancellation, endpoint failover, or which
  failures may create a successor attempt, except that this health admission
  itself is one endpoint-bound wire execution with no transparent retry or
  failover;
- exact diagnostic retention or private/public error mapping;
- the account RPC method, grouping, mapping, cardinality, encoding, or
  authoritative total-length proof; or
- balance, fee, blockhash, signing, simulation, preflight, submission,
  confirmation, or ambiguous-broadcast coherence.

ADR-0016 through ADR-0022 record the accepted no-reference, exact-input, and
empty-batch boundaries. The accepted Public Transaction Semantics and
Destination Account Acquisition decisions consolidate order/index behavior,
slot plausibility, account acquisition, and mapping. The former nested future
labels are historical.

## Alternatives considered

### Make no native health call

Rejected for the initial interface. It avoids one round trip and
avoids relying on operator configuration, but it ignores Solana's purpose-built
coarse signal when an honest node already knows it is lagging or unhealthy.
The accepted check must remain explicitly labeled as operator-trusted
admission rather than an SDK numeric guarantee.

### Treat `"ok"` as proof of a fixed maximum lag

Rejected. The client cannot observe or select the server's threshold from a
successful result, the threshold is version- and operator-dependent, and the
server may override health. Naming a fixed SDK bound would be unsupported.

### Use `numSlotsBehind` from an unhealthy error

Rejected as semantic input. It appears only on an error, supplies no trusted
reference slot or configured threshold, and cannot make the failed attempt
eligible. It may be useful for separately designed diagnostics.

### Cache health at startup or across attempts

Rejected. Health changes over time, and a cached success does not admit a
later attempt. Startup readiness and background health monitoring are separate
runtime policies.

### Call health after the opening base or account reads

Rejected. A failed admission would allow disallowed acquisition work and would
make the attempt's failure boundary less direct. Health must precede every
destination state acquisition in that attempt.

### Infer health from same-node slot, shred, time, or indexing signals

Rejected. These signals have distinct local meanings and cannot independently
establish current cluster proximity. Combining them would invent thresholds
without improving the trust boundary.

### Compare an independent endpoint or quorum here

Rejected for initial support by ADR-0016. Any later product expansion would
require explicit provider roles, same-genesis validation, administrative
independence assumptions, aggregation, threshold, sampling, configuration, and
failure policy. It must not be smuggled into a native one-endpoint admission
check.

## Consequences

### Positive

- An honest RPC node that reports itself unhealthy cannot supply destination
  eligibility evidence.
- Every attempt uses Solana's native health contract before reading state.
- Exact `"ok"` handling is small, deterministic, and fail closed.
- Health admission remains separate from every numeric slot floor and future
  independent-reference decision.

### Negative

- Every destination-observation attempt adds one sequential RPC round trip.
- A provider that omits, filters, rejects, or transiently fails `getHealth`
  blocks native SOL destination validation even if its account methods would
  respond.
- A misconfigured or overridden node can return `"ok"` while too stale for the
  deployment's expectations.

### Neutral

- This decision does not establish a numerical maximum lag or wall-clock age.
- A malicious or eclipsed endpoint can still fabricate a coherent healthy
  view; the configured RPC remains trusted for truthful responses.
- Bitcoin and Ethereum behavior and generic RPC semantics remain unchanged.
- No implementation or test code is authorized by S1.6.3.5.3.2.1.

## Validation requirements

Focused future tests must prove:

- exact `"ok"` from `getHealth` precedes the opening confirmed `getSlot` and
  every destination account request;
- a transport failure, JSON-RPC error, unsupported method, malformed envelope,
  malformed result, or successful non-`"ok"` result produces no base, account
  request, eligibility handoff, signing, simulation, or broadcast;
- one logical health admission emits exactly one endpoint-bound wire request,
  and generic transport retry or endpoint failover cannot make it succeed;
- the check is repeated for every separately authorized successor attempt and
  is never satisfied by a cached predecessor or startup result;
- a successor's health failure leaves its operation-local `P` unchanged, and
  a later separately authorized successor still applies that exact floor;
- health success never creates, lowers, raises, replaces, or validates `F`,
  `H`, `P`, or a request-local `minContextSlot`;
- `numSlotsBehind` or other error data never changes destination-validation
  state or permits recovery inside the failed attempt;
- health, opening `getSlot`, and destination account calls use the same
  logical endpoint-affine context with no hidden generic failover;
- a late health response from a closed or cancelled attempt is ignored;
- no SDK numeric lag threshold, public per-send override, persisted health
  state, indexing coupling, or transaction-snapshot field is introduced by
  this decision;
- existing commitment and floor guards remain mandatory after health success;
  and
- Bitcoin and Ethereum behavior remains unchanged.

## Approval boundary

Decision `S1.6.3.5.3.2.1` was explicitly approved on 2026-08-27. Acceptance
records only the native `getHealth` admission decision, the matching canonical
requirements correction, and the narrow ADR-0011 and ADR-0014 ordering
clarifications. It does not authorize Solana source, wallet, transaction, RPC,
configuration, dependency, API, or test implementation, and it does not
approve S1.6.3.5.3.2.2, S1.6.3.5.3.3, S1.6.3.6, or later decisions.

## References

- [Solana `getHealth`](https://solana.com/docs/rpc/http/gethealth)
- [Solana `getSlot`](https://solana.com/docs/rpc/http/getslot)
- [Reviewed Agave revision](https://github.com/anza-xyz/agave/commit/1ec6a9ae33ee18a91a79fa5f184e333945b9e00d)
- [Agave RPC health implementation at reviewed revision](https://github.com/anza-xyz/agave/blob/1ec6a9ae33ee18a91a79fa5f184e333945b9e00d/rpc/src/rpc_health.rs#L60-L134)
- [Agave `getHealth` handler at reviewed revision](https://github.com/anza-xyz/agave/blob/1ec6a9ae33ee18a91a79fa5f184e333945b9e00d/rpc/src/rpc.rs#L2871-L2883)
- [Agave unhealthy error mapping at reviewed revision](https://github.com/anza-xyz/agave/blob/1ec6a9ae33ee18a91a79fa5f184e333945b9e00d/rpc-client-api/src/custom_error.rs#L150-L165)
- [Agave validator health-distance configuration at reviewed revision](https://github.com/anza-xyz/agave/blob/1ec6a9ae33ee18a91a79fa5f184e333945b9e00d/validator/src/commands/run/args/json_rpc_config.rs#L23-L66)
- [Agave health-distance constant at reviewed revision](https://github.com/anza-xyz/agave/blob/1ec6a9ae33ee18a91a79fa5f184e333945b9e00d/rpc-client-types/src/request.rs#L166-L167)
- [Agave RPC health override configuration at reviewed revision](https://github.com/anza-xyz/agave/blob/1ec6a9ae33ee18a91a79fa5f184e333945b9e00d/rpc/src/rpc.rs#L162-L214)
- [Agave validator health-override wiring at reviewed revision](https://github.com/anza-xyz/agave/blob/1ec6a9ae33ee18a91a79fa5f184e333945b9e00d/core/src/validator.rs#L1262-L1263)
- [Agave temporary force-healthy path at reviewed revision](https://github.com/anza-xyz/agave/blob/1ec6a9ae33ee18a91a79fa5f184e333945b9e00d/core/src/validator.rs#L3074-L3080)
- `docs/SYSTEM_REQUIREMENTS.md`
- `docs/adr/0008-treat-sol-destination-observations-as-one-attempt.md`
- `docs/adr/0011-anchor-sol-destination-reads-to-one-confirmed-attempt-slot.md`
- `docs/adr/0012-reject-sol-destination-responses-below-the-requested-floor.md`
- `docs/adr/0013-chain-sol-destination-reads-through-a-monotonic-floor.md`
- `docs/adr/0014-carry-sol-destination-progress-across-operation-retries.md`
- `docs/adr/0016-use-no-independent-sol-lag-reference-initially.md`
- `packages/json-rpc/src/client.rs`
- `packages/json-rpc/src/lib.rs`
