# Orbit documentation

Start with [the project overview](../README.md). Architecture and reference
pages describe the current system; guides and operations pages explain how to
use it. Qualification records preserve observed results and limitations at
specific baselines. Use the [current roadmap](ROADMAP.md) for remaining gates.

## Workflows

- [Local server and repository workflow](guides/local-development.md)
- [Interactive execution boundaries and skill flows](guides/interactive-execution.md)
- [ACP editor service and Zed setup](guides/editor-acp.md)
- [External BA/SA reasoning and business acceptance](guides/external-reasoning.md)
- [Command agent runtime](guides/command-agent.md)
- [Remote coding worker and OCI tools](guides/remote-coding.md)
- [Submit, inspect and review a repository change](guides/repository-review.md)
- [ACP repository worker and Codex bridge](guides/acp-coding.md)
- [Antigravity runtime packaging and enrollment](guides/antigravity-acp.md)
- [ACP installation preflight](guides/acp-preflight.md) — credential-free initialization
- [Continuation contracts and pure recovery decisions](guides/continuation.md)
- [Operations console and definition studio](guides/console.md)

## Architecture and reference

- [Current architecture and code map](architecture/README.md)
- [Subsystem ownership](architecture/subsystem-ownership.md)
- [Credential registry and local secret backend](architecture/credential-registry.md)
- [ACP worker integration](architecture/acp-agent-integration.md)
- [Self-development control plane](architecture/self-development-control-plane.md)
- [Workflow execution contract](reference/workflow-execution.md)
- [Candidate-bound verification](reference/verification.md)
- [Workflow requirements and acceptance conditions](reference/workflow-requirements.md)
- [Core semantics](reference/engine-semantics.md), [state machines](reference/state-machines.md), [graphs](reference/graphs.md)
- [API/CLI/SDK contract](reference/api-cli-sdk.md), [worker protocol](reference/worker-protocol.md)
- [Agent execution, budgets, delegation and approval](reference/agents.md)
- [Accounting and resource evidence](reference/agent-accounting.md)
- [Container execution and artifact providers](reference/compute-artifacts.md)
- [Timers and signals](reference/timers-signals.md)
- [Child runs, fan-out and limits](reference/children-limits.md)
- [Governance and credential providers](reference/governance.md)
- [Private signed packages](reference/packages.md)
- [Long-term vision](architecture/vision.md) — aspirational, not a support promise

## Operations and contribution

- [Deployment](operations/deployment.md), [observability and lifecycle](operations/observability.md)
- [Backup and restore](operations/backup-restore.md), [upgrades and rotation](operations/upgrades.md)
- [Testing and CI](development/testing.md)
- [Workflow qualification procedures](development/workflow-qualification.md)
- [ACP adapter qualification requirements](development/acp-qualification.md)
- [Dogfood qualification procedure](development/dogfooding.md)
- [Agent onboarding and repository skills](development/agents.md)

Temporary plans, investigations, execution checklists and agent scratch material
belong in the ignored `.local/plans/`, `.local/investigations/` and
`.local/implementation/` directories. After implementation, extract durable
contracts, rationale and operating rules into `docs/`; discard transient history.
See [repository instructions](../AGENTS.md).

## Qualification and compatibility records

These records retain requirement IDs, runtime pins, execution identities, failed
attempts and evidence limits. They do not replace current contracts or setup
instructions. Local generated evidence may be absent from a checkout, and a
fixture pass does not establish owner acceptance or broader deployment support.

- [Interactive workflows: qualification and pending acceptance](operations/interactive-workflows-report.md)
- [Workflow stabilization and self-hosting](operations/r4-stabilization-final-report.md)
- [CLI self-hosting cutover](operations/r4-s8-cli-cutover-report-final-v4.md)
- [Subsystem modularization qualification](operations/r5-modularization-report.md)
- [Codex ACP compatibility](operations/acp-codex-compatibility.md)
- [Antigravity and Claude compatibility](operations/acp-agent-compatibility.md)
- [Coding runtime review and historical accounting](operations/post-q6-hardening.md)
- [Cross-provider adapters and preflight results](operations/cross-provider-coding.md)
- [Provider status discovery qualification](operations/provider-status-discovery.md)
- [Deployable alpha qualification](operations/qualification.md)
- [Remote coding qualification](operations/remote-coding-qualification.md)
- [Historical kernel, interface and release records](archive/README.md)
