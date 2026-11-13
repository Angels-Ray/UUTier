#!/usr/bin/env bash
# 删除 netns、veth、ip rule 和 NAT 规则。不修改 uuremote 服务。
set -euo pipefail

NS=uutier-b
HOST_IF=uutier-b0
SUBNET=10.200.0.0/24
FORWARD_FILE=/run/uutier-lab-ip-forward

if [ "$(id -u)" -ne 0 ]; then
    echo "需要 root：sudo $0" >&2
    exit 1
fi

remove_nat_rules() {
    iptables -t nat -S POSTROUTING | awk -v subnet="$SUBNET" '
        {
            hit_subnet = 0
            hit_masquerade = 0
            iface = ""
            for (i = 1; i < NF; i++) {
                if ($i == "-s" && $(i + 1) == subnet) hit_subnet = 1
                if ($i == "-o") iface = $(i + 1)
                if ($i == "-j" && $(i + 1) == "MASQUERADE") hit_masquerade = 1
            }
            if (hit_subnet && hit_masquerade && iface != "") print iface
        }' | sort -u | while read -r iface; do
        while iptables -t nat -C POSTROUTING -s "$SUBNET" -o "$iface" -j MASQUERADE 2>/dev/null; do
            iptables -t nat -D POSTROUTING -s "$SUBNET" -o "$iface" -j MASQUERADE
        done
    done
}

if ip netns list | awk -v ns="$NS" '$1 == ns { found = 1 } END { exit !found }'; then
    pids="$(ip netns pids "$NS")"
    if [ -n "$pids" ]; then
        echo "netns $NS 内仍有进程：" >&2
        printf '%s\n' "$pids" >&2
        echo "先停止这些进程，再运行 sudo $0" >&2
        exit 1
    fi
    ip netns del "$NS"
fi
if ip link show "$HOST_IF" >/dev/null 2>&1; then
    ip link delete "$HOST_IF"
fi

ip rule del pref 8000 to "$SUBNET" lookup main 2>/dev/null || true
ip rule del pref 8001 from "$SUBNET" lookup main 2>/dev/null || true

while iptables -C FORWARD -s "$SUBNET" -j ACCEPT 2>/dev/null; do
    iptables -D FORWARD -s "$SUBNET" -j ACCEPT
done
while iptables -C FORWARD -d "$SUBNET" -j ACCEPT 2>/dev/null; do
    iptables -D FORWARD -d "$SUBNET" -j ACCEPT
done
remove_nat_rules

rm -rf "/etc/netns/$NS"
if [ -f "$FORWARD_FILE" ]; then
    sysctl -qw "net.ipv4.ip_forward=$(cat "$FORWARD_FILE")"
    rm -f "$FORWARD_FILE"
fi
echo "netns $NS 已删除。"
