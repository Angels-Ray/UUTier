# UUTier

UUTier 是一个组网工具，节点之间优先直连，直连不通时用 UU 云 TURN 转发 UDP 数据。

## 命令

| 命令 | 行为 |
|---|---|
| `uutier login` | 扫码登录 UU 账号；`--new-device` 注册新设备 |
| `uutier logout` | 删除凭据文件 |
| `uutier up` | 前台运行节点，需要创建 TUN 的权限 |
| `uutier status` | 读取状态文件；`--json` 输出机器可读格式 |
| `uutier doctor` | 检查配置、凭据、身份、UU 设备列表和 TUN |
| `uutier config check` | 校验配置 |
| `uutier config show` | 输出脱敏配置 |
| `uutier key show` | 创建或读取身份密钥，输出 peer ID 和公钥 |

退出码：0 表示成功，1 表示运行错误，2 表示参数、配置或启动错误；`uutier doctor` 有检查失败时返回 1，`uutier status` 在状态文件不存在、过期或读取失败时返回 1。

所有命令接受 `--log-level <level>`，默认 `info`，未指定时读取 `RUST_LOG`；空值视为未设置，值无效时输出警告并使用 `info`。

读取配置的命令接受 `--config <path>`：`login`、`logout`、`up`、`doctor`、`config check`、`config show`，也可以用 `UUTIER_CONFIG` 指定；`status` 和 `key show` 不读取配置文件，显式给出 `--config` 时返回 2。

节点通过 UU 设备列表发现对端：本账号下别名以 `uutier-` 开头、平台不是 Android、iOS 或 TV 的设备参与组网，最多三台，`uutier login` 注册的设备带这个标记，每台设备需要独立的 UU 设备 ID，在每台主机上分别执行 `uutier login`。设备按 UU 设备 ID 排序后分配 host 和 viewer 角色，UUTier 只操作带 `uutier-` 标记设备的房间；已保存的设备不带标记时，用 `uutier login --new-device` 注册新设备。

## 安装

从最新 Release 下载并安装：

```bash
curl -fsSL "https://github.com/Angels-Ray/UUTier/blob/main/install.sh?raw=true" | sudo bash -s install
```

脚本按架构和 C 库选择 `uutier-linux-<arch>-<libc>` 资产，校验 SHA256，安装后设置 `cap_net_admin`。通过 `sudo` 运行时安装到发起 `sudo` 的用户。脚本识别 systemd 或 OpenRC，安装系统服务。服务以该用户运行。配置存在时脚本启动服务；没有配置时只启用服务。`--no-service` 跳过服务安装。`install`、`update`、`uninstall`、服务管理和选项见 [部署](packaging/README.md)。

## 构建

构建需要 Rust 1.95 和 C 编译器。

```bash
cargo build --workspace --locked
cargo install --locked --path crates/uutier-cli
sudo setcap cap_net_admin+ep "$(command -v uutier)"
```

创建 TUN 需要 `CAP_NET_ADMIN`，重新安装二进制后要重新执行 `setcap`；多用户主机还要执行 `chmod 700 "$(command -v uutier)"`，避免其他用户执行这个带能力的二进制。

本机构建的产物链接本机 C 库。Release 的 gnu 资产在 Debian 10 上构建，可在 glibc 2.28 及更新的发行版上运行；Alpine 使用 musl 资产。目标发行版更旧时，直接在目标主机执行 `cargo build --release --locked`。

## 配置

```bash
install -d -m 700 "${XDG_CONFIG_HOME:-$HOME/.config}/uutier"
[ -e "${XDG_CONFIG_HOME:-$HOME/.config}/uutier/config.toml" ] || \
    install -m 600 config.example.toml "${XDG_CONFIG_HOME:-$HOME/.config}/uutier/config.toml"
${EDITOR:-vi} "${XDG_CONFIG_HOME:-$HOME/.config}/uutier/config.toml"
uutier login
uutier config check
uutier doctor
```

`network.secret` 用 `openssl rand -hex 32` 生成，同一网络的所有节点使用同一个值。`uutier login` 把凭据写入 `uu.json`，路径由 `[uu] path` 指定，未指定时用 `$XDG_CONFIG_HOME/uutier/uu.json`，`XDG_CONFIG_HOME` 未设置或不是绝对路径时用 `~/.config/uutier/uu.json`；`uutier doctor` 检查本机设备是否带 `uutier-` 标记、标记设备是否超过三台，并显示在线数量。

启动节点：

```bash
uutier up
```

字段说明见 [配置](docs/configuration.md)，连通性排查见 [连通性](docs/connectivity.md)。

## 部署

部署步骤见 [packaging/README.md](packaging/README.md)，包括安装脚本、systemd 单元和 OpenRC 服务。用 `sudo uutier up` 启动时，root 的 `HOME` 下没有配置和凭据，节点无法启动。

## 限制

只支持 Linux 和 IPv4，需要 `/dev/net/tun`、`tun` 模块和 `CAP_NET_ADMIN`；TUN 设备名固定为 `uutier0`，同一主机运行多个节点需要不同的 network namespace，以及不同的 `XDG_CONFIG_HOME` 和 `XDG_STATE_HOME`；TURN 只使用 UDP，UDP 出站被阻断时无法建立路径。

## 隐私

日志和状态文件不记录 token、TURN 凭据、二维码 URL 和完整公网地址。凭据文件、身份密钥和状态文件的权限是 0600，所在目录是 0700。

## 文档

- [架构](docs/architecture.md)
- [配置](docs/configuration.md)
- [连通性](docs/connectivity.md)
- [线上协议](docs/protocol.md)
- [部署](packaging/README.md)

## 开发

- [开发记录](docs/dev/roadmap.md)
- [实机测试脚本](scripts/lab/README.md)

## 依赖与参考实现

完整依赖见 `Cargo.lock`，许可证用 `cargo deny check licenses` 检查。

| 组件 | 用途 | 许可 |
|---|---|---|
| uu-remote-linux | UU 登录和 TURN 协议参考 | GPL-3.0-or-later |
| EasyTier | 组网设计参考 | LGPL-3.0 |
| `snow` | Noise 协议 | Apache-2.0 OR MIT |
| `x25519-dalek` | X25519 密钥 | BSD-3-Clause |
| `blake2` | peer ID 和 PSK 哈希 | MIT OR Apache-2.0 |
| `tun` | Linux TUN 设备 | WTFPL |
