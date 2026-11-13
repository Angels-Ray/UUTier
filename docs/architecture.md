# 架构

UUTier 把几台 Linux 主机连成一个虚拟网络，节点之间优先直连，直连不通时用 UU 云 TURN 转发 UDP 数据。

## 组件

| Crate | 职责 |
|---|---|
| `uutier-core` | 配置、凭据类型、状态类型、错误 |
| `uutier-turn` | UU 扫码登录、凭据存储、TURN 分配、本地 UDP bridge |
| `uutier-node` | 组网层参数、虚拟网卡、peer 会话、节点生命周期、状态文件 |
| `uutier-cli` | 参数解析、日志、命令输出 |

依赖方向：`uutier-core` <- `uutier-turn` <- `uutier-node` <- `uutier-cli`。`MeshParams` 从 `[network]` 派生，`uutier-node` 用它创建 TUN 设备和 UDP 监听。

## 数据流

1. 组网层从 TUN 读取 IP 包。
2. 按包的目的 IPv4 查 peer 表，得到对应的 peer。
3. 用该 peer 的 Noise 会话加密包。
4. 帧加上 16 byte 线上头部、8 byte nonce 和 16 byte 认证标签，从 UDP socket 发出。直连时发到对端端点，TURN 路径发到本地 bridge。

解封装后的包写回 TUN，内核再把它交给本地进程；帧格式和握手流程见 [线上协议](protocol.md)。

## 发现和对端

节点启动时读取 UU 设备列表，筛选本账号下别名以 `uutier-` 开头、平台不是 Android、iOS 或 TV 的设备，按 UU 设备 ID 排序，最多三台。两台设备时 ID 较大的一方创建 host 房间，较小的一方加入；三台时每台为下一台创建 host 房间，并加入上一台创建的房间。一台设备只创建一个 host 房间，host 只处理下一台设备的 offer，viewer 加入失败时按 2 s 起步、最长 30 s 的退避重试。

发现循环每 10 s 重新读取设备列表；标记设备超过三台时，节点中止现有信令任务，错误写入状态文件，设备数回到三台以内后重新建立信令。

双方通过 UU 房间的 `soac` 消息交换 `uutier_record`，内容为公钥、虚拟 IPv4、监听端口、直连候选地址和 TURN relay 地址。收到记录后，节点校验网络标识、设备 ID 和虚拟网段，校验通过后把该 peer 加入 peer 表；标记设备从列表里消失时立即移除该 peer，设备离线但仍保留在列表里时，会话在 30 s 后超时移除。

创建或加入房间前，节点检查设备标记：UUTier 只操作带 `uutier-` 标记设备的房间，运行 `uuremote-server` 的设备使用自己的别名和房间。为避免共用房间，每台参与组网的设备需要独立的 UU 设备 ID，由 `uutier login` 在本机注册。

节点每 5 s 向信令得到的候选地址发起握手；没有候选地址时向当前端点发起，TURN 路径下当前端点是本地 bridge。节点同时接受对端发起的握手，握手成功后从认证过的帧记下对端来源地址；对端更换地址或 NAT 映射后，后续帧发往新地址。

两个节点之间最多保留两条已建立会话：本机作为发起方的一条、作为响应方的一条，另加一条待确认的响应方会话。发送时优先用最近收到认证帧的会话；没有这样的会话时，用发起方会话，再没有时用响应方会话。

## 路径选择

| 路径 | 建立方式 |
|---|---|
| 直连 | 向 UU 信令得到的候选地址发送握手，之后从通过认证的帧记下对端端点 |
| TURN | 本地 bridge 绑定 `127.0.0.1:0`，经 UU 云 TURN 转发到对端 relay 地址 |

`turn.mode = "direct-first"` 先尝试直连，5 s 内未完成握手的 peer 改用 bridge；`turn.mode = "always"` 不公布也不使用直连候选，收到信令记录后直接使用 bridge。不同 peer 可以使用不同路径，状态文件记录每个 peer 实际使用的路径。

## 转发

每个节点与所有其他节点直接建立会话；目的 IPv4 在 peer 表中查不到时，节点丢弃该包；唯一的转发点是 UU 云 TURN，它只转发 UDP 数据报。

## 状态

节点每 2 s 写一次状态文件 `$XDG_STATE_HOME/uutier/status.json`，`XDG_STATE_HOME` 未设置或不是绝对路径时使用 `~/.local/state/uutier/status.json`，文件权限 0600，目录 0700。

状态文件字段：

| 字段 | 含义 |
|---|---|
| `schema_version` | 格式版本，值为 1 |
| `pid` | 节点进程号 |
| `updated_unix` | 写入时间，Unix 时间戳，单位 s |
| `peers[].peer_id` | peer 标识，64 位 |
| `peers[].ipv4` | peer 的虚拟 IPv4 |
| `peers[].path` | `direct` 或 `turn` |
| `peers[].mesh_rtt_us` | 会话层测量的 RTT，单位 µs |
| `peers[].turn_rtt_us` | `path=turn` 时的会话 RTT，单位 µs |
| `peers[].rx_bytes`、`tx_bytes` | 收发字节数 |
| `turn.enabled` | TURN 是否启用 |
| `turn.allocation` | `inactive`、`active` 或 `failed` |
| `turn.refreshed_unix` | 最近一次分配刷新时间，Unix 时间戳，单位 s |
| `turn.last_error` | 最近的 TURN 或信令错误，不包含凭据和完整公网地址 |

文件超过 5 s 未更新时，`uutier status` 认为节点未运行，返回退出码 1。

## 秘密

- UU token 和 csrf 只在内存和 `uu.json` 中出现，TURN 用户名和密码由 UU 云信令下发，只存在于内存。
- 身份私钥保存在 `$XDG_CONFIG_HOME/uutier/identity.key`，`XDG_CONFIG_HOME` 未设置或不是绝对路径时使用 `~/.config/uutier/identity.key`，权限 0600。
- 日志记录状态、计数、peer ID、虚拟 IPv4 和路径，不记录凭据、二维码 URL、完整公网地址。
