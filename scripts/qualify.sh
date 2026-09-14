#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
: "${ORBIT_TEST_DATABASE_URL:?Set a disposable PostgreSQL database; see docs/development/testing.md}"
: "${ORBIT_TEST_S3_ACCESS_KEY:?Provision the local qualification MinIO bucket}"
: "${ORBIT_TEST_S3_SECRET_KEY:?Provision the local qualification MinIO bucket}"
: "${ORBIT_TEST_ACP_IMAGE:?Build the offline pinned ACP fixture image; see docs/development/testing.md}"
export ORBIT_CONTAINER_RUNTIME=${ORBIT_CONTAINER_RUNTIME:-podman}
case "$ORBIT_CONTAINER_RUNTIME" in docker|podman) ;; *) echo 'Select docker or podman' >&2; exit 2;; esac
command -v "$ORBIT_CONTAINER_RUNTIME" >/dev/null
test -f ui/dist/index.html || { echo 'Build the UI first.' >&2; exit 2; }
export PLAYWRIGHT_BROWSERS_PATH=${PLAYWRIGHT_BROWSERS_PATH:-"$PWD/target/playwright"}
export ORBIT_EVIDENCE_DIR=${ORBIT_EVIDENCE_DIR:-"$PWD/target/qualification-alpha"}
# Each case launches its own servers, workers and sometimes multiple containers.
# CPU-count parallelism can exhaust a small qualification host before its short
# lease/fault deadlines. Bound case concurrency, not the assertions or deadlines.
export RUST_TEST_THREADS=${RUST_TEST_THREADS:-2}
cargo test --locked --features fault-injection --test kernel -- --ignored "$@"
