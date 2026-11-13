# 路线图

开发记录。最后更新：2026-11-13。

## 现状

| 能力 | 状态 |
|---|---|
| 配置解析、校验、脱敏输出 | 完成 |
| 凭据类型、存储、权限检查 | 完成 |
| 状态文件读写 | 完成 |
| CLI 参数、日志、退出码 | 完成 |
| 组网参数派生 | 完成 |
| 帧编解码 | 完成 |
| 身份密钥和 `uutier key show` | 完成 |
| Noise 握手和会话 | 完成 |
| TUN 设备创建 | 完成，运行需要 root 或 `CAP_NET_ADMIN` |
| 动态 peer 表 | 完成 |
| 直连握手、保活、超时和重连 | 完成 |
| `uutier up` 节点运行时 | 完成 |
| `uutier status` 的 peer 输出 | 完成 |
| UU 扫码登录 | 完成 |
| 按 `uutier-` 标记发现设备，最多三台 | 完成；两台设备的自动发现已于 2026-11-13 实机验收 |
| UU 信令自动发现 | 完成 |
| TURN 分配和 bridge | 完成 |
| `uutier doctor` 的 UU API 检查 | 完成 |
| `uutier doctor` 的 TURN 分配和端口检查 | 未实现 |

workspace 不依赖 EasyTier，也不包含从 EasyTier 复制或移植的代码。

## M0 信令实验

要回答的问题：

1. 没有 viewer 连接时，能否取得 UU 云 TURN 凭据。
2. 两个 UUTier 节点能否通过 UU 信令交换候选记录，包括公钥、虚拟 IPv4、监听端口、公网端点和 TURN relay 地址。
3. UU 云 TURN 是否允许向任意对端地址转发。

方法：

1. 用官方客户端或 uu-remote 连接一次，触发被控端会话，记录信令消息的字段号。不在日志中输出凭据。
2. 用取得的凭据向一个公网 UDP 回显服务创建分配并发送数据。
3. 检查 `room/create` 和 `be-controlled` 能否在不建立 viewer 会话时返回 ICE 服务器。

参考代码：uu-remote-linux 仓库的 `src/signaling.rs`、`src/main.rs`、`src/session_ice.rs`。只参考协议行为，不复制代码。

静态分析结论见 [spikes/uu-signaling.md](spikes/uu-signaling.md)。用真实账号和第二台设备验证后，三个问题都通过：两个节点取得 TURN 凭据、交换记录，并经 UU 云 TURN 转发数据。TURN 因此使用 UU 云服务，M4 的自动发现也依赖记录交换。

## M1 骨架

创建 workspace、四个 crate、配置和状态类型、CLI 参数。`uutier --help`、`uutier config check`、`uutier config show` 可用。

验收：

```bash
./scripts/check.sh
```

## M2 虚拟网卡和线上协议

实现 TUN 设备创建、IP 包读写、帧编解码、身份密钥、Noise 握手和会话。在本机用两个 UDP socket 验证握手和数据帧。

状态：完成。握手测试见 `crates/uutier-node/tests/handshake.rs`。

验收：

- 帧编解码的单元测试覆盖所有 kind 和非法输入。
- 两个 UDP socket 完成握手，互相发送加密数据帧。
- 错误密钥握手失败。
- 重放和乱序 nonce 的处理符合 [线上协议](../protocol.md)。
- TUN 设备创建需要 root 或 `CAP_NET_ADMIN`，在目标主机上手工验证。

## M3 直连

实现直连握手、端点学习、保活、超时和握手重试。`uutier up` 启动完整节点。`uutier status` 输出 peer、虚拟 IP、路径、RTT 和收发字节数。本阶段的 peer 表来自静态配置，M4 改为 UU 设备发现。

状态：完成。本机 UDP socket 测试见 `crates/uutier-node/tests/direct.rs`。两台主机联调通过：双向 ping、`path=direct`、对端退出后 30 s 内清除会话、重启后 5 s 内重连。

验收：

- 两台 Linux 主机通过静态 peer 配置得到对方的公钥和虚拟 IPv4，互相 ping 通。
- `uutier status` 显示 `path=direct`。
- 对端退出后，30 s 内会话被清除，`uutier status` 不再显示该 peer。
- 对端重启后，5 s 内重新完成握手。

## M4 UU 登录、设备发现、信令和 TURN

实现扫码登录、凭据存储、UU 设备发现、UU 信令的记录交换、TURN 分配和本地 UDP bridge。设备别名以 `uutier-` 开头才参与组网，最多三台；配置里没有 peer 表。

状态：部分完成。两台按设备标记自动发现的主机于 2026-11-13 实机验收：双方 `status` 显示 `path=direct`，双向 ping 通过。两台主机阻断直连 UDP 后，两端 `status` 显示 `path=turn`，双向 ping 通过；240 s 后 TURN 分配刷新，连接不中断。三台设备曾用旧版静态 peer 配置验证 A-B、B-C 和 TURN 回退。凭据过期只在代码路径中处理，未做过期测试。

验收：

- 阻断两台节点之间的直连 UDP 后，流量经 UU 云 TURN 到达对端。
- `uutier status` 显示 `path=turn`。
- UU 信令能交换公钥、虚拟 IPv4、UDP 端点和 TURN relay 地址。
- TURN 分配刷新后连接不中断。
- 凭据过期时给出明确错误。

## 已知限制

- 一个网络最多三台带 `uutier-` 标记的设备。启动时发现第四台，`uutier up` 报错；运行中超过三台时中止现有信令任务，错误写入状态文件。需要先从 UU 设备列表移除一台。
- 参与组网的设备必须使用独立的 UU 设备 ID。与 `uuremote-server` 共用设备 ID 会让 UUTier 进入 uuremote 的 host 房间，信令会断开。每台机器用 `uutier login` 注册专用设备。见 [spikes/uu-signaling.md](spikes/uu-signaling.md)。
- UU 房间同时只接受一个 viewer。同一 host 的第二个 viewer 收到错误码 2002，节点按退避重试。
- 离线设备保留环形拓扑中的位置。剩余两台仍能通过其中一条信令关系建立会话，但离线设备对应的 viewer 任务会持续重试。
- 按标记自动发现的三台环形拓扑未实机验收。
- TURN 只使用 UDP `turn:` URL，不支持 `turns:`/TLS。
- TURN bridge 刷新失败后不重建分配，节点回退到直连。
- 直连会话超时后，节点继续重试直连，不重建已释放的 TURN bridge。分配重建属于 M5。
- `uutier doctor` 不检查 TURN 分配和端口可达性。

## M5 加固

- 三台带标记设备的自动发现实机验收。
- 断线重连和分配重建。
- MTU 校验。
- 日志脱敏检查。
- 加固并验收 systemd 用户单元和示例配置。两者已存在，systemd 单元见 `packaging/systemd/`，示例配置见 `config.example.toml`。

验收：连续运行 24 h，无凭据泄漏，无未处理错误退出。
