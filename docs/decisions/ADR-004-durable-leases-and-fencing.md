# Fence worker ownership with leases and generations

Status: accepted architectural constraint.

## Context

Process liveness and delayed messages cannot determine whether a worker still owns a task.

## Decision

Persist ownership epochs and confirmed lease bounds. Recheck generation, lease, cancellation and request identity on every accepted operation, including after I/O.

## Consequences

Expired owners cannot publish authoritative outputs. New attempts do not reuse stale ownership. Local process shutdown remains separately observed, and draining preserves existing leases.
