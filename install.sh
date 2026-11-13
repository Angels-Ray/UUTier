#!/bin/sh
# 从 GitHub Release 下载并安装 UUTier。
set -eu

repo=${UUTIER_REPO:-Angels-Ray/UUTier}
action=
version=
prefix=
libc=
service_skip=0
target_user=
target_uid=
target_gid=
target_group=
target_home=
init_system=
service_active=0
service_existed=0
systemctl_bin=
rc_update_bin=
rc_service_bin=

usage() {
    cat <<'EOF'
用法：install.sh <install|update|uninstall|help> [选项]

命令：
  install     下载并安装最新版本
  update      安装最新版本
  uninstall   删除服务和二进制
  help        显示帮助

选项：
  --version VERSION  安装指定版本，例如 0.0.1
  --prefix DIR       安装目录；默认使用现有 uutier 所在目录
  --libc gnu|musl    选择 gnu 或 musl 构建；默认按 ldd 判断
  --no-service       不安装系统服务

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
        --no-service)
            service_skip=1
            shift
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
    [ "$service_skip" -eq 0 ] || die "uninstall 不支持 --no-service"
fi

target_user=$(id -un)
if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
    target_user=$SUDO_USER
fi
target_uid=$(id -u "$target_user" 2>/dev/null) || die "用户不存在：$target_user"
target_gid=$(id -g "$target_user" 2>/dev/null) || die "无法读取用户组：$target_user"
target_group=$(id -gn "$target_user" 2>/dev/null) || die "无法读取用户组名：$target_user"

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

service_unit=/etc/systemd/system/uutier.service
openrc_service=/etc/init.d/uutier
service_log_dir=/var/log/uutier
config_path=$target_home/.config/uutier/config.toml

systemctl_bin=$(find_tool systemctl || true)
rc_update_bin=$(find_tool rc-update || true)
rc_service_bin=$(find_tool rc-service || true)

detect_init() {
    if [ "$service_skip" -eq 1 ]; then
        echo none
        return 0
    fi
    if [ -n "$systemctl_bin" ] && [ -d /run/systemd/system ]; then
        echo systemd
    elif [ -n "$rc_update_bin" ] && [ -n "$rc_service_bin" ] && [ -d /run/openrc ]; then
        echo openrc
    else
        echo none
    fi
}

init_system=$(detect_init)
if [ "$init_system" != none ] && [ "$(id -u)" -ne 0 ]; then
    if [ "$action" != uninstall ] || [ -f "$service_unit" ] || [ -f "$openrc_service" ]; then
        if [ "$action" = uninstall ]; then
            die "删除系统服务需要 root；用 sudo 运行"
        fi
        die "安装系统服务需要 root；用 sudo 运行，或加 --no-service"
    fi
fi
if [ "$init_system" = none ] && [ "$service_skip" -eq 0 ]; then
    echo "未检测到 systemd 或 OpenRC，跳过系统服务" >&2
fi

service_is_active() {
    case "$init_system" in
        systemd) "$systemctl_bin" is-active --quiet uutier.service 2>/dev/null ;;
        openrc) "$rc_service_bin" uutier status >/dev/null 2>&1 ;;
        *) return 1 ;;
    esac
}

stop_service() {
    case "$init_system" in
        systemd) "$systemctl_bin" stop uutier.service ;;
        openrc) "$rc_service_bin" uutier stop ;;
        *) return 0 ;;
    esac
}

start_service() {
    case "$init_system" in
        systemd)
            if [ "$service_active" -eq 1 ]; then
                "$systemctl_bin" restart uutier.service
            else
                "$systemctl_bin" start uutier.service
            fi
            ;;
        openrc)
            if [ "$service_active" -eq 1 ]; then
                "$rc_service_bin" uutier restart
            else
                "$rc_service_bin" uutier start
            fi
            ;;
        *) return 0 ;;
    esac
}

wait_service() {
    i=0
    while [ "$i" -lt 10 ]; do
        if service_is_active; then
            return 0
        fi
        i=$((i + 1))
        sleep 1
    done
    return 1
}

write_service() {
    case "$init_system" in
        systemd)
            cat > "$service_unit" <<EOF
[Unit]
Description=UUTier mesh node
Wants=network-online.target
After=network-online.target
StartLimitIntervalSec=300
StartLimitBurst=10

[Service]
Type=simple
User=$target_user
Group=$target_group
WorkingDirectory=$target_home
Environment="HOME=$target_home"
Environment="XDG_CONFIG_HOME=$target_home/.config"
Environment="XDG_STATE_HOME=$target_home/.local/state"
ExecStart="$prefix/uutier" up
Restart=on-failure
RestartSec=10
RestartPreventExitStatus=2
TimeoutStopSec=15
CapabilityBoundingSet=CAP_NET_ADMIN
AmbientCapabilities=CAP_NET_ADMIN
NoNewPrivileges=yes
ProtectSystem=full
PrivateTmp=yes
ProtectKernelTunables=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
UMask=0077

[Install]
WantedBy=multi-user.target
EOF
            chmod 0644 "$service_unit" || die "无法设置 $service_unit 的权限"
            ;;
        openrc)
            mkdir -p "$service_log_dir" || die "无法创建 $service_log_dir"
            touch "$service_log_dir/service.log" "$service_log_dir/service.err" || die "无法创建服务日志"
            chown "$target_user:$target_gid" \
                "$service_log_dir" \
                "$service_log_dir/service.log" \
                "$service_log_dir/service.err" || die "无法设置服务日志的所有者"
            chmod 0700 "$service_log_dir" || die "无法设置 $service_log_dir 的权限"
            chmod 0600 "$service_log_dir/service.log" "$service_log_dir/service.err" || die "无法设置服务日志的权限"
            cat > "$openrc_service" <<EOF
#!/sbin/openrc-run

name="uutier"
description="UUTier mesh node"

supervisor="supervise-daemon"
command="$prefix/uutier"
command_args="up"
command_user="$target_user:$target_group"
directory="$target_home"
pidfile="/run/uutier.pid"
respawn_delay=10
respawn_max=10
respawn_period=300
output_log="/var/log/uutier/service.log"
error_log="/var/log/uutier/service.err"

export HOME="$target_home"
export XDG_CONFIG_HOME="$target_home/.config"
export XDG_STATE_HOME="$target_home/.local/state"

depend() {
    need net
    after firewall
}
EOF
            chmod 0755 "$openrc_service" || die "无法设置 $openrc_service 的权限"
            ;;
    esac
}

reload_service() {
    case "$init_system" in
        systemd) "$systemctl_bin" daemon-reload ;;
        *) return 0 ;;
    esac
}

enable_service() {
    case "$init_system" in
        systemd) "$systemctl_bin" enable uutier.service >/dev/null ;;
        openrc) "$rc_update_bin" add uutier default >/dev/null ;;
    esac
}

remove_service() {
    case "$init_system" in
        systemd)
            if [ -f "$service_unit" ]; then
                "$systemctl_bin" stop uutier.service >/dev/null 2>&1 || true
                "$systemctl_bin" disable uutier.service >/dev/null 2>&1 || true
                rm -f "$service_unit"
                "$systemctl_bin" daemon-reload || true
            fi
            ;;
        openrc)
            if [ -f "$openrc_service" ]; then
                "$rc_service_bin" uutier stop >/dev/null 2>&1 || true
                "$rc_update_bin" del uutier default >/dev/null 2>&1 || true
                rm -f "$openrc_service"
            fi
            ;;
    esac
}

if [ "$action" = update ]; then
    [ -x "$prefix/uutier" ] || die "未安装：$prefix/uutier"
fi

if [ "$action" = uninstall ]; then
    remove_service
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

if [ "$init_system" != none ] && service_is_active; then
    service_active=1
    stop_service || die "无法停止 uutier 服务"
fi

if command -v pgrep >/dev/null 2>&1 && pgrep -x uutier >/dev/null 2>&1; then
    if [ "$service_active" -eq 1 ]; then
        start_service >/dev/null 2>&1 || true
    fi
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

if [ "$init_system" != none ]; then
    case "$init_system" in
        systemd)
            if [ -f "$service_unit" ]; then
                service_existed=1
            fi
            ;;
        openrc)
            if [ -f "$openrc_service" ]; then
                service_existed=1
            fi
            ;;
    esac
    write_service
    reload_service || die "systemctl daemon-reload 失败"
    if [ "$action" = install ] || [ "$service_existed" -eq 0 ]; then
        enable_service || die "无法启用 uutier 服务"
    fi

    start=1
    config_missing=0
    [ -f "$config_path" ] || config_missing=1
    if [ "$service_active" -eq 0 ] && [ "$action" = update ] && [ "$service_existed" -eq 1 ]; then
        start=0
    fi
    if [ "$service_active" -eq 0 ] && [ "$config_missing" -eq 1 ]; then
        start=0
    fi

    if [ "$start" -eq 1 ]; then
        if ! start_service; then
            case "$init_system" in
                systemd) echo "查看服务状态：systemctl status uutier" >&2 ;;
                openrc) echo "查看服务状态：rc-service uutier status" >&2 ;;
            esac
            die "无法启动 uutier 服务"
        fi
        if ! wait_service; then
            case "$init_system" in
                systemd) echo "查看服务状态：systemctl status uutier" >&2 ;;
                openrc) echo "查看服务状态：rc-service uutier status" >&2 ;;
            esac
            exit 1
        fi
        echo "服务已启动：uutier"
    elif [ "$config_missing" -eq 1 ]; then
        echo "未找到 $config_path，服务已启用但未启动" >&2
        case "$init_system" in
            systemd) echo "配置和登录后执行：systemctl start uutier" >&2 ;;
            openrc) echo "配置和登录后执行：rc-service uutier start" >&2 ;;
        esac
    else
        echo "服务未运行，保持停止" >&2
    fi
fi

echo "已安装：$prefix/uutier"
"$prefix/uutier" --version
if getcap_bin=$(find_tool getcap); then
    "$getcap_bin" "$prefix/uutier"
fi
