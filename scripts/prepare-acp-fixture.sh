#!/usr/bin/env bash
# Offline image assembly from an explicitly supplied, verified Codex binary.
set -euo pipefail
if [[ $# != 1 ]]; then
  echo "usage: bash scripts/prepare-acp-fixture.sh /absolute/path/to/codex-0.153.4-linux-musl" >&2
  exit 2
fi
acp_binary=$(realpath "$1")
acp_expected=56ef98ab4032d317ab26e9b5e5a175650717351edb16ed9cde0cb6d1734d62da
acp_actual=$(sha256sum "$acp_binary")
[[ ${acp_actual%% *} == "$acp_expected" ]] || { echo 'Codex binary digest mismatch' >&2; exit 1; }
acp_context=$(mktemp -d /tmp/orbit-acp-fixture.XXXXXX)
cp tests/fixtures/acp.Containerfile "$acp_context/Containerfile"
cp tests/fixtures/acp-workflow.mjs "$acp_context/acp-workflow.mjs"
cp "$acp_binary" "$acp_context/codex"
chmod 755 "$acp_context/codex"
podman --remote=false --cgroup-manager=cgroupfs build --pull=never --network=none --iidfile "$acp_context/image-id" "$acp_context" >&2
echo "Retained disposable build context: $acp_context" >&2
sed -n '1p' "$acp_context/image-id"
