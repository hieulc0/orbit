# Pinned Orbit-on-Orbit qualification

This case uses a separately built committed Orbit revision to operate on a
disposable clone of itself. It never modifies, commits or pushes the developer
checkout. Coding is a deterministic operator-provisioned command, not a paid model.

```sh
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_EVIDENCE_DIR="$PWD/target/qualification-alpha" \
ORBIT_DOGFOOD_REVISION=9b9e3d252615589104067d2d272124fc795b3f2b \
cargo test --locked --features fault-injection --test kernel dogfood:: -- --ignored
```

Choose a reviewed full 40-character committed revision; no mutable refs. Without
the variable the test resolves HEAD once and records the full commit. The Rust
toolchain and locked crates must already be cached: baseline/candidate builds
use `--offline`. Keep sufficient disk space for a separate debug build. The normal
qualification suite includes this case, so do not recursively run ignored kernel
tests inside the candidate check.

[The test](../../tests/kernel/dogfood.rs) creates a random PostgreSQL schema and
fresh source clone, checks out the pin, builds a separate baseline binary, and
starts that binary as the server and both repository workers. The baseline
appends a bounded reproducibility section to README.md. A separate test workspace
applies the accepted patch and runs [the independent check wrapper](../../tests/fixtures/dogfood-check.py):
README-only diff, formatting, and regular locked/offline Rust tests. Its lease is
15 seconds with ongoing heartbeats; task and test deadlines remain bounded.

Assertions require SUCCEEDED, an accepted manifest pinned to that commit with
only README.md changed, and an unchanged source clone. The qualification record
contains the baseline commit/binary hash and run ID. Artifacts include the patch,
manifest, logs and test report. Private source/build/workspaces remain under
`target/qualification-alpha/fixtures`; export excludes them.

## Review, not automatic acceptance

Export into a new destination using `orbit export-evidence`. Recheck manifest
hashes, inspect the actual patch and every test-report command/exit result, and
match the run ID to the baseline record. The test proves a bounded self-hosted
repository workflow, not arbitrary autonomous development, live-model competence,
or production readiness. Project-owner acceptance remains a separate decision.
Current evidence and failed/unqualified cases are in [qualification](../operations/qualification.md).
