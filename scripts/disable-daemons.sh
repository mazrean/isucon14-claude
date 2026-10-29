#!/usr/bin/env bash
# One-time: stop CPU-eating daemons not needed during the contest.
# Usage: scripts/disable-daemons.sh isucon14-1 [isucon14-2 ...]
# NOTE: disabling snapd also stops amazon-ssm-agent (snap); ssh is unaffected.
set -euo pipefail
for h in "${@:-isucon14-1}"; do
  echo "==> $h"
  ssh -o BatchMode=yes "$h" 'bash -s' <<'REMOTE'
set -u
for u in snap.amazon-ssm-agent.amazon-ssm-agent.service snapd.service snapd.socket snapd.seeded.service \
         unattended-upgrades.service ModemManager.service multipathd.service multipathd.socket udisks2.service fwupd.service packagekit.service; do
  sudo systemctl disable --now "$u" >/dev/null 2>&1 || true
done
for t in apt-daily.timer apt-daily-upgrade.timer motd-news.timer man-db.timer fwupd-refresh.timer; do
  sudo systemctl mask --now "$t" >/dev/null 2>&1 || true
done
systemctl list-units --type=service --state=running --no-pager --no-legend | awk '{print "  "$1}'
REMOTE
done
