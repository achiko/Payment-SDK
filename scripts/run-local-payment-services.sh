#!/usr/bin/env bash

# Start the local Indexer, ephemeral custody, Wallet, and Payment services.
# The Ethereum JSON-RPC node is an external prerequisite and is never started
# or stopped by this script.

set -euo pipefail
# Keep each background service out of the launcher's foreground process group.
# Terminal Ctrl-C then reaches this supervisor first, which drains children in
# dependency order by their exact PIDs.
set -m

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPOSITORY_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

NO_BUILD=0
DISPOSABLE_POLICY=0
STACK_ROOT=""
STACK_ROOT_DISPLAY=""
CLEANUP_STARTED=0
SERVICE_NAMES=()
SERVICE_PIDS=()
SERVICE_LOGS=()

usage() {
  cat <<'EOF'
Usage:
  ./scripts/run-local-payment-services.sh [--no-build] [--disposable-policy]

Starts these repository-local processes:
  1. Indexer Service       http://127.0.0.1:8080
  2. Ephemeral custody     http://127.0.0.1:8181
  3. Wallet Service        http://127.0.0.1:8082
  4. Payment Service       http://127.0.0.1:8081

The script does NOT start Anvil or any other Ethereum node. An unauthenticated
loopback HTTP JSON-RPC endpoint must already be running.

Main environment variables:
  STRICT_AUTHENTICATION_MODE       required exact true/false; no default
  ETHEREUM_RPC_URL                 default: http://127.0.0.1:8545
  ETHEREUM_CHAIN_ID                optional expected chain ID
  PAYMENT_NETWORK                  default: local
  IX_BOOTSTRAP_HEIGHT              default: 0 (disposable local chains only)
  IX_STARTUP_TIMEOUT_SECONDS       default: 180
  SERVICE_STARTUP_TIMEOUT_SECONDS  default: 60
  STACK_SHUTDOWN_TIMEOUT_SECONDS   default: 25 per service
  STACK_POLICY_TEMPLATE            reviewed policy JSON; required unless
                                   --disposable-policy is present
  STACK_MASTER_DESTINATION         optional reviewed local master address for
                                   --disposable-policy; otherwise the first
                                   eth_accounts address is used
  RUST_LOG                         default: info

Optional bind overrides use STACK_ prefixes:
  STACK_IX_HTTP_BIND, STACK_IX_METRICS_BIND, STACK_CUSTODY_BIND,
  STACK_CUSTODY_METRICS_BIND, STACK_WS_HTTP_BIND, STACK_WS_METRICS_BIND,
  STACK_PS_HTTP_BIND, STACK_PS_METRICS_BIND

The launcher creates fresh private IX/PS databases, logs, a runtime policy,
under ./tmp/payment-sdk-stack.XXXXXX. Strict mode also creates local
credentials, writes them to client.env with mode 600, and never prints them.

Options:
  --no-build          Reuse already-built debug binaries.
  --disposable-policy Explicitly permit a generated native-only test policy.
                      Never use this option with real funds.
  -h, --help          Show this help.
EOF
}

info() {
  printf '[stack] %s\n' "$*"
}

warn() {
  printf '[stack] WARNING: %s\n' "$*" >&2
}

die() {
  printf '[stack] ERROR: %s\n' "$*" >&2
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-build)
      NO_BUILD=1
      ;;
    --disposable-policy)
      DISPOSABLE_POLICY=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      die "unknown argument: $1"
      ;;
  esac
  shift
done

# Capture optional strict-mode inputs once, then remove every service-facing
# credential alias before invoking Cargo, curl, or a child service. The copied
# values are deliberately non-exported and are scoped again per child below.
STACK_CUSTODY_BEARER_TOKEN_INPUT="${STACK_CUSTODY_BEARER_TOKEN-}"
STACK_WALLET_BEARER_TOKEN_INPUT="${STACK_WALLET_BEARER_TOKEN-}"
STACK_INDEXER_BEARER_TOKEN_INPUT="${STACK_INDEXER_BEARER_TOKEN-}"
STACK_PS_API_BEARER_TOKEN_INPUT="${STACK_PS_API_BEARER_TOKEN-}"
STACK_PS_ADMIN_BEARER_TOKEN_INPUT="${STACK_PS_ADMIN_BEARER_TOKEN-}"
export -n STACK_CUSTODY_BEARER_TOKEN_INPUT STACK_WALLET_BEARER_TOKEN_INPUT
export -n STACK_INDEXER_BEARER_TOKEN_INPUT STACK_PS_API_BEARER_TOKEN_INPUT
export -n STACK_PS_ADMIN_BEARER_TOKEN_INPUT
unset STACK_CUSTODY_BEARER_TOKEN STACK_WALLET_BEARER_TOKEN
unset STACK_INDEXER_BEARER_TOKEN STACK_PS_API_BEARER_TOKEN
unset STACK_PS_ADMIN_BEARER_TOKEN
unset CUSTODY_BEARER_TOKEN IX_BEARER_TOKEN WS_BEARER_TOKEN
unset WS_CUSTODY_BEARER_TOKEN WS_BITCOIN_IX_BEARER_TOKEN
unset PS_API_BEARER_TOKEN PS_ADMIN_BEARER_TOKEN
unset PS_INDEXER_BEARER_TOKEN PS_WALLET_BEARER_TOKEN
unset CUSTODY_BEARER_TOKEN_VALUE WALLET_BEARER_TOKEN
unset INDEXER_BEARER_TOKEN_VALUE PS_API_BEARER_TOKEN_VALUE
unset PS_ADMIN_BEARER_TOKEN_VALUE

require_command() {
  local command_name="$1"
  command -v "$command_name" >/dev/null 2>&1 \
    || die "required command is not installed: $command_name"
}

require_positive_integer() {
  local name="$1"
  local value="$2"
  case "$value" in
    ''|*[!0-9]*) die "$name must be a positive integer" ;;
  esac
  case "$value" in
    [1-9]*) ;;
    *) die "$name must be a canonical positive decimal integer" ;;
  esac
}

validate_network_slug() {
  local value="$1"
  [[ -n "$value" ]] || die "PAYMENT_NETWORK must not be empty"
  case "$value" in
    *[[:space:]]*) die "PAYMENT_NETWORK must not contain whitespace" ;;
  esac
}

validate_canonical_ethereum_address() {
  local name="$1"
  local value="$2"
  [[ "$value" =~ ^0x[0-9a-f]{40}$ ]] \
    || die "$name must be a lowercase canonical 20-byte Ethereum address"
}

validate_loopback_bind() {
  local name="$1"
  local value="$2"
  local port
  local port_number

  case "$value" in
    127.0.0.1:*) ;;
    *) die "$name must use a 127.0.0.1:<port> loopback bind" ;;
  esac

  port="${value##*:}"
  case "$port" in
    ''|*[!0-9]*) die "$name contains an invalid port" ;;
  esac
  case "$port" in
    0|0*) die "$name port must not contain leading zeros or be zero" ;;
  esac
  port_number=$((10#$port))
  [[ "$port_number" -le 65535 ]] \
    || die "$name port must be between 1 and 65535"
}

port_from_bind() {
  printf '%s' "${1##*:}"
}

ensure_port_is_free() {
  local service_name="$1"
  local port="$2"
  local owner

  if owner="$(lsof -nP -iTCP:"$port" -sTCP:LISTEN 2>/dev/null)"; then
    printf '[stack] ERROR: %s cannot start because TCP port %s is already in use.\n' \
      "$service_name" "$port" >&2
    printf '%s\n' "$owner" >&2
    die "stop or reconfigure the known process; the launcher will not kill it"
  fi
}

relative_to_repository() {
  local path="$1"
  case "$path" in
    "$REPOSITORY_ROOT"/*) printf './%s' "${path#"$REPOSITORY_ROOT"/}" ;;
    *) printf '%s' "$path" ;;
  esac
}

show_log_tail() {
  local service_name="$1"
  local log_path="$2"
  warn "$service_name log tail ($(relative_to_repository "$log_path")) follows"
  tail -n 40 "$log_path" >&2 2>/dev/null || true
}

register_service() {
  local service_name="$1"
  local pid="$2"
  local log_path="$3"
  local index="${#SERVICE_PIDS[@]}"

  SERVICE_NAMES[$index]="$service_name"
  SERVICE_PIDS[$index]="$pid"
  SERVICE_LOGS[$index]="$log_path"
  printf '%s\t%s\n' "$service_name" "$pid" >>"$STACK_ROOT/pids"
  info "$service_name started (PID $pid, log $(relative_to_repository "$log_path"))"
}

stop_service() {
  local index="$1"
  local pid="${SERVICE_PIDS[$index]}"
  local service_name="${SERVICE_NAMES[$index]}"
  local deadline

  if kill -0 "$pid" 2>/dev/null; then
    info "stopping $service_name"
    # IX currently handles SIGINT only; all four local services handle it.
    kill -INT "$pid" 2>/dev/null || true
    deadline=$((SECONDS + STACK_SHUTDOWN_TIMEOUT_SECONDS))
    while kill -0 "$pid" 2>/dev/null && [[ "$SECONDS" -lt "$deadline" ]]; do
      sleep 1
    done
  fi

  if kill -0 "$pid" 2>/dev/null; then
    warn "$service_name did not stop after SIGINT; sending SIGTERM"
    kill -TERM "$pid" 2>/dev/null || true
    sleep 1
  fi
  if kill -0 "$pid" 2>/dev/null; then
    warn "$service_name did not stop after SIGTERM; sending SIGKILL"
    kill -KILL "$pid" 2>/dev/null || true
  fi
  wait "$pid" 2>/dev/null || true
}

cleanup() {
  local exit_status=$?
  local index

  if [[ "$CLEANUP_STARTED" -eq 1 ]]; then
    return "$exit_status"
  fi
  CLEANUP_STARTED=1
  trap - EXIT
  trap '' INT TERM
  set +e

  if [[ "${#SERVICE_PIDS[@]}" -gt 0 ]]; then
    info "stopping services in reverse dependency order"
    for ((index=${#SERVICE_PIDS[@]} - 1; index >= 0; index--)); do
      stop_service "$index"
    done
  fi

  if [[ -n "$STACK_ROOT_DISPLAY" ]]; then
    info "runtime data and logs preserved at $STACK_ROOT_DISPLAY"
    warn "custody keys are now gone; do not reuse this run's databases or policy"
  fi
  return "$exit_status"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

curl_service() {
  local token="$1"
  shift
  if [[ "$AUTHENTICATION_MODE_VALUE" == true ]]; then
    printf 'header = "Authorization: Bearer %s"\n' "$token" \
      | curl --disable --config - --noproxy '*' "$@"
  else
    curl --disable --noproxy '*' "$@"
  fi
}

rpc_request() {
  local payload="$1"
  curl --disable --noproxy '*' --fail --silent --show-error \
    --connect-timeout 2 \
    --max-time 10 \
    --header 'Content-Type: application/json' \
    --data "$payload" \
    "$ETHEREUM_RPC_URL"
}

json_rpc_result() {
  jq --exit-status --raw-output \
    'if (.error? != null) or (.result? == null) then empty else .result end'
}

http_status_is() {
  local url="$1"
  local expected="$2"
  local token="${3:-}"
  local response

  if [[ -n "$token" ]]; then
    response="$(curl_service "$token" --fail --silent --show-error \
      --connect-timeout 2 --max-time 3 \
      "$url" 2>/dev/null)" || return 1
  else
    response="$(curl --disable --noproxy '*' --fail --silent --show-error \
      --connect-timeout 2 --max-time 3 \
      "$url" 2>/dev/null)" || return 1
  fi

  printf '%s' "$response" \
    | jq --exit-status \
      --arg expected "$expected" \
      --arg mode "$AUTHENTICATION_MODE_NAME" \
      '(.status == $expected) and (.authentication_mode == $mode)' \
      >/dev/null 2>&1
}

wait_for_service() {
  local service_name="$1"
  local pid="$2"
  local log_path="$3"
  local timeout_seconds="$4"
  local probe_function="$5"
  local deadline=$((SECONDS + timeout_seconds))
  local exit_code

  while [[ "$SECONDS" -lt "$deadline" ]]; do
    if "$probe_function"; then
      info "$service_name is ready"
      return 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      exit_code=0
      wait "$pid" || exit_code=$?
      warn "$service_name exited during startup with status $exit_code"
      show_log_tail "$service_name" "$log_path"
      return 1
    fi
    sleep 1
  done

  warn "$service_name did not become ready within ${timeout_seconds}s"
  show_log_tail "$service_name" "$log_path"
  return 1
}

provision_address() {
  local operation_id="$1"
  local purpose="$2"
  local payload

  payload="$(jq --compact-output --null-input \
    --arg operation_id "$operation_id" \
    --arg purpose "$purpose" \
    '{operation_id: $operation_id, asset: {kind: "native"}, key_purpose: $purpose}')"

  curl_service "$WALLET_BEARER_TOKEN" \
    --fail --silent --show-error \
    --connect-timeout 2 --max-time 15 \
    --request POST \
    --header 'Content-Type: application/json' \
    --data "$payload" \
    "$WS_URL/v1/ethereum/addresses"
}

generate_token() {
  openssl rand -hex 32
}

validate_bearer_value() {
  local name="$1"
  local value="$2"
  [[ "$value" =~ ^[-A-Za-z0-9._~]+$ ]] \
    || die "$name must contain only letters, digits, dot, underscore, tilde, or hyphen"
}

write_export() {
  local name="$1"
  local value="$2"
  printf 'export %s=%q\n' "$name" "$value"
}

cd "$REPOSITORY_ROOT"

for required in bash cargo curl jq lsof mktemp tail; do
  require_command "$required"
done

ETHEREUM_RPC_URL="${ETHEREUM_RPC_URL:-http://127.0.0.1:8545}"
AUTHENTICATION_MODE_VALUE="${STRICT_AUTHENTICATION_MODE-}"
case "$AUTHENTICATION_MODE_VALUE" in
  true) AUTHENTICATION_MODE_NAME='strict' ;;
  false) AUTHENTICATION_MODE_NAME='global_trusted' ;;
  *) die "STRICT_AUTHENTICATION_MODE must be exactly true or false" ;;
esac
if [[ "$AUTHENTICATION_MODE_VALUE" == true ]]; then
  require_command openssl
fi
export STRICT_AUTHENTICATION_MODE="$AUTHENTICATION_MODE_VALUE"
if [[ "$AUTHENTICATION_MODE_VALUE" == false ]]; then
  unset CUSTODY_BEARER_TOKEN IX_BEARER_TOKEN WS_BEARER_TOKEN
  unset WS_CUSTODY_BEARER_TOKEN WS_BITCOIN_IX_BEARER_TOKEN
  unset PS_API_BEARER_TOKEN PS_ADMIN_BEARER_TOKEN
  unset PS_INDEXER_BEARER_TOKEN PS_WALLET_BEARER_TOKEN
fi
PAYMENT_NETWORK="${PAYMENT_NETWORK:-local}"
IX_BOOTSTRAP_HEIGHT="${IX_BOOTSTRAP_HEIGHT:-0}"
IX_STARTUP_TIMEOUT_SECONDS="${IX_STARTUP_TIMEOUT_SECONDS:-180}"
SERVICE_STARTUP_TIMEOUT_SECONDS="${SERVICE_STARTUP_TIMEOUT_SECONDS:-60}"
STACK_SHUTDOWN_TIMEOUT_SECONDS="${STACK_SHUTDOWN_TIMEOUT_SECONDS:-25}"
STACK_POLICY_TEMPLATE="${STACK_POLICY_TEMPLATE:-}"
STACK_MASTER_DESTINATION="${STACK_MASTER_DESTINATION:-}"
STACK_RUST_LOG="${RUST_LOG:-info}"
NO_PROXY='127.0.0.1,localhost'
no_proxy="$NO_PROXY"
export NO_PROXY no_proxy

IX_HTTP_BIND_VALUE="${STACK_IX_HTTP_BIND:-127.0.0.1:8080}"
IX_METRICS_BIND_VALUE="${STACK_IX_METRICS_BIND:-127.0.0.1:9090}"
CUSTODY_BIND_VALUE="${STACK_CUSTODY_BIND:-127.0.0.1:8181}"
CUSTODY_METRICS_BIND_VALUE="${STACK_CUSTODY_METRICS_BIND:-127.0.0.1:9093}"
WS_HTTP_BIND_VALUE="${STACK_WS_HTTP_BIND:-127.0.0.1:8082}"
WS_METRICS_BIND_VALUE="${STACK_WS_METRICS_BIND:-127.0.0.1:9092}"
PS_HTTP_BIND_VALUE="${STACK_PS_HTTP_BIND:-127.0.0.1:8081}"
PS_METRICS_BIND_VALUE="${STACK_PS_METRICS_BIND:-127.0.0.1:9091}"

if [[ "$DISPOSABLE_POLICY" -eq 1 && -n "$STACK_POLICY_TEMPLATE" ]]; then
  die "use either STACK_POLICY_TEMPLATE or --disposable-policy, not both"
fi
if [[ "$DISPOSABLE_POLICY" -eq 0 && -z "$STACK_POLICY_TEMPLATE" ]]; then
  die "set STACK_POLICY_TEMPLATE to a reviewed policy, or explicitly use --disposable-policy for a no-funds local test"
fi

case "$ETHEREUM_RPC_URL" in
  http://127.0.0.1|http://127.0.0.1:*|http://localhost|http://localhost:*) ;;
  *)
    die "ETHEREUM_RPC_URL must be an unauthenticated loopback HTTP endpoint; this launcher is local-only"
    ;;
esac
case "$ETHEREUM_RPC_URL" in
  *'@'*|*'?'*|*'#'*)
    die "ETHEREUM_RPC_URL must not contain credentials, a query, or a fragment"
    ;;
esac

validate_network_slug "$PAYMENT_NETWORK"
require_positive_integer IX_STARTUP_TIMEOUT_SECONDS "$IX_STARTUP_TIMEOUT_SECONDS"
require_positive_integer SERVICE_STARTUP_TIMEOUT_SECONDS "$SERVICE_STARTUP_TIMEOUT_SECONDS"
require_positive_integer STACK_SHUTDOWN_TIMEOUT_SECONDS "$STACK_SHUTDOWN_TIMEOUT_SECONDS"
case "$IX_BOOTSTRAP_HEIGHT" in
  ''|*[!0-9]*) die "IX_BOOTSTRAP_HEIGHT must be a non-negative integer" ;;
esac
case "$IX_BOOTSTRAP_HEIGHT" in
  0|[1-9]*) ;;
  *) die "IX_BOOTSTRAP_HEIGHT must be a canonical decimal integer without leading zeros" ;;
esac

validate_loopback_bind STACK_IX_HTTP_BIND "$IX_HTTP_BIND_VALUE"
validate_loopback_bind STACK_IX_METRICS_BIND "$IX_METRICS_BIND_VALUE"
validate_loopback_bind STACK_CUSTODY_BIND "$CUSTODY_BIND_VALUE"
validate_loopback_bind STACK_CUSTODY_METRICS_BIND "$CUSTODY_METRICS_BIND_VALUE"
validate_loopback_bind STACK_WS_HTTP_BIND "$WS_HTTP_BIND_VALUE"
validate_loopback_bind STACK_WS_METRICS_BIND "$WS_METRICS_BIND_VALUE"
validate_loopback_bind STACK_PS_HTTP_BIND "$PS_HTTP_BIND_VALUE"
validate_loopback_bind STACK_PS_METRICS_BIND "$PS_METRICS_BIND_VALUE"

OWNED_SERVICE_NAMES=(
  "Indexer API"
  "Indexer metrics"
  "Custody API"
  "Custody metrics"
  "Wallet API"
  "Wallet metrics"
  "Payment API"
  "Payment metrics"
)
OWNED_PORTS=(
  "$(port_from_bind "$IX_HTTP_BIND_VALUE")"
  "$(port_from_bind "$IX_METRICS_BIND_VALUE")"
  "$(port_from_bind "$CUSTODY_BIND_VALUE")"
  "$(port_from_bind "$CUSTODY_METRICS_BIND_VALUE")"
  "$(port_from_bind "$WS_HTTP_BIND_VALUE")"
  "$(port_from_bind "$WS_METRICS_BIND_VALUE")"
  "$(port_from_bind "$PS_HTTP_BIND_VALUE")"
  "$(port_from_bind "$PS_METRICS_BIND_VALUE")"
)

for ((i=0; i<${#OWNED_PORTS[@]}; i++)); do
  for ((j=i + 1; j<${#OWNED_PORTS[@]}; j++)); do
    if [[ "${OWNED_PORTS[$i]}" == "${OWNED_PORTS[$j]}" ]]; then
      die "${OWNED_SERVICE_NAMES[$i]} and ${OWNED_SERVICE_NAMES[$j]} use the same port"
    fi
  done
  ensure_port_is_free "${OWNED_SERVICE_NAMES[$i]}" "${OWNED_PORTS[$i]}"
done

info "probing the existing Ethereum RPC (the node is not managed by this script)"
CHAIN_RESPONSE="$(rpc_request \
  '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}')" \
  || die "the configured Ethereum RPC did not answer eth_chainId"
CHAIN_HEX="$(printf '%s' "$CHAIN_RESPONSE" | json_rpc_result)" \
  || die "the configured Ethereum RPC returned an invalid eth_chainId response"
case "$CHAIN_HEX" in
  0x*|0X*) ;;
  *) die "the configured Ethereum RPC returned a non-hexadecimal chain ID" ;;
esac
CHAIN_DIGITS="${CHAIN_HEX#0x}"
CHAIN_DIGITS="${CHAIN_DIGITS#0X}"
case "$CHAIN_DIGITS" in
  ''|*[!0-9a-fA-F]*) die "the configured Ethereum RPC returned an invalid chain ID" ;;
esac
DISCOVERED_CHAIN_ID=$((16#$CHAIN_DIGITS))
[[ "$DISCOVERED_CHAIN_ID" -gt 0 ]] || die "Ethereum chain ID must be greater than zero"

if [[ -n "${ETHEREUM_CHAIN_ID:-}" ]]; then
  require_positive_integer ETHEREUM_CHAIN_ID "$ETHEREUM_CHAIN_ID"
  [[ "$ETHEREUM_CHAIN_ID" -eq "$DISCOVERED_CHAIN_ID" ]] \
    || die "ETHEREUM_CHAIN_ID does not match the configured RPC"
fi
CHAIN_ID="$DISCOVERED_CHAIN_ID"

GENESIS_RESPONSE="$(rpc_request \
  '{"jsonrpc":"2.0","id":2,"method":"eth_getBlockByNumber","params":["0x0",false]}')" \
  || die "the configured Ethereum RPC did not return block zero"
GENESIS_HASH="$(printf '%s' "$GENESIS_RESPONSE" \
  | jq --exit-status --raw-output \
    'if (.error? != null) or (.result.hash? == null) then empty else .result.hash end')" \
  || die "the configured Ethereum RPC returned an invalid block-zero response"
case "$GENESIS_HASH" in
  0x????????????????????????????????????????????????????????????????) ;;
  *) die "the configured Ethereum RPC returned an invalid block-zero hash" ;;
esac
case "${GENESIS_HASH#0x}" in
  *[!0-9a-fA-F]*) die "the configured Ethereum RPC returned a non-hexadecimal block-zero hash" ;;
esac
info "Ethereum RPC preflight passed (chain ID $CHAIN_ID, network slug $PAYMENT_NETWORK)"

if [[ -n "$STACK_POLICY_TEMPLATE" ]]; then
  [[ -f "$STACK_POLICY_TEMPLATE" ]] \
    || die "STACK_POLICY_TEMPLATE must identify a regular JSON file"
  jq --exit-status \
    --arg network "$PAYMENT_NETWORK" \
    --argjson chain_id "$CHAIN_ID" \
    '(.scope.chain == "ethereum")
      and (.scope.network == $network)
      and (.scope.chain_id == $chain_id)
      and (.assets | type == "array" and length > 0)
      and (.gas_funder | type == "object")' \
    "$STACK_POLICY_TEMPLATE" >/dev/null \
    || die "policy template scope/assets do not match this launcher run"
else
  if [[ -z "$STACK_MASTER_DESTINATION" ]]; then
    ACCOUNTS_RESPONSE="$(rpc_request \
      '{"jsonrpc":"2.0","id":3,"method":"eth_accounts","params":[]}')" \
      || die "the configured Ethereum RPC did not answer eth_accounts"
    STACK_MASTER_DESTINATION="$(printf '%s' "$ACCOUNTS_RESPONSE" \
      | jq --exit-status --raw-output \
        'if (.error? != null) or ((.result? | type) != "array") or (.result | length == 0)
         then empty else (.result[0] | ascii_downcase) end')" \
      || die "--disposable-policy needs STACK_MASTER_DESTINATION or at least one local eth_accounts address"
  fi
  validate_canonical_ethereum_address \
    STACK_MASTER_DESTINATION "$STACK_MASTER_DESTINATION"
  warn "using an explicitly authorized disposable native-only policy; never fund it with real assets"
fi

if [[ "$NO_BUILD" -eq 0 ]]; then
  info "building IX, custody, WS, and PS once with locked dependencies"
  cargo build --locked \
    -p indexer-worker \
    -p custody-worker \
    -p wallet-worker \
    -p payment-api
fi

TARGET_DIRECTORY="$(cargo metadata --locked --format-version 1 --no-deps \
  | jq --exit-status --raw-output '.target_directory')"
INDEXER_BINARY="$TARGET_DIRECTORY/debug/indexer-worker"
CUSTODY_BINARY="$TARGET_DIRECTORY/debug/custody-worker"
WALLET_BINARY="$TARGET_DIRECTORY/debug/wallet-worker"
PAYMENT_BINARY="$TARGET_DIRECTORY/debug/payment-api"

for binary in "$INDEXER_BINARY" "$CUSTODY_BINARY" "$WALLET_BINARY" "$PAYMENT_BINARY"; do
  [[ -x "$binary" ]] || die "missing executable $binary; rerun without --no-build"
done

umask 077
mkdir -p "$REPOSITORY_ROOT/tmp"
STACK_ROOT="$(mktemp -d "$REPOSITORY_ROOT/tmp/payment-sdk-stack.XXXXXX")"
STACK_ROOT_DISPLAY="$(relative_to_repository "$STACK_ROOT")"
mkdir -p "$STACK_ROOT/ix-db" "$STACK_ROOT/ps-db" "$STACK_ROOT/logs"
: >"$STACK_ROOT/pids"

CUSTODY_BEARER_TOKEN_VALUE=""
WALLET_BEARER_TOKEN=""
INDEXER_BEARER_TOKEN_VALUE=""
PS_API_BEARER_TOKEN_VALUE=""
PS_ADMIN_BEARER_TOKEN_VALUE=""
if [[ "$AUTHENTICATION_MODE_VALUE" == true ]]; then
  CUSTODY_BEARER_TOKEN_VALUE="${STACK_CUSTODY_BEARER_TOKEN_INPUT:-$(generate_token)}"
  WALLET_BEARER_TOKEN="${STACK_WALLET_BEARER_TOKEN_INPUT:-$(generate_token)}"
  INDEXER_BEARER_TOKEN_VALUE="${STACK_INDEXER_BEARER_TOKEN_INPUT:-$(generate_token)}"
  PS_API_BEARER_TOKEN_VALUE="${STACK_PS_API_BEARER_TOKEN_INPUT:-$(generate_token)}"
  PS_ADMIN_BEARER_TOKEN_VALUE="${STACK_PS_ADMIN_BEARER_TOKEN_INPUT:-$(generate_token)}"

  validate_bearer_value STACK_CUSTODY_BEARER_TOKEN "$CUSTODY_BEARER_TOKEN_VALUE"
  validate_bearer_value STACK_WALLET_BEARER_TOKEN "$WALLET_BEARER_TOKEN"
  validate_bearer_value STACK_INDEXER_BEARER_TOKEN "$INDEXER_BEARER_TOKEN_VALUE"
  validate_bearer_value STACK_PS_API_BEARER_TOKEN "$PS_API_BEARER_TOKEN_VALUE"
  validate_bearer_value STACK_PS_ADMIN_BEARER_TOKEN "$PS_ADMIN_BEARER_TOKEN_VALUE"
  [[ "$PS_API_BEARER_TOKEN_VALUE" != "$PS_ADMIN_BEARER_TOKEN_VALUE" ]] \
    || die "Payment Service API and administrator tokens must be different"
fi

# Keep each credential scoped to the one child subshell that explicitly
# exports it. Values copied into these internal variables must not retain an
# inherited export attribute.
export -n CUSTODY_BEARER_TOKEN_VALUE WALLET_BEARER_TOKEN
export -n INDEXER_BEARER_TOKEN_VALUE PS_API_BEARER_TOKEN_VALUE
export -n PS_ADMIN_BEARER_TOKEN_VALUE
unset STACK_CUSTODY_BEARER_TOKEN_INPUT STACK_WALLET_BEARER_TOKEN_INPUT
unset STACK_INDEXER_BEARER_TOKEN_INPUT STACK_PS_API_BEARER_TOKEN_INPUT
unset STACK_PS_ADMIN_BEARER_TOKEN_INPUT

IX_URL="http://$IX_HTTP_BIND_VALUE"
CUSTODY_URL="http://$CUSTODY_BIND_VALUE"
WS_URL="http://$WS_HTTP_BIND_VALUE"
PS_URL="http://$PS_HTTP_BIND_VALUE"

CLIENT_ENV_PATH="$STACK_ROOT/client.env"
{
  write_export PAYMENT_STACK_ROOT "$STACK_ROOT"
  write_export STRICT_AUTHENTICATION_MODE "$AUTHENTICATION_MODE_VALUE"
  write_export ETHEREUM_CHAIN_ID "$CHAIN_ID"
  write_export PAYMENT_NETWORK "$PAYMENT_NETWORK"
  write_export IX_URL "$IX_URL"
  write_export CUSTODY_URL "$CUSTODY_URL"
  write_export WS_URL "$WS_URL"
  write_export PS_URL "$PS_URL"
  if [[ "$AUTHENTICATION_MODE_VALUE" == true ]]; then
    write_export IX_BEARER_TOKEN "$INDEXER_BEARER_TOKEN_VALUE"
    write_export CUSTODY_BEARER_TOKEN "$CUSTODY_BEARER_TOKEN_VALUE"
    write_export WS_BEARER_TOKEN "$WALLET_BEARER_TOKEN"
    write_export PS_API_BEARER_TOKEN "$PS_API_BEARER_TOKEN_VALUE"
    write_export PS_ADMIN_BEARER_TOKEN "$PS_ADMIN_BEARER_TOKEN_VALUE"
  fi
} >"$CLIENT_ENV_PATH"
chmod 600 "$CLIENT_ENV_PATH" "$STACK_ROOT/pids"

IX_LOG="$STACK_ROOT/logs/indexer.log"
CUSTODY_LOG="$STACK_ROOT/logs/custody.log"
WS_LOG="$STACK_ROOT/logs/wallet.log"
PS_LOG="$STACK_ROOT/logs/payment.log"

(
  export IX_DATABASE_PATH="$STACK_ROOT/ix-db"
  export IX_NETWORK="$PAYMENT_NETWORK"
  export IX_BOOTSTRAP_HEIGHT
  export IX_EXPECTED_CHAIN_ID="$CHAIN_ID"
  export IX_EXPECTED_GENESIS_HASH="$GENESIS_HASH"
  export IX_RPC_HTTP_URL="$ETHEREUM_RPC_URL"
  export IX_HTTP_BIND="$IX_HTTP_BIND_VALUE"
  export IX_METRICS_BIND="$IX_METRICS_BIND_VALUE"
  if [[ "$AUTHENTICATION_MODE_VALUE" == true ]]; then
    export IX_BEARER_TOKEN="$INDEXER_BEARER_TOKEN_VALUE"
  fi
  export RUST_LOG="$STACK_RUST_LOG"
  exec "$INDEXER_BINARY" serve
) >"$IX_LOG" 2>&1 &
IX_PID=$!
register_service "Indexer Service" "$IX_PID" "$IX_LOG"

(
  export CUSTODY_BIND="$CUSTODY_BIND_VALUE"
  export CUSTODY_METRICS_BIND="$CUSTODY_METRICS_BIND_VALUE"
  if [[ "$AUTHENTICATION_MODE_VALUE" == true ]]; then
    export CUSTODY_BEARER_TOKEN="$CUSTODY_BEARER_TOKEN_VALUE"
  fi
  export CUSTODY_SHUTDOWN_GRACE_SECONDS=10
  export RUST_LOG="$STACK_RUST_LOG"
  exec "$CUSTODY_BINARY" serve
) >"$CUSTODY_LOG" 2>&1 &
CUSTODY_PID=$!
register_service "Ephemeral custody" "$CUSTODY_PID" "$CUSTODY_LOG"

probe_indexer() {
  http_status_is "$IX_URL/health/ready" ready
}

probe_custody() {
  http_status_is "$CUSTODY_URL/v1/readiness" available "$CUSTODY_BEARER_TOKEN_VALUE"
}

wait_for_service "Ephemeral custody" "$CUSTODY_PID" "$CUSTODY_LOG" \
  "$SERVICE_STARTUP_TIMEOUT_SECONDS" probe_custody
wait_for_service "Indexer Service" "$IX_PID" "$IX_LOG" \
  "$IX_STARTUP_TIMEOUT_SECONDS" probe_indexer

(
  export WS_ETHEREUM_CHAIN_ID="$CHAIN_ID"
  export WS_ETHEREUM_RPC_URL="$ETHEREUM_RPC_URL"
  export WS_CUSTODY_URL="$CUSTODY_URL"
  export WS_CUSTODY_AUTHENTICATION_POLICY='repository_mode_matched'
  if [[ "$AUTHENTICATION_MODE_VALUE" == true ]]; then
    export WS_CUSTODY_BEARER_TOKEN="$CUSTODY_BEARER_TOKEN_VALUE"
    export WS_BEARER_TOKEN="$WALLET_BEARER_TOKEN"
  fi
  export WS_HTTP_BIND="$WS_HTTP_BIND_VALUE"
  export WS_METRICS_BIND="$WS_METRICS_BIND_VALUE"
  export WS_SHUTDOWN_GRACE_SECONDS=10
  export RUST_LOG="$STACK_RUST_LOG"
  exec "$WALLET_BINARY" serve
) >"$WS_LOG" 2>&1 &
WS_PID=$!
register_service "Wallet Service" "$WS_PID" "$WS_LOG"

probe_wallet() {
  http_status_is "$WS_URL/health/ready" ready
}

wait_for_service "Wallet Service" "$WS_PID" "$WS_LOG" \
  "$SERVICE_STARTUP_TIMEOUT_SECONDS" probe_wallet

RUN_ID="$(basename "$STACK_ROOT")"
GAS_FUNDER_RESPONSE="$(provision_address \
  "$RUN_ID-gas-funder" "local-gas-funder")" \
  || die "Wallet Service could not provision the local gas-funder identity"
GAS_FUNDER_ADDRESS="$(printf '%s' "$GAS_FUNDER_RESPONSE" \
  | jq --exit-status --raw-output '.address')" \
  || die "Wallet Service returned an invalid gas-funder address"
GAS_FUNDER_LOCATOR="$(printf '%s' "$GAS_FUNDER_RESPONSE" \
  | jq --exit-status --raw-output '.key_locator.value')" \
  || die "Wallet Service returned an invalid gas-funder locator"

POLICY_PATH="$STACK_ROOT/policy.json"
POLICY_TEMP_PATH="$STACK_ROOT/policy.json.tmp"

if [[ -n "$STACK_POLICY_TEMPLATE" ]]; then
  jq \
    --arg address "$GAS_FUNDER_ADDRESS" \
    --arg locator "$GAS_FUNDER_LOCATOR" \
    '.gas_funder.address = $address | .gas_funder.key_locator = $locator' \
    "$STACK_POLICY_TEMPLATE" >"$POLICY_TEMP_PATH"
else
  MASTER_DESTINATION="$STACK_MASTER_DESTINATION"

  jq --null-input \
    --arg network "$PAYMENT_NETWORK" \
    --argjson chain_id "$CHAIN_ID" \
    --arg master_destination "$MASTER_DESTINATION" \
    --arg gas_funder_address "$GAS_FUNDER_ADDRESS" \
    --arg gas_funder_locator "$GAS_FUNDER_LOCATOR" \
    '{
      version: 1,
      scope: {chain: "ethereum", network: $network, chain_id: $chain_id},
      deposit_ttl_seconds: 86400,
      assets: [{
        asset: "native",
        master_destination: $master_destination,
        minimum_collection_amount: "1000000000000000"
      }],
      fees: {
        max_fee_per_gas: "100000000000",
        max_priority_fee_per_gas: "5000000000",
        max_gas_limit: 200000,
        max_total_fee: "20000000000000000"
      },
      gas_funder: {
        address: $gas_funder_address,
        key_locator: $gas_funder_locator,
        maximum_funding_amount: "5000000000000000"
      }
    }' >"$POLICY_TEMP_PATH"
fi
mv "$POLICY_TEMP_PATH" "$POLICY_PATH"
chmod 600 "$POLICY_PATH"

(
  export PS_DATABASE_PATH="$STACK_ROOT/ps-db"
  export PS_POLICY_PATH="$POLICY_PATH"
  export PS_INDEXER_URL="$IX_URL"
  export PS_INDEXER_NETWORK="$PAYMENT_NETWORK"
  export PS_WALLET_URL="$WS_URL"
  if [[ "$AUTHENTICATION_MODE_VALUE" == true ]]; then
    export PS_INDEXER_BEARER_TOKEN="$INDEXER_BEARER_TOKEN_VALUE"
    export PS_WALLET_BEARER_TOKEN="$WALLET_BEARER_TOKEN"
    export PS_API_BEARER_TOKEN="$PS_API_BEARER_TOKEN_VALUE"
    export PS_ADMIN_BEARER_TOKEN="$PS_ADMIN_BEARER_TOKEN_VALUE"
  fi
  export PS_HTTP_BIND="$PS_HTTP_BIND_VALUE"
  export PS_METRICS_BIND="$PS_METRICS_BIND_VALUE"
  export PS_SHUTDOWN_GRACE_SECONDS=10
  export RUST_LOG="$STACK_RUST_LOG"
  exec "$PAYMENT_BINARY" serve
) >"$PS_LOG" 2>&1 &
PS_PID=$!
register_service "Payment Service" "$PS_PID" "$PS_LOG"

probe_payment() {
  http_status_is "$PS_URL/health/ready" ready
}

wait_for_service "Payment Service" "$PS_PID" "$PS_LOG" \
  "$SERVICE_STARTUP_TIMEOUT_SECONDS" probe_payment

ADMIN_STATUS="$(curl_service "$PS_ADMIN_BEARER_TOKEN_VALUE" \
  --fail --silent --show-error \
  --connect-timeout 2 --max-time 10 \
  "$PS_URL/v1/admin/status")" \
  || die "Payment Service administrator status verification failed"
printf '%s' "$ADMIN_STATUS" \
  | jq --exit-status \
    --arg network "$PAYMENT_NETWORK" \
    --arg authentication_mode "$AUTHENTICATION_MODE_NAME" \
    --argjson chain_id "$CHAIN_ID" \
    '(.service == "payment-service")
      and (.ready == true)
      and (.authentication_mode == $authentication_mode)
      and (.indexer_ready == true)
      and (.wallet_ready == true)
      and (.scope.chain == "ethereum")
      and (.scope.network == $network)
      and (.scope.chain_id == $chain_id)' \
    >/dev/null \
  || die "Payment Service administrator status does not match the ready local stack"

info "all four services are ready"
info "Payment Service: $PS_URL"
info "authentication mode: $AUTHENTICATION_MODE_NAME"
info "Indexer metrics: http://$IX_METRICS_BIND_VALUE/metrics"
info "Custody metrics: http://$CUSTODY_METRICS_BIND_VALUE/metrics"
info "Wallet metrics: http://$WS_METRICS_BIND_VALUE/metrics"
info "Payment metrics: http://$PS_METRICS_BIND_VALUE/metrics"
info "private client environment: $(relative_to_repository "$CLIENT_ENV_PATH")"
if [[ "$AUTHENTICATION_MODE_VALUE" == true ]]; then
  info "from the repository root, source that file to use the generated tokens"
else
  info "no repo-owned service credentials were created; source that file for URLs and mode"
fi
info "press Ctrl-C to stop IX, custody, WS, and PS; the Ethereum RPC is untouched"

while :; do
  for ((i=0; i<${#SERVICE_PIDS[@]}; i++)); do
    if ! kill -0 "${SERVICE_PIDS[$i]}" 2>/dev/null; then
      SERVICE_EXIT_CODE=0
      wait "${SERVICE_PIDS[$i]}" || SERVICE_EXIT_CODE=$?
      warn "${SERVICE_NAMES[$i]} exited unexpectedly with status $SERVICE_EXIT_CODE"
      show_log_tail "${SERVICE_NAMES[$i]}" "${SERVICE_LOGS[$i]}"
      exit 1
    fi
  done
  sleep 1
done
