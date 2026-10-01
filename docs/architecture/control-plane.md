# Control plane

- [Accepted state and control-plane authority](#accepted-state-and-control-plane-authority)
- [Identity and atomic transitions](#identity-and-atomic-transitions)
- [Module ownership and compatibility](#module-ownership-and-compatibility)
- [Governance audit](#governance-audit)

## Accepted state and control-plane authority

PostgreSQL owns accepted runs, task transitions, requests, leases and journals.
API, reconciler, CLI and editor clients share this authority. Workers execute
bounded assignments and publish proposals; provider conversations cannot commit
accepted state. The server requires no container runtime socket.

## Identity and atomic transitions

### Identity and ownership

Each logical task has a stable `task_id`; each claim creates a new `attempt_id`
and monotonically increasing task generation. A task's stable idempotency key
does not change across attempts. An attempt also carries:

```text
run_id, task_id, attempt_id, generation
base_revision, workspace_id
recovery_policy, deadline_at
input_artifact_ids, output_artifact_ids, checkpoint_artifact_id (optional)
```

The kernel persists the identities and ownership. Workers manage workspace
creation and execution. Workspace IDs MUST be unique per attempt. A retry MUST
NOT reuse the mutable workspace of an earlier attempt, even on the same worker.
The developer's checkout MUST NOT be used as an attempt workspace.

In `orbit/v0`, a coding result is a patch against `base_revision`, with a
manifest containing its checksum, producing attempt, and changed paths. Binary
changes MUST be representable; untracked output files intended as changes MUST
be included. The worker rejects unsupported patch content explicitly. An empty
patch is a valid output if reported as such; qualification requires a real change.

Testing starts from a fresh workspace at the same base revision, applies exactly
the accepted patch, and runs the frozen commands. It does not consume the coding
worker's mutable directory. Patch application failure is a task failure with
diagnostics, not permission to modify the patch. Test output includes command
arguments, exit status, timeout status, and logs as artifacts.

### Durability and atomicity

PostgreSQL is authoritative for execution state. Each accepted state transition,
its reason, and its journal event MUST commit in the same transaction. An API
response or in-memory notification is not the durable transition.

Claims serialize eligibility checks with attempt creation and lease assignment.
Completion serializes ownership checks with attempt finalization, accepted
artifact references, task outcome, and dependent readiness. Completion and
cancellation for a run MUST serialize through a common lock or equivalent
transactional guard. No observer may see a successful task without its accepted
outputs, or a ready test task without an accepted coding output.

Reconciliation MUST be repeatable across server restarts and concurrent
reconcilers. It resolves expired leases, due retries, exceeded deadlines, and run
outcomes using durable state. It MUST NOT recreate completed work. PostgreSQL
time is authoritative for leases, deadlines, and retry eligibility.

## Module ownership and compatibility

Each module has a domain owner. Public compatibility paths remain available;
physical namespaces do not change database, workflow or policy authority.
[Core invariants](../requirements/core-invariants.md) apply across all owners.

### Interactive workflows

| Owner | Modules | Responsibility |
| --- | --- | --- |
| Interactive | `interactive.rs` | Durable sessions, coordinator control and candidate views/actions |
| ACP | `acp/editor.rs`, compatibility `acp/service.rs` | Client protocol/presentation over interactive control |
| Execution | `execution/local.rs`, `execution/worktree.rs` | Confined exploratory terminals and managed candidate actions |
| Execution | private `execution/process.rs` | Bounded native coordinator process capture |
| Tools | `tools/budget.rs` | Role resource admission and usage |
| Workflow | `workflow/flow.rs`, `workflow/reasoning.rs` | Immutable skill policy and external reasoning/acceptance contracts |

Integration targets follow the same ownership: `editor_qualification`,
`developer_local`, and `role_budget`.

CLI and protocol clients share `InteractiveService`; neither owns workflow
progression, provider selection or verification truth. Sessions bind immutable
instructions, policy and execution profile to managed candidates in PostgreSQL.
Clients recover by session ID and query state without provider conversation
history. Status notifications are observations, not transition authority.

Cancellation revokes workflow and role authority before cleanup completes. A
matching supervisor receipt may subsequently record cleanup for that exact
cancelled execution. This observation cannot publish late results or success;
candidate actions still require confirmed cleanup and released ownership.

### Modules and compatibility paths

Each library module has one owner. Compatibility exports preserve established callers.

| Owner | Established library path | File and responsibility |
| --- | --- | --- |
| ACP | `orbit::acp` | `src/acp/preflight.rs`: credential-free runtime preflight |
| ACP | `orbit::acp_broker` | `src/acp/broker.rs`: fenced client callback dispatch |
| ACP | `orbit::acp_capabilities` | `src/acp/capabilities.rs`: advertised protocol capabilities |
| ACP | `orbit::acp_contract` | `src/acp/contract.rs`: pinned execution and accounting policy |
| ACP | `orbit::acp_files` | `src/acp/files.rs`: confined ACP file callbacks |
| ACP | `orbit::acp_process` | `src/acp/process.rs`: supervised process lifecycle and receipts |
| ACP | `orbit::acp_runtime` | `src/acp/runtime.rs`: runtime registry, auth staging, execution |
| ACP | `orbit::acp_terminal` | `src/acp/terminal.rs`: bounded asynchronous terminal ownership |
| ACP | `orbit::acp_wire` | `src/acp/wire.rs`: bounded protocol sessions |
| Workflow | `orbit::workflow` | `src/workflow/domain.rs`: roles, handoffs, state, runtime resolution and mutation ownership |
| Workflow | `orbit::workflow_coordinator` | `src/workflow/coordinator.rs`: progression, repair, completion and repository callbacks |
| Workflow | `Private role prompts` | `src/workflow/role_prompt.rs`: private role prompts and tool lists |
| Workflow | `Private coordinator child` | `src/workflow/role_execution.rs`: private coordinator child for live ACP execution |
| Workflow | `orbit::continuation` | `src/workflow/continuation.rs`: pure continuation and handoff decisions |
| Verification | `orbit::verification` | `src/verification/engine.rs`: authoritative commands, exact candidate identity and durable results |
| Verification | `orbit::regression_strategy` | `src/verification/regression.rs`: immutable check selection and regression tiers |
| Verification | `orbit::integration_environment` | `src/verification/integration_environment.rs`: managed services, readiness and teardown |
| Verification | `orbit::browser_verification` | `src/verification/browser.rs`: isolated browser harness and evidence |
| Credentials | `orbit::credential_registry` | `src/credentials/registry.rs`: provider-neutral catalog and lifecycle |
| Credentials | `orbit::credential_enrollment` | `src/credentials/enrollment.rs`: enrollment publication and orchestration |
| Credentials | `orbit::credential_status_view` | `src/credentials/status_view.rs`: safe status presentation |
| Credentials | `orbit::secret_backend` | `src/credentials/secret_backend.rs`: private secret storage and staging |
| Providers | `orbit::provider_scope` | `src/providers/scope.rs`: observed provider identity and confirmation |
| Providers | `orbit::provider_status` | `src/providers/status.rs`: bounded status normalization and persistence |
| Providers | `orbit::availability` | `src/providers/availability.rs`: availability freshness and eligibility evidence |
| Codex provider | `orbit::codex_bridge` | `src/providers/codex/bridge.rs`: version-specific App Server to ACP bridge |
| Codex provider | `orbit::codex_session` | `src/providers/codex/session.rs`: observed Codex session lifecycle |
| Codex provider | `orbit::codex_credential_enrollment` | `src/providers/codex/enrollment.rs`: isolated device-code enrollment |
| Codex provider | `orbit::codex_status_probe` | `src/providers/codex/status_probe.rs`: catalog-backed native status probing |
| Antigravity provider | `orbit::agy_cli_representation` | `src/providers/antigravity/cli_representation.rs`: CLI credential representation |
| Antigravity provider | `orbit::agy_usage_schema` | `src/providers/antigravity/usage_schema.rs`: selective usage schema parsing |
| Execution | `orbit::agent` | `src/execution/agent.rs`: worker runtime bindings and bounded agent contracts |
| Execution | `orbit::command_agent` | `src/execution/command_agent.rs`: trusted command adapter |
| Execution | `orbit::coding_agent` | `src/execution/coding_agent.rs`: bounded Responses adapter and per-call receipts |
| Execution | `orbit::execution` | `src/execution/profile.rs`: logical requirements and pinned execution profiles |
| Execution | `orbit::workspace` | `src/execution/workspace.rs`: isolated workspace materialization and validation |
| Execution | `orbit::repository` | `src/execution/repository.rs`: private Git materialization and repository bindings |
| Execution | `orbit::container` | `src/execution/container.rs`: lease-bound container execution |
| Execution | `orbit::compute` | `src/execution/compute.rs`: compute capabilities and execution metadata |
| Tools | `orbit::fs_tools` | `src/tools/filesystem.rs`: explicit confined repository mutations |
| Tools | `orbit::tool_surface` | `src/tools/surface.rs`: canonical tools, policy, bounded reads and safe Git |
| Control plane | `orbit::engine` | `src/control_plane/engine.rs`: durable graph transitions and reconciliation |
| Control plane | `orbit::worker` | `src/control_plane/worker.rs`: assignment leases and worker supervision |
| Control plane | `orbit::api` | `src/control_plane/api.rs`: authenticated HTTP interface |
| Control plane | `orbit::governance` | `src/control_plane/governance.rs`: scoped authorization and admission |
| Control plane | `orbit::registry` | `src/control_plane/registry.rs`: immutable signed package registry |
| Control plane | `orbit::ops` | `src/control_plane/operations.rs`: probes, metrics and lifecycle |
| Telemetry | `orbit::telemetry` | `src/telemetry/agent.rs`: bounded agent execution telemetry |
| Telemetry | `orbit::artifacts` | `src/telemetry/artifacts.rs`: immutable artifact publication and verified reads |
| Telemetry | `orbit::evidence` | `src/telemetry/evidence.rs`: qualification evidence export |
| Telemetry | `orbit::run_export` | `src/telemetry/run_export.rs`: journal-bounded review exports |
| Shared domain | `orbit::model` | Retain `src/model.rs`: cross-subsystem durable graph definitions and legacy digests |
| Client interface | `orbit::mcp` | Retain `src/mcp.rs`: MCP interface over the same control plane |
| Client interface | `orbit::sdk` | Retain `src/sdk.rs`: supported Rust peer interface |

`src/lib.rs` remains the library facade. `src/main.rs` and
`src/bin/orbit-status-gate-a.rs` remain executable entry points. Database
migrations retain their existing directory and immutable history.

Provider-specific implementations may depend on the provider-neutral credential
catalog. The catalog's existing enrollment orchestration can continue invoking
those adapters; separating their files does not justify redesigning that call
direction. The generic ACP broker remains separate from the Codex bridge.

### Public compatibility

Explicit subsystem `mod.rs` files define the public module surface. Root modules such as
`orbit::acp_runtime` and `orbit::workflow_coordinator` are already consumed by
integration tests, the CLI, examples, and SDK clients. Preserve those paths with
explicit compatibility re-exports; file layout must not create an unannounced API break.

The current `orbit::acp`, `orbit::workflow`, `orbit::verification`,
`orbit::execution`, and `orbit::telemetry` paths also expose domain items.
Subsystem facades must retain those item paths while adding their owned modules.
Keep the role prompt and live role execution implementation private. The live
executor remains a child of the coordinator because it intentionally accesses
coordinator internals; moving its file does not require widening visibility.

Public API cleanup means making these compatibility exports intentional and
documented. Removing a supported root path requires a separate compatibility
decision. Module moves must not rename serialized types, table fields, policy
identifiers, or persisted evidence values.

## Governance audit

### Audit controls

With governance enabled, authorization of mutations and denied resource actions
is recorded in `orbit_audit`, with authenticated actor, action, scope, optional
run ID, decision and database timestamp. No request bodies, bearer credentials or
provider keys are copied into this journal. The record means authorization was
evaluated, not that a later mutation committed successfully. Committed run/attempt
effects remain in the transactional run journal with the correct actor.

`orbit audit --after CURSOR` / `GET /audit?after=CURSOR` returns up to 256 ordered
records. Each carries the previous record hash and SHA-256 of compact JSON
`[previous_hash,event]`. Concurrent servers serialize appends under the control
row lock. API callers cannot update/delete audit records. A database administrator
can still rewrite/truncate history; retain independently witnessed hash heads,
backups and restricted database credentials for stronger tamper evidence.
Retention, external witnessing, denied worker transport auditing, and policy
change distribution are not automated in this release.
