//! 直连路径的本机集成测试。
//!
//! 两个状态机通过真实的 UDP socket 交换握手、保活和加密数据包，不创建 TUN。

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use uutier_node::{Action, Identity, Mesh, MeshParams, PeerRecord};

const READ_TIMEOUT: Duration = Duration::from_secs(2);
const IPV4_A: [u8; 4] = [10, 144, 0, 2];
const IPV4_B: [u8; 4] = [10, 144, 0, 3];

fn params(ipv4: [u8; 4], listen_port: u16) -> MeshParams {
    MeshParams {
        name: "uutier".to_owned(),
        secret: "network-secret".to_owned(),
        ipv4: Ipv4Addr::from(ipv4),
        mtu_bytes: 1280,
        listen_port,
    }
}

fn mesh(
    identity: &Identity,
    ipv4: [u8; 4],
    listen_port: u16,
    peer: &Identity,
    peer_ipv4: [u8; 4],
    peer_addr: SocketAddr,
    now: Instant,
) -> Mesh {
    Mesh::new(
        params(ipv4, listen_port),
        identity.clone(),
        vec![PeerRecord::new(
            *peer.public_key(),
            Ipv4Addr::from(peer_ipv4),
            Some(peer_addr),
        )],
        now,
    )
    .unwrap()
}

fn send_actions(socket: &UdpSocket, actions: Vec<Action>) -> Vec<Vec<u8>> {
    let mut delivered = Vec::new();
    for action in actions {
        match action {
            Action::Send { to, datagram } => {
                socket.send_to(&datagram, to).unwrap();
            }
            Action::Deliver(packet) => delivered.push(packet),
            Action::PeerUp { .. } | Action::PeerDown { .. } => {}
        }
    }
    delivered
}

fn receive(socket: &UdpSocket, buf: &mut [u8]) -> (usize, SocketAddr) {
    socket.recv_from(buf).unwrap()
}

fn ipv4_packet(source: [u8; 4], destination: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let total = 20 + payload.len();
    let mut packet = vec![0u8; total];
    packet[0] = 0x45;
    packet[2..4].copy_from_slice(&u16::try_from(total).expect("packet fits u16").to_be_bytes());
    packet[8] = 64;
    packet[9] = 17;
    packet[12..16].copy_from_slice(&source);
    packet[16..20].copy_from_slice(&destination);
    packet[20..].copy_from_slice(payload);
    packet
}

#[test]
fn two_nodes_handshake_ping_and_exchange_packets() {
    let socket_a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let socket_b = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket_a.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    socket_b.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let addr_a = socket_a.local_addr().unwrap();
    let addr_b = socket_b.local_addr().unwrap();

    let identity_a = Identity::from_secret_bytes([1u8; 32]);
    let identity_b = Identity::from_secret_bytes([2u8; 32]);
    let start = Instant::now();
    let mut mesh_a = mesh(
        &identity_a,
        IPV4_A,
        addr_a.port(),
        &identity_b,
        IPV4_B,
        addr_b,
        start,
    );
    let mut mesh_b = mesh(
        &identity_b,
        IPV4_B,
        addr_b.port(),
        &identity_a,
        IPV4_A,
        addr_a,
        start,
    );

    // A 发起握手，B 回应。
    let actions = mesh_a.tick(start);
    assert!(send_actions(&socket_a, actions).is_empty());
    let mut buf = [0u8; 2048];
    let (len, from) = receive(&socket_b, &mut buf);
    let init = buf[..len].to_vec();
    let actions = mesh_b.handle_datagram(start, from, &buf[..len]);
    // 应答方在收到认证帧之前不建立会话，重放的握手 init 不会拆掉现有会话。
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::PeerUp { .. }))
    );
    assert!(send_actions(&socket_b, actions).is_empty());

    let (len, from) = receive(&socket_a, &mut buf);
    let actions = mesh_a.handle_datagram(start, from, &buf[..len]);
    assert!(actions.iter().any(
        |action| matches!(action, Action::PeerUp { peer_id } if *peer_id == identity_b.peer_id())
    ));
    assert!(send_actions(&socket_a, actions).is_empty());

    let status_a = mesh_a.peer_status();
    let status_b = mesh_b.peer_status();
    assert_eq!(status_a.len(), 1);
    assert!(status_b.is_empty());
    assert_eq!(status_a[0].ipv4.as_deref(), Some("10.144.0.3"));
    assert_eq!(status_a[0].path.as_str(), "direct");

    // A 每 10 s 发 Ping，B 回 Pong，A 得到 RTT。B 收到认证帧后建立会话。
    let ping_at = start + Duration::from_secs(10);
    let actions = mesh_a.tick(ping_at);
    assert!(send_actions(&socket_a, actions).is_empty());
    let (len, from) = receive(&socket_b, &mut buf);
    let actions = mesh_b.handle_datagram(ping_at, from, &buf[..len]);
    assert!(actions.iter().any(
        |action| matches!(action, Action::PeerUp { peer_id } if *peer_id == identity_a.peer_id())
    ));
    assert!(send_actions(&socket_b, actions).is_empty());
    assert_eq!(mesh_b.peer_status().len(), 1);
    let (len, from) = receive(&socket_a, &mut buf);
    let pong_at = ping_at + Duration::from_millis(1);
    let _ = mesh_a.handle_datagram(pong_at, from, &buf[..len]);
    assert_eq!(mesh_a.peer_status()[0].mesh_rtt_us, Some(1000));

    // 重放握手 init 不建立新会话，也不影响已认证的会话。
    let actions = mesh_b.handle_datagram(ping_at, addr_a, &init);
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::PeerUp { .. }))
    );
    assert!(send_actions(&socket_b, actions).is_empty());
    let (len, from) = receive(&socket_a, &mut buf);
    assert!(
        mesh_a
            .handle_datagram(ping_at, from, &buf[..len])
            .is_empty()
    );
    assert_eq!(mesh_b.peer_status().len(), 1);

    // A 向 B 的虚拟 IPv4 发送一个 IP 包。
    let packet = ipv4_packet(IPV4_A, IPV4_B, b"hello");
    let actions = mesh_a.handle_ip(&packet);
    assert!(send_actions(&socket_a, actions).is_empty());
    let (len, from) = receive(&socket_b, &mut buf);
    let actions = mesh_b.handle_datagram(pong_at, from, &buf[..len]);
    let delivered = send_actions(&socket_b, actions);
    assert_eq!(delivered, vec![packet]);
    assert_eq!(mesh_b.peer_status()[0].rx_bytes, 25);
    assert_eq!(mesh_a.peer_status()[0].tx_bytes, 25);

    // 超过接收缓冲区的数据帧被丢弃。
    let oversized = ipv4_packet(IPV4_A, IPV4_B, &[0u8; 1400]);
    let actions = mesh_a.handle_ip(&oversized);
    assert!(send_actions(&socket_a, actions).is_empty());
    let (len, from) = receive(&socket_b, &mut buf);
    let actions = mesh_b.handle_datagram(pong_at, from, &buf[..len]);
    assert!(send_actions(&socket_b, actions).is_empty());

    // TUN 读到的包如果源地址不是本机地址，直接丢弃。
    let spoofed = ipv4_packet([10, 144, 0, 99], IPV4_B, b"spoofed");
    assert!(mesh_a.handle_ip(&spoofed).is_empty());

    // 30 s 没有收到认证帧后，peer 从表中移除。
    let timeout_at = pong_at + Duration::from_secs(31);
    let actions = mesh_a.tick(timeout_at);
    assert!(actions.iter().any(
        |action| matches!(action, Action::PeerDown { peer_id } if *peer_id == identity_b.peer_id())
    ));
    assert!(mesh_a.peer_status().is_empty());

    // 超时后继续按 5 s 间隔重试握手。
    let retry_at = timeout_at + Duration::from_secs(5);
    let actions = mesh_a.tick(retry_at);
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, Action::Send { .. }))
    );
}
