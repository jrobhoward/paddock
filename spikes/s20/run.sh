#!/usr/bin/env bash
# Runs the design configs exhaustively, one after another, with checkpoints,
# into ~/tla-runs/s20-<date>. Usage: run.sh [config ...] (default Design1).
set -euo pipefail
cd "$(dirname "$0")"
out=${S20_RUNS:-$HOME/tla-runs/s20-2026-10-17}
mkdir -p "$out"
for name in "${@:-Design1}"; do
    java -XX:+UseParallelGC -Xmx${S20_HEAP:-40g} -jar "$HOME/tla/tla2tools.jar" \
        -workers "${S20_WORKERS:-12}" -deadlock -noGenerateSpecTE -checkpoint 30 \
        -metadir "$out/$name" -config "$name.cfg" PairReplication.tla > "$out/$name.log" 2>&1 || true
    echo "$name: $(grep -E 'is violated|No error has been found' "$out/$name.log" | head -1)" >> "$out/summary.txt"
done
