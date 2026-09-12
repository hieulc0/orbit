# Current architecture

Orbit is a Rust control plane for bounded durable graphs. It coordinates work
performed by external workers; it does not run workflow code or an LLM loop in
the scheduler. The [vision](vision.md) describes the long-term direction. The
[roadmap](../ROADMAP.md) separates current capabilities from remaining gates.

```text
CLI / MCP / React UI / SDK
            |
       authenticated HTTP
            |
       API + reconciler ----- PostgreSQL (authoritative state and journal)
            |                       |
       artifact providers      scoped claims / leases
                                    |
                       trusted workers on dedicated hosts
                       repository / OCI / command agent
```

The server image contains the API, reconciler and static UI. It requires no
container runtime socket. Host workers own their runtime access and disposable
workspaces. The deployment is single-host first; PostgreSQL is not replaced by
an in-memory broker or a second execution authority.

## Code map

| Module | Responsibility |
| --- | --- |
| `src/model.rs` | Strict definitions, immutable plans, durable run/task/attempt types |
| `src/engine.rs`, `migrations/` | PostgreSQL transitions, coordination, deduplication, reconciliation |
| `src/api.rs`, `src/governance.rs` | Authentication, scoped authorization, admission policy |
| `src/worker.rs`, `src/container.rs` | Lease-bound execution, workspaces, process supervision |
| `src/agent.rs`, `src/command_agent.rs` | Agent contracts and trusted command-runtime adapter |
| `src/artifacts.rs` | Local/S3 immutable publication and verified reads |
| `src/registry.rs` | Signed immutable package metadata; no code loading |
| `src/ops.rs`, `src/main.rs` | Lifecycle, probes, metrics, executable commands |
| `src/mcp.rs`, `src/sdk.rs`, `sdk/python/`, `ui/` | Peer interfaces over the same server |
| `tests/kernel/`, `scripts/` | Disposable qualification and repeatable operational checks |

## Invariants to preserve

PostgreSQL owns accepted state. A run is a JSONB aggregate plus its ordered journal.
A shared control-row lock serializes cross-run mutations; this favors auditable
correctness over high-throughput scheduling. Storage I/O must not hold that lock.
Recheck lease/generation/cancellation and request identity after external I/O.

Plans pin definitions, repository/model/tool bindings and execution scope. Preserve
legacy serialized digests. Worker protocol success requires an accepted receipt,
not merely HTTP 200. At-least-once execution never proves exactly-once external
effects. Draining does not cancel existing leases; stopping is not proof that an
external process or provider stopped. See the [worker contract](../reference/worker-protocol.md).

## Supported boundary

Trusted Linux operators/workers, bounded graph/agent/artifact sizes, static
replica-consistent configuration, single-host packaging and coordinated upgrades.
No hostile multi-tenant isolation, checkpoint resume, remote runtime mounts,
automatic model selection, provider billing enforcement, rolling mixed-version
upgrades, or HA/performance guarantee. [Security](../../SECURITY.md) expands this boundary.
