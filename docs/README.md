# Orbit documentation

Start with [the project overview](../README.md), then choose a workflow. Current
contracts live in reference documents; dated qualification is historical evidence,
not the source of current setup instructions.

## Workflows

- [Local server and repository workflow](guides/local-development.md)
- [Interactive execution boundaries](guides/interactive-execution.md)
- [Container execution and artifact providers](reference/compute-artifacts.md)
- [Agents, budgets, delegation and approval](reference/agents.md)
- [Command agent runtime](guides/command-agent.md)
- [Remote coding worker and OCI tools](guides/remote-coding.md)
- [Submit, inspect and review a repository change](guides/repository-review.md)
- [ACP coding worker and Codex bridge](guides/acp-coding.md) — experimental runtime,
  offline workflow evidence and live acceptance gaps
- [ACP installation preflight](guides/acp-preflight.md) — credential-free probe,
  not workflow execution
- [Cross-agent continuation and workspace snapshots](guides/continuation.md)
- [Operations console and definition studio](guides/console.md)
- [Timers and signals](reference/timers-signals.md)
- [Child runs, fan-out and limits](reference/children-limits.md)
- [Private signed packages](reference/packages.md)

## Architecture and reference

- [Current architecture and code map](architecture/README.md)
- [Credential registry and local secret backend](architecture/credential-registry.md) — operator-owned credential enrollment, metadata and private secret boundary
- [ACP agent integration](architecture/acp-agent-integration.md) — implemented
  worker/broker boundaries and remaining qualification gates
- [Candidate-bound verification](reference/verification.md) — independent checks, regression selection, managed services and browser evidence
- [Self-development control plane](architecture/self-development-control-plane.md) —
  workflow authority, resource identity, availability and acceptance constraints
- [Long-term vision](architecture/vision.md) — aspirational, not a support promise
- [Core semantics](reference/engine-semantics.md), [state machines](reference/state-machines.md), [graphs](reference/graphs.md)
- [API/CLI/SDK contract](reference/api-cli-sdk.md), [worker protocol](reference/worker-protocol.md)
- [Governance and credential providers](reference/governance.md)

## Operating and changing Orbit

- [Deployment](operations/deployment.md), [observability and lifecycle](operations/observability.md)
- [Backup and restore](operations/backup-restore.md), [upgrades and rotation](operations/upgrades.md)
- [Testing and CI](development/testing.md), [Workflow qualification](development/workflow-qualification.md), [dogfooding](development/dogfooding.md)
- [Agent onboarding and repository skills](development/agents.md)
- [ACP adapter qualification](development/acp-qualification.md)
- [Codex ACP compatibility and qualification record](operations/acp-codex-compatibility.md)
- [Antigravity and Claude compatibility](operations/acp-agent-compatibility.md)
- [Post-Q6 coding runtime hardening and accounting](operations/post-q6-hardening.md)
- [Cross-provider coding adapters and preflights](operations/cross-provider-coding.md)
- [Provider status discovery and offline normalization](operations/provider-status-discovery.md)
- [Current roadmap](ROADMAP.md), [alpha qualification](operations/qualification.md)
- [Remote coding qualification](operations/remote-coding-qualification.md)
- [Historical records](archive/README.md)

## Editor and external reasoning

Editor clients: [ACP and Zed setup](guides/editor-acp.md), [external BA/SA reasoning](guides/external-reasoning.md),
[qualification and acceptance status](operations/interactive-workflows-report.md).
