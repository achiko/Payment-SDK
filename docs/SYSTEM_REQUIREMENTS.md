# System requirements

This file defines the accepted canonical scope for the design-stage workspace.
An ADR marked Proposed does not change these requirements until it is accepted
and the affected canonical documents are reconciled. Native SOL Submission
(ADR-0025) and Solana Runtime Composition (ADR-0027) are Accepted but
unimplemented.

## Product boundary

The system MUST provide one API process that can:

1. initialize Bitcoin, Ethereum, and Solana RPC clients and embedded indexers;
2. generate or import a wallet through a chain-neutral wallet collection;
3. return a wallet's canonical address, exact balance, and complete paginated
   transaction history;
4. send one transfer or a non-empty ordered batch;
5. continuously index the authoritative wallet address/birthday set;
6. survive restarts without losing canonical checkpoints, history, live
   outputs, or retained reorg recovery; and
7. expose business and HTTP behavior without importing a concrete chain after
   composition.

The current system MUST NOT contain deposit accounting, ledgers, collection or
sweep jobs, payment state machines, reservation systems, hardware-wallet
workflows, remote custody, separate wallet/indexer processes, or internal
wallet/indexer transports.

The project is pre-release. Obsolete internal Rust and wire formats MUST be
replaced directly rather than retained behind compatibility aliases, legacy
runtime readers, or project-defined versioned DTOs. Durable rows in the shared
PostgreSQL database are different: indexing schema changes MUST preserve and
validate existing data unless an exact indexing scope is explicitly approved
for replacement. External Bitcoin, Ethereum, Solana, HTTP, and JSON-RPC
standards remain compatibility contracts.

## Layering

### `packages/*`

- MUST remain useful outside blockchain projects.
- MAY depend on external libraries and other packages.
- MUST NOT import SDK or application crates.
- HTTP helpers MUST remain transport mechanics, not wallet/indexing DTOs.
- JSON-RPC MUST delegate framing/correlation to `jsonrpsee` and own only
  bounded transport, retry, and ordered endpoint failover.
- A state-changing Ethereum submission MUST use one transport attempt rather
  than hidden endpoint failover; retry and reconciliation MUST preserve the
  exact envelope at the Ethereum coordinator boundary.
- Crypto MUST contain no chain names, addresses, transactions, assets, or
  wallet policy.
- Generic redb mechanics MUST remain separate from indexing records.

### `sdk/chains/base`

- MUST remain small and explicitly approved.
- MUST contain only semantics shared across substantially different chains.
- MUST NOT import a concrete chain, RPC implementation, indexing, or wallets.
- MUST NOT define a universal chain RPC or native transaction representation.

### Concrete chain crates

- MUST own native address, network validation, RPC, blocks, transactions,
  signing, fees, wallets, and indexing interpretation.
- MUST use mature external protocol libraries where they correctly implement a
  standard; local code SHOULD cover only abstraction or policy gaps.
- MUST keep redundant chain prefixes out of types within the crate namespace.
- MUST satisfy the common directory skeleton enforced by design lint.
- MUST be independently deletable without breaking generic crates or the other
  concrete chains.

### `sdk/chains/solana` target

- MUST be the independently deletable `chain-solana` package at
  `sdk/chains/solana` and satisfy the common concrete-chain skeleton.
- Its design-lint dependency layer MUST depend only on `package`, `base`,
  `indexing`, and `wallets`; only application and acceptance layers MAY depend
  on that Solana layer.
- MUST privately own Solana addresses, Ed25519 seed/keypair handling, lamports,
  RPC DTOs, legacy messages and transactions, account policy, submission
  coordination, source/interpreter translation, provider/sender behavior, and
  the one-method submission-task registration capability. Solana protocol DTOs,
  slot variables, exact envelopes, and coordinator state MUST NOT leak into a
  generic crate.
- Direct protocol dependencies MUST use the selected modular family:
  `solana-address = 2.2.0`, `solana-hash = 4.1.0`,
  `solana-keypair = 3.1.2`, `solana-instruction = 3.0.0`,
  `solana-message = 3.1.0`, `solana-signature = 3.3.0`,
  `solana-transaction = 3.1.0`, `solana-system-interface = 3.0.0`, and
  `spl-memo-interface = 2.0.0`, with the features fixed by ADR-0027. Wire and
  token support MUST use `bincode = 1.3.3`, `base64 = 0.22.1`, `bs58 = 0.5.1`,
  and `getrandom = 0.3.4`; `Cargo.lock` MUST pin the resolved graph.
- Feature selection MUST include `std`, `decode`, and `curve25519` for
  `solana-address`; `decode` for `solana-hash`; `bincode` for
  `solana-message`; `verify` for `solana-signature`; `bincode` and `verify` for
  `solana-transaction`; and `bincode` for `solana-system-interface`.
- MUST NOT add `solana-client`, a monolithic Solana SDK, handwritten transaction
  encoding, copied System Program discriminants, or an alternative crypto/RPC
  stack.
- The authoritative workspace baseline MUST remain Rust 1.91. The reverted
  Rust 1.85 experiment is historical evidence and MUST NOT be treated as an
  active requirement. A Solana manifest patch MUST preserve the proven graph,
  including `alloy-consensus`, `alloy-eips`, and `alloy-rpc-types-eth` 1.8.3;
  `alloy-primitives` and `alloy-sol-types` 1.6.1; and redb 4.2.0. No Alloy or
  redb downgrade is required.
- Every repository manifest or lockfile change for Solana MUST repeat the
  locked Rust 1.91 workspace all-target check, strict workspace Clippy, and the
  focused Ethereum, redb, indexing, runtime, wallet, and API regressions. A
  divergence from the proven graph or behavior MUST stop the affected step.

### `sdk/indexing`

- MUST define storage-independent block synchronization, scoped address
  filters, canonical history, checkpoints, output projections, and persistence
  collections.
- MUST NOT know backend keys/records, HTTP routes, wallets, or business labels.
- `BlockPosition` MUST represent the native RPC coordinate used for traversal,
  canonical lookup, restart, readiness, and address birthdays. For Solana it
  is a slot.
- `BlockHeight` MUST represent produced-block count and MUST drive
  confirmations, history/output ordering, journal keys, and retention.
- A checkpoint MUST contain position, produced height, hash, and one atomic
  optional parent value pairing parent position with parent hash. Only genesis
  MAY have no parent.
- `Service` for one scope and `Composer` for several scopes MUST implement the
  same `Indexer` contract.
- `Indexer::sync` MUST accept a caller-owned `FilterSource`; every read from it
  MUST yield one complete `AddressFilter` snapshot. Indexing MUST NOT own or
  persist watches or an address registry.
- `Composer` MUST validate a complete snapshot before effects, partition it by
  scope, synchronize every configured child, and reject empty composition or
  duplicate scopes.
- Filter addresses MUST be non-empty, unique, and belong to a configured scope;
  invalid snapshots MUST fail before source I/O.
- `Outputs` MUST remain an independent optional capability rather than an
  `Indexer` requirement.
- A transaction MUST preserve every stable movement, including separate
  Bitcoin inputs and outputs and distinct native/token assets.
- Confirmation MUST be derived from inclusion produced height and the page
  checkpoint's produced height, not stored as a transition.
- Confirmation policy MUST be depth-only. `Confirmed` MUST report the observed
  depth and MUST NOT claim chain-finality proof.
- History and output pagination MUST be checkpoint-bound and reject a changed
  snapshot.
- Persistence MUST be expressed by `Blocks`, `Transactions`, and `Outputs`.
- `Blocks::add` MUST atomically compare/move the checkpoint, persist canonical
  address-primary history, apply live output changes, and write/prune the
  storage-derived rollback journal.
- `Blocks::remove` MUST verify the expected current tip and derive its entire
  inverse from the repository's private journal. Callers MUST NOT supply undo
  state.
- A retained reorg MUST remove orphan history and restore live outputs.
  `ReorgTooDeep` MUST require recreation/rescan when the common ancestor is
  outside retention.
- RPC failures MUST remain retryable/unknown and MUST NOT imply a dropped
  transaction.
- Synchronization errors MUST be typed errors. `SyncStatus` MUST describe only
  active progress (`CatchingUp` or `Ready`), not cache failure variants.
- MUST NOT expose a public event feed, raw-block archive, backfill/rebuild
  command, watch lifecycle, or migration surface.

### `sdk/indexing/redb`

- MUST implement indexing persistence collections only.
- MUST own all indexing keys, records, codecs, ordered scans,
  compare-and-swap conditions, atomic batches, and journal encoding.
- MUST expose no storage record or undo type through generic indexing APIs.
- MUST persist only canonical checkpoint, address-primary history, current
  live outputs, and bounded rollback journal.
- MUST NOT own a synchronizer task, runtime handle, filter registry, or public
  service.

### `sdk/indexing/postgres`

- MUST implement the same indexing persistence collections over one shared
  schema serving every configured chain/network scope.
- The deployment-owned canonical shared-schema creation and ordered migration
  scripts MUST live physically under `sdk/indexing/postgres/migrations/`.
- `Repository::new` MUST bind a cloned process-wide pool to exactly one
  `(chain, network)` and MUST reject operations for another scope.
- MUST own only the indexing checkpoint, history/movement, live-output, and
  bounded-journal runtime model, including row mapping, set-based statements,
  transactions, and compare-and-swap behavior.
- MUST NOT create a schema, database, pool, or repository per asset. Native and
  token assets are facts in shared history rows.
- The runtime adapter MUST NOT read, write, truncate, delete, or issue DDL for
  application-owned wallet or custody tables. In particular, physical
  colocation of `payment_wallets` MUST NOT make that table part of indexing.
- A central migration script that changes an application-owned table MUST have
  separate application-level approval and preservation evidence. Script
  location and deployment ownership MUST NOT be treated as indexing runtime or
  domain ownership.
- MUST preserve all unrelated scopes and application-owned rows during schema
  evolution or an explicitly approved scope-local rescan.
- MUST NOT add legacy runtime readers, inferred coordinate fallbacks,
  compatibility aliases, or versioned storage DTOs.

### `sdk/wallets`

- MUST expose one `Wallets<I, F>` collection for chain-neutral application
  behavior.
- MUST own the family map `(IndexScope, Provider, Sender)`, constructed wallet
  instances/public metadata, and authoritative address birthdays.
- MUST inject the composed `Arc<dyn Checkpoint>` needed for safe runtime
  birthdays and expose the complete deduplicated filters needed by the sync
  task.
- MUST support provider-selected generation/import without returning secrets.
- Import MUST require exclusive startup access and an explicit birthday.
  Runtime generation MUST start at the checked successor of the current
  checkpoint position, or at position zero when no checkpoint exists.
- MUST expose get, exact selected-asset balance, complete checkpoint-bound
  selected-asset history, one send, and ordered batch send without leaking
  concrete chain transaction types.
- One-wallet build/prepare/broadcast/ID verification MUST live on the wallet
  abstraction, not in HTTP.
- MUST NOT own indexing persistence, a background runtime, or durable custody.

### `apps/api`

- MUST be the only executable and composition root; no crate may depend on it.
- `main.rs` MUST explicitly construct and connect RPC clients, repositories,
  chain services, `Composer`, wallet families, sync, readiness, and HTTP.
- MUST open exactly one configured PostgreSQL database/schema and one
  process-wide pool, then clone that pool into one scope-bound indexing
  repository per configured `(chain, network)`.
- MUST use the shared PostgreSQL repositories for the production composition;
  a per-chain database, schema, pool, or asset repository is not permitted.
- MUST NOT hide this object graph behind a process or service facade.
- MUST share one concrete composed indexing object through narrow trait views.
- MUST load/import the complete authoritative startup wallet set before the
  first sync.
- MUST supervise synchronization and HTTP in one process and own cancellation,
  fatal-task handling, and graceful shutdown.
- MUST pass `Wallets` and readiness into handlers rather than construct
  dependencies per request.
- MUST keep endpoint-specific wire input/output structs immediately above
  their handler and generate one Utoipa contract from those routes.
- MUST keep secrets out of JSON, schemas, logs, and ordinary `Debug`.
- MUST implement the Solana-owned submission-task registrar through one
  application-owned `mpsc` admission queue and `JoinSet`. Registration MUST
  succeed only after the supervisor has inserted the task into its tracked set;
  a closed registrar or lost acknowledgement before insertion MUST fail before
  dispatch.
- MUST keep submission and readiness tasks under application supervision; a
  handler or SDK object MUST NOT detach an untracked send or bare readiness
  task.

These are target requirements. The current `apps/api` still composes separate
per-chain redb repositories. That implementation gap MUST remain visible until
the PostgreSQL composition and its preservation evidence are complete.

## Accepted Solana runtime configuration

The target configuration MUST contain exactly one top-level PostgreSQL object
and MAY contain one Solana index object:

```text
postgres { url_env, schema, max_connections }

indexes.solana {
  network
  genesis_hash
  rpc { endpoint, headers, timeout_seconds, max_response_bytes }
  sync { confirmation_depth, reorg_retention, poll_millis, batch_size }
}
```

- These objects MUST be non-flattened, deny unknown fields, and expose no
  per-chain database path. Native and token assets on one scope MUST share the
  same repository.
- `postgres.url_env` MUST name the environment variable containing the database
  URL. Credentials MUST NOT enter JSON or ordinary `Debug` output.
- `postgres.schema` MUST match `[a-z][a-z0-9_]{0,62}`, MUST NOT begin with
  `pg_`, and MUST pin every pooled connection's search path to exactly that
  schema plus `pg_catalog`. A URL-supplied search path MUST NOT override it.
  Startup MUST validate the already-applied schema read-only and MUST NOT run
  DDL.
- Solana `rpc.endpoint` MUST be singular and have no alias, transparent retry,
  or failover. The index loop and submission coordinator own every explicit
  retry. A load-balanced URL remains an explicit operator trust assumption.
- Configuration MUST expose no commitment selector, priority-fee/Compute
  Budget setting, lag/reference/quorum control, retry knob, Memo override, or
  accepted-but-ignored field.
- Every configured wallet MUST use `start_position`; the pre-release
  `start_height` spelling MUST be rejected for Bitcoin, Ethereum, and Solana
  rather than accepted as an alias.
- Before Solana client construction, generic JSON-RPC configuration MUST have a
  manual redacted `Debug` implementation that may reveal counts, header names,
  timeouts, bounds, and retry policy but never endpoint text or header values.

## Accepted Solana custody and lifecycle

- A configured Solana import MUST read exactly 64 lowercase ASCII hexadecimal
  characters from its named environment variable and decode one 32-byte seed.
  Prefixes, whitespace, uppercase, and alternate keypair encodings MUST fail.
  The environment string, decoded temporaries, construction failures, and final
  private wrapper MUST be zeroized and MUST NOT implement ordinary formatting,
  cloning, or serialization that exposes the secret.
- A generated Solana wallet is process-lifetime only and is not restart-
  recoverable. A configured import is reconstructible at restart. This target
  adds no Solana row to `payment_wallets`, remote custody, HSM, or plaintext key
  database.
- Startup MUST validate the complete closed configuration, construct clients,
  and verify every configured chain identity before database mutation. Solana
  verification MUST call one-shot `getGenesisHash`, then prove the exact Memo-v3
  account executable using finalized `getSlot` and contextual `getAccountInfo`.
- Only after identity and Memo verification MAY the application construct the
  one PostgreSQL pool, validate its schema, load scope checkpoints, initialize
  filter/commit coordination, and compose services. The Solana coordinator MUST
  receive its own service's `Checkpoint`, `History`, and checkpoint notification.
  Only native `sol` is registered; every configured seed and `start_position`
  is imported before the first synchronization snapshot.
- The public listener MUST bind only after every configured index is Ready with
  a persisted checkpoint. Per-send `getHealth` MUST NOT substitute for startup
  readiness or cluster identity.
- Destination account acquisition MUST finish before submission-task
  registration. Registration and registrar closure MUST form one serialized
  boundary, and a task MUST be visible in the tracked set before dispatch.
- A fatal Solana indexer exit MUST publish not-ready and close new HTTP
  admission. With no guarded envelope, supervised tasks terminate and the fatal
  error is returned. With any submitted or ambiguous envelope, shutdown MUST
  remain pending while indexing/status reconciliation needed for safety stays
  available.
- Graceful shutdown MUST publish not-ready, stop HTTP admission, close task
  registration, drain handlers, and wait for guarded envelopes before stopping
  synchronization or storage work. It has no automatic deadline for an unknown
  envelope. After a fatal indexer exit, only positive historical status may
  clear that guard in-process; force-kill is the only other exit and explicitly
  accepts the documented duplicate-payment risk.

## Accepted Solana test environment

- Default tests MUST use owned RPC doubles and temporary repositories and MUST
  never call a public network.
- The wire/system integration fixture MUST pin Agave
  `solana-test-validator v3.1.14` at commit
  `3134055b562e95902233be308453fffa1c4a8902`, verify every platform artifact
  against a committed SHA-256, own its ledger/ports/keys/cleanup, and verify the
  bundled `spl_memo-3.0.0.so` at the exact Memo-v3 address.
- Because application autotests are disabled, the harness MUST declare the
  explicit `solana_stack` target at `tests/solana_stack.rs`. The suite MUST NOT
  be described as CI-automated until a checked-in workflow owns the pinned
  tools and runs that target.

## Central PostgreSQL preservation requirements

- The central database MUST be treated as a shared multi-chain, multi-asset
  system of record, not as disposable indexer scratch space.
- Indexing schema evolution MUST inventory affected scopes, add generic
  columns or constraints without destroying existing rows, backfill only facts
  that are proven for the named scopes, validate the result, and enforce final
  constraints only after validation succeeds.
- A dense-coordinate `position = height` backfill MAY be used only for
  explicitly verified Bitcoin and Ethereum scopes. It MUST NOT be inferred for
  Solana, an unknown chain, or an unverified custom scope.
- A scope-local rescan MAY replace only indexing-owned rows for the explicitly
  selected `(chain, network)`. It MUST NOT drop or recreate the database, affect
  another scope, or modify `payment_wallets`.
- Application-owned wallet rows MUST remain readable and byte-for-byte
  preserved through an indexing migration unless a separate application-owned
  change explicitly authorizes otherwise.
- An application-owned restart read path MUST be implemented and verified
  before the current indexing-owned `payment_wallets` query path is removed;
  ownership cleanup MUST NOT strand preserved rows.

## Address coverage requirements

- An address birthday MUST be a `BlockPosition`, not a produced-block height.
- A fresh scope MUST locate the first produced block at or after its earliest
  birthday and establish that block's actual parent as its anchor. It MUST NOT
  manufacture `birthday - 1`, because a native coordinate may be skipped.
- Birthday zero MUST interpret from genesis. A fresh scope with no addresses
  MUST establish the actual produced source tip as an empty anchor.
- A restart MUST resume at the checked successor of the persisted checkpoint
  position after verifying its hash remains canonical.
- A generated runtime wallet MUST begin at the checked successor native
  position and require no historical backfill. If that position is skipped,
  it MUST activate at the first later produced block.
- Historical import MUST require exclusive startup access and MUST be
  unavailable after the wallet collection is shared.
- On restart, the embedding application MUST supply the same complete
  historical address/birthday set that produced the checkpoint. Because filters
  are deliberately not persisted, a changed set requires explicit scope
  recreation/rescan and cannot be auto-detected safely.

## Bitcoin requirements

- Address parsing MUST use the standard Bitcoin library and enforce the
  configured network.
- Transactions MUST retain exact outpoints and all inputs/outputs.
- Signing MUST support only the explicitly implemented script/address kinds and
  verify each input belongs to its signer.
- Fee calculations MUST use checked integer satoshis.
- Indexed history MUST expose input and output movements separately.
- Reorg rollback MUST restore spent output state exactly.

## Ethereum requirements

- Addresses MUST parse canonical 20-byte values without storing an `0x` prefix
  in base address bytes.
- Transaction building MUST validate chain ID, nonce, gas, and EIP-1559 fees.
- The recovered signer MUST match the requested sender before a signed envelope
  is accepted.
- Native and token movements MUST use distinct assets.
- Receipts/logs and reorg correction MUST preserve exact `U256` values.
- One Ethereum wallet provider MUST select exactly one native or allowlisted
  token asset. Balance and send behavior MUST remain fixed to that selection.
- Generated Ethereum asset families MUST use independent keys. Startup imports
  MUST reject registering one EOA under both native ETH and an ERC-20 asset.
- Token startup validation MUST verify chain identity, deployed code, decimals,
  and a strict balance response against one canonical block through one
  endpoint-affine RPC context.
- ERC-20 transfer preparation MUST target only the configured contract, use
  zero native value, simulate the exact call, and accept only canonical ABI
  `bool true` output before signing.
- An ERC-20 send MUST verify both selected-token funds and native ETH for the
  worst-case configured gas fee. Native gas is fee state, not a second public
  wallet balance.
- Ethereum history presentation MUST retain only the wallet's selected-asset
  movements plus attributable native fee metadata. Unrelated token or native
  movements MUST NOT make the selected-asset page fail.
- One process-wide Ethereum coordinator MUST assign nonces by sender address,
  so native and token providers using the same EOA cannot reserve the same
  nonce. This coordination is independent of the selected wallet asset.

## Solana requirements

- Initial Solana support MUST be native SOL only. SPL Token and Token-2022
  wallet families, balances, history, and sends MUST be rejected as
  unsupported.
- Wallet generation and import MUST use Ed25519. Import MUST accept exactly one
  validated 32-byte secret seed and reject other key encodings.
- Addresses MUST decode to exactly 32 bytes and round-trip through canonical
  plain Base58. Solana addresses MUST NOT be labeled or processed as
  Base58Check.
- Each configured scope MUST provide a stable network slug and expected genesis
  hash. Startup MUST verify the hash through the endpoint-affine RPC context
  used by that scope and fail readiness on mismatch.
- One coherent identity check, indexing operation, or send operation MUST NOT
  silently span multiple RPC endpoints.
- Canonical indexing MUST initially admit only finalized slots. Lower indexing
  commitment policies MUST be rejected as unsupported.
- Chain traversal MUST use slots and parent slots. RPC-reported block height is
  separate metadata, and skipped slots MUST NOT be synthesized as blocks.
- History MUST interpret both legacy and version-0 transactions, including
  loaded addresses and top-level or inner System Program transfers.
- Every outbound native SOL payment occurrence MUST use one legacy transaction
  containing exactly one System Program transfer followed by one zero-account
  Memo-v3 instruction. The source MUST be fee payer and sole signer. The Memo
  MUST contain a fresh opaque 256-bit operating-system-CSPRNG token encoded as
  canonical Base58, disclose no payment/customer facts, remain immutable across
  exact-envelope replay, and be distinct for every occurrence. It supplies
  transaction uniqueness, not request idempotency. Construction MUST use exactly
  `spl_memo_interface::v3::ID`,
  `MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr`; that account MUST be executable
  during startup validation and in the owned validator fixture. Another Memo
  version, a configured override, or a memo-less fallback is unsupported.
- Destination syntax alone MUST NOT authorize a transfer. Destination account
  state MUST be validated before signing, and executable, non-System-owned, or
  otherwise unsupported recipient states MUST be rejected.
- Priority fees and Compute Budget instructions are unsupported initially.
  Meaningful priority-fee configuration or request input MUST be rejected
  rather than ignored.

### Native SOL wallet acceptance

- A generated wallet MUST be returned and remain retrievable with native asset
  `sol`, chain `solana`, the configured network slug, and one canonical Base58
  address. Responses, metadata, logs, and errors MUST expose no secret
  material.
- Importing the same valid seed into the same configured scope MUST derive the
  same canonical address. A rejected generation or import MUST register neither
  an active wallet nor an indexing filter.
- Native SOL values MUST use checked integer lamports and exact base-10 decimal
  conversion, where one SOL is `1_000_000_000` lamports. Balance responses MUST
  preserve the exact finalized lamport value without floating-point conversion.

### Native SOL transaction acceptance

- A send MUST reject zero, negative, sub-lamport, or greater-than-`u64`-lamport
  amounts before signing or broadcast.
- A supported destination MUST be on-curve and either absent or an existing,
  non-executable, zero-data account owned by the System Program. Every
  off-curve destination, executable account, non-zero-data account, and other
  existing account owner MUST be rejected before signing or broadcast. General
  address parsing and indexing MUST continue to accept valid off-curve Solana
  addresses.
- Every RPC acquisition supplying destination account observations MUST
  explicitly use `confirmed` commitment. Failure to obtain confirmed state MUST
  fail destination validation without omitting commitment or substituting
  `processed` or `finalized`.
- Every supplied destination account observation, including explicit absence,
  MUST carry a valid returned Solana context slot. Missing or malformed context
  MUST fail destination validation, and the context slot MUST NOT be treated as
  a produced-block height or indexing checkpoint.
- Before each destination-observation attempt obtains its confirmed `getSlot`
  base, it MUST issue exactly one endpoint-bound, no-parameter `getHealth`
  request through the same endpoint-affine RPC context and receive the exact
  JSON string `"ok"`. The call MUST NOT be transparently retried or failed over.
  A failed, unsupported, malformed, or non-`"ok"` response MUST close the
  attempt before any base or account acquisition and produce no eligibility
  handoff. Health admission MUST be repeated for every attempt, is ephemeral,
  supplies no slot, and MUST NOT be represented as an SDK-enforced numeric
  maximum-lag guarantee.
- Initial native SOL destination validation MUST have no independently trusted
  numeric cluster-progress reference. Primary-endpoint methods, generic
  failover endpoints, indexing or readiness state, wall clock, and other
  transaction-preparation contexts MUST NOT be treated as one. Initial support
  therefore MUST NOT claim an SDK-enforced numeric maximum lag relative to
  current cluster progress. If a separately supported future mode requires
  reference or quorum evidence, missing, malformed, wrong-genesis, timed-out,
  or insufficient evidence MUST fail closed and MUST NOT fall back to the
  no-reference path.
- Initial Solana application configuration MUST expose no maximum-lag,
  reference-provider, provider-role, quorum, sampling, fallback, or explicit
  no-reference option. Absence is the only valid initial representation. The
  root configuration and every relevant nested object MUST enforce exact
  allowed keys end to end. Any attempted meaningful member MUST fail
  configuration deserialization before application composition or runtime
  effects, regardless of whether its value is `null`, zero, false, empty, or
  apparently disabled. Generic RPC endpoints MUST retain only their ordered
  primary/failover transport meaning and MUST NOT become independent reference
  evidence.

## Sending requirements

The public batch send input MUST be one ordered list containing from one
through 50 wallet, destination, and exact-amount occurrences. The single-send
route represents one occurrence without the batch wrapper. Destination syntax,
positive amounts, fee bounds, wallet/family compatibility, and chain invariants
MUST validate before the first broadcast. A request MUST target one exact
family; mixed-asset batches, including ETH plus an ERC-20 on the same chain,
MUST be rejected rather than split.

- Following existing transport and authentication rejection, both transaction
  POST routes MUST reject every non-empty URI query string with `400` and
  `{"message":"transaction query parameters are not supported"}` before JSON
  shape extraction, request conversion, or wallet delegation. An empty query
  component MUST have no semantic effect.
- There MUST be no transaction-control header contract. Ordinary HTTP, proxy,
  authentication, content-negotiation, and tracing headers MAY be present but
  MUST NOT control lag, reference selection, commitment, retry, priority fee,
  or other send behavior.
- Public transaction-error precedence MUST be transport/authentication,
  non-empty query rejection, JSON exact-schema validation, collection
  cardinality, wire-item conversion in original order, itemwise common
  validation in original order, chain-specific complete preparation, and
  ordered broadcast. Itemwise common validation MUST check one occurrence's
  positive amount, resolve its wallet, and check family compatibility before
  advancing to the next occurrence.
- The shared public destination JSON object used by both transaction POST
  endpoints MUST remain an exact address-only schema containing `encoding` and
  `text`. It MUST expose no lag/reference member. Any unrecognized destination
  member MUST reject the complete authenticated JSON body with the generic
  `400` schema error, regardless of its name or JSON value, before
  post-deserialization conversion, `Wallets::send`, or `Wallets::send_all`.
  Batch rejection at this boundary MUST return no accepted transaction IDs or
  failed index. OpenAPI MUST publish this object with
  `additionalProperties: false`.
- The public single-transfer JSON body for
  `POST /v1/wallets/{id}/transactions` MUST remain an exact top-level schema
  containing only the required `destination` and `amount` members.
  `destination` MUST retain the shared `AddressInput` contract and `amount`
  MUST remain a JSON string. Any unrecognized top-level member, including a
  lag/reference control or wrapper, MUST reject the complete authenticated
  body with the generic `400` schema error before post-deserialization
  conversion, wallet lookup, or `Wallets::send`, with no transaction-ID or
  failed-index metadata. OpenAPI MUST publish `SendFunds` with
  `additionalProperties: false`, exactly those two required properties, the
  `AddressInput` reference, and string-typed `amount`.
- Every public `WalletTransfer` item inside `POST /v1/transactions` MUST remain
  an exact schema containing only the required `wallet_id`, `destination`, and
  `amount` properties. `wallet_id` and `amount` MUST remain JSON strings, and
  `destination` MUST retain the shared `AddressInput` contract. Any
  unrecognized item property, regardless of item position, property order,
  name, or JSON value, MUST reject the complete authenticated body with the
  generic `400` schema error before post-deserialization request conversion or
  `Wallets::send_all`, with no accepted transaction IDs or failed index.
  OpenAPI MUST publish `WalletTransfer` with `additionalProperties: false`,
  exactly those three required properties, string-typed `wallet_id` and
  `amount`, and the `AddressInput` reference.
- The public `TransferRequest` root for `POST /v1/transactions` MUST remain an
  exact schema containing only required `transfers`, represented as a JSON
  array whose items reference `WalletTransfer`. Missing or non-array
  `transfers`, an invalid root JSON type, or any unrecognized root property
  MUST reject the complete authenticated body with the generic `400` schema
  error before post-deserialization request conversion or `Wallets::send_all`,
  with no accepted transaction IDs or failed index. OpenAPI MUST publish
  `TransferRequest` with `additionalProperties: false`, exactly that required
  array property and item reference, and the batch operation MUST reference
  that component. This structural closure MUST NOT imply item uniqueness.
- `TransferRequest.transfers` MUST contain from one through 50 items. OpenAPI
  MUST publish `minItems: 1` and `maxItems: 50` without `uniqueItems` or a
  default array. `Wallets::send_all` MUST be the authoritative minimum-and-
  maximum guard for HTTP and direct SDK callers; every concrete sender MUST
  defensively reject an out-of-contract count before chain I/O.
- An authenticated, structurally valid
  `{"transfers":[]}` MUST reach that guard and return exactly `400` with
  `{"message":"at least one transfer is required"}`, no transaction-ID or
  failed-index property, no registered `Sender::send` invocation, and no
  transaction or chain-side external effect. The SDK source classification
  MUST be `InvalidBatch`; the collection-level `SendError` and every public
  projection, including `Display`, MUST represent no failed item index and
  MUST NOT imply `transaction 0`. The batch operation description MUST make
  accepted IDs and failed-item metadata conditional on a real-item failure.
- A structurally valid body with more than 50 transfers MUST return the index-
  free `InvalidBatch` error `400` with
  `{"message":"at most 50 transfers are allowed"}`, no accepted IDs, failed
  index, sender call, or RPC call. The HTTP adapter MUST apply the shared
  maximum before converting any item, so an invalid item inside an oversized
  body cannot outrank the cardinality error.
- Every batch occurrence MUST retain the identity of its zero-based position in
  the authored array. Conversion, lookup, validation, sender handoff, result
  mapping, and item-scoped errors MUST preserve exact length, order, and
  multiplicity. Repeated wallet IDs, destinations, amounts, and identical
  items MUST remain separate payment occurrences. Internal observation
  deduplication MUST map its result back to every original occurrence.
- Chain-neutral transaction, wallet, and send errors MUST be able to carry one
  optional canonical `ambiguous_transaction_id`; `SendError.failed_index` MUST
  be optional. The chain transaction layer MUST derive the ambiguous ID from
  the exact locally signed envelope and be its sole authority. Error conversion
  MUST preserve the value unchanged and HTTP MUST only project it. Provider
  prose, a provider-supplied candidate, or a mismatched returned ID MUST NOT
  become reconciliation metadata. Its presence MUST always render as `503`.
- A per-occurrence batch failure MUST preserve its original `failed_index`. A
  batch-wide preparation/resource failure or grouped-transaction broadcast
  failure that cannot truthfully identify one public occurrence MUST be index-
  free. A grouped ambiguity MAY still carry its locally derived ambiguous ID.
  Accepted IDs MUST contain only a definitely acknowledged prefix and MUST NOT
  be inferred from the failed index.

### Native SOL account acquisition

- One native SOL single or batch send MUST perform exactly one initial account
  acquisition against one configured endpoint, with no automatic retry,
  transparent transport retry, endpoint failover, or chunking. After the
  endpoint-bound health admission above, it MUST execute exactly:

  ```text
  getSlot(confirmed) = F
    -> getMultipleAccounts(confirmed, base64, minContextSlot = F) = (C, values)
    -> getSlot(confirmed, minContextSlot = C) = U
  ```

- Before RPC, the operation MUST validate destination syntax and on-curve
  policy. It MUST then build one stable query list by walking original
  transfers in order and appending each resolved source followed by its parsed
  destination only at that canonical 32-byte address's first occurrence. The
  50-transfer public limit permits at most 100 unique addresses, so the account
  observation MUST use one unchunked `getMultipleAccounts` request.
- The account request MUST ask for complete Base64 data, send no `dataSlice`,
  and receive exactly one value per requested address. Positional mapping MUST
  occur only after exact cardinality validation. Explicit JSON `null` alone
  means absence. Every existing account MUST provide structurally valid
  lamports, owner, executable, data, and total-space fields. Data MUST use the
  exact `[string, "base64"]` tuple and strict Base64 decoding; owner text MUST
  be canonical Base58 for exactly 32 bytes; and decoded data length MUST equal
  reported space.
- The complete response structure and encoding MUST validate before the closing
  request. Its context MUST satisfy `C >= F`; the closing witness MUST return
  `U >= C`. `F` and `C` remain attempt-local and provisional. Only a successful
  closing `U` may become operation floor `P`, and only atomically with the
  complete successful eligibility and source-balance handoff. This is
  endpoint-local consistency evidence, not a freshness, fork, or maximum-lag
  proof; a self-consistent `F = C = U = u64::MAX` is not rejected by this
  witness alone.
- After the witnessed snapshot closes, an absent destination is eligible and
  an existing destination is eligible only when non-executable, System-owned,
  and zero-data. Existing sources MUST satisfy the same account shape. An absent
  source contributes zero lamports to later checked balance sufficiency. A
  structurally valid but unsupported account shape is assigned to the earliest
  original occurrence that uses it, prevents the atomic handoff, and publishes
  no operation floor.
- Timeout or cancellation at any acquisition await; a transport, HTTP, or
  JSON-RPC failure; response-size rejection; malformed JSON, Base64, owner, or
  account field; cardinality or data-space mismatch; a below-floor response; or
  a failed closing witness MUST terminate the complete acquisition. The failure
  MUST be operation-wide and index-free, discard all observed values and
  absences, publish no `F`, `C`, `U`, or derived floor, release every pre-
  envelope lexical source lease already held by the invocation, leave no
  background acquisition, supply no transaction ID, accepted IDs, failed
  index, or ambiguous ID, and perform no fee call, construction, signing,
  simulation, or broadcast. It MUST NOT release a coordinator-owned submitted
  or ambiguous envelope guard.
- Initial support authorizes no successor acquisition. If a later accepted
  decision adds one inside the same live invocation, it MUST reacquire every
  account and may inherit only the last `U` from a fully witnessed predecessor
  that completed the atomic handoff. A separate caller invocation or operation
  restored after process loss starts without account facts or a retained floor.

### Native SOL submission coordination and preparation

- One process-local Solana coordinator MUST acquire every resolved source in
  canonical byte order before account RPC. If any source is preparing,
  submitting, or guarded by unresolved ambiguity, the invocation MUST release
  every newly acquired lease, perform no RPC or transaction work, and return
  `SourceBusy` as `503`. A batch MUST attach the earliest original occurrence
  using that source; a single send has no failed index.
- Self-transfers MUST fail before RPC. After account acquisition atomically
  hands off balances and operation floor `P`, preparation MUST obtain one
  confirmed recent-blockhash lifetime, construct every exact transfer-plus-Memo
  message, obtain each exact fee sequentially, and use checked arithmetic to
  verify cumulative `amount + fee` per source without crediting incoming batch
  transfers. A `null` `getFeeForMessage` result MUST fail preparation and MUST
  NOT be interpreted as a zero fee.
- Every message MUST be signed once with its source Ed25519 signer, locally
  verified, serialized to exact bytes, and distinct in both message and first
  signature. Every exact signed transaction MUST then simulate successfully in
  original order with Base64, confirmed commitment, signature verification,
  no blockhash replacement, and the nondecreasing operation floor.
- Any amount, address, randomness, blockhash, fee, arithmetic, signing,
  encoding, or simulation failure MUST occur before the first broadcast. An
  item failure reports the first original occurrence failing that stage; an
  operation-wide RPC/coherence failure has no synthetic item index.
- Recent-blockhash expiry MUST use confirmed block height, never slot. The
  lifetime remains valid through `currentBlockHeight == lastValidBlockHeight`
  and expires only above it. It MUST be checked before the first broadcast and
  every later item. Once any item may have been submitted, no envelope may be
  rebuilt or re-signed.

### Native SOL broadcast and ambiguity

- Transactions MUST broadcast in original order as Base64 through one-shot,
  endpoint-stable HTTP execution with preflight enabled, confirmed preflight
  commitment, the current operation floor, and provider retries disabled.
  Success requires the returned signature to equal the canonical first
  signature derived locally from the exact signed bytes and means submitted,
  not confirmed.
- Immediately before the first potentially submitting call, source leases MUST
  transition atomically into coordinator-owned exact-envelope state. Dropping
  or cancelling the request waiter after dispatch MUST NOT cancel submission or
  reconciliation. Application task ownership MUST follow the accepted Solana
  Runtime Composition decision; the current implementation does not yet provide
  that supervisor.
- After an unknown response, the coordinator MAY make at most two additional
  byte-identical submissions, for three wire calls total. Before a replay it
  MUST query the one local signature with historical search, require one valid
  position-correlated result whose context meets the operation floor, and query
  confirmed block height only after a valid null status. Any valid non-null
  status, including one carrying an execution error, proves observation and is
  returned as submitted. A malformed, unavailable, incoherent, low-context,
  short, or extra-cardinality status result remains ambiguous and permits no
  replay. It MUST NOT replay after expiry or an unavailable lifetime check.
- Once the first `sendTransaction` wire call begins, every timeout, disconnect,
  cancellation, JSON-RPC error, malformed/uncorrelated response, provider
  message, or returned-signature mismatch MUST remain ambiguous. A batch MUST
  stop at that original occurrence, preserve only the definitely acknowledged
  prefix, retain its `failed_index`, and expose the locally derived signature as
  `ambiguous_transaction_id`; a single send exposes the ID without a batch
  index.
- A guarded source MUST remain unavailable until signature status or canonical
  indexed history proves observation, or until blockhash expiry plus complete,
  unpruned, checkpoint-stable finalized history proves absence. Unavailable
  status/history, an indexing gap, pruning, reorg, or fatal source failure MUST
  leave the source blocked; ordinary confirmation and execution failure still
  come only from indexing.
- Background reconciliation MUST use the same scope's `Checkpoint` and
  `History` capabilities plus an application checkpoint-advance notification.
  It MUST retry on notifications and a deterministic capped backoff from 500
  milliseconds to 10 seconds. An absence proof MUST exhaust fee-payer history
  in pages of at most 100 at one unchanged checkpoint whose complete finalized
  coverage reaches the blockhash-expiry height. A cursor conflict, checkpoint
  change, reorg, page error, pruning, or incomplete traversal invalidates the
  proof and MUST NOT release the source.
- Submission coordination is initially process-local and supports one active
  API writer per managed source. There is no durable outgoing-operation or
  request-idempotency store. Callers MUST NOT automatically retry an unknown
  logical payment; response loss, restart, failover, active-active writers, or a
  new invocation MAY double-pay because it creates a new Memo token.

### Chain-native batch behavior

Bitcoin MUST build one native transaction for a compatible batch. It MAY
consume UTXOs from several source wallets, MUST read them at one output
checkpoint, create one requested output per transfer, preserve per-source
change, allocate fees deterministically, and sign each input with its owner. A
successful batch returns one submitted ID; a pre-submit failure returns none.

Ethereum MUST build one native transaction per transfer and broadcast in input
order with consecutive nonces per source. On failure it MUST report the
accepted prefix and first failed input and MUST NOT imply later inputs were
attempted. Every Ethereum batch item MUST be simulated, checked against
cumulative per-sender native/token requirements, and signed before its first
broadcast.

Solana MUST build one distinct native transaction per transfer, complete the
entire account, fee, cumulative-balance, signing, and simulation preparation
before the first broadcast, then submit in input order. On failure or ambiguity
it MUST stop, preserve only the definitely acknowledged prefix, report the
truthful original occurrence, and never imply that a later item was attempted.

The sender MUST preserve exact signed bytes across retryable ambiguous outcomes
and verify the returned ID against those bytes. An unresolved ambiguous
Ethereum submission MUST block later nonce use for that sender until the exact
local transaction ID is observed or the same envelope is accepted on replay.
Submission MUST NOT be called confirmation; indexing provides canonical
confirmation.

Ethereum nonce and ambiguous-envelope coordination is process-local. One
running API process MUST be the only transaction writer for a managed EOA;
restart-safe outgoing-operation recovery and active-active writers require a
separately approved durable submission boundary.

## HTTP requirements

The public API MUST provide chain-neutral routes for:

- generating a wallet for a configured family;
- reading wallet metadata/address;
- reading current indexed balance;
- reading paginated complete transactions;
- sending one exact transfer; and
- sending a non-empty ordered batch.

Network is selected at startup and returned as wallet metadata, not accepted as
untrusted route policy. Authentication and request limits belong to the server
boundary. Liveness and indexing readiness MUST be distinct.

Every endpoint has one handler. Its endpoint-specific request and response
objects MUST be directly above it, simple serde/Utoipa wire structs. Handlers
MUST do extraction, one `Wallets` call, error/status mapping, and encoding only.

## Quality gates

Completion requires:

```bash
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked --workspace --no-deps
cargo run --locked -p design-lint -- --policy lint.toml check .
git diff --check
```

Repository contract tests MUST cover both redb and PostgreSQL, including both
block coordinates, atomic parent presence, scope rejection, and unchanged
confirmation/retention behavior for dense Bitcoin and Ethereum coordinates.

System tests for the accepted shared prerequisites MUST compose the public
router, wallet families, one composed indexer, chain RPC doubles, synchronizer,
and one owned temporary PostgreSQL database/schema through one process-wide
pool. They MUST prove Bitcoin and Ethereum birthdays, restart, retained reorg,
orphan removal, output restoration, one and batch sends, readiness, and
shutdown, plus Bitcoin, Ethereum, and Solana scope isolation in shared tables,
native/token asset coexistence, and preservation of sentinel application-owned
wallet rows.

Accepted Solana indexing tests MUST additionally prove sparse finalized-slot
traversal, legacy/version-0 history interpretation, retained reorg behavior, and
no UTXO projection. Native SOL submission contract tests MUST cover transaction
uniqueness, full-batch zero-broadcast preparation failures, source locking,
exact fees/balances/signatures/simulation, blockhash expiry, returned-signature
mismatch, three-call exact-byte replay, ambiguity metadata, cancellation,
status/history reconciliation, indefinite evidence failure, and documented
restart/double-payment limitations. Exact application runtime-composition tests
required by ADR-0027 are also unimplemented. Every Solana test MUST use owned
RPC doubles or a local validator and MUST NOT contact a public RPC endpoint.
