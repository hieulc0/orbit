# Orbit Roadmap

## 1. Purpose

Orbit is evolving from a qualified autonomous coding-agent runtime into a
general agent orchestration platform for:

- autonomous software engineering;
- interactive local development;
- editor integration;
- multi-agent workflows;
- external reasoning roles such as BA and domain experts;
- policy-controlled execution across different isolation levels.

The central architectural principle remains:

> Orbit, not the agent, owns truth about task state, execution, verification,
> and completion.

Provider/model sessions are disposable.

Durable state belongs to Orbit:

- Task
- Attempt
- WorkspaceState
- RoleExecution
- AgentExecution
- ToolInvocation
- VerificationRun
- review/decision artifacts
- cleanup evidence

Agents propose and modify.

Orbit:

- controls authority;
- owns workspaces;
- executes authoritative verification;
- records evidence;
- enforces policy;
- decides whether a workflow may complete.

---

# 2. Current State

The B1-B6 verification and workflow foundation has already been implemented and
qualified.

Major proven capabilities include:

- isolated command execution;
- durable verification evidence;
- clean verification environments;
- policy-digest binding;
- role-based workflows;
- managed integration environments;
- browser verification;
- FAST / STANDARD / FULL regression strategy;
- real ACP role execution;
- repository filesystem mutation tools;
- exact WorkspaceState binding;
- durable handoffs;
- repair loops;
- provider fallback;
- credential isolation;
- runtime/model resolution;
- reset-aware provider selection;
- exact Codex tool-call audit correlation;
- early-failure AgentExecution evidence;
- supervisor and cleanup evidence.

S8 completed the major self-hosting cutover qualification work.

S9 and S10 are complete. The completed S10 checkpoint is:

    14769c6083b0ac47d4ba6c77a773b6040a9d0267

Current status after the September 30, 2026 handoff and subsequent qualification:

| Milestone or gate | Status |
| --- | --- |
| S9 workflow execution boundary refactor | Complete at `b216101107d092a12679b2452b1b0827fd8bcd8d` |
| S10 typed execution and cleanup outcomes | Complete at `14769c6083b0ac47d4ba6c77a773b6040a9d0267` |
| Frozen R4 stabilization campaign | Passed |
| Final R4 self-hosting dogfood acceptance | Complete: `ORBIT_SELF_HOSTING_QUALIFIED` |
| R5 structural modularization | Implemented and qualified; see the R5 report |
| R6–R11 | Pending |

The S10 report location is `target/roadmap-evidence/S10-report.md`. Generated
evidence is local and may be absent from another checkout; this status records
the handoff and does not replace the qualification artifacts.

The handoff reports 13% remaining quota, below the 15% admission minimum, and a
fresh guarded dogfood run queued for September 30 at 07:39 after reset. A queued
run is not acceptance evidence. Confirm its outcome and refresh quota before
starting any further live execution.

The fresh guarded run subsequently completed with independent review, repair,
FULL, exact-state completion, and confirmed cleanup. See the
[final R4 report](../operations/r4-stabilization-final-report.md). R5 is now
[implemented and qualified](../operations/r5-modularization-report.md). R6–R11
remain pending, so the roadmap is still incomplete.

---

# 3. Admission Policy and Historical S9 Entry State

The S9 entry state below is historical. S9 and R4 dogfood have since completed;
the quota and EXACT audit requirements continue to govern live self-hosting.

The pre-S9 execution contract repair is complete and independently approved.

Checkpoint:

    4c5d2bd0e65d3fe2f32e9544c05d67d5f0a3e309

The repair established:

- Codex native sandbox remains read-only;
- repository mutation occurs through Orbit-authorized callbacks;
- implementer prompts explain the callback mutation path;
- no-op implementation cannot be recorded as successful;
- changed-file claims are checked against repository evidence;
- unchanged repair attempts do not consume another verification cycle;
- mutation locks and workflow ownership are released correctly.

Admission policy requires an authoritative runtime with:

    tool_audit_correlation = EXACT

Provider state recorded at S9 entry:

    Codex:
        audit capability = EXACT
        availability = READY

    Antigravity:
        audit capability = PARTIAL
        therefore not eligible for authoritative S9 roles

Orbit quota admission policy currently requires:

    5H remaining >= 15%
    7D remaining >= 5%

Live self-hosting starts only when an EXACT-capable runtime satisfies normal
eligibility.

Do not weaken quota or audit policy merely to start the milestone.

---

# 4. Stabilization Sequence

The stabilization sequence is:

    S9
      ->
    S10
      ->
    final R4 qualification
      ->
    final self-hosting dogfood
      ->
    R5 modularization

S9 and S10 are completed stabilization work. The frozen R4 campaign has passed;
final self-hosting dogfood acceptance has also passed. R5 modularization is
qualified; R6 is the next milestone.

New major product features should begin after this sequence is complete.

---

# 5. S9 — Workflow Execution Boundary Refactor

Status: complete at `b216101107d092a12679b2452b1b0827fd8bcd8d`. The contract
below is retained for traceability.

## Goal

Reduce the responsibility concentrated in `WorkflowCoordinator` without
changing workflow semantics.

S9 is structural extraction, not architectural redesign.

The workflow coordinator currently contains several responsibilities that
should become clearer subsystem boundaries.

Conceptually, move from:

    WorkflowCoordinator
        ├── workflow state decisions
        ├── prompt construction
        ├── handoff parsing
        ├── live ACP execution
        ├── verification resolution
        ├── repair handling
        └── stage progression

toward:

    WorkflowCoordinator
        ├── stage decisions
        ├── progression
        └── repair decisions

    Role prompt / handoff layer
        ├── planner prompt
        ├── implementer prompt
        ├── reviewer prompt
        └── structured handoff parsing

    Live role executor
        ├── provider execution
        ├── ACP lifecycle
        ├── AgentExecution
        └── execution outcome

    Verification resolver
        ├── workflow requirement
        └── verification work selection

## Extraction Order

Perform extraction in this order:

1. Prompt and handoff construction.
2. Live role-execution implementation.
3. Verification resolution, only where naturally separable.

Do not force abstractions solely to reduce file size.

## WorkflowCoordinator Must Continue to Own

- state-machine decisions;
- role/stage progression;
- repair decisions;
- transition policy;
- completion decisions.

## Preserve

S9 must preserve:

- `Task != Attempt != AgentExecution`;
- WorkspaceState identity;
- role authority;
- mutation locking;
- provider-session independence;
- real ACP execution;
- exact tool audit requirements;
- capability-aware provider selection;
- reset-aware ranking;
- repair-loop semantics;
- authoritative verification;
- reviewer binding;
- cleanup behavior;
- completion policy.

## Non-Goals

S9 must not:

- redesign the workflow state machine;
- redesign provider selection;
- change quota scheduling;
- weaken exact tool audit;
- redesign fallback;
- introduce broad dependency injection;
- create a generic plugin framework;
- create a universal tool registry;
- create a new Rust crate merely for organization;
- reorganize the complete source tree;
- perform the R5 module migration;
- rewrite verification semantics.

## Self-Hosting Requirement

S9 must be implemented through Orbit itself.

Create a fresh workflow and attempt after admission succeeds.

Do not reuse terminal S9 attempts from before the execution-contract repair.

The self-hosted workflow must demonstrate:

    PLAN
      ->
    IMPLEMENT
      ->
    real repository mutation
      ->
    FAST
      ->
    STANDARD
      ->
    REVIEW
      ->
    FULL
      ->
    COMPLETE

## Implementer Proof

The S9 implementer must prove the repaired write contract:

- native Codex filesystem remains read-only;
- implementer sees actual Orbit mutation tools;
- implementer uses Orbit callbacks for mutation;
- at least one real candidate mutation occurs;
- output WorkspaceState differs from input WorkspaceState;
- handoff changed files match actual repository evidence.

No-op implementation must fail before verification.

## Qualification

Run relevant qualification suites including:

- B3;
- B3.1;
- B3.2;
- B3.4;
- B6;
- C1 documentation checks where applicable.

Standard gates:

    cargo fmt --all -- --check

    cargo clippy --locked --all-targets --all-features -- -D warnings

    cargo test --locked --all-targets --all-features

    git diff --check

Orbit-owned evidence must include:

- FAST;
- STANDARD;
- reviewer decision;
- FULL;
- exact WorkspaceState consistency;
- cleanup confirmation.

## Definition of Done

S9 is complete when:

- responsibilities have been extracted according to scope;
- workflow semantics are unchanged;
- self-hosted implementation performed real mutations;
- exact tool auditing passes;
- reviewer approves;
- FULL passes;
- final WorkspaceState matches reviewed/FULL state;
- cleanup is confirmed;
- no locks or step ownership remain.

Suggested checkpoint:

    refactor workflow execution boundaries

---

# 6. S10 — Typed Execution and Cleanup Outcomes

Status: complete at `14769c6083b0ac47d4ba6c77a773b6040a9d0267`. Report:
`target/roadmap-evidence/S10-report.md`. The contract below is retained for
traceability.

## Goal

Make important execution and cleanup decisions explicit and typed instead of
depending on incidental strings or loosely interpreted process failures.

S10 is a narrow correctness/maintainability phase.

## Target Concepts

Normalize concepts such as:

    execution completed
    cancelled
    timed out
    exited(code)
    signaled(signal)

and:

    cleanup confirmed
    cleanup unconfirmed
    cleanup not required

Exact type names should follow existing Orbit conventions.

## Likely Scope

Primary modules may include:

    integration_environment.rs
    browser_verification.rs
    verification.rs
    acp_runtime.rs

and only directly related supporting code.

## Desired Model

Instead of:

    string error
        ->
    later interpretation
        ->
    workflow decision

prefer:

    typed execution outcome
        ->
    explicit policy decision

For example:

    ExecutionOutcome::TimedOut

is meaningfully different from:

    ExecutionOutcome::Exited(1)

and:

    CleanupOutcome::Unconfirmed

must not silently become successful cleanup.

## Preserve

S10 must preserve:

- workflow semantics;
- provider selection;
- quota/reset scheduling;
- capability policy;
- WorkspaceState identity;
- verification policy;
- Engine/worker fencing;
- role authority;
- tool auditing.

## Non-Goals

Do not:

- convert all `anyhow` usage;
- build a repository-wide error hierarchy;
- introduce a universal `RuntimeError`;
- redesign ACP;
- redesign Engine;
- redesign Worker;
- redesign scheduler behavior;
- redesign provider fallback;
- redesign cleanup architecture broadly.

## Qualification

Run:

- B1;
- B2;
- B4;
- B5;
- ACP runtime qualification;
- focused timeout tests;
- focused cancellation tests;
- signal/exit tests;
- cleanup-confirmed tests;
- cleanup-unconfirmed tests.

And standard Rust gates.

## Definition of Done

S10 is complete when important execution and cleanup decisions are represented
by explicit bounded types and all relevant qualification remains green.

Suggested checkpoint:

    normalize verification cleanup outcomes

---

# 7. Final R4 Stabilization Campaign

Status: passed, as recorded in the September 30 handoff. The frozen campaign
contract below remains the qualification reference.

After S9 and S10, run the complete frozen stabilization campaign.

Do not expand the qualification contract at this stage unless new evidence
demonstrates violation of an already-defined invariant.

Run:

- B1;
- B2;
- B3;
- B3.1;
- B3.2;
- B3.3;
- B3.4;
- B4;
- B5;
- B6.

Explicitly run required ignored/service-backed cases.

Generic `cargo test` skips must not be counted as qualification passes.

Standard gates:

    cargo fmt --all -- --check
    cargo clippy --locked --all-targets --all-features -- -D warnings
    cargo test --locked --all-targets --all-features
    git diff --check

---

# 8. Final R4 Self-Hosting Dogfood

Status: accepted as `ORBIT_SELF_HOSTING_QUALIFIED` on September 30, 2026.
Workflow `wf-6af64fb2-65c8-4b89-a80f-64deeea3529a` completed the documentation
extraction, independent review, a real repair, FULL, exact-state completion,
and confirmed cleanup. See the [final report](../operations/r4-stabilization-final-report.md).

After the frozen campaign, run a realistic end-to-end task through Orbit.

Use the original C1-style documentation refactor or an equivalent real
repository task.

It must require:

- real planning;
- multiple file reads;
- real filesystem mutations;
- potentially file/directory topology changes;
- authoritative verification;
- independent review;
- repair if necessary;
- FULL;
- exact WorkspaceState completion.

The purpose is not to test one API.

The purpose is to prove:

> Orbit can use Orbit to perform meaningful software-engineering work while
> preserving all qualified invariants.

Final report:

    ORBIT_R4_STABILIZATION_FINAL_REPORT

Possible outcomes:

    ORBIT_SELF_HOSTING_QUALIFIED

    ORBIT_SELF_HOSTING_PARTIALLY_QUALIFIED

    ORBIT_SELF_HOSTING_NOT_QUALIFIED

Only after this should the project move from stabilization-first development
toward feature-first development.

---

# 9. R5 — Structural Modularization

Status: implemented and qualified. See the [R5 report](../operations/r5-modularization-report.md)
and [subsystem ownership map](../architecture/subsystem-ownership.md).

## Goal

Reorganize the Rust repository around real subsystem boundaries.

The codebase is increasingly difficult to scan because many modules currently
live directly under `src/`.

This is primarily a navigation and ownership problem, not evidence that the
architecture itself is broken.

R5 should be mostly behavior-neutral.

## Principle

    behavioral extraction first
    physical movement second

S9 establishes clearer responsibility boundaries.

R5 physically expresses them in the repository.

## Candidate Structure

A likely target is:

    src/
      acp/
      workflow/
      verification/
      credentials/
      providers/
      execution/
      tools/
      control_plane/
      telemetry/

Potential ownership:

### `acp/`

- broker;
- capabilities;
- contract;
- files;
- process;
- runtime;
- terminal;
- wire.

### `workflow/`

- workflow domain;
- coordinator;
- roles;
- handoffs;
- role execution;
- workflow policies.

### `verification/`

- verification engine;
- regression selection;
- integration environments;
- browser verification;
- verification evidence.

### `credentials/`

- credential registry;
- enrollment;
- secret backend;
- status representation.

### `providers/`

Provider-specific code, for example:

    providers/
      codex/
      antigravity/

### `execution/`

- execution;
- workspace;
- repository;
- container/runtime execution;
- compute.

### `tools/`

- filesystem tools;
- tool-surface policy;
- common tool definitions.

### `control_plane/`

- engine;
- worker;
- API;
- registry/coordination components.

### `telemetry/`

- telemetry;
- evidence export;
- artifacts;
- run export.

## Tests

Eventually mirror subsystem ownership:

    tests/
      acp/
      workflow/
      verification/
      credentials/
      execution/
      providers/

Qualification tests may remain clearly marked as qualification tests even after
physical reorganization.

## Suggested Sequence

### R5.1 — Ownership Map

Document which subsystem owns every major existing module.

No file moves yet.

Status: complete. See the [subsystem ownership map](../architecture/subsystem-ownership.md).
Physical movement, compatibility exports, documentation, and R5 qualification
are also complete; see the R5 report.

### R5.2 — ACP

Move ACP modules into a coherent namespace.

### R5.3 — Workflow

Move coordinator, workflow domain, handoff, and extracted S9 components.

### R5.4 — Verification

Organize verification, regression, browser, and integration-environment code.

### R5.5 — Credentials and Providers

Separate provider-neutral credential concepts from provider-specific adapters.

### R5.6 — Execution and Tools

Clarify repository/workspace/container/tool ownership.

### R5.7 — Tests

Mirror production module boundaries where useful.

### R5.8 — Public API Cleanup

Reduce accidental public exports and expose intentional subsystem APIs.

### R5.9 — Documentation

Update architecture maps and developer documentation.

### R5.10 — Qualification

Run the full relevant regression/qualification suite.

## Non-Goals

Do not use R5 as an excuse for:

- semantic rewrites;
- new scheduler architecture;
- provider redesign;
- generic abstractions without immediate need;
- new product features mixed into file moves.

---

# 10. R6 — Developer Local Execution Profile

## Motivation

Orbit's current isolated execution is appropriate for autonomous work, CI,
remote workers, qualification, and unknown repositories.

It is heavier than necessary for interactive editor development.

Do not create an unrestricted "host mode."

Create an explicit developer-local execution profile.

## Execution Profiles

Long-term:

    DEV_LOCAL
    TRUSTED
    UNTRUSTED

### DEV_LOCAL

For trusted interactive local development.

Fast paths may include:

- native filesystem reads;
- native repository search;
- native Git operations;
- Orbit-managed Git worktrees;
- Orbit-mediated repository mutations.

Terminal execution should still have a lightweight boundary.

Candidate mechanisms include:

- Linux namespaces;
- bubblewrap;
- Landlock;
- seccomp;
- other lightweight local confinement.

Sensitive host state should remain unavailable unless explicitly granted.

Examples:

    ~/.ssh
    ~/.aws
    ~/.orbit/private
    browser profiles
    unrelated repositories
    host container sockets

### TRUSTED

Current strong rootless OCI execution profile.

Use for:

- autonomous production work;
- authoritative qualification;
- final verification;
- CI-like workflows.

### UNTRUSTED

Future stronger isolation for unknown or hostile workloads.

Potential implementations:

- gVisor;
- Firecracker;
- equivalent stronger isolation.

## Key Architectural Point

Agent execution profile and verification profile may differ.

Example:

    AgentExecution:
        DEV_LOCAL

    final VerificationRun:
        TRUSTED

This gives fast interactive iteration while preserving authoritative final
evidence.

---

# 11. R7 — Production Agent Resource Budgets

## Goal

Replace qualification-era conservative limits with explicit, observable,
role-aware production budgets.

Do not make agent execution unlimited.

## Candidate Model

    RoleBudget {
        max_total_calls
        max_mutating_calls
        max_terminal_calls
        max_file_read_bytes
        max_output_bytes
    }

Possible initial production values:

    planner:
        max_total_calls ~150

    implementer:
        max_total_calls ~300

    reviewer:
        max_total_calls ~150

These numbers should later be tuned from real telemetry.

## File Reading

Support ranged reads.

A read result should expose:

    bytes_returned
    total_size
    truncated
    next_offset

Prefer:

    search
      ->
    targeted range read

rather than loading very large source files blindly.

## Budget Exhaustion

Budget exhaustion should be explicit:

    TOOL_BUDGET_EXHAUSTED

It should not masquerade as a provider or tool failure.

Orbit may later allow:

    AgentExecution #1
        reaches bounded budget

        ->
    durable handoff

        ->
    AgentExecution #2
        same Attempt/workspace

This fits the existing model:

    one Attempt
        may contain
    multiple sequential AgentExecutions

---

# 12. R8 — Orbit-ACP Service

## Goal

Expose Orbit as an editor/client-facing ACP service.

The client talks to Orbit.

The client should not directly own orchestration.

Architecture:

    Editor
      |
      v
    Orbit-ACP
      |
      v
    Orbit
      ├── task state
      ├── workflow
      ├── provider selection
      ├── roles
      ├── workspaces
      ├── tool policy
      ├── verification
      └── evidence

Possible clients:

- Zed;
- CLI;
- desktop UI;
- future editor integrations.

## Interactive Orchestrator

Allow the user to interact with an orchestrator model such as Luna-Max.

The orchestrator may:

- understand the user's request;
- inspect workflow state;
- choose or configure a flow;
- explain progress;
- request clarification;
- start or continue role executions.

It should not silently become an unrestricted implementer.

If the user says:

    "fix this error"

the orchestrator should create or continue the appropriate Orbit flow.

---

# 13. R9 — Editor / Zed Integration

## Goal

Make Orbit useful interactively without reducing it to another chat window.

The editor should be a client of Orbit.

Orbit remains the source of truth.

## Suggested Panel

Display:

    Task

    Flow
      PLAN
      IMPLEMENT
      VERIFY
      REVIEW
      FULL

    Current role

    Provider/model/account

    Tool budget

    File-read budget

    Candidate changes

    Verification state

    Quota/reset state

    Cleanup/execution state

Actions may include:

    View Diff
    Open Attempt
    Cancel
    Request Review
    Apply Candidate
    Discard Candidate

## Workspace UX

Prefer Orbit-managed Git worktrees.

Interactive agent changes should not immediately modify the developer's main
checkout.

Conceptually:

    main repository

        +
    Orbit attempt worktree

Then expose actions such as:

    Open Attempt
    Apply
    Discard

This preserves safety while remaining fast.

---

# 14. R10 — Skill → Flow

## Goal

Turn user intent into structured Orbit workflows.

Do not make skills giant prompt templates.

A skill should primarily select/configure a flow.

Example:

    user:
        "refactor credential resolver"

        ↓

    skill:
        software.change

        ↓

    policy determines:

        PLAN
        IMPLEMENT
        FAST
        REVIEW

A higher-risk task may become:

    PLAN
    IMPLEMENT
    FAST
    STANDARD
    REVIEW
    FULL

Possible initial skills:

- investigate;
- fix bug;
- implement feature;
- refactor;
- review;
- update documentation;
- dependency update;
- release preparation;
- security review.

The workflow engine remains generic.

Skills configure it.

---

# 15. R11 — BA-ChatGPT / External Reasoning Roles

## Goal

Integrate the BA-ChatGPT side project after Orbit-ACP becomes stable.

Do not create a second workflow control plane.

Architecture:

    ChatGPT
       |
       v
    BA bridge/proxy
       |
       v
    Orbit external-role interface
       |
       v
    Orbit workflow
       ├── BA
       ├── SA
       ├── implementer
       ├── technical reviewer
       └── verification

## BA Role

BA-ChatGPT provides strengths that repository-focused coding agents may not
have:

- broader product context;
- requirements discovery;
- web research;
- current external documentation;
- user-facing reasoning;
- ambiguity identification;
- challenging assumptions;
- acceptance criteria.

## SA Role

A repository-aware model such as Luna-Max may serve as System Architect.

Responsibilities:

- architecture;
- subsystem ownership;
- technical proposal;
- invariants;
- data model;
- APIs;
- migrations;
- failure modes;
- verification plan.

## Implementer

Responsible for repository mutation.

## Technical Reviewer

Responsible for implementation and architecture correctness.

## BA Acceptance

Responsible for asking:

> Did the implementation actually satisfy the requirement?

This is different from:

> Is the code technically correct?

---

# 16. BA / SA Structured Artifacts

Provider conversation history must not become the source of truth.

Orbit should own durable artifacts.

Possible artifacts:

    RequirementBrief

    TechnicalProposal

    Challenge

    Resolution

    AcceptanceContract

## RequirementBrief

Possible fields:

    objective
    user_problem
    functional_requirements
    non_functional_requirements
    external_facts
    assumptions
    acceptance_criteria
    open_questions

## TechnicalProposal

Possible fields:

    affected_subsystems
    architecture
    invariants
    data_model
    APIs
    migrations
    security
    failure_modes
    verification_plan

## Challenge

Possible fields:

    finding_id
    category
    claim
    evidence
    severity
    requires_resolution

The BA ↔ SA interaction becomes durable structured reasoning rather than an
unbounded provider chat.

---

# 17. BA / SA Workflow

A future feature workflow may look like:

    USER
      |
      v
    BA_DISCOVERY
      |
      v
    RequirementBrief
      |
      v
    SA_PROPOSAL
      |
      v
    TechnicalProposal
      |
      v
    BA_CHALLENGE
      |
      v
    SA_RESOLUTION
      |
      v
    REQUIREMENT_FREEZE
      |
      v
    IMPLEMENT
      |
      v
    VERIFY
      |
      v
    TECHNICAL_REVIEW
      |
      v
    BA_ACCEPTANCE
      |
      v
    COMPLETE

This turns Orbit into more than a coding-agent runner.

It becomes a software-delivery workflow engine.

---

# 18. External Role Authority

External roles must have explicit authority.

Suggested defaults:

    BA
      web research          yes
      requirement artifact  yes
      repository read       optional
      repository write      no
      terminal              no

    SA
      repository read       yes
      architecture artifact yes
      repository write      no

    Implementer
      repository read       yes
      repository write      yes

    Reviewer
      repository read       yes
      repository write      no

Prompt text never grants authority.

Orbit policy grants authority.

---

# 19. BA-ChatGPT Parallel Development Strategy

Some work may happen before Orbit-ACP is complete.

Safe parallel work:

- protocol design;
- artifact schemas;
- BA/SA workflow state design;
- external-role permission model;
- mock CLI client;
- fixture-based message exchange.

Do not yet create a production path like:

    ChatGPT
      ->
    direct Codex control

The production integration should use Orbit as the authority:

    ChatGPT
      ->
    BA bridge
      ->
    Orbit
      ->
    agents

---

# 20. Longer-Term Runtime Architecture

The desired end state is one Orbit with multiple execution profiles.

Do not create separate "heavy Orbit" and "light Orbit" products.

Conceptually:

                            ORBIT

              ┌──────────────┼──────────────┐
              │              │              │
          DEV_LOCAL       TRUSTED       UNTRUSTED
              │              │              │
       editor iteration   autonomous      hostile/
                          production       unknown
              │              │              │
         lightweight      rootless       gVisor /
         confinement        OCI          Firecracker

All profiles share:

- Task;
- Attempt;
- WorkspaceState;
- RoleExecution;
- AgentExecution;
- ToolInvocation;
- VerificationRun;
- policy;
- credentials;
- evidence;
- provider selection;
- handoffs.

Only the execution/isolation policy changes.

---

# 21. Long-Term Product Shape

The long-term Orbit system should support two major usage modes.

## Autonomous

Example:

    orbit run task ...

Orbit autonomously performs:

    planning
    implementation
    verification
    review
    repair
    FULL

using the appropriate isolation profile.

## Interactive

Example:

    Zed
      ->
    Orbit ACP
      ->
    Luna-Max orchestrator
      ->
    Skill -> Flow
      ->
    role agents

The user can observe and intervene without becoming responsible for manually
coordinating provider sessions.

---

# 22. Design Principles to Preserve

All future milestones must preserve these principles.

## Orbit Owns State

Provider sessions are disposable.

No workflow may depend on provider conversation history as the only source of
state.

## WorkspaceState Is the Candidate Identity

Review and verification apply only to the exact state they observed.

Mutation invalidates stale evidence.

## Agent Claims Are Not Verification

An agent saying:

    "tests passed"

does not create authoritative verification evidence.

## Authority Is Policy-Driven

Prompt text does not grant filesystem, terminal, credential, or network
authority.

## Mutation Is Explicit

Only authorized roles may mutate an Attempt workspace.

## Evidence Must Be Truthful

Unknown remains unknown.

Examples:

    actual model unavailable
        ->
    UNKNOWN

    quota stale
        ->
    STALE

    tool events not correlatable
        ->
    PARTIAL / UNRESOLVED

Do not invent certainty.

## Isolation Is a Profile

Do not weaken the whole architecture to make interactive development faster.

Choose an appropriate execution profile.

## Bounded Resources

Tool calls, terminal commands, file reads, output, time, and processes remain
bounded.

Production limits may be larger than qualification limits, but not infinite.

---

# 23. Milestone Overview

The planned sequence is:

    S9
      Workflow execution boundary extraction

        ↓

    S10
      Typed execution / cleanup outcomes

        ↓

    Final R4 Qualification
      Full frozen campaign + self-hosting dogfood

        ↓

    R5
      Structural modularization

        ↓

    R6
      DEV_LOCAL execution profile

        ↓

    R7
      Production agent resource budgets

        ↓

    R8
      Orbit-ACP service

        ↓

    R9
      Zed/editor integration

        ↓

    R10
      Skill -> Flow

        ↓

    R11
      BA-ChatGPT / external reasoning roles

        ↓

    Later
      stronger untrusted isolation
      broader provider/runtime ecosystem
      richer workflow libraries
      organization/team workflows

---

# 24. Immediate Next Step

R4 self-hosting acceptance and R5 structural modularization are complete. R6 is
next: define and implement an explicit developer-local execution profile while
preserving Orbit-mediated mutation and authoritative isolated final verification.
The profile must keep sensitive host state outside its granted boundary.

R6–R11 remain pending. Completed stabilization and modularization reports retain
their exact scope and evidence; they do not establish acceptance of later
execution profiles or product features.

Fresh live executions still require normal capability and quota admission.
The former 13% quota observation is historical; the guarded R4 run admitted
with 96% remaining 5H quota and 83% remaining 7D quota. Scheduled reset times
alone are not fresh eligibility evidence.

---

# 25. Roadmap Success Criterion

The roadmap is successful when Orbit evolves from:

    qualified autonomous coding runtime

into:

    trusted agent orchestration platform

capable of supporting:

    autonomous engineering
    interactive development
    multiple execution profiles
    editor integration
    skill-driven workflows
    BA/SA collaboration
    external research roles

while preserving the core invariant:

> Orbit, not the agent, decides what happened, what is authoritative, and
> whether the task is complete.
