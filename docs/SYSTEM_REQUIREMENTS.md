# System requirements

This file defines the canonical scope for the design-stage workspace.

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

The project is pre-release. Obsolete internal types and persisted formats MUST
be replaced directly rather than retained behind compatibility versions or
migrations. External Bitcoin, Ethereum, Solana, HTTP, and JSON-RPC standards
remain compatibility contracts.

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

### `sdk/indexing`

- MUST define storage-independent block synchronization, scoped address
  filters, canonical history, checkpoints, output projections, and persistence
  collections.
- MUST NOT know redb keys/records, HTTP routes, wallets, or business labels.
- A checkpoint MUST contain height and hash.
- `Service` for one scope and `Composer` for several scopes MUST implement the
  same `Indexer` contract.
- `Indexer::sync` MUST accept the complete caller-owned `AddressFilter`
  snapshot for that invocation. Indexing MUST NOT own or persist watches or an
  address registry.
- `Composer` MUST validate the complete snapshot before effects, partition it
  by scope, synchronize every configured child, and reject empty composition
  or duplicate scopes.
- Filter addresses MUST be non-empty, unique, and belong to a configured scope;
  invalid snapshots MUST fail before source I/O.
- `Outputs` MUST remain an independent optional capability rather than an
  `Indexer` requirement.
- A transaction MUST preserve every stable movement, including separate
  Bitcoin inputs and outputs and distinct native/token assets.
- Confirmation MUST be derived from inclusion height and the page checkpoint,
  not stored as a transition.
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
  Runtime generation MUST start after the current checkpoint, or at zero when
  no checkpoint exists.
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

## Address coverage requirements

- A fresh scope with earliest birthday `B > 0` MUST establish `B - 1` as its
  parent anchor and interpret from `B` forward.
- Birthday zero MUST interpret from genesis. A fresh scope with no addresses
  MUST establish the current source tip as an empty anchor.
- A restart MUST resume at the persisted checkpoint plus one after verifying
  its hash remains canonical.
- A generated runtime wallet MUST begin at the next block and require no
  historical backfill.
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
- Outbound native transfers MUST use System Program transfer instructions in
  legacy transactions. History MUST interpret both legacy and version-0
  transactions, including loaded addresses and top-level or inner System
  Program transfers.
- Destination syntax alone MUST NOT authorize a transfer. Destination account
  state MUST be validated before signing, and executable, non-System-owned, or
  otherwise unsupported recipient states MUST be rejected.
- Priority fees are unsupported initially. Meaningful priority-fee
  configuration or request input MUST be rejected rather than ignored.

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
  that component. This structural rule sets no maximum, uniqueness,
  duplicate-item, or ordering contract.
- `TransferRequest.transfers` MUST contain at least one item, and OpenAPI MUST
  publish `minItems: 1` without adding `maxItems`, `uniqueItems`, or a default
  array. `Wallets::send_all` MUST remain the authoritative non-empty guard for
  HTTP and direct SDK callers. An authenticated, structurally valid
  `{"transfers":[]}` MUST reach that guard and return exactly `400` with
  `{"message":"at least one transfer is required"}`, no transaction-ID or
  failed-index property, no registered `Sender::send` invocation, and no
  transaction or chain-side external effect. The SDK source classification
  MUST be `InvalidBatch`; the collection-level `SendError` and every public
  projection, including `Display`, MUST represent no failed item index and
  MUST NOT imply `transaction 0`. The batch operation description MUST make
  accepted IDs and failed-item metadata conditional on a real-item failure.
- Before destination account reads, each observation attempt MUST obtain one
  explicit confirmed `getSlot` result and use that slot as its immutable base
  `minContextSlot` floor. Every destination account request MUST carry a floor
  no lower than that base; inability to establish or satisfy it MUST fail
  destination validation without omitting or lowering the floor.
- A nominally successful destination account response whose returned context
  slot is below the exact `minContextSlot` sent on that request MUST be rejected
  before any account value or absence classification; the complete attempt MUST
  produce no eligibility handoff.
- When an attempt requires several contextual destination account responses,
  requests MUST be issued causally and sequentially. Each request after the
  first MUST use the greatest predecessor slot accepted by every approved
  contextual guard as its exact `minContextSlot`, unless another separately
  approved floor is higher. Accepted response slots MUST be nondecreasing; this
  ordering guard alone MUST NOT impose a maximum forward spread.
- If separately approved behavior starts another destination-observation
  attempt inside the same live native SOL send invocation, the greatest slot
  accepted by every approved guard in any closed predecessor attempt MUST cross
  the attempt boundary as the operation's only retained destination slot
  constraint.
  The successor MUST reacquire every account observation and establish its own
  confirmed `getSlot` base using that inherited floor as `minContextSlot`. An
  inability to satisfy the exact sent floor or a nominal success below it MUST
  fail before any destination account request without omitting or lowering the
  floor. Internal preparation reconstruction MUST NOT reset the operation
  floor; a separate caller invocation or operation restored after process loss
  MUST start without one.
- Before a single send broadcasts, it MUST determine the network fee, verify
  with checked arithmetic that the source can pay the requested lamports plus
  that fee, sign, and successfully simulate the exact transaction. Any failure
  during preparation MUST produce zero broadcasts.
- A successful send MUST return the canonical Base58 first signature derived
  from the exact signed transaction. A different identifier returned by RPC
  MUST NOT be reported as success.

## Sending requirements

The public send input MUST be one non-empty ordered list of wallet,
destination, and exact amount. Destination syntax, positive amounts, fee
bounds, wallet/family compatibility, and chain invariants MUST validate before
the first broadcast. A request MUST target one exact family; mixed-asset
batches, including ETH plus an ERC-20 on the same chain, are rejected rather
than split.

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

Solana MUST build one legacy native transaction per transfer. Before any item
is broadcast, the complete batch MUST validate every amount and destination,
verify cumulative requested lamports plus network fees per source, and sign and
successfully simulate every exact transaction. Transactions MUST then broadcast
in input order. On failure it MUST report the accepted prefix and first failed
input and MUST NOT imply later inputs were attempted.

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

System tests MUST compose the public router, wallet families, one composed
indexer, chain RPC doubles, synchronizer, and temporary redb files in one process.
They MUST cover birthdays, restart, retained reorg, orphan removal, output
restoration, one and batch sends, readiness, and shutdown without contacting a
public network. Solana integration tests MUST use owned RPC doubles or a local
validator and MUST NOT contact a public RPC endpoint.
