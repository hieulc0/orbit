# Orbit documentation

`docs/` contains durable knowledge about Orbit. Each subject has an authoritative
home; temporary plans, investigations and run evidence stay in the ignored `.local/` tree.

| Directory | Authority |
| --- | --- |
| [Architecture](architecture/README.md) | How the current system works, its ownership boundaries and recovery mechanisms |
| [Requirements](requirements/README.md) | Constraints and invariants every implementation must preserve |
| [Reference](reference/README.md) | Configuration, database concepts, CLI commands, API contracts and terminology |
| [Operations](operations/README.md) | Installation, deployment, upgrades, diagnostics and recovery procedures |
| [Decisions](decisions/README.md) | Lasting choices, their context and consequences |
| [Roadmap](ROADMAP.md) | Pending acceptance and future work only |

## Workflows

Start with [installation](operations/installation.md) for a local server, workers,
credentials, repository changes, the editor service or external BA/SA reasoning.
Use [troubleshooting](operations/troubleshooting.md) for observed failures and
qualification prerequisites. [CLI](reference/cli.md) and [API](reference/api.md)
are interface references; procedures cannot grant authority absent from their contracts.

## Working-note lifecycle

Use `.local/plans/` before implementation, `.local/investigations/` for debugging,
`.local/implementation/` for checklists and agent notes, `.local/qualification/`
for execution evidence, and `.local/scratch/` for temporary commands and experiments.

Promote only durable conclusions into architecture, requirements, reference,
operations or decisions. Run IDs, captured output, pass/fail tables, temporary
campaigns and completed implementation narratives do not belong in `docs/`.
See [repository instructions](../AGENTS.md).
