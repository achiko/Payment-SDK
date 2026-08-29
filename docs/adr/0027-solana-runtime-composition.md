# ADR-0027: Solana Runtime Composition

## Status

Accepted

## Date

2026-08-27

## Accepted

Accepted by the user on 2026-08-28 under the simplified decision name
**Solana Runtime Composition**.

Acceptance includes the complete runtime boundary in this record: the
independently deletable `chain-solana` crate and modular dependency family; the
proven Rust 1.91 dependency boundary; zeroizing Ed25519 custody limits;
one singular no-retry Solana endpoint; one shared PostgreSQL schema and pool;
closed configuration; startup genesis and executable Memo-v3 checks; explicit
composition, readiness, submission supervision, and shutdown ordering; and the
pinned owned-validator test environment.

This acceptance completes the simplified Solana architecture decision set and
authorizes implementation planning against it. It is not implementation
evidence and does not by itself authorize Rust, manifest, lockfile, SQL, or
migration edits; dependency installation; migration execution; signing; or
live-network actions. Those changes still require explicit implementation-step
approval and must preserve the proven Rust 1.91 graph and regressions.

## Context

There is no Solana crate, dependency, configuration, or composition today. The
generic JSON-RPC client retries and fails over per call, so it cannot guarantee
that one multi-call operation remains on one endpoint. The application also
needs an exact startup identity check, dependency boundary, readiness rule,
shutdown order, and deterministic test environment.

## Decision

### Crate and dependencies

Add package `chain-solana` at `sdk/chains/solana` with the design-lint chain
skeleton. Add it to the workspace, workspace/app dependencies, and a new
`solana-chain` dependency layer whose exact `may_depend_on` set is `package`,
`base`, `indexing`, and `wallets`. Permit that layer from `application` and
`acceptance`, map the package explicitly, and give only `apps/` and
`sdk/chains/solana/` ownership of `solana`/`sol` vocabulary.

The crate owns native address, Ed25519 seed/keypair wrapper, lamports, RPC DTOs,
legacy messages, transactions, account policy, coordinator, source,
interpreter, provider, sender, and the one-method submission-task registration
contract consumed by its coordinator. Generic crates gain only accepted
provider-generation, block-position, and filter/commit coordination
prerequisites. The accepted-but-unimplemented Public Transaction Semantics
decision owns the transaction-error changes. The app implements that
registration contract, owns its queue and task set, and owns config,
enum/OpenAPI additions, construction, readiness, and shutdown. Single-wallet
and batch paths share the same source-keyed coordinator.

Use maintained modular Anza/SPL interfaces, not `solana-client`, a monolithic
SDK, hand-written transaction encoding, or copied System discriminants. The
selected direct version family is:

| Dependency | Version/purpose |
|---|---|
| `solana-address` | `=2.2.0` with `std`, `decode`, `curve25519` |
| `solana-hash` | `=4.1.0` with `decode`, recent blockhash |
| `solana-keypair` | `=3.1.2`, Ed25519 generation/signing primitive |
| `solana-instruction` | `=3.0.0`, native instruction model |
| `solana-message` | `=3.1.0` with `bincode`, legacy message |
| `solana-signature` | `=3.3.0` with `verify`, signature parse/verify |
| `solana-transaction` | `=3.1.0` with `bincode`, `verify` |
| `solana-system-interface` | `=3.0.0` with `bincode`, transfer/decode |
| `spl-memo-interface` | `=2.0.0`, exact v3 Memo instruction |
| `bincode` | `=1.3.3`, Solana wire serialization |
| `base64` | `=0.22.1`, RPC wire encoding |
| `bs58` | `=0.5.1`, arbitrary 32-byte Memo-token encoding |
| `getrandom` | `=0.3.4`, Memo-token OS randomness |

Memo tokens are encoded from raw random bytes; they are never coerced into an
account-address type merely to obtain Base58. No alternative cryptographic or
RPC stack is added. `Cargo.lock` pins the complete resolved graph.

The authoritative workspace baseline is Rust 1.91. The attempted Rust 1.85
cutover was reverted and is rejected historical evidence, not an active
requirement. An exact scratch-only proof used `rustc 1.91.0 (f8297e351)` and
`cargo 1.91.0 (ea2d97820)` to combine the current workspace with the selected
modular Solana family. The resolved lockfile SHA-256 is
`5d578ca06eb117006b5dd518220d741963a1036091b7c756d40e17eb05bfe060`.
It contains 472 packages: 367 declare `rust-version`, 105 omit it, and zero
declare a version above 1.91. The exact modular fixture passed 1/1; locked
offline workspace all-target check, strict workspace Clippy, and 163 focused
Ethereum, redb, indexing, runtime, wallet, and API tests passed without public
RPC access.

The proven current graph retains `alloy-consensus`, `alloy-eips`, and
`alloy-rpc-types-eth` 1.8.3; `alloy-primitives` and `alloy-sol-types` 1.6.1;
and redb 4.2.0. No Alloy or redb downgrade is required. Repository manifest
and lockfile changes must repeat the exact Rust 1.91 checks; graph or
behavioral divergence stops the affected implementation step.

Ed25519 secrets use the existing `SecretBytes` handoff and stay in a private
key owner with no `Clone`, `Debug`, `Display`, or Serde; owned secret bytes are
zeroized on drop. The registry-ownership prerequisite in the accepted
Indexing & Central Database decision
removes the plain secret-material query surface from indexing. It does not
delete or rewrite the existing application-owned `payment_wallets` table, and
this decision adds no Solana row to that table. Configured imports accept
exactly lowercase ASCII
`[0-9a-f]{64}` from an environment variable—no `0x` prefix, whitespace, or
alternate keypair encoding—and decode it into one 32-byte seed.
The seed is never included in errors or ordinary output. Environment strings
and rejected decode temporaries follow the same handling as current
Bitcoin/Ethereum imports rather than a stronger Solana-only zeroization
guarantee. This decision adds no remote custody, HSM, or plaintext key database.

Generated Solana wallets follow the repository's existing development-custody
boundary: they are process-lifetime only and are neither returned as secrets
nor restart-recoverable. Configured imports are reconstructible on restart.
Funding a generated wallet across restart requires a separately approved
durable encrypted custody boundary; this decision does not imply otherwise.

### Configuration and endpoint coherence

Add exactly one top-level, non-flattened, `deny_unknown_fields` PostgreSQL
object and one optional Solana index object:

```text
postgres { url_env, schema, max_connections }

indexes.solana {
  network
  genesis_hash
  rpc { endpoint, headers, timeout_seconds, max_response_bytes }
  sync { confirmation_depth, reorg_retention, poll_millis, batch_size }
}
```

`apps/api` constructs exactly one process-wide PostgreSQL pool and uses one
shared indexing schema. It creates one
`indexing_postgres::Repository::new(pool.clone(), scope)` handle for each exact
`(chain, network)` scope. Every asset on that scope uses the same repository and
indexer because assets are movement facts, not database partitions. Per-chain
database fields and paths for Bitcoin, Ethereum, or Solana are rejected. The
URL is loaded through the configured environment-variable name so credentials
do not enter the configuration document or its debug output.

`schema` is required and accepts one canonical lowercase ASCII identifier
`[a-z][a-z0-9_]{0,62}` that does not begin with `pg_`. Pool construction pins
every connection's search path to exactly that schema and `pg_catalog`, and
startup verifies that the selected schema and required relations match the
read-only compatibility contract. A URL-supplied search path cannot override
the explicit field. Deployment tooling applies the scripts to that same named
schema; application startup performs no DDL.

The current `apps/api` still composes per-chain redb repositories. The shared
PostgreSQL composition in this ADR is a target implementation requirement, not
a claim about current runtime behavior. Redb may remain an embedded contract-
test implementation, but `apps/api` does not select it in the target design.

`endpoint` is singular. Zero or multiple Solana endpoints are not representable
inside that object, and configuration aliases are rejected. Its RPC type is a
new singular `SolanaRpcConfig`; it does not reuse the existing plural
`RpcConfig`. The Solana JSON-RPC client has no transparent retry; indexing
loops and the transaction coordinator own every explicit retry. A load-balanced
URL may still switch physical backends and is an operator trust assumption,
not an SDK coherence guarantee.

Apply the already accepted native-position vocabulary to configured imports at
the same breaking pre-release boundary: rename `ConfiguredWallet.start_height`
to `start_position` for Bitcoin, Ethereum, and Solana, and reject the old JSON
spelling rather than accepting an alias. Chain adapters convert that native
position into their own validated coordinate.

The generic `json_rpc::Config` currently derives `Debug` over endpoint and
header values. Replace that derive with a manual redacted implementation before
constructing the Solana client. It may show endpoint count, header names,
timeouts, size bounds, and retry policy, but never endpoint text or header
values. `json_rpc::Http` retains its existing redacted `Debug` behavior.

No commitment selector, priority-fee setting, maximum-lag/reference/quorum
control, retry knob, Memo program override, or accepted-but-ignored field is
exposed initially.

### Composition and readiness

The application performs this order:

1. deserialize and validate the complete closed config before effects;
2. construct every configured chain client, including the one no-retry,
   redacting Solana client;
3. verify every configured chain identity before database mutation; for Solana,
   call one-shot `getGenesisHash` and compare canonical Base58 to the expected
   hash;
4. call `getSlot(finalized) = S`, then call `getAccountInfo` for exactly
   `MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr` with Base64,
   `finalized`, and `minContextSlot = S`; require response context at least
   `S` and a non-null executable account;
5. only then construct the one PostgreSQL pool and validate the already-applied
   shared indexing schema; startup performs no implicit or destructive DDL;
6. construct one scope-bound repository handle per configured
   `(chain, network)` from cloned pool handles and load every persisted
   checkpoint;
7. initialize each per-scope filter/commit coordinator from its checkpoint;
8. build one source, interpreter, indexing service, provider, submission
   coordinator, and sender; inject the same service's `Checkpoint` and
   `History` views plus its checkpoint notification into the submission
   coordinator, then add the indexer to the shared `Composer`;
9. register only native `WalletAsset::Sol`, with no durable plaintext wallet
   registry;
10. import every configured Solana seed and `start_position` before the first
   synchronization snapshot; and
11. bind the HTTP listener only after every configured index reports `Ready`
   with a persisted checkpoint.

The accepted Provider Generation, Block Position, Public Transaction
Semantics, Destination Account Acquisition, and Indexing & Central Database
decisions, plus the accepted Native SOL Submission decision, are prerequisites.
Canonical requirements and contract documents must remain reconciled with
those decisions before the first source patch.

Per-send `getHealth` is not reused as startup readiness or freshness evidence.
A fatal Solana indexer exit fails startup. At runtime it immediately publishes
not-ready and closes new HTTP admission. If no submitted or ambiguous envelope
is guarded, the application joins its tasks and returns the fatal error. If an
envelope is still guarded, the process remains in the shutdown barrier below
so losing the indexer cannot be misreported as proof that the transaction was
absent. Retryable source failure makes readiness false until successful
catch-up.

The application runs one submission supervisor backed by an `mpsc` admission
queue and `JoinSet`, retains the only close/wait control, and injects its
registrar implementation into the Solana coordinator. Registration returns
success only after the supervisor has inserted the task into the set. If the
registrar is closed or the acknowledgement is lost before insertion, the
coordinator fails before dispatch. A handler or SDK object therefore cannot
detach an untracked submission task. The existing readiness bridge also moves
under the application supervisor; no bare `tokio::spawn` remains outside
tracked application task ownership.

Destination account acquisition completes before submission-task registration.
Cancellation or failure there publishes no floor, releases every pre-envelope
lexical source lease, and registers no task. Only after the complete account
handoff may a task win registration and later cross the dispatch boundary; a
submitted or ambiguous envelope guard is then coordinator-owned and cannot be
released by acquisition cleanup.

### Shutdown

Graceful shutdown first publishes not-ready and stops HTTP admission, then
closes submission registration and drains active handlers. Registration and
registrar closure are one serialized boundary: a task that wins registration is
visible in the tracked set before it may dispatch, while a close that wins makes
the preparing handler fail before dispatch. Only after handlers drain may the
application inspect the guarded-envelope set. Shutdown waits for registered
active sends and process-local ambiguity reconciliation rather than dropping
the serving future. Indexing and historical status reconciliation remain
running while any guarded envelope is unknown, because they are required for
the terminal absence proof.

After the guard set is empty, the application cancels synchronization, awaits
the submission set and all remaining supervised storage work, and exits. If an
envelope remains unknown, graceful shutdown remains pending without an
automatic deadline. During retryable index/RPC failure the operator may restore
that path and allow reconciliation to finish. After a fatal indexer exit,
terminal absence is unavailable: only a positive historical status can clear
the guard in-process; otherwise force-kill is the sole exit and explicitly
accepts the documented duplicate-payment risk. Neither shutdown nor any
indexer failure releases an unknown source.

### Test environment

Default tests use owned RPC doubles and temporary repositories and never call a
public network. The integration harness pins Agave `solana-test-validator`
`v3.1.14` at commit
`3134055b562e95902233be308453fffa1c4a8902`; every downloaded platform
artifact is verified against a committed SHA-256 before execution. That Agave
release bundles and loads `spl_memo-3.0.0.so` under the exact Memo-v3 account
used by this design, so the harness verifies that bundled account rather than
claiming a separate Memo binary download. It owns its ledger directory, ports,
keys, and cleanup and covers actual wire serialization and System/Memo
execution. Missing Memo and wrong genesis use owned RPC-negative fixtures;
unavailable history and unsupported transaction version are likewise explicit
negative fixtures, not skipped checks.

Because `apps/api/Cargo.toml` sets `autotests = false`, the integration target
must be declared explicitly as `[[test]] name = "solana_stack"` at
`tests/solana_stack.rs`. The repository currently has no CI workflow, so the
suite is not called automated until a checked-in workflow owns the pinned tools
and executes that target.

## Consequences

- Endpoint affinity is honest with the current generic HTTP client.
- Solana remains independently deletable and does not leak protocol DTOs into
  base, wallets, or indexing.
- Startup fails before repository mutation on wrong chain or missing Memo.
- All configured scopes share one pool and schema while each repository rejects
  cross-scope access; adding an asset does not create another database.
- An exact scratch-only Rust 1.91 proof shows the selected modular APIs can
  construct, sign, and serialize the legacy System-plus-Memo transaction while
  retaining the current Alloy/redb graph. It proves dependency feasibility,
  not repository integration or runtime support.
- Multi-endpoint failover, durable request idempotency, and production custody
  require separate product decisions.

## Alternatives considered

### Reuse generic per-call endpoint failover

Rejected. Health, slots, accounts, blockhash, simulation, and broadcast could
come from incompatible backends.

### Depend on the full Solana RPC client/SDK

Rejected. The repository already owns bounded HTTP JSON-RPC policy, and the
full client brings transport and validator vocabulary across the chain boundary.

### Exit immediately on Ctrl-C

Rejected. Dropping an in-flight `sendTransaction` future can erase the only
process-local record of an accepted-but-unknown envelope.

## Validation requirements

Tests must cover exact configuration keys, schema-identifier validation,
search-path pinning, rejection of `start_height`, one endpoint, and rejection
of every per-chain database field; redacted database URL, endpoint, header, and
secret debug output; canonical seed length and decoding; wrong genesis before
pool creation; incompatible shared schema
rejection without mutation; one pool shared by Bitcoin, Ethereum, and Solana;
scope isolation under concurrent writes; native/token asset coexistence;
byte-for-byte preservation of `payment_wallets`; finalized Memo context and
executable-account checks; complete wallet import before first sync; readiness
startup/regression/fatal-exit behavior; configured response-size exhaustion and
cancellation during account acquisition with no floor publication, no leaked
lexical lease, and no submission-task registration; task tracking; graceful
shutdown during preparation, registrar-close races, dispatch, ambiguity, and
fatal indexing;
positive-status-only recovery after fatal indexing; tracked readiness; owned validator isolation
and binary hashes; the explicit application test target; locked dependency
resolution on Rust 1.91 with the proven Alloy/redb graph; design lint; and full
workspace gates.

## Implementation boundary

This accepted decision consolidates dependencies, crate ownership,
configuration, composition, endpoint coherence, readiness, shutdown, and test
environment. Its prerequisites include all earlier accepted Solana decisions,
and it closes the architecture gate for a small-step implementation plan. It
does not claim current implementation or authorize source, dependency,
lockfile, SQL, migration, signing, or live-network changes without the
corresponding approved implementation step.

## References

- [Anza modular Solana SDK](https://github.com/anza-xyz/solana-sdk)
- [Agave `v3.1.14`](https://github.com/anza-xyz/agave/releases/tag/v3.1.14)
- [Agave bundled program binaries](https://github.com/anza-xyz/agave/blob/3134055b562e95902233be308453fffa1c4a8902/program-binaries/src/lib.rs)
- `Cargo.toml`
- `lint.toml`
- `apps/api/src/config.rs`
- `apps/api/src/main.rs`
- `packages/json-rpc/src/http.rs`
- `sdk/wallets/src/provider.rs`
