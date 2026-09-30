# Bind authoritative evidence to the exact candidate

Status: accepted architectural constraint.

## Context

An agent can mutate code or tests after an exploratory command, making an earlier pass irrelevant to the final result.

## Decision

Use Orbit-controlled independent verification and review under immutable policy, bound to WorkspaceState. Recheck candidate identity before completion and explicit apply.

## Consequences

Mutation invalidates old passing evidence for completion. Provider claims do not substitute for checks. Frozen external requirements additionally require BA attestation for the exact technically accepted candidate.
