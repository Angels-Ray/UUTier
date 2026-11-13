# UU 信令

日期：2026-11-12

## 问题

1. UU 云 TURN 凭据在什么时点下发。
2. UU 信令能否作为两个 UUTier 节点之间的记录交换通道。
3. UU 云 TURN 是否允许向任意公网 UDP 地址转发。
4. 同一账号下如何识别运行 UUTier 的设备。

## 方法

用两个已登录 uu-remote 的设备实测。本机作为 host 创建房间并连接信令，远端设备作为 viewer 加入房间并发送 control。另用 STUN/TURN 客户端向 UU 云 TURN 申请分配、建立权限、发送 Send Indication 并读取 Data Indication。

日志和本记录只包含状态、字段名、计数和布尔值。不包含 token、TURN 用户名、TURN 密码、二维码、设备 ID、别名和本机地址。

参考源码：

- `src/auth.rs`
- `src/viewer/api.rs`
- `src/viewer/signal.rs`
- `src/viewer/rtc.rs`
- `src/signaling.rs`
- `src/session_ice.rs`
- `src/main.rs`

## 结果

### 房间和信令

host 调用签名的 `POST /api/v1/room/create`，请求体为 `{}`，返回 code 0。host 必须先连上信令，否则 viewer 的 `POST /api/v1/room/join/by_device/{device}` 返回 HTTP 400 code 2001。viewer 请求体为 `{"force_join":false}`。

连接使用 Engine.IO 4 加 Socket.IO，URL 为 `{server}/socket.io/?transport=websocket&EIO=4`，房间 token 放在 `X-NRD-AUTH`。

viewer 发送控制事件后，host 收到 `be-controlled`，二进制附件为 ConnectOptions，2 byte，字段为 `[(1, varint)]`。viewer 的 control ack 返回会话标识和 ICE 配置。

### 房间的 viewer 数量

A 创建 host 房间，C 先加入，B 再加入。B 的 `room/join/by_device` 返回 HTTP 400 code 2002。停止 C 后，B 的加入成功。uu-remote 的源码把 2002 解释为“被控端已有控制者”。一个 host 房间同时只接受一个 viewer。

### TURN 凭据

`room/create` 的响应没有 `iceServers`。凭据只在一次 viewer 控制会话中下发：

| 角色 | 事件 | 字段 |
|---|---|---|
| host | `be-controlled` | `iceServers`、`client_id`、`ice_id`、`app_control_id` |
| viewer | control ack | `iceServers`、`client_id`、`ice_id`、`code` |

一次会话返回 7 个 ICE 服务器项，包含 `stun`、`turn`、`turns`。其中 2 个 `turn:` URL 使用 `transport=udp`，2 个 `turns:` URL 使用 `transport=tcp`。

### SOAC 记录

viewer 在 `42["soac", {"client_id": ..., "data": {...}}]` 的 `data` 中放入 `type`、`sdp`、`ice_id`、`app_control_id`，另加自定义字段 `uutier_record`。host 原样收到该字段，键和值都没有变化。host 用 `{"client_id": ..., "data": {...}}` 回复 answer，viewer 也原样收到自定义字段。

结论：`soac` 可以作为 UUTier 的记录交换通道。服务器只按会话路由，不解析 `data` 内容。消息大小上限未测。

### TURN 数据面

两个 `turn:` URL 都能在 20 ms 到 33 ms 内完成分配，lifetime 为 600 s，CreatePermission 成功。

向 DNS 服务器 `223.5.5.5:53` 发送 DNS 查询，Data Indication 返回 61 byte，事务 ID 匹配，来源地址匹配。这证明 TURN 允许向任意公网 UDP 地址转发。

用同一组凭据建立两个分配：

- 同一 TURN 服务器：两个 relay 地址的 IP 相同，双向发送 16 byte 数据都到达，来源匹配。
- 不同 TURN 服务器：两个 relay 地址的 IP 不同，双向发送 16 byte 数据都到达，来源匹配。

结论：每对 peer 各自申请分配、互相发送 relay 地址即可。同一服务器上的多个分配也能互通。

### 设备列表

`GET /api/v1/device/list` 返回 code 0，`data` 的键为 `current_device`、`my_binded_devices`、`others_shared_devices`。本次账号有 3 个绑定设备。设备项包含 `device_id`、`alias`、`platform`、`status` 和 `publisher_availability_status` 等字段。

列表包含手机和其他没有运行 UUTier 的设备。UUTier 不能把列表中的每个设备都当作成员。

## 对 UUTier 的影响

- 设备列表用于发现成员：本账号、别名以 `uutier-` 开头、平台为桌面的设备，最多三台。配置里没有 peer 表。
- 两台设备时 ID 较大的一方创建 host 房间；三台设备时每台为下一台创建房间，并加入上一台创建的房间。一个 host 房间同时只接受一个 viewer；房间已有 viewer 时，第二个 viewer 收到 2002 并按退避重试。
- 会话建立后，双方各自申请 TURN 分配，通过 `soac` 交换 `public_key`、`ipv4`、`listen_port`、`endpoints` 和 `relay_addr`。记录里还有 `network_id`，用于区分同一账号下的不同网络。
- TURN 路径每条信令会话一个本地 bridge。bridge 收到 TUN 帧后封装为 Send Indication，发到对端 relay 地址；Data Indication 解封装后送回组网层。
- TURN 只使用 UDP `turn:` URL。`turns:`/TLS 未实现。

### 与 uuremote-server 的房间冲突

共用设备 ID 时，运行 `uuremote-server` 的设备作为 host，对端调用 `room/join/by_device/{device_id}` 会进入该设备已有的 uuremote 房间，而不是 UUTier 新建的房间。`uuremote-server` 收到 UUTier 的 SOAC offer 后关闭 WebRTC，信令随即断开。实测 host 房间每 3 s 左右被断开并重建。

UUTier 现在为每台设备使用独立的带标记设备 ID，不进入 uuremote 的房间。本次实机测试的远端运行 `uuremote-native.service`，UUTier 没有停止或修改该服务。

## 未验证

- `soac` 消息大小上限。
- TURN 分配和 UU 会话的时长上限，token 过期后的行为。
- `turns:`/TLS 的实现和认证方式。
- 长时间运行（超过 10 min）下的分配和权限刷新。
