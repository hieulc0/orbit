# Delivery roadmap

Milestone 1 was accepted by the project owner on 2026-09-07. The architecture
vision remains the long-term scope; acceptance does not imply production readiness.

## Complete: Phase 2 durable interaction

Build on the accepted kernel in this dependency order:

1. Introduce a versioned graph definition while preserving strict `orbit/v0`
   compatibility. Validate dependency references and cycles, compile immutable
   plans, and remove positional assumptions from scheduling and artifact routing.
   Implemented with static branches and joins; see [graph execution](GRAPH_EXECUTION.md).
2. Add durable timers and signal waits. Specify early delivery, duplicate and
   conflicting signals, deadlines, cancellation races, and restart behavior before
   implementing transactional transitions and operator API/CLI commands.
   Implemented with one-shot signals and server-kill qualification; see
   [durable interaction](DURABLE_INTERACTION.md).
3. Add bounded fan-out and joins with stable task identities and explicit failure
   semantics. Validate limits before admitting work.
   Implemented with literal/signal-driven child inputs and bounded parallelism.
4. Add child definition execution with pinned plans, transactional linkage, and
   explicit completion and cancellation propagation.
   Implemented with inline definitions, nested propagation and creation-commit kill tests.
5. Add concurrency limits and admission backpressure with database-enforced
   bounds across concurrent server processes.
   Implemented with shared operator limits, per-run caps and retryable admission.

Each increment needs executable examples, documented semantics, and relevant
PostgreSQL recovery/concurrency qualification. Phase 2 completion requires all
five increments; all five are implemented and mapped to executable evidence in
[Phase 2 qualification](PHASE_2_QUALIFICATION.md).

## Complete: Phase 3 developer surface

SSE with durable replay cursors, JSONL and CLI event following, Rust worker SDK
exports and a dependency-free Python transport SDK are implemented. The existing
API/CLI compatibility contract and SDK responsibilities are documented in
[developer surface](DEVELOPER_SURFACE.md). All 28 PostgreSQL/process tests pass
with both concurrent and serial runners; regular Rust/Python checks also pass.
Qualification fixes cover publication without blocking lease renewal, confirmed
lease budgets and race-correct fixtures. See the evidence mapping and bounded
completion record in [Phase 3 qualification](PHASE_3_QUALIFICATION.md).
The next phase is compute and artifacts.

## Phase scopes

| Phase | Deliverables |
| --- | --- |
| 3: Developer surface | SSE, JSONL, Rust and Python worker SDKs, stable API and CLI contracts |
| 4: Compute and artifacts | S3-compatible storage, container runner, resources and worker pools |
| 5: Agent execution | Model/tool bindings, budgets, permissions, MCP, delegation and approvals |
| 6: Operations UI | Runs, timeline, attempts, artifacts, workers, queues and operator actions |
| 7: Definition IDE | Graph views/editing, schema panels, source synchronization and validation |
| 8: Governance | Projects, environments, RBAC, service accounts, secret providers and audit |
| 9: Ecosystem | Package registry, verification and SDK stabilization; marketplace if justified |

Phases 4 onward describe future release scopes, not implemented capabilities.
External providers will require selected services and runtime configuration.
Publishing, deployment, and third-party messages require their own authorization.
