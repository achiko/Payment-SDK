# ADR-0017: Reject unsupported SOL lag-reference startup configuration

## Status

Accepted

## Date

2026-08-27

## Context

ADR-0016 establishes that initial native SOL support has no independently
trusted numeric cluster-progress reference and therefore no SDK-enforced
numeric maximum-lag guarantee. The remaining product boundary is how the SDK
responds when an operator nevertheless supplies configuration that appears to
enable such a guarantee.

Startup configuration and public transaction input are different adapters:

- `apps/api` reads one JSON configuration before constructing RPC clients,
  storage, synchronizers, wallets, or the public listener; and
- single-send and batch request bodies are separately decoded after the
  process is running and have an HTTP error boundary.

S1.6.3.5.3.2.2.2 is therefore split again:

1. S1.6.3.5.3.2.2.2.1 decides only startup/static configuration; and
2. S1.6.3.5.3.2.2.2.2 decides public transaction input, subsequently split by
   ADR-0018 into shared destination, single-send envelope, batch item, and
   batch root ownership.

The root `Config`, `IndexConfig`, and nested `RpcConfig` declare
`#[serde(deny_unknown_fields)]`. The Bitcoin and Ethereum chain objects also
declare it but flatten `SyncConfig`; Serde documents that
`deny_unknown_fields` is not supported in combination with `flatten`.
Therefore the future Solana schema cannot copy that combination and assume it
enforces exact keys. `Config::read` deserializes and validates the file before
application composition. Existing `RpcConfig.endpoints` is an ordered
transport list whose first member is primary and whose later members are
failovers; that ordering supplies no independently trusted reference role,
response provenance, provider voting, or quorum meaning.

There is no implemented `SolanaConfig` yet. This ADR does not select its full
shape. It decides only that the initial Solana schema must not create a field
that falsely suggests an unavailable lag/reference capability.

An unsupported setting must not be modeled as optional-but-disabled. Doing so
would expand the configuration contract and make omission, `null`, zero,
false, an empty collection, and an explicit disabled mode appear to be
supported alternatives. Accepting and ignoring a meaningful field would be
worse: an operator could believe a safety bound is active when the SDK has no
reference with which to enforce it.

## Decision

The future initial Solana startup configuration, including every nested object
in which an operator could attempt these settings, must enforce an exact closed
schema end to end. It must expose no maximum-lag, reference-provider,
provider-role, quorum, reference-sampling, or reference-fallback field. The
implementation must not rely on the unsupported combination of
`deny_unknown_fields` and `flatten`, or use a catch-all map or permissive custom
deserializer. It may instead use non-flattened closed Serde objects or an
explicit exact-key parser with equivalent rejection behavior. Field absence is
the only valid initial no-reference representation of the unsupported
capability.

Representative attempted keys include names such as:

- `max_lag_slots` or `max_lag_seconds`;
- `reference_endpoint` or `reference_endpoints`;
- `reference_provider` or `reference_providers`;
- `reference_quorum` or `min_reference_responses`; and
- `allow_unbounded_lag`, `freshness_mode`, or another explicit disabled/no-
  reference selector.

These names are test probes, not reserved aliases or recognized fields. Every
unknown member is rejected by the closed schema regardless of its spelling.
The SDK must not add aliases that convert representative names into accepted
configuration.

Presence must fail configuration loading regardless of value shape or apparent
disabled meaning, including:

```text
positive or zero number
null
false
empty string
empty array
empty object
```

No default, coercion, warning, log entry, or later semantic validation may turn
such a structurally unsupported member into an accepted configuration. The
failure occurs during `apps/api::Config::read` deserialization, so explicit
`Config::validate` and application composition are never reached. No RPC
client, database, indexing task, wallet, readiness task, or listener is
constructed or started.

The absence rule applies only to the initial no-reference mode established by
ADR-0016. If a separately approved future schema selects a mode that requires
reference or quorum evidence, omitted, incomplete, or invalid reference
configuration must fail startup. It must not be interpreted as this initial
absence case or silently downgrade to the no-reference path.

The result is an application startup configuration error. It is not a Solana
RPC error, wallet-domain error, HTTP status, retryable destination attempt, or
runtime health state. Exact diagnostic text and source-location presentation
remain implementation details, but the error must identify invalid
configuration rather than silently continuing.

The generic `rpc.endpoints` member retains only its existing transport meaning.
If a separately approved Solana endpoint-cardinality policy permits several
URLs, none gains reference or quorum meaning from list membership. They must not
be sampled, compared, counted, voted, or reinterpreted as independent lag
references by position, naming convention, headers, or operator intent. This
ADR does not decide how many endpoints Solana may configure or how an endpoint-
affine operation selects one; it decides only that a transport list is not a
reference set.

No environment-variable, command-line, dynamic-reload, persisted, HTTP, wallet,
transaction, or per-send lag/reference configuration surface is introduced.
An arbitrary unused process environment variable is not a supported
configuration input and need not be discovered; the supported structured
configuration contract simply exposes no such option.

`apps/api` owns the future closed Solana configuration type, deserialization,
startup validation boundary, and composition. The concrete Solana chain crate,
`sdk/chains/base`, `sdk/wallets`, `sdk/indexing`, persistence, and
`packages/json-rpc` gain no lag/reference configuration type or state.

Acceptance adds the matching canonical startup rule: initial Solana application
configuration defines no lag/reference/quorum option; any attempted meaningful
member is rejected by exact closed-schema loading before application
composition or runtime side effects regardless of a null, zero, false, empty,
or apparently disabled value. Generic RPC endpoints remain transport/failover
inputs only and do not become reference evidence.

Acceptance also narrowly clarifies ADR-0014 through ADR-0016 future-step
pointers: S1.6.3.5.3.2.2.2.1 governs startup/static configuration only, while
S1.6.3.5.3.2.2.2.2 governs public transaction input only. ADR-0018 later
accepts its shared destination part as S1.6.3.5.3.2.2.2.2.1. ADR-0019 later
accepts the single-send envelope as S1.6.3.5.3.2.2.2.2.2. ADR-0020 later
accepts the batch item as S1.6.3.5.3.2.2.2.2.3. ADR-0021 later accepts the
batch-root schema as S1.6.3.5.3.2.2.2.2.4.1. ADR-0022 later accepts its
cardinality/empty behavior under its historical approval label. Public
Transaction Semantics would own order/index and non-body input behavior upon
acceptance.

## Scope boundary

S1.6.3.5.3.2.2.2.1 decides only absence and closed-schema rejection of
startup/static lag/reference configuration, the pre-composition/runtime-effect
failure boundary, generic endpoint non-reference meaning, and ownership. It
does not decide:

- public single-send, batch-top-level, or per-transfer request members;
- HTTP deserialization, status, error body, or OpenAPI behavior;
- the complete accepted Solana application configuration shape;
- whether Solana accepts one or several transport endpoints;
- endpoint selection, affinity, retry, backoff, or failover behavior;
- a reference endpoint, provider count, quorum, threshold, unit, aggregation,
  sampling, fault model, or future supported configuration;
- exact startup diagnostic wording, JSON path formatting, or process exit code;
- startup readiness, runtime monitoring, metrics, alerts, or public health;
- apparently future base or account-response slots;
- the account RPC method, grouping, mapping, cardinality, or encoding; or
- balance, fee, blockhash, signing, simulation, preflight, submission,
  confirmation, or ambiguous-broadcast coherence.

ADR-0018 through ADR-0022 record the accepted exact-input and empty-batch
boundaries. If accepted, the named Public Transaction Semantics and Destination
Account Acquisition proposals would consolidate order/index behavior,
slot-plausibility, account acquisition, and mapping. The former nested future
labels become historical only upon that acceptance.

## Alternatives considered

### Add optional lag/reference fields with no value

Rejected. `Option` fields would make the unsupported feature part of the
accepted configuration contract and create ambiguity between omission,
`null`, and later activation.

### Treat zero, false, null, or empty values as disabled

Rejected. Zero can mean a strict zero-slot tolerance, while the other sentinels
can silently disable an operator's intended safety policy. Every presence is
unsupported and must fail.

### Accept and ignore the fields with a warning

Rejected. Startup would continue under false operator assurance, and warning
delivery is not a safety boundary.

### Add an explicit `no_reference` or `unbounded` mode

Rejected. Initial support has only one behavior. A ceremonial mode expands the
contract without adding capability and creates a future compatibility burden.

### Reinterpret generic RPC endpoints as references

Rejected. Ordered transport fallback has no independently trusted reference-
provider role, provenance, independence, or aggregation and is explicitly
excluded by ADR-0016.

### Accept future-looking configuration before implementing the feature

Rejected. The repository forbids accepted-but-ignored meaningful input. A
future reference mode requires a separate product decision and an atomic
implementation of its trust and failure rules.

### Decide HTTP request input in the same ADR

Rejected for this micro-step. HTTP DTOs have a different owner, lifetime,
error mapping, OpenAPI contract, and downstream-effect boundary.

## Consequences

### Positive

- Operators cannot mistakenly believe an unenforceable lag bound is active.
- The initial configuration remains smaller and has no compatibility surface
  for a speculative reference design.
- Rejection occurs before network, storage, synchronization, wallet, or
  listener side effects.
- Generic RPC configuration retains one transport-only meaning.

### Negative

- An operator cannot pre-stage future lag/reference settings in a current
  configuration file.
- Adding a real reference mode later requires an explicit schema and product
  change rather than activating dormant fields.

### Neutral

- Absence of lag/reference keys does not disable an otherwise valid configured
  Solana scope; it is the only supported initial no-reference representation.
- A future reference-required mode cannot reuse absence as a fallback from
  omitted, incomplete, or invalid reference configuration.
- This decision does not choose Solana endpoint cardinality or failover policy.
- Bitcoin and Ethereum configuration behavior remains unchanged.
- No implementation or test code is authorized by this decision.

## Validation requirements

Focused future tests must prove:

- a valid initial Solana configuration containing no lag/reference/quorum key
  can pass this specific schema boundary;
- end-to-end root-configuration deserialization rejects unknown keys inside the
  Solana object and every relevant nested object without relying on
  `deny_unknown_fields` combined with `flatten`, a catch-all map, or a
  permissive custom deserializer;
- representative maximum-lag, reference endpoint/provider, provider-role,
  quorum, sampling, fallback, and explicit-disabled keys fail at the intended
  Solana or nested RPC configuration object;
- every representative key fails with a positive number, zero, `null`, false,
  empty string, empty array, and empty object where JSON permits that value;
- no alias, default, coercion, warning-only path, or later validator accepts an
  unsupported member;
- rejection occurs before RPC construction, network access, database opening,
  indexing or readiness tasks, wallet restoration, and listener binding;
- generic endpoint-list membership alone never supplies, votes, or aggregates
  `R`, without this decision requiring a valid multi-endpoint Solana config;
- implementing this startup-only decision introduces no lag/reference type or
  state in the concrete chain, base, wallet, indexing, persistence, or generic
  RPC layers;
- this startup-only decision leaves the current single-send and batch HTTP
  schemas unchanged. ADR-0018 later accepts their shared destination closure,
  ADR-0019 later accepts the single-send envelope, ADR-0020 later accepts the
  batch item, ADR-0021 later accepts the batch-root schema, ADR-0022 later
  accepts cardinality/empty behavior, and the Public Transaction Semantics
  proposal would own order/index behavior upon acceptance; and
- Bitcoin and Ethereum configuration behavior remains unchanged.

## Approval boundary

Decision `S1.6.3.5.3.2.2.2.1` was explicitly approved on 2026-08-27.
Acceptance records only the startup/static closed-schema rejection policy, the
matching canonical requirement, and the narrow ADR-0014 through ADR-0016
future-step clarifications. It does not authorize Solana source, wallet,
transaction, RPC, configuration, dependency, API, or test implementation, and
it does not approve S1.6.3.5.3.2.2.2.2, S1.6.3.5.3.3, S1.6.3.6, or later
decisions.

## References

- `ARCHITECTURE.md`
- `docs/API.md`
- `docs/SYSTEM_REQUIREMENTS.md`
- `apps/api/src/main.rs`
- `apps/api/src/config.rs`
- `packages/json-rpc/src/http.rs`
- [Serde container attributes](https://serde.rs/container-attrs.html)
- `docs/adr/0014-carry-sol-destination-progress-across-operation-retries.md`
- `docs/adr/0015-gate-sol-destination-reads-on-native-rpc-health.md`
- `docs/adr/0016-use-no-independent-sol-lag-reference-initially.md`
