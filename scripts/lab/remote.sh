#!/usr/bin/env bash
# 推送 UUTier 到 UUTIER_LAB_REMOTE，生成远端配置并启动节点。
#
#   UUTIER_LAB_REMOTE=user@host ./scripts/lab/remote.sh login [--new-device]
#   UUTIER_LAB_REMOTE=user@host ./scripts/lab/remote.sh
#
# 第一次 ssh 和远端 sudo 会提示输入密码。
set -euo pipefail

REMOTE="${UUTIER_LAB_REMOTE:?set UUTIER_LAB_REMOTE=user@host}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LAB="$ROOT/target/lab"
BIN="$ROOT/target/release/uutier"
REMOTE_DIR="uutier-lab"
PORT=12010

export PATH="$HOME/.cargo/bin:$PATH"

mkdir -p "$LAB"
chmod 700 "$LAB"

if [ ! -f "$LAB/secret" ]; then
    (umask 077; head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$LAB/secret")
fi
SECRET="$(cat "$LAB/secret")"

echo "==> 构建 release 二进制"
cargo build --release --locked -p uutier-cli

SSH_OPTS=(-o ControlMaster=auto -o ControlPath="$LAB/ssh.sock" -o ControlPersist=600)
SSH=(ssh "${SSH_OPTS[@]}" "$REMOTE")
SSH_TTY=(ssh "${SSH_OPTS[@]}" -t "$REMOTE")
cleanup() {
    ssh "${SSH_OPTS[@]}" -O exit "$REMOTE" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "==> 检查远端架构"
REMOTE_ARCH="$("${SSH[@]}" 'uname -m')"
LOCAL_ARCH="$(uname -m)"
if [ "$REMOTE_ARCH" != "$LOCAL_ARCH" ]; then
    echo "远端架构 $REMOTE_ARCH 与本机 $LOCAL_ARCH 不一致，不能直接推送二进制" >&2
    exit 1
fi

echo "==> 推送二进制到 $REMOTE"
"${SSH[@]}" "mkdir -p \"\$HOME/$REMOTE_DIR\""
scp -q -o ControlPath="$LAB/ssh.sock" "$BIN" "$REMOTE:$REMOTE_DIR/uutier"
"${SSH[@]}" "chmod 755 \"\$HOME/$REMOTE_DIR/uutier\""

REMOTE_HOME="$("${SSH[@]}" 'printf %s "$HOME"')"
REMOTE_CREDENTIALS="$REMOTE_HOME/.config/uutier/uu.json"

if [ "${1:-}" = "login" ]; then
    shift
    case "${1:-}" in
        "") LOGIN_ARGS="" ;;
        "--new-device") LOGIN_ARGS="--new-device" ;;
        *)
            echo "用法：$0 login [--new-device]" >&2
            exit 2
            ;;
    esac
    if [ "$#" -gt 1 ]; then
        echo "用法：$0 login [--new-device]" >&2
        exit 2
    fi
    if [ "$LOGIN_ARGS" != "--new-device" ] && "${SSH[@]}" "test -f '$REMOTE_CREDENTIALS'"; then
        echo "远端已有 UUTier 凭据，跳过登录。需要注册新设备时加 --new-device。"
        exit 0
    fi
    echo "==> 远端扫码登录。用 UU 远程手机应用扫描二维码。"
    "${SSH_TTY[@]}" "env HOME=\"\$HOME/$REMOTE_DIR\" XDG_CONFIG_HOME=\"\$HOME/.config\" \"\$HOME/$REMOTE_DIR/uutier\" login $LOGIN_ARGS"
    echo "远端登录完成。"
    exit 0
fi

if ! "${SSH[@]}" "test -f '$REMOTE_CREDENTIALS'"; then
    echo "远端没有 UUTier 凭据：$REMOTE_CREDENTIALS" >&2
    echo "先运行 ./scripts/lab/remote.sh login" >&2
    exit 1
fi

echo "==> 写入远端配置"
REMOTE_CONFIG="$LAB/remote-config.toml"
cat > "$REMOTE_CONFIG" <<EOF
[turn]
enabled = true
mode = "direct-first"
request_timeout_s = 5

[network]
name = "uutier-lab"
secret = "$SECRET"
ipv4 = "10.144.0.3"
mtu_bytes = 1280
listen_port = $PORT
EOF
chmod 600 "$REMOTE_CONFIG"
scp -q -o ControlPath="$LAB/ssh.sock" "$REMOTE_CONFIG" "$REMOTE:$REMOTE_DIR/config.toml"
"${SSH[@]}" "chmod 600 \"\$HOME/$REMOTE_DIR/config.toml\""

if "${SSH[@]}" "ip link show uutier0 >/dev/null 2>&1"; then
    echo "远端已存在 uutier0。停止旧节点后执行 sudo ip link delete uutier0" >&2
    exit 1
fi

echo "==> 启动远端节点，按 Ctrl+C 停止"
echo "远端虚拟地址 10.144.0.3"
echo "查看远端状态："
echo "  ssh $REMOTE"
echo "  sudo env HOME=\"\$HOME/$REMOTE_DIR\" XDG_CONFIG_HOME=\"\$HOME/.config\" \"\$HOME/$REMOTE_DIR/uutier\" status"
echo "在本机另外的终端运行 scripts/lab/login-local.sh 和 scripts/lab/local.sh。"

"${SSH_TTY[@]}" 'sudo env HOME="$HOME/uutier-lab" XDG_CONFIG_HOME="$HOME/.config" RUST_LOG=info "$HOME/uutier-lab/uutier" --config "$HOME/uutier-lab/config.toml" up'
