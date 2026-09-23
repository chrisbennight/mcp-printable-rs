#!/usr/bin/env bash
# Build and smoke-test the server locally. GitHub CI publishes releases.
set -euo pipefail
repo_root="$(cd "$(dirname "$0")" && pwd)"
cd "$repo_root"
if [ "$#" -ne 0 ]; then
  echo "usage: $0 (publication runs automatically in GitHub CI)" >&2
  exit 2
fi
identity_output="$(python3 scripts/release_identity.py)"
mapfile -t release_identity <<< "$identity_output"
BASE="${release_identity[0]}"
SOURCE_REPOSITORY="${release_identity[3]}"
smoke_port="${PRINTABLE_SMOKE_PORT:-8000}"
if [[ ! "$smoke_port" =~ ^[1-9][0-9]{0,4}$ ]] || (( smoke_port > 65535 )); then
  echo "PRINTABLE_SMOKE_PORT must be a TCP port from 1 to 65535" >&2
  exit 2
fi
index_build_args=()
if [ -n "${CRATES_INDEX_URL:-}" ]; then
  index_build_args=(--build-arg "CRATES_INDEX_URL=${CRATES_INDEX_URL}")
fi
revision="$(git rev-parse HEAD)"
mkdir -p target/release
docker buildx build --platform linux/amd64 --provenance=false \
  "${index_build_args[@]}" \
  --build-arg "SOURCE_REVISION=${revision}" \
  --build-arg "SOURCE_REPOSITORY=${SOURCE_REPOSITORY}" \
  --target smoke-export --output type=local,dest=target/release .
test -x target/release/printable-smoke
# Run the Linux/amd64 image and drive the MCP smoke against its published port.
smoke() {
  tag="$1"
  platform="$2"
  port="$3"
  # A non-default container port proves the image honors the canonical env var.
  internal_port=8123
  name="printable-smoke-local-$$"
  bearer="$(printf '%064d' 0)"
  cleanup_app() {
    if docker inspect "$name" >/dev/null 2>&1; then
      docker rm -f "$name" >/dev/null
    fi
  }
  trap 'cleanup_app || true' EXIT
  cleanup_app
  docker run -d --name "$name" --platform "$platform" \
    -p "127.0.0.1:${port}:${internal_port}" \
    -e PRINTABLE_HTTP_HOST=0.0.0.0 \
    -e "PRINTABLE_HTTP_PORT=${internal_port}" \
    -e PRINTABLE_WORKSPACE_ROOT=/workspace \
    -e "PRINTABLE_MCP_BEARER=${bearer}" \
    -e "PRINTABLE_ALLOWED_HOSTS=127.0.0.1,127.0.0.1:${port},localhost,localhost:${port}" \
    --tmpfs /workspace:rw,uid=10001,gid=10001,mode=1770 \
    "$tag" >/dev/null
  up=""
  for _ in $(seq 1 60); do
    if curl -fsS "http://127.0.0.1:${port}/healthz" >/dev/null 2>&1; then up=1; break; fi
    sleep 2
  done
  if [ -z "$up" ]; then
    echo "::error /healthz never came up"; docker logs "$name" || true
    cleanup_app
    trap - EXIT
    return 1
  fi
  rc=0
  PRINTABLE_SMOKE_BEARER="$bearer" ./target/release/printable-smoke \
    "http://127.0.0.1:${port}" smoke/expected-tools.txt || rc=$?
  [ "$rc" -ne 0 ] && docker logs "$name" || true
  cleanup_app
  trap - EXIT
  return "$rc"
}

local_tag="${BASE}:dev"
docker build --platform linux/amd64 "${index_build_args[@]}" \
  --build-arg "SOURCE_REVISION=${revision}" \
  --build-arg "SOURCE_REPOSITORY=${SOURCE_REPOSITORY}" -t "$local_tag" .
smoke "$local_tag" linux/amd64 "$smoke_port"
echo "Local build and smoke test passed."
