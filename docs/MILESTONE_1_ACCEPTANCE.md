# Milestone 1 Qualification Result

## Automated qualification

Run on 2026-09-07 in the Linux development environment with Rust 1.98.1 and
PostgreSQL 17:

```sh
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_EVIDENCE_DIR="$PWD/target/qualification" \
cargo test --locked --features fault-injection --test kernel -- --ignored
```

Result: **8 passed, 0 failed** in 12.49 seconds.

The passing suite covered:

- durable recovery, fencing, idempotency, and artifact ownership;
- concurrent claim/cancellation/completion behavior;
- policy limits, uncertainty, retry exhaustion, and deadlines;
- invalid outputs, failed checks, and cancellation during backoff;
- a real repository change through HTTP workers;
- server and worker process termination and recovery;
- transaction-boundary server termination;
- standalone local recovery without engine mutation.

Evidence was retained under `target/qualification` and occupied approximately
15 MiB after this run. PostgreSQL and artifact storage were retained during the
run. The generated evidence is ignored and is not committed.

The evidence bundle is not yet safe to distribute: generated fixture
`server.json` files contain ephemeral runtime test tokens. They are outside Git
and belong only to the disposable local qualification environment, but an
evidence-export/redaction step is still required before sharing the bundle.

## Current qualification status

The executable qualification suite passes. The Milestone 1 acceptance gate is
still **pending** because the specification also requires:

1. An operator review of the recovered patch, test report, and redacted evidence.
2. A case-by-case mapping from the failure matrix to retained evidence.
3. Orbit-on-Orbit dogfooding against a committed, pinned Orbit revision.

Checkpoint continuation is not advertised and remains intentionally unimplemented.
Storage-loss recovery, high availability, throughput, sandbox enforcement, and
production readiness are outside this milestone.
