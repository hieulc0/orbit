# Current architecture

Orbit is a Rust control plane for bounded durable work. PostgreSQL owns accepted
state and ordered history; workers perform assignments under leases. The scheduler
does not run repository tools or a provider's model loop.

```text
CLI / API / SDK / MCP / console / editor
                    |
       PostgreSQL control plane and journal
                    |
       leased workers and role coordinator
           /                     \
 provider process            confined tools
 private auth HOME           attempt workspace
           \                     /
       immutable artifacts and candidate-bound verification
```

| Subject | Document |
| --- | --- |
| Accepted-state ownership and module boundaries | [Control plane](control-plane.md) |
| Graphs, attempts, roles and isolation | [Execution model](execution-model.md) |
| Claims, leases, generation fencing and limits | [Scheduler](scheduler.md) |
| Worker lifecycle, supervision, drain and shutdown | [Workers](workers.md) |
| Provider adapters, status and runtime selection | [Providers](providers.md) |
| Catalog, local secret backend and credential staging | [Credentials](credentials.md) |
| PostgreSQL metadata and immutable artifact bytes | [Storage](storage.md) |
| Independent checks, review, repair and evidence | [Verification](verification.md) |
| Uncertainty, retries, continuation and fallback | [Failure recovery](failure-recovery.md) |

[Requirements](../requirements/README.md) state what must remain true.
[Reference](../reference/README.md) records exact interfaces and configuration.
[Operations](../operations/README.md) explains how to operate these mechanisms.
Pending acceptance and conditional extensions belong in [the roadmap](../ROADMAP.md).
