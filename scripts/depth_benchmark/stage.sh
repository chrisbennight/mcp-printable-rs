#!/bin/sh
# Runs inside the benchmark container: stage.sh <stage> [...]
# Stages: scenes inputs gpu budget cpu timing-gpu timing-budget timing-cpu report
#
# A model that fails or exceeds its time limit is recorded as a failure and the
# stage continues with the next model; any other failure ends the stage with a
# non-zero status.
set -u
cd /bench/scripts

MODELS_MAIN="moge3-vitg moge3-vitl moge2-vitl-normal moge2-vitb-normal moge2-vits-normal dp2-h dp2-l dp2-b dp2-s"
MODELS_DA3="da3-nested-giant da3-mono-large da3-base da3-small"
# The cpu tier scores the same three scenes for every model; faster models also run two photographs.
CPU_IMAGES="gt-relief,gt-coin,gt-head,01-portrait-elderly,07-coin-silver"
CPU_IMAGES_MARIGOLD="gt-relief,gt-coin,gt-head"
CPU_TIMING_IMAGES="02-dog-fur"
LIMIT_PER_IMAGE=${BENCH_CPU_LIMIT_PER_IMAGE:-1800}
MARIGOLD=/models/marigold-v2
RESULTS=/bench/results

log() { echo "[$(date -u +%H:%M:%S)] $*"; }
count() { echo "$1" | tr ',' '\n' | grep -c .; }

# run_model <tier> <results-dir> <images> <time-limit-seconds> <name> <command...>
run_model() {
  tier=$1 dir=$2 images=$3 limit=$4 name=$5
  shift 5
  log "$dir $name"
  if [ "$limit" -gt 0 ]; then timeout "$limit" "$@" --tier "$tier" --images "$images"
  else "$@" --tier "$tier" --images "$images"; fi
  status=$?
  if [ "$status" -ne 0 ]; then
    log "$dir $name ended with status $status"
    mkdir -p "$RESULTS/$dir"
    printf '{"tier": "%s", "model": "%s", "image": "run", "error": "%s", "message": "runner exit status %s"}\n' \
      "$tier" "$name" "$([ "$status" -eq 124 ] && echo TimeLimit || echo ExitStatus)" "$status" \
      >> "$RESULTS/$dir/failures.jsonl"
  fi
}

# run_all <tier> <timing: 0|1> <images> <marigold-images> <time-limit-per-image>
run_all() {
  tier=$1 timing=$2 images=$3 marigold_images=$4 per_image=$5
  if [ "$timing" -eq 1 ]; then dir=timing-$tier flag=--timing; else dir=$tier flag=; fi
  rm -rf "${RESULTS:?}/$dir"
  limit=0 marigold_limit=0
  if [ "$per_image" -gt 0 ]; then
    limit=$(( per_image * ($(count "$images") + 1) ))
    marigold_limit=$(( per_image * ($(count "$marigold_images") + 1) ))
  fi
  for m in $MODELS_MAIN; do
    run_model "$tier" "$dir" "$images" "$limit" "$m" /opt/main/bin/python run_main.py "$m" $flag
  done
  for m in $MODELS_DA3; do
    run_model "$tier" "$dir" "$images" "$limit" "$m" /opt/da3/bin/python run_da3.py "$m" $flag
  done
  if [ -d "$MARIGOLD" ]; then
    run_model "$tier" "$dir" "$marigold_images" "$marigold_limit" marigold-v2 \
      /opt/marigold/bin/python run_marigold.py "$MARIGOLD" $flag
  else
    log "marigold-v2 skipped: no converted model at $MARIGOLD"
  fi
}

for stage in "$@"; do
  case $stage in
    scenes)
      log "rendering scenes"
      mkdir -p /bench/work/gt
      /opt/blender/blender -b --factory-startup --python-exit-code 1 -P scenes.py -- /bench/work/gt \
        > /bench/work/scenes.log 2>&1 || { tail -n 40 /bench/work/scenes.log; log "scenes failed"; exit 1; }
      grep -E 'rendered|device' /bench/work/scenes.log ;;
    inputs)
      /opt/main/bin/python -c "import common; print(len(common.prepare_inputs()), 'inputs')" || exit 1 ;;
    gpu|budget)
      run_all "$stage" 0 "" "" 0 ;;
    cpu)
      run_all cpu 0 "$CPU_IMAGES" "$CPU_IMAGES_MARIGOLD" "$LIMIT_PER_IMAGE" ;;
    timing-gpu|timing-budget)
      run_all "${stage#timing-}" 1 "" "" 0 ;;
    timing-cpu)
      run_all cpu 1 "$CPU_TIMING_IMAGES" "$CPU_TIMING_IMAGES" "$(( LIMIT_PER_IMAGE * 4 ))" ;;
    report)
      log "report"; /opt/main/bin/python report.py || exit 1 ;;
    *)
      log "unknown stage $stage"; exit 2 ;;
  esac
done
log "done: $*"
