# Recover uncertainty without implicit provider redispatch

Status: accepted architectural constraint.

## Context

A lost acknowledgement cannot distinguish an unperformed effect from one that completed externally.

## Decision

Replay accepted request identities for reconciliation, not as redispatch permission. Keep unresolved provider outcomes conservative; allow only configured bounded recovery with explicit ownership and cleanup.

## Consequences

Some failures require intervention even after local cleanup. Pure continuation/fallback helpers do not imply automatic integration. New fallback policies must preserve budgets, candidate identity and uncertain external effects.
