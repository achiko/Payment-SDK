# Open requirements

The canonical consolidated specification, including settled requirements and
all Mermaid flows, is [`SYSTEM_REQUIREMENTS.md`](./SYSTEM_REQUIREMENTS.md). This
file remains the compact unresolved-decision checklist.

This checklist keeps unresolved decisions visible before their implementation.
They should become architecture decisions and tests, not assumptions hidden in
code.

## Wallet and address lifecycle

Bitcoin PS block-only v1 resolves its address subset: the mandatory policy
selects P2WPKH or P2TR key-path for newly generated deposits; imported/watch-
only addresses, additional scripts, and automated orphan-key retirement are
excluded. See
[`BITCOIN_PAYMENT_SERVICE_IMPLEMENTATION_PLAN.md`](../TODO/BITCOIN_PAYMENT_SERVICE_IMPLEMENTATION_PLAN.md).
The following questions remain general or post-v1 decisions.

- Is a wallet one root key, one chain account, one customer, or an accounting
  container containing multiple key sources?
- Can wallets be watch-only using xpubs/public keys, or must every wallet have
  an attached signer?
- Which derivation standards and address types are supported per chain?
- Is the wallet birthday a block height, timestamp, block hash, or a combination?
- Are imported addresses allowed, and how is their historical start chosen?
- How are orphaned keys/addresses retired if address generation succeeds but PS
  persistence permanently fails?

## Index scope and completeness

Ethereum IX v1 resolves this checklist for its own scope: one process per
RocksDB path, all canonical blocks filtered locally, no mempool, no traces or
internal-transfer completeness, depth 12, 50 reversible bundles plus an anchor,
and ERC-20 `Transfer` logs only. See
[`INDEXER_SERVICE.md`](./INDEXER_SERVICE.md). The questions remain open for
other chains and future versions.

Bitcoin IX v1 separately selects one Bitcoin Core 31 network per process,
unpruned canonical block reads, a synchronized transaction index, block-only
observations, P2WPKH/P2TR address watches, and an IX-owned canonical UTXO
projection. Confirmation depth and reversible-bundle retention are mandatory
deployment inputs rather than universal defaults. Mempool conflict and
replacement tracking remain future-version questions.

- How are multiple IX workers leased so only one advances a chain/network scope?
- Are all blocks downloaded and filtered locally, or may chain-specific sources
  perform address/script filtering?
- Are mempool deposits required, or are confirmed blocks sufficient?
- What confirmation/finality thresholds apply to deposits and withdrawals?
- How deep a reorg must be recoverable without a full rebuild?
- How long is undo data retained?
- For Ethereum, are internal native transfers required? If yes, which trace API
  and node retention guarantees are mandatory?
- Which token standards are in scope, and are fee-on-transfer/rebasing tokens
  represented by events, balance snapshots, or both?

## Transaction construction

Bitcoin WS v1 selects native SegWit v0 P2WPKH and Taproot key-path P2TR,
finalized raw consensus transactions rather than PSBT, integer satoshis per kvB,
and exact PS-selected inputs. IX supplies canonical UTXO facts, PS atomically
owns reservations, and stateless WS validates each outpoint/value/script/key
association before signing.

Bitcoin PS block-only v1 further selects an explicit same-principal batch,
complete eligible-UTXO selection in canonical `(txid, vout)` order, one
full-drain master output, and no change. Exact outpoints are atomically reserved;
UTXO-batch v1 has no generic failure or reservation-release path, so an unsigned
required reservation remains active/retryable until a future explicit safe
cancellation design exists. Signed reservations have no time-based expiry.
Requested/maximum sat/kvB, maximum absolute fee, minimum amount/confirmations,
and batch limits are mandatory policy values. Recovery retains and rebroadcasts
only the same bytes and accepts same-txid re-inclusion. PS-generated RBF
replacement, CPFP, fee bumping, PSBT, and conflicting replacement signing are
excluded. The following questions are post-v1. Retained per-deposit ownership
also limits v1 to one Bitcoin collection aggregate per deposit. Later payments
remain watched/accounted but cannot form another collection; multi-collection-
per-deposit ownership and archival remain post-v1 decisions.

- Which additional Bitcoin script types should follow P2WPKH and P2TR:
  legacy, nested SegWit, script-path Taproot, or multisig?
- Which future hardware/multisignature workflows require PSBT, and which PSBT
  version and proprietary-field preservation rules apply?
- Which privacy grouping, partial-selection, change, or address-reuse policies
  should follow the full-drain v1 rule?
- For Ethereum, which envelope types are required and how are pending nonces
  reserved across concurrent withdrawals?
- Which future mempool/RBF, CPFP, replacement, cancellation, or fee-bump
  workflows are required?

## Hardware signing boundary

The generic signer can sign messages/digests, but native Trezor Bitcoin signing
requires complete transaction interaction. Choose one explicitly:

1. support only raw cryptographic operations through the generic signer;
2. add a higher integration package depending on both Bitcoin and Trezor;
3. define a protocol-neutral interactive signing session capable of requesting
   structured external data without importing chain types;
4. treat hardware transaction signing as a separate application workflow.

Do not resolve this by making Bitcoin depend on `signer-trezor` or by teaching
the base signer contract about Bitcoin transactions.

## Signing and custody

- Can one transaction require multiple signers or partial signatures?
- Does a `Signer` instance own one key or a collection addressed by `KeyLocator`?
- Which curves, signature encodings, and recoverability formats are required?
- Must a signer attest/display the address or transaction intent?
- What timeout, retry, cancellation, and user-rejection behavior is required?
- How are signer capabilities and availability cached safely?

## Storage

Ethereum IX/PS v1 selects separate RocksDB databases, a serialized conditional
writer, synchronous atomic batches, explicit record versions/migrations, and a
staged IX rebuild. Semantic repositories remain the public persistence
boundary.

Bitcoin PS v1 selects one exclusive RocksDB database per network and policy,
chain-neutral opaque spend-resource IDs in `sdk/deposits`, atomic uniqueness for
the complete exact-outpoint set, retained non-debug exact selection/envelope
evidence, and one physical projection batch for the leg/reservations plus all
affected ledgers and the cursor. The implementation and migration logic are
present in source with deterministic real-store coverage. This does not prove
the pending composed Core 31 regtest scenario or production operation. V1 does
not archive signed aggregates or release their per-deposit ownership for a
second collection.

- Is the proposed atomic key/value contract sufficient, or should semantic
  repositories own all persistence contracts?
- What consistency and isolation guarantees are required?
- Must block commit, balance materialization, transaction history, watch state,
  and outbox notifications share one atomic commit?
- How are schema versions and migrations represented without naming a backend?
- What idempotency keys prevent duplicate address creation, withdrawals, and
  notifications?

## Deposit accounting

Ethereum v1 handles a post-credit reorg by preserving `accounted`, opening a
durable manual reconciliation case, correcting canonical fields in later
absolute ledger rows, and blocking automated credit/collection until explicit
resolution.

Bitcoin PS v1 uses IX `Confirmed` as the confirmation-qualified PS fact while
keeping user credit explicit. It reuses the typed blocking post-credit reorg
case. Shared batch fees use proportional integer largest-remainder allocation
by gross input, with canonical deposit-ID tie-breaking. Exact-outpoint
uniqueness prevents concurrent reuse, and a confirmed/reorged batch updates all
source deposits consistently in one physical projection commit.

- Which assets permit a reliable event-derived balance, and which require a
  periodic chain balance reconciliation?
- What exact dust, fee, unsolicited-spend, fee-on-transfer, and rebasing drift
  is permitted by the reconciliation relation?
- Which post-v1 asset or business policies require a stronger gate than IX
  `Confirmed`?

## Application API

Ethereum v1 keeps reconciliation/delivery loops inside the existing apps and
uses authenticated versioned HTTP for IX commands/queries plus cursor replay.
Transport remains at-least-once; repository idempotency makes effects stable.

Bitcoin PS v1 makes the same application-topology selection for a separate
Bitcoin mode. An explicit multi-deposit batch is one durable job; it may cross
user IDs only when every durable user belongs to the authenticated exchange
principal. The database, IX feed, WS, network, asset, policy, and master
destination remain single-scope. The implemented mode is selected with
`payment-api bitcoin`; its live Core 31 operational acceptance remains pending.

- Are reconciliation and event-delivery workers separate executables or loops
  inside `apps/api` and `apps/indexer`?
- Is transaction building synchronous from already indexed state, or may it
  query RPC during the request?
- Which operations are commands returning jobs versus immediate results?
- What is the external wallet/address/transaction status model?
- Which events must be delivered exactly once versus at least once?
