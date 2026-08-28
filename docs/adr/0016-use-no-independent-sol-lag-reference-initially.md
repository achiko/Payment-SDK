# ADR-0016: Use no independent SOL lag reference initially

## Status

Accepted

## Date

2026-08-27

## Context

ADR-0015 now requires every native SOL destination-observation attempt to pass
one endpoint-bound `getHealth` admission before its opening confirmed `getSlot`.
ADR-0011 through ADR-0014 then establish and preserve the numeric floors `F`,
`H`, and `P` without permitting declared slot regression.

Those decisions trust one configured endpoint-affine RPC context for health,
the base candidate, and destination account observations. They do not establish
whether the endpoint's declared slot is near the cluster's current progress. A
uniformly stale or dishonest endpoint can return `"ok"`, a self-consistent
confirmed base, and account contexts that pass every accepted floor rule.

An SDK-enforced numeric lag rule would need a reference observation `R` whose
trust boundary is independent of the primary endpoint candidate. Reference
selection is separate from choosing a lag threshold, units, comparison,
configuration shape, or failure mapping. S1.6.3.5.3.2.2 is therefore split:

1. S1.6.3.5.3.2.2.1 selects the independent reference, if any, and records the
   inseparable presence or absence of an independently enforceable numeric
   guarantee; and
2. S1.6.3.5.3.2.2.2 decides unsupported lag/reference input behavior, split
   into startup/static configuration under S1.6.3.5.3.2.2.2.1 and public
   transaction input under S1.6.3.5.3.2.2.2.2. ADR-0018 further splits the
   public part by shared destination, single-send envelope, batch item, and
   batch root ownership.

The current product and repository provide no such independent reference:

- one long-lived chain RPC client is shared by indexing and wallet-side
  capabilities;
- generic RPC endpoints are ordered primary/failover transport targets, not
  labeled independently trusted reference roles;
- generic RPC returns a successful result without provider provenance or
  cross-provider aggregation;
- indexing obtains `observed_tip` from that same chain source and defines
  readiness by catching its checkpoint up to that reported tip; and
- no Solana reference endpoint, provider quorum, trust policy, or independent
  cluster-progress service is composed.

Solana's native methods also retain the selected endpoint's trust boundary:

| Candidate | What it establishes | Why it is not independent `R` |
|---|---|---|
| `getSlot` at processed, confirmed, or finalized | A bank slot selected by the responding node | Changing commitment does not change the observer; a uniformly stale node can report normal-looking gaps. |
| `getHealth` | The node's operator-configured health assertion | Success returns only `"ok"`; `Behind` data supplies a server-asserted delta while `Unknown` supplies none, and neither exposes an independently observed absolute slot. |
| `getMaxShredInsertSlot` | The node's local completed-shred insertion maximum | It is a local ingestion diagnostic without commitment or canonical cluster-head semantics. |
| `getMaxRetransmitSlot` | The highest slot seen by the node's retransmit stage | It is another local, uncommitted observation. |
| `getBlockTime` plus local clock | An estimated production time for a caller-selected block | It measures estimated age, not cluster-relative slot lag, and mixes endpoint lag, cluster halt, timestamp estimation, and local-clock error. |
| Finalized indexer checkpoint or readiness | SDK progress relative to the shared source's reported tip | It is same-source, may lag, has different ownership, and is not a current cluster-tip oracle. |
| Another URL in the generic endpoint list | An ordered transport fallback | The configuration states no independent provider role, and one URL may itself front several correlated backends. |

Official Solana confirmation guidance recommends comparing confirmed context
slots from a few different RPC nodes when choosing a fresh blockhash. That
creates comparative observations, but it does not define provider independence,
a fault model, quorum, aggregation, same-genesis validation, sampling
coherence, or failure behavior. Those roles are absent from the approved
one-endpoint native interface.

## Decision

Initial native SOL support has no SDK-owned independently trusted numeric
cluster-progress reference. No value `R` is established or retained for
destination validation.

Consequently, initial native SOL support has no SDK-enforced numeric maximum-
lag guarantee relative to current cluster progress. This is an inseparable
consequence of the empty reference set, not a selected threshold or unit.

The future concrete Solana destination coordinator must not promote any of the
following into an independent reference:

- another commitment level or another method on the primary endpoint;
- `getHealth` success or its unhealthy error data;
- maximum shred-insert or retransmit slots;
- `getBlockTime`, wall clock, or an inferred slot duration;
- an indexing source tip, checkpoint, phase, or application readiness state;
- a balance, blockhash, simulation, or transaction response context; or
- another unlabeled endpoint from generic ordered failover configuration.

ADR-0015 health admission remains mandatory and retains only its coarse,
operator-trusted meaning. `F`, `H`, `P`, and every request-local
`minContextSlot` remain coherence and non-regression guards within the primary
endpoint trust boundary. None becomes `R`, and none proves proximity to the
current cluster head.

The initial destination flow therefore performs no independent-reference RPC,
clock, indexing, readiness, or quorum acquisition:

```text
primary endpoint-bound getHealth
  -> primary confirmed getSlot candidate
  -> accepted coherence and plausibility guards
  -> establish F and H
  -> primary endpoint-bound destination account reads
```

The apparently-future-slot guard was unresolved when this ADR was accepted.
The accepted Destination Account Acquisition ADR now supplies an endpoint-
local closing witness without pretending it is an independent reference. This
ADR does not authorize bypassing that required guard.

No reference state, trait, enum, repository record, transaction snapshot,
public field, RPC abstraction, or configuration is introduced. In particular,
`packages/json-rpc` remains a generic transport and `sdk/indexing` remains the
owner of synchronization state; neither gains Solana freshness or provider-
trust semantics.

If a later product change requires an independently enforced numeric bound,
`apps/api` must explicitly compose role-labeled, genesis-validated reference
provider inputs. The private concrete Solana coordinator would own attempt-
local sampling and comparison. A single reference would be a trusted oracle
and denial-of-service point. A quorum would require an explicit bounded-fault
assumption, genuine provider/operator independence, aggregation, availability,
sampling, and failure rules. None is authorized here.

Same-genesis validation would prevent only accidental cross-cluster mixing. It
would prove neither provider independence, freshness, fork agreement, nor
truthfulness. Independence would remain an operational trust assumption that
cannot be derived from URL count or RPC responses, and a correlated or
dishonest quorum could still cause false-high denial of service or false-low
acceptance.

If a later accepted mode requires reference or quorum evidence, missing,
malformed, wrong-genesis, timed-out, or insufficient evidence must follow that
mode's separately approved fail-closed rule. It must never silently fall back
to this initial no-reference validation path.

The absence of `R` means initial support must not substantiate a claim such as
"within N slots" or "within N seconds" of the current cluster head. Whether
meaningful lag/reference input must be rejected as unsupported belongs to
S1.6.3.5.3.2.2.2. ADR-0017 and S1.6.3.5.3.2.2.2.1 decide its startup/static
configuration part. ADR-0018 and S1.6.3.5.3.2.2.2.2.1 decide the shared public
destination object. ADR-0019 and S1.6.3.5.3.2.2.2.2.2 decide the single-send
envelope. ADR-0020 and S1.6.3.5.3.2.2.2.2.3 decide the batch item;
ADR-0021 and S1.6.3.5.3.2.2.2.2.4.1 decide the batch-root schema.
ADR-0022 and its historical approval label decide cardinality/empty behavior;
the accepted Public Transaction Semantics decision owns order/index and non-
body input behavior.

Acceptance adds the matching canonical reference boundary: initial native SOL
destination validation has no independently trusted numeric cluster-progress
reference and must not treat primary-endpoint methods, generic failover
endpoints, indexer or readiness state, wall clock, or another transaction-
preparation context as one. It therefore must not claim an SDK-enforced numeric
maximum lag relative to current cluster progress.

Acceptance also narrowly clarifies ADR-0014 and ADR-0015 future-step pointers:
S1.6.3.5.3.2.2.1 decides the reference set and inseparable numeric non-
guarantee, while S1.6.3.5.3.2.2.2 decides unsupported lag/reference input
behavior. ADR-0017 later accepts its startup/static configuration part as
S1.6.3.5.3.2.2.2.1. ADR-0018 later accepts the shared public destination part
as S1.6.3.5.3.2.2.2.2.1. ADR-0019 later accepts the single-send envelope as
S1.6.3.5.3.2.2.2.2.2. ADR-0020 later accepts the batch item as
S1.6.3.5.3.2.2.2.2.3. ADR-0021 later accepts the batch-root schema as
S1.6.3.5.3.2.2.2.2.4.1. ADR-0022 later accepts cardinality/empty behavior as
its historical cardinality label. The accepted Public Transaction Semantics
decision owns the remaining order/index behavior.

## Scope boundary

S1.6.3.5.3.2.2.1 decides only that the initial reference set is empty, which
candidate sources are not independent, the resulting absence of an SDK-
enforced numeric maximum-lag guarantee, and the ownership boundary for any
future reference design. It does not decide:

- whether or how meaningful lag/reference configuration is rejected;
- a lag threshold, units, comparison formula, equality rule, or arithmetic;
- a reference endpoint, provider count, quorum, aggregation, fault model,
  sampling window, timeout, or availability rule;
- provider identity, administrative independence, genesis validation, fork
  ancestry, bank hash, authenticated state, or behavior behind load balancers;
- maximum wall-clock observation age or cluster-halt detection;
- apparently future base or account-response slots;
- startup readiness, monitoring, metrics, alerts, or public health endpoints;
- retries, backoff, cancellation, endpoint failover, or which failures create
  a successor attempt;
- exact transport, JSON-RPC, private-domain, or public error mapping;
- the account RPC method, grouping, cardinality, mapping, or encoding; or
- balance, fee, blockhash, signing, simulation, preflight, submission,
  confirmation, or ambiguous-broadcast coherence.

ADR-0017 through ADR-0022 record the accepted startup, exact-input, and
empty-batch boundaries. The accepted Public Transaction Semantics and
Destination Account Acquisition decisions consolidate order/index behavior,
slot plausibility, account acquisition, and mapping. The former nested future
labels are historical.

## Alternatives considered

### Use another commitment from the primary endpoint

Rejected. Processed, confirmed, and finalized select different banks known to
the same node. Their gaps may diagnose that node's local pipeline but do not
provide an independent cluster observation.

### Use `getHealth` or `numSlotsBehind`

Rejected as `R`. ADR-0015 already gives exact `"ok"` its narrow admission
meaning. The server selects the threshold, performs the comparison, may force
healthy, and exposes no independently trusted absolute reference slot.

### Use maximum shred or retransmit slots

Rejected. They are useful node-local data-plane diagnostics without commitment,
canonicality, or independent trust.

### Use block time or an assumed slot duration

Rejected. Block time is estimated and may be unavailable. Slot production and
skipping vary, and a local-clock comparison cannot isolate node-to-cluster lag
from a cluster halt or timestamp error.

### Reuse indexing checkpoint or readiness

Rejected. Those values derive from the shared chain source, may legitimately
lag, and belong to canonical synchronization. Reuse would neither add
independence nor respect indexing ownership.

### Treat generic failover endpoints as a reference set

Rejected. Ordered fallback selects a transport result; it supplies no endpoint
role, provenance, independence assertion, or aggregation. Reference fan-out is
not failover.

### Add one secondary endpoint

Deferred as a separate product trust expansion. It would provide only a bound
relative to one trusted oracle: a false-high value could deny every send, while
a false-low, stale, or correlated value could admit a stale primary.

### Add three or more reference providers

Deferred. Provider count alone is not a quorum guarantee. A meaningful design
must define independence, permitted faulty providers, aggregation, coherent
sampling, failure policy, same-genesis validation, and correlated-failure risk.
Same genesis prevents accidental cluster mixing but proves neither independence
nor truthful or fresh data. A correlated or dishonest quorum can still cause
false-high denial of service or false-low acceptance.

## Consequences

### Positive

- The SDK does not mislabel correlated or same-source values as independent
  evidence.
- Initial destination validation adds no reference infrastructure, cross-crate
  dependency, clock policy, or provider-trust state.
- Accepted health and monotonic-floor guards keep their precise meanings.

### Negative

- A uniformly stale but internally consistent primary endpoint can still pass
  health and every numeric non-regression guard.
- Initial support has no independently enforceable bound relative to current
  cluster progress.
- Adding such a bound later requires explicit application composition and a
  larger operational trust model.

### Neutral

- No reference request is made, so there is no reference outage or quorum
  failure path in initial destination validation.
- This decision neither weakens nor strengthens protection against a false-high
  primary candidate; the accepted Destination Account Acquisition decision
  owns that plausibility question.
- Bitcoin, Ethereum, generic RPC, and indexing behavior remain unchanged.
- No implementation or test code is authorized by S1.6.3.5.3.2.2.1.

## Validation requirements

Focused future tests must prove:

- destination validation performs no processed, finalized, max-shred,
  max-retransmit, block-time, clock, indexing, readiness, secondary-endpoint,
  or quorum acquisition for an independent lag reference;
- `getHealth` remains mandatory but creates no reference slot or threshold;
- `F`, `H`, `P`, and `minContextSlot` never become independent-reference state;
- generic ordered endpoint failover cannot contribute, vote, or aggregate a
  reference observation;
- no indexer checkpoint, observed tip, sync phase, or readiness value is read by
  the destination coordinator for freshness admission;
- an internally consistent stale RPC double can pass this reference-selection
  decision when every other separately approved guard passes, demonstrating
  that no independent freshness claim is being made;
- no reference state is persisted, cached, configured, publicly exposed, or
  stored in a transaction snapshot;
- if a later accepted reference-required mode is configured, unavailable,
  malformed, wrong-genesis, timed-out, or insufficient reference evidence can
  never fall back to the no-reference path; and
- Bitcoin and Ethereum behavior remains unchanged.

## Approval boundary

Decision `S1.6.3.5.3.2.2.1` was explicitly approved on 2026-08-27. Acceptance
records only the empty initial reference set, its inseparable SDK numeric non-
guarantee, the matching canonical reference-boundary correction, and the
narrow ADR-0014 and ADR-0015 future-step clarifications. It does not authorize
Solana source, wallet, transaction, RPC, configuration, dependency, API, or
test implementation, and it does not approve S1.6.3.5.3.2.2.2,
S1.6.3.5.3.3, S1.6.3.6, or later decisions.

## References

- [Solana commitment configuration](https://solana.com/docs/rpc#configuring-state-commitment)
- [Solana `getSlot`](https://solana.com/docs/rpc/http/getslot)
- [Solana `getHealth`](https://solana.com/docs/rpc/http/gethealth)
- [Solana `getMaxShredInsertSlot`](https://solana.com/docs/rpc/http/getmaxshredinsertslot)
- [Solana `getMaxRetransmitSlot`](https://solana.com/docs/rpc/http/getmaxretransmitslot)
- [Solana `getBlockTime`](https://solana.com/docs/rpc/http/getblocktime)
- [Solana `getGenesisHash`](https://solana.com/docs/rpc/http/getgenesishash)
- [Solana confirmation guidance](https://solana.com/developers/cookbook/transactions/confirmation#use-healthy-rpc-nodes-when-fetching-blockhashes)
- [Solana cluster endpoint documentation](https://solana.com/docs/references/clusters)
- [Reviewed Agave revision](https://github.com/anza-xyz/agave/commit/1f6e0fbf3d8ae4a37364a1d9eb31c6b5bcac8869)
- [Agave commitment bank selection](https://github.com/anza-xyz/agave/blob/1f6e0fbf3d8ae4a37364a1d9eb31c6b5bcac8869/rpc/src/rpc.rs#L350-L399)
- [Agave `getSlot` implementation](https://github.com/anza-xyz/agave/blob/1f6e0fbf3d8ae4a37364a1d9eb31c6b5bcac8869/rpc/src/rpc.rs#L975-L991)
- [Agave RPC health implementation](https://github.com/anza-xyz/agave/blob/1f6e0fbf3d8ae4a37364a1d9eb31c6b5bcac8869/rpc/src/rpc_health.rs#L60-L133)
- [Agave `getHealth` result mapping](https://github.com/anza-xyz/agave/blob/1f6e0fbf3d8ae4a37364a1d9eb31c6b5bcac8869/rpc/src/rpc.rs#L2871-L2883)
- [Agave completed-shred update](https://github.com/anza-xyz/agave/blob/1f6e0fbf3d8ae4a37364a1d9eb31c6b5bcac8869/core/src/completed_data_sets_service.rs#L205-L210)
- [Agave retransmit-slot update](https://github.com/anza-xyz/agave/blob/1f6e0fbf3d8ae4a37364a1d9eb31c6b5bcac8869/turbine/src/retransmit_stage.rs#L401-L410)
- [Agave `getBlockTime` implementation](https://github.com/anza-xyz/agave/blob/1f6e0fbf3d8ae4a37364a1d9eb31c6b5bcac8869/rpc/src/rpc.rs#L1613-L1644)
- `ARCHITECTURE.md`
- `apps/api/src/config.rs`
- `packages/json-rpc/src/client.rs`
- `packages/json-rpc/src/http.rs`
- `sdk/indexing/src/service.rs`
- `sdk/indexing/src/synchronizer.rs`
- `sdk/indexing/runtime/src/lib.rs`
- `docs/SYSTEM_REQUIREMENTS.md`
- `docs/adr/0011-anchor-sol-destination-reads-to-one-confirmed-attempt-slot.md`
- `docs/adr/0012-reject-sol-destination-responses-below-the-requested-floor.md`
- `docs/adr/0013-chain-sol-destination-reads-through-a-monotonic-floor.md`
- `docs/adr/0014-carry-sol-destination-progress-across-operation-retries.md`
- `docs/adr/0015-gate-sol-destination-reads-on-native-rpc-health.md`
