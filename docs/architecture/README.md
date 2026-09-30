# Current architecture

Orbit is a Rust control plane for bounded durable graphs. It coordinates work
performed by external workers; it does not run workflow code or an LLM loop in
the scheduler. The [vision](vision.md) describes the long-term direction. The
[roadmap](../ROADMAP.md) separates current capabilities from remaining gates.

The [ACP integration design](acp-agent-integration.md) describes the experimental
worker registry, supervised agent process, file/terminal broker and Codex bridge.
Local qualification includes offline workflows, fault regressions and bounded
live Codex execution. Account-specific and separate-host acceptance remain gates;
see [worker setup and evidence](../guides/acp-coding.md). Existing
Responses/command runtimes and trusted isolation stay intact.

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
with Attempt-owned Git metadata, without provider credentials or network. Tasks request a logical isolation
class; the operator pins its execution profile. Only `trusted` workspace execution
is implemented. Optional governance stays compatible, but adding a tenant
hierarchy requires a concrete product requirement. Authorization and containment are
independent boundaries, not interchangeable permission strings.

## Interactive workflows

The [stdio ACP service](../guides/editor-acp.md) presents durable workflow state
and managed candidates to an editor. `acp/service.rs` delegates progression to the
same workflow coordinator; `acp/editor.rs` owns protocol presentation and replay.
`execution/local.rs` confines developer-local exploratory terminals, while final
verification retains its independent pinned rootless OCI profile.
`execution/worktree.rs` manages explicit candidate apply/discard, and
`tools/budget.rs` accounts for production role resources.

`workflow/flow.rs` maps skills to immutable flow policy.
`workflow/reasoning.rs` stores external BA/SA artifacts and frozen contracts;
BUSINESS_ACCEPTANCE is a durable coordinator stage after technical qualification.
External reasoning connections receive artifact authority without mutation or
implementation control. See [external reasoning](../guides/external-reasoning.md).

## Code map

The [subsystem ownership map](subsystem-ownership.md) covers every library module
and the compatibility exports retained by the subsystem namespaces.

| Module | Responsibility |
| --- | --- |
| `src/model.rs` | Strict definitions, immutable plans, durable run/task/attempt types |
| `src/credentials/registry.rs`, `src/credentials/secret_backend.rs` | Operator credential metadata/lifecycle and owner-only local secret storage; see [credential registry](credential-registry.md) |
| `src/control_plane/engine.rs`, `migrations/` | PostgreSQL transitions, coordination, deduplication, reconciliation |
| `src/control_plane/api.rs`, `src/control_plane/governance.rs` | Authentication, scoped authorization, admission policy |
| `src/control_plane/worker.rs`, `src/execution/container.rs` | Lease-bound execution, workspaces, process supervision |
| `src/execution/profile.rs`, `src/execution/workspace.rs`, `src/execution/repository.rs` | Logical requirements, pinned OCI profiles, isolated tools and private Git materialization |
| `src/execution/agent.rs`, `src/execution/command_agent.rs` | Agent contracts and trusted command-runtime adapter |
| `src/execution/coding_agent.rs` | Bounded Responses loop, per-call dispatch intent/receipts and tool authorization |
| `src/acp/preflight.rs`, `src/acp/contract.rs` | Credential-free ACP preflight, pinned policy, execution-only limits/charges |
| `src/acp/runtime.rs`, `src/acp/process.rs`, `src/acp/wire.rs` | Pinned registry, auth quarantine, agent supervision and bounded protocol sessions |
| `src/acp/broker.rs`, `src/acp/files.rs`, `src/acp/terminal.rs` | Lease-fenced client callbacks, confined files and asynchronous supervised terminals |
| `src/providers/codex/bridge.rs`, `src/providers/codex/session.rs`, `src/providers/codex/enrollment.rs`, `src/providers/codex/status_probe.rs` | Version-specific Codex App Server bridge, isolated device-code enrollment, catalog-backed status probing, and dynamic-tool-to-ACP routing |
| `src/telemetry/artifacts.rs` | Local/S3 immutable publication and verified reads |
| `src/control_plane/registry.rs` | Signed immutable package metadata; no code loading |
| `src/control_plane/operations.rs`, `src/main.rs` | Lifecycle, probes, metrics, executable commands |
| `src/mcp.rs`, `src/sdk.rs`, `sdk/python/`, `ui/` | Peer interfaces over the same server |
| `src/workflow/domain.rs` | Role definitions, structured handoffs, durable workflow state and fenced mutation ownership |
| `src/workflow/coordinator.rs` | Workflow stage, progression, repair and completion decisions; repository callbacks and exact tool auditing |
| `src/workflow/role_prompt.rs` | Private role prompt construction and advertised repository tool lists |
| `src/workflow/role_execution.rs` | Private coordinator child module for live ACP role execution, lifecycle evidence and supervisor cleanup |
| `src/verification/engine.rs`, `src/verification/regression.rs` | Authoritative verification, immutable check selection and candidate-bound evidence |
| `tests/kernel/`, `scripts/` | Disposable qualification and repeatable operational checks |

The role workflow coordinator delegates prompt construction and live execution
while retaining workflow decisions. `RoleAgentExecutor`, `RoleExecutionOutcome`
and `RealAcpRoleExecutor` remain available through `workflow::coordinator` and the compatibility path `workflow_coordinator`; the
extracted implementation modules are private. Structured handoff parsing and
verification selection retain their existing owners. This boundary does not
change role permissions, mutation locks, exact tool auditing, WorkspaceState
binding or cleanup requirements.

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
LLM-directed model selection, provider billing enforcement, rolling mixed-version
upgrades, or HA/performance guarantee. [Security](../../SECURITY.md) expands this boundary.
