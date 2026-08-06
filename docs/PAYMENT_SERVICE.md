# Ethereum v1 Payment Service

For step-by-step startup, configuration, the complete curl request catalog,
lifecycle workflows, and maintenance commands, see
[`PAYMENT_SERVICE_USAGE.md`](./PAYMENT_SERVICE_USAGE.md).

`apps/api` is the Payment Service (PS) composition root. It owns users,
deposits, command idempotency, the append-only IX mirror, business
classification, absolute ledgers, reconciliation cases, collection workflows,
and policy-selected master destinations. It never opens or writes IX storage.

The v1 deployment unit is deliberately narrow: one process exclusively owns
one PS RocksDB path, one Ethereum network scope, one versioned policy, and one
IX event feed. Run a separate process and database for another network.

## HTTP contract

All operation routes use `/v1`, strict JSON, bounded bodies, and bearer
authentication. Atomic amounts and large integers are unsigned decimal
strings. Ethereum addresses and transaction IDs are canonical lowercase
`0x`-prefixed hexadecimal. Non-loopback listeners require an explicit assertion
that a trusted upstream terminates TLS.

The ordinary exchange credential can use deposit, job, balance, ledger,
observation, and collection routes. The administrator credential can also use
those routes and is required for accounting, reconciliation, and status routes.
An ordinary credential receives `403` on administrator routes.

| Method and path | Result |
|---|---|
| `POST /v1/deposits` | Queue idempotent deposit creation; returns `202`, `job_id`, and `deposit_id`. |
| `GET /v1/deposits` | Page deposits by optional user/lifecycle filters. |
| `GET /v1/deposits/{id}` | Lifecycle, payment progress, expected amount, and address only after IX acknowledgement. |
| `GET /v1/deposits/{id}/balances` | Current absolute `received`, `confirmed`, `balance`, `collected`, and `accounted`. |
| `GET /v1/deposits/{id}/ledger` | Immutable absolute ledger rows. |
| `GET /v1/deposits/{id}/observations` | IX facts from the durable deposit-to-observation index, including relevant facts with no token-ledger delta such as gas funding. |
| `POST /v1/deposits/{id}/close` | Queue an atomic zero-balance close eligibility check. |
| `GET /v1/jobs/{id}` | Durable job state, attempts, safe error, and retry time. |
| `POST /v1/collections` | Queue collection using policy destination and current spendable value. |
| `GET /v1/collections` | Page collection aggregates and durable legs. |
| `GET /v1/collections/{id}` | Collection, reservation, legs, attribution, and safe failure state. |
| `POST /v1/collections/{id}/retry` | Queue an explicit retry of a failed/reorged collection. |
| `POST /v1/deposits/{id}/accounting` | Administrator-only absolute accounting decision with expected ledger head and reason. |
| `GET /v1/reconciliations[/{id}]` | Administrator-only post-correction business cases. |
| `POST /v1/reconciliations/{id}/resolve` | Administrator-only typed resolution: reverse credit, accept liability, or record external debt. |
| `GET /v1/admin/status` | Policy identity, dependency readiness, ingestion/projection lag, and bounded job backlog. |

Mutations require `Idempotency-Key`. The durable identity is the authenticated
principal, semantic operation, client key, and request hash. Exact replay
returns the original IDs/result. Reusing a key for different content returns
`409`.

Errors use a stable safe envelope:

```json
{
  "code": "machine_readable_code",
  "message": "safe contextual message",
  "retryable": false,
  "request_id": "ps-request-..."
}
```

## Deposit activation and birthday ownership

PS first reads the IX Ready checkpoint and captures its height as the deposit
birthday. It then asks stateless WS to provision an address using a durable
server-owned operation ID. WS returns only the address and opaque key locator.
PS atomically persists `AwaitingWatch` plus the zero ledger row, obtains IX's
durable idempotent watch acknowledgement, changes the deposit to `Active`, and
only then exposes the address.

A crash in the split-database window is recoverable: `AwaitingWatch` retains
the captured address, locator, purpose, and birthday, so reconciliation retries
only the IX acknowledgement and never provisions a replacement key.

Expiration retains the IX watch, allowing late and excess payments to remain
visible. Expiration never credits or collects automatically. Explicit close is
allowed only at an exact zero-balance ledger head with no active collection
reservation or open reconciliation. The close commit conditions on and
advances the ledger-head, collection-eligibility, and reconciliation versions
atomically, so a concurrent business change must retry. Ethereum v1
deliberately retains the IX address watch after `Closed`; without an IX
cutoff-and-drain barrier, removing it could hide an in-flight or late payment.
Such payments continue to project and remain eligible for collection.

## Event projection and accounting

The runtime mirrors IX events and advances the ingestion cursor atomically.
Projection advances independently in cursor order. Classification precedence
is a durable collection transaction mapping, gas-funding mapping, incoming
movement to a known deposit, then another balance change. A relevant fact that
cannot be classified stops projection and readiness.

Projection supplies only stable IX movement IDs. The repository resolves
status, previous status, revision, asset, and amounts from the immutable mirror,
runs checked U256 arithmetic, and appends one complete absolute snapshot per
affected deposit/event. Projection cannot change `accounted`; only an explicit
administrator accounting command can.

The same projection commit updates the durable deposit-to-observation index for
every affected deposit, including a token deposit whose separate native-asset
gas-funding fact intentionally produces no token ledger row. The observation
route therefore does not reconstruct history from ledger causes.

Network fees are applied only when IX identifies the deposit address as payer
and the fee asset matches the affected ledger asset. A confirmed owned native
collection records the gross deposit debit, master credit, and allocated fee
separately; fee-only outgoing facts still affect the canonical balance.

## Reconciliation decisions

A post-credit reorg preserves the historical accounting decision and opens a
blocking reconciliation case. Resolution is durable, typed, idempotent, and
atomic with any required accounting correction:

- `reverse_credit` requires `expected_ledger_head` and appends the absolute
  accounting correction before resolving the case;
- `accept_liability` requires only a reason and preserves `accounted`; and
- `external_debt_recorded` requires an opaque `external_reference`, preserves
  `accounted`, and records that business action.

Unknown fields and resolution-specific field combinations are rejected.

## Policy

The required JSON policy has no financial defaults. It binds one scope, TTL,
asset allowlist, per-asset collection threshold and master destination, fee
ceilings, and a dedicated gas-funder locator. Example:

```json
{
  "version": 1,
  "scope": {
    "chain": "ethereum",
    "network": "sepolia",
    "chain_id": 11155111
  },
  "deposit_ttl_seconds": 86400,
  "assets": [
    {
      "asset": "native",
      "master_destination": "0x1111111111111111111111111111111111111111",
      "minimum_collection_amount": "1000000000000000"
    }
  ],
  "fees": {
    "max_fee_per_gas": "100000000000",
    "max_priority_fee_per_gas": "5000000000",
    "max_gas_limit": 200000,
    "max_total_fee": "20000000000000000"
  },
  "gas_funder": {
    "address": "0x2222222222222222222222222222222222222222",
    "key_locator": "opaque-custody-locator",
    "maximum_funding_amount": "5000000000000000"
  }
}
```

The policy file and database metadata are compared at startup. The human-readable
network label and numeric EVM `chain_id` are both required. A different scope,
chain ID, or policy identity fails closed rather than reinterpreting durable
jobs. Before a collection envelope is persisted or broadcast, PS decodes it in
the Ethereum crate and rejects a mismatched chain ID, gas limit, fee cap,
priority-fee cap, or maximum total fee.

## Operations and recovery

The `serve` runtime opens RocksDB and dependency clients once, then
supervises HTTP, loopback-only Prometheus metrics, jobs, IX ingestion,
projection, watch reconciliation, expiration, collection, and readiness.
SIGINT/SIGTERM disables readiness before a bounded drain. IX and WS options use
distinct CLI names, and a command-definition regression test protects the
flattened `serve` command. See the usage guide for the complete local startup
sequence and maintenance commands.

`backup` creates and verifies a RocksDB BackupEngine snapshot. `migrate` first
creates that verified physical backup and then performs semantic migration. It
validates service ownership, Ethereum scope, journals, mirrored events,
consumer cursors, users, jobs, collections, reservations, transaction indexes,
and reconciliation references; rebuilds deposit association and
deposit-to-observation indexes; and only then atomically binds/upgrades the
database metadata to schema v2 and the active policy. Normal `serve` fails
closed on an old, unbound, IX-owned, or scope/policy-mismatched store.

Migration requires an explicit operator-supplied network for legacy rows that
did not persist network identity. Stop every process using the database before
migration. Restore the verified backup into a new database directory, validate
it, and switch configuration to that path; never overwrite the old directory
during rollback.

PS stores only public addresses, opaque key locators/purposes, ownership
metadata, and opaque signed envelopes required for crash recovery. An envelope
is persisted before the first broadcast and retained while the outcome is
unknown; its expiry is an operational alert/retention hint, not permission to
sign replacement bytes. The record is deleted atomically when broadcast is
accepted. PS does not store raw private keys, seeds, custody bearer credentials,
or signer secrets. Production storage volumes must be encrypted.

## Evidence boundary and v1 limitations

The source implements the Ethereum v1 API, persistence, workers, remote-WS
client, and native/ERC-20 collection paths. It is not evidence of a production
deployment. No checked-in result currently proves the opt-in Anvil end-to-end
scenario, a real durable custody backend, or operation against production
nodes. The v1 topology is one exclusive writer for one scope, not HA or a
multi-network database.

One strict acceptance item remains open in the current composition: collection
leg transitions driven by IX facts and the corresponding ledger/projection
cursor commit are replay-safe and idempotent, but are not yet one physical PS
storage batch. Closing that atomicity boundary and adding the complete
crash-window/workflow test matrix remain required before claiming the entire
Payment Service plan complete.
