# Repository guide for coding agents

## Scope and source of truth

This file applies to the entire repository. Read it before changing code.

Use these sources in this order:

1. `docs/SYSTEM_REQUIREMENTS.md` for canonical system requirements and acceptance criteria.
2. `ARCHITECTURE.md` for concise ownership and dependency rules.
3. `docs/CONTRACTS.md` for how the current Rust traits compose.
4. `docs/FEATURE_VALIDATION.md` for requirement traceability and accounting corrections.
5. `docs/INDEXING.md`, `docs/REQUIREMENTS.md`, and `docs/RESEARCH.md` for focused design context and open decisions.
6. Current code and tests for implemented behavior.

Do not silently resolve an item listed as an open decision. Record or propose the decision first, then update the canonical docs together with the implementation when approval is in scope.

The repository is contract-first. The stateless Bitcoin/Ethereum Wallet Service execution path is implemented, while Payment Service, Indexer Service, concrete storage, transport, and Trezor integration are still largely contracts or composition placeholders. Do not describe scaffolded behavior as production-complete.

`old/` is a recoverable previous design and `reference/` contains upstream research material. Both are excluded from the workspace. Do not copy their architecture into production code or edit them unless the task explicitly targets them.

## Workspace map

| Path | Ownership |
|---|---|
| `apps/api` | Payment Service (PS) composition root and user/deposit orchestration |
| `apps/indexer` | Indexer Service (IX) composition root |
| `apps/wallet` | Stateless Wallet Service (WS) composition root and facade |
| `sdk/chains/identity` | Opaque cross-process identifiers and 256-bit atomic amounts |
| `sdk/chains/contract` | Small stateless, chain-typed wallet/transaction capabilities |
| `sdk/chains/bitcoin` | All Bitcoin RPC, address, UTXO, transaction, signing-payload, collection, and indexing types |
| `sdk/chains/ethereum` | All Ethereum RPC, address, EIP-1559, ERC-20, collection, and indexing types |
| `sdk/signing/contract` | Chain-independent key provisioning and signing contracts |
| `sdk/signing/local` | Ephemeral in-memory secp256k1 signer for tests and local examples only |
| `sdk/signing/trezor` | Placeholder for chain-independent Trezor operations |
| `sdk/transactions/utxo` | Pure reusable UTXO selection/funding contracts; no Bitcoin protocol code |
| `sdk/transactions/account` | Narrow reusable account-model construction; not an Ethereum universal model |
| `sdk/indexing` | Chain-independent sync, checkpoint, watch, observation, finality, replay, and reorg contracts |
| `sdk/deposits` | PS-owned deposits, event mirror, classification, ledger, and durable collection workflows |
| `sdk/storage` | Backend-independent atomic persistence mechanics |
| `packages/*` | Transportable non-blockchain infrastructure; may depend only on other `packages/*` |

## Architectural rules

- Preserve chain-native types through build, signing, broadcast, and receipt handling. Do not invent a universal Bitcoin/Ethereum transaction or wallet model.
- Keep `Chain` as the associated-type map and prefer the small capability traits in `chain-contract`. Do not add a god `ChainService`.
- Keep composition roots thin. Applications select concrete chains, signers, storage, transport, and workers; SDK crates implement reusable behavior.
- WS is stateless. It may generate addresses, read balances, build, sign, broadcast, read receipts, report requirements, and execute one collection transaction. It must not own deposits, watches, retries, reservations, ledgers, databases, or multi-leg token workflow state.
- PS owns users, deposits, idempotency, the IX event mirror, movement classification, accounting, reservations, retries, collection legs, and master destinations.
- IX owns watches, canonical checkpoints, chain facts, confirmation/finality, replay cursors, undo data, and reorg processing. IX must not label facts as deposits, sweeps, gas funding, or user credits.
- Generic signing knows keys, curves, payloads/digests, schemes, encodings, tweaks, and user interaction only. It must not import chain transactions, RPC, wallets, or indexing types.
- A concrete chain may receive `&dyn Signer`; it must not select or construct local, Trezor, HSM, KMS, or remote custody.
- Concrete Bitcoin/Ethereum RPC methods stay in their chain crates. `packages/json-rpc` owns framing only.
- `packages/*` must not import `sdk/*`.
- Do not introduce flat `crates/`, catch-all `core`/`common`/`utils` packages, chain-specific signer packages, or a `signing_plan` layer.
- The chain deletion test is mandatory: removing one concrete chain crate should remove all of that chain's types without breaking generic signing, indexing, storage, transport, or reusable transaction algorithms.

## Transaction and custody invariants

The required flow is:

```text
chain-native request
  -> unsigned chain-native transaction
  -> chain computes message/digest
  -> injected signer signs the cryptographic payload
  -> chain validates and inserts the signature
  -> signed chain-native transaction
  -> broadcast
```

- Bitcoin owns scripts, address/network checks, UTXOs, dust, fee/weight calculations, RBF sequences, sighashes, witnesses, consensus encoding, and any future PSBT integration.
- Current Bitcoin signing supports native SegWit v0 P2WPKH and Taproot key-path inputs. Verify that every input script belongs to the requested key before signing.
- Taproot address generation uses the untweaked x-only internal key; the chain code applies the BIP341 tweak. Only public tweak material crosses the signer boundary.
- Ethereum owns chain ID, nonce, gas, EIP-1559 fees/envelopes, ERC-20 calldata, receipts, logs, and trace capability reporting.
- Verify Ethereum build context chain ID and recover the signer from the signature before accepting a signed envelope.
- Use integer atomic units only. Never use floating point for money. Keep checked arithmetic and reject overflow, underflow, dust, zero-value collections, and insufficient fee/gas balances explicitly.
- `KeyLocator` is opaque. Never derive business or chain semantics from its string representation.
- `LocalSigner::ephemeral_for_testing()` is not production custody. It must not export, log, serialize, or debug-print private keys.
- Native hardware-wallet transaction protocols are an unresolved architecture decision. Do not make Bitcoin depend on `signer-trezor` or teach the base signer Bitcoin types.

## Indexing, deposits, and accounting invariants

- A canonical checkpoint includes height and hash, not height alone. Block effects, undo data, observation revisions, event-feed rows, and checkpoint movement must be committed atomically by a real IX store.
- Mempool state is non-canonical and remains separate from block inclusion. RPC outages are unknown/retryable states, not proof that a transaction was dropped.
- Reorgs append new revisions and corrected absolute ledger rows. Never delete historical IX events or PS ledger rows.
- Observation idempotency uses event ID/revision, not `(txid, status)`, because a status may recur after reorg and re-inclusion.
- IX facts support multiple stable movements. Never collapse a UTXO transaction into a fake single `from -> to -> amount` record.
- A deposit address is not returned until PS has persisted `AwaitingWatch` and IX has durably acknowledged the idempotent watch. Preserve the retry window.
- Every PS ledger entry is a complete absolute snapshot of `received`, `confirmed`, `balance`, `collected`, and `accounted`; it is not a delta.
- Only an explicit PS accounting command may change `accounted`. Only confirmation-qualified collection facts may change `collected`.
- `collected` is gross deposit debit. Keep net master credit and allocated network fee separately.
- Do not use `received - collected - accounted` as sweepable value. Collection eligibility comes from current spendable balance minus reservations, fee reserve, dust, and chain-specific minimums.
- Token gas funding and token sweep are separate durable PS legs. WS performs each stateless operation but never sequences the workflow in memory.

## Rust conventions

- The workspace uses Rust 2024, resolver 3, and MSRV 1.85. Respect pinned dependency versions in the root manifest and `Cargo.lock`.
- `unsafe_code` is forbidden workspace-wide. Do not weaken this lint.
- Prefer precise domain newtypes/enums over strings and booleans when behavior differs by state, chain, asset, network, or transaction stage.
- Keep public interfaces chain-typed and return structured error kinds plus contextual messages. Preserve retryability where source errors expose it.
- Async boundary traits currently use object-safe `BoxFuture<'a, T>` aliases. Follow the existing style unless a repository-wide API change is explicitly approved.
- Derive `Clone`, `Debug`, `PartialEq`, and `Eq` when they support boundary DTOs and deterministic tests. Do not derive or implement `Debug` in a way that exposes secrets.
- Add `#[must_use]` to pure constructors/accessors where ignoring the value is likely a mistake.
- Use `Result`/`Option` and checked arithmetic in production code. `expect` is acceptable in tests when its message states the invariant; avoid `unwrap` and panic-driven runtime handling.
- Keep modules focused. Re-export intentional public API from each crate's `lib.rs`; leave helpers private or `pub(crate)`.
- Put focused unit tests beside the implementation. Use deterministic RPC doubles and offline signers; test success plus wrong-network/key, invalid encoding, overflow, insufficient funds/gas, dust, and duplicate-input boundaries as relevant.
- Comments should explain protocol or ownership invariants, not restate the code.

## Security and external effects

- Never print or commit private keys, seed phrases, signed transaction envelopes, auth headers, API keys, or custody credentials.
- Offline examples may print addresses, public keys, and opaque locators only.
- `apps/wallet/examples/live_ethereum_transaction.rs` can sign and broadcast real funds. Do not run it, set its signing/broadcast approval variables, or use a funded key unless the user explicitly requests that exact external action and has reviewed the transaction fields.
- Treat RPC acceptance as submission, not confirmation. Durable callers must monitor IX/receipts and handle replacement, reorg, and broadcast-response-loss windows.
- Do not add a production network call to a unit test. Integration tests requiring a node must be opt-in and clearly documented.

## Change workflow

1. Inspect `git status --short` and preserve unrelated user changes.
2. Read the relevant canonical docs and the target crate's manifest, public API, implementation, and tests.
3. Identify the owning layer before editing. If a change needs a reverse dependency or crosses PS/WS/IX ownership, stop and revisit the design.
4. Keep the smallest coherent change. Update docs when behavior, ownership, a public contract, or an open decision changes.
5. Add focused tests in the owning crate. Do not claim scaffolded components are implemented because their traits compile.
6. Run scope-appropriate validation and report pre-existing failures separately from failures caused by the change.

## Validation

Use locked dependencies for reproducibility.

```bash
# Always
cargo fmt --all -- --check
git diff --check

# Fast package-focused checks (replace package names)
cargo check --locked -p chain-bitcoin --all-targets
cargo test --locked -p chain-bitcoin
cargo clippy --locked -p chain-bitcoin --all-targets --no-deps -- -D warnings

# Full workspace before handing off substantial or cross-crate Rust changes
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked --workspace --no-deps
```

For Markdown-only changes, `git diff --check` plus a link/path review is normally sufficient. For Cargo or public-API changes, run full workspace check and tests. For chain transaction/signing changes, also run the changed chain's Clippy and focused tests. If full-workspace Clippy exposes a baseline failure outside the change, do not suppress it globally; document it and provide the passing targeted command.

Safe offline examples:

```bash
cargo run --locked -p chain-ethereum --example ethereum_test_wallet
cargo run --locked -p chain-bitcoin --example bitcoin_test_wallet
cargo run --locked -p wallet-worker --example three_asset_wallet_service
```
