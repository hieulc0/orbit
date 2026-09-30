# Contributor and agent onboarding

Read [repository instructions](../../AGENTS.md), then the
[current architecture](../architecture/README.md) and [roadmap](../ROADMAP.md).
Use the reference for the requested behavior and inspect its owning module and
tests. Current contracts and qualification scope must be understandable without
prior conversations or implementation plans.

## Repository skills

- [orbit-development](../../skills/orbit-development/SKILL.md): implementation and review contracts.
- [orbit-qualification](../../skills/orbit-qualification/SKILL.md): disposable qualification and evidence review.

Skills refer to canonical docs and scripts. They do not introduce a separate
source of runtime authority. Where the local agent supports repository skill
discovery, configure the existing skill directories using its documented setup;
do not overwrite an existing installation.

## Working notes

Temporary plans, investigations, implementation notes, agent scratch material and
execution checklists belong in the Git-ignored `.local/` directories:

```text
.local/
  plans/
  investigations/
  implementation/
```

After implementation, extract durable architecture, contracts, invariants,
operating procedures and rationale into `docs/`. Retain task/milestone identifiers
only for intentional requirements, qualification, migration or compatibility
traceability. Do not copy a completed plan into documentation as project history.
