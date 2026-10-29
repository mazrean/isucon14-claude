#!/usr/bin/env bash
# Defaults = main env. Exp env:
#   BENCH_HOST=isucon14-exp-bench TARGET_IP=10.100.0.71 BENCH_IP=10.100.0.121 ./scripts/bench.sh
set -euo pipefail
cd "$(dirname "$0")/.."
BENCH_HOST=${BENCH_HOST:-isucon14-bench}
TARGET_IP=${TARGET_IP:-10.100.0.218}
BENCH_IP=${BENCH_IP:-10.100.0.67}
mkdir -p logs
LOG=logs/bench-$(date +%Y%m%d-%H%M%S)${LOG_SUFFIX:+-$LOG_SUFFIX}.log
ssh -o BatchMode=yes "$BENCH_HOST" "cd /home/isucon && sudo -u isucon ./bench run --addr $TARGET_IP:443 --target https://isuride.xiv.isucon.net --payment-url http://$BENCH_IP:12346 --payment-bind-port 12346" >"$LOG" 2>&1 || true
tail -n 15 "$LOG"
echo "full log: $LOG"
