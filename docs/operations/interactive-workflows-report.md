# Interactive workflow qualification

Implementation checkpoint: `8dec490`, following modularization baseline `7c6ea4f`.
Qualification completed September 30, 2026 (Asia/Ho_Chi_Minh).
The roadmap remains incomplete. This report separates implemented interfaces,
disposable qualification, and the live/editor acceptance still required.

| Milestone | Checkpoint status |
| --- | --- |
| R6 developer-local profile | Implemented; Linux confinement, immutable profile and exact atomic-shell callback qualified |
| R7 production budgets | Implemented; role limits, counters, typed exhaustion and byte paging qualified |
| R8 Orbit ACP service | Implemented and qualified offline; guarded real-provider ACP fixture accepted |
| R9 editor integration | Managed worktrees, ACP panel/actions/replay and Zed configuration implemented; actual Zed GUI acceptance pending |
| R10 skills selecting flows | Implemented; immutable skill policy, read-only completion and conservative escalation qualified |
| R11 external BA/SA roles | Typed interface, durable artifacts, freeze/acceptance and operator bridge client staged and qualified offline; live bridge integration/acceptance pending |

## Qualified contracts

[Developer-local execution](../guides/interactive-execution.md) confines exploratory
shells in bubblewrap namespaces. Host homes, private credentials, unrelated
repositories and container sockets are absent. Environment inheritance, network
and Git metadata mutation are disabled. Missing confinement fails; no host fallback
exists. Process descendants are killed before terminal cleanup is confirmed.
Final verification retains its independent, digest-pinned rootless OCI profile.

Production roles admit 150 planner/reviewer callbacks or 300 implementer callbacks,
with separate mutation, terminal, file-read and output limits. Running/final metadata
records limits and usage. Exhaustion is TOOL_BUDGET_EXHAUSTED. Byte reads preserve
UTF-8 boundaries and report bytes_returned, total_size, truncation and next_offset.
Legacy workers retain their existing line reads and terminal protocol. The Codex
CLI workflow bridge negotiates byte reads and one correlated atomic shell callback.

The [editor service](../guides/editor-acp.md) delegates progression to the existing
coordinator. PostgreSQL owns session state, replay notifications, pinned settings,
flows and action ownership. A detached worktree holds changes until explicit apply.
Apply checks review, final verification, cleanup, exact candidate/index identity,
source HEAD and a clean checkout. The source must be its canonical Git root; a
repository-wide durable claim prevents separate sessions from applying concurrently.
Post-I/O ownership and exact resulting WorkspaceState are checked. Interrupted
application reconciles only after proving unchanged or accepted checkout identity.

[Skills](../guides/interactive-execution.md#skill-flows) configure the generic workflow.
Low-risk documentation may use FAST/review/final FAST; code, configuration, manifests
and unknown/empty changes escalate. Pinned regression policies can demand stronger
checks. Read-only analysis has a matching successful handoff and unchanged candidate;
it does not claim technical review or authoritative verification.

The [external reasoning interface](../guides/external-reasoning.md) stores versioned
RequirementBrief, TechnicalProposal, Challenge, Resolution and AcceptanceContract.
BA/SA authority comes from operator connection policy. Frozen requirements generate
the implementation task; a later client prompt cannot substitute another objective.
Unresolved required challenges block freeze. Technical completion enters
BUSINESS_ACCEPTANCE; only a matching BA attestation for every frozen criterion,
exact reviewed/verified candidate and confirmed cleanup permits completion.
Synthetic acceptance tests prove state-machine guards, not real BA acceptance.
The existing BA bridge repository and conversation history were not modified.

Legacy role/plan/policy serialization remains unchanged by default. New interactive
selection policies explicitly opt into canonical JSON digests under a new immutable
ID/version. Existing compatibility exports and runtime selection, quota, callback
correlation, step/mutation fencing and verified-artifact authority remain in place.

## Qualification

Fresh checks of this checkpoint passed:

| Gate | Result | Local evidence |
| --- | --- | --- |
| Complete repository check | 468 regular Rust passes, zero failures; fmt, Clippy with warnings denied, 2 Python SDK and 15 script tests, UI build and documentation links passed | `current-complete-check.log` |
| All targets/features | 468 passes, zero failures; 206 ignored cases are not passes | `current-all-features.log` |
| Editor and developer-local qualification | 7 explicitly selected ignored cases passed | `current-editor-local.log` |
| Shared workflow and offline ACP qualification | 36 explicitly selected ignored cases passed | `current-workflow-shared.log` |
| Documentation links | All current local links resolve | `current-docs.log` |

The **43 fresh disposable cases** cover namespace/privacy/descendant cleanup,
profile immutability, exact atomic shell auditing, durable editor replay/modes,
external artifact authority, freeze/acceptance, readonly completion, interrupted
apply recovery, concurrent apply fencing and shared coordinator behavior. Regular
editor cases also cover canonical source-root rejection and candidate/file bounds.
The readonly budget regression proves denied writes consume callback capacity
without spending nonexistent mutation allowances or exhausting the role.

A prior serial campaign reported 170 passes across 26 suites, including broader
verification, integration environment, browser, regression, credential, accounting
and recovery coverage. Its generated `target/` evidence is absent from this checkout.
It is historical qualification, not the fresh 43-case result or additional current
passes. The fresh logs and manifest are under `target/roadmap-evidence/`.

Typed ACP v1 messages validate initialization, new session, modes, prompt response,
progress/command notifications, replay and load response against the protocol crate.
Editor filesystem callbacks are rejected. Worktree fixtures check stale candidates,
dirty source checkouts, staged changes, file-size bounds and host Git filters.
The operator bridge client rejects human/ambiguous/untyped/oversized exported turns.

Earlier qualification reported sandbox loopback denials and a browser durability
environment error/screenshot timeout; permitted and unchanged retries passed.
Those earlier generated logs are absent here. During the fresh documentation
cleanup, an initial full check reached the docs gate with two removed-page links
still referenced. Both links were repaired and the complete check passed again.
The failed attempt remains in `current-check.log`. No quota, lease, deadline,
isolation or verification assertion was weakened.

The database was a dedicated rootless PostgreSQL 17 container with tmpfs data on
loopback port 55444. The campaign used locally provisioned rootless images;
Docker backend parity, separate-host deployment, S3/deployment campaign expansion,
actual Zed GUI and live browser BA acceptance are not established by this result.

## Live ACP acceptance

The user-requested catalog-backed `credential status --all --quota` command refreshed
Codex's default bucket to 97% five-hour and 68% seven-day remaining. The previously
reported 23:24 timer and generated evidence were absent on resumption, so no scheduled
result was assumed. A new bounded fixture ran with a frozen normal binary and a new
disposable Git repository. Fresh guards admitted it at **89% / 66%** before execution
and **88% / 66%** before review, above unchanged 15% / 5% thresholds.

The real provider completed planner, implementer and reviewer executions. One confined
shell appended the required documentation line and checked host/private/socket access.
All **13 callbacks** correlated one-to-one with provider tool calls; unmatched callback
and provider-call counts were zero. Independent FAST, STANDARD and FULL passed for
the same candidate. All three executions confirmed cleanup. The source checkout stayed
unchanged before explicit apply; exact apply and discard passed, the ACP process exited
zero, and read-only database inspection found no step, mutation or repository owners.

| Identity | Value |
| --- | --- |
| Workflow | `wf-117aebdd-dd36-4932-9b06-40d0142682cb` |
| ACP session | `editor-0e34ecc5-5145-4bdc-ac4e-88a966f27e84` |
| Accepted candidate | `ws-v2-596b12d2268a6b49790d313b77ac711945f53fb028baecd3e85f1a54e0a74e09` |
| Frozen binary SHA-256 | `defd38e70fb5644cb0d882150e3e2be1f726998f51af6f025c90f5330ed0748a` |

Evidence is `interactive-current-live/`: accepted/reviewed states, protocol transcript,
quota guards, callback audit and ownership inspection. This accepts the bounded R8
provider-backed ACP fixture. It does not establish actual Zed GUI behavior, external
BA acceptance, arbitrary autonomous development or separate-host deployment.

## Retention and remaining acceptance

`current-disposable-database.sql` retains the owned synthetic qualification database.
The exact PostgreSQL container `4702f6488ebc` was removed after checks and dump completed.
The live control-plane PostgreSQL container remains running. Cached pinned images,
private local evidence and the accepted disposable source fixture are retained; its
managed candidate was discarded. `current-manifest.json` hashes 28 local evidence files. The existing
`orbit export-evidence` command produced a fresh control-evidence export in
`target/roadmap-evidence-review`; its manifest was independently verified. Raw
verification commands, zero exits, the README-only patch and callback correlation
were inspected. The actual private catalog URL was absent from retained text.

R9 requires an actual Zed session. R11 requires the development `../orbit-ba-bridge`,
an identified authenticated BA conversation, actual typed artifacts and matching
business acceptance after real implementation/review/verification. The bridge's
existing uncommitted development files and conversation state were preserved.
No actual Zed installation/session or authenticated BA conversation was identified.

Creation/start/discard orphan reconciliation remains an explicit operator procedure;
uncertain rows and cleanup are retained. The developer-local profile is for trusted
interactive repositories and does not establish hostile multi-tenant isolation.
