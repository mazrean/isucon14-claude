#!/usr/bin/env bash
# Idempotent, non-interactive deploy.
#   NGINX node: nginx (+ upstream pointing at APP_UPSTREAM), sysctl
#   APP   node: binary, sql, env.sh (ISUCON_DB_HOST=DB_PRIVATE_IP), systemd units, sysctl
#   DB    node: mysql config, sysctl
# Defaults = main env (nginx isucon14-1 / app isucon14-2 / MySQL isucon14-3).
# Example (exp env, nginx node1 / app node2 / db node3):
#   NGINX_HOST=isucon14-exp-1 APP_HOST=isucon14-exp-2 DB_HOST=isucon14-exp-3 \
#   APP_UPSTREAM=10.100.0.47:8080 DB_PRIVATE_IP=10.100.0.20 ./scripts/deploy.sh
set -euo pipefail
cd "$(dirname "$0")/.."

APP=${APP_HOST:-isucon14-2}
DB=${DB_HOST:-isucon14-3}
NGINX=${NGINX_HOST:-isucon14-1}
# 追加の TLS 終端 nginx (NGINX ノードがカーネルの DNAT で新規接続を振り分ける)
EXTRA_NGINX=${EXTRA_NGINX_HOSTS:-isucon14-3 isucon14-2}
# "ip:重み"。本ノード (NGINX) の重みは LB_SELF_WEIGHT。app ノード (Rust 版) は 0.3 が最良
# (0.3: 2.02M/2.04M, 0.5: 1.99M/1.98M, 0.8: 1.96M/1.84M。Go 版のときは TLS を持たせると悪化した)
LB_TARGETS=${LB_TARGETS:-10.100.0.19:1 10.100.0.176:0.3}
LB_SELF_WEIGHT=${LB_SELF_WEIGHT:-0.75}
# app と同居する nginx は unix domain socket で app に繋ぐ
APP_SOCKET=${APP_SOCKET:-/run/isuride/app.sock}
# address MySQL is reachable at from the app node
DB_PRIVATE_IP=${DB_PRIVATE_IP:-10.100.0.19}
# nginx upstream "server" value (host:port or unix:/path)
APP_UPSTREAM=${APP_UPSTREAM:-10.100.0.176:8080}
BIN=${BIN:-webapp/rust/target/x86_64-unknown-linux-gnu/release/isuride}
# MATCHER=1 keeps the curl-based isuride-matcher running (only needed by the original app)
MATCHER=${MATCHER:-0}
SSH_OPTS="-o BatchMode=yes"

rs() { # rs <src> <host:dst> [extra rsync args...]
  local src=$1 dst=$2; shift 2
  rsync -az --rsync-path="sudo rsync" -e "ssh $SSH_OPTS" "$@" "$src" "$dst"
}
remote() { local h=$1; shift; ssh $SSH_OPTS "$h" "$@"; }

WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
sed -E "s|^ISUCON_DB_HOST=.*|ISUCON_DB_HOST=\"$DB_PRIVATE_IP\"|" env.sh >"$WORK/env.sh"
sed -E "s|^(\s*server )127\.0\.0\.1:8080;|\1$APP_UPSTREAM;|" etc/nginx/sites-available/isuride.conf >"$WORK/isuride.conf"
sed -E "s|^(\s*server )127\.0\.0\.1:8080;|\1unix:$APP_SOCKET;|" etc/nginx/sites-available/isuride.conf >"$WORK/isuride-uds.conf"

echo "==> [$DB] mysql config + sysctl"
rs etc/mysql/mysql.conf.d/zz-isucon.cnf "$DB:/etc/mysql/mysql.conf.d/zz-isucon.cnf" --chown=root:root --chmod=F644 -i | tee "$WORK/dbsync" >/dev/null || true
rs etc/sysctl.d/99-isucon.conf "$DB:/etc/sysctl.d/99-isucon.conf" --chown=root:root --chmod=F644
DB_CHANGED=$(grep -c '^<f' "$WORK/dbsync" || true)
remote "$DB" "sudo sysctl -q --system >/dev/null; sudo systemctl enable mysql >/dev/null 2>&1; \
  if [ '$DB_CHANGED' != '0' ] || ! systemctl is-active -q mysql; then echo 'restarting mysql'; sudo systemctl restart mysql; fi; \
  sudo mysql -e \"CREATE DATABASE IF NOT EXISTS isuride DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci; \
    CREATE USER IF NOT EXISTS 'isucon'@'%' IDENTIFIED BY 'isucon'; GRANT ALL ON isuride.* TO 'isucon'@'%';\""

echo "==> [$APP] binary, sql, env.sh (DB=$DB_PRIVATE_IP), systemd, sysctl"
rs "$BIN" "$APP:/home/isucon/webapp/go/isuride" --chown=isucon:isucon --chmod=F755
rs webapp/sql/ "$APP:/home/isucon/webapp/sql/" --chown=isucon:isucon
rs "$WORK/env.sh" "$APP:/home/isucon/env.sh" --chown=isucon:isucon --chmod=F755
rs etc/systemd/system/ "$APP:/etc/systemd/system/" --chown=root:root --include='isuride-*.service' --exclude='*'
rs etc/sysctl.d/99-isucon.conf "$APP:/etc/sysctl.d/99-isucon.conf" --chown=root:root --chmod=F644

for h in $NGINX $EXTRA_NGINX; do
  echo "==> [$h] nginx, sysctl (upstream $APP_UPSTREAM)"
  rs etc/sysctl.d/99-isucon.conf "$h:/etc/sysctl.d/99-isucon.conf" --chown=root:root --chmod=F644
  if [ "$h" = "$APP" ]; then
    # app と同居する nginx は 1 ワーカーに絞り、app に CPU を残す
    sed -E "s|^worker_processes .*;|worker_processes 1;|" etc/nginx/nginx.conf >"$WORK/nginx-app.conf"
    rs "$WORK/nginx-app.conf" "$h:/etc/nginx/nginx.conf" --chown=root:root --chmod=F644
  else
    rs etc/nginx/nginx.conf "$h:/etc/nginx/nginx.conf" --chown=root:root
  fi
  conf="$WORK/isuride.conf"
  [ "$h" = "$APP" ] && conf="$WORK/isuride-uds.conf"
  rs "$conf" "$h:/etc/nginx/sites-available/isuride.conf" --chown=root:root --chmod=F644
done
# AWS-LC 版 nginx (scripts/build-nginx-awslc.sh でビルド済みなら配る。切り替えは再起動時)
for h in $NGINX $EXTRA_NGINX; do
  if [ -x build/nginx-awslc ]; then
    rs build/nginx-awslc "$h:/usr/local/sbin/nginx-awslc" --chown=root:root --chmod=F755
    remote "$h" "sudo mkdir -p /etc/systemd/system/nginx.service.d"
    rs etc/systemd/system/nginx.service.d/awslc.conf "$h:/etc/systemd/system/nginx.service.d/awslc.conf" --chown=root:root --chmod=F644
  fi
done
echo "==> [$NGINX] TLS load balancing -> [$LB_TARGETS]"
printf 'LB_TARGETS="%s"\nLB_SELF_WEIGHT=%s\n' "$LB_TARGETS" "$LB_SELF_WEIGHT" >"$WORK/tls-lb.env"
remote "$NGINX" "sudo mkdir -p /etc/isucon"
rs "$WORK/tls-lb.env" "$NGINX:/etc/isucon/tls-lb.env" --chown=root:root --chmod=F644
rs etc/isucon/tls-lb.sh "$NGINX:/usr/local/bin/isucon-tls-lb.sh" --chown=root:root --chmod=F755
rs etc/systemd/system/isucon-tls-lb.service "$NGINX:/etc/systemd/system/isucon-tls-lb.service" --chown=root:root --chmod=F644
remote "$NGINX" "sudo systemctl daemon-reload && sudo systemctl enable -q isucon-tls-lb && sudo systemctl restart isucon-tls-lb && sudo iptables -t nat -S ISUCON_LB | tail -n +2"

for h in $(printf '%s\n' "$APP" "$NGINX" "$DB" $EXTRA_NGINX | sort -u); do
  echo "==> [$h] NIC IRQ affinity / XPS / qdisc"
  rs etc/isucon/net-tune.sh "$h:/usr/local/bin/isucon-net-tune.sh" --chown=root:root --chmod=F755
  rs etc/systemd/system/isucon-net-tune.service "$h:/etc/systemd/system/isucon-net-tune.service" --chown=root:root --chmod=F644
  remote "$h" "sudo systemctl daemon-reload && sudo systemctl disable -q irqbalance 2>/dev/null; sudo systemctl enable -q isucon-net-tune && sudo systemctl restart isucon-net-tune"
done

# every node: stop services it does not own
for h in $(printf '%s\n' "$APP" "$NGINX" "$DB" $EXTRA_NGINX | sort -u); do
  ROLES=""
  [ "$h" = "$APP" ] && ROLES="$ROLES app"
  [ "$h" = "$NGINX" ] && ROLES="$ROLES nginx"
  case " $EXTRA_NGINX " in *" $h "*) ROLES="$ROLES nginx";; esac
  [ "$h" = "$DB" ] && ROLES="$ROLES db"
  remote "$h" "ROLES='$ROLES' MATCHER=$MATCHER bash -s" <<'REMOTE'
set -e
cd /
has() { case " $ROLES " in *" $1 "*) return 0;; esac; return 1; }
sudo sysctl -q --system >/dev/null
DISABLE="isuride-payment_mock"
has app || DISABLE="$DISABLE isuride-go isuride-matcher"
has app && [ "$MATCHER" != 1 ] && DISABLE="$DISABLE isuride-matcher"
has nginx || DISABLE="$DISABLE nginx"
has db || DISABLE="$DISABLE mysql"
for s in $DISABLE; do
  if systemctl is-enabled -q $s 2>/dev/null || systemctl is-active -q $s; then sudo systemctl disable --now -q $s; fi
done
if has app; then
  sudo systemctl daemon-reload
  if [ "$MATCHER" = 1 ]; then sudo systemctl enable -q isuride-matcher; sudo systemctl restart isuride-matcher; fi
  sudo systemctl enable -q isuride-go
  sudo systemctl restart isuride-go
  for i in $(seq 1 60); do
    curl -s -o /dev/null -m 2 http://127.0.0.1:8080/api/app/nearby-chairs && break; sleep 0.5
  done
fi
if has nginx; then
  # precompress static assets for gzip_static (idempotent)
  sudo -u isucon find /home/isucon/webapp/public -type f \( -name "*.js" -o -name "*.css" -o -name "*.html" -o -name "*.svg" -o -name "*.json" \) -exec gzip -kf9 {} +
  sudo systemctl enable -q nginx
  sudo nginx -t -q && sudo systemctl reload-or-restart nginx
fi
echo "  $(hostname):$ROLES -> $(systemctl is-active isuride-go nginx mysql | tr '\n' ' ')"
REMOTE
done
echo "==> deploy done"
