#!/usr/bin/env bash
# Idempotent, non-interactive deploy.
#   APP node (isucon14-1): binary, sql, env.sh, nginx, systemd units, sysctl
#   DB  node (isucon14-2): mysql config, sysctl
set -euo pipefail
cd "$(dirname "$0")/.."

APP=${APP_HOST:-isucon14-1}
DB=${DB_HOST:-isucon14-2}
BIN=${BIN:-webapp/go/isuride}
# MATCHER=1 keeps the curl-based isuride-matcher running (only needed by the original app)
MATCHER=${MATCHER:-0}
SSH_OPTS="-o BatchMode=yes"

rs() { # rs <src> <host:dst> [extra rsync args...]
  local src=$1 dst=$2; shift 2
  rsync -az --rsync-path="sudo rsync" -e "ssh $SSH_OPTS" "$@" "$src" "$dst"
}
remote() { local h=$1; shift; ssh $SSH_OPTS "$h" "$@"; }

TMPF=$(mktemp)
echo "==> [$DB] mysql config + sysctl"
rs etc/mysql/mysql.conf.d/zz-isucon.cnf "$DB:/etc/mysql/mysql.conf.d/zz-isucon.cnf" --chown=root:root --chmod=F644 -i | tee "$TMPF" >/dev/null || true
rs etc/sysctl.d/99-isucon.conf "$DB:/etc/sysctl.d/99-isucon.conf" --chown=root:root --chmod=F644
DB_CHANGED=$(grep -c '^<f' "$TMPF" || true); rm -f "$TMPF"
remote "$DB" "sudo sysctl -q --system >/dev/null; sudo systemctl enable mysql >/dev/null 2>&1; \
  if [ '$DB_CHANGED' != '0' ] || ! systemctl is-active -q mysql; then echo 'restarting mysql'; sudo systemctl restart mysql; fi; \
  sudo mysql -e \"CREATE DATABASE IF NOT EXISTS isuride DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci; \
    CREATE USER IF NOT EXISTS 'isucon'@'%' IDENTIFIED BY 'isucon'; GRANT ALL ON isuride.* TO 'isucon'@'%';\""

echo "==> [$APP] binary, sql, env.sh"
rs "$BIN" "$APP:/home/isucon/webapp/go/isuride" --chown=isucon:isucon --chmod=F755
rs webapp/sql/ "$APP:/home/isucon/webapp/sql/" --chown=isucon:isucon
rs env.sh "$APP:/home/isucon/env.sh" --chown=isucon:isucon --chmod=F755

echo "==> [$APP] nginx, systemd, sysctl"
rs etc/nginx/nginx.conf "$APP:/etc/nginx/nginx.conf" --chown=root:root
rs etc/nginx/sites-available/isuride.conf "$APP:/etc/nginx/sites-available/isuride.conf" --chown=root:root
rs etc/systemd/system/ "$APP:/etc/systemd/system/" --chown=root:root --include='isuride-*.service' --exclude='*'
rs etc/sysctl.d/99-isucon.conf "$APP:/etc/sysctl.d/99-isucon.conf" --chown=root:root --chmod=F644

remote "$APP" "MATCHER=$MATCHER bash -s" <<'REMOTE'
set -e
cd /
  sudo sysctl -q --system >/dev/null
  # precompress static assets for gzip_static (idempotent)
  sudo -u isucon find /home/isucon/webapp/public -type f \( -name "*.js" -o -name "*.css" -o -name "*.html" -o -name "*.svg" -o -name "*.json" \) -exec gzip -kf9 {} +
  sudo systemctl daemon-reload
  # matcher / payment_mock / local mysql are not used
  DISABLE="isuride-payment_mock mysql"
  if [ "$MATCHER" = 1 ]; then sudo systemctl enable -q isuride-matcher; sudo systemctl restart isuride-matcher; else DISABLE="isuride-matcher $DISABLE"; fi
  for s in $DISABLE; do
    if systemctl is-enabled -q $s 2>/dev/null || systemctl is-active -q $s; then sudo systemctl disable --now $s; fi
  done
  sudo systemctl enable -q isuride-go nginx
  sudo systemctl restart isuride-go
  sudo nginx -t -q && sudo systemctl reload-or-restart nginx
  for i in $(seq 1 30); do
    curl -s -o /dev/null -m 2 http://127.0.0.1:8080/api/app/nearby-chairs && break; sleep 0.5
  done
  systemctl is-active isuride-go nginx
REMOTE
echo "==> deploy done"
