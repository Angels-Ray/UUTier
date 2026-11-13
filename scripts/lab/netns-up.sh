#!/usr/bin/env bash
# 创建运行第三台设备 B 的 netns。B 的出站流量经主机 NAT。
set -euo pipefail

NS=uutier-b
HOST_IF=uutier-b0
NS_IF=uutier-b1
HOST_IP=10.200.0.1/24
NS_IP=10.200.0.2/24
SUBNET=10.200.0.0/24
FORWARD_FILE=/run/uutier-lab-ip-forward

if [ "$(id -u)" -ne 0 ]; then
    echo "需要 root：sudo $0" >&2
    exit 1
fi
if ip netns list | awk -v ns="$NS" '$1 == ns { found = 1 } END { exit !found }'; then
    echo "netns $NS 已存在。先运行 sudo scripts/lab/netns-down.sh" >&2
    exit 1
fi
if ip link show "$HOST_IF" >/dev/null 2>&1; then
    echo "$HOST_IF 已存在。先运行 sudo scripts/lab/netns-down.sh" >&2
    exit 1
fi
IFACES="$(ip route show default | awk '{ for (i = 1; i < NF; i++) if ($i == "dev") { print $(i + 1); break } }' | sort -u)"
if [ -z "$IFACES" ]; then
    echo "没有 IPv4 默认路由，无法为 $NS 设置 NAT" >&2
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

cleanup() {
    status="$1"
    trap - ERR INT TERM
    set +e
    ip netns del "$NS" 2>/dev/null
    if ip link show "$HOST_IF" >/dev/null 2>&1; then
        ip link delete "$HOST_IF"
    fi
    ip rule del pref 8000 to "$SUBNET" lookup main 2>/dev/null
    ip rule del pref 8001 from "$SUBNET" lookup main 2>/dev/null
    while iptables -C FORWARD -s "$SUBNET" -j ACCEPT 2>/dev/null; do
        iptables -D FORWARD -s "$SUBNET" -j ACCEPT || break
    done
    while iptables -C FORWARD -d "$SUBNET" -j ACCEPT 2>/dev/null; do
        iptables -D FORWARD -d "$SUBNET" -j ACCEPT || break
    done
    remove_nat_rules
    rm -rf "/etc/netns/$NS"
    if [ -f "$FORWARD_FILE" ]; then
        sysctl -qw "net.ipv4.ip_forward=$(cat "$FORWARD_FILE")"
        rm -f "$FORWARD_FILE"
    fi
    exit "$status"
}

trap 'cleanup $?' ERR
trap 'cleanup 130' INT
trap 'cleanup 143' TERM

printf '%s\n' "$(sysctl -n net.ipv4.ip_forward)" > "$FORWARD_FILE"
sysctl -qw net.ipv4.ip_forward=1

ip netns add "$NS"
ip link add "$HOST_IF" type veth peer name "$NS_IF"
ip link set "$NS_IF" netns "$NS"
ip addr add "$HOST_IP" dev "$HOST_IF"
ip link set "$HOST_IF" up
ip netns exec "$NS" ip link set lo up
ip netns exec "$NS" ip addr add "$NS_IP" dev "$NS_IF"
ip netns exec "$NS" ip link set "$NS_IF" up
ip netns exec "$NS" ip route add default via 10.200.0.1
ip netns exec "$NS" sysctl -qw net.ipv6.conf.all.disable_ipv6=1

mkdir -p "/etc/netns/$NS"
printf 'nameserver 223.5.5.5\nnameserver 119.29.29.29\n' > "/etc/netns/$NS/resolv.conf"

ip rule add pref 8000 to "$SUBNET" lookup main
ip rule add pref 8001 from "$SUBNET" lookup main
iptables -I FORWARD 1 -s "$SUBNET" -j ACCEPT
iptables -I FORWARD 1 -d "$SUBNET" -j ACCEPT
for iface in $IFACES; do
    iptables -t nat -A POSTROUTING -s "$SUBNET" -o "$iface" -j MASQUERADE
done

trap - ERR INT TERM
echo "netns $NS 已创建，B 在 netns 内使用 10.200.0.2。"
