# Workflow roadmap requirements

This document retains R4–R11 identifiers for requirements traceability. Current
status is in [the roadmap](../ROADMAP.md); current interfaces are in the linked
contracts. Historical qualification reports retain their own checkpoint identities.

## State and authority

Orbit owns workflow truth. Task, Attempt, WorkspaceState, RoleExecution,
AgentExecution, ToolInvocation, VerificationRun, handoffs, review decisions and
cleanup evidence are durable identities. Provider conversations are disposable.
A proposed result becomes accepted only through Orbit's fenced transitions.

WorkspaceState identifies the exact candidate, including tracked changes and
eligible untracked content. Agent claims and exploratory commands do not establish
verification. Review, verification and completion must refer to the same candidate.
Unknown usage, billing, provider outcomes and cleanup remain explicitly unknown.
Mutation authority is policy-controlled and explicit; roles cannot grant themselves
credentials, tools, isolation exceptions or completion authority.

## R4: stabilized autonomous workflow

The coordinator must retain stage progression, repair bounds and completion
invariants while protocol, prompt and process responsibilities have separate owners.
Typed cancellation, timeout, process exit and cleanup uncertainty must survive
contextual errors. Diagnostics must not determine outcome by substring matching.

Acceptance requires a frozen campaign covering verification, workflow orchestration,
provider mediation, filesystem mutation, credential isolation, reset-aware selection,
callback correlation and cleanup. Self-hosting also requires a real change from an
immutable Orbit baseline, independent review, repair where required, authoritative
final verification for the exact candidate, and confirmed process/resource cleanup.
See [the qualification report](../operations/r4-stabilization-final-report.md).

## R5: subsystem ownership

Physical namespaces express domain responsibility. They do not change state machines,
database authority, serialized plans, policy digests, provider selection or fencing.
Compatibility exports remain available. Qualification must preserve legacy wire and
digest fixtures and shared execution behavior. See
[subsystem ownership](../architecture/subsystem-ownership.md) and
[the modularization record](../operations/r5-modularization-report.md).

## R6: developer-local execution

Exploratory terminals require an operator-selected confinement profile. Repository
writes are confined to the managed candidate; Git metadata, host homes, secrets,
other repositories, container sockets and network are unavailable. Missing isolation
fails closed. Process descendants must stop before cleanup is confirmed.
Final authoritative verification uses an independently pinned environment.
`trusted` does not imply hostile-code isolation, and a future `untrusted` profile
requires its own threat model and qualification. See
[execution boundaries](../guides/interactive-execution.md).

## R7: role resource budgets

Production roles have finite callback, mutation, terminal, file-read and output
allowances separate from provider tokens or billing. Running and terminal evidence
must record both limits and consumption. Budget exhaustion is a typed controlled
outcome; genuine dispatch uncertainty remains conservative. Reads support bounded
pages with explicit continuation metadata, UTF-8 boundaries and path confinement.
Unknown provider usage must not be synthesized from tool activity.

## R8: ACP service

The service is a presentation and control interface over the existing coordinator,
not a second workflow engine. Durable sessions pin configuration, skill flow,
candidate identity and accepted replay. Request framing, notification retention,
active jobs and output are bounded. Cancellation must reach owned execution and
preserve cleanup uncertainty. Native editor filesystem/shell callbacks and arbitrary
MCP grants cannot bypass Orbit policy. See [the ACP service](../guides/editor-acp.md).

Acceptance requires a real provider-backed workflow through this interface, a
confined effect, independent verification/review, exact candidate acceptance and
confirmed cleanup under the same quota and execution guards.

## R9: editor and managed candidates

An editor shows task, role, runtime/account, quota freshness, budgets, verification,
changed paths and cleanup. Main-checkout mutation requires explicit apply. Apply
must prove the canonical Git source root, unchanged base and clean source, exact
reviewed/verified candidate and index, confirmed cleanup and exclusive durable
repository ownership. Discard requires an explicit matching candidate identity.
Recovery must reconcile observed state before releasing ownership.

Typed ACP qualification is required but does not establish Zed GUI acceptance.
Actual editor acceptance includes progress, replay, cancellation, diff navigation,
apply/discard and safe failure/recovery against a disposable repository.

## R10: skills select immutable flows

Skills configure generic workflow behavior; they do not introduce separate engines.
Investigation and analysis remain read-only and do not claim technical qualification.
Conservative changes require independent review and final authoritative checks.
A low-risk documentation flow can use lighter checks only while observed changes
satisfy the documentation policy. Code, configuration, manifests, unknown and empty
changes must escalate. Pinned regression requirements may demand stronger checks.

## R11: external BA and SA reasoning

The BA owns requirements, challenges and business acceptance; the SA owns technical
proposals and resolutions. Orbit stores typed, versioned artifacts and immutable
request identities. Connection policy grants authority; prompts and artifact content
do not. A required unresolved challenge blocks freeze.

The frozen AcceptanceContract binds requirements, proposal, challenges and resolutions
to one workflow. Implementation derives its objective from that contract. Technical
completion enters BUSINESS_ACCEPTANCE. Only BA attestation for every criterion,
matching the frozen digest and exact technically accepted candidate, permits final
completion. External roles have no repository mutation, terminal, implementation
progression or apply/discard authority.

The development bridge supplies conversation provenance and SA analysis transport.
Free-form chat is context, not an accepted artifact or approval. Live acceptance
requires an identified authenticated conversation, actual typed artifacts, real
implementation/review/verification and matching BA acceptance. See
[external reasoning](../guides/external-reasoning.md).

## Future runtime constraints

Execution profiles describe the containment boundary independently of tenancy and
account authorization. Additional backends require explicit policy, reproducible
identity and cleanup evidence; silent isolation downgrades are prohibited.
Parallel writers require separate candidates and explicit integration. Bounded
branch/join policies must describe partial failure, reviewer availability and
cancellation. No provider transcript substitutes for durable Orbit state.
