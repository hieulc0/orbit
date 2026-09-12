# Orbit

## Durable Execution Control Plane

**Status:** Architecture North Star / Greenfield Project\
**Project:** Orbit\
**CLI:** `orbit`\
**Core implementation:** Rust\
**API:** Rust + Axum\
**Durable state:** PostgreSQL\
**Frontend:** TypeScript + React\
**Execution engine:** Orbit Native Engine --- a purpose-built durable
graph runtime

------------------------------------------------------------------------

# 1. Executive Summary

Orbit is a standalone durable execution control plane for coordinating
heterogeneous work across software, compute, agents, humans, and
external systems.

Orbit is not primarily a visual workflow builder.

Orbit is not intended to be an n8n replacement, a CI/CD replacement, an
agent framework, a Kubernetes replacement, or a general-purpose clone of
Temporal.

Its purpose is narrower and deeper:

> **Submit durable work. Orbit keeps its state, coordinates its
> dependencies, survives failure, routes it to the appropriate runtime,
> and carries it toward a terminal state.**

The core product is the execution model and runtime. The CLI, API, MCP
interface, web operations console, and eventual visual editor are
clients of that runtime.

Orbit should remain useful even if no visual editor is installed.

The defining architectural principles are:

1.  **Execution-first, not canvas-first.**
2.  **Durability is a product property.**
3.  **Definitions are portable executable artifacts.**
4.  **The engine owns coordination, not computation.**
5.  **Agents, humans, containers, services, and compute are first-class
    execution participants.**
6.  **The native engine executes a constrained declarative graph, not
    arbitrary durable workflow code.**
7.  **PostgreSQL is the durable source of truth.**
8.  **Rust is the foundation of the control plane and engine.**
9.  **The core is standalone and domain-neutral.**
10. **Specialized systems should be coordinated rather than
    unnecessarily reimplemented.**

------------------------------------------------------------------------

# 2. Why Orbit Exists

Visual workflow products are already mature. Products such as n8n are
excellent at connecting applications, APIs, SaaS services, triggers, and
AI steps through a visual canvas.

Building another product whose central value is:

``` text
Trigger
  ↓
Node
  ↓
Node
  ↓
Node
```

does not create a sufficiently distinct platform.

The problem Orbit targets is different.

Consider an execution such as:

``` text
Input
  ↓
fan out 50 GPU evaluations
  ↓
join results
  ↓
run a containerized simulation
  ↓
delegate analysis to several agents
  ↓
human approval
  ↓
deploy
  ↓
wait 12 hours
  ↓
observe external health signal
  ↓
promote or rollback
```

During this execution:

-   workers may disappear;
-   the server may restart;
-   tasks may take milliseconds or hours;
-   an approval may arrive days later;
-   external systems may timeout after actually completing a side
    effect;
-   agents may fail or exhaust budgets;
-   compute may need GPUs or specialized workers;
-   cancellation may occur while work is in progress;
-   the entire execution must remain inspectable and auditable.

Orbit exists to provide the durable coordination layer for this class of
work.

------------------------------------------------------------------------

# 3. Product Philosophy

## 3.1 Orbit is execution-first

The architecture is:

``` text
              Definition
                  │
                  ▼
               Compiler
                  │
                  ▼
          Immutable ExecutionPlan
                  │
                  ▼
             Orbit Engine
                  │
       durable coordination
                  │
       ┌──────────┼──────────┐
       ▼          ▼          ▼
    Compute      Agent      Human
```

Clients sit above the execution platform:

``` text
           CLI     API     MCP     Web     Agents
            │       │       │      │        │
            └───────┴───────┼──────┴────────┘
                            ▼
                         Orbit
```

The canvas is therefore not the architecture. It is an IDE for authoring
and inspecting Orbit definitions.

## 3.2 Orbit owns coordination, not computation

Orbit owns:

-   durable execution state;
-   dependency resolution;
-   scheduling;
-   retries;
-   timeouts;
-   cancellation;
-   timers;
-   signals;
-   fan-out/fan-in;
-   task routing;
-   worker leases;
-   recovery;
-   execution history;
-   policies;
-   concurrency control.

Workers and external systems own:

-   AI inference;
-   LLM reasoning;
-   3D processing;
-   compilation;
-   container execution;
-   deployment implementation;
-   scientific computing;
-   specialized domain logic.

The rule is:

> **Orbit decides when and why work executes. Runtimes determine how the
> work is performed.**

## 3.3 Coordinate existing systems instead of replacing them

Orbit should not become:

-   a container orchestrator when Kubernetes is the correct runtime;
-   a model server when a dedicated inference system is better;
-   a Git hosting platform;
-   a full CI product;
-   a SaaS integration catalog;
-   an LLM itself.

Orbit should be able to coordinate these systems durably.

## 3.4 Standalone and domain-neutral

Orbit must not depend on a parent product, company, industry, customer,
or historical project.

A completely unrelated organization must be able to install Orbit
without knowing why it was originally created.

Domain-specific behavior belongs in packages, workers, adapters, and
integrations outside the core.

------------------------------------------------------------------------

# 4. What Orbit Is Not

Orbit is deliberately **not**:

-   a visual automation product with an execution engine attached;
-   an n8n clone;
-   a Temporal clone;
-   an arbitrary durable-code runtime;
-   a replacement for Kubernetes;
-   a replacement for GitLab CI or GitHub Actions;
-   a replacement for agent-native subagent reasoning;
-   a universal integration marketplace at launch;
-   an event broker;
-   a database;
-   a secrets manager.

Orbit may integrate with all of these.

------------------------------------------------------------------------

# 5. Product Identity

**Name:** Orbit

**Category:**

> Durable Execution Control Plane

**Technical description:**

> Orbit is a standalone, Rust-native durable execution platform for
> coordinating compute, agents, humans, and external systems.

**Core promise:**

> **Define the work. Orbit keeps it moving toward completion.**

**Architectural principle:**

> **Orbit owns coordination, not computation.**

A useful conceptual image is:

``` text
                    Orbit
                      │
        ┌─────────────┼─────────────┐
        ▼             ▼             ▼
     Agents         Compute       Humans
        │             │             │
        └─────────────┼─────────────┘
                      ▼
               External Systems
```

------------------------------------------------------------------------

# 6. Core Vocabulary

Orbit should avoid inheriting visual-workflow terminology as its
internal model.

## Definition

A user-authored, portable declaration of work.

## Step

A logical unit in a Definition.

A Step may represent computation, control flow, an agent, a wait, a
human action, or another definition.

## ExecutionPlan

The compiled, validated, immutable representation that the engine
executes.

## Run

One execution of an ExecutionPlan.

## Task

A schedulable unit of work generated from a Step.

## Attempt

One physical attempt to execute a Task.

A Task can have multiple Attempts due to retry or worker failure.

## Worker

A runtime capable of claiming and executing Tasks for specific
capabilities.

## Signal

External information delivered to a waiting Run or Step.

## Timer

A durable wake-up condition based on time.

## Artifact

A durable output or input referenced by execution.

## Journal

The append-only history of meaningful execution transitions.

------------------------------------------------------------------------

# 7. Definition as a First-Class Executable Artifact

An Orbit Definition is not merely an export from a database.

It is a portable program for the Orbit execution model.

Example:

``` yaml
apiVersion: orbit/v1
kind: Definition

metadata:
  name: model-release

inputs:
  model:
    type: artifact

steps:
  benchmark:
    uses: container.run
    with:
      image: evaluator:1.4

  analyze:
    uses: agent.run
    needs:
      - benchmark

  approve:
    uses: human.approval
    needs:
      - analyze

  deploy:
    uses: deployment.execute
    needs:
      - approve
```

The same Definition should be usable from:

-   Git;
-   a developer laptop;
-   CI;
-   an autonomous coding agent;
-   the Orbit API;
-   the Orbit web UI;
-   MCP;
-   another product embedding Orbit.

The CLI should eventually support:

``` bash
orbit validate model-release.yaml
orbit run model-release.yaml
orbit inspect <run-id>
orbit events <run-id>
orbit cancel <run-id>
```

------------------------------------------------------------------------

# 8. Compilation Model

Definitions are not executed directly.

``` text
Definition
    │
    ▼
Parse
    │
    ▼
Schema Validation
    │
    ▼
Step/Package Resolution
    │
    ▼
Policy Validation
    │
    ▼
Binding Resolution
    │
    ▼
Dependency Analysis
    │
    ▼
Compile
    │
    ▼
Immutable ExecutionPlan
```

The ExecutionPlan contains all information required for deterministic
graph scheduling without reinterpreting a mutable Definition.

A Run always references an immutable ExecutionPlan.

Changes to a Definition affect new Runs, never silently alter an active
Run.

------------------------------------------------------------------------

# 9. Why Orbit Has a Native Engine

A greenfield Orbit does not need to inherit Temporal.

Temporal is excellent at durable workflow execution, but it introduces a
substantial independent runtime and is designed to support general
durable workflow programming.

Orbit has a narrower execution model:

> **A declarative, immutable, durable execution graph.**

Orbit does not need to replay arbitrary workflow source code to discover
what should happen next.

Given:

``` text
A ──→ B ──┬──→ D
           │
           └──→ C ──→ E
```

the durable state may simply be:

``` text
A = SUCCEEDED
B = SUCCEEDED
C = RUNNING
D = READY
E = BLOCKED
```

After restart, Orbit reads the state, reconciles leases, and continues.

This permits a substantially simpler model than building a general
Temporal-equivalent system.

Orbit therefore builds a:

> **Purpose-built journaled durable graph runtime.**

It must not expand into an arbitrary durable-code framework without a
separate architectural decision.

------------------------------------------------------------------------

# 10. Engine Invariants

The native engine should be designed around explicit invariants.

## 10.1 Durable accepted runs

Once Orbit acknowledges that a Run has been durably accepted, loss of
the API process must not erase that Run.

## 10.2 Durable state transitions

Meaningful execution transitions are committed to PostgreSQL.

## 10.3 Immutable plans

An active Run executes the ExecutionPlan it started with.

## 10.4 At-least-once task execution

Orbit should not claim exactly-once execution for arbitrary external
side effects.

The baseline guarantee is:

> **Durable at-least-once task execution with idempotency support.**

## 10.5 Idempotency identity

Every Task should expose a stable idempotency identity where
appropriate.

Conceptually:

``` text
run_id
step_instance_id
task_id
attempt_id
idempotency_key
```

## 10.6 Leased execution

Worker ownership of a Task is temporary and renewable.

A dead worker must not permanently own work.

## 10.7 Recovery by reconciliation

Recovery reads durable state and repairs incomplete or expired execution
ownership.

## 10.8 No worker is required during a wait

A timer, approval, or external signal may wait for days without holding
a worker process.

## 10.9 Cancellation is durable intent

Cancellation is persisted before propagation.

## 10.10 History is inspectable

The system should make it possible to answer:

-   what happened;
-   in what order;
-   on which attempt;
-   on which worker;
-   why a retry occurred;
-   why a task was blocked;
-   why a Run terminated.

------------------------------------------------------------------------

# 11. Execution State Model

A first task state machine can be intentionally small:

``` text
             PENDING
                │
                ▼
              READY
                │
                ▼
             CLAIMED
                │
                ▼
             RUNNING
             /  |   \
            /   |    \
           ▼    ▼     ▼
   SUCCEEDED  FAILED  WAITING
               │
               ▼
        RETRY_SCHEDULED
               │
               ▼
             READY
```

Terminal/cancellation states may include:

``` text
SUCCEEDED
FAILED
CANCELLED
SKIPPED
```

The exact state machine must be specified formally before
implementation.

------------------------------------------------------------------------

# 12. PostgreSQL as the Durable Source of Truth

Orbit should begin with PostgreSQL as its only mandatory external
stateful dependency.

Core state may include:

``` text
definitions
definition_revisions
execution_plans

runs
run_steps
tasks
task_attempts

run_events

timers
signals

workers
worker_leases

artifact_metadata
```

Redis should not be required by the core.

Specialized integrations may use Redis, Kafka, NATS, MQTT, or other
systems independently.

A minimal Orbit deployment should be approximately:

``` text
Orbit
  +
PostgreSQL
```

Object storage becomes required only when the chosen workload needs
external artifact storage.

Project/environment storage is optional and follows a concrete product requirement;
it is not a prerequisite for run ownership or worker authorization.

------------------------------------------------------------------------

# 13. Journal + Materialized State

Orbit should not begin as a pure event-sourced system.

Use both:

``` text
           Append-only Journal
                  │
             run_events

                  +

          Current Projection
            │           │
           runs       run_steps
```

A meaningful transition should update current state and append its
journal record transactionally where possible.

Example events:

``` text
RUN_CREATED
PLAN_COMPILED
RUN_STARTED

STEP_READY

TASK_CREATED
TASK_CLAIMED
TASK_STARTED
TASK_HEARTBEAT
TASK_SUCCEEDED
TASK_FAILED
TASK_RETRY_SCHEDULED

TIMER_CREATED
TIMER_FIRED

SIGNAL_RECEIVED

RUN_CANCEL_REQUESTED
RUN_CANCELLED

RUN_SUCCEEDED
RUN_FAILED
```

This supports:

-   efficient state queries;
-   complete history;
-   auditability;
-   debugging;
-   SSE;
-   agent inspection;
-   future projection rebuilding.

------------------------------------------------------------------------

# 14. Scheduling and Worker Leases

Workers claim eligible Tasks through durable coordination.

PostgreSQL may initially provide the queue/claim mechanism using
transactions and patterns such as `FOR UPDATE SKIP LOCKED`.

Conceptually:

``` text
READY task
   │
   ▼
atomic claim
   │
   ▼
lease created
   │
   ▼
worker executes
   │
   ├── heartbeat
   │
   ▼
complete / fail
```

If a lease expires:

``` text
worker disappears
      │
      ▼
lease expires
      │
      ▼
scheduler reconciles
      │
      ▼
task becomes retryable
```

The engine must carefully define behavior around:

-   claim transactions;
-   worker acknowledgement;
-   lease renewal;
-   completion races;
-   cancellation races;
-   retry ownership;
-   stale completion;
-   duplicated delivery.

------------------------------------------------------------------------

# 15. Failure Semantics

Distributed execution cannot promise that arbitrary external side
effects happen exactly once.

Example:

``` text
Worker
  │
  ├── POST payment/deployment/action
  │
  ▼
External system succeeds
  │
  X network response lost
  │
Orbit observes timeout
```

Orbit cannot infer whether retrying the external action is safe.

Therefore the worker protocol should support:

``` text
ExecutionContext
├── run_id
├── step_instance_id
├── task_id
├── attempt_id
├── idempotency_key
├── deadline
└── cancellation state
```

Integrations should use idempotency keys when supported.

Orbit guarantees durable coordination, not magical exactly-once external
side effects.

------------------------------------------------------------------------

# 16. Timers and Waits

A wait is persisted state, not a sleeping worker.

For a timer:

``` text
Step = WAITING

Timer
  wake_at = 2026-09-07T12:00:00Z
```

A timer service detects due timers and transactionally makes the
corresponding execution eligible to continue.

This permits millions of long-lived executions without keeping
equivalent numbers of tasks or processes alive.

Timer scalability should be benchmarked explicitly.

------------------------------------------------------------------------

# 17. Signals and Human Interaction

External events should be able to resume waiting execution.

Conceptually:

``` text
Run
  │
  ▼
WAITING: approval
  │
  │       external signal
  │             │
  └─────────────┘
          │
          ▼
        READY
```

Signals need:

-   durable persistence;
-   correlation;
-   authorization;
-   deduplication;
-   payload schema validation;
-   expiry policy;
-   handling of signal-before-wait races.

Human approval is a specialized use of durable signaling, not a worker
sleeping until a user clicks a button.

------------------------------------------------------------------------

# 18. Fan-Out and Fan-In

Orbit should support dynamic Step instances without abandoning its
explicit execution model.

Example:

``` text
Input
  │
  ▼
Foreach
  │
  ├── Task[0]
  ├── Task[1]
  ├── Task[2]
  └── Task[N]
          │
          ▼
         Join
```

The engine persists concrete instances and dependency state.

Large fan-out must be designed with:

-   bounded expansion;
-   concurrency limits;
-   batching;
-   backpressure;
-   efficient dependency accounting;
-   cancellation behavior;
-   partial failure policy.

------------------------------------------------------------------------

# 19. Capability-Based Runtime Routing

A Definition should not care about a worker's implementation language.

Example Step:

``` yaml
benchmark:
  uses: model.evaluate@1
```

Resolved runtime metadata might require:

``` yaml
runtime:
  capabilities:
    - model.evaluate
    - gpu
  resources:
    gpu: 1
    memory: 8Gi
```

Workers register capabilities:

``` json
{
  "worker_id": "worker-123",
  "capabilities": [
    "python",
    "model.evaluate",
    "gpu"
  ]
}
```

The scheduler routes work based on capabilities and policy.

Execution requirements also describe network policy, filesystem policy, credential
requirements and a logical isolation class. Operator policy selects an eligible
backend that satisfies every requirement. Definitions do not select gVisor,
Firecracker or another infrastructure implementation. An unsupported requirement
must fail closed; it cannot silently fall back to weaker execution.

Potential worker types:

``` text
Rust Worker
├── system tasks
├── HTTP
├── artifact operations
└── CPU-efficient primitives

Python Worker
├── AI
├── scientific computing
├── data processing
└── 3D libraries

Container Runner
└── arbitrary OCI workloads

Agent Runtime
├── LLM agents
├── coding agents
└── MCP-enabled agents

External Worker
└── domain-specific services
```

------------------------------------------------------------------------

# 20. Agents as First-Class Participants

Orbit should not reduce agents to a decorative "LLM node."

An Agent Step can have first-class execution semantics:

``` text
Agent Step
├── agent identity
├── model binding
├── tools
├── permissions
├── context
├── token/cost budget
├── timeout
├── delegation policy
└── output contract
```

Example:

``` text
                    Input
                      │
           ┌──────────┼──────────┐
           ▼          ▼          ▼
       Agent A     Agent B     Compute
           │          │          │
           └──────────┼──────────┘
                      ▼
                    Review
                      │
               Human Approval
                      │
                    Deploy
```

Orbit provides durability around the agents.

An agent runtime remains responsible for reasoning, model interaction,
tool loops, and agent-specific execution.

The first live coding runtime keeps model interaction in a trusted worker and
repository tools in a disposable OCI workspace. Repository fetching and model
calls have separately authorized network/credential access. Networkless tools
receive neither provider keys nor Orbit lease credentials. Independent tests
apply accepted patch artifacts to a fresh base workspace before human review.

------------------------------------------------------------------------

# 21. Agent Fan-Out and Delegation

Orbit should distinguish two kinds of orchestration.

## Durable orchestration

Orbit determines the durable structure:

``` text
Agent A ─┐
Agent B ─┼── Join → Reviewer
Agent C ─┘
```

## Agent-native delegation

An individual agent may internally create temporary subagents or tool
calls.

Orbit does not need to persist every internal thought or ephemeral
subagent as a top-level Step.

The boundary is:

> **If the work requires independent durability, policy, observability,
> approval, retry, or lifecycle management, it belongs in Orbit.**

Otherwise it may remain internal to the agent runtime.

------------------------------------------------------------------------

# 22. Humans as Execution Participants

Humans should be modeled explicitly where durable process interaction
requires them.

Potential Step types:

``` text
human.approval
human.input
human.review
```

These should support:

-   assignment;
-   authorization;
-   deadline;
-   escalation;
-   structured response schema;
-   audit;
-   cancellation;
-   timeout policy.

------------------------------------------------------------------------

# 23. Containers and Compute

Container execution is an important generic escape hatch.

A Step may eventually specify:

``` yaml
uses: container.run

with:
  image: example/evaluator:1.4
  command:
    - evaluate

resources:
  cpu: 4
  memory: 16Gi
  gpu: 1
```

Orbit should not become a container scheduler.

Small installations may use a local container runner.

Larger installations may route container work to Kubernetes or another
compute backend.

The Definition should express execution requirements rather than
hard-code infrastructure where possible.

Isolation is a first-class execution requirement, separate from authorization.
The first supported profile targets trusted workloads with rootless Podman/OCI.
An operator may later qualify gVisor/runsc for a `sandboxed` class or Firecracker
microVMs for an `untrusted` class. These are candidate policy mappings, not
universal security guarantees or an obligation to implement all backends.
Each class needs a defined threat model and evidence. The backend must enforce
network, filesystem and resource restrictions as well as the isolation class.

------------------------------------------------------------------------

# 24. Artifacts

Large outputs should not flow through the engine database.

Orbit stores artifact metadata and references while bytes live in an
artifact provider.

Conceptually:

``` text
Artifact
├── id
├── optional scope association
├── run_id
├── producing_step
├── provider
├── object_key
├── content_type
├── size
├── checksum
└── metadata
```

Providers may include:

-   local filesystem for development;
-   S3-compatible storage;
-   cloud object stores.

Artifact identity should remain durable even when access URLs are
temporary.

------------------------------------------------------------------------

# 25. Rust Technology Foundation

Orbit's core should be Rust-native because it is infrastructure software
rather than primarily an application backend.

Initial stack:

``` text
Language          Rust
Async runtime     Tokio
HTTP/API          Axum
Serialization     Serde
Database          PostgreSQL
DB client         SQLx
CLI               Clap
Tracing           tracing
API schema        OpenAPI where useful
Frontend          TypeScript + React
Realtime          SSE initially
```

Rust is particularly appropriate for:

-   scheduler loops;
-   large concurrent worker populations;
-   timers;
-   streaming;
-   worker protocols;
-   backpressure;
-   predictable resource usage;
-   long-running infrastructure processes;
-   future high-throughput event handling.

This does not mean every worker must be Rust.

Python remains a first-class runtime for AI, scientific, and
ecosystem-heavy workloads.

------------------------------------------------------------------------

# 26. Modular Monolith First

Orbit should begin as a modular monolith, not a microservice
architecture.

Initial deployment:

``` text
┌─────────────────────────────────┐
│ Orbit Server                    │
│                                 │
│ Axum API                        │
│ Definition Service              │
│ Compiler                        │
│ Execution Engine                │
│ Scheduler                       │
│ Timer Service                   │
│ Signal Service                  │
│ Worker Gateway                  │
│ Journal                         │
└────────────────┬────────────────┘
                 │
             PostgreSQL
```

The internal codebase should nevertheless use strong module boundaries.

Later, if scaling requires it, the same crates may power independent
processes:

``` text
orbit-api
orbit-scheduler
orbit-worker-gateway
orbit-event-gateway
```

Deployment topology should follow measured requirements rather than
being fixed prematurely.

------------------------------------------------------------------------

# 27. Proposed Repository Layout

``` text
orbit/
├── crates/
│   ├── orbit-core/
│   ├── orbit-definition/
│   ├── orbit-compiler/
│   ├── orbit-engine/
│   ├── orbit-journal/
│   ├── orbit-scheduler/
│   ├── orbit-worker-protocol/
│   ├── orbit-artifacts/
│   ├── orbit-policy/
│   ├── orbit-server/
│   └── orbit-cli/
│
├── sdk/
│   ├── python/
│   └── typescript/
│
├── workers/
│   └── python/
│
├── web/
│   └── ...
│
├── examples/
│   └── ...
│
└── docs/
    └── ...
```

The crate boundaries should be validated through dependency rules rather
than treated as a final structure on day one.

------------------------------------------------------------------------

# 28. CLI-First Product Surface

The CLI should exist before the visual editor.

Initial desired experience:

``` bash
orbit validate example.yaml

orbit run example.yaml

orbit runs

orbit inspect <run-id>

orbit events <run-id>

orbit cancel <run-id>

orbit workers
```

Machine-readable output is mandatory:

``` bash
orbit runs --output json
orbit events <run-id> --output jsonl
```

The CLI should contain minimal business logic.

It calls the same domain/API contracts used by other clients.

The test is:

> **If Orbit is valuable with only its CLI and API, the execution
> platform is genuinely the product.**

------------------------------------------------------------------------

# 29. API and Realtime

The Axum server should expose stable APIs around domain concepts rather
than engine internals.

Representative resources:

``` text
/definitions
/plans
/runs
/runs/{id}
/runs/{id}/events
/runs/{id}/signals
/workers
/artifacts
```

SSE is a good initial fit for Run event streaming:

``` text
GET /runs/{id}/events/stream
```

WebSocket should only be introduced when bidirectional realtime
semantics materially justify it.

------------------------------------------------------------------------

# 30. MCP and Agent Operation

Orbit should be easy for autonomous agents to operate.

An MCP interface can expose resources such as:

``` text
orbit://run/{id}
orbit://definition/{id}
orbit://artifact/{id}
```

and tools such as:

``` text
validate_definition
submit_run
get_run
get_run_events
cancel_run
signal_run
get_artifact
list_workers
```

MCP is an adapter over Orbit's domain services.

It must not become Orbit's internal architecture.

The same principle applies to REST, CLI, and the web UI.

------------------------------------------------------------------------

# 31. Web UI Philosophy

The first web UI should be an **operations console**, not a canvas.

Initial UI:

``` text
Runs
├── status
├── timeline
├── steps
├── tasks
├── attempts
├── failures
└── artifacts

Workers
├── capabilities
├── leases
├── health
└── active tasks

Definitions
├── versions
├── validation
└── runs
```

Only after the execution experience is strong should Orbit add a visual
authoring IDE.

The visual editor should edit the same Definition that the CLI and Git
use.

There must not be a separate "visual workflow format."

------------------------------------------------------------------------

# 32. Security Direction

Orbit should eventually distinguish:

``` text
User
Service Account
Worker Identity
Agent Identity
External Integration
```

Authorization should operate on resources and actions rather than be
embedded in HTTP routes.

Examples:

``` text
definition.read
definition.write
run.submit
run.cancel
run.signal
artifact.read
worker.register
agent.execute
```

Workers should only receive secrets required for their Task.

The engine database should not become an unrestricted plaintext secret
store.

A provider/binding abstraction can be introduced once the basic engine
is proven.

Authorization determines which tools, credentials, artifacts, budgets and
delegation a task may use. Containment limits what its processes can physically
access or affect. Orbit checks authorization at admission and dispatch; execution
backends enforce filesystem, network, process and resource restrictions. Shell or
filesystem permissions do not constitute containment. Execution isolation and
tenant identity management are separate concerns with separate acceptance gates.

------------------------------------------------------------------------

# 33. Scoping and Ownership

Orbit preserves authenticated run attribution, worker admission, credential-use
authorization and artifact access boundaries. These contracts remain compatible
with future namespace or project scoping without requiring a tenant hierarchy.

Full tenancy, organization management, RBAC administration, SSO and hostile
multi-tenant identity management are not current kernel requirements. Introduce
them only for a concrete product requirement.

Existing optional organization/project/environment scopes and authorization
contracts remain supported. Deferring further tenancy work does not remove
enforcement or change accepted plans. Workers cannot redefine accepted scope
through arbitrary Step parameters. Attribution alone is not an access grant.

------------------------------------------------------------------------

# 34. Packages and Extensibility

Orbit should eventually support installable capabilities without making
the core runtime unsafe.

A package may provide:

-   Step definitions;
-   worker implementations;
-   schemas;
-   UI metadata;
-   MCP capabilities;
-   agent definitions;
-   connection types.

Core built-ins should remain small.

Potential core primitives:

``` text
control.if
control.switch
control.parallel
control.foreach
control.join

wait.timer
wait.signal

http.request

definition.call
```

Heavy functionality belongs outside the scheduler/server process.

Do not dynamically load arbitrary untrusted Python/Rust code into the
core Orbit server.

------------------------------------------------------------------------

# 35. Observability

Orbit should be observable from its first serious engine milestone.

Required dimensions include:

-   Run throughput;
-   Run latency;
-   ready task count;
-   claim latency;
-   queue wait time;
-   task execution duration;
-   retry rate;
-   lease expiry rate;
-   scheduler loop latency;
-   timer wake-up lag;
-   DB pool saturation;
-   journal append latency;
-   worker utilization;
-   active waits;
-   cancellation latency.

Tracing should connect:

``` text
Run
  ↓
Step
  ↓
Task
  ↓
Attempt
  ↓
Worker
```

Logs alone are insufficient.

------------------------------------------------------------------------

# 36. Engine Correctness Testing

The native engine must be developed as infrastructure.

Happy-path tests are insufficient.

The test suite should deliberately inject failures at boundaries such
as:

``` text
after task claim
before claim commit
after claim commit

before worker start
during execution

after external work
before completion report

before completion transaction
after completion transaction

during cancellation

during timer firing

during signal delivery
```

Qualification scenarios should include:

``` text
submit 1,000 runs
kill Orbit
restart Orbit
verify recovery

kill random workers
verify lease recovery

duplicate completion messages
verify state correctness

deliver duplicate signals
verify deduplication

cancel during retry
verify no unintended continuation

large fan-out
verify bounded DB and memory behavior
```

Property-based and state-machine testing should be considered for core
engine invariants.

------------------------------------------------------------------------

# 37. Performance Philosophy

Correctness comes before extreme throughput.

Orbit should not prematurely add:

-   Redis;
-   Kafka;
-   distributed caches;
-   separate scheduler clusters;
-   custom storage engines.

Start with PostgreSQL and measure.

Scale architecture only when measurements identify a real bottleneck.

Potential future optimization paths include:

``` text
PostgreSQL queue
      ↓
partitioned queues
      ↓
dedicated dispatch subsystem
```

or specialized event ingress without changing the public execution
model.

------------------------------------------------------------------------

# 38. Built-In Engine Scope Boundary

The biggest architectural risk is accidentally building a
general-purpose Temporal competitor.

Orbit's native engine supports:

-   declarative graphs;
-   explicit dependencies;
-   task execution;
-   retry;
-   timeout;
-   durable timers;
-   durable signals;
-   fan-out/fan-in;
-   cancellation;
-   worker leases;
-   recovery;
-   history;
-   capability routing.

It does **not initially support**:

-   arbitrary durable Rust/Python workflow source code;
-   deterministic replay of arbitrary application code;
-   arbitrary mutation of an active ExecutionPlan;
-   unbounded recursive execution;
-   implicit hidden control flow;
-   exactly-once external side effects.

This boundary should be defended.

------------------------------------------------------------------------

# 39. Why Not Temporal

Orbit is not rejecting Temporal because Temporal is technically weak.

The choice is based on product architecture.

Using Temporal would provide mature durability quickly, but:

1.  Orbit is greenfield and can define a constrained graph execution
    model.
2.  The native engine is strategically part of Orbit's product identity.
3.  Standalone deployment should remain small.
4.  PostgreSQL can serve as the initial durable coordination substrate.
5.  Orbit wants control over scheduling, worker capabilities, agent
    semantics, and execution introspection.
6.  There is no migration cost from an existing Orbit runtime.

If native-engine complexity begins to dominate the project without
delivering product value, this decision must be revisited rather than
defended ideologically.

------------------------------------------------------------------------

# 40. Why Not Build on a Basic Queue

A task queue alone does not provide the semantics Orbit requires.

Starting with a simple queue and incrementally adding:

``` text
workflow state
dependencies
timers
signals
leases
recovery
fan-in
cancellation
history
```

would create an accidental workflow engine.

Orbit should either build its durable graph engine deliberately or use
an established durable execution system.

The native engine is therefore a conscious architecture commitment, not
an accidental evolution from a background-job queue.

------------------------------------------------------------------------

# 41. Why Rust

Rust is not chosen because Python is unsuitable for application APIs.

Rust is chosen because Orbit's center is infrastructure:

``` text
scheduler
journal
leases
worker protocol
timers
signals
streaming
backpressure
concurrency
recovery
```

Rust provides strong memory safety, concurrency support, predictable
runtime characteristics, and an excellent foundation for a long-lived
execution service.

Python remains intentionally supported where its ecosystem is strongest.

------------------------------------------------------------------------

# 42. Why a New Project

Orbit should not be implemented as a refactor of Pipeline Studio.

The two systems have different product abstractions.

A traditional Pipeline Studio architecture centers on:

``` text
Visual Editor
    ↓
Node Graph
    ↓
Workflow Runtime
```

Orbit centers on:

``` text
Portable Definition
      ↓
Compiler
      ↓
Immutable ExecutionPlan
      ↓
Native Durable Engine
      ↓
Heterogeneous Runtimes

Clients:
CLI / API / MCP / Web / Agents
```

Trying to preserve historical schemas, frontend assumptions, runtime
contracts, and workflow-engine semantics would constrain the new design
before its execution model is proven.

The correct approach is:

> **Carry forward lessons, failure cases, tests, and architectural
> knowledge---not legacy product structure.**

Orbit is a new project.

------------------------------------------------------------------------

# 43. Development Roadmap

The phases below describe the original development sequence. Active priorities,
implemented support and remaining acceptance gates are in [the current roadmap](../ROADMAP.md).
Historical phase records do not make future tenancy or infrastructure mandatory.

## Phase 0 --- Semantics Before Code

Define formally:

-   Definition;
-   Step;
-   ExecutionPlan;
-   Run;
-   Task;
-   Attempt;
-   Worker;
-   lease;
-   Signal;
-   Timer;
-   Artifact;
-   cancellation;
-   retry;
-   terminal states.

Deliverables:

``` text
VISION.md
ENGINE_SEMANTICS.md
STATE_MACHINES.md
FAILURE_MODEL.md
DEFINITION_SPEC.md
```

Do not build a web editor.

## Phase 1 --- Native Engine Kernel

Implement:

-   PostgreSQL schema;
-   immutable plans;
-   Run creation;
-   DAG dependencies;
-   ready-task scheduling;
-   task claiming;
-   leases;
-   attempts;
-   retry;
-   timeout;
-   cancellation;
-   journal;
-   recovery.

Success criterion:

> Runs survive repeated server and worker termination without violating
> documented state invariants.

## Phase 2 --- Durable Interaction

Implement:

-   timers;
-   signals;
-   waits;
-   fan-out;
-   join;
-   child Definition execution;
-   concurrency limits;
-   backpressure.

## Phase 3 --- Developer Surface

Implement:

-   `orbit` CLI;
-   YAML/JSON Definition format;
-   Axum API;
-   SSE;
-   JSON/JSONL output;
-   Rust worker SDK;
-   Python worker SDK.

At this milestone Orbit should already be useful without a web UI.

## Phase 4 --- Compute and Artifacts

Implement:

-   artifact abstraction;
-   local artifact provider;
-   S3-compatible provider;
-   container runner;
-   resource requirements;
-   capability routing;
-   worker pools.

## Phase 5 --- Agent Execution

Implement:

-   Agent Step;
-   model/tool bindings;
-   budgets;
-   permissions;
-   MCP;
-   durable agent execution;
-   controlled delegation;
-   human approval.

## Phase 6 --- Operations UI

Implement React operations console:

-   Runs;
-   timeline;
-   attempts;
-   artifacts;
-   workers;
-   queues;
-   failure inspection;
-   cancellation/signaling.

## Phase 7 --- Visual Definition IDE

Only now implement:

-   graph visualization;
-   visual editing;
-   schema-driven Step panels;
-   Definition source synchronization;
-   diff/validation.

The graph editor edits the canonical Definition.

## Phase 8 --- Ownership and Authorization

Implement:

-   authenticated run attribution;
-   worker and artifact authorization;
-   service accounts;
-   secret/binding providers;
-   policies;
-   audit controls.

Existing optional scoped governance is retained for compatibility. Further tenant
hierarchy, RBAC administration and SSO require a concrete product requirement.

## Phase 9 --- Ecosystem

Implement:

-   package registry;
-   capability packages;
-   verified packages;
-   SDK stabilization;
-   marketplace if justified.

------------------------------------------------------------------------

# 44. First Engineering Milestone

The first milestone should intentionally look unimpressive from the
outside.

Example Definition:

``` yaml
apiVersion: orbit/v1
kind: Definition

metadata:
  name: recovery-test

steps:
  first:
    uses: test.sleep
    with:
      milliseconds: 500

  second:
    uses: test.echo
    needs:
      - first

  third:
    uses: test.fail_n_times
    needs:
      - second
    retry:
      max_attempts: 3
```

Then:

``` bash
orbit run recovery-test.yaml
```

During execution:

1.  kill the Orbit server;
2.  restart it;
3.  kill the worker;
4.  restart the worker;
5.  verify the failed task retries correctly;
6.  verify every journal event;
7.  verify the Run reaches the correct terminal state.

If this is boring and correct, the foundation is good.

------------------------------------------------------------------------

# 45. Architecture Decision Rules

Every major feature should be tested against these questions.

### Is this coordination or computation?

If computation, it probably belongs in a worker/runtime.

### Does this require durable state?

If not, it may not belong in the engine.

### Can an existing specialized system do this better?

If yes, integrate rather than recreate.

### Does the Definition need to know deployment topology?

Prefer capability/resource requirements over infrastructure-specific
details.

### Does this require a new abstraction?

Prefer the current execution contract when it can satisfy the selected workload.
Add isolation backends, credential providers and tenant models only for a defined
requirement; do not build a universal policy or plugin framework preemptively.

### Does this require arbitrary workflow code?

If yes, reconsider whether it belongs in Orbit's native engine.

### Can the feature work without the visual editor?

Core execution features should.

### Does it make PostgreSQL insufficient based on measurements?

Only then introduce another mandatory infrastructure dependency.

### Does it leak a domain/customer/product assumption into Core?

If yes, move it to an extension.

------------------------------------------------------------------------

# 46. Long-Term Product Shape

A mature Orbit may look like:

``` text
                         Orbit

             Durable Execution Control Plane

     ┌──────────┬───────────┬───────────┬──────────┐
     │          │           │           │          │
    CLI        API         MCP         Web       SDKs
     │          │           │           │          │
     └──────────┴───────────┼───────────┴──────────┘
                            │
                      Definition Layer
                            │
                         Compiler
                            │
                    Immutable Plans
                            │
                      Native Engine
                            │
         ┌──────────────────┼───────────────────┐
         │                  │                   │
      Scheduler          Journal             Policy
         │                  │                   │
         └──────────────────┼───────────────────┘
                            │
                        PostgreSQL
                            │
          ┌─────────────────┼──────────────────┐
          │                 │                  │
      Rust Workers      Python Workers    Agent Runtime
          │                 │                  │
       systems           AI / data          LLM/MCP
          │                 │                  │
          ├─────────────────┼──────────────────┤
          │                 │                  │
      Containers       GPU / 3D            Humans
          │                 │                  │
          └─────────────────┼──────────────────┘
                            │
                    External Systems
```

The graph editor, if present, remains one view over this architecture.

------------------------------------------------------------------------

# 47. Success Criteria

Orbit is succeeding if:

-   a developer prefers `orbit run` because it provides durable
    execution without operational complexity;
-   an agent can safely submit and inspect work through stable
    machine-readable interfaces;
-   a long-running Run survives process and worker failure;
-   Python, Rust, containers, agents, and humans participate in the same
    execution model;
-   a Definition can live naturally in Git;
-   the web UI is optional for execution;
-   a visual editor does not create a second proprietary representation;
-   the core remains usable without domain-specific packages;
-   PostgreSQL remains sufficient until measurements prove otherwise;
-   users think of Orbit as execution infrastructure rather than a
    low-code automation clone.

Orbit is drifting if:

-   the canvas becomes the product;
-   integration count becomes the primary metric;
-   arbitrary plugin code runs inside the scheduler;
-   engine semantics become hidden and ambiguous;
-   every feature becomes another visual "node";
-   exactly-once behavior is promised where it cannot be guaranteed;
-   domain-specific assumptions enter the core;
-   the native engine grows into an unrestricted Temporal clone.

------------------------------------------------------------------------

# 48. Final Principle

Orbit exists because durable coordination across heterogeneous work is a
distinct infrastructure problem.

It should not attempt to perform every specialized task itself.

It should make those tasks composable, durable, inspectable,
recoverable, and operable by both humans and machines.

> **Orbit owns coordination, not computation.**

> **Definitions are portable executable artifacts.**

> **Durability is part of the product contract.**

> **The engine executes explicit plans, not arbitrary hidden workflow
> logic.**

> **The CLI, API, MCP, web UI, and agents are peers over the same
> execution platform.**

> **Define the work. Orbit keeps it moving toward completion.**

------------------------------------------------------------------------

# 49. Open Source Strategy and License

Orbit should be developed as a **public open-source project from the beginning**, unless there is a concrete short-lived reason to keep an early security-sensitive prototype private.

The default repository posture is:

```text
Repository visibility: Public
License:               Apache License 2.0
SPDX identifier:       Apache-2.0
```

## 49.1 Why Public

Orbit's intended identity is infrastructure rather than a proprietary application built around hidden workflow definitions.

A public repository supports the product goals:

- users can inspect the durability and failure semantics they are trusting;
- infrastructure engineers can audit the scheduler and recovery behavior;
- worker SDKs and protocols can develop in the open;
- agents and tool builders can target stable public contracts;
- integrations can be developed without requiring access to a private monorepo;
- architectural decisions and engine invariants can be reviewed publicly;
- the project can build an ecosystem around workers, SDKs, packages, and runtime adapters.

Public development also creates useful discipline: examples, tests, schemas, migrations, configuration, and documentation must be understandable without knowledge of an internal parent platform.

Orbit must never depend on secrets, private infrastructure names, private endpoints, customer data, or organization-specific configuration being committed to the repository.

If an experimental branch temporarily requires private material, keep that material in a separate private repository rather than making Orbit Core depend on it.

## 49.2 Why Apache License 2.0

Orbit should use the **Apache License, Version 2.0**.

Apache-2.0 is permissive: individuals and companies can use, modify, redistribute, embed, and commercially build on Orbit while complying with the license conditions.

For infrastructure software, Apache-2.0 is preferred over a minimal permissive license such as MIT because it also contains an explicit patent license from contributors.

This is valuable for a project expected to contain:

- a durable execution engine;
- scheduling algorithms;
- worker protocols;
- distributed coordination mechanisms;
- agent execution infrastructure;
- SDKs and runtime integrations.

The intention is to make Orbit easy to adopt in personal, research, startup, and enterprise environments without requiring downstream applications to adopt Orbit's license.

The project should **not** begin with GPL/AGPL-style copyleft unless the project's strategic goal changes toward requiring downstream modifications or network-hosted derivatives to remain open.

The initial philosophy is instead:

> **Make the execution substrate open, auditable, embeddable, and easy to adopt. Build ecosystem value around the open core rather than restricting use of the core.**

## 49.3 Repository License Files

The repository root should contain:

```text
LICENSE
NOTICE
README.md
CONTRIBUTING.md
SECURITY.md
```

`LICENSE` contains the unmodified Apache License 2.0 text.

`NOTICE` contains project-level attribution notices when required.

Source files may use the concise SPDX form:

```text
SPDX-License-Identifier: Apache-2.0
```

The exact copyright holder should be chosen deliberately before adding
copyright headers throughout the repository.

## 49.4 Contribution Policy

At the beginning, keep contribution mechanics simple.

Pull requests should be contributed under the repository's Apache-2.0 terms. Do not introduce a Contributor License Agreement merely because the project is open source.

If Orbit later develops substantial outside corporate contribution, a foundation model, dual licensing, or other requirements that make additional contributor agreements useful, revisit the policy explicitly.

Contributor provenance, dependency licenses, generated code, vendored code, and third-party assets must remain traceable.

## 49.5 Open Source Does Not Mean Uncontrolled Core

A permissive license does not require Orbit to accept every feature into the engine.

The project should maintain a strict architectural boundary:

```text
Orbit Core
    │
    ├── execution semantics
    ├── compiler
    ├── scheduler
    ├── journal
    ├── worker protocol
    └── core SDK/contracts

Orbit Ecosystem
    │
    ├── specialized workers
    ├── integrations
    ├── agents
    ├── packages
    ├── compute adapters
    └── optional services
```

Core changes require stronger review because they affect durability and compatibility guarantees.

The open-source model should encourage extension **around** a small trusted core rather than continuously expanding the core itself.

## 49.6 Commercial Future

Apache-2.0 leaves Orbit free to develop a commercial ecosystem later without changing the open-source identity of the core.

Potential commercial offerings could include:

- hosted Orbit;
- managed control plane;
- enterprise identity and governance;
- managed worker fleets;
- advanced observability;
- support;
- compliance tooling;
- enterprise integrations.

These are future product decisions, not requirements for the initial architecture.

The foundational commitment should remain clear:

> **Orbit Core is open infrastructure.**

# 50. Timescale Boundary: Fast to Continue, Not Fast to Twitch

Orbit is not a hard-real-time control system and must not become one in pursuit of low latency.

Its responsibility is **durable coordination between intent and execution**.

```text
Intelligence / Intent
Human · Agent · Application
          │
          ▼
Durable Coordination — ORBIT
Run · Step · Task · Attempt · Signal
Lease · Retry · Wait · Artifact · Policy
Cancellation · Recovery · Audit
          │
          ▼
Execution / Real-Time Control
ROS 2 · robot controllers · Linux · Blender
Kubernetes · GPU runtimes · device firmware
```

Orbit may coordinate a robot mission, but it does not control motor torque, stabilization,
trajectory control, collision avoidance, sensor fusion, or other hard-real-time loops.

Orbit may coordinate computer-use or GPU work without implementing those runtimes itself.

The memorable engineering rule is:

> **Fast to continue, not fast to twitch.**

Latency matters at the coordination boundary: worker completion should make dependent work ready
promptly; signals should resume waiting Runs promptly; cancellation should propagate promptly;
due timers should wake execution promptly; and expired worker leases should be recovered promptly.

It does not mean Orbit belongs in millisecond or sub-millisecond device control loops.

```text
microseconds / few milliseconds
    hard-real-time control              OUT OF SCOPE

tens / hundreds of milliseconds
    coordination reaction               IMPORTANT

seconds → minutes
    tools / agents / containers         CORE USE CASE

hours → days → weeks
    waits / approvals / observation     CORE USE CASE
```

Exact coordination SLOs must come from measurements. PostgreSQL remains the only required durable
store until measurements demonstrate that another component is necessary.

Do not add Redis, Kafka, NATS, or another broker merely to make Orbit appear fast.

## 50.1 The Worker Protocol Is a First-Class Pillar

Orbit has two foundational execution components:

```text
Durable Engine
truth + coordination
       │
       ▼
Worker Protocol
durable action boundary
       │
 ┌─────┼─────┬─────┐
 ▼     ▼     ▼     ▼
Agent Robot Linux Cloud
```

The engine establishes durable truth. The worker protocol defines how that truth safely causes
actions in the world.

The protocol should eventually define explicit semantics for:

```text
Task identity
Attempt identity
Stable idempotency key

Claim
Acknowledge
Heartbeat
Progress
Complete
Fail

Cancellation requested
Cancellation acknowledged

Deadline
Lease expiration

Artifact produced
External side-effect reference
```

A critical recovery case is an external action succeeding while its completion response is lost.
After reconnection or restart, Orbit should support reconciliation rather than assuming the
action never occurred and blindly repeating it.

Workers and integrations should associate durable Orbit task identity and idempotency identity
with external operations whenever the external system permits it.

This reinforces three permanent architectural principles:

> **Orbit owns coordination, not computation.**

> **Orbit owns durable action, not intelligence.**

> **Orbit coordinates real-time systems; it does not replace their control loops.**

The universality of Orbit should come from a small shared execution contract, not from putting
every capability into the engine.
