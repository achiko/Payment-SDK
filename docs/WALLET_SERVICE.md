# Wallet Service HTTP APIs

`apps/wallet` is the stateless Wallet Service (WS) composition root. It selects
one configured Ethereum or Bitcoin network, chain-owned RPC/index adapters, and
the chain-independent remote custody client. It does not open a database and
does not own users, deposits, watches, jobs, reservations, retries, accounting,
or multi-leg collection sequencing.

For direct in-process Rust integration with every asynchronous
`WalletService` operation, use the step-by-step
[`Wallet Service Rust library guide`](./WALLET_SERVICE_USAGE.md).
For the Bitcoin Core 31 prerequisites, exact CLI/environment names, IX API,
selected-UTXO flow, and real-node acceptance status, use the focused
[`Bitcoin services runbook`](./BITCOIN_SERVICES.md).

## Runtime safety

- `STRICT_AUTHENTICATION_MODE` is required and accepts exactly `true` or
  `false`. In strict mode every operation route requires the configured bearer;
  in global-trusted mode Authorization is ignored and every reachable caller
  receives the same authority. Global-trusted mode is not identity isolation.
- `/health/live` is unauthenticated and detail-free. `/health/ready` is also
  public and adds only the sanitized `authentication_mode` posture.
- Readiness is enabled only after the RPC reports the configured chain ID and
  custody reports the required signing capabilities and availability. Bitcoin
  additionally verifies Core 31, network, genesis, pruning, synchronization,
  transaction-index readiness, authenticated IX readiness/network status, and
  the IX checkpoint against its own Core node.
- After startup, WS periodically rechecks custody availability and its reported
  authentication mode. Bitcoin also rechecks IX readiness, network, and
  authentication mode. `/health/ready` clears on a failed check and recovers
  when those dependencies again report the configured posture. Ethereum RPC
  and Bitcoin Core operation failures remain separate from this monitor.
- Readiness is disabled before SIGINT/SIGTERM graceful drain.
- Plain HTTP RPC and custody endpoints are accepted only on loopback. External
  endpoints require HTTPS. A non-loopback WS listener requires an explicit
  trusted-upstream TLS assertion.
- Request bodies, RPC/custody responses, timeouts, retries, gas, and fee values
  are bounded by configuration.
- RPC URLs, RPC header values, bearer tokens, custody URLs, signed envelopes,
  and custody credentials are redacted from `Debug` and logs.
- WS verifies that repo-owned IX and repository-mode-matched custody report the
  same mode and fails startup on missing or mismatched posture. Setting
  `WS_CUSTODY_AUTHENTICATION_POLICY=independent_strict` instead requires a
  custody bearer and a reported strict posture regardless of the repo-wide
  mode. Bitcoin Core and other vendor authentication remain independent.

## Encoding rules

The JSON contract is strict: unknown fields are rejected. Ethereum addresses
and transaction IDs use lowercase `0x`-prefixed hexadecimal. Amounts use
canonical unsigned U256 decimal strings. Signed envelopes use lowercase
`0x`-prefixed hexadecimal. Key locators remain opaque and use either:

```json
{"kind":"identifier","value":"opaque-backend-handle"}
```

or:

```json
{"kind":"derivation_path","children":[{"index":7,"hardened":true}]}
```

Assets use `{"kind":"native"}` or
`{"kind":"erc20","token":"0x..."}`.

Bitcoin addresses use their canonical network-specific display. Bitcoin txids
and block hashes use lowercase Core display order without a `0x` prefix.
Satoshi amounts, output indexes, virtual sizes, and satoshi-per-kvB fee rates
use canonical unsigned decimal strings. Scripts and exact raw transactions use
lowercase `0x`-prefixed hexadecimal.

## Ethereum routes

All routes below use `POST` and `application/json`.

| Path | Semantics |
|---|---|
| `/v1/ethereum/addresses` | Provision one key and return only its canonical address and opaque key locator. Request fields are `operation_id`, `asset`, and `key_purpose`. No birthday is returned; PS owns birthday/watch orchestration. |
| `/v1/ethereum/balances` | Read factual native or ERC-20 confirmed, pending, and spendable amounts. |
| `/v1/ethereum/transfers/native/sign` | Build and sign one EIP-1559 native transfer without broadcast. |
| `/v1/ethereum/transfers/erc20/sign` | Build canonical ERC-20 `transfer` calldata and sign without broadcast. |
| `/v1/ethereum/collections/requirements` | Report factual native-gas prerequisites for one native or token collection request. |
| `/v1/ethereum/collections/native/sign` | Prepare and sign one native sweep without broadcast; returns attribution. |
| `/v1/ethereum/collections/erc20/sign` | Prepare and sign one token sweep without broadcast; returns attribution. |
| `/v1/ethereum/transactions/broadcast` | Accept `expected_transaction_id` and the exact `signed_envelope`, verify their Keccak-256 relationship, broadcast unchanged bytes, and require the provider to return the same ID. |
| `/v1/ethereum/receipts` | Read the current factual receipt for one canonical transaction ID. |

Signing responses contain `transaction_id` and `signed_envelope`. Collection
signing also returns `attribution`. These calls do not broadcast. PS can persist
the exact envelope before calling the broadcast route, making response loss and
retry recovery explicit without giving WS durable workflow ownership.

Requests that already define an `operation_id` keep it mandatory in strict
mode. Global-trusted direct callers may omit it; WS generates UUIDv7 identity
before any custody/RPC effect and returns it in the response. Caller-supplied
IDs preserve the previous response shape. Durable PS workflows continue to
provide deterministic child identities.

Error responses use stable JSON fields: `code`, `message`, `retryable`, and an
opaque `request_id`. Transport, RPC, and custody internals are sanitized.

## Bitcoin routes

All routes use `POST` and strict `application/json` unless stated otherwise.

| Path | Semantics |
|---|---|
| `/v1/bitcoin/addresses` | Provision a P2WPKH or P2TR key and return the canonical network address plus opaque key locator. |
| `/v1/bitcoin/balances` | Read confirmed, pending, and gross confirmation/maturity-qualified spendable satoshis from IX-owned canonical UTXOs. |
| `/v1/bitcoin/transfers/sign` | Validate an exact caller-selected set of outpoints, values, scripts, addresses, and key locators; build and sign without broadcast. |
| `/v1/bitcoin/collections/requirements` | Report source addresses with no confirmation-qualified spendable outputs. |
| `/v1/bitcoin/collections/sign` | Validate exact inputs grouped by source, sign one drain transaction, and return gross input attribution per source. |
| `/v1/bitcoin/transactions/broadcast` | Accept an expected txid and exact raw transaction, verify their consensus relationship, preflight, submit unchanged bytes, and require Core to return the same txid. |
| `/v1/bitcoin/receipts` | Read Core's current lookup for one canonical txid; a known zero-confirmation transaction has no block reference, while RPC not-found returns `null`. This is not mempool lifecycle tracking. |

Bitcoin signing responses contain the txid, exact raw transaction, selected
outpoints, outputs, fee, virtual size, and—when collecting—gross attribution.
WS validates selected inputs against the current IX projection but never
reserves them. PS owns the atomic reservation and must persist the exact signed
bytes before calling broadcast.

A broadcast timeout or lost response is ambiguous. Durable callers query the
receipt/mempool and retry only the same persisted txid and exact bytes; they do
not automatically sign a conflicting replacement. Reverse proxies and capture
tooling must not log sign/broadcast bodies or secret-bearing CLI arguments.

Each IX UTXO page identifies its projection generation, monotonic revision, and
canonical height/hash checkpoint. WS binds that full snapshot across every page
and address in a request, verifies the checkpoint hash against its own Core
node, and fails retryably if canonical state moves while it is reading. It never
signs a selection stitched across a commit, reorg, backfill, or rebuild.
Spendability requires the configured minimum confirmations and, for coinbase
outputs, at least 100 confirmations. The reported value does not subtract PS
reservations, fee reserve, dust, or input spending cost and is therefore not a
PS available-to-withdraw or sweepable balance.

## Serve configuration

Run `wallet-worker serve --help` for the legacy Ethereum command and
`wallet-worker bitcoin serve --help` for Bitcoin. The required inputs select:

- Ethereum chain ID, RPC URL, optional repeatable `name=value` RPC headers,
  request timeout/retry/response bounds, gas margin, and gas/fee ceilings;
- remote custody URL, bearer credential, timeouts, retries, and response bound;
- WS bind address, WS bearer credential, upstream TLS assertion, request-body
  bound, `WS_METRICS_BIND` (default `127.0.0.1:9092`, loopback only), and
  graceful-shutdown deadline.

`STRICT_AUTHENTICATION_MODE` controls the repo-owned bearer fields above.
They are required only for `true`; for `false` they are ignored without being
logged. Node/RPC and vendor custody credentials are never disabled by this
setting. The readiness/status output, startup log, and
`payment_sdk_strict_authentication_mode{service="wallet"}` metric expose the
selected posture.

Bitcoin additionally requires the network and conventional genesis hash,
authenticated Core and IX endpoints, an explicit confirmation threshold, fee
estimation target, and maximum broadcast fee rate. P2TR readiness requires
Schnorr digest signing plus the `secp256k1_add` public tweak capability.
The maximum fee-rate setting may not exceed Bitcoin Core's 1 BTC/kvB
(`100000000` sat/kvB) RPC limit.

The required Bitcoin-specific deployment variables are
`WS_BITCOIN_NETWORK`, `WS_BITCOIN_EXPECTED_GENESIS_HASH`,
`WS_BITCOIN_CORE_RPC_URL`, `WS_BITCOIN_IX_URL`,
`WS_BITCOIN_MINIMUM_CONFIRMATIONS`, and
`WS_BITCOIN_MAX_SATOSHIS_PER_KVB`, together with the shared authentication,
custody, and WS settings. `WS_BITCOIN_IX_BEARER_TOKEN` is additionally required
in strict mode. Core requires exactly one Authorization header after combining
`WS_BITCOIN_CORE_RPC_HEADERS` with the optional dedicated
`WS_BITCOIN_CORE_RPC_AUTHORIZATION` value. Embedded URL credentials, query
strings, fragments, remote plaintext endpoints, injected header newlines, and
duplicate header names fail closed.

The checked-in loopback custody adapter is ephemeral and destroys every private
key on restart. It is suitable only for disposable local/regtest use; follow the
[`manual Core 31 regtest acceptance guide`](./manual-bitcoin-regtest/README.md)
without restarting custody after generating addresses.

No database path exists by design.
