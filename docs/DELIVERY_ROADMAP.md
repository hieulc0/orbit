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
Phase 4 builds on this developer surface.

## Complete: Phase 4 compute and artifacts

Implemented local/S3-compatible artifact providers, immutable publication,
repository-free container definitions, a supervised local container runner,
resource reservations, GPU device assignments, capability/pool placement and
worker/queue inspection. Storage verification occurs outside scheduler locks and
rechecks ownership before commit. See [the contract](COMPUTE_AND_ARTIFACTS.md).

All 33 PostgreSQL/process/S3/OCI cases pass with the concurrent runner, including
actual rootless Podman output and recovery after combined server/worker kills.
Regular Rust/Python checks pass and local evidence is exported and reviewed.
This qualifies the bounded CPU/OCI and artifact contract; physical GPU execution
and the stalled host Docker backend are not claimed as qualified. See
[Phase 4 qualification](PHASE_4_QUALIFICATION.md) for evidence and limits.
The later phases are now implemented as bounded contracts, described below.

## Implemented and qualified: Phases 5–9

- Phase 5: First-class agent steps, immutable model/tool bindings, conservative
  cross-attempt budgets, permissions, MCP stdio, controlled durable delegation and
  assigned human approval. [Agent contract](AGENT_EXECUTION.md).
- Phase 6: React operations console for runs, durable timeline, attempts, artifacts,
  workers, queues, failure inspection and confirmed operator actions.
- Phase 7: Canonical YAML/graph editing, schema-driven panels, source synchronization,
  comparison, validation and import/export. [Console/studio contract](WEB_CONSOLE.md).
- Phase 8: Organizations/projects/environments, scoped RBAC, service accounts,
  environment/file credential providers, admission policies and an access-audit
  chain. [Governance contract](GOVERNANCE.md).
- Phase 9: Private immutable package/capability catalog, trusted Ed25519 publishers,
  verified package-to-run CLI and documented SDK wire compatibility.
  [Registry contract](PACKAGE_REGISTRY.md). A public marketplace remains conditional
  and is deferred because an external distribution need has not been established.

The complete 41-case PostgreSQL/process/OCI/S3/browser suite passes, alongside
regular Rust, Python and browser checks. [Release qualification](RELEASE_QUALIFICATION.md)
records the evidence and bounded scope. This closes the local implementation
increments, not every aspirational production feature in the architecture vision.

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

External model/secret providers still require selected services and runtime configuration.
Physical GPU qualification, the stalled Docker backend, hostile-agent isolation,
HA/performance, SSO/policy distribution and external marketplace operations are
not claimed by the local release. The individual contracts state these limits.
Publishing, deployment, and third-party messages require their own authorization.
