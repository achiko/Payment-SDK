# Payment SDK demos

This package keeps the existing Ethereum examples and adds three Bitcoin
examples:

- `bitcoin-indexer` embeds one block-only Bitcoin Indexer Service instance.
- `bitcoin-wallet` directly uses the Bitcoin SDK to generate addresses, build
  one chain-native transaction, and sign its input locally.
- `bitcoin-payment-service` runs a complete offline, persistent native-Bitcoin
  deposit and collection workflow without an HTTP API or node.

Use `regtest` for development. The wallet example signs only a deterministic
fictional previous output using ephemeral in-memory keys; it performs no RPC,
preflight, or broadcast operation and cannot spend funds. The indexer example
performs read-only Bitcoin Core RPC calls, but it does create a local RocksDB
database and bind its configured HTTP and metrics listeners.

The demo binaries use only their process environment and never discover a
current-directory `.env`. Start from the safe template, keep the filled file
ignored by git, and explicitly source the copy you reviewed:

```bash
cp payment_sdk_demo/.env.example payment_sdk_demo/.env
chmod 600 payment_sdk_demo/.env
set -a
source payment_sdk_demo/.env
set +a
```

Never put Core authorization or bearer values in a checked-in file or a
command-line argument. The offline wallet and Payment Service examples need no
environment configuration, and this repository does not create or implicitly
load a populated `.env` for them.

## Bitcoin indexer sample

The sample requires a synchronized, unpruned Bitcoin Core 31.x node with
`txindex` enabled. Its configured genesis hash uses Bitcoin Core's conventional
lowercase 64-hex display order.

Required environment:

| Variable | Value |
|---|---|
| `DEMO_BITCOIN_EXPECTED_GENESIS_HASH` | Network block-zero hash, without `0x` |
| `DEMO_BITCOIN_CORE_AUTHORIZATION` | Complete Core `Authorization` value, such as a protected Basic value |
| `STRICT_AUTHENTICATION_MODE` | Exact `true` for strict or `false` for global-trusted mode |
| `DEMO_BITCOIN_IX_BEARER_TOKEN` | Non-empty bearer accepted by the sample IX API; strict mode only |

Optional environment:

| Variable | Default |
|---|---|
| `DEMO_BITCOIN_NETWORK` | `regtest` |
| `DEMO_BITCOIN_CORE_RPC_URL` | `http://127.0.0.1:18443` |
| `DEMO_BITCOIN_INDEXER_DATABASE_PATH` | `./tmp/bitcoin-indexer-demo-db` |
| `DEMO_BITCOIN_INDEXER_HTTP_BIND` | `127.0.0.1:18080` |
| `DEMO_BITCOIN_INDEXER_METRICS_BIND` | `127.0.0.1:19090` |
| `DEMO_BITCOIN_BOOTSTRAP_HEIGHT` | `0` |
| `DEMO_BITCOIN_CONFIRMATION_DEPTH` | `2` |
| `DEMO_BITCOIN_REORG_RETENTION` | `100` |
| `DEMO_BITCOIN_RPC_TIMEOUT_SECONDS` | `15` |
| `DEMO_BITCOIN_POLL_SECONDS` | `5` |
| `DEMO_BITCOIN_READY_MAX_LAG` | `2` |
| `DEMO_BITCOIN_READY_MAX_AGE_SECONDS` | `30` |

Run it from the repository root after explicitly exporting the required
environment. The binaries never discover or auto-load a `.env` file:

```bash
set -a
source payment_sdk_demo/.env
set +a
cargo run --locked -p payment_sdk_demo --bin bitcoin-indexer
```

The checked-in template explicitly selects global-trusted mode. Set
`STRICT_AUTHENTICATION_MODE=true` and provide the bearer to authenticate the
API at `127.0.0.1:18080`. Metrics remain on the separate loopback listener at
`127.0.0.1:19090` in either mode. Bitcoin Core authentication is always
required and is never disabled by the service authentication mode.

## Bitcoin wallet sample

This sample mirrors the direct lifecycle in `src/main.rs` without an HTTP API:

1. Create an ephemeral local secp256k1 signer.
2. Generate source and recipient regtest P2WPKH addresses.
3. Validate a deterministic fictional previous output against the source
   address script.
4. Build a version-2, RBF-enabled native Bitcoin transaction using integer
   satoshis and an explicit `1000 sat/kvB` fee rate.
5. Compute the Bitcoin sighash, sign the input, verify the returned signature,
   and assemble the witness through the Bitcoin transaction codec.

It prints public addresses, the fictional outpoint, fee, transaction ID, and
virtual size. It never prints private keys or raw signed transaction bytes.

Run it from the repository root:

```bash
cargo run --locked -p payment_sdk_demo --bin bitcoin-wallet
```

The fictional outpoint does not exist, so the resulting signed transaction is
an offline construction/signing demonstration and must not be broadcast.

## Bitcoin Payment Service sample

This direct sample exercises the PS-owned parts of a native-Bitcoin collection
without an HTTP API, environment configuration, Bitcoin Core node, or
broadcast operation:

1. Open a temporary RocksDB-backed `PersistentPaymentRepository` bound to
   `bitcoin/regtest` and one explicit policy identity.
2. Persist two opaque users owned by the same exchange principal.
3. Generate two P2WPKH deposit addresses with ephemeral local custody, then
   atomically persist each deposit and its opening zero-balance ledger row.
4. Mirror one deterministic fictional confirmed UTXO per deposit and append
   the resulting absolute confirmed ledger snapshot.
5. Persist a multi-user collection job and atomically reserve both exact
   outpoints with bounded, versioned evidence.
6. Build and sign one P2WPKH drain transaction with one master output and no
   change, allocate its shared fee proportionally with deterministic
   largest-remainder rounding, and persist the txid, exact signed bytes, and
   per-deposit allocations together.
7. Stop before broadcast. The sample has no node client or broadcast path.

Run it from the repository root:

```bash
cargo run --locked -p payment_sdk_demo --bin bitcoin-payment-service
```

Only public addresses and safe transaction summaries are printed. Key
locators, private material, and raw signed transaction bytes are never printed.
The temporary database is deleted after the process exits, and the fictional
outpoints cannot spend funds. This is structural/offline coverage; the
explicitly opt-in Bitcoin Core 31 regtest acceptance procedure remains a
separate step.

For the full service configuration, watch contract, and the explicitly opt-in
Core 31 regtest acceptance procedure, see
[`../docs/BITCOIN_SERVICES.md`](../docs/BITCOIN_SERVICES.md).
