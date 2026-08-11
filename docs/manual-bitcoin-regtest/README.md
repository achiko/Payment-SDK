# Manual Bitcoin Core 31 regtest acceptance

Status: preserved diagnostic fallback. The repository-owned
[`automated system-acceptance suite`](../../tests/bitcoin-regtest-acceptance/README.md)
passed all eight strict/global-trusted executions on 2026-08-11 against exact
Bitcoin Core 31.1.0. Use this longer manual procedure to inspect individual
IX/custody/WS transitions when diagnosing a failure; it is no longer the
primary acceptance runner. The simpler
[`global_trusted` Payment Service happy path](./PAYMENT_HAPPY_PATH.md) was also
manually exercised on 2026-08-11.

This procedure exercises the composed Bitcoin Indexer Service (IX), ephemeral
development custody, and stateless Wallet Service (WS) against one disposable
Bitcoin Core 31 regtest node. It covers P2WPKH and P2TR, `Included` to
`Confirmed`, signing before broadcast, exact-byte broadcast, batch collection,
restart/replay, forced rollback, UTXO restoration, and re-inclusion.

This manual procedure does not exercise the implemented Bitcoin PS mode, a
public network, production custody, organic proof-of-work competition, mempool
replacement/drop tracking, or production deployment monitoring. A complete
Bitcoin PS operational acceptance run must supplement this guide with
`payment-api bitcoin serve`, deposits, the explicit `deposit_ids` batch,
restart/replay, and PS projection evidence described in
[`../BITCOIN_SERVICES.md`](../BITCOIN_SERVICES.md). `invalidateblock` below
deliberately mutates only this disposable regtest node to exercise the
application rollback path.

## 1. Verify tools and pin Core 31

Run every command from the repository root in one dedicated control terminal.
Keep that terminal open so its process IDs and private variables remain local
to the run. Commands assume `zsh` or `bash`, `curl`, `jq`, `openssl`, `base64`,
and `cargo` are installed.

Set `BITCOIND` and `BITCOIN_CLI` to explicit Bitcoin Core 31 binary paths when
31.x is not the shell default. Do not download or replace a system binary as
part of this procedure.

```bash
set -o errexit
set -o nounset
set -o pipefail
umask 077

export BITCOIND="${BITCOIND:-bitcoind}"
export BITCOIN_CLI="${BITCOIN_CLI:-bitcoin-cli}"

command -v "$BITCOIND"
command -v "$BITCOIN_CLI"
command -v curl
command -v jq
command -v openssl
command -v base64
command -v cargo

"$BITCOIND" --version | grep -E 'Bitcoin Core version v?31([.]|$)'
"$BITCOIN_CLI" --version | grep -E 'Bitcoin Core RPC client version v?31([.]|$)'
test -f Cargo.toml
test -d apps/indexer
test -d apps/wallet
test -d apps/custody
```

The later live `getnetworkinfo.version` assertion is authoritative for the
running node. Do not continue with a different major version and do not treat a
successful Rust build as a substitute for the live checks.

## 2. Create one private disposable run root

The following creates a new narrow temporary root, a unique marker, private
logs/evidence directories, three random service tokens, and one mode-600
environment file. Never enable `set -x`, print the environment file, or commit
anything from this root.

```bash
export BTC_REPO_ROOT="$(pwd -P)"
export BTC_RUN_ID="btc31-$(date -u +%Y%m%dT%H%M%SZ)-$$"
export BTC_TMP_BASE="${TMPDIR:-/tmp}"
export BTC_TMP_BASE="${BTC_TMP_BASE%/}"
case "$BTC_TMP_BASE" in
  /*) ;;
  *) printf '%s\n' 'temporary base must be an absolute path' >&2; false ;;
esac
test -d "$BTC_TMP_BASE"
test -w "$BTC_TMP_BASE"
export BTC_RUN_ROOT="$(mktemp -d "$BTC_TMP_BASE/payment-sdk-btc31.XXXXXX")"
export BTC_PRIVATE_ENV="$BTC_RUN_ROOT/private.env"
export BTC_CORE_DATADIR="$BTC_RUN_ROOT/core"
export BTC_IX_DATABASE="$BTC_RUN_ROOT/indexer/database"
export BTC_LOG_DIR="$BTC_RUN_ROOT/logs"
export BTC_EVIDENCE_DIR="$BTC_RUN_ROOT/evidence"
export BTC_REQUEST_DIR="$BTC_RUN_ROOT/requests"

test -d "$BTC_RUN_ROOT"
printf '%s\n' "$BTC_RUN_ID" > "$BTC_RUN_ROOT/.payment-sdk-bitcoin-regtest-run"
mkdir -p "$BTC_CORE_DATADIR" "$BTC_IX_DATABASE" "$BTC_LOG_DIR" \
  "$BTC_EVIDENCE_DIR" "$BTC_REQUEST_DIR"
chmod 700 "$BTC_RUN_ROOT" "$BTC_LOG_DIR" "$BTC_EVIDENCE_DIR" "$BTC_REQUEST_DIR"

export BTC_CORE_RPC_PORT='19443'
export BTC_IX_HTTP_PORT='18080'
export BTC_IX_METRICS_PORT='19090'
export BTC_CUSTODY_PORT='18181'
export BTC_CUSTODY_METRICS_PORT='19093'
export BTC_WS_HTTP_PORT='18082'
export BTC_WS_METRICS_PORT='19092'

export STRICT_AUTHENTICATION_MODE='true'

export IX_BEARER_TOKEN="$(openssl rand -hex 32)"
export CUSTODY_BEARER_TOKEN="$(openssl rand -hex 32)"
export WS_BEARER_TOKEN="$(openssl rand -hex 32)"

{
  printf "export BTC_RUN_ID='%s'\n" "$BTC_RUN_ID"
  printf "export BTC_RUN_ROOT='%s'\n" "$BTC_RUN_ROOT"
  printf "export BTC_CORE_DATADIR='%s'\n" "$BTC_CORE_DATADIR"
  printf "export BTC_IX_DATABASE='%s'\n" "$BTC_IX_DATABASE"
  printf "export BTC_LOG_DIR='%s'\n" "$BTC_LOG_DIR"
  printf "export BTC_EVIDENCE_DIR='%s'\n" "$BTC_EVIDENCE_DIR"
  printf "export BTC_REQUEST_DIR='%s'\n" "$BTC_REQUEST_DIR"
  printf "export BTC_CORE_RPC_PORT='%s'\n" "$BTC_CORE_RPC_PORT"
  printf "export BTC_IX_HTTP_PORT='%s'\n" "$BTC_IX_HTTP_PORT"
  printf "export BTC_IX_METRICS_PORT='%s'\n" "$BTC_IX_METRICS_PORT"
  printf "export BTC_CUSTODY_PORT='%s'\n" "$BTC_CUSTODY_PORT"
  printf "export BTC_CUSTODY_METRICS_PORT='%s'\n" "$BTC_CUSTODY_METRICS_PORT"
  printf "export BTC_WS_HTTP_PORT='%s'\n" "$BTC_WS_HTTP_PORT"
  printf "export BTC_WS_METRICS_PORT='%s'\n" "$BTC_WS_METRICS_PORT"
  printf "export STRICT_AUTHENTICATION_MODE='%s'\n" "$STRICT_AUTHENTICATION_MODE"
  printf "export IX_BEARER_TOKEN='%s'\n" "$IX_BEARER_TOKEN"
  printf "export CUSTODY_BEARER_TOKEN='%s'\n" "$CUSTODY_BEARER_TOKEN"
  printf "export WS_BEARER_TOKEN='%s'\n" "$WS_BEARER_TOKEN"
} > "$BTC_PRIVATE_ENV"
chmod 600 "$BTC_PRIVATE_ENV"
```

If any listed port is already occupied, stop here, choose unused loopback ports,
and recreate the private file before starting a process. Never reuse a live IX
database, Core datadir, wallet, cookie, or service credential.

## 3. Start Core and assert local node readiness

This node is isolated intentionally. Its local regtest readiness does not prove
mainnet/testnet peer connectivity or tip freshness.

```bash
"$BITCOIND" \
  -regtest \
  -datadir="$BTC_CORE_DATADIR" \
  -server=1 \
  -txindex=1 \
  -prune=0 \
  -listen=0 \
  -discover=0 \
  -rpcbind=127.0.0.1 \
  -rpcallowip=127.0.0.1 \
  -rpcport="$BTC_CORE_RPC_PORT" \
  -fallbackfee=0.00010000 \
  -printtoconsole=1 \
  > "$BTC_LOG_DIR/bitcoind.log" 2>&1 &
export BTC_CORE_PID=$!

btc_cli() {
  "$BITCOIN_CLI" -regtest -datadir="$BTC_CORE_DATADIR" \
    -rpcport="$BTC_CORE_RPC_PORT" "$@"
}

btc_cli -rpcwait getblockchaininfo > "$BTC_EVIDENCE_DIR/core-chain-start.json"
btc_cli getnetworkinfo > "$BTC_EVIDENCE_DIR/core-network.json"
btc_cli getindexinfo txindex > "$BTC_EVIDENCE_DIR/core-txindex-start.json"

jq -e '.version >= 310000 and .version < 320000' \
  "$BTC_EVIDENCE_DIR/core-network.json"
jq -e '
  .chain == "regtest"
  and .pruned == false
  and .initialblockdownload == false
  and .blocks == .headers
' "$BTC_EVIDENCE_DIR/core-chain-start.json"
jq -e '.txindex.synced == true' "$BTC_EVIDENCE_DIR/core-txindex-start.json"

export BTC_REGTEST_GENESIS="$(btc_cli getblockhash 0)"
printf '%s\n' "$BTC_REGTEST_GENESIS" | grep -E '^[0-9a-f]{64}$'
```

Derive the one HTTP Authorization value from Core's private cookie without
printing either value. Services receive it through environment configuration,
not a URL or command-line secret flag.

```bash
CORE_COOKIE="$(< "$BTC_CORE_DATADIR/regtest/.cookie")"
export CORE_AUTHORIZATION="Basic $(printf '%s' "$CORE_COOKIE" | base64 | tr -d '\r\n')"
unset CORE_COOKIE

{
  printf "export BTC_REGTEST_GENESIS='%s'\n" "$BTC_REGTEST_GENESIS"
  printf "export CORE_AUTHORIZATION='%s'\n" "$CORE_AUTHORIZATION"
} >> "$BTC_PRIVATE_ENV"
chmod 600 "$BTC_PRIVATE_ENV"
```

## 4. Mature miner funds and warm the fee estimator

A fresh regtest fee estimator may not return `feerate`. WS intentionally fails
retryably rather than silently trusting only the caller's requested rate, so
the acceptance run must create fee-paying history before signing. Core 31.1
does not expose the older `settxfee` RPC; each warm-up `sendtoaddress` therefore
sets an explicit `2 sat/vB` rate through its named `fee_rate` argument.

```bash
btc_cli createwallet miner > "$BTC_EVIDENCE_DIR/create-miner-wallet.json"

btc_wallet() {
  "$BITCOIN_CLI" -regtest -datadir="$BTC_CORE_DATADIR" \
    -rpcport="$BTC_CORE_RPC_PORT" -rpcwallet=miner "$@"
}

export BTC_MINER_ADDRESS="$(btc_wallet getnewaddress 'acceptance-miner' bech32)"
btc_cli generatetoaddress 101 "$BTC_MINER_ADDRESS" \
  > "$BTC_EVIDENCE_DIR/maturity-blocks.json"
test "$(jq 'length' "$BTC_EVIDENCE_DIR/maturity-blocks.json")" -eq 101
FEE_ESTIMATE_READY='false'
for ROUND in $(seq 1 50); do
  if btc_cli estimatesmartfee 6 conservative 2>/dev/null \
    | jq -e '.feerate != null and .feerate > 0' >/dev/null; then
    FEE_ESTIMATE_READY='true'
    break
  fi

  WARM_ADDRESS="$(btc_wallet getnewaddress "fee-warmup-$ROUND" bech32)"
  for ITEM in $(seq 1 12); do
    btc_wallet -named sendtoaddress \
      address="$WARM_ADDRESS" \
      amount=0.00100000 \
      fee_rate=2 >/dev/null
  done
  btc_cli generatetoaddress 1 "$BTC_MINER_ADDRESS" >/dev/null
done

test "$FEE_ESTIMATE_READY" = 'true'
btc_cli estimatesmartfee 6 conservative \
  > "$BTC_EVIDENCE_DIR/core-fee-estimate.json"
jq -e '.feerate != null and .feerate > 0' \
  "$BTC_EVIDENCE_DIR/core-fee-estimate.json"

btc_cli getblockchaininfo > "$BTC_EVIDENCE_DIR/core-chain-ready.json"
export BTC_CORE_HEIGHT="$(jq -er '.blocks' "$BTC_EVIDENCE_DIR/core-chain-ready.json")"
jq -e '.initialblockdownload == false and .blocks == .headers and .pruned == false' \
  "$BTC_EVIDENCE_DIR/core-chain-ready.json"
btc_cli getindexinfo txindex > "$BTC_EVIDENCE_DIR/core-txindex-ready.json"
jq -e --argjson height "$BTC_CORE_HEIGHT" '
  .txindex.synced == true
  and ((.txindex.best_block_height // $height) == $height)
' "$BTC_EVIDENCE_DIR/core-txindex-ready.json"
```

## 5. Build and configure the three services

This build is validation setup, not proof that the live scenario passes.

```bash
cargo build --locked \
  -p custody-worker \
  -p indexer-worker \
  -p wallet-worker

export IX_DATABASE_PATH="$BTC_IX_DATABASE"
export IX_NETWORK='regtest'
export IX_BOOTSTRAP_HEIGHT='0'
export IX_CONFIRMATION_DEPTH='2'
export IX_REORG_RETENTION='20'
export IX_EXPECTED_GENESIS_HASH="$BTC_REGTEST_GENESIS"
export IX_RPC_HTTP_URL="http://127.0.0.1:$BTC_CORE_RPC_PORT"
export IX_RPC_HEADERS="authorization=$CORE_AUTHORIZATION"
export IX_RPC_TIMEOUT_SECONDS='15'
export IX_RPC_MAX_RESPONSE_BYTES='268435456'
export IX_HTTP_BIND="127.0.0.1:$BTC_IX_HTTP_PORT"
export IX_METRICS_BIND="127.0.0.1:$BTC_IX_METRICS_PORT"
export IX_POLL_SECONDS='1'
export IX_READY_MAX_LAG='0'
export IX_READY_MAX_AGE_SECONDS='30'

export CUSTODY_BIND="127.0.0.1:$BTC_CUSTODY_PORT"
export CUSTODY_METRICS_BIND="127.0.0.1:$BTC_CUSTODY_METRICS_PORT"

export WS_BITCOIN_NETWORK='regtest'
export WS_BITCOIN_EXPECTED_GENESIS_HASH="$BTC_REGTEST_GENESIS"
export WS_BITCOIN_CORE_RPC_URL="http://127.0.0.1:$BTC_CORE_RPC_PORT"
export WS_BITCOIN_CORE_RPC_AUTHORIZATION="$CORE_AUTHORIZATION"
unset WS_BITCOIN_CORE_RPC_HEADERS || true
export WS_BITCOIN_IX_URL="http://127.0.0.1:$BTC_IX_HTTP_PORT"
export WS_BITCOIN_IX_BEARER_TOKEN="$IX_BEARER_TOKEN"
export WS_BITCOIN_MINIMUM_CONFIRMATIONS='1'
export WS_BITCOIN_FEE_TARGET_BLOCKS='6'
export WS_BITCOIN_MAX_SATOSHIS_PER_KVB='100000'
export WS_CUSTODY_URL="http://127.0.0.1:$BTC_CUSTODY_PORT"
export WS_CUSTODY_AUTHENTICATION_POLICY='repository_mode_matched'
export WS_CUSTODY_BEARER_TOKEN="$CUSTODY_BEARER_TOKEN"
export WS_HTTP_BIND="127.0.0.1:$BTC_WS_HTTP_PORT"
export WS_METRICS_BIND="127.0.0.1:$BTC_WS_METRICS_PORT"
```

Create private curl configuration files so bearer values do not appear in the
copied commands or process arguments. Reverse proxies and capture tooling must
not log WS signing request/response bodies.

```bash
export IX_CURL_CONFIG="$BTC_RUN_ROOT/ix-curl.conf"
export WS_CURL_CONFIG="$BTC_RUN_ROOT/ws-curl.conf"

{
  printf 'silent\nshow-error\nfail\n'
  printf 'header = "Authorization: Bearer %s"\n' "$IX_BEARER_TOKEN"
  printf 'header = "Content-Type: application/json"\n'
} > "$IX_CURL_CONFIG"

{
  printf 'silent\nshow-error\nfail\n'
  printf 'header = "Authorization: Bearer %s"\n' "$WS_BEARER_TOKEN"
  printf 'header = "Content-Type: application/json"\n'
} > "$WS_CURL_CONFIG"

chmod 600 "$IX_CURL_CONFIG" "$WS_CURL_CONFIG"

ix_call() {
  curl --config "$IX_CURL_CONFIG" "$@"
}

ws_call() {
  curl --config "$WS_CURL_CONFIG" "$@"
}
```

## 6. Start IX, disposable custody, and WS

`apps/custody` is loopback-only and in-memory. Every generated private key is
destroyed when that process exits. Never use these addresses outside this
disposable regtest. Keep `BTC_CUSTODY_PID` alive through the IX/WS restart test;
restarting custody would make the returned key locators unusable.

```bash
start_ix() {
  "$BTC_REPO_ROOT/target/debug/indexer-worker" bitcoin serve \
    > "$BTC_LOG_DIR/indexer.log" 2>&1 &
  export BTC_IX_PID=$!
}

start_custody() {
  "$BTC_REPO_ROOT/target/debug/custody-worker" serve \
    > "$BTC_LOG_DIR/custody.log" 2>&1 &
  export BTC_CUSTODY_PID=$!
}

start_ws() {
  "$BTC_REPO_ROOT/target/debug/wallet-worker" bitcoin serve \
    > "$BTC_LOG_DIR/wallet.log" 2>&1 &
  export BTC_WS_PID=$!
}

start_ix
start_custody

IX_READY='false'
for ATTEMPT in $(seq 1 300); do
  if curl --fail --silent --show-error \
      "http://127.0.0.1:$BTC_IX_HTTP_PORT/health/ready" >/dev/null 2>&1 \
    && ix_call --url \
      "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/status" \
      | jq -e '
          .scope.chain == "bitcoin"
          and .scope.network == "regtest"
          and .phase == "ready"
          and .checkpoint != null
        ' >/dev/null; then
    IX_READY='true'
    break
  fi
  sleep 1
done
test "$IX_READY" = 'true'

curl --fail --silent --show-error \
  "http://127.0.0.1:$BTC_CUSTODY_PORT/health/ready" >/dev/null

start_ws
WS_READY='false'
for ATTEMPT in $(seq 1 120); do
  if curl --fail --silent --show-error \
      "http://127.0.0.1:$BTC_WS_HTTP_PORT/health/ready" >/dev/null 2>&1; then
    WS_READY='true'
    break
  fi
  sleep 1
done
test "$WS_READY" = 'true'

ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/status" \
  --output "$BTC_EVIDENCE_DIR/ix-status-start.json"
jq -e --arg genesis "$BTC_REGTEST_GENESIS" '
  .phase == "ready"
  and .scope.chain == "bitcoin"
  and .scope.network == "regtest"
  and .confirmation_depth == "2"
  and (.checkpoint.hash | test("^[0-9a-f]{64}$"))
' "$BTC_EVIDENCE_DIR/ix-status-start.json"
```

WS periodically rechecks custody availability/mode and IX
readiness/network/mode. Those failures clear `/health/ready`, which recovers
after the dependencies do. Core operation failures remain separate, so the
remaining steps still assert mode-aware functional responses and Core/IX state,
not health alone.

## 7. Generate P2WPKH/P2TR addresses and register watches

Signing responses and key locators remain in mode-600 files. Do not print or
copy those files into logs, tickets, or the repository.

```bash
jq -n '{
  operation_id: "core31-address-p2wpkh-1",
  address_kind: "p2wpkh",
  key_purpose: "core31-acceptance-p2wpkh"
}' > "$BTC_REQUEST_DIR/address-p2wpkh.json"

jq -n '{
  operation_id: "core31-address-p2tr-1",
  address_kind: "p2tr",
  key_purpose: "core31-acceptance-p2tr"
}' > "$BTC_REQUEST_DIR/address-p2tr.json"

ws_call --request POST \
  --url "http://127.0.0.1:$BTC_WS_HTTP_PORT/v1/bitcoin/addresses" \
  --data-binary "@$BTC_REQUEST_DIR/address-p2wpkh.json" \
  --output "$BTC_EVIDENCE_DIR/address-p2wpkh-private.json"

ws_call --request POST \
  --url "http://127.0.0.1:$BTC_WS_HTTP_PORT/v1/bitcoin/addresses" \
  --data-binary "@$BTC_REQUEST_DIR/address-p2tr.json" \
  --output "$BTC_EVIDENCE_DIR/address-p2tr-private.json"

export P2WPKH_ADDRESS="$(jq -er '.address' \
  "$BTC_EVIDENCE_DIR/address-p2wpkh-private.json")"
export P2TR_ADDRESS="$(jq -er '.address' \
  "$BTC_EVIDENCE_DIR/address-p2tr-private.json")"

btc_cli validateaddress "$P2WPKH_ADDRESS" \
  | jq -e '.isvalid == true and .iswitness == true and .witness_version == 0'
btc_cli validateaddress "$P2TR_ADDRESS" \
  | jq -e '.isvalid == true and .iswitness == true and .witness_version == 1'

export WATCH_START_HEIGHT="$(btc_cli getblockcount)"

jq -n --arg address "$P2WPKH_ADDRESS" --arg height "$WATCH_START_HEIGHT" '{
  selector: {type: "address", value: $address},
  start_height: $height,
  idempotency_key: "core31-watch-p2wpkh-1"
}' > "$BTC_REQUEST_DIR/watch-p2wpkh.json"

jq -n --arg address "$P2TR_ADDRESS" --arg height "$WATCH_START_HEIGHT" '{
  selector: {type: "address", value: $address},
  start_height: $height,
  idempotency_key: "core31-watch-p2tr-1"
}' > "$BTC_REQUEST_DIR/watch-p2tr.json"

ix_call --request POST \
  --url "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/watches" \
  --data-binary "@$BTC_REQUEST_DIR/watch-p2wpkh.json" \
  --output "$BTC_EVIDENCE_DIR/watch-p2wpkh.json"

ix_call --request POST \
  --url "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/watches" \
  --data-binary "@$BTC_REQUEST_DIR/watch-p2tr.json" \
  --output "$BTC_EVIDENCE_DIR/watch-p2tr.json"

jq -e '.id | type == "string" and length > 0' \
  "$BTC_EVIDENCE_DIR/watch-p2wpkh.json"
jq -e '.id | type == "string" and length > 0' \
  "$BTC_EVIDENCE_DIR/watch-p2tr.json"
```

## 8. Fund both address types and prove Included to Confirmed

Create two P2WPKH outputs so one can fund the transfer while another remains
for the later batch collection. Create one P2TR output for the same collection.

```bash
export P2WPKH_FUND_TX_A="$(btc_wallet sendtoaddress "$P2WPKH_ADDRESS" 0.00500000)"
export P2WPKH_FUND_TX_B="$(btc_wallet sendtoaddress "$P2WPKH_ADDRESS" 0.00500000)"
export P2TR_FUND_TX="$(btc_wallet sendtoaddress "$P2TR_ADDRESS" 0.00500000)"

register_tx_watch() {
  TXID="$1"
  WATCH_KEY="$2"
  REQUEST_PATH="$BTC_REQUEST_DIR/watch-tx-$WATCH_KEY.json"
  RESPONSE_PATH="$BTC_EVIDENCE_DIR/watch-tx-$WATCH_KEY.json"
  START_HEIGHT="$(btc_cli getblockcount)"
  jq -n --arg txid "$TXID" --arg height "$START_HEIGHT" \
    --arg key "$WATCH_KEY" '{
      selector: {type: "transaction", value: $txid},
      start_height: $height,
      idempotency_key: ("core31-watch-tx-" + $key)
    }' > "$REQUEST_PATH"
  ix_call --request POST \
    --url "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/watches" \
    --data-binary "@$REQUEST_PATH" --output "$RESPONSE_PATH"
  jq -e '.id | type == "string" and length > 0' "$RESPONSE_PATH" >/dev/null
}

wait_ix_tx_kind() {
  TXID="$1"
  EXPECTED_KIND="$2"
  OUTPUT_PATH="$3"
  FOUND='false'
  for ATTEMPT in $(seq 1 120); do
    if ix_call --url \
        "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/transactions/$TXID" \
        --output "$OUTPUT_PATH.tmp" 2>/dev/null \
      && jq -e --arg txid "$TXID" --arg kind "$EXPECTED_KIND" '
          .transaction_id == $txid and .status.kind == $kind
        ' "$OUTPUT_PATH.tmp" >/dev/null; then
      mv "$OUTPUT_PATH.tmp" "$OUTPUT_PATH"
      FOUND='true'
      break
    fi
    sleep 1
  done
  test "$FOUND" = 'true'
}

register_tx_watch "$P2WPKH_FUND_TX_A" 'fund-p2wpkh-a'
register_tx_watch "$P2TR_FUND_TX" 'fund-p2tr'

btc_cli generatetoaddress 1 "$BTC_MINER_ADDRESS" \
  > "$BTC_EVIDENCE_DIR/funding-inclusion-block.json"

wait_ix_tx_kind "$P2WPKH_FUND_TX_A" 'included' \
  "$BTC_EVIDENCE_DIR/fund-p2wpkh-included.json"
wait_ix_tx_kind "$P2TR_FUND_TX" 'included' \
  "$BTC_EVIDENCE_DIR/fund-p2tr-included.json"
jq -e '.status.confirmations == "1"' \
  "$BTC_EVIDENCE_DIR/fund-p2wpkh-included.json"
jq -e '.status.confirmations == "1"' \
  "$BTC_EVIDENCE_DIR/fund-p2tr-included.json"

btc_cli generatetoaddress 1 "$BTC_MINER_ADDRESS" \
  > "$BTC_EVIDENCE_DIR/funding-confirmation-block.json"

wait_ix_tx_kind "$P2WPKH_FUND_TX_A" 'confirmed' \
  "$BTC_EVIDENCE_DIR/fund-p2wpkh-confirmed.json"
wait_ix_tx_kind "$P2TR_FUND_TX" 'confirmed' \
  "$BTC_EVIDENCE_DIR/fund-p2tr-confirmed.json"

ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/addresses/$P2WPKH_ADDRESS/utxos?limit=100" \
  --output "$BTC_EVIDENCE_DIR/p2wpkh-utxos-before-transfer.json"
ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/addresses/$P2TR_ADDRESS/utxos?limit=100" \
  --output "$BTC_EVIDENCE_DIR/p2tr-utxos-before-transfer.json"

jq -e '
  (.outputs | length) == 2
  and ([.outputs[].value_sats | tonumber] | add) == 1000000
' "$BTC_EVIDENCE_DIR/p2wpkh-utxos-before-transfer.json"
jq -e '
  (.outputs | length) == 1
  and ([.outputs[].value_sats | tonumber] | add) == 500000
' "$BTC_EVIDENCE_DIR/p2tr-utxos-before-transfer.json"
```

## 9. Sign one transfer and prove it was not broadcast

The selected IX output supplies every exact prevout field. WS derives the input
weight after script/address validation; the request does not choose it.

```bash
export TRANSFER_RECIPIENT="$(btc_wallet getnewaddress 'acceptance-transfer' bech32)"

jq -n \
  --arg recipient "$TRANSFER_RECIPIENT" \
  --arg change "$P2WPKH_ADDRESS" \
  --slurpfile address_response "$BTC_EVIDENCE_DIR/address-p2wpkh-private.json" \
  --slurpfile utxos "$BTC_EVIDENCE_DIR/p2wpkh-utxos-before-transfer.json" '{
    operation_id: "core31-sign-transfer-1",
    inputs: [($utxos[0].outputs[0] | {
      transaction_id,
      output_index,
      value_satoshis: .value_sats,
      script_pubkey,
      address,
      key_locator: $address_response[0].key_locator
    })],
    recipients: [{address: $recipient, value_satoshis: "200000"}],
    change_address: $change,
    fee_rate_satoshis_per_kvb: "1000"
  }' > "$BTC_REQUEST_DIR/sign-transfer-private.json"

ws_call --request POST \
  --url "http://127.0.0.1:$BTC_WS_HTTP_PORT/v1/bitcoin/transfers/sign" \
  --data-binary "@$BTC_REQUEST_DIR/sign-transfer-private.json" \
  --output "$BTC_EVIDENCE_DIR/signed-transfer-private.json"

jq -e '
  (.transaction_id | test("^[0-9a-f]{64}$"))
  and (.raw_transaction | test("^0x[0-9a-f]+$"))
  and (.selected_outpoints | length) == 1
  and (.outputs | length) >= 1
  and (.fee_satoshis | tonumber) > 0
  and (.virtual_size | tonumber) > 0
' "$BTC_EVIDENCE_DIR/signed-transfer-private.json"

export TRANSFER_TXID="$(jq -er '.transaction_id' \
  "$BTC_EVIDENCE_DIR/signed-transfer-private.json")"

if btc_cli getmempoolentry "$TRANSFER_TXID" >/dev/null 2>&1; then
  printf '%s\n' 'transfer unexpectedly entered the mempool during signing' >&2
  false
fi
if btc_cli getrawtransaction "$TRANSFER_TXID" true >/dev/null 2>&1; then
  printf '%s\n' 'Core unexpectedly knew the transfer before broadcast' >&2
  false
fi

jq -n --arg txid "$TRANSFER_TXID" '{transaction_id: $txid}' \
  > "$BTC_REQUEST_DIR/receipt-transfer.json"
ws_call --request POST \
  --url "http://127.0.0.1:$BTC_WS_HTTP_PORT/v1/bitcoin/receipts" \
  --data-binary "@$BTC_REQUEST_DIR/receipt-transfer.json" \
  --output "$BTC_EVIDENCE_DIR/receipt-transfer-before-broadcast.json"
jq -e --arg txid "$TRANSFER_TXID" '
  .transaction_id == $txid and .receipt == null
' "$BTC_EVIDENCE_DIR/receipt-transfer-before-broadcast.json"

register_tx_watch "$TRANSFER_TXID" 'signed-transfer'
```

## 10. Broadcast the exact bytes and verify the zero-confirmation shape

Persist the exact signed response before constructing the broadcast request.
Never put `raw_transaction` on a command line or print either private file.

```bash
jq -n --arg txid "$TRANSFER_TXID" \
  --slurpfile signed "$BTC_EVIDENCE_DIR/signed-transfer-private.json" '{
  expected_transaction_id: $txid,
  raw_transaction: $signed[0].raw_transaction
}' > "$BTC_REQUEST_DIR/broadcast-transfer-private.json"

ws_call --request POST \
  --url "http://127.0.0.1:$BTC_WS_HTTP_PORT/v1/bitcoin/transactions/broadcast" \
  --data-binary "@$BTC_REQUEST_DIR/broadcast-transfer-private.json" \
  --output "$BTC_EVIDENCE_DIR/broadcast-transfer.json"
jq -e --arg txid "$TRANSFER_TXID" '.transaction_id == $txid' \
  "$BTC_EVIDENCE_DIR/broadcast-transfer.json"

btc_cli getmempoolentry "$TRANSFER_TXID" \
  > "$BTC_EVIDENCE_DIR/transfer-mempool-entry.json"
btc_cli getrawtransaction "$TRANSFER_TXID" true \
  | jq 'del(.hex)' > "$BTC_EVIDENCE_DIR/transfer-mempool-transaction-sanitized.json"

ws_call --request POST \
  --url "http://127.0.0.1:$BTC_WS_HTTP_PORT/v1/bitcoin/receipts" \
  --data-binary "@$BTC_REQUEST_DIR/receipt-transfer.json" \
  --output "$BTC_EVIDENCE_DIR/receipt-transfer-mempool.json"
jq -e --arg txid "$TRANSFER_TXID" '
  .transaction_id == $txid
  and .receipt != null
  and .receipt.confirmations == 0
  and .receipt.included_in == null
  and .receipt.replaced_by == null
' "$BTC_EVIDENCE_DIR/receipt-transfer-mempool.json"

btc_cli generatetoaddress 1 "$BTC_MINER_ADDRESS" \
  > "$BTC_EVIDENCE_DIR/transfer-inclusion-block.json"
wait_ix_tx_kind "$TRANSFER_TXID" 'included' \
  "$BTC_EVIDENCE_DIR/transfer-included.json"
btc_cli generatetoaddress 1 "$BTC_MINER_ADDRESS" \
  > "$BTC_EVIDENCE_DIR/transfer-confirmation-block.json"
wait_ix_tx_kind "$TRANSFER_TXID" 'confirmed' \
  "$BTC_EVIDENCE_DIR/transfer-confirmed.json"
```

If the broadcast call times out or loses its response, its outcome is
ambiguous. Query the receipt and Core mempool first; retry only the unchanged
private broadcast request. Do not construct or sign a replacement implicitly.

## 11. Sign and broadcast one P2WPKH/P2TR batch collection

Refresh both IX projections after the transfer so the batch uses current exact
outpoints. It must drain both source groups to one Core-wallet destination and
return gross attribution for each source.

```bash
ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/addresses/$P2WPKH_ADDRESS/utxos?limit=100" \
  --output "$BTC_EVIDENCE_DIR/p2wpkh-utxos-before-collection.json"
ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/addresses/$P2TR_ADDRESS/utxos?limit=100" \
  --output "$BTC_EVIDENCE_DIR/p2tr-utxos-before-collection.json"

jq -e '.outputs | length > 0' \
  "$BTC_EVIDENCE_DIR/p2wpkh-utxos-before-collection.json"
jq -e '.outputs | length > 0' \
  "$BTC_EVIDENCE_DIR/p2tr-utxos-before-collection.json"

jq -n --arg p2wpkh "$P2WPKH_ADDRESS" --arg p2tr "$P2TR_ADDRESS" '{
  sources: [{address: $p2wpkh}, {address: $p2tr}]
}' > "$BTC_REQUEST_DIR/collection-requirements.json"
ws_call --request POST \
  --url "http://127.0.0.1:$BTC_WS_HTTP_PORT/v1/bitcoin/collections/requirements" \
  --data-binary "@$BTC_REQUEST_DIR/collection-requirements.json" \
  --output "$BTC_EVIDENCE_DIR/collection-requirements.json"
jq -e '.requirements | length == 0' \
  "$BTC_EVIDENCE_DIR/collection-requirements.json"

export COLLECTION_DESTINATION="$(btc_wallet getnewaddress 'acceptance-collection' bech32)"

jq -n \
  --arg p2wpkh "$P2WPKH_ADDRESS" \
  --arg p2tr "$P2TR_ADDRESS" \
  --arg destination "$COLLECTION_DESTINATION" \
  --slurpfile p2w_address "$BTC_EVIDENCE_DIR/address-p2wpkh-private.json" \
  --slurpfile p2tr_address "$BTC_EVIDENCE_DIR/address-p2tr-private.json" \
  --slurpfile p2w_utxos "$BTC_EVIDENCE_DIR/p2wpkh-utxos-before-collection.json" \
  --slurpfile p2tr_utxos "$BTC_EVIDENCE_DIR/p2tr-utxos-before-collection.json" '{
    operation_id: "core31-sign-collection-1",
    sources: [
      {
        address: $p2wpkh,
        key_locator: $p2w_address[0].key_locator,
        inputs: ($p2w_utxos[0].outputs | map({
          transaction_id,
          output_index,
          value_satoshis: .value_sats,
          script_pubkey
        }))
      },
      {
        address: $p2tr,
        key_locator: $p2tr_address[0].key_locator,
        inputs: ($p2tr_utxos[0].outputs | map({
          transaction_id,
          output_index,
          value_satoshis: .value_sats,
          script_pubkey
        }))
      }
    ],
    destination: $destination,
    fee_rate_satoshis_per_kvb: "1000"
  }' > "$BTC_REQUEST_DIR/sign-collection-private.json"

ws_call --request POST \
  --url "http://127.0.0.1:$BTC_WS_HTTP_PORT/v1/bitcoin/collections/sign" \
  --data-binary "@$BTC_REQUEST_DIR/sign-collection-private.json" \
  --output "$BTC_EVIDENCE_DIR/signed-collection-private.json"

export P2WPKH_COLLECTION_GROSS="$(jq '
  [.outputs[].value_sats | tonumber] | add
' "$BTC_EVIDENCE_DIR/p2wpkh-utxos-before-collection.json")"
export P2TR_COLLECTION_GROSS="$(jq '
  [.outputs[].value_sats | tonumber] | add
' "$BTC_EVIDENCE_DIR/p2tr-utxos-before-collection.json")"
export COLLECTION_TOTAL_GROSS="$((P2WPKH_COLLECTION_GROSS + P2TR_COLLECTION_GROSS))"

jq -e \
  --arg p2wpkh "$P2WPKH_ADDRESS" \
  --arg p2tr "$P2TR_ADDRESS" \
  --arg p2w_gross "$P2WPKH_COLLECTION_GROSS" \
  --arg p2tr_gross "$P2TR_COLLECTION_GROSS" \
  --arg total "$COLLECTION_TOTAL_GROSS" '
    (.outputs | length) == 1
    and (.attribution | length) == 2
    and ([.attribution[] | select(.address == $p2wpkh)
      | .gross_input_satoshis][0] == $p2w_gross)
    and ([.attribution[] | select(.address == $p2tr)
      | .gross_input_satoshis][0] == $p2tr_gross)
    and ((.outputs[0].value_satoshis | tonumber)
      + (.fee_satoshis | tonumber) == ($total | tonumber))
  ' "$BTC_EVIDENCE_DIR/signed-collection-private.json"

export COLLECTION_TXID="$(jq -er '.transaction_id' \
  "$BTC_EVIDENCE_DIR/signed-collection-private.json")"
if btc_cli getmempoolentry "$COLLECTION_TXID" >/dev/null 2>&1; then
  printf '%s\n' 'collection unexpectedly entered the mempool during signing' >&2
  false
fi

register_tx_watch "$COLLECTION_TXID" 'batch-collection'

jq -n --arg txid "$COLLECTION_TXID" \
  --slurpfile signed "$BTC_EVIDENCE_DIR/signed-collection-private.json" '{
  expected_transaction_id: $txid,
  raw_transaction: $signed[0].raw_transaction
}' > "$BTC_REQUEST_DIR/broadcast-collection-private.json"

ws_call --request POST \
  --url "http://127.0.0.1:$BTC_WS_HTTP_PORT/v1/bitcoin/transactions/broadcast" \
  --data-binary "@$BTC_REQUEST_DIR/broadcast-collection-private.json" \
  --output "$BTC_EVIDENCE_DIR/broadcast-collection.json"
jq -e --arg txid "$COLLECTION_TXID" '.transaction_id == $txid' \
  "$BTC_EVIDENCE_DIR/broadcast-collection.json"
btc_cli getmempoolentry "$COLLECTION_TXID" \
  > "$BTC_EVIDENCE_DIR/collection-mempool-entry.json"

export COLLECTION_OLD_BLOCK_HASH="$(
  btc_cli generatetoaddress 1 "$BTC_MINER_ADDRESS" | jq -er '.[0]'
)"
wait_ix_tx_kind "$COLLECTION_TXID" 'included' \
  "$BTC_EVIDENCE_DIR/collection-included.json"
export COLLECTION_INCLUDED_REVISION="$(jq -er '.revision' \
  "$BTC_EVIDENCE_DIR/collection-included.json")"

btc_cli generatetoaddress 1 "$BTC_MINER_ADDRESS" \
  > "$BTC_EVIDENCE_DIR/collection-confirmation-block.json"
wait_ix_tx_kind "$COLLECTION_TXID" 'confirmed' \
  "$BTC_EVIDENCE_DIR/collection-confirmed.json"
export COLLECTION_CONFIRMED_REVISION="$(jq -er '.revision' \
  "$BTC_EVIDENCE_DIR/collection-confirmed.json")"
test "$COLLECTION_CONFIRMED_REVISION" -gt "$COLLECTION_INCLUDED_REVISION"

ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/addresses/$P2WPKH_ADDRESS/utxos?limit=100" \
  --output "$BTC_EVIDENCE_DIR/p2wpkh-utxos-after-collection.json"
ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/addresses/$P2TR_ADDRESS/utxos?limit=100" \
  --output "$BTC_EVIDENCE_DIR/p2tr-utxos-after-collection.json"
jq -e '.outputs | length == 0' \
  "$BTC_EVIDENCE_DIR/p2wpkh-utxos-after-collection.json"
jq -e '.outputs | length == 0' \
  "$BTC_EVIDENCE_DIR/p2tr-utxos-after-collection.json"
```

## 12. Restart IX/WS and prove durable replay

Capture the checkpoint, projections, and complete small event page before the
restart. Stop WS first, then IX. Keep Core and custody running.

```bash
ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/status" \
  --output "$BTC_EVIDENCE_DIR/restart-status-before.json"
ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/addresses/$P2WPKH_ADDRESS/utxos?limit=100" \
  --output "$BTC_EVIDENCE_DIR/restart-p2wpkh-before.json"
ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/addresses/$P2TR_ADDRESS/utxos?limit=100" \
  --output "$BTC_EVIDENCE_DIR/restart-p2tr-before.json"
ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/events?after_cursor=0&limit=100" \
  --output "$BTC_EVIDENCE_DIR/restart-events-before.json"
export RESTART_LAST_CURSOR="$(jq -er '.events[-1].cursor' \
  "$BTC_EVIDENCE_DIR/restart-events-before.json")"

kill -INT "$BTC_WS_PID"
wait "$BTC_WS_PID"
kill -INT "$BTC_IX_PID"
wait "$BTC_IX_PID"
kill -0 "$BTC_CUSTODY_PID"

start_ix
IX_RESTARTED='false'
for ATTEMPT in $(seq 1 180); do
  if curl --fail --silent --show-error \
      "http://127.0.0.1:$BTC_IX_HTTP_PORT/health/ready" >/dev/null 2>&1; then
    IX_RESTARTED='true'
    break
  fi
  sleep 1
done
test "$IX_RESTARTED" = 'true'

start_ws
WS_RESTARTED='false'
for ATTEMPT in $(seq 1 120); do
  if curl --fail --silent --show-error \
      "http://127.0.0.1:$BTC_WS_HTTP_PORT/health/ready" >/dev/null 2>&1; then
    WS_RESTARTED='true'
    break
  fi
  sleep 1
done
test "$WS_RESTARTED" = 'true'
kill -0 "$BTC_CUSTODY_PID"

ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/status" \
  --output "$BTC_EVIDENCE_DIR/restart-status-after.json"
ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/addresses/$P2WPKH_ADDRESS/utxos?limit=100" \
  --output "$BTC_EVIDENCE_DIR/restart-p2wpkh-after.json"
ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/addresses/$P2TR_ADDRESS/utxos?limit=100" \
  --output "$BTC_EVIDENCE_DIR/restart-p2tr-after.json"
ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/events?after_cursor=0&limit=100" \
  --output "$BTC_EVIDENCE_DIR/restart-events-after.json"

jq -S '{phase, checkpoint, confirmation_depth}' \
  "$BTC_EVIDENCE_DIR/restart-status-before.json" \
  > "$BTC_EVIDENCE_DIR/restart-status-before.canonical.json"
jq -S '{phase, checkpoint, confirmation_depth}' \
  "$BTC_EVIDENCE_DIR/restart-status-after.json" \
  > "$BTC_EVIDENCE_DIR/restart-status-after.canonical.json"
cmp "$BTC_EVIDENCE_DIR/restart-status-before.canonical.json" \
  "$BTC_EVIDENCE_DIR/restart-status-after.canonical.json"

jq -S . "$BTC_EVIDENCE_DIR/restart-p2wpkh-before.json" \
  > "$BTC_EVIDENCE_DIR/restart-p2wpkh-before.canonical.json"
jq -S . "$BTC_EVIDENCE_DIR/restart-p2wpkh-after.json" \
  > "$BTC_EVIDENCE_DIR/restart-p2wpkh-after.canonical.json"
cmp "$BTC_EVIDENCE_DIR/restart-p2wpkh-before.canonical.json" \
  "$BTC_EVIDENCE_DIR/restart-p2wpkh-after.canonical.json"

jq -S . "$BTC_EVIDENCE_DIR/restart-p2tr-before.json" \
  > "$BTC_EVIDENCE_DIR/restart-p2tr-before.canonical.json"
jq -S . "$BTC_EVIDENCE_DIR/restart-p2tr-after.json" \
  > "$BTC_EVIDENCE_DIR/restart-p2tr-after.canonical.json"
cmp "$BTC_EVIDENCE_DIR/restart-p2tr-before.canonical.json" \
  "$BTC_EVIDENCE_DIR/restart-p2tr-after.canonical.json"

jq -S . "$BTC_EVIDENCE_DIR/restart-events-before.json" \
  > "$BTC_EVIDENCE_DIR/restart-events-before.canonical.json"
jq -S . "$BTC_EVIDENCE_DIR/restart-events-after.json" \
  > "$BTC_EVIDENCE_DIR/restart-events-after.canonical.json"
cmp "$BTC_EVIDENCE_DIR/restart-events-before.canonical.json" \
  "$BTC_EVIDENCE_DIR/restart-events-after.canonical.json"

ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/events?after_cursor=$RESTART_LAST_CURSOR&limit=100" \
  --output "$BTC_EVIDENCE_DIR/restart-events-exclusive-after.json"
jq -e '.events | length == 0' \
  "$BTC_EVIDENCE_DIR/restart-events-exclusive-after.json"
```

## 13. Force rollback, prove restoration, and re-include

This is the only intentional chainstate mutation. It targets the recorded
collection block in this run's isolated regtest datadir. Invalidating it also
invalidates its confirmation descendant, a depth of two within the configured
retention of twenty.

```bash
btc_cli invalidateblock "$COLLECTION_OLD_BLOCK_HASH"

wait_ix_tx_kind "$COLLECTION_TXID" 'reorged' \
  "$BTC_EVIDENCE_DIR/collection-reorged.json"
export COLLECTION_REORGED_REVISION="$(jq -er '.revision' \
  "$BTC_EVIDENCE_DIR/collection-reorged.json")"
test "$COLLECTION_REORGED_REVISION" -gt "$COLLECTION_CONFIRMED_REVISION"

ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/addresses/$P2WPKH_ADDRESS/utxos?limit=100" \
  --output "$BTC_EVIDENCE_DIR/p2wpkh-utxos-restored.json"
ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/addresses/$P2TR_ADDRESS/utxos?limit=100" \
  --output "$BTC_EVIDENCE_DIR/p2tr-utxos-restored.json"

jq -e --arg gross "$P2WPKH_COLLECTION_GROSS" '
  ([.outputs[].value_sats | tonumber] | add) == ($gross | tonumber)
' "$BTC_EVIDENCE_DIR/p2wpkh-utxos-restored.json"
jq -e --arg gross "$P2TR_COLLECTION_GROSS" '
  ([.outputs[].value_sats | tonumber] | add) == ($gross | tonumber)
' "$BTC_EVIDENCE_DIR/p2tr-utxos-restored.json"

ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/events?after_cursor=$RESTART_LAST_CURSOR&limit=100" \
  --output "$BTC_EVIDENCE_DIR/events-after-reorg.json"
jq -e --arg txid "$COLLECTION_TXID" '
  any(.events[];
    .transaction.transaction_id == $txid
    and .transaction.status.kind == "reorged")
' "$BTC_EVIDENCE_DIR/events-after-reorg.json"

if ! btc_cli getmempoolentry "$COLLECTION_TXID" >/dev/null 2>&1; then
  ws_call --request POST \
    --url "http://127.0.0.1:$BTC_WS_HTTP_PORT/v1/bitcoin/transactions/broadcast" \
    --data-binary "@$BTC_REQUEST_DIR/broadcast-collection-private.json" \
    --output "$BTC_EVIDENCE_DIR/rebroadcast-collection.json"
  jq -e --arg txid "$COLLECTION_TXID" '.transaction_id == $txid' \
    "$BTC_EVIDENCE_DIR/rebroadcast-collection.json"
fi

export COLLECTION_NEW_BLOCK_HASH="$(
  btc_cli generatetoaddress 1 "$BTC_MINER_ADDRESS" | jq -er '.[0]'
)"
test "$COLLECTION_NEW_BLOCK_HASH" != "$COLLECTION_OLD_BLOCK_HASH"

wait_ix_tx_kind "$COLLECTION_TXID" 'included' \
  "$BTC_EVIDENCE_DIR/collection-reincluded.json"
export COLLECTION_REINCLUDED_REVISION="$(jq -er '.revision' \
  "$BTC_EVIDENCE_DIR/collection-reincluded.json")"
test "$COLLECTION_REINCLUDED_REVISION" -gt "$COLLECTION_REORGED_REVISION"

btc_cli generatetoaddress 1 "$BTC_MINER_ADDRESS" \
  > "$BTC_EVIDENCE_DIR/collection-reconfirmation-block.json"
wait_ix_tx_kind "$COLLECTION_TXID" 'confirmed' \
  "$BTC_EVIDENCE_DIR/collection-reconfirmed.json"
test "$(jq -er '.revision' "$BTC_EVIDENCE_DIR/collection-reconfirmed.json")" \
  -gt "$COLLECTION_REINCLUDED_REVISION"

ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/addresses/$P2WPKH_ADDRESS/utxos?limit=100" \
  --output "$BTC_EVIDENCE_DIR/p2wpkh-utxos-respent.json"
ix_call --url \
  "http://127.0.0.1:$BTC_IX_HTTP_PORT/v1/scopes/bitcoin/regtest/addresses/$P2TR_ADDRESS/utxos?limit=100" \
  --output "$BTC_EVIDENCE_DIR/p2tr-utxos-respent.json"
jq -e '.outputs | length == 0' \
  "$BTC_EVIDENCE_DIR/p2wpkh-utxos-respent.json"
jq -e '.outputs | length == 0' \
  "$BTC_EVIDENCE_DIR/p2tr-utxos-respent.json"
```

## 14. Review and redact evidence

Before reporting a result, record the Core binary version, network, genesis,
configuration bounds, command date, txids, block hashes/heights, IX revisions,
checkpoints, and each assertion outcome. It is safe to retain sanitized copies
of the following:

- Core version, chain, txindex, and fee-estimate JSON;
- IX status, transactions, UTXOs, and event pages;
- transaction IDs and block IDs; and
- the acceptance matrix below.

Do not copy the entire run root. It contains Core's RPC cookie, the descriptor
wallet, service bearer values, key locators, and exact signed transaction bytes.
Do not commit Core/WS/IX/custody logs without a separate secret and raw-body
review. The HTTP sign responses and private broadcast request files must remain
private even after their transactions have been submitted.

## 15. Complete the acceptance matrix

Mark an item passed only from the command and stored evidence named above.

| Acceptance item | Required evidence |
|---|---|
| Core 31 identity/readiness | 31.x binary output; live numeric version; regtest; genesis; unpruned; not IBD; blocks equal headers; txindex synced/current |
| P2WPKH | Core validates witness v0; watched funding becomes Included then Confirmed; WS signs the selected input |
| P2TR key path | Core validates witness v1; watched funding becomes Included then Confirmed; batch signing succeeds with the P2TR source |
| Sign before broadcast | Transfer and collection txids are absent from Core/mempool after signing |
| Exact-byte broadcast | Persisted txid/raw pair is submitted separately; WS and Core return the same txid |
| Zero-confirmation receipt | Core-known mempool transaction returns a receipt with `confirmations=0` and no block reference |
| Batch attribution | One destination output; two source rows; each gross row equals selected source inputs; output plus fee equals all inputs |
| Restart/replay | Same IX database restores checkpoint, UTXO snapshots, event rows, and exclusive-after cursor behavior; WS restarts while custody remains alive |
| Controlled rollback | `invalidateblock` produces a later `Reorged` revision within retention |
| UTXO restoration | Every collection input returns to the block-only IX projection during rollback |
| Re-inclusion | Same txid enters a different block, receives a later Included revision, and becomes Confirmed again |

This manual matrix and the final locked Rust validation commands are separate.
Neither one may be inferred from the other. Current composed acceptance comes
from the automated suite's eight isolated Core 31.1 executions; these rows
remain the step-by-step diagnostic equivalent.

## 16. Stop processes and remove only the marked run root

Stop application processes in reverse dependency order, then ask this run's
Core node to stop. `kill -0` guards each PID; no broad process-name kill is used.

```bash
if test -n "${BTC_WS_PID:-}" && kill -0 "$BTC_WS_PID" 2>/dev/null; then
  kill -INT "$BTC_WS_PID"
  wait "$BTC_WS_PID" || true
fi
if test -n "${BTC_IX_PID:-}" && kill -0 "$BTC_IX_PID" 2>/dev/null; then
  kill -INT "$BTC_IX_PID"
  wait "$BTC_IX_PID" || true
fi
if test -n "${BTC_CUSTODY_PID:-}" && kill -0 "$BTC_CUSTODY_PID" 2>/dev/null; then
  kill -INT "$BTC_CUSTODY_PID"
  wait "$BTC_CUSTODY_PID" || true
fi
if test -n "${BTC_CORE_PID:-}" && kill -0 "$BTC_CORE_PID" 2>/dev/null; then
  btc_cli stop >/dev/null
  wait "$BTC_CORE_PID" || true
fi
```

Copy only individually reviewed, sanitized evidence if it must survive. Then
resolve and validate the exact directory plus its unique marker before removal.
The deletion command receives one explicit path and no glob or unresolved
environment variable.

```bash
export BTC_REAL_RUN_ROOT="$(cd "$BTC_RUN_ROOT" && pwd -P)"
test -n "$BTC_REAL_RUN_ROOT"
test -f "$BTC_REAL_RUN_ROOT/.payment-sdk-bitcoin-regtest-run"
test "$(< "$BTC_REAL_RUN_ROOT/.payment-sdk-bitcoin-regtest-run")" = "$BTC_RUN_ID"
case "$BTC_REAL_RUN_ROOT" in
  */payment-sdk-btc31.*) ;;
  *) printf '%s\n' 'refusing to remove an unexpected run root' >&2; false ;;
esac

find "$BTC_REAL_RUN_ROOT" -depth -mindepth 1 -delete
rmdir "$BTC_REAL_RUN_ROOT"
```

After a manual real-node run, report the exact Core 31 version and sanitized
evidence in the implementation handoff. Do not copy cookies, tokens, key
locators, or signed transaction bytes from the private run root.
