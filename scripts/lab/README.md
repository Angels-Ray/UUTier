# 实机测试

脚本用本机和远端 `user@host` 测试自动发现、直连和 UU 云 TURN，远端地址通过 `UUTIER_LAB_REMOTE` 指定，三台设备使用同一个 UU 账号。

## 条件

- 本机和远端为 x86_64 Linux，远端允许 ssh 和 sudo。
- 本机有一个供 B 使用的 netns，地址为 `10.200.0.2`。
- 两台主机放行 UDP 12010，本机另放行 12011。
- 不需要预先安装 uu-remote。登录使用的 UU 账号需要能接收扫码确认。
- 参与组网的设备别名以 `uutier-` 开头，最多三台。其他 uu-remote 设备保持原样。
- 远端不需要 Rust；脚本推送本机构建的二进制。

## 登录

每台设备注册一次，各扫码一次，登录顺序不限。

远端设备 C：

```bash
UUTIER_LAB_REMOTE=user@host ./scripts/lab/remote.sh login
```

本机设备 A 和 B：

```bash
./scripts/lab/login-local.sh
```

脚本把凭据放在独立目录，与 uu-remote 的 `config.json` 分开：

| 设备 | 凭据目录 |
|---|---|
| A | `target/lab/a/home/.config/uutier/uu.json` |
| B | `target/lab/b/home/.config/uutier/uu.json` |
| C | 远端 `~/.config/uutier/uu.json` |

已经登录过时，脚本跳过。需要注册新设备时给脚本加 `--new-device`。一个网络最多三台带标记的设备；超过时在 UU 远程应用里解绑旧设备。

## 运行

终端 1，远端 C：

```bash
UUTIER_LAB_REMOTE=user@host ./scripts/lab/remote.sh
```

终端 2，本机 A：

```bash
./scripts/lab/local.sh
```

终端 3，netns 中的 B：

```bash
sudo ./scripts/lab/netns-up.sh
./scripts/lab/b.sh
```

`netns-up.sh` 开启 `net.ipv4.ip_forward` 并添加 NAT 规则，`netns-down.sh` 恢复原来的值并删除规则。

本机 A 和远端 C 在同一局域网，直连候选可达；B 在 netns 内只看到 `10.200.0.2`，对端无法连到这个地址，所以 A-B 和 B-C 应走 TURN。

## 验证

本机 A：

```bash
ping -c 3 10.144.0.3
ping -c 3 10.144.0.4
sudo env HOME="$PWD/target/lab/a/home" XDG_CONFIG_HOME="$PWD/target/lab/a/home/.config" \
    ./target/release/uutier status
```

远端 C：

```bash
ssh user@host
sudo env HOME="$HOME/uutier-lab" XDG_CONFIG_HOME="$HOME/.config" "$HOME/uutier-lab/uutier" status
```

B：

```bash
sudo ip netns exec uutier-b env HOME="$PWD/target/lab/b/home" \
    XDG_CONFIG_HOME="$PWD/target/lab/b/home/.config" ./target/release/uutier status
```

三端 `status` 都显示另外两个 peer，A-C 显示 `path=direct`，A-B 和 B-C 显示 `path=turn`；`turn.allocation` 为 `active`，`turn.refreshed_unix` 随 240 s 刷新增长，对端退出后 30 s 内 peer 从 `status` 消失。

## 强制 A-C 走 TURN

直连建立后再阻断 UDP 不会切换路径；要测试回退，在启动节点之前加规则：

```bash
REMOTE_IP=192.0.2.10  # 替换为远端地址
sudo iptables -I OUTPUT -d "$REMOTE_IP" -p udp --dport 12010 -j DROP
sudo iptables -I INPUT -s "$REMOTE_IP" -p udp --sport 12010 -j DROP
```

然后启动 A 和 C。直连探测 5 s 超时后，A-C 的 `status` 显示 `path=turn`。

测试结束删除规则：

```bash
sudo iptables -D OUTPUT -d "$REMOTE_IP" -p udp --dport 12010 -j DROP
sudo iptables -D INPUT -s "$REMOTE_IP" -p udp --sport 12010 -j DROP
```

## 文件

脚本生成的文件都在 `target/lab`：

| 路径 | 内容 |
|---|---|
| `secret` | 随机网络密钥，三端配置共用 |
| `a/home` | A 的凭据、身份和状态文件 |
| `a/config.toml` | A 的配置 |
| `b/home` | B 的凭据、身份和状态文件 |
| `b/config.toml` | B 的配置 |
| `remote-config.toml` | 推送到远端的配置 |

远端文件在 `~/uutier-lab`。远端状态文件在 `~/uutier-lab/.local/state/uutier/status.json`。

## 清理

三个终端各按一次 Ctrl+C，然后：

```bash
sudo ./scripts/lab/netns-down.sh
sudo ip link delete uutier0 2>/dev/null || true
```

`target/lab` 中的凭据、身份密钥和网络密钥保留，下次运行脚本继续使用；不再需要时手动删除该目录。

远端残留的 `uutier0`：

```bash
ssh user@host 'sudo ip link delete uutier0'
```

这些脚本只操作 UUTier 的进程和带标记设备，`uuremote-native.service` 保持原样。
