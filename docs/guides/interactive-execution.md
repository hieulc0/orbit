# Interactive execution boundaries

Orbit owns repository tools, role permissions, mutation ownership, workflow
state and verification. A provider prompt does not grant host access.

## Developer-local role execution

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
This profile is for trusted interactive repositories, not hostile workloads.
Untrusted execution remains unsupported.

The provider process remains in its existing supervised OCI runtime. Codex
negotiates an atomic shell extension for exactly one provider invocation and
one audited callback. Existing worker clients retain their terminal protocol.
Final workflow verification uses the separately pinned rootless OCI environment;
developer-local terminal results are exploratory evidence.

## Role resources

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

## Skill flows

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

The [editor service](editor-acp.md) uses these flows and the same coordinator.
The [external reasoning interface](external-reasoning.md) adds frozen requirements
and candidate-bound BA acceptance.
