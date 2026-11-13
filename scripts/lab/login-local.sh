#!/usr/bin/env bash
# 用两个独立的配置目录登录两台带 uutier- 标记的设备。扫码两次。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LAB="$ROOT/target/lab"
BIN="$ROOT/target/release/uutier"
NEW_DEVICE=0

if [ "${1:-}" = "--new-device" ]; then
    NEW_DEVICE=1
fi

export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release --locked -p uutier-cli

login_instance() {
    local name="$1"
    local home="$LAB/$name/home"
    local config_home="$home/.config"
    local credentials="$config_home/uutier/uu.json"
    mkdir -p "$config_home"
    chmod 700 "$LAB" "$home" "$config_home"

    local args=()
    if [ "$NEW_DEVICE" -eq 1 ]; then
        args+=(--new-device)
    elif [ -f "$credentials" ]; then
        echo "$name 已有凭据，跳过登录。重新注册设备用 --new-device。"
        return
    fi

    echo "登录 $name：用 UU 远程手机应用扫描下面的二维码。"
    HOME="$home" XDG_CONFIG_HOME="$config_home" "$BIN" login "${args[@]}"
    echo "$name 登录完成。"
}

login_instance a
login_instance b

echo "本机两台设备已登录，凭据分别在 $LAB/a/home/.config/uutier 和 $LAB/b/home/.config/uutier。"
