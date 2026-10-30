#!/usr/bin/env bash
# NIC 割り込みとキューの調整 (c5.large / ENA 2 キュー前提)。
# - irqbalance が 2 本の割り込みを同じ CPU に寄せることがある (3 号機で両方 CPU0 だった) ので、
#   止めて ens5-Tx-Rx-N を CPU N に固定する (softirq を 2 コアに分散)
# - XPS で送信キューも CPU と 1 対 1 に揃える
# - qdisc を fq_codel から軽い pfifo_fast に替える (サーバー用途で AQM は不要)
set -euo pipefail
systemctl stop irqbalance 2>/dev/null || true
IF=$(ip -o route get 10.100.0.1 | grep -o "dev [^ ]*" | cut -d" " -f2)
ncpu=$(nproc)
grep "$IF-Tx-Rx-" /proc/interrupts | while read -r line; do
  irq=${line%%:*}; irq=${irq// /}
  q=${line##*-Tx-Rx-}
  echo $((q % ncpu)) > "/proc/irq/$irq/smp_affinity_list"
done
# XPS: CPU N から送るパケットは送信キュー N に (キューのロック競合を避ける)
for q in /sys/class/net/"$IF"/queues/tx-*; do
  n=${q##*tx-}
  printf '%x' $((1 << (n % ncpu))) > "$q/xps_cpus"
done
sysctl -q -w net.core.default_qdisc=pfifo_fast
tc qdisc replace dev "$IF" root mq 2>/dev/null || true
