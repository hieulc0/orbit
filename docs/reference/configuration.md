# Configuration reference

- [Definition versions and repository fields](#definition-versions-and-repository-fields)
- [Graph definition fields](#graph-definition-fields)
- [Agent bindings](#agent-bindings)
- [Scoped authorization and secret references](#scoped-authorization-and-secret-references)
- [ACP registry and launch policy](#acp-registry-and-launch-policy)
- [Supported runtime boundaries and pins](#supported-runtime-boundaries-and-pins)

## Definition versions and repository fields

### Definition and accepted run

The v0 repository convention is `.orbit/definitions/implement.yaml`. Its
logical contract MUST contain:

| Field | Requirement |
| --- | --- |
| `apiVersion` | `orbit/v0`; reject unknown versions |
| `kind` | `Definition` |
| `metadata.name` | Nonempty definition name |
| inputs | Repository identifier, full immutable Git commit ID, bounded task text |
| coding step | Capability `repository.code`, task deadline, recovery policy, maximum attempts |
| testing step | Capability `repository.test`, dependency on coding, same limits and recovery policy |
| test commands | Explicit argument arrays, working directory relative to workspace, per-command timeout |

The v0 shape is illustrated below. The revision value is a placeholder that MUST
be replaced before submission. Inputs are literal values in `orbit/v0`;
there is no expression language or implicit environment-variable interpolation.

```yaml
apiVersion: orbit/v0
kind: Definition
metadata:
  name: implement
inputs:
  repository_id: orbit
  base_revision: REPLACE_WITH_FULL_COMMIT_ID
  task: Fix the bounded bug described in the qualification fixture.
steps:
  code:
    uses: repository.code
    recovery_policy: restart_from_inputs
    max_attempts: 3
    timeout_seconds: 1800
    retry_backoff_seconds: 5
  test:
    uses: repository.test
    needs: [code]
    recovery_policy: restart_from_inputs
    max_attempts: 2
    timeout_seconds: 600
    retry_backoff_seconds: 5
    commands:
      - argv: [cargo, test, --locked]
        cwd: .
        timeout_seconds: 300
```

All illustrated fields are required except `needs`, which is prohibited on `code`
and must be exactly `[code]` on `test`. The v0 schema accepts exactly these two
step IDs and capabilities. Unknown fields are rejected. Attempt and timeout
values are positive integers; backoff is a nonnegative integer. Commands are a
nonempty list with nonempty string argument arrays and workspace-relative paths
that cannot escape the workspace. Commands run sequentially and stop at the first
failure. A command timeout is capped by the remaining task deadline.

The repository identifier resolves through server-controlled bindings frozen in
the plan; credentials are supplied at execution time through scoped references,
not copied into the plan. Coding receives the repository inputs. Testing receives
the base revision and the accepted coding patch/manifest automatically through
this fixed dependency contract. No user-authored artifact expression is needed.
Shell interpretation is not implicit. An explicitly requested shell command
remains subject to the worker's configured execution policy.

Compilation validates the two-step acyclic dependency, input and output contracts,
capability bindings, positive limits, and configured command permissions. It
resolves configuration into an immutable plan. A run references that exact plan,
including its digest. Mutable branches MUST NOT stand in for `base_revision`.
Changing source files or configuration MUST NOT change an accepted run.

Submission supplies a client-generated idempotency key. In one PostgreSQL
transaction, Orbit stores the plan reference, run, tasks, initial states, and
journal entries. It acknowledges acceptance only after commit. Repeating the
same key and payload returns the original run; a different payload conflicts.
This permits recovery from a lost submission response without duplicate runs.

## Graph definition fields

### Definitions and inputs

A graph contains 1–256 statically declared steps. Step IDs contain 1–128 ASCII
letters, digits, underscores, or hyphens. Dependencies must reference distinct
existing steps; self dependencies and cycles are rejected. Definition inputs and
repository bindings are required for repository steps; coordination and compute-only
v1 graphs need only `inputs.task`. Unknown fields and capabilities are rejected.
Plans remain immutable and digest-protected. Later compute, agent and approval
capabilities are documented in [compute](../architecture/execution-model.md#repository-and-oci-execution) and [agents](../architecture/execution-model.md#delegated-work).

Supported capabilities:

| Capability | Contract |
| --- | --- |
| `repository.code` | Runs the bound coding command against the original revision; produces patch and manifest |
| `repository.test` | Has exactly one direct coding dependency; applies its accepted patch in a fresh workspace and runs its own allowed commands |
| `engine.join` | Requires at least one dependency; succeeds when all dependencies succeed, without a worker or artifacts |
| `engine.timer` | Waits for a persisted delay after dependencies succeed; requires `delay_seconds` |
| `engine.wait` | Waits for one operator signal; `timeout_seconds` starts after dependencies succeed |
| `engine.child` | Executes one pinned inline definition and waits for its outcome |
| `engine.fan_out` | Executes bounded parallel children from literal or signaled task inputs and joins their outcomes |

All steps retain required recovery policy, attempt, timeout, and backoff fields.
Joins validate those fields for format consistency but consume no attempts and
have no independent deadline. Commands are prohibited on coding and engine steps.
Testing commands are checked against the server binding for every testing step.
See [durable interaction](../architecture/scheduler.md#timers-signals-and-durable-waiting) for timer durations, wait
deadlines, signals, early delivery, and cancellation semantics.
See [child-run and scheduling semantics](../architecture/execution-model.md#graphs-and-child-execution) for child templates and shared limits.

Coding dependencies control ordering only: each coding step starts from the
original base revision, not an upstream patch. Tests may have additional testing
or engine dependencies as ordering gates. Only the one direct coding dependency
supplies artifacts. Joins do not combine patches or forward artifacts.

## Agent bindings

The built-in provider-neutral single-call worker is described in the
[command-agent guide](../operations/installation.md#command-agent-setup). The contracts below also serve
external runtimes that reserve each call through the SDK.
The [remote coding guide](../architecture/workers.md#trusted-worker-isolation) covers the built-in multi-turn
Responses adapter: an `agent` on an explicitly isolated `repository.code` step.
It uses private Git, OCI tools, independent testing and existing human approval.

`orbit/v1` supports `agent.run` and `human.approval`. See
[the executable definition](../../examples/agent.yaml) and
[fixture bindings](../../examples/agent-bindings.json). Agents are external trusted
worker runtimes using the existing Rust or Python transport SDK. Orbit does not
run a model/tool loop in its scheduler or silently select a paid provider.

### Pinned bindings and authority

The server's `agent_bindings` maps a name to a model revision, additional runtime
capability, tool implementation revisions, allowed permissions, maximum budget
and maximum delegation count. Definitions request an identity, binding, subset
of tools/permissions, budget, context (16 KiB maximum) and output type (`json`,
`object`, `array`, or `string`). Submission rejects requests exceeding the binding.
Only referenced bindings, including nested child bindings, enter the immutable
plan digest. Existing plans without agent bindings keep their original digests.

A worker must be authorized for the step capability (`agent.run` or isolated
`repository.code`) and the binding's runtime capability. Isolated repository
steps additionally require `execution.podman-v1`. Model/tool revision strings are operator assertions: the trusted
runtime must resolve and verify the actual implementation. No provider keys are
embedded in bindings, assignments, reports or the engine database. Provision
provider credentials separately at the trusted runtime. Permissions are a
checked runtime contract, not an OS sandbox or a provider-side spending limit.

## Scoped authorization and secret references

This document describes implemented, optional governance capabilities. Existing
scopes, roles, principal types, policies and checks remain supported. Expanding
tenant hierarchy, identity administration or SSO is outside the current remote
repository workflow, which must also work with governance disabled.

### Ownership and credential scope

Run attribution comes from authenticated submission. Execution authority comes
from server-authorized worker admission and the current attempt lease. Artifact
access follows resource and dependency authorization. Attribution alone is not
an access grant.

An attempt-scoped credential means authorized use of a logical credential binding
for a particular task or attempt; it does not require a tenant hierarchy. Existing
configured scope restrictions continue to apply. File/environment references are
credential sources, not a general lease/revocation service.

Governance is an opt-in, server-configured authorization boundary. It models
organizations, projects, environments, roles, users, service accounts, agent
identities and integrations. This bounded release uses deployment configuration,
not an identity administration UI, SSO or a live policy-distribution service.
All replicas must deploy the same authorization configuration; changing it
requires a coordinated restart. Keep configurations and credential files local.

### Immutable execution scope

Submission accepts `scope: {organization_id, project_id, environment_id}`. The
configured `default_scope` is used if omitted. Governed submissions require a
known environment. Scope enters the immutable plan digest, workers cannot
redefine it, child runs inherit it, and parent linkage cannot cross scopes.
`submitted_by` retains the authenticated root submitter through child execution.
Unscoped legacy plans retain their digests and existing deployments work with
governance disabled.

```sh
orbit identity
orbit projects
orbit run definition.yaml --scope acme/research/development --request-id request-1
```

`GET /projects` returns only environments visible to the caller. Scoped run
listing filters in PostgreSQL before the latest-100 limit. Run inspection,
journal/SSE, artifacts, cancellation, signaling and approval all check resource
scope and action. Workers have independent server-owned scope allowlists;
scoped workers cannot claim unscoped runs or another environment's runs. Their
persisted profile detects conflicting capacity, capability or scope declarations
across replicas. Use a new worker identity when changing that profile.

### Roles and policy

`governance.roles` maps names to explicit action strings. `principals` map an
identity to a `kind`, credential reference and grants. A grant has a scope and
role names; an explicit `scope: null` grants those actions globally. A scoped
grant never grants global worker, queue, scheduler-limit or audit access.
Supported actions are enumerated in `src/control_plane/governance.rs` and include
`run.read/submit/cancel/signal/approve`, `artifact.read`,
`definition.read/validate`, `worker.read`, `queue.read`, `limits.read/write`,
`audit.read` and `package.read/publish`.

Human approval additionally requires a `user` identity and assignment on the
step. Service accounts, agents and integrations cannot approve, even with an
approval role. The legacy operator remains an explicit global/break-glass
identity, but still cannot bypass a step's assignee list or environment admission
policy. Legacy approval-only tokens can decide only unscoped approvals.

Environment policies allow capabilities, repository IDs and agent binding names;
they cap per-step resources, agent budgets and per-run concurrency. Validation
walks nested child definitions before admission. These are per-run/task bounds,
not monthly organizational spending quotas. Model/tool credentials and provider
billing remain the trusted runtime's responsibility.

See [the example configuration](../../examples/server-governance.json). Replace its
credential references with operator-provisioned environment variables or private
files; it deliberately contains no usable credentials.

### Secret references

`operator_credential`, worker `credential`, and principal `credential` support:

```json
{"provider":"env","name":"ORBIT_SERVICE_CREDENTIAL"}
```

or `{"provider":"file","path":"/absolute/private/credential"}`. File references
must resolve to an owner-private regular file (no group/other permission bits),
at most 8 KiB; the final component cannot be a symlink. A trailing newline is
trimmed. Credential values must be 24–8,192 bytes, without CR/LF. They are resolved
at startup and never put in execution plans or API responses. Legacy literal
operator/worker tokens remain compatible, but cannot be combined with a reference
for the same identity. Duplicate credentials/identities are rejected.

File parent directories and server configuration must be trusted and protected.
There is no cloud vault, OAuth rotation or per-task secret delivery service here.
S3 credentials continue to use the artifact provider's environment references.

The [remote coding worker](../architecture/workers.md#trusted-worker-isolation) reuses SecretRef for its local
credential grants. These are resolved at use, outside scheduler transactions, and
restricted by purpose, repository/agent binding, exact URL audience and optional
existing scopes. Empty scope lists allow only unscoped attempts. Only logical Git
references enter plans; model credentials belong to the provisioned adapter.
Tool processes receive neither. This is worker-local authorized use, not a new
tenant hierarchy, a server-side credential broker or provider-key revocation.

## ACP registry and launch policy

### Contracts and installation

Bindings keep their existing runtime capability, tools, permissions and budget
fields. An optional `acp` descriptor adds logical agent/revision, launch SHA-256,
protocol 1, auth identity, security/filesystem/terminal policy, model attribution
and maximum execution limits. Only referenced bindings—including nested
definitions—affect plan digests. Absent additions are omitted; old present fields
retain their order and required model/token/cost validation.

ACP definitions use isolated `repository.code`, no delegation, object/JSON output,
`budget: {calls: N}` and `acp_limits`. Token/cost fields must be omitted, not zero.
ACP cannot be selected by legacy Responses/command runtimes or `agent.run`.
Existing scoped policies must explicitly allow execution-only budgets.

The private worker registry `acp_agents` contains exact binding copies, pinned
agent image/argv/identity/resource/network policy and explicit auth-file mappings.
No install path, argv, auth path or credential enters the Definition. Registry
entries and capabilities must be unambiguous. The worker compares binding digests,
scope, repository admission and execution profiles before materialization.
`orbit acp-launch-digest` validates and hashes a launch object without login or
API access. Task execution never installs or pulls an agent image.

Model policy is either `exact` with a specified model confirmed by session creation,
or `agent_configured` with no claimed exact revision. The initial Codex bridge
requires exact selection and disables provider fallback. Adapter version does not
identify a model. Generic agents must demonstrate their actual semantics before
being qualified.

## Supported runtime boundaries and pins

ACP initialization verifies a protocol peer. Repository execution additionally
requires pre-effect file and terminal mediation, exact launch policy, account
validation and confirmed cleanup. Displayed native tool events or forwarded
permission approvals do not prove that Orbit executed the effect.

### Runtime boundaries

| Runtime | Repository effect boundary | Supported scope and limits |
| --- | --- | --- |
| Orbit Codex bridge with Codex 0.156.0 | Only selected dynamic tools route through Orbit callbacks; native tools, web, MCP, hooks and delegation are disabled | Supports bounded file/terminal callbacks; account refresh/expiry and separate-host deployment require deployment-specific acceptance |
| Maintained `codex-acp` 1.11.0 | Native approvals and display metadata do not replace native execution with Orbit callbacks | Compatibility reference and initialize-only probe; not Orbit's coding backend |
| Unmodified Antigravity ACP 1.1.1 | Client files are mediated; native commands execute in the provider harness | Not eligible for brokered terminal execution |
| Orbit Antigravity `agy_acp_server_1.1.1-orbit-terminal-v2` | Versioned overlay removes native commands and local file fallback, exposing client-terminal calls | Catalog OAuth validation and model execution are distinct; model acceptance must identify this exact pinned runtime |
| Orbit Antigravity `agy_acp_server_1.1.1-orbit-correlated-tools-v2` | Native action IDs are preserved through brokered file and atomic shell callbacks; native tools, MCP, hooks and browser agents are disabled | EXACT correlation applies only to its declared immutable image and native adapter pair; account eligibility and complete workflow acceptance remain separate |
| Maintained Claude ACP 0.76.0 | Native `claude_code` tool preset and inherited settings; reviewed client helpers do not establish native Read/Write/Bash mediation | No Orbit execution bridge for native Read/Write/Bash mediation |

The CLI/editor workflow requires EXACT provider/tool/callback correlation when
its policy says so. The legacy Antigravity terminal runtime's PARTIAL correlation
cannot satisfy that requirement. The `antigravity-acp` and
`antigravity-correlated-acp` role preferences address the same active qualified
interface. The legacy terminal artifact is not in the bootstrap active catalog.
Bootstrap binds the correlated image to `gemini-3.7-flash-high`; subsequent
operator activation selects exact qualified model/role scopes. ACP must
confirm the selected model before a prompt is dispatched; an unavailable model
fails closed. The stdio editor service is a distinct interface over Orbit's
coordinator; protocol qualification is not actual Zed GUI acceptance.
Use [the roadmap](../ROADMAP.md) for current acceptance gates.

### Immutable installation policy

Use [Codex worker setup](../architecture/workers.md#provider-and-repository-process-separation) and
[Antigravity packaging and enrollment](../architecture/storage.md#private-provider-state). Runtime
version, image digest, adapter revision and launch digest are distinct pins.
A new release requires its own binary pins, patch review, reproducible package,
effect-boundary qualification and distinct binding identity. Existing runs keep
their accepted runtime and plan identities. No mutable tag or executable fallback
can inherit another runtime's qualification.

Codex's official package must include its matching Code Mode host. The declared
absolute executable is `/opt/codex/bin/codex`; its companion is
`/opt/codex/bin/codex-code-mode-host`. `Launch.command[0]` is included in the
launch digest and passed verbatim as the container entrypoint. Launch preflight
checks the pinned local image and exact `--version`, with no network, mounts or
credentials and a bounded timeout. A version-only check does not prove model
execution or tool mediation.

Antigravity's pinned distroless image includes the required
`nobody:x:65534:` group entry. Its embedded libraries provide runtime dependencies;
it does not rely on host CA copies, host executables or build-time package
installation. The builder's `UNQUALIFIED` result requires a separate qualification
scope before use. Full model acceptance cannot be inferred from OAuth reuse.

## Environment variables

| Variable | Purpose |
| --- | --- |
| `DATABASE_URL` | Server/worker PostgreSQL connection; use a private deployment source |
| `ORBIT_DATABASE_URL_FILE` | Private direct-catalog connection file for credential and role workflow commands |
| `ORBIT_URL` | Operator/worker API endpoint |
| `ORBIT_TOKEN`, `ORBIT_TOKEN_FILE` | API credential value or private file; contradictory nonempty sources are rejected |
| `ORBIT_HOME` | Explicit private operator-root override; default uses the effective user's account record |
| `ORBIT_CONTAINER_RUNTIME` | Runtime for generic container/local execution; does not override a pinned repository execution profile |
| `ORBIT_TEST_DATABASE_URL` | Disposable qualification database only |
| `ORBIT_EVIDENCE_DIR` | Local generated qualification output |
| `ORBIT_TASK`, `ORBIT_ATTEMPT_ID`, `ORBIT_BASE_REVISION` | Assignment context delivered to trusted repository commands |

Values are installation inputs, not grants or evidence of availability. Keep
secrets outside Git, command arguments, repository tools and diagnostic output.
Use [installation](../operations/installation.md) for deployment-specific setup
and [troubleshooting](../operations/troubleshooting.md) for qualification variables.

## Effective configuration inspection

`orbit config show` aggregates existing authorities without changing settings or
resolving secrets. It shows resolver defaults and the source-bound accepted
runtime/model descriptors. This is not a runtime registry or an availability
check.

Use `--config /path/to/orbit-editor.json` to inspect the product configuration,
`--server-config /path/to/server.json` for safe server metadata, and
`--config ... --session SESSION` to inspect current durable preferences separately
from the latest 16 immutable turn snapshots. Session inspection requires a
matching product configuration and uses a read-only PostgreSQL transaction.
Connection metadata is redacted. A session or `--runtimes` query reads the private database URL;
ordinary config inspection does not resolve that file or API credentials.

| Domain | Authority and precedence | Persistence and change scope |
| --- | --- | --- |
| Product repository, workspace, execution and verification policy | Explicit operator product file; omitted optional risk/skill use typed defaults | File; sessions check its stored digest, admitted executions retain their identities |
| Server bindings, authorization and storage | Explicit server file; inspection reports counts/configured metadata only | File; restart-sensitive, accepted plans retain referenced binding snapshots |
| Database/API credential input | CLI option overrides environment; direct database operations otherwise use the private default file | Operator private source; credential values are never shown |
| Resolver quota floors | Domain policy defaults (15% short window, 5% weekly); explicit policy APIs retain ownership | Selection policy; inspection is not a fresh quota observation |
| Bootstrap runtime/model descriptors | Immutable accepted source checkpoint; discovery does not add entries | Initialization and old admitted-target compatibility only |
| Installed, qualified and active agent runtimes | Durable operator registry; inspect `runtime status` or `config show --runtimes` | Exact OCI descriptors, model/role/effort evidence and explicit activation; resolver still owns current eligibility |
| User preference | PostgreSQL product session, shared by ACP and CLI | Applies to subsequent admitted turns; active turns cannot be retargeted |
| Admitted turn | Immutable PostgreSQL preference snapshot and resolved execution identity | Existing execution only; current preferences cannot rewrite it |

Reasoning uses provider-native canonical values. The bounded representation can
express `low`, `medium`, `high`, `xhigh` and `max`, but selectable support comes
from the active qualified runtime/model/role scope. Bootstrap Codex support is
`low`, `medium`, `high`; bootstrap Gemini 3.7 supports Auto only. Auto sends no
explicit effort. Additional levels require corresponding Orbit execution
evidence and explicit operator activation; discovery cannot add them. Gemini
3.8 is not bootstrapped from Gemini 3.7 evidence.

Old persisted reasoning spellings have a read compatibility path that preserves
their original native effort without rewriting rows. New inputs reject those
historical spellings; new serialized preferences use native values. This
compatibility does not qualify an additional effort or runtime.
