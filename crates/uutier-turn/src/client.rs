//! TURN 客户端。实现 UDP 分配、刷新、权限和收发。

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::ops::Range;
use std::sync::Mutex;
use std::time::Duration;

use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use sha1::Sha1;
use tokio::net::UdpSocket;
use tracing::debug;

use uutier_core::REDACTED;

use crate::error::TurnError;

const MAGIC_COOKIE: u32 = 0x2112_a442;
const HEADER_LEN: usize = 20;
const TRANSACTION_ID_LEN: usize = 12;
const MESSAGE_INTEGRITY_LEN: usize = 20;
const MESSAGE_INTEGRITY_ATTR_LEN: usize = 4 + MESSAGE_INTEGRITY_LEN;
const REQUESTED_TRANSPORT_UDP: [u8; 4] = [17, 0, 0, 0];
const DEFAULT_LIFETIME_S: u32 = 600;
const TURN_PACKET_BYTES: usize = 64 * 1024;
const RETRANSMIT_INTERVAL: Duration = Duration::from_millis(500);

const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_ERROR_CODE: u16 = 0x0009;
const ATTR_LIFETIME: u16 = 0x000d;
const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
const ATTR_DATA: u16 = 0x0013;
const ATTR_REALM: u16 = 0x0014;
const ATTR_NONCE: u16 = 0x0015;
const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;

const ALLOCATE_REQUEST: u16 = 0x0003;
const REFRESH_REQUEST: u16 = 0x0004;
const CREATE_PERMISSION_REQUEST: u16 = 0x0008;
const SEND_INDICATION: u16 = 0x0016;
const DATA_INDICATION: u16 = 0x0017;

/// TURN 服务器和凭据。
#[derive(Clone)]
pub struct TurnServerConfig {
    /// `turn:` 或 `turns:` URL。
    pub server: String,
    pub username: String,
    pub password: String,
    pub realm: Option<String>,
    /// TURN 请求超时，单位 s。
    pub request_timeout_s: u64,
}

impl fmt::Debug for TurnServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnServerConfig")
            .field("server", &self.server)
            .field("username", &REDACTED)
            .field("password", &REDACTED)
            .field("realm", &self.realm)
            .field("request_timeout_s", &self.request_timeout_s)
            .finish()
    }
}

/// 一条 TURN 分配。
pub struct TurnAllocation {
    socket: UdpSocket,
    relay_addr: SocketAddr,
    lifetime_s: u32,
    auth: Mutex<Option<Auth>>,
    timeout: Duration,
    /// Send Indication 的复用缓冲区。数据路径每个包都会用到。
    send_buf: Vec<u8>,
    /// Send Indication 不需要响应，事务号用计数器生成，避免每个包请求随机数。
    indication_sequence: u64,
    /// 已发出、等待响应的刷新或权限请求。同一时间最多一个。
    control: Option<PendingControl>,
}

/// 收到的报文。
pub(crate) enum TurnEvent {
    /// 一条要转发的 Data Indication，`from` 是 TURN 报文里声明的对端地址。
    Data {
        from: SocketAddr,
        payload: Range<usize>,
    },
    /// 分配刷新完成。
    Refreshed,
    /// 权限创建完成。
    PermissionCreated,
    /// 控制请求失败。
    ControlFailed(TurnError),
    /// 没有需要处理的内容。
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlKind {
    Refresh,
    Permission(SocketAddr),
}

struct PendingControl {
    kind: ControlKind,
    transaction_id: [u8; TRANSACTION_ID_LEN],
    encoded: Vec<u8>,
    /// 整体超时时间。
    deadline: tokio::time::Instant,
    /// 下一次重发时间。
    resend_at: tokio::time::Instant,
}

impl Drop for TurnAllocation {
    fn drop(&mut self) {
        // 尽力让服务器立即回收分配。Drop 不能等待响应，只发送一次 Refresh，lifetime 为 0。
        let Ok(auth) = self.lock_auth() else {
            return;
        };
        let Some(auth) = auth.as_ref() else {
            return;
        };
        let mut attributes = auth.attributes();
        attributes.push(Attribute::new(ATTR_LIFETIME, 0u32.to_be_bytes().to_vec()));
        let Ok(transaction_id) = random_transaction_id() else {
            return;
        };
        let message = Message::new(REFRESH_REQUEST, transaction_id, attributes);
        if let Ok(encoded) = message.encode_with_integrity(&auth.key) {
            let _ = self.socket.try_send(&encoded);
        }
    }
}

/// 创建一条 TURN 分配。
pub async fn allocate(config: &TurnServerConfig) -> Result<TurnAllocation, TurnError> {
    TurnAllocation::allocate(config).await
}

impl TurnAllocation {
    /// 创建一条 TURN 分配。只支持 UDP。
    pub async fn allocate(config: &TurnServerConfig) -> Result<Self, TurnError> {
        let timeout = Duration::from_secs(config.request_timeout_s.max(1));
        let server = resolve_server(&config.server, timeout).await?;
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
        socket.connect(server).await?;
        let seed = random_transaction_id()?;
        let mut seed_bytes = [0u8; 8];
        seed_bytes.copy_from_slice(&seed[4..]);
        let mut allocation = Self {
            socket,
            relay_addr: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            lifetime_s: 0,
            auth: Mutex::new(None),
            timeout,
            send_buf: Vec::with_capacity(2 * 1024),
            indication_sequence: u64::from_be_bytes(seed_bytes),
            control: None,
        };
        allocation
            .allocate_inner(&config.username, &config.password)
            .await?;
        Ok(allocation)
    }

    /// 返回 TURN 服务器分配的 relay 地址。
    pub fn relay_addr(&self) -> SocketAddr {
        self.relay_addr
    }

    /// 返回分配剩余时间，单位 s。
    pub fn lifetime_s(&self) -> u32 {
        self.lifetime_s
    }

    /// 返回本地 UDP 地址。
    pub fn local_addr(&self) -> Result<SocketAddr, TurnError> {
        Ok(self.socket.local_addr()?)
    }

    /// 发出刷新请求，不等待响应。已有请求未完成时不重复发送。
    ///
    /// 响应由 [`Self::recv_into`] 处理，完成后返回 [`TurnEvent::Refreshed`]。
    pub(crate) async fn begin_refresh(&mut self) -> Result<(), TurnError> {
        self.begin_control(ControlKind::Refresh).await
    }

    /// 发出权限请求，不等待响应。
    ///
    /// 响应由 [`Self::recv_into`] 处理，完成后返回 [`TurnEvent::PermissionCreated`]。
    pub(crate) async fn begin_permission(&mut self, peer: SocketAddr) -> Result<(), TurnError> {
        self.begin_control(ControlKind::Permission(peer)).await
    }

    /// 返回当前控制请求的整体超时时间。
    pub(crate) fn control_deadline(&self) -> Option<tokio::time::Instant> {
        self.control.as_ref().map(|control| control.deadline)
    }

    /// 返回下一次需要处理控制请求的时间。
    pub(crate) fn control_wake(&self) -> Option<tokio::time::Instant> {
        self.control
            .as_ref()
            .map(|control| control.resend_at.min(control.deadline))
    }

    /// 重发当前控制请求。请求不存在时不做任何事。
    pub(crate) async fn resend_control(&mut self) -> Result<(), TurnError> {
        let Some(control) = self.control.as_mut() else {
            return Ok(());
        };
        self.socket.send(&control.encoded).await?;
        control.resend_at = tokio::time::Instant::now() + RETRANSMIT_INTERVAL;
        Ok(())
    }

    /// 为对端地址建立权限。用于数据路径启动前的一次性调用。
    pub async fn create_permission(&mut self, peer: SocketAddr) -> Result<(), TurnError> {
        let attributes = [Attribute::new(
            ATTR_XOR_PEER_ADDRESS,
            encode_xor_address(peer)?.to_vec(),
        )];
        self.request(CREATE_PERMISSION_REQUEST, &attributes).await?;
        Ok(())
    }

    /// 通过发送指示向对端发送数据。
    ///
    /// 数据路径每包调用一次，报文直接写入复用缓冲区。
    pub async fn send_to(&mut self, peer: SocketAddr, payload: &[u8]) -> Result<(), TurnError> {
        let peer_address = encode_xor_address(peer)?;
        let data_len = u16::try_from(payload.len())
            .map_err(|_| TurnError::Protocol("data attribute too long"))?;
        let padded = payload.len().div_ceil(4) * 4;
        let attributes_len = 4 + peer_address.len() + 4 + padded;
        let length =
            u16::try_from(attributes_len).map_err(|_| TurnError::Protocol("message too long"))?;
        let total = HEADER_LEN + attributes_len;

        self.indication_sequence = self.indication_sequence.wrapping_add(1);
        let mut transaction_id = [0u8; TRANSACTION_ID_LEN];
        transaction_id[4..].copy_from_slice(&self.indication_sequence.to_be_bytes());

        let buffer = &mut self.send_buf;
        buffer.clear();
        buffer.reserve(total);
        buffer.extend_from_slice(&SEND_INDICATION.to_be_bytes());
        buffer.extend_from_slice(&length.to_be_bytes());
        buffer.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        buffer.extend_from_slice(&transaction_id);
        buffer.extend_from_slice(&ATTR_XOR_PEER_ADDRESS.to_be_bytes());
        buffer.extend_from_slice(
            &u16::try_from(peer_address.len())
                .map_err(|_| TurnError::Protocol("invalid peer address length"))?
                .to_be_bytes(),
        );
        buffer.extend_from_slice(&peer_address);
        buffer.extend_from_slice(&ATTR_DATA.to_be_bytes());
        buffer.extend_from_slice(&data_len.to_be_bytes());
        buffer.extend_from_slice(payload);
        buffer.resize(total, 0);
        self.socket.send(buffer).await?;
        Ok(())
    }

    /// 接收一条报文，处理数据转发和当前控制请求的响应。
    ///
    /// 载荷留在 `packet` 里，调用方直接发送这一段，不再复制。控制请求未完成时
    /// 到达的 Data Indication 仍然返回，不会被丢弃。格式错误的报文只记录并忽略。
    pub(crate) async fn recv_into(&mut self, packet: &mut [u8]) -> Result<TurnEvent, TurnError> {
        let len = self.socket.recv(packet).await?;
        let bytes = &packet[..len];
        if len >= 2 && u16::from_be_bytes([bytes[0], bytes[1]]) == DATA_INDICATION {
            return match parse_data_indication_packet(bytes) {
                Ok(Some((from, payload))) => Ok(TurnEvent::Data { from, payload }),
                Ok(None) => Ok(TurnEvent::None),
                Err(error) => {
                    debug!(%error, "dropping malformed TURN data indication");
                    Ok(TurnEvent::None)
                }
            };
        }
        self.accept_control_response(bytes).await
    }

    async fn begin_control(&mut self, kind: ControlKind) -> Result<(), TurnError> {
        if self.control.is_some() {
            return Ok(());
        }
        let (transaction_id, encoded) = self.encode_control(kind)?;
        self.socket.send(&encoded).await?;
        let now = tokio::time::Instant::now();
        self.control = Some(PendingControl {
            kind,
            transaction_id,
            encoded,
            deadline: now + self.timeout,
            resend_at: now + RETRANSMIT_INTERVAL,
        });
        Ok(())
    }

    fn encode_control(
        &self,
        kind: ControlKind,
    ) -> Result<([u8; TRANSACTION_ID_LEN], Vec<u8>), TurnError> {
        let extra = match kind {
            ControlKind::Refresh => {
                vec![Attribute::new(
                    ATTR_LIFETIME,
                    DEFAULT_LIFETIME_S.to_be_bytes().to_vec(),
                )]
            }
            ControlKind::Permission(peer) => vec![Attribute::new(
                ATTR_XOR_PEER_ADDRESS,
                encode_xor_address(peer)?.to_vec(),
            )],
        };
        let auth = self.lock_auth()?.clone();
        let mut attributes = Vec::with_capacity(extra.len() + 3);
        if let Some(auth) = &auth {
            attributes.extend(auth.attributes());
        }
        attributes.extend(extra);
        let (request_kind, transaction_id) = match kind {
            ControlKind::Refresh => (REFRESH_REQUEST, random_transaction_id()?),
            ControlKind::Permission(_) => (CREATE_PERMISSION_REQUEST, random_transaction_id()?),
        };
        let message = Message::new(request_kind, transaction_id, attributes);
        let encoded = match &auth {
            Some(auth) => message.encode_with_integrity(&auth.key)?,
            None => message.encode()?,
        };
        Ok((transaction_id, encoded))
    }

    /// 把报文当作控制响应处理。事务号不匹配或没有请求在等待时忽略。
    async fn accept_control_response(&mut self, bytes: &[u8]) -> Result<TurnEvent, TurnError> {
        let Some((kind, transaction_id)) = self
            .control
            .as_ref()
            .map(|control| (control.kind, control.transaction_id))
        else {
            return Ok(TurnEvent::None);
        };
        let message = match Message::decode(bytes) {
            Ok(message) => message,
            Err(error) => {
                debug!(%error, "dropping malformed TURN control packet");
                return Ok(TurnEvent::None);
            }
        };
        if message.transaction_id != transaction_id {
            return Ok(TurnEvent::None);
        }
        if let Some(auth) = self.lock_auth()?.as_ref() {
            let verified = verify_message_integrity(bytes, &auth.key)?;
            if !verified && message.is_success() {
                return Err(TurnError::Protocol("response has no message integrity"));
            }
        }
        if message.is_error() {
            let code = message.error_code().unwrap_or(0);
            self.control = None;
            if code == 438 {
                // nonce 过期：更新认证信息后用新事务号重试。
                self.update_auth(&message)?;
                return match self.begin_control(kind).await {
                    Ok(()) => Ok(TurnEvent::None),
                    Err(error) => Ok(TurnEvent::ControlFailed(error)),
                };
            }
            return Ok(TurnEvent::ControlFailed(TurnError::ServerError { code }));
        }
        if !message.is_success() {
            self.control = None;
            return Ok(TurnEvent::ControlFailed(TurnError::Protocol(
                "unexpected response class",
            )));
        }
        self.update_auth(&message)?;
        self.control = None;
        match kind {
            ControlKind::Refresh => {
                if let Some(lifetime) = lifetime_attribute(&message)? {
                    self.lifetime_s = lifetime;
                }
                Ok(TurnEvent::Refreshed)
            }
            ControlKind::Permission(_) => Ok(TurnEvent::PermissionCreated),
        }
    }

    async fn allocate_inner(&mut self, username: &str, password: &str) -> Result<(), TurnError> {
        let attributes = [Attribute::new(
            ATTR_REQUESTED_TRANSPORT,
            REQUESTED_TRANSPORT_UDP.to_vec(),
        )];
        let transaction_id = random_transaction_id()?;
        let message = Message::new(ALLOCATE_REQUEST, transaction_id, attributes.to_vec());
        let encoded = message.encode()?;
        let response = self.exchange(&encoded, &transaction_id).await?;
        if response.is_error() {
            let code = response.error_code().unwrap_or(0);
            if code != 401 {
                return Err(TurnError::ServerError { code });
            }
            let realm = text_attribute(&response, ATTR_REALM)?;
            let nonce = text_attribute(&response, ATTR_NONCE)?;
            let auth = Auth::new(username.to_owned(), password.to_owned(), realm, nonce);
            *self.lock_auth()? = Some(auth);
        } else if !response.is_success() {
            return Err(TurnError::Protocol("unexpected allocate response"));
        } else if self.lock_auth()?.is_none() {
            // 服务器不应对未认证的 Allocate 直接返回成功。
            return Err(TurnError::Protocol("unauthenticated allocate response"));
        } else {
            self.apply_allocation(&response)?;
            return Ok(());
        }

        let response = self.request(ALLOCATE_REQUEST, &attributes).await?;
        self.apply_allocation(&response)
    }

    fn apply_allocation(&mut self, response: &Message) -> Result<(), TurnError> {
        let value = response
            .attribute(ATTR_XOR_RELAYED_ADDRESS)
            .ok_or(TurnError::Protocol("missing relay address"))?;
        self.relay_addr = decode_xor_address(value, &response.transaction_id)?;
        self.lifetime_s = lifetime_attribute(response)?.unwrap_or(DEFAULT_LIFETIME_S);
        Ok(())
    }

    /// 发送一个已编码的请求，按固定间隔重传，直到收到同一事务的响应或整体超时。
    async fn exchange(
        &self,
        encoded: &[u8],
        transaction_id: &[u8; TRANSACTION_ID_LEN],
    ) -> Result<Message, TurnError> {
        let Some(deadline) = tokio::time::Instant::now().checked_add(self.timeout) else {
            return Err(TurnError::Timeout);
        };
        // UDP 请求可能丢失，按固定间隔重传同一个事务，直到整体超时。
        loop {
            self.socket.send(encoded).await?;
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(TurnError::Timeout);
            }
            let wait = RETRANSMIT_INTERVAL.min(deadline - now);
            match self.receive_until_for(transaction_id, wait).await {
                Ok(response) => return Ok(response),
                Err(TurnError::Timeout) => {}
                Err(error) => return Err(error),
            }
        }
    }

    async fn request(&self, kind: u16, extra: &[Attribute]) -> Result<Message, TurnError> {
        let mut attempt = 0;
        loop {
            let auth = self.lock_auth()?.clone();
            let mut attributes = Vec::with_capacity(extra.len() + 3);
            if let Some(auth) = &auth {
                attributes.extend(auth.attributes());
            }
            attributes.extend_from_slice(extra);
            let transaction_id = random_transaction_id()?;
            let message = Message::new(kind, transaction_id, attributes);
            let encoded = match &auth {
                Some(auth) => message.encode_with_integrity(&auth.key)?,
                None => message.encode()?,
            };
            let response = self.exchange(&encoded, &transaction_id).await?;
            if response.is_error() {
                let code = response.error_code().unwrap_or(0);
                if code == 438 && attempt == 0 {
                    self.update_auth(&response)?;
                    attempt += 1;
                    continue;
                }
                return Err(TurnError::ServerError { code });
            }
            if !response.is_success() {
                return Err(TurnError::Protocol("unexpected response class"));
            }
            self.update_auth(&response)?;
            return Ok(response);
        }
    }

    async fn receive_until_for(
        &self,
        transaction_id: &[u8; TRANSACTION_ID_LEN],
        wait: Duration,
    ) -> Result<Message, TurnError> {
        let Some(deadline) = tokio::time::Instant::now().checked_add(wait) else {
            return Err(TurnError::Timeout);
        };
        let mut packet = vec![0u8; TURN_PACKET_BYTES];
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(TurnError::Timeout);
            }
            let len = match tokio::time::timeout(remaining, self.socket.recv(&mut packet)).await {
                Ok(result) => result?,
                Err(_) => return Err(TurnError::Timeout),
            };
            // 控制请求期间到达的数据不属于响应，直接跳过。
            if len >= 2 && u16::from_be_bytes([packet[0], packet[1]]) == DATA_INDICATION {
                continue;
            }
            let message = match Message::decode(&packet[..len]) {
                Ok(message) => message,
                Err(_) => continue,
            };
            if message.transaction_id == *transaction_id {
                if let Some(auth) = self.lock_auth()?.as_ref() {
                    let verified = verify_message_integrity(&packet[..len], &auth.key)?;
                    if !verified && message.is_success() {
                        return Err(TurnError::Protocol("response has no message integrity"));
                    }
                }
                return Ok(message);
            }
        }
    }

    fn update_auth(&self, response: &Message) -> Result<(), TurnError> {
        let mut guard = self.lock_auth()?;
        if let Some(auth) = guard.as_mut() {
            auth.update(response)?;
        }
        Ok(())
    }

    fn lock_auth(&self) -> Result<std::sync::MutexGuard<'_, Option<Auth>>, TurnError> {
        self.auth.lock().map_err(|_| TurnError::Internal)
    }
}

impl fmt::Debug for TurnAllocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnAllocation")
            .field("relay_addr", &REDACTED)
            .field("lifetime_s", &self.lifetime_s)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
impl TurnAllocation {
    /// 构造只用于测试的分配。
    pub(crate) fn for_test(socket: UdpSocket, relay_addr: SocketAddr, timeout: Duration) -> Self {
        Self {
            socket,
            relay_addr,
            lifetime_s: DEFAULT_LIFETIME_S,
            auth: Mutex::new(None),
            timeout,
            send_buf: Vec::new(),
            indication_sequence: 0,
            control: None,
        }
    }
}

struct Auth {
    username: String,
    password: String,
    realm: String,
    nonce: String,
    key: Vec<u8>,
}

impl Clone for Auth {
    fn clone(&self) -> Self {
        Self {
            username: self.username.clone(),
            password: self.password.clone(),
            realm: self.realm.clone(),
            nonce: self.nonce.clone(),
            key: self.key.clone(),
        }
    }
}

impl Auth {
    fn new(username: String, password: String, realm: String, nonce: String) -> Self {
        let key = auth_key(&username, &realm, &password);
        Self {
            username,
            password,
            realm,
            nonce,
            key,
        }
    }

    fn attributes(&self) -> Vec<Attribute> {
        vec![
            Attribute::new(ATTR_USERNAME, self.username.as_bytes().to_vec()),
            Attribute::new(ATTR_REALM, self.realm.as_bytes().to_vec()),
            Attribute::new(ATTR_NONCE, self.nonce.as_bytes().to_vec()),
        ]
    }

    fn update(&mut self, message: &Message) -> Result<(), TurnError> {
        let mut realm_changed = false;
        if let Some(value) = message.attribute(ATTR_REALM) {
            let realm = std::str::from_utf8(value)
                .map_err(|_| TurnError::Protocol("invalid realm"))?
                .to_owned();
            if realm != self.realm {
                self.realm = realm;
                realm_changed = true;
            }
        }
        if let Some(value) = message.attribute(ATTR_NONCE) {
            self.nonce = std::str::from_utf8(value)
                .map_err(|_| TurnError::Protocol("invalid nonce"))?
                .to_owned();
        }
        if realm_changed {
            self.key = auth_key(&self.username, &self.realm, &self.password);
        }
        Ok(())
    }
}

fn auth_key(username: &str, realm: &str, password: &str) -> Vec<u8> {
    let mut hasher = Md5::new();
    hasher.update(username.as_bytes());
    hasher.update(b":");
    hasher.update(realm.as_bytes());
    hasher.update(b":");
    hasher.update(password.as_bytes());
    hasher.finalize().to_vec()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Attribute {
    kind: u16,
    value: Vec<u8>,
}

impl Attribute {
    fn new(kind: u16, value: Vec<u8>) -> Self {
        Self { kind, value }
    }

    fn encode(&self) -> Result<Vec<u8>, TurnError> {
        let length = u16::try_from(self.value.len())
            .map_err(|_| TurnError::Protocol("attribute too long"))?;
        let padded = self.value.len().div_ceil(4) * 4;
        let mut output = Vec::with_capacity(4 + padded);
        output.extend_from_slice(&self.kind.to_be_bytes());
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(&self.value);
        output.resize(4 + padded, 0);
        Ok(output)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Message {
    kind: u16,
    transaction_id: [u8; TRANSACTION_ID_LEN],
    attributes: Vec<Attribute>,
}

impl Message {
    fn new(
        kind: u16,
        transaction_id: [u8; TRANSACTION_ID_LEN],
        attributes: Vec<Attribute>,
    ) -> Self {
        Self {
            kind,
            transaction_id,
            attributes,
        }
    }

    fn attribute(&self, kind: u16) -> Option<&[u8]> {
        self.attributes
            .iter()
            .find(|attribute| attribute.kind == kind)
            .map(|attribute| attribute.value.as_slice())
    }

    fn is_error(&self) -> bool {
        self.kind & 0x0110 == 0x0110
    }

    fn is_success(&self) -> bool {
        self.kind & 0x0100 == 0x0100 && !self.is_error()
    }

    fn error_code(&self) -> Option<u16> {
        let value = self.attribute(ATTR_ERROR_CODE)?;
        // 第 3 byte 的高 5 bit 保留，只有低 3 bit 是错误类别。
        value
            .get(2..4)
            .map(|bytes| u16::from(bytes[0] & 0x07) * 100 + u16::from(bytes[1]))
    }

    fn encode(&self) -> Result<Vec<u8>, TurnError> {
        let body = self.encode_body()?;
        let length =
            u16::try_from(body.len()).map_err(|_| TurnError::Protocol("message too long"))?;
        let mut output = Vec::with_capacity(HEADER_LEN + body.len());
        output.extend_from_slice(&self.kind.to_be_bytes());
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        output.extend_from_slice(&self.transaction_id);
        output.extend_from_slice(&body);
        Ok(output)
    }

    fn encode_with_integrity(&self, key: &[u8]) -> Result<Vec<u8>, TurnError> {
        let body = self.encode_body()?;
        let length = u16::try_from(body.len() + MESSAGE_INTEGRITY_ATTR_LEN)
            .map_err(|_| TurnError::Protocol("message too long"))?;
        let mut output = Vec::with_capacity(HEADER_LEN + body.len() + MESSAGE_INTEGRITY_ATTR_LEN);
        output.extend_from_slice(&self.kind.to_be_bytes());
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        output.extend_from_slice(&self.transaction_id);
        output.extend_from_slice(&body);
        let mut mac = Hmac::<Sha1>::new_from_slice(key)
            .map_err(|_| TurnError::Protocol("invalid integrity key"))?;
        mac.update(&output);
        output.extend_from_slice(&ATTR_MESSAGE_INTEGRITY.to_be_bytes());
        output.extend_from_slice(
            &u16::try_from(MESSAGE_INTEGRITY_LEN)
                .map_err(|_| TurnError::Protocol("invalid integrity length"))?
                .to_be_bytes(),
        );
        output.extend_from_slice(&mac.finalize().into_bytes());
        Ok(output)
    }

    fn encode_body(&self) -> Result<Vec<u8>, TurnError> {
        let mut body = Vec::new();
        for attribute in &self.attributes {
            body.extend_from_slice(&attribute.encode()?);
        }
        Ok(body)
    }

    fn decode(bytes: &[u8]) -> Result<Self, TurnError> {
        if bytes.len() < HEADER_LEN {
            return Err(TurnError::Protocol("message shorter than header"));
        }
        let kind = u16::from_be_bytes([bytes[0], bytes[1]]);
        let length = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
        let magic = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if magic != MAGIC_COOKIE {
            return Err(TurnError::Protocol("invalid magic cookie"));
        }
        if bytes.len() < HEADER_LEN + length {
            return Err(TurnError::Protocol("truncated message"));
        }
        let mut transaction_id = [0u8; TRANSACTION_ID_LEN];
        transaction_id.copy_from_slice(&bytes[8..HEADER_LEN]);
        let end = HEADER_LEN + length;
        let mut attributes = Vec::new();
        let mut offset = HEADER_LEN;
        while offset + 4 <= end {
            let attribute_kind = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
            let size = usize::from(u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]));
            let start = offset + 4;
            let stop = start + size;
            if stop > end {
                return Err(TurnError::Protocol("truncated attribute"));
            }
            attributes.push(Attribute::new(attribute_kind, bytes[start..stop].to_vec()));
            if attribute_kind == ATTR_MESSAGE_INTEGRITY {
                // HMAC 只覆盖到 MESSAGE-INTEGRITY 末尾，后续属性不参与认证。
                break;
            }
            offset = stop + (4 - size % 4) % 4;
        }
        Ok(Self {
            kind,
            transaction_id,
            attributes,
        })
    }
}

/// 验证 TURN 报文的 MESSAGE-INTEGRITY。报文没有该属性时返回 `false`。
fn verify_message_integrity(bytes: &[u8], key: &[u8]) -> Result<bool, TurnError> {
    if bytes.len() < HEADER_LEN {
        return Err(TurnError::Protocol("message shorter than header"));
    }
    let length = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
    let end = HEADER_LEN + length;
    if bytes.len() < end {
        return Err(TurnError::Protocol("truncated message"));
    }
    let mut offset = HEADER_LEN;
    let mut integrity = None;
    while offset + 4 <= end {
        let kind = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
        let size = usize::from(u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]));
        let start = offset + 4;
        let stop = start + size;
        if stop > end {
            return Err(TurnError::Protocol("truncated attribute"));
        }
        if kind == ATTR_MESSAGE_INTEGRITY {
            if size != MESSAGE_INTEGRITY_LEN {
                return Err(TurnError::Protocol("invalid integrity length"));
            }
            integrity = Some((offset, start..stop));
            break;
        }
        offset = stop + (4 - size % 4) % 4;
    }
    let Some((attribute_start, signature)) = integrity else {
        return Ok(false);
    };
    // 计算 MAC 时，头部长度字段只覆盖到 MESSAGE-INTEGRITY 属性末尾。
    let covered_len = attribute_start + MESSAGE_INTEGRITY_ATTR_LEN - HEADER_LEN;
    let covered_len =
        u16::try_from(covered_len).map_err(|_| TurnError::Protocol("invalid integrity length"))?;
    let mut covered = Vec::with_capacity(attribute_start);
    covered.extend_from_slice(&bytes[..attribute_start]);
    covered[2..4].copy_from_slice(&covered_len.to_be_bytes());
    let mut mac = Hmac::<Sha1>::new_from_slice(key)
        .map_err(|_| TurnError::Protocol("invalid integrity key"))?;
    mac.update(&covered);
    mac.verify_slice(&bytes[signature])
        .map_err(|_| TurnError::Protocol("invalid message integrity"))?;
    Ok(true)
}

/// 读取 LIFETIME 属性。属性长度不足 4 byte 时报协议错误。
fn lifetime_attribute(message: &Message) -> Result<Option<u32>, TurnError> {
    let Some(value) = message.attribute(ATTR_LIFETIME) else {
        return Ok(None);
    };
    let bytes: [u8; 4] = value
        .try_into()
        .map_err(|_| TurnError::Protocol("invalid lifetime attribute"))?;
    Ok(Some(u32::from_be_bytes(bytes)))
}

fn text_attribute(message: &Message, kind: u16) -> Result<String, TurnError> {
    let value = message
        .attribute(kind)
        .ok_or(TurnError::Protocol("missing text attribute"))?;
    Ok(std::str::from_utf8(value)
        .map_err(|_| TurnError::Protocol("invalid text attribute"))?
        .to_owned())
}

/// 在收到的 TURN 报文里定位 Data Indication 的载荷，不分配中间结构。
fn parse_data_indication_packet(
    packet: &[u8],
) -> Result<Option<(SocketAddr, Range<usize>)>, TurnError> {
    if packet.len() < HEADER_LEN {
        return Err(TurnError::Protocol("message shorter than header"));
    }
    let kind = u16::from_be_bytes([packet[0], packet[1]]);
    if kind != DATA_INDICATION {
        return Ok(None);
    }
    let length = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    let magic = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
    if magic != MAGIC_COOKIE {
        return Err(TurnError::Protocol("invalid magic cookie"));
    }
    let end = HEADER_LEN + length;
    if packet.len() < end {
        return Err(TurnError::Protocol("truncated message"));
    }
    let mut transaction_id = [0u8; TRANSACTION_ID_LEN];
    transaction_id.copy_from_slice(&packet[8..HEADER_LEN]);

    let mut peer = None;
    let mut data = None;
    let mut offset = HEADER_LEN;
    while offset + 4 <= end {
        let attribute_kind = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let size = usize::from(u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]));
        let start = offset + 4;
        let stop = start + size;
        if stop > end {
            return Err(TurnError::Protocol("truncated attribute"));
        }
        match attribute_kind {
            ATTR_XOR_PEER_ADDRESS => peer = Some(&packet[start..stop]),
            ATTR_DATA => data = Some(start..stop),
            _ => {}
        }
        offset = stop + (4 - size % 4) % 4;
    }

    let peer = peer.ok_or(TurnError::Protocol("missing peer address"))?;
    let data = data.ok_or(TurnError::Protocol("missing data"))?;
    let from = decode_xor_address(peer, &transaction_id)?;
    Ok(Some((from, data)))
}

fn random_transaction_id() -> Result<[u8; TRANSACTION_ID_LEN], TurnError> {
    let mut transaction_id = [0u8; TRANSACTION_ID_LEN];
    getrandom::fill(&mut transaction_id).map_err(|_| TurnError::Random)?;
    Ok(transaction_id)
}

fn encode_xor_address(address: SocketAddr) -> Result<[u8; 8], TurnError> {
    let SocketAddr::V4(address) = address else {
        return Err(TurnError::UnsupportedAddressFamily);
    };
    let mut output = [0u8; 8];
    output[1] = 1;
    output[2..4].copy_from_slice(&(address.port() ^ 0x2112).to_be_bytes());
    let mask = MAGIC_COOKIE.to_be_bytes();
    for (index, (byte, mask)) in address.ip().octets().iter().zip(mask.iter()).enumerate() {
        output[4 + index] = byte ^ mask;
    }
    Ok(output)
}

fn decode_xor_address(
    value: &[u8],
    transaction_id: &[u8; TRANSACTION_ID_LEN],
) -> Result<SocketAddr, TurnError> {
    let (prefix, rest) = value
        .split_first_chunk::<2>()
        .ok_or(TurnError::Protocol("short XOR address"))?;
    let family = prefix[1];
    let (port, rest) = rest
        .split_first_chunk::<2>()
        .ok_or(TurnError::Protocol("short XOR address"))?;
    let port = u16::from_be_bytes(*port) ^ 0x2112;
    match family {
        1 => {
            let (octets, _) = rest
                .split_first_chunk::<4>()
                .ok_or(TurnError::Protocol("short XOR address"))?;
            let mask = MAGIC_COOKIE.to_be_bytes();
            let ip = Ipv4Addr::new(
                octets[0] ^ mask[0],
                octets[1] ^ mask[1],
                octets[2] ^ mask[2],
                octets[3] ^ mask[3],
            );
            Ok(SocketAddr::from((ip, port)))
        }
        2 => {
            let (octets, _) = rest
                .split_first_chunk::<16>()
                .ok_or(TurnError::Protocol("short XOR address"))?;
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            mask[4..].copy_from_slice(transaction_id);
            let mut bytes = [0u8; 16];
            for (index, byte) in octets.iter().enumerate() {
                bytes[index] = byte ^ mask[index];
            }
            Ok(SocketAddr::from((Ipv6Addr::from(bytes), port)))
        }
        _ => Err(TurnError::UnsupportedAddressFamily),
    }
}

struct ServerUrl {
    host: String,
    port: u16,
}

fn parse_server_url(server: &str) -> Result<ServerUrl, TurnError> {
    let server = server.trim();
    let (scheme, rest) = server.split_once(':').ok_or(TurnError::InvalidServer)?;
    if scheme == "turns" {
        return Err(TurnError::UnsupportedTransport("TLS"));
    }
    if scheme != "turn" {
        return Err(TurnError::InvalidServer);
    }
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let (authority, query) = rest.split_once('?').unwrap_or((rest, ""));
    let authority = authority.trim_end_matches('/');
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        match pair {
            "transport=udp" => {}
            "transport=tcp" => return Err(TurnError::UnsupportedTransport("TCP")),
            "transport=tls" => return Err(TurnError::UnsupportedTransport("TLS")),
            _ => return Err(TurnError::InvalidServer),
        }
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            if host.is_empty() || host.contains(['[', ']', ':']) {
                return Err(TurnError::InvalidServer);
            }
            let port = port.parse::<u16>().map_err(|_| TurnError::InvalidServer)?;
            (host.to_owned(), port)
        }
        None => {
            if authority.is_empty() || authority.contains(['[', ']']) {
                return Err(TurnError::InvalidServer);
            }
            (authority.to_owned(), 3478)
        }
    };
    if host.is_empty() || port == 0 {
        return Err(TurnError::InvalidServer);
    }
    Ok(ServerUrl { host, port })
}

async fn resolve_server(server: &str, timeout: Duration) -> Result<SocketAddr, TurnError> {
    let url = parse_server_url(server)?;
    let lookup = tokio::net::lookup_host((url.host.as_str(), url.port));
    let mut addresses = tokio::time::timeout(timeout, lookup)
        .await
        .map_err(|_| TurnError::Timeout)?
        .map_err(TurnError::Resolve)?;
    addresses
        .find(SocketAddr::is_ipv4)
        .ok_or(TurnError::UnsupportedAddressFamily)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_turn_urls() {
        let url = parse_server_url("turn:turn.example.com:3478?transport=udp").unwrap();
        assert_eq!(url.host, "turn.example.com");
        assert_eq!(url.port, 3478);

        let url = parse_server_url("turn:203.0.113.10").unwrap();
        assert_eq!(url.host, "203.0.113.10");
        assert_eq!(url.port, 3478);
    }

    #[test]
    fn debug_redacts_credentials() {
        let config = TurnServerConfig {
            server: "turn:turn.example.com:3478".to_owned(),
            username: "turn-user".to_owned(),
            password: "turn-password".to_owned(),
            realm: Some("turn-realm".to_owned()),
            request_timeout_s: 5,
        };
        let text = format!("{config:?}");
        assert!(!text.contains("turn-user"));
        assert!(!text.contains("turn-password"));
        assert!(text.contains("turn.example.com"));
    }

    #[test]
    fn rejects_unsupported_transports() {
        assert!(matches!(
            parse_server_url("turns:turn.example.com:5349?transport=tcp"),
            Err(TurnError::UnsupportedTransport("TLS"))
        ));
        assert!(matches!(
            parse_server_url("turn:turn.example.com:3478?transport=tcp"),
            Err(TurnError::UnsupportedTransport("TCP"))
        ));
        assert!(parse_server_url("turn:").is_err());
        assert!(parse_server_url("turn:host:0").is_err());
    }

    #[test]
    fn attribute_length_excludes_padding() {
        let encoded = Attribute::new(ATTR_USERNAME, b"abc".to_vec())
            .encode()
            .unwrap();
        assert_eq!(&encoded[..2], &ATTR_USERNAME.to_be_bytes());
        assert_eq!(&encoded[2..4], &3u16.to_be_bytes());
        assert_eq!(encoded.len(), 8);
        assert_eq!(&encoded[4..7], b"abc");
        assert_eq!(encoded[7], 0);
    }

    #[test]
    fn xor_address_round_trip() {
        let address: SocketAddr = "203.0.113.10:12010".parse().unwrap();
        let encoded = encode_xor_address(address).unwrap();
        let decoded = decode_xor_address(&encoded, &[0u8; 12]).unwrap();
        assert_eq!(decoded, address);
    }

    #[test]
    fn parses_data_indication_payload_range() {
        let peer: SocketAddr = "203.0.113.10:12010".parse().unwrap();
        let message = Message::new(
            DATA_INDICATION,
            [7u8; 12],
            vec![
                Attribute::new(
                    ATTR_XOR_PEER_ADDRESS,
                    encode_xor_address(peer).unwrap().to_vec(),
                ),
                Attribute::new(ATTR_DATA, b"payload".to_vec()),
            ],
        );
        let encoded = message.encode().unwrap();
        let (from, range) = parse_data_indication_packet(&encoded).unwrap().unwrap();
        assert_eq!(from, peer);
        assert_eq!(&encoded[range], b"payload");

        let indication = Message::new(SEND_INDICATION, [0u8; 12], Vec::new())
            .encode()
            .unwrap();
        assert!(parse_data_indication_packet(&indication).unwrap().is_none());
    }

    #[tokio::test]
    async fn sends_data_indication_in_one_message() {
        let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = receiver.local_addr().unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        socket.connect(address).await.unwrap();
        let mut allocation = TurnAllocation {
            socket,
            relay_addr: address,
            lifetime_s: 0,
            auth: Mutex::new(None),
            timeout: Duration::from_secs(1),
            send_buf: Vec::new(),
            indication_sequence: 0,
            control: None,
        };

        allocation.send_to(address, b"hello").await.unwrap();
        let mut packet = [0u8; 128];
        let len = receiver.recv(&mut packet).await.unwrap();
        let message = Message::decode(&packet[..len]).unwrap();
        assert_eq!(message.kind, SEND_INDICATION);
        assert_eq!(message.attribute(ATTR_DATA), Some(b"hello".as_slice()));
        let peer = decode_xor_address(
            message.attribute(ATTR_XOR_PEER_ADDRESS).unwrap(),
            &message.transaction_id,
        )
        .unwrap();
        assert_eq!(peer, address);
        assert_eq!(allocation.indication_sequence, 1);
    }

    #[test]
    fn decodes_error_code() {
        let message = Message::new(
            0x0113,
            [0u8; 12],
            vec![Attribute::new(ATTR_ERROR_CODE, vec![0, 0, 4, 1, b'x'])],
        );
        assert!(message.is_error());
        assert!(!message.is_success());
        assert_eq!(message.error_code(), Some(401));

        let message = Message::new(
            0x0113,
            [0u8; 12],
            vec![Attribute::new(ATTR_ERROR_CODE, vec![0, 0, 0xfc, 38, b'x'])],
        );
        assert_eq!(message.error_code(), Some(438));
    }

    #[test]
    fn verifies_message_integrity() {
        let message = Message::new(
            REFRESH_REQUEST,
            [7u8; 12],
            vec![Attribute::new(ATTR_LIFETIME, 600u32.to_be_bytes().to_vec())],
        );
        let encoded = message.encode_with_integrity(b"key").unwrap();
        assert!(verify_message_integrity(&encoded, b"key").unwrap());
        assert!(verify_message_integrity(&encoded, b"other").is_err());

        let plain = message.encode().unwrap();
        assert!(!verify_message_integrity(&plain, b"key").unwrap());

        let mut tampered = encoded;
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        assert!(verify_message_integrity(&tampered, b"key").is_err());
    }

    #[tokio::test]
    async fn retransmits_turn_requests() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        socket.connect(address).await.unwrap();
        let allocation = TurnAllocation {
            socket,
            relay_addr: address,
            lifetime_s: 0,
            auth: Mutex::new(None),
            timeout: Duration::from_secs(2),
            send_buf: Vec::new(),
            indication_sequence: 0,
            control: None,
        };
        let server_task = tokio::spawn(async move {
            let mut packet = [0u8; 1024];
            let (_, from) = server.recv_from(&mut packet).await.unwrap();
            let (len, _) = server.recv_from(&mut packet).await.unwrap();
            let request = Message::decode(&packet[..len]).unwrap();
            let response = Message::new(
                0x0104,
                request.transaction_id,
                vec![Attribute::new(ATTR_LIFETIME, 600u32.to_be_bytes().to_vec())],
            );
            server
                .send_to(&response.encode().unwrap(), from)
                .await
                .unwrap();
        });

        allocation.request(REFRESH_REQUEST, &[]).await.unwrap();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn ignores_unrelated_packets_while_waiting() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        socket.connect(address).await.unwrap();
        let allocation = TurnAllocation {
            socket,
            relay_addr: address,
            lifetime_s: 0,
            auth: Mutex::new(None),
            timeout: Duration::from_secs(2),
            send_buf: Vec::new(),
            indication_sequence: 0,
            control: None,
        };
        let server_task = tokio::spawn(async move {
            let mut packet = [0u8; 1024];
            let (len, from) = server.recv_from(&mut packet).await.unwrap();
            let request = Message::decode(&packet[..len]).unwrap();
            let lifetime = || Attribute::new(ATTR_LIFETIME, 600u32.to_be_bytes().to_vec());
            let wrong = Message::new(0x0104, [0xaa; 12], vec![lifetime()]);
            server
                .send_to(&wrong.encode().unwrap(), from)
                .await
                .unwrap();
            let data = Message::new(
                DATA_INDICATION,
                [0xbb; 12],
                vec![Attribute::new(ATTR_DATA, b"x".to_vec())],
            );
            server.send_to(&data.encode().unwrap(), from).await.unwrap();
            let response = Message::new(0x0104, request.transaction_id, vec![lifetime()]);
            server
                .send_to(&response.encode().unwrap(), from)
                .await
                .unwrap();
        });

        let response = allocation.request(REFRESH_REQUEST, &[]).await.unwrap();
        assert!(response.is_success());
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn times_out_without_a_response() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        socket.connect(address).await.unwrap();
        let allocation = TurnAllocation {
            socket,
            relay_addr: address,
            lifetime_s: 0,
            auth: Mutex::new(None),
            timeout: Duration::from_millis(900),
            send_buf: Vec::new(),
            indication_sequence: 0,
            control: None,
        };
        let server_task = tokio::spawn(async move {
            let mut packet = [0u8; 1024];
            let (len, _) = server.recv_from(&mut packet).await.unwrap();
            let first = Message::decode(&packet[..len]).unwrap();
            let (len, _) = server.recv_from(&mut packet).await.unwrap();
            let second = Message::decode(&packet[..len]).unwrap();
            assert_eq!(first.transaction_id, second.transaction_id);
            assert_eq!(first.kind, REFRESH_REQUEST);
        });

        let result = allocation.request(REFRESH_REQUEST, &[]).await;
        assert!(matches!(result, Err(TurnError::Timeout)), "{result:?}");
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("server task did not finish")
            .unwrap();
    }

    #[tokio::test]
    async fn rejects_unauthenticated_success_response() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        socket.connect(address).await.unwrap();
        let allocation = TurnAllocation {
            socket,
            relay_addr: address,
            lifetime_s: 0,
            auth: Mutex::new(Some(Auth::new(
                "user".to_owned(),
                "pass".to_owned(),
                "realm".to_owned(),
                "nonce".to_owned(),
            ))),
            timeout: Duration::from_secs(2),
            send_buf: Vec::new(),
            indication_sequence: 0,
            control: None,
        };
        let server_task = tokio::spawn(async move {
            let mut packet = [0u8; 1024];
            let (len, from) = server.recv_from(&mut packet).await.unwrap();
            let request = Message::decode(&packet[..len]).unwrap();
            let response = Message::new(0x0104, request.transaction_id, Vec::new());
            server
                .send_to(&response.encode().unwrap(), from)
                .await
                .unwrap();
        });

        let result = allocation.request(REFRESH_REQUEST, &[]).await;
        assert!(
            matches!(
                result,
                Err(TurnError::Protocol("response has no message integrity"))
            ),
            "{result:?}"
        );
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_unauthenticated_allocate_success() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        socket.connect(address).await.unwrap();
        let mut allocation = TurnAllocation::for_test(socket, address, Duration::from_secs(2));
        let server_task = tokio::spawn(async move {
            let mut packet = [0u8; 1024];
            let (len, from) = server.recv_from(&mut packet).await.unwrap();
            let request = Message::decode(&packet[..len]).unwrap();
            assert_eq!(request.kind, ALLOCATE_REQUEST);
            let relay = encode_xor_address(SocketAddr::from((Ipv4Addr::LOCALHOST, 5000))).unwrap();
            let response = Message::new(
                0x0103,
                request.transaction_id,
                vec![Attribute::new(ATTR_XOR_RELAYED_ADDRESS, relay.to_vec())],
            );
            server
                .send_to(&response.encode().unwrap(), from)
                .await
                .unwrap();
        });

        let result = allocation.allocate_inner("user", "pass").await;
        assert!(
            matches!(
                result,
                Err(TurnError::Protocol("unauthenticated allocate response"))
            ),
            "{result:?}"
        );
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("server task did not finish")
            .unwrap();
    }

    #[tokio::test]
    async fn accepts_authenticated_success_response() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        socket.connect(address).await.unwrap();
        let auth = Auth::new(
            "user".to_owned(),
            "pass".to_owned(),
            "realm".to_owned(),
            "nonce".to_owned(),
        );
        let key = auth.key.clone();
        let allocation = TurnAllocation {
            socket,
            relay_addr: address,
            lifetime_s: 0,
            auth: Mutex::new(Some(auth)),
            timeout: Duration::from_secs(2),
            send_buf: Vec::new(),
            indication_sequence: 0,
            control: None,
        };
        let server_task = tokio::spawn(async move {
            let mut packet = [0u8; 1024];
            let (len, from) = server.recv_from(&mut packet).await.unwrap();
            let request = Message::decode(&packet[..len]).unwrap();
            let response = Message::new(0x0104, request.transaction_id, Vec::new());
            server
                .send_to(&response.encode_with_integrity(&key).unwrap(), from)
                .await
                .unwrap();
        });

        allocation.request(REFRESH_REQUEST, &[]).await.unwrap();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn forwards_data_while_a_control_request_is_pending() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        socket.connect(address).await.unwrap();
        let mut allocation = TurnAllocation {
            socket,
            relay_addr: address,
            lifetime_s: 0,
            auth: Mutex::new(None),
            timeout: Duration::from_secs(2),
            send_buf: Vec::new(),
            indication_sequence: 0,
            control: None,
        };
        allocation.begin_refresh().await.unwrap();
        let server_task = tokio::spawn(async move {
            let mut packet = [0u8; 1024];
            let (len, from) = server.recv_from(&mut packet).await.unwrap();
            let request = Message::decode(&packet[..len]).unwrap();
            let relay: SocketAddr = "203.0.113.10:12010".parse().unwrap();
            let data = Message::new(
                DATA_INDICATION,
                [1u8; 12],
                vec![
                    Attribute::new(
                        ATTR_XOR_PEER_ADDRESS,
                        encode_xor_address(relay).unwrap().to_vec(),
                    ),
                    Attribute::new(ATTR_DATA, b"payload".to_vec()),
                ],
            );
            server.send_to(&data.encode().unwrap(), from).await.unwrap();
            let response = Message::new(
                0x0104,
                request.transaction_id,
                vec![Attribute::new(ATTR_LIFETIME, 300u32.to_be_bytes().to_vec())],
            );
            server
                .send_to(&response.encode().unwrap(), from)
                .await
                .unwrap();
        });

        let mut packet = vec![0u8; 2048];
        match allocation.recv_into(&mut packet).await.unwrap() {
            TurnEvent::Data { from, payload } => {
                assert_eq!(from, "203.0.113.10:12010".parse().unwrap());
                assert_eq!(&packet[payload], b"payload");
            }
            _ => panic!("expected a data indication"),
        }
        assert!(matches!(
            allocation.recv_into(&mut packet).await.unwrap(),
            TurnEvent::Refreshed
        ));
        assert_eq!(allocation.lifetime_s(), 300);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_data_packets_do_not_stop_receiving() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        socket.connect(address).await.unwrap();
        let allocation_addr = socket.local_addr().unwrap();
        let mut allocation = TurnAllocation {
            socket,
            relay_addr: address,
            lifetime_s: 0,
            auth: Mutex::new(None),
            timeout: Duration::from_secs(2),
            send_buf: Vec::new(),
            indication_sequence: 0,
            control: None,
        };
        let server_task = tokio::spawn(async move {
            server.send_to(&[], allocation_addr).await.unwrap();
            // 头部声明 64 byte 属性，实际没有。
            let truncated = [
                0x00, 0x17, 0x00, 0x40, 0x21, 0x12, 0xa4, 0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ];
            server.send_to(&truncated, allocation_addr).await.unwrap();
            let relay: SocketAddr = "203.0.113.10:12010".parse().unwrap();
            let data = Message::new(
                DATA_INDICATION,
                [2u8; 12],
                vec![
                    Attribute::new(
                        ATTR_XOR_PEER_ADDRESS,
                        encode_xor_address(relay).unwrap().to_vec(),
                    ),
                    Attribute::new(ATTR_DATA, b"ok".to_vec()),
                ],
            );
            server
                .send_to(&data.encode().unwrap(), allocation_addr)
                .await
                .unwrap();
        });

        let mut packet = vec![0u8; 2048];
        assert!(matches!(
            allocation.recv_into(&mut packet).await.unwrap(),
            TurnEvent::None
        ));
        assert!(matches!(
            allocation.recv_into(&mut packet).await.unwrap(),
            TurnEvent::None
        ));
        match allocation.recv_into(&mut packet).await.unwrap() {
            TurnEvent::Data { from, payload } => {
                assert_eq!(from, "203.0.113.10:12010".parse().unwrap());
                assert_eq!(&packet[payload], b"ok");
            }
            _ => panic!("expected a data indication"),
        }
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn retries_a_control_request_with_an_updated_nonce() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        socket.connect(address).await.unwrap();
        let auth = Auth::new(
            "user".to_owned(),
            "pass".to_owned(),
            "realm".to_owned(),
            "nonce".to_owned(),
        );
        let key = auth.key.clone();
        let mut allocation = TurnAllocation {
            socket,
            relay_addr: address,
            lifetime_s: 0,
            auth: Mutex::new(Some(auth)),
            timeout: Duration::from_secs(2),
            send_buf: Vec::new(),
            indication_sequence: 0,
            control: None,
        };
        allocation.begin_refresh().await.unwrap();
        let server_task = tokio::spawn(async move {
            let mut packet = [0u8; 1024];
            let (len, from) = server.recv_from(&mut packet).await.unwrap();
            let first = Message::decode(&packet[..len]).unwrap();
            let stale = Message::new(
                0x0114,
                first.transaction_id,
                vec![
                    Attribute::new(ATTR_ERROR_CODE, vec![0, 0, 4, 38]),
                    Attribute::new(ATTR_NONCE, b"new-nonce".to_vec()),
                ],
            );
            server
                .send_to(&stale.encode().unwrap(), from)
                .await
                .unwrap();

            let (len, _) = server.recv_from(&mut packet).await.unwrap();
            let second = Message::decode(&packet[..len]).unwrap();
            assert_ne!(second.transaction_id, first.transaction_id);
            assert_eq!(second.attribute(ATTR_NONCE), Some(b"new-nonce".as_slice()));
            let response = Message::new(
                0x0104,
                second.transaction_id,
                vec![Attribute::new(ATTR_LIFETIME, 600u32.to_be_bytes().to_vec())],
            );
            server
                .send_to(&response.encode_with_integrity(&key).unwrap(), from)
                .await
                .unwrap();
        });

        let mut packet = vec![0u8; 2048];
        assert!(matches!(
            allocation.recv_into(&mut packet).await.unwrap(),
            TurnEvent::None
        ));
        assert!(matches!(
            allocation.recv_into(&mut packet).await.unwrap(),
            TurnEvent::Refreshed
        ));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn serializes_control_requests() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        socket.connect(address).await.unwrap();
        let auth = Auth::new(
            "user".to_owned(),
            "pass".to_owned(),
            "realm".to_owned(),
            "nonce".to_owned(),
        );
        let key = auth.key.clone();
        let mut allocation = TurnAllocation::for_test(socket, address, Duration::from_secs(2));
        *allocation.auth.get_mut().unwrap() = Some(auth);
        let peer: SocketAddr = "8.8.8.8:5000".parse().unwrap();

        allocation.begin_refresh().await.unwrap();
        allocation.begin_permission(peer).await.unwrap();
        allocation.resend_control().await.unwrap();

        let mut packet = [0u8; 1024];
        let (len, from) = server.recv_from(&mut packet).await.unwrap();
        let first = Message::decode(&packet[..len]).unwrap();
        let (len, _) = server.recv_from(&mut packet).await.unwrap();
        let second = Message::decode(&packet[..len]).unwrap();
        assert_eq!(first.kind, REFRESH_REQUEST);
        assert_eq!(second.kind, REFRESH_REQUEST);
        assert_eq!(first.transaction_id, second.transaction_id);

        let response = Message::new(
            0x0104,
            first.transaction_id,
            vec![Attribute::new(ATTR_LIFETIME, 600u32.to_be_bytes().to_vec())],
        );
        server
            .send_to(&response.encode_with_integrity(&key).unwrap(), from)
            .await
            .unwrap();
        let mut buffer = [0u8; 1024];
        assert!(matches!(
            allocation.recv_into(&mut buffer).await.unwrap(),
            TurnEvent::Refreshed
        ));

        allocation.begin_permission(peer).await.unwrap();
        let (len, from) = server.recv_from(&mut packet).await.unwrap();
        let third = Message::decode(&packet[..len]).unwrap();
        assert_eq!(third.kind, CREATE_PERMISSION_REQUEST);
        let response = Message::new(0x0108, third.transaction_id, Vec::new());
        server
            .send_to(&response.encode_with_integrity(&key).unwrap(), from)
            .await
            .unwrap();
        assert!(matches!(
            allocation.recv_into(&mut buffer).await.unwrap(),
            TurnEvent::PermissionCreated
        ));
    }

    #[tokio::test]
    async fn bridge_forwards_datagrams_both_ways() {
        use crate::bridge::start_with_allocation;

        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let allocation_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        allocation_socket.connect(server_addr).await.unwrap();
        let allocation =
            TurnAllocation::for_test(allocation_socket, server_addr, Duration::from_secs(2));
        let remote_relay: SocketAddr = "203.0.113.20:50000".parse().unwrap();
        let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let peer_addr = peer.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let mut packet = vec![0u8; 2048];
            let (len, from) = server.recv_from(&mut packet).await.unwrap();
            let request = Message::decode(&packet[..len]).unwrap();
            assert_eq!(request.kind, 0x0008);
            let response = Message::new(0x0108, request.transaction_id, Vec::new());
            server
                .send_to(&response.encode().unwrap(), from)
                .await
                .unwrap();

            // bridge 收到格式错误的报文后应继续转发。
            server.send_to(&[], from).await.unwrap();
            let truncated = [
                0x00, 0x17, 0x00, 0x40, 0x21, 0x12, 0xa4, 0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ];
            server.send_to(&truncated, from).await.unwrap();

            let (len, _) = server.recv_from(&mut packet).await.unwrap();
            let sent = Message::decode(&packet[..len]).unwrap();
            assert_eq!(sent.kind, SEND_INDICATION);
            assert_eq!(
                sent.attribute(ATTR_XOR_PEER_ADDRESS)
                    .and_then(|value| decode_xor_address(value, &sent.transaction_id).ok()),
                Some(remote_relay)
            );
            assert_eq!(sent.attribute(ATTR_DATA), Some(b"hello".as_slice()));

            let data = Message::new(
                DATA_INDICATION,
                [3u8; 12],
                vec![
                    Attribute::new(
                        ATTR_XOR_PEER_ADDRESS,
                        encode_xor_address(remote_relay).unwrap().to_vec(),
                    ),
                    Attribute::new(ATTR_DATA, b"world".to_vec()),
                ],
            );
            server.send_to(&data.encode().unwrap(), from).await.unwrap();
        });

        let bridge = start_with_allocation(allocation, remote_relay, peer_addr)
            .await
            .unwrap();
        peer.send_to(b"hello", bridge.local_addr).await.unwrap();
        let mut buffer = [0u8; 64];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buffer[..len], b"world");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn bridge_drops_data_from_other_turn_peers() {
        use crate::bridge::start_with_allocation;

        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let allocation_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        allocation_socket.connect(server_addr).await.unwrap();
        let allocation =
            TurnAllocation::for_test(allocation_socket, server_addr, Duration::from_secs(2));
        let remote_relay: SocketAddr = "203.0.113.20:50000".parse().unwrap();
        let other_peer: SocketAddr = "203.0.113.21:50000".parse().unwrap();
        let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let peer_addr = peer.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let mut packet = vec![0u8; 2048];
            let (len, from) = server.recv_from(&mut packet).await.unwrap();
            let request = Message::decode(&packet[..len]).unwrap();
            assert_eq!(request.kind, 0x0008);
            let response = Message::new(0x0108, request.transaction_id, Vec::new());
            server
                .send_to(&response.encode().unwrap(), from)
                .await
                .unwrap();

            let messages: [(SocketAddr, &[u8]); 2] =
                [(other_peer, b"bad"), (remote_relay, b"good")];
            for (address, payload) in messages {
                let data = Message::new(
                    DATA_INDICATION,
                    [4u8; 12],
                    vec![
                        Attribute::new(
                            ATTR_XOR_PEER_ADDRESS,
                            encode_xor_address(address).unwrap().to_vec(),
                        ),
                        Attribute::new(ATTR_DATA, payload.to_vec()),
                    ],
                );
                server.send_to(&data.encode().unwrap(), from).await.unwrap();
            }
        });

        let _bridge = start_with_allocation(allocation, remote_relay, peer_addr)
            .await
            .unwrap();
        let mut buffer = [0u8; 64];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buffer[..len], b"good");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn releases_allocation_on_drop() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        socket.connect(address).await.unwrap();
        // Drop 用 try_send，只有 socket 已经可写时才会真正发出报文。
        socket.writable().await.unwrap();
        let auth = Auth::new(
            "user".to_owned(),
            "pass".to_owned(),
            "realm".to_owned(),
            "nonce".to_owned(),
        );
        let key = auth.key.clone();
        let allocation = TurnAllocation {
            socket,
            relay_addr: address,
            lifetime_s: 600,
            auth: Mutex::new(Some(auth)),
            timeout: Duration::from_secs(1),
            send_buf: Vec::new(),
            indication_sequence: 0,
            control: None,
        };

        drop(allocation);

        let mut packet = [0u8; 1024];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), server.recv_from(&mut packet))
            .await
            .expect("allocation drop did not send a refresh")
            .unwrap();
        let request = Message::decode(&packet[..len]).unwrap();
        assert_eq!(request.kind, REFRESH_REQUEST);
        assert_eq!(request.attribute(ATTR_LIFETIME), Some(&[0u8; 4][..]));
        assert!(verify_message_integrity(&packet[..len], &key).unwrap());
    }

    #[test]
    fn integrity_length_includes_attribute() {
        let message = Message::new(
            REFRESH_REQUEST,
            [7u8; 12],
            vec![Attribute::new(ATTR_LIFETIME, 600u32.to_be_bytes().to_vec())],
        );
        let encoded = message.encode_with_integrity(b"key").unwrap();
        let length = usize::from(u16::from_be_bytes([encoded[2], encoded[3]]));
        assert_eq!(length, encoded.len() - HEADER_LEN);
        let decoded = Message::decode(&encoded).unwrap();
        assert_eq!(
            decoded
                .attribute(ATTR_MESSAGE_INTEGRITY)
                .map(|value| value.len()),
            Some(20)
        );
    }
}
