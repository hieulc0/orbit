# Editor ACP service

`orbit acp-serve --config FILE --database-url-file PRIVATE_FILE` exposes ACP v1
on standard input/output. It uses the existing PostgreSQL stores, runtime
selection, role execution, callbacks, verification and cancellation. It creates
no network listener. The operator launching the process grants the client its
session and candidate actions; the client receives no filesystem or shell callbacks.

## Operator configuration

Create a canonical, owner-private workspace root (`mkdir -m 700`) and an operator
JSON file with these fields:

| Field | Value |
| --- | --- |
| `repository` | Absolute canonical source Git checkout |
| `workspaces` | Absolute canonical owner-private directory for detached worktrees |
| `agent_execution_profile` | `{"profile":"dev_local","bubblewrap":"/usr/bin/bwrap"}` or `{"profile":"trusted"}` |
| `verification_environment` | Independently qualified, digest-pinned rootless Podman environment |
| `selection_policy` | Versioned project checks, with `canonical_digest: true` and a new policy ID/version |
| `risk` | `conservative` by default; `low` permits the documentation flow described in [interactive execution](interactive-execution.md) |
| `skill` | Optional pinned snake-case skill; omitted allows task selection |
| `external_role` | Omitted for an editor; pinned `business_analyst` or `system_architect` for the [reasoning interface](external-reasoning.md) |

Use the meaningful checks already qualified for the project. A JSON file or a
synthetic test image identity is not verification qualification. New canonical
selection-policy digests remain stable across JSON/database round trips;
policies without this opt-in retain their legacy encoding. Do not reuse an
existing immutable policy version with a changed encoding or checks.

The service admits up to 64 retained candidates, including applied candidates
until discarded. A connection admits four active prompts, 4,096 request IDs,
1 MiB protocol frames and a bounded durable transcript. Session configuration
and repository identity must match on reload. Role identity is pinned separately
for each external-role connection; editing it in prompt text grants no authority.

## Zed setup

Configure a custom agent in Zed settings:

```json
{
  "agent_servers": {
    "orbit": {
      "type": "custom",
      "command": "/absolute/path/to/orbit",
      "args": ["acp-serve", "--config", "/absolute/path/to/editor.json"],
      "env": {
        "ORBIT_DATABASE_URL_FILE": "/absolute/private/database-url-file"
      }
    }
  }
}
```

This follows the [Zed external-agent contract](https://zed.dev/docs/ai/external-agents).
Use an absolute executable and configuration path. Select Orbit in the agent
panel; its modes choose skills before the task is pinned. Repository/provider
credentials stay in the operator catalog and supervised runtime.

ACP initialization, new session, load/replay, prompt, cancel and mode selection
are supported. Reload replays the durable notifications before its response, as
required by [ACP session setup](https://agentclientprotocol.com/protocol/v1/session-setup).
The panel uses standard plan/message notifications. A typed stdio qualification
is distinct from an actual Zed GUI acceptance run.

## Interactive actions

The panel shows task, flow, current role, provider/model/logical account, all five
resource counters, candidate identity and paths, verification, quota observation
freshness/reset timestamps, execution and confirmed cleanup. Unknown quota is
shown as unavailable; status does not silently probe a provider. Structured role
handoffs are shown after a turn; large payloads have explicit truncated previews.

| Command | Action |
| --- | --- |
| `/status` | Refresh the durable panel |
| `/open` | Show the managed attempt path |
| `/diff` | Show candidate changes |
| `/continue` | Advance to the review gate |
| `/review` | Request review from REVIEWING and run final verification |
| `/cancel` | Cancel, then wait for supervised cleanup |
| `/apply WorkspaceStateId` | Apply an accepted exact candidate to the clean original checkout |
| `/discard WorkspaceStateId` | Remove that exact retained candidate after cleanup |

A normal task prompt creates one immutable task. Continuing uses `/continue`;
a different task needs a new session. Read/control commands work while a prompt
is active. Apply and discard are explicit actions, not agent tools.

Optional typed extensions provide `_orbit/session/status`, `/open`, `/diff`, and
`_orbit/candidate/apply`, `/discard`, `/recover_application`; each uses `sessionId`.
Candidate actions also require `workspaceStateId`. The diff extension returns
32 KiB UTF-8 pages with `totalBytes`, `truncated`, and `nextOffset`; pass `offset`
to continue. Native filesystem methods remain unsupported at this editor boundary.

## Candidate and recovery invariants

Iteration changes a detached worktree. Applying requires completed review and
verification for the exact candidate, confirmed role cleanup, no step or mutation
owner, an unchanged candidate index, unchanged source HEAD and a clean developer
checkout. Native Git filters are rejected. A durable repository claim serializes
application across sessions. Ownership is checked after Git I/O before publishing
APPLIED. The patch leaves the developer index unchanged, and the resulting
WorkspaceState must match the accepted candidate, including new binary files.
Discard remains explicit after application.

An uncertain action is retained as RECOVERY_REQUIRED. Application recovery requires
the original durable repository claim, confirmed cleanup and the exact candidate.
`_orbit/candidate/recover_application` reconciles to APPLIED only when the checkout
matches that candidate, or READY only when application admission proves it is still
clean at the pinned baseline. An unrelated or partial checkout stays blocked.

Creation/start/discard interruptions currently require operator reconciliation:
inspect the retained session row, exact registered worktree path, workflow/step
owners and cleanup evidence before removing or repairing an orphan. Do not mark
cleanup confirmed merely because the connection closed. The status panel retains
session state when its candidate is unavailable. Automatic orphan reconciliation
and a hostile multi-tenant local profile remain outside this interface.
