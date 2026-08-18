# Top 20 chain payment-integration research

Status: design research, 2026-08-14. This is not a claim of implementation or production readiness.

## Scope and selection

The set below is a deliberately engineering-oriented sample of prominent native
payment networks: high market relevance and usage, plus enough protocol-family
diversity to test whether the SDK boundaries survive additional chains. It
excludes stablecoins, ordinary application tokens, duplicate test networks, and
ecosystems whose native asset is not normally transferred on the named chain.
Selection is not a market-cap ranking: market rankings move daily and would
overweight near-identical EVM networks. The 20 selected networks are Bitcoin,
Ethereum, Solana, BNB Smart Chain, XRP Ledger, Cardano, Dogecoin, TRON,
Avalanche C-Chain, Polygon PoS, Litecoin, Bitcoin Cash, Polkadot, Cosmos Hub,
NEAR, Arbitrum One, OP Mainnet, Base, Sui, and Aptos.

Protocol facts below come from official specifications, project documentation,
or canonical implementation repositories. A derivation path labelled
"convention" is wallet interoperability policy, not consensus. Public RPC URLs
are examples, not production SLAs.

## Normalized comparison

| Network | State model | Address / main key | Native unit | Transaction / finality | Primary integration API |
|---|---|---|---|---|---|
| Bitcoin | UTXO | Base58Check or Bech32(m); secp256k1 | 8, satoshi | Bitcoin wire, ECDSA/Schnorr; probabilistic PoW | Core JSON-RPC, optional ZMQ |
| Ethereum | account/EVM | 20-byte hex/EIP-55; secp256k1 | 18, wei | EIP-2718/RLP; PoS `safe`/`finalized` | execution JSON-RPC HTTP/WS |
| Solana | account/program | 32-byte base58 Ed25519 pubkey | 9, lamport | compact legacy/v0 message; confirmed/finalized | JSON-RPC HTTP/WS |
| BNB Smart Chain | account/EVM | EVM address; secp256k1 | 18, wei | typed EVM tx; Parlia finalized head | EVM JSON-RPC |
| XRP Ledger | account/ledger entries | 20-byte AccountID, Ripple Base58; secp256k1/Ed25519 | 6, drop | canonical STObject; validated ledger final | xrpld/Clio HTTP+WS |
| Cardano | extended UTXO | CIP-19 Bech32; Ed25519-BIP32 | 6, lovelace | era-specific CBOR; probabilistic settlement | node-to-client, Ogmios WS |
| Dogecoin | UTXO | Base58Check; secp256k1 | 8, koinu | Bitcoin-derived, no SegWit; PoW/AuxPoW | Dogecoin Core JSON-RPC |
| TRON | account | 21-byte `41..` / Base58Check; secp256k1 | 6, sun | protobuf + TAPOS; solidified DPoS block | java-tron HTTP/gRPC |
| Avalanche C-Chain | account/EVM | EVM address; secp256k1 | 18, atomic AVAX | typed EVM tx; accepted ~= irreversible | Coreth JSON-RPC |
| Polygon PoS | account/EVM | EVM address; secp256k1 | 18, wei (POL) | typed EVM tx; milestone finality | EVM JSON-RPC |
| Litecoin | UTXO (+MWEB) | Base58Check/Bech32; secp256k1 | 8, litoshi | BTC-like plus MWEB; PoW | Litecoin Core JSON-RPC |
| Bitcoin Cash | UTXO | CashAddr; secp256k1 | 8, satoshi | BCH wire/sighash; PoW | BCHN JSON-RPC |
| Polkadot | account/runtime | 32-byte AccountId, SS58; sr25519 default | 10, planck | SCALE extrinsic; GRANDPA finalized | Substrate JSON-RPC WS |
| Cosmos Hub | account/SDK | 20-byte Bech32 `cosmos`; secp256k1 | 6, uatom | protobuf TxRaw; CometBFT final | Comet RPC + gRPC |
| NEAR | named/implicit accounts | AccountId; Ed25519 commonly | 24, yoctoNEAR | Borsh actions/receipts; final outcome | JSON-RPC + indexer stream |
| Arbitrum One | optimistic-rollup EVM | EVM address; secp256k1 | 18, wei | EVM tx; unsafe/safe/finalized L2 heads | EVM RPC + sequencer feed |
| OP Mainnet | optimistic-rollup EVM | EVM address; secp256k1 | 18, wei | EVM + deposit tx; L1-derived finality | EVM RPC |
| Base | optimistic-rollup EVM | EVM address; secp256k1 | 18, wei | OP Stack; L1-derived finality | EVM RPC/Flashblocks |
| Sui | versioned object/Move | 32-byte hex; tagged Ed25519/k1/r1 | 9, MIST | BCS programmable tx block; BFT effects | gRPC + GraphQL |
| Aptos | account/resource Move | 32-byte hex; Ed25519 plus authenticators | 8, octa | BCS raw tx; ledger-version BFT finality | REST/BCS + transaction stream |

## Protocol-family detail

### Bitcoin

Addresses are P2PKH/P2SH Base58Check or SegWit witness programs: mainnet
`bc1q` uses Bech32 and Taproot `bc1p` uses Bech32m. A private key is a valid
32-byte secp256k1 scalar; WIF and BIP32 are wallet containers. Common wallet
paths (BIP44/49/84/86) are conventions, not consensus. A transaction consumes
outpoints and creates integer-valued outputs; SegWit distinguishes txid from
wtxid. ECDSA/BIP143 and BIP340/341 Schnorr sighashes must remain chain-owned.
Blocks have an 80-byte PoW header and probabilistic finality. Scan canonical
`(height,hash,parent)` with `getblockhash`, `getblock`, mempool reconciliation,
and prevout lookup; `getrawtransaction` needs block context or `-txindex`.
Build with UTXO reservation, current fee estimation and dust rules, sign each
input, run `testmempoolaccept`, then `sendrawtransaction`. ZMQ is a latency hint.
[Addresses](https://github.com/bitcoin/bips/blob/master/bip-0173.mediawiki),
[Taproot](https://github.com/bitcoin/bips/blob/master/bip-0341.mediawiki),
[RPC](https://bitcoincore.org/en/doc/).

### Ethereum and the shared EVM engine

An EOA address is the last 20 bytes of Keccak-256 of the uncompressed
secp256k1 public key, displayed as `0x` hex with optional EIP-55 checksum. A
private key is a 32-byte scalar. Type-2 transactions encode chain ID, nonce,
fee caps, gas, destination, value, calldata, access list and signature in an
EIP-2718 envelope; legacy and newer types must remain decodable. Native asset
uses 18 decimal places. Use `eth_chainId`, pending transaction count, balance,
`eth_estimateGas`, fee history, `eth_sendRawTransaction`, receipt, numbered
blocks, logs, and safe/finalized tags. Receipts and allowlisted `Transfer` logs
cover token movement; complete internal native movement requires client-specific
traces. Reserve nonce atomically and track replacements by sender+nonce.
[Accounts](https://ethereum.org/developers/docs/accounts/),
[typed transactions](https://eips.ethereum.org/EIPS/eip-2718),
[JSON-RPC](https://ethereum.org/developers/docs/apis/json-rpc/),
[PoS finality](https://ethereum.org/developers/docs/consensus-mechanisms/pos/gasper/).

BNB Smart Chain uses chain ID 56 and BNB, with Parlia/fast-finality. Its public
RPC can restrict `eth_getLogs`; production indexing needs capable redundant or
self-hosted nodes. [Official endpoints and limits](https://docs.bnbchain.org/bnb-smart-chain/developers/json_rpc/json-rpc-endpoint/),
[finality APIs](https://docs.bnbchain.org/bnb-smart-chain/developers/json_rpc/bsc-api-list/).

Avalanche C-Chain uses chain ID 43114 and AVAX. It has rapid accepted finality,
but its header carries Avalanche extension data: never recompute a block hash
with an Ethereum-only header codec. X/P chains are out of this adapter's scope.
[Exchange integration](https://build.avax.network/docs/primary-network/exchange-integration),
[C-Chain API](https://build.avax.network/docs/rpcs/c-chain/api).

Polygon PoS uses chain ID 137 and POL. Query the finalized head; milestone
finality and Ethereum checkpointing are different stages. Token identity is
`(chain, contract)`, especially for bridged mappings.
[RPC](https://docs.polygon.technology/pos/reference/rpc-endpoints),
[finality](https://docs.polygon.technology/pos/concepts/finality/finality),
[fees](https://docs.polygon.technology/pos/concepts/transactions/eip-1559).

Arbitrum One (42161), OP Mainnet (10), and Base (8453) reuse EVM address and
transaction codecs but require rollup-owned fee and finality strategies.
Sequencer acceptance is only soft confirmation. Persist unsafe, safe and
finalized heads separately. L2 execution plus L1 data/operator fees cannot be
estimated with Ethereum assumptions. OP/Base blocks may contain deposit/system
type `0x7e`; preserve unknown types. Bridge withdrawals are multi-transaction,
multi-day workflows and are not ordinary L2 transfers.
[Arbitrum chain data](https://docs.arbitrum.io/for-devs/dev-tools-and-resources/chain-info),
[OP status model](https://docs.optimism.io/app-developers/guides/transactions/statuses),
[OP fees](https://docs.optimism.io/op-stack/transactions/fees),
[Base finality](https://docs.base.org/base-chain/network-information/transaction-finality),
[Base fees](https://docs.base.org/base-chain/network-information/network-fees).

### Other UTXO chains

Litecoin has mainnet P2PKH `L...`, P2SH `M...`, SegWit `ltc1`, 8 decimals,
secp256k1, Scrypt PoW and a 2.5-minute target. Its MWEB extension adds
confidential outputs/kernels and cannot be represented as transparent vin/vout;
make it an explicit later capability.
[Prefixes](https://litecoin.info/docs/key-concepts/addresses-prefixes),
[MWEB LIP](https://github.com/litecoin-project/lips/blob/master/lip-0003.mediawiki),
[Core](https://github.com/litecoin-project/litecoin).

Bitcoin Cash prefers CashAddr (`bitcoincash:`) with network-bearing prefix and
40-bit checksum. Its superficially Bitcoin-like transaction uses BCH-specific
fork-ID/value-aware signing, BCH ECDSA or non-BIP340 Schnorr, and current
CashTokens/output rules. Never reuse Bitcoin sighash insertion.
[CashAddr](https://documentation.cash/protocol/blockchain/encoding/cashaddr.html),
[transactions](https://documentation.cash/protocol/blockchain/transaction.html),
[signatures](https://documentation.cash/protocol/blockchain/cryptography/signatures.html),
[BCHN](https://docs.bitcoincashnode.org/).

Dogecoin uses Base58Check (typical P2PKH `D...`), secp256k1, 8 decimals,
Bitcoin-derived non-SegWit transactions, Scrypt and AuxPoW. Pin Dogecoin Core
RPC schemas rather than assuming modern Bitcoin Core behavior.
[Network parameters](https://github.com/dogecoin/dogecoin/blob/master/src/chainparams.cpp),
[AuxPoW](https://github.com/dogecoin/dogecoin/blob/master/src/auxpow.h).

For all UTXO chains, address history is derived from canonical blocks plus
prevouts, not a universal account query. Fees, dust, standardness, replacement,
coinbase maturity and finality thresholds are network policies. Every movement
is an outpoint effect; there is no honest single `from -> to` abstraction.

### Solana

A wallet is normally a 32-byte Ed25519 public key rendered Base58, but the same
address space includes off-curve PDAs and program-owned accounts. Verify native
withdrawal recipients. The wallet path `m/44'/501'/0'/0'` is a tooling
convention. Legacy/v0 transactions contain ordered signatures and an exact
message with account keys, recent blockhash and compiled instructions; sign the
message bytes directly. Resolve v0 address lookup tables and inner instructions.
The first signature is the transaction ID. Scan produced slots, checkpoint
`(slot,blockhash,parentSlot)`, and decode successful top-level and inner System
transfers. Sending uses latest blockhash, fee-for-message, simulation,
`sendTransaction`, status and transaction lookup. Blockhash expiry requires a
new transaction/signature; durable nonce is a separate capability. One SOL is
1e9 lamports.
[Core](https://solana.com/docs/core),
[transaction structure](https://solana.com/docs/core/transactions/transaction-structure),
[RPC](https://solana.com/docs/rpc),
[send](https://solana.com/docs/rpc/http/sendtransaction),
[recipient safety](https://solana.com/docs/payments/send-payments/verify-address).

### XRP Ledger

A classic address is Ripple-Base58Check of a 20-byte AccountID; X-address is an
application encoding that additionally carries destination tag and network.
Keys may be secp256k1 or Ed25519, and account authority can rotate to RegularKey
or multisigners. XRPL seed/family derivation is its own convention. Native
Payment amounts are integer drops (1e6 per XRP). Serialize canonical STObject,
sign its domain-separated hash, and hash the signed binary for the transaction
ID. Query dynamic fee and account reserve; allocate Sequence or Ticket and set
LastLedgerSequence. Submit the same signed blob idempotently and require a
validated ledger with `meta.TransactionResult == tesSUCCESS`. For deposits use
metadata `delivered_amount`, not requested Amount, and validate DestinationTag.
Backfill `account_tx` or whole validated ledgers and accelerate with WS accounts
and ledger subscriptions.
[Addresses](https://xrpl.org/docs/concepts/accounts/addresses),
[binary format](https://xrpl.org/docs/references/protocol/binary-format),
[reliable submission](https://xrpl.org/docs/concepts/transactions/reliable-transaction-submission),
[finality](https://xrpl.org/docs/concepts/transactions/finality-of-results),
[subscriptions](https://xrpl.org/docs/references/http-websocket-apis/public-api-methods/subscription-methods/subscribe).

### Cardano

Shelley addresses encode a type/network header and payment/stake credentials as
Bech32 (`addr1`/`addr_test1`); Byron Base58 remains historical. Cardano uses
Ed25519-BIP32 and the modern wallet convention `m/1852'/1815'/account'/role/index`;
Icarus versus legacy master derivation must be explicit. Transactions are
era-specific canonical CBOR. Hash the body with Blake2b-256 and add vkey
witnesses. Coin selection must preserve all native asset bundles, minimum ADA,
fee, validity and change. Observe through node chain-sync RollForward/RollBackward,
with `(slot,hash)` checkpoints and UTXO undo. Native ADA is 1e6 lovelace.
cardano-node uses local Ouroboros protocols; Ogmios is the practical official
ecosystem JSON-RPC bridge. Submission acceptance is not settlement.
[CIP-19](https://cips.cardano.org/cip/CIP-19),
[CIP-1852](https://cips.cardano.org/cip/CIP-1852),
[formal ledger specs](https://github.com/IntersectMBO/cardano-formal-specifications),
[Ogmios chain sync](https://ogmios.dev/mini-protocols/local-chain-sync/),
[confirmation](https://docs.cardano.org/about-cardano/learn/chain-confirmation-versus-transaction-confirmation).

### TRON

TRON derives a 20-byte EVM-like payload from secp256k1 but prefixes `0x41` and
Base58Check-encodes the resulting 21 bytes (`T...`). Addresses do not by
themselves identify the network. The HD path `m/44'/195'/0'/0/index` is wallet
convention. Native TransferContract is protobuf, not RLP. `raw_data` includes
contract, recent-block TAPOS bytes/hash, expiration and fees; txID/signing digest
is SHA-256 of serialized raw_data and the recoverable signature is appended.
Scan solidified blocks and successful direct/internal TRX movements. Use
java-tron FullNode for building/broadcast and SolidityNode for finalized reads;
HTTP JSON and protobuf gRPC are native surfaces. Account activation and dynamic
bandwidth/resource burn affect send cost. One TRX is 1e6 sun.
[Accounts](https://developers.tron.network/docs/account),
[transaction protocol](https://developers.tron.network/docs/tron-protocol-transaction),
[protobuf](https://github.com/tronprotocol/java-tron/blob/develop/Tron%20protobuf%20protocol%20document.md),
[exchange integration](https://developers.tron.network/docs/exchangewallet-integrate-with-the-tron-network).

### Polkadot

The stable identity is 32-byte AccountId, while SS58 is a network-presented
Base58/checksum encoding (Polkadot prefix 0). Default signing is sr25519;
MultiSignature also admits Ed25519 and ECDSA. Mnemonic and derivation behavior is
scheme/tooling convention. DOT uses 10 planck decimals. Runtime calls and signed
extensions are SCALE encoded and defined by metadata; signing commonly commits
nonce, mortality/checkpoint, genesis, specVersion and transactionVersion.
Refresh metadata at runtime upgrades. Index finalized block bodies plus
System.Events and require ExtrinsicSuccess; calls can be nested and failed
extrinsics still consume fee/nonce. Prefer standardized `chainHead_v1`, archive
and `transactionWatch_v1`, negotiating legacy RPC where necessary. GRANDPA
finality, existential deposit and locks/freezes are chain-owned semantics.
[Accounts](https://docs.polkadot.com/polkadot-protocol/basics/accounts/),
[extrinsic encoding](https://paritytech.github.io/polkadot-sdk/master/polkadot_sdk_docs/reference_docs/extrinsic_encoding/index.html),
[RPC specification](https://paritytech.github.io/json-rpc-interface-spec/api.html),
[consensus](https://docs.polkadot.com/polkadot-protocol/architecture/polkadot-chain/pos-consensus/).

### Cosmos Hub

Cosmos Hub is account-based Cosmos SDK/CometBFT. A user address is Bech32
`cosmos` over RIPEMD160(SHA256(compressed secp256k1 pubkey)); coin type 118 and
`m/44'/118'/0'/0/0` are wallet conventions. A Direct-mode transaction uses
protobuf TxBody, AuthInfo, SignDoc(chain ID, account number) and TxRaw; sequence
is the account nonce. Native send is `cosmos.bank.v1beta1.MsgSend`, with integer
`uatom` (1e6 per ATOM). Scan every committed block and execution result, decode
messages/events, and require result code zero. Comet RPC offers blocks, results,
search and WS; Cosmos gRPC offers accounts, bank queries, simulation and
broadcast. `SYNC` broadcast is only CheckTx. Committed BFT blocks have immediate
protocol finality, while checkpoints remain hash-qualified.
[Transactions](https://docs.cosmos.network/sdk/v0.50/learn/advanced/transactions),
[tx protobuf](https://raw.githubusercontent.com/cosmos/cosmos-sdk/v0.50.0/proto/cosmos/tx/v1beta1/tx.proto),
[Comet RPC](https://docs.cosmos.network/cometbft/v0.38/api-reference/rpc),
[Hub registry metadata](https://raw.githubusercontent.com/cosmos/chain-registry/master/cosmoshub/chain.json).

### NEAR

An address is an AccountId: named, 64-hex implicit, or newer supported forms;
it is not generally a key hash. Accounts can rotate and hold multiple FullAccess
or restricted keys. Ed25519 is the sensible first custody scheme; coin type 397
paths are wallet conventions. An unsigned Borsh transaction contains signer,
public key, per-access-key nonce, receiver, recent block hash and ordered actions.
Sign SHA-256(Borsh(Transaction)); native send is Transfer and one NEAR is 1e24
yoctoNEAR. Transactions create asynchronous cross-shard receipts: inclusion is
not execution. Require EXECUTED/FINAL outcome semantics and index finalized
blocks, chunks, receipts and outcomes via a nearcore indexer/data service; an
RPC-only top-level transaction scan misses contract-created transfers. HTTP
JSON-RPC provides access keys, blocks/chunks, gas, send and tx lookup.
[Account IDs](https://docs.near.org/protocol/accounts-contracts/account-id),
[transaction anatomy](https://docs.near.org/protocol/transactions/transaction-anatomy),
[execution](https://docs.near.org/protocol/transactions/transaction-execution),
[RPC](https://docs.near.org/api/rpc/transactions),
[indexers](https://docs.near.org/data-infrastructure/indexers).

### Sui

Sui is object-centric Move. A 32-byte address is Blake2b-256 of a scheme tag and
public key; Ed25519, secp256k1 and secp256r1 are supported. `suiprivkey` and HD
paths are wallet containers/conventions. A BCS TransactionData contains sender,
gas data and a programmable transaction block of object inputs and commands.
Sign its intent-separated digest. Native movement splits/merges/transfers SUI
objects (or newer balance form); stale object references and sponsored gas are
first-class. Index ordered checkpoints and authoritative transaction effects,
including all balance/object changes. Certified effects are deterministic
finality; checkpoint inclusion supplies canonical order. New work should use
gRPC for streaming/execution and GraphQL for flexible reads: Foundation
JSON-RPC retirement was scheduled in 2026.
[Cryptography](https://docs.sui.io/develop/cryptography/),
[transactions](https://docs.sui.io/develop/transactions/),
[objects](https://docs.sui.io/develop/objects/),
[API migration](https://docs.sui.io/references/sui-api).

### Aptos

Aptos uses 32-byte account addresses and Move resources. Ed25519 is the legacy
default, but authentication keys can rotate without changing address and modern
authenticator variants include other schemes. Canonical BCS RawTransaction
contains sender, sequence, payload, gas caps, expiration and chain ID; signing
is domain-separated, and multi-agent/fee-payer layouts differ. Native transfer
is an Aptos account entry function with integer octas (1e8 per APT). REST `/v1`
supports ledger/account reads, simulation, BCS submit, transactions and blocks;
Transaction Stream is the scalable ingestion path. Cursor by monotonically
increasing ledger version, inspect authoritative events/write sets, and require
committed `success=true`; committed failure can still charge gas. AptosBFT
committed versions have deterministic finality.
[REST transactions](https://aptos.dev/rest-api/operations/get_transactions),
[submit](https://aptos.dev/rest-api/operations/submit_transaction),
[transaction building](https://js-pro.aptos.dev/typescript/mutations/build-transaction),
[fungible assets](https://aptos.dev/build/smart-contracts/fungible-asset).

## SDK conclusions

Reusable abstractions should be deliberately smaller than protocol objects:

- `Address` is bytes plus chain/network/type validation; textual codecs stay in
  the concrete chain. Do not assume address maps to one key (NEAR/Aptos/XRPL),
  contains network identity (EVM/Solana/TRON), or is spendable (Solana PDA).
- `NetworkId`, atomic `Amount`, chain metadata, `Addresser`, key-scheme-tagged
  signing request/result, transport request, canonical checkpoint, submission
  state, and append-only observation identity are reusable.
- A signer signs chain-prepared exact bytes or digest and returns a tagged
  signature/public key. Chain code owns derivation of the signing payload,
  signer/address verification and signature insertion. `KeyPair` is custody
  input, not a universal wallet or transaction.
- Observation can share worker mechanics: durable cursor, backfill, live hint,
  canonicality/finality promotion, revision/undo, idempotency and health. The
  block/transaction/effect decoder remains concrete-chain code.
- Sending can share a state-machine vocabulary: built, signed, submitted,
  included/executed, finalized, expired/replaced/unknown. Builders, fee rules,
  nonce/input/object reservation and receipts remain chain-owned.

Do **not** universalize UTXO inputs, EVM nonce/envelopes, Cardano era CBOR,
Solana instruction messages, XRPL tags, Substrate metadata, Cosmos messages,
NEAR receipt graphs, Sui objects/PTBs, or Aptos authenticators. A universal
`Transaction { from, to, amount }` loses consensus-significant data and cannot
support correct indexing.

The dependency rule should therefore be: generic indexing/signing/base crates
contain no concrete-chain vocabulary or imports; each concrete chain may depend
on base, signing and indexing contracts; applications depend on and compose
both. Deleting one chain must delete its codecs, RPC profile, asset standards,
builders and interpreters without affecting generic crates.

## Suggested integration order

1. Finish Bitcoin and Ethereum as the reference UTXO/EVM implementations and
   prove the generic signing/indexing contracts without chain names.
2. Add one non-EVM account chain: Solana. This forces Ed25519, message-byte
   signing, expiring blockhashes and instruction/effect observation.
3. Add XRP Ledger. It tests tags, account reserves, validated finality, two key
   schemes and metadata-derived delivered amount.
4. Add Cosmos Hub and Polkadot. They test protobuf/SCALE, runtime metadata,
   deterministic BFT finality and event-driven movement interpretation.
5. Add Cardano. It validates that UTXO abstractions did not accidentally encode
   Bitcoin assumptions and introduces era-aware CBOR and explicit rollback.
6. Add TRON and NEAR. They test protobuf/TAPOS and asynchronous receipt graphs.
7. Add EVM profiles BSC, Avalanche and Polygon, then rollup profiles Arbitrum,
   OP and Base. Share codecs but keep finality/fees/system tx/bridges separate.
8. Add Litecoin, BCH and Dogecoin using distinct chain-owned signing/relay
   policies; do not call them Bitcoin configurations.
9. Add Aptos, then Sui. Aptos is closer to account/nonce systems; Sui requires
   the largest new object/PTB reservation and effect model.

For every adapter, definition of done is: official vectors for address and
signature codecs; offline deterministic build/sign tests; wrong-network/key and
overflow tests; a pinned-node integration suite; durable sequential backfill;
disconnect/reorg or finality tests; ambiguous broadcast reconciliation; and an
explicit unsupported-feature list (tokens, scripts, privacy layers, bridges,
multisig, hardware workflows) rather than silent partial parsing.
