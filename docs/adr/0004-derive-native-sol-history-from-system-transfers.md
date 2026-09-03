# ADR-0004: Derive native SOL history from System Program transfers

## Status

Accepted

## Date

2026-08-27

## Context

The chain-neutral indexing model already stores a scoped transaction identity,
included or failed status, an ordered list of stable value movements, and an
optional exact network fee. Bitcoin and Ethereum translate their native block
data into that model without exposing native transactions to generic indexing
or persistence.

Native Solana blocks contain legacy and versioned transactions. A version-0
message may refer to accounts loaded from address lookup tables, and programs
may invoke the System Program through inner instructions. Transaction metadata
separately reports execution failure, the actual fee, resolved lookup-table
addresses, and account balances.

Balance differences alone do not identify a transfer. They include network
fees and can include account creation, nonce withdrawal, rewards, rent, or
program-owned lamport changes. Inventing sender/recipient pairs from those
differences would make payment history look precise when it is not.

Initial Solana scope is native SOL payment history, not an account-activity
ledger. It must preserve every supported payment movement in a relevant
transaction, retain actual fees, reject incomplete meaningful data, and avoid
adding Solana concepts to the generic history model.

## Decision

Reuse the existing indexing path without adding a Solana transaction, movement,
status, participant, or output-projection type:

```text
finalized getBlock response
    -> resolve native transaction accounts and metadata
    -> derive transaction identity, status, and actual fee
    -> decode successful top-level and inner System transfers
    -> validate watched-address lamport deltas
    -> select financially relevant transactions
    -> ObservationDraft -> canonical history
```

### Block RPC contract

The Solana source will request a block with these semantics:

```text
commitment: finalized
encoding: json
transactionDetails: full
maxSupportedTransactionVersion: 0
rewards: false
```

Raw JSON supplies compiled instructions and, for version-0 transactions, the
resolved loaded addresses in metadata. The Solana crate will decode native
instruction data with a pinned maintained Anza dependency rather than trusting
provider-generated `jsonParsed` labels.

Only legacy and version-0 transactions are admitted initially. A transaction
with a higher version is an unsupported source result for the entire block;
the source must not return an empty or partial block and the checkpoint must not
advance. Solana v1 is not active on a public cluster at this decision's date,
but its activation is a release gate: support and tests must be expanded before
raising `maxSupportedTransactionVersion`.

### Transaction identity and resolved accounts

For each transaction, the interpreter will:

1. require a non-empty signature list whose length matches the message's
   required signer count;
2. validate and canonicalize every signature and address through native types;
3. use the canonical Base58 first signature as `TransactionRef.value`; and
4. use the first static account, which must be a signer, as the fee payer.

For a legacy transaction, the runtime account-key vector is its static message
keys. For version 0, it is built in this exact order:

```text
static message keys
    + loaded writable addresses
    + loaded read-only addresses
```

Every top-level and inner program/account index resolves against that complete
vector. The pre-balance and post-balance arrays must have the same length as
each other and as the resolved vector. Missing transaction metadata, missing
version-0 loaded addresses, malformed signatures or addresses, duplicate inner
instruction groups, invalid indices, or mismatched balance vectors invalidate
the source block before persistence.

### Status and exact fee

`meta.err == null` maps to `ObservationDraftStatus::Included`. A non-null error
maps to `ObservationDraftStatus::Failed`; its optional reason comes only from a
deterministic chain-owned error representation, never provider prose or logs.

`meta.fee` is the authoritative total network fee. It becomes:

```rust,ignore
NetworkFee {
    asset: AssetId { chain: "solana", asset: "native" },
    amount: exact_scale_zero_lamports,
    payer: Some(first_static_account),
}
```

The fee is retained for both successful and failed transactions. It includes
any priority fee paid by an inbound transaction even though SDK-created sends
initially reject priority-fee configuration. Fees never become transfer
movements and are never reconstructed from balance differences.

A failed transaction emits no movements because its instruction state changes
did not commit. The charged fee remains its only native value effect. Signature
presence or inclusion in a finalized slot never overrides `meta.err`.

### Supported native movements

Successful transactions will emit `ValueMovement::Transfer` only for these
explicit System Program variants:

| Variant | Source | Destination | Amount |
|---|---|---|---|
| `Transfer` | instruction account 0 | instruction account 1 | encoded lamports |
| `TransferWithSeed` | instruction account 0 | instruction account 2 | encoded lamports |

Account 1 of `TransferWithSeed` is its base authority, not the destination.
The interpreter will recognize the exact System Program ID and decode the
maintained `SystemInstruction` enum. It will not use hand-written discriminants
or decode lookalike bytes owned by another program.

Both top-level instructions and recorded inner instructions are inspected.
Top-level instruction order and the listed inner-instruction order determine
movement order. Every non-zero transfer remains separate; repeated or
self-directed transfers are not netted or aggregated. A zero-lamport transfer
creates no movement.

Movement identities are deterministic execution paths:

```text
<signature>:ix:<outer-index>
<signature>:ix:<outer-index>:inner:<inner-ordinal>
```

Once one active address makes a transaction relevant, every supported movement
in that transaction is retained, including movements between other addresses.
Solana emits `OutputChanges::default()` because it has no UTXO projection.

`CreateAccount`, `CreateAccountWithSeed`, `CreateAccountAllowPrefund`,
`WithdrawNonceAccount`, rewards, rent effects, SPL instructions, and direct
program-owned lamport changes are outside the initial movement allowlist. They
must not be mislabeled as ordinary payment transfers.

### Completeness shield

Pre- and post-balances are integrity evidence, not movement sources. For every
active address that could have a native value effect in a successful
transaction, the interpreter will compare its observed checked lamport delta
with:

```text
incoming supported transfers
    - outgoing supported transfers
    - actual fee when the address is the fee payer
```

An unexplained difference is a typed unsupported-native-movement error and the
checkpoint does not advance. The interpreter does not guess a counterparty.
This makes the initial completeness boundary explicit when a selected wallet
is affected by a System account-creation or nonce operation, a program-direct
lamport change, a reward, or another unsupported mechanism.

For a successful transaction that could affect an active address,
`innerInstructions: []` means no recorded inner invocation. A null or omitted
inner-instruction field is incomplete and prevents advancement because a
supported inner transfer could be hidden. Failed transactions do not require
inner movement interpretation because their movements are empty. A transaction
that only injects an active address as a read-only, non-paying account is not
made relevant and cannot create history by that mention alone.

### Relevance and presentation

A transaction is financially relevant when an active address is:

- the fee payer; or
- the source or destination of a successful supported movement.

Signer-only, read-only, program-account, or general account-list appearance is
not payment history. A successful fee-only transaction remains visible to its
active fee payer because the payer's SOL balance changed.

A failed transaction is queryable only for an active fee payer. Failed
attempted transfer endpoints are not persisted: the generic model correctly
forbids failed movements and has no separate participant collection. Adding
attempted participants would be a separate product and persistence decision;
S1.5 does not create fake movements to obtain that behavior.

The internal asset remains `{ chain: "solana", asset: "native" }`. The Solana
wallet resolves it to SOL metadata with nine decimals while all indexed
amounts remain exact scale-zero lamports. The existing transfer, fee, status,
and transaction API shapes require no Solana-specific variant.

Finalized admission remains source policy. Generic history continues to derive
`Included` or depth-based `Confirmed` from produced-block height; this decision
does not introduce a misleading Solana-finality status. Transaction pages keep
their existing deterministic height-then-transaction-ID ordering. Only the
movement vector promises native execution order.

## Consequences

### Positive

- Native SOL history reuses the existing chain-neutral transaction, movement,
  fee, persistence, wallet, and HTTP pipeline.
- The first signature connects submitted transaction IDs to indexed history.
- Version-0 transfers through loaded addresses and CPI inner transfers are not
  lost.
- Fees remain exact and distinct from value movement on success and failure.
- Balance evidence detects unsupported watched-wallet effects without
  fabricating counterparties.
- SPL Token and general Solana account activity do not leak into native SOL
  payment semantics.

### Negative

- A selected wallet affected by an unsupported lamport mechanism stops that
  scope until support is deliberately expanded or the scope is recreated.
- Historical blocks whose successful relevant transactions lack recorded inner
  instructions cannot be claimed complete and will block historical catch-up.
- Failed attempted endpoints are not queryable unless one is also the fee
  payer.
- Version-1 activation will block readiness until explicitly supported.
- Native block transaction order is not exposed by the current history cursor;
  pages remain deterministically ordered by height and transaction ID.

### Neutral

- Inbound priority fees are recorded through `meta.fee`; outbound priority-fee
  construction remains unsupported.
- Rewards are omitted from the block request and are not transaction movements.
- No new generic value, trait, movement kind, status, output record, persistence
  collection, or HTTP variant is introduced.
- The accepted S1.4 position/height changes remain a prerequisite for the
  Solana block reference but are not reopened here.

## Alternatives considered

### Infer transfers from pre- and post-balance differences

Rejected. Deltas establish net change but not causal source/destination pairs,
and they mix fees with account, reward, rent, and program effects.

### Trust `jsonParsed` transfer objects

Rejected. Provider parsing is not the chain-owned compatibility boundary, and
raw full responses provide the compiled instructions and loaded addresses
needed for pinned native decoding.

### Inspect only top-level instructions

Rejected. Programs can invoke the System Program through CPI, and recorded
inner transfers are committed native movements.

### Treat every account-key appearance as history

Rejected. Read-only or program accounts can be mentioned without a value
effect. This would create a general activity index, amplify spam, and disagree
with address-primary value history.

### Add a generic participant or signer collection

Rejected for initial native payment scope. It would expand generic domain and
persistence solely to retain failed attempts or non-financial participation.

### Store the fee as an outgoing transfer

Rejected. The existing model intentionally represents network fee separately,
and Solana metadata reports its exact total and payer directly.

### Interpret every lamport-bearing System Program variant now

Rejected for initial payment scope. Account creation and nonce withdrawal have
distinct account-lifecycle meaning. The completeness shield reports their
effect on a watched wallet rather than relabeling them as an ordinary payment.

### Silently ignore unexplained watched-address changes

Rejected. It would let exact balance and purportedly complete history disagree
without an observable failure.

### Add version-1 parsing before activation

Rejected by the approved legacy-and-version-0 scope. The release gate remains
visible so it can be expanded with a maintained dependency and fixtures rather
than claimed through a configuration number alone.

## Failure modes and required validation

- The source request must use finalized, full raw JSON transaction data,
  `maxSupportedTransactionVersion: 0`, and no rewards.
- A legacy transfer must use its first signature as ID, account zero as fee
  payer, accounts zero and one as movement endpoints, and exact lamports.
- A version-0 transfer must resolve static, loaded writable, and loaded
  read-only keys in native order, including a loaded program or endpoint.
- `TransferWithSeed` must use account two as destination, never its base
  authority at account one.
- Top-level and multiple or nested inner transfers must retain execution order,
  unique path IDs, and every movement when one endpoint is active.
- Repeated and self-directed transfers must remain separate; zero-lamport
  transfers must create no movement.
- A distinct fee payer, transfer source, and destination must remain distinct.
- A failed transaction with valid transfer instructions must emit no movements,
  retain its exact fee, and remain visible only to an active fee payer.
- A successful fee-payer-only transaction must remain visible with its fee.
- An inbound priority-fee transaction must use the priority-inclusive
  `meta.fee` without enabling outbound priority-fee policy.
- Missing metadata, missing version-0 loaded addresses, malformed signatures or
  canonical addresses, duplicate inner groups, out-of-range indices, balance
  vector mismatch, or successful relevant `innerInstructions: null` must leave
  the checkpoint unchanged.
- A non-System instruction with transfer-shaped bytes must produce no movement.
- An unexplained active-address delta must return an unsupported movement error
  rather than an inferred transfer or a partial commit.
- A read-only active-address injection must create no history.
- An unsupported version must fail the whole block rather than appear as an
  empty block.
- SPL instructions and rewards must never become native transfer movements.
- Wallet presentation must convert exact atomic lamports to nine-decimal SOL
  without floating point.
- redb and PostgreSQL contract tests must round-trip Solana transfer movements,
  fees, and failure status without adding a Solana-specific stored shape.
- API tests must preserve canonical Base58 transaction IDs and addresses, SOL
  metadata, existing movement/status shapes, and the S1.4 block reference.
- Bitcoin and Ethereum regression tests must prove their existing movement,
  fee, failure, and history behavior is unchanged.

## Approval boundary

Decision `S1.5` was explicitly approved on 2026-08-27. Acceptance records the
history-interpretation decision only; it does not authorize Solana source,
interpreter, wallet, persistence, API, dependency, or test implementation.

## References

- [Solana `getBlock`](https://solana.com/docs/rpc/http/getblock)
- [Solana RPC JSON structures](https://solana.com/docs/rpc/json-structures)
- [Solana transaction structure](https://solana.com/docs/core/transactions/transaction-structure)
- [Solana versioned transactions](https://solana.com/docs/core/transactions/versioned-transactions)
- [Solana fee structure](https://solana.com/docs/core/fees/fee-structure)
- [Anza `SystemInstruction`](https://docs.rs/solana-system-interface/latest/solana_system_interface/instruction/enum.SystemInstruction.html)
- `sdk/indexing/src/observation.rs`
- `sdk/indexing/src/block.rs`
- `sdk/indexing/src/indexer.rs`
- `sdk/chains/bitcoin/src/indexer/interpreter.rs`
- `sdk/chains/ethereum/src/indexer/interpreter.rs`
- `sdk/wallets/src/wallet.rs`
- `sdk/indexing/redb/src/repository/blocks.rs`
- `sdk/indexing/postgres/src/columns.rs`
- `apps/api/src/api/transaction.rs`
- `docs/SYSTEM_REQUIREMENTS.md`
- `docs/INDEXING.md`
