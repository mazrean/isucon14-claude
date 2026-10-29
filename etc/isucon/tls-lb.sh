#!/usr/bin/env bash
# ベンチからの :443 の新規接続を、カーネル (netfilter) で複数の nginx に振り分ける。
# TLS 終端の CPU を複数ノードに分散するため。戻りは MASQUERADE で本ノード経由にする。
#   LB_TARGETS: 転送先の private IP (空白区切り)。本ノード自身の nginx も 1 台分として扱う。
set -euo pipefail
LB_TARGETS=${LB_TARGETS:-}
sysctl -q -w net.ipv4.ip_forward=1
iptables -t nat -N ISUCON_LB 2>/dev/null || iptables -t nat -F ISUCON_LB
iptables -t nat -C PREROUTING -p tcp --dport 443 -j ISUCON_LB 2>/dev/null || iptables -t nat -A PREROUTING -p tcp --dport 443 -j ISUCON_LB
iptables -t nat -N ISUCON_LB_POST 2>/dev/null || iptables -t nat -F ISUCON_LB_POST
iptables -t nat -C POSTROUTING -j ISUCON_LB_POST 2>/dev/null || iptables -t nat -A POSTROUTING -j ISUCON_LB_POST
n=$(( $(echo $LB_TARGETS | wc -w) + 1 ))
i=0
for t in $LB_TARGETS; do
  # 残りの接続のうち 1/(n-i) を t に送る (nth は先頭から順に評価されるので均等になる)
  iptables -t nat -A ISUCON_LB -p tcp --dport 443 -m conntrack --ctstate NEW -m statistic --mode nth --every $((n - i)) --packet 0 -j DNAT --to-destination "$t:443"
  iptables -t nat -A ISUCON_LB_POST -p tcp -d "$t" --dport 443 -j MASQUERADE
  i=$((i + 1))
done
