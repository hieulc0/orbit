# Separate provider runtimes from accepted workflow authority

Status: accepted architectural constraint.

## Context

Providers expose different authentication, tools, usage and protocol semantics, while Orbit must keep one policy and evidence boundary.

## Decision

Bind each runtime and credential explicitly. Route repository effects through Orbit-owned callbacks with permissions and audit; separate private provider HOME from the candidate workspace.

## Consequences

Initialization, native tool displays and agent text cannot qualify mediation. Unknown billing stays unknown. A new provider or runtime version needs evidence for its exact effect boundary without changing legacy bindings.
