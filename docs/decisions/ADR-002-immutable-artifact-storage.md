# Separate metadata from immutable artifact bytes

Status: accepted architectural constraint.

## Context

Patches, logs and reports exceed coordination-state scope and must survive worker replacement.

## Decision

Keep metadata in PostgreSQL and immutable bytes in explicit local or S3-compatible providers. RustFS is the local/CI S3-compatible target; it is not a mandatory production service.

## Consequences

Hash and size verification precede acceptance. Publication conflicts cannot overwrite bytes. Database-only backups are insufficient, and backend changes do not reinterpret existing artifact locations.
