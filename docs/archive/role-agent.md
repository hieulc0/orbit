You are acting as the architecture/planning agent for Orbit.

Your task is to inspect the current Orbit repository and write the design and
requirements for the next major evolution of Orbit:

    Agent Availability, Role-Based Orchestration, and Self-Development

This is a DOCUMENTATION-ONLY task.

Do NOT implement the feature.
Do NOT modify runtime behavior.
Do NOT add migrations.
Do NOT add CLI commands.
Do NOT refactor existing code.
Do NOT change tests except if absolutely required by an existing documentation
validation mechanism.
Do NOT dispatch another model/agent.
Do NOT perform live provider requests that consume quota.

The purpose of this task is to turn the vision below into a concrete,
implementation-ready architecture document grounded in the CURRENT Orbit
codebase.

======================================================================
BASELINE
======================================================================

Start from the current repository state.

The pre-Q7 hardening baseline is:

    833ffe7721be34780dc1b2f511c55f5bb0c11464

Verify the repository state before beginning.

Inspect the existing implementation rather than assuming abstractions exist.

In particular, locate and understand the current implementation of:

- Task
- Attempt
- AgentExecution
- execution/termination status
- ExecutionResolver / runtime capability discovery
- AgentCandidate / continuation policy
- credentials and credential leases
- worker ownership
- Attempt leases
- heartbeat renewal
- fencing
- workspace/Git Attempts
- ACP execution
- Codex integration
- Antigravity integration
- brokered filesystem tools
- brokered terminal tools
- validation
- accepted artifacts
- WorkspaceSnapshot
- HandoffRecord
- continuation/recovery
- observability/accounting
- CLI inspection commands
- persisted run/workflow state

The resulting design MUST extend these concepts rather than casually replacing
them.

======================================================================
CURRENT ARCHITECTURAL INVARIANTS
======================================================================

Preserve these principles.

1. Orbit owns durable task state.

    The agent is not the owner of task state; Orbit is.

2. Keep these concepts distinct:

    Task
    Attempt
    AgentExecution

3. Workspace belongs to Attempt, not AgentExecution.

4. An Attempt may contain multiple sequential AgentExecutions.

5. Only one implementing agent may mutate an Attempt workspace at a time.

6. Provider conversation/session state is disposable.

7. Cross-agent continuation must work from Orbit-owned durable state and the
   workspace, not provider conversation history.

8. Validation is controlled by Orbit.

   Agent claims such as "tests passed" are not authoritative.

9. Validation must remain independently reproducible from:

       pinned baseline
       + exact accepted patch
       + predetermined validation definition

10. Credentials remain provider-isolated.

11. Runtime/model capabilities are discovered/resolved dynamically.

12. Runtime implementations/images remain immutable and pinned.

13. Capability mismatch fails closed.

14. Missing provider usage is UNKNOWN/null.

    Never infer unknown usage as zero.

15. Security policy is controlled by Orbit, not by repository content,
    previous agents, planner output, reviewer output, or provider responses.

16. Continuation and verification are different concepts.

    Continuation:
        an execution cannot continue; another eligible execution takes over.

    Verification:
        implementation completed; validation/review determines correctness.

17. Existing lease/fencing/ownership semantics must remain authoritative.

18. Do not weaken rootless execution, workspace isolation, path jailing,
    credential isolation, tool allowlists, network policy, or accepted-artifact
    identity checks.

======================================================================
VISION
======================================================================

Orbit currently has a strong execution plane.

The next destination is a control plane capable of turning a human-defined
goal into independently verified engineering work.

Long-term:

                         Human
                           |
                          Goal
                           |
                           v
                  +----------------+
                  |     ORBIT      |
                  | control plane  |
                  +-------+--------+
                          |
              +-----------+-----------+
              |           |           |
              v           v           v
           Planner    Implementer   Reviewer
                          |
                          v
                       Tester
                          |
                          v
                      Validator
                          |
                   +------+------+
                   |             |
                  PASS          REPAIR
                   |             |
                   v             +-----> bounded loop
                  DONE

The important destination is:

    Orbit can develop Orbit.

This does NOT mean uncontrolled recursive autonomous modification.

It means Orbit can execute a bounded, policy-controlled engineering loop:

    Goal
      -> Plan
      -> Implement
      -> Validate
      -> Test/Review
      -> Repair when authorized
      -> Revalidate
      -> Finish or require human intervention

with:

- immutable starting revision
- explicit budgets
- isolated workspaces
- provider-isolated credentials
- independent validation
- independent review
- durable evidence
- deterministic security policy
- bounded repair cycles
- explicit human approval boundaries

======================================================================
CURRENT EXECUTION RESOURCES
======================================================================

The operator currently has approximately these credential resources:

    ~/.orbit/credentials/
        antigravity-ch9b2013
        antigravity-prvmrala
        antigravity-jc
        antigravity-weedy
        codex
        github-hieulc0

There are four Antigravity credentials and one Codex credential.

Do NOT encode these specific credential names as product architecture.

They are examples of why Orbit needs credential pools.

Current primary execution families include:

Antigravity:
    Gemini 3.8 Flash
    reasoning efforts such as low / medium / high

Codex:
    GPT-6 Luna
    multiple reasoning efforts

Other model families may exist or become available, including stronger models
appropriate for planning/review.

Do NOT hard-code today's model catalog into core architecture.

Model IDs must remain opaque runtime/provider data.

======================================================================
CORE CONCEPT: EXECUTION RESOURCE
======================================================================

The architecture should treat these as separate dimensions:

    Runtime
    Credential
    Model
    Reasoning Effort
    Role
    Availability
    Capability

For example:

    runtime:
        antigravity

    credential:
        antigravity-weedy

    model:
        gemini-3.8-flash

    reasoning_effort:
        high

is one executable candidate.

A credential is NOT an agent.

A model is NOT a role.

A runtime is NOT a credential.

A role is NOT a model.

Document this distinction explicitly.

A useful conceptual identity is:

    ExecutionResource =
        Runtime
        x Credential
        x Model
        x ReasoningEffort

subject to capabilities, availability, security policy, and current leases.

======================================================================
FEATURE 1 — AGENT / MODEL AVAILABILITY
======================================================================

Design a first-class availability subsystem.

Orbit needs to know whether an execution candidate is currently usable before
starting expensive work.

Desired CLI experience:

    orbit agents status

Example presentation:

    RUNTIME       CREDENTIAL              MODEL              5H LEFT  WEEK LEFT  RESET      HEALTH
    antigravity   antigravity-weedy       gemini-3.8-flash    82%      64%       03h21m     READY
    antigravity   antigravity-jc          gemini-3.8-flash    47%      91%       01h08m     READY
    antigravity   antigravity-prvmrala    gemini-3.8-flash     9%      31%       04h42m     LIMITED
    antigravity   antigravity-ch9b2013    gemini-3.8-flash     0%      26%       00h37m     COOLDOWN
    codex         codex                   gpt-6-luna           71%      53%       02h16m     READY

THIS IS AN EXAMPLE ONLY.

Do not assume every provider exposes both 5-hour and weekly windows.

Do not assume percentages or reset timestamps always exist.

UNKNOWN must be a valid result.

The architecture must support model-specific availability because different
models under one credential may have different limits.

Conceptually consider something similar to:

    AvailabilitySnapshot

containing:

    runtime
    credential_id
    model
    observed_at
    expires_at

    health/availability state

    quota windows[]

    evidence source

    optional provider reset information

Possible availability states:

    READY
    LIMITED
    COOLDOWN
    RATE_LIMITED
    QUOTA_EXHAUSTED
    AUTH_FAILED
    RUNTIME_UNAVAILABLE
    CAPABILITY_MISMATCH
    UNKNOWN

Do not blindly adopt this enum if existing Orbit types suggest a cleaner
representation.

======================================================================
QUOTA WINDOWS
======================================================================

The design should support arbitrary provider-defined windows rather than
hard-coding:

    5 hours
    weekly

Possible normalized representation:

    QuotaWindow {
        kind/provider_label
        used_percent?
        remaining_percent?
        resets_at?
    }

Important invariant:

    Unknown != zero.

If a provider reports only:

    exhausted

then Orbit records exhausted.

If it reports:

    reset_at

record reset_at.

If it reports:

    5h remaining percentage

record it.

Do NOT synthesize missing values.

======================================================================
AVAILABILITY EVIDENCE
======================================================================

Availability must carry provenance.

Potential sources include:

    provider/native status
    runtime-native status
    recent execution result
    operator override

For example, if an execution returns a normalized:

    QuotaExhausted

Orbit should immediately update availability for the relevant execution
resource.

If the provider supplies a reset timestamp, record it.

If it does not, do not invent one.

The architecture should define freshness and confidence semantics.

======================================================================
NATIVE QUOTA DISCOVERY
======================================================================

A key implementation question remains intentionally unresolved.

Codex has native status behavior such as /status.

Antigravity CLI has usage/quota behavior such as /usage or /quota.

However:

    DO NOT assume these are currently programmatically accessible through
    Orbit's ACP integrations.

This must be treated as a discovery requirement.

The first availability implementation phase should experimentally determine:

1. Can Codex usage/status be queried programmatically?

2. Can Antigravity usage/quota be queried programmatically?

3. Can this be done without consuming a model inference turn?

4. What structured information is actually available?

5. Does the information apply per credential, per model, or globally?

6. What reset timestamps/windows are exposed?

7. What authentication context is required?

8. Can the operation be executed safely inside the existing isolated runtime?

Prefer:

    native documented/runtime-supported status mechanism

over:

    reverse-engineered undocumented provider APIs.

Do not design Orbit around scraping unstable terminal text unless no better
interface exists.

If parsing CLI output becomes necessary, explicitly document the fragility and
adapter boundary.

======================================================================
HEALTH != QUOTA
======================================================================

Clearly distinguish:

    RuntimeHealth
    CredentialHealth
    CapabilityHealth
    Availability/Quota

For example:

    runtime image exists               YES
    executable launches                YES
    credential authenticated           YES
    model advertised                   YES
    required reasoning supported       YES
    quota remaining                    UNKNOWN

This execution resource may still be usable.

The architecture must not collapse all of these into one boolean "healthy".

======================================================================
AVAILABILITY CACHE
======================================================================

Do not perform an expensive provider probe for every scheduling decision.

Design cached AvailabilitySnapshot semantics.

For example:

    snapshot
        observed_at
        expires_at

A scheduler may use a sufficiently fresh snapshot.

Execution results should update availability immediately when they provide
stronger evidence.

Example:

    candidate READY
        |
        v
    execution
        |
        +-- QuotaExhausted
                 |
                 v
        candidate marked unavailable
                 |
                 v
        continuation/scheduler chooses next candidate

Define how stale information becomes UNKNOWN or triggers refresh.

Do not silently treat stale READY as permanently authoritative.

======================================================================
FEATURE 2 — CREDENTIAL POOLS
======================================================================

The four Antigravity credentials demonstrate the need for credential pools.

Conceptually:

    Gemini Flash implementation pool

        credential A
        credential B
        credential C
        credential D

Each credential/resource can have:

    current availability
    last successful use
    last normalized failure
    known cooldown/reset
    active leases
    capability information

The scheduler should be able to select another compatible credential when one
is unavailable.

Do not put provider credentials into repository/workflow content.

Credential selection remains Orbit policy.

======================================================================
FEATURE 3 — EXECUTION ROLES
======================================================================

Introduce/design first-class execution roles.

At minimum consider:

    Planner
    Implementer
    Tester
    Reviewer

Do not automatically assume they all need to exist as one enum if the current
architecture suggests a more extensible representation.

But the semantics must be explicit.

----------------------------------------------------------------------
PLANNER
----------------------------------------------------------------------

Planner produces a structured implementation plan.

Planner does NOT:

- change security policy
- grant mounts
- grant credentials
- disable validation
- increase its own budget
- select arbitrary host execution
- bypass role policy

Planner output is untrusted input to Orbit orchestration.

Prefer structured output over unrestricted prose.

Potential concepts:

    objective
    constraints
    acceptance criteria
    implementation steps
    expected validation
    risks

The planner should not own workflow policy.

----------------------------------------------------------------------
IMPLEMENTER
----------------------------------------------------------------------

Implementer mutates an Attempt workspace.

This is closest to today's coding AgentExecution.

Existing invariants remain:

    one mutating agent at a time
    workspace belongs to Attempt
    continuation may replace an interrupted implementer
    accepted patch is Orbit-owned evidence

----------------------------------------------------------------------
TESTER
----------------------------------------------------------------------

Tester is an agent role for exploratory/adversarial testing.

Tester is NOT the deterministic validator.

Potential responsibilities:

    inspect implementation
    devise edge cases
    run authorized tests
    look for behavioral failures
    produce structured findings

Whether Tester receives write access should be explicitly decided.

Preferred initial direction:

    no source mutation

unless there is a strong reason otherwise.

----------------------------------------------------------------------
REVIEWER
----------------------------------------------------------------------

Reviewer provides independent semantic review.

Reviewer should initially be READ-ONLY.

Reviewer examines:

    original task/goal
    accepted implementation patch
    relevant repository state
    deterministic validation report
    tester findings when present

Reviewer looks for:

    task satisfaction
    correctness
    architecture problems
    concurrency issues
    security regressions
    missing tests
    incorrect assumptions
    maintainability concerns

Reviewer MUST NOT silently repair the implementation.

Reviewer produces structured findings.

Potential shape:

    decision:
        approved
        changes_requested

    findings[]:
        severity
        category
        location?
        summary
        evidence
        suggested direction?

Do not make reviewer prose directly mutate task state without Orbit
interpretation.

======================================================================
VALIDATOR VS TESTER VS REVIEWER
======================================================================

Document this distinction clearly.

Validator:

    deterministic
    Orbit-controlled
    authoritative for configured checks

Examples:

    cargo fmt
    clippy
    cargo test
    task-specific acceptance commands

Tester:

    model-driven
    exploratory
    adversarial/behavioral
    structured findings

Reviewer:

    model-driven
    semantic/architectural
    structured findings
    initially read-only

An agent saying "tests pass" never replaces Validator.

======================================================================
FEATURE 4 — ROLE POLICY
======================================================================

Roles should specify requirements, not hard-coded permanent models.

Conceptual example:

    reviewer:
        requirements:
            reasoning: high
            workspace: read_only
            terminal: allowed
        candidates:
            - runtime: codex
              model: <strong-review-model>
              reasoning_effort: high
            - ...

This is illustrative.

The real design should reuse current capability discovery and AgentCandidate
concepts where appropriate.

Model identifiers remain opaque.

======================================================================
FEATURE 5 — ROLE SCHEDULER
======================================================================

Design a deterministic scheduler before considering an LLM-based router.

Candidate selection should account for:

    requested role
    runtime capability
    model capability
    reasoning-effort capability
    credential availability
    quota/cooldown state
    active credential/resource leases
    security policy
    continuation constraints
    operator policy

Conceptually:

    RoleRequirements
          +
    RuntimeCapabilities
          +
    ModelCapabilities
          +
    CredentialAvailability
          +
    ResourceAvailability
          +
    Policy
          |
          v
    resolved AgentCandidate

Selection should be explainable.

Orbit should be able to answer:

    Why was this candidate selected?

and:

    Why was this candidate rejected?

======================================================================
SCHEDULER EXPLAINABILITY
======================================================================

Design an inspection interface such as:

    orbit roles explain reviewer

or an equivalent command consistent with the existing CLI.

Example conceptual output:

    Role: reviewer

    Candidate: codex / sol-like-model / high
        health: READY
        capability: MATCH
        availability: READY
        selected: YES

    Candidate: antigravity / flash-model / high
        health: READY
        capability: MATCH
        selected: NO
        reason: lower role-policy priority

Do not encode the exact example syntax if it conflicts with current CLI
patterns.

Machine-readable JSON should be considered.

======================================================================
FEATURE 6 — CONTINUATION VS ROLE TRANSITION
======================================================================

Preserve the distinction.

Continuation:

    Implementer A starts work.
    Implementer A hits eligible failure.
    Implementer B continues the SAME Attempt workspace.

Role transition:

    Implementation completes.
    Validator runs.
    Reviewer runs.
    Reviewer requests changes.
    A new repair execution begins.

These are not the same state transition.

Do not overload the existing continuation machinery to mean repair/review.

Reuse primitives where appropriate, but preserve semantics.

======================================================================
FEATURE 7 — BOUNDED REPAIR LOOP
======================================================================

Long-term desired loop:

    PLAN
      |
      v
    IMPLEMENT
      |
      v
    VALIDATE
      |
      +---- failure ----+
      |                 |
      v                 v
    REVIEW            REPAIR
      |                 |
      +-- findings -----+
      |
      +-- approved --> DONE

After repair:

    revalidate
    then rereview when policy requires it

This loop MUST be bounded.

Potential goal/workflow budgets:

    max_planning_executions
    max_implementation_executions
    max_continuations
    max_repairs
    max_review_cycles
    max_tester_cycles
    max_wall_time
    provider/resource budgets

Do not create infinite:

    while findings:
        run_agent()

When the budget is exhausted:

    NEEDS_INTERVENTION

or an equivalent explicit terminal state.

======================================================================
FEATURE 8 — GOAL
======================================================================

Assess whether Orbit needs a durable concept above Task:

    Goal

Possible relationship:

    Goal
      -> planning
      -> one or more Tasks
      -> Attempts
      -> AgentExecutions
      -> Validation
      -> Review
      -> Repair
      -> terminal Goal result

Do NOT add Goal merely because this prompt names it.

Inspect the existing workflow/run/task model and determine whether:

    A. a new durable Goal aggregate is justified

or:

    B. existing workflow/run structures already provide the correct aggregate.

Document the decision and reasoning.

Avoid redundant state models.

======================================================================
FEATURE 9 — SELF-DEVELOPMENT SAFETY
======================================================================

Orbit eventually needs to be capable of developing its own repository.

This requires stronger boundaries, not weaker ones.

Document required invariants for self-development.

At minimum consider:

    immutable starting Git SHA
    isolated Git Attempt
    no direct mutation of operator checkout
    exact accepted patch identity
    independent validation workspace
    reviewer independence
    bounded loops
    credential isolation
    immutable runtime images
    no agent-controlled security policy
    no agent-controlled mounts
    no agent-controlled secrets
    no arbitrary host shell
    explicit network policy
    durable evidence
    human-controlled commit/release boundary

An agent modifying Orbit source MUST NOT automatically gain the ability to
modify the currently running Orbit control plane.

Discuss the bootstrap/trust boundary explicitly.

======================================================================
FEATURE 10 — HUMAN APPROVAL BOUNDARIES
======================================================================

The design should distinguish:

    autonomous execution

from:

    authorization to change high-impact system state.

Identify operations that should initially require explicit human approval.

Examples to evaluate:

    committing accepted Orbit changes
    pushing to remote
    merging
    changing runtime security policy
    changing credential policy
    increasing budgets
    enabling network
    adding mounts
    modifying deployment configuration
    releasing/deploying Orbit itself

Do not assume all of these permanently require approval.

Define a safe initial policy and possible future evolution.

======================================================================
FEATURE 11 — OBSERVABILITY
======================================================================

Extend existing observability concepts rather than creating an unrelated
telemetry subsystem.

The future system should make it possible to inspect:

    goal/workflow outcome
    role transitions
    selected execution resources
    rejected candidates and reasons
    availability snapshots used for decisions
    quota evidence
    planner execution
    implementation executions
    continuation chain
    tester executions
    validation results
    reviewer findings
    repair cycles
    budgets consumed
    provider-reported usage
    unknown usage
    final accepted artifact

Preserve bounded/sanitized durable diagnostics.

Do not persist prompts, credentials, raw tool arguments, or arbitrary terminal
output merely for convenience.

======================================================================
FEATURE 12 — CLI / OPERATOR EXPERIENCE
======================================================================

Design CLI requirements consistent with the current CLI.

Potential commands to evaluate:

    orbit agents list
    orbit agents status
    orbit agents status --json
    orbit agents status --refresh

    orbit credentials list
    orbit credentials status

    orbit models list
    orbit models capabilities

    orbit roles list
    orbit roles explain <role>

    orbit health

    orbit inspect <run>

Potential long-term command:

    orbit goal run ...

These are requirements candidates, not mandatory exact command names.

Inspect the existing CLI and recommend the smallest coherent extension.

======================================================================
PHASED DELIVERY
======================================================================

The design MUST propose an incremental implementation plan.

Do not attempt to deliver the entire vision in one phase.

At minimum evaluate a progression similar to:

Phase A — Native availability discovery spike

    Determine exactly how Codex and Antigravity expose quota/status.
    Prove whether it can be queried without consuming inference turns.
    Record actual provider/runtime semantics.

Phase B — Availability model

    normalized snapshots
    quota windows
    freshness
    evidence provenance
    execution-result updates

Phase C — CLI status

    human-readable status
    JSON status
    refresh behavior

Phase D — Credential/resource pools

    availability-aware candidate filtering
    leases/concurrency interaction

Phase E — Execution roles

    Planner
    Implementer
    Tester
    Reviewer

Phase F — Reviewer qualification

    read-only reviewer
    structured findings
    independent execution identity

Phase G — Bounded repair loop

    implement
    validate
    review
    repair
    revalidate
    bounded termination

Phase H — Planner

    structured plan
    policy constrained

Phase I — Goal/self-development orchestration

    complete bounded control loop

You may recommend a different decomposition if it better fits the existing
codebase.

Explain dependencies between phases.

======================================================================
Q7 / Q8 BOUNDARY
======================================================================

Do not confuse this architecture document with the current qualification
campaign.

Current baseline:

    833ffe7721be34780dc1b2f511c55f5bb0c11464

Q7 should first prove the existing implementation plane on a real engineering
task.

The availability/control-plane work described here should begin only after
that checkpoint/campaign unless repository evidence demonstrates a compelling
reason otherwise.

The document should make this sequencing explicit.

======================================================================
IMPORTANT DESIGN QUESTIONS
======================================================================

The document must explicitly answer:

1. What is the durable identity of an execution resource?

2. Is availability keyed by:
       runtime?
       credential?
       model?
       reasoning effort?
       some combination?

3. How do provider quota windows map into Orbit without assuming fixed
   5-hour/weekly semantics?

4. How is stale availability handled?

5. How do execution failures update availability?

6. How does availability interact with existing continuation?

7. How do credential pools interact with credential leases?

8. Should role be stored on AgentExecution?

9. Should Planner/Reviewer reuse AgentExecution or require a separate execution
   type?

10. How is read-only reviewer access enforced technically rather than only by
    prompt?

11. How is Tester different from Validator?

12. What state owns reviewer findings?

13. What state owns a repair cycle?

14. Is a new Goal aggregate necessary?

15. How do we prevent planner output from changing security policy?

16. How do we prevent a reviewer from silently becoming an implementer?

17. How does the scheduler explain candidate selection?

18. What happens when all eligible candidates are quota exhausted?

19. What happens when quota information is UNKNOWN?

20. Which provider status probes can safely run automatically?

21. How do we prove a status probe does not consume an inference/model turn?

22. How should quota/status snapshots be persisted and retained?

23. Which data is operational state versus historical evidence?

24. How do we keep self-development from mutating the live Orbit control plane?

25. Where must explicit human authorization remain initially?

======================================================================
NON-GOALS FOR THIS DOCUMENT
======================================================================

Do NOT design:

- arbitrary parallel multi-writer agents
- swarm behavior
- LLM-selected security policy
- autonomous credential creation
- autonomous privilege escalation
- unrestricted host shell
- unrestricted network
- automatic production deployment
- infinite repair loops
- provider-specific model IDs embedded in core domain types
- fake quota estimates
- inferred token usage
- speculative cost optimization before usage evidence exists

Parallel read-only analysis may be discussed as future work, but it is not a
requirement for the first implementation.

======================================================================
REQUIRED OUTPUT
======================================================================

Create a documentation file in the repository.

Choose the location based on the existing documentation structure.

Preferred name if consistent with the repository:

    docs/architecture/self-development-control-plane.md

Otherwise choose the nearest appropriate existing architecture/design
location.

The document should contain approximately these sections:

# Orbit Self-Development Control Plane

## 1. Purpose

## 2. Current Baseline

## 3. Existing Architecture to Preserve

## 4. Problem Statement

## 5. Design Principles

## 6. Domain Model

## 7. Execution Resource Identity

## 8. Runtime / Credential / Model / Role Separation

## 9. Availability and Quota Model

## 10. Availability Evidence and Freshness

## 11. Native Provider Status Discovery

## 12. Credential Pools

## 13. Execution Roles
### Planner
### Implementer
### Tester
### Reviewer

## 14. Validator vs Tester vs Reviewer

## 15. Role Policy

## 16. Deterministic Candidate Scheduler

## 17. Scheduler Explainability

## 18. Continuation Interaction

## 19. Review and Repair State Machine

## 20. Goal / Workflow Aggregate Decision

## 21. Self-Development Trust Boundary

## 22. Human Approval Boundaries

## 23. Security Model

## 24. Persistence

## 25. Observability

## 26. CLI / API Requirements

## 27. Failure Semantics

## 28. Incremental Delivery Plan

## 29. Migration / Compatibility

## 30. Testing Strategy

## 31. Qualification Strategy

## 32. Open Questions

## 33. Explicit Non-Goals

## 34. Definition of Done

Adapt headings when repository architecture makes another organization clearer.

======================================================================
DIAGRAMS
======================================================================

Include useful ASCII or Mermaid diagrams where appropriate.

At minimum document:

1. Resource model:

    Runtime
       x
    Credential
       x
    Model
       x
    Reasoning
       |
       v
    Execution Resource
       |
       + Availability
       + Capabilities
       + Lease state
       |
       v
    Role Scheduler

2. Goal loop:

    Goal
      -> Plan
      -> Implement
      -> Validate
      -> Test/Review
      -> Repair
      -> Validate
      -> Done

3. Continuation versus repair.

4. Self-development trust boundary.

======================================================================
GROUND EVERYTHING IN THE REPOSITORY
======================================================================

For every proposed major type or subsystem:

- identify existing Orbit types/modules that it should extend or reuse
- identify conflicts with current semantics
- avoid duplicate state ownership
- state whether persistence/migration is required
- state whether it changes plan digests/signatures
- state security implications
- state crash-recovery implications

Do not invent an entirely new architecture beside the existing one.

======================================================================
DEFINITION OF DONE FOR THIS TASK
======================================================================

This documentation task is complete when:

1. The current repository has been inspected.

2. The document accurately describes the existing relevant architecture.

3. The future vision is concrete enough to implement incrementally.

4. Availability/quota is modeled without assuming provider-specific windows.

5. Runtime, credential, model, reasoning effort, role, capability, and
   availability are clearly separated.

6. Planner/Implementer/Tester/Reviewer responsibilities are explicit.

7. Validator/Tester/Reviewer are clearly distinguished.

8. Continuation and repair remain separate.

9. Existing Attempt/AgentExecution/workspace/fencing invariants are preserved.

10. Self-development trust boundaries are explicit.

11. Human approval boundaries are explicit.

12. Native Codex/Antigravity quota discovery remains an evidence-driven spike,
    not an unsupported assumption.

13. The implementation roadmap is phased and dependency-aware.

14. Open questions are explicitly identified rather than silently guessed.

15. Documentation validation passes.

16. No runtime implementation behavior has changed.

======================================================================
FINAL RESPONSE
======================================================================

After writing the document, report:

# Self-Development Architecture Draft

## Repository Findings

List the important existing types/modules that influenced the design.

## Document Created

Give the exact path.

## Major Architectural Decisions

Summarize the important decisions.

## Availability Model

Summarize the proposed runtime × credential × model availability design.

## Role Model

Summarize Planner / Implementer / Tester / Reviewer.

## Existing Components Reused

Explain which existing Orbit primitives remain authoritative.

## Persistence / Migration Impact

Describe anticipated future persistence changes.

## Security / Trust Boundaries

Summarize the important invariants.

## Proposed Delivery Phases

Give the phase sequence and dependencies.

## Q7 Boundary

Confirm that this task did not start Q7 or implement post-Q7 functionality.

## Open Questions

List questions that require experimental evidence, especially native quota
discovery.

## Validation

Report documentation/static validation actually run.

## Changed Files

List every changed file.

## Decision

End with exactly one:

    DESIGN READY FOR REVIEW

or:

    DESIGN BLOCKED — <specific reason>

Do not implement the architecture after writing the document.
Stop and wait for review.
======================================================================
FUTURE EXECUTION TOPOLOGY — BRANCHING, LOOPS, AND PARALLELISM
======================================================================

The initial control-plane design may use a mostly sequential execution model,
but the architecture MUST NOT assume that future Goal execution is linear.

Orbit is expected to evolve toward a durable execution graph supporting:

    sequential dependencies
    conditional branching
    bounded loops
    fan-out
    fan-in
    parallel read-only roles
    eventually isolated parallel implementation branches

Examples include:

    Implement
        -> Validate
            -> success -> Review
            -> failure -> Repair -> Validate

and:

                         Accepted Patch
                              |
                +-------------+-------------+
                |             |             |
                v             v             v
              Tester      Security      Architecture
                          Reviewer        Reviewer
                |             |             |
                +-------------+-------------+
                              |
                              v
                         Findings Join

and eventually:

                            Plan
                             |
                   +---------+---------+
                   |                   |
                   v                   v
                Task A              Task B
                   |                   |
              Attempt A           Attempt B
              Workspace A         Workspace B
                   |                   |
                   v                   v
             Implementer A       Implementer B
                   |                   |
                Patch A             Patch B
                   |                   |
                   +---------+---------+
                             |
                             v
                         Integration
                             |
                             v
                          Validate

The design should therefore distinguish:

1. Sequential dependency

2. Conditional branch

3. Parallel fan-out

4. Fan-in / join

5. Bounded control-flow loop

6. Continuation of an interrupted execution

7. Repair iteration after validation/review

These are different semantics and MUST NOT be collapsed into one generic
"retry" mechanism.

----------------------------------------------------------------------
SINGLE-WRITER INVARIANT
----------------------------------------------------------------------

Preserve the existing invariant:

    only one mutating AgentExecution may own a mutable Attempt workspace at
    one time.

Initial parallelism should therefore prioritize independent/read-only work:

    reviewers
    testers
    researchers
    analyzers

Multiple parallel writers MUST NOT mutate the same workspace.

Future parallel implementation should instead use isolated workspaces /
Attempts / branches and produce independent artifacts that are later combined
through an explicit integration step.

The design should explain how the current:

    Task
    Attempt
    AgentExecution
    Workspace
    Artifact

model can support this without weakening workspace ownership.

----------------------------------------------------------------------
EXECUTION GRAPH
----------------------------------------------------------------------

Evaluate whether the long-term Goal/workflow representation should support a
durable execution graph.

Conceptually, graph nodes may describe:

    role
    task/objective
    capability requirements
    inputs
    expected outputs
    execution policy
    budget
    security profile

Edges may describe:

    dependency
    condition
    required artifacts/evidence

Do NOT prematurely implement a generic workflow language.

The architecture document should determine which minimal abstractions are
needed now so today's sequential implementation does not prevent future graph
execution.

----------------------------------------------------------------------
LOOPS
----------------------------------------------------------------------

A pure DAG is insufficient for repair/review cycles.

Loops MUST be explicit and bounded.

For example:

    Implement
        -> Validate
        -> Review
        -> Repair
        -> Validate

may repeat only according to Goal/workflow policy such as:

    max_repairs
    max_review_cycles
    max_executions
    max_wall_time
    provider/resource budgets

No planner or agent may create an unbounded execution loop.

Describe how loop iteration identity and evidence should be persisted so crash
recovery does not accidentally repeat completed work.

----------------------------------------------------------------------
FAN-OUT / FAN-IN
----------------------------------------------------------------------

Future orchestration should support one completed artifact/state feeding
multiple independent roles concurrently.

Example:

    accepted implementation
        -> tester
        -> security reviewer
        -> architecture reviewer

The resulting findings must converge through an explicit durable join rather
than race to mutate shared state.

The design should address:

    join completion policy
    partial failure
    unavailable/quota-exhausted branch
    cancellation
    timeout
    deterministic aggregation
    crash recovery

Possible join policies to evaluate include:

    ALL_REQUIRED
    ALL_AVAILABLE
    MIN_SUCCESS
    FIRST_SUCCESS

Do not adopt these exact names unless they fit the existing model.

----------------------------------------------------------------------
PARALLEL RESOURCE SCHEDULING
----------------------------------------------------------------------

Availability-aware scheduling must eventually account for concurrency.

The scheduler should consider:

    role requirements
    capabilities
    credential availability
    quota windows
    active leases
    provider concurrency restrictions
    worker capacity
    execution security profile

Having multiple credentials for one runtime/provider should make it possible
to safely execute independent work concurrently when policy allows.

Do not assume that quota availability implies concurrency permission.

----------------------------------------------------------------------
PARALLEL IMPLEMENTATION
----------------------------------------------------------------------

Parallel implementation is a later feature and should be treated more
carefully than parallel analysis/review.

Do not allow:

    Implementer A --+
                     +--> same mutable workspace
    Implementer B --+

Instead evaluate:

    Task A -> Attempt A -> Workspace A -> Patch A
    Task B -> Attempt B -> Workspace B -> Patch B

followed by:

    explicit Integration
        -> conflict detection
        -> combined artifact
        -> independent validation

Integration itself must have clear ownership and validation semantics.

----------------------------------------------------------------------
GENERALIZED ACTIVITIES
----------------------------------------------------------------------

The execution graph should not become coding-specific.

Future nodes may represent activities such as:

    coding
    research
    browser/computer use
    infrastructure operations
    data analysis
    model inference
    deterministic validation
    human approval

Do NOT implement these activity types now.

Instead ensure the proposed control-plane architecture does not require every
future execution to own a Git workspace.

Assess whether the long-term architecture may need an abstraction such as:

    ExecutionEnvironment

with possible future implementations:

    GitWorkspace
    BrowserEnvironment
    ComputerEnvironment
    DataEnvironment

without prematurely generalizing the current proven Git Attempt model.

----------------------------------------------------------------------
ARCHITECTURAL QUESTION
----------------------------------------------------------------------

Explicitly answer:

    Can the proposed Goal/role architecture evolve from today's sequential
    execution into a durable branching/parallel execution graph without
    breaking Attempt ownership, AgentExecution durability, continuation,
    fencing, validation, artifact identity, and security invariants?

If not, identify what minimal design change should be made now.

Do NOT implement graph execution as part of this documentation task.
======================================================================
ORBIT IS NOT AN AGENTIC-CODING-SPECIFIC PLATFORM
======================================================================

Do not redefine Orbit around coding agents.

Orbit's original and long-term purpose is a durable workflow/orchestration
center capable of coordinating heterogeneous activities, including:

    agent reasoning
    coding
    research
    computer use
    deterministic tools
    shell/CLI operations
    APIs
    external services
    DevOps
    build/test
    deployment
    waits
    schedules
    event triggers
    human approval
    future activity types

Agentic coding is currently the most deeply qualified workload and should be
used as a proving ground for the orchestration architecture.

It MUST NOT become the assumption underlying every core execution primitive.

----------------------------------------------------------------------
ACTIVITY VS AGENT EXECUTION
----------------------------------------------------------------------

The architecture document must explicitly assess whether Orbit needs a
general durable activity/execution abstraction above or alongside
AgentExecution.

Conceptually:

    Workflow / Goal
          |
          v
    Execution Graph
          |
          v
       Activity
          |
          +-- Agent Activity
          |
          +-- Tool Activity
          |
          +-- Service/API Activity
          |
          +-- Human Approval
          |
          +-- Wait / Timer / Event Activity
          |
          +-- future activity types

An AgentExecution is the durable execution record for an agent activity.

It should NOT automatically become the representation for:

    deterministic validators
    shell commands
    deployment operations
    HTTP calls
    approval gates
    timers
    event waits

unless repository inspection demonstrates that the existing domain model
already intentionally models these through a suitable generic abstraction.

Do not introduce a new ActivityExecution hierarchy merely because this prompt
shows one.

First inspect the current workflow/task/run/execution model.

Then determine the minimal domain abstraction needed to preserve Orbit's
general-purpose nature.

----------------------------------------------------------------------
ROLES APPLY TO AGENT ACTIVITIES
----------------------------------------------------------------------

Roles such as:

    Planner
    Implementer
    Reviewer
    Tester
    Researcher
    BrowserOperator

describe the intent of an AGENT activity.

Do not force agent roles onto deterministic activities.

For example:

    cargo test
    docker build
    HTTP request
    deployment
    approval gate

do not need fictional agent roles.

Keep:

    activity kind

separate from:

    agent role.

----------------------------------------------------------------------
GENERALIZED CAPABILITIES
----------------------------------------------------------------------

The long-term scheduler/resource model should not understand only:

    model
    reasoning effort
    quota

It should be capable of evolving toward general execution capabilities such
as:

    agent.reason
    repository.read
    repository.write
    terminal.execute
    browser.navigate
    browser.interact
    github.read
    github.write
    container.build
    service.call
    deployment.staging
    deployment.production

These names are illustrative, not a required schema.

Capabilities are permissions/abilities.

Roles describe intent.

Activity kind describes execution semantics.

Resources provide capabilities.

Policy decides which capabilities may actually be granted.

Keep these concepts separate.

----------------------------------------------------------------------
GENERAL WORKFLOW EXAMPLE
----------------------------------------------------------------------

The architecture should demonstrate that its proposed model can eventually
represent a workflow such as:

    repository event
        |
        v
    agent analyzes change
        |
        v
    deterministic tests
        |
        +-- failure --> coding agent repair --+
        |                                     |
        +<------------------------------------+
        |
        v
    reviewer agent
        |
        v
    container build
        |
        v
    deploy staging
        |
        v
    automated/computer-use smoke test
        |
        v
    human approval
        |
        v
    deploy production
        |
        v
    monitor rollout

without pretending every node is an AgentExecution.

----------------------------------------------------------------------
ARCHITECTURAL TEST
----------------------------------------------------------------------

Use the following as a test of the proposed architecture:

    Can the same Orbit control plane orchestrate:

        A. an agentic coding workflow,

        B. a DevOps deployment workflow,

        C. a computer-use workflow,

without requiring three unrelated orchestration engines?

If the answer is no, identify which proposed abstraction is too
coding-specific.

The architecture should preserve today's proven coding primitives while
placing them inside a model that can evolve toward heterogeneous workflow
orchestration.

Do NOT implement this generalization as part of the documentation task.
