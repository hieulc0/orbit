# Orbit Self-Development Control Plane

Status: proposed architecture draft, documentation-only.

Repository baseline inspected for this draft: 833ffe7721be34780dc1b2f511c55f5bb0c11464
(feat(runtime): harden agent execution and qualification). The baseline is the
pre-Q7 hardening state. This document describes a future evolution; it does not
claim that the proposed availability, role, review, repair, or goal features are
implemented.

The design is grounded in the [current architecture and code
map](README.md), the [engine semantics](../reference/engine-semantics.md), the
[graph contract](../reference/graphs.md), the [agent contract](../reference/agents.md),
the [ACP integration design](acp-agent-integration.md), and the current
[roadmap](../ROADMAP.md).

The source prompt for this draft is the archived
[role-agent brief](../archive/role-agent.md). It is an input to this design,
not a runtime contract.

## 1. Purpose

Orbit already has a durable execution plane for bounded graph work. The next
evolution is a policy-controlled control plane that can coordinate a human goal
through independently verified agent activities:

~~~text
Human goal
    |
    v
Orbit plan and policy
    |
    +--> plan
    +--> implement
    +--> deterministic validate
    +--> exploratory test / semantic review
    +--> bounded repair when authorized
    +--> revalidate and rereview
    |
    +--> terminal result or human intervention
~~~

The immediate proving ground is an engineering change to Orbit itself. “Orbit
can develop Orbit” means that Orbit can run this loop from an immutable
starting revision, with isolated workspaces, independently controlled
validation, explicit budgets, durable evidence, and a human-controlled
commit/release boundary. It does not mean that an agent can alter the live
control plane, grant itself privileges, or recursively redefine its own policy.

This document answers the design questions required before implementation:

- what an execution resource is and how its availability is observed;
- how runtime, credential, model, reasoning effort, role, capability, and
  activity kind remain distinct;
- how Planner, Implementer, Tester, and Reviewer activities fit the existing
  Task/Attempt/AgentExecution model;
- how deterministic validation remains authoritative;
- how continuation differs from role transition and repair;
- why the existing Task/Attempt model remains generic and why no universal
  activity-execution hierarchy is needed now;
- how the architecture can grow toward durable branching and parallel analysis;
- which boundaries remain human-controlled.

No runtime code, migration, CLI command, test, provider integration, or
security policy is changed by this draft.

## 2. Current Baseline

### 2.1 Durable execution model

The current implementation is a Rust control plane for bounded declarative
graphs. A submitted Definition is compiled into an immutable Plan; a Run
stores that plan, its Task instances, accepted artifacts, and a monotonic
journal sequence. PostgreSQL stores the authoritative run JSON aggregate in
orbit_runs, the journal in orbit_events, request deduplication in
orbit_requests, and the shared coordination boundary in orbit_control.

The current orbit/v1 graph is a bounded static dependency graph. It supports
repository coding/testing, containers, joins, timers, waits, child runs,
bounded fan-out, agent activities, and human approval. It is not yet a
general-purpose dynamic workflow language or a loop controller.

### 2.2 Code findings

| Existing code | Finding relevant to this design |
| --- | --- |
| [src/model.rs](../../src/model.rs) | Defines Definition, Step, Plan, Run, Task, Attempt, Artifact, Assignment, and fenced worker actions. Step.uses is the current activity-kind discriminator. |
| [src/engine.rs](../../src/engine.rs) and [migrations/](../../migrations/) | Own PostgreSQL transitions, shared coordination locking, request deduplication, claims, task/attempt leases, artifact acceptance, dependency advancement, cancellation, recovery, and journal events. |
| [src/continuation.rs](../../src/continuation.rs) | Defines normalized agent termination, AgentExecution, WorkspaceSnapshot, HandoffRecord, FallbackPolicy, AgentCandidate, and pure continuation decisions. These are reusable domain primitives, but at this baseline the engine does not invoke next_agent or continuation_recovery_action, and Run/Attempt do not persist handoff or validation collections. The proposed control plane must not mistake these pure contracts and tests for a fully wired scheduler path. |
| [src/execution.rs](../../src/execution.rs) | Defines logical workspace execution requirements, operator-selected immutable OCI profiles, validator requirements, and worker configuration. It has one qualified trusted profile, no general runtime plugin or credential-pool service. |
| [src/acp_capabilities.rs](../../src/acp_capabilities.rs) | Provides provider-neutral model capability types, image/ACP/CLI/static discovery parsers, an in-memory cache, and ExecutionResolver. Resolution fails closed, but the resolver is not currently integrated into the claim scheduler. |
| [src/agent.rs](../../src/agent.rs) | Defines immutable agent bindings, requested tools/permissions, budgets, output contracts, call reservations, receipts, and nullable usage. A binding model/runtime field is operator policy, not a provider credential. |
| [src/coding_agent.rs](../../src/coding_agent.rs) | Implements the bounded Responses coding loop. It resolves one configured model credential, reserves calls before dispatch, routes tools through Orbit, and retains unknown dispatch outcomes. |
| [src/command_agent.rs](../../src/command_agent.rs) | Implements the trusted single-call agent adapter. Its command and environment are operator provisioned; it is not a model selector or security sandbox. |
| [src/acp_runtime.rs](../../src/acp_runtime.rs) and [src/acp_process.rs](../../src/acp_process.rs) | Define pinned ACP installations, exact assignment authorization, isolated auth staging, cleanup receipts, and local auth-store locking/quarantine. The current AuthLease is a worker-local file lock, not a database-wide credential-pool lease. |
| [src/acp_broker.rs](../../src/acp_broker.rs), [src/acp_files.rs](../../src/acp_files.rs), and [src/acp_terminal.rs](../../src/acp_terminal.rs) | Enforce session-bound broker callbacks, reserve-before-effect, workspace path confinement, bounded file access, terminal supervision, and cleanup. |
| [src/codex_bridge.rs](../../src/codex_bridge.rs) and [src/codex_session.rs](../../src/codex_session.rs) | Provide the version-pinned Codex App Server bridge. Native effects are disabled; requested effects cross the Orbit broker. |
| [src/repository.rs](../../src/repository.rs) and [src/workspace.rs](../../src/workspace.rs) | Materialize private Attempt-owned Git repositories at a frozen base, reuse only a matching live Attempt workspace, extract exact binary patches/manifests, and take read-only workspace snapshots. |
| [src/worker.rs](../../src/worker.rs) | Registers a generic capability, claims an eligible task, starts a fenced Attempt, starts AgentExecution evidence when applicable, renews the Attempt lease independently, performs work, and publishes through the normal operation protocol. |
| [src/artifacts.rs](../../src/artifacts.rs), [src/run_export.rs](../../src/run_export.rs), and [src/evidence.rs](../../src/evidence.rs) | Keep immutable, checksum-verified artifact bytes outside the run document and support bounded private review/export. |
| [src/telemetry.rs](../../src/telemetry.rs) and [src/ops.rs](../../src/ops.rs) | Aggregate safe agent timing/tool/nullable usage data and expose bounded operational metrics. |
| [src/api.rs](../../src/api.rs) and [src/main.rs](../../src/main.rs) | Expose authenticated run, artifact, worker, queue, approval, signal, package, health, inspection, event, and export interfaces. There is no current agents, roles, resources, or goal command. |

### 2.3 Current state and ownership

The current lifecycle is split deliberately:

~~~text
Definition
    -> immutable Plan
        -> Run aggregate
            -> Task (durable activity/node instance)
                -> Attempt (one worker claim and lease when physical execution is required)
                    -> optional AgentExecution records
                    -> Attempt-owned workspace/environment as applicable
                    -> accepted artifacts
~~~

Run owns task state, accepted output identity, child-run links, plan identity,
and the journal sequence. Task owns deadlines, retry state, accepted outputs,
and the ordered Attempt history. Attempt owns worker/generation/token
authority, workspace identity, lease expiry, outputs, and any sequential
AgentExecution records. An AgentExecution records one provider/runtime
dispatch; it does not own the workspace or task state.

The current engine creates one current Attempt per schedulable task. Engine
steps such as joins and timers have no worker Attempt. repository.test is a
deterministic validation Task/Attempt, not an AgentExecution.

### 2.4 Current validation and agent boundaries

For repository work, repository.code produces a patch and manifest from a
pinned baseline. repository.test materializes a fresh workspace, applies only
the accepted patch, runs server-authorized commands, and publishes a bounded
test_report/log. The agent completion text or self-reported test result is not
authoritative.

An ACP coding execution is more constrained still: its image, adapter, launch
digest, model policy, auth identity, files, tools, limits, and accounting mode
are pinned by operator configuration. File and terminal effects pass through
the Orbit broker and workspace supervisor. The provider conversation is
disposable; accepted state is the Run, workspace, artifacts, and journal.

The current implementation has no first-class quota snapshot, native provider
status probe, credential pool, role policy, read-only Reviewer activity, repair
controller, or durable Goal aggregate. Those are design targets, not hidden
assumptions.

## 3. Existing Architecture to Preserve

The following are hard compatibility and safety constraints for every future
phase.

1. PostgreSQL owns accepted state. In-memory caches, provider sessions, worker
   messages, and UI state never become an alternate authority.
2. Task is Orbit's generic durable workflow activity/node instance. Attempt
   represents a physical claim, lease, and execution environment only when
   the Task requires physical execution. AgentExecution is specialized
   evidence for an agent/provider dispatch under an Attempt. A role or model
   label must not collapse these into one agent-task object.
3. Any workspace belongs to an Attempt. An AgentExecution only uses that
   Attempt's workspace when the activity has one.
4. A mutable Attempt workspace has one mutating owner at a time. Parallel
   analysis may use immutable or separately materialized inputs; parallel
   writers require separate Attempts and workspaces.
5. Provider sessions and conversations are disposable. Cross-agent continuation
   uses Orbit-owned task state, accepted artifacts, workspace state, and
   handoff evidence.
6. Validation is Orbit-controlled and independently reproducible from the
   pinned baseline, exact accepted patch, and predetermined commands/policy.
7. Credentials remain provider/runtime isolated. Only logical references may
   enter plans, assignments, safe execution evidence, or inspection.
8. Runtime images, adapter revisions, tool revisions, and execution profiles
   remain immutable and pinned. Missing or mismatched capability fails closed.
9. Missing provider usage is null/unknown. No counter, reservation, elapsed
   time, or model response shape may be presented as inferred token or cost
   usage.
10. Security policy, mounts, credentials, network, budgets, and isolation are
    controlled by Orbit/operator policy. Planner, reviewer, prior agent,
    repository content, and provider response are untrusted inputs.
11. Continuation means that an execution cannot continue and another eligible
    execution takes over the same Attempt. Verification and repair are
    different graph/control transitions.
12. Attempt ownership, generation, lease token, request deduplication,
    heartbeat fencing, cancellation, deadline, and drain semantics remain
    authoritative. Draining stops new claims; it does not revoke active
    leases by implication.
13. Storage/provider I/O occurs outside scheduler coordination locks. Authority,
    generation, cancellation, lease, request identity, and artifact metadata
    are rechecked before committing the result.
14. At-least-once execution never proves exactly-once provider or worker
    effects. An unresolved dispatch remains visible and blocks unsafe automatic
    redispatch.
15. Accepted artifact identity is the checksum/size/location plus producing
    Attempt and provenance-bearing metadata. Uploaded bytes alone never release
    a dependent Task.

The proposed control plane extends these boundaries. It does not replace the
run lock with a broker, use provider conversations as checkpoints, or allow an
LLM to become the scheduler.

## 4. Problem Statement

The current execution plane can run an isolated coding activity and
independently validate its accepted patch. It can also record provider/runtime
identity, bounded tool telemetry, execution-only ACP accounting, and normalized
termination evidence. It does not yet answer the following operational
questions before dispatch:

- Which concrete runtime, credential, model, and reasoning setting is usable
  now?
- Is an unavailable result a provider quota, a bad credential, a missing
  runtime, a capability mismatch, or merely stale information?
- Which agent activity is intended to plan, mutate, explore, review, or repair?
- How can a reviewer be technically read-only and independent?
- What durable state owns structured findings and a bounded repair iteration?
- How can a policy-controlled loop run without turning continuation into an
  unbounded retry mechanism?
- How can a coding proving ground grow into heterogeneous workflow
  orchestration without forcing deterministic validation, deployment, timers,
  or approvals into AgentExecution?

The target is an explainable control plane:

~~~text
immutable intent + role policy + current inventory
    + capabilities + availability + leases + worker capacity
                                |
                                v
                     deterministic candidate selection
                                |
                                v
                       fenced Attempt execution
~~~

## 5. Design Principles

### 5.1 Intent, activity, role, capability, and resource are different

An activity kind describes execution semantics (repository.code,
repository.test, container.run, human.approval, and so on). A role describes
the intent of an agent activity. Capabilities describe what a runtime/resource
can do. A resource identifies one concrete runtime/credential/model/reasoning
combination. Policy determines which capabilities a role may use. None of
these concepts is a credential value or a provider conversation.

### 5.2 Durable decisions must be deterministic

The first scheduler is a deterministic filter and ranker. A language model may
produce a plan or finding, but it does not select security policy, grant tools,
change mounts, change budgets, or route around a failed authorization check.

### 5.3 Unknown is a first-class evidence state

Unknown quota, unknown usage, unknown process cleanup, unknown provider
outcome, and unknown model attribution must stay unknown. The scheduler may
apply an explicit operator policy to an unknown candidate, but it must never
turn unknown into zero, unlimited, ready, or successful.

### 5.4 Independence is a technical property

Independent reviewer means a distinct review Task/Attempt and independently
selected execution resource, fresh or read-only materialization, no writer
lease, no provider conversation inheritance, and a role policy that denies
source mutation. A prompt instruction alone is not independence.

### 5.5 Bounded control flow is part of the plan

Every role execution, continuation, test/review branch, repair iteration,
status probe, fan-out, join, and wall-clock lifetime has an explicit bound.
No agent output can create a new executable graph or enlarge its own budget.

### 5.6 Preserve Orbit’s heterogeneous purpose

Agentic coding is the best-qualified workload, not the definition of Orbit.
The model must continue to represent deterministic tools, containers, API
calls, deployments, waits, schedules, event waits, and human approvals
without inventing an agent role for each one.

## 6. Domain Model

The near-term model reuses the existing aggregate:

~~~text
Definition
  -> Plan (immutable, digest-protected)
      -> Run (durable workflow aggregate)
          -> Task (one activity/node instance)
              -> Attempt (one physical claim/lease/environment)
                  -> AgentExecution (only for an agent activity)
                  -> Artifact(s) and journal evidence
~~~

Task is the generic durable activity/node instance in this model. It is not an
agent-specific task and does not require a new Activity persistence object.
Attempt represents the physical execution, lease, and environment when a Task
needs one; an engine or human transition can advance a Task without an
Attempt. AgentExecution[] is created only when the activity actually dispatches
an agent/provider execution. This keeps deterministic tools, services,
deployments, approvals, and timers in the same durable Task model without
giving them agent records.

The proposed additions are intentionally small:

| Concept | Owner | Proposed role |
| --- | --- | --- |
| ActivityKind | Conceptual classification of Step.uses | Distinguishes agent, repository, container, engine, human, and future activities. It does not require an Activity persistence object or force every kind through an agent adapter. |
| AgentRole | Agent activity policy and AgentExecution evidence | Semantic intent such as planner, implementer, tester, or reviewer; it is not a model or credential. |
| RolePolicy | Immutable plan/policy resolution | Requirements and security constraints for a role. It is compiled and authorized by Orbit. |
| ExecutionResource | Operator/worker resource inventory | A concrete runtime × logical credential × model × reasoning candidate, with immutable identity and capabilities. |
| AvailabilitySnapshot | PostgreSQL operational resource state | Time-bounded, provenance-bearing evidence about a resource or a broader provider/account scope. |
| ReviewReport/PlanReport/TesterReport | Immutable accepted artifacts produced by the corresponding Task/Attempt | Structured, schema-validated evidence. Agent prose never directly mutates Run state. |
| RepairIteration | Run-level orchestration state, initially represented by explicit bounded graph tasks | Binds a repair to its input patch, validation/review evidence, iteration budget, and resulting artifacts. |

ReviewReport, PlanReport, and TesterReport are output contracts/artifact
schemas, not replacement aggregates. The first implementation should store
their immutable bytes as accepted artifacts and retain only bounded references
and a normalized decision in the Run aggregate. A new aggregate is justified
only if recovery or joins demonstrate that the bounded references are
insufficient.

### 6.1 Activity kind versus agent role

The current Step.uses is already an activity discriminator. Keep this
separation:

~~~text
ActivityKind                 AgentRole
-------------                ---------
repository.test              (none; deterministic validator)
container.run                (none)
human.approval               (none)
engine.timer                 (none)
agent.run                    planner / researcher / ...
repository.code              implementer / repair implementer
~~~

The role field is meaningful only when the activity is an agent activity or an
agent-assisted repository activity. A validator does not become a Tester
because it executes tests. A deployment does not become an Implementer because
it changes an external system.

The same Task/Attempt rule covers heterogeneous work:

~~~text
Task: repository.code
    -> Attempt
        -> AgentExecution (Implementer)

Task: agent.run
    -> Attempt
        -> AgentExecution (Planner / Reviewer / Tester)

Task: repository.test
    -> Attempt
        -> deterministic validator execution
    -> no AgentExecution

Task: container.run
    -> Attempt
    -> no AgentExecution

Task: deploy
    -> Attempt
    -> no AgentExecution unless an agent is explicitly part of the activity

Task: browser/computer activity
    -> Attempt / execution environment as required
    -> AgentExecution only if the activity is model-driven

Task: human.approval
    -> engine/human transition
    -> no AgentExecution

Task: timer / wait / event
    -> engine transition
    -> no AgentExecution
~~~

### 6.2 AgentExecution reuse

Planner, Implementer, Tester, Reviewer, and future agent roles reuse the
existing AgentExecution lifecycle. StartExecution remains the authoritative
creation point after a Task is running; Orbit assigns the execution ID and
sequence under the run fence; runtime-confirmed identity and bounded telemetry
arrive through fenced updates; finalization is driven by the Attempt outcome.

The future additive record should carry:

~~~text
role_id
role_policy_digest
execution_resource_id
selection_decision_id
~~~

Each is optional when deserializing legacy records. role_id and the policy
digest are evidence of what Orbit authorized, not claims made by the agent.
The selected resource is recorded after deterministic resolution and before
provider effects. It is not copied into an old plan digest.

A separate PlannerExecution, ReviewerExecution, TesterExecution, ToolExecution,
or DeploymentExecution type is not required. A universal ActivityExecution
hierarchy is not a planned replacement for Task/Attempt. If a future concrete
recovery or persistence requirement demonstrates that Task/Attempt cannot
represent one specific activity correctly, that gap can be evaluated on its
own evidence; it is not a current implementation requirement. The existing
engine/worker Task/Attempt behavior remains the default representation for
non-agent work, and AgentExecution remains specialized provider-dispatch
evidence.

## 7. Execution Resource Identity

### 7.1 Durable identity

The durable identity of a concrete execution resource is the canonical tuple:

~~~text
ExecutionResourceIdentity =
    RuntimeIdentity
    × LogicalCredentialIdentity
    × OpaqueModelIdentity
    × OptionalOpaqueReasoningEffort
~~~

RuntimeIdentity must include enough immutable identity to prevent silent
rebinding:

~~~text
runtime family / adapter
runtime binding or installation identity
immutable image digest
binary/agent revision
protocol or adapter version
~~~

The logical credential identity includes only a provider/runtime-local
reference, account class or scope metadata, and an operator-defined
credential generation. It never includes secret bytes, auth files, bearer
tokens, or a developer HOME path.

Model IDs and reasoning-effort IDs remain opaque strings supplied by the
runtime/provider. Orbit may validate their syntax and capability membership,
but must not embed today’s catalog in core enums or infer a stronger model
from a display name.

The resource ID should be a domain-separated digest of canonical identity
fields, not a display label. Changing a runtime image, credential generation,
model binding, or reasoning configuration creates a new resource identity. A
worker cannot claim that a different resource is the same resource merely
because its human-readable name matches.

### 7.2 Candidate identity versus availability scope

Scheduling candidates are concrete at dispatch time. A candidate with an
unspecified model may remain a legacy/provider-configured request, but it does
not have model-specific availability and must not be presented as a
model-qualified candidate.

Availability can be observed at a broader scope than a candidate:

~~~text
exact resource
credential + model
credential/account
runtime/provider
~~~

Therefore AvailabilitySnapshot carries an explicit applies_to scope. A
credential-wide quota may block all matching model resources, while a
model-specific quota must not block an unrelated model. If the provider does
not reveal the scope, the evidence is recorded as unknown/broad and the
scheduler applies only the conservative policy configured for that evidence.

### 7.3 Runtime / credential / model / role separation

| Dimension | Meaning | What it must not mean |
| --- | --- | --- |
| Runtime | The executable adapter/installation and its immutable launch identity | A credential or a model |
| Credential | One operator-provisioned provider account/auth store reference | An agent role or permission grant |
| Model | An opaque provider/runtime model identity | A durable task or role |
| Reasoning effort | An opaque runtime setting, when the runtime exposes one | A universal quality scale or quota window |
| Role | The intent of an agent activity | A model selection or security grant |
| Capability | An ability/permission advertised by a runtime/resource | Authorization by itself; Orbit policy still decides |
| Availability | Time-bounded evidence that a resource may be usable | Runtime health, authentication, or correctness |
| Activity kind | Execution semantics for a graph node | An agent identity |

This separation allows one runtime to expose several models, one model to be
served by several credentials, and one role to use multiple compatible
resources without hard-coding the current catalog.

## 8. Runtime, Credential, Model, Role, and Capability Contracts

### 8.1 Current contracts to extend

The existing Binding already separates an operator-selected model revision,
runtime capability string, tools, permissions, budget, and optional ACP
descriptor. acp_runtime::Runtime adds a pinned launch and auth store. The
future resource registry should extend these contracts rather than putting
provider secrets or mutable installation paths into Definition.

ExecutionIntent, AgentRuntimeCapabilities, RuntimeCapabilityCache, and
ExecutionResolver are the natural capability-resolution seam. Their claim-time
integration must be added only with explicit compatibility and freshness
semantics; the current resolver’s existence is not evidence that a worker
already performs dynamic model selection.

### 8.2 Role policy

A role policy is an Orbit-controlled, versioned value containing at least:

~~~text
role_id
activity kind allowed
required / forbidden capabilities
workspace mode: none, read_only, or mutable_attempt
filesystem and terminal policy
network policy
minimum resource profile / isolation class
model and reasoning requirements
candidate pool or runtime allowlist
budget and wall-clock limits
independence requirements
accepted output schema
retry, continuation, fan-out, join, and repair bounds
~~~

“High reasoning” is a requirement interpreted by a runtime capability
descriptor or explicit ordered policy; it is not permission for the model to
choose a stronger model. If a runtime cannot prove the requested capability,
the candidate is rejected.

Role policy is resolved by the operator/server before execution. A user may
request the semantic role, but cannot request arbitrary mounts, credentials,
host commands, network, or a larger budget through role content.

### 8.3 Planner output is untrusted

Planner output must be a bounded structured report containing objective
interpretation, constraints, acceptance criteria, proposed steps, expected
validation, and risks. Orbit validates it against a schema and policy. The
planner cannot:

- add a credential or resource pool;
- add a mount, network route, tool, or permission;
- change the security profile or execution image;
- disable or redefine validation;
- increase any plan, role, provider, or wall-clock budget;
- select arbitrary host execution;
- create an unbounded loop or executable child definition.

The server may compile a planner proposal into a new immutable plan only after
policy validation and, where required, human approval. The proposal is never
executed as authority merely because it is syntactically valid.

## 9. Availability and Quota Model

### 9.1 Separate health dimensions

The scheduler must expose separate decisions for:

| Dimension | Example result | Consequence |
| --- | --- | --- |
| Runtime health | image present, launch preflight passed | May execute if all other checks pass |
| Credential health | auth store usable, not quarantined | May use this credential |
| Capability health | model/reasoning/tool requirement matches | Candidate is eligible for this role |
| Lease/concurrency | resource lease free within policy | Candidate can be claimed now |
| Provider availability | quota/rate/cooldown evidence | Candidate may be eligible, limited, or blocked |

These dimensions must not be collapsed into one healthy boolean. For example:

~~~text
runtime launchable       yes
credential authenticated yes
model advertised         yes
reasoning supported      yes
quota remaining          unknown
effective result         capability match, availability unknown
~~~

### 9.2 Snapshot shape

The future normalized snapshot should be conceptually equivalent to:

~~~text
AvailabilitySnapshot {
    snapshot_id
    resource_identity or scoped applies_to identity
    observed_at
    expires_at
    state
    quota_windows[]
    source
    confidence
    evidence_digest
    provider_observed_at?
    diagnostic_class?
}

QuotaWindow {
    provider_window_id or provider_label
    used_percent?
    remaining_percent?
    resets_at?
    exhausted?
}
~~~

All optional numerical and timestamp fields are absent/null when the provider
did not report them. exhausted=true is recorded only when explicitly reported
or strongly normalized from an execution result. It does not imply a
percentage, reset time, or weekly/rolling window.

The state vocabulary may be refined during implementation, but must represent
at least:

~~~text
READY
LIMITED
COOLDOWN
RATE_LIMITED
QUOTA_EXHAUSTED
AUTH_FAILED
RUNTIME_UNAVAILABLE
CAPABILITY_MISMATCH
UNKNOWN
~~~

AUTH_FAILED, RUNTIME_UNAVAILABLE, and CAPABILITY_MISMATCH are normally
health/eligibility outcomes rather than quota observations. Keeping them
visible in the normalized decision surface is useful, but their provenance and
remediation differ from QUOTA_EXHAUSTED.

### 9.3 Unknown is not zero

Orbit must not:

- render an absent usage field as 0%;
- infer a weekly limit from a five-hour window;
- infer a reset timestamp from a rate-limit message;
- estimate provider tokens from ACP calls, tool callbacks, elapsed time, or
  output bytes;
- treat a provider that reports only “exhausted” as reporting the missing
  window values;
- treat an unreported model as available simply because another model under
  the credential is available.

Unknown may be eligible only under an explicit role/operator policy. It is not
ranked as a healthy, fully available candidate.

### 9.4 Availability cache and effective state

Provider probes are expensive and must not run for every claim. PostgreSQL
stores the current normalized snapshot and bounded historical evidence; a
process-local cache may accelerate reads but is never authoritative.

Freshness rules:

1. A snapshot is usable until its local expires_at.
2. A stale positive snapshot is no longer authoritative as READY; effective
   availability becomes UNKNOWN until refreshed.
3. A stale negative snapshot may remain a conservative block until its
   configured expiry, but the UI must show that it is stale.
4. A provider reset timestamp is evidence about the provider window, not an
   instruction to sleep until that time unless policy explicitly schedules a
   retry.
5. A refresh request is rate-limited and itself has a bounded activity/lease.

The scheduler may use a current exact snapshot, a matching broader snapshot,
or an explicit unknown result according to policy. It must record which
snapshot IDs and freshness decisions were used for a claim.

### 9.5 Execution-result updates

A normalized execution result is immediate evidence:

~~~text
provider result: quota exhausted
    -> update exact/scoped resource availability
    -> retain provider reset only if supplied
    -> release the execution’s active resource lease transactionally
    -> allow continuation policy to consider another eligible resource
~~~

The update must be scoped to the concrete runtime/credential/model/reasoning
identity when that identity is known. If only a provider-wide error is known,
the evidence is broad/unknown rather than falsely model-specific. An execution
result cannot silently change role policy or authorize a new credential.

Strong negative evidence may override a cached READY snapshot. A later fresh,
stronger, or operator-confirmed observation may clear it. Conflicting
evidence is retained with ordering/source metadata; it is not resolved by
assuming the more optimistic value.

## 10. Availability Evidence and Freshness

### 10.1 Evidence sources

The initial source taxonomy should include:

- provider_native_status: a documented provider status/usage interface;
- runtime_native_status: a documented status operation exposed by the pinned
  runtime/adapter;
- execution_result: a normalized result from a real dispatched execution;
- operator_override: an explicitly audited operational assertion.

Every snapshot records source, source revision/adapter identity, observation
time, scope, normalized facts, and an evidence digest. Raw provider payloads,
prompts, tokens, auth files, and arbitrary terminal output are not retained in
the run journal merely to support status display.

Operator overrides can block or mark a resource limited. They must not grant a
credential, bypass capability/security policy, or claim that a provider value
was observed.

### 10.2 Freshness and confidence

Freshness is a local policy over an observation; confidence is provenance, not
a probability estimate:

~~~text
confidence:
    authoritative_native
    execution_observed
    operator_asserted
    unknown
~~~

An execution result is often stronger for the exact resource than a stale
provider status page. It is not necessarily stronger for an entire account.
The scheduler must preserve this scope distinction.

### 10.3 Retention

Retain the current effective snapshot plus a bounded history of changes and
the evidence needed to explain claims. Expired snapshots may be compacted
only after no accepted Run references them and the operational audit policy
allows it. Accepted Run evidence must remain inspectable for the normal
artifact/journal retention period.

Availability history is operational state, not immutable plan content. A claim
references the snapshot/decision IDs it used; later status refreshes do not
rewrite the historical decision.

## 11. Native Provider Status Discovery

This is an evidence-gathering phase, not a design assumption.

The current repository has ACP initialization/model discovery and provider
error normalization. It does not establish that Codex /status, Antigravity
/usage or /quota, or equivalent commands are programmatically available
through the installed ACP integrations. The credential-free ACP preflight
explicitly does not authenticate or execute a model turn. No current code
proves that a quota command is free, structured, or scoped per model.

Phase A must answer, separately for each selected runtime/version:

1. Is there a documented/native status mechanism?
2. Can it be invoked programmatically from Orbit’s isolated runtime?
3. Does it avoid a model inference turn and brokered repository effect?
4. What structured fields and window semantics are actually returned?
5. Is the result per credential, account, model, runtime, or global?
6. Which reset values are provider evidence?
7. What authentication context is needed?
8. Can the probe run safely without exposing credentials or workspace data?
9. Does the operation itself consume quota, rate limit, or a billable request?

The discovery order is:

~~~text
documented native status
    -> documented runtime/adapter status
        -> bounded CLI adapter with an explicit fragility record
            -> no automatic probe if only reverse-engineered access exists
~~~

Scraping unstable terminal text is not a core contract. If a CLI parser is
temporarily unavoidable, it is an adapter with a version pin, parser tests,
bounded output, source digest, and an UNKNOWN fallback on any ambiguity.

The proof that a probe does not consume a model turn requires provider/runtime
evidence, not inference from a command name. A qualification experiment should
use a disposable credential/account, compare provider-side or native usage
before/after, capture the exact request class, and run the same operation
against a no-network fixture. It must be explicitly authorized and must not be
performed by this documentation task.

## 12. Credential Pools

### 12.1 Pool model

The operator should be able to define a pool of compatible logical resources:

~~~text
pool: <operator-defined role/model pool>
    resource A: runtime R, credential C1, model M, effort E
    resource B: runtime R, credential C2, model M, effort E
    resource C: runtime R, credential C3, model M, effort E
~~~

Provider-specific credential names are configuration examples, not product
architecture. A credential is not an agent. Pool entries contain no secret
value and are not sourced from repository/workflow content.

Each entry should expose:

- immutable runtime identity and capability descriptor;
- logical credential identity and allowed scope;
- model/reasoning candidate;
- availability snapshot references;
- maximum concurrent leases;
- worker/host placement and capacity constraints;
- last-use/health metadata for deterministic tie-breaking;
- quarantine or operator-disabled state.

### 12.2 Interaction with existing credential handling

The current worker resolves repository/model credentials from private
configuration. ACP stages only explicitly mapped auth files into a fresh
control HOME, locks the canonical auth store with flock, writes an active
marker, and quarantines uncertain cleanup. This behavior remains the final
credential boundary.

The future server-side pool lease is a logical resource lease, not a secret
lease. It prevents two workers from intentionally selecting one account beyond
its configured concurrency. The worker still validates the local credential,
scope, runtime, and auth-store lock before provider I/O.

Resource lease lifecycle:

~~~text
atomic claim transaction
    -> Attempt lease + resource lease
        -> provider/runtime work outside DB locks
            -> terminal completion/failure releases lease
            -> authority loss expires logical lease
            -> uncertain auth cleanup keeps local quarantine
~~~

An expired database resource lease must not automatically clear an ACP
.orbit-acp-active.json marker or reuse an auth store. Local cleanup evidence
and operator recovery remain separate from database fencing.

### 12.3 Pool scheduling

Pool selection is deterministic and policy-constrained. It may use stable
priority, fresh READY evidence, least active leases, and a stable resource ID
tie-breaker. It must not optimize on fabricated cost or inferred quota.
Provider concurrency permission is distinct from remaining quota.

When all compatible entries are known exhausted, the Task enters a bounded
retry/cooldown or NEEDS_INTERVENTION according to policy. When all entries are
UNKNOWN, the result is an explicit unknown-availability decision, not zero
remaining and not an implicit provider retry loop.

## 13. Execution Roles

Roles are requirements and evidence labels, not permanent model assignments.

### Planner

The Planner turns a human task into a bounded structured proposal. It may
inspect a read-only repository view and relevant accepted artifacts, subject
to role policy. It may not mutate source, grant permissions, change policy,
select arbitrary execution, or create an executable child definition.

Its accepted output is a bounded plan report. Orbit validates and compiles
any resulting plan; the report itself is not a state transition.

### Implementer

The Implementer is the current coding agent role. It is the only role in the
initial loop that may mutate the source workspace. It uses the existing
Attempt-owned Git workspace, single-writer rule, tool allowlists, broker,
runtime image, credentials, patch extraction, and manifest provenance.

A continuation from an interrupted Implementer to another compatible
Implementer remains within the same Attempt and workspace. A repair after a
completed validation/review stage is a role transition and normally receives a
new repair Task/Attempt materialized from the accepted patch and evidence.

### Tester

The Tester is a model-driven, exploratory or adversarial agent. It can inspect
the implementation, devise edge cases, and run authorized checks. It is not
the deterministic Validator and cannot declare the Run correct.

The initial policy should grant no source mutation. A Tester that needs build
or test scratch space receives an isolated read-only source materialization
plus ephemeral output space; the source mount and broker deny writes. Its
structured findings are an accepted artifact owned by its Task/Attempt.

Parallel Testers are safe only when they receive independent read-only inputs
or isolated workspaces. They must converge through a durable join; none may
race to mutate the implementation.

### Reviewer

The Reviewer performs independent semantic/architectural review. It examines
the original task, accepted patch/manifest, deterministic validation report,
Tester findings, and relevant repository state. It checks task satisfaction,
correctness, security/concurrency regressions, maintainability, assumptions,
and missing tests.

The initial Reviewer policy is technically read-only, independently selected,
and independently credentialed where provider policy requires it. The Reviewer
cannot silently repair. It emits a schema-validated decision and bounded
findings such as:

~~~text
decision: approved | changes_requested
finding:
    severity
    category
    location?
    summary
    evidence
    suggested_direction?
~~~

Orbit interprets the structured decision. Reviewer prose does not mutate task
state directly.

## 14. Validator vs Tester vs Reviewer

| Participant | Execution semantics | Authority |
| --- | --- | --- |
| Validator | Deterministic, Orbit-controlled commands against the pinned baseline plus exact accepted patch | Authoritative for configured checks and report/exit/cleanup evidence |
| Tester | Model-driven exploratory/adversarial analysis, normally read-only | Produces findings; cannot replace validation or approve correctness |
| Reviewer | Model-driven semantic/architectural review, read-only and independent | Produces structured recommendation/findings; Orbit policy decides the next transition |

Today the Validator is the repository.test Task and its fresh workspace.
Future deterministic validators should continue to use Task/Attempt and
validation artifacts, not AgentExecution, unless an explicitly agent-driven
test is being run as a separate Tester activity.

An agent saying “tests pass” is always non-authoritative. A Reviewer may point
out that configured validation is insufficient, but it cannot redefine the
validation contract during the same Run.

## 15. Role Policy

### 15.1 Policy resolution

At plan compilation, Orbit resolves each requested agent role to a versioned
role policy. The policy digest and requirements that affect authorization
belong to the immutable plan or an immutable policy reference. Mutable
operator configuration must not silently change the meaning of an accepted
Run.

The selected concrete resource and current availability snapshot do not belong
in the plan digest: they are dispatch-time evidence. The plan does contain the
allowed candidate pool, capability/security requirements, and budget ceiling
needed to make dispatch reproducible and explainable.

### 15.2 Example requirements

~~~text
reviewer:
    activity: agent.run
    source_access: read_only
    terminal: allowed through isolated supervisor
    network: none unless separately approved
    required_capabilities: [agent.reason, repository.read]
    forbidden_capabilities: [repository.write, deployment.*]
    minimum_reasoning: high (runtime capability interpretation)
    independent_from: implementation resource and Attempt writer
    output: reviewer-report/v1
~~~

This is an illustrative policy shape, not a schema commitment. The important
rule is that a role specifies requirements. It does not permanently name a
provider model, account, image path, or secret.

### 15.3 Technical read-only enforcement

A read-only Reviewer is enforced by all applicable layers:

1. Orbit assigns a separate Reviewer Task/Attempt and no mutable writer lease.
2. The environment materializes the accepted patch from a pinned baseline in
   a fresh directory or mounts it read-only.
3. The broker omits write_file/write permissions and rejects write calls.
4. The terminal supervisor mounts source read-only and gives any temporary
   output a separate ephemeral path.
5. The worker/runtime rejects a Reviewer binding that requests write tools or
   a mutable repository execution profile.
6. Accepted outputs are limited to report/findings artifacts, never a source
   patch from the Reviewer.

The prompt repeats these constraints for clarity but is not an enforcement
boundary.

## 16. Deterministic Candidate Scheduler

### 16.1 Inputs

The scheduler resolves a candidate from:

~~~text
role/activity requirements
    + immutable RolePolicy
    + RuntimeCapabilities / ModelCapabilities
    + operator resource inventory
    + credential pool scope
    + AvailabilitySnapshot(s)
    + active resource/Attempt leases
    + worker capacity and placement
    + governance scope
    + continuation/repair constraints
    + deadlines and budgets
~~~

The current /worker/claim path filters by a generic capability and worker
capacity. The future path may add a resource inventory/selection phase while
preserving the existing claim and fencing protocol. A worker must not
self-select an unadvertised model or bypass the server’s selected resource.

### 16.2 Selection algorithm

The first implementation should:

1. Load the immutable role/activity requirements and current Run state.
2. Enumerate operator-authorized resource candidates and worker capacity.
3. Reject candidates that fail scope, runtime image, adapter, capability,
   model, reasoning, tool, workspace, network, or security policy.
4. Resolve requested model/reasoning through ExecutionResolver or its successor;
   never silently downgrade or combine unsupported settings.
5. Evaluate exact and scoped availability snapshots using freshness rules.
6. Reject active lease conflicts, drained workers, exhausted concurrency,
   insufficient capacity, expired deadlines, and unavailable credentials.
7. Rank remaining candidates using explicit policy priority, availability
   class, active lease count, and stable resource ID.
8. Atomically create the Attempt and logical resource lease under the existing
   database coordination boundary.
9. Record the selected candidate, evidence IDs, and bounded rejection reasons.
10. After any external probe or runtime I/O, reacquire/recheck authority before
    accepting the result.

Candidate selection must happen before provider/model effects. A selected
candidate is not proof that the provider accepted a request; normal
AgentExecution and call-receipt semantics still apply.

### 16.3 Candidate rejection

Rejection reasons are typed and bounded:

~~~text
policy_denied
scope_denied
runtime_missing_or_unpinned
capability_mismatch
model_unsupported
reasoning_unsupported
credential_unavailable
availability_negative
availability_stale_or_unknown
resource_lease_busy
worker_draining
worker_capacity
deadline_or_budget
~~~

A rejection is an explanation, not an invitation for an agent to change policy.
The scheduler must not hold the coordination lock while probing a provider,
launching a runtime, reading artifacts, or resolving secret material.

### 16.4 All candidates unavailable

If every candidate is QUOTA_EXHAUSTED, COOLDOWN, or RATE_LIMITED, the Task may
use a provider-supplied reset time for bounded scheduling. Without a reset, it
enters NEEDS_INTERVENTION or a finite policy-defined backoff; it does not
invent a reset.

If every candidate is UNKNOWN, the result is UNKNOWN_AVAILABILITY. A role
policy may permit one controlled attempt with that explicit risk, or may fail
closed into intervention. The scheduler never treats the candidates as
ready-with-zero-usage.

## 17. Scheduler Explainability

An operator must be able to answer “why this candidate?” and “why not that
candidate?” without seeing secrets or raw provider payloads.

The durable selection evidence should contain:

~~~text
decision_id
role/activity
selected resource identity (logical fields only)
policy digest/version
capability evidence IDs
availability snapshot IDs and freshness
resource lease/worker capacity result
ordered bounded rejection reasons
decision timestamp and scheduler version
~~~

The current CLI pattern suggests a small read-only extension:

~~~text
orbit agents status [--json] [--refresh]
orbit roles list
orbit roles explain <role> [--run RUN_ID] [--task TASK_ID] [--json]
orbit inspect RUN_ID
orbit events RUN_ID
~~~

Exact names remain subject to current CLI conventions. roles explain should
accept a Run/Task context when available; a role-only explanation can show
policy requirements and configured candidates without claiming a live
selection. agents status must distinguish runtime health, credential health,
capability, availability, freshness, and unknown fields. A refresh operation
is explicit, authorized, rate-limited, and must report whether it performed a
status probe or only refreshed local configuration.

Machine-readable output is required for automation. No status output contains
credential values, auth paths, lease tokens, raw prompts, raw tool arguments,
or unconstrained provider diagnostics.

## 18. Continuation Interaction

Continuation and role transition are separate:

~~~text
CONTINUATION (same logical activity/Task)

Attempt A / mutable workspace
    -> AgentExecution #1: Implementer on resource A
    -> eligible interruption
    -> WorkspaceSnapshot + HandoffRecord
    -> AgentExecution #2: Implementer on resource B
    -> same Attempt, same workspace, new credential lease

ROLE TRANSITION / REPAIR (new Task/Attempt)

Implementer Task
    -> accepted patch
    -> Validator Task
    -> Reviewer/Tester Task(s)
    -> Repair Task / new Attempt
    -> new Implementer AgentExecution
~~~

Continuation may change runtime, credential, model, or reasoning effort only
after deterministic candidate selection and policy checks. It must not create
a second mutable writer in the same Attempt. The existing workspace snapshot,
diff digest, handoff schema, normalized termination, and fencing primitives are
the intended foundation, but their durable engine integration must be designed
and qualified separately from the repair loop.

Validation failure is not automatically continuation. If an Implementer
completed and a Validator failed, the control plane must decide whether the
configured repair policy authorizes a new repair activity. It must not
overload the continuation sequence or silently retry the provider conversation.

## 19. Review and Repair State Machine

### 19.1 Initial bounded loop

The minimal sequential control flow is:

~~~text
PLAN (optional)
  |
  v
IMPLEMENT
  |
  v
VALIDATE
  | pass
  v
TEST / REVIEW (optional, possibly fan-out read-only)
  | approved
  v
DONE

VALIDATE failure or REVIEW changes_requested
  |
  v
REPAIR (if authorized and within bounds)
  |
  v
VALIDATE again
~~~

The first repair implementation should prefer a statically bounded set of
predeclared graph activities or an explicit bounded controller. It should not
introduce a general workflow language solely to express this loop.

### 19.2 Ownership of state

- The implementation Task/Attempt owns its mutable workspace and candidate
  patch artifacts.
- Each Validator Task/Attempt owns its fresh materialization and validation
  report.
- Each Tester/Reviewer Task/Attempt owns its report/findings artifacts.
- The Run aggregate owns accepted output references, normalized decisions,
  transition/budget state, and the durable journal.
- A future RepairIteration record (or an explicit graph task pair in the first
  version) owns the relationship between an input patch, validator/reviewer
  evidence, iteration number, policy budget, and repair result.

Agent output is not allowed to write Run.state, Task.state, accepted outputs,
or repair counters directly.

### 19.3 Bounded budgets

At minimum, the loop policy needs explicit bounds for:

~~~text
max_planning_executions
max_implementation_executions
max_continuations
max_repairs
max_review_cycles
max_tester_cycles
max_total_executions
max_wall_time
provider/resource call budgets
~~~

The engine records consumed and remaining budget by durable iteration identity.
When a bound is exhausted, the result is NEEDS_INTERVENTION (or a declared
terminal failure) with the final evidence references. There is no
while-findings-run-agent behavior.

### 19.4 Idempotent iteration recovery

Every iteration has a stable identity derived from the Run, target activity,
and iteration number. Creation of a repair/review branch, join, or next
iteration is committed under the existing run fence with its journal event.
A reconciler seeing an existing completed iteration must reuse it, not create a
duplicate. A pending provider call retains normal unresolved-dispatch
intervention semantics.

### 19.5 Review decisions

The engine accepts a Reviewer result only after:

1. the report artifact is checksum/provenance verified;
2. the report conforms to the role schema and size bounds;
3. its Task/Attempt/resource identity is authorized;
4. the required Validator result and input artifact identities match;
5. the reviewer was not the mutable writer for the reviewed Attempt;
6. policy determines whether approved is sufficient or human approval is still
   required.

changes_requested creates a repair transition only when policy allows it.
Reviewer suggestions are evidence, not executable commands.

## 20. Goal / Workflow Aggregate Decision

### Decision: do not add a separate durable Goal aggregate in the first phases

The current Run already is the durable workflow aggregate: it owns one
immutable plan, task graph, child-run relationships, attempts, artifacts,
state transitions, and journal. A first-class Goal added above it would
duplicate objective, budget, task, and terminal state before the repository
contains a proven need for multiple plan generations under one objective.

For the first control-plane loop:

~~~text
human objective -> immutable Definition/Plan -> one Run -> role/activity Tasks
~~~

Planner output can be an accepted artifact or a human-approved new plan
submission. Existing child runs remain the mechanism for bounded nested
execution. An operator-supplied parent_run_id remains a history/recovery
reference unless the existing engine creates a managed child.

### Future threshold for a Goal

A thin Goal aggregate becomes justified if a human objective must durably
span multiple immutable Runs/plan revisions, for example:

- an approved plan is superseded by a repair plan while preserving both;
- a human approval/commit gate spans several Runs;
- budgets and intervention state must cover multiple independent Run trees;
- the operator needs one stable objective identity across re-planning.

If introduced, Goal must own only objective-level policy, budget, approval,
and ordered Run references. It must not own Task/Attempt state, leases,
artifacts, provider sessions, or a second journal. It submits/links Runs
through the existing engine boundary and inherits scope/security policy.

### 20.1 Future execution topology

The current static graph can evolve if the control plane keeps these semantics
distinct:

~~~text
dependency      one node waits for required predecessors
branch          one outcome selects a bounded path
fan-out         one accepted input starts bounded independent nodes
fan-in          a durable join aggregates completed branches
loop            an explicit bounded iteration with durable identity
continuation    another execution takes over one Attempt
repair          a new role/activity consumes prior evidence
~~~

Current fan-out/child runs already provide a bounded basis for independent
read-only analysis. Parallel Implementers must use separate Attempts and
workspaces:

~~~text
Task A -> Attempt A -> Workspace A -> Patch A
Task B -> Attempt B -> Workspace B -> Patch B
                         \          /
                          Integration
                              |
                         independent validation
~~~

An integration activity owns conflict detection and combined artifact
provenance. Multiple writers must never converge by racing on one mutable
workspace.

The architecture should not prematurely add a generic workflow language or a
second universal activity hierarchy. Task is already the common durable
activity/node record. Use Attempt only when that Task needs physical execution,
and add AgentExecution[] only when that physical execution dispatches an
agent/provider:

~~~text
Workflow/Run
    |
    +-- Task: coding activity
    +-- Task: deterministic tool activity
    +-- Task: container activity
    +-- Task: API/service activity
    +-- Task: deployment activity
    +-- Task: browser/computer-use activity
    +-- Task: human approval
    +-- Task: timer/wait/event
    +-- Task: agent reasoning activity
~~~

Only activities that actually dispatch an agent create AgentExecution evidence.
Do not force a validator, deployment, HTTP call, approval, timer, or browser
activity to own a Git Workspace. An execution environment can evolve
independently from AgentExecution:

~~~text
ExecutionEnvironment
    +-- GitWorkspace
    +-- ContainerEnvironment
    +-- BrowserEnvironment
    +-- ComputerEnvironment
    +-- DataEnvironment
~~~

These are future conceptual environment shapes, not new aggregates or current
implementation requirements. The current proven Git Attempt model remains the
first implementation.

Therefore the answer to the architectural test is yes: the model can evolve
from today’s sequential graph into durable branching, read-only fan-out,
bounded loops, and eventually isolated implementation branches, provided that
role is agent-specific, iteration is explicit, resource leases are separate,
and integrations produce immutable artifacts before fan-in. The minimal design
change required now is to preserve those distinctions; no universal
ActivityExecution or new Activity aggregate is implied, and generic graph
execution is not implemented here.

## 21. Self-Development Trust Boundary

The self-development path must have a stronger trust boundary than an
ordinary coding task:

~~~text
Human / authorized approver
        |
        v
Orbit API + immutable plan/policy
        |
        v
PostgreSQL Run aggregate + journal
        |
        v
Dedicated trusted worker
        |
        +--> isolated Attempt at immutable Orbit base SHA
        |       |
        |       +--> Implementer / brokered tools
        |       +--> exact patch + manifest
        |
        +--> fresh independent Validator workspace
        +--> independent Tester/Reviewer resource
        |
        v
accepted evidence + human review
        |
        v
human-controlled commit / merge / release boundary
~~~

Required invariants:

- The starting Git SHA is immutable and recorded in the plan and manifest.
- The Attempt repository is isolated; the operator/developer checkout is
  never the mutable workspace.
- The exact accepted patch identity is checksum- and provenance-verified.
- Independent validation reconstructs from the pinned baseline plus exact patch.
- Reviewers do not share the Implementer’s mutable workspace or conversation.
- Runtime images, adapters, tools, credentials, mounts, network, and budgets
  are operator-pinned.
- Agents cannot modify the worker/server configuration that grants those
  authorities.
- An agent modifying Orbit source does not gain authority over the currently
  running Orbit process, database, artifact store, deployment socket, or
  operator checkout.
- No agent can commit, push, merge, publish an image, change deployment
  configuration, or release Orbit without the explicit boundary policy.
- Evidence is durable and reviewable, but raw prompts, secrets, and arbitrary
  terminal output are not retained merely for convenience.

The bootstrap trust boundary is explicit: the first version of Orbit that
builds or changes Orbit must be launched from a human-selected immutable
baseline and independently verified outside the live control-plane process.
The candidate may produce a patch; only a human-controlled integration/release
process may make it the next control-plane version.

“Orbit can orchestrate development of Orbit” therefore does not mean that the
currently running Orbit instance may rewrite or redeploy itself. The initial
boundary remains objective -> bounded Run -> planning/implementation ->
deterministic validation -> independent test/review -> bounded repair ->
accepted evidence -> human-controlled commit/merge/release.

## 22. Human Approval Boundaries

The initial policy should allow autonomous execution only within a submitted,
immutable, bounded plan:

| Operation | Initial policy |
| --- | --- |
| Read task/plan and inspect isolated inputs | Autonomous within scope |
| Planner proposal | Autonomous proposal; Orbit validation required |
| Implement isolated patch | Autonomous when policy authorizes the role |
| Deterministic validation | Autonomous and Orbit-controlled |
| Tester/Reviewer analysis | Autonomous recommendation within read-only policy |
| Accept a patch as Run evidence | Orbit-controlled after artifact/validation checks |
| Commit to a repository branch | Explicit human approval initially |
| Push to a remote | Explicit human approval initially |
| Merge to a protected branch | Explicit human approval initially |
| Change runtime/security policy, mounts, network, credentials, or tool allowlist | Explicit operator approval and a new immutable plan/configuration |
| Increase role/provider/run budgets | Explicit operator approval; no in-place mutation |
| Publish an image or package | Explicit human approval |
| Deploy Orbit or change production configuration | Explicit human approval and separate deployment policy |

The existing human.approval activity and authenticated approval boundary are
the starting mechanism. A Reviewer is not automatically an approver. Future
automation may narrow approval requirements after independently qualified
controls exist, but it must not remove the initial high-impact boundary by
default.

## 23. Security Model

### 23.1 Policy authority

Only Orbit/operator configuration controls:

- resource pool membership;
- capability allowlists;
- role policies;
- execution profile/image;
- filesystem/network/terminal permissions;
- credential references and scopes;
- budgets and concurrency;
- validator definitions;
- human approval requirements.

Repository content, planner output, reviewer findings, model responses,
provider status text, and previous agent instructions are untrusted data.

### 23.2 Enforcement

Future role scheduling must reuse:

- server authorization and governance scope checks;
- immutable plan digests and legacy digest preservation;
- worker registration/admission and server-owned capacity;
- Attempt generation/token/lease fencing and heartbeat;
- rootless OCI profiles, no-new-privileges, dropped capabilities, and
  workspace-only mounts;
- ACP session binding, exact model confirmation, broker reservations,
  openat2 path confinement, terminal supervisors, and auth quarantine;
- private repository materialization and no operator checkout mutation;
- artifact checksum/size/provenance verification;
- independent validation and human approval.

Read-only access is enforced by environment and broker configuration, not by a
prompt. Provider credentials are not mounted into deterministic tool
containers. A status probe cannot acquire repository write access merely
because it uses the same runtime family.

### 23.3 Network and external effects

Provider network access, if required, belongs to the trusted agent runtime
policy and must not be confused with tool/repository network access. The
current ACP design explicitly warns that host networking is not provider-only
egress. Repository tools remain networkless unless a separately qualified
policy says otherwise.

No role may infer exactly-once effects from an HTTP success, process exit,
cleanup attempt, or provider response. Unknown external outcomes retain
existing intervention/fencing semantics.

## 24. Persistence

### 24.1 Existing authoritative persistence

The current system stores each Run aggregate and ordered journal in PostgreSQL
and stores artifact metadata in the Run with immutable bytes in an artifact
provider. Request IDs deduplicate accepted API/worker operations. The shared
control row serializes admission and cross-Run mutations. These remain the
authority for accepted workflow state.

AgentExecution currently persists within its Attempt, including requested,
resolved, and actual model evidence, runtime image/digest, capability source,
nullable usage, tool counters, and a safe logical credential reference.

### 24.2 Proposed new durable state

Availability and resource leases are cross-Run operational state and should not
be copied into every Run document. A future additive migration should provide
conceptual equivalents of:

~~~text
orbit_execution_resources
    immutable resource identity, operator pool, capabilities, worker scope

orbit_resource_leases
    resource, attempt/generation, worker, lease expiry, state, request identity

orbit_availability_snapshots
    scoped resource evidence, freshness, state, quota windows, source, digest

orbit_availability_events
    bounded normalized changes and operator/audit metadata
~~~

The exact table layout is implementation work. PostgreSQL must own current
resource lease state and the effective availability pointer; provider probes
and runtime launch happen outside the coordination lock.

The Run aggregate may receive additive optional fields for role policy,
resource selection evidence, review/repair references, and normalized
decisions. High-volume raw evidence remains an artifact or bounded separate
record, not an unbounded JSONB append.

### 24.3 Digest and compatibility impact

Role policy requirements that affect authorization must be included in a new
plan version or immutable policy digest. Selected runtime/credential/model
availability is dispatch-time evidence and must not be added to the immutable
plan digest.

Existing plans and legacy orbit/v0 serialized digests must remain valid. New
optional fields must be omitted for old shapes where the current compatibility
contract requires omission. Any change to serialized plan input, binding,
execution profile, or schema ordering requires a legacy digest test and an
explicit protocol/version decision.

No migration, schema change, or plan-digest change is made by this document.

### 24.4 Crash recovery

Resource selection, Attempt creation, resource lease creation, and the
selection journal record commit atomically. A lost response is recovered by
the existing request identity. A worker or server crash leaves the durable
lease/generation decision to reconciliation; a new resource claim cannot
adopt the old Attempt’s workspace or provider outcome without the normal
fence.

Availability updates are idempotent by evidence identity and monotonic
observation ordering. A duplicate status result cannot create a second
resource lease or rewrite a completed review/repair iteration.

## 25. Observability

The future system extends existing AgentExecution telemetry,
AttemptObservabilityAggregate, safe operational metrics, run journal, and
private run export.

Inspectability should include:

- Run/Task/Attempt outcome and role transitions;
- selected resource identity and scheduler version;
- bounded rejected-candidate reasons;
- availability snapshot IDs, source, scope, freshness, and unknown fields;
- quota evidence and explicit reset values;
- Planner, Implementer, Tester, Reviewer execution records;
- continuation sequence and handoff identity;
- validator reports and failure fingerprints;
- review findings/decision references;
- repair iteration number and consumed budgets;
- nullable provider usage and whether aggregate usage is partial;
- final accepted patch/manifest/validation/review artifacts.

Durable logs and metrics remain bounded and sanitized. Do not add labels or
fields containing run IDs at unbounded cardinality, prompts, credentials,
auth paths, raw commands, raw provider payloads, or arbitrary terminal output.
An availability status table may show logical credential/resource identifiers
only where the operator is authorized to see them.

The current agent input/output token metrics continue to mean reported values,
not estimates. Null/unknown usage must be represented as unknown in JSON,
inspection, and CLI output.

## 26. CLI / API Requirements

### 26.1 Smallest coherent extension

The current CLI already has health, workers, queues, inspect, events, artifact,
and export-run. The smallest extension is:

~~~text
orbit agents status [--json] [--refresh]
orbit roles list
orbit roles explain <role> [--run RUN_ID] [--task TASK_ID] [--json]
~~~

credentials list/status and models list/capabilities may be views over the
same resource/role APIs rather than separate sources of truth. Avoid a large
command family until the resource model is proven.

inspect should gain optional role/resource/availability/iteration sections and
events should expose normalized decision events already authorized by the
caller. A future orbit goal run should be an intent/submission client over the
existing engine, not a second execution API.

### 26.2 API boundary

Potential read APIs:

~~~text
GET  /agents/status
GET  /roles
GET  /roles/{role}/explain
GET  /runs/{run}/decisions
~~~

An explicit refresh API, if needed, must be authenticated, audited, bounded,
rate-limited, and represented as a controlled status-probe activity. It must
not grant an agent access to provider credentials or bypass the engine.

All mutations continue through the existing authorization/engine boundary.
No UI, MCP tool, planner, or reviewer may mutate a resource lease, accepted
artifact, role policy, or Run state by writing directly to PostgreSQL.

## 27. Failure Semantics

| Failure/evidence | Required result |
| --- | --- |
| Runtime image or adapter missing | Candidate rejected; no provider effect; report runtime-unavailable |
| Model/reasoning capability mismatch | Candidate rejected; no downgrade; fail closed |
| Credential missing/invalid/quarantined | Candidate rejected or Task intervention; do not borrow another provider credential implicitly |
| Fresh known quota exhausted | Candidate unavailable; update scoped snapshot; consider another pool resource |
| Rate limit/cooldown with reset | Candidate unavailable until policy permits; retain provider reset only when reported |
| Rate limit/cooldown without reset | Bounded retry or intervention; no invented reset |
| Status probe returns incomplete data | Record partial/unknown fields; do not synthesize zeros |
| Snapshot stale | Do not use stale READY as authoritative; refresh or apply explicit unknown policy |
| All candidates known unavailable | Bounded wait/intervention; no infinite retry |
| All candidates unknown | Explicit unknown-availability decision; policy may fail closed or allow one marked attempt |
| Worker/resource lease race | Existing PostgreSQL serialization chooses one owner; loser gets no work/ownership loss |
| Worker lease expires during provider work | Attempt loses authority; external effect remains unknown; no late accepted result |
| Prompt dispatch outcome unresolved | Preserve pending reservation and intervention semantics; no automatic redispatch |
| Implementer continuation eligible | New sequential AgentExecution in the same Attempt/workspace, after handoff/drift checks |
| Validator fails | Orbit records deterministic report; repair only through explicit bounded policy |
| Tester finds a defect | Finding artifact; no direct source mutation or success transition |
| Reviewer requests changes | Repair transition if authorized and budget remains; otherwise intervention |
| Reviewer unavailable | Apply explicit join policy (required, optional, or bounded minimum); never silently approve |
| Repair budget exhausted | NEEDS_INTERVENTION or declared terminal failure with evidence |
| Human denies commit/release approval | No commit/release/deploy; preserve prior accepted evidence |

## 28. Incremental Delivery Plan

Delivery must be dependency-aware and must not attempt the entire vision in one
change.

### Phase A — Native availability discovery spike

After the Q7 baseline/qualification checkpoint, experimentally determine
Codex and Antigravity status semantics, structured fields, scope, cost, auth,
and no-inference-turn behavior. Use credential-free fixtures first and obtain
separate authority for any live account probe. Produce an adapter evidence
record, not a scheduler feature.

### Phase B — Availability model

Add normalized resource identity, scoped snapshots, arbitrary quota windows,
freshness, evidence provenance, unknown semantics, and execution-result
updates. Persist cross-Run operational state with an additive migration. Keep
probes and storage/provider I/O outside coordination locks.

### Phase C — CLI status

Expose human-readable and JSON status, explicit refresh, health dimensions,
unknown fields, snapshot age, and evidence source. Add tests for redaction,
freshness, provider-window variability, and no fake zero values.

### Phase D — Credential/resource pools

Register operator-authorized resource inventory, add durable logical resource
leases and concurrency limits, integrate worker capacity/draining, and
preserve local ACP auth locks/quarantine. Prove that two workers cannot
intentionally oversubscribe one pool entry and that lost cleanup does not
silently reuse an auth store.

### Phase E — Execution roles and deterministic scheduler

Add role policy resolution, role evidence on AgentExecution, candidate
filtering/ranking, selection explanations, and additive protocol fields. Start
with Implementer/continuation compatibility and a read-only Reviewer-compatible
capability profile. Preserve legacy agent.run and repository plans without role
fields.

### Phase F — Reviewer qualification

Implement a read-only Reviewer activity with an independent resource,
structured findings, fresh/read-only materialization, and technical write
denial. Qualify reviewer independence, artifact provenance, failure recovery,
unknown provider outcome handling, and human approval behavior.

### Phase G — Bounded repair loop

Add explicit Validator → Review → Repair → Revalidate state transitions,
iteration identity, accepted evidence joins, and bounded repair/review/
wall-clock/provider budgets. Begin with statically bounded graph expansion or
one explicit controller; do not add an unbounded general loop language.

### Phase H — Planner

Add a structured, policy-constrained Planner. Compile its proposal only
through Orbit validation and immutable plan creation. Prove that planner
output cannot alter security policy, resources, credentials, mounts, network,
budgets, or validator definitions.

### Phase I — Goal/self-development orchestration

Only after the earlier phases qualify should Orbit coordinate the complete
goal-to-reviewed-patch loop and Orbit-on-Orbit proving tasks. Add a thin Goal
aggregate only if multiple immutable Runs/plan revisions demonstrate the need.
Add a human-controlled commit/release integration step.

Dependencies are strict: resource identity precedes availability; availability
and leases precede scheduler selection; role policy precedes reviewer/repair;
independent review precedes self-development; bounded loops and approval
precede autonomous Orbit changes.

## 29. Migration / Compatibility

The first implementation must be additive:

- preserve orbit/v0 and existing orbit/v1 definitions and digests;
- keep absent role/selection/availability fields absent or defaulted for old
  serialized records;
- use a new protocol/version or explicit feature negotiation for resource
  inventory and role-selection operations;
- require compatible server/worker upgrades before sharing new resource lease
  semantics;
- never reinterpret an old agent.run binding as a role-policy grant;
- retain old model/token/cost accounting rules and ACP execution-only nulls;
- keep current worker claim/heartbeat/fencing semantics while adding
  resource-specific checks;
- migrate availability/resource tables independently of Run document history;
- test legacy digest serialization before and after every plan schema change.

No in-place rewrite of accepted plan digests, artifacts, provider sessions,
workspace identities, or historical execution outcomes is allowed.

## 30. Testing Strategy

### 30.1 Pure domain tests

Test:

- canonical resource identity and collision resistance;
- runtime/credential/model/reasoning separation;
- arbitrary quota windows and absent fields;
- UNKNOWN versus zero/exhausted behavior;
- snapshot scope matching and freshness;
- evidence precedence/conflict retention;
- deterministic candidate ordering and complete rejection explanations;
- capability mismatch fail-closed behavior;
- role-policy schema and planner-output rejection;
- continuation versus repair classification;
- bounded iteration counters and idempotent recovery.

Property tests should assert that changing candidate input order does not
change a deterministic selected resource when all policy inputs are equal.

### 30.2 PostgreSQL/concurrency tests

Use disposable PostgreSQL to cover:

- concurrent claims for one resource pool entry;
- resource lease plus Attempt claim atomicity;
- lease expiry, heartbeat, generation fencing, cancellation, and drain;
- status update racing with claim;
- provider/storage I/O outside the coordination lock and authority recheck;
- request deduplication and conflicting refresh/selection IDs;
- server restart and reconciliation without duplicate iterations;
- retention/reference behavior for snapshots and accepted evidence.

### 30.3 Runtime/security tests

Use offline pinned fixtures before live providers:

- status probe does not dispatch an inference turn;
- provider/runtime capability discovery has bounded output;
- exact model and reasoning confirmation remains required;
- Reviewer write calls, mutable mounts, unauthorized terminals, network,
  host paths, credentials, and policy changes are denied;
- Tester scratch output cannot mutate the source;
- ACP auth locks/quarantine and resource leases do not race into reuse;
- independent validation uses a fresh baseline-plus-patch workspace;
- unresolved prompt/cleanup outcomes remain unknown;
- raw prompts, secrets, commands, and peer payloads do not enter durable
  reports or metrics.

### 30.4 State-machine and graph tests

Cover sequential continuation, read-only fan-out/fan-in, partial branch
failure, cancellation, timeout, unavailable/quota-exhausted branches,
deterministic joins, bounded repair loops, and crash recovery. Parallel
writers must prove separate Attempt/workspace identities and explicit
integration before any combined artifact is accepted.

## 31. Qualification Strategy

Passing unit tests is not acceptance. Each phase needs reviewed evidence,
failure-matrix mapping, and reproducible fixtures.

The qualification sequence should be:

1. credential-free parser/capability/status fixtures;
2. disposable PostgreSQL/resource-lease races;
3. pinned runtime/OCI confinement and read-only role checks;
4. selected provider status evidence under explicit live-account authority;
5. independent reviewer/repair workflow with retained artifacts;
6. Orbit-on-Orbit task from an immutable baseline, independent validation,
   human review, and no developer-checkout mutation;
7. separate human decision on commit/merge/release.

Evidence must record exact baselines, image/runtime digests, resource
identities, availability source/scope, accepted artifacts, journal ranges,
checks actually run, and unresolved uncertainty. Export and inspect evidence
before sharing; redaction does not make artifact bytes public.

The current roadmap’s remote coding and ACP gates remain in force. Local
fixtures do not qualify live providers, a separately hosted worker, quota
semantics, or self-development safety.

## 32. Open Questions

These questions require evidence or an explicit product decision before the
corresponding phase is implemented:

1. Which documented status mechanism does each selected Codex/Antigravity
   runtime expose?
2. Can each status mechanism be proven not to consume a model inference turn?
3. Does each provider scope quota by credential, account, model, reasoning
   setting, runtime, or another dimension?
4. Which windows, reset timestamps, and exhaustion signals are structured?
5. How should a provider-wide negative result interact with model-specific
   resources when scope is not reported?
6. Which status probes may run automatically, and what probe budget/rate limit
   applies?
7. Should unknown availability be allowed for implementers, reviewers, or only
   explicit operator policy?
8. Where is the resource inventory authoritative when credentials are local to
   workers on different hosts?
9. What concurrency semantics does each provider permit per credential/model?
10. What is the smallest stable role-policy schema that covers read-only
    reviewer enforcement without coding-specific assumptions?
11. Should structured review findings first remain accepted artifacts, or does
    a qualified workload require a normalized indexed finding record?
12. Which initial join policy is required for multiple reviewers/testers:
    all-required, all-available, minimum-success, or another explicit rule?
13. When does a thin Goal aggregate become necessary rather than duplicative?
14. Which minimal loop/iteration abstraction is required after static bounded
    repair graphs are proven?
15. Which future activity types require an ExecutionEnvironment abstraction
    separate from the current Git Workspace?
16. What evidence is sufficient to approve automated commit/push/merge/release
    for Orbit itself?
17. What retention/compaction policy preserves status explanations without
    making provider payloads or sensitive repository artifacts durable by
    default?

## 33. Explicit Non-Goals

This design does not propose:

- arbitrary parallel multi-writer mutation of one workspace;
- swarm behavior or an LLM-selected scheduler/security policy;
- autonomous credential creation, privilege escalation, or secret discovery;
- unrestricted host shell, host paths, mounts, or network;
- automatic production deployment or release;
- infinite repair/continuation loops;
- provider-specific model IDs embedded in core domain enums;
- fake quota estimates, inferred token/cost usage, or speculative cost
  optimization;
- treating a provider conversation as durable task state;
- a generic workflow language or new execution engine beside the current one;
- a second authorization boundary in the UI/MCP/agent layer;
- a new Goal aggregate before the current Run model proves insufficient;
- a universal ActivityExecution hierarchy or per-role execution type; a future
  concrete recovery/persistence gap must first demonstrate that
  Task/Attempt cannot represent the specific activity.

Parallel read-only analysis and future isolated implementation branches may be
qualified later. They are not a first-phase requirement.

## 34. Definition of Done

The architecture is ready for implementation review when:

- the resource identity is canonical and secret-free;
- runtime, credential, model, reasoning effort, role, capability,
  availability, and activity kind are separate;
- arbitrary provider quota windows preserve unknown fields;
- stale evidence and execution-result updates are deterministic and scoped;
- native status discovery remains an evidence-driven spike;
- credential pools interact with both database resource leases and local ACP
  auth locks without implicit reuse;
- roles have explicit responsibilities and technically enforced read-only
  Tester/Reviewer boundaries;
- Validator, Tester, and Reviewer authority is unambiguous;
- role is persisted as execution evidence without replacing AgentExecution;
- continuation, branch/join, repair, and loop semantics remain distinct;
- every repair/review loop has explicit iteration and budget bounds;
- the Run/Task/Attempt model remains the first aggregate, with a clear Goal
  threshold;
- the architecture can grow from the current static graph to durable
  branching/fan-in/loop execution without shared-workspace writers;
- self-development and human approval boundaries are explicit;
- persistence, migration, crash recovery, observability, CLI/API, testing,
  qualification, and open questions are specified;
- the Q7 baseline and live-provider acceptance gates remain separate.

This draft satisfies the documentation task only. It does not satisfy any
future implementation or qualification gate.
