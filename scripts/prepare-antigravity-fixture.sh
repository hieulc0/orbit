#!/usr/bin/env bash
# Offline image assembly from explicitly supplied, verified Antigravity ACP binaries.
set -euo pipefail

DEFAULT_DIR="$HOME/.local/share/zed/external_agents/registry/antigravity-acp/v_1.1.1_c5752c93158aa0bc_eef079d17742fe39"

AGY_DIR="${1:-$DEFAULT_DIR}"

if [[ ! -d "$AGY_DIR" ]]; then
  echo "usage: bash scripts/prepare-antigravity-fixture.sh [/path/to/antigravity-dir]" >&2
  echo "Expected directory containing agy_acp_server.par and localharness_external" >&2
  exit 2
fi

SERVER_PAR="$AGY_DIR/agy_acp_server.par"
LOCALHARNESS="$AGY_DIR/localharness_external"

EXPECTED_SERVER_HASH="267affa691085fe5d78895e34dffe723d6528713e01bd37ed40feb7b43d1f4c7"
EXPECTED_HARNESS_HASH="d98770b161eb3cc37ae4fa17f088285f9b6ac75c7cb53e0d90ed5077def9d69a"

ACTUAL_SERVER_HASH=$(sha256sum "$SERVER_PAR" | awk '{print $1}')
ACTUAL_HARNESS_HASH=$(sha256sum "$LOCALHARNESS" | awk '{print $1}')

if [[ "$ACTUAL_SERVER_HASH" != "$EXPECTED_SERVER_HASH" ]]; then
  echo "Error: agy_acp_server.par hash mismatch!" >&2
  echo "Expected: $EXPECTED_SERVER_HASH" >&2
  echo "Actual:   $ACTUAL_SERVER_HASH" >&2
  exit 1
fi

if [[ "$ACTUAL_HARNESS_HASH" != "$EXPECTED_HARNESS_HASH" ]]; then
  echo "Error: localharness_external hash mismatch!" >&2
  echo "Expected: $EXPECTED_HARNESS_HASH" >&2
  echo "Actual:   $ACTUAL_HARNESS_HASH" >&2
  exit 1
fi

BUILD_CONTEXT=$(mktemp -d /tmp/orbit-antigravity-fixture.XXXXXX)
trap 'rm -rf "$BUILD_CONTEXT"' EXIT

cat << 'EOF' > "$BUILD_CONTEXT/Containerfile"
FROM docker.io/library/debian:bookworm-slim
RUN echo "nobody:x:65534:" >> /etc/group
RUN mkdir -p /opt/antigravity /etc/ssl/certs /etc/orbit
COPY ssl-certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY agent-capabilities.json /etc/orbit/agent-capabilities.json
COPY --chmod=755 agy_acp_server.par /opt/antigravity/agy_acp_server.par
COPY --chmod=755 localharness_external /opt/antigravity/localharness_external
ENV GEMINI_HOME=/orbit/home/.gemini \
    AGY_ACP_FORCE_FILE_STORAGE=1 \
    NO_BROWSER=1 \
    ANTIGRAVITY_HARNESS_PATH=/opt/antigravity/localharness_external \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    REQUESTS_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt
WORKDIR /orbit/home
ENTRYPOINT ["/opt/antigravity/agy_acp_server.par"]
EOF
cat << 'CAP_EOF' > "$BUILD_CONTEXT/agent-capabilities.json"
{
  "agent": "antigravity-acp",
  "runtime_version": "1.1.1",
  "models": [
    {
      "id": "gemini-3.8-flash",
      "display_name": "Gemini 3.8 Flash",
      "reasoning_efforts": ["low", "medium", "high"]
    },
    {
      "id": "gemini-3.7-flash",
      "display_name": "Gemini 3.7 Flash",
      "reasoning_efforts": ["low", "medium", "high"]
    },
    {
      "id": "gemini-3.6-flash",
      "display_name": "Gemini 3.6 Flash",
      "reasoning_efforts": ["low", "medium", "high"]
    },
    {
      "id": "gemini-3.1-pro",
      "display_name": "Gemini 3.1 Pro",
      "reasoning_efforts": ["low", "high"]
    }
  ]
}
CAP_EOF

mkdir -p "$BUILD_CONTEXT/ssl-certs"
cp -L /etc/ssl/certs/ca-certificates.crt "$BUILD_CONTEXT/ssl-certs/ca-certificates.crt"
cp "$SERVER_PAR" "$BUILD_CONTEXT/agy_acp_server.par"
cp "$LOCALHARNESS" "$BUILD_CONTEXT/localharness_external" 

IMAGE_TAG="localhost/orbit-antigravity:1.1.1"

podman --remote=false --cgroup-manager=cgroupfs build \
  --pull=never \
  --network=none \
  -t "$IMAGE_TAG" \
  --iidfile "$BUILD_CONTEXT/image-id" \
  "$BUILD_CONTEXT" >&2

IMAGE_ID=$(cat "$BUILD_CONTEXT/image-id")
echo "Successfully built $IMAGE_TAG ($IMAGE_ID)" >&2
echo "$IMAGE_ID"
