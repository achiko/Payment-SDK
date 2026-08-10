# Bitcoin Wallet, Indexer, and Payment Service: block-only v1

Status: Bitcoin Wallet, Indexer, and Payment Service are implemented in source
with deterministic coverage. This is not real-node acceptance evidence. The
composed disposable Bitcoin Core 31 regtest scenario remains pending because a
Core 31 binary is not available in the current development environment. No
funded-network operation is part of this runbook. The IX/WS opt-in procedure is
checked in separately as the
[`manual Core 31 regtest acceptance guide`](./manual-bitcoin-regtest/README.md);
the existence of that guide is not evidence that it passed.

This document is the operational contract for the implemented Bitcoin modes of
`indexer-worker` (IX), `wallet-worker` (WS), and `payment-api` (PS). The
canonical cross-chain requirements remain in
[`SYSTEM_REQUIREMENTS.md`](./SYSTEM_REQUIREMENTS.md).
Detailed Bitcoin PS work and acceptance status are tracked in
[`BITCOIN_PAYMENT_SERVICE_IMPLEMENTATION_PLAN.md`](../TODO/BITCOIN_PAYMENT_SERVICE_IMPLEMENTATION_PLAN.md).

## V1 boundary

Bitcoin v1 is deliberately block-only. IX indexes canonical blocks, appends
included/confirmed/reorg revisions, materializes watched canonical UTXOs, and
publishes replayable events. It does not index the mempool or claim dropped,
replacement, RBF, CPFP, or zero-confirmation lifecycle coverage.

Each process owns one configured network. Supported names are `mainnet`,
`testnet3`, `testnet4`, `signet`, and `regtest`. P2WPKH and P2TR key-path
addresses are the only wallet-owned spend types in v1. Values are integer
satoshis; fee rates are integer satoshis per kvB.

| Component | Owns |
|---|---|
| Bitcoin Core 31 | Node identity and canonical blocks; conservative fee estimates; mempool-policy preflight; submission; current transaction receipt facts |
| IX | Watches, canonical height/hash checkpoint, observations and revisions, event cursor, reorg undo, canonical watched UTXO projection, RocksDB generations and rebuilds |
| WS | Stateless address generation, IX-backed balances, exact-input validation, construction/signing, preflight/broadcast, and receipt lookup |
| Custody | Opaque key provisioning plus ECDSA, Schnorr, and public-tweak operations; never IX or WS state |
| Bitcoin PS | Deposit-to-key mapping, same-principal cross-user jobs, atomic exact-outpoint reservations, retained exact signed bytes, deterministic fee allocation, accounting, and collection workflow |

Calling WS directly still does not create a durable reservation or collection
workflow. The implemented PS mode owns that state; the decisions below do not
move it into WS or IX. Deterministic coverage does not count as live Core 31 or
production acceptance.

## Implemented Bitcoin PS v1 boundary

- One `payment-api` process and exclusive PS RocksDB path own one native-BTC
  network, policy identity, IX feed, and exchange-principal namespace. Bitcoin
  and Ethereum or multiple Bitcoin networks never share one PS database.
- Deposit creation uses the policy-selected P2WPKH or P2TR address kind and
  preserves `AwaitingWatch`: PS returns no address until the Bitcoin IX watch is
  durable.
- One explicit batch may contain deposits for different users only when every
  durable user belongs to the same authenticated exchange principal. A batch
  cannot mix principals, networks, assets, policies, or master destinations.
- PS reads one fenced IX UTXO snapshot, selects every eligible output for each
  requested deposit in canonical `(txid, vout)` order, atomically reserves the
  complete set, and drains it to exactly one policy master output without
  change. Any overlapping outpoint makes the whole reservation retry.
- The actual fee is allocated proportionally by gross input using integer
  largest remainder. Equal remainders use canonical deposit-ID order. Fee
  shares sum to the exact fee and per-deposit master credits sum to the one
  master output.
- The mandatory policy has no permissive financial defaults. It specifies
  deposit address kind/TTL, master destination, minimum collection amount,
  minimum spend confirmations, requested and maximum sat/kvB, maximum absolute
  fee, and maximum deposits/inputs per batch.
- PS independently verifies the returned txid, exact inputs, one master output,
  gross attribution, fee, vsize, fee ceilings, and dust/value bounds. It then
  persists exact signed bytes, reservations, and all allocations before
  broadcast.
- UTXO-batch v1 has no generic failure or reservation-release path. An unsigned
  required reservation stays active and retryable; cancellation/release needs a
  future explicit safe design.
- Once durably signed, exact bytes, txid, reservations, allocations, and the IX
  watch are retained indefinitely in Bitcoin PS v1. Confirmation, time, or IX's
  separately configured rollback-retention boundary does not release them. A
  reorg authorizes only identical-byte rebroadcast and same-txid re-inclusion;
  it does not authorize a newly signed transaction or outpoint reuse.
- Retained per-deposit ownership means one deposit can participate in only one
  Bitcoin collection aggregate in v1. A later payment remains watched and is
  reflected in the ledger, but PS cannot create a second collection for it.
  Multi-collection-per-deposit support, archival, and space reclamation are
  future work.
- Bitcoin PS remains block-only. It has no mempool/drop/conflict/replacement
  feed and no PS-generated RBF replacement, CPFP, fee bumping, PSBT, multisig,
  or hardware-wallet transaction workflow. Time or a missing receipt is not
  evidence that an outpoint is reusable.

The repository commits a batch collection's leg/reservation transition,
all affected absolute ledger rows and observation indexes, reconciliation
cases, and projection cursor in one physical PS write. A batch cannot be
confirmed for only a subset of its source deposits.

## Bitcoin Core 31 prerequisite

IX and WS connect directly to one authenticated Bitcoin Core 31.x JSON-RPC
endpoint; PS reaches chain operations only through those services. IX/WS
startup fails closed unless Core reports all of the following:

- the configured network and conventional block-zero hash;
- version `31.x` (numeric Core version at least `310000` and below `320000`);
- an unpruned node;
- `initialblockdownload=false` and equal local block/header heights;
- `txindex` enabled, synchronized, and at the current canonical height.

Configure the node with full block history and transaction indexing, then wait
for Core to report local chain/index synchronization before starting IX or WS.
The services do not fall back to Core wallet scans, `scantxoutset`, a pruned
history, or a partially synchronized index.

These checks attest only to the configured node's internally reported state.
An isolated or stale node can report `initialblockdownload=false` and equal
block/header heights. Non-regtest operators must independently monitor peer
connectivity, tip age/chainwork, and upstream diversity; IX lag is measured
against the same Core node and is not independent network-freshness evidence.

The configured genesis value is exactly 64 hexadecimal characters in Bitcoin
Core display byte order, without a `0x` prefix. API txids and block hashes use
the same conventional lowercase display order. Internal hash byte order is not
an API encoding.

Core RPC URLs must not contain user information, passwords, query strings, or
fragments. Plain `http://` is restricted to `localhost` or a loopback IP;
non-loopback endpoints require `https://`. Header inputs use `name=value`,
reject CR/LF injection and case-insensitive duplicate names, and redact values
from diagnostics. Both Bitcoin IX and WS require exactly one Core
`Authorization` header. Supply the value through protected deployment
configuration; never place it in a URL, logs, or checked-in files. Although the
CLI accepts header flags, do not put authorization or bearer values on a command
line where shell history or process inspection can expose them. Prefer a
protected environment/secret-file injection mechanism. Reverse proxies and
capture tooling must not log WS signing request/response bodies because those
responses contain exact signed transactions.

## Chain-specific commands

Bitcoin uses a nested CLI namespace so existing top-level Ethereum commands
retain their behavior.

| Process | Bitcoin command |
|---|---|
| WS HTTP runtime | `wallet-worker bitcoin serve` |
| IX HTTP/sync runtime | `indexer-worker bitcoin serve` |
| Consistent IX backup | `indexer-worker bitcoin backup` |
| IX schema migration | `indexer-worker bitcoin migrate schema` |
| IX confirmation/retention migration | `indexer-worker bitcoin migrate policy` |
| IX staged generation rebuild | `indexer-worker bitcoin rebuild` |
| Abort an unpublished rebuild | `indexer-worker bitcoin rebuild-abort` |
| Remove a verified inactive generation | `indexer-worker bitcoin cleanup` |
| PS HTTP runtime and workers | `payment-api bitcoin serve` |
| Consistent PS backup | `payment-api bitcoin backup` |
| PS schema/policy migration | `payment-api bitcoin migrate` |
| Retry pending deposit watches | `payment-api bitcoin reconcile-watches` |
| Bounded IX event ingestion | `payment-api bitcoin ingest-events` |
| Ingestion/projection status | `payment-api bitcoin projection-status` |

These commands are safe ways to inspect the exact checked-in CLI without
starting a service or touching funds:

```bash
cargo run --locked -p indexer-worker -- bitcoin serve --help
cargo run --locked -p indexer-worker -- bitcoin migrate policy --help
cargo run --locked -p wallet-worker -- bitcoin serve --help
cargo run --locked -p payment-api -- bitcoin serve --help
cargo run --locked -p payment-api -- bitcoin migrate --help
```

Run IX maintenance commands only while IX is stopped, and PS backup/migration
only while PS is stopped. IX backup, migration, rebuild, abort, and cleanup
preserve the generic generation/audit rules documented in
[`INDEXER_SERVICE.md`](./INDEXER_SERVICE.md). PS maintenance never opens the IX
database; every PS command points only at its exclusive PS path.

## IX configuration

Bitcoin confirmation and rollback policy have no defaults. Every Bitcoin
deployment supplies both values explicitly.

| Environment variable | Requirement |
|---|---|
| `IX_DATABASE_PATH` | Exclusive RocksDB directory for this Bitcoin scope |
| `IX_NETWORK` | One supported canonical Bitcoin network name |
| `IX_BOOTSTRAP_HEIGHT` | Earliest supported watch birthday |
| `IX_CONFIRMATION_DEPTH` | Explicit nonzero depth at which IX appends `Confirmed` |
| `IX_REORG_RETENTION` | Explicit nonzero exact-rollback window |
| `IX_EXPECTED_GENESIS_HASH` | Conventional 64-hex Core block-zero hash, no prefix |
| `IX_RPC_HTTP_URL` | Authenticated Core 31 endpoint subject to the URL rules above |
| `IX_RPC_HEADERS` | Comma-delimited `name=value` headers containing exactly one `authorization` entry |
| `IX_RPC_TIMEOUT_SECONDS` | Per-request Core timeout in seconds; defaults to 15 |
| `IX_RPC_MAX_RESPONSE_BYTES` | Per-response Core bound; defaults to 256 MiB. IX ingests verbosity-2 blocks, resolves each external transaction once with at most four Core calls in flight, and retains only compact value/address prevout facts rather than amplified verbosity-3 scripts |
| `IX_BEARER_TOKEN` | Required non-empty bearer token for every Bitcoin `/v1` route, including loopback deployments |

`IX_HTTP_BIND`, `IX_METRICS_BIND`, `IX_POLL_SECONDS`, `IX_READY_MAX_LAG`, and
`IX_READY_MAX_AGE_SECONDS` retain the general IX defaults. The metrics listener
must remain loopback. A non-loopback API bind additionally requires
`IX_UPSTREAM_TLS_TERMINATED=true`; TLS must terminate at a trusted upstream.
Programmatic `BitcoinIndexerServiceConfig` composition must likewise set one
Core authorization header and the IX bearer token before validation.

IX is ready only when its repository phase is `ready`, canonical lag is within
the configured maximum, and recent reconciliation is within the configured
age. Startup Core checks are necessary but do not make an out-of-date IX ready.

## WS configuration

| Environment variable | Requirement |
|---|---|
| `WS_BITCOIN_NETWORK` | One supported canonical Bitcoin network name |
| `WS_BITCOIN_EXPECTED_GENESIS_HASH` | Conventional 64-hex Core block-zero hash, no prefix |
| `WS_BITCOIN_CORE_RPC_URL` | Core 31 endpoint subject to the URL rules above |
| `WS_BITCOIN_CORE_RPC_HEADERS` | Optional comma-delimited repeatable Core headers |
| `WS_BITCOIN_CORE_RPC_AUTHORIZATION` | Optional dedicated Authorization value; after combining header inputs, exactly one Authorization header is required |
| `WS_BITCOIN_IX_URL` | Bitcoin IX base URL; loopback HTTP or non-loopback HTTPS |
| `WS_BITCOIN_IX_HEADERS` | Optional comma-delimited repeatable IX headers; must not duplicate Authorization |
| `WS_BITCOIN_IX_BEARER_TOKEN` | Required bearer value accepted by Bitcoin IX |
| `WS_BITCOIN_MINIMUM_CONFIRMATIONS` | Explicit nonzero spend policy; no default |
| `WS_BITCOIN_MAX_SATOSHIS_PER_KVB` | Explicit nonzero signing and broadcast fee-rate ceiling, at most Core's `100000000` sat/kvB limit; no default |
| `WS_CUSTODY_URL` / `WS_CUSTODY_BEARER_TOKEN` | Authenticated remote custody endpoint and credential |
| `WS_BEARER_TOKEN` | Required bearer token for all WS operation routes |

`WS_BITCOIN_IX_BEARER_TOKEN` is a copy of the secret accepted by the IX
process, not a reference that WS can resolve from another process's
environment. When using separate private IX and WS environment files, place the
same protected value in both files explicitly.

Core header values may be supplied either with repeatable
`--core-rpc-header`/`WS_BITCOIN_CORE_RPC_HEADERS` entries or the dedicated
`--core-rpc-authorization`/`WS_BITCOIN_CORE_RPC_AUTHORIZATION` setting. Missing
authorization and duplicates across the two forms both fail closed.

The fee-estimation target defaults to six blocks. Core request limits, IX page
and response limits, custody limits, maximum input/output counts, HTTP bind,
request-body size, and shutdown grace are bounded options shown by `--help`.
WS requires a trusted-upstream TLS assertion for a non-loopback listener.

> **Disposable custody warning:** the checked-in `apps/custody` process is for
> loopback regtest/local development only. Its private keys exist only in
> memory and are destroyed on restart, so previously returned key locators can
> no longer sign. Never fund its addresses on a public network, and keep the
> same custody process alive for the manual restart/replay scenario. Production
> deployments require durable external custody.

WS becomes ready only after it has validated Core, received a successful IX
`/health/ready` probe plus an exact authenticated `bitcoin`/network `ready`
status, and confirmed that
the IX checkpoint hash is canonical on its own Core node. Every later UTXO read
repeats this checkpoint comparison. WS also confirms that custody is available
with secp256k1 digest ECDSA, Schnorr, and
`secp256k1_add` public-tweak support. The tweak is required for BIP341 P2TR
output-key derivation; private tweak material never crosses the signer
boundary.

The current WS health flag is latched after those startup probes and is cleared
only during graceful shutdown. A later Core, IX, or custody outage may therefore
leave `/health/ready` returning success. UTXO operations still re-read the IX
snapshot and compare its checkpoint with Core, but operators must monitor all
dependencies and functional request failures separately; `/health/ready` alone
is not continuous dependency health.

The tweak field extends custody capability discovery. During a rolling upgrade,
deploy WS/remote-signer clients that accept the field before deploying custody
servers that emit it; older clients reject unknown capability fields. A
server-first mixed-version rollout is unsupported.

Commented Bitcoin settings are available in [`.env.example`](../.env.example).
They do not alter the Ethereum local-stack defaults.
The complete disposable real-node procedure is in the
[`manual Core 31 regtest acceptance guide`](./manual-bitcoin-regtest/README.md).

## PS configuration and strict policy

Bitcoin PS talks only to authenticated IX and WS endpoints; it does not open the
IX database or call Core directly. Its endpoint/network/policy scope checks fail
closed before the HTTP listener starts. Bitcoin requires IX bearer
authentication even on loopback.

| Environment variable | Requirement |
|---|---|
| `PS_DATABASE_PATH` | Exclusive RocksDB directory for one Bitcoin PS scope; never the IX path |
| `PS_POLICY_PATH` | Regular file containing the strict Bitcoin policy below; maximum 1 MiB |
| `PS_INDEXER_URL` | Bitcoin IX origin; loopback HTTP or non-loopback HTTPS, with no credentials/path/query/fragment |
| `PS_INDEXER_NETWORK` | Canonical name matching the policy: `mainnet`, `testnet3`, `testnet4`, `signet`, or `regtest` |
| `PS_INDEXER_BEARER_TOKEN` | Required Bitcoin IX bearer token, including on loopback |
| `PS_WALLET_URL` / `PS_WALLET_BEARER_TOKEN` | Authenticated Bitcoin WS origin and required bearer token |
| `PS_API_BEARER_TOKEN` / `PS_ADMIN_BEARER_TOKEN` | Required, distinct ordinary and administrator credentials |
| `PS_HTTP_BIND` | API bind; defaults to `127.0.0.1:8081`. A non-loopback bind requires `PS_TLS_TERMINATED_UPSTREAM=true` |
| `PS_METRICS_BIND` | Loopback-only metrics bind; defaults to `127.0.0.1:9091` |

The existing bounded retry, timeout, worker interval/page size, body/page limit,
and shutdown settings are shared with Ethereum. Inspect them with
`payment-api bitcoin serve --help`. Backup and migration additionally use
`PS_BACKUP_PATH`; migration requires `PS_MIGRATION_NETWORK` to exactly match
the policy.

Every field in the policy is mandatory and unknown fields are rejected. Atomic
money and fee values are canonical unsigned decimal strings, not JSON numbers;
the other counts are JSON integers. This is a syntax example only: replace the
regtest master address and review every bound before use.

```json
{
  "version": 1,
  "scope": {"chain": "bitcoin", "network": "regtest"},
  "deposit_address_kind": "p2wpkh",
  "deposit_ttl_seconds": 3600,
  "master_destination": "bcrt1qtwxw3vnj3f29szvhvr84k0aekcrhh9cla5nxa0",
  "minimum_collection_satoshis": "10000",
  "minimum_spend_confirmations": 6,
  "requested_satoshis_per_kvb": "1000",
  "maximum_satoshis_per_kvb": "5000",
  "maximum_absolute_fee_satoshis": "50000",
  "maximum_deposits": 20,
  "maximum_inputs": 200
}
```

Validation requires `version > 0`, a canonical network-matched master address,
`p2wpkh` or `p2tr`, positive TTL/minimum/confirmation/fee values, requested fee
rate no greater than maximum, and maximum fee rate no greater than Core's
`100000000 sat/kvB` ceiling. `maximum_deposits` is `1..=1000`;
`maximum_inputs` is `1..=16384` and must be at least the deposit limit. The
SHA-256 digest of the exact policy file bytes and its version bind the PS
database. Changing a policy requires the explicit migration path; do not edit
the active file in place while serving.

Commented PS settings are included in [`.env.example`](../.env.example). That
file is a template only. The direct offline `bitcoin-wallet` and
`bitcoin-payment-service` demos require no `.env`; only the Core-backed indexer
demo needs environment configuration.

## IX HTTP surface

Every `/v1` request requires `Authorization: Bearer ...`. Health endpoints are
unauthenticated and sanitized. The metrics route is served only by the separate
loopback metrics listener.

| Method and path | Semantics |
|---|---|
| `GET /v1/scopes/bitcoin/{network}/status` | Scope, phase, checkpoint, observed tip, configured confirmation depth, and recovery reason |
| `POST /v1/scopes/bitcoin/{network}/watches` | Idempotently register an address or transaction watch from an unsigned-decimal start height |
| `DELETE /v1/scopes/bitcoin/{network}/watches/{watch_id}` | Soft-deactivate a watch without deleting history |
| `GET /v1/scopes/bitcoin/{network}/transactions/{txid}` | Latest revision of one observed transaction |
| `GET /v1/scopes/bitcoin/{network}/addresses/{address}/transactions?after=...&limit=...` | Exclusive-after transaction page for one canonical address |
| `GET /v1/scopes/bitcoin/{network}/addresses/{address}/utxos?after=...&limit=...` | Canonically fenced watched UTXOs for one address; unavailable unless IX is ready and all historical watch backfills are complete |
| `GET /v1/events?after_cursor=...&limit=...` | Exclusive-after, at-least-once observation revision feed for this process scope |
| `GET /health/live` | Process/supervisor liveness only |
| `GET /health/ready` | Sanitized readiness only |
| `GET /metrics` | Prometheus output on the configured loopback metrics listener |

A watch body contains `selector`, `start_height`, and `idempotency_key`.
Selectors are `{"type":"address","value":"..."}` or
`{"type":"transaction","value":"..."}`. Reusing an idempotency key with a
different selector or height conflicts.

The UTXO response contains canonical decimal `generation` and `revision`, the
exact height/hash `checkpoint`, `outputs`, and an optional cursor bound to that
full snapshot. Each output contains
`transaction_id`, `output_index`, `value_sats`, `script_pubkey`, `address`,
`created_height`, `coinbase`, and `confirmations`. WS binds the first page's
generation, revision, and checkpoint across later pages and other addresses,
then verifies the checkpoint hash against its own Core node. It never stitches
UTXOs across a normal commit, reorg, backfill, or rebuild activation.

IX emits stable `txid:vin:index` and `txid:vout:index` movements rather than a
fabricated single from/to transfer. A non-coinbase fee is the checked sum of
all resolved previous-output values minus all output values. Fee payer is
absent when input ownership is ambiguous. Coinbase transactions have no
network-fee fact.

UTXO creation/spend mutations, observation revisions, event rows, undo, raw
block payload, and checkpoint movement commit atomically. A reorg removes
orphaned creations, restores orphaned spends by reversing spent markers, and
appends corrected revisions; it does not delete published history. A reorg
beyond configured retention moves IX to `rebuild_required`.

Prevout enrichment is also bounded across a block. The source accepts at most
25,000 distinct external outpoints, a ceiling above the loose consensus maximum
of 24,390 derived from block weight and minimum input size. Each retained
value/address fact is at most 192 encoded bytes, for at most 4.8 MB of added
replay data; full historical scripts are verified against transaction consensus
bytes and discarded before accumulation.

## WS HTTP surface

All operation routes use strict `POST application/json`, reject unknown fields,
and require the configured WS bearer token.

| Path | Request and response contract |
|---|---|
| `/v1/bitcoin/addresses` | Request `operation_id`, `address_kind` (`p2wpkh` or `p2tr`), and `key_purpose`; returns canonical `address` and opaque `key_locator` |
| `/v1/bitcoin/balances` | Request canonical `address`; returns `confirmed_satoshis`, `pending_satoshis`, and gross confirmation/maturity-qualified `spendable_satoshis` |
| `/v1/bitcoin/transfers/sign` | Request operation ID, exact `inputs`, recipient outputs, change address, and requested sat/kvB rate; returns txid, exact raw bytes, selected outpoints, actual outputs, fee, and vsize without broadcasting |
| `/v1/bitcoin/collections/requirements` | Request unique source-address objects; returns any `no_spendable_outputs` requirements |
| `/v1/bitcoin/collections/sign` | Request operation ID, sources with exact inputs and source key locators, one destination, and requested sat/kvB rate; returns the signed transaction review fields plus gross input attribution per source |
| `/v1/bitcoin/transactions/broadcast` | Request `expected_transaction_id` and exact `raw_transaction`; validates their consensus relationship, preflights, then submits unchanged bytes |
| `/v1/bitcoin/receipts` | Request one txid; returns the current Core lookup; a Core-known mempool/conflicted transaction can have `confirmations=0` and no block reference, while RPC not-found returns `null` |
| `/health/live` | Unauthenticated, detail-free process liveness |
| `/health/ready` | Unauthenticated, detail-free readiness |

An exact transfer input contains `transaction_id`, decimal-string
`output_index`, decimal-string `value_satoshis`, canonical `0x`-prefixed
`script_pubkey`, canonical network address, and opaque `key_locator`. Collection
sources carry the address/key locator once and use the same exact outpoint,
value, and script fields for their inputs. Duplicate outpoints, duplicate
collection sources, wrong-network addresses, unsupported scripts, zero values,
and non-canonical decimal/hex values fail before signing.

Prepared responses return `raw_transaction` as lowercase `0x`-prefixed exact
consensus bytes. Raw transactions, header values, endpoint URLs, and bearer or
custody credentials are redacted from diagnostic output. A key locator remains
either an opaque identifier or an opaque derivation path; callers must not infer
business semantics from it. Application/proxy access logs must not capture
request or response bodies for signing and broadcast routes.

## PS HTTP surface and Bitcoin request shapes

Bitcoin PS reuses the authenticated `/v1` deposit, balance, ledger,
observation, collection, job, accounting, reconciliation, and administrator
routes documented for PS in [`PAYMENT_SERVICE.md`](./PAYMENT_SERVICE.md). Bodies
remain strict JSON, mutations require `Idempotency-Key`, ordinary and
administrator credentials are separate, and large atomic values are decimal
strings. Bitcoin-specific create-deposit input is:

```json
{
  "user_id": "customer-123",
  "scope": {"chain": "bitcoin", "network": "regtest"},
  "asset": "native",
  "expected_amount": "250000"
}
```

`expected_amount` must be positive and fit the native unsigned 64-bit satoshi
range. Deposit creation captures IX Ready as the birthday, asks WS for the
policy-selected address kind, persists `AwaitingWatch`, and returns no usable
address until IX has durably acknowledged its idempotent address watch.

Bitcoin collection creation uses the plural field only:

```json
{
  "deposit_ids": ["deposit-a", "deposit-b"]
}
```

`deposit_ids` must be non-empty, unique, and within the active policy limit.
PS canonicalizes their order for idempotency, loads and authorizes every source,
and rejects the whole command if one deposit is outside the authenticated
exchange principal, native-BTC scope, policy, or master-destination boundary.
The command returns one durable job and collection ID. Collection reads expose
all participants, safe exact-resource summaries, and per-deposit gross debit,
allocated fee, and master credit; they never expose key locators, reservation
evidence, or raw signed transaction bytes.

After a deposit has joined one Bitcoin collection aggregate, another
`deposit_ids` command containing that deposit cannot produce a second
collection in v1, even if a late payment creates a new UTXO. IX and PS still
watch and account for that payment. Do not present the address as repeatedly
collectable; use a new deposit address until multi-collection-per-deposit
ownership and archival are designed.

Creating or retrying a collection can sign and broadcast a transaction. Do not
call those routes against funded addresses merely as a health check. The
`bitcoin-payment-service` demo exercises the repository/signing boundary
offline and stops before broadcast.

## Confirmation and coinbase policy

IX confirmation depth and WS minimum spend confirmations are separate explicit
deployment policies. IX returns every canonical, unspent watched output with
its factual confirmation count and coinbase flag; the UTXO query itself does
not reserve or apply WS business policy.

WS reports any output with at least one canonical confirmation in the
`confirmed` balance, but includes an output in `spendable` and accepts it for
signing only when it reaches `WS_BITCOIN_MINIMUM_CONFIRMATIONS`. Coinbase
outputs additionally require the consensus maturity of 100 confirmations,
regardless of a lower configured minimum. Because IX v1 is block-only,
`pending_satoshis` is normally zero; it must not be interpreted as mempool
coverage.

`spendable_satoshis` is the gross sum after only those WS confirmation and
coinbase-maturity checks. It is not a PS available-to-withdraw or sweepable
balance: it does not subtract reservations, workflow state, fee reserve, dust,
or the cost of spending the selected inputs. Exact construction may still fail
fee/dust policy even when this field is nonzero.

An IX `Included` observation becomes `Confirmed` only at the configured IX
depth. Later canonical blocks update depth even when they contain no watched
transaction. Reorgs append new revisions and correct the canonical UTXO view.

## Exact selected-UTXO signing and broadcast flow

The implemented durable flow is:

1. Bitcoin PS queries one fenced IX projection snapshot, selects
   every eligible outpoint for each explicitly requested same-principal deposit
   in canonical `(txid, vout)` order, and atomically reserves the complete set.
2. PS sends every selected txid/index/value/script/address/key locator to the
   non-broadcasting WS sign route.
3. WS validates request uniqueness and limits, obtains Core's conservative fee
   estimate, and uses the greater of the requested and estimated sat/kvB rates;
   the result must remain within the configured ceiling.
4. As its final remote read before deterministic construction and custody
   signing, WS re-queries active IX UTXOs. Every exact outpoint must still
   exist in one generation/revision/checkpoint snapshot, whose block hash WS
   verifies against Core, and match value, ownership script, confirmation
   policy, and coinbase maturity.
5. WS builds one no-change drain transaction and signs its P2WPKH/P2TR inputs.
   It returns txid, exact raw bytes, selected outpoints, the one master output,
   fee, vsize, and gross input attribution per source.
6. PS independently validates the returned transaction, applies proportional
   largest-remainder fee allocation, and persists the exact signed bytes,
   durable leg/reservations, and all source allocations before any submission
   attempt.
7. A separate broadcast request supplies those unchanged bytes and expected
   txid. WS recomputes the txid, calls Core `testmempoolaccept`, refuses a
   policy rejection or mismatched vsize, calls `sendrawtransaction` with the
   configured maximum fee rate, and requires Core to return the same txid.
8. RPC acceptance means submission only. PS retains the exact bytes and
   reservations, attaches an idempotent transaction watch, and completes
   accounting only from later IX facts. A reorg keeps the same txid/watch and
   permits only same-byte rebroadcast and same-txid re-inclusion.

A timeout or connection loss after `sendrawtransaction` is ambiguous: Core may
have accepted the transaction even when WS did not receive the response. Query
the receipt/mempool first, then retry only the same persisted txid and exact
bytes. Do not automatically construct or sign a conflicting replacement.

Steps 1, 6, and the durable observation-driven workflow belong to the
implemented Bitcoin PS mode. WS routes alone remain stateless chain execution
and do not constitute a durable payment workflow. The complete composed flow
still requires the pending Core 31 operational acceptance run before any
production-readiness claim.

## Acceptance status

Source and deterministic fixtures cover the Core 31 readiness parser, exact
amount/fee conversion, txid/raw-byte validation, P2WPKH/P2TR signing,
same-block spend netting, movements and fees, atomic UTXO projection and undo,
snapshot-bound queries, reorg/rebuild behavior, strict HTTP DTOs, input
revalidation, coinbase maturity, preflight rejection, authentication, and
redaction. The final validation results for the checkout must be reported from
the actual commands run; compilation alone is not operational acceptance. The
earlier IX/WS validation result does not automatically cover later PS changes.

Bitcoin PS source includes deterministic and real-store coverage for strict
policy parsing, same-principal multi-user authorization, canonical request
membership, atomic exact-outpoint collision rejection, full-eligible-UTXO/no-
change selection, largest-remainder fee allocation, fee/dust bounds, retained
exact bytes, and all-participant confirmation/reorg/re-inclusion projection.
The handoff must still report the exact validation commands actually run; this
document does not turn source coverage into operational evidence. The final
matrix must also lock the one-aggregate-per-deposit boundary while proving that
later incoming facts remain watchable and projectable.

Still pending is an opt-in disposable Bitcoin Core 31 regtest run that proves
P2WPKH/P2TR inclusion-to-confirmation, sign-before-broadcast, batch collection,
restart/replay, a controlled reorg, UTXO restoration, and re-inclusion through
the composed HTTP services. The exact commands and evidence matrix are in the
[`manual Core 31 regtest acceptance guide`](./manual-bitcoin-regtest/README.md).
That checked-in guide currently exercises IX/WS directly and must be extended
or supplemented with the PS process for complete PS acceptance. The binary was
unavailable during this implementation pass, so none of those real-node
outcomes is claimed.

Bitcoin PS source and deterministic coverage are present; Core 31 regtest
acceptance remains pending. Deliberately excluded from block-only v1 are mempool/
replacement/drop tracking, PS-generated RBF replacement, fee bumping, CPFP,
PSBT, multisig, hardware-wallet transaction protocols, HA, and multi-network
process ownership.

Bitcoin PS multi-collection-per-deposit support and archival/space reclamation
for indefinitely retained signed UTXO batches are also deferred; operators must
capacity-plan the PS database and issue new deposit addresses for later
collectable payments.
