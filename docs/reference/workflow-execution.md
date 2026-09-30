# Workflow execution contract

The CLI and editor workflows use the same coordinator, persisted ownership and
candidate-bound acceptance rules. See [verification](verification.md),
[execution boundaries](../guides/interactive-execution.md), and
[qualification procedures](../development/workflow-qualification.md).

## Immutable policy and candidate identity

Verification policy ID/version pairs are immutable. Workflow runs pin the
definition digest at creation and fail if that exact version is missing or its
content changes. Every selected required check needs a declared command or a
validated integration/browser action; unresolved checks stop selection. CLI
workflow verification also requires a pinned rootless Podman profile and has no
host or generic Cargo/docs fallback.

Workflow creation persists an absolute canonical repository path, including
when the caller supplied a relative path. Disk candidates use the version 2
workspace identity: tracked Git changes and eligible untracked files are
included, and Git or file-read errors fail the operation. Legacy snapshot
identities retain their original encoding. Verification, review, and final
completion recheck the candidate identity; reviewer diff generation errors
record `REVIEW_ERROR` instead of supplying an empty diff.

## Callback authority

The CLI ACP repository callback checks the role and canonical workspace identity
for every tool call. Mutations require the persisted role execution's database
lock, and callback calls, output, and runtime are bounded by tool metadata.
Filesystem mutations use directory-relative handles to reject traversal and
symlink replacement. Git status/diff path filters are normalized to repository
relative pathspecs before host Git runs. The trusted CLI profile denies terminal
calls; an explicitly pinned developer-local profile permits implementer terminals
under confinement and mutation ownership.
Role prompts identify the ACP virtual workspace root and require
workspace-relative repository tool paths; the host repository path is not
exposed to the role.

## Ownership and recovery

Workflow steps claim a persisted owner and generation before work. Stage,
handoff, and role writes check that claim; cancellation fences the generation
and revokes callback locks. Mutation locks use the canonical repository path
across attempts. On restart, a dead owner is reclaimed only when no role or
verification remains active; an uncertain external execution requires explicit
reconciliation before retry. Existing attempt-only locks must be reconciled
before migration 0024 can add workspace identity.

Role execution waits for the local ACP supervisor and matching cleanup
receipt before recording success. Cancellation reaches active role and isolated
verification processes, including when another coordinator cancels the workflow.
The coordinator checks the selected credential generation again before launch;
`actual_model` remains unset unless the runtime reports an observed model. A
missing cleanup receipt leaves the mutation lock fenced for reconciliation.

## Process outcomes and cleanup

Verification command policy uses the observed process outcome: numeric exit,
signal, timeout or unknown. Cancellation and cleanup uncertainty carry typed
errors through diagnostic context; an unrelated diagnostic containing “timeout”
or “cancelled” does not determine the result. Managed readiness deadlines and
ACP turn deadlines likewise retain their types. Browser crash classification
uses the process status when no structured harness report is available.

Container and integration network teardown confirm resource absence before
publishing successful cleanup. Only the Podman existence check's absence exit
code confirms removal; a failed or timed out check leaves cleanup unconfirmed.
Cleanup uncertainty takes precedence over cancellation or a successful command
and is recorded as an error. These checks preserve workflow fencing, role
permissions and candidate-bound verification.

## Credential selection

Role runtime resolution applies reset-aware account selection over fresh,
credential-scoped availability evidence. The default safety thresholds are 15%
for a known 5-hour window and 5% for a known 7-day window; exact-threshold
observations remain eligible. Eligible accounts with known safe weekly quota are
ordered by earliest reset, then the role's provider preference, then stable
catalog credential ID and reference. Unknown or inapplicable quota windows remain
eligible under the existing availability policy and rank after known weekly
resets. Codex's configured Luna target uses the `default` bucket and ignores
`gpt-reserve`; Antigravity's Gemini target uses the Gemini group and ignores
the Claude/GPT group. Embedders can override the narrow defaults with
`WorkflowCoordinator::with_quota_selection_policy` and
`RuntimeQuotaSelectionPolicy`. Resolution reasons include observed headroom,
reset, rank and bounded rejection details. Automatic runtime re-resolution after
an execution quota failure is not implemented; use the workflow
[recovery procedures](../guides/continuation.md#pure-recovery-decisions).

