#!/bin/sh
# Depth benchmark: scripts/depth_benchmark/run.sh <data-dir> [stage ...]
#
# Stages (default: all, in this order):
#   fetch   download the photographs in photos.json into <data-dir>/images
#   build   build the benchmark image from the repository root
#   scenes  render the scenes with exact depth and normals
#   inputs  write every input at the shared size
#   gpu budget cpu   run every model on that execution tier
#   timing  measure each model at 2048 pixels on the GPU
#   report  score everything and write tables, metrics and sheets
#
# Settings (environment):
#   BENCH_MODEL_CACHE   model download cache (default <data-dir>/models)
#   BENCH_MARIGOLD_DIR  converted Marigold V2 model (see docs/photo-depth.md); Marigold is skipped without it
#   BENCH_GPU_BUDGET_GB GPU memory limit for the budget tier (default 3)
#   BENCH_CPU_THREADS   CPU threads for the cpu tier (default all)
#   BENCH_GPU_LOCK      optional file to hold with flock while the GPU is in use
set -eu
[ $# -ge 1 ] || { sed -n '2,20p' "$0"; exit 2; }
data=$(cd "$1" 2>/dev/null && pwd || { mkdir -p "$1" && cd "$1" && pwd; })
shift
stages=${*:-"fetch build scenes inputs gpu budget cpu timing report"}
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
image=printable-depth-benchmark:local
cache=${BENCH_MODEL_CACHE:-$data/models}
mkdir -p "$data/images" "$data/work" "$data/results" "$cache"

container() {
  # The cpu stage gets no GPU at all, so it measures a host without one.
  if [ "$1" = cpu ]; then gpus="--env=CUDA_VISIBLE_DEVICES="; else gpus="--gpus=all"; fi
  set -- docker run --rm "$gpus" --shm-size 8g --user "$(id -u):$(id -g)" \
    -e HOME=/tmp -e HF_HOME=/cache -e TORCH_HOME=/cache/torch -e TRITON_CACHE_DIR=/tmp/triton \
    -e MPLCONFIGDIR=/tmp/matplotlib -e PYTHONUNBUFFERED=1 \
    -e BENCH_GPU_BUDGET_GB="${BENCH_GPU_BUDGET_GB:-3}" -e BENCH_CPU_THREADS="${BENCH_CPU_THREADS:-0}" \
    -v "$data/images:/bench/images:ro" -v "$data/work:/bench/work" -v "$data/results:/bench/results" \
    -v "$cache:/cache" ${BENCH_MARIGOLD_DIR:+-v "$BENCH_MARIGOLD_DIR:/models/marigold-v2:ro"} \
    "$image" /bench/scripts/stage.sh "$@"
  if [ -n "${BENCH_GPU_LOCK:-}" ]; then flock "$BENCH_GPU_LOCK" "$@"; else "$@"; fi
}

for stage in $stages; do
  case $stage in
    fetch) python3 -I "$here/fetch_photos.py" "$data/images" ;;
    build) docker build -f "$here/Dockerfile" -t "$image" "$repo" ;;
    scenes|inputs|gpu|budget|cpu|timing|report) container "$stage" ;;
    *) echo "unknown stage: $stage" >&2; exit 2 ;;
  esac
done
