#!/usr/bin/env bash
set -euo pipefail

rust=false
addon=false
tooling=false
docs=false
workflows=false
fuzz=false
images=false
shell=false
case "${GITHUB_EVENT_NAME:?event is required}" in
  workflow_dispatch) full=true ;;
  push|pull_request) full=false ;;
  *) echo 'Unsupported CI event' >&2; exit 1 ;;
esac
if [[ "$full" == true ]]; then
  rust=true; addon=true; tooling=true; docs=true
  workflows=true; fuzz=true; images=true; shell=true
else
  [[ "${BASE_SHA:-}" =~ ^[0-9a-f]{40}$ ]] || { echo 'A full base commit is required' >&2; exit 1; }
  changed_files="$(mktemp)"
  trap 'rm -f "$changed_files"' EXIT
  if [[ "$GITHUB_EVENT_NAME" == pull_request ]]; then
    git diff --name-only --no-renames -z "$BASE_SHA...HEAD" >"$changed_files"
  else
    git diff --name-only --no-renames -z "$BASE_SHA" HEAD >"$changed_files"
  fi
  while IFS= read -r -d '' path; do
    case "$path" in
      scripts/ci-scope.sh|.github/workflows/ci.yml)
        rust=true; addon=true; tooling=true; docs=true; workflows=true; fuzz=true; images=true; shell=true ;;
      Cargo.toml|Cargo.lock|rust-toolchain.toml)
        rust=true; fuzz=true; images=true ;;
      .cargo/*) rust=true; fuzz=true ;;
      crates/*/tests/*|crates/*/benches/*) rust=true ;;
      crates/printable-scad/fuzz/*) fuzz=true ;;
      crates/*/examples/*) rust=true ;;
      crates/*/Cargo.toml|crates/*/build.rs|crates/*/src/*|crates/*/assets/*|crates/*/resources/*)
        rust=true; images=true ;;
      addon/tests/*) addon=true ;;
      addon/*) addon=true; images=true ;;
      Dockerfile|.dockerignore|blender/*|cad/*|slicer/*|LICENSE|THIRD_PARTY_NOTICES.md|smoke/*|release/*)
        images=true ;;
      acceptance/products/*) rust=true; images=true ;;
    esac
    case "$path" in
      crates/printable-scad/src/gate.rs|crates/printable-scad/src/lib.rs|crates/printable-scad/Cargo.toml) fuzz=true ;;
      *.md|scripts/docgate) docs=true ;;
      .github/workflows/*) workflows=true; tooling=true ;;
      requirements-tooling.txt|scripts/*.py|scripts/tests/*) tooling=true ;;
      release/*) tooling=true ;;
    esac
    case "$path" in
      build-docker.sh|scripts/run-headless-blender|scripts/smoke-*|scripts/test-*|scripts/*_smoke.py|scripts/printable_client.py|scripts/installation_recovery.py|scripts/quickstart.py|scripts/gpu_process.py|scripts/openscad-headless|scripts/orca-headless|scripts/registry-manifest-digest|scripts/release_*.py|scripts/publish_images.py|scripts/verify_release_image.py)
        images=true ;;
    esac
    case "$path" in
      *.sh|scripts/run-headless-blender|scripts/smoke-blender-gpu|scripts/smoke-blender-cpu|scripts/smoke-release-pair|scripts/check-release-target)
        shell=true; tooling=true ;;
    esac
    if [[ ! -e "$path" ]]; then docs=true; fi
  done <"$changed_files"
fi
printf 'rust=%s\naddon=%s\ntooling=%s\ndocs=%s\nworkflows=%s\nfuzz=%s\nimages=%s\nshell=%s\n' \
  "$rust" "$addon" "$tooling" "$docs" "$workflows" "$fuzz" "$images" "$shell" >>"${GITHUB_OUTPUT:?output file is required}"
