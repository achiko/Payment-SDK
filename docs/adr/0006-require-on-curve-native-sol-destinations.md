# ADR-0006: Require on-curve destinations for initial native SOL sends

## Status

Accepted

## Date

2026-08-27

## Context

A canonical 32-byte Solana address may be either on the Ed25519 curve or
off-curve. On-curve addresses can correspond to ordinary keypairs. An off-curve
address has no corresponding Ed25519 private key; its spending mechanism may
instead depend on program-derived signing or a separate base-and-seed authority
and cannot be inferred from the address alone.

The accepted S1.2 representation decision intentionally parses both forms.
Indexing, program identification, and general Solana RPC must be able to carry
valid off-curve addresses. Destination eligibility is therefore a transaction
policy, not an address-syntax rule.

A System Program transfer does not require the destination to sign or be
on-curve and may credit a writable destination under the runtime's account
rules. It does not prove that the recipient can later spend the lamports. The
Solana Foundation's reference payment flow rejects an absent off-curve address
but accepts an existing System-owned account without checking its curve. The
current system requirement uses that broader asymmetric policy.

Payment-SDK wallet generation and import produce Ed25519 keypair wallets. This
ADR proposes deliberately narrowing outbound recipients to the same
keypair-compatible address shape. The project has no approved program-specific
recipient registry, derivation evidence, or contract describing how an
off-curve destination is controlled.

## Decision

Every initial native SOL destination must be on the Ed25519 curve, regardless
of whether an account currently exists at that address.

After canonical Base58 parsing and exact 32-byte validation, Solana transaction
preparation will use a maintained, upstream-compatible Ed25519 curve predicate.
Selection and pinning of the exact dependency remains deferred. The policy
result is:

| Address fact | S1.6.2.1 result |
|---|---|
| On-curve | Continue to the separately approved account-state check |
| Off-curve | Reject as an unsupported native SOL destination |

An off-curve result is not a syntax error. The address remains a valid Solana
address, but the initial payment product cannot establish its spending
mechanism. It is rejected before external effects without changing
`AddressFormat::parse_address` or labeling the address malformed. Exact error
mapping remains a later transaction/API decision.

The curve check is pure and local. A rejected off-curve destination causes no
account RPC, blockhash request, fee quotation, signing, simulation, or
broadcast. An on-curve result is necessary but not sufficient: S1.6.2.2 must
still decide how existence, owner, executable state, and account data affect
eligibility.

This decision narrows the current canonical requirement. If S1.6.2.1 is
accepted, `docs/SYSTEM_REQUIREMENTS.md` must be updated so both absent and
existing native SOL destinations are required to be on-curve.

## Scope boundary

S1.6.2.1 decides only the local curve policy. It does not decide:

- whether an on-curve account is absent or present;
- accepted owner, executable, or data states;
- RPC method, commitment, context slot, endpoint affinity, or retries;
- whether the destination equals the source;
- blockhash, fee, balance, transaction, signing, or simulation rules;
- broadcast, ambiguity, or batch behavior; or
- any allowlist for a specific PDA, program, multisig, or custody protocol.

## Alternatives considered

### Check curve only when the account is absent

Rejected for the initial product. This matches the broader Foundation
reference flow, but an existing off-curve System account still has no private
key for its own address. Spending may depend on program-derived signing or a
separate base-and-seed authority Payment-SDK does not model, and disappearance
after validation could leave the transfer targeting an absent off-curve
address.

### Accept an existing off-curve account when it is System-owned

Rejected initially. System ownership alone does not establish the spending
mechanism or intended controller for that address.

### Reject off-curve values during Base58 parsing

Rejected. Off-curve addresses are valid Solana protocol addresses and remain
necessary for indexing and RPC. Only native payment destinations receive this
narrower policy.

### Add configurable PDA or program allowlists now

Rejected. That would add a new program-aware payment capability and trust
policy beyond initial native wallet sends.

## Consequences

- Initial native SOL sends cannot target PDAs or other off-curve custody
  arrangements, even when a particular program could spend those lamports.
- General Solana address parsing and indexed history continue to support
  off-curve addresses.
- No generic address, wallet, transaction, indexing, persistence, or HTTP type
  changes for S1.6.2.1.
- The policy removes one class of potentially unrecoverable destination but
  does not prove that anybody controls a particular on-curve private key.
- Plain Base58 still has no checksum, so caller typos remain a residual product
  risk even when the resulting address is on-curve.

## Validation requirements

Focused tests must prove:

- a known keypair public key passes the curve gate;
- known program-derived addresses fail the curve gate;
- both on-curve and off-curve addresses still round-trip through general
  canonical Base58 parsing;
- an off-curve payment destination fails before every RPC and signer double;
- no generic address behavior changes; and
- Bitcoin and Ethereum destination parsing remains unchanged.

## Approval boundary

Decision `S1.6.2.1` was explicitly approved on 2026-08-27. Acceptance records
the destination-curve policy and the matching canonical-requirement correction
only; it does not authorize Solana source, wallet, transaction, RPC, dependency,
API, or test implementation.

## References

- [Solana native-payment address verification](https://solana.com/docs/payments/send-payments/verify-address)
- [Solana program-derived addresses](https://solana.com/docs/core/pda)
- [Anza `Address::is_on_curve` candidate](https://docs.rs/solana-address/latest/solana_address/struct.Address.html#method.is_on_curve)
- [Anza System Program instructions](https://docs.rs/solana-system-interface/latest/solana_system_interface/instruction/enum.SystemInstruction.html)
- `sdk/wallets/src/address.rs`
- `sdk/chains/base/src/transaction.rs`
- `docs/SYSTEM_REQUIREMENTS.md`
- `docs/adr/0001-use-canonical-base58-for-solana-addresses.md`
