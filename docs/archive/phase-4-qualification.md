# Phase 4 qualification

Phase 4's bounded CPU/OCI and artifact contract is implemented and qualified
with PostgreSQL, MinIO and rootless Podman. Do not infer production readiness,
hardware GPU qualification or completion of later roadmap phases.
See [compute and artifact semantics](../reference/compute-artifacts.md).

## Local services and commands

Use only the disposable Compose services and fixture workspaces:

```sh
docker compose --profile compute up -d --wait
docker compose --profile compute exec -T minio mc alias set qualification \
  http://127.0.0.1:9000 orbit-local-test orbit-local-test-secret
docker compose --profile compute exec -T minio mc mb --ignore-existing \
  qualification/orbit-qualification
podman pull docker.io/library/alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b

cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
python3 -m unittest discover -s sdk/python -p 'test_*.py'

ORBIT_CONTAINER_RUNTIME=podman \
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_TEST_S3_ACCESS_KEY=orbit-local-test \
ORBIT_TEST_S3_SECRET_KEY=orbit-local-test-secret \
ORBIT_EVIDENCE_DIR="$PWD/target/qualification-phase4" \
cargo test --locked --features fault-injection --test kernel -- --ignored
```

The container case requires the Alpine digest in `examples/container.yaml`
already available in the selected runtime. For a working Docker installation,
provision it there and select `ORBIT_CONTAINER_RUNTIME=docker`. Provisioning an
image is an operator action.
The MinIO credentials above belong only to the disposable local test service.
MinIO is optional outside this qualification profile. Evidence and retained S3
test prefixes are disposable local data; review before exporting or sharing.

## Evidence mapping

| Test | Contract |
| --- | --- |
| `compute_definitions_require_pinned_images_and_bounded_resources` | Repository-free compilation, immutable plan changes, invalid image/command/resource rejection |
| `immutable_local_artifacts_survive_reopen_and_reject_corruption` | Concurrent immutable publication, fresh provider read, conflict/corruption/path rejection |
| `supervisor_cleans_up_after_lifeline_process_dies` | Actual lifeline process termination triggers supervisor cleanup against a controlled CLI fixture; not OCI qualification |
| `local_container_recovery_preserves_result_and_provenance` | CLI local-only recovery through a controlled runtime fixture preserves fresh attempt identity, result, logs and report checksums |
| `slow_storage_verification_never_blocks_leases_or_cancellation` | A deliberately delayed storage read leaves renewal and cancellation responsive and rejects publication after cancellation |
| `resource_reservations_and_pools_across_servers` | Shared resource reservations, wrong-pool rejection, conflicting server profile rejection, cancellation release and queue/worker inspection |
| `gpu_device_reservations_and_capability_requirements` | Required-capability matching, disjoint logical GPU devices across simultaneous server claims, exhaustion and device reuse after cancellation; no GPU hardware execution |
| `s3_artifacts_reopen_immutable_and_fenced` | Actual MinIO bytes, restart with changed default provider, denied unrelated reader, duplicate publication, conflict rejection, completion retransmission and stale completion fencing |
| `container_worker_outputs_cancellation_and_kill_cleanup` | Actual OCI output through rootless Podman, API submission without a repository, cancellation, combined server/worker SIGKILL, independent cleanup, fresh retry and recovered result |
| Existing 28 kernel cases | Repository execution, retry/fencing, recovery, child runs, timers/signals, SSE and SDK compatibility |

## Completion record and evidence review

On 2026-09-12 all 33 PostgreSQL/process/S3/OCI cases passed together with the
default concurrent runner in 14.14 seconds. All 11 regular Rust tests, formatting,
Clippy with warnings denied and the Python transport test also passed. No task
deadline, lease duration or retry limit was relaxed to obtain these results.

The reviewed local export is `target/qualification-phase4-review`: 1,409 files,
4,381,180 bytes, covering 41 retained scenario directories including earlier
successful runs. Its manifest checksums and sizes were independently rechecked.
Runtime fixtures were excluded; structured lease/operator credentials and the
known fixture tokens were absent from the export. S3 artifacts were downloaded
through the provider and retained for review alongside local artifacts. This is
local generated evidence, not committed or published evidence. Review arbitrary
command arguments and artifact bytes again before sharing a new export.

Qualification found an HTTP client option ordering bug in S3 configuration and
an uncertain/concurrent PUT response case. The provider now preserves explicit
HTTP opt-in and reconciles a failed PUT response by verifying the existing
object, without falling back to overwrite. Delayed verification keeps heartbeats
and cancellation responsive and rechecks ownership before commit.

The host Docker daemon stalled on direct no-op container-create requests outside
Orbit, including a 150-second warm-up. It was not restarted or used to interrupt
unrelated workloads. Rootless Podman provided independent, successful OCI
qualification. The Docker adapter's live behavior on this host remains
unqualified; this is not presented as a passing Docker test. Named, stopped probe
containers were removed; retained fixtures and exported evidence remain local.

## Scope limits

The qualified release supports bounded CPU containers, logical GPU reservations,
the local provider and conditional-write S3-compatible storage. Physical GPU/CDI
execution, hostile-code isolation, remote daemon mounts, Kubernetes scheduling,
multipart transfer, presigned URLs, automatic retention and cloud identity
federation remain outside this bounded qualification. Storage/server power-loss
and production performance claims also require separate evidence. Milestone 1's
owner acceptance and its historical evidence gaps are unchanged.
