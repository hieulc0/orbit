#!/usr/bin/env bash
# Build a separate unqualified runtime; preserve the existing terminal image.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
BASE_IMAGE="gcr.io/distroless/base-nossl-debian13@sha256:792f51c506fc67f7eaa38093f6d4937a053cebb79ec0e7c3b7746f6bbba85606"
IMAGE_TAG="localhost/orbit-antigravity-runtime:agy_acp_server_1.1.1-orbit-correlated-tools-v2"

die() { printf 'error: %s\n' "$1" >&2; exit 1; }
verify_sha256() {
  local path=$1 expected=$2 actual
  [[ -f "$path" && ! -L "$path" ]] || die "input is not a regular non-symlink file: $path"
  actual=$(sha256sum -- "$path" | awk '{print $1}')
  [[ "$actual" == "$expected" ]] || die "input digest mismatch: $path"
}

[[ $# -eq 1 ]] || die "usage: bash scripts/build-antigravity-correlated-runtime.sh /path/to/pinned-terminal-binaries"
SOURCE_DIR=$1
verify_sha256 "$SOURCE_DIR/agy_acp_server.par" "98890a0a1afc3ebe91f6018c15bef26b429147e4b61c408d08b2374465fc10c7"
verify_sha256 "$SOURCE_DIR/localharness_external" "d98770b161eb3cc37ae4fa17f088285f9b6ac75c7cb53e0d90ed5077def9d69a"
verify_sha256 "$SCRIPT_DIR/patch-antigravity-terminal.py" "27ce5c2ed5f38f4dc6bd99b0027bf5f54c123938deb46bc53bcd87946f4ff502"
verify_sha256 "$SCRIPT_DIR/patch-antigravity-correlation.py" "94178772fe2c9db54def6a65c70925c638a9a95cad29f3c9b3f06a45144ca3c7"
verify_sha256 "$REPO_ROOT/deploy/antigravity/tool_correlation.py" "1b42234d9f15ad91522ed07afbffe081b9ac330ddc33c73101eb144d46aa791a"
verify_sha256 "$REPO_ROOT/deploy/antigravity/Containerfile.correlated" "a2672f14f4112d1510ef5eb500e21afa671f9427136edfb13df1defe6166d291"
verify_sha256 "$REPO_ROOT/deploy/antigravity/runtime.group" "5a2aa1c5c06249d726a2bd35bba20289a94d37c18cac2485da47f62e07379b25"
[[ $(python3.14 -c 'import sys; print(".".join(map(str, sys.version_info[:3])))') == "3.14.7" ]] \
  || die "Python 3.14.7 is required"
podman --remote=false --cgroup-manager=cgroupfs image exists "$BASE_IMAGE" \
  || die "exact base image must already be provisioned"
if podman --remote=false --cgroup-manager=cgroupfs image exists "$IMAGE_TAG"; then
  die "output tag already exists; refusing to replace an existing runtime"
fi

BUILD_ROOT=$(mktemp -d /tmp/orbit-antigravity-correlation-build.XXXXXXXXXX)
trap 'rm -rf -- "$BUILD_ROOT"' EXIT
CONTEXT="$BUILD_ROOT/context"
mkdir -m 0700 "$CONTEXT"
cp -- "$REPO_ROOT/deploy/antigravity/Containerfile.correlated" "$CONTEXT/Containerfile"
cp -- "$REPO_ROOT/deploy/antigravity/runtime.group" "$CONTEXT/runtime.group"
cp -- "$SOURCE_DIR/localharness_external" "$CONTEXT/localharness_external"
python3.14 "$SCRIPT_DIR/patch-antigravity-correlation.py" \
  "$SOURCE_DIR/agy_acp_server.par" "$CONTEXT/agy_acp_server.par"
verify_sha256 "$CONTEXT/agy_acp_server.par" "c6b1002a3bd35714731661f8a27106c932ace46d5337e995856ddb2aad7cb02f"
podman --remote=false --cgroup-manager=cgroupfs build \
  --pull=never --network=none --platform=linux/amd64 --format=oci --timestamp=0 \
  --tag "$IMAGE_TAG" --file "$CONTEXT/Containerfile" "$CONTEXT"
IMAGE_DIGEST=$(podman --remote=false --cgroup-manager=cgroupfs image inspect --format '{{.Digest}}' "$IMAGE_TAG")
[[ "$IMAGE_DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]] || die "runtime digest unavailable"
printf 'runtime_revision=agy_acp_server_1.1.1-orbit-correlated-tools-v2\n'
printf 'image_ref=%s@%s\nqualification=UNQUALIFIED\n' "$IMAGE_TAG" "$IMAGE_DIGEST"
