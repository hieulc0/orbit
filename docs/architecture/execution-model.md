# Execution model

- [Tasks, attempts and roles](#tasks-attempts-and-roles)
- [Persisted state machines](#persisted-state-machines)
- [Graphs and child execution](#graphs-and-child-execution)
- [Repository and OCI execution](#repository-and-oci-execution)
- [Developer-local tools and immutable skill flows](#developer-local-tools-and-immutable-skill-flows)
- [Delegated work](#delegated-work)

## Tasks, attempts and roles

### Execution state and ownership

The graph engine compiles Definition into an immutable Plan and stores each Run,
its Tasks, Attempts, accepted artifacts and ordered journal in PostgreSQL. Task
is a generic activity: deterministic commands, containers, waits and approvals
do not need agent records. Attempt represents a physical claim, lease and execution
environment. AgentExecution records a provider/runtime dispatch under an Attempt.
It does not own the workspace or decide task completion.

The role workflow coordinator stores workflow stages, bounded repair iterations,
role executions, handoffs, candidate identities and verification references.
The editor ACP service delegates progression to this coordinator. These interfaces
share the existing persistence and policy boundaries; an editor or provider
conversation cannot become a second execution authority.

A workspace belongs to an Attempt. Only one fenced mutating owner may use a mutable
workspace. Read-only analysis may inspect a candidate but cannot edit it. Parallel
writers require separate candidates and explicit integration before verification.
A role's external session is disposable; durable handoffs preserve the accepted
objective, candidate state and bounded findings without depending on chat history.

### Responsibilities

| Component | Authority |
| --- | --- |
| Planner | Proposes a bounded plan and findings under immutable policy |
| Implementer | Changes the candidate only while holding mutation ownership |
| Deterministic validator | Runs authorized checks in a pinned independent environment |
| Reviewer | Inspects the exact candidate read-only and returns a structured decision |
| Coordinator | Enforces stages, repair bounds, candidate identity and completion |
| External BA | Owns requirements, challenges and business acceptance |
| External SA | Owns technical proposals and resolutions; repository analysis is read-only |
| Operator | Selects credentials, profiles, policy and authorized apply/release actions |

Tester findings and reviewer prose are evidence, not direct success transitions.
Exploratory commands do not count as authoritative verification. A planner proposal
cannot add a credential, tool, mount, network route, isolation exception or budget.
Repository content, previous agent output and provider responses are untrusted inputs.

## Persisted state machines

Scope and recovery policies are defined in
[engine semantics](control-plane.md#identity-and-atomic-transitions). All transitions below are persisted with
journal events. Unlisted transitions are rejected. Terminal states are immutable.

This document describes v0. The v1 graph extends it with
`PENDING → WAITING → SUCCEEDED` for timers and signal waits,
`WAITING → FAILED` for signal deadlines, and direct `PENDING → SUCCEEDED`
for joins and waits with early receipts. Engine tasks have no attempts.
Graph success requires all tasks to succeed. See [graph execution](scheduler.md#graph-activation)
and [durable interaction](scheduler.md#timers-signals-and-durable-waiting) for v1 transitions and races.
Child/fan-out tasks also follow `PENDING → WAITING → SUCCEEDED`, with deadline
failure or child intervention/failure propagation. Empty fan-out can complete in
the same activation transaction. See [child-run execution](execution-model.md#graphs-and-child-execution).

### Run states

| From | To | Trigger |
| --- | --- | --- |
| — | `ACCEPTED` | Submission transaction commits |
| `ACCEPTED` | `RUNNING` | Reconciliation activates first task |
| `RUNNING` | `NEEDS_INTERVENTION` | A task needs intervention |
| `RUNNING` | `SUCCEEDED` | Both tasks succeeded |
| `RUNNING`, `NEEDS_INTERVENTION` | `FAILED` | A task fails terminally, including deadline exhaustion |
| `ACCEPTED`, `RUNNING`, `NEEDS_INTERVENTION` | `CANCEL_REQUESTED` | Cancellation commits |
| `CANCEL_REQUESTED` | `CANCELLED` | Remaining logical work is finalized |

`SUCCEEDED`, `FAILED`, and `CANCELLED` are terminal. Repeated cancellation on
`CANCEL_REQUESTED` or `CANCELLED` is idempotent. Cancellation of another terminal
state returns its existing outcome without rewriting it.

### Task states

Submission creates coding as `PENDING` and testing as `PENDING`. Activation makes
coding ready. Testing becomes ready only in the accepted coding completion
transaction while the run is still `RUNNING`.

| From | To | Trigger |
| --- | --- | --- |
| `PENDING` | `READY` | Run active and dependencies satisfied |
| `READY` | `CLAIMED` | Atomic claim creates attempt and lease |
| `CLAIMED` | `RUNNING` | Current owner acknowledges start |
| `CLAIMED`, `RUNNING` | `RETRY_SCHEDULED` | Recoverable failure/loss; policy permits another attempt |
| `CLAIMED`, `RUNNING` | `NEEDS_INTERVENTION` | Policy, uncertainty, or unavailable checkpoint prevents retry |
| `CLAIMED`, `RUNNING` | `FAILED` | Nonretryable failure, deadline, or attempt exhaustion |
| `RUNNING` | `SUCCEEDED` | Valid successful completion and artifacts accepted |
| `RETRY_SCHEDULED` | `READY` | Backoff due, run active, budget and deadline permit |
| `READY`, `RETRY_SCHEDULED`, `NEEDS_INTERVENTION` | `FAILED` | Previously established task deadline expires |
| `PENDING` | `SKIPPED` | Predecessor fails terminally |
| Any nonterminal state | `CANCELLED` | Run cancellation finalizes outstanding work |

`SUCCEEDED`, `FAILED`, `SKIPPED`, and `CANCELLED` are terminal. A predecessor in
`NEEDS_INTERVENTION` leaves its successor `PENDING`. No claim occurs while the run
is paused or cancellation has been requested. A failed coding task skips testing
and fails the run in the same transaction. A failed testing task fails the run.

### Attempt states

| From | To | Trigger |
| --- | --- | --- |
| — | `CLAIMED` | Claim commits |
| `CLAIMED` | `RUNNING` | Start acknowledgement commits |
| `RUNNING` | `SUCCEEDED` | Successful completion accepted |
| `CLAIMED`, `RUNNING` | `FAILED` | Worker reports failure or task deadline expires |
| `CLAIMED`, `RUNNING` | `LOST` | Lease expires and recovery transaction commits |
| `CLAIMED`, `RUNNING` | `CANCELLED` | Run cancellation finalizes attempt |

All four outcome states are terminal. An attempt that expires but has not yet
been marked `LOST` still cannot send authoritative messages. Every new claim
creates a new attempt, including checkpoint continuation. An attempt is never
reset to `READY` or transferred to a different worker.

### Role workflows and external acceptance

Role workflows use their own fenced coordinator stages, separate from the graph
run/task states above. Skill flows pin immutable policy; read-only analysis can
complete from PLANNING only with a matching successful handoff and unchanged
candidate. Mutation flows retain implementation, verification, review, repair
and regression. See [interactive execution](execution-model.md#developer-local-tools-and-immutable-skill-flows).

A frozen external contract changes the final transition to
`REGRESSION → BUSINESS_ACCEPTANCE → COMPLETED`. Technical evidence is checked
before entering BUSINESS_ACCEPTANCE. BA attestation must bind the exact frozen
contract, all criteria and the reviewed/verified WorkspaceState. The coordinator
rechecks disk identity and completion evidence before COMPLETED. Cancellation
and failure remain available from nonterminal stages; attestation does not grant
mutation authority. See [external reasoning](../operations/installation.md#external-requirements-and-ba-acceptance).

## Graphs and child execution

Bounded durable interaction includes child runs and fan-out alongside
[graphs](scheduler.md#graph-activation) and [timers/signals](scheduler.md#timers-signals-and-durable-waiting).

### Pinned child definitions

`engine.child` requires an inline `definition`. Its dependencies must succeed
before it starts. The accepted parent's digest covers the entire nested definition
and repository binding. Child definitions use the same repository identifier and
frozen server binding as their parent, with their own full base revision and task
input. Compilation validates all nested commands against the binding's allowlist.
There is no mutable registry lookup or external definition fetch during execution.

The engine creates exactly one child run, recording its ID on the parent task
and recording `parent_run_id`, `parent_task_id`, and `root_run_id` on the child.
Both sides and their journal entries commit atomically. Reconciliation reuses this
link after restart; it does not resubmit the definition or create another child.

An operator submission's existing `parent_run_id` flag remains a recovery/history
reference. It does not create a managed child, inherit cancellation, or bypass
admission. Only the engine creates managed `parent_task_id` links.

The parent task remains `WAITING` until its child succeeds, then releases its
dependents. `timeout_seconds` covers the whole child operation from dependency
release through observed completion, including queueing and descendant work.
It never resets. Parent coordination consumes no worker attempts; the common
recovery, max-attempt and backoff fields are validated but unused there. Worker
tasks inside each child retain their ordinary retry and fencing policies.

### Bounded fan-out and join

`engine.fan_out` requires an inline child `definition` and `fan_out` settings:

```yaml
fan_out:
  max_items: 4
  max_parallel: 2
  items: [first task, second task]
```

Alternatively, use `signal_from: items` in place of `items`. The named source
must be a direct dependency with capability `engine.wait`, and its signal payload
must be a JSON array of strings. This permits the item count and contents to arrive
at runtime. The engine snapshots and validates the whole array before creating
any children. An invalid payload fails the fan-out task and run with a reason;
signal acceptance alone does not imply valid fan-out input.

Each item replaces only `inputs.task` in the inline child template. Other inputs,
commands, limits, and nested definitions stay pinned. Items are literal strings,
not expressions or shell substitutions. Each child starts from its own frozen
base revision; patches are not combined or applied to another child's workspace.

| Bound | Contract |
| --- | --- |
| `max_items` | Required integer from 1 to 64 |
| `max_parallel` | Required integer from 1 to `max_items`; counts nonterminal child runs, including waiting children |
| Item | Nonempty after trimming, at most 16384 UTF-8 bytes; duplicate text is allowed |
| Signal payload | Existing 16 KiB serialized payload limit also applies |
| Graph size | 1–256 steps per definition |
| Nested depth | At most four child-definition levels below the root |
| Execution tree | At most 256 runs, including the root, using declared `max_items` recursively as the worst case |

Static lists are validated before submission. Dynamic lists cannot exceed their
declared bound. The conservative tree bound applies even if a literal list is
shorter than `max_items`. These limits are checked before accepting the root and
cannot be bypassed by nested templates.

The parent task stores `expansion` and an ordered `child_run_ids` prefix. Index
zero is the first input; equal strings at different indices get distinct runs.
Each child's task and attempt IDs follow the ordinary identity contract. The
engine fills free slots in order and starts no more than `max_parallel` children
at once. An empty list succeeds without creating a child. The fan-out step joins
all children: it succeeds only when every item has produced a successful child.
An `engine.join` may combine this result with other graph dependencies.

## Repository and OCI execution

`orbit/v1` supports `container.run`, resource requirements and worker placement.
Definitions containing only compute or engine steps need only `inputs.task`;
repository steps still require a repository binding and a full Git revision.
Existing v0 definitions retain their validation and serialized plan digests.

### Repository execution requirements

`repository.code` and `repository.test` may opt into `execution` requirements:
`isolation: trusted`, `network: none`, `filesystem: workspace`, with positive
CPU/memory in the existing `resources` fields and no GPU. Private remote bindings
and coding-agent steps require this containment. The server's `execution_profiles`
maps the logical class to a digest-pinned `rootless_podman` image; the selected
profile enters the plan digest. The worker checks its own exact profile and
repository allowlists. Missing profiles and unsupported `sandboxed`/`untrusted`
classes fail closed, without fallback. Structural definition validation alone
does not establish operator profile availability; submission compiles the binding.

Orbit runs tools in disposable OCI containers on the host's
rootless Podman, not Docker-in-Docker. The trusted worker handles Git/model network
access outside the tool container. It publishes an attempt/plan/profile/resource
bound `execution_report`; successful completion checks its provenance. See the
[remote coding guide](workers.md#trusted-worker-isolation) for setup, credential authority,
mounts, threat model and recovery. This field is deliberately limited to repository
execution, not a new general runtime plugin API. Existing `container.run` semantics
below and legacy repository commands without this field remain unchanged.

Future operator mappings may use gVisor/runsc for `sandboxed` and Firecracker for
`untrusted`, only after their required threat model is defined and qualified. These
are conditional isolation integrations, separate from tenant identity management.

### Compute contract

See [the executable container definition](../../examples/container.yaml). Each
container has an OCI image pinned by SHA-256 digest, an argv command, explicit
CPU (`cpu_millis`), memory (`memory_mib`) and optional GPU count. The operator
must provision that image on a Linux Docker or rootless Podman runner; Orbit
uses `--pull=never`. `ORBIT_CONTAINER_RUNTIME` selects `docker` (default) or
`podman` for the worker or `execute-local`. Podman uses a user namespace that
preserves the worker UID/GID and the cgroupfs manager; its host must delegate
CPU, memory and PID controllers to the worker's user. Provisioned images live in
the operator's runtime image store, outside attempt workspaces.
The runner executes the requested command as the host worker's UID/GID, with
no network, a read-only root filesystem, dropped capabilities, no new
privileges, a bounded tmpfs, a PID limit and CPU/memory limits. Definitions
cannot add host mounts, privileged flags, Docker endpoints or environment secrets.
The OCI runtime remains trusted host infrastructure; this does not qualify a hostile-code
or multi-tenant sandbox.

Direct dependency artifacts are mounted read-only at `/orbit/inputs/<artifact-id>`;
`/orbit/inputs/manifest.json` describes them. The optional regular file
`/orbit/outputs/result` becomes a `data` artifact. A result is limited to 32 MiB.
Symlinks and special files are rejected. Logs and a provenance-bearing
`container_report` are required for a successful completion. Repository testing
continues to receive only its direct coding dependency's artifacts.

The task ID is also the stable idempotency key. Containers receive
`ORBIT_TASK_ID`, `ORBIT_ATTEMPT_ID`, `ORBIT_ATTEMPT_GENERATION` and
`ORBIT_IDEMPOTENCY_KEY`. Attempts use
distinct container names and workspaces. The worker maintains leases while
running and publishing outputs. A separate supervisor owns runtime cleanup;
closing its input pipe after cancellation, lease loss or worker SIGKILL triggers
cleanup. Task timeout also triggers cleanup. Removal retries are bounded. A
failed or unreachable runtime can leave stopping unconfirmed; inspect
the `orbit-<attempt-id>` container in that case. Orbit does not equate revoked
durable ownership with proof that an external process has stopped.

## Developer-local tools and immutable skill flows

Orbit owns repository tools, role permissions, mutation ownership, workflow
state and verification. A provider prompt does not grant host access.

### Developer-local role execution

Linux operators can select a developer-local terminal profile when creating a
trusted interactive workflow:

```json
{"profile":"dev_local","bubblewrap":"/usr/bin/bwrap"}
```

Pass this JSON file using `orbit workflow start --agent-execution-profile FILE`
alongside the required `--verification-environment FILE` and pinned verification
policies. Orbit stores the profile before execution; it cannot be overwritten
or changed while a step owns the workflow. Resuming loads the stored profile.
Omission retains the existing trusted role execution with terminals disabled.
Existing plan and role digests do not change.

Files, search and Git inspection use the existing confined native callbacks.
Only an implementer holding the durable workspace mutation lock can run a local
terminal. Bubblewrap creates fresh mount, PID, user and network namespaces with
read-only system tools, a writable candidate workspace, read-only `.git`, fresh
temporary storage and a synthetic home. Host home directories, unrelated
repositories, private Orbit credentials and container sockets are not mounted.
Environment inheritance is disabled. No additional host grants are implemented.
Missing bubblewrap or unavailable namespaces cause failure; there is no host
execution fallback. Tool processes and their descendants are stopped before
role cleanup can be confirmed.

Git metadata in a linked worktree points outside the terminal mount. Use Orbit's
native Git callbacks for inspection. Repository tools still have no network;
dependency installation requires operator-provisioned tools or separate policy.
The terminal sees system tools under `/usr`, `/bin`, `/sbin`, `/lib` and
`/lib64`. A compiler installed only in the operator home is unavailable; provision
required system tools before selecting this profile. Exploratory checks must not
add home-directory mounts to make a build pass.

This profile is for trusted interactive repositories, not hostile workloads.
CPU time, open files, output and file sizes are bounded; the local terminal does
not provide the OCI verifier's memory or process-count cgroup limits.
Untrusted execution remains unsupported.

The provider process remains in its existing supervised OCI runtime. Codex
negotiates an atomic shell extension for exactly one provider invocation and
one audited callback. Existing worker clients retain their terminal protocol.
Final workflow verification uses the separately pinned rootless OCI environment;
developer-local terminal results are exploratory evidence.

### Role resources

See [accounting and resource evidence](../reference/api.md#resource-accounting) for the
difference between role ceilings, graph-worker reservations and provider usage.

Production CLI workflow executions use these independent resource ceilings:

| Resource | Planner | Implementer | Reviewer |
| --- | ---: | ---: | ---: |
| Total repository callbacks | 150 | 300 | 150 |
| Mutating callbacks | 0 | 200 | 0 |
| Terminal creations | 0 | 40 | 0 |
| File read bytes | 8 MiB | 16 MiB | 8 MiB |
| Serialized callback output | 8 MiB | 8 MiB | 8 MiB |

The agent execution metadata records limits, usage and exhaustion. Running
callback audit snapshots also contain resource usage. These ceilings are
separate from provider token/cost accounting and from individual tool bounds.
Denied and unsupported requests consume the call budget. A bounded final budget
diagnostic is reserved from the output ceiling. Budget exhaustion ends the role
with `TOOL_BUDGET_EXHAUSTED`; it is not a provider failure or an unlimited retry.
Automatic continuation into another agent execution is not implemented.

Read results retain line paging and expose `bytes_returned`, `total_size`,
`truncated` and `next_offset` under `_meta.orbit`. Use `offset` and `max_bytes`
for negotiated byte reads, mutually exclusive with `line` and `limit`. Continue
at the returned offset. Byte pages preserve UTF-8 boundaries and work for very
long lines. Prefer search followed by a targeted read. Line reads currently
charge the complete file bytes inspected; byte reads charge their bounded page.
Failed byte reads conservatively retain their reserved read bytes. Legacy
worker clients reject unsupported byte reads rather than treating them as full
file reads. Callback evidence retains up to 1,024 rows; text diagnostics display
at most 64 rows.

Terminal processes additionally have a 64 MiB file-size ceiling, 256 open file
descriptors and 300 CPU seconds. Managed candidates admit at most 8,192 files,
64 MiB per file and 256 MiB in total, excluding Git-ignored build outputs.
Native coordinator Git capture has byte and time limits. These bounds do not
provide hostile-workload memory or CPU isolation for the whole service.

### Skill flows

`workflow start --skill investigate` pins a read-only analysis flow. Investigation,
review, release preparation and security review use a planner inspection and a
structured handoff; they do not claim implementation review or verification.
A successful handoff must match the unchanged candidate before analysis completes.

Fix bug, implement feature, refactor and dependency update select PLAN, IMPLEMENT,
FAST, STANDARD, REVIEW and FULL. Documentation with explicit `--risk low` may use
FAST, REVIEW and final FAST when every changed path is documentation Markdown.
Changes to code, manifests, configuration, `AGENTS.md`, or an empty change set
escalate to STANDARD and FULL. An explicitly pinned regression policy may require
stronger checks. Flow identity and policy are immutable after workflow creation.

The [editor service](../operations/installation.md#editor-acp-service-and-zed) uses these flows and the same coordinator.
The [external reasoning interface](../operations/installation.md#external-requirements-and-ba-acceptance) adds frozen requirements
and candidate-bound BA acceptance.

## Delegated work

### Controlled delegation

An agent may propose at most its configured number of nonempty work-item strings
(16 KiB each). A downstream `engine.fan_out` uses `agent_from: planner` and an
inline pinned child definition. It must depend directly on that agent. The
fan-out limit must cover the agent's bound and all existing tree-size,
parallelism, deadline, cancellation and child-outcome rules apply. The agent
chooses inputs, not executable child definitions, bindings or new permissions.
A human approval dependency can gate admission of those children. Ephemeral
internal subagents remain the trusted runtime's responsibility and must share
the containing task's reservation budget.
