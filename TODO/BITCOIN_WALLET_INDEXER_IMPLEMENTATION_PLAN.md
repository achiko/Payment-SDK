# Bitcoin Wallet and Indexer block-only v1

Status: Approved on 2026-08-09. Source implementation and the complete locked
workspace validation matrix passed on 2026-08-10. Deterministic validation is
not operational evidence; the pinned Bitcoin Core 31 regtest scenario remains
pending.

| Workstream | Status |
|---|---|
| Bitcoin chain, IX/WS runtime, API, projection, and documentation source | Implemented in the working tree |
| Deterministic unit/integration coverage | Passed: full workspace check, tests, strict Clippy, docs, formatting, and diff validation |
| Disposable Bitcoin Core 31 regtest acceptance | Pending: Core 31 binary unavailable in the current environment |
| Bitcoin PS reservation/accounting/orchestration | Implemented separate workstream; Core 31 operational acceptance remains pending and is tracked in [`BITCOIN_PAYMENT_SERVICE_IMPLEMENTATION_PLAN.md`](./BITCOIN_PAYMENT_SERVICE_IMPLEMENTATION_PLAN.md) |

The operational/configuration contract is documented in
[`docs/BITCOIN_SERVICES.md`](../docs/BITCOIN_SERVICES.md). The checked-in,
still-unexecuted real-node procedure is
[`docs/manual-bitcoin-regtest/README.md`](../docs/manual-bitcoin-regtest/README.md).

## Summary

- Reuse the existing Bitcoin address, P2WPKH/P2TR transaction construction,
  signing, fee/dust checks, and broadcast contracts.
- Complete the production Bitcoin Core adapter, signed-transaction boundary,
  authenticated Wallet Service HTTP API, configuration, and runtime.
- Reuse the generic Indexer checkpoint, watch, confirmation, revision, replay,
  reorg, rebuild, and RocksDB machinery.
- Implement the missing Bitcoin block source, interpreter, codec, canonical
  UTXO projection, HTTP API, and runtime.
- IX materializes canonical UTXOs, PS atomically reserves exact outpoints, and
  stateless WS validates and signs supplied selections.

## Locked v1 decisions

- One chain and network per WS/IX process, using chain-specific CLI modes.
- Direct authenticated Bitcoin Core 31 RPC; the node must be unpruned, report
  local block/header synchronization, be on the configured network/genesis, and
  have a synchronized transaction index. Production operators independently
  monitor peer connectivity, tip freshness/chainwork, and upstream diversity.
  Both IX and WS require one protected Core Authorization header; embedded URL
  credentials are rejected.
- P2WPKH and P2TR key-path wallet addresses only.
- Block-only IX lifecycle: included, confirmed, reorg revisions, and replay.
  Mempool, dropped, replacement, and RBF lifecycle tracking are deferred.
- Raw finalized Bitcoin transactions cross the WS boundary; PSBT is deferred.
- Confirmation depth and reorg retention are mandatory deployment inputs with
  no production defaults.
- IX owns canonical UTXOs. PS owns reservations and workflow state. WS stores
  neither.

## Bitcoin chain and Wallet Service

- Separate node operations from spendable-output sourcing. Core supplies
  identity, readiness, fee estimates, preflight, broadcast, and receipts; the
  authenticated IX client supplies canonical balances and UTXOs.
- Validate Core version, network, genesis hash, pruning, initial block download,
  and transaction-index readiness before serving traffic.
- Represent fee rates as integer satoshis per kvB and compute fees with checked
  ceiling arithmetic over virtual bytes. Never parse BTC amounts through
  floating point.
- Add canonical txid parsing/display and verify every claimed txid against the
  decoded exact consensus bytes. Signed bytes must not appear in `Debug` or
  logs.
- Expose non-broadcasting transfer and batch-collection signing. Requests carry
  exact selected outpoints, values, scripts, and key locators. Responses carry
  txid, exact bytes, inputs, outputs, fee, vsize, and gross attribution.
- Bind all IX UTXO pages and addresses used by one WS read to one projection
  generation/revision/checkpoint snapshot; verify that checkpoint against WS
  Core and fail retryably across any canonical movement.
- Broadcast is separate: recompute the txid, run Core policy preflight, submit
  unchanged bytes, and verify the returned txid.
- Custody readiness requires ECDSA/DER for P2WPKH and Schnorr/raw plus the
  BIP341 secp256k1 public tweak for P2TR.

Wallet routes:

- `POST /v1/bitcoin/addresses`
- `POST /v1/bitcoin/balances`
- `POST /v1/bitcoin/transfers/sign`
- `POST /v1/bitcoin/collections/requirements`
- `POST /v1/bitcoin/collections/sign`
- `POST /v1/bitcoin/transactions/broadcast`
- `POST /v1/bitcoin/receipts`

## Bitcoin Indexer Service

- Implement the Core verbosity-2 block source with exact raw transaction bytes;
  resolve each external transaction once with bounded concurrency, validate its
  exact consensus bytes, and retain only size-bounded value/address evidence.
- Emit stable `txid:vin:index` input movements and `txid:vout:index` output
  movements. Never fabricate a single from-to movement for a UTXO transaction.
- Compute fees from all previous-output values minus outputs. Coinbase has no
  fee; fee payer is absent when attribution is ambiguous.
- Support network-matched P2WPKH/P2TR address watches and transaction watches.
- Commit opaque chain projection mutations atomically with raw block data,
  observations, undo, event rows, and checkpoint movement. Bitcoin retains
  creation values and records spends as markers, so undo removes orphaned
  creations/markers without reconstructing values; Ethereum uses an empty
  projection.
- Create watched canonical outputs and consume watched previous outputs.
  Reorg rollback removes orphaned creations and restores orphaned spends.
- Mirror the Ethereum IX API under `/v1/scopes/bitcoin/{network}/...` and add
  `GET /v1/scopes/bitcoin/{network}/addresses/{address}/utxos` with cursor
  pagination and an explicit generation/revision/checkpoint snapshot.

Service commands:

- `wallet-worker bitcoin serve`
- `indexer-worker bitcoin serve|backup|migrate|rebuild|rebuild-abort|cleanup`

## Validation and acceptance

- Unit-test txid byte order, exact amount/rate parsing, fee rounding, both spend
  types, tweak capability, key/script mismatch, duplicate inputs, dust, zero,
  overflow, coinbase maturity, signed-byte redaction, and preflight rejection.
- Test Core readiness failures, interpreter input/output semantics, same-block
  spends, missing prevouts, atomic UTXO connect/spend, reorg restoration,
  backfill, restart, replay, rebuild, cleanup, and rollback beyond retention.
- Test strict authenticated HTTP DTOs and retain Ethereum regression coverage.
- Run an opt-in disposable Core 31 regtest scenario covering P2WPKH/P2TR,
  included-to-confirmed transitions, sign-before-broadcast, batch collection,
  restart/replay, controlled reorg, UTXO restoration, and re-inclusion, using
  the checked-in manual procedure. Its existence does not satisfy acceptance;
  record actual sanitized command evidence before marking this item complete.
- Run focused package validation followed by the complete locked workspace
  check, tests, Clippy, docs, formatting check, and `git diff --check`.

## Deferred work

- Bitcoin PS classification, ledger accounting, exact-outpoint reservations,
  fee allocation, collection orchestration, and transaction-watch registration
  remain outside the ownership of this completed WS/IX plan, but are now
  implemented in the separate workstream tracked in
  [`BITCOIN_PAYMENT_SERVICE_IMPLEMENTATION_PLAN.md`](./BITCOIN_PAYMENT_SERVICE_IMPLEMENTATION_PLAN.md).
- Mempool indexing, RBF/replacement/drop detection, CPFP, fee bumping, PSBT,
  multisig, hardware-wallet interaction, imported descriptors, HA, and
  multi-network processes are out of scope.
- No funded-network transaction is part of implementation or validation.
