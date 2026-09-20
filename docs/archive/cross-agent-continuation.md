# Implement Cross-Agent Continuation and Fallback in Orbit

## Objective

Implement first-class support for continuing a failed, interrupted, quota-limited, or exhausted coding attempt with a different agent/provider.

Example:

```text
Antigravity / Gemini
        │
        │ edits repository
        │
        ▼
   validation fails
        │
        │ quota / turn limit / failure
        ▼
      Codex
        │
        │ continues from existing workspace
        ▼
   validation succeeds
```

Later:

```text
Antigravity → Codex → Claude ACP → ...
```

The implementation MUST NOT depend on transferring the previous agent's internal conversation history.

Orbit owns the durable execution state.

Agents are replaceable execution engines.

---

# 1. Core Architecture

Separate three concepts:

```text
Task
 │
 ├── specification
 ├── repository
 ├── acceptance criteria
 └── execution policy
        │
        ▼
Attempt
 │
 ├── workspace
 ├── baseline commit
 ├── current git state
 ├── artifacts
 ├── validation evidence
 ├── handoff records
 └── agent executions
        │
        ▼
Agent Execution
 ├── agent type
 ├── provider
 ├── model
 ├── credentials
 ├── start/end timestamps
 ├── termination reason
 └── logs
```

A critical rule:

```text
Task != Attempt != Agent Execution
```

Do not model an attempt as belonging permanently to one agent.

An attempt may contain multiple sequential agent executions.

Example:

```text
Task #123
   │
   └── Attempt #4
          │
          ├── AgentExecution #1
          │      antigravity
          │
          │      result:
          │      turn_limit
          │
          ├── AgentExecution #2
          │      codex
          │
          │      result:
          │      validation_failed
          │
          └── AgentExecution #3
                 claude-acp

                 result:
                 success
```

The workspace belongs to `Attempt #4`, not to any individual `AgentExecution`.

---

# 2. Durable State Requirements

Orbit must preserve enough state that a completely new agent process can continue the task without access to the previous agent's conversation.

At minimum preserve:

```text
Original task specification
Repository
Baseline revision
Current working tree
Git diff
Untracked files
Validation commands
Validation output
Previous execution result
Artifacts
Logs
Agent execution history
```

Do not depend on:

```text
LLM context window
provider conversation ID
provider-specific memory
hidden reasoning
agent process lifetime
```

Provider-specific session information may be recorded for diagnostics, but MUST NOT be required for continuation.

---

# 3. Introduce AgentExecution

If the current data model assumes:

```text
Attempt
    agent = antigravity
```

refactor toward:

```text
Attempt
    workspace
    status
    baseline_revision
    ...

AgentExecution
    id
    attempt_id
    sequence
    agent_type
    provider
    model
    started_at
    finished_at
    status
    termination_reason
    exit_code
    metadata
```

Possible statuses:

```text
pending
running
completed
failed
interrupted
```

Termination reasons should be structured.

For example:

```text
success
agent_error
validation_failed
rate_limited
quota_exhausted
turn_limit
timeout
process_crash
credential_error
cancelled
infrastructure_error
unknown
```

Do not infer fallback policy from arbitrary stderr strings throughout the scheduler.

Normalize provider-specific errors inside the agent adapter.

Example:

```text
Antigravity HTTP 429
        ↓
AgentTermination::RateLimited

Codex quota message
        ↓
AgentTermination::QuotaExhausted

ACP max-turn termination
        ↓
AgentTermination::TurnLimit
```

The orchestration layer should operate on normalized termination reasons.

---

# 4. Preserve the Workspace

Cross-agent continuation must reuse the same attempt workspace.

Example:

```text
~/.orbit/workspaces/<attempt-id>/
```

Sequence:

```text
1. checkout repository
2. establish baseline
3. launch Antigravity
4. Antigravity modifies files
5. Antigravity exits
6. DO NOT reset workspace
7. collect evidence
8. launch Codex against SAME workspace
9. Codex inspects and continues changes
```

The following operations MUST NOT happen automatically during continuation:

```text
git reset --hard
git clean -fd
fresh checkout
discard previous diff
```

unless an explicit policy requests a clean retry.

We need to distinguish:

```text
retry_clean
```

from:

```text
continue_workspace
```

Cross-agent fallback uses:

```text
continue_workspace
```

by default.

---

# 5. Capture a Workspace Snapshot Before Handoff

Before starting another agent, collect deterministic workspace information.

At minimum:

```bash
git status --porcelain=v1
git diff
git diff --cached
git ls-files --others --exclude-standard
```

Record:

```text
baseline revision
HEAD
changed files
added files
deleted files
untracked files
diff checksum
```

Prefer storing the complete diff as an artifact rather than putting a potentially huge diff directly into database rows.

Example metadata:

```json
{
  "baseline_revision": "abc123",
  "head_revision": "abc123",
  "changed_files": [
    "src/auth.rs",
    "src/config.rs"
  ],
  "untracked_files": [
    "tests/github_auth.rs"
  ],
  "diff_artifact_id": "...",
  "diff_sha256": "..."
}
```

This snapshot is evidence.

It does NOT replace the actual workspace.

---

# 6. Introduce a Handoff Record

Before changing agents, generate a structured handoff record.

Do not rely solely on natural-language summaries.

Example:

```json
{
  "version": 1,
  "task_id": "...",
  "attempt_id": "...",
  "from_execution_id": "...",

  "task": {
    "title": "Implement GitHub authentication",
    "acceptance_criteria": [
      "Support PAT",
      "Support SSH",
      "Existing tests continue to pass"
    ]
  },

  "workspace": {
    "baseline_revision": "abc123",
    "changed_files": [
      "src/github/auth.rs",
      "src/config.rs"
    ],
    "untracked_files": [
      "tests/github_auth.rs"
    ],
    "diff_artifact_id": "..."
  },

  "previous_execution": {
    "agent": "antigravity",
    "provider": "google",
    "termination_reason": "turn_limit"
  },

  "validation": {
    "status": "failed",
    "command": "cargo test",
    "exit_code": 101,
    "evidence_artifact_id": "...",
    "summary": "Compilation failed in src/github/auth.rs"
  }
}
```

Store this as an immutable artifact or database record.

Use a versioned schema:

```text
handoff/v1
```

so it can evolve later.

---

# 7. Handoff Prompt

When launching the next agent, Orbit should construct a provider-neutral continuation prompt.

Example:

```text
You are continuing an existing implementation attempt.

Original task:
<task specification>

Another coding agent previously worked on this task.

The existing workspace contains that agent's changes.
Do not discard those changes unless they are incorrect.

Current repository state:
- baseline: <sha>
- changed files: ...
- untracked files: ...

Previous execution ended because:
<termination reason>

Latest validation:
Command:
    cargo test

Result:
    exit code 101

Failure summary:
    mismatched types in src/github/auth.rs

Your task:
1. Inspect the existing workspace and git diff.
2. Understand the previous changes.
3. Continue or correct the implementation.
4. Run the required validation.
5. Leave the workspace in the best valid state possible.

Do not assume the previous implementation is correct.
The repository and validation evidence are authoritative.
```

Keep this prompt provider-neutral.

The same handoff should work with:

```text
Antigravity
Codex
Claude ACP
future agents
```

Agent adapters may wrap the prompt as necessary but should not change its semantic meaning.

---

# 8. Validation Must Be External to the Agent

Do not rely only on an agent saying:

```text
"Tests pass."
```

Orbit should execute validation itself after the agent exits.

Example:

```text
Agent exits
    ↓
Orbit validator
    ↓
cargo test
cargo clippy
...
    ↓
ValidationEvidence
```

Architecture:

```text
Agent
  │
  │ modifies files
  ▼
Workspace
  │
  ▼
Orbit Validator
  │
  ├── compile
  ├── tests
  ├── lint
  └── configured acceptance commands
  │
  ▼
Evidence
```

This is essential because validation evidence is what makes cross-agent continuation reliable.

---

# 9. Fallback Policy

Do not hardcode:

```text
if antigravity fails:
    run codex
```

Introduce a generic execution/fallback policy.

Initial implementation can be simple.

Example conceptual configuration:

```yaml
agents:
  - id: antigravity-primary
    adapter: antigravity

  - id: codex-fallback
    adapter: codex

execution_policy:
  strategy:
    - agent: antigravity-primary

    - agent: codex-fallback
      continue_workspace: true
      on:
        - rate_limited
        - quota_exhausted
        - turn_limit
        - timeout
        - validation_failed
```

Do not over-engineer the configuration format if Orbit already has an execution configuration abstraction.

Reuse existing structures where appropriate.

---

# 10. Distinguish Infrastructure Failure From Implementation Failure

Fallback behavior should understand failure categories.

Example classification:

```text
Provider failures
-----------------
rate_limited
quota_exhausted
turn_limit

Agent failures
--------------
agent_error
process_crash

Infrastructure failures
-----------------------
worker_lost
container_failure
storage_failure

Implementation failures
-----------------------
validation_failed

User/system actions
-------------------
cancelled
timeout
```

Different policies may apply.

For example:

```text
rate_limited
    → immediately try another provider

quota_exhausted
    → immediately try another provider

turn_limit
    → continue with another agent

validation_failed
    → optionally allow another agent

credential_error
    → usually do not blindly retry

cancelled
    → never fallback automatically
```

Keep policy separate from classification.

---

# 11. Prevent Infinite Agent Loops

The orchestrator MUST enforce limits.

At minimum:

```text
max_agent_executions_per_attempt
max_total_attempt_duration
max_same_failure_repetitions
```

Example:

```yaml
max_agent_executions: 3
max_same_failure_repetitions: 2
```

Never allow:

```text
Gemini → Codex → Gemini → Codex → ...
```

forever.

---

# 12. Failure Fingerprints

Add a basic failure fingerprint abstraction.

Purpose:

Detect when multiple agents repeatedly produce essentially the same validation failure.

Initial implementation does not need AI.

Normalize:

```text
validation command
exit code
failing test names
compiler diagnostic codes
important error lines
```

Then hash the normalized representation.

Example:

```text
cargo test
E0308
src/auth.rs
mismatched types
```

becomes:

```text
failure_fingerprint =
SHA256(normalized_failure)
```

Store it with validation evidence.

This allows future policies such as:

```text
same failure twice
       ↓
switch provider/model family
```

Do not make sophisticated routing mandatory in the first implementation.

Build the data model so it becomes possible.

---

# 13. Evidence Chain

Every agent execution should produce an auditable chain:

```text
AgentExecution #1
      │
      ├── logs
      ├── workspace snapshot
      └── validation #1
              │
              ▼
          Handoff #1
              │
              ▼
AgentExecution #2
      │
      ├── logs
      ├── workspace snapshot
      └── validation #2
```

This should be visible through `orbit inspect`.

Example desired output:

```text
Attempt: 45578...
Status: succeeded

Execution chain:

1. antigravity
   status: interrupted
   reason: turn_limit
   duration: 12m31s

   workspace:
     4 files changed

   validation:
     FAILED
     cargo test
     E0308 src/auth.rs

2. codex
   status: completed
   duration: 4m12s

   workspace:
     5 files changed

   validation:
     PASSED
     84 tests
```

If `orbit inspect` currently emits JSON, expose equivalent structured fields there first.

Pretty CLI formatting can come later.

---

# 14. Concurrency and Ownership

Only one agent may mutate an attempt workspace at a time.

Enforce:

```text
Attempt workspace
       │
       ├── Agent A running  ← lock owner
       │
       └── Agent B          ← cannot start
```

Before fallback:

```text
Agent A terminated
       ↓
logs flushed
       ↓
workspace snapshot
       ↓
validation
       ↓
handoff persisted
       ↓
release/transfer execution ownership
       ↓
Agent B starts
```

Never run two fallback agents simultaneously against the same writable workspace.

Parallel agents require separate workspace branches/copies and are outside the scope of this change.

---

# 15. Crash Recovery

Design continuation so Orbit itself may restart between executions.

This sequence must work:

```text
Antigravity exits
      ↓
handoff persisted
      ↓
Orbit crashes/restarts
      ↓
scheduler reloads Attempt
      ↓
sees pending fallback
      ↓
launches Codex
```

Therefore do not keep fallback state only in process memory.

Persist enough orchestration state to determine:

```text
current execution
last completed execution
last validation
next eligible fallback
execution count
```

Follow the existing Orbit persistence patterns rather than introducing an unrelated state system.

---

# 16. Agent Adapter Contract

Agent adapters should have a common conceptual interface.

Something equivalent to:

```rust
trait AgentAdapter {
    async fn execute(
        &self,
        context: AgentExecutionContext,
    ) -> Result<AgentExecutionResult>;
}
```

Context should contain things such as:

```text
workspace path
task specification
handoff context
credentials
execution limits
environment
```

Result should contain normalized information:

```text
status
termination reason
exit code
provider metadata
usage metadata if available
logs/artifacts
```

Provider-specific concepts MUST NOT leak into the scheduler unless necessary.

For example, the scheduler should understand:

```text
TerminationReason::RateLimited
```

not:

```text
GeminiError::Http429RESOURCE_EXHAUSTED
```

---

# 17. Credential Isolation

Each execution must receive only credentials required by its selected adapter.

Example:

```text
Attempt
   │
   ├── execution: antigravity
   │      credentials:
   │      ~/.orbit/credentials/antigravity
   │
   └── execution: codex
          credentials:
          ~/.orbit/credentials/codex
```

Do NOT expose every provider credential to every worker.

Cross-agent continuation shares:

```text
workspace
task
evidence
```

It does NOT imply sharing provider credentials.

---

# 18. Security

Treat previous agent output as untrusted execution output.

The next agent may inspect source changes, but Orbit should not automatically interpret arbitrary repository content as trusted orchestration commands.

In particular, a previous agent must not be able to modify repository files to change:

```text
which credentials are mounted
which host paths are mounted
worker security profile
fallback policy
secret access
Orbit daemon configuration
```

unless those changes pass through an explicit trusted configuration mechanism.

Keep control-plane configuration separate from the mutable repository workspace.

---

# 19. Minimum Viable Implementation

Implement this incrementally.

## Phase 1 — Data Model

Add:

```text
AgentExecution
termination reason
execution sequence
workspace snapshot metadata
handoff record
validation linkage
```

Preserve compatibility with existing attempts.

## Phase 2 — Sequential Continuation

Support exactly:

```text
primary agent
      ↓ failure
one fallback agent
```

Example:

```text
Antigravity → Codex
```

Reuse the same workspace.

No sophisticated routing yet.

## Phase 3 — External Validation

Ensure Orbit executes validation between executions and persists the result.

## Phase 4 — Generic Fallback Chain

Allow:

```text
agent A
  ↓
agent B
  ↓
agent C
```

with execution limits.

## Phase 5 — Failure Fingerprints

Detect repeated validation failures.

## Phase 6 — Policy-Based Routing

Later support policies such as:

```text
quota       → different provider
turn limit  → continuation
same failure twice → different model family
validation failure → stronger coding agent
```

Do not implement an unnecessarily complex policy engine in Phase 1.

---

# 20. Required Tests

Add unit and integration tests covering at least the following.

### Successful primary execution

```text
Antigravity
    ↓
validation passes
    ↓
no Codex execution
```

### Primary fails, fallback succeeds

```text
Antigravity
    ↓
turn_limit
    ↓
workspace preserved
    ↓
Codex
    ↓
validation passes
```

Verify Codex sees files modified by Antigravity.

### Validation failure fallback

```text
Agent A modifies code
    ↓
cargo test fails
    ↓
failure evidence persisted
    ↓
Agent B receives handoff
```

### Quota fallback

```text
Agent A → quota_exhausted
Agent B starts
```

### Cancellation

```text
Agent A → cancelled
```

Verify no automatic fallback occurs.

### Credential failure

Verify policy behavior and ensure no infinite retry.

### Workspace preservation

Explicitly verify:

```text
modified files
untracked files
staged files
```

survive the handoff.

### Execution limit

Given:

```text
max_agent_executions = 2
```

verify a third agent cannot start.

### Crash recovery

Persist:

```text
Agent A finished
handoff created
fallback pending
```

restart orchestration state and verify Agent B can continue.

### Concurrent ownership

Verify two agents cannot obtain writable ownership of the same attempt workspace simultaneously.

---

# 21. Observability

Expose metrics/log fields for:

```text
task_id
attempt_id
agent_execution_id
agent_type
provider
model
execution_sequence
termination_reason
fallback_triggered
fallback_from
fallback_to
validation_status
failure_fingerprint
```

Useful future metrics:

```text
orbit_agent_executions_total
orbit_agent_fallbacks_total
orbit_agent_termination_total
orbit_cross_agent_recovery_total
```

Do not introduce high-cardinality labels such as task IDs into Prometheus labels.

Keep IDs in structured logs/traces.

---

# 22. Definition of Done

The implementation is complete when this scenario works end-to-end:

```text
1. Orbit creates an attempt.

2. Orbit creates one workspace.

3. Antigravity starts in that workspace.

4. Antigravity modifies repository files.

5. Antigravity terminates with a normalized
   retryable/fallback reason.

6. Orbit preserves the workspace.

7. Orbit runs configured validation.

8. Orbit stores:
   - Antigravity execution result
   - workspace snapshot
   - git diff artifact
   - validation evidence
   - handoff record

9. Orbit selects Codex according to policy.

10. Codex starts in the SAME workspace.

11. Codex receives:
    - original task
    - current repository
    - previous execution information
    - validation evidence

12. Codex continues/fixes the implementation.

13. Orbit independently runs validation again.

14. Validation passes.

15. Attempt becomes successful.

16. `orbit inspect <attempt>` shows both
    agent executions and their evidence.
```

Most importantly, prove with an integration test that Codex can continue the workspace after Antigravity without receiving Antigravity's internal conversation/session.

---

# 23. Architectural Constraints

Keep these invariants explicit in code and documentation:

```text
Orbit owns state.
Agents own no durable task state.

Workspace is attempt-scoped.
Agent process is execution-scoped.

Validation is performed by Orbit.
Agent claims are not validation evidence.

Fallback preserves workspace by default.

Provider errors are normalized at adapter boundaries.

Credentials remain provider-isolated.

Only one agent may mutate an attempt workspace at once.

Fallback state survives Orbit restart.

Cross-agent continuation never requires provider conversation history.
```

Do not tightly couple the implementation specifically to Antigravity → Codex.

Those two adapters should be the first integration test, but the architecture must allow:

```text
Antigravity
Codex
Claude ACP
Gemini CLI
future ACP agents
future native adapters
```

without redesigning the attempt lifecycle.

---

# 24. Before Coding

First inspect the existing Orbit implementation and identify:

1. Current `Task` and `Attempt` models.
2. Where agent/worker identity is currently attached.
3. Workspace lifecycle and cleanup behavior.
4. Artifact/evidence persistence.
5. Validation/test execution.
6. Worker lease/ownership behavior.
7. Antigravity adapter.
8. Codex adapter.
9. Credential mounting.
10. `orbit inspect` serialization.
11. Restart/recovery logic.
12. Database migrations required.

Then produce a short implementation map showing:

```text
existing component
→ required change
→ files/modules affected
→ migration impact
```

Do not create duplicate abstractions when Orbit already has an equivalent concept.

After that analysis, implement the smallest coherent version:

```text
Antigravity
      ↓
normalized fallback condition
      ↓
persistent handoff
      ↓
same workspace
      ↓
Codex
      ↓
Orbit validation
```

Run the existing test suite plus the new cross-agent continuation integration tests before considering the implementation complete.
