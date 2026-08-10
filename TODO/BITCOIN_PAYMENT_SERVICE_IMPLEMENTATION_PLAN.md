# Bitcoin Payment Service block-only v1

Status: Approved and implemented in source on 2026-08-10. The remaining
acceptance item is the composed disposable Bitcoin Core 31 regtest scenario.
Source, deterministic tests, and the offline demo are not production or live-
node evidence, and no funded-network broadcast is claimed.

| Workstream | Status |
|---|---|
| Canonical Bitcoin PS v1 decisions | Approved and documented |
| PS domain and RocksDB schema changes | Implemented with multi-source repository coverage |
| Bitcoin `payment-api` composition, policy, IX/WS clients, and workers | Implemented under `payment-api bitcoin` |
| `payment_sdk_demo` Bitcoin Payment Service sample | Implemented as a direct offline, non-broadcast sample |
| Deterministic unit, repository, HTTP, and workflow validation | Implemented; final command results belong in the checkout handoff |
| Disposable Bitcoin Core 31 regtest acceptance | Pending |

The existing Bitcoin Wallet and Indexer implementation and its remaining
real-node acceptance boundary are recorded in
[`BITCOIN_WALLET_INDEXER_IMPLEMENTATION_PLAN.md`](./BITCOIN_WALLET_INDEXER_IMPLEMENTATION_PLAN.md).
The canonical system requirements remain
[`docs/SYSTEM_REQUIREMENTS.md`](../docs/SYSTEM_REQUIREMENTS.md).

## Locked v1 scope

- One Bitcoin network, one IX feed, one active policy, and one exclusive PS
  RocksDB path per `payment-api` process. A database never mixes networks or
  Ethereum and Bitcoin records.
- Native BTC only. Amounts are integer satoshis and fee rates are integer
  satoshis per kvB.
- Deposit addresses and owned collection inputs are native SegWit v0 P2WPKH or
  Taproot key-path P2TR. The deployment policy selects the deposit address kind.
  Imported/watch-only addresses, additional scripts, multisig, and hardware
  transaction protocols are excluded.
- Bitcoin IX remains block-only. PS consumes `Included`, depth-based
  `Confirmed`, and `Reorged` facts. It does not claim mempool, dropped,
  conflict, or replacement detection.
- Deposit creation, accounting, collection, retry, and reconciliation remain
  explicit authenticated commands. There is no automatic user credit or
  automatic collection decision.
- Collection uses finalized raw consensus transactions. PSBT, PS-generated RBF
  replacement, CPFP, fee bumping, and signing a conflicting replacement are
  excluded.
- A deposit participates in at most one Bitcoin collection aggregate in v1.
  Later payments remain watched and accounted, but retained per-deposit
  collection ownership prevents a second collection until a future multi-
  reservation/archival design is approved.

## Policy and API decisions

The Bitcoin policy is versioned, hashed, and mandatory. It has no permissive
financial defaults. It supplies at least:

- canonical network and native-BTC asset identity;
- deposit address kind and TTL;
- master destination;
- minimum per-deposit collection amount;
- minimum spend confirmations;
- requested and maximum fee rate in satoshis per kvB;
- maximum absolute transaction fee in satoshis;
- maximum deposits and maximum inputs per batch; and
- the existing typed post-credit reorg reconciliation behavior.

Create-deposit, close, accounting, reconciliation, collection, and retry
commands retain the existing authenticated `/v1` job/idempotency model. A
Bitcoin batch collection command explicitly supplies multiple deposit IDs and
returns one durable job and collection ID.

One batch may include deposits belonging to different users only when every
durable user record belongs to the same authenticated exchange principal.
Authorization checks every source before a job or reservation is created. A
batch must never mix exchange principals, Bitcoin networks, assets, policies,
or master destinations. Canonical request hashing sorts the deposit IDs and the
API rejects duplicates, so an idempotent replay cannot change batch membership.

## Deposit and observation workflow

- Reuse the existing PS-owned users, jobs, command idempotency, deposit
  lifecycle, append-only event mirror, absolute ledger, and typed
  reconciliation contracts.
- Capture the birthday from the configured Bitcoin IX Ready checkpoint.
- Ask Bitcoin WS to provision the policy-selected P2WPKH or P2TR address.
- Atomically persist `AwaitingWatch` plus the opening zero-balance ledger row,
  register an idempotent IX address watch, mark the deposit `Active`, and only
  then expose the address.
- Mirror the Bitcoin IX cursor feed before projection. Classify independent
  input and output movements; never collapse a UTXO transaction into one
  sender/recipient transfer.
- `Included` may change `received` and current `balance`. Only `Confirmed` may
  change `confirmed` or `collected`. Only an explicit accounting command may
  change `accounted`.
- A post-credit reorg appends corrected absolute rows, preserves `accounted`,
  opens the existing typed blocking reconciliation case, and requires explicit
  resolution.

## Exact-outpoint reservation model

- `sdk/deposits` remains chain-neutral. It stores caller-owned opaque spend
  resource IDs, source deposit association, amounts, bounded non-debug exact
  selection evidence, and reservation state without importing Bitcoin types.
  Bitcoin parsing and canonical outpoint construction remain in
  `sdk/chains/bitcoin` and the Bitcoin application adapter.
- One UTXO batch aggregate contains N source deposits, N source reservations,
  one ordered sweep leg, and N allocation records. Existing Ethereum
  single-source collection remains representable.
- PS queries all selected addresses from one generation/revision/checkpoint-
  fenced IX projection. An eligible output is canonical and unspent, satisfies
  the policy confirmation threshold and coinbase maturity, belongs to the
  durable deposit address/key, and is not already reserved.
- For every explicitly selected deposit, PS chooses its complete eligible UTXO
  set in canonical `(txid, vout)` order. It does not perform privacy grouping,
  partial selection, change selection, or address reuse.
- The batch is a full drain to exactly one policy master output. It creates no
  change output. Every selected deposit must contribute at least one eligible
  output and meet the minimum amount after policy checks.
- Collection creation atomically writes all exact resource reservations and a
  uniqueness index. A concurrent job that overlaps even one outpoint loses the
  conditional write and must reload/reselect; partial reservation is forbidden.
- UTXO-batch v1 has no generic `fail_leg` or reservation-release path. A
  required unsigned reservation remains active and may be retried; cancellation
  or release requires a future explicit safe design. Once signed bytes are
  durably recorded, the reservation has no time-based expiry and is never
  released merely because an RPC request failed, a receipt was not found, or
  time elapsed.

## Shared fee allocation

The actual signed transaction fee is allocated proportionally to each source's
gross input using integer largest remainder:

1. Let `F` be the checked total transaction fee, `G_i` one deposit's gross
   selected input, and `G` the checked sum of all gross inputs.
2. Assign `floor(F * G_i / G)` satoshis to each deposit.
3. Rank the fractional remainders `F * G_i mod G` from largest to smallest and
   assign the remaining satoshis one at a time.
4. Break equal-remainder ties by canonical deposit ID ascending.

Each allocation records `gross_debit = G_i`, `allocated_fee`, and
`master_credit = G_i - allocated_fee`. The implementation must prove that fee
shares sum to the exact transaction fee, master credits sum to the one master
output, and no subtraction or multiplication overflows. A batch that cannot
leave valid positive attribution within the configured minimum/fee ceilings is
rejected before persistence or broadcast.

## Signing, persistence, and broadcast

1. PS sends the exact reserved txid/index/value/script/address/key selection,
   the one master destination, and the policy fee rate to Bitcoin WS.
2. WS revalidates every selected output against one current fenced IX snapshot,
   verifies the checkpoint against Core, builds a no-change drain transaction,
   signs P2WPKH/P2TR inputs, and returns the expected txid, exact raw bytes,
   selected inputs, master output, fee, vsize, and gross source attribution.
3. PS independently decodes and validates the returned Bitcoin transaction:
   expected txid, exact input set, policy master script/output, input/output
   sums, gross attribution, vsize, requested/maximum fee rate, maximum absolute
   fee, and dust/value bounds.
4. In one durable write PS records the exact bytes, expected txid, selected
   resources, one signed leg, N allocations, and transaction index before any
   broadcast side effect.
5. Broadcast always submits the same persisted bytes and expected txid. A
   response loss first triggers receipt lookup and then only an identical-byte
   retry. PS never re-signs the leg as a new transaction.
6. Once durably signed, PS v1 retains the exact bytes, reservations,
   allocations, and transaction watch indefinitely across submission,
   inclusion, confirmation, and reorg. IX's separately configured rollback
   retention is not permission to delete the envelope or release ownership. No
   archival/cleanup transition is implemented in v1.
7. A reorg does not authorize replacement signing or outpoint reuse. PS keeps
   the same txid/bytes and watch, may rebroadcast those exact bytes, and accepts
   later re-inclusion of that same transaction. A canonical conflicting spend
   becomes a blocking manual reconciliation case in block-only v1.

Core `testmempoolaccept` is a node-local policy preflight, not consensus proof
or confirmation. RPC acceptance is submission only; IX confirmation remains
the accounting-grade completion source.

## Atomic projection and recovery

- Mirroring one IX event and its ingestion cursor remains one atomic PS write.
- A collection event affecting N deposits must apply the leg/reservation
  transition, all N ledger rows, all N deposit-observation index rows, any
  reconciliation cases, and the projection cursor in one physical PS batch.
  One batch transaction must never appear confirmed for only a subset of its
  sources.
- Crash recovery resumes from durable job, reservation, signed bytes,
  transaction index, watch ID, ingestion cursor, and projection cursor state.
- Because Bitcoin IX v1 has no mempool/drop/replacement feed, a signed or
  broadcast transaction may remain unresolved indefinitely. Timeout, RPC
  outage, or absence from one receipt lookup is never proof that its outpoints
  are safe to reuse.

## Implemented source workstreams

1. Extended `sdk/deposits` and its migration to support multi-source jobs,
   exact-resource reservations, N prepared allocations, retained envelopes,
   and the atomic collection-projection command.
2. Added Bitcoin policy parsing, canonical validation, and fail-closed PS database
   binding without weakening the existing Ethereum policy/runtime.
3. Added authenticated Bitcoin IX clients for Ready/status, watches, replay, and
   fenced UTXO pages.
4. Added authenticated Bitcoin WS clients for address generation, exact batch
   signing, exact-byte broadcast, and receipts.
5. Added the Bitcoin collection executor, cross-user same-principal authorization,
   deterministic selection, fee allocation, signed-transaction inspection,
   watch reconciliation, same-byte rebroadcast, and reorg/re-inclusion handling.
6. Added Bitcoin PS CLI/runtime composition, health/readiness, backup/migration,
   API DTOs, operational documentation, and the `payment_sdk_demo` sample.

## Test and acceptance plan

- Unit-test canonical ordering, duplicate deposits/outpoints, full-drain/no-
  change enforcement, P2WPKH/P2TR selection, coinbase maturity, integer fee
  allocation and tie-breaking, overflow, dust, minimums, and both fee ceilings.
- Real-RocksDB tests must prove all-or-nothing cross-user reservations,
  outpoint collision handling, multi-source migrations, retained-envelope
  reopen, and one-commit batch confirmation/reorg projection.
- HTTP/client tests must cover strict DTOs, same-principal authorization,
  cross-principal rejection, network/address mismatch, authentication,
  idempotency replay/conflict, bounded pages/bodies, and redaction.
- Failure-window tests must cover crash/loss before and after reservation,
  signing response, signed-byte persistence, Core submission, broadcast
  response, IX watch attachment, confirmation, reorg, same-byte rebroadcast,
  and same-txid re-inclusion.
- Restart tests must prove that no key, watch, ledger row, broadcast,
  reservation, allocation, or accounting command is duplicated.
- Availability-boundary tests must prove that a deposit already owned by one
  retained UTXO batch cannot join another collection while a later incoming
  payment still appends the correct watched/accounting facts.
- Run the locked targeted and full workspace format, check, test, strict Clippy,
  documentation, and diff validation matrix.
- Finally run an opt-in disposable Bitcoin Core 31 regtest scenario with only
  disposable regtest coins. It must exercise multiple users owned by one
  exchange principal, P2WPKH/P2TR deposits, one batch drain, confirmation,
  restart/replay, controlled reorg, retained exact-byte rebroadcast, UTXO
  restoration, and same-txid re-inclusion. Until sanitized evidence is recorded,
  real-node acceptance remains pending.

## Exclusions

- No mainnet/testnet/signet funded transaction is part of ordinary validation.
- No mempool deposit accounting, dropped/replacement/conflict detection,
  PS-generated RBF replacement, CPFP, fee bumping, PSBT, multisig, imported
  descriptors, or native hardware-wallet transaction protocol.
- No automatic credit, automatic collection decision, webhook, message broker,
  HA writer lease, mixed-network database, or production custody claim.
- No archival or space-reclamation transition for signed UTXO batches; v1
  retains their exact bytes and outpoint ownership indefinitely.
- No generic UTXO-batch failure/cancellation or unsigned-reservation release
  transition; required reservations remain active and retryable.
- No repeated late-payment collection for a deposit that already belongs to a
  Bitcoin collection aggregate. Multi-collection-per-deposit support requires a
  future ownership, reservation, and archival design.
