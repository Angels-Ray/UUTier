//! 握手和数据帧的本机集成测试。

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::net::Ipv4Addr;
use std::time::Duration;

use tokio::net::UdpSocket;
use uutier_node::mesh::wire::{Header, encode};
use uutier_node::{
    FrameKind, HandshakePayload, Identity, InitiatorHandshake, ResponderHandshake, psk_from_secret,
};

const TIMEOUT: Duration = Duration::from_secs(2);

async fn recv_frame(socket: &UdpSocket) -> (Header, Vec<u8>) {
    let mut buf = [0u8; 2048];
    let len = tokio::time::timeout(TIMEOUT, socket.recv(&mut buf))
        .await
        .expect("receive timed out")
        .expect("receive failed");
    let (header, payload) = Header::decode(&buf[..len]).expect("invalid frame");
    (header, payload.to_vec())
}

#[tokio::test]
async fn peers_handshake_and_exchange_data_over_udp() {
    let socket_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let socket_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr_b = socket_b.local_addr().unwrap();
    let addr_a = socket_a.local_addr().unwrap();

    let identity_a = Identity::from_secret_bytes([1u8; 32]);
    let identity_b = Identity::from_secret_bytes([2u8; 32]);
    let psk = psk_from_secret("network-secret");

    let mut initiator =
        InitiatorHandshake::new(&identity_a, identity_b.public_key(), &psk).unwrap();
    let mut responder =
        ResponderHandshake::new(&identity_b, identity_a.public_key(), &psk).unwrap();

    let init = initiator
        .write_init(HandshakePayload {
            ipv4: Ipv4Addr::new(10, 144, 0, 2),
            listen_port: addr_a.port(),
        })
        .unwrap();
    let mut frame = [0u8; 512];
    let len = encode(
        FrameKind::HandshakeInit,
        identity_b.peer_id(),
        &init,
        &mut frame,
    )
    .unwrap();
    socket_a.send_to(&frame[..len], addr_b).await.unwrap();

    let (header, payload) = recv_frame(&socket_b).await;
    assert_eq!(header.kind, FrameKind::HandshakeInit);
    assert_eq!(header.session_id, identity_b.peer_id());
    let payload = responder.read_init(&payload).unwrap();
    assert_eq!(payload.ipv4, Ipv4Addr::new(10, 144, 0, 2));

    let response = responder
        .write_response(HandshakePayload {
            ipv4: Ipv4Addr::new(10, 144, 0, 3),
            listen_port: addr_b.port(),
        })
        .unwrap();
    let len = encode(
        FrameKind::HandshakeResp,
        identity_a.peer_id(),
        &response,
        &mut frame,
    )
    .unwrap();
    socket_b.send_to(&frame[..len], addr_a).await.unwrap();

    let (header, payload) = recv_frame(&socket_a).await;
    assert_eq!(header.kind, FrameKind::HandshakeResp);
    assert_eq!(header.session_id, identity_a.peer_id());
    let payload = initiator.read_response(&payload).unwrap();
    assert_eq!(payload.ipv4, Ipv4Addr::new(10, 144, 0, 3));

    let mut session_a = initiator.into_session().unwrap();
    let mut session_b = responder.into_session().unwrap();
    assert_eq!(session_a.session_id(), session_b.session_id());
    assert_eq!(session_a.remote_peer_id(), identity_b.peer_id());
    assert_eq!(session_b.remote_peer_id(), identity_a.peer_id());

    let mut frame = [0u8; 512];
    let len = session_a
        .seal(FrameKind::Data, b"packet from a", &mut frame)
        .unwrap();
    socket_a.send_to(&frame[..len], addr_b).await.unwrap();

    let (header, payload) = recv_frame(&socket_b).await;
    assert_eq!(header.kind, FrameKind::Data);
    assert_eq!(header.session_id, session_a.session_id());
    let mut opened = [0u8; 512];
    let len = session_b.open(&payload, &mut opened).unwrap();
    assert_eq!(&opened[..len], b"packet from a");

    let len = session_b
        .seal(FrameKind::Data, b"packet from b", &mut frame)
        .unwrap();
    socket_b.send_to(&frame[..len], addr_a).await.unwrap();

    let (_, payload) = recv_frame(&socket_a).await;
    let len = session_a.open(&payload, &mut opened).unwrap();
    assert_eq!(&opened[..len], b"packet from b");
}
