# Open requirements

The canonical consolidated specification, including settled requirements and
all Mermaid flows, is [`SYSTEM_REQUIREMENTS.md`](./SYSTEM_REQUIREMENTS.md). This
file remains the compact unresolved-decision checklist.

The scaffold is meant to expose these decisions before implementation. They
should become architecture decisions and tests, not assumptions hidden in code.

## Wallet and address lifecycle

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

- Which Bitcoin script types are supported initially: legacy, nested SegWit,
  native SegWit, Taproot, multisig?
- Is PSBT the durable unsigned/partially-signed Bitcoin representation?
- Which coin-selection policies are required: minimize fee, minimize change,
  privacy grouping, exact match, sweep?
- How are UTXOs reserved so concurrent withdrawals cannot select the same input?
- When is a change address allocated, and when may it be reused?
- Which fee ceilings, dust rules, and approval policies prevent accidental
  overpayment?
- For Ethereum, which envelope types are required and how are pending nonces
  reserved across concurrent withdrawals?
- Are replacement, fee bump, cancellation, and rebroadcast workflows required?

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

- Is the proposed atomic key/value contract sufficient, or should semantic
  repositories own all persistence contracts?
- What consistency and isolation guarantees are required?
- Must block commit, balance materialization, transaction history, watch state,
  and outbox notifications share one atomic commit?
- How are schema versions and migrations represented without naming a backend?
- What idempotency keys prevent duplicate address creation, withdrawals, and
  notifications?

## Deposit accounting

- Which assets permit a reliable event-derived balance, and which require a
  periodic chain balance reconciliation?
- How is a shared Bitcoin batch fee allocated across deposits?
- What exact dust, fee, unsolicited-spend, fee-on-transfer, and rebasing drift
  is permitted by the reconciliation relation?
- After a post-credit reorg, does PS post a user-ledger reversal, create debt,
  or accept the loss?
- Is credit allowed at IX `Confirmed`, or does the business require an even
  stronger per-asset threshold?
- Which reservation state prevents a deposit or UTXO from being collected by
  two concurrent jobs?

## Application API

- Are reconciliation and event-delivery workers separate executables or loops
  inside `apps/api` and `apps/indexer`?
- Is transaction building synchronous from already indexed state, or may it
  query RPC during the request?
- Which operations are commands returning jobs versus immediate results?
- What is the external wallet/address/transaction status model?
- Which events must be delivered exactly once versus at least once?
