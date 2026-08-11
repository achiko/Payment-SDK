#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"

: "${BITCOIND:?Set BITCOIND to the explicit Bitcoin Core 31.1 bitcoind path}"
: "${BITCOIN_CLI:?Set BITCOIN_CLI to the explicit Bitcoin Core 31.1 bitcoin-cli path}"

test -x "$BITCOIND"
test -x "$BITCOIN_CLI"

cd "$REPO_ROOT"
cargo build --locked \
  -p bitcoin-regtest-acceptance \
  -p custody-worker \
  -p indexer-worker \
  -p wallet-worker \
  -p payment-api

exec "$REPO_ROOT/target/debug/bitcoin-regtest-acceptance" \
  --bitcoind "$BITCOIND" \
  --bitcoin-cli "$BITCOIN_CLI" \
  "$@"
