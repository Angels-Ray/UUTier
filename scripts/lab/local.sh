#!/usr/bin/env bash
# 生成并启动本机节点 A。先运行 scripts/lab/login-local.sh。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LAB="$ROOT/target/lab"
BIN="$ROOT/target/release/uutier"
HOME_A="$LAB/a/home"
CONFIG_HOME_A="$HOME_A/.config"
CONFIG="$LAB/a/config.toml"
PORT=12010

if [ ! -x "$BIN" ] || [ ! -f "$LAB/secret" ]; then
    echo "先运行 scripts/lab/remote.sh 生成 secret 并构建二进制" >&2
    exit 1
fi
if [ ! -f "$CONFIG_HOME_A/uutier/uu.json" ]; then
    echo "本机设备 A 没有凭据。先运行 scripts/lab/login-local.sh" >&2
    exit 1
fi
if ip link show uutier0 >/dev/null 2>&1; then
    echo "本机已存在 uutier0。停止旧节点后执行 sudo ip link delete uutier0" >&2
    exit 1
fi

SECRET="$(cat "$LAB/secret")"

echo "==> 写入本机配置"
mkdir -p "$LAB/a" "$CONFIG_HOME_A"
chmod 700 "$LAB" "$LAB/a" "$HOME_A" "$CONFIG_HOME_A"
cat > "$CONFIG" <<EOF
[turn]
enabled = true
mode = "direct-first"
request_timeout_s = 5

[network]
name = "uutier-lab"
secret = "$SECRET"
ipv4 = "10.144.0.2"
mtu_bytes = 1280
listen_port = $PORT
EOF
chmod 600 "$CONFIG"

echo "==> 启动本机节点 A，按 Ctrl+C 停止"
echo "本机虚拟地址 10.144.0.2，远端 10.144.0.3，netns 设备 B 为 10.144.0.4"
echo "查看状态："
echo "  sudo env HOME=\"$HOME_A\" XDG_CONFIG_HOME=\"$CONFIG_HOME_A\" \"$BIN\" status"
echo "测试："
echo "  ping -c 3 10.144.0.3"
echo "  ping -c 3 10.144.0.4"

sudo env HOME="$HOME_A" XDG_CONFIG_HOME="$CONFIG_HOME_A" RUST_LOG=info "$BIN" --config "$CONFIG" up
