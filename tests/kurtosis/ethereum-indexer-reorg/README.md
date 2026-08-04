# Opt-in Ethereum Indexer reorg validation

This scenario validates IX against a real pre-finality Ethereum fork. It is
manual/opt-in and is never run by `cargo test`.

The network isolates participant 1 from participants 2–4 at both execution and
consensus P2P layers. Point IX at participant 1, wait for a watched transaction
to be included on that minority branch, then heal the partition. The majority
branch should win and IX must reconcile through HTTP, append `Reorged`, replay
the replacement branch, and converge without duplicate event cursors.

Exact fork depth is deliberately not asserted. Proposer assignment and timing
make a Disruptoor fork nondeterministic; deterministic Rust doubles own the
depth 1, 12, 49, 50, and 51 acceptance cases.

## Pins

- `ethereum-package`: `8e81c5709deffad01d0bd1770d259c224f1ed349`
- Geth: `ethereum/client-go:v1.17.3`, multi-platform digest
  `sha256:ee8af3a081fd8f780509541cf8115be03259e106375e50efa9d995400f86e15d`
- Lighthouse: `sigp/lighthouse:v8.1.3`, multi-platform digest
  `sha256:44aa773dcf274122c32cc41c4dae7124ed3cab5c267ee82174c6dfd9c688da96`
- Disruptoor and Dora are also pinned by multi-platform digest in
  [`network_params.yaml`](./network_params.yaml).

Review and deliberately update all pins together. Do not replace them with
`latest`.

## Prerequisites

- Docker backend, not Kubernetes;
- a current Kurtosis CLI and engine supporting `--privileged`;
- permission to mount the Docker socket and use the host PID namespace;
- enough local CPU, memory, and disk for four Geth/Lighthouse pairs; and
- a built Indexer binary plus a fresh disposable IX database path.

Disruptoor needs privileged containers, `/var/run/docker.sock`, and host PID
access. Never run this profile on an untrusted shared Docker host.

## Start the network

Start the host-level OTel stack first:

```bash
kurtosis otel start
```

Then run the pinned package from the repository root:

```bash
kurtosis run \
  --enclave payment-sdk-ix-reorg \
  github.com/ethpandaops/ethereum-package@8e81c5709deffad01d0bd1770d259c224f1ed349 \
  --args-file tests/kurtosis/ethereum-indexer-reorg/network_params.yaml \
  --image-download always \
  --privileged \
  --verbosity detailed
```

Inspect the enclave and record participant 1's published Geth HTTP and
WebSocket endpoints, the majority participant endpoints, Dora, and Disruptoor:

```bash
kurtosis enclave inspect payment-sdk-ix-reorg
```

Do not put RPC credentials, bearer tokens, or private keys in this directory or
captured test output.

## Exercise IX

1. Start IX with participant 1's Geth HTTP endpoint as the authoritative RPC.
   Keep WebSocket disabled for the first run, or deliberately disconnect it
   during the fork to prove HTTP reconciliation is sufficient.
2. Use the enclave's disposable prefunded test account to submit a transfer to
   a watched address on participant 1. This is devnet-only test value; never
   reuse a real key.
3. Wait until participant 1 includes the transaction and IX publishes its
   `Included` revision. Record the event ID, cursor, transaction hash, inclusion
   height, and inclusion hash.
4. Confirm through Dora or both branch RPC endpoints that the partition has
   produced different block hashes. If participant 1 has not included the test
   transaction, keep the partition active rather than weakening assertions.
5. Heal `isolate-indexer-branch` using the Disruptoor control surface exposed by
   the enclave. Confirm peer connectivity and wait for participant 1 to adopt
   the majority canonical branch.
6. Allow at least one regular IX poll cycle. Optional WebSocket reconnection may
   wake the worker but is not evidence of canonical state.

## Required assertions

- The stored pre-heal inclusion hash becomes non-canonical at its height.
- IX appends exactly one `Reorged` revision for that orphaned inclusion; it does
  not delete or rewrite the prior event.
- Replacement blocks connect in ascending order and the IX checkpoint
  height/hash converges to participant 1's HTTP canonical tip.
- Event cursors are strictly increasing and unique before and after an IX
  restart/reconnect.
- Replaying from the cursor before the fork returns the same published event
  prefix and all correction events.
- No `eth_pendingTransactions`, pending subscription, txpool, trace,
  `safe`, or `finalized` RPC is issued.
- An RPC outage pauses canonical mutation and is reported retryable; it never
  creates `Dropped` or `Failed` by inference.

Keep the enclave on failure so logs and OTel data remain inspectable. Remove it
explicitly after collecting evidence:

```bash
kurtosis enclave rm -f payment-sdk-ix-reorg
```

The host-level OTel stack is shared with other enclaves; stop it only when no
other work depends on it.
