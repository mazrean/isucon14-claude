#!/usr/bin/env bash
# 実験用 (既定では使わない)。node1 で root 実行: xdp-lb-node1.sh on|off
# ビルド: clang -O2 -g -target bpf -I/usr/include/x86_64-linux-gnu -c xdp-lb.c -o /usr/local/lib/isucon/xdp-lb.o
# on 時は node2/node3 で `ip route replace 10.100.0.218/32 dev ens5 mtu lock 3498 advmss 3458` も必要
set -u
IF=ens5; OBJ=/usr/local/lib/isucon/xdp-lb.o; PIN=/sys/fs/bpf/xdplb
SELF=10.100.0.218; CLIENT=10.100.0.67
# 64 buckets: self 21, node3 29, node2 14  (≈ 0.75 : 1 : 0.5)
TARGETS=${TARGETS:-"10.100.0.19:29 10.100.0.176:14"}
iphex() { echo "$1" | awk -F. '{printf "%d %d %d %d", $1,$2,$3,$4}'; }
machex() { ip neigh show "$1" dev $IF | awk '{print $3}' | python3 -c "import sys;m=sys.stdin.read().split()[0];v=int(m.replace(':',''),16);print(' '.join(str(b) for b in v.to_bytes(8,'little')))"; }
if [ "$1" = on ]; then
  # remove netfilter LB (conntrack then has no users)
  nft delete table inet isucon_ft 2>/dev/null; nft delete table ip nat 2>/dev/null
  ethtool -L $IF combined 1 2>&1; sleep 2
  ip link set dev $IF mtu 3498
  for ip in 10.100.0.19 10.100.0.176 $CLIENT; do ping -c1 -W1 $ip >/dev/null; done
  for ip in 10.100.0.19 10.100.0.176 $CLIENT; do m=$(ip neigh show $ip dev $IF | awk '{print $3}'); ip neigh replace $ip lladdr $m dev $IF nud permanent; done
  rm -rf $PIN; mkdir -p $PIN
  bpftool prog load $OBJ $PIN/prog type xdp pinmaps $PIN || exit 1
  selfmac=$(cat /sys/class/net/$IF/address | python3 -c "import sys;print(' '.join(str(int(x,16)) for x in sys.stdin.read().strip().split(':')))")
  ifx=$(cat /sys/class/net/$IF/ifindex)
  bpftool map update pinned $PIN/cfg_map key 0 0 0 0 value $(iphex $SELF) $(iphex $CLIENT) $ifx 0 0 0 $selfmac 0 0
  b=0
  for t in $TARGETS; do ip=${t%%:*}; n=${t#*:}; for i in $(seq 1 $n); do
    bpftool map update pinned $PIN/bucket_map key $b 0 0 0 value $(iphex $ip); b=$((b+1)); done; done
  # 残り (self) は 0 のまま。ただし self を散らすため順序をずらす: bucket_of がハッシュなので連続でも可
  for ip in 10.100.0.19 10.100.0.176 $CLIENT; do bpftool map update pinned $PIN/mac_map key $(iphex $ip) value $(machex $ip); done
  ip link set dev $IF xdpdrv pinned $PIN/prog || exit 1
  # RPS: 1 キューを 2 CPU に分散 / XPS: 両 CPU -> tx-0
  echo 3 > /sys/class/net/$IF/queues/rx-0/rps_cpus
  echo 3 > /sys/class/net/$IF/queues/tx-0/xps_cpus
  irq=$(grep "$IF-Tx-Rx-0" /proc/interrupts | cut -d: -f1 | tr -d ' '); echo 0 > /proc/irq/$irq/smp_affinity_list
  systemctl restart nginx
else
  ip link set dev $IF xdpdrv off 2>/dev/null
  rm -rf $PIN
  for ip in 10.100.0.19 10.100.0.176 $CLIENT; do ip neigh del $ip dev $IF 2>/dev/null; done
  ip link set dev $IF mtu 9001
  ethtool -L $IF combined 2 2>&1; sleep 2
  echo 0 > /sys/class/net/$IF/queues/rx-0/rps_cpus 2>/dev/null
  /usr/local/bin/isucon-net-tune.sh
  systemctl restart isucon-tls-lb
  systemctl restart nginx
fi
