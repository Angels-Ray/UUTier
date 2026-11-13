# 部署

## 安装脚本

从最新 Release 安装：

```bash
curl -fsSL "https://github.com/Angels-Ray/UUTier/blob/main/install.sh?raw=true" | sudo bash -s install
```

脚本按 `uname -m` 和 `ldd` 选择 `uutier-linux-<arch>-<libc>` 资产，下载 `SHA256SUMS` 校验，安装后执行 `setcap cap_net_admin+ep`。通过 `sudo` 运行时安装到发起 `sudo` 的用户；默认使用该用户现有 `uutier` 所在目录，没有时用 `~/.cargo/bin`。以 root 直接运行且没有现有 `uutier` 时使用 `/usr/local/bin`。脚本不修改配置和凭据。

| 命令 | 行为 |
|---|---|
| `install` | 下载并安装最新版本 |
| `update` | 安装最新版本，要求已安装 |
| `uninstall` | 删除二进制，保留配置和凭据 |
| `help` | 显示帮助 |

`--version VERSION` 安装指定版本，`--prefix DIR` 指定安装目录，`--libc gnu|musl` 跳过 C 库判断；`UUTIER_REPO` 覆盖 GitHub 仓库，`sudo` 默认不传递该变量，需要时用 `sudo -E`。需要 sudo 密码且没有终端时，先下载脚本，再用 `sh install.sh install` 运行，密码从标准输入读取：

```bash
curl -fsSL "https://github.com/Angels-Ray/UUTier/blob/main/install.sh?raw=true" -o install.sh
sh install.sh install
```

Alpine 没有 `bash` 时用 `sudo sh -s install`。

## 从源码安装

安装二进制并授予创建 TUN 的权限：

```bash
cargo install --locked --path crates/uutier-cli
sudo setcap cap_net_admin+ep "$(command -v uutier)"
```

多用户主机还要执行 `chmod 700 "$(command -v uutier)"`，避免其他用户执行这个带能力的二进制。

`cargo install` 写入 `~/.cargo/bin/uutier`；设置 `CARGO_HOME` 时写入 `$CARGO_HOME/bin/uutier`，用户单元的 `ExecStart` 需要改成实际路径。

创建配置：

```bash
install -d -m 700 "${XDG_CONFIG_HOME:-$HOME/.config}/uutier"
[ -e "${XDG_CONFIG_HOME:-$HOME/.config}/uutier/config.toml" ] || \
    install -m 600 config.example.toml "${XDG_CONFIG_HOME:-$HOME/.config}/uutier/config.toml"
${EDITOR:-vi} "${XDG_CONFIG_HOME:-$HOME/.config}/uutier/config.toml"
```

`network.secret` 必填，至少 16 byte，用 `openssl rand -hex 32` 生成。同一网络的所有节点使用同一个值。

登录并检查：

```bash
uutier login
uutier config check
uutier doctor
```

## systemd 用户单元

安装单元：

```bash
mkdir -p ~/.config/systemd/user
cp packaging/systemd/uutier.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now uutier
```

用户单元把 `XDG_CONFIG_HOME` 固定为 `~/.config`，`XDG_STATE_HOME` 固定为 `~/.local/state`；使用其他路径时，执行 `systemctl --user edit uutier` 添加 `Environment=`，然后执行 `systemctl --user restart uutier`。

用户单元没有 `CapabilityBoundingSet`、`ProtectKernelTunables` 和 `RestrictAddressFamilies`，因为这些选项在用户管理器里会强制 `NoNewPrivs=1`，使 setcap 授予的 `CAP_NET_ADMIN` 失效。

用户单元用 `StartLimitIntervalSec=0` 关闭启动限速；开机时网络未就绪的启动失败每 10 s 重试一次，直到网络可用。

无图形登录的服务器需要开启 linger，用户服务才会在开机后启动：

```bash
loginctl enable-linger "$USER"
```

查看日志和停止：

```bash
journalctl --user -u uutier -f
systemctl --user stop uutier
```

## systemd 系统单元

服务器使用系统单元，由 `AmbientCapabilities=CAP_NET_ADMIN` 授予服务进程 TUN 权限；节点以 `uutier@用户名` 运行，该用户需要先完成上文的配置和登录。

```bash
cargo build --release --locked -p uutier-cli
sudo install -m 0755 target/release/uutier /usr/local/bin/uutier
sudo cp packaging/systemd/uutier@.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now uutier@alice
```

`uutier@alice` 中的 `alice` 是执行过 `uutier login` 的用户名，也是单元里的 `User=`。

系统单元不设置 `XDG_*`；登录时使用自定义 `XDG_CONFIG_HOME` 或 `XDG_STATE_HOME` 时，执行 `sudo systemctl edit uutier@alice` 添加 `Environment=`，指向相同的路径，然后执行 `sudo systemctl restart uutier@alice`。

`uutier doctor` 的 TUN 检查读取登录会话的能力，可能报 `missing CAP_NET_ADMIN`，该失败不影响服务启动。

查看日志和停止：

```bash
journalctl -u uutier@alice -f
sudo systemctl stop uutier@alice
```

系统单元在 300 s 内启动失败 10 次后停止重试；配置、凭据、身份文件、设备标记和 TUN 权限错误以退出码 2 结束，不会触发重启。

## 升级

执行安装脚本的 `update`：

```bash
curl -fsSL "https://github.com/Angels-Ray/UUTier/blob/main/install.sh?raw=true" | sudo bash -s update
```

从源码安装时执行：

```bash
cargo install --locked --path crates/uutier-cli
sudo setcap cap_net_admin+ep "$(command -v uutier)"
```

`cargo install` 替换二进制并重置文件权限；多用户主机要再次执行 `chmod 700 "$(command -v uutier)"`，然后重启单元：

用户单元：

```bash
systemctl --user restart uutier
```

系统单元：

```bash
sudo systemctl restart uutier@alice
```

升级时只替换二进制，不重新复制单元文件，因为重新复制会覆盖 `systemctl edit` 的修改和单元里的 `Environment=`。

## 卸载

先删除凭据：

```bash
uutier logout
```

`uutier logout` 删除配置指定的凭据文件，包括自定义 `[uu] path`。

用户单元：

```bash
systemctl --user disable --now uutier
rm ~/.config/systemd/user/uutier.service
systemctl --user daemon-reload
sudo setcap -r "$(command -v uutier)" 2>/dev/null || true
cargo uninstall uutier-cli
loginctl disable-linger "$USER"
```

使用过 linger 时执行最后一行。

系统单元：

```bash
sudo systemctl disable --now uutier@alice
sudo rm /etc/systemd/system/uutier@.service
sudo systemctl daemon-reload
sudo rm /usr/local/bin/uutier
```

执行过 `cargo install` 时，再执行 `cargo uninstall uutier-cli`。

删除配置和数据：

```bash
rm -r ~/.config/uutier ~/.local/state/uutier
```

使用自定义 XDG 路径时，删除对应的配置和状态目录。
