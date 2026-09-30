# Architectural decisions

ADRs preserve lasting choices and their consequences, independently of implementation
plans or execution history. Current mechanisms live in [architecture](../architecture/README.md);
mandatory constraints live in [requirements](../requirements/README.md).

- [PostgreSQL owns accepted state](ADR-001-postgresql-authority.md)
- [Separate metadata from immutable artifact bytes](ADR-002-immutable-artifact-storage.md)
- [Use pinned rootless Podman for repository tools](ADR-003-rootless-repository-isolation.md)
- [Fence worker ownership with leases and generations](ADR-004-durable-leases-and-fencing.md)
- [Separate provider runtimes from accepted workflow authority](ADR-005-provider-boundaries.md)
- [Recover uncertainty without implicit provider redispatch](ADR-006-conservative-recovery.md)
- [Bind authoritative evidence to the exact candidate](ADR-007-candidate-bound-verification.md)
