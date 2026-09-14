# Current architecture

Orbit is a Rust control plane for bounded durable graphs. It coordinates work
performed by external workers; it does not run workflow code or an LLM loop in
the scheduler. The [vision](vision.md) describes the long-term direction. The
[roadmap](../ROADMAP.md) separates current capabilities from remaining gates.

The [ACP integration design](acp-agent-integration.md) describes the experimental
worker registry, supervised agent process, file/terminal broker and Codex bridge.
Offline workflows and the final local fault regression have passed; live acceptance
remains a separate gate. Existing Responses/command runtimes and trusted isolation
stay intact.

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
                       repository / OCI / command or coding agent
```

The server image contains the API, reconciler and static UI. It requires no
container runtime socket. Host workers own their runtime access and disposable
workspaces. The deployment is single-host first; PostgreSQL is not replaced by
an in-memory broker or a second execution authority.

The [remote coding worker](../guides/remote-coding.md) materializes private HTTPS
Git repositories and performs a bounded model loop on the trusted worker. Each
repository tool runs in a disposable host-managed rootless Podman container,
without Git metadata, credentials or network. Tasks request a logical isolation
class; the operator pins its execution profile. Only `trusted` workspace execution
is implemented. Optional governance stays compatible, but adding a tenant
hierarchy is not part of this milestone. Authorization and containment are
independent boundaries, not interchangeable permission strings.

## Code map

| Module | Responsibility |
| --- | --- |
| `src/model.rs` | Strict definitions, immutable plans, durable run/task/attempt types |
| `src/engine.rs`, `migrations/` | PostgreSQL transitions, coordination, deduplication, reconciliation |
| `src/api.rs`, `src/governance.rs` | Authentication, scoped authorization, admission policy |
| `src/worker.rs`, `src/container.rs` | Lease-bound execution, workspaces, process supervision |
| `src/execution.rs`, `src/workspace.rs`, `src/repository.rs` | Logical requirements, pinned OCI profiles, isolated tools and private Git materialization |
| `src/agent.rs`, `src/command_agent.rs` | Agent contracts and trusted command-runtime adapter |
| `src/coding_agent.rs` | Bounded Responses loop, per-call dispatch intent/receipts and tool authorization |
| `src/acp.rs`, `src/acp_contract.rs` | Credential-free ACP preflight, pinned policy, execution-only limits/charges |
| `src/acp_runtime.rs`, `src/acp_process.rs`, `src/acp_wire.rs` | Pinned registry, auth quarantine, agent supervision and bounded protocol sessions |
| `src/acp_broker.rs`, `src/acp_files.rs`, `src/acp_terminal.rs` | Lease-fenced client callbacks, confined files and asynchronous supervised terminals |
| `src/codex_bridge.rs`, `src/codex_session.rs` | Version-specific Codex App Server bridge and dynamic-tool-to-ACP routing |
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

Plans pin definitions, repository/model/tool bindings, referenced execution profiles
and optional existing execution scope. Preserve
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
