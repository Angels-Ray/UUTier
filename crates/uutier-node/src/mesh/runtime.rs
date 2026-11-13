//! 组网运行时状态机。
//!
//! 状态机的输入是 TUN 读到的 IP 包、UDP 收到的数据报和定时事件，输出是要发送的
//! UDP 数据报、要写回 TUN 的 IP 包和 peer 状态变化。socket 和 TUN 的 I/O 由
//! `supervisor` 负责，便于在本机用两个 UDP socket 测试。

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use tracing::{debug, trace, warn};
use uutier_core::{PathKind, PeerStatus, REDACTED};

use super::identity::{IDENTITY_KEY_LEN, Identity, peer_id};
use super::noise::{
    HandshakePayload, InitiatorHandshake, NoiseSession, PSK_LEN, ResponderHandshake,
    psk_from_secret,
};
use super::wire::{
    DATA_NONCE_LEN, FrameKind, HEADER_LEN, Header, TAG_LEN, decode_sequence, encode,
    encode_sequence,
};
use super::{MeshError, MeshParams};

/// peer 超时，单位 s。
pub const PEER_TIMEOUT_S: u64 = 30;

/// 握手重试间隔，单位 s。
pub const HANDSHAKE_RETRY_S: u64 = 5;

/// Ping 间隔，单位 s。
pub const PING_INTERVAL_S: u64 = 10;

/// 一个网络最多三台设备，本机之外的 peer 最多两个。
pub const MAX_PEERS: usize = 2;

const PEER_TIMEOUT: Duration = Duration::from_secs(PEER_TIMEOUT_S);
const HANDSHAKE_RETRY: Duration = Duration::from_secs(HANDSHAKE_RETRY_S);
const PING_INTERVAL: Duration = Duration::from_secs(PING_INTERVAL_S);

/// 复用缓冲区的数量上限。
const BUFFER_POOL_MAX: usize = 64;

/// 超过这个容量的缓冲区不回收。
const BUFFER_CAPACITY_MAX: usize = 64 * 1024;

/// 静态 peer 记录。
#[derive(Clone, PartialEq, Eq)]
pub struct PeerRecord {
    pub peer_id: u64,
    pub public_key: [u8; IDENTITY_KEY_LEN],
    pub ipv4: Ipv4Addr,
    /// 对端能接受连接的 UDP 地址。
    pub endpoint: Option<SocketAddr>,
}

impl fmt::Debug for PeerRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerRecord")
            .field("peer_id", &self.peer_id)
            .field("ipv4", &self.ipv4)
            .field("endpoint", &self.endpoint.as_ref().map(|_| REDACTED))
            .finish()
    }
}

impl PeerRecord {
    /// 由公钥、虚拟 IPv4 和端点创建记录。
    pub fn new(
        public_key: [u8; IDENTITY_KEY_LEN],
        ipv4: Ipv4Addr,
        endpoint: Option<SocketAddr>,
    ) -> Self {
        Self {
            peer_id: peer_id(&public_key),
            public_key,
            ipv4,
            endpoint,
        }
    }
}

/// 状态机产生的动作。
#[derive(PartialEq, Eq)]
pub enum Action {
    /// 发送一个 UDP 数据报。
    Send { to: SocketAddr, datagram: Vec<u8> },
    /// 把 IP 包写入 TUN。
    Deliver(Vec<u8>),
    /// peer 建立了第一条会话。
    PeerUp { peer_id: u64 },
    /// peer 超时，会话已清除。
    PeerDown { peer_id: u64 },
}

impl fmt::Debug for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Send { datagram, .. } => f
                .debug_struct("Send")
                .field("datagram_bytes", &datagram.len())
                .finish(),
            Self::Deliver(packet) => f.debug_tuple("Deliver").field(&packet.len()).finish(),
            Self::PeerUp { peer_id } => f.debug_struct("PeerUp").field("peer_id", peer_id).finish(),
            Self::PeerDown { peer_id } => f
                .debug_struct("PeerDown")
                .field("peer_id", peer_id)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Initiator,
    Responder,
}

struct PeerState {
    record: PeerRecord,
    /// 最近一次通过认证的来源地址或配置的地址。
    endpoint: Option<SocketAddr>,
    /// 信令发现的候选直连地址，握手失败时轮流尝试。
    candidates: Vec<SocketAddr>,
    candidate_index: usize,
    /// TURN bridge 在 loopback 上的地址。来源等于该地址的帧表示走了 TURN。
    turn_endpoint: Option<SocketAddr>,
    path: PathKind,
    initiator: Option<NoiseSession>,
    responder: Option<NoiseSession>,
    /// 本端应答了握手、但还没收到认证帧的会话。收到认证帧后才替换 `responder`。
    responder_pending: Option<NoiseSession>,
    /// 最近一次成功解密的会话。对端重启后新会话会替换旧会话作为发送路径。
    last_rx_session: Option<u64>,
    pending: Option<InitiatorHandshake>,
    next_handshake: Instant,
    last_rx: Option<Instant>,
    next_ping: Instant,
    pending_ping: Option<(u64, Instant)>,
    ping_sequence: u64,
    rtt_us: Option<u32>,
    rx_bytes: u64,
    tx_bytes: u64,
}

impl PeerState {
    fn session_mut(&mut self, session_id: u64) -> Option<&mut NoiseSession> {
        if self
            .initiator
            .as_ref()
            .is_some_and(|session| session.session_id() == session_id)
        {
            self.initiator.as_mut()
        } else if self
            .responder
            .as_ref()
            .is_some_and(|session| session.session_id() == session_id)
        {
            self.responder.as_mut()
        } else if self
            .responder_pending
            .as_ref()
            .is_some_and(|session| session.session_id() == session_id)
        {
            self.responder_pending.as_mut()
        } else {
            None
        }
    }

    fn outbound_mut(&mut self) -> Option<&mut NoiseSession> {
        if let Some(session_id) = self.last_rx_session {
            if self
                .initiator
                .as_ref()
                .is_some_and(|session| session.session_id() == session_id)
            {
                return self.initiator.as_mut();
            }
            if self
                .responder
                .as_ref()
                .is_some_and(|session| session.session_id() == session_id)
            {
                return self.responder.as_mut();
            }
        }
        if self.initiator.is_some() {
            self.initiator.as_mut()
        } else {
            self.responder.as_mut()
        }
    }

    fn has_session(&self) -> bool {
        self.initiator.is_some() || self.responder.is_some()
    }

    /// 记录已认证帧的来源，并按 bridge 地址判断路径。
    fn note_source(&mut self, from: SocketAddr) {
        self.endpoint = Some(from);
        let path = if self.turn_endpoint == Some(from) {
            PathKind::Turn
        } else {
            PathKind::Direct
        };
        if path != self.path {
            self.path = path;
            self.rtt_us = None;
        }
    }
}

/// 组网状态机。
pub struct Mesh {
    params: MeshParams,
    identity: Identity,
    psk: [u8; PSK_LEN],
    local_peer_id: u64,
    peers: BTreeMap<u64, PeerState>,
    by_ipv4: HashMap<Ipv4Addr, u64>,
    /// session_id 到 peer_id。
    sessions: HashMap<u64, u64>,
    open_buf: Vec<u8>,
    /// 发送帧和交付包的复用缓冲区。
    buffers: Vec<Vec<u8>>,
}

impl Mesh {
    /// 创建状态机。`now` 是握手和保活计时的起点。
    pub fn new(
        params: MeshParams,
        identity: Identity,
        records: Vec<PeerRecord>,
        now: Instant,
    ) -> Result<Self, MeshError> {
        let local_peer_id = identity.peer_id();
        let local_ipv4 = params.ipv4;
        let mut peers = BTreeMap::new();
        let mut by_ipv4 = HashMap::new();
        if records.len() > MAX_PEERS {
            return Err(MeshError::TooManyPeers { max: MAX_PEERS });
        }
        for record in records {
            if record.peer_id == local_peer_id || record.ipv4 == local_ipv4 {
                return Err(MeshError::PeerIsLocal);
            }
            if record.ipv4.octets()[..3] != local_ipv4.octets()[..3] {
                return Err(MeshError::PeerOutsideSubnet(record.ipv4));
            }
            if matches!(record.ipv4.octets()[3], 0 | 255) {
                return Err(MeshError::PeerIpv4Unusable(record.ipv4));
            }
            if peers.contains_key(&record.peer_id) {
                return Err(MeshError::DuplicatePeerId(record.peer_id));
            }
            if by_ipv4.contains_key(&record.ipv4) {
                return Err(MeshError::DuplicatePeerIpv4(record.ipv4));
            }
            by_ipv4.insert(record.ipv4, record.peer_id);
            peers.insert(
                record.peer_id,
                PeerState {
                    endpoint: record.endpoint,
                    candidates: Vec::new(),
                    candidate_index: 0,
                    turn_endpoint: None,
                    path: PathKind::Direct,
                    record,
                    initiator: None,
                    responder: None,
                    responder_pending: None,
                    last_rx_session: None,
                    pending: None,
                    next_handshake: now,
                    last_rx: None,
                    next_ping: now,
                    pending_ping: None,
                    ping_sequence: 0,
                    rtt_us: None,
                    rx_bytes: 0,
                    tx_bytes: 0,
                },
            );
        }
        let open_buf = vec![0u8; usize::from(params.mtu_bytes) + 64];
        let psk = psk_from_secret(&params.secret);
        Ok(Self {
            params,
            identity,
            psk,
            local_peer_id,
            peers,
            by_ipv4,
            sessions: HashMap::new(),
            open_buf,
            buffers: Vec::new(),
        })
    }

    /// 加入一个从 UU 信令发现的 peer。重复记录返回 `false`。
    pub fn add_peer(&mut self, record: PeerRecord, now: Instant) -> Result<bool, MeshError> {
        if record.peer_id == self.local_peer_id || record.ipv4 == self.params.ipv4 {
            return Err(MeshError::PeerIsLocal);
        }
        if record.ipv4.octets()[..3] != self.params.ipv4.octets()[..3] {
            return Err(MeshError::PeerOutsideSubnet(record.ipv4));
        }
        if matches!(record.ipv4.octets()[3], 0 | 255) {
            return Err(MeshError::PeerIpv4Unusable(record.ipv4));
        }
        if let Some(existing) = self.peers.get(&record.peer_id) {
            if existing.record.ipv4 != record.ipv4 {
                return Err(MeshError::PeerIpv4Changed(record.peer_id));
            }
            return Ok(false);
        }
        if self.peers.len() >= MAX_PEERS {
            return Err(MeshError::TooManyPeers { max: MAX_PEERS });
        }
        if let Some(&peer_id) = self.by_ipv4.get(&record.ipv4)
            && peer_id != record.peer_id
        {
            return Err(MeshError::DuplicatePeerIpv4(record.ipv4));
        }
        self.by_ipv4.insert(record.ipv4, record.peer_id);
        self.peers.insert(
            record.peer_id,
            PeerState {
                endpoint: record.endpoint,
                candidates: Vec::new(),
                candidate_index: 0,
                turn_endpoint: None,
                path: PathKind::Direct,
                record,
                initiator: None,
                responder: None,
                responder_pending: None,
                last_rx_session: None,
                pending: None,
                next_handshake: now,
                last_rx: None,
                next_ping: now,
                pending_ping: None,
                ping_sequence: 0,
                rtt_us: None,
                rx_bytes: 0,
                tx_bytes: 0,
            },
        );
        Ok(true)
    }

    /// 移除一个 peer 及其会话。
    pub fn remove_peer(&mut self, peer_id: u64) -> bool {
        let Some(peer) = self.peers.remove(&peer_id) else {
            return false;
        };
        self.by_ipv4.remove(&peer.record.ipv4);
        if let Some(session) = peer.initiator {
            self.sessions.remove(&session.session_id());
        }
        if let Some(session) = peer.responder {
            self.sessions.remove(&session.session_id());
        }
        if let Some(session) = peer.responder_pending {
            self.sessions.remove(&session.session_id());
        }
        true
    }

    /// 收回已经发送或交付的缓冲区，供下一个包复用。
    pub fn recycle(&mut self, buffer: Vec<u8>) {
        if buffer.capacity() > BUFFER_CAPACITY_MAX || self.buffers.len() >= BUFFER_POOL_MAX {
            return;
        }
        self.buffers.push(buffer);
    }

    /// 返回本机 peer ID。
    pub fn local_peer_id(&self) -> u64 {
        self.local_peer_id
    }

    /// 返回组网参数。
    pub fn params(&self) -> &MeshParams {
        &self.params
    }

    /// 返回 peer 数量，包括未建立会话的 peer。
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// 返回已建立会话的 peer 状态。
    pub fn peer_status(&self) -> Vec<PeerStatus> {
        self.peers
            .values()
            .filter(|peer| peer.has_session())
            .map(|peer| PeerStatus {
                peer_id: peer.record.peer_id,
                ipv4: Some(peer.record.ipv4.to_string()),
                path: peer.path,
                mesh_rtt_us: peer.rtt_us,
                turn_rtt_us: (peer.path == PathKind::Turn)
                    .then_some(peer.rtt_us)
                    .flatten(),
                rx_bytes: peer.rx_bytes,
                tx_bytes: peer.tx_bytes,
            })
            .collect()
    }

    /// 返回 peer 是否已建立会话。
    pub fn peer_has_session(&self, peer_id: u64) -> bool {
        self.peers.get(&peer_id).is_some_and(PeerState::has_session)
    }

    /// 返回 peer 当前使用的路径。
    pub fn peer_path(&self, peer_id: u64) -> Option<PathKind> {
        self.peers.get(&peer_id).map(|peer| peer.path)
    }

    /// 返回 peer 最近一次认证帧的来源地址。
    pub fn peer_endpoint(&self, peer_id: u64) -> Option<SocketAddr> {
        self.peers.get(&peer_id).and_then(|peer| peer.endpoint)
    }

    /// 登记 TURN bridge 的本地地址。bridge 建立后即可收到帧，早于 supervisor 启用 TURN。
    pub fn set_turn_endpoint(&mut self, peer_id: u64, endpoint: SocketAddr) -> bool {
        let Some(peer) = self.peers.get_mut(&peer_id) else {
            return false;
        };
        peer.turn_endpoint = Some(endpoint);
        if peer.endpoint == Some(endpoint) && peer.path != PathKind::Turn {
            peer.path = PathKind::Turn;
            peer.rtt_us = None;
        }
        true
    }

    /// 把 peer 的发送地址切到 `endpoint`。`turn = true` 表示该地址是 TURN bridge。
    pub fn set_peer_transport(
        &mut self,
        peer_id: u64,
        endpoint: SocketAddr,
        turn: bool,
        now: Instant,
    ) -> bool {
        let Some(peer) = self.peers.get_mut(&peer_id) else {
            return false;
        };
        let path = if turn {
            peer.turn_endpoint = Some(endpoint);
            PathKind::Turn
        } else {
            // 回退到直连后，来自旧 bridge 地址的帧不再算作 TURN 路径。
            peer.turn_endpoint = None;
            PathKind::Direct
        };
        if path != peer.path {
            peer.path = path;
            peer.rtt_us = None;
        }
        peer.candidates.clear();
        peer.candidate_index = 0;
        if peer.endpoint != Some(endpoint) {
            peer.endpoint = Some(endpoint);
            peer.pending = None;
            peer.pending_ping = None;
            peer.next_handshake = now;
            peer.next_ping = now;
        }
        true
    }

    /// 设置信令发现的直连候选地址。握手失败时按顺序轮换。
    pub fn set_peer_candidates(
        &mut self,
        peer_id: u64,
        endpoints: &[SocketAddr],
        now: Instant,
    ) -> bool {
        let Some(peer) = self.peers.get_mut(&peer_id) else {
            return false;
        };
        let mut candidates = Vec::new();
        for endpoint in endpoints {
            let std::net::IpAddr::V4(address) = endpoint.ip() else {
                continue;
            };
            if !super::is_usable_endpoint(address) {
                continue;
            }
            if !candidates.contains(endpoint) {
                candidates.push(*endpoint);
            }
        }
        let Some(first) = candidates.first().copied() else {
            return false;
        };
        // 已有会话时不重置发送端点、路径和计时器，只更新回退用的候选列表。
        if peer.has_session() {
            peer.candidates = candidates;
            return true;
        }
        peer.path = PathKind::Direct;
        peer.endpoint = Some(first);
        peer.candidates = candidates;
        peer.candidate_index = 0;
        peer.pending = None;
        peer.pending_ping = None;
        peer.next_handshake = now;
        peer.next_ping = now;
        true
    }

    /// 处理一个从 TUN 读到的 IP 包。
    pub fn handle_ip(&mut self, packet: &[u8]) -> Vec<Action> {
        let Some((source, destination)) = ipv4_addresses(packet) else {
            trace!("dropping malformed IPv4 packet");
            return Vec::new();
        };
        if source != self.params.ipv4 {
            trace!("dropping IP packet with a foreign source address");
            return Vec::new();
        }
        let Some(&peer_id) = self.by_ipv4.get(&destination) else {
            trace!("dropping IP packet for an unknown destination");
            return Vec::new();
        };
        let Some(peer) = self.peers.get_mut(&peer_id) else {
            return Vec::new();
        };
        let Some(endpoint) = peer.endpoint else {
            trace!(peer_id, "dropping IP packet: peer has no endpoint");
            return Vec::new();
        };
        if peer.outbound_mut().is_none() {
            trace!(peer_id, "dropping IP packet: peer has no session");
            return Vec::new();
        }
        let mut datagram = take_buffer(
            &mut self.buffers,
            HEADER_LEN + DATA_NONCE_LEN + packet.len() + TAG_LEN,
        );
        let len = {
            let Some(session) = peer.outbound_mut() else {
                self.recycle(datagram);
                return Vec::new();
            };
            match session.seal(FrameKind::Data, packet, &mut datagram) {
                Ok(len) => len,
                Err(err) => {
                    debug!(peer_id, error = %err, "cannot seal packet");
                    self.recycle(datagram);
                    return Vec::new();
                }
            }
        };
        datagram.truncate(len);
        peer.tx_bytes += packet.len() as u64;
        vec![Action::Send {
            to: endpoint,
            datagram,
        }]
    }

    /// 处理一个 UDP 数据报。
    pub fn handle_datagram(
        &mut self,
        now: Instant,
        from: SocketAddr,
        datagram: &[u8],
    ) -> Vec<Action> {
        let (header, payload) = match Header::decode(datagram) {
            Ok(decoded) => decoded,
            Err(err) => {
                trace!(error = %err, "dropping malformed frame");
                return Vec::new();
            }
        };
        match header.kind {
            FrameKind::HandshakeInit => {
                self.handle_handshake_init(now, from, header.session_id, payload)
            }
            FrameKind::HandshakeResp => {
                self.handle_handshake_response(now, from, header.session_id, payload)
            }
            FrameKind::Data | FrameKind::Keepalive | FrameKind::Ping | FrameKind::Pong => {
                self.handle_sealed(now, from, header, payload)
            }
        }
    }

    /// 处理定时事件：保活、超时和握手重试。
    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        let mut actions = Vec::new();
        for (&peer_id, peer) in self.peers.iter_mut() {
            if peer.has_session() {
                let timed_out = peer
                    .last_rx
                    .is_some_and(|last| now.saturating_duration_since(last) >= PEER_TIMEOUT);
                if timed_out {
                    if let Some(session) = peer.initiator.take() {
                        self.sessions.remove(&session.session_id());
                    }
                    if let Some(session) = peer.responder.take() {
                        self.sessions.remove(&session.session_id());
                    }
                    if let Some(session) = peer.responder_pending.take() {
                        self.sessions.remove(&session.session_id());
                    }
                    peer.pending = None;
                    peer.pending_ping = None;
                    // 保留信令发现的候选地址，重连时继续按顺序尝试。
                    peer.rtt_us = None;
                    peer.next_handshake = now;
                    peer.next_ping = now;
                    actions.push(Action::PeerDown { peer_id });
                    continue;
                }
                if peer
                    .pending_ping
                    .is_some_and(|(_, sent)| now.saturating_duration_since(sent) >= PING_INTERVAL)
                {
                    peer.pending_ping = None;
                }
                if peer.pending_ping.is_none() && now >= peer.next_ping {
                    let Some(endpoint) = peer.endpoint else {
                        continue;
                    };
                    peer.ping_sequence = peer.ping_sequence.wrapping_add(1);
                    let sequence = peer.ping_sequence;
                    let mut datagram =
                        take_buffer(&mut self.buffers, HEADER_LEN + DATA_NONCE_LEN + 8 + TAG_LEN);
                    let sealed = peer.outbound_mut().and_then(|session| {
                        match session.seal(
                            FrameKind::Ping,
                            &encode_sequence(sequence),
                            &mut datagram,
                        ) {
                            Ok(len) => {
                                datagram.truncate(len);
                                Some(())
                            }
                            Err(err) => {
                                debug!(peer_id, error = %err, "cannot seal ping");
                                None
                            }
                        }
                    });
                    if sealed.is_some() {
                        peer.pending_ping = Some((sequence, now));
                        peer.next_ping = now + PING_INTERVAL;
                        actions.push(Action::Send {
                            to: endpoint,
                            datagram,
                        });
                    }
                }
            } else {
                if now < peer.next_handshake {
                    continue;
                }
                peer.next_handshake = now + HANDSHAKE_RETRY;
                if !peer.candidates.is_empty() {
                    let index = peer.candidate_index % peer.candidates.len();
                    peer.endpoint = Some(peer.candidates[index]);
                    peer.candidate_index = (index + 1) % peer.candidates.len();
                }
                let Some(endpoint) = peer.endpoint else {
                    continue;
                };
                let mut handshake = match InitiatorHandshake::new(
                    &self.identity,
                    &peer.record.public_key,
                    &self.psk,
                ) {
                    Ok(handshake) => handshake,
                    Err(err) => {
                        debug!(peer_id, error = %err, "cannot start handshake");
                        continue;
                    }
                };
                let init = match handshake.write_init(HandshakePayload {
                    ipv4: self.params.ipv4,
                    listen_port: self.params.listen_port,
                }) {
                    Ok(init) => init,
                    Err(err) => {
                        debug!(peer_id, error = %err, "cannot write handshake");
                        continue;
                    }
                };
                peer.pending = Some(handshake);
                let mut datagram = take_buffer(&mut self.buffers, HEADER_LEN + init.len());
                match encode(
                    FrameKind::HandshakeInit,
                    peer.record.peer_id,
                    &init,
                    &mut datagram,
                ) {
                    Ok(len) => {
                        datagram.truncate(len);
                        actions.push(Action::Send {
                            to: endpoint,
                            datagram,
                        });
                    }
                    Err(err) => debug!(peer_id, error = %err, "cannot encode handshake"),
                }
            }
        }
        actions
    }

    fn handle_handshake_init(
        &mut self,
        now: Instant,
        from: SocketAddr,
        receiver: u64,
        message: &[u8],
    ) -> Vec<Action> {
        if receiver != self.local_peer_id {
            trace!("dropping handshake for another peer");
            return Vec::new();
        }
        let mut matched = None;
        for peer in self.peers.values() {
            let Ok(mut handshake) =
                ResponderHandshake::new(&self.identity, &peer.record.public_key, &self.psk)
            else {
                continue;
            };
            if let Ok(payload) = handshake.read_init(message) {
                matched = Some((peer.record.peer_id, peer.record.ipv4, handshake, payload));
                break;
            }
        }
        let Some((peer_id, expected_ipv4, mut handshake, payload)) = matched else {
            debug!("dropping handshake from an unknown peer");
            return Vec::new();
        };
        if payload.ipv4 != expected_ipv4 {
            warn!(
                peer_id,
                payload_ipv4 = %payload.ipv4,
                expected_ipv4 = %expected_ipv4,
                "rejecting handshake init with a mismatched virtual IPv4"
            );
            return Vec::new();
        }
        let response = match handshake.write_response(HandshakePayload {
            ipv4: self.params.ipv4,
            listen_port: self.params.listen_port,
        }) {
            Ok(response) => response,
            Err(err) => {
                debug!(peer_id, error = %err, "cannot write handshake response");
                return Vec::new();
            }
        };
        let session = match handshake.into_session() {
            Ok(session) => session,
            Err(err) => {
                debug!(peer_id, error = %err, "handshake did not complete");
                return Vec::new();
            }
        };
        let mut actions = Vec::new();
        if let Some(action) = self.install_session(peer_id, Role::Responder, session, now) {
            actions.push(action);
        }
        // 握手 init 可能被重放，此时来源地址不受对端密钥保护。端点要等收到
        // 认证帧后再更新，否则重放报文能把已建立的路径改到别的地址。
        let mut datagram = take_buffer(&mut self.buffers, HEADER_LEN + response.len());
        match encode(FrameKind::HandshakeResp, peer_id, &response, &mut datagram) {
            Ok(len) => {
                datagram.truncate(len);
                actions.push(Action::Send { to: from, datagram });
            }
            Err(err) => debug!(peer_id, error = %err, "cannot encode handshake response"),
        }
        actions
    }

    fn handle_handshake_response(
        &mut self,
        now: Instant,
        from: SocketAddr,
        receiver: u64,
        message: &[u8],
    ) -> Vec<Action> {
        if receiver != self.local_peer_id {
            trace!("dropping handshake response for another peer");
            return Vec::new();
        }
        let Some(peer_id) = self.pending_peer_at(from) else {
            trace!("dropping unexpected handshake response");
            return Vec::new();
        };
        let Some(expected_ipv4) = self.peers.get(&peer_id).map(|peer| peer.record.ipv4) else {
            return Vec::new();
        };
        let Some(peer) = self.peers.get_mut(&peer_id) else {
            return Vec::new();
        };
        // 验证通过前保留 pending，伪造的响应不会结束本轮握手。
        let Some(pending) = peer.pending.as_mut() else {
            return Vec::new();
        };
        let payload = match pending.read_response(message) {
            Ok(payload) => payload,
            Err(err) => {
                debug!(peer_id, error = %err, "handshake response rejected");
                return Vec::new();
            }
        };
        if payload.ipv4 != expected_ipv4 {
            warn!(
                peer_id,
                payload_ipv4 = %payload.ipv4,
                expected_ipv4 = %expected_ipv4,
                "rejecting handshake response with a mismatched virtual IPv4"
            );
            return Vec::new();
        }
        let Some(pending) = peer.pending.take() else {
            return Vec::new();
        };
        let session = match pending.into_session() {
            Ok(session) => session,
            Err(err) => {
                debug!(peer_id, error = %err, "handshake did not complete");
                return Vec::new();
            }
        };
        let action = self.install_session(peer_id, Role::Initiator, session, now);
        if let Some(peer) = self.peers.get_mut(&peer_id) {
            peer.note_source(from);
        }
        action.into_iter().collect()
    }

    fn handle_sealed(
        &mut self,
        now: Instant,
        from: SocketAddr,
        header: Header,
        payload: &[u8],
    ) -> Vec<Action> {
        let Some(&peer_id) = self.sessions.get(&header.session_id) else {
            trace!(
                session_id = header.session_id,
                "dropping frame for an unknown session"
            );
            return Vec::new();
        };
        let Some(peer) = self.peers.get_mut(&peer_id) else {
            return Vec::new();
        };
        let Some(session) = peer.session_mut(header.session_id) else {
            return Vec::new();
        };
        // 数据帧直接解密到交付缓冲区，省掉一次从 `open_buf` 的复制。
        let plain_len = payload.len().saturating_sub(DATA_NONCE_LEN + TAG_LEN);
        if header.kind == FrameKind::Data && plain_len > self.open_buf.len() {
            debug!(
                peer_id,
                bytes = plain_len,
                limit_bytes = self.open_buf.len(),
                "dropping oversized data frame"
            );
            return Vec::new();
        }
        let mut delivered = match header.kind {
            FrameKind::Data => Some(take_buffer(&mut self.buffers, plain_len)),
            _ => None,
        };
        let out = delivered.as_deref_mut().unwrap_or(&mut self.open_buf);
        let len = match session.open(payload, out) {
            Ok(len) => len,
            Err(err) => {
                trace!(peer_id, error = %err, "dropping unauthenticated frame");
                if let Some(delivered) = delivered.take() {
                    self.recycle(delivered);
                }
                return Vec::new();
            }
        };
        peer.last_rx = Some(now);
        peer.last_rx_session = Some(header.session_id);
        peer.note_source(from);

        let mut actions = Vec::new();
        // 待确认会话收到认证帧，说明对端确实持有握手密钥，此时才替换现有会话。
        if peer
            .responder_pending
            .as_ref()
            .is_some_and(|session| session.session_id() == header.session_id)
        {
            let was_up = peer.has_session();
            if let Some(session) = peer.responder_pending.take()
                && let Some(previous) = peer.responder.replace(session)
            {
                self.sessions.remove(&previous.session_id());
            }
            peer.pending_ping = None;
            peer.candidates.clear();
            peer.candidate_index = 0;
            if !was_up {
                actions.push(Action::PeerUp { peer_id });
            }
        }
        match header.kind {
            FrameKind::Data => {
                let Some(mut delivered) = delivered else {
                    return actions;
                };
                delivered.truncate(len);
                let valid = ipv4_addresses(&delivered).is_some_and(|(source, destination)| {
                    source == peer.record.ipv4 && destination == self.params.ipv4
                });
                if !valid {
                    debug!(peer_id, "dropping packet with mismatched addresses");
                    self.recycle(delivered);
                    return actions;
                }
                peer.rx_bytes += len as u64;
                actions.push(Action::Deliver(delivered));
            }
            FrameKind::Keepalive => {}
            FrameKind::Ping => {
                let Ok(sequence) = decode_sequence(&self.open_buf[..len]) else {
                    trace!(peer_id, "dropping malformed ping");
                    return actions;
                };
                let mut datagram =
                    take_buffer(&mut self.buffers, HEADER_LEN + DATA_NONCE_LEN + 8 + TAG_LEN);
                let sealed = peer.outbound_mut().and_then(|session| {
                    match session.seal(FrameKind::Pong, &encode_sequence(sequence), &mut datagram) {
                        Ok(sent) => {
                            datagram.truncate(sent);
                            Some(())
                        }
                        Err(err) => {
                            debug!(peer_id, error = %err, "cannot seal pong");
                            None
                        }
                    }
                });
                if sealed.is_some() {
                    actions.push(Action::Send { to: from, datagram });
                }
            }
            FrameKind::Pong => {
                if let Ok(sequence) = decode_sequence(&self.open_buf[..len])
                    && peer
                        .pending_ping
                        .is_some_and(|(expected, _)| expected == sequence)
                    && let Some((_, sent)) = peer.pending_ping.take()
                {
                    let rtt_us = now.saturating_duration_since(sent).as_micros();
                    peer.rtt_us = Some(u32::try_from(rtt_us).unwrap_or(u32::MAX));
                }
            }
            FrameKind::HandshakeInit | FrameKind::HandshakeResp => {}
        }
        actions
    }

    fn install_session(
        &mut self,
        peer_id: u64,
        role: Role,
        session: NoiseSession,
        now: Instant,
    ) -> Option<Action> {
        let session_id = session.session_id();
        let peer = self.peers.get_mut(&peer_id)?;
        if role == Role::Responder {
            // 握手 init 可能是重放报文。先保存为待确认会话，收到认证帧后再替换现有会话。
            if let Some(previous) = peer.responder_pending.replace(session) {
                self.sessions.remove(&previous.session_id());
            }
            self.sessions.insert(session_id, peer_id);
            return None;
        }
        let was_up = peer.has_session();
        let previous = peer.initiator.replace(session);
        if let Some(previous) = previous {
            self.sessions.remove(&previous.session_id());
        }
        peer.pending = None;
        peer.candidates.clear();
        peer.candidate_index = 0;
        peer.last_rx = Some(now);
        peer.last_rx_session = Some(session_id);
        peer.next_ping = now;
        peer.pending_ping = None;
        self.sessions.insert(session_id, peer_id);
        if was_up {
            None
        } else {
            Some(Action::PeerUp { peer_id })
        }
    }

    fn pending_peer_at(&self, from: SocketAddr) -> Option<u64> {
        self.peers.iter().find_map(|(&peer_id, peer)| {
            (peer.pending.is_some() && peer.endpoint == Some(from)).then_some(peer_id)
        })
    }
}

/// 取一个至少 `len` byte 的缓冲区。复用长度足够的缓冲区时不清零。
fn take_buffer(pool: &mut Vec<Vec<u8>>, len: usize) -> Vec<u8> {
    let mut buffer = pool.pop().unwrap_or_default();
    if buffer.len() < len {
        buffer.resize(len, 0);
    }
    buffer
}

/// 解析 IPv4 包的源地址和目的地址。只接受版本 4 且头部完整的包。
fn ipv4_addresses(packet: &[u8]) -> Option<(Ipv4Addr, Ipv4Addr)> {
    if packet.len() < 20 || (packet[0] >> 4) != 4 {
        return None;
    }
    let header_len = usize::from(packet[0] & 0x0f) * 4;
    if header_len < 20 || packet.len() < header_len {
        return None;
    }
    Some((
        Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]),
        Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> MeshParams {
        MeshParams {
            name: "uutier".to_owned(),
            secret: "hidden".to_owned(),
            ipv4: Ipv4Addr::new(10, 144, 0, 2),
            mtu_bytes: 1280,
            listen_port: 12010,
        }
    }

    fn mesh_with_peer() -> (Mesh, u64) {
        let now = Instant::now();
        let mut mesh = Mesh::new(params(), Identity::generate(), Vec::new(), now).unwrap();
        let peer = Identity::generate();
        let record = PeerRecord::new(*peer.public_key(), Ipv4Addr::new(10, 144, 0, 3), None);
        let peer_id = record.peer_id;
        assert!(mesh.add_peer(record, now).unwrap());
        (mesh, peer_id)
    }

    #[test]
    fn candidates_skip_unusable_addresses() {
        let (mut mesh, peer_id) = mesh_with_peer();
        let endpoints: Vec<SocketAddr> = [
            "127.0.0.1:12010",
            "192.0.2.5:12010",
            "192.168.1.5:12010",
            "192.168.1.5:12010",
        ]
        .iter()
        .map(|value| value.parse().unwrap())
        .collect();
        assert!(mesh.set_peer_candidates(peer_id, &endpoints, Instant::now()));
        assert_eq!(
            mesh.peer_endpoint(peer_id),
            Some("192.168.1.5:12010".parse().unwrap())
        );
        assert!(!mesh.set_peer_candidates(peer_id, &endpoints[..2], Instant::now()));
    }

    #[test]
    fn debug_hides_peer_key_endpoint_and_payload() {
        let record = PeerRecord::new(
            [7u8; IDENTITY_KEY_LEN],
            Ipv4Addr::new(10, 144, 0, 3),
            Some("192.0.2.5:12010".parse().unwrap()),
        );
        let text = format!("{record:?}");
        assert!(!text.contains("192.0.2.5"), "{text}");
        assert!(!text.contains("public_key"), "{text}");
        assert!(text.contains("10.144.0.3"), "{text}");

        let action = Action::Send {
            to: "192.0.2.5:12010".parse().unwrap(),
            datagram: vec![1, 2, 3],
        };
        let text = format!("{action:?}");
        assert!(!text.contains("192.0.2.5"), "{text}");
        assert!(!text.contains("1, 2, 3"), "{text}");
        assert!(text.contains("datagram_bytes: 3"), "{text}");
    }

    fn noise_session() -> NoiseSession {
        let initiator_identity = Identity::generate();
        let responder_identity = Identity::generate();
        let psk = psk_from_secret("hidden");
        let mut initiator =
            InitiatorHandshake::new(&initiator_identity, responder_identity.public_key(), &psk)
                .unwrap();
        let mut responder =
            ResponderHandshake::new(&responder_identity, initiator_identity.public_key(), &psk)
                .unwrap();
        let init = initiator
            .write_init(HandshakePayload {
                ipv4: Ipv4Addr::new(10, 144, 0, 2),
                listen_port: 12010,
            })
            .unwrap();
        responder.read_init(&init).unwrap();
        let response = responder
            .write_response(HandshakePayload {
                ipv4: Ipv4Addr::new(10, 144, 0, 3),
                listen_port: 12011,
            })
            .unwrap();
        initiator.read_response(&response).unwrap();
        initiator.into_session().unwrap()
    }

    #[test]
    fn duplicate_candidates_keep_the_established_session() {
        let (mut mesh, peer_id) = mesh_with_peer();
        let now = Instant::now();
        let first: SocketAddr = "192.168.1.5:12010".parse().unwrap();
        let second: SocketAddr = "192.168.1.6:12010".parse().unwrap();
        assert!(mesh.set_peer_candidates(peer_id, &[first], now));
        let session = noise_session();
        let session_id = session.session_id();
        assert!(
            mesh.install_session(peer_id, Role::Initiator, session, now)
                .is_some()
        );
        let next_handshake = mesh.peers[&peer_id].next_handshake;

        // 发现循环每 10 s 重复上报候选，不能重置已建立会话的端点或握手计时。
        assert!(mesh.set_peer_candidates(peer_id, &[second], now + Duration::from_secs(10)));
        assert_eq!(mesh.peer_endpoint(peer_id), Some(first));
        assert_eq!(mesh.peers[&peer_id].next_handshake, next_handshake);
        assert!(mesh.sessions.contains_key(&session_id));
    }

    #[test]
    fn replayed_handshake_init_does_not_move_the_endpoint() {
        let now = Instant::now();
        let mut mesh = Mesh::new(params(), Identity::generate(), Vec::new(), now).unwrap();
        let peer_identity = Identity::generate();
        let endpoint: SocketAddr = "192.168.1.5:12010".parse().unwrap();
        let record = PeerRecord::new(
            *peer_identity.public_key(),
            Ipv4Addr::new(10, 144, 0, 3),
            Some(endpoint),
        );
        let peer_id = record.peer_id;
        assert!(mesh.add_peer(record, now).unwrap());

        let psk = psk_from_secret("hidden");
        let mut initiator =
            InitiatorHandshake::new(&peer_identity, mesh.identity.public_key(), &psk).unwrap();
        let init = initiator
            .write_init(HandshakePayload {
                ipv4: Ipv4Addr::new(10, 144, 0, 3),
                listen_port: 12010,
            })
            .unwrap();
        let mut datagram = vec![0u8; 256];
        let len = encode(
            FrameKind::HandshakeInit,
            mesh.local_peer_id(),
            &init,
            &mut datagram,
        )
        .unwrap();

        let spoofed: SocketAddr = "203.0.113.99:40000".parse().unwrap();
        let actions = mesh.handle_datagram(now, spoofed, &datagram[..len]);
        let response = actions
            .into_iter()
            .find_map(|action| match action {
                Action::Send { datagram, .. } => Some(datagram),
                _ => None,
            })
            .unwrap();
        assert_eq!(mesh.peer_endpoint(peer_id), Some(endpoint));
        assert!(mesh.peers[&peer_id].responder_pending.is_some());

        // 握手响应是新的握手，不受重放的 init 影响。认证帧到达后才更新端点。
        initiator.read_response(&response[HEADER_LEN..]).unwrap();
        let mut session = initiator.into_session().unwrap();
        let mut frame = vec![0u8; 128];
        let len = session
            .seal(FrameKind::Ping, &encode_sequence(7), &mut frame)
            .unwrap();
        let real: SocketAddr = "192.168.1.7:12020".parse().unwrap();
        let actions = mesh.handle_datagram(now, real, &frame[..len]);
        assert_eq!(mesh.peer_endpoint(peer_id), Some(real));
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, Action::Send { to, .. } if *to == real))
        );
    }

    #[test]
    fn handshake_payload_ipv4_must_match_the_record() {
        let now = Instant::now();
        let mut mesh = Mesh::new(params(), Identity::generate(), Vec::new(), now).unwrap();
        let peer_identity = Identity::generate();
        let record = PeerRecord::new(
            *peer_identity.public_key(),
            Ipv4Addr::new(10, 144, 0, 3),
            None,
        );
        let peer_id = record.peer_id;
        mesh.add_peer(record, now).unwrap();

        let psk = psk_from_secret("hidden");
        let mut initiator =
            InitiatorHandshake::new(&peer_identity, mesh.identity.public_key(), &psk).unwrap();
        let init = initiator
            .write_init(HandshakePayload {
                ipv4: Ipv4Addr::new(10, 144, 0, 4),
                listen_port: 12010,
            })
            .unwrap();
        let mut datagram = vec![0u8; 256];
        let len = encode(
            FrameKind::HandshakeInit,
            mesh.local_peer_id(),
            &init,
            &mut datagram,
        )
        .unwrap();
        let from: SocketAddr = "192.168.1.5:12010".parse().unwrap();

        let actions = mesh.handle_datagram(now, from, &datagram[..len]);
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::Send { .. }))
        );
        assert!(mesh.peers[&peer_id].responder_pending.is_none());
    }

    #[test]
    fn data_frames_must_match_the_peer_addresses() {
        let now = Instant::now();
        let mut mesh = Mesh::new(params(), Identity::generate(), Vec::new(), now).unwrap();
        let peer_identity = Identity::generate();
        let record = PeerRecord::new(
            *peer_identity.public_key(),
            Ipv4Addr::new(10, 144, 0, 3),
            None,
        );
        mesh.add_peer(record, now).unwrap();

        let psk = psk_from_secret("hidden");
        let from: SocketAddr = "192.168.1.5:12010".parse().unwrap();
        let mut initiator =
            InitiatorHandshake::new(&peer_identity, mesh.identity.public_key(), &psk).unwrap();
        let init = initiator
            .write_init(HandshakePayload {
                ipv4: Ipv4Addr::new(10, 144, 0, 3),
                listen_port: 12010,
            })
            .unwrap();
        let mut datagram = vec![0u8; 256];
        let len = encode(
            FrameKind::HandshakeInit,
            mesh.local_peer_id(),
            &init,
            &mut datagram,
        )
        .unwrap();
        let response = mesh
            .handle_datagram(now, from, &datagram[..len])
            .into_iter()
            .find_map(|action| match action {
                Action::Send { datagram, .. } => Some(datagram),
                _ => None,
            })
            .unwrap();
        initiator.read_response(&response[HEADER_LEN..]).unwrap();
        let mut session = initiator.into_session().unwrap();

        let mut frame = vec![0u8; 128];
        let len = session
            .seal(FrameKind::Ping, &encode_sequence(1), &mut frame)
            .unwrap();
        mesh.handle_datagram(now, from, &frame[..len]);

        let mut packet = [0u8; 20];
        packet[0] = 0x45;
        packet[2..4].copy_from_slice(&20u16.to_be_bytes());
        packet[9] = 17;
        packet[12..16].copy_from_slice(&[10, 144, 0, 4]);
        packet[16..20].copy_from_slice(&[10, 144, 0, 2]);
        let mut frame = vec![0u8; 128];
        let len = session.seal(FrameKind::Data, &packet, &mut frame).unwrap();
        let actions = mesh.handle_datagram(now, from, &frame[..len]);
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::Deliver(_)))
        );

        packet[12..16].copy_from_slice(&[10, 144, 0, 3]);
        let mut frame = vec![0u8; 128];
        let len = session.seal(FrameKind::Data, &packet, &mut frame).unwrap();
        let actions = mesh.handle_datagram(now, from, &frame[..len]);
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, Action::Deliver(_)))
        );
    }

    #[test]
    fn add_peer_rejects_conflicting_records() {
        let now = Instant::now();
        let mut mesh = Mesh::new(params(), Identity::generate(), Vec::new(), now).unwrap();
        let local = PeerRecord::new(
            *Identity::generate().public_key(),
            Ipv4Addr::new(10, 144, 0, 2),
            None,
        );
        assert!(matches!(
            mesh.add_peer(local, now),
            Err(MeshError::PeerIsLocal)
        ));

        let peer = Identity::generate();
        let record = PeerRecord::new(*peer.public_key(), Ipv4Addr::new(10, 144, 0, 3), None);
        let peer_id = record.peer_id;
        assert!(mesh.add_peer(record, now).unwrap());
        let changed = PeerRecord::new(*peer.public_key(), Ipv4Addr::new(10, 144, 0, 4), None);
        assert!(matches!(
            mesh.add_peer(changed, now),
            Err(MeshError::PeerIpv4Changed(id)) if id == peer_id
        ));

        let duplicate = Identity::generate();
        let record = PeerRecord::new(*duplicate.public_key(), Ipv4Addr::new(10, 144, 0, 3), None);
        assert!(matches!(
            mesh.add_peer(record, now),
            Err(MeshError::DuplicatePeerIpv4(ipv4)) if ipv4 == Ipv4Addr::new(10, 144, 0, 3)
        ));
    }

    #[test]
    fn switching_to_turn_marks_the_path() {
        let (mut mesh, peer_id) = mesh_with_peer();
        let now = Instant::now();
        let bridge: SocketAddr = "127.0.0.1:41000".parse().unwrap();
        let direct: SocketAddr = "192.168.1.5:12010".parse().unwrap();
        assert!(mesh.set_peer_transport(peer_id, bridge, true, now));
        assert_eq!(mesh.peer_path(peer_id), Some(PathKind::Turn));
        assert!(mesh.set_turn_endpoint(peer_id, bridge));
        assert_eq!(mesh.peer_path(peer_id), Some(PathKind::Turn));
        assert!(mesh.set_peer_transport(peer_id, direct, false, now));
        assert_eq!(mesh.peer_path(peer_id), Some(PathKind::Direct));
    }
}
