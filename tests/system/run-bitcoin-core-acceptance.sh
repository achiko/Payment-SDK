#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"

: "${BITCOIND:?Set BITCOIND to an absolute path to Bitcoin Core 31.x bitcoind}"
: "${BITCOIN_CLI:?Set BITCOIN_CLI to an absolute path to the matching bitcoin-cli}"

case "$BITCOIND" in /*) ;; *) echo "BITCOIND must be an absolute path" >&2; exit 2;; esac
case "$BITCOIN_CLI" in /*) ;; *) echo "BITCOIN_CLI must be an absolute path" >&2; exit 2;; esac
test -x "$BITCOIND"
test -x "$BITCOIN_CLI"

cd "$repo_root"
exec cargo test --locked -p system-tests \
  --features live-bitcoin-core \
  --test bitcoin_core \
  -- --ignored --exact real_wallet_broadcast_is_indexed_on_disposable_regtest --nocapture
