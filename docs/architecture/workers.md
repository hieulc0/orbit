# Workers

- [Worker ownership and runtime lifecycle](#worker-ownership-and-runtime-lifecycle)
- [Trusted worker isolation](#trusted-worker-isolation)
- [Provider and repository process separation](#provider-and-repository-process-separation)
- [Drain and shutdown](#drain-and-shutdown)

## Worker ownership and runtime lifecycle

This document defines the implemented transport-independent operation contracts,
exposed through HTTP and the Rust/Python SDKs.
See [engine semantics](control-plane.md#identity-and-atomic-transitions) and [state machines](execution-model.md#persisted-state-machines).

Worker draining is an additive operational control: existing accepted claims,
heartbeats and completion remain valid while new claims stop. Registration never
clears a durable drain flag. See [lifecycle and authorization](workers.md#drain-and-shutdown).

The v0 wire protocol includes `container.run`, optional
`gpu_devices` in assignments, provider/object-key metadata on artifacts, and
`data`/`container_report` artifact kinds. Server-authorized capacity and pool
membership constrain claims; clients cannot supply capacity overrides. See
[compute and artifacts](execution-model.md#repository-and-oci-execution) for the additional contract.

### Workspace lifecycle

1. Allocate an attempt-specific directory or worktree from the assigned ID.
2. Materialize exactly `base_revision`, verifying the full revision locally.
3. Record workspace identity and attempt metadata before starting commands.
4. For testing, retrieve and checksum the accepted patch and apply it to the clean
   base. For checkpoint continuation, validate the runtime checkpoint and restore
   it into this new workspace under the runtime's documented contract.
5. Execute only the assigned task within configured filesystem, process, network,
   credential, and resource limits.
6. Preserve outputs, upload artifacts, and submit completion.

Workers MUST NOT infer a successful prior attempt from leftover directories.
Abandoned workspaces remain distinguishable from active work. They are retained
for qualification and accessible through a documented manual recovery
procedure. A later cleanup feature must account for ownership and retention.

The coding worker may use an agent runtime, but the engine does not interpret its
conversation or tool loop. A restart begins from the immutable task inputs; a
checkpoint resume requires an explicitly advertised runtime format. Persisted
logs are not automatically a resumable checkpoint.

Explicit repository `execution` requirements add pinned `plan.execution_profiles`
and require server-authorized `execution.podman-v1` capability. Private remote Git
bindings carry only an approved URL and logical credential reference; the worker
resolves a local purpose/binding/audience grant. Workspaces and tool containers are
attempt-owned; credentials and host Git metadata are never mounted. See the
[remote coding guide](workers.md#trusted-worker-isolation). A successful isolated repository
attempt additionally requires `execution_report` provenance matching its plan,
requirements, selected profile and resources. Existing fields and old plans retain
their semantics and serialized digests.

Isolated coding reservations carry attempt-prefixed call IDs and `request_digest`.
The result is acknowledged separately through `finish_agent_call`. Unresolved
model dispatch prevents automatic retry; terminal deadline/cancellation retains
uncertainty. See [agent accounting](../reference/api.md#tracked-coding-invocations) for replay,
receipt and fresh-workspace recovery rules. Neither an accepted reservation nor a
replayed receipt authorizes repeating the provider effect.

### Failure contract

A failure contains `category`, machine-readable `code`, readable `message`,
optional diagnostic artifact IDs, and `side_effect_status`:

| Category | Typical meaning |
| --- | --- |
| `task_failure` | Assertions failed, invalid patch, or bounded task could not be completed |
| `infrastructure_failure` | Runtime unavailable, transient storage or process failure |
| `uncertain_outcome` | An external action may have happened without a confirmed result |

`side_effect_status` is `none`, `confirmed`, or `unknown`. Unknown external effects
require intervention unless the task must terminate for budget/deadline reasons,
in which case unresolved uncertainty remains in its failure record. The server
applies recovery policy and limits; a worker's retry suggestion cannot override
them. Unknown failure codes default to intervention, not automatic retry.

For the initial restricted repository workers, local edits are attempt-owned
outputs and may be discarded by restarting in a fresh workspace. External writes
are not permitted. A future worker allowing external writes needs an explicit
idempotency/reconciliation contract before automatic retry is safe.

### Checkpoints and cancellation

A checkpoint records producing attempt, input/plan digest, runtime identity,
format/version, and immutable artifact reference. Only a current lease can publish
one. A checkpoint reference is not permission to reuse its workspace. It may
contain sensitive context and uses the same scoped artifact access rules.

Workers learn cancellation through heartbeat responses or `get_attempt`; a push
channel is optional. They stop child processes and may report stopping as a
diagnostic acknowledgement. Cancellation can finalize logical state before this
acknowledgement arrives. Such acknowledgement MUST NOT reopen an attempt or
replace its terminal result. If process termination is unconfirmed, inspection
must say so explicitly.

## Trusted worker isolation

### Topology and isolation policy

Run the API in its existing server image or on a host. Run the worker as a dedicated
non-root Linux host user with Git and local rootless Podman. It launches task OCI
containers using the host runtime; it does not need Docker-in-Docker. Never mount
a runtime socket into the API, a repository workspace or a tool container. A
containerized worker that controls a host runtime is a different deployment
requiring deliberate path/identity mapping and qualification; it is not implemented
by mounting the socket into the server image.

The trusted worker performs Git authentication and model HTTP calls. Only the
Attempt repository is mounted read/write at `/workspace` in tool containers.
The Attempt contains its own normal Git metadata, while host Git configuration,
host HOME, Orbit/lease credentials, model/Git secrets, and runtime sockets remain
outside that mount. Containers have no network, a read-only image
root, dropped capabilities, no new privileges, PID/CPU/memory limits, and a bounded
writable `/tmp`. Filesystem `workspace` describes the writable repository boundary;
tools can still read their image's files and use its scratch tmpfs. Rootless OCI
shares the host kernel and is not a hostile-code security claim.

Keep task requirements logical:

```yaml
resources: {cpu_millis: 2000, memory_mib: 4096}
execution:
  isolation: trusted
  network: none
  filesystem: workspace
```

Reuse the existing `resources` fields, rather than adding competing CPU/memory
syntax. The server's `execution_profiles.trusted` selects `rootless_podman` plus
a digest-pinned image. The referenced profile enters the immutable plan. Workers
must also allow that exact profile and repository ID locally. `sandboxed` and
`untrusted` are reserved requirement values: submission rejects them because no
backend is implemented. There is no fallback or task-level `runtime` selector.
The initial `execution` field applies only to `orbit/v1` repository code/test steps;
it is not a generic runtime plugin API. Existing `container.run` and workflows
without this field retain their existing runtime behavior and digests.

Align existing worker pools and `placement` with repository/image availability.
The execution capability is not an inventory of locally provisioned images or
credentials. A worker with a mismatched local allowlist rejects its assignment
before execution; it does not silently change the selected image or credential.

## Provider and repository process separation

### What the agent and tools can access

The agent runs in a separate read-only OCI image, with only a fresh private
control HOME mounted. It receives only the selected auth files in that HOME; no repository, worker
token, container socket or developer HOME is mounted. The broker accesses an
isolated Attempt repository with normal Git metadata, detached at the pinned
baseline. The ACP-visible
cwd is the virtual path `/orbit/home/workspace`. Client file/terminal requests under
that virtual root map only to the actual Attempt repository; host paths are not
exposed as a second namespace.

`launch.network: none` is appropriate for pure offline ACP fixtures. `host` gives
the agent host-network connectivity, **not provider-only egress**. A live provider
requires explicit network policy on a dedicated host; local services and egress
must be protected separately. The task's `execution.network: none` always applies
to repository command containers, not to provider communication.

Agent and terminal containers each receive at most half of the coding step's
CPU/memory allocation. The configured agent limits must fit that half; the terminal
uses the other half. The example reserves 2 CPUs/1 GiB, split into 1 CPU/512 MiB
per container. Containers also have a read-only root, dropped capabilities,
no-new-privileges, 128 PID limit and bounded temporary storage. This remains the
existing **trusted** isolation class, not hostile-code or provider-only isolation.

#### Workspace capacity

Coding worker `--workspaces` roots must be dedicated, disk-backed storage with an
operator-enforced bounded allocation of at least 12 GiB per active Rust attempt.
The repository, Cargo `target/` data, temporary files, logs and retained review
artifacts all count against that allocation. Do not place coding workspaces on a
small `/tmp` tmpfs: a normal Orbit Rust validation can require several GiB even
when the container writable layer is read-only. After evidence export and review,
apply the operator's retention policy to reclaim the attempt directory; the
worker does not silently delete workspaces needed for recovery or qualification.

Filesystem callbacks reject traversal, symlinks, mount crossings, hard links,
devices and FIFOs using directory-relative Linux `openat2`. Reads/writes are UTF-8
and bounded to 64 KiB. Reads accept positive one-based line/limit values. Writes
preserve an existing file's executable mode and require an existing parent.
Use a brokered shell command to create directories. No file callback is permitted
while a terminal remains unreleased.

One terminal may be live. Its command and argv are bounded; agent-supplied
environment variables are rejected. `create` returns a handle while execution
continues; `output`, `wait_for_exit`, `kill`, and `release` check session ownership.
Output retains a UTF-8-safe tail, capped at 64 KiB, while counting all produced
bytes. Overflow stops the invocation. A failed test exit is returned to the agent
for revision, not treated as successful verification. Native permission requests,
extensions, external MCP, delegation, interactive auth and automatic resume are
not supported. A denied callback fails the turn; it never authorizes native effects.

An existing Attempt directory is reclaimable after a worker restart only when its
persisted identity marker and Git `HEAD` still match the assignment. A mismatch
fails closed. Reclaiming an assignment preserves its Attempt repository and
uncommitted changes. The [continuation contracts](failure-recovery.md#continuation-and-provider-fallback) describe stored
handoffs and pure recovery decisions; automatic continuation into another coding
execution is not implemented.
Each Attempt uses a `--no-local --no-hardlinks` clone, so cleanup removes the
Attempt directory and its private `.git` together; it does not create shared
`.git/worktrees` entries in the operator checkout. Retention remains governed by
the existing evidence policy.

### Accounting, evidence and recovery

The [submit, inspect and review workflow](../operations/installation.md#submit-inspect-and-review-a-candidate) applies to ACP too.
`orbit export-run` collects accepted patch, transcript and test artifacts through
the operator API. The export keeps unknown accounting values as `null` and does
not copy worker auth stores or workspaces.

The [accounting reference](../reference/api.md#resource-accounting) distinguishes
reservations, callbacks, reported tool IDs and provider usage. Reserve one prompt
before dispatch and every broker effect before execution.
Calls, prompt count, broker count and worst-case terminal duration remain charged
across retries. One ACP prompt may contain many provider exchanges. Token and
monetary values are explicitly `null`; stable-v1 usage extensions are not enabled,
and reported context occupancy is not treated as billed usage.

Ordered metadata-only session batches go through `record_acp_session` under the
same lease/generation fencing. The engine checks sequence, replay digest and
cumulative output/reported-tool limits. Logs contain those accepted batches,
not raw reasoning, model text, file contents, commands or credentials. The final
report includes identity, launch pin, model attribution, stop reason and cleanup
status; completion checks its session totals and transcript against the ledger.
Patch/manifest and execution reports use the existing artifact boundary. Independent
testing starts from the pinned base plus the accepted patch. Only an authorized
human can approve the review step.

On worker death or lease loss, closing stdio stops the independent agent supervisor;
the existing workspace supervisor separately stops tools. A success receipt is
written only after confirmed container removal. The worker cannot accept a patch
from an unconfirmed cleanup. A pending prompt stays externally uncertain and blocks
automatic retry even if local containers were removed. Killing a local process
does not prove a remote provider stopped computation or billing.
If the ACP turn deadline expires, the bounded failure log records the session
identity, elapsed time, last protocol activity kind/time, pending model/tool state,
broker poison state, active terminal count and runtime state observed at the
deadline. It does not record prompts, credentials or raw tool payloads.

The selected auth store has an exclusive file lock and a durable
`.orbit-acp-active.json` marker. After confirmed container removal, approved refresh
files are atomically copied back, staged credential copies are cleared and the
marker is removed. Invalid refresh files or uncertain cleanup leave the marker
in place and deny reuse. Other private agent-created state is retained and may
contain sensitive information; never export the control HOME.

For a quarantined store, drain its worker, inspect the marker's exact container
and attempt, confirm all relevant containers are stopped/removed, and inspect
the private refresh state before operator recovery. Do not delete the auth store,
clear markers automatically, restart an uncertain prompt or share one auth store
across hosts. A host/supervisor crash needs this explicit recovery review.

## Drain and shutdown

```sh
ORBIT_TOKEN_FILE=/absolute/private/operator.token orbit drain-worker compute
ORBIT_TOKEN_FILE=/absolute/private/operator.token orbit workers
# After maintenance:
ORBIT_TOKEN_FILE=/absolute/private/operator.token orbit drain-worker compute --resume
```

`POST /workers/{id}/drain` accepts `{"draining":true}` or false and requires
global `worker.write`. Unknown workers are rejected. PostgreSQL retains drain
across server restart and registration. Drain stops new claims, not active leases
or accepted claim retransmissions. An idle drained worker continues polling.
Inspect reservations before stopping it; give separate processes separate identities.

SIGTERM/Ctrl-C stops local worker claims. Active work continues heartbeating and
may finish within `--shutdown-grace-seconds` (default 30). At the deadline, owned
process groups stop or the OCI supervisor lifeline closes. No success is fabricated:
lease expiry and recovery policy decide the durable outcome. Dispatched external
effects can remain uncertain. Existing retry exhaustion takes precedence over
intervention when no attempts remain.

On shutdown the server becomes unready and stops accepting connections.
Requests drain within the same configurable 30-second default, including a bound
on open streams. The reconciler stops when this bounded drain ends. Server
shutdown does not cancel runs. Allow at least 40 seconds
before service-manager forced termination. Drain workers first when maintenance
outlasts confirmed leases; see [upgrades](../operations/upgrades.md#coordinated-upgrades-and-credential-rotation).
