#!/usr/bin/env bash
set -euo pipefail

readonly CODEX_VERSION="0.156.0"
readonly CODEX_TARGET="x86_64-unknown-linux-musl"
readonly RELEASE_TAG="rust-v${CODEX_VERSION}"
readonly PACKAGE_SHA256="e8b744b03adb90b296bf632c8a29167e75ea1b9d2980e49d3dfc6e84f5dba749"
readonly CODEX_SHA256="78a11f06e0a2dda42d13fba1d50dc62e8cbdb2d5f69789722f4d4d99b5cdbe30"
readonly CODE_MODE_HOST_SHA256="a5c727845f8418acfe5a3d0ff05ad892d76545e51834da817cf77d6d83bdeb18"
readonly IMAGE_TAG="localhost/orbit-codex:${CODEX_VERSION}-orbit"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
context="$(mktemp -d "${TMPDIR:-/tmp}/orbit-codex-image.XXXXXX")"
cleanup() {
  rm -rf -- "$context"
}
trap cleanup EXIT INT TERM

archive="${context}/codex-package.tar.gz"
package_dir="${context}/codex-package"
package_name="codex-package-${CODEX_TARGET}.tar.gz"
package_url="https://github.com/openai/codex/releases/download/${RELEASE_TAG}/${package_name}"

curl --fail --location --silent --show-error --max-time 300 "$package_url" --output "$archive"
printf '%s  %s\n' "$PACKAGE_SHA256" "$archive" | sha256sum --check --status
mkdir "$package_dir"
tar --extract --gzip --file "$archive" --directory "$package_dir" --no-same-owner

jq --exit-status \
  --arg version "$CODEX_VERSION" \
  --arg target "$CODEX_TARGET" \
  '.version == $version and .target == $target and .variant == "codex" and .entrypoint == "bin/codex"' \
  "${package_dir}/codex-package.json" >/dev/null
printf '%s  %s\n' "$CODEX_SHA256" "${package_dir}/bin/codex" | sha256sum --check --status
printf '%s  %s\n' "$CODE_MODE_HOST_SHA256" "${package_dir}/bin/codex-code-mode-host" | sha256sum --check --status
test -x "${package_dir}/bin/codex"
test -x "${package_dir}/bin/codex-code-mode-host"

podman build \
  --pull=never \
  --file "${repo_root}/deploy/codex/Containerfile" \
  --tag "$IMAGE_TAG" \
  "$context"

version="$(podman run --rm --pull=never --network=none --read-only \
    --cap-drop=ALL --security-opt=no-new-privileges --pids-limit=16 \
    --userns=keep-id --user "$(id -u):$(id -g)" \
    --memory=256m --memory-swap=256m \
    --tmpfs /tmp:rw,nosuid,nodev,size=16777216 \
    --workdir /tmp --env HOME=/tmp --env CODEX_HOME=/tmp \
    --entrypoint /opt/codex/bin/codex "$IMAGE_TAG" --version)"
test "$version" = "codex-cli ${CODEX_VERSION}"

podman run --rm --pull=never --network=none --read-only \
  --cap-drop=ALL --security-opt=no-new-privileges --pids-limit=16 \
  --userns=keep-id --user "$(id -u):$(id -g)" \
  --memory=256m --memory-swap=256m \
  --tmpfs /tmp:rw,nosuid,nodev,size=16777216 \
  --workdir /tmp --env HOME=/tmp --env CODEX_HOME=/tmp \
  --entrypoint /opt/codex/bin/codex "$IMAGE_TAG" app-server --help >/dev/null

podman run --rm --pull=never --network=none --read-only \
  --cap-drop=ALL --security-opt=no-new-privileges --pids-limit=16 \
  --userns=keep-id --user "$(id -u):$(id -g)" \
  --memory=256m --memory-swap=256m \
  --tmpfs /tmp:rw,nosuid,nodev,size=16777216 \
  --workdir /tmp --env HOME=/tmp --env CODEX_HOME=/tmp \
  --entrypoint /opt/codex/bin/codex-code-mode-host "$IMAGE_TAG" --help >/dev/null

digest="$(podman image inspect --format '{{.Digest}}' "$IMAGE_TAG")"
case "$digest" in
  sha256:????????????????????????????????????????????????????????????????)
    printf 'Verified Codex image: %s@%s\n' "${IMAGE_TAG%:*}" "$digest"
    ;;
  *)
    echo "Podman did not report an immutable image digest for $IMAGE_TAG" >&2
    exit 1
    ;;
esac
