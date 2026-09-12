# Testing and CI

Run from the repository root. Linux, Git, Rust 1.98.1 (rustfmt/Clippy), Node.js
24/npm and Python 3.11+ are required. `npm --prefix ui ci --ignore-scripts`
installs the locked UI toolchain. Build the UI before real-browser qualification.

```sh
bash scripts/check.sh
bash scripts/check.sh rust
bash scripts/check.sh python
bash scripts/check.sh docs
```

`all` includes Rust/Python checks and the strict UI build, not a hidden database
or browser provisioner. `ui` additionally runs Playwright; install its pinned
Chromium first:

```sh
cd ui
PLAYWRIGHT_BROWSERS_PATH="$PWD/../target/playwright" npx playwright install chromium --only-shell
cd ..
bash scripts/check.sh ui
```

## Disposable database/process qualification

The root Compose file is for qualification only. It must never be pointed at
deployment volumes. The credentials below belong only to its loopback fixtures.

```sh
docker compose --profile compute up -d --wait
docker compose --profile compute exec -T minio mc alias set qualification \
  http://127.0.0.1:9000 orbit-local-test orbit-local-test-secret
docker compose --profile compute exec -T minio mc mb --ignore-existing qualification/orbit-qualification
podman pull docker.io/library/alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b
npm --prefix ui run build

ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_TEST_S3_ACCESS_KEY=orbit-local-test \
ORBIT_TEST_S3_SECRET_KEY=orbit-local-test-secret \
bash scripts/qualify.sh
```

Select `ORBIT_CONTAINER_RUNTIME=docker` only after provisioning the same image in
that runtime. The shell entry point does not pull images, provision services or
silently skip prerequisites. Test schemas and `target/qualification-alpha` are
retained. Use a targeted `cargo test --locked --test kernel NAME -- --ignored`
for a database-only case; pass `fault-injection` for transaction-barrier tests.

The `remote_coding` cases require local Git/Python and rootless Podman with the
pinned Alpine image even if legacy `ORBIT_CONTAINER_RUNTIME=docker` is selected.
They run authenticated loopback Git and deterministic Responses fixtures, never
a paid provider, private production repository or remote deployment:

```sh
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_EVIDENCE_DIR="$PWD/target/qualification-remote-coding" \
cargo test --locked --features fault-injection --test kernel remote_coding -- --ignored
```

Regular `tests/execution.rs` and the repository-helper unit test need no database,
container or credential account. Shared engine changes still require the full
qualification suite, including legacy agent, artifact, governance and digest cases.

## CI and evidence

CI runs regular checks, mocked Chromium, the full disposable suite and image
build/smoke checks. It uses read-only repository permissions and no model or cloud
credentials. Third-party actions are pinned by commit. Image publication and
remote installation are not part of PR checks. Qualification output stays local
to its runner; no raw evidence/workspaces are uploaded automatically.

Export with `orbit export-evidence` to a new destination before review. Verify
manifest hashes and accepted artifacts, then inspect command arguments and raw
artifacts for credentials. Automated redaction does not make a bundle public.
Update [qualification](../operations/qualification.md) with checks actually run,
backend/host differences and remaining gates. Rebuild `cargo build --locked`
without `fault-injection` before normal use.
