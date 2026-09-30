# Terminology

| Term | Meaning |
| --- | --- |
| Definition | Strict user-authored workflow input, before accepted compilation |
| Plan | Immutable compiled definition with referenced bindings, scope and policy digests |
| Run | Accepted graph aggregate and its ordered journal |
| Task | Logical activity with dependencies, deadline, recovery policy and budgets |
| Attempt | One physical task claim with workspace, lease and generation |
| Generation | Durable ownership epoch; older epochs cannot mutate accepted state |
| Receipt | Accepted immutable acknowledgement for an operation/request identity |
| Artifact | Immutable finalized bytes plus provenance and content identity |
| WorkspaceState | Baseline, HEAD and eligible content identity for one candidate |
| Workflow | Coordinator-owned role progression, verification, repair and acceptance state |
| RoleExecution | Planner, implementer or reviewer dispatch within a workflow |
| AgentExecution | Provider/runtime execution identity; not task or workspace authority |
| ToolInvocation | Audited repository callback associated with owning execution and policy |
| Handoff | Durable bounded context for authorized unfinished work or role progression |
| VerificationRun | Orbit-controlled checks bound to candidate, plan, policy and environment |
| Credential | Operator catalog identity with provider, current generation and lifecycle |
| Representation | Interface-specific secret material for one credential generation |
| Secret locator | Logical backend reference; not a physical path or secret value |
| AuthLease | Worker-local exclusive staging/write-back lock; not a provider-issued lease |
| AgentRuntime | Protocol, adapter, binary/image identity, launch policy and capabilities |
| AvailabilitySnapshot | Scoped observation with source, confidence, freshness and quota metadata |
| Drain | Stops new claims without invalidating active leases |
| Cleanup uncertainty | External resource removal is unconfirmed; reuse requires reconciliation |
| AcceptanceContract | Frozen requirements and criteria governing business acceptance |

Provider sessions and transcripts are disposable external context. They cannot
replace PostgreSQL state, ownership, receipts, candidate identity or accepted artifacts.
