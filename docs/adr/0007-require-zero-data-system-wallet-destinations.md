# ADR-0007: Require zero-data System wallets for existing SOL destinations

## Status

Accepted

## Date

2026-08-27

## Context

S1.6.2.1 requires every initial native SOL destination to be on-curve. Curve
eligibility alone does not show whether an account exists or what the runtime
currently stores at that address.

Solana account metadata exposes program owner, executable state, and account
data. A successful `getAccountInfo` response uses `value: null` when the account
does not exist at the requested commitment. A present account may be owned by
the System Program without being an ordinary zero-data wallet: durable nonce
accounts and allocated System accounts carry data and have specialized state or
authority rules.

The Solana Foundation's illustrative address classifier accepts every existing
System-owned account. Payment-SDK's initial native SOL sends need a narrower
ordinary-wallet boundary and do not support nonce or allocated-account payment
semantics.

## Decision

After the approved on-curve gate, transaction preparation will classify one
complete account observation using this policy:

| Complete observation | S1.6.2.2 result |
|---|---|
| Explicitly absent | Eligible at the observed state |
| System Program owner, non-executable, total data length zero | Eligible at the observed state |
| System Program owner with non-zero data length | Reject as an unsupported special account |
| Executable account | Reject |
| Any other program owner | Reject |
| Missing, malformed, truncated, or otherwise unknown required evidence | Eligibility not established; fail closed |

Only a successful contextual RPC response whose value is explicitly null may
be classified as absent. Transport failure, JSON-RPC failure, a missing result,
invalid owner bytes, a missing executable flag, or an unknown data length must
never be converted to absence.

Owner comparison uses the exact 32-byte System Program identity. Data emptiness
means the account's authoritative total allocated data length is zero. An empty
returned slice does not prove this if the RPC request omitted or sliced account
data. A non-zero allocation is rejected even when every allocated byte happens
to contain zero.

The policy does not decode or guess which special System account is present.
Nonce accounts, uninitialized allocated accounts, and future non-empty System
states all remain outside initial ordinary-wallet support. Rejection does not
claim their funds are inherently unrecoverable; it means their control and
withdrawal rules are unsupported by this payment family.

Lamport balance and rent epoch do not affect this destination classification.
The minimal future Solana-local account fact contains only owner, executable,
and authoritative total data length. An acquisition wrapper may carry the RPC
context selected in S1.6.3. No generic account or destination model is
introduced.

The observation is ephemeral and must not be serialized into a transaction
snapshot or treated as a permanent property of the address. A restored or new
preparation must establish eligibility again before signing, simulation, or
broadcast. Its ordering relative to blockhash and fee RPC belongs to later
operation-coherence decisions.

## Point-in-time boundary

This policy establishes only that a destination was eligible at its observed
pre-sign state. It does not atomically constrain execution-time state. An
on-curve account holder may create, allocate, assign, or otherwise change the
account after validation. Re-reading can narrow that race but cannot remove it;
a plain System transfer carries no destination-owner or data-length assertion.

An execution-time guard would require a different program or transaction shape
and is not part of S1.6.2.2.

## Scope boundary

S1.6.2.2 decides only account-state eligibility. It does not decide:

- the RPC method, encoding, data slicing, or response DTO;
- commitment, context-slot freshness, endpoint affinity, failover, or retries;
- batching or deduplication of account reads;
- exact internal or public error mapping;
- revalidation timing or execution-time guards;
- blockhash, fee, balance, transaction, signing, or simulation rules; or
- broadcast, ambiguity, or batch-submission behavior.

Those acquisition and operation-coherence choices begin with S1.6.3.

## Alternatives considered

### Accept every existing System-owned account

Rejected for the initial product. This matches the broader Foundation example
but admits nonce and allocated System accounts whose specialized control rules
are not modeled by an ordinary wallet send.

### Decode System account data and allow selected variants

Rejected. Supporting nonce or allocated accounts would be a separate native
capability with its own authority, transaction, and recovery requirements.

### Treat all-zero data as empty

Rejected. Allocation length defines whether account data exists; byte contents
do not remove the account's allocated state.

### Require the destination account to exist

Rejected. An absent on-curve address is the normal unfunded-wallet case and is
explicitly supported by the accepted product requirements.

### Trust simulation to classify the recipient

Rejected. A System transfer may succeed while crediting an unintended account,
and simulation is neither an owner predicate nor a state lock.

## Consequences

- Unfunded on-curve wallets remain valid destinations.
- Existing destinations are limited to ordinary zero-data System wallets.
- Native sends reject on-curve nonce and allocated accounts even when their
  authority could eventually recover the lamports.
- No generic address, wallet, transaction, indexing, persistence, or HTTP type
  changes for S1.6.2.2.
- Account eligibility remains a point-in-time safety check rather than a claim
  of private-key possession or execution-time atomicity.

## Validation requirements

Focused tests must prove:

- explicit absence is eligible;
- a System-owned, non-executable, zero-data account is eligible;
- one byte of System-owned data is rejected, including when the byte is zero;
- initialized nonce and uninitialized allocated System accounts are rejected;
- executable and non-System-owned accounts are rejected;
- missing or malformed owner, executable, or total-length evidence fails closed;
- an empty sliced payload is not treated as proof of zero total data;
- RPC failure is never converted to explicit absence;
- every refusal occurs before signer, simulation, and broadcast doubles are
  invoked; and
- Bitcoin and Ethereum preparation behavior remains unchanged.

## Approval boundary

Decision `S1.6.2.2` was explicitly approved on 2026-08-27. Acceptance records
the account-state policy and the matching zero-data canonical-requirement
correction only; it does not authorize Solana source, wallet, transaction, RPC,
dependency, API, or test implementation.

## References

- [Solana native-payment address verification](https://solana.com/docs/payments/send-payments/verify-address)
- [Solana `getAccountInfo`](https://solana.com/docs/rpc/http/getaccountinfo)
- [Solana RPC account-data fields](https://solana.com/docs/rpc/json-structures#account-data)
- [Solana account structure](https://solana.com/docs/core/accounts/account-structure)
- [Anza nonce-account construction](https://docs.rs/solana-system-interface/latest/solana_system_interface/instruction/fn.create_nonce_account.html)
- [Anza System Program instructions](https://docs.rs/solana-system-interface/latest/solana_system_interface/instruction/enum.SystemInstruction.html)
- `sdk/wallets/src/address.rs`
- `sdk/chains/base/src/transaction.rs`
- `sdk/wallets/src/wallet.rs`
- `sdk/chains/ethereum/src/rpc/accounts.rs`
- `docs/SYSTEM_REQUIREMENTS.md`
- `docs/adr/0006-require-on-curve-native-sol-destinations.md`
