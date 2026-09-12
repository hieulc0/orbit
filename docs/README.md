# Orbit documentation

Start with [the project overview](../README.md), then choose a workflow. Current
contracts live in reference documents; dated qualification is historical evidence,
not the source of current setup instructions.

## Workflows

- [Local server and repository workflow](guides/local-development.md)
- [Container execution and artifact providers](reference/compute-artifacts.md)
- [Agents, budgets, delegation and approval](reference/agents.md)
- [Command agent runtime](guides/command-agent.md)
- [Remote coding worker and OCI tools](guides/remote-coding.md)
- [Operations console and definition studio](guides/console.md)
- [Timers and signals](reference/timers-signals.md)
- [Child runs, fan-out and limits](reference/children-limits.md)
- [Private signed packages](reference/packages.md)

## Architecture and reference

- [Current architecture and code map](architecture/README.md)
- [Long-term vision](architecture/vision.md) — aspirational, not a support promise
- [Core semantics](reference/engine-semantics.md), [state machines](reference/state-machines.md), [graphs](reference/graphs.md)
- [API/CLI/SDK contract](reference/api-cli-sdk.md), [worker protocol](reference/worker-protocol.md)
- [Governance and credential providers](reference/governance.md)

## Operating and changing Orbit

- [Deployment](operations/deployment.md), [observability and lifecycle](operations/observability.md)
- [Backup and restore](operations/backup-restore.md), [upgrades and rotation](operations/upgrades.md)
- [Testing and CI](development/testing.md), [dogfooding](development/dogfooding.md)
- [Agent onboarding and repository skills](development/agents.md)
- [Current roadmap](ROADMAP.md), [alpha qualification](operations/qualification.md)
- [Remote coding qualification](operations/remote-coding-qualification.md)
- [Historical records](archive/README.md)
