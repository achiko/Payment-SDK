# Upstream architecture research

This document records design lessons from established open-source chain and
wallet projects. They are research sources, not workspace dependencies or
vendored code.

## Alloy

Reviewed concepts: `Network`, `TransactionBuilder`, `NetworkWallet`,
`TxSigner`, and `Signer`.

Useful lessons:

- focused crates compose through a facade;
- associated types keep network-native requests, unsigned transactions,
  envelopes, receipts, headers, and RPC responses connected;
- building is distinct from signing; and
- concrete signer implementations can remain optional.

Alloy's network and signer abstractions are EVM-oriented. They are not a
universal Bitcoin/Ethereum model, and an Ethereum address or chain ID does not
belong in this workspace's minimal generic signer.

## ethers.js

Reviewed concept: `AbstractSigner`.

ethers connects a signer to a provider and lets it populate nonce, gas, chain
ID, and fees. That is convenient but couples key authority to network access.
Here the signer remains network-free; the concrete chain builder performs RPC
population and asks the signer for only the cryptographic operation.

## rust-bitcoin and BDK

Reviewed concepts: `Psbt`, `SighashCache`, `SyncRequest`, `CheckPoint`,
`Indexer`, and `TxGraph`.

Findings:

- Bitcoin signing needs previous-output, script, key-origin, and sighash data;
- partially signed and multiple-signer transactions are valid protocol states;
- sync targets may be scripts, transaction IDs, and outpoints;
- checkpoints represent chain identity, while change sets separate mutation
  from persistence;
- confirmed anchors and mempool last-seen/eviction are different facts; and
- conflicting transactions coexist until canonicalization resolves them.

The SDK adopts checkpoint and reversible-effect ideas without putting Bitcoin
scripts or transaction graphs into generic indexing.

## Trezor Blockbook

Reviewed concepts: the chain interface, resynchronization, and RocksDB block
connect/disconnect behavior.

Findings:

- compare local and remote hashes at the same height;
- find a common ancestor, disconnect, then reconnect on forks;
- fetching may be parallel but durable connection is ordered; and
- Bitcoin and Ethereum projections have materially different rollback data.

Blockbook's broad daemon interface mixes lifecycle, RPC, mempool, fees,
parsing, tokens, and chain-specific methods. This workspace keeps source,
interpreter, repository, and worker contracts separate.

## NBXplorer and BTCPay Server

Reviewed concepts: transaction events, transaction queries, UTXO changes,
listeners, and payment persistence.

Findings:

- watched-source infrastructure can remain smaller than a full explorer;
- events retain matched inputs/outputs, key paths, confirmations, replacement
  links, and block identity;
- event delivery, transaction/UTXO query, construction, and broadcast are
  separate capabilities;
- consumers query again after restart instead of trusting ephemeral events;
  and
- invoice/payment meaning is owned above the indexer.

The embedded indexer adopts replayable events and exact movements without
copying Bitcoin-only tracked-source types or invoice semantics.

## SHKeeper

Reviewed concepts: invoice/address persistence, callbacks, confirmation work,
and payout submission.

Its business workflows are outside the current workspace. The retained lesson
is that transaction delivery must be replayable, idempotent, and queryable
after consumer restarts. Partial/paid/overpaid are business classifications,
not indexing statuses.

## Hardware-wallet protocols

Trezor's Bitcoin transaction signing is a repeated request/acknowledgement
protocol that may request current inputs, outputs, previous transactions, and
replacement data. The device also verifies and displays intent. Blind digest
signing is not an equivalent abstraction.

Hardware wallets are outside the current workspace. If added later, their
native interactive protocols must not be forced through an inadequate generic
digest-only implementation.

## Solana SDK and keychain

Reviewed concepts: signer, transaction, and Solana-specific signer traits.

Findings:

- public key plus message signing is a useful small boundary;
- a Solana transaction contains signatures plus a serialized message and may
  require several signers;
- partial signing is valid;
- a signer may be interactive or unavailable; and
- “account based” is not synonymous with Ethereum-shaped transactions.

This reinforces preservation of chain-native transactions and argues against a
universal account-chain builder.

## Resulting principles

1. Preserve chain-native transaction types instead of normalizing too early.
2. Keep cryptographic signing independent from providers and chains.
3. Let concrete chains combine pure protocol construction with injected
   signers.
4. Make indexing block-hash aware and reversible.
5. Persist block effects and checkpoint movement atomically.
6. Keep mempool observations distinct from canonical inclusion.
7. Make missing trace and historical-query capabilities explicit.
8. Do not design away partial or multiple signatures.
9. Treat inclusion and configured confirmation as different states.
10. Preserve corrected observations as stable revisions queryable in history.
11. Compose concrete chain, indexing, storage, and wallet implementations only
    in `apps/api`.

## Current application

The current Bitcoin/Ethereum implementation applies those principles with:

- same-height hash comparison and ordered reorg rollback;
- durable address watches, transaction queries, and immutable revisions;
- polling as authoritative synchronization, with websocket heads only as an
  optional wake-up hint;
- standard Ethereum blocks/receipts/logs with missing trace completeness made
  explicit;
- chain-owned parsing and semantic projection effects; and
- one embedded redb adapter owning physical records and atomic batches.

See [`INDEXING.md`](INDEXING.md) for the concrete indexing design and
[`CHAIN_RESEARCH.md`](CHAIN_RESEARCH.md) for additional-chain constraints.
