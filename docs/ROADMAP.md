# Current roadmap

## Current capabilities

Orbit provides durable graph execution, attempt leases and fencing, immutable
plans and artifacts, repository workers, managed verification environments,
credential status discovery, and bounded role workflows. The
[architecture](architecture/README.md) and [reference contracts](README.md#architecture-and-reference)
describe supported behavior. Qualification records describe the checks actually run.

## Workflow requirements and acceptance

Requirement IDs below are retained for roadmap traceability. They are not names
for runtime components. The [workflow requirements](reference/workflow-requirements.md) define
acceptance conditions; reports preserve historical results and limitations.

| Requirement | Current state | Evidence or contract |
| --- | --- | --- |
| R4 stabilization and self-hosting | Accepted; workflow boundary extraction and typed outcomes complete at `14769c6` | [Stabilization report](operations/r4-stabilization-final-report.md) |
| R5 subsystem organization | Implemented and qualified at `7c6ea4f` | [Modularization report](operations/r5-modularization-report.md) |
| R6 developer-local execution | Implemented; confinement and cleanup qualified | [Execution contract](guides/interactive-execution.md) |
| R7 production role budgets | Implemented; resource limits and byte paging qualified | [Execution contract](guides/interactive-execution.md) |
| R8 ACP service | Implemented and qualified offline; bounded real-provider ACP fixture accepted at `8dec490` | [Interactive qualification](operations/interactive-workflows-report.md) |
| R9 editor integration | Managed candidates and ACP protocol qualified offline; actual Zed acceptance pending | [Editor setup](guides/editor-acp.md) |
| R10 skill-selected flows | Implemented; immutable flow selection and conservative escalation qualified | [Skill flows](guides/interactive-execution.md#skill-flows) |
| R11 external BA/SA reasoning | Typed artifacts and authority staged and qualified offline; development bridge integration and live acceptance pending | [External reasoning](guides/external-reasoning.md) |

The roadmap remains incomplete. Live admission requires fresh credential-scoped
quota evidence: at least 15% for a known five-hour window and 5% for a known
seven-day window. A queued run, successful initialization, mocked artifact exchange,
or protocol test does not satisfy a live acceptance gate. Refresh with
`credential status --all --quota` before execution and review. Previously generated
local evidence and timers may be absent; inspect their actual state before relying
on them. Current observed run status belongs in the qualification report.

The development BA bridge is `../orbit-ba-bridge`. Integration must preserve its
existing conversation state and use explicitly identified BA/SA conversations.
The required acceptance is a frozen contract, real implementation, independent
review and final verification, followed by BA attestation for that exact candidate.

## Deployment and provider acceptance

Local packaging supports a non-root server image, Compose, Quadlet, host workers,
probes, durable drain, backup/restore and coordinated upgrades. See
[alpha qualification](operations/qualification.md) for the verified scope.

Selected live-provider and separately hosted worker acceptance remain distinct
from local fixtures. The remote repository workflow must demonstrate an actual
inspect/edit/test/revise cycle, accepted artifacts, independent verification,
human review, account refresh/expiry behavior, denied unauthorized effects and
recovery after worker/server failure. Unknown provider outcomes remain unknown.
See [remote coding qualification](operations/remote-coding-qualification.md) and
[ACP compatibility](operations/acp-codex-compatibility.md).

## Conditional extensions

The following require a selected workload or deployment need and their own evidence:

- Stronger isolation for hostile code, such as gVisor or Firecracker.
- Physical GPU execution and additional GPU runtimes.
- Multi-host capacity, HA, throughput and retention guarantees.
- Kubernetes or cloud provisioning for demonstrated capacity requirements.
- SSO, tenant administration or external policy distribution.
- Additional credential providers, public package distribution and SDK publication.
- Parallel analysis or isolated implementation branches with explicit integration.

These are not prerequisites for the bounded local workflow. Publishing, pushing,
merging and deploying use explicit authority and selected destinations.
