# ACP agent integration architecture

Status: experimental worker implementation, 2026-09-14. Codex and generic ACP
offline workflows and the final local fault regression have passed; live-account /
separate-host acceptance remains open. See [current evidence](../operations/acp-codex-compatibility.md),
[setup](../guides/acp-coding.md) and the [delivery gates](../development/acp-implementation-plan.md).
This design applies the user-provided `acp-orbit-agent-integration.md` to Orbit's
existing architecture at baseline `f7f9b177a1ff64de6152b738b9913857aa1d2907`.
Existing Responses/command workflows and immutable legacy plan digests remain intact.

## Boundaries and ownership

ACP is a worker-to-agent protocol, not a second workflow engine or authorization
service. PostgreSQL owns accepted plans, reservations, receipts, session records
and artifacts. Provider computation is externally uncertain, never implicitly
exactly-once. Delivery order is Codex, official Antigravity ACP, then maintained
Claude ACP; each real agent needs independent mediation and account evidence.

```text
Authenticated API → Engine operations → PostgreSQL accepted state
                          |
                    lease-owning worker
                          |
        +-----------------+-------------------+
        |                 |                   |
  private Git        ACP supervisor      ACP client broker
  materializer            |              /              \
        |          pinned agent OCI   confined files   workspace supervisor
        |          private auth HOME       |              |
        |          NO repository mount     |        pinned tool OCI
        +------------------------------ attempt repository
                                             |
                           accepted patch / manifest / session evidence
                                             |
                            fresh independent test → human approval
```

The worker's Orbit token, Git credentials, container runtime and actual repository
path never enter the agent container. Only a fresh control HOME and selected auth
files are mounted. The virtual client workspace `/orbit/home/workspace` exists
as an empty directory in that HOME. The broker maps its absolute paths to the
real attempt workspace; an agent cannot use arbitrary host paths.

Both agent and tool containers use read-only roots, dropped capabilities,
no-new-privileges, rootless UID mapping, bounded CPU/memory/PIDs and temporary
storage. The step reserves their **combined** resources: agent limits must fit
half; the tool container gets half. Tool networking is always disabled. Agent
networking is explicitly `none` or `host`; `host` is not provider-only egress.
Dedicated-host policy must protect other local/network services. This does not
upgrade the established `trusted` isolation class or prove safety for hostile agents.

## Code ownership

| Component | Responsibility |
| --- | --- |
| [acp_contract.rs](../../src/acp_contract.rs), [agent.rs](../../src/agent.rs) | Immutable policy, nullable execution-only accounting, bounded session batches |
| [acp_runtime.rs](../../src/acp_runtime.rs), [execution.rs](../../src/execution.rs), [worker.rs](../../src/worker.rs) | Operator registry, exact assignment authorization, capability advertisement and session lifecycle |
| [acp_wire.rs](../../src/acp_wire.rs) | Bounded JSON-RPC framing and correlation, without background queues or payload logging |
| [acp_process.rs](../../src/acp_process.rs) | Independent agent OCI supervision, auth lock/refresh/quarantine and cleanup receipts |
| [codex_session.rs](../../src/codex_session.rs), [codex_bridge.rs](../../src/codex_bridge.rs) | Codex 0.153.4 App Server translation and dynamic tools routed through ACP callbacks |
| [acp_broker.rs](../../src/acp_broker.rs) | Session-bound callbacks, reserve-before-effect, terminal ownership, record/receipt submission |
| [acp_files.rs](../../src/acp_files.rs) | Linux directory-fd confinement, file bounds and atomic private auth replacement |
| [acp_terminal.rs](../../src/acp_terminal.rs), [workspace.rs](../../src/workspace.rs), [container.rs](../../src/container.rs) | Asynchronous terminal handles over the existing workspace supervisor; repository lifecycle |
| [engine.rs](../../src/engine.rs) | Fenced operations, retained budgets, accepted transcript/report validation and uncertainty |
| [acp.rs](../../src/acp.rs) | Separate initialize-only installation probe; never authorizes execution |

## Contracts and installation

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

## Protocol lifecycle and bounds

One attempt creates one fresh session and sends one prompt. There is no
`session/load`, transparent resume, task-time authentication, delegation,
external MCP or dynamic permission approval. A prompt can contain many provider
model requests; it is reserved as one externally uncertain dispatch.

The production wire pump owns bounded reads directly: 1 MiB per frame, 16 MiB
incoming bytes per peer, 16,384 messages, correlated outgoing IDs and bounded
unique callback IDs. There is no unbounded reader queue, SDK transport logger or
raw payload diagnostic. The SDK's stable schema types validate callback/update
structures; the initialize-only probe separately uses its high-level connection.
Version 0.10.2/schema 0.11.2 avoids enabling `serde_json/preserve_order` through
dependency feature unification; an upgrade requires a legacy-digest review.

During a pending prompt, the worker services notifications and reverse requests.
Terminal creation returns promptly; the process continues under an independent
supervisor. Callback handling is serialized, with one terminal and no simultaneous
file operations. Ownership loss/cancellation is watched by the existing outer
worker loop and drops the entire ACP future; it does not wait behind a terminal
callback. A turn timeout sends a bounded cancel notification and closes transport.
Execution/drain grace and cleanup limits remain separate from a model's response.

## Codex bridge

The maintained `codex-acp` release is a compatibility reference, not Orbit's
execution backend: it forwards native tool approvals and presents native terminal
events. Zed supports both those display events and client-owned execution.
Displaying a terminal is not proof of an ACP terminal callback.

Orbit's bridge speaks ACP to the worker and the pinned Codex App Server protocol
inside the agent image. It initializes experimental API support, checks existing
auth without starting login, and creates a thread with `environments: []`,
empty workspace/capability roots, exact model/no fallback and only selected
`orbit_read_file`, `orbit_write_file`, `orbit_shell` dynamic tools.
Native environment tools, web, user-input/update-plan tools, selected effect
features, hooks, MCP and delegation are disabled. A clean control directory
prevents developer/project config discovery. The model's requested operation
passes through the Orbit broker before any repository effect.

Foreign thread/turn identities, namespaces, duplicate call IDs, native reverse
requests and native effect items fail closed. Shell maps to create → wait →
output → release, including release after an error. The real binary's loopback
qualification checks provider tool names as well as repository effects; see the
[fixed-source anchors and evidence](../operations/acp-codex-compatibility.md).

## Filesystem and terminal broker

All effect requests require an active exact session and a fresh accepted call
reservation with tool permissions and an attempt-bound request digest.
A replayed reservation never authorizes redispatch.

File access uses `openat2` with BENEATH, NO_SYMLINKS and NO_XDEV. It validates
the opened descriptor before truncating: regular file, owning UID and one link.
Traversal, mounts, symlinks, devices, FIFOs and hard links fail. UTF-8 reads/writes
are capped at 64 KiB; optional line/limit values are positive and bounded.
Writes preserve existing modes. File callbacks require all terminals released.

Terminal callbacks validate argv/cwd and reject agent-controlled environment.
A handle is session-bound and cannot be reused after release. Output retains
a character-safe tail while counting total bytes; overflow aborts execution.
The caller can poll, wait, kill or release without another effect reservation.
A shell reserves its entire allowed duration before creation; fast completion
does not refund time. A supervisor cleanup receipt distinguishes actual tool
exit 1 from an unconfirmed supervisor failure. No successful call receipt or
patch is published while cleanup is uncertain.

## Accepted state and artifacts

Reservations retain prompt, broker and worst-case terminal-time charges across
attempts. Token/cost totals are explicitly null. Stable usage extensions are
disabled; no context-window measurement or subscription cost is invented.

`record_acp_session` is an ordinary authenticated worker operation. Batches are
attempt/session-bound, sequential, limited to 32 metadata records each and 4,096
batches per session. Content digests, output byte charges and unique reported-tool
counts are retained under existing transaction/lease/generation fencing.
Same-content replay is idempotent; conflicting replay, gaps, closed-session writes,
foreign attempts or task-wide limit overflow fail.

Accepted logs contain normalized metadata batches, never raw reasoning or
provider/auth payloads. Final completion verifies transcript batch digests/counts
against accepted session state, plus report attempt/binding/session identity,
accounting mode, totals and cleanup status. The normal patch, manifest,
execution-report, fresh-base independent test and human review boundaries remain.
Storage verification happens outside coordination locks and authority is rechecked
afterward. Neither ACP nor a UI bypasses those checks.

## Authentication and failure recovery

One private canonical auth directory is exclusively locked on the worker host.
Only explicitly mapped private regular files are staged into a fresh control HOME.
A durable marker records the exact agent container and attempt before launch.
After confirmed removal, refresh files are checked and atomically replaced;
staged copies are cleared and the marker removed. Invalid refresh or uncertain
cleanup quarantines the store. Other agent-created private state is retained,
not exported; it can still contain secrets and requires reviewed retention.

Stdio is the worker lifeline. Independent supervisors remove agent/tool containers
after EOF, deadlines or worker death. If a supervisor/host itself dies, do not
infer cleanup: retain quarantine and inspect exact resources before operator
recovery. Auth directories are not shared across hosts. Draining stops new claims,
not active leases.

Unknown provider outcomes remain unknown after local cleanup. A reserved prompt
without an accepted receipt blocks automatic retry. Artifact or completion
acknowledgement replay uses existing immutable receipts and request deduplication;
it never repeats a provider invocation.

## Qualification and remaining gates

Regular tests cover serialization, policy, wire bounds, safe files, auth lock/
refresh/quarantine, records and bridge routing. Ignored `acp_workflow` tests cover
real/generic offline workflows, denied native/path effects, floods, worker death
and active-terminal cancellation. Their exact run status—not merely their
presence—is recorded in [compatibility](../operations/acp-codex-compatibility.md).
Shared engine/supervisor changes require full legacy qualification regression.

Antigravity and Claude need their own pre-effect terminal/native-tool bridge
evidence; a generic ACP registry does not establish it. A selected live account,
refresh/expiry tests, separately hosted worker, egress/repository-data policy and
owner evidence review remain required. No image publication, remote deployment,
repository push or paid call is authorized by local implementation/qualification.
