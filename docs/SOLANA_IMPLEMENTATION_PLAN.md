# Native Solana Implementation Plan

## Status

**Superseded.** The [Native Solana Master Implementation Plan](SOLANA_MASTER_IMPLEMENTATION_PLAN.md)
is the only active Native Solana implementation plan. Everything below is
retained as historical, non-executable planning evidence; its checkboxes,
ordering, approvals, and Rust 1.85 instructions MUST NOT guide implementation.

## Historical content (non-executable)

## Execution rule

- Work on one bold, uniquely named step at a time.
- Approve a step by its name; there are no nested numeric approval IDs.
- Unless a step explicitly says **read-only** or **test-only**, its focused
  behavioral proof and implementation belong in the same reviewable diff.
- After every step, report its exact files, commands, results, and remaining
  risk before asking to advance.
- Do not commit, push, execute migrations, access a retained database, install
  a dependency, or contact a public RPC endpoint unless that exact action is
  separately approved.
- Preserve unrelated working-tree changes and the user-provided migration
  scripts. Never rewrite an accepted baseline migration to make Solana fit.
- Stop immediately if Rust 1.85 cannot preserve existing Bitcoin, Ethereum,
  indexing, storage, or API behavior. A higher MSRV requires a new decision.

The implementation order is:

```text
MSRV proof
  -> shared contracts
  -> Solana-native values and RPC
  -> account acquisition
  -> submission construction and dispatch
  -> sparse indexing and wallet handoff
  -> submission reconciliation and wallet adapters
  -> application composition
  -> owned system evidence
```

## Five workstreams

| Workstream | Accepted scope it delivers |
|---|---|
| **Foundation & Shared Contracts** | Rust 1.85, provider-owned generation, plain Base58, public transaction semantics, crate ownership, and native Solana values |
| **Account Reads & Wallets** | singular RPC, identity/account methods, exact SOL balance, destination acquisition, provider, wallet, and sender adapters |
| **Native Submission** | source exclusion, System-plus-Memo construction, fees, signing, simulation, ordered broadcast, replay, ambiguity, and reconciliation |
| **Indexing & Central Database** | shared generic prerequisites, sparse finalized slots, complete transaction interpretation, retained-reorg evidence, and PostgreSQL coexistence |
| **Runtime & Release Evidence** | closed configuration, startup order, one pool, readiness, supervision, shutdown, owned validator tests, and honest documentation evidence |

## Cross-plan gates

The [Indexing & Central Database Implementation Plan](INDEXING_CENTRAL_DATABASE_PLAN.md)
already owns generic persistence and synchronization work. Its small steps are
not duplicated here. A gate closes only after every named constituent step is
approved and completed individually in that plan.

| Gate | Required central-plan steps | Blocks |
|---|---|---|
| **Schema Knowledge** | **Receive scripts** through **Validate startup compatibility** | PostgreSQL application composition |
| **Adapter Safety** | **Reject zero pool size** through **Prove shared-pool isolation** | using PostgreSQL as the production runtime repository |
| **Block Coordinates** | **Add coordinate vocabulary** through **Record roll-forward recovery** | any Solana source or public position/cursor claim |
| **Sparse Synchronization** | **Specify source contract tests** through **Rewrite synchronization** | implementing `chain-solana`'s `BlockSource` |
| **Wallet Handoff** | **Rename birthdays** through **Break old cursors explicitly** | runtime wallet publication and removal of indexing custody coupling |
| **Central Composition** | **Open one application pool** through **Prove central coexistence** | adding the Solana scope to the application object graph |

The migration scripts are present, so **Receive scripts** is ready for its
read-only checksum and ownership review. It is not automatically complete.

## Foundation & Shared Contracts

- [ ] **Record repository state** — read-only: capture `git status`, toolchain
  versions, locked dependency metadata, and design-lint status without changing
  repository files.
- [ ] **Record the Bitcoin baseline** — read-only: run the current focused
  Bitcoin crate and application contracts and preserve exact pass/fail output.
- [ ] **Record the wallet/API baseline** — read-only: run current wallets and
  declared `wallet_api` tests outside the Ethereum-specific filter.
- [ ] **Record the indexing/storage baseline** — read-only: run current generic
  indexing, runtime, storage-redb, and indexing-redb contract suites.
- [ ] **Record the PostgreSQL baseline** — read-only: run its repository
  contract only against an approved owned test database. With none configured,
  record **not run**, never passing because `POSTGRES_TEST_URL` silently skipped.
- [ ] **Install the MSRV toolchain** — install Rust `1.85.0` with the required
  Cargo, rustfmt, and Clippy components; prove the exact version. This is an
  external tool installation and needs its own approval if a download is
  required.
- [ ] **Reproduce MSRV blockers** — read-only: run the locked workspace check
  with Rust 1.85, then audit the complete lockfile/registry `rust-version`
  metadata because Cargo may stop at the workspace's current Rust 1.91
  declaration before reporting all incompatible transitive packages.
- [ ] **Freeze Ethereum regression evidence** — read-only: record current
  transaction encoding, signature, ID, fee, RPC parsing, and application test
  results before changing Alloy, including the real declared command
  `cargo test --locked -p payment-api --test wallet_api ethereum` rather than
  the non-existent standalone `ethereum_stack` target.
- [ ] **Select an MSRV dependency set** — read-only: resolve one coherent
  Rust-1.85-compatible Alloy, redb, `ruint`, `nybbles`, `serde_with`, ICU, and
  IDNA graph in an isolated `mktemp` scratch workspace so the repository
  lockfile cannot change. Treat locally visible versions as candidates, not
  accepted pins, and request network approval if the scratch resolution needs
  uncached registry data.
- [ ] **Rehearse the MSRV cutover** — read-only to the repository: apply the
  candidate dependency graph in an isolated workspace copy, discover every
  required Ethereum/redb API repair, and prove the complete candidate compiles
  before editing the real manifests or lockfile.
- [ ] **Apply the MSRV cutover** — in the one intentionally broad compiling
  packet, atomically update `Cargo.toml`, affected manifests, `Cargo.lock`, and
  every repair proven by rehearsal; restore `rust-version = "1.85"`, preserve
  frozen Ethereum wire behavior and redb reopen/recovery behavior, and add no
  Solana package or dependency.
- [ ] **Pass the MSRV gate** — with Rust 1.85 run `cargo fmt --all -- --check`,
  locked workspace check/all-targets, locked workspace tests, strict all-
  features Clippy, no-deps docs, design-lint tests and repository check, plus
  `git diff --check`. No Solana manifest edit may precede this result.
- [ ] **Record MSRV evidence** — update validation documentation with exact
  versions and commands only after the gate passes; otherwise record the stop
  condition without claiming Solana readiness.

- [ ] **Specify JSON-RPC redaction** — test-only: add the intentionally failing
  security regression that requires endpoint/header values to be absent from
  `json_rpc::Config` debug output while retaining the existing `Http` redaction
  contract; do not add a passing test that blesses the leak.
- [ ] **Redact JSON-RPC configuration** — replace derived `Debug` with a manual
  implementation that shows counts, header names, timeout, response limit, and
  retry policy but no endpoint text, header values, URL credentials, or bearer
  values.
- [ ] **Prove one-shot transport bounds** — extend the generic RPC tests for
  exactly one call, first-endpoint affinity, no failover, no retry, cancellation,
  and response-size rejection; add no Solana DTO or floor state to `json-rpc`.

- [ ] **Add plain Base58 vocabulary** — add `wallets::AddressEncoding::Base58`
  as a value distinct from Base58Check; defer public `Solana`/`Sol` enum
  variants until application composition can handle every exhaustive match.
- [ ] **Make Bitcoin generation explicit** — move secp256k1 generation policy
  into the Bitcoin provider and add a chain-private deterministic failure seam
  that cannot be configured in production; successful generation must reuse
  `Provider::create` and derive the address from its signer, while a
  `Wallets::generate` randomness failure remains typed as
  `wallets::ErrorKind::Generation` and publishes no wallet or filter.
- [ ] **Make Ethereum generation explicit** — move secp256k1 generation policy
  into the Ethereum provider with the same production-closed, chain-private
  failure seam; successful generation must reuse `Provider::create` and derive
  the address from its signer, while a `Wallets::generate` failure remains
  typed as `wallets::ErrorKind::Generation` and publishes no wallet or filter.
- [ ] **Update fixture providers** — make every wallets/API test provider choose
  its own generation behavior before the trait default disappears.
- [ ] **Remove generic generation policy** — make `Provider::generate` mandatory
  while preserving `Arc<T>` forwarding; a workspace check proves every family
  now owns its native key algorithm.

- [ ] **Add the base ambiguity carrier** — let the base transaction error carry
  an optional transaction ID; only a concrete chain transaction layer may
  originate it, while ordinary errors contain none.
- [ ] **Preserve wallet ambiguity** — carry the exact chain-originated
  ambiguous ID from the base error through wallet errors without regenerating
  or accepting provider prose.
- [ ] **Make send failures truthful** — add `InvalidBatch`, make
  `SendError.failed_index` optional, carry optional ambiguity, and provide
  distinct item and operation constructors with accurate `Display` output.
- [ ] **Enforce SDK batch bounds** — export one `MAX_TRANSFERS = 50`; reject
  zero and 51 items in `Wallets::send_all` before lookup or sender invocation;
  prove success at 1 and 50. Preserve exact `InvalidBatch` messages
  `at least one transfer is required` and `at most 50 transfers are allowed`
  with no accepted IDs, failed index, or ambiguous ID.
- [ ] **Prove authored occurrence identity** — test that repeated IDs,
  destinations, amounts, aliases, and identical items retain exact length,
  order, multiplicity, and original indices.
- [ ] **Enforce common item precedence** — validate each occurrence in authored
  order as positive amount, wallet lookup, then family compatibility before
  advancing to the next item.
- [ ] **Defend the Bitcoin sender bound** — reject impossible zero/51-item
  direct calls before chain I/O and make grouped transaction failures
  index-free instead of inventing item zero.
- [ ] **Defend the Ethereum sender bound** — reject impossible zero/51-item
  direct calls before chain I/O while preserving original item indices and the
  definitely accepted prefix.
- [ ] **Attach Bitcoin local ambiguity** — originate reconciliation identity
  only from the exact locally signed Bitcoin envelope, reject provider or
  mismatched-ID authority, and keep grouped ambiguity index-free.
- [ ] **Attach Ethereum local ambiguity** — originate reconciliation identity
  only from the exact locally signed Ethereum envelope, preserve its original
  item index/prefix, and reject provider or mismatched-ID authority.
- [ ] **Project ambiguity through HTTP** — add optional
  `ambiguous_transaction_id` to the public error body; make any ambiguity a
  `503` and omit unrelated fields for definite, single, item, and grouped cases.
- [ ] **Reject transaction queries first** — reject every non-empty query on
  both transaction POST routes before JSON extraction/conversion. Prove
  authentication beats query rejection; query rejection beats malformed JSON,
  empty batch, and 51 items; an empty query component has no effect; and
  ordinary infrastructure headers remain inert.
- [ ] **Apply the HTTP maximum first** — reject more than 50 wire items before
  converting any amount while leaving the empty list to the authoritative SDK
  minimum guard.
- [ ] **Prove exact collection responses** — at the HTTP boundary prove 0 and
  51 items return `400` with only the accepted minimum/maximum message and no
  transaction IDs, failed index, ambiguity, wallet/sender call, or RPC effect;
  prove 1 and 50 continue to delegation.
- [ ] **Publish exact OpenAPI bounds** — emit `minItems: 1`, `maxItems: 50`, no
  `uniqueItems`, and the optional ambiguity field with exact omission rules.
- [ ] **Correct transaction operation prose** — update the existing OpenAPI
  operation descriptions to include native SOL and make accepted IDs and a
  failed item index conditional rather than promising an index for every batch
  failure.
- [ ] **Lock transaction schemas** — test unknown destination, single-root,
  batch-item, and batch-root members plus `additionalProperties: false`; no
  lag, reference, commitment, retry, or priority-fee control is accepted.
- [ ] **Regress existing transaction paths** — run Bitcoin/Ethereum single,
  batch, duplicate, grouped, prefix, malformed, query, header, and ambiguity
  contracts before a Solana sender exists.

- [x] **Add Solana lint ownership** — add the `solana-chain` dependency layer,
  application/acceptance permissions, package mapping, and `solana`/`sol`
  vocabulary ownership limited to `apps/` and `sdk/chains/solana/`. In this
  same coherent lint change, add only exact, reasoned `owned-vocabulary`
  suppressions beside Ethereum's standard `alloy_sol_types::sol` import and
  `sol!` invocation; do not broaden ownership or weaken the rule.

The first package slice resolves the complete accepted direct protocol family
once, under Rust 1.85:

| Dependency | Exact version and features |
|---|---|
| `solana-address` | `2.2.0`; `std`, `decode`, `curve25519` |
| `solana-hash` | `4.1.0`; `decode` |
| `solana-keypair` | `3.1.2` |
| `solana-instruction` | `3.0.0` |
| `solana-message` | `3.1.0`; `bincode` |
| `solana-signature` | `3.3.0`; `verify` |
| `solana-transaction` | `3.1.0`; `bincode`, `verify` |
| `solana-system-interface` | `3.0.0`; `bincode` |
| `spl-memo-interface` | `2.0.0` |
| `bincode` | `1.3.3` |
| `base64` | `0.22.1` |
| `bs58` | `0.5.1` |
| `getrandom` | `0.3.4` |

- [ ] **Select the Solana dependency graph** — read-only: verify the table's
  complete modular graph and the required generic `base`, `indexing`,
  `wallets`, `json-rpc`, serialization, async, and zeroization edges against
  Rust 1.85 without changing a repository manifest or lockfile.
- [x] **Create the first Solana package slice** — in one unavoidable topology
  packet, add the workspace member, `chain-solana` manifest, complete accepted
  dependency table, lock resolution, required chain skeleton, and these six
  real second-owner slices: singular one-shot `rpc/client.rs`; private
  `transaction/lifetime.rs`; canonical 32-byte
  `transaction/operations/memo.rs`; structural `wallet/account.rs`; scoped
  `indexer/interpreter.rs`; and bounded `indexer/source/budget.rs`. Later steps
  extend those owners rather than recreate them. Do not land empty/filler
  `mod.rs` directories or weaken design lint. Before completion, run an
  immediate Rust-1.85 locked package check, dependency-tree/MSRV audit, design
  lint, formatting, and diff check.
- [x] **Parse canonical Solana addresses** — accept canonical Base58 that
  decodes to exactly 32 bytes; reject invalid alphabet, wrong length, and
  non-canonical spellings; preserve valid off-curve addresses for reading and
  indexing.
- [x] **Render canonical Solana addresses** — make display/public conversion
  emit plain canonical Base58 and the chain-neutral address carry the exact
  32 account-address bytes; keep chain and network scope solely in existing
  wallet/indexing metadata.
- [x] **Separate Base58 codec tags** — prove Solana `AddressFormat` accepts only
  plain `Base58`, rejects `Base58Check`, and no plain-Base58 value is decoded
  through Bitcoin's Base58Check path or vice versa.
- [x] **Classify on-curve destinations** — add the maintained curve check as a
  send-only predicate; prove off-curve parsing/history remains valid while
  native SOL destination submission rejects it before RPC.
- [x] **Add checked lamports** — introduce a private/native `u64` lamport value
  with checked add/subtract; reject zero, negative, fractional-lamport,
  overflow, and out-of-range public payment inputs; prove `u64::MAX` round-
  trips and one lamport above it fails before RPC.
- [x] **Convert exact SOL decimals** — prove `1 SOL = 1_000_000_000` lamports
  with exact base-10 formatting and no floating point.
- [x] **Add native SOL identity** — define only the Solana chain/network/native
  asset facts needed by wallets and indexing; do not add SPL or Token-2022.
- [ ] **Parse configured seeds** — accept exactly 64 lowercase ASCII hex
  characters from the named environment value; reject prefixes, whitespace,
  uppercase, wrong length, and alternate keypair encodings.
- [x] **Use shared secret handling** — pass an accepted decoded seed through
  the existing `SecretBytes` boundary into a Solana-private key owner. Match
  Bitcoin/Ethereum behavior: zeroize owned secret bytes on drop, expose no
  secret through errors or ordinary traits, and add no stronger Solana-only
  guarantee for environment strings or rejected decode temporaries.
- [x] **Generate Ed25519 seed material** — generate one OS-random 32-byte seed
  through a Solana-private deterministic failure seam that is not production
  configurable; do not construct or return a wallet yet.
- [x] **Sign and verify locally** — sign one exact message, derive its canonical
  first-signature ID, verify locally, and reject any mismatched key/signature.
- [x] **Pass the package foundation gate** — run Rust-1.91 package checks,
  address/key/value tests, dependency tree review, design lint, formatting, and
  diff checks before adding RPC behavior.

## Account Reads & Wallets

- [x] **Add the Solana RPC double** — create a deterministic loopback harness
  that asserts exact method, params, call count, ordering, body limit, and
  cancellation without any public network.
- [x] **Verify the singular RPC client** — prove the package slice's one
  endpoint uses fixed headers, timeout, maximum response bytes, and
  `request_once`; expose no endpoint list, failover, transparent retry, or
  ignored safety field.
- [x] **Map Solana RPC failures** — preserve transport, HTTP, JSON-RPC,
  malformed-data, resource, retryable-source, and post-wire ambiguity classes
  without using provider prose as authority.
- [x] **Read the genesis hash** — implement one-shot `getGenesisHash` with
  canonical Base58/32-byte validation.
- [x] **Read node health** — implement one-shot `getHealth`; only exact `"ok"`
  admits account acquisition.
- [x] **Read contextual slots** — implement confirmed/finalized `getSlot` with
  an optional `minContextSlot`; accept JSON integers from zero through
  `u64::MAX`, reject signed/fractional/exponent/string/Boolean/collection forms,
  and validate the exact sent floor without narrowing or arithmetic.
- [x] **Read one contextual account** — implement Base64 `getAccountInfo` with
  commitment/floor, exact context, null handling, and complete account fields
  for the Memo readiness probe.
- [x] **Read multiple contextual accounts** — implement one
  `getMultipleAccounts` call for at most 100 addresses with Base64/full data,
  no `dataSlice`, exact cardinality, and positional mapping.
- [x] **Decode strict account data** — require exact `[string, "base64"]`,
  strict alphabet/padding, canonical 32-byte owner, valid lamports/executable/
  space, and decoded-length agreement before semantic classification. Require
  `context.slot` even for `value: null` and ignore additive `apiVersion` or
  unrelated context metadata regardless of its JSON value.
- [x] **Read finalized SOL balance** — implement exact finalized `getBalance`
  with context validation and checked lamport conversion.
- [x] **Prove RPC endpoint affinity** — run every account/identity method
  through one client and prove one-shot call counts, no switch, and no retry.

- [x] **Resolve source occurrences** — map every authored item to its actual
  wallet/address and retain the earliest original occurrence for each source.
- [x] **Create lexical source leases** — add process-local preparing state
  without leaking it into a generic crate or PostgreSQL. Submitted and
  ambiguous envelope ownership remains in the Native Submission phase.
- [x] **Acquire sources canonically** — acquire all distinct sources by
  canonical bytes, fail fast/all-or-nothing, and release provisional leases in
  reverse ownership-safe order.
- [x] **Report source busy truthfully** — map an occupied source to the earliest
  original occurrence as `503 SourceBusy`, with no accepted or ambiguous ID
  from the new invocation.
- [x] **Reject self transfers** — reject every resolved source-equals-
  destination occurrence before health or account RPC.
- [x] **Validate destination syntax first** — parse and require on-curve native
  destinations before any account request while retaining original indices.
- [x] **Build the stable account query** — append source then destination per
  occurrence, deduplicate by canonical bytes at first occurrence, preserve
  reverse mapping, and cap the one request at 100 unique addresses.
- [x] **Open the confirmed attempt** — execute `getHealth`, then
  `getSlot(confirmed) = F`, exactly once on the singular endpoint.
- [x] **Acquire the account snapshot** — request all addresses with
  `minContextSlot = F`; validate structure/cardinality before interpreting any
  account or calling the closing witness.
- [x] **Validate the account context** — require `C >= F`; keep `F` and `C`
  provisional and private to the attempt.
- [x] **Close the confirmed attempt** — call `getSlot(confirmed,
  minContextSlot = C) = U`, require `U >= C`, and publish none of the three
  floors if this witness fails.
- [x] **Classify destination accounts** — in original-item order accept absence
  or a non-executable, zero-data, System-owned account; assign coherent semantic
  failures to the earliest truthful occurrence.
- [x] **Classify source accounts** — require the same supported System-account
  shape; treat absence as zero balance for later sufficiency checks.
- [x] **Handoff snapshot atomically** — publish only witnessed `U` together
  with complete eligibility and source-balance facts; never publish a floor by
  itself, and never treat the observed balance as a durable reservation.
- [x] **Cancel every account await** — race health, both slot reads, account
  fetch, decoding boundary, classification boundary, and pre-handoff point;
  leave no background work, floor, task registration, or lexical lease.
- [x] **Reject oversized account responses** — enforce configured response
  bytes before unbounded buffering and prove zero downstream effects.
- [x] **Prove false-high behavior** — reject an unclosed `u64::MAX` claim
  without publication and allow a self-consistent `F = C = U = u64::MAX` only
  as one live atomic handoff that later stages may reject.
- [x] **Prove acquisition atomicity** — cover malformed JSON/Base64/owner/
  fields, short/extra values, below-floor responses, timeouts, cancellation,
  and semantic shape errors with exact index-free versus item-scoped outcomes.
- [x] **Pass the account acquisition gate** — run Rust-1.91 identity/account
  RPC, decoding, acquisition, cancellation, source-lease, balance, and
  design-lint tests with zero public RPC access.

## Native Submission

- [x] **Create the submission registrar contract** — define one Solana-owned
  registration capability whose success means the application has inserted
  the task before dispatch; do not depend on Tokio task ownership in generic
  crates.
- [x] **Model immutable envelope state** — privately bind source, original
  occurrence, message, first signature, exact signed bytes, operation floor,
  blockhash, and last valid block height.
- [x] **Generate Memo tokens** — generate a fresh opaque 256-bit OS-random value
  for every occurrence and encode its raw bytes as canonical Base58; expose no
  caller or payment data.
- [x] **Build the System instruction** — use the maintained System interface to
  construct one native transfer with the source as fee payer and only signer.
- [x] **Build the Memo-v3 instruction** — use exactly
  `MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr`, zero accounts, and the unique
  token; support no program override or memo-less fallback.
- [x] **Build one legacy message** — create exactly System transfer then Memo
  for one occurrence and prove instruction order/account roles.
- [x] **Distinguish identical payments** — prove duplicate items and sequential
  sends sharing a blockhash receive different messages and first signatures.
- [x] **Read the blockhash lifetime** — implement confirmed
  `getLatestBlockhash` with current `minContextSlot` and keep blockhash, context,
  and `lastValidBlockHeight` atomic.
- [x] **Advance preparation floors** — require each contextual preparation
  response to meet the sent floor and advance it monotonically; bare block
  height never advances a slot floor.
- [x] **Read one exact fee** — implement sequential `getFeeForMessage` using
  Base64 of the exact bincode message; reject null as failure, never zero.
- [x] **Accumulate source requirements** — checked-sum every amount plus exact
  fee by source without crediting incoming transfers in the same batch.
- [x] **Reject insufficient sources** — compare the complete checked totals to
  the acquired snapshot and report the first original item made impossible by
  the fixed stage order.
- [x] **Sign each message once** — sign in original order, verify every local
  signature, serialize exact bytes, and retain the locally derived first
  signature as sole transaction-ID authority.
- [x] **Require distinct envelopes** — reject duplicate messages, signatures,
  or signed bytes inside one fully prepared batch.
- [x] **Simulate one exact envelope** — implement sequential
  `simulateTransaction` with Base64, confirmed, `sigVerify: true`,
  `replaceRecentBlockhash: false`, and the current floor; require `err == null`.
- [x] **Prove full preparation atomicity** — every address, RNG, blockhash, fee,
  arithmetic, signing, encoding, or simulation failure must yield zero
  broadcasts and release only this invocation's lexical leases. Preserve an
  original index for a truthful item failure and no index for an operation-wide
  RPC, coherence, cancellation, or resource failure.
- [x] **Check lifetime equality** — implement confirmed `getBlockHeight` with
  the current floor and allow broadcast while height equals the last valid
  height; expire only when it is greater.
- [x] **Register before dispatch** — transition lexical leases into guarded
  immutable envelopes, register the application task, and begin no wire call
  until insertion acknowledgement succeeds.
- [x] **Reject closed registration** — if registration closes or acknowledgement
  is lost before insertion, fail definitely before dispatch and create no
  ambiguous guard.
- [x] **Broadcast exact bytes** — implement one-shot `sendTransaction` with
  Base64, preflight enabled, confirmed preflight commitment, current floor, and
  `maxRetries: 0`.
- [x] **Validate the returned signature** — accept only canonical equality with
  the locally derived first signature; a mismatch after the wire boundary is
  ambiguous, never a provider-authoritative ID.
- [x] **Define the wire boundary** — treat timeout, disconnect, cancellation,
  every JSON-RPC error code, malformed/uncorrelated response, internal error,
  and signature mismatch after the call begins as unknown acceptance.
- [x] **Read one historical status** — implement
  `getSignatureStatuses([local_id], searchTransactionHistory: true)` with exact
  one-entry cardinality, context at least the operation floor, transaction slot
  inside the inclusive floor-to-response-context range, valid status shape,
  and no floor advancement.
- [x] **Map non-null status** — treat any coherent non-null entry, including an
  execution error, as submitted while leaving final success/failure to indexing.
- [x] **Keep unavailable status unknown** — null, malformed, short, extra,
  incoherent, or low-context status proves neither landing nor absence and
  permits no unsafe source release.
- [x] **Authorize exact replay** — only after a valid null status and confirmed
  block height at or below expiry may the coordinator resend the identical
  bytes.
- [x] **Bound wire submissions** — permit at most the original call plus two
  identical replays, checking status and height between attempts and once after
  the final unknown response.
- [x] **Stop the ordered batch** — submit in original order; expose only the
  definitely acknowledged prefix, attach the ambiguous original index, and
  attempt no later item after definite/ambiguous broadcast outcome. Release
  sources used only by unattempted later items while retaining the ambiguous
  occurrence's source guard.
- [x] **Detach the HTTP waiter safely** — handler cancellation after task
  registration drops only its result waiter; submission and reconciliation
  stay application-supervised.

Submission construction and dispatch pause here. Complete the sparse indexing
and wallet-handoff steps below before implementing history-based reconciliation
or exposing the concrete wallet/provider/sender.

## Indexing & Central Database

Before the first source edit, the **Block Coordinates**, **Sparse
Synchronization**, and **Wallet Handoff** cross-plan gates must be closed. The
Solana plan owns the following native source/interpreter work; the central plan
must link its **Add the Solana source** and **Prove Solana edge cases** evidence
here rather than implementing a second source.

- [x] **Add indexing RPC methods** — implement finalized
  `getFirstAvailableBlock`, closed-range `getBlocks`, and full/json/version-0/
  no-rewards `getBlock` with strict bounds and response shapes.
- [x] **Create the tip request fixture** — model `getSlot`, both lower-bound
  samples, exact closed windows, and complete selected block responses.
- [x] **Traverse backward tip windows** — search descending 10,000-position
  windows from finalized `T`, sending `minContextSlot = T` on every
  `getBlocks` call, without fabricating `T` as a produced block.
- [x] **Validate tip enumeration** — reject unordered, duplicate, out-of-range,
  empty-retained-range, `A0 > T`, and unavailable selected-candidate results.
- [x] **Validate the tip block** — require hash, previous hash, actual parent
  slot, non-null produced block height, and the exact requested slot pairing.
- [x] **Create the sparse range fixture** — cover positions `100, 103, 107`,
  produced heights `50, 51, 52`, skipped positions, and returned-block limits.
- [x] **Open the range pruning bound** — sample
  `A0 = getFirstAvailableBlock()` before every range enumeration and reject an
  attempt whose required start, anchor, or checkpoint is already below it.
- [x] **Return empty past-tip ranges** — when `start > tip.position`, return an
  empty generic range without issuing an enumeration or block RPC.
- [x] **Traverse forward sparse windows** — use checked inclusive numeric
  windows sized as `min(max(remaining, 10_000), 500_000)`, append only earliest
  remaining produced slots, send `minContextSlot = tip.position`, and never
  synthesize skipped slots.
- [x] **Validate range enumeration** — require strict increasing uniqueness and
  exact range membership while validating but deferring a response suffix;
  prove the implementation does not substitute `getBlocksWithLimit`.
- [x] **Fetch every selected block** — request each selected slot with the exact
  finalized/full/json/version-0/no-rewards configuration, pair the response to
  its requested slot, and make an unavailable/incomplete selected block a
  retryable result with no checkpoint advance.
- [x] **Enforce known-tip consistency** — reject a final covered window that
  omits the independently proved produced tip.
- [x] **Enforce enumeration bounds** — cap one window at 500,000 numeric
  positions and one attempt at 64 enumeration calls.
- [x] **Enforce the source deadline** — apply one 30-second monotonic deadline
  to every RPC future and loop edge, independent of per-call timeout.
- [x] **Fail source attempts atomically** — deadline, call-budget, arithmetic,
  or cancellation exhaustion discards every fetched fact, publishes no tip,
  and permits no commit; deadline and call-budget exhaustion are retryable
  source errors.
- [x] **Cancel source attempts fully** — cancellation discards all fetched
  facts, publishes no tip, and permits no checkpoint commit.
- [x] **Close the pruning sandwich** — sample `A1` after all selected blocks and
  reject a plan when any required start/anchor/checkpoint has been pruned.
- [x] **Validate child continuity** — require increasing slot, produced height
  exactly parent height plus one, and exact parent position/hash equality.
- [x] **Lookup canonical references** — query the stored native slot and return
  a complete reference; never subtract one from a sparse slot.
- [x] **Detect complete canonical mismatch** — treat only a complete changed
  hash or parent as ordinary bounded-reorg evidence.
- [x] **Prove canonical omission** — require `T > S`, both lower bounds at or
  below `S`, and an empty exact `[S,S]` enumeration with `minContextSlot = T`
  before treating omitted stored slot `S` as mismatch.
- [x] **Keep unavailable data retryable** — null, pruned, incomplete,
  temporarily unavailable, failed-witness, or `T <= S` data is not reorg
  evidence.
- [x] **Implement the Solana source contract** — connect complete tip, bounded
  range, and canonical lookup to the accepted generic `BlockSource` without a
  Solana-specific synchronizer.

- [x] **Validate transaction identity** — require canonical first-signature
  IDs, coherent signer/signature cardinality, and a first static fee payer that
  is marked as a signer for every admitted transaction.
- [x] **Resolve legacy account keys** — map static keys and fee payer in exact
  message order.
- [x] **Resolve version-zero keys** — append loaded writable then loaded
  read-only addresses exactly as the protocol defines.
- [x] **Validate balance vectors** — reject missing metadata/loaded addresses,
  invalid indices, or pre/post length mismatch before emitting facts.
- [x] **Validate inner instruction groups** — reject duplicate outer-index
  groups; for a successful relevant transaction require present inner metadata,
  treating `[]` as valid but omitted/null as incomplete.
- [x] **Map transaction status** — use `meta.err` for failed status and never
  invent meaning from provider prose.
- [x] **Map the actual fee** — preserve exact `meta.fee` for successful and
  failed transactions with the resolved fee payer.
- [x] **Decode top-level transfers** — use the maintained System interface to
  decode supported `Transfer` instructions in execution order; transfer-shaped
  bytes owned by any non-System program produce no movement.
- [x] **Decode transfer-with-seed** — use the protocol-defined destination
  account index and retain exact source/destination/lamports.
- [x] **Decode inner transfers** — preserve outer/inner execution order and
  deterministic movement identities.
- [x] **Preserve transfer occurrences** — keep repeated and self-transfer facts
  distinct while omitting zero-lamport movements.
- [x] **Suppress failed movements** — failed transactions retain actual fee and
  status but emit no attempted value movement.
- [x] **Apply relevance policy** — persist transactions only when an active
  address is fee payer or endpoint of a supported movement. Once relevant,
  retain every supported movement in that transaction, including movements
  whose endpoints are both outside the active filter.
- [x] **Shield selected balances** — reconcile selected-wallet pre/post deltas
  against supported movements and fee so unsupported value effects fail the
  whole block.
- [x] **Reject unsupported versions** — transaction versions above zero fail
  the complete block with no partial history or checkpoint movement. Raising
  the supported version requires separate decoder and fixture evidence before
  cluster activation.
- [x] **Emit no UTXO changes** — every Solana interpreted block returns empty
  output changes.
- [x] **Pass interpreter completeness** — cover legacy/v0, loaded addresses,
  outer/inner transfers, failed transactions, fees, unsupported effects,
  relevance, and all-or-nothing block behavior.
- [x] **Pass sparse source evidence** — cover backward/forward windows, huge
  gaps, skipped birthdays, prefix resume, pruning movement, deadlines, call
  budgets, cancellation, unavailable blocks, retained reorg, restart, and deep
  reorg failure.
- [x] **Link central-plan evidence** — mark its Solana source/edge-case steps
  complete only by linking these exact source/interpreter tests; do not copy
  implementation or create Solana-only storage tables.
- [x] **Prove PostgreSQL coexistence** — after central composition, write
  Bitcoin, Ethereum, and Solana scopes plus native/token assets through one
  pool/schema; prove isolation and byte-for-byte unchanged `payment_wallets`.
- [x] **Pass the indexing gate** — run generic indexing, runtime, both
  repository contracts, Solana source/interpreter, PostgreSQL, and design-lint
  gates under Rust 1.91 with no silent test skip.

The central **Wallet Handoff** gate and native indexing evidence are now
available. Return to the **Native Submission** workstream and complete its
history-dependent surface:

- [x] **Wake reconciliation on checkpoints** — inject the matching scope's
  checkpoint notification and reset deterministic backoff on progress.
- [x] **Cap reconciliation backoff** — retry status/history from 500 ms doubling
  to 10 s without a busy loop and without changing endpoint or envelope bytes.
- [x] **Resolve indexed presence** — clear a guard as submitted when canonical
  source history contains the exact signature, regardless of execution status.
- [x] **Start an absence proof** — require confirmed height beyond expiry and a
  finalized checkpoint height at least the last valid height before scanning.
- [x] **Scan absence pages** — traverse source history in pages of at most 100
  against one checkpoint-bound cursor until exhaustion.
- [x] **Reject unstable absence** — discard the scan on cursor conflict,
  checkpoint change, reorg, pruning, page failure, gap, or incomplete
  traversal; wait for later evidence.
- [x] **Release on proven absence** — only one complete exhausted scan at one
  unchanged checkpoint may classify expiry-without-landing and release the
  guarded source.
- [x] **Hold unresolved sources indefinitely** — null status, unavailable
  history, indexer failure, pruning, or lost in-process state never becomes a
  false absence proof.
- [x] **Build the Solana wallet view** — expose canonical address, native SOL
  balance, and checkpoint-bound history through existing wallet capabilities
  without returning secret material.
- [x] **Build the Solana provider** — inject the singular client, index
  capabilities, and completed shared submission coordinator into generated and
  imported wallets; generation and import converge on the same native
  validation path, successful generation derives the address from its signer,
  and generated wallets remain process-lifetime only. Prove an injected
  randomness failure through `Wallets::generate` remains typed as
  `wallets::ErrorKind::Generation` and registers no wallet/filter.
- [x] **Route single sends to the coordinator** — implement the Solana wallet's
  chain-native transaction/broadcast bridge so `Wallets::send` enters the same
  coordinator exactly once and never creates an independent source guard.
- [x] **Build the Solana batch sender** — route the registered-family `Sender`
  batch path to that same private coordinator rather than adding a universal
  transaction/RPC abstraction.
- [x] **Defend the Solana sender bound** — reject impossible zero/51-item direct
  calls before account RPC, RNG, fee, signing, simulation, or task
  registration; prove success at 1 and 50.
- [x] **Prove shared coordinator identity** — an ambiguity entered through
  single send must block batch send for the same source, and the inverse must
  also hold.
- [x] **Prove submission state is ephemeral** — prove neither PostgreSQL nor
  redb persists source leases, signed envelopes, attempts, guarded outcomes,
  replay counters, or reconciliation state.
- [x] **Prove the restart limitation** — deterministic tests and documentation
  show that process restart, active-active writers, response loss, or a new
  logical invocation may double-pay because durable idempotency is out of scope.
- [x] **Pass the account/wallet gate** — run Rust-1.91 Solana provider, wallet,
  balance, history, sender, shared-coordinator, and public-contract tests with
  owned doubles only.
- [x] **Pass the submission gate** — run all construction, fee, balance,
  signature, simulation, replay, status, cancellation, prefix, reconciliation,
  and shared public-error tests under Rust 1.91 with owned doubles only.

## Runtime & Release Evidence

The **Schema Knowledge**, **Adapter Safety**, and **Central Composition** gates
must be closed before the application adds a Solana repository. Startup must
perform every chain identity/Memo check before opening the PostgreSQL pool.

- [ ] **Specify closed Solana configuration tests** — after the central plan
  owns the top-level PostgreSQL contract, cover exact Solana keys, unknown
  fields, singular endpoint, forbidden controls, per-chain database rejection,
  and old `start_height` rejection without reimplementing PostgreSQL config.
- [ ] **Add singular Solana config** — accept only network, expected genesis,
  singular RPC settings, and sync settings inside `indexes.solana`; configured
  imports remain in the existing top-level wallet list. Reject aliases and
  ignored commitment/lag/reference/retry/priority/Memo fields.
- [ ] **Add Solana start positions** — after the central plan's **Rename
  birthdays** cutover, require `start_position` for configured Solana imports
  and prove old `start_height` remains rejected across every chain.
- [ ] **Expose public Solana variants** — add `Chain::Solana` and
  `WalletAsset::Sol`, then map the already-added wallet Base58 vocabulary into
  the public `AddressEncoding::Base58`; update every exhaustive configuration
  and composition match in one compiling slice.
- [ ] **Publish Solana OpenAPI** — expose native SOL and Base58 only on existing
  wallet/transaction routes; add no Solana-specific endpoint or SPL asset.
- [ ] **Parse imported seed environments** — load through the configured
  environment-variable name, pass accepted bytes through `SecretBytes`, and
  never include seed text in config/debug/error/OpenAPI output. Use the same
  temporary-value handling as current Bitcoin/Ethereum imports.

- [ ] **Test genesis before database** — prove a wrong canonical genesis fails
  before pool construction or schema access.
- [ ] **Test Memo before database** — prove absent, non-executable, malformed,
  or below-floor Memo-v3 state fails before pool construction.
- [ ] **Verify Solana identity** — call one-shot `getGenesisHash` and compare
  canonical expected bytes.
- [ ] **Verify executable Memo-v3** — obtain finalized `S`, request exactly the
  accepted Memo account with `minContextSlot = S`, and require context at least
  `S` plus a non-null executable account. This startup probe is distinct from
  the per-send confirmed `getHealth` admission and does not replace it.
- [ ] **Order identity before pool access** — move no database contract into
  the Solana crate; prove every configured chain identity and Memo check
  succeeds before the central plan's one PostgreSQL pool is constructed or its
  read-only schema validator runs.
- [ ] **Construct the Solana scope handle** — clone the shared pool into one
  exact `(solana, network)` repository; do not partition by native asset.
- [ ] **Load the Solana checkpoint** — load persisted scope state before
  coordinator/service construction and reject incompatible/incomplete rows.
- [ ] **Construct filter coordination** — initialize the central-plan
  checkpoint/revision coordinator and its checkpoint notification from the
  persisted checkpoint.
- [ ] **Compose the Solana service** — build client, source, interpreter,
  service, provider, coordinator, and sender once; inject the same service's
  checkpoint/history/notification views into submission.
- [ ] **Add Solana to the Composer** — register the native indexer through the
  existing chain-neutral `Indexer` surface.
- [ ] **Register native SOL only** — add one Solana wallet family and no SPL,
  plaintext durable registry, remote custody, or hidden asset partition.
- [ ] **Import before first snapshot** — reconstruct every configured seed and
  publish its explicit `start_position` before synchronization captures its
  first filter revision.

- [ ] **Add runtime-loop tests** — cover CatchingUp, Ready with persisted
  checkpoint, retryable error, successful recovery, fatal error, and
  cancellation without making runtime own coordinator state.
- [ ] **Remove the readiness spawn** — turn the readiness bridge into an async
  body tracked by the application; leave no bare self-owned `tokio::spawn`.
- [ ] **Create submission supervision** — add one precise application-owned
  `mpsc` admission queue and `JoinSet`; retain the only close/wait controls.
- [ ] **Acknowledge after insertion** — registration succeeds only after the
  submission task is visible in the tracked set.
- [ ] **Reject registrar closure** — prove close-winning and lost-
  acknowledgement races fail before dispatch.
- [ ] **Track synchronization** — supervise the indexing runtime future and
  make its completion/error visible to the application.
- [ ] **Track readiness** — supervise readiness publication and make unexpected
  completion/error visible to the application.
- [ ] **Gate listener startup** — bind HTTP only after every configured index
  reports Ready with a persisted checkpoint and all configured imports are
  visible.
- [ ] **Fail on startup index exit** — if the Solana indexer exits fatally
  before listener admission, stop startup, join owned tasks, and return the
  fatal error without opening HTTP.
- [ ] **Handle retryable source errors** — publish not-ready until successful
  catch-up without terminating the process or admitting stale work.
- [ ] **Handle fatal indexer exit** — immediately publish not-ready, close new
  HTTP admission, and return the fatal error only when no guarded envelope
  still requires in-process evidence.

- [ ] **Stop HTTP admission gracefully** — on shutdown publish not-ready and
  stop new admission without dropping the serving future.
- [ ] **Serialize registrar close** — prove a registration winner is inserted
  before dispatch while a close winner prevents dispatch.
- [ ] **Drain admitted handlers** — wait for handlers before inspecting guarded
  envelopes.
- [ ] **Finish preparation on shutdown** — pre-dispatch work may end without
  creating an ambiguous guard or submission task.
- [ ] **Drain registered sends** — wait for every application-tracked submission
  task instead of detaching it.
- [ ] **Keep evidence services alive** — while any envelope is unknown, keep
  synchronization, status reconciliation, and history access running.
- [ ] **Hold the ambiguity barrier** — graceful shutdown has no automatic
  deadline while evidence is unavailable.
- [ ] **Handle fatal-index ambiguity** — after fatal indexing, only positive
  historical status may clear a guard; otherwise only explicit force-kill can
  accept duplicate-payment risk.
- [ ] **Cancel sync after guards clear** — once the guard set is empty, cancel
  synchronization and await submission, readiness, storage, and remaining
  supervised tasks.
- [ ] **Prove shutdown order** — verify not-ready, stop admission, close
  registrar, drain handlers, drain guards, cancel sync, and join tasks in that
  exact order.

- [ ] **Declare the Solana system target** — add explicit `[[test]] name =
  "solana_stack"` with path `tests/solana_stack.rs` in `apps/api/Cargo.toml`
  because application autotest discovery is disabled.
- [ ] **Record pinned validator artifacts** — identify Agave v3.1.14 commit
  `3134055b562e95902233be308453fffa1c4a8902` and commit platform-specific
  SHA-256 values before executing any downloaded binary.
- [ ] **Acquire the validator artifact** — with separate network approval,
  download only the exact platform artifact named by the checksum manifest into
  harness-owned cache/storage; do not execute it yet.
- [ ] **Reject checksum mismatch** — make the harness refuse to start an
  artifact whose bytes do not match its committed platform checksum.
- [ ] **Own validator resources** — allocate an isolated temporary ledger,
  ports, keys, child process, logs, disposable PostgreSQL database/schema, and
  cleanup. Create an ephemeral local payer and fund it only from the owned
  validator's genesis/faucet; use no externally funded key or public endpoint.
  Retain an application-owned sentinel to prove the test does not mutate
  custody rows.
- [ ] **Verify validator genesis** — prove the owned fixture's configured
  identity before application database setup.
- [ ] **Verify bundled Memo-v3** — require executable `spl_memo-3.0.0.so` at the
  exact accepted account; do not invent a separate Memo download.
- [ ] **Execute the native wire transaction** — submit the exact legacy
  System-transfer-plus-Memo bytes to the owned validator and match the local
  signature.
- [ ] **Index the native transaction** — prove the finalized transaction becomes
  canonical SOL history with one System movement, exact fee, and no UTXO rows.
- [ ] **Keep negative fixtures owned** — exercise wrong genesis, missing Memo,
  unavailable history, malformed data, and unsupported version with
  deterministic doubles/fixtures rather than public clusters.
- [ ] **Choose CI ownership** — until a checked-in workflow owns the pinned
  tools and explicit target, report `solana_stack` as a manual integration
  target rather than automated CI evidence.

- [ ] **Update capability evidence** — move each Solana row in
  `FEATURE_VALIDATION.md` from Accepted/unimplemented only when the cited code
  and focused tests for that row exist.
- [ ] **Update current API examples** — replace target-only caveats only after
  the exact runtime config, enums, errors, and OpenAPI are implemented.
- [ ] **Update architecture evidence** — retain accepted constraints while
  distinguishing implemented source from accepted limitations such as
  process-local custody and non-durable idempotency.
- [ ] **Run focused crate gates** — execute every changed crate's Rust-1.85
  locked tests, including non-skipping PostgreSQL contracts and the explicit
  Solana target where its owned fixture is available.
- [ ] **Run workspace release gates** — run formatting, locked workspace check
  and tests, strict Clippy, docs, design lint, and `git diff --check`; report
  unavailable tools and pre-existing failures separately.
- [ ] **Review the final dependency boundary** — prove `chain-solana` depends
  only on packages, base, indexing, and wallets and that deleting it leaves
  Bitcoin, Ethereum, and generic crates usable.
- [ ] **Review the final safety boundary** — prove no secrets/logged endpoints,
  public-network tests, live signing, migration execution, Solana-specific
  generic DTOs, hidden retries, SPL support, or unapproved schema changes were
  introduced.
- [ ] **Prepare the implementation handoff** — summarize exact implemented
  capabilities, tested/untested boundaries, process-local duplicate risks,
  database transition status, and the first separately authorized deployment
  action; do not commit or deploy unless asked.

## First approval boundary

The first implementation step is **Record repository state**. It is
read-only and changes no files. The first source-changing step cannot occur
until the subsequent MSRV evidence proves the exact compatible dependency
direction.
