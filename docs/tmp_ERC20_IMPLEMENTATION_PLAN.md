# ERC-20 One-Asset Wallet Implementation Plan

## Current Branch Status

Implemented on local branch `implement-ERC20`:

- [x] documentation and application configuration extraction;
- [x] application-owned `WalletAsset::{Btc, Eth, Usdc}` selection without shared/base changes;
- [x] optional allowlisted USDC composition with endpoint-affine startup validation;
- [x] Alloy-typed ERC-20 ABI calls and strict result decoding;
- [x] selected-asset balances and integrity-first history projection;
- [x] exact-transfer simulation, checked token/native balance preflight, and typed terminal errors;
- [x] deterministic generated-USDC API evidence, including signed-envelope intent checks, native
  fee presentation, and mixed ETH/USDC zero-broadcast rejection;
- [x] shared per-sender nonce coordination across ETH and USDC;
- [x] whole-batch Ethereum simulation, aggregate balance checks, and signing before the first
  broadcast;
- [x] exact-envelope reconciliation and nonce blocking after ambiguous submission outcomes.

Phases 1-5 are recorded in commit `944acf1`. Phase 6 is complete in the current working tree but
remains uncommitted. Nothing from this Phase 6 task has been pushed.

Current verification:

- `chain-ethereum`: 74 unit tests and 1 external-adapter integration test pass;
- `json-rpc`: 4 unit tests pass, including the no-failover submission-attempt test;
- `payment-api`: all 21 tests outside the combined reorg case pass, including deterministic
  generated-USDC and Phase 6 batch cases;
- workspace check, documentation, formatting, design-lint, focused strict Clippy, and diff checks
  pass; all workspace tests outside `payment-api` pass;
- the full workspace test gate stops on the pre-existing generated-wallet/filter snapshot race in
  `acceptance::bitcoin_and_ethereum_history_follow_canonical_reorgs`, before that case reaches its
  reorg assertions;
- full workspace strict Clippy stops on pre-existing `collapsible_if` findings in unchanged
  `packages/http` and `packages/design-lint` source.

## Approved Product Model

- One wallet generation selects exactly one payment asset.
- `btc` wallets expose and send BTC only.
- `eth` wallets expose and send native ETH only.
- `usdc` wallets expose and send the configured USDC contract only.
- ETH and USDC generation use separate provider registrations and therefore generate separate
  secrets and addresses.
- An Ethereum address cannot reject unsolicited ETH or unrelated tokens on-chain. The platform
  enforces the selected asset in balance, send, and history presentation.
- A USDC wallet still needs native ETH internally for transaction gas. Its public balance remains
  USDC-only.

## Architecture Decision

Keep every reusable wallet and base definition unchanged:

- no changes to `sdk/chains/base`;
- no changes to `wallets::Wallets`, `wallets::WalletInfo`, private `wallets::Family`, or shared
  balance/history structures;
- no changes to indexing contracts or redb schemas.

Use the existing generic family parameter as an application-owned asset selector:

```rust,ignore
// Shared definition remains unchanged.
wallets::Wallets<I, F>

// Only payment-api chooses this key.
wallets::Wallets<String, WalletAsset>
```

`WalletAsset` is a closed application enum:

```rust,ignore
pub enum WalletAsset {
    Btc,
    Eth,
    Usdc,
}
```

Each key owns exactly one existing provider configuration:

```text
btc  -> Bitcoin WalletProvider
eth  -> Ethereum WalletProvider { asset: Native, decimals: 18 }
usdc -> Ethereum WalletProvider { asset: Erc20(configured_contract), decimals: 6 }
```

ETH and USDC providers share the same long-lived Ethereum RPC and history objects. There is no
new `UsdcProvider` type; `usdc_provider` is only a variable containing an existing
`chain_ethereum::WalletProvider` with token-specific `WalletConfig`.

## Public Configuration and HTTP

Add optional USDC configuration under the enabled Ethereum index:

```json
{
  "indexes": {
    "ethereum": {
      "network": "mainnet",
      "chain_id": 1,
      "usdc": {
        "contract": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
      }
    }
  },
  "wallets": [
    {
      "id": "treasury-usdc",
      "asset": "usdc",
      "secret_env": "TREASURY_USDC_SECRET",
      "start_height": 1
    }
  ]
}
```

- USDC decimals are fixed to the expected value `6` and verified against the contract at startup.
- Reject a malformed, noncanonical, or zero contract address.
- Reject a configured/imported USDC wallet when USDC is not enabled.
- Reject registering the same imported Ethereum address under both `eth` and `usdc`.
- Require exactly one Ethereum RPC endpoint while USDC is enabled so startup chain and contract
  validation is endpoint-affine. Multi-endpoint token admission remains future hardening.
- HTTP callers select `{"asset":"btc"}`, `{"asset":"eth"}`, or `{"asset":"usdc"}`.
- Callers never supply a token contract address, symbol, or decimals.
- Wallet responses include both the selected `asset` and its derived `chain`.
- An unavailable asset registration returns `404` through the existing family lookup behavior.
- This is a pre-release wire replacement; do not add a legacy `chain` request alias.

## Implementation Sequence

### 1. Documentation and application headroom

- Update the canonical requirements, architecture, contracts, and API documentation to say that
  wallet history is complete for the wallet's selected asset.
- Extract application configuration from the 497-line `apps/api/src/main.rs` into
  `apps/api/src/config.rs` before adding fields. Keep concrete composition visible in `main.rs`.
- Keep `docs/FEATURE_VALIDATION.md` unchanged until deterministic USDC evidence passes.

### 2. Application-owned asset selection

- Add `WalletAsset::{Btc, Eth, Usdc}` under `apps/api` with `WalletAsset::chain()`.
- Change only the application's generic instantiation to `Wallets<String, WalletAsset>`.
- Change configured wallet selection and `POST /v1/wallets` from `chain` to `asset`.
- Include `asset` and derived `chain` in public wallet responses.
- Register BTC and ETH as today, plus optional USDC using a second existing Ethereum provider.
- Rely on the existing wallet-family comparison to reject ETH/USDC mixed batches before broadcast.

### 3. Typed, allowlisted ERC-20 boundary

- Add a direct `alloy-sol-types` dependency to `chain-ethereum`.
- Define private typed ABI calls for `balanceOf(address)`, `decimals()`, and
  `transfer(address,uint256)` in the Ethereum crate.
- Replace manual ERC-20 call encoding/decoding with those typed calls.
- At startup, pin validation to one canonical Ethereum block and require:
  - chain identity and every contract probe use one endpoint-affine RPC context;
  - nonempty contract code;
  - exact `decimals() == 6`;
  - an exact 32-byte `balanceOf` response.
- Do not query or trust on-chain `name()` or `symbol()`.

### 4. Selected-asset reads and history

- Keep the public balance response unchanged; the existing wallet configuration already selects
  native `eth_getBalance` or token `balanceOf`.
- Filter history only in `sdk/chains/ethereum/src/wallet/history.rs`:
  - ETH wallets retain native movements;
  - USDC wallets retain only movements emitted by the configured USDC contract;
  - unrelated native/token movements are ignored instead of failing the page;
  - a retained USDC transaction keeps its native ETH network fee;
  - a failed fee-only outgoing transaction is retained only when this wallet paid the fee.
- Preserve the underlying checkpoint and cursor exactly. Sparse or empty selected-asset pages may
  still return a next cursor; do not perform unbounded page filling.

### 5. ERC-20 send safety

- Build token calldata only for the provider's configured contract, with zero native value.
- Simulate the exact transfer using `eth_call` before gas estimation.
- Accept only the exact canonical ABI result `bool true`; reject false, empty, malformed,
  noncanonical, trailing-data, and reverted results.
- Before signing:
  - ETH send: require native balance for value plus worst-case gas;
  - USDC send: require token balance for the amount and native ETH for worst-case gas.
- Keep missing-return, fee-on-transfer, rebasing, reflection, ERC-777 hooks, and arbitrary tokens
  explicitly unsupported.

### 6. Nonce and ambiguous-submission hardening

- Add one Ethereum-owned coordinator shared by ETH and USDC providers.
- Coordinate single and batch sends by sender address, not by payment-asset key.
- Assign checked consecutive pending nonces and complete whole-batch simulation, balance checks,
  and signing before the first broadcast.
- Preserve exact signed bytes for ambiguous outcomes and block later nonce use until the local
  transaction ID is reconciled.
- Submit each signed envelope through one visible `eth_sendRawTransaction` attempt on configured
  endpoint 0; generic JSON-RPC retry or endpoint failover must not hide submission provenance.
- Before assigning a later nonce for the same sender, look up an unknown transaction ID and replay
  the byte-identical envelope when it is not yet known by the node.
- Keep the coordinator explicitly process-local. Operations require one active writer per EOA;
  durable restart recovery is outside this phase.
- This is an Ethereum transaction-safety slice, not a shared wallet/base-model change.

### 7. Deterministic evidence

- Add focused Ethereum tests for typed ABI, startup probes, strict transfer return handling,
  selected-asset history, token/native gas preflight, and malformed responses.
- Extend the in-process Ethereum RPC fixture for USDC code, calls, balances, transfers, receipts,
  and logs.
- Prove create, read, balance, send, confirmation, and history for a generated USDC wallet.
- Prove ETH and USDC generations produce asset-specific behavior and mixed-family batches cause
  zero broadcasts.
- Update `docs/FEATURE_VALIDATION.md` only after the matching tests pass.

## Explicit Boundaries

- Direct `transfer` only: no approvals, `transferFrom`, permits, swaps, bridges, relayers, or
  arbitrary contract calls.
- No gas sponsorship or automatic ETH top-up.
- The EOA must already hold ETH for gas.
- Upgradeable-token behavior, pause/blocklist policy, and contract upgrades remain operator risks;
  startup validation is repeated after every process restart.
- Submission is not confirmation; canonical indexing remains authoritative.
- Outgoing nonce/envelope coordination is process-local and requires exactly one active writer for
  each EOA. After an unclean restart, operators must reconcile outstanding submissions before the
  EOA can be used safely; there is no durable outbox in this phase.
- A manually prepared standalone transaction that is abandoned before submission intentionally
  blocks later local nonces for that sender; callers should prepare only when they intend to
  submit.
- Ethereum submission uses the first configured RPC endpoint for one visible attempt. Endpoint 0
  must therefore be submission-ready; endpoint failover is not performed for that call.
- The shipped application routes batches through `Wallets::send_all`, which proves that every
  wallet belongs to the sender's registered asset family. Direct SDK integrations must preserve
  that boundary instead of pairing a `Transfer` with an unrelated provider's sender.
- No token-contract-wide indexing is introduced. The indexer continues to retain complete facts
  for watched addresses, while each wallet presents only its selected asset.

## Verification Gates

```bash
cargo test --locked -p chain-ethereum
cargo test --locked -p payment-api
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked --workspace --no-deps
cargo run --locked -p design-lint -- --policy lint.toml check .
git diff --check
```

The local deterministic API tests require permission to bind loopback RPC listeners. Report
sandbox-denied listener failures separately from source failures.

Fresh Phase 6 handoff results:

- `cargo fmt --all -- --check`: pass;
- `cargo check --locked --workspace --all-targets`: pass;
- `cargo test --locked --workspace --exclude payment-api`: pass;
- `payment-api` with the isolated reorg case skipped: 21 pass;
- full `cargo test --locked --workspace`: one failure in that isolated pre-existing indexing race;
- focused strict Clippy for `json-rpc`, `chain-ethereum`, and `payment-api --no-deps`: pass;
- full workspace strict Clippy: pre-existing failures in unchanged HTTP/design-lint files;
- `cargo doc --locked --workspace --no-deps`: pass;
- design-lint tests, case generation, and policy check: pass with zero cases/findings;
- `git diff --check`: pass.

The reorg gate failure is not in the Phase 6 transaction path. The indexing runtime can capture a
stale address-filter snapshot while a runtime wallet is being generated, then advance the Ethereum
checkpoint with no new address before the generation request returns. Correctly closing that race
requires a wallet-registration/synchronization admission boundary and is a separate indexing
lifecycle change; adding a sleep to the test would only hide it.
