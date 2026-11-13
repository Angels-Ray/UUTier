# 线上协议

## 帧格式

协议版本为 1。每个 UDP 数据报是一个帧。头部 16 byte，网络字节序：

| 偏移，单位 byte | 长度，单位 byte | 字段 | 取值 |
|---|---|---|---|
| 0 | 4 | magic | ASCII `UUT1` |
| 4 | 1 | version | 1 |
| 5 | 1 | kind | 见下表 |
| 6 | 2 | flags | 保留，值为 0 |
| 8 | 8 | session_id | 握手帧为接收方 peer ID，数据帧为会话标识 |

| kind | 值 | 载荷 |
|---|---|---|
| handshake_init | 1 | Noise IK 第一条消息 |
| handshake_resp | 2 | Noise IK 第二条消息 |
| data | 3 | 8 byte nonce，之后是 Noise 密文 |
| keepalive | 4 | 未使用 |
| ping | 5 | 8 byte nonce，之后是 Noise 密文 |
| pong | 6 | 8 byte nonce，之后是 Noise 密文 |

data 帧的明文是一个 IPv4 包，ping 和 pong 的明文是 8 byte 序号，密文末尾带 16 byte 认证标签，nonce 按大端编码；节点直接丢弃版本、magic 或 flags 不匹配的帧。

## 身份

每个节点首次启动时生成 X25519 静态密钥对并保存到 `$XDG_CONFIG_HOME/uutier/identity.key`，`XDG_CONFIG_HOME` 未设置或不是绝对路径时使用 `~/.config/uutier/identity.key`，目录权限 0700，文件权限 0600，加载时拒绝组或其他用户可访问的文件。

peer ID 取静态公钥 BLAKE2s-256 哈希的前 8 byte，按大端解释为 `u64`，`uutier status` 显示 peer ID，不显示公钥和完整地址。

## 握手

握手使用 Noise IKpsk2：

| 参数 | 取值 |
|---|---|
| DH | X25519 |
| 加密 | ChaCha20-Poly1305 |
| 哈希 | BLAKE2s |
| 预共享密钥 | BLAKE2s-256(`network.secret` 的 UTF-8 字节) |

PSK 固定为 32 byte，`network.secret` 至少 16 byte，双方用同一算法派生；PSK 不匹配时握手失败，节点丢弃该消息，不记录载荷内容。

握手载荷 6 byte：发送方的虚拟 IPv4 占 4 byte，UDP 监听端口占 2 byte，按大端编码，载荷在 Noise 消息内加密。

initiator 在握手帧的 `session_id` 填入 responder 的 peer ID，responder 在响应帧填入 initiator 的 peer ID；initiator 可以复用静态密钥发起多个会话。

## 会话

握手完成后，双方从 Noise 握手哈希的前 8 byte 得到同一个会话标识，数据帧的 `session_id` 使用该值。

`session_id` 之后是 8 byte nonce，从 0 开始，每发一帧加 1；接收方维护 64 帧的滑动窗口，接受未出现过的 nonce，拒绝重复的 nonce 和窗口之外的旧 nonce，nonce 耗尽时节点丢弃该帧，会话不重建。

一个 peer 最多保留两条已建立会话：本机作为发起方的一条、作为响应方的一条，另加一条待确认的响应方会话。发送时优先用最近收到认证帧的会话；没有这样的会话时，用发起方会话，再没有时用响应方会话。同一角色建立新会话时，节点替换旧会话。

## 保活和超时

会话建立后发送第一个 ping，之后每 10 s 发送一次，对端用 pong 回应同一个序号；节点每秒检查一次状态，已建立会话的 peer 连续 30 s 没有收到认证帧时，节点清除会话，之后继续每 5 s 重试握手。

状态文件中的 `mesh_rtt_us` 是 ping 和 pong 的往返时间，单位 µs。

## 发现

节点每 10 s 读取一次 `GET /api/v1/device/list`；参与组网的设备必须属于本账号、别名以 `uutier-` 开头、平台不是 Android、iOS 或 TV，设备按 `device_id` 字符串排序，最多三台。离线设备仍留在设备列表里，各节点看到相同的排序，角色分配不随在线状态变化。

| 设备数 | 信令关系 |
|---|---|
| 1 | 只读取设备列表 |
| 2 | ID 较大的一方创建 host 房间，较小的一方加入 |
| 3 | 每台为下一台创建 host 房间，并加入上一台创建的房间 |

host 调用 `room/create`，viewer 调用 `room/join/by_device`，一台设备只创建一个 host 房间，host 只处理下一台设备的 offer，其他 offer 丢弃。房间已有 viewer 时，第二个 `room/join/by_device` 返回 HTTP 400 code 2002；viewer 加入失败时按 2 s 起步、最长 30 s 的退避重试。

host 收到 offer 后，把 `uutier_record.uu_device_id` 和下一台设备比对，再按 offer 的 `client_id` 找到该 viewer 的 control 会话；`client_id` 缺失时按 `ice_id` 匹配，只有一个 viewer 时 host 直接使用该会话。记录放在 SOAC 数据的 `uutier_record` 字段：

| 字段 | 含义 |
|---|---|
| version | 记录版本，值为 2 |
| uu_device_id | 发送方设备 ID |
| network_id | 网络标识，BLAKE2s-256(name + 0x00 + secret)，64 个十六进制字符 |
| public_key | 发送方 X25519 公钥 |
| ipv4 | 发送方虚拟 IPv4 |
| listen_port | UDP 监听端口 |
| endpoints | 直连候选地址，最多 8 个 |
| relay_addr | TURN relay 地址，没有分配时为空 |

收到记录后，双方检查记录格式和 `network_id`；`uu_device_id` 不能等于本机设备 ID，虚拟 IPv4 要在本机 /24 内、不等于本机地址、最后一段不是 0 或 255，`relay_addr` 存在时必须是公网单播 IPv4。host 丢弃不通过的 offer，viewer 让本次尝试失败并按退避重试。

节点每 5 s 向候选地址发送 handshake_init，多个候选地址按顺序轮换；没有候选地址时，节点向 peer 的当前端点发送，当前端点是 TURN bridge 的本地地址时握手经 TURN 发出。节点收到该 peer 的 handshake_init 后，向来源地址回复 handshake_resp；回复使用的来源地址不写入 peer 端点，peer 端点只从 handshake_resp 和通过认证的帧更新。

节点只接受当前待完成握手的来源地址发来的 handshake_resp；握手成功后，data、ping 和 pong 帧都来自已认证的会话，来源地址可以更新 peer 的当前端点。

## TUN

TUN 设备名为 `uutier0`，地址是 `network.ipv4`，掩码固定为 255.255.255.0，MTU 是 `network.mtu_bytes`，所有节点使用同一个 /24 网段。

## TURN bridge

节点为每个 peer 链路向 UU 云 TURN 申请一个分配，分配绑定本机的一个 UDP socket，返回 relay 地址。

每个使用 TURN 路径的 peer 对应一个 bridge：

1. bridge 绑定 `127.0.0.1:0`。
2. 组网层把发往该 peer 的帧发到 bridge 的本地端口。
3. bridge 用 TURN Send indication 把帧发到对端 relay 地址。
4. TURN 服务器安装对端 relay 地址的权限后转发。
5. 对端 bridge 收到 Data indication，取出载荷，转给组网层。

bridge 只搬运 UDP 载荷，按分配有效期的一半刷新，间隔限制在 30 s 到 240 s 之间。

## MTU

线上开销：

| 项 | byte |
|---|---|
| 帧头 | 16 |
| 数据帧 nonce | 8 |
| Noise 认证标签 | 16 |
| UDP/IPv4 头 | 28 |
| TURN 头 | 36 到 39，含 0 到 3 byte 对齐填充 |

默认 TUN MTU 为 1280 byte，直连路径总长 1348 byte；TURN 路径总长 1384 byte 到 1387 byte，低于常见链路的 1500 byte。

## 安全

- 节点用 Noise 会话加密并认证所有 data、ping 和 pong 帧，网络密钥不匹配的节点无法完成握手。
- 数据帧的 `session_id` 与握手协商值不符时，节点丢弃该帧。
- data 帧的源地址必须是该 peer 的虚拟 IPv4，目的地址必须是本机虚拟 IPv4。
- 作为响应方建立的会话在收到认证帧之前不替换现有会话，重放的 handshake_init 不会中断已建立的会话。
- 节点拒绝重复的 nonce，丢弃认证失败的帧。
- TURN 用户名和密码只用于 TURN 认证，不进入组网层。
- 状态文件记录 peer ID、路径、RTT 和字节数；日志记录 peer ID、虚拟 IPv4 和路径，不记录密钥、公钥、完整地址和载荷。
