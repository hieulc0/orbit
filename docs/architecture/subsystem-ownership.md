# Rust subsystem ownership

This ownership map relates the established library paths at checkpoint
`14769c6083b0ac47d4ba6c77a773b6040a9d0267` to their subsystem files. Physical
movement and its [qualification](../operations/r5-modularization-report.md) are complete. Existing
[architecture invariants](README.md#invariants-to-preserve) continue to apply.

The folders below express responsibility rather than introducing new runtime
boundaries. State machines, database tables, serialized types and digests,
provider selection, policy authority, callback fencing, and cleanup retain
their current semantics. No new crate or generic abstraction is required.

## Ownership and destination

Each existing module has one owner. The destination is a navigation
change; moving a module does not move authority to a different component.

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
| Workflow | `orbit::continuation` | `src/workflow/continuation.rs`: durable attempt continuation and handoffs |
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

## Public compatibility

Explicit subsystem `mod.rs` files define the public module surface. Root modules such as
`orbit::acp_runtime` and `orbit::workflow_coordinator` are already consumed by
integration tests, the CLI, examples, and SDK clients. Preserve those paths with
explicit re-exports during physical movement rather than making file layout an
unannounced API break.

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

## Test ownership and migration

| Owner | Existing integration targets |
| --- | --- |
| ACP | `acp`, `acp_broker`, `acp_capabilities`, `acp_contract`, `acp_runtime` |
| Workflow | `workflow_qualification`, `workflow_orchestration_qualification`, `real_acp_role_execution_qualification`, `continuation` |
| Verification | `verification_qualification`, `regression_strategy_qualification`, `integration_environment_qualification`, `browser_verification_qualification` |
| Credentials | `credential_registry_pg`, `codex_enrollment_pg`, `secret_backend` |
| Providers | `availability`, `provider_scope`, `codex_bridge` |
| Execution and tools | `agents`, `command_agent`, `execution`, `repository_filesystem_mutation_qualification`, `core_coding_agent_tool_surface_qualification` |
| Telemetry | `agent_telemetry`, `evidence`, `run_export` |
| Control plane and shared domain | `kernel`, `definition`, `developer`, `governance`, `registry`, `submission` |

Tests mirror production ownership where it improves navigation. Explicit
`[[test]]` paths preserve Cargo target names, so documented qualification commands
retain their meaning. Relative fixture/example includes and shared test support
paths follow their new locations. The mixed kernel harness remains together: its database,
worker, deployment and fault cases intentionally qualify shared control-plane
behavior. Qualification names remain explicit after movement.

## Movement and qualification

Move ACP, workflow, verification, credentials/providers, then execution/tools.
Apply the remaining control-plane and telemetry ownership without changing
behavior. Update test locations, compatibility exports, current architecture
links and contributor guidance afterward. Historical results remain historical;
repair source links without upgrading old evidence claims.

Review include paths, source-inspection assertions, fixture paths, image build
inputs, and test filters as part of each move. The final qualification includes
formatting, Clippy, all regular tests, documentation links, and the applicable
explicit disposable B1–B6/ACP cases. Live-provider calls remain separately
guarded. A compiling namespace or generic ignored-test count alone does not
close R5 acceptance.
