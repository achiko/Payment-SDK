# Global cross-service authentication mode

Status: **Implementation authorized by user direction on 2026-08-10. The
selected implementation decisions are recorded in Section 16.**

This plan defines one global authentication configuration for Wallet Service
(WS), Indexer Service (IX), Payment Service (PS), and repository-owned local
custody HTTP adapters across every supported chain.

It replaces the earlier local-only `TrustedLocal` proposal. Authentication may
be strict or simple regardless of whether the deployment is on loopback, a
private network, or another operator-controlled environment. The mode is
selected by one shared environment value:

```bash
export STRICT_AUTHENTICATION_MODE='true'
```

or:

```bash
export STRICT_AUTHENTICATION_MODE='false'
```

The variable controls service/API authentication only. It does not remove
Bitcoin Core RPC authentication, external Ethereum RPC authentication,
cryptographic private keys, signer authorization imposed by an external
custody provider, durable command idempotency, policy validation, or
chain-native transaction validation.

## 1. Exact mode contract

`STRICT_AUTHENTICATION_MODE` is global in meaning: every repo-owned WS, IX, PS,
custody adapter, HTTP client, CLI, example, and supervisor must interpret the
same value with the same semantics.

Accepted values are exactly lowercase `true` and `false`. Whitespace, an empty
value, and other spellings fail startup with a safe configuration error.

The variable is required for network service composition. There is no silent
missing-value fallback because an omitted environment variable must not
accidentally change a deployment's trust model. Templates may select a value
appropriate to their purpose:

- production/network deployment templates use `true`;
- disposable/demo templates may use `false`; and
- a deployment changes mode only through an explicit configuration change and
  restart.

| Value | Service bearer credentials | Caller identity | Intended meaning |
|---|---|---|---|
| `true` | Required | Authenticated exchange, administrator, or service principal | Existing strict behavior |
| `false` | Optional and not used for authorization | One global trusted principal | Simple operator-controlled deployment |

`false` is globally available and is not restricted to a local network. It is
also intentionally powerful: every process or person that can reach a service
endpoint receives the authority described in Section 6. Network isolation,
firewalls, service-mesh policy, VPNs, Unix account controls, and TLS may still
be used externally, but they are outside the repository's application-layer
identity when strict mode is false.

## 2. Problem being solved

The current manual Bitcoin composition generated five independent bearer
values and two credential-bearing curl files:

- IX bearer;
- local custody bearer;
- WS bearer;
- PS ordinary bearer;
- PS administrator bearer;
- WS curl credential configuration; and
- PS administrator curl credential configuration.

Ethereum standalone composition uses the same general pattern of authenticated
WS, IX, custody, and PS boundaries. This is appropriate when each listener is a
separate trust boundary, but it is unnecessarily complex when the operator
already trusts the deployment network or delegates access control to external
infrastructure.

With `STRICT_AUTHENTICATION_MODE=false`, the operator must be able to start all
three services without creating any repo-owned bearer token or curl credential
file:

```text
Wallet Service  -> no WS/custody bearer required
Indexer Service -> no IX bearer required
Payment Service -> no ordinary/admin/WS/IX bearer required
```

The services remain separate processes if the operator wants them to be. A
future in-process composition may still remove internal HTTP boundaries, but
it is no longer required to obtain the simple authentication mode.

## 3. Non-negotiable ownership and protocol rules

Authentication simplification must not change service ownership:

- WS remains stateless and owns chain-native address generation, transaction
  construction, signing-payload calculation, signature validation, preflight,
  exact-byte broadcast, balances, and receipts.
- IX owns canonical chain checkpoints, watches, observation revisions,
  projections, event cursors, undo, replay, rebuild, and reorg processing.
- PS owns users, deposits, business commands, IX mirroring, classification,
  absolute ledgers, accounting, reservations, collection jobs/legs, retries,
  and reconciliation.
- Generic signing owns keys, curves, payloads, schemes, encodings, and public
  tweaks only. Disabling bearer authentication does not remove the signing
  key or allow chain crates to construct their own custody backend.
- Bitcoin and Ethereum keep chain-native addresses, transactions, fees,
  receipts, and movement types.
- PS and IX keep separate databases with exclusive owners even when
  authentication is false.
- Every chain/node identity, signature, amount, fee, dust, exact-input,
  confirmation, checkpoint, cursor, policy, and reorg invariant remains
  enforced in both modes.
- Removing one concrete chain crate must continue to leave generic signing,
  indexing, storage, transport, and reusable transaction algorithms buildable.

The simplification removes identity checks at selected repo-owned transport
boundaries. It does not remove correctness validation.

## 4. Authentication stays outside semantic contracts

Reusable WS, IX, PS, signing, indexing, and deposit-domain contracts must not
accept or return:

- bearer values;
- HTTP Authorization headers;
- endpoint URLs;
- TLS settings;
- curl configuration;
- environment-variable names; or
- transport-specific authentication errors.

Semantic contracts continue to accept correctness identities such as signer
`OperationId`, IX watch idempotency identity, PS command identity, event
ID/revision, expected ledger head, and projection cursor. These are not
credentials.

Application HTTP adapters select one of two authorizers:

```rust
pub enum AuthenticationMode {
    Strict,
    GlobalTrusted,
}
```

The shared infrastructure type is `AuthenticationMode`. It is derived from
`STRICT_AUTHENTICATION_MODE`; individual service-specific `AUTH_ENABLED`
booleans must not be added.

In strict mode, the existing bearer middleware establishes authenticated
principals. In simple mode, middleware injects one typed global trusted
principal without reading the Authorization header.

## 5. Global configuration propagation

Every relevant composition root must read the same environment key:

- `apps/wallet`;
- `apps/indexer`;
- `apps/api`;
- `apps/custody` for the repository-owned development adapter;
- PS-to-WS and PS-to-IX clients;
- WS-to-custody and Bitcoin WS-to-IX clients;
- examples, supervisors, and Postman/demo configuration; and
- backup/migration/maintenance commands only where they expose or call an
  authenticated HTTP boundary.

The root [`.env.example`](../.env.example) must define the variable once in a
shared section rather than repeating one flag per service. Each independently
started process still needs the variable in its process environment; the file
is a template and does not automatically configure a process unless it is
sourced or loaded by an approved deployment mechanism.

Clients must not guess the remote mode merely because a token is absent. They
receive the global mode through configuration and validate it against a
sanitized service capability/readiness field:

```json
{
  "authentication_mode": "strict"
}
```

or:

```json
{
  "authentication_mode": "global_trusted"
}
```

Mode disagreement is a startup/readiness failure. This prevents a strict PS
from silently treating an unintentionally open WS/IX as equivalent, and it
prevents simple-mode clients from repeatedly omitting credentials against a
strict service.

Secrets remain redacted in configuration/debug output in either mode. When
strict mode is false, configured repo-owned bearer values are ignored for
authorization and produce a warning naming only the variable, never its value.

## 6. Principal and authorization semantics

### Strict mode: `true`

Preserve current behavior:

- PS distinguishes exchange and administrator credentials.
- Administrator credentials may use ordinary routes; ordinary credentials
  receive `403` on administrator-only routes.
- User/deposit ownership is scoped to the authenticated exchange principal.
- WS, IX, and custody operations require their configured service bearer.
- Missing/invalid credentials return the existing `401`/`403` envelopes.
- Non-loopback transport/TLS requirements remain unchanged.

### Simple mode: `false`

There is no application-layer caller identity. All reachable callers share one
stable typed principal, tentatively named `GlobalTrustedPrincipal`.

To make the mode genuinely simple and controlled by one switch:

- ordinary and administrator bearer variables are not required;
- service-to-service bearer variables are not required;
- Authorization headers are ignored for authorization;
- all callers share one PS ownership/idempotency scope;
- all ordinary and administrator PS routes are accessible through the global
  trusted principal;
- all WS address/sign/broadcast/receipt routes are accessible;
- all IX watch/query/event/maintenance routes allowed by the selected command
  family are accessible; and
- the repository-owned local custody API is accessible without a bearer.

These semantics mean every caller that can reach PS can read all PS resources,
invoke administrator accounting/reconciliation, create/close/retry deposits,
and initiate collections. Every caller that can reach WS can request key
provisioning/signing and transaction broadcast. This is not hidden in logs,
status, docs, or CLI help.

The global trusted principal is a deployment trust decision, not an anonymous
multi-tenant mode. It must not attempt to infer principals from IP address,
proxy headers, user-supplied IDs, or `KeyLocator` strings.

## 7. Idempotency behavior in both modes

Authentication and idempotency solve different problems. Turning strict
authentication off does not make durable effects safe to repeat without an
identity.

### Strict mode

- Preserve mandatory external `Idempotency-Key` for PS mutations.
- Preserve required direct WS operation IDs and IX watch/maintenance
  idempotency identities.
- Scope PS commands by authenticated role/principal and operation.
- Exact replay returns the original result; changed meaning conflicts.

### Simple mode

User-supplied idempotency becomes optional at HTTP/CLI DTO boundaries:

- PS generates a UUIDv7 root request identity when `Idempotency-Key` is
  absent, returns it in the response, and persists it before external effects.
- WS generates an operation identity when an effectful direct request omits
  one and returns it with the result.
- IX generates a watch/maintenance identity when a direct request omits one
  and returns it with the durable result.
- PS still supplies deterministic child identities to WS, IX, and custody for
  orchestrated workflows.
- Repositories remain idempotent by their existing typed command/projection
  identities.

The response-loss limitation must be explicit: if a caller omits the identity
and loses the entire response before persisting the generated value, a later
request is indistinguishable from an intentionally new command and may create
a second resource. Retry-safe callers may still provide and reuse an identity
in simple mode.

Generated identities are safe metadata, not credentials. They must not grant
authorization when strict mode is true.

## 8. Wallet Service changes

### Server

- Add the global mode to Ethereum and Bitcoin WS runtime configuration.
- Select strict bearer middleware or global-trusted middleware once when the
  router is constructed.
- Keep health routes public and detail-free; add mode to an appropriate
  sanitized readiness/capability response.
- Keep chain/network, signer capability, signature verification, fee, exact
  input, preflight, broadcast, and receipt checks identical between modes.
- Make direct request operation IDs optional only when the global mode is
  false; preserve current DTO behavior when true.

### Clients

- PS wallet clients send Authorization only when strict mode is true.
- Remote-custody clients omit repo-owned bearer authentication only when the
  selected custody adapter declares compatible global-trusted mode. External
  custody products may retain independent mandatory authentication.
- Bitcoin WS-to-IX clients follow the same global mode as the target IX.
- Client debug output remains redacted in both modes.

## 9. Indexer Service changes

### Server

- Add the global mode to Ethereum and Bitcoin IX configuration and all HTTP
  command families.
- In false mode, omit bearer middleware rather than accepting an empty token.
- Preserve scope/network validation, watch semantics, Ready/backfill gates,
  projection snapshot fencing, pagination, rebuild/maintenance safety, and
  structured errors.
- Make caller watch/maintenance idempotency optional only in false mode;
  internal repository commands always receive an identity.
- Expose the sanitized mode so PS/WS can fail on configuration disagreement.

### Clients

- PS and WS omit IX Authorization only when their configured global mode is
  false and the remote reports `global_trusted`.
- Strict clients never downgrade after a `401`, connection error, or missing
  status field.
- Mode mismatch is nonretryable configuration failure, not a fallback trigger.

## 10. Payment Service changes

### Server

- Add the global mode once to PS configuration and router construction.
- Keep existing strict ordinary/admin middleware unchanged for `true`.
- For `false`, inject the stable global trusted principal and allow both
  ordinary and administrator routes without bearer headers.
- Make external mutation idempotency optional only in false mode, generate and
  return the root identity, and keep all repository idempotency records.
- Preserve strict JSON, request-size limits, policy binding, durable jobs,
  AwaitingWatch, accounting, collection reservations, exact signed-byte
  persistence, and reorg/reconciliation behavior.
- Report the mode through administrator status and a safe readiness field.

### Dependencies

- PS-to-WS and PS-to-IX clients use the same global mode.
- In false mode PS requires neither `PS_WALLET_BEARER_TOKEN` nor
  `PS_INDEXER_BEARER_TOKEN`.
- In strict mode both remain required and redacted.
- PS startup verifies dependency scope, readiness, checkpoint identity, and
  authentication-mode agreement before reporting Ready.

## 11. Custody boundary

The global variable applies directly only to the repository-owned
`apps/custody` development HTTP adapter and its matching remote client:

- `true` requires the existing custody bearer;
- `false` exposes that development adapter without bearer authentication; and
- signing keys remain in custody memory and are never exported.

An external HSM/KMS/custody provider owns its own authentication policy.
`STRICT_AUTHENTICATION_MODE=false` must not force a third-party provider to
disable authentication or cause its client to silently omit required vendor
credentials.

## 12. Environment-variable matrix

### Always required where applicable

- `STRICT_AUTHENTICATION_MODE`;
- chain/network identity;
- node/RPC URL and node-required authentication;
- database paths and exclusive ownership;
- policy path/identity;
- HTTP/metrics bind addresses; and
- fee, confirmation, reorg, collection, and operational limits.

### Required only when strict mode is true

- `IX_BEARER_TOKEN`;
- `CUSTODY_BEARER_TOKEN` for `apps/custody`;
- `WS_BEARER_TOKEN`;
- `WS_CUSTODY_BEARER_TOKEN` for the repo-owned remote adapter;
- `WS_BITCOIN_IX_BEARER_TOKEN`;
- `PS_API_BEARER_TOKEN`;
- `PS_ADMIN_BEARER_TOKEN`;
- `PS_INDEXER_BEARER_TOKEN`; and
- `PS_WALLET_BEARER_TOKEN`.

### False-mode startup behavior

- Missing listed bearer variables are accepted.
- Present listed bearer variables are not printed and do not partially enable
  authentication.
- URLs containing credentials remain rejected.
- Core/vendor credentials remain governed by their own transport contracts.
- An explicitly independent-strict custody policy still requires
  `WS_CUSTODY_BEARER_TOKEN` and a strict remote posture.
- One high-severity startup warning and one persistent metric/status value
  identify global-trusted mode.

## 13. Operational visibility

Every service must make the selected mode unambiguous without exposing a
credential:

- structured startup field `authentication_mode`;
- readiness/capability field;
- Prometheus gauge such as
  `payment_sdk_strict_authentication_mode{service="..."} 0|1`;
- CLI help explaining `true` and `false` consequences; and
- documentation examples for both modes.

When false, startup emits a warning equivalent to:

```text
STRICT AUTHENTICATION IS DISABLED: every reachable caller is globally trusted
```

Do not log one warning per request. Do not include bearer values, Core cookies,
private keys, signed transactions, or sensitive request bodies.

## 14. Implementation workstreams

### Workstream A — approve and document the security change

- Amend `docs/SYSTEM_REQUIREMENTS.md` to define the global mode and explicitly
  qualify the current mandatory-bearer requirements.
- Update `ARCHITECTURE.md` and `docs/CONTRACTS.md` to keep authentication in
  transport/application adapters.
- Update `docs/FEATURE_VALIDATION.md` with strict and simple mode evidence.
- Update Ethereum and Bitcoin operational docs without presenting false mode
  as authenticated or multi-tenant.

### Workstream B — shared parsing without a catch-all crate

- Define one small, precise authentication-mode parser in an owning
  infrastructure layer already used by all HTTP applications, or duplicate a
  tiny value type if dependency direction forbids sharing.
- Do not create a generic `common`, `core`, or `utils` package.
- Require exact boolean parsing and redacted Debug output.
- Thread the typed mode through each application config rather than rereading
  environment variables deep in clients/routers.

### Workstream C — transport middleware

- Add strict and global-trusted authorizer construction for PS.
- Add equivalent route selection for WS, IX, and repository-owned custody.
- Ensure false mode does not construct fake empty bearer tokens.
- Keep all semantic handlers independent of Authorization headers.

### Workstream D — client configuration and negotiation

- Update PS-to-WS, PS-to-IX, WS-to-IX, and repo-owned remote-custody clients.
- Add sanitized remote-mode reporting and startup agreement checks.
- Reject mismatch without downgrade/retry loops.
- Preserve URL/header redaction and redirect restrictions.

### Workstream E — optional external idempotency in false mode

- Extend DTOs carefully so strict-mode wire compatibility remains unchanged.
- Generate root/operation/watch identities before external effects.
- Return generated identities consistently.
- Derive PS child identities with versioned domain separation.
- Test response-loss and replay limitations explicitly.

### Workstream F — examples and global `.env`

- Add `STRICT_AUTHENTICATION_MODE` once near the top of `.env.example`.
- Make strict examples use `true` with credentials.
- Make explicitly trusted/simple examples use `false` without credential files.
- Update `payment_sdk_demo` samples to read the same global value.
- Remove instructions that generate bearer/curl files when false mode is
  selected.

### Workstream G — cleanup and migration

- Stop running services before deleting temporary credential files; deletion
  does not revoke credentials already loaded into a process.
- Provide a narrowly scoped cleanup command for a validated run root.
- Preserve existing strict databases and HTTP routes; authentication mode is a
  deployment setting, not a database migration unless principal ownership
  metadata requires an explicit binding decision.
- Decide whether PS database metadata must bind the authentication mode to
  prevent switching an existing multi-principal strict database into one
  global principal without migration.

## 15. Validation plan

### Configuration

- Missing/empty/invalid global mode fails startup.
- Every app interprets `true` and `false` identically.
- Bearer variables are mandatory only in strict mode.
- Core and external custody credentials remain independently enforced.
- Debug and error output never contains configured secrets.

### Strict mode regression

- Missing/invalid bearer returns `401`.
- Ordinary PS bearer receives `403` on administrator routes.
- Every WS, IX, PS, and custody authenticated path retains current behavior.
- External mutation idempotency remains mandatory.
- Strict clients never downgrade automatically.

### Simple mode

- WS, IX, PS, and local custody routes work without Authorization.
- No repo-owned bearer or curl credential files are needed.
- All PS callers resolve to the same global principal and can use ordinary and
  administrator routes as documented.
- Direct missing operation/idempotency values are generated and returned.
- Explicit identities still replay safely and changed meaning conflicts.
- Dependency mode mismatch prevents readiness.

### Cross-chain

- Ethereum/Anvil and Bitcoin/regtest complete deposit address provisioning,
  durable IX watch acknowledgement, Included/Confirmed projection, balance,
  and reorg correction in both modes.
- Strict/simple mode does not alter chain ID/genesis/network validation,
  signature checks, fee ceilings, transaction inspection, or exact-byte
  broadcast rules.
- Removing one chain crate still passes the chain-deletion architecture test.

### Operational security

- False mode reports an unmistakable warning/status/metric.
- A reachability test demonstrates and documents that false mode grants global
  access rather than identity isolation.
- No test calls a funded public-network signer or broadcast endpoint.
- Live mutation tests remain opt-in and disposable-network only.

## 16. Selected implementation decisions

The implementation uses these approved fail-closed choices:

1. **Missing-variable behavior.** A missing value fails startup. There is no
   default authentication mode.
2. **PS database binding.** PS stores the selected mode in application-owned
   database metadata. Existing unbound databases are treated as strict only
   through explicit migration. A bound database cannot change modes without a
   future principal-aware migration; a new global-trusted deployment therefore
   uses a new or explicitly migrated empty database.
3. **Generated request identity response.** PS returns the effective identity
   in the `Idempotency-Key` response header and in typed accepted-job responses.
   Direct WS and IX responses that generate an identity return it as a typed
   JSON field; IX watch registration also returns the response header.
4. **Mode capability endpoint.** Public readiness exposes the sanitized mode
   because security posture is not a secret. Liveness remains detail-free.
5. **Maintenance commands.** Global-trusted mode grants maintenance HTTP
   authority wherever such routes exist, with the same persistent warning.
   Offline commands that cross no HTTP boundary do not acquire artificial
   bearer handling.
6. **External custody.** Vendor authentication remains independent and cannot
   be disabled by the repository-wide flag. Only the matching repo-owned
   development custody adapter supports global-trusted transport. WS uses a
   typed custody policy: the default repository-matched policy fails startup
   against a strict vendor when WS is global-trusted, while explicit
   `independent_strict` always requires the custody bearer and strict posture.
7. **Automatic `.env` loading.** Binaries continue to use the process
   environment only. They do not discover or load a current-directory `.env`
   file automatically.

## 17. Non-goals

- Removing Bitcoin/Ethereum signing keys.
- Removing Bitcoin Core, Ethereum node, or vendor-required RPC authentication.
- Treating global-trusted mode as identity isolation or multi-tenancy.
- Merging WS, IX, and PS ownership into one god service.
- Creating universal chain transaction or receipt types.
- Sharing PS and IX databases.
- Weakening idempotent repositories, append-only ledgers, checkpoint fencing,
  exact signed-byte persistence, or reorg correction.
- Automatically broadcasting or crediting merely because authentication is
  disabled.

## 18. Completion definition

The plan is complete only when:

- one documented `STRICT_AUTHENTICATION_MODE` value configures all repo-owned
  WS, IX, PS, custody, and matching client boundaries;
- strict mode preserves every existing authenticated behavior;
- false mode requires no repo-owned bearer or curl credential file;
- false mode consistently injects one global trusted principal and permits the
  documented ordinary/administrator/service operations;
- external idempotency may be omitted in false mode while internal effects
  remain idempotent;
- dependency mode disagreement fails readiness;
- mode is visible in startup/status/metrics without leaking secrets;
- Ethereum and Bitcoin deterministic and disposable live validation passes;
- canonical requirements explicitly approve the changed trust model; and
- no documentation describes false mode as authenticated, identity-isolated,
  or safe merely because it is simple.
