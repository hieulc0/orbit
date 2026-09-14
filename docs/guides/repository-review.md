# Submit, inspect and review a repository change

Use one bounded repository task to exercise the complete handoff: a coding worker
produces a patch, an independent tester applies it to the pinned base, and a human
reviews the accepted artifacts. The CLI uses the same authorization and durable
engine boundary as the console. No merge or push is part of this workflow.

## Select and configure the work

Choose the repository, full base commit, issue and independent test command. Set
up one existing runtime using the [Responses worker guide](remote-coding.md) or
[Codex ACP guide](acp-coding.md). Use the corresponding definition example and
private server/worker configurations. The tester needs the repository credential
and execution profile, with no model account access. For a local, credential-free
fixture, start with [local development](local-development.md).

A live run additionally needs a selected provider account/model, approved repository
access and a separately hosted worker with no shared developer checkout. Confirm
the configured image contains the repository's dependencies and that the independent
test command works within its network-free workspace. Record limits and the chosen
operating boundary before submitting work. Local fixture success does not qualify
the selected account, network or host.

## Submit and inspect

Set `ORBIT_URL` and a private `ORBIT_TOKEN_FILE` for the operator session, then:

```sh
orbit validate /absolute/private/change.yaml
orbit run /absolute/private/change.yaml --request-id issue-123-1
orbit inspect RUN_ID
orbit events RUN_ID --after 0 --follow
```

Use the returned `run_id` in subsequent commands. Keep the definition and request
ID unchanged after an uncertain submission; retransmitting returns the original
receipt. An intentional new run needs a new ID. Interrupting `events --follow`
stops observation only; the run continues. Resume from the last printed journal
sequence if needed.

Inspect task states. With the supplied code/test/review graph, the coding and
testing tasks should succeed and `review` should be `WAITING`; the run itself is
still `RUNNING`. A failed test or uncertain provider outcome requires investigation.
Do not approve merely because the coding worker produced a patch.

## Collect and review the accepted result

```sh
orbit export-run RUN_ID --output /absolute/private/issue-123-review
```

The parent directory must exist. The export creates a new private directory and
prints its manifest in the usual JSON/JSONL format. `manifest.json` maps artifacts
by step, kind and attempt, so the patch and independent test report can be located
without manually downloading each artifact ID. It also records the plan digest,
snapshot state, journal sequence, file sizes and hashes. See the
[export contract](../reference/api-cli-sdk.md#private-run-export) for limits and
failure behavior.

Review the actual patch, including changes to tests, against the issue and recorded
base commit. Read the independent test report and logs, plus the execution/agent
reports. Confirm the tester used a distinct fresh workspace and the accepted patch.
Inspect unknown provider outcomes and retained accounting explicitly. A passed
test is evidence for that test's scope, not a substitute for reviewing the change.

For further manual verification, use a separate disposable checkout of the recorded
base and apply the exported patch there. Do not apply it to the developer checkout
as part of qualification. The export does not run artifact contents or apply patches.
It contains private repository material and requires review before sharing.

## Record the human decision

After reviewing the artifacts, an authorized human assignee can record a decision:

```sh
orbit approve RUN_ID review --request-id issue-123-review-1 \
  --comment 'Reviewed patch and independent tests'
orbit inspect RUN_ID
orbit export-run RUN_ID --output /absolute/private/issue-123-final
```

Use `--deny` to reject the candidate. Preserve the approval request ID, decision and
comment when retrying after a lost response. An export never makes this decision
for the reviewer. The final export uses a new directory and includes the subsequent
decision; the earlier review snapshot remains unchanged. Always inspect the actual
terminal state rather than treating an accepted approval receipt as run success.

## Recovery and repeated use

Server restart should preserve the same run and journal. Workers must stop at their
last confirmed lease; permitted retries use fresh workspaces. An unresolved model
call may have incurred an external effect or cost even when its worker stopped.
Inspect the run, provider evidence and private worker diagnostics before deciding
how to proceed. The current intervention path is cancellation and a separately
reviewed new submission with `--parent-run-id OLD_RUN_ID`; it is not conversation
resume or permission to repeat an uncertain provider call.

For repeated tasks, retain the issue, base commit, run ID, review/final exports,
human decision and manual recovery work. Assess whether the output solved the task,
how long setup and completion took, and where intervention was needed. Use those
observations to choose the next improvement. Live host failure/recovery and provider
qualification remain the gates in the [roadmap](../ROADMAP.md).
