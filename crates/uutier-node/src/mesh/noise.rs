//! Noise 握手和会话。

use std::net::Ipv4Addr;

use blake2::{Blake2s256, Digest};
use snow::{Builder, HandshakeState, StatelessTransportState, params::NoiseParams};
use thiserror::Error;

use super::identity::{IDENTITY_KEY_LEN, Identity, peer_id};
use super::wire::{
    DATA_NONCE_LEN, FrameKind, HEADER_LEN, Header, TAG_LEN, WireError, parse_data_payload,
};

/// Noise 协议名。
pub const NOISE_PATTERN: &str = "Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s";

/// PSK 长度，单位 byte。
pub const PSK_LEN: usize = 32;

/// 握手载荷长度，单位 byte。
pub const HANDSHAKE_PAYLOAD_LEN: usize = 6;

/// 握手消息的缓冲区长度，单位 byte。
const HANDSHAKE_BUFFER_BYTES: usize = 256;

/// 从网络密钥派生 PSK。
pub fn psk_from_secret(secret: &str) -> [u8; PSK_LEN] {
    let digest = Blake2s256::digest(secret.as_bytes());
    let mut psk = [0u8; PSK_LEN];
    psk.copy_from_slice(&digest);
    psk
}

/// 握手载荷。双方交换虚拟 IPv4 和 UDP 监听端口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandshakePayload {
    pub ipv4: Ipv4Addr,
    pub listen_port: u16,
}

impl HandshakePayload {
    fn encode(self) -> [u8; HANDSHAKE_PAYLOAD_LEN] {
        let mut out = [0u8; HANDSHAKE_PAYLOAD_LEN];
        out[..4].copy_from_slice(&self.ipv4.octets());
        out[4..6].copy_from_slice(&self.listen_port.to_be_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, NoiseError> {
        if bytes.len() != HANDSHAKE_PAYLOAD_LEN {
            return Err(NoiseError::HandshakePayload);
        }
        let mut ipv4 = [0u8; 4];
        ipv4.copy_from_slice(&bytes[..4]);
        let mut port = [0u8; 2];
        port.copy_from_slice(&bytes[4..6]);
        let listen_port = u16::from_be_bytes(port);
        if listen_port == 0 {
            return Err(NoiseError::HandshakePayload);
        }
        Ok(Self {
            ipv4: Ipv4Addr::from(ipv4),
            listen_port,
        })
    }
}

/// initiator 的握手状态。
pub struct InitiatorHandshake {
    state: HandshakeState,
    remote_public: [u8; IDENTITY_KEY_LEN],
    remote_peer_id: u64,
}

impl InitiatorHandshake {
    /// 创建 initiator。`remote_public` 来自发现记录。
    pub fn new(
        identity: &Identity,
        remote_public: &[u8; IDENTITY_KEY_LEN],
        psk: &[u8; PSK_LEN],
    ) -> Result<Self, NoiseError> {
        let secret = identity.secret_bytes();
        let state = Builder::new(noise_params()?)
            .psk(2, psk)?
            .local_private_key(&secret)?
            .remote_public_key(remote_public)?
            .build_initiator()?;
        Ok(Self {
            state,
            remote_public: *remote_public,
            remote_peer_id: peer_id(remote_public),
        })
    }

    /// 返回对端 peer ID。
    pub fn remote_peer_id(&self) -> u64 {
        self.remote_peer_id
    }

    /// 写入第一条握手消息。
    pub fn write_init(&mut self, payload: HandshakePayload) -> Result<Vec<u8>, NoiseError> {
        let mut message = vec![0u8; HANDSHAKE_BUFFER_BYTES];
        let len = self.state.write_message(&payload.encode(), &mut message)?;
        message.truncate(len);
        Ok(message)
    }

    /// 读取第二条握手消息。
    pub fn read_response(&mut self, message: &[u8]) -> Result<HandshakePayload, NoiseError> {
        read_payload(&mut self.state, message)
    }

    /// 转入传输状态。
    pub fn into_session(self) -> Result<NoiseSession, NoiseError> {
        finish(self.state, &self.remote_public, self.remote_peer_id)
    }
}

/// responder 的握手状态。
pub struct ResponderHandshake {
    state: HandshakeState,
    remote_public: [u8; IDENTITY_KEY_LEN],
    remote_peer_id: u64,
}

impl ResponderHandshake {
    /// 创建 responder。`remote_public` 来自发现记录。
    pub fn new(
        identity: &Identity,
        remote_public: &[u8; IDENTITY_KEY_LEN],
        psk: &[u8; PSK_LEN],
    ) -> Result<Self, NoiseError> {
        let secret = identity.secret_bytes();
        let state = Builder::new(noise_params()?)
            .psk(2, psk)?
            .local_private_key(&secret)?
            .remote_public_key(remote_public)?
            .build_responder()?;
        Ok(Self {
            state,
            remote_public: *remote_public,
            remote_peer_id: peer_id(remote_public),
        })
    }

    /// 返回对端 peer ID。
    pub fn remote_peer_id(&self) -> u64 {
        self.remote_peer_id
    }

    /// 读取第一条握手消息。
    ///
    /// `snow` 会用消息里的发起方公钥覆盖预配置的 `rs`，这里显式比对，避免把
    /// 另一个 peer 的握手当成当前 peer 的。
    pub fn read_init(&mut self, message: &[u8]) -> Result<HandshakePayload, NoiseError> {
        let payload = read_payload(&mut self.state, message)?;
        if self.state.get_remote_static() != Some(self.remote_public.as_slice()) {
            return Err(NoiseError::RemoteKeyMismatch);
        }
        Ok(payload)
    }

    /// 写入第二条握手消息。
    pub fn write_response(&mut self, payload: HandshakePayload) -> Result<Vec<u8>, NoiseError> {
        let mut message = vec![0u8; HANDSHAKE_BUFFER_BYTES];
        let len = self.state.write_message(&payload.encode(), &mut message)?;
        message.truncate(len);
        Ok(message)
    }

    /// 转入传输状态。
    pub fn into_session(self) -> Result<NoiseSession, NoiseError> {
        finish(self.state, &self.remote_public, self.remote_peer_id)
    }
}

/// 加密会话。
pub struct NoiseSession {
    state: StatelessTransportState,
    session_id: u64,
    remote_peer_id: u64,
    outbound_nonce: u64,
    replay: ReplayWindow,
}

impl NoiseSession {
    fn new(state: StatelessTransportState, session_id: u64, remote_peer_id: u64) -> Self {
        Self {
            state,
            session_id,
            remote_peer_id,
            outbound_nonce: 0,
            replay: ReplayWindow::default(),
        }
    }

    /// 返回会话标识。
    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    /// 返回对端 peer ID。
    pub fn remote_peer_id(&self) -> u64 {
        self.remote_peer_id
    }

    /// 加密一帧并写入完整帧。`kind` 只能是 data、keepalive、ping 或 pong。
    pub fn seal(
        &mut self,
        kind: FrameKind,
        packet: &[u8],
        out: &mut [u8],
    ) -> Result<usize, NoiseError> {
        match kind {
            FrameKind::Data | FrameKind::Keepalive | FrameKind::Ping | FrameKind::Pong => {}
            FrameKind::HandshakeInit | FrameKind::HandshakeResp => {
                return Err(NoiseError::InvalidFrameKind);
            }
        }
        let total = HEADER_LEN + DATA_NONCE_LEN + packet.len() + TAG_LEN;
        if out.len() < total {
            return Err(NoiseError::OutputTooSmall {
                need: total,
                got: out.len(),
            });
        }

        let nonce = self.outbound_nonce;
        let next = nonce.checked_add(1).ok_or(NoiseError::NonceExhausted)?;
        Header {
            kind,
            session_id: self.session_id,
        }
        .encode(out)?;
        out[HEADER_LEN..HEADER_LEN + DATA_NONCE_LEN].copy_from_slice(&nonce.to_be_bytes());
        let ciphertext = &mut out[HEADER_LEN + DATA_NONCE_LEN..total];
        let len = self.state.write_message(nonce, packet, ciphertext)?;
        self.outbound_nonce = next;
        Ok(HEADER_LEN + DATA_NONCE_LEN + len)
    }

    /// 解密数据帧载荷。`payload` 是帧头之后的部分。
    pub fn open(&mut self, payload: &[u8], out: &mut [u8]) -> Result<usize, NoiseError> {
        let (nonce, ciphertext) = parse_data_payload(payload)?;
        let need = ciphertext.len().saturating_sub(TAG_LEN);
        if out.len() < need {
            return Err(NoiseError::OutputTooSmall {
                need,
                got: out.len(),
            });
        }
        let len = self.state.read_message(nonce, ciphertext, out)?;
        if !self.replay.check_and_update(nonce) {
            return Err(NoiseError::Replay(nonce));
        }
        Ok(len)
    }
}

fn noise_params() -> Result<NoiseParams, NoiseError> {
    Ok(NOISE_PATTERN.parse()?)
}

fn read_payload(
    state: &mut HandshakeState,
    message: &[u8],
) -> Result<HandshakePayload, NoiseError> {
    let mut payload = [0u8; HANDSHAKE_BUFFER_BYTES];
    let len = state.read_message(message, &mut payload)?;
    HandshakePayload::decode(&payload[..len])
}

fn finish(
    state: HandshakeState,
    remote_public: &[u8; IDENTITY_KEY_LEN],
    remote_peer_id: u64,
) -> Result<NoiseSession, NoiseError> {
    if !state.is_handshake_finished() {
        return Err(NoiseError::HandshakeIncomplete);
    }
    if state
        .get_remote_static()
        .ok_or(NoiseError::RemoteKeyMismatch)?
        != remote_public.as_slice()
    {
        return Err(NoiseError::RemoteKeyMismatch);
    }
    let hash = state.get_handshake_hash();
    if hash.len() < 8 {
        return Err(NoiseError::InvalidHandshakeHash);
    }
    let mut session_id = [0u8; 8];
    session_id.copy_from_slice(&hash[..8]);
    let transport = state.into_stateless_transport_mode()?;
    Ok(NoiseSession::new(
        transport,
        u64::from_be_bytes(session_id),
        remote_peer_id,
    ))
}

/// 会话的错误。
#[derive(Debug, Error)]
pub enum NoiseError {
    #[error(transparent)]
    Wire(#[from] WireError),

    #[error("noise error: {0}")]
    Noise(#[from] snow::Error),

    #[error("handshake payload is invalid")]
    HandshakePayload,

    #[error("handshake is not finished")]
    HandshakeIncomplete,

    #[error("remote static key does not match the discovery record")]
    RemoteKeyMismatch,

    #[error("handshake hash is too short")]
    InvalidHandshakeHash,

    #[error("output buffer is {got} bytes, need {need}")]
    OutputTooSmall { need: usize, got: usize },

    #[error("replayed nonce {0}")]
    Replay(u64),

    #[error("outbound nonce exhausted")]
    NonceExhausted,

    #[error("handshake frames cannot be sealed by a session")]
    InvalidFrameKind,
}

#[derive(Debug, Default)]
struct ReplayWindow {
    highest: Option<u64>,
    bitmap: u64,
}

impl ReplayWindow {
    fn check_and_update(&mut self, nonce: u64) -> bool {
        match self.highest {
            None => {
                self.highest = Some(nonce);
                self.bitmap = 1;
                true
            }
            Some(highest) if nonce > highest => {
                let shift = nonce - highest;
                self.bitmap = if shift >= 64 {
                    1
                } else {
                    (self.bitmap << shift) | 1
                };
                self.highest = Some(nonce);
                true
            }
            Some(highest) => {
                let distance = highest - nonce;
                if distance >= 64 {
                    return false;
                }
                let bit = 1u64 << distance;
                if self.bitmap & bit != 0 {
                    return false;
                }
                self.bitmap |= bit;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(seed: u8) -> Identity {
        Identity::from_secret_bytes([seed; IDENTITY_KEY_LEN])
    }

    fn payload(ipv4: [u8; 4], port: u16) -> HandshakePayload {
        HandshakePayload {
            ipv4: Ipv4Addr::from(ipv4),
            listen_port: port,
        }
    }

    fn exchange() -> (NoiseSession, NoiseSession) {
        let a = identity(1);
        let b = identity(2);
        let psk = psk_from_secret("network-secret");

        let mut initiator = InitiatorHandshake::new(&a, b.public_key(), &psk).unwrap();
        let mut responder = ResponderHandshake::new(&b, a.public_key(), &psk).unwrap();

        let init = initiator
            .write_init(payload([10, 144, 0, 2], 12010))
            .unwrap();
        let received = responder.read_init(&init).unwrap();
        assert_eq!(received, payload([10, 144, 0, 2], 12010));

        let response = responder
            .write_response(payload([10, 144, 0, 3], 12011))
            .unwrap();
        let received = initiator.read_response(&response).unwrap();
        assert_eq!(received, payload([10, 144, 0, 3], 12011));

        let a_session = initiator.into_session().unwrap();
        let b_session = responder.into_session().unwrap();
        assert_eq!(a_session.session_id(), b_session.session_id());
        assert_eq!(a_session.remote_peer_id(), b.peer_id());
        assert_eq!(b_session.remote_peer_id(), a.peer_id());
        (a_session, b_session)
    }

    #[test]
    fn handshake_and_transport() {
        let (mut a, mut b) = exchange();
        let packet = b"an ip packet";
        let mut frame = [0u8; 128];
        let len = a.seal(FrameKind::Data, packet, &mut frame).unwrap();

        let (header, payload) = Header::decode(&frame[..len]).unwrap();
        assert_eq!(header.kind, FrameKind::Data);
        assert_eq!(header.session_id, a.session_id());

        let mut opened = [0u8; 128];
        let opened_len = b.open(payload, &mut opened).unwrap();
        assert_eq!(&opened[..opened_len], packet);
    }

    #[test]
    fn responder_rejects_wrong_remote_key() {
        let a = identity(1);
        let b = identity(2);
        let other = identity(3);
        let psk = psk_from_secret("network-secret");

        let mut initiator = InitiatorHandshake::new(&a, b.public_key(), &psk).unwrap();
        let mut responder = ResponderHandshake::new(&b, other.public_key(), &psk).unwrap();
        let init = initiator
            .write_init(payload([10, 144, 0, 2], 12010))
            .unwrap();
        assert!(matches!(
            responder.read_init(&init),
            Err(NoiseError::RemoteKeyMismatch)
        ));
    }

    #[test]
    fn rejects_wrong_psk() {
        let a = identity(1);
        let b = identity(2);
        let mut initiator =
            InitiatorHandshake::new(&a, b.public_key(), &psk_from_secret("one")).unwrap();
        let mut responder =
            ResponderHandshake::new(&b, a.public_key(), &psk_from_secret("two")).unwrap();

        let init = initiator
            .write_init(payload([10, 144, 0, 2], 12010))
            .unwrap();
        responder.read_init(&init).unwrap();
        let response = responder
            .write_response(payload([10, 144, 0, 3], 12011))
            .unwrap();
        assert!(matches!(
            initiator.read_response(&response),
            Err(NoiseError::Noise(_))
        ));
    }

    #[test]
    fn rejects_tampered_ciphertext() {
        let (mut a, mut b) = exchange();
        let mut frame = [0u8; 128];
        let len = a.seal(FrameKind::Data, b"packet", &mut frame).unwrap();
        let last = len - 1;
        frame[last] ^= 1;

        let (_, payload) = Header::decode(&frame[..len]).unwrap();
        let mut opened = [0u8; 128];
        assert!(matches!(
            b.open(payload, &mut opened),
            Err(NoiseError::Noise(_))
        ));
    }

    #[test]
    fn rejects_replayed_nonce() {
        let (mut a, mut b) = exchange();
        let mut frame = [0u8; 128];
        let len = a.seal(FrameKind::Data, b"packet", &mut frame).unwrap();
        let (_, payload) = Header::decode(&frame[..len]).unwrap();
        let mut opened = [0u8; 128];

        b.open(payload, &mut opened).unwrap();
        assert!(matches!(
            b.open(payload, &mut opened),
            Err(NoiseError::Replay(0))
        ));
    }

    #[test]
    fn accepts_reordered_packets_within_window() {
        let (mut a, mut b) = exchange();
        let mut frames = Vec::new();
        for _ in 0..3 {
            let mut frame = [0u8; 64];
            let len = a.seal(FrameKind::Data, b"packet", &mut frame).unwrap();
            frames.push(frame[..len].to_vec());
        }

        let mut opened = [0u8; 64];
        for index in [2, 0, 1] {
            let (_, payload) = Header::decode(&frames[index]).unwrap();
            b.open(payload, &mut opened).unwrap();
        }
    }

    #[test]
    fn rejects_zero_port_payload() {
        assert!(matches!(
            HandshakePayload::decode(&[10, 144, 0, 2, 0, 0]),
            Err(NoiseError::HandshakePayload)
        ));
    }

    #[test]
    fn derives_stable_psk() {
        let psk = psk_from_secret("network-secret");
        assert_eq!(psk, psk_from_secret("network-secret"));
        assert_ne!(psk, psk_from_secret("other"));
        assert_eq!(psk.len(), PSK_LEN);
    }
}
