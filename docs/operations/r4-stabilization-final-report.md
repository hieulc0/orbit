# ORBIT_R4_STABILIZATION_FINAL_REPORT

Decision: **ORBIT_SELF_HOSTING_QUALIFIED** for the bounded, live documentation
workflow recorded below on September 30, 2026.

## Baseline and scope

The accepted candidate starts at
`14769c6083b0ac47d4ba6c77a773b6040a9d0267`, after S9 and S10. A separately
pinned Orbit binary operated a disposable candidate clone. Its SHA-256 is
`d7dd5340c60bff6a4f42bc26ef1a73a08d6213a4d1e99b7c2372e276ba9908a4`.

The September 30 handoff records that the frozen B1–B6 stabilization campaign
passed. That campaign was not rerun or expanded for this documentation task.
Its generated S10 report, `target/roadmap-evidence/S10-report.md`, was absent
from this checkout; the inherited campaign result is attributed to the handoff.
Regular Cargo ignored cases are not counted as new service-backed passes.

The real task extracted workflow qualification guidance into
`docs/development/workflow-qualification.md`, linked it from the general testing
guide, and updated the documentation index. Only these three documents changed.
Commands and contract text were preserved; relocation pointers were repaired.
The accepted document bytes were subsequently applied to the developer checkout.

## Admission and execution

A fresh catalog-backed status probe reported Codex `codex-main` READY with
confirmed scope and valid authentication. The immediate admission guard checked
96% remaining 5H quota and 83% remaining 7D quota against the unchanged 15%/5%
minimums. The runtime required EXACT tool-audit capability.

| Evidence | Result |
| --- | --- |
| Workflow | `wf-6af64fb2-65c8-4b89-a80f-64deeea3529a`, COMPLETED |
| Attempt | `att-d9c82455-7df1-4949-ad5f-03ec325d62db` |
| Planner | `acp-exec-e7c4f204-b99d-4bec-9f41-f66d7aad8932`, 6 successful correlated calls |
| Implementer | `acp-exec-c23420a4-7d63-4acc-86e5-4115ae3f8032`, 12 successful correlated calls and real mutations |
| First reviewer | `acp-exec-0f2b05e7-545f-4403-aadf-92ae0d62d5ad`, 9 successful correlated calls, CHANGES_REQUESTED |
| Repair implementer | `acp-exec-5bbd1f55-63e7-47de-b0b9-05cc39470f89`, 11 successful correlated calls and a new candidate state |
| Final reviewer | `acp-exec-0d3f47f2-03bd-49b9-9b7e-81c27bf000ae`, 5 successful correlated calls, APPROVE |
| Final review artifact | `ha-45346209-4be6-4dd2-8e2a-7ef594a58018` |
| Repaired FAST | `vrun-ccb5c696-d4eb-4690-a520-1f4ff68f8b47`, PASSED |
| Repaired STANDARD | `vrun-d48ae664-2417-49f5-a43b-20f62503f9eb`, PASSED |
| FULL | `vrun-b6060bf2-617b-4d2c-a703-d20d7607baff`, PASSED |

The first implementation honestly handed off an incomplete extraction. The
independent reviewer identified duplicated guidance and a temporary comment.
Orbit ran its normal repair stage, removed both, verified the changed candidate,
and obtained a new independent approval before FULL. The earlier failed dogfood
workflow was not resumed.

FAST executed formatting, library tests, ACP runtime tests, whitespace checks,
and documentation links. STANDARD executed formatting, Clippy, whitespace,
and documentation links. FULL executed formatting, Clippy with warnings denied,
locked/offline tests across all targets/features, whitespace, and documentation
links. Every recorded step passed with exit 0. Execution used the cached pinned
rootless Podman image `sha256:d58b84f1e69bd4912d34fa02790fe028124d0ae25f7f9cf4fc8d59b63dd17576`,
with network disabled and clean environment policy.

## Identity, audit, and cleanup

Reviewed, repaired FAST/STANDARD, FULL, workflow-final, and independently
recomputed on-disk WorkspaceState all equal:

`ws-v2-0c256664bbcc0e696c3c6050aedd72705e5c8873d6f468b7ffaaa6764d8d8724`.

All 43 tool calls have exact provider/callback/Orbit correlation. There are zero
unsuccessful, denied, unmatched, or omitted calls. Mutations used Orbit callbacks;
planner and reviewer authority remained read-only. All five AgentExecutions
succeeded, resolved prompt uncertainty, exited their supervisors with code 0,
and confirmed cleanup. No candidate mutation lock or workflow step owner remains.

## Retained evidence and limits

Local raw evidence is under `target/roadmap-evidence/dogfood-20260930/`, including
the admission snapshot, immutable policies, task, workflow records, agent audits,
handoffs, verification steps, accepted candidate, and independent acceptance
checks. These generated files are not shipped as repository configuration.

All roles requested/resolved `gpt-6-luna`; actual model identity was not reported
and remains UNKNOWN. Runtime resolution recorded quota headroom as UNKNOWN,
including after the snapshot became stale. The separate immediate admission
guard used observed quota; no stale role snapshot is promoted to fresh evidence.
The result qualifies this live self-hosting workflow, not arbitrary tasks,
separate-host deployment, or later execution profiles.

R5 structural modularization is the next milestone. R5–R11 remain pending.
