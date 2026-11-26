#!/bin/sh
# Runs inside the benchmark container: stage.sh <stage> [...]
# Stages: scenes inputs gpu budget cpu timing report
set -u
cd /bench/scripts

GPU_MODELS_MAIN="moge3-vitg moge3-vitl moge2-vitl-normal moge2-vitb-normal moge2-vits-normal dp2-h dp2-l dp2-b dp2-s"
GPU_MODELS_DA3="da3-nested-giant da3-mono-large da3-base da3-small"
# CPU runs use a subset of inputs; each model gets a time limit per image.
CPU_IMAGES="gt-relief,gt-coin,gt-head,01-portrait-elderly,07-coin-silver"
CPU_IMAGES_SLOW="gt-relief,07-coin-silver"
CPU_LIMIT_PER_IMAGE=${BENCH_CPU_LIMIT_PER_IMAGE:-1800}
MARIGOLD=/models/marigold-v2

log() { echo "[$(date -u +%H:%M:%S)] $*"; }
count() { echo "$1" | tr ',' '\n' | grep -c .; }

run_tier() {
  tier=$1; images=${2:-}; limit=${3:-0}
  for m in $GPU_MODELS_MAIN; do
    log "$tier $m"
    if [ "$limit" -gt 0 ]; then timeout "$limit" /opt/main/bin/python run_main.py "$m" --tier "$tier" --images "$images"
    else /opt/main/bin/python run_main.py "$m" --tier "$tier" --images "$images"; fi || log "$tier $m ended with status $?"
  done
  for m in $GPU_MODELS_DA3; do
    log "$tier $m"
    if [ "$limit" -gt 0 ]; then timeout "$limit" /opt/da3/bin/python run_da3.py "$m" --tier "$tier" --images "$images"
    else /opt/da3/bin/python run_da3.py "$m" --tier "$tier" --images "$images"; fi || log "$tier $m ended with status $?"
  done
}

run_marigold() {
  tier=$1; images=${2:-}; limit=${3:-0}
  if [ ! -d "$MARIGOLD" ]; then log "marigold-v2 skipped: no converted model at $MARIGOLD"; return; fi
  log "$tier marigold-v2"
  if [ "$limit" -gt 0 ]; then timeout "$limit" /opt/marigold/bin/python run_marigold.py "$MARIGOLD" --tier "$tier" --images "$images"
  else /opt/marigold/bin/python run_marigold.py "$MARIGOLD" --tier "$tier" --images "$images"; fi || log "$tier marigold-v2 ended with status $?"
}

for stage in "$@"; do
  case $stage in
    scenes)
      log "rendering scenes"
      /opt/blender/blender -b --factory-startup -P scenes.py -- /bench/work/gt 2>&1 | grep -E 'rendered|Error|Traceback' ;;
    inputs)
      /opt/main/bin/python -c "import common; print(len(common.prepare_inputs()), 'inputs')" ;;
    gpu)
      run_tier gpu; run_marigold gpu ;;
    budget)
      run_tier budget; run_marigold budget ;;
    cpu)
      run_tier cpu "$CPU_IMAGES" $(( CPU_LIMIT_PER_IMAGE * ($(count "$CPU_IMAGES") + 1) ))
      run_marigold cpu "$CPU_IMAGES_SLOW" $(( CPU_LIMIT_PER_IMAGE * ($(count "$CPU_IMAGES_SLOW") + 1) )) ;;
    timing)
      for m in $GPU_MODELS_MAIN; do /opt/main/bin/python run_main.py "$m" --tier gpu --timing || true; done
      for m in $GPU_MODELS_DA3; do /opt/da3/bin/python run_da3.py "$m" --tier gpu --timing || true; done
      [ -d "$MARIGOLD" ] && /opt/marigold/bin/python run_marigold.py "$MARIGOLD" --tier gpu --timing || true ;;
    report)
      log "report"; /opt/main/bin/python report.py ;;
    *)
      log "unknown stage $stage"; exit 2 ;;
  esac
done
log "done: $*"
