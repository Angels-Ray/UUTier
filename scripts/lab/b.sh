#!/usr/bin/env bash
# 在 netns 中启动第三台设备 B。先运行 scripts/lab/login-local.sh 和 netns-up.sh。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LAB="$ROOT/target/lab"
BIN="$ROOT/target/release/uutier"
HOME_B="$LAB/b/home"
CONFIG_HOME_B="$HOME_B/.config"
CONFIG="$LAB/b/config.toml"
NS=uutier-b
PORT=12011

if [ ! -x "$BIN" ] || [ ! -f "$LAB/secret" ]; then
    echo "先运行 scripts/lab/remote.sh 生成 secret 并构建二进制" >&2
    exit 1
fi
if [ ! -f "$CONFIG_HOME_B/uutier/uu.json" ]; then
    echo "设备 B 没有凭据。先运行 scripts/lab/login-local.sh" >&2
    exit 1
fi
if ! ip netns list | awk -v ns="$NS" '$1 == ns { found = 1 } END { exit !found }'; then
    echo "先运行 sudo scripts/lab/netns-up.sh" >&2
    exit 1
fi
if sudo ip netns exec "$NS" ip link show uutier0 >/dev/null 2>&1; then
    echo "netns $NS 内已存在 uutier0。停止旧节点后执行 sudo ip netns exec $NS ip link delete uutier0" >&2
    exit 1
fi

SECRET="$(cat "$LAB/secret")"

echo "==> 写入设备 B 配置"
mkdir -p "$LAB/b" "$CONFIG_HOME_B"
chmod 700 "$LAB" "$LAB/b" "$HOME_B" "$CONFIG_HOME_B"
cat > "$CONFIG" <<EOF
[turn]
enabled = true
mode = "direct-first"
request_timeout_s = 5

[network]
name = "uutier-lab"
secret = "$SECRET"
ipv4 = "10.144.0.4"
mtu_bytes = 1280
listen_port = $PORT
EOF
chmod 600 "$CONFIG"

echo "==> 在 netns $NS 中启动设备 B，按 Ctrl+C 停止"
echo "B 虚拟地址 10.144.0.4。netns 内只有 10.200.0.2，直连候选不可达，B 的链路应走 TURN。"
echo "查看状态："
echo "  sudo ip netns exec \"$NS\" env HOME=\"$HOME_B\" XDG_CONFIG_HOME=\"$CONFIG_HOME_B\" \"$BIN\" status"

sudo ip netns exec "$NS" env HOME="$HOME_B" XDG_CONFIG_HOME="$CONFIG_HOME_B" RUST_LOG=info \
    "$BIN" --config "$CONFIG" up
