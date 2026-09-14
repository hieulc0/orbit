# ACP implementation and acceptance plan

Status: Codex local implementation/qualification complete; rollout in progress,
2026-09-14. This is the delivery companion to the
[ACP integration design](../architecture/acp-agent-integration.md). The worker,
broker and Codex workflow are implemented and locally fault-qualified; live
integration and later named agents are not accepted. Current setup is in
[ACP coding](../guides/acp-coding.md).
Current live-provider/remote-host acceptance gaps remain in
[remote coding qualification](../operations/remote-coding-qualification.md).

## Current implementation and next sequence

Deliver Codex first, then the official Google `antigravity-acp` distribution,
then maintained `claude-agent-acp`. Do not substitute another agent when a Codex
compatibility gate is incomplete. Each later agent needs its own native-effect
and account qualification; shared ACP wire support alone is insufficient.

| Package | Implemented now | Remaining before exit |
| --- | --- | --- |
| P0 | Real Codex ACP initialization; reviewed Codex 0.153.4 dynamic tools/no-environment policy; actual Codex binary passed offline broker-only workflow | Selected account/expiry/refresh and later-agent terminal handoffs |
| P1 | Strict optional contracts, nested/legacy digest compatibility, operator registry, image/launch pins, scope/profile checks and capability routing; configuration tests passed | Selected installation under P7 |
| P2 | Retained prompt/broker/time charges, null billing, fenced bounded session batches, transcript/report agreement and SDK helper; final durable replay/gap/cancel checks passed | Stable usage extensions intentionally disabled |
| P3 | Bounded bidirectional wire pump, real Codex App Server bridge, independent OCI supervisor, auth locking/refresh/quarantine, resource split and lifeline cleanup; worker-death/cancel passed | Selected-account refresh semantics |
| P4 | Directory-fd filesystem confinement, line-bounded UTF-8 reads/writes, async owned terminal handles and cleanup receipts; flood/cancel/container regression passed | Live installation and stronger isolation are separate gates |
| P5 | Worker routing, accepted patch/manifest/evidence, fresh independent tests and human review; final real/generic offline workflows passed | Live account/host under P7 |
| P6 | Offline/adversarial tests, setup templates, reviewed evidence and source records; 60 shared fault-regression cases passed with bounded concurrency | Hosted CI execution and owner evidence acceptance |
| P7 | Not performed | Explicit live account, separately hosted worker, repository/egress policy and owner evidence acceptance |

The next slice is the specific
[Antigravity/Claude handoff gaps](../operations/acp-agent-compatibility.md), not a
claim that generic ACP transport qualifies their native tools.
The production wire pump has bounded direct reads instead of unbounded background
queues. Agents run in pinned OCI images with an empty virtual workspace, not host
user access. Codex uses experimental `dynamicTools` with `environments: []`, not
approval of native tools. These implementation choices supersede provisional
transport/path details in the original work-package descriptions below.

See the [compatibility record](../operations/acp-codex-compatibility.md) for source
pins, actual checks and unresolved gates. The [preflight guide](../guides/acp-preflight.md)
remains an initialize-only installation check, separate from workflow setup.

## Intended outcome

An authorized worker launches one pinned ACP agent using an already provisioned
account, completes an inspect/edit/test/revise repository task, publishes durable
patch and agent evidence, and passes independent testing and human review.
Forbidden tools and paths are rejected before broker effects. Cancellation and
worker/server failures preserve budgets, fencing and explicit external uncertainty.

The first delivery has one session per attempt, one active prompt per session,
one live broker terminal, no delegation or external MCP, no interactive task-time
login and no automatic session resume. It supports trusted private workers only.
Neither this plan nor its implementation selects a paid account or authorizes
publication, remote deployment, repository pushes or acceptance on behalf of a
reviewer.

## Work packages and dependencies

```text
P0 compatibility evidence
        |
P1 contracts and legacy compatibility
        |
P2 durable accounting and records
        |
P3 supervised ACP transport
        |
P4 filesystem and terminal broker
        |
P5 repository workflow integration
        |
P6 fault qualification and operator documentation
        |
P7 selected live account and remote worker acceptance
```

P0 starts the dependency chain. Its fake ACP peer tests client assumptions;
accepting a real adapter still requires mediation evidence.

### P0 — Validate the selected adapter and protocol subset

Inspect a fixed release of the maintained
[codex-acp implementation](https://github.com/agentclientprotocol/codex-acp) and
its underlying agent dependency. Record package/binary hashes, release/source
revisions, required runtime, fixed configuration and the stable ACP schema used.
Select and review a [Rust ACP SDK](https://docs.rs/agent-client-protocol/latest/agent_client_protocol/)
release; demonstrate concurrent callbacks while a prompt is pending and bounded
transport reads. Do not equate crate version with wire version.

Produce a compatibility record covering:

| Question | Required evidence |
| --- | --- |
| Can an existing private auth store be used unattended? | Sanitized startup/auth transcript; expired/missing auth behavior; refresh-write and account-lock requirements |
| Do repository reads and edits call Orbit's client methods? | Correlated callback trace and filesystem observation, including create and symlink cases |
| Can native commands route through Orbit terminals? | Actual callback trace with no native host-command fallback |
| Can other effect paths be disabled? | Fixed configuration/source mapping for native tools, web/MCP, background tasks, subagents and user/repository-loaded plugins/config |
| Can model choice be pinned or observed? | Exact selection/response comparison, or explicit `agent_configured` classification |
| What usage is actually reported? | Missing usage, cumulative snapshots, units/currency and reset semantics |
| What happens on cancel/EOF/death? | Agent and child-process exit evidence, unfinished prompt behavior and surviving terminal/session state |
| Does the SDK fit Orbit's runtime? | Version negotiation, callback dispatch, bounded framing and shutdown demonstration |

Use source inspection and disposable offline fixtures first. Any account-backed
experiment needs a selected account and repository-data policy. Do not claim
broker compatibility from tool display events or an advertised terminal capability.

Exit: a reviewed pass/fail matrix and fixed candidate identity. If required native
effects cannot be mediated, record the exact gap and a bounded bridge proposal.
Keep that integration unqualified until the bridge or an explicitly revised
isolation design passes. Supporting another named agent is not an automatic
substitute for the selected compatibility decision.

### P1 — Add contracts without changing legacy serialized plans

Primary files: [src/agent.rs](../../src/agent.rs),
[src/model.rs](../../src/model.rs), [src/execution.rs](../../src/execution.rs),
[src/api.rs](../../src/api.rs), definition schema consumers and SDK types.

Implement the proposed optional ACP binding descriptor, worker-local registry,
definition limits, model attribution and execution-only accounting. Preserve
legacy required model/token/cost validation. Add omitted optional serialization
for new fields; keep original field order and digest logic. Keep Responses and
command configurations valid. Reject ACP settings on unrelated step/runtime types.

Validate logical auth source/owner and optional existing scope without introducing
mandatory governance. Select runtime by exact pinned binding plus authorized
capabilities. Validate executable integrity and local profile matching before
dispatch. Definitions cannot carry executable paths, arbitrary args or auth files.

Exit: regular tests prove old serialized fixtures and digests are byte-identical;
new nested/referenced ACP bindings affect digests while unreferenced bindings do
not. Denied tools, unsupported isolation, missing model attribution, ambiguous
runtime selection, excessive limits and unknown fields fail closed. Update strict
definition schema/UI validation and Python/Rust callers for optional accounting.

### P2 — Extend the ledger and accepted evidence

Primary files: [src/agent.rs](../../src/agent.rs),
[src/engine.rs](../../src/engine.rs), [src/model.rs](../../src/model.rs),
[src/sdk.rs](../../src/sdk.rs), [src/artifacts.rs](../../src/artifacts.rs),
[src/evidence.rs](../../src/evidence.rs) and Python SDK operations.

Add transactional ACP limit charges and compact usage/session projection. Reuse
tracked prompt/tool reservations and finish receipts. Add bounded normalized
record batches with sequence/content deduplication, transcript chunk references
and completeness flags. Keep payload storage in immutable artifacts; keep the
run aggregate bounded. Use current PostgreSQL storage unless concrete indexing
needs justify a migration; do not invent a second ledger.

Make every new write pass through existing worker identity, request identity,
lease, generation and cancellation checks. Hash/verify artifacts outside scheduler
coordination locks and recheck before accepting them. Implement nullable observed
usage separately from execution budget; UI/CLI/export must not render missing
cost as zero. Unknown ACP prompt outcome enters existing intervention semantics.

Exit: competing servers cannot over-reserve or accept conflicting records; retries
retain all charges; identical retransmissions recover accepted receipts; stale
owners cannot finish calls or finalize transcript references. Lost reservation
acknowledgement never grants a second dispatch. Backup/restore and evidence
inspection retain the new records and artifact checksums.

### P3 — Implement the supervised ACP client

Proposed new module: `src/acp_agent.rs` with transport/process helpers as needed.
Integrate with [src/worker.rs](../../src/worker.rs),
[src/main.rs](../../src/main.rs) and [src/lib.rs](../../src/lib.rs).
The reviewed SDK was added during P0 to exercise real initialization; retain its
locked version until serialization and wire compatibility are requalified.

Implement bounded JSON-RPC stdio, capability negotiation, configured auth/session
setup, prompt reservation/receipt, update normalization and permission denial.
Keep one active prompt while servicing reverse requests concurrently. Distinguish
protocol stdout from bounded sanitized stderr. Do not capture raw reasoning or
auth payloads. Initial unsupported file/terminal capabilities remain unadvertised
until P4 is complete.

Add a process supervisor with worker lifeline, explicit cancel signal, deadlines,
graceful stop and bounded process-tree cleanup. Give the agent process a separately
verified resource limit. Acquire the auth-store lock before launch and release it
only after children stop; quarantine the store if cleanup is unconfirmed. Failure must preserve
evidence already accepted, including when upload/receipt acknowledgement fails.

Exit: deterministic subprocess tests cover fragmented/oversized frames, malformed
JSON, unknown/wrong IDs, EOF during prompt, unsupported version, auth-required,
stderr flooding, cancellation while a callback is pending and worker death.
No callback deadlock or unattended browser login. No reservation replay causes
prompt retransmission.

### P4 — Implement brokered files and asynchronous terminals

Primary files: [src/workspace.rs](../../src/workspace.rs),
[src/container.rs](../../src/container.rs), ACP broker helpers and
[src/coding_agent.rs](../../src/coding_agent.rs) only where a shared interface is
needed. Retain the current synchronous workspace execution wrapper for Responses
and independent test callers.

Implement absolute host-to-container path mapping and race-resistant confined
text access. Introduce explicit tool implementation revisions. Add line-range,
size/type checks, safe file creation and mutation tests. Implement terminal
start/output/wait/kill/release with attempt/session ownership, bounded output,
fixed environment policy, timeout reservations and cleanup. Use structured command
arguments and the existing pinned OCI profile; verify network, mounts and cgroups.

Authorize and reserve each actual broker effect even if no preceding permission
request arrived. Keep reported agent tool calls separate from dispatch counts.
Count rejected/polling request traffic against bounded queues/rates, and allow
cleanup despite an exhausted execution budget. Do not block cancel behind a
terminal wait or hold a database coordination lock during a tool invocation.

Exit: all advertised methods work against the fake peer and selected compatibility
adapter. Escape/race attempts cannot read a host marker or modify outside the
workspace through the broker. Native host effect paths remain disabled and tested.
Agent and terminal process trees stop under cancellation/worker death; inability
to confirm removal is visible, not accepted as success.

### P5 — Complete the repository workflow

Primary files: [src/workspace.rs](../../src/workspace.rs), worker runtime routing,
agent report validation, examples, CLI/API inspection and existing review UI.

Route only the agent phase through ACP; reuse private Git materialization,
patch/manifest creation, artifact finalization, independent tests and final human
approval. Normalize final text into bounded typed output with binding provenance
and model/accounting attribution. Refusal, limit exhaustion and unresolved calls
cannot complete successfully. Stop all terminals before extracting the patch.

Add fake-agent, server/worker and workflow templates after schema support exists.
Use placeholder logical names and file paths, never real credentials or a floating
agent installer. Give the tester only repository access. Provide existing API/CLI
inspection of session status, stop reason, retained charges, unknown usage,
transcript completeness and cleanup outcome before adding a dedicated transcript UI.

Exit: a fixture run demonstrates inspect → edit → failed test → revise → passed
test; accepted patch; independent testing in a fresh workspace; worker denied
approval; authorized human review. Verify the source fixture/developer checkout
remains unchanged and no push/deploy command occurs.

### P6 — Qualify failures and write current operating instructions

Proposed tests: `tests/acp_agent.rs`, `tests/fixtures/acp-agent.py`, and
`tests/kernel/acp.rs` included from the kernel harness. Names are planned, not
current runnable test targets. Reuse the existing private Git/Podman fixtures and
fault-injection barriers. Cover every matrix row below and inspect its evidence.

Write a current setup guide with installed version/hash, required runtime,
dedicated account, agent/process resource limits, private auth preflight, local
registry, coder/tester commands, cancellation, drain, orphan handling and coordinated
upgrade/rollback. Add an ACP qualification record separating implemented, locally
verified, live verified and reviewer-accepted items. Update canonical references
and roadmap status only to match observed evidence.

Exit: required regular and full disposable checks pass, sanitized exports and
patch/test artifacts are independently reviewed, and all acceptance gaps are
explicit. Local fixture success is not the live acceptance gate.

### P7 — Qualify a selected account on a separate worker host

Inputs: explicitly selected agent release/account owner, permitted authentication
method, repository-data policy, read-only private repository credential, pinned
base, bounded task, execution image, worker destination and named reviewer.
These choices are prerequisites for live work, not defaults inferred by a worker.

Run the same workflow without shared developer source paths. Check real auth
refresh/failure, TLS/provider connectivity, broker use, agent process limits,
independent tests and final review. Exercise controlled worker/server interruption
with the account owner's authorization; verify uncertain prompts are not resent.
Report optional usage as observed, without inferring subscription prices.

Exit: a dated acceptance record links reviewed patch, manifest, transcript summary,
invocation ledger, test report, failure evidence and actual human decision. State
which remote-coding gates it satisfies and which adapter-specific gates remain.
Additional agents require their own P0/P7 compatibility evidence.

## Required failure and acceptance matrix

| ID | Stimulus | Required result and evidence |
| --- | --- | --- |
| ACP-01 | Old plans/configurations round-trip; unrelated ACP binding added | Legacy bytes/digests unchanged; unused binding does not change plan |
| ACP-02 | Missing runtime capability, local binary/config mismatch or unavailable profile | No agent launch; assignment denied without fallback |
| ACP-03 | Missing/expired auth, wrong owner/scope, concurrent auth-store use | No unauthorized prompt or browser launch; sanitized failure and exclusive-store behavior |
| ACP-04 | Unsupported version/capability; oversized/malformed input; stdout noise | Bounded protocol failure and cleanup; no accidental task success |
| ACP-05 | Parent/symlink/race/hard-link escape, special file or huge line range | Denial before broker effect; host marker unchanged and unread |
| ACP-06 | Forbidden tool, forged permission details or native-tool fallback | No authorized host effect; callback ledger proves broker control; compatibility fails on bypass |
| ACP-07 | Parallel create, foreign terminal ID, wait plus cancel, release then output | Owned lifecycle, responsive cancellation, no cross-attempt access or unbounded queue |
| ACP-08 | Flood stdout/stderr/updates or repeat cumulative usage | Byte/event/rate limits stop work; no double-counted billing or unbounded aggregate |
| ACP-09 | Competing reservations and retries across attempts | Transactional limits; retained charges; calls/turns/terminal allowance never reset |
| ACP-10 | Lost reserve/finish/record acknowledgement and conflicting retransmission | Identical receipt recovery, conflicts rejected, no duplicate prompt/tool dispatch |
| ACP-11 | Worker kill after prompt reservation or during active tool callback | Unresolved parent prompt requires intervention; no automatic tool-only retry |
| ACP-12 | Server restart or last confirmed lease expires during I/O | Durable records retained; stale result/record/artifact acceptance denied |
| ACP-13 | Cancellation/deadline during prompt, terminal or artifact upload | Logical outcome and provider uncertainty preserved; local cleanup independently checked |
| ACP-14 | Agent exits cleanly with unresolved calls, refusal or limit stop | No accepted success; reason and evidence remain inspectable |
| ACP-15 | All calls settled, completion lost, new safe attempt | Fresh workspace/session, retained charges, no history replay; old owner fenced |
| ACP-16 | Supervisor/runtime refuses cleanup or host becomes unreachable | Physical stop unconfirmed and orphan identity retained; no false cleanup claim |
| ACP-17 | Successful candidate plus independent testing/review | Fresh base plus accepted patch, verified artifacts, worker denied approval, actual reviewer decision |
| ACP-18 | Secret/think payload injection, export, backup/restore | Raw auth/reasoning excluded; private code access controlled; restored counters/hashes agree |
| ACP-19 | Worker drain and shutdown grace | New claims stop; active leases continue until completion or bounded shutdown |
| ACP-20 | Selected live agent on separate host | Account/model attribution, actual broker use and remote failure evidence; no shared checkout |

Regular tests cover pure validation, serialization, protocol and local subprocess
behavior. PostgreSQL/process/Podman tests cover transaction races, authorization,
cleanup and accepted artifacts. ACP-20 requires live authorization and cannot be
replaced by the fake agent. Maintain a case-name-to-matrix-ID mapping in the future
qualification record; test count alone does not establish coverage.

## Verification commands and evidence handoff

For documentation-only edits:

```sh
bash scripts/check.sh docs
```

During implementation, use the smallest relevant regular test target first,
followed by the repository-required Rust checks:

```sh
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
bash scripts/check.sh
```

Use `bash scripts/check.sh ui` when browser behavior changes. Provision only
disposable prerequisites from [testing](testing.md), then run
`bash scripts/qualify.sh`. Shared engine/schema/supervisor changes need the full
qualification regression suite, including legacy command/Responses agents,
governance, artifacts, recovery and digest compatibility. Add targeted ACP commands
to the testing guide only after the named test module exists. The current ACP
foundation targets are now listed there. Rebuild without
`fault-injection` before normal use.

Keep generated data under a private `target/qualification-acp*` directory and
export it with the existing evidence exporter before review. The handoff must
include exact Orbit/agent/SDK/image revisions, tested configuration digests,
commands actually run, skipped checks, matrix mapping, manifest/hash verification,
patch and independent-test review, cleanup findings and unresolved gates.
Inspect raw commands and artifacts for credentials; automated redaction is not
permission to publish. Do not commit runtime configuration, auth stores, lease
tokens, workspaces or generated evidence.

## Decisions that remain evidence-dependent

| Decision | Default direction | Resolution gate |
| --- | --- | --- |
| First compatible real agent | Maintained `codex-acp` candidate | P0 proves all required broker paths or records a bridge requirement |
| SDK release and execution model | Reviewed stable-v1 high-level Rust SDK | P0 concurrency/framing tests, then locked dependency in P3 |
| Exact model versus agent-configured | Explicit attribution, no invented revision | P0 capabilities; P7 deployment policy chooses what is acceptable |
| Auth store layout and refresh writes | Private operator-provisioned store, one active process | P0/P7 actual agent behavior |
| Full-frame and event limits | 1 MiB frame, bounded 8 MiB transcript plus task quotas | P3/P6 stress evidence; increase only with explicit bounded design |
| Mid-prompt approvals and session resume | Deferred | Separate durable authority/recovery design after first acceptance |
| Stronger agent containment | Dedicated trusted worker first | Concrete threat model and separate backend qualification |

Implementation completion requires P0–P6. Supported live integration requires
P7 and a recorded reviewer decision; neither is implied by the presence of these
documents or passing fixture tests.
