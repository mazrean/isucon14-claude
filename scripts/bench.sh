#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p logs
LOG=logs/bench-$(date +%Y%m%d-%H%M%S).log
ssh -o BatchMode=yes isucon14-bench 'cd /home/isucon && sudo -u isucon ./bench run --addr 10.100.0.218:443 --target https://isuride.xiv.isucon.net --payment-url http://10.100.0.67:12346 --payment-bind-port 12346' >"$LOG" 2>&1 || true
tail -n 15 "$LOG"
echo "full log: $LOG"
