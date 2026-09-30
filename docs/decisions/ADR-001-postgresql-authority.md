# PostgreSQL owns accepted state

Status: accepted architectural constraint.

## Context

Concurrent servers and unreliable workers need one observable authority for state, requests and history.

## Decision

Store accepted transitions, journals, ownership and deduplicated receipts in PostgreSQL. Serialize coordination under shared database authority.

## Consequences

Restart and response loss can be reconciled from committed state. Coordination favors bounded auditable correctness; it is not a high-throughput or HA guarantee. External I/O must release coordination locks and recheck authority before acceptance.
