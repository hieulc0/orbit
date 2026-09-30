# Self-development control plane

Orbit coordinates bounded engineering work through durable workflow state,
policy-controlled roles, independent verification and explicit acceptance. Current
support and remaining live acceptance gates are in [the roadmap](../ROADMAP.md).
Provider discovery evidence is in [the status record](../operations/provider-status-discovery.md).

## Execution state and ownership

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

## Responsibilities

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

## Runtime and resource identity

Keep these dimensions separate:

- Runtime: pinned executable/image, adapter revision and launch policy.
- Credential: logical catalog identity, generation, representation and provider scope.
- Model and reasoning: requested requirements and observed activation, when reported.
- Capability: the runtime's supported operations and execution constraints.
- Role: semantic responsibility and allowed effects.
- Availability: scoped observations with explicit freshness and unknown fields.
- Ownership: current lease, generation, request identity and cancellation state.

Authentication does not establish capability or quota. An advertised model does
not prove activation. Exact-model policies require confirmation before prompting;
`actual_model` remains unknown unless reported. Do not silently substitute providers,
models, reasoning levels or isolation profiles.

Logical resource identities are canonical and secret-free. Credentials and auth
paths are resolved by operator policy at execution time; plans, prompts and safe
inspection use logical references. Provider credentials remain isolated between
runtime executions. Auth-store locking/quarantine and database mutation ownership
protect different resources and neither replaces the other.

## Availability and selection

Availability snapshots record scope, source, observation/expiry timestamps,
confidence and normalized quota buckets, windows and groups. Raw provider bucket
IDs become domain-separated fingerprints. Flat window projections remain a legacy
compatibility surface; consumers use explicit bucket/group membership.

Missing percentages, reset times, token usage and billing are null or unknown.
An exhausted result does not supply the missing numerical values. Provider resets
are separate from Orbit's evidence freshness TTL. Stale positive evidence cannot
remain authoritative READY. Conflicting evidence retains provenance and scope;
optimistic observations do not automatically override stronger negative evidence.
An account-scoped observation must not be presented as model-specific readiness.

The role resolver filters fresh credential-scoped evidence and capabilities before
ranking eligible accounts. Known five-hour headroom below 15% or seven-day headroom
below 5% blocks the default selection; exact thresholds remain eligible. Safe known
weekly quotas rank by earliest reset, then provider preference and stable credential
identity. Unknown windows follow explicit availability policy. Codex uses its default
bucket; reserve quota cannot override that bucket's safety guard. Antigravity uses
the group matching the requested provider/model. See
[resolver qualification](../development/workflow-qualification.md).

Selection records explain selected and rejected candidates using bounded evidence.
The LLM does not choose account eligibility or change selection policy. Status probes
are bounded observations, not inference turns or a source of fabricated billing.
A fresh status refresh does not by itself authorize a provider invocation.

## Verification, review and repair

WorkspaceState binds the baseline, HEAD and eligible candidate content. Git and
file-read failures fail candidate observation rather than producing an empty diff.
Each verification result retains its plan/policy/environment identity and exact
WorkspaceState. A later mutation makes previous passing evidence stale for completion.
See [the verification contract](../reference/verification.md).

The coordinator owns PLAN, IMPLEMENT, verification, REVIEW, bounded REPAIR and final
qualification. Skill-selected flow policy is immutable. Documentation may use a
lighter flow only under explicit low-risk policy and observed documentation-only
changes; code, configuration, unknown and empty changes escalate. Read-only analysis
requires an unchanged candidate and matching successful handoff and does not claim
technical qualification. See [interactive flows](../guides/interactive-execution.md).

Continuation transfers eligible unfinished work under a configured bounded policy.
Repair responds to accepted verification/review findings with a new authorized
mutation. Neither is an unbounded retry or automatic provider-session replay.
Cancellation, exhausted budgets, stale ownership, unresolved dispatch and uncertain
cleanup must retain their distinct meanings. See
[continuation contracts](../guides/continuation.md).

Completion requires candidate-matching review and final authoritative verification,
confirmed cleanup and no active mutation/step owners. Frozen external requirements
also require matching BA acceptance. A successful provider turn cannot establish
workflow success. See [external reasoning](../guides/external-reasoning.md).

## Persistence and recovery

PostgreSQL owns state transitions, request deduplication and fencing. Persist execution
identity before dispatch. Provider/storage I/O must not hold scheduler coordination
locks; recheck ownership, generation, cancellation, lease and artifact identity after
I/O before accepting results. At-least-once execution does not imply exactly-once
external effects. A lost acknowledgement is handled through the same request identity,
not by repeating a provider call.

Cancellation persists intent and fences further effects before requesting process
shutdown. Drain stops new claims without revoking active leases. Successful local
cleanup does not prove a provider stopped work or billing. Missing cleanup receipts
retain ownership/quarantine for explicit reconciliation rather than authorizing reuse.
Recovery uses durable state and exact disk observations, never a provider transcript
or an optimistic leftover-directory check.

The editor keeps a detached candidate until explicit apply. Application requires
exclusive durable ownership of the canonical source repository, unchanged clean
source/base, exact accepted candidate/index and confirmed cleanup. Interrupted apply
can release its claim only after proving either unchanged source or the exact
accepted result. Partial or unrelated source changes remain blocked. See
[editor operations](../guides/editor-acp.md).

## Self-development trust boundary

Engineering work runs from an immutable human-selected Orbit revision in disposable
candidate environments. An agent cannot modify the running control plane, its tokens,
private credential backend, authorization, selected verification policy or release
boundary. Changing Orbit source does not grant that source authority over its own
acceptance. Independent verification and review use the frozen inputs and policy.

Commit, merge, publication and deployment are explicit authorized actions. A
qualification experiment preserves its budget, failures and evidence; an operator
does not repair the candidate or feed hints into an autonomous run. See
[dogfooding](../development/dogfooding.md).

## Extension constraints

A multi-host resource inventory and database-wide provider-capacity leases require
separate qualification. They must preserve Attempt leases, worker-local auth locks,
credential locality, cancellation and generation fencing. Current role resolution is
not a promise of provider concurrency reservation across hosts.

A new Goal aggregate or universal ActivityExecution hierarchy is justified only by a
concrete persistence or recovery gap in the existing Task/Attempt model. Bounded
branch/join policies must define partial failure, reviewer availability, cancellation
and integration; they cannot permit simultaneous writers in one candidate.
Additional environment/isolation backends need explicit policy and cleanup evidence.

New contracts are additive: preserve `orbit/v0`, existing `orbit/v1`, absent optional
fields, legacy digests, accepted artifacts and historical workspace identities.
Feature negotiation or explicit versions govern incompatible wire changes. No in-place
rewrite of accepted history or silent interpretation of old bindings as new grants.

## Qualification

Required checks cover legacy serialization, scoped availability, deterministic
selection, concurrent ownership, denied effects, workspace drift, bounded repair,
process/namespace cleanup, cancellation and restart. Evidence records exact revisions,
images, policies, candidate IDs and checks actually run. Live accounts, actual editor
UI, authenticated BA conversations and separate-host deployment have distinct gates.
Passing local tests or checksum review is not owner acceptance.
