#!/bin/sh
# 从 GitHub Release 下载并安装 UUTier，保留现有配置和凭据。
set -eu

repo=${UUTIER_REPO:-Angels-Ray/UUTier}
action=
version=
prefix=
libc=
target_user=
target_uid=
target_gid=
target_home=

usage() {
    cat <<'EOF'
用法：install.sh <install|update|uninstall|help> [选项]

命令：
  install     下载并安装最新版本
  update      安装最新版本，保留配置和凭据
  uninstall   删除二进制，保留配置和凭据
  help        显示帮助

选项：
  --version VERSION  安装指定版本，例如 0.0.1
  --prefix DIR       安装目录；默认使用现有 uutier 所在目录
  --libc gnu|musl    选择 gnu 或 musl 构建；默认按 ldd 判断

环境变量：UUTIER_REPO 覆盖 GitHub 仓库，格式 owner/name。
EOF
}

die() {
    echo "install.sh: $*" >&2
    exit 1
}

run_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    elif [ -t 0 ]; then
        sudo "$@"
    else
        case "$0" in
            sh|bash|dash|ash|/bin/sh|/bin/bash|/bin/dash|/bin/ash)
                if [ -r /dev/tty ] && [ -w /dev/tty ]; then
                    sudo "$@" </dev/tty
                else
                    sudo -n "$@" || die "需要 sudo 权限：先下载脚本，再用 sh install.sh 运行"
                fi
                ;;
            *)
                sudo -S -p '' "$@"
                ;;
        esac
    fi
}

find_tool() {
    name=$1
    if tool_path=$(command -v "$name" 2>/dev/null); then
        echo "$tool_path"
        return 0
    fi
    for dir in /sbin /usr/sbin /bin /usr/bin; do
        if [ -x "$dir/$name" ]; then
            echo "$dir/$name"
            return 0
        fi
    done
    return 1
}

[ "$#" -ge 1 ] || { usage; exit 0; }
action=$1
shift
case "$action" in
    -h|--help|help)
        usage
        exit 0
        ;;
    install|update|uninstall) ;;
    *)
        usage >&2
        exit 2
        ;;
esac

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || die "--version 缺少参数"
            version=$2
            shift 2
            ;;
        --prefix)
            [ "$#" -ge 2 ] || die "--prefix 缺少参数"
            prefix=$2
            shift 2
            ;;
        --libc)
            [ "$#" -ge 2 ] || die "--libc 缺少参数"
            libc=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            exit 2
            ;;
    esac
done

if [ "$action" = uninstall ]; then
    [ -z "$version" ] || die "uninstall 不支持 --version"
    [ -z "$libc" ] || die "uninstall 不支持 --libc"
fi

target_user=$(id -un)
if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
    target_user=$SUDO_USER
fi
target_uid=$(id -u "$target_user" 2>/dev/null) || die "用户不存在：$target_user"
target_gid=$(id -g "$target_user" 2>/dev/null) || die "无法读取用户组：$target_user"

if [ "$target_user" = "$(id -un)" ]; then
    target_home=${HOME:-}
else
    target_home=$(awk -F: -v u="$target_user" '$1 == u { print $6; exit }' /etc/passwd 2>/dev/null || true)
fi

if [ -n "$prefix" ]; then
    case "$prefix" in
        */) prefix=${prefix%/} ;;
    esac
    [ -n "$prefix" ] || die "--prefix 不能为空"
fi

if [ -z "$prefix" ]; then
    if [ "$target_user" = "$(id -un)" ]; then
        existing=$(command -v uutier 2>/dev/null || true)
        if [ -n "$existing" ] && [ -x "$existing" ]; then
            prefix=$(dirname -- "$existing")
        fi
    fi
    if [ -z "$prefix" ] && [ -n "$target_home" ]; then
        for candidate in "$target_home/.cargo/bin/uutier" "$target_home/.local/bin/uutier"; do
            if [ -x "$candidate" ]; then
                prefix=$(dirname -- "$candidate")
                break
            fi
        done
    fi
    if [ -z "$prefix" ]; then
        if [ "$target_user" = root ]; then
            prefix=/usr/local/bin
        elif [ -n "$target_home" ]; then
            prefix="$target_home/.cargo/bin"
        else
            die "无法确定安装目录，用 --prefix 指定"
        fi
    fi
fi

if [ "$action" = update ]; then
    [ -x "$prefix/uutier" ] || die "未安装：$prefix/uutier"
fi

if [ "$action" = uninstall ]; then
    bin=$prefix/uutier
    if [ ! -e "$bin" ]; then
        echo "未安装：$bin"
        exit 0
    fi
    if command -v pgrep >/dev/null 2>&1 && pgrep -x uutier >/dev/null 2>&1; then
        die "uutier 正在运行，先停止进程再卸载"
    fi
    if [ -w "$prefix" ] || [ "$(id -u)" -eq 0 ]; then
        rm -f "$bin"
    elif command -v sudo >/dev/null 2>&1; then
        run_root rm -f "$bin"
    else
        die "没有权限删除 $bin"
    fi
    echo "已删除：$bin"
    echo "配置和凭据未改动"
    exit 0
fi

[ "$(uname -s)" = Linux ] || die "只支持 Linux"
case "$(uname -m)" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) die "不支持的架构：$(uname -m)" ;;
esac

case "$libc" in
    "")
        if ldd --version 2>&1 | sed -n '1p' | grep -qi musl; then
            libc=musl
        else
            libc=gnu
        fi
        ;;
    gnu|musl) ;;
    *) die "--libc 只接受 gnu 或 musl" ;;
esac

if [ -n "$version" ]; then
    case "$version" in
        latest) version= ;;
        v*) version=${version#v} ;;
    esac
    if [ -n "$version" ]; then
        case "$version" in
            *[!0-9.]*|.*|*.) die "版本号格式无效：$version" ;;
        esac
    fi
fi

case "$repo" in
    */*) ;;
    *) die "UUTIER_REPO 格式应为 owner/name" ;;
esac
case "$repo" in
    *[!A-Za-z0-9._/-]*) die "UUTIER_REPO 包含无效字符" ;;
esac

if [ -n "$version" ]; then
    base="https://github.com/$repo/releases/download/v$version"
else
    base="https://github.com/$repo/releases/latest/download"
fi
asset="uutier-linux-$arch-$libc"

command -v sha256sum >/dev/null 2>&1 || die "缺少 sha256sum"
setcap_bin=$(find_tool setcap) || die "缺少 setcap：Debian/Ubuntu 执行 apt install libcap2-bin，Alpine 执行 apk add libcap"

if [ "$(id -u)" -ne 0 ] && ! command -v sudo >/dev/null 2>&1; then
    die "需要 root 或 sudo 来安装和设置 CAP_NET_ADMIN"
fi

download() {
    url=$1
    dest=$2
    if command -v curl >/dev/null 2>&1; then
        curl -fL --retry 3 --connect-timeout 10 --progress-bar -o "$dest" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$dest" "$url"
    else
        die "缺少 curl 或 wget"
    fi
}

tmp=$(mktemp -d) || die "无法创建临时目录"
trap 'rm -rf "$tmp"' 0 1 2 15

echo "下载 $asset"
download "$base/$asset" "$tmp/$asset" || die "下载失败：$base/$asset"
download "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "下载校验文件失败：$base/SHA256SUMS"

line=$(grep "  $asset\$" "$tmp/SHA256SUMS" || true)
[ -n "$line" ] || die "SHA256SUMS 中没有 $asset"
printf '%s\n' "$line" > "$tmp/asset.sha"
(cd "$tmp" && sha256sum -c asset.sha >/dev/null) || die "SHA256 校验失败"
chmod 0700 "$tmp/$asset"
"$tmp/$asset" --version >/dev/null 2>&1 || die "下载的二进制无法执行：$asset"

if command -v pgrep >/dev/null 2>&1 && pgrep -x uutier >/dev/null 2>&1; then
    die "uutier 正在运行，先停止进程再安装"
fi

if [ "$(id -u)" -eq 0 ]; then
    if [ ! -d "$prefix" ]; then
        mkdir -p "$prefix" || die "无法创建 $prefix"
        if [ "$target_user" != root ]; then
            chown "$target_uid:$target_gid" "$prefix" || die "无法设置 $prefix 的所有者"
            if [ "$prefix" = "$target_home/.cargo/bin" ]; then
                chown "$target_uid:$target_gid" "$target_home/.cargo" || die "无法设置 $target_home/.cargo 的所有者"
            fi
        fi
    fi
    cp "$tmp/$asset" "$prefix/uutier" || die "无法写入 $prefix/uutier"
    chmod 0700 "$prefix/uutier" || die "无法设置 $prefix/uutier 的权限"
    if [ "$target_user" != root ]; then
        chown "$target_uid:$target_gid" "$prefix/uutier" || die "无法设置 $prefix/uutier 的所有者"
    fi
    if ! "$setcap_bin" cap_net_admin+ep "$prefix/uutier"; then
        rm -f "$prefix/uutier"
        die "setcap 失败：$prefix/uutier"
    fi
elif mkdir -p "$prefix" 2>/dev/null && [ -w "$prefix" ]; then
    cp "$tmp/$asset" "$prefix/uutier" || die "无法写入 $prefix/uutier"
    chmod 0700 "$prefix/uutier" || die "无法设置 $prefix/uutier 的权限"
    if ! run_root "$setcap_bin" cap_net_admin+ep "$prefix/uutier"; then
        rm -f "$prefix/uutier"
        die "setcap 失败：$prefix/uutier"
    fi
else
    run_root mkdir -p "$prefix"
    run_root cp "$tmp/$asset" "$prefix/uutier" || die "无法写入 $prefix/uutier"
    run_root chmod 0700 "$prefix/uutier" || die "无法设置 $prefix/uutier 的权限"
    run_root chown "$target_uid:$target_gid" "$prefix/uutier" || die "无法设置 $prefix/uutier 的所有者"
    if ! run_root "$setcap_bin" cap_net_admin+ep "$prefix/uutier"; then
        run_root rm -f "$prefix/uutier"
        die "setcap 失败：$prefix/uutier"
    fi
fi

echo "已安装：$prefix/uutier"
"$prefix/uutier" --version
if getcap_bin=$(find_tool getcap); then
    "$getcap_bin" "$prefix/uutier"
fi
echo "配置和凭据未改动"
