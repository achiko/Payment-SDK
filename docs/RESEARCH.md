# Upstream architecture research

The repositories listed below are shallow local checkouts under `reference/`.
They are research sources, not workspace dependencies or vendored production
code.

## Alloy

Relevant sources:

- [`Network`](../reference/alloy/crates/network/src/lib.rs)
- [`TransactionBuilder`](../reference/alloy/crates/network/src/transaction/builder.rs)
- [`NetworkWallet` and `TxSigner`](../reference/alloy/crates/network/src/transaction/signer.rs)
- [`Signer`](../reference/alloy/crates/signer/src/signer.rs)

Useful decisions:

- focused crates with a composition/facade layer;
- associated types keep network-native request, unsigned transaction, signed
  envelope, receipt, header, and RPC response types connected;
- building an unsigned transaction is distinct from signing it;
- concrete signer implementations are optional dependencies.

Decisions not copied:

- Alloy's signer is Ethereum-oriented: it owns an Ethereum address and chain
  ID. Our generic signer must not own either;
- Alloy's network abstraction is designed for EVM-family networks, not a
  universal Bitcoin/Ethereum/Solana transaction model;
- provider fillers are useful for EVM request population but are not yet a
  justified universal layer for this payment indexer.

## ethers.js

Relevant source:

- [`AbstractSigner`](../reference/ethers-js/src.ts/providers/abstract-signer.ts)

ethers connects many signers to a provider and lets the signer populate nonce,
gas, chain ID, and fees before broadcasting. This is convenient for an
application SDK but couples key authority to network access. Our signer remains
network-free; concrete chain transaction services perform RPC population.

## rust-bitcoin and BDK

Relevant sources:

- [`Psbt`](../reference/rust-bitcoin/bitcoin/src/psbt/mod.rs)
- [`SighashCache`](../reference/rust-bitcoin/bitcoin/src/crypto/sighash.rs)
- [`SyncRequest`](../reference/bdk/crates/core/src/spk_client.rs)
- [`CheckPoint`](../reference/bdk/crates/core/src/checkpoint.rs)
- [`Indexer`](../reference/bdk/crates/chain/src/indexer.rs)
- [`TxGraph`](../reference/bdk/crates/chain/src/tx_graph.rs)

Findings:

- a Bitcoin signing input needs previous-output data, script information,
  derivation/key origin, and a sighash policy;
- partially signed transactions and multiple signers are real states, even if
  the first payment workflow uses one signer;
- sync targets may include scripts, transaction IDs, and outpoints;
- checkpoints represent chain identity, while change sets separate in-memory
  mutation from durable persistence;
- confirmed anchors and mempool `last_seen`/eviction data are different facts;
- transaction conflicts must coexist until canonicalization decides which one
  is effective.

The scaffold borrows checkpoint and reversible-change ideas without moving
Bitcoin script or transaction graph types into generic indexing.

## Trezor Blockbook

Relevant sources:

- [`BlockChain` interface](../reference/blockbook/bchain/types.go)
- [`ResyncIndex`](../reference/blockbook/db/sync.go)
- [database connect/disconnect](../reference/blockbook/db/rocksdb.go)

Findings:

- synchronization compares local and remote hashes at the same height;
- a fork is handled by finding a common ancestor, disconnecting blocks, and
  reconnecting the canonical branch;
- fetch can be parallel but durable block connection is ordered;
- Bitcoin-type and Ethereum-type index data have materially different connect
  and disconnect behavior.

Blockbook's daemon interface is intentionally not copied as one Rust trait: it
mixes lifecycle, RPC, mempool, fees, parsing, token, and chain-specific methods.
Our source, interpreter, storage, and service contracts remain separate.

## NBXplorer and BTCPay Server

Relevant sources:

- [`NewTransactionEvent`](../reference/nbxplorer/NBXplorer.Client/Models/NewTransactionEvent.cs)
- [`GetTransactionsResponse`](../reference/nbxplorer/NBXplorer.Client/Models/GetTransactionsResponse.cs)
- [`UTXOChanges`](../reference/nbxplorer/NBXplorer.Client/Models/UTXOChanges.cs)
- [NBXplorer transaction query](../reference/nbxplorer/NBXplorer/Controllers/MainController.cs)
- [`NBXplorerListener`](../reference/btcpayserver/BTCPayServer/Payments/Bitcoin/NBXplorerListener.cs)
- [BTCPay payment persistence](../reference/btcpayserver/BTCPayServer/Services/Invoices/PaymentService.cs)

Findings:

- NBXplorer is infrastructure for tracked sources rather than a full explorer;
- its transaction events retain matched inputs and outputs, key paths,
  confirmations, replacement links, and block identity;
- its API separates event delivery (polling, long polling, WebSocket),
  transaction queries, UTXO queries, PSBT construction, and broadcast;
- BTCPay consumes NBXplorer events but queries again after restarts to find
  payments that arrived while offline;
- BTCPay associates matched outputs with invoice records in its own database;
- confirmations can move a payment from processing to settled, while replaced
  or orphaned transactions can become unaccounted.

This directly supports an IX event/query boundary with durable replay and a
separate PS classifier. We do not copy NBXplorer's Bitcoin-only tracked-source
types into generic indexing; IX normalizes chain facts and preserves stable
movement IDs so Bitcoin inputs/outputs remain attributable.

## SHKeeper

Relevant sources:

- [invoice and callback contract](../reference/shkeeper/README.md)
- [invoice/transaction persistence](../reference/shkeeper/shkeeper/models.py)
- [confirmation/callback worker](../reference/shkeeper/shkeeper/callback.py)
- [payout service](../reference/shkeeper/shkeeper/services/payout_service.py)

Findings:

- invoices map unique external IDs to generated asset addresses;
- callbacks include every transaction associated with the invoice and are
  retried until acknowledged;
- partial, paid, and overpaid are business classifications above transaction
  observations;
- payout submission is asynchronous and is followed by status/confirmation
  polling;
- current persistence deduplicates transactions using invoice/asset/txid-like
  keys.

The useful behavior is retained: idempotent address issuance, partial deposits,
late additional transactions, callbacks/replay, and asynchronous collection.
Its invoice/chain/payout coupling is not used as the package boundary here;
PS classification, IX facts, and WS operations remain independent.

## Why both Blockbook and NBXplorer matter

Blockbook is the stronger model for a multi-chain address/balance index and
ordered connect/disconnect mechanics. NBXplorer is the stronger model for a
private watched-source service with exact input/output matches, replayable
events, and transaction/UTXO queries. The proposed IX combines those lessons:

- Blockbook-style canonical checkpoint and reorg rollback;
- NBXplorer-style watch/query/event surface;
- chain-owned parsing rather than a universal transaction parser;
- no payment/invoice semantics in IX.

## Trezor firmware

Relevant sources:

- [Bitcoin protobuf messages](../reference/trezor-firmware/common/protob/messages-bitcoin.proto)
- [Ethereum protobuf messages](../reference/trezor-firmware/common/protob/messages-ethereum.proto)

Findings:

- public-key and arbitrary-message operations fit a generic key/signer API;
- Bitcoin transaction signing does not: the device drives a `SignTx` / repeated
  `TxRequest` / `TxAck` protocol and may request current inputs, outputs,
  previous transactions, and replacement transaction data;
- the device verifies amounts and displays transaction intent, so replacing
  the native flow with blind digest signing can weaken hardware-wallet policy.

This creates a deliberate open architecture decision rather than permission to
put Trezor directly inside the Bitcoin chain crate.

## Solana SDK and Solana Keychain

Relevant sources:

- [`Signer`](../reference/solana-sdk/signer/src/lib.rs)
- [`Transaction`](../reference/solana-sdk/transaction/src/lib.rs)
- [`SolanaSigner`](../reference/solana-keychain/rust/src/traits.rs)

Findings:

- public key plus message signing is a small, useful signer boundary;
- a Solana transaction is signatures plus a serialized message and can require
  multiple signers;
- partial signing is a valid state;
- signer implementations may be interactive or unavailable;
- “account-based” does not mean Ethereum-shaped: Solana uses instructions,
  account metadata, fee payer, recent blockhash, and possibly address lookup
  tables.

This validates keeping the account builder narrow and preserving chain-native
transaction associated types.

## Resulting design principles

1. Preserve chain-native types with associated types instead of normalizing
   transactions prematurely.
2. Keep signer identity/key operations independent from providers and chains.
3. Let concrete chains combine pure builders with signers.
4. Make indexing block-hash aware and explicitly reversible.
5. Persist block effects and checkpoint movement atomically.
6. Keep mempool observations separate from canonical confirmation.
7. Model backend capabilities so missing trace/history support is visible.
8. Keep partial/multiple signatures possible even if not implemented first.
9. Treat block inclusion and accounting-grade confirmation as distinct states.
10. Make observation delivery replayable and idempotent by revision.
11. Keep long-lived collection workflow state in PS, not stateless WS.
