#!/usr/bin/env bash

# Start a release-built, production-shaped Ethereum payment stack while using
# the repository's ephemeral mock custody adapter.
#
# This launcher is intentionally limited to development/staging and test funds.
# Mock custody destroys every key on exit, so the entire run root is one-shot
# and must never be reopened by a later stack.

set +x
set -euo pipefail
set -m
set +a
umask 077

# Environment inputs retain their export attribute after a normal assignment.
# Capture credential-bearing values before the first external command, then
# remove their public names so build tools, probes, and unrelated services
# cannot inherit them. Each service subshell exports only what it requires.
ETHEREUM_RPC_URL_VALUE="${ETHEREUM_RPC_URL-}"
ETHEREUM_RPC_WS_URL_VALUE="${ETHEREUM_RPC_WS_URL-}"
CUSTODY_BEARER_TOKEN_VALUE="${CUSTODY_BEARER_TOKEN-}"
IX_BEARER_TOKEN_VALUE="${IX_BEARER_TOKEN-}"
WS_BEARER_TOKEN_VALUE="${WS_BEARER_TOKEN-}"
PS_API_BEARER_TOKEN_VALUE="${PS_API_BEARER_TOKEN-}"
PS_ADMIN_BEARER_TOKEN_VALUE="${PS_ADMIN_BEARER_TOKEN-}"
HTTP_PROXY_VALUE="${HTTP_PROXY-}"
HTTPS_PROXY_VALUE="${HTTPS_PROXY-}"
ALL_PROXY_VALUE="${ALL_PROXY-}"
http_proxy_VALUE="${http_proxy-}"
https_proxy_VALUE="${https_proxy-}"
all_proxy_VALUE="${all_proxy-}"
NO_PROXY_VALUE="127.0.0.1,localhost${NO_PROXY:+,$NO_PROXY}${no_proxy:+,$no_proxy}"
no_proxy_VALUE="$NO_PROXY_VALUE"
SSL_CERT_FILE_VALUE="${SSL_CERT_FILE-}"
SSL_CERT_DIR_VALUE="${SSL_CERT_DIR-}"
export -n ETHEREUM_RPC_URL_VALUE ETHEREUM_RPC_WS_URL_VALUE
export -n CUSTODY_BEARER_TOKEN_VALUE IX_BEARER_TOKEN_VALUE
export -n WS_BEARER_TOKEN_VALUE PS_API_BEARER_TOKEN_VALUE
export -n PS_ADMIN_BEARER_TOKEN_VALUE
export -n HTTP_PROXY_VALUE HTTPS_PROXY_VALUE ALL_PROXY_VALUE
export -n http_proxy_VALUE https_proxy_VALUE all_proxy_VALUE
export -n NO_PROXY_VALUE no_proxy_VALUE SSL_CERT_FILE_VALUE SSL_CERT_DIR_VALUE
unset ETHEREUM_RPC_URL ETHEREUM_RPC_WS_URL
unset CUSTODY_BEARER_TOKEN IX_BEARER_TOKEN WS_BEARER_TOKEN
unset PS_API_BEARER_TOKEN PS_ADMIN_BEARER_TOKEN
unset IX_RPC_HTTP_URL IX_RPC_WS_URL
unset WS_ETHEREUM_RPC_URL WS_CUSTODY_BEARER_TOKEN
unset PS_INDEXER_BEARER_TOKEN PS_WALLET_BEARER_TOKEN
unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy
unset NO_PROXY no_proxy SSL_CERT_FILE SSL_CERT_DIR

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPOSITORY_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

NO_BUILD=0
CHECK_ONLY=0
ACKNOWLEDGED_EPHEMERAL_CUSTODY=0
CLEANUP_STARTED=0
RUN_ROOT_CREATED=0
POLICY_SNAPSHOT_PATH=""
POLICY_SNAPSHOT_TEMPORARY=0
SERVICE_NAMES=()
SERVICE_PIDS=()
SERVICE_LOGS=()

usage() {
  cat <<'EOF'
Usage:
  ./scripts/run-release-payment-services-with-mock-custody.sh \
    --acknowledge-ephemeral-custody [--no-build] [--check]

Starts release binaries for:
  1. Indexer Service
  2. Ephemeral mock custody
  3. Wallet Service
  4. Payment Service

THIS IS NOT A PRODUCTION-SAFE DEPLOYMENT. The custody process keeps random
keys only in memory. When custody exits, persisted deposit and gas-funder key
locators become unusable. Use development/staging networks and test funds only.
Ethereum mainnet is refused.

Allowed chain IDs:
  1337/31337 (local private development), 11155111 (Sepolia), and
  560048 (Hoodi). Custom private and other public chain IDs are refused.

Required non-secret environment:
  STACK_ENVIRONMENT       development or staging; production is refused
  STACK_RUN_ROOT          new absolute directory outside this repository
  STACK_POLICY_TEMPLATE   absolute reviewed policy JSON path
  ETHEREUM_RPC_URL        HTTPS remote RPC, or loopback HTTP for local testing
  ETHEREUM_CHAIN_ID       explicit allowlisted test/private chain ID
  ETHEREUM_GENESIS_HASH   explicit expected 0x-prefixed block-zero hash
  PAYMENT_NETWORK         exact policy/IX network slug
  IX_BOOTSTRAP_HEIGHT     explicit non-negative decimal height

Required credentials, supplied by a secret manager or private parent shell:
  CUSTODY_BEARER_TOKEN
  IX_BEARER_TOKEN
  WS_BEARER_TOKEN
  PS_API_BEARER_TOKEN
  PS_ADMIN_BEARER_TOKEN

All five credentials must be distinct. The launcher removes their exported
input names before running tools, gives each service only the credentials it
needs, and passes authenticated probe headers through curl configuration on
standard input. It never prints credentials or writes a client.env file.

Optional environment:
  ETHEREUM_RPC_WS_URL              WSS remote URL or loopback WS URL
  IX_CONFIRMATION_DEPTH            default: 12
  IX_REORG_RETENTION               default: 50
  IX_STARTUP_TIMEOUT_SECONDS       default: 1800
  SERVICE_STARTUP_TIMEOUT_SECONDS  default: 90
  STACK_SHUTDOWN_TIMEOUT_SECONDS   default: 60 per service; must exceed 30
  RUST_LOG                         default: info

Optional loopback bind overrides:
  STACK_IX_HTTP_BIND, STACK_IX_METRICS_BIND, STACK_CUSTODY_BIND,
  STACK_WS_HTTP_BIND, STACK_PS_HTTP_BIND, STACK_PS_METRICS_BIND

All service and metrics listeners remain on 127.0.0.1. Put a separately
reviewed TLS reverse proxy in front of Payment Service when remote access is
required. The proxy is not configured or validated by this launcher.
Service log files are not rotated; long-running staging use requires external
rotation and disk-space monitoring for STACK_RUN_ROOT.

Options:
  --acknowledge-ephemeral-custody
      Confirm that this run is one-shot, non-production, and test-funds-only.
  --no-build
      Reuse already-built target/release binaries.
  --check
      Validate configuration, ports, policy, and chain identity without
      building, creating STACK_RUN_ROOT, or starting any service process.
  -h, --help
      Show this help.
EOF
}

info() {
  printf '[release-stack] %s\n' "$*"
}

warn() {
  printf '[release-stack] WARNING: %s\n' "$*" >&2
}

die() {
  printf '[release-stack] ERROR: %s\n' "$*" >&2
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --acknowledge-ephemeral-custody)
      ACKNOWLEDGED_EPHEMERAL_CUSTODY=1
      ;;
    --no-build)
      NO_BUILD=1
      ;;
    --check)
      CHECK_ONLY=1
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

require_command() {
  local command_name="$1"
  command -v "$command_name" >/dev/null 2>&1 \
    || die "required command is not installed: $command_name"
}

require_value() {
  local name="$1"
  local value="$2"
  [[ -n "$value" ]] || die "$name is required"
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

require_non_negative_integer() {
  local name="$1"
  local value="$2"
  case "$value" in
    ''|*[!0-9]*) die "$name must be a non-negative integer" ;;
  esac
  case "$value" in
    0|[1-9]*) ;;
    *) die "$name must be a canonical decimal integer without leading zeros" ;;
  esac
}

require_integer_at_most() {
  local name="$1"
  local value="$2"
  local maximum="$3"
  if [[ "${#value}" -gt "${#maximum}" ]] \
    || { [[ "${#value}" -eq "${#maximum}" ]] && [[ "$value" > "$maximum" ]]; }; then
    die "$name must not exceed $maximum"
  fi
}

validate_network_slug() {
  local value="$1"
  [[ -n "$value" ]] || die "PAYMENT_NETWORK must not be empty"
  case "$value" in
    *[[:space:]]*) die "PAYMENT_NETWORK must not contain whitespace" ;;
  esac
}

validate_genesis_hash() {
  local value="$1"
  [[ "$value" =~ ^0x[0-9a-fA-F]{64}$ ]] \
    || die "ETHEREUM_GENESIS_HASH must be a canonical 32-byte hexadecimal hash"
}

validate_bearer_value() {
  local name="$1"
  local value="$2"
  [[ "${#value}" -ge 32 && "${#value}" -le 4096 ]] \
    || die "$name must contain between 32 and 4096 characters"
  [[ "$value" =~ ^[-A-Za-z0-9._~+/=]+$ ]] \
    || die "$name must use visible URL-safe or base64 token characters"
}

validate_loopback_bind() {
  local name="$1"
  local value="$2"
  local port

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
  require_integer_at_most "$name port" "$port" 65535
}

validate_rpc_http_url() {
  local value="$1"
  case "$value" in
    https://*) ;;
    http://127.0.0.1|http://127.0.0.1:*|http://localhost|http://localhost:*) ;;
    *) die "ETHEREUM_RPC_URL must use HTTPS unless it targets loopback" ;;
  esac
  case "$value" in
    *'@'*|*'?'*|*'#'*)
      die "ETHEREUM_RPC_URL must not contain embedded credentials, a query, or a fragment"
      ;;
  esac
  case "$value" in
    *[[:space:]]*|*'"'*|*\\*)
      die "ETHEREUM_RPC_URL contains characters that cannot be handled safely"
      ;;
  esac
}

validate_rpc_ws_url() {
  local value="$1"
  [[ -z "$value" ]] && return 0
  case "$value" in
    wss://*) ;;
    ws://127.0.0.1|ws://127.0.0.1:*|ws://localhost|ws://localhost:*) ;;
    *) die "ETHEREUM_RPC_WS_URL must use WSS unless it targets loopback" ;;
  esac
  case "$value" in
    *'@'*|*'?'*|*'#'*)
      die "ETHEREUM_RPC_WS_URL must not contain embedded credentials, a query, or a fragment"
      ;;
  esac
  case "$value" in
    *[[:space:]]*|*'"'*|*\\*)
      die "ETHEREUM_RPC_WS_URL contains characters that cannot be handled safely"
      ;;
  esac
}

export_network_environment() {
  [[ -z "$HTTP_PROXY_VALUE" ]] || export HTTP_PROXY="$HTTP_PROXY_VALUE"
  [[ -z "$HTTPS_PROXY_VALUE" ]] || export HTTPS_PROXY="$HTTPS_PROXY_VALUE"
  [[ -z "$ALL_PROXY_VALUE" ]] || export ALL_PROXY="$ALL_PROXY_VALUE"
  [[ -z "$http_proxy_VALUE" ]] || export http_proxy="$http_proxy_VALUE"
  [[ -z "$https_proxy_VALUE" ]] || export https_proxy="$https_proxy_VALUE"
  [[ -z "$all_proxy_VALUE" ]] || export all_proxy="$all_proxy_VALUE"
  export NO_PROXY="$NO_PROXY_VALUE"
  export no_proxy="$no_proxy_VALUE"
  [[ -z "$SSL_CERT_FILE_VALUE" ]] || export SSL_CERT_FILE="$SSL_CERT_FILE_VALUE"
  [[ -z "$SSL_CERT_DIR_VALUE" ]] || export SSL_CERT_DIR="$SSL_CERT_DIR_VALUE"
}

export_loopback_environment() {
  export NO_PROXY="$NO_PROXY_VALUE"
  export no_proxy="$no_proxy_VALUE"
}

clear_exported_environment() {
  local exported_name
  while IFS= read -r exported_name; do
    export -n "$exported_name" 2>/dev/null || true
  done < <(compgen -e)
}

port_from_bind() {
  printf '%s' "${1##*:}"
}

ensure_port_is_free() {
  local service_name="$1"
  local port="$2"
  local owner

  if owner="$(lsof -nP -iTCP:"$port" -sTCP:LISTEN 2>/dev/null)"; then
    printf '[release-stack] ERROR: %s cannot start because TCP port %s is already in use.\n' \
      "$service_name" "$port" >&2
    printf '%s\n' "$owner" >&2
    die "stop or reconfigure the known process; the launcher will not kill it"
  fi
}

display_path() {
  local path="$1"
  case "$path" in
    "$REPOSITORY_ROOT"/*) printf './%s' "${path#"$REPOSITORY_ROOT"/}" ;;
    *) printf '%s' "$path" ;;
  esac
}

show_log_tail() {
  local service_name="$1"
  local log_path="$2"
  warn "$service_name log tail ($(display_path "$log_path")) follows"
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
  printf '%s\t%s\n' "$service_name" "$pid" >>"$STACK_RUN_ROOT/pids"
  info "$service_name started (PID $pid, log $(display_path "$log_path"))"
}

stop_service() {
  local index="$1"
  local pid="${SERVICE_PIDS[$index]}"
  local service_name="${SERVICE_NAMES[$index]}"
  local deadline

  if kill -0 "$pid" 2>/dev/null; then
    info "stopping $service_name"
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
  if [[ "$POLICY_SNAPSHOT_TEMPORARY" -eq 1 && -n "$POLICY_SNAPSHOT_PATH" ]]; then
    rm -f -- "$POLICY_SNAPSHOT_PATH"
  fi
  if [[ "$RUN_ROOT_CREATED" -eq 1 ]]; then
    info "one-shot runtime data and logs preserved at $(display_path "$STACK_RUN_ROOT")"
    warn "mock custody keys are gone; never reopen this run's IX/PS databases or policy"
  fi
  return "$exit_status"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

curl_with_bearer() {
  local token="$1"
  shift
  printf 'header = "Authorization: Bearer %s"\n' "$token" \
    | curl --disable --config - --noproxy '*' "$@"
}

rpc_request() (
  local payload="$1"
  export_network_environment
  printf 'url = "%s"\n' "$ETHEREUM_RPC_URL_VALUE" \
    | curl --disable --config - --fail --silent --show-error \
      --connect-timeout 5 \
      --max-time 20 \
      --header 'Content-Type: application/json' \
      --data "$payload"
)

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
    response="$(curl_with_bearer "$token" --fail --silent --show-error \
      --connect-timeout 2 --max-time 3 "$url" 2>/dev/null)" || return 1
  else
    response="$(curl --disable --noproxy '*' --fail --silent --show-error \
      --connect-timeout 2 --max-time 3 "$url" 2>/dev/null)" || return 1
  fi
  printf '%s' "$response" \
    | jq --exit-status --arg expected "$expected" '.status == $expected' \
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
  curl_with_bearer "$WS_BEARER_TOKEN_VALUE" \
    --fail --silent --show-error \
    --connect-timeout 2 --max-time 15 \
    --request POST \
    --header 'Content-Type: application/json' \
    --data "$payload" \
    "$WS_URL/v1/ethereum/addresses"
}

cd "$REPOSITORY_ROOT"

for required in bash cargo chmod curl dirname head jq lsof mkdir mktemp mv rm tail tr wc; do
  require_command "$required"
done

[[ "$ACKNOWLEDGED_EPHEMERAL_CUSTODY" -eq 1 ]] \
  || die "pass --acknowledge-ephemeral-custody after reviewing the test-funds-only warning"

STACK_ENVIRONMENT="${STACK_ENVIRONMENT:-}"
STACK_RUN_ROOT="${STACK_RUN_ROOT:-}"
STACK_POLICY_TEMPLATE="${STACK_POLICY_TEMPLATE:-}"
ETHEREUM_CHAIN_ID="${ETHEREUM_CHAIN_ID:-}"
ETHEREUM_GENESIS_HASH="${ETHEREUM_GENESIS_HASH:-}"
PAYMENT_NETWORK="${PAYMENT_NETWORK:-}"
IX_BOOTSTRAP_HEIGHT="${IX_BOOTSTRAP_HEIGHT:-}"
IX_CONFIRMATION_DEPTH="${IX_CONFIRMATION_DEPTH:-12}"
IX_REORG_RETENTION="${IX_REORG_RETENTION:-50}"
IX_STARTUP_TIMEOUT_SECONDS="${IX_STARTUP_TIMEOUT_SECONDS:-1800}"
SERVICE_STARTUP_TIMEOUT_SECONDS="${SERVICE_STARTUP_TIMEOUT_SECONDS:-90}"
STACK_SHUTDOWN_TIMEOUT_SECONDS="${STACK_SHUTDOWN_TIMEOUT_SECONDS:-60}"
STACK_RUST_LOG="${RUST_LOG:-info}"

for required_name in \
  STACK_ENVIRONMENT STACK_RUN_ROOT STACK_POLICY_TEMPLATE \
  ETHEREUM_CHAIN_ID ETHEREUM_GENESIS_HASH PAYMENT_NETWORK IX_BOOTSTRAP_HEIGHT; do
  require_value "$required_name" "${!required_name:-}"
done
require_value ETHEREUM_RPC_URL "$ETHEREUM_RPC_URL_VALUE"
require_value CUSTODY_BEARER_TOKEN "$CUSTODY_BEARER_TOKEN_VALUE"
require_value IX_BEARER_TOKEN "$IX_BEARER_TOKEN_VALUE"
require_value WS_BEARER_TOKEN "$WS_BEARER_TOKEN_VALUE"
require_value PS_API_BEARER_TOKEN "$PS_API_BEARER_TOKEN_VALUE"
require_value PS_ADMIN_BEARER_TOKEN "$PS_ADMIN_BEARER_TOKEN_VALUE"

case "$STACK_ENVIRONMENT" in
  development|staging) ;;
  production) die "STACK_ENVIRONMENT=production is forbidden while custody is ephemeral" ;;
  *) die "STACK_ENVIRONMENT must be development or staging" ;;
esac

validate_network_slug "$PAYMENT_NETWORK"
NETWORK_LOWER="$(printf '%s' "$PAYMENT_NETWORK" | tr '[:upper:]' '[:lower:]')"
case "$NETWORK_LOWER" in
  mainnet|ethereum-mainnet|homestead)
    die "Ethereum mainnet is forbidden while custody is ephemeral"
    ;;
esac

require_positive_integer ETHEREUM_CHAIN_ID "$ETHEREUM_CHAIN_ID"
case "$ETHEREUM_CHAIN_ID" in
  1337|31337|560048|11155111) ;;
  *) die "ETHEREUM_CHAIN_ID is not an allowlisted test/private chain" ;;
esac
validate_genesis_hash "$ETHEREUM_GENESIS_HASH"
validate_rpc_http_url "$ETHEREUM_RPC_URL_VALUE"
validate_rpc_ws_url "$ETHEREUM_RPC_WS_URL_VALUE"
require_non_negative_integer IX_BOOTSTRAP_HEIGHT "$IX_BOOTSTRAP_HEIGHT"
require_integer_at_most IX_BOOTSTRAP_HEIGHT "$IX_BOOTSTRAP_HEIGHT" 9007199254740991
require_positive_integer IX_CONFIRMATION_DEPTH "$IX_CONFIRMATION_DEPTH"
require_integer_at_most IX_CONFIRMATION_DEPTH "$IX_CONFIRMATION_DEPTH" 1000000
require_positive_integer IX_REORG_RETENTION "$IX_REORG_RETENTION"
require_integer_at_most IX_REORG_RETENTION "$IX_REORG_RETENTION" 1000000
require_positive_integer IX_STARTUP_TIMEOUT_SECONDS "$IX_STARTUP_TIMEOUT_SECONDS"
require_integer_at_most IX_STARTUP_TIMEOUT_SECONDS "$IX_STARTUP_TIMEOUT_SECONDS" 86400
require_positive_integer SERVICE_STARTUP_TIMEOUT_SECONDS "$SERVICE_STARTUP_TIMEOUT_SECONDS"
require_integer_at_most SERVICE_STARTUP_TIMEOUT_SECONDS "$SERVICE_STARTUP_TIMEOUT_SECONDS" 3600
require_positive_integer STACK_SHUTDOWN_TIMEOUT_SECONDS "$STACK_SHUTDOWN_TIMEOUT_SECONDS"
require_integer_at_most STACK_SHUTDOWN_TIMEOUT_SECONDS "$STACK_SHUTDOWN_TIMEOUT_SECONDS" 3600
[[ "$STACK_SHUTDOWN_TIMEOUT_SECONDS" -gt 30 ]] \
  || die "STACK_SHUTDOWN_TIMEOUT_SECONDS must exceed the Wallet Service 30-second grace"

case "$STACK_RUN_ROOT" in
  /*) ;;
  *) die "STACK_RUN_ROOT must be an absolute path" ;;
esac
[[ "$STACK_RUN_ROOT" != "/" ]] || die "STACK_RUN_ROOT must not be the filesystem root"
[[ ! -e "$STACK_RUN_ROOT" && ! -L "$STACK_RUN_ROOT" ]] \
  || die "STACK_RUN_ROOT must not already exist; every mock-custody run is one-shot"
RUN_ROOT_PARENT="$(dirname "$STACK_RUN_ROOT")"
[[ -d "$RUN_ROOT_PARENT" && -w "$RUN_ROOT_PARENT" ]] \
  || die "the existing parent of STACK_RUN_ROOT must be a writable directory"
RUN_ROOT_PARENT="$(cd "$RUN_ROOT_PARENT" && pwd -P)"
RUN_ROOT_NAME="${STACK_RUN_ROOT##*/}"
[[ "$RUN_ROOT_NAME" =~ ^[A-Za-z0-9._-]{1,128}$ ]] \
  || die "STACK_RUN_ROOT final name must contain 1-128 ASCII letters, digits, dot, underscore, or hyphen"
STACK_RUN_ROOT="$RUN_ROOT_PARENT/$RUN_ROOT_NAME"
case "$STACK_RUN_ROOT" in
  "$REPOSITORY_ROOT"|"$REPOSITORY_ROOT"/*)
    die "STACK_RUN_ROOT must live outside the source repository"
    ;;
esac

case "$STACK_POLICY_TEMPLATE" in
  /*) ;;
  *) die "STACK_POLICY_TEMPLATE must be an absolute path" ;;
esac
[[ -f "$STACK_POLICY_TEMPLATE" ]] \
  || die "STACK_POLICY_TEMPLATE must identify a regular JSON file"
POLICY_SIZE="$(wc -c <"$STACK_POLICY_TEMPLATE" | tr -d '[:space:]')"
[[ "$POLICY_SIZE" -le 1048576 ]] \
  || die "STACK_POLICY_TEMPLATE must not exceed 1 MiB"
POLICY_SOURCE_PATH="$STACK_POLICY_TEMPLATE"
if [[ "$CHECK_ONLY" -eq 0 ]]; then
  POLICY_SNAPSHOT_PATH="$(mktemp "$RUN_ROOT_PARENT/.payment-policy.snapshot.XXXXXX")"
  POLICY_SNAPSHOT_TEMPORARY=1
  chmod 600 "$POLICY_SNAPSHOT_PATH"
  head -c 1048577 "$STACK_POLICY_TEMPLATE" >"$POLICY_SNAPSHOT_PATH" \
    || die "STACK_POLICY_TEMPLATE could not be read"
  POLICY_SIZE="$(wc -c <"$POLICY_SNAPSHOT_PATH" | tr -d '[:space:]')"
  [[ "$POLICY_SIZE" -le 1048576 ]] \
    || die "STACK_POLICY_TEMPLATE changed or exceeds 1 MiB"
  POLICY_SOURCE_PATH="$POLICY_SNAPSHOT_PATH"
fi

TOKEN_NAMES=(
  CUSTODY_BEARER_TOKEN IX_BEARER_TOKEN WS_BEARER_TOKEN
  PS_API_BEARER_TOKEN PS_ADMIN_BEARER_TOKEN
)
TOKEN_VALUES=(
  "$CUSTODY_BEARER_TOKEN_VALUE" "$IX_BEARER_TOKEN_VALUE"
  "$WS_BEARER_TOKEN_VALUE" "$PS_API_BEARER_TOKEN_VALUE"
  "$PS_ADMIN_BEARER_TOKEN_VALUE"
)
for ((i=0; i<${#TOKEN_VALUES[@]}; i++)); do
  validate_bearer_value "${TOKEN_NAMES[$i]}" "${TOKEN_VALUES[$i]}"
  for ((j=i + 1; j<${#TOKEN_VALUES[@]}; j++)); do
    [[ "${TOKEN_VALUES[$i]}" != "${TOKEN_VALUES[$j]}" ]] \
      || die "${TOKEN_NAMES[$i]} and ${TOKEN_NAMES[$j]} must be distinct"
  done
done

POLICY_LIMITS="$(jq --slurp --exit-status --raw-output \
  --arg network "$PAYMENT_NETWORK" \
  --argjson chain_id "$ETHEREUM_CHAIN_ID" \
  --arg u256_max "115792089237316195423570985008687907853269984665640564039457584007913129639935" \
  'def exact_keys($expected):
      (type == "object") and (keys == ($expected | sort));
   def safe_positive_integer:
      type == "number" and . == floor and . > 0 and . <= 9007199254740991;
   def canonical_u256:
      type == "string"
      and test("^(0|[1-9][0-9]*)$")
      and ((length < 78) or (length == 78 and . <= $u256_max));
   def positive_u256: canonical_u256 and . != "0";
   def canonical_address:
      type == "string" and test("^0x[0-9a-f]{40}$");
   def decimal_lte($left; $right):
      (($left | length) < ($right | length))
      or ((($left | length) == ($right | length)) and ($left <= $right));
   select(length == 1)
   | .[0]
   | select(
    exact_keys(["version", "scope", "deposit_ttl_seconds", "assets", "fees", "gas_funder"])
    and (.scope | exact_keys(["chain", "network", "chain_id"]))
    and (.fees | exact_keys(["max_fee_per_gas", "max_priority_fee_per_gas", "max_gas_limit", "max_total_fee"]))
    and (.gas_funder | exact_keys(["address", "key_locator", "maximum_funding_amount"]))
    and (.version | type == "number" and . == floor and . > 0 and . <= 4294967295)
    and (.scope.chain == "ethereum")
    and (.scope.network == $network)
    and (.scope.chain_id == $chain_id)
    and (.deposit_ttl_seconds | safe_positive_integer)
    and (.assets | type == "array" and length > 0)
    and (all(.assets[];
      exact_keys(["asset", "master_destination", "minimum_collection_amount"])
      and ((.asset == "native") or (.asset | canonical_address))
      and (.master_destination | canonical_address)
      and (.minimum_collection_amount | positive_u256)))
    and ([.assets[].asset] as $assets
      | ($assets | length) == ($assets | unique | length))
    and (.fees.max_gas_limit | safe_positive_integer)
    and (.fees.max_fee_per_gas | positive_u256)
    and (.fees.max_priority_fee_per_gas | canonical_u256)
    and (decimal_lte(.fees.max_priority_fee_per_gas; .fees.max_fee_per_gas))
    and (.fees.max_total_fee | positive_u256)
    and (.gas_funder.address | canonical_address)
    and (.gas_funder.key_locator | type == "string" and test("\\S"))
    and (.gas_funder.maximum_funding_amount | positive_u256)
   )
   | [.fees.max_gas_limit, .fees.max_fee_per_gas,
      .fees.max_priority_fee_per_gas, .fees.max_total_fee]
   | @tsv' \
  "$POLICY_SOURCE_PATH")" \
  || die "STACK_POLICY_TEMPLATE is invalid or does not match the configured scope"
IFS=$'\t' read -r \
  POLICY_MAX_GAS_LIMIT \
  POLICY_MAX_FEE_PER_GAS \
  POLICY_MAX_PRIORITY_FEE_PER_GAS \
  POLICY_MAX_TOTAL_FEE \
  <<<"$POLICY_LIMITS"
unset POLICY_LIMITS

IX_HTTP_BIND_VALUE="${STACK_IX_HTTP_BIND:-127.0.0.1:8080}"
IX_METRICS_BIND_VALUE="${STACK_IX_METRICS_BIND:-127.0.0.1:9090}"
CUSTODY_BIND_VALUE="${STACK_CUSTODY_BIND:-127.0.0.1:8181}"
WS_HTTP_BIND_VALUE="${STACK_WS_HTTP_BIND:-127.0.0.1:8082}"
PS_HTTP_BIND_VALUE="${STACK_PS_HTTP_BIND:-127.0.0.1:8081}"
PS_METRICS_BIND_VALUE="${STACK_PS_METRICS_BIND:-127.0.0.1:9091}"

validate_loopback_bind STACK_IX_HTTP_BIND "$IX_HTTP_BIND_VALUE"
validate_loopback_bind STACK_IX_METRICS_BIND "$IX_METRICS_BIND_VALUE"
validate_loopback_bind STACK_CUSTODY_BIND "$CUSTODY_BIND_VALUE"
validate_loopback_bind STACK_WS_HTTP_BIND "$WS_HTTP_BIND_VALUE"
validate_loopback_bind STACK_PS_HTTP_BIND "$PS_HTTP_BIND_VALUE"
validate_loopback_bind STACK_PS_METRICS_BIND "$PS_METRICS_BIND_VALUE"

OWNED_SERVICE_NAMES=(
  "Indexer API" "Indexer metrics" "Custody API"
  "Wallet API" "Payment API" "Payment metrics"
)
OWNED_PORTS=(
  "$(port_from_bind "$IX_HTTP_BIND_VALUE")"
  "$(port_from_bind "$IX_METRICS_BIND_VALUE")"
  "$(port_from_bind "$CUSTODY_BIND_VALUE")"
  "$(port_from_bind "$WS_HTTP_BIND_VALUE")"
  "$(port_from_bind "$PS_HTTP_BIND_VALUE")"
  "$(port_from_bind "$PS_METRICS_BIND_VALUE")"
)
for ((i=0; i<${#OWNED_PORTS[@]}; i++)); do
  for ((j=i + 1; j<${#OWNED_PORTS[@]}; j++)); do
    [[ "${OWNED_PORTS[$i]}" != "${OWNED_PORTS[$j]}" ]] \
      || die "${OWNED_SERVICE_NAMES[$i]} and ${OWNED_SERVICE_NAMES[$j]} use the same port"
  done
  ensure_port_is_free "${OWNED_SERVICE_NAMES[$i]}" "${OWNED_PORTS[$i]}"
done

info "probing the configured Ethereum RPC without printing its URL"
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
DISCOVERED_CHAIN_DIGITS="$(printf '%s' "$CHAIN_DIGITS" | tr '[:upper:]' '[:lower:]')"
EXPECTED_CHAIN_DIGITS="$(printf '%x' "$ETHEREUM_CHAIN_ID")"
[[ "$DISCOVERED_CHAIN_DIGITS" == "$EXPECTED_CHAIN_DIGITS" ]] \
  || die "ETHEREUM_CHAIN_ID does not match the configured RPC"

GENESIS_RESPONSE="$(rpc_request \
  '{"jsonrpc":"2.0","id":2,"method":"eth_getBlockByNumber","params":["0x0",false]}')" \
  || die "the configured Ethereum RPC did not return block zero"
DISCOVERED_GENESIS_HASH="$(printf '%s' "$GENESIS_RESPONSE" \
  | jq --exit-status --raw-output \
    'if (.error? != null) or (.result.hash? == null) then empty else .result.hash end')" \
  || die "the configured Ethereum RPC returned an invalid block-zero response"
validate_genesis_hash "$DISCOVERED_GENESIS_HASH"
EXPECTED_GENESIS_LOWER="$(printf '%s' "$ETHEREUM_GENESIS_HASH" | tr '[:upper:]' '[:lower:]')"
DISCOVERED_GENESIS_LOWER="$(printf '%s' "$DISCOVERED_GENESIS_HASH" | tr '[:upper:]' '[:lower:]')"
[[ "$DISCOVERED_GENESIS_LOWER" == "$EXPECTED_GENESIS_LOWER" ]] \
  || die "ETHEREUM_GENESIS_HASH does not match the configured RPC"
info "Ethereum identity preflight passed (chain ID $ETHEREUM_CHAIN_ID, network $PAYMENT_NETWORK)"

TARGET_DIRECTORY="$(
  export_network_environment
  cargo metadata --locked --format-version 1 --no-deps \
    | jq --exit-status --raw-output '.target_directory'
)"
INDEXER_BINARY="$TARGET_DIRECTORY/release/indexer-worker"
CUSTODY_BINARY="$TARGET_DIRECTORY/release/custody-worker"
WALLET_BINARY="$TARGET_DIRECTORY/release/wallet-worker"
PAYMENT_BINARY="$TARGET_DIRECTORY/release/payment-api"

if [[ "$NO_BUILD" -eq 1 ]]; then
  for binary in "$INDEXER_BINARY" "$CUSTODY_BINARY" "$WALLET_BINARY" "$PAYMENT_BINARY"; do
    [[ -x "$binary" ]] || die "missing release executable $binary; rerun without --no-build"
  done
fi

if [[ "$CHECK_ONLY" -eq 1 ]]; then
  info "preflight passed; no build, run directory, or service process was created"
  exit 0
fi

if [[ "$NO_BUILD" -eq 0 ]]; then
  info "building release binaries once with locked dependencies"
  (
    export_network_environment
    cargo build --locked --release \
      -p indexer-worker \
      -p custody-worker \
      -p wallet-worker \
      -p payment-api
  )
fi
for binary in "$INDEXER_BINARY" "$CUSTODY_BINARY" "$WALLET_BINARY" "$PAYMENT_BINARY"; do
  [[ -x "$binary" ]] || die "missing release executable $binary"
done

umask 077
mkdir "$STACK_RUN_ROOT"
chmod 700 "$STACK_RUN_ROOT"
RUN_ROOT_CREATED=1
mkdir "$STACK_RUN_ROOT/ix-db" "$STACK_RUN_ROOT/ps-db" \
  "$STACK_RUN_ROOT/logs" "$STACK_RUN_ROOT/runtime"
chmod 700 "$STACK_RUN_ROOT/ix-db" "$STACK_RUN_ROOT/ps-db" \
  "$STACK_RUN_ROOT/logs" "$STACK_RUN_ROOT/runtime"
REVIEWED_POLICY_SNAPSHOT_PATH="$STACK_RUN_ROOT/runtime/policy-template.snapshot.json"
mv "$POLICY_SNAPSHOT_PATH" "$REVIEWED_POLICY_SNAPSHOT_PATH"
POLICY_SNAPSHOT_PATH="$REVIEWED_POLICY_SNAPSHOT_PATH"
POLICY_SNAPSHOT_TEMPORARY=0
chmod 600 "$POLICY_SNAPSHOT_PATH"
: >"$STACK_RUN_ROOT/pids"
chmod 600 "$STACK_RUN_ROOT/pids"
{
  printf '%s\n' 'DO NOT REUSE THIS RUN.'
  printf '%s\n' 'Ephemeral mock custody destroys all keys and operation state on exit.'
  printf '%s\n' 'The retained IX/PS databases and runtime policy are diagnostic artifacts only.'
} >"$STACK_RUN_ROOT/EPHEMERAL_CUSTODY_DO_NOT_REUSE"
chmod 600 "$STACK_RUN_ROOT/EPHEMERAL_CUSTODY_DO_NOT_REUSE"

IX_URL="http://$IX_HTTP_BIND_VALUE"
CUSTODY_URL="http://$CUSTODY_BIND_VALUE"
WS_URL="http://$WS_HTTP_BIND_VALUE"
PS_URL="http://$PS_HTTP_BIND_VALUE"

IX_LOG="$STACK_RUN_ROOT/logs/indexer.log"
CUSTODY_LOG="$STACK_RUN_ROOT/logs/custody.log"
WS_LOG="$STACK_RUN_ROOT/logs/wallet.log"
PS_LOG="$STACK_RUN_ROOT/logs/payment.log"

(
  clear_exported_environment
  export_network_environment
  export IX_DATABASE_PATH="$STACK_RUN_ROOT/ix-db"
  export IX_NETWORK="$PAYMENT_NETWORK"
  export IX_BOOTSTRAP_HEIGHT
  export IX_CONFIRMATION_DEPTH
  export IX_REORG_RETENTION
  export IX_EXPECTED_CHAIN_ID="$ETHEREUM_CHAIN_ID"
  export IX_EXPECTED_GENESIS_HASH="$ETHEREUM_GENESIS_HASH"
  export IX_RPC_HTTP_URL="$ETHEREUM_RPC_URL_VALUE"
  if [[ -n "$ETHEREUM_RPC_WS_URL_VALUE" ]]; then
    export IX_RPC_WS_URL="$ETHEREUM_RPC_WS_URL_VALUE"
  else
    unset IX_RPC_WS_URL || true
  fi
  export IX_HTTP_BIND="$IX_HTTP_BIND_VALUE"
  export IX_METRICS_BIND="$IX_METRICS_BIND_VALUE"
  export IX_BEARER_TOKEN="$IX_BEARER_TOKEN_VALUE"
  export IX_UPSTREAM_TLS_TERMINATED=false
  export RUST_LOG="$STACK_RUST_LOG"
  exec "$INDEXER_BINARY" serve
) >"$IX_LOG" 2>&1 &
IX_PID=$!
register_service "Indexer Service" "$IX_PID" "$IX_LOG"

(
  clear_exported_environment
  export_loopback_environment
  export CUSTODY_BIND="$CUSTODY_BIND_VALUE"
  export CUSTODY_BEARER_TOKEN="$CUSTODY_BEARER_TOKEN_VALUE"
  export CUSTODY_SHUTDOWN_GRACE_SECONDS=10
  export RUST_LOG="$STACK_RUST_LOG"
  exec "$CUSTODY_BINARY" serve
) >"$CUSTODY_LOG" 2>&1 &
CUSTODY_PID=$!
register_service "Ephemeral mock custody" "$CUSTODY_PID" "$CUSTODY_LOG"

probe_indexer() {
  http_status_is "$IX_URL/health/ready" ready
}

probe_custody() {
  http_status_is "$CUSTODY_URL/v1/readiness" available "$CUSTODY_BEARER_TOKEN_VALUE"
}

wait_for_service "Ephemeral mock custody" "$CUSTODY_PID" "$CUSTODY_LOG" \
  "$SERVICE_STARTUP_TIMEOUT_SECONDS" probe_custody
wait_for_service "Indexer Service" "$IX_PID" "$IX_LOG" \
  "$IX_STARTUP_TIMEOUT_SECONDS" probe_indexer

(
  clear_exported_environment
  export_network_environment
  export WS_ETHEREUM_CHAIN_ID="$ETHEREUM_CHAIN_ID"
  export WS_ETHEREUM_RPC_URL="$ETHEREUM_RPC_URL_VALUE"
  export WS_CUSTODY_URL="$CUSTODY_URL"
  export WS_CUSTODY_BEARER_TOKEN="$CUSTODY_BEARER_TOKEN_VALUE"
  export WS_BEARER_TOKEN="$WS_BEARER_TOKEN_VALUE"
  export WS_HTTP_BIND="$WS_HTTP_BIND_VALUE"
  export WS_UPSTREAM_TLS_TERMINATED=false
  export WS_MAX_GAS_LIMIT="$POLICY_MAX_GAS_LIMIT"
  export WS_MAX_FEE_PER_GAS_WEI="$POLICY_MAX_FEE_PER_GAS"
  export WS_MAX_PRIORITY_FEE_PER_GAS_WEI="$POLICY_MAX_PRIORITY_FEE_PER_GAS"
  export WS_MAX_TOTAL_FEE_WEI="$POLICY_MAX_TOTAL_FEE"
  export WS_SHUTDOWN_GRACE_SECONDS=30
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

GAS_FUNDER_RESPONSE="$(provision_address \
  "release-stack-$ETHEREUM_CHAIN_ID-gas-funder" \
  "$STACK_ENVIRONMENT-gas-funder")" \
  || die "Wallet Service could not provision the mock gas-funder identity"
GAS_FUNDER_ADDRESS="$(printf '%s' "$GAS_FUNDER_RESPONSE" \
  | jq --exit-status --raw-output '.address')" \
  || die "Wallet Service returned an invalid gas-funder address"
GAS_FUNDER_LOCATOR="$(printf '%s' "$GAS_FUNDER_RESPONSE" \
  | jq --exit-status --raw-output '.key_locator.value')" \
  || die "Wallet Service returned an invalid gas-funder locator"

POLICY_PATH="$STACK_RUN_ROOT/runtime/policy.json"
POLICY_TEMP_PATH="$STACK_RUN_ROOT/runtime/policy.json.tmp"
jq \
  --arg address "$GAS_FUNDER_ADDRESS" \
  --arg locator "$GAS_FUNDER_LOCATOR" \
  '.gas_funder.address = $address | .gas_funder.key_locator = $locator' \
  "$POLICY_SNAPSHOT_PATH" \
  >"$POLICY_TEMP_PATH"
mv "$POLICY_TEMP_PATH" "$POLICY_PATH"
chmod 600 "$POLICY_PATH"

(
  clear_exported_environment
  export_loopback_environment
  export PS_DATABASE_PATH="$STACK_RUN_ROOT/ps-db"
  export PS_POLICY_PATH="$POLICY_PATH"
  export PS_INDEXER_URL="$IX_URL"
  export PS_INDEXER_NETWORK="$PAYMENT_NETWORK"
  export PS_INDEXER_BEARER_TOKEN="$IX_BEARER_TOKEN_VALUE"
  export PS_WALLET_URL="$WS_URL"
  export PS_WALLET_BEARER_TOKEN="$WS_BEARER_TOKEN_VALUE"
  export PS_API_BEARER_TOKEN="$PS_API_BEARER_TOKEN_VALUE"
  export PS_ADMIN_BEARER_TOKEN="$PS_ADMIN_BEARER_TOKEN_VALUE"
  export PS_HTTP_BIND="$PS_HTTP_BIND_VALUE"
  export PS_METRICS_BIND="$PS_METRICS_BIND_VALUE"
  export PS_TLS_TERMINATED_UPSTREAM=false
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

ADMIN_STATUS="$(curl_with_bearer "$PS_ADMIN_BEARER_TOKEN_VALUE" \
  --fail --silent --show-error \
  --connect-timeout 2 --max-time 10 \
  "$PS_URL/v1/admin/status")" \
  || die "Payment Service administrator status verification failed"
printf '%s' "$ADMIN_STATUS" \
  | jq --exit-status \
    --arg network "$PAYMENT_NETWORK" \
    --argjson chain_id "$ETHEREUM_CHAIN_ID" \
    '(.service == "payment-service")
      and (.ready == true)
      and (.indexer_ready == true)
      and (.wallet_ready == true)
      and (.scope.chain == "ethereum")
      and (.scope.network == $network)
      and (.scope.chain_id == $chain_id)' \
    >/dev/null \
  || die "Payment Service administrator status does not match the ready stack"

warn "release binaries are running with EPHEMERAL MOCK CUSTODY; test funds only"
warn "service logs are not rotated; configure external rotation and disk monitoring"
info "all four services are ready for $STACK_ENVIRONMENT"
info "Payment Service (loopback): $PS_URL"
info "Payment metrics (loopback): http://$PS_METRICS_BIND_VALUE/metrics"
info "one-shot runtime root: $(display_path "$STACK_RUN_ROOT")"
info "credentials were not written; keep the invoking environment private"
info "press Ctrl-C to stop services; the external Ethereum RPC is untouched"

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
