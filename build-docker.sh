#!/usr/bin/env bash
# Local image build + MCP container smoke. Default: build the Linux/amd64 Rust
# image and smoke it without publishing. The opt-in --push path preserves the
# release invariant: test, push matching commit-scoped Linux/amd64 Rust and
# Blender and CAD tags, smoke each, prove the exact Blender digest on the NVIDIA
# host, and only then publish their digest-pinned release record. The --push path
# requires a trusted NVIDIA host, a coexistence workload, and an authenticated registry client.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")" && pwd)"
cd "$repo_root"

identity_output="$(python3 scripts/release_identity.py)"
mapfile -t release_identity <<< "$identity_output"
BASE="${release_identity[0]}"
BLENDER_BASE="${release_identity[1]}"
RELEASE_BASE="${release_identity[2]}"
SOURCE_REPOSITORY="${release_identity[3]}"
CAD_BASE="${release_identity[4]}"

push=0
if [ "${1:-}" = "--push" ]; then
  push=1
elif [ -n "${1:-}" ]; then
  echo "usage: $0 [--push]" >&2
  exit 2
fi

# A local build may fall back to crates.io; it publishes nothing. A publishing
# run may not: an absent address there means the fleet's injection regressed,
# and the pushed image would carry crates that bypassed the proxy's cache,
# audit, and blocklist.
if [ "$push" -eq 1 ] && [ -z "${CRATES_INDEX_URL:-}" ]; then
  echo "CRATES_INDEX_URL is unset; refusing to publish images built outside the artifact proxy" >&2
  exit 1
fi


# Where crates come from, for the builds that compile Rust. Forwarded by value
# because the daemon never sees this shell's environment, and only when the name
# holds one: an empty value would pin a source naming an empty registry. With
# nothing configured the builds resolve from crates.io, which is what makes them
# work away from the network the proxy lives on.
index_build_args=()
if [ -n "${CRATES_INDEX_URL:-}" ]; then
  index_build_args=(--build-arg "CRATES_INDEX_URL=${CRATES_INDEX_URL}")
fi

# The publishing path also runs cargo on this host, outside any image, and those
# commands resolve dependencies too. Passed as cargo's own --config assignments
# rather than by writing .cargo/config.toml: this path refuses to publish from a
# dirty worktree, and a generated file would be exactly that.
cargo_index_args=()
if [ -n "${CRATES_INDEX_URL:-}" ]; then
  cargo_index_args=(
    --config 'source.crates-io.replace-with="mirror"'
    --config "source.mirror.registry=\"${CRATES_INDEX_URL}\""
  )
fi

short_sha="$(git rev-parse --short=12 HEAD)"
revision="$(git rev-parse HEAD)"

echo "==> Building the smoke driver"
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
  port="${3:-8000}"
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

smoke_blender() {
  tag="$1"
  blender_mode="${2:-background}"
  name="printable-blender-smoke-local-$$"
  cleanup_blender() {
    if docker inspect "$name" >/dev/null 2>&1; then
      docker rm -f "$name" >/dev/null
    fi
  }
  trap 'cleanup_blender || true' EXIT
  cleanup_blender
  docker run -d --name "$name" --platform linux/amd64 \
    --read-only \
    --cap-drop ALL \
    --security-opt no-new-privileges \
    --pids-limit 512 \
    --tmpfs /tmp:rw,uid=10001,gid=10001,mode=1770 \
    --tmpfs /home/printable:rw,uid=10001,gid=10001,mode=0750 \
    --tmpfs /run/printable-blender:rw,uid=10001,gid=10001,mode=0750 \
    --tmpfs /workspace:rw,uid=10001,gid=10001,mode=1770 \
    -e "PRINTABLE_BLENDER_MODE=${blender_mode}" \
    "$tag" >/dev/null
  healthy=""
  for _ in $(seq 1 90); do
    state="$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{end}}' "$name")"
    if [ "$state" = healthy ]; then healthy=1; break; fi
    if [ "$(docker inspect --format '{{.State.Running}}' "$name")" != true ]; then
      docker logs "$name" || true
      return 1
    fi
    sleep 2
  done
  if [ -z "$healthy" ]; then
    docker logs "$name" || true
    return 1
  fi
  ip="$(docker inspect --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$name")"
  if [ "$blender_mode" = ui ]; then
    python3 addon/integration_smoke.py --host "$ip" --mode ui
  fi
  python3 addon/integration_smoke.py --host "$ip" --mode capabilities
  docker exec "$name" test -s /workspace/smoke/render.png
  docker stop --time 10 "$name" >/dev/null
  test "$(docker inspect --format '{{.State.ExitCode}}' "$name")" = 0
  cleanup_blender
  trap - EXIT
}

if [ "$push" -eq 0 ]; then
  local_tag="${BASE}:dev"
  echo "==> Building local Linux/amd64 image ${local_tag}"
  docker build --platform linux/amd64 "${index_build_args[@]}" \
    --build-arg "SOURCE_REVISION=${revision}" \
    --build-arg "SOURCE_REPOSITORY=${SOURCE_REPOSITORY}" -t "$local_tag" .
  echo "==> Smoke test"
  smoke "$local_tag" linux/amd64
  echo "==> OK — local build + smoke passed. Publishing is opt-in: $0 --push"
  exit 0
fi

if [ -n "$(git status --porcelain)" ]; then
  echo "refusing to publish a dirty worktree under a commit-derived tag" >&2
  exit 1
fi

echo "==> Tests (test-before-publish)"
python3 -m compileall -q addon cad scripts/verify_release_image.py scripts/release_security.py
bash -n build-docker.sh scripts/run-headless-blender scripts/smoke-blender-gpu \
  scripts/smoke-release-pair
PYTHONPATH=addon python3 -m unittest discover -s addon/tests -v
PYTHONPATH=scripts python3 -m unittest discover -s scripts/tests -v
python3 scripts/docgate
sh scripts/check-release-target
cargo fmt --all -- --check
cargo "${cargo_index_args[@]}" clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo "${cargo_index_args[@]}" test --workspace --all-features --locked

server_commit_tag="${BASE}:sha-${short_sha}"
blender_commit_tag="${BLENDER_BASE}:sha-${short_sha}"
cad_commit_tag="${CAD_BASE}:sha-${short_sha}"
echo "==> Build + push ${server_commit_tag} (linux/amd64)"
docker buildx build --platform linux/amd64 --provenance=false --push \
  "${index_build_args[@]}" \
  --build-arg "SOURCE_REVISION=${revision}" \
  --build-arg "SOURCE_REPOSITORY=${SOURCE_REPOSITORY}" -t "$server_commit_tag" .
server_digest="$(scripts/registry-manifest-digest "$server_commit_tag")"
case "$server_digest" in sha256:*) ;; *) echo "invalid server digest" >&2; exit 1 ;; esac
verified_server="${server_commit_tag}@${server_digest}"

echo "==> Build + push ${blender_commit_tag} (linux/amd64)"
docker buildx build --platform linux/amd64 --provenance=false --push \
  --build-arg "SOURCE_REVISION=${revision}" \
  --build-arg "SOURCE_REPOSITORY=${SOURCE_REPOSITORY}" \
  --file blender/Dockerfile -t "$blender_commit_tag" .
blender_digest="$(scripts/registry-manifest-digest "$blender_commit_tag")"
case "$blender_digest" in sha256:*) ;; *) echo "invalid Blender digest" >&2; exit 1 ;; esac
verified_blender="${blender_commit_tag}@${blender_digest}"

echo "==> Build + push ${cad_commit_tag} (linux/amd64)"
docker buildx build --platform linux/amd64 --provenance=false --push \
  "${index_build_args[@]}" \
  --build-arg "SOURCE_REVISION=${revision}" \
  --build-arg "SOURCE_REPOSITORY=${SOURCE_REPOSITORY}" \
  --target cad-runtime -t "$cad_commit_tag" .
cad_digest="$(scripts/registry-manifest-digest "$cad_commit_tag")"
case "$cad_digest" in sha256:*) ;; *) echo "invalid CAD digest" >&2; exit 1 ;; esac
verified_cad="${cad_commit_tag}@${cad_digest}"

echo "==> Smoke the pushed amd64 images"
docker pull --platform linux/amd64 "$verified_server"
docker pull --platform linux/amd64 "$verified_blender"
docker pull --platform linux/amd64 "$verified_cad"
python3 scripts/verify_release_image.py server "$verified_server" "$revision"
python3 scripts/verify_release_image.py blender "$verified_blender" "$revision"
python3 scripts/verify_release_image.py cad "$verified_cad" "$revision"
python3 scripts/release_security.py "$verified_server" "$verified_blender" "$verified_cad"
cad_smoke_tag="${CAD_BASE}:smoke-${short_sha}"
docker buildx build --platform linux/amd64 --load \
  --build-arg "CAD_IMAGE=${verified_cad}" \
  --file cad/Dockerfile.smoke --tag "$cad_smoke_tag" .
docker run --rm --network none --read-only --cap-drop ALL \
  --security-opt no-new-privileges --pids-limit 256 --memory 4g --cpus 2 \
  --tmpfs /tmp:rw,size=2g,uid=10001,gid=10001,mode=1770 \
  --entrypoint /opt/cad/bin/python "$cad_smoke_tag" \
  /opt/printable/cad/smoke.py --worker
smoke "$verified_server" linux/amd64 8000
smoke_blender "$verified_blender"
smoke_blender "$verified_blender" ui

echo "==> Smoke the pushed pair through a durable render"
PRINTABLE_PAIR_SMOKE_RUN_ID="local-${short_sha}-$$" \
  scripts/smoke-release-pair \
    "$verified_server" "$verified_blender" \
    target/release/printable-smoke smoke/expected-tools.txt

echo "==> GPU-smoke the exact published Blender digest"
PRINTABLE_GPU_SMOKE_RUN_ID="local-${short_sha}" \
  scripts/smoke-blender-gpu "$verified_blender"

pair_commit_tag="${RELEASE_BASE}:sha-${short_sha}"
echo "==> Publish tested pair record ${pair_commit_tag}"
docker buildx build --platform linux/amd64 --provenance=false --push \
  --file release/Dockerfile \
  --build-arg "SOURCE_REVISION=${revision}" \
  --build-arg "SERVER_IMAGE=${server_commit_tag}" \
  --build-arg "SERVER_DIGEST=${server_digest}" \
  --build-arg "BLENDER_IMAGE=${blender_commit_tag}" \
  --build-arg "BLENDER_DIGEST=${blender_digest}" \
  --build-arg "CAD_IMAGE=${cad_commit_tag}" \
  --build-arg "CAD_DIGEST=${cad_digest}" \
  --tag "$pair_commit_tag" release
pair_digest="$(scripts/registry-manifest-digest "$pair_commit_tag")"
case "$pair_digest" in sha256:*) ;; *) echo "invalid release-pair digest" >&2; exit 1 ;; esac
pair_ref="${pair_commit_tag}@${pair_digest}"
docker pull --platform linux/amd64 "$pair_ref"
test "$(docker image inspect --format '{{ index .Config.Labels "org.opencontainers.image.revision" }}' "$pair_ref")" = "$revision"
test "$(docker image inspect --format '{{ index .Config.Labels "org.printable.server.digest" }}' "$pair_ref")" = "$server_digest"
test "$(docker image inspect --format '{{ index .Config.Labels "org.printable.blender.digest" }}' "$pair_ref")" = "$blender_digest"
test "$(docker image inspect --format '{{ index .Config.Labels "org.printable.cad.digest" }}' "$pair_ref")" = "$cad_digest"
echo "==> Published ${server_commit_tag}, ${blender_commit_tag}, ${cad_commit_tag}, and ${pair_ref}"
