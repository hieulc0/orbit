# Use pinned rootless Podman for repository tools

Status: accepted architectural constraint.

## Context

Repository commands must not inherit worker credentials, host homes, runtime sockets or unrelated workspace state.

## Decision

Use operator-pinned rootless OCI profiles with no tool network, read-only roots, dropped capabilities, no-new-privileges and bounded resources. Developer-local exploratory execution requires its own explicit namespace profile; final verification stays independent.

## Consequences

Unavailable isolation fails closed. Operators provision images/tools before execution. Trusted containment does not establish hostile multi-tenant safety; stronger backends require separate threat models and qualification.
