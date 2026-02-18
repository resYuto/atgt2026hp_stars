#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
SKI_DIR="$ROOT_DIR/ski_eval_rs"
OUT_BASE="${1:-$ROOT_DIR/logs/baseline_io}"
RUNS="${RUNS:-3}"

mkdir -p "$OUT_BASE"
STAMP="$(date +%Y%m%d_%H%M%S)"
OUT_DIR="$OUT_BASE/$STAMP"
mkdir -p "$OUT_DIR"

CMD=(
  "$SKI_DIR/target/release/ski-eval"
  "$ROOT_DIR/very_large_txt/stars_compact.txt"
  --fuel 2000000000
  --decode io
  --key 5,0,17,5,3
  --img "$ROOT_DIR/images/zoom"
)

echo "baseline_dir=$OUT_DIR"
echo "runs=$RUNS"
echo "command=${CMD[*]}"

(
  cd "$SKI_DIR"
  cargo build --release
)

for i in $(seq 1 "$RUNS"); do
  log_file="$OUT_DIR/run${i}.log"
  echo "[run $i/$RUNS] $log_file"
  /usr/bin/time -f "ELAPSED_SECONDS=%e" "${CMD[@]}" >"$log_file" 2>&1
done

"$ROOT_DIR/scripts/summarize_baseline_io.py" "$OUT_DIR" >"$OUT_DIR/summary.tsv"
echo "summary=$OUT_DIR/summary.tsv"
