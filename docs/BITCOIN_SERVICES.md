# Bitcoin service composition

## Implemented boundary

`sdk/chains/bitcoin` owns Bitcoin addresses, networks, RPC methods, UTXOs,
transaction construction, signing, broadcast validation, and block
interpretation. `apps/indexer` composes its source/interpreter with the generic
RocksDB repository and HTTP router.

The runnable command is:

```bash
mac cargo run --locked -p indexer-worker -- bitcoin serve --help
```

Indexer maintenance also supports the Bitcoin-scoped backup, rebuild,
rebuild-abort, and cleanup commands. Run maintenance only while the relevant
database is not owned by a serving process.

## Generic Indexer API

The router is shared by all chains; chain and network are path parameters:

```text
GET    /v1/scopes/{chain}/{network}/status
POST   /v1/scopes/{chain}/{network}/watches
DELETE /v1/scopes/{chain}/{network}/watches/{watch_id}
GET    /v1/scopes/{chain}/{network}/transactions/{transaction}
GET    /v1/scopes/{chain}/{network}/addresses/{address}/transactions
GET    /v1/scopes/{chain}/{network}/addresses/{address}/outputs
GET    /v1/scopes/{chain}/{network}/events
```

Watch registration always requires a caller-owned idempotency key. Output
pages contain chain-neutral asset, exact `Decimal` amount, and opaque evidence;
`chain_bitcoin::IndexUtxos` validates and converts that evidence to Bitcoin
outpoints and scripts for wallet construction.

## Wallet and payments

Bitcoin wallet providers implement the protocol-neutral `wallets::Wallet`
capabilities. A Bitcoin builder additionally exposes UTXO input policy and
change through `BuilderCast::utxo`. Preparation produces a durable
`SignedTransaction`; its broadcaster accepts only the matching Bitcoin kind,
ID, and exact envelope.

`apps/wallet` supplies a runtime whose binary can compose a Bitcoin wallet,
focused Bitcoin Core capabilities, and remote Indexer output/history readers
from complete `WS_BITCOIN_*` configuration. With no chain configuration it is
live but not ready.
`apps/api` supplies both an injected runtime and a payment binary
that composes a configured Bitcoin wallet, Bitcoin Core RPC, remote indexing,
and payment RocksDB. It may also select one finite Bitcoin-native deposit scope,
compose address/watch/observation/history, derive a UTXO collection from stable
IDs plus durable ledger/canonical output state, and execute it. Keys come from
named environment variables, bearer authentication is mandatory, and production
custody/TLS remain external. The payment layer persists signed
bytes, registers the transaction watch, broadcasts, and then reconciles
confirmation/reorg revisions through indexing.

## Evidence boundary

Deterministic tests cover Bitcoin address parsing, transaction construction and
signing, output-query validation, index interpretation, HTTP behavior, RocksDB
restart, and payment retry ordering. The current repository does not claim a
funded live broadcast or a complete production Bitcoin payment deployment.

The composed loopback acceptance covers `Runtime::build`, real Bitcoin Indexer
and payment RocksDB instances, deposit watch/observation, snapshot-derived
two-input planning, replay/conflict, restart, signing, and exact broadcast
capture:

```bash
mac cargo test --locked -p system-tests --test collection_runtime
```
