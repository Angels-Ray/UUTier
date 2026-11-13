# 连通性

## 路径优先级

节点按 peer 选择路径，同一节点上有的 peer 走直连，有的走 TURN：

1. 直连：节点之间直接收发 UDP。
2. UU 云 TURN：直连失败时，本地 bridge 经 UU TURN 把 UDP 数据送到对端。

## 直连

peer 来自 UU 设备列表，本账号下别名以 `uutier-` 开头、平台不是 Android、iOS 或 TV 的设备参与组网，最多三台；节点通过 UU 信令得到对端的候选地址，每 5 s 向候选地址发起 Noise 握手，同时接受对端发起的握手。

host 和 viewer 角色随标记设备数量变化，见 [配置](configuration.md) 的设备发现一节；一个 host 房间同时只接受一个 viewer，房间已有 viewer 时，第二个 viewer 收到错误码 2002，并按退避重试。

握手成功后，节点从认证过的帧记下对端的来源地址，`uutier status` 显示 `path=direct`；对端更换地址或 NAT 映射后，节点自动使用新地址。

对端 30 s 内没有发出认证帧时，节点清除会话，`uutier status` 不再显示该 peer，之后每 5 s 重试握手。

直连是否成功取决于双方的 NAT 类型和防火墙：双方都在 NAT 后时，需要至少一端在路由器上转发 `listen_port` 的 UDP 流量；对称 NAT 下通常无法直连，这类网络使用 TURN 路径。UDP 出站被完全阻断时，直连和 TURN 都不可用。

## TURN 路径

TURN 路径使用 UDP，每个节点为每个 peer 申请一个 TURN 分配，得到公网 relay 地址；节点交换 relay 地址后，各自启动一个本地 UDP bridge：

- bridge 监听 `127.0.0.1:0`。
- 组网层把 bridge 地址当作对端端点。
- bridge 用 TURN Send/Data indication 发到对端 relay 地址。
- 对端 bridge 把数据转给本机组网层。

UU 云下发的 TURN 分配有效期是 600 s；bridge 按有效期的一半刷新，间隔限制在 30 s 到 240 s 之间，所以每 240 s 刷新一次。刷新失败时 bridge 停止，有直连端点时节点回退到直连。UU 信令或分配失败时，错误写入 `uutier status` 的 `turn.last_error`；有 bridge 在运行时 `turn.allocation` 为 `active`，没有 bridge 且 `turn.last_error` 有内容时为 `failed`，其余情况为 `inactive`。使用 TURN 的 peer 在 `uutier status` 显示 `path=turn`。

TURN 请求每 500 ms 重传同一事务，直到 `turn.request_timeout_s` 到期；响应成功时节点校验 MESSAGE-INTEGRITY，bridge 退出时发送 lifetime 为 0 的 Refresh 请求，让服务器尽快回收分配。

## MTU

TURN 头占 36 byte 到 39 byte，其中 0 到 3 byte 是数据属性的 4 byte 对齐填充；线上头部 16 byte，数据帧 nonce 8 byte，Noise 认证标签 16 byte，UDP/IPv4 头 28 byte，所有开销合计 104 byte 到 107 byte。

默认 `mtu_bytes = 1280`，加上开销后最多 1387 byte，低于常见链路的 1500 byte。

直连路径可以把 `mtu_bytes` 调到链路 MTU 减去直连开销；调高后如果链路不支持，会出现分片或丢包。

## 状态

状态文件每 2 s 更新一次，超过 5 s 未更新时 `uutier status` 认为它过期，字段和路径见 [架构](architecture.md) 的状态一节。

`peers` 只列出已建立会话的 peer，`mesh_rtt_us` 是会话层测量的 RTT，`turn_rtt_us` 是 `path=turn` 时的会话层 RTT，其他路径为空。

## 失败模式

| 现象 | 检查 |
|---|---|
| `uutier status` 返回 1，提示状态文件不存在 | 节点没有运行，或 `XDG_STATE_HOME` 不一致 |
| `uutier up` 返回 `tun error: Operation not permitted` | 进程没有 `CAP_NET_ADMIN`。用户单元按 README 的构建一节执行 `setcap`；系统单元检查 `AmbientCapabilities` |
| `uutier doctor` 报 `missing CAP_NET_ADMIN` | 用户单元按 README 的构建一节执行 `setcap`；系统单元的 TUN 权限由 `AmbientCapabilities` 提供，登录会话里的这项检查失败不影响服务 |
| `uutier doctor` 报 `uutier0 already exists` | 已有节点在运行，先停止该进程 |
| `uutier up` 返回 `tun error: ... another process is using uutier0` | 停止占用 `uutier0` 的旧节点 |
| `uutier up` 返回 `tun error: ... load the tun module` | 加载 `tun` 模块，检查 `/dev/net/tun` 是否存在 |
| `uutier up` 返回 `cannot access credentials file ...` | 文件不存在时执行 `uutier login` |
| `uutier up` 返回 `credentials file ... is accessible by group or others` | 对提示的路径执行 `chmod 600` |
| `uutier up` 返回 `credentials directory ... is accessible by group or others` | 对提示的目录执行 `chmod 700` |
| 状态文件里没有某个 peer | `network.name`、`network.secret` 是否与对端一致；对端是否在 UU 设备列表里 |
| peer 一直没有会话 | 防火墙是否放行 `listen_port` 的 UDP，路由器是否转发该端口；双方公布的候选地址是否可达 |
| 一对 peer 都没有会话 | 两端是否都带 `uutier-` 标记，标记设备是否超过三台；`uutier doctor` 的输出 |
| 日志出现 `UU host room already has a viewer` | host 房间已有 viewer。viewer 收到 2002 后自动退避重试；持续出现时检查是否有旧进程占用房间 |
| `turn.last_error` 有错误 | UU 凭据是否过期，两端是否登录同一个 UU 账号 |
| UDP 出站被阻断 | 直连和 TURN 都需要放行 UDP 出站；这类网络无法建立路径 |

## 隐私

日志和状态文件不记录 token、TURN 用户名、TURN 密码、二维码 URL、完整公网地址；凭据文件、身份密钥和状态文件的权限是 0600，所在目录是 0700。凭据目录已存在且组或其他用户可访问时，读取和写入都会报错；配置文件包含 `network.secret`，组或其他用户可读时，启动日志里会有警告。
