Implement Phase B1 for Orbit:

    PHASE B1 — ISOLATED COMMAND EXECUTION + VERIFICATION EVIDENCE

This phase establishes the execution/verification substrate that later RoleAgent
and multi-agent workflows will depend on.

Do NOT start implementing planner/reviewer roles yet.

Do NOT build a generic workflow DAG yet.

Do NOT mix this phase with Antigravity/Codex provider-model capability work.

The objective is:

    agents may develop interactively,
    but Orbit independently executes verification,
    records durable evidence,
    and ties that evidence to the exact workspace state being verified.

======================================================================
0. ARCHITECTURAL PRINCIPLE
======================================================================

The key invariant is:

    An agent saying "tests pass" is NOT verification.

Only Orbit-produced execution evidence counts toward workflow completion.

The target model is:

    Task
      |
      v
    Attempt
      |
      +-- Workspace
      |
      +-- AgentExecution
      |     edit files
      |     run exploratory commands
      |
      +-- Workspace Snapshot
      |
      +-- VerificationRun
            |
            +-- VerificationStep
            +-- durable Evidence

Later phases will add:

    planner
    implementer
    reviewer
    regression gates

but Phase B1 must stand on its own.

======================================================================
1. PRESERVE CURRENT CORE MODEL
======================================================================

Keep these existing principles unchanged:

    Task != Attempt != AgentExecution

    Workspace belongs to Attempt
    not to AgentExecution

    Attempt may contain multiple sequential AgentExecutions

    only one agent mutates an Attempt workspace at once

    cross-agent continuation does not depend on provider chat history

    Orbit owns task state

Do not collapse VerificationRun into AgentExecution.

They are different concepts.

AgentExecution:

    provider/runtime-driven agent work

VerificationRun:

    Orbit-controlled command execution used to establish evidence

======================================================================
2. NEW DOMAIN CONCEPTS
======================================================================

Introduce explicit domain types roughly equivalent to:

    WorkspaceState
    VerificationPlan
    VerificationStep
    VerificationRun
    VerificationStepRun
    VerificationEvidence

Exact names may differ if existing naming conventions suggest better ones.

Recommended conceptual structure:

    VerificationPlan
      id
      version
      steps[]

    VerificationStep
      id
      name
      kind
      command / execution specification
      cwd
      env
      timeout
      required
      dependencies
      artifact capture policy

    VerificationRun
      id
      attempt_id
      workspace_state_id
      plan identity/version
      status
      started_at
      finished_at
      overall result

    VerificationStepRun
      id
      verification_run_id
      step_id
      status
      exit_code
      started_at
      finished_at
      duration
      stdout evidence
      stderr evidence
      artifacts

Do not over-generalize yet.

The first implementation should support command-oriented verification well.

======================================================================
3. VERIFICATION RESULT MODEL
======================================================================

Do not use only boolean pass/fail.

Use explicit states such as:

    PENDING
    RUNNING
    PASSED
    FAILED
    ERROR
    TIMED_OUT
    CANCELLED
    SKIPPED

At overall VerificationRun level, use a normalized result.

For example:

    PASSED
    FAILED
    ERROR
    TIMED_OUT
    CANCELLED

Distinguish:

    command exited non-zero
        -> FAILED

from:

    Orbit could not execute command
        -> ERROR

from:

    exceeded timeout
        -> TIMED_OUT

This distinction will matter later for retries/fallback.

======================================================================
4. COMMAND EXECUTION CONTRACT
======================================================================

Build a reusable bounded command execution abstraction.

It should support at minimum:

    argv / command
    working directory
    controlled environment variables
    timeout
    cancellation
    stdout capture
    stderr capture
    exit code
    start timestamp
    finish timestamp
    duration

Prefer argv-style execution over shell-string execution internally where
possible.

For example conceptually:

    CommandSpec {
        program
        args
        cwd
        env
        timeout
    }

If shell execution is required for compatibility, make it explicit:

    shell: true

Do not silently wrap every command in:

    sh -c

without recording that fact.

======================================================================
5. COMMAND SAFETY BOUNDARIES
======================================================================

Verification commands execute inside the Attempt's isolated execution
environment.

They must not gain:

    unrestricted host filesystem
    host process namespace
    host root
    unrestricted Docker socket
    arbitrary host Podman socket
    raw credential store access

Keep current rootless isolation direction.

Do not mount:

    /var/run/docker.sock

into agent or verification environments.

Do not weaken existing credential leasing boundaries.

Verification receives only environment/resources explicitly needed by policy.

======================================================================
6. AGENT DEVELOPMENT COMMANDS VS VERIFICATION COMMANDS
======================================================================

Preserve a hard semantic distinction:

    AgentExecution command
        exploratory / implementation activity

    VerificationRun command
        Orbit-controlled evidence

Example:

Agent does:

    cargo test parser_test

That command may be recorded in AgentExecution logs.

But it does NOT satisfy required verification.

After implementation:

    Orbit runs:
      cargo test --workspace

That VerificationRun is the evidence.

This distinction should be represented in code/data rather than only
documented.

======================================================================
7. WORKSPACE STATE BINDING
======================================================================

This is critical.

A VerificationRun must be bound to the exact workspace state it verified.

Do not allow:

    verify workspace
    then mutate workspace
    then reuse old verification as proof

Introduce a durable workspace identity.

Possible implementations:

    immutable workspace snapshot
    content digest
    Git tree/commit-like identity
    existing Orbit workspace snapshot mechanism

Prefer reusing the existing continuation snapshot/handoff machinery if it can
reliably identify exact workspace state.

A VerificationRun must record:

    workspace_state_id

or equivalent immutable identity.

Core invariant:

    verification evidence is valid only for the workspace state it was run on.

======================================================================
8. MUTATION INVALIDATION
======================================================================

After a successful VerificationRun:

    WorkspaceState A
      -> VerificationRun PASS

if the workspace changes to:

    WorkspaceState B

then VerificationRun(A) remains durable historical evidence,
but MUST NOT satisfy completion requirements for B.

Do not delete old evidence.

Instead treat it as:

    valid_for_workspace_state = A

Later workflow logic can decide current verification freshness by comparing
workspace state identities.

Add tests for this invariant.

======================================================================
9. WORKSPACE SNAPSHOT TIMING
======================================================================

Recommended sequence:

    AgentExecution finishes implementation
        |
        v
    Orbit determines/freezes WorkspaceState X
        |
        v
    VerificationRun references X
        |
        v
    commands execute
        |
        v
    Evidence persisted against X

If current Attempt workspace cannot be made immutable during verification,
enforce that no agent mutation occurs concurrently.

Current design already expects one mutating agent at a time.

Extend this to:

    no mutation while final verification is running

unless using an isolated immutable snapshot copy.

======================================================================
10. FIRST VERIFICATION STEP TYPE
======================================================================

Start with:

    COMMAND

Do not attempt to support every future check type immediately.

Example:

    VerificationStep {
        kind: COMMAND
        command: ["cargo", "test", "--workspace"]
    }

Later phases may add:

    SERVICE
    HTTP_PROBE
    BROWSER
    CONTAINER_ENVIRONMENT

but do not make B1 depend on them.

Design the type system so these can be added later without a destructive
migration.

======================================================================
11. VERIFICATION PLAN SOURCE
======================================================================

For B1, support a programmatic/static VerificationPlan.

Do not yet build full repository policy discovery.

It is enough to be able to construct:

    VerificationPlan {
      steps: [
        cargo fmt,
        cargo clippy,
        cargo test
      ]
    }

via API/domain/test fixture.

If there is already a natural place for task-level verification config,
integrate minimally.

Do NOT yet trust arbitrary repository `.orbit/verify.yaml` as an authoritative
completion policy.

That needs provenance/security rules later.

======================================================================
12. REQUIRED VS OPTIONAL STEPS
======================================================================

VerificationStep should know whether it is:

    REQUIRED
    OPTIONAL

Overall PASS rule for B1:

    all required steps PASSED

Optional failures may be recorded without failing the whole run,
depending on existing policy.

Keep logic explicit and tested.

Example:

    fmt       required
    clippy    required
    tests     required
    coverage  optional

======================================================================
13. STEP DEPENDENCIES
======================================================================

For B1, sequential execution is acceptable.

Do not implement parallel scheduling yet.

Allow simple ordered steps.

If useful, reserve a dependency model:

    depends_on[]

but execution may still be sequential.

Example:

    cargo fmt
      ->
    cargo clippy
      ->
    cargo test

If a required earlier step fails, choose and document one policy:

Preferred:

    stop execution after first required failure

unless there is a strong reason to continue.

Alternative:

    continue collecting evidence

Either is acceptable, but behavior must be deterministic.

I recommend:

    fail-fast for required steps in B1

with potential future:

    collect-all

mode.

======================================================================
14. TIMEOUTS
======================================================================

Every step must have a bounded timeout.

Do not allow an unbounded verification command.

Support:

    per-step timeout

and optionally:

    run-level timeout

Use sane defaults if no explicit value is provided.

Do not hard-code language-specific timeout values.

Example:

    VerificationStep.timeout = 15m

When timeout occurs:

    terminate child process/process group
    collect available stdout/stderr
    mark TIMED_OUT
    continue cleanup
    persist evidence

No orphan processes.

======================================================================
15. PROCESS TREE CLEANUP
======================================================================

This matters for code that starts child processes.

If a verification command starts:

    server
    worker
    subprocess

and then times out/cancels/fails,
Orbit must clean up the full process group/cgroup/container execution.

Do not only kill the immediate shell PID.

Prefer using the isolation runtime's process/cgroup boundary where possible.

Add a test with a child process to prove cleanup.

======================================================================
16. STDOUT / STDERR EVIDENCE
======================================================================

Capture:

    stdout
    stderr

separately.

Do not store unlimited output directly in PostgreSQL.

Use bounded inline metadata plus artifact storage if the output is large.

Recommended:

    small output:
      bounded inline preview

    full output:
      artifact/object storage

Persist metadata such as:

    byte count
    truncated flag
    artifact reference
    first/last bounded preview if useful

Do not silently discard large output.

======================================================================
17. OUTPUT LIMITS
======================================================================

Introduce explicit output budgets.

Example conceptually:

    max_inline_stdout
    max_inline_stderr
    max_total_artifact_bytes

Exact values may follow existing Orbit conventions.

The important rule:

    verification output is bounded

If output exceeds inline budget:

    continue command if policy allows
    stream/spool to bounded artifact
    mark preview as truncated

Do not let a command OOM the worker by printing endlessly.

======================================================================
18. DURABLE EVIDENCE
======================================================================

Verification results must survive:

    worker restart
    API restart
    CLI restart

Persist durable records in PostgreSQL.

Artifacts may live in the configured object-storage backend.

Evidence must include enough to answer:

    what command ran?
    against which workspace state?
    when?
    where?
    what exit status?
    how long?
    what output?
    what final result?

Do not rely solely on Temporal history for durable operator inspection.

======================================================================
19. ENVIRONMENT IDENTITY
======================================================================

A VerificationRun should record enough environment identity to make results
meaningful.

At minimum capture non-secret metadata such as:

    execution profile
    worker/build identity
    container/runtime image digest if applicable
    architecture/platform
    Orbit version/build SHA where available

Do not overbuild a full reproducibility manifest yet.

Do not persist secrets/environment secret values.

======================================================================
20. ENVIRONMENT VARIABLES
======================================================================

Support explicit environment variables for verification steps.

Do not dump the complete process environment into evidence.

Persist only:

    variable names
    non-sensitive approved values if policy says safe

Never persist leased secret contents.

Prefer policy like:

    evidence.env_keys = ["RUST_BACKTRACE", "CI"]

not raw secret-bearing env.

======================================================================
21. CREDENTIALS DURING VERIFICATION
======================================================================

Default:

    VerificationRun receives NO provider credentials.

Typical code tests do not need:

    Codex auth
    Antigravity auth

Do not automatically lease coding-agent credentials into verification.

If a future verification step requires an external credential,
that must be an explicit capability/policy decision.

This is out of scope for initial B1.

======================================================================
22. NETWORK POLICY
======================================================================

Preserve existing sandbox network restrictions.

Do not automatically give verification unrestricted outbound network.

For B1, use current execution-profile behavior.

Record the effective network profile as non-secret evidence if available.

Do not implement a large network-policy engine in this phase.

======================================================================
23. AGENT SHELL EXECUTION SUPPORT
======================================================================

Existing coding agents need to be able to run development commands such as:

    cargo test
    pytest
    npm test
    go test

inside their isolated Attempt workspace.

If current ACP execution already supports shell/tool calls sufficiently,
do not duplicate it.

Instead:

    reuse/normalize the underlying execution primitive

where practical.

But keep data/evidence semantics separate:

    agent shell tool call
        belongs to AgentExecution

    verification command
        belongs to VerificationRun

======================================================================
24. DO NOT HARD-CODE LANGUAGE DETECTION
======================================================================

Do NOT make B1:

    if Cargo.toml:
        cargo test

    if pyproject.toml:
        pytest

    if package.json:
        npm test

That belongs to later policy/autodetection work.

B1 executes an explicit VerificationPlan.

This keeps the foundation generic.

======================================================================
25. EXAMPLE RUST VERIFICATION
======================================================================

The system should be able to execute a plan like:

    Plan: rust-standard

    Step 1:
      name: fmt
      command:
        cargo fmt --all -- --check
      required: true
      timeout: 2m

    Step 2:
      name: clippy
      command:
        cargo clippy --locked --all-targets --all-features -- -D warnings
      required: true
      timeout: 10m

    Step 3:
      name: tests
      command:
        cargo test --locked --all-targets --all-features
      required: true
      timeout: 20m

And persist evidence independently for each step.

======================================================================
26. EXAMPLE PYTHON VERIFICATION
======================================================================

The same substrate must support:

    Step:
      command:
        python -m pytest -q

without adding Python-specific core logic.

The command runner should treat this exactly like Rust from an orchestration
perspective.

======================================================================
27. LONG-RUNNING SERVER SUPPORT IN B1
======================================================================

We ultimately need services such as:

    python server
    Node dev server
    Rust API
    PostgreSQL
    Redis

However, do NOT implement full service orchestration in the first B1 checkpoint
unless current primitives make it trivial.

For B1 checkpoint 1:

    bounded command execution
    process-group cleanup

is required.

Explicit managed services/readiness probes belong to B1.2 / B5.

But design command execution so long-running service support is possible later.

======================================================================
28. VERIFICATION API / SERVICE INTERFACE
======================================================================

Expose a clean internal application/service interface.

Conceptually:

    create_verification_run(...)
    start_verification_run(...)
    get_verification_run(...)
    cancel_verification_run(...)
    list_verification_runs(attempt_id)

Do not expose raw worker internals to API callers.

If Orbit already has a command/service architecture, align with it.

A public HTTP endpoint is optional for this checkpoint.

Domain/service correctness is more important than broad API surface.

======================================================================
29. CLI SUPPORT
======================================================================

Add a minimal operator/developer CLI if practical.

For example:

    orbit verification run <attempt-id> --plan <fixture-or-id>

    orbit verification show <run-id>

or another style consistent with current CLI.

Do not spend large effort on final UX.

A minimal inspection interface is useful for qualification.

At minimum, tests/services must be callable without a role agent.

======================================================================
30. VERIFICATION SHOW OUTPUT
======================================================================

Human-readable output should roughly communicate:

    Verification <id>
    Attempt       <attempt>
    Workspace     <workspace-state>
    Result        PASSED
    Duration      42.8s

    STEPS
    NAME        RESULT   DURATION   EXIT
    fmt         PASSED   0.4s       0
    clippy      PASSED   12.1s      0
    tests       PASSED   30.3s      0

Do not dump full stdout/stderr by default.

Allow deeper inspection separately.

======================================================================
31. TEMPORAL INTEGRATION
======================================================================

Use Temporal only where it adds value.

A VerificationRun may be orchestrated by Temporal,
especially for:

    cancellation
    durable retry semantics
    worker-loss recovery
    long-running execution

But do not make Temporal history the sole source of evidence.

Persist final and intermediate execution records in PostgreSQL.

If existing execution infrastructure already provides durable worker execution,
reuse it.

Avoid introducing a second unrelated worker framework.

======================================================================
32. RETRIES
======================================================================

Do NOT automatically retry a failed test command just because it failed.

A deterministic test failure:

    exit_code != 0

should remain FAILED.

Infrastructure/transient ERROR may be eligible for retry later.

For B1:

    no automatic semantic retry

is the safest default.

If Temporal retries activities for infrastructure reasons,
ensure it cannot create duplicate ambiguous evidence.

Use stable step-run attempt identity.

======================================================================
33. IDEMPOTENCY
======================================================================

Verification run creation/start must be safe under retries.

Do not accidentally execute the same verification step multiple times while
recording only one opaque result.

Use stable identifiers.

Suggested hierarchy:

    verification_run_id
      step_run_id
        execution_attempt

If infrastructure retries happen, preserve evidence that a retry occurred.

Do not falsely present duplicate execution as one atomic command.

======================================================================
34. CANCELLATION
======================================================================

Support explicit VerificationRun cancellation.

Cancellation should:

    signal running command
    terminate full process group
    capture available output
    persist CANCELLED
    cleanup execution resources

Cancellation must not leave:

    server processes
    child processes
    temp containers
    open leases

behind.

======================================================================
35. WORKER LOSS
======================================================================

Define behavior when the worker disappears during a command.

At minimum:

    do not mark PASSED

Persist/reconcile to something equivalent to:

    ERROR
    WORKER_LOST

if exact process result cannot be established.

This can reuse current worker-loss/recovery mechanisms where applicable.

Do not invent success from missing evidence.

======================================================================
36. SECURITY: REPOSITORY IS UNTRUSTED
======================================================================

Remember:

    repository content is untrusted input.

A verification command may execute arbitrary repository code.

Therefore VerificationRun must execute under the same or stronger isolation
boundary as agent work.

Do not execute verification commands directly on the Orbit host shell.

Do not source arbitrary repository scripts in the control-plane process.

All repository code execution belongs inside the isolated execution runtime.

======================================================================
37. SECURITY: BUILD FILES ARE CODE
======================================================================

Treat:

    build.rs
    Makefile
    package scripts
    setup.py
    pyproject hooks
    Dockerfile
    test fixtures

as executable untrusted code.

The fact that a step is called:

    cargo test

does not make it safe to run outside the sandbox.

======================================================================
38. CURRENT EXECUTION PROFILE
======================================================================

Use the current practical execution profile:

    trusted
      rootless Podman + current runtime

Do not block B1 on:

    gVisor
    Firecracker

Keep sandboxed/untrusted profiles future-compatible.

Record profile identity in VerificationRun evidence.

======================================================================
39. ARTIFACT MODEL
======================================================================

A VerificationStep may produce artifacts.

For B1 minimally support:

    stdout
    stderr

Design artifact references so later we can add:

    junit.xml
    coverage.xml
    screenshots
    Playwright traces
    crash dumps
    benchmark reports

Do not hard-code evidence to text only.

======================================================================
40. LOG REDACTION
======================================================================

Verification output may accidentally print secrets.

Reuse existing redaction/sanitization infrastructure if available.

At minimum:

    do not intentionally inject provider secrets
    redact known leased secret material where possible
    bound output
    document remaining limitations

Do not persist credential backend paths or secret contents as evidence.

======================================================================
41. DATABASE SCHEMA
======================================================================

Add migrations as required.

Likely tables:

    verification_plans
    verification_runs
    verification_step_runs

or equivalent normalized design.

If plans are not yet intended to be durable entities, they may be embedded as a
versioned snapshot on VerificationRun.

Important:

A run should preserve the plan it actually executed.

Do not let later plan mutation rewrite history.

Store:

    plan version / immutable snapshot

======================================================================
42. RUN PLAN IMMUTABILITY
======================================================================

Once a VerificationRun starts:

    its plan is immutable.

If a user wants different steps:

    create another VerificationRun

Do not edit a running/completed verification plan in place.

======================================================================
43. COMPLETION POLICY IS NOT YET THE WORKFLOW ENGINE
======================================================================

Do not implement full Task completion gating yet.

But expose enough information for the next phase to answer:

    does current WorkspaceState X have a PASSED required VerificationRun?

Add a domain query/helper such as:

    latest_passing_verification_for_workspace(...)

or:

    workspace_verification_status(...)

Do not silently mark the Task complete in B1.

======================================================================
44. FAST CHECK VS REGRESSION
======================================================================

Do not implement multiple verification classes deeply yet.

But reserve a classification:

    CHECK
    REGRESSION
    RELEASE

or similar if inexpensive.

For initial qualification, one generic verification kind is enough.

The key is not to make schema impossible to extend later.

======================================================================
45. TESTING STRATEGY
======================================================================

Add strong tests for the substrate.

Unit tests:

    command spec validation
    timeout handling
    result normalization
    required/optional aggregation
    workspace-state matching
    invalidation after workspace mutation
    output truncation
    plan immutability

Integration tests:

    successful command
    non-zero exit
    timeout
    cancellation
    stdout + stderr
    child process cleanup
    large output
    worker error simulation where possible

Persistence tests:

    VerificationRun survives service restart / reload
    step results round-trip
    workspace identity round-trip
    plan snapshot round-trip

======================================================================
46. QUALIFICATION FIXTURE
======================================================================

Create a small deterministic test repository/fixture.

Example:

    fixture/
      Cargo.toml
      src/lib.rs
      tests/

or a simpler shell-based fixture.

Qualification cases:

A. PASS:

    command exits 0

B. FAIL:

    test exits 1

C. TIMEOUT:

    command sleeps beyond timeout

D. OUTPUT:

    command writes stdout and stderr

E. CHILD PROCESS:

    command starts child then hangs
    cancellation/timeout cleans everything

F. MUTATION INVALIDATION:

    verify WorkspaceState A
    mutate file
    create WorkspaceState B
    prove A evidence does not satisfy B

======================================================================
47. NO FAKE SUCCESS
======================================================================

Explicit invariants:

    missing exit status != PASSED

    worker loss != PASSED

    timeout != PASSED

    agent claim != PASSED

    stale workspace verification != current PASSED

    optional test success cannot override required failure

Add assertions around these.

======================================================================
48. OBSERVABILITY
======================================================================

Emit bounded internal events/metrics where existing infrastructure supports it.

Potential events:

    verification.run.created
    verification.run.started
    verification.step.started
    verification.step.completed
    verification.run.completed
    verification.run.cancelled

Do not build a large new telemetry system.

Reuse current event/metrics conventions.

======================================================================
49. AUDITABILITY
======================================================================

An operator inspecting a completed run must be able to reconstruct:

    which Attempt
    which workspace state
    which plan
    which commands
    which environment identity
    which results
    which artifacts
    which timing

without asking the agent.

That is the definition of usable verification evidence.

======================================================================
50. B1 CHECKPOINT SCOPE
======================================================================

For this first checkpoint, MUST implement:

    [1] VerificationRun domain model

    [2] VerificationStep / plan snapshot

    [3] bounded isolated COMMAND execution

    [4] timeout

    [5] cancellation

    [6] process-tree cleanup

    [7] stdout/stderr capture

    [8] durable PostgreSQL evidence

    [9] workspace-state binding

    [10] stale-evidence invalidation semantics

    [11] environment identity metadata

    [12] unit + integration tests

    [13] minimal operator inspection path

May defer:

    managed services
    HTTP readiness probes
    database dependency orchestration
    container-compose test environments
    Playwright/headless browser
    coverage analysis
    test auto-detection
    repo-owned verify.yaml
    reviewer-generated tests
    affected-test selection
    arbitrary DAGs
    parallel verification steps

======================================================================
51. DO NOT BREAK CURRENT BASELINE
======================================================================

Existing functionality must remain intact:

    Codex ACP execution
    Antigravity ACP execution
    agy CLI representation/status
    credential registry
    provider availability
    quota status
    cross-agent continuation
    workspace handoff/snapshot behavior
    runtime capability discovery

Do not refactor unrelated provider code.

======================================================================
52. VALIDATION
======================================================================

Run:

    cargo fmt --all -- --check

    cargo clippy --locked --all-targets --all-features -- -D warnings

    cargo test --locked --all-targets --all-features

    git diff --check

Also run the deterministic B1 qualification fixture.

Capture concise evidence for:

    PASS
    FAIL
    TIMEOUT
    CANCEL
    OUTPUT
    WORKSPACE INVALIDATION

======================================================================
53. DEFINITION OF DONE
======================================================================

Phase B1 checkpoint is complete when all of these are true:

    1. Orbit can execute an explicit verification command inside the isolated
       Attempt environment.

    2. Orbit records:
         command
         cwd
         timestamps
         duration
         exit status
         stdout/stderr evidence
         result

    3. Execution is bounded by timeout.

    4. Cancellation cleans up the entire process tree.

    5. Evidence is durable.

    6. Evidence is tied to exact WorkspaceState.

    7. Mutation after verification prevents old evidence from satisfying the
       new workspace state.

    8. AgentExecution claims are not treated as VerificationRun evidence.

    9. Missing/incomplete execution evidence can never become PASSED.

    10. No host Docker/Podman socket or host shell escape is introduced.

    11. Existing provider/credential functionality still passes.

======================================================================
54. RETURN FORMAT
======================================================================

On success return:

    PHASE_B1_VERIFICATION_SUBSTRATE_COMPLETE

Include:

    architecture summary

    new modules/files

    migrations

    VerificationRun model

    workspace-state binding strategy

    execution isolation strategy

    timeout strategy

    cancellation/process-tree cleanup strategy

    stdout/stderr evidence strategy

    persistence strategy

    environment identity captured

    CLI/API inspection path

    tests added

    qualification results:
      PASS
      FAIL
      TIMEOUT
      CANCEL
      OUTPUT
      WORKSPACE_INVALIDATION

    deferred items

    validation:
      fmt
      clippy
      tests
      diff-check

Also report any architectural deviation from this plan before extending scope.

The central Phase B1 rule is:

    AGENTS MAY IMPLEMENT AND TEST INTERACTIVELY,
    BUT ONLY ORBIT-CONTROLLED VERIFICATION EXECUTION PRODUCES
    COMPLETION-GRADE EVIDENCE.
