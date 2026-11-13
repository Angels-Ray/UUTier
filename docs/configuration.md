# 配置

配置文件路径：`$XDG_CONFIG_HOME/uutier/config.toml`。`XDG_CONFIG_HOME` 未设置或不是绝对路径时，使用 `~/.config/uutier/config.toml`。用 `--config` 或 `UUTIER_CONFIG` 覆盖。

配置文件包含 `network.secret`，权限应为 0600。组或其他用户可读时，加载会在 stderr 给出警告。

## 示例

```toml
[uu]
# path = "/path/to/uu.json"

[turn]
enabled = true
mode = "direct-first"
request_timeout_s = 5

[network]
name = "uutier"
secret = ""                # 必填，至少 16 byte，用 openssl rand -hex 32 生成
ipv4 = "10.144.0.2"
mtu_bytes = 1280
listen_port = 12010
```

## 设备发现

`uutier login` 注册设备时，别名以 `uutier-` 开头，例如 `uutier-laptop`。UUTier 只使用属于当前 UU 账号、别名以 `uutier-` 开头、平台不是 Android、iOS 或 TV 的设备，只创建或加入这些设备的房间。

一个网络最多三台带标记的设备，`uutier doctor` 显示标记设备数量。启动时发现第四台，`uutier up` 报错并退出；运行中出现第四台时，节点中止现有信令任务，错误写入状态文件的 `turn.last_error`，设备数恢复到三台后重新建立信令。设备离线时保留排序位置，上线后自动恢复信令，发现循环每 10 s 读取一次设备列表。

设备按 UU 设备 ID 排序后分配信令角色：

| 标记设备数 | 本机角色 |
|---|---|
| 1 | 只读取设备列表 |
| 2 | ID 较大的一方创建 host 房间，较小的一方加入 |
| 3 | 每台为下一台创建 host 房间，并加入上一台的 host 房间 |

一台设备只创建一个 host 房间，host 只处理下一台设备的 offer，viewer 加入失败时按 2 s 起步、最长 30 s 的退避重试。

参与组网的每台设备需要独立的 UU 设备 ID，每台主机分别执行 `uutier login`，`uu.json` 不能复制到另一台主机。

## `[uu]`

| 字段 | 类型 | 默认值 | 含义 |
|---|---|---|---|
| `path` | path | 无 | 凭据文件路径，必须是绝对路径 |

不填 `path` 时，`XDG_CONFIG_HOME` 已设置为绝对路径则读取 `$XDG_CONFIG_HOME/uutier/uu.json`，否则读取 `~/.config/uutier/uu.json`；填了 `path` 时只读取这个文件，`uutier login`、`uutier logout`、`uutier up`、`uutier doctor` 都使用这个路径。

`uutier login` 和 `uutier logout` 只使用 `[uu]`，不校验 `[network]` 和 `[turn]`，但配置文件仍需能解析。

`uutier login` 在终端显示二维码，扫码后把凭据写入 `uu.json`，权限 0600；已保存的设备别名不带 `uutier-` 时，用 `uutier login --new-device` 在同一个 `uu.json` 里注册新设备。

加载时拒绝组或其他用户可访问的凭据文件，所在目录也要只有所有者可访问；目录带组或其他用户权限位时，读取和写入都报错。

## `[turn]`

| 字段 | 类型 | 默认值 | 含义 |
|---|---|---|---|
| `enabled` | bool | `true` | 是否启用 TURN 路径 |
| `mode` | `direct-first`、`always` | `direct-first` | 何时使用 TURN |
| `request_timeout_s` | integer | `5` | TURN 请求超时，单位 s，范围 1 到 3600 |

TURN 服务器地址和凭据由 UU 云下发；启用 TURN 时，每个节点为每个 peer 建立一个 TURN 分配，TURN 只使用 UDP 服务器。

`mode` 对每个 peer 单独判定：

| `mode` | 行为 |
|---|---|
| `direct-first` | 先向信令记录里的候选地址发握手。5 s 内完成握手的 peer 使用直连，其余 peer 启动 TURN bridge。 |
| `always` | 不公布也不使用直连候选，收到 UU 信令记录后直接启动 TURN bridge。 |

`always` 适合已知直连不可用的网络，例如对端在对称 NAT 后；`enabled = false` 时不启动 TURN，`mode = "always"` 要求 `enabled = true`。

## `[network]`

| 字段 | 类型 | 默认值 | 含义 |
|---|---|---|---|
| `name` | string | `uutier` | 网络名，不能为空 |
| `secret` | string | 无 | 网络密钥，必填，至少 16 byte |
| `ipv4` | string | 无 | 本机虚拟 IPv4 地址，必填 |
| `mtu_bytes` | integer | `1280` | MTU，单位 byte，范围 64 到 65427 |
| `listen_port` | integer | `12010` | UDP 监听端口，必须大于 0 |

`name` 和 `secret` 相同的节点属于同一网络；`secret` 经 BLAKE2s-256 派生为 Noise 握手使用的 32 byte PSK，`name` 和 `secret` 一起派生网络标识，网络标识随信令记录发送。PSK 或网络标识不同的节点不会互通，短于 16 byte 的密钥可以被离线字典攻击还原。

所有节点的 `ipv4` 使用同一个 /24 网段，例如 `10.144.0.0/24`，TUN 的掩码固定为 255.255.255.0。

`mtu_bytes` 默认 1280 byte，为 TURN 头留出空间；直连路径可以调高，但不超过链路 MTU，配置上限 65427 byte，来自 UDP 载荷和 TURN 开销。

防火墙需要放行 `listen_port` 的 UDP。

TUN 设备名固定为 `uutier0`，状态文件和身份密钥的路径也是固定的；同一主机运行多个节点需要不同的 network namespace，以及不同的 `XDG_CONFIG_HOME` 和 `XDG_STATE_HOME`。

## 校验

`uutier config check` 检查以下内容：

- 未知字段报错。
- `network.name`、`network.secret`、`network.ipv4` 非空，纯空白按空处理。
- `network.secret` 至少 16 byte。
- `network.ipv4` 是合法 IPv4 地址，且不是未指定地址、回环地址、组播地址、广播地址，最后一段不为 0 或 255。
- `network.mtu_bytes` 在 64 到 65427 byte 之间。
- `network.listen_port` 在 1 到 65535 之间。
- `uu.path` 是绝对路径。
- `turn.request_timeout_s` 在 1 到 3600 s 之间。
- `turn.mode` 为 `always` 时 `turn.enabled` 必须是 `true`。

退出码：0 通过，2 配置错误。`uutier config show` 输出脱敏配置，`network.secret` 替换为 `***`。
