# Cross-agent continuation and workspace snapshots

Orbit provides cross-agent continuation and automated fallback to allow multi-agent
collaboration and recovery within a single execution attempt. When an initial
agent reaches a turn budget, hits provider rate or quota limits, or leaves an
implementation that fails external validation, Orbit can seamlessly transition the
task to a fallback agent without discarding intermediate work.

Orbit owns the durable execution state and workspace lifecycle, treating individual
agents (such as Antigravity, Codex, or generic ACP agents) as replaceable execution
engines.

---

## Core architecture and concepts

### Attempt workspace ownership
In Orbit, an **Attempt** owns the workspace for its entire lifetime. An attempt
sequences one or more `AgentExecution` records (`sequence = 1, 2, …`) across
different agent types and providers operating within the exact same workspace.

- **Workspace retention**: Files created, modified, or deleted by previous agents
  are preserved in place in the attempt working directory.
- **Credential isolation**: Agent credential leases are strictly scoped to the active
  agent invocation. When an agent terminates, its authentication lease is released
  before the fallback agent acquires its own isolated credentials.
- **Bounded execution**: Multi-agent continuation is bounded by `max_executions`
  (default: 2) to prevent runaway retry loops.

```
                  ┌──────────────────────────────────────────────────┐
                  │                 Orbit Attempt                    │
                  │  (Owns workspace, lifecycle, and verification)  │
                  └──────────┬───────────────────────────┬───────────┘
                             │                           │
                   Sequence 1│                 Sequence 2│
                             ▼                           ▼
                     ┌───────────────┐           ┌───────────────┐
                     │ Primary Agent │           │Fallback Agent │
                     │ (Antigravity) │           │    (Codex)    │
                     └───────┬───────┘           └───────▲───────┘
                             │                           │
                    TurnLimit / 429 /                    │ Handoff Record
                    ValidationFailure                    │ & Prompt
                             │                           │
                             ▼                           │
                     ┌───────────────────────────────────┴───┐
                     │       Orbit Snapshot & Handoff        │
                     │  - Non-destructive workspace snapshot │
                     │  - Failure fingerprinting             │
                     │  - Provider-neutral prompt synthesis  │
                     └───────────────────────────────────────┘
```

---

## Fallback triggers and error normalization

### Normalized termination reasons
When an agent exits or encounters an error, the runtime adapter normalizes the outcome
into a `NormalizedAgentResult` with a machine-readable `TerminationReason`:

| Termination reason | Description | Eligible for fallback? |
| :--- | :--- | :--- |
| `turn_limit` | Agent exhausted configured ACP turn timeout or prompt turn limits | **Yes** |
| `rate_limited` | Temporary HTTP 429 (requests/tokens per minute, retry-after) | **Yes** |
| `quota_exhausted` | Provider account quota or monthly usage limit exhausted | **Yes** |
| `resource_exhausted` | Ambiguous backend resource exhaustion (e.g., gRPC code 8) | **Yes** |
| `timeout` | Session initialization or prompt response timeout | **Yes** |
| `agent_error` | Internal provider or adapter protocol error | **Yes** (when configured) |
| `cancelled` | Explicit operator or workflow cancellation | **No** (fail immediately) |
| `credential_error` | Missing, invalid, expired, or quarantined credentials | **No** (requires intervention) |
| `infrastructure_error`| Supervisor launch failure or host container errors | **No** (environment fault) |
| `process_crash` | Container or supervisor process killed by signal/exit code | **No** (fail-closed) |

### External validation triggers
Agent termination classification is separate from external verification. Orbit runs
independent test commands (e.g. `cargo test`) after agent termination. If the agent
exited cleanly (`TerminationReason::Success`) but external verification fails
(`exit_code != 0`), Orbit captures a `FallbackTrigger::ValidationFailed`.

### Failure fingerprinting
To detect repetitive errors and prevent infinite cycling between agents, Orbit normalizes
compiler and test diagnostic output into deterministic `FailureFingerprint` records
(schema version `validation/v1`).

- **Normalization**: Strips ANSI escape codes, ISO-8601 timestamps, absolute paths
  (retaining workspace-relative paths), and compiler line/column numbers.
- **Repetition limits**: If the same failure fingerprint occurs more than
  `max_same_failure_repetitions` (default: 2), the continuation engine advances past
  the repeating candidate or halts the attempt to prevent endless retry loops.

---

## Workspace snapshots and handoff records

### Non-destructive workspace snapshots
Before invoking a fallback agent, Orbit captures a `WorkspaceSnapshot` without modifying
the working tree (`Workspace::snapshot_workspace`):

- **Baseline revision**: The pinned immutable Git commit SHA.
- **Working tree inspection**: Runs `git status --porcelain=v1 --untracked-files=all -z`
  to accurately parse modified, added, deleted, and untracked files (including paths
  with spaces or Unicode characters).
- **Binary diff digest**: Computes a binary diff against the baseline revision and
  records its SHA-256 hash (`diff_sha256`).
- **Safety guarantee**: The snapshotting process is strictly read-only. It never runs
  `git reset`, `git checkout`, or `git clean`.

### Handoff records (`handoff/v1`)
Orbit structures the handoff context into a versioned `HandoffRecord`:

```json
{
  "schema": "handoff/v1",
  "task_id": "task-42",
  "attempt_id": "attempt-101",
  "from_execution_id": "exec-001",
  "trigger": "turn_limit",
  "workspace": {
    "baseline_revision": "78174688aaa37edaee05369988af66334a57454b",
    "head_revision": "78174688aaa37edaee05369988af66334a57454b",
    "changed_files": ["src/service.rs"],
    "added_files": ["src/feature.rs"],
    "deleted_files": [],
    "untracked_files": ["tests/feature_test.rs"],
    "diff_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
  },
  "previous_execution": {
    "execution_id": "exec-001",
    "agent_type": "antigravity",
    "provider": "google",
    "model": "gemini-3.7-flash-high",
    "termination_reason": "turn_limit",
    "message": "ACP turn timeout; prompt outcome unconfirmed"
  },
  "validation": {
    "command": "cargo test",
    "exit_code": 101,
    "summary": "error[E0308]: mismatched types in src/feature.rs:42:5"
  },
  "created_at": 1726820150
}
```

### Provider-neutral continuation prompt
Orbit automatically constructs a continuation prompt (`build_handoff_prompt`) for the
receiving agent. The prompt includes:

1. **Original task description**.
2. **Previous execution summary**: Agent type, model, termination reason, and diagnostics.
3. **Repository state summary**: Explicit lists of changed, added, deleted, and untracked files.
4. **Validation diagnostics**: Failing command, exit code, and bounded compiler/test error summaries.
5. **Standard instructions**: Instructs the agent to inspect the live workspace and git diff,
   preserve valid work, correct defects, and verify fixes.

> [!NOTE]
> Raw diffs and full log files are intentionally excluded from the prompt text.
> The agent has direct access to the live repository in the workspace and inspects
> diffs using standard file and terminal tools.

---

## Policy configuration

Continuation and fallback behavior is configured via `FallbackPolicy` or `ContinuationPolicy`.
By default, automatic fallback is **disabled** (`enabled: false`) to preserve backwards compatibility.

### Configuration fields

```json
{
  "enabled": true,
  "fallback_agent": "codex",
  "max_executions": 2,
  "on_triggers": [
    "turn_limit",
    "rate_limited",
    "quota_exhausted",
    "timeout",
    "validation_failed"
  ]
}
```

- `enabled` (*bool*): Enables multi-agent continuation for the step or runtime.
- `fallback_agent` (*string*): The binding name or identifier of the fallback agent candidate.
- `max_executions` (*u32*): Maximum number of agent execution attempts per Attempt (default: `2`).
- `on_triggers` (*array*): List of triggers that qualify for automatic fallback handoff.

### Chained agent candidates
For complex pipelines, `ContinuationPolicy` supports ordered multi-agent candidate lists
(`AgentCandidate`), allowing transitions across multiple specialized agents (e.g. Primary Antigravity
→ Fallback Codex → Tertiary Claude) with repetition back-off limits.

---

## Deterministic crash recovery and drift detection

Orbit's reconciler evaluates continuation state using a pure, deterministic state machine
(`continuation_recovery_action`):

1. **Terminal state protection**: If an attempt has reached a terminal state (`Succeeded`, `Failed`, `Cancelled`),
   recovery is a no-op.
2. **Cancellation priority**: Cancellation requests take immediate precedence and prevent subsequent fallbacks.
3. **Workspace drift validation**: During recovery, Orbit compares the live workspace diff SHA-256
   against the persisted `HandoffRecord.workspace.diff_sha256`. If unexpected file alterations or corruption
   occurred outside the supervisor lifecycle, recovery fails closed.
4. **Resumption actions**:
   - `ResumeValidation`: Re-runs external test validation if an agent completed before verification finished.
   - `PrepareHandoff`: Generates snapshot and handoff record when a fallback trigger is detected.
   - `StartFallback`: Launches the fallback agent with `sequence = 2`.
   - `FinalizeFailure`: Halts the attempt if max executions are reached or non-retryable errors occurred.

---

## Inspection and verification

Operators can monitor multi-agent execution status through the CLI and API:

### Inspecting an active run
Use `orbit inspect` to view the continuation state of all tasks and attempts:

```sh
orbit run inspect --run-id <run-id>
```

The output contains real-time continuation tracking for each attempt:
- `continuation.state`: `not_configured`, `fallback_pending`, `fallback_running`, `completed`, or `exhausted`.
- `continuation.sequence`: Current execution sequence number.
- `agent_executions`: Full chronological history of agent executions, providers, models, timestamps,
  and termination reasons.

### Attempt success criteria
An attempt succeeds if:
1. At least one agent execution finishes with status `Completed` (either primary or fallback).
2. The final external validation command passes with exit code `0`.
3. The final patch and execution report artifacts are successfully uploaded and sealed.
