#!/usr/bin/env bash
# Build a new, unqualified Antigravity runtime from verified preserved inputs.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
CONTAINERFILE="$REPO_ROOT/deploy/antigravity/Containerfile.reproducible"
GROUP_FILE="$REPO_ROOT/deploy/antigravity/runtime.group"

EXPECTED_SERVER_SHA256="267affa691085fe5d78895e34dffe723d6528713e01bd37ed40feb7b43d1f4c7"
EXPECTED_HARNESS_SHA256="d98770b161eb3cc37ae4fa17f088285f9b6ac75c7cb53e0d90ed5077def9d69a"
EXPECTED_PATCHED_SERVER_SHA256="98890a0a1afc3ebe91f6018c15bef26b429147e4b61c408d08b2374465fc10c7"
EXPECTED_PATCHER_SHA256="27ce5c2ed5f38f4dc6bd99b0027bf5f54c123938deb46bc53bcd87946f4ff502"
EXPECTED_CLIENT_TERMINAL_SHA256="c9a93b16ffca08e313026eee9fbcab1e793368b9b668c6a8351d96822ff33024"
EXPECTED_CONTAINERFILE_SHA256="9ff3a5898c3df0ab304985fa2372dd3e52b21372ff644f03270e5419c6e6b86e"
EXPECTED_RUNTIME_GROUP_SHA256="5a2aa1c5c06249d726a2bd35bba20289a94d37c18cac2485da47f62e07379b25"
BASE_IMAGE="gcr.io/distroless/base-nossl-debian13@sha256:792f51c506fc67f7eaa38093f6d4937a053cebb79ec0e7c3b7746f6bbba85606"
IMAGE_TAG="localhost/orbit-antigravity-runtime:agy_acp_server_1.1.1-orbit-terminal-v2"
RUNTIME_REVISION="agy_acp_server_1.1.1-orbit-terminal-v2"

die() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

[[ $# -eq 1 ]] || die "usage: bash scripts/build-antigravity-runtime.sh /path/to/pinned-antigravity-binaries"
AGY_SOURCE_DIR=$1
SERVER_SOURCE="$AGY_SOURCE_DIR/agy_acp_server.par"
HARNESS_SOURCE="$AGY_SOURCE_DIR/localharness_external"

verify_sha256() {
  local label=$1 path=$2 expected=$3 actual
  [[ -f "$path" && ! -L "$path" ]] || die "$label is missing or is not a regular non-symlink file"
  actual=$(sha256sum -- "$path" | awk '{print $1}')
  [[ "$actual" == "$expected" ]] || die "$label SHA-256 mismatch (expected $expected, got $actual)"
}

verify_sha256 "agy_acp_server.par" "$SERVER_SOURCE" "$EXPECTED_SERVER_SHA256"
verify_sha256 "localharness_external" "$HARNESS_SOURCE" "$EXPECTED_HARNESS_SHA256"
verify_sha256 "tracked patcher" "$SCRIPT_DIR/patch-antigravity-terminal.py" "$EXPECTED_PATCHER_SHA256"
verify_sha256 "tracked client terminal" "$REPO_ROOT/deploy/antigravity/client_terminal.py" "$EXPECTED_CLIENT_TERMINAL_SHA256"
verify_sha256 "reproducible Containerfile" "$CONTAINERFILE" "$EXPECTED_CONTAINERFILE_SHA256"
verify_sha256 "runtime group input" "$GROUP_FILE" "$EXPECTED_RUNTIME_GROUP_SHA256"

python_version=$(python3.14 -c 'import sys; print(".".join(map(str, sys.version_info[:3])))') \
  || die "Python 3.14.7 is required to regenerate the pinned Python 3.14 bytecode"
[[ "$python_version" == "3.14.7" ]] || die "expected Python 3.14.7, got $python_version"
grep -Fqx "FROM $BASE_IMAGE" "$CONTAINERFILE" || die "Containerfile base-image pin differs from the reviewed digest"

# Never pull implicitly and never overwrite an existing output tag.
if podman --remote=false --cgroup-manager=cgroupfs image exists "$IMAGE_TAG"; then
  die "output tag already exists; refusing to retag existing local evidence"
fi
podman --remote=false --cgroup-manager=cgroupfs image exists "$BASE_IMAGE" \
  || die "exact digest-pinned base image is not local; fetch that exact digest separately before building"

BUILD_ROOT=$(mktemp -d /tmp/orbit-antigravity-runtime-build.XXXXXXXXXX)
cleanup() {
  case "$BUILD_ROOT" in
    /tmp/orbit-antigravity-runtime-build.*) rm -rf -- "$BUILD_ROOT" ;;
    *) printf 'warning: not removing unexpected temporary path\n' >&2 ;;
  esac
}
trap cleanup EXIT

CONTEXT="$BUILD_ROOT/context"
mkdir -m 0700 "$CONTEXT"
cp -- "$CONTAINERFILE" "$CONTEXT/Containerfile"
cp -- "$GROUP_FILE" "$CONTEXT/runtime.group"
cp -- "$HARNESS_SOURCE" "$CONTEXT/localharness_external"

python3.14 "$SCRIPT_DIR/patch-antigravity-terminal.py" \
  "$SERVER_SOURCE" "$CONTEXT/agy_acp_server.par"
verify_sha256 "generated terminal-patched ACP server" \
  "$CONTEXT/agy_acp_server.par" "$EXPECTED_PATCHED_SERVER_SHA256"
verify_sha256 "staged localharness_external" \
  "$CONTEXT/localharness_external" "$EXPECTED_HARNESS_SHA256"

podman --remote=false --cgroup-manager=cgroupfs build \
  --pull=never \
  --network=none \
  --platform=linux/amd64 \
  --format=oci \
  --timestamp=0 \
  --tag "$IMAGE_TAG" \
  --iidfile "$BUILD_ROOT/image-id" \
  --file "$CONTEXT/Containerfile" \
  "$CONTEXT"

IMAGE_DIGEST=$(podman --remote=false --cgroup-manager=cgroupfs image inspect \
  --format '{{.Digest}}' "$IMAGE_TAG")
[[ "$IMAGE_DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]] \
  || die "Podman did not report a usable immutable digest for the newly built image"

printf 'runtime_revision=%s\n' "$RUNTIME_REVISION"
printf 'image_ref=%s@%s\n' "$IMAGE_TAG" "$IMAGE_DIGEST"
printf 'image_id=%s\n' "$(<"$BUILD_ROOT/image-id")"
printf 'qualification=UNQUALIFIED\n'
