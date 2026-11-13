//! UU 信令。实现 Engine.IO 4 / Socket.IO 握手、control / be-controlled 和 SOAC 记录交换。
//!
//! viewer 和 host 使用同一个房间连接，职责不同：
//! - viewer 发送一次 control，取得本会话的 TURN 凭据，然后发送 offer、等待 answer。
//! - host 房间长期运行，处理 viewer 的 control 和 offer。UU 房间同时只接受一个 viewer。

use std::fmt;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};
use tracing::debug;
use uuid::Uuid;

use uutier_core::REDACTED;

use crate::client::TurnServerConfig;
use crate::error::TurnError;
use crate::record::{MINIMAL_SDP, PeerRecord, SoacData};
use crate::uu::Room;

const SOCKET_IO_ACK_ID: u64 = 11;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const RECORD_WAIT_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_ATTACHMENTS: usize = 8;
const MAX_ICE_SERVERS: usize = 16;
const MAX_ICE_URLS: usize = 8;
const MAX_MESSAGE_BYTES: usize = 1 << 20;
const MAX_FRAME_BYTES: usize = 256 << 10;
/// 信令里 client_id、ice_id、app_control_id 的长度上限，单位 byte。
const MAX_ID_BYTES: usize = 128;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 一个 ICE 服务器项。用户名和密码是会话凭据，Debug 不输出。
#[derive(Clone)]
struct IceServer {
    urls: Vec<String>,
    username: String,
    credential: String,
}

impl fmt::Debug for IceServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IceServer")
            .field("urls", &self.urls)
            .field("username", &REDACTED)
            .field("credential", &REDACTED)
            .finish()
    }
}

/// 从 SOAC 收到的一条记录。
#[derive(Clone)]
pub struct SignalRecord {
    pub record: PeerRecord,
    /// 发起 control 的一方的 ID，host 回复 answer 时使用。
    pub app_control_id: Option<String>,
}

impl fmt::Debug for SignalRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalRecord")
            .field("record", &self.record)
            .field(
                "app_control_id",
                &self.app_control_id.as_ref().map(|_| REDACTED),
            )
            .finish()
    }
}

/// host 房间里一个 viewer 的 control 会话。
pub struct ViewerControl {
    client_id: String,
    ice_id: String,
    app_control_id: String,
    ice_servers: Vec<IceServer>,
}

impl ViewerControl {
    /// 返回 viewer 的 client ID，回复 answer 时使用。
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// 返回 viewer 的 ICE ID。offer 缺少 client ID 时用它匹配。
    pub fn ice_id(&self) -> &str {
        &self.ice_id
    }

    /// 返回本 viewer 可用的 UDP TURN 服务器。
    pub fn turn_server(&self, request_timeout_s: u64) -> Result<TurnServerConfig, TurnError> {
        select_turn(&self.ice_servers, request_timeout_s)
    }
}

impl fmt::Debug for ViewerControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewerControl")
            .field("client_id", &REDACTED)
            .field("ice_id", &REDACTED)
            .field("app_control_id", &REDACTED)
            .field("ice_servers", &self.ice_servers.len())
            .finish()
    }
}

/// host 房间收到的一条 offer。
#[derive(Clone)]
pub struct HostOffer {
    /// offer 的发送方 client ID，回复 answer 时使用。
    pub client_id: String,
    /// offer 携带的 ICE ID。
    pub ice_id: String,
    /// offer 携带的 `app_control_id`。
    pub control_id: Option<String>,
    pub record: PeerRecord,
}

impl fmt::Debug for HostOffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostOffer")
            .field("client_id", &REDACTED)
            .field("ice_id", &REDACTED)
            .field("control_id", &self.control_id.as_ref().map(|_| REDACTED))
            .field("record", &self.record)
            .finish()
    }
}

/// host 房间事件。
pub enum HostEvent {
    /// 一个 viewer 发起 control。
    Control(ViewerControl),
    /// 一个 viewer 发来带记录的 offer。
    Offer(HostOffer),
}

/// 一个长期运行的 host 房间。UU 房间同时只接受一个 viewer。
pub struct HostRoom {
    connection: Connection,
}

impl HostRoom {
    /// 创建 host 房间。
    pub async fn create(api: &crate::uu::UuApi) -> Result<Self, TurnError> {
        let room = api.create_room().await?;
        let connection = Connection::open(&room).await?;
        Ok(Self { connection })
    }

    /// 等待下一个 control 或 offer。心跳由连接层应答，收到心跳后重新计时。
    pub async fn next_event(&mut self) -> Result<HostEvent, TurnError> {
        loop {
            let event = self.connection.next_event_idle().await?;
            match event.get(0).and_then(Value::as_str) {
                Some("be-controlled") => {
                    let data = event.get(1).cloned().unwrap_or(Value::Null);
                    if let Some(control) = parse_control(&data) {
                        return Ok(HostEvent::Control(control));
                    }
                    debug!("ignoring be-controlled event without client_id");
                }
                Some("soac") => {
                    let payload = event.get(1).cloned().unwrap_or(Value::Null);
                    match parse_offer(&payload) {
                        Ok(Some(offer)) => return Ok(HostEvent::Offer(offer)),
                        Ok(None) => {}
                        Err(error) => debug!(error = %error, "ignoring invalid offer"),
                    }
                }
                _ => {}
            }
        }
    }

    /// 向 offer 的发送方回复 answer。
    pub async fn send_answer(
        &mut self,
        viewer: &ViewerControl,
        control_id: &str,
        record: &PeerRecord,
    ) -> Result<(), TurnError> {
        let app_control_id = if viewer.app_control_id.is_empty() {
            control_id.to_owned()
        } else {
            viewer.app_control_id.clone()
        };
        let payload = json!({
            "client_id": viewer.client_id,
            "data": SoacData {
                kind: "answer".to_owned(),
                sdp: Some(MINIMAL_SDP.to_owned()),
                ice_id: Some(viewer.ice_id.clone()),
                app_control_id: (!app_control_id.is_empty()).then_some(app_control_id),
                uutier_record: Some(record.clone()),
            }
        });
        self.connection.send_soac(payload).await
    }

    /// 关闭信令连接。
    pub async fn close(self) {
        self.connection.close().await;
    }
}

impl fmt::Debug for HostRoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostRoom").finish_non_exhaustive()
    }
}

/// 一个已建立的 UU viewer 会话。
pub struct SignalSession {
    connection: Connection,
    client_id: String,
    ice_id: String,
    app_control_id: String,
    ice_servers: Vec<IceServer>,
}

impl fmt::Debug for SignalSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalSession")
            .field("client_id", &REDACTED)
            .field("ice_id", &REDACTED)
            .field("app_control_id", &REDACTED)
            .field("ice_servers", &self.ice_servers.len())
            .finish()
    }
}

impl SignalSession {
    /// 加入 host 房间并发送 control，取得本会话的 TURN 配置。
    pub async fn viewer(api: &crate::uu::UuApi, host_device_id: &str) -> Result<Self, TurnError> {
        let room = api.join_room(host_device_id).await?;
        let mut connection = Connection::open(&room).await?;
        let credentials = control(&mut connection).await?;
        Ok(Self {
            connection,
            client_id: credentials.client_id,
            ice_id: credentials.ice_id,
            app_control_id: credentials.app_control_id,
            ice_servers: credentials.ice_servers,
        })
    }

    /// 返回本会话中可用的 UDP TURN 服务器。
    pub fn turn_server(&self, request_timeout_s: u64) -> Result<TurnServerConfig, TurnError> {
        select_turn(&self.ice_servers, request_timeout_s)
    }

    /// viewer 发送带记录的 offer。
    pub async fn send_offer(&mut self, record: &PeerRecord) -> Result<(), TurnError> {
        let payload = json!({
            "client_id": self.client_id,
            "data": SoacData {
                kind: "offer".to_owned(),
                sdp: Some(MINIMAL_SDP.to_owned()),
                ice_id: Some(self.ice_id.clone()),
                app_control_id: (!self.app_control_id.is_empty())
                    .then(|| self.app_control_id.clone()),
                uutier_record: Some(record.clone()),
            }
        });
        self.connection.send_soac(payload).await
    }

    /// 等待指定类型的 SOAC 记录。
    pub async fn next_record(&mut self, expected_kind: &str) -> Result<SignalRecord, TurnError> {
        let deadline = tokio::time::Instant::now() + RECORD_WAIT_TIMEOUT;
        self.next_record_until(expected_kind, deadline).await
    }

    async fn next_record_until(
        &mut self,
        expected_kind: &str,
        deadline: tokio::time::Instant,
    ) -> Result<SignalRecord, TurnError> {
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(TurnError::SignalTimeout);
            }
            let event = self.connection.next_event_until(deadline).await?;
            if event.get(0).and_then(Value::as_str) == Some("soac") {
                let payload = event.get(1).cloned().unwrap_or(Value::Null);
                if let Some(record) = record_from_payload(&payload, expected_kind)? {
                    return Ok(record);
                }
            }
        }
    }

    /// 保持信令连接，应答心跳，直到连接关闭。
    ///
    /// TURN 凭据由本会话下发，连接关闭后不再使用对应的分配。
    pub async fn pump(&mut self) -> Result<(), TurnError> {
        loop {
            self.connection.next_event_idle().await?;
        }
    }

    /// 关闭信令连接。
    pub async fn close(self) {
        self.connection.close().await;
    }
}

/// 一个 Socket.IO 连接。viewer 和 host 共用读写与心跳处理。
struct Connection {
    ws: Socket,
}

impl Connection {
    async fn open(room: &Room) -> Result<Self, TurnError> {
        let server = room
            .signaling_list
            .first()
            .ok_or(TurnError::InvalidResponse)?;
        let timeout = if room.ws_connect_timeout_ms == 0 {
            CONNECT_TIMEOUT
        } else {
            Duration::from_millis(room.ws_connect_timeout_ms.clamp(1_000, 30_000))
        };
        let mut request = socket_url(server)
            .into_client_request()
            .map_err(|_| TurnError::InvalidResponse)?;
        request.headers_mut().insert(
            "X-NRD-AUTH",
            room.token()
                .parse()
                .map_err(|_| TurnError::InvalidResponse)?,
        );
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES));
        let (ws, _) = tokio::time::timeout(
            timeout,
            connect_async_with_config(request, Some(config), true),
        )
        .await
        .map_err(|_| TurnError::SignalTimeout)?
        .map_err(|_| TurnError::SignalDisconnected)?;
        let mut connection = Self { ws };
        // 握手期间逐帧读取，没有整体截止时间时，服务器持续发心跳就会一直挂起。
        tokio::time::timeout(timeout, connection.handshake())
            .await
            .map_err(|_| TurnError::SignalTimeout)??;
        Ok(connection)
    }

    async fn handshake(&mut self) -> Result<(), TurnError> {
        loop {
            match self.read_frame().await? {
                Message::Text(text) => {
                    if text.starts_with('0') {
                        self.send_text("40").await?;
                    } else if text.starts_with("40") {
                        return Ok(());
                    } else if text.starts_with('2') {
                        self.send_text(&pong(&text)).await?;
                    } else if text.starts_with("41") || text == "1" {
                        return Err(TurnError::SignalDisconnected);
                    }
                }
                Message::Ping(payload) => self.send(Message::Pong(payload)).await?,
                Message::Close(_) => return Err(TurnError::SignalDisconnected),
                _ => {}
            }
        }
    }

    /// 读取下一个 Socket.IO 事件，使用绝对截止时间。
    async fn next_event_until(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> Result<Value, TurnError> {
        loop {
            if let Some(event) = self.handle_frame(deadline).await? {
                return Ok(event);
            }
        }
    }

    /// 读取下一个 Socket.IO 事件，每收到一帧重新计时。
    async fn next_event_idle(&mut self) -> Result<Value, TurnError> {
        loop {
            let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
            if let Some(event) = self.handle_frame(deadline).await? {
                return Ok(event);
            }
        }
    }

    /// 处理一帧。心跳、二进制附件和无关帧返回 `None`。
    async fn handle_frame(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> Result<Option<Value>, TurnError> {
        match self.read_frame_until(deadline).await? {
            Message::Text(text) => {
                if text.starts_with('2') {
                    self.send_text(&pong(&text)).await?;
                    return Ok(None);
                }
                if let Some(event) = text_event(&text) {
                    return Ok(Some(event));
                }
                if text.starts_with("45") || text.starts_with("46") {
                    let Some(packet) = parse_binary_packet(&text) else {
                        return Ok(None);
                    };
                    self.consume_attachments(packet.count, deadline).await?;
                    return Ok(Some(packet.payload));
                }
                if text.starts_with("41") || text == "1" {
                    return Err(TurnError::SignalDisconnected);
                }
                Ok(None)
            }
            Message::Binary(_) => Ok(None),
            Message::Ping(payload) => {
                self.send(Message::Pong(payload)).await?;
                Ok(None)
            }
            Message::Close(_) => Err(TurnError::SignalDisconnected),
            _ => Ok(None),
        }
    }

    async fn send_soac(&mut self, payload: Value) -> Result<(), TurnError> {
        self.send_text(&format!("42{}", json!(["soac", payload])))
            .await
    }

    async fn consume_attachments(
        &mut self,
        count: usize,
        deadline: tokio::time::Instant,
    ) -> Result<(), TurnError> {
        for _ in 0..count.min(MAX_ATTACHMENTS) {
            loop {
                match self.read_frame_until(deadline).await? {
                    Message::Binary(_) => break,
                    Message::Text(text) if text.starts_with('2') => {
                        self.send_text(&pong(&text)).await?;
                    }
                    Message::Ping(payload) => self.send(Message::Pong(payload)).await?,
                    Message::Close(_) => return Err(TurnError::SignalDisconnected),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    async fn read_frame(&mut self) -> Result<Message, TurnError> {
        let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
        self.read_frame_until(deadline).await
    }

    async fn read_frame_until(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> Result<Message, TurnError> {
        match tokio::time::timeout_at(deadline, self.ws.next()).await {
            Ok(Some(Ok(message))) => Ok(message),
            Ok(Some(Err(_))) | Ok(None) => Err(TurnError::SignalDisconnected),
            Err(_) => Err(TurnError::SignalTimeout),
        }
    }

    async fn send(&mut self, message: Message) -> Result<(), TurnError> {
        tokio::time::timeout(WRITE_TIMEOUT, self.ws.send(message))
            .await
            .map_err(|_| TurnError::SignalTimeout)?
            .map_err(|_| TurnError::SignalDisconnected)
    }

    async fn send_text(&mut self, text: &str) -> Result<(), TurnError> {
        self.send(Message::Text(text.to_owned().into())).await
    }

    async fn close(mut self) {
        let _ = tokio::time::timeout(WRITE_TIMEOUT, self.ws.close(None)).await;
    }
}

/// viewer control 应答里的会话参数。
struct ViewerCredentials {
    client_id: String,
    ice_id: String,
    app_control_id: String,
    ice_servers: Vec<IceServer>,
}

async fn control(connection: &mut Connection) -> Result<ViewerCredentials, TurnError> {
    let control_id = Uuid::new_v4().to_string();
    let header = json!({
        "app_control_id": control_id,
        "app_data": {"_placeholder": true, "num": 0},
        "streamer_data": json!({
            "control_id": control_id,
            "device_capability": {
                "display_info": [],
                "ice_id": "",
                "video_codec_capability": []
            }
        })
        .to_string(),
    });
    let text = format!("451-{SOCKET_IO_ACK_ID}{}", json!(["control", header]));
    connection.send_text(&text).await?;
    connection.send(Message::Binary(vec![8, 1].into())).await?;
    let deadline = tokio::time::Instant::now() + CONTROL_TIMEOUT;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(TurnError::SignalTimeout);
        }
        let frame = connection.read_frame_until(deadline).await?;
        match frame {
            Message::Text(text) => {
                if text.starts_with('2') {
                    connection.send_text(&pong(&text)).await?;
                    continue;
                }
                if let Some((id, payload)) = parse_ack(&text)
                    && id == SOCKET_IO_ACK_ID
                {
                    return accept_ack(&payload, &control_id);
                }
                if text.starts_with("45") || text.starts_with("46") {
                    let Some(packet) = parse_binary_packet(&text) else {
                        continue;
                    };
                    connection
                        .consume_attachments(packet.count, deadline)
                        .await?;
                    if packet.ack_id == Some(SOCKET_IO_ACK_ID) {
                        return accept_ack(&packet.payload, &control_id);
                    }
                }
                if text.starts_with("41") || text == "1" {
                    return Err(TurnError::SignalDisconnected);
                }
            }
            Message::Ping(payload) => connection.send(Message::Pong(payload)).await?,
            Message::Close(_) => return Err(TurnError::SignalDisconnected),
            _ => {}
        }
    }
}

fn accept_ack(payload: &Value, control_id: &str) -> Result<ViewerCredentials, TurnError> {
    let array = payload.as_array().ok_or(TurnError::ControlRejected)?;
    if array.first().and_then(Value::as_str) != Some("success") {
        return Err(TurnError::ControlRejected);
    }
    let body = array.get(1).ok_or(TurnError::ControlRejected)?;
    let client_id = body
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_ID_BYTES)
        .ok_or(TurnError::InvalidResponse)?
        .to_owned();
    let ice_id = body
        .get("ice_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_ID_BYTES)
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
    let ice_servers = parse_ice_servers(body.get("iceServers"));
    Ok(ViewerCredentials {
        client_id,
        ice_id,
        app_control_id: control_id.to_owned(),
        ice_servers,
    })
}

fn parse_control(data: &Value) -> Option<ViewerControl> {
    let field = |name: &str| {
        data.get(name)
            .or_else(|| data.get("data").and_then(|inner| inner.get(name)))
    };
    let client_id = field("client_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_ID_BYTES)
        .map(str::to_owned)?;
    let ice_id = field("ice_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_ID_BYTES)
        .map(str::to_owned)
        .unwrap_or_default();
    let app_control_id = field("app_control_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_ID_BYTES)
        .map(str::to_owned)
        .unwrap_or_default();
    let ice_servers = parse_ice_servers(field("iceServers"));
    Some(ViewerControl {
        client_id,
        ice_id,
        app_control_id,
        ice_servers,
    })
}

fn parse_offer(payload: &Value) -> Result<Option<HostOffer>, TurnError> {
    let inner = payload.get("data").unwrap_or(payload);
    if inner.get("type").and_then(Value::as_str) != Some("offer") {
        return Ok(None);
    }
    let Some(value) = inner.get("uutier_record") else {
        return Ok(None);
    };
    let record: PeerRecord = serde_json::from_value(value.clone())
        .map_err(|_| TurnError::InvalidRecord("malformed record"))?;
    record.validate()?;
    let client_id = payload
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|value| value.len() <= MAX_ID_BYTES)
        .unwrap_or("")
        .to_owned();
    let ice_id = inner
        .get("ice_id")
        .and_then(Value::as_str)
        .filter(|value| value.len() <= MAX_ID_BYTES)
        .unwrap_or("")
        .to_owned();
    let control_id = inner
        .get("app_control_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_ID_BYTES)
        .map(str::to_owned);
    Ok(Some(HostOffer {
        client_id,
        ice_id,
        control_id,
        record,
    }))
}

fn socket_url(server: &str) -> String {
    let base = server.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_owned()
    };
    format!("{base}/socket.io/?transport=websocket&EIO=4")
}

fn pong(text: &str) -> String {
    format!("3{}", &text[1..])
}

fn text_event(text: &str) -> Option<Value> {
    serde_json::from_str(text.strip_prefix("42")?).ok()
}

fn parse_ack(text: &str) -> Option<(u64, Value)> {
    let rest = text.strip_prefix("43")?;
    let begin = rest.find('[')?;
    let id = rest[..begin].parse().ok()?;
    let payload = serde_json::from_str(&rest[begin..]).ok()?;
    Some((id, payload))
}

struct BinaryPacket {
    count: usize,
    ack_id: Option<u64>,
    payload: Value,
}

fn parse_binary_packet(text: &str) -> Option<BinaryPacket> {
    let (is_ack, rest) = if let Some(rest) = text.strip_prefix("45") {
        (false, rest)
    } else if let Some(rest) = text.strip_prefix("46") {
        (true, rest)
    } else {
        return None;
    };
    let dash = rest.find('-')?;
    let count = rest[..dash].parse::<usize>().ok()?;
    if count > MAX_ATTACHMENTS {
        return None;
    }
    let after = &rest[dash + 1..];
    let begin = after.find('[')?;
    let ack_id = if is_ack {
        after[..begin].parse().ok()
    } else {
        None
    };
    let payload = serde_json::from_str(&after[begin..]).ok()?;
    Some(BinaryPacket {
        count,
        ack_id,
        payload,
    })
}

fn record_from_payload(
    payload: &Value,
    expected_kind: &str,
) -> Result<Option<SignalRecord>, TurnError> {
    let inner = payload.get("data").unwrap_or(payload);
    if inner.get("type").and_then(Value::as_str) != Some(expected_kind) {
        return Ok(None);
    }
    let value = inner
        .get("uutier_record")
        .ok_or(TurnError::InvalidRecord("missing record"))?;
    let record: PeerRecord = serde_json::from_value(value.clone())
        .map_err(|_| TurnError::InvalidRecord("malformed record"))?;
    record.validate()?;
    let app_control_id = inner
        .get("app_control_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_ID_BYTES)
        .map(str::to_owned);
    Ok(Some(SignalRecord {
        record,
        app_control_id,
    }))
}

fn select_turn(
    servers: &[IceServer],
    request_timeout_s: u64,
) -> Result<TurnServerConfig, TurnError> {
    for server in servers {
        for url in &server.urls {
            if !url.starts_with("turn:") {
                continue;
            }
            if url.contains("transport=tcp") || url.contains("transport=tls") {
                continue;
            }
            return Ok(TurnServerConfig {
                server: url.clone(),
                username: server.username.clone(),
                password: server.credential.clone(),
                realm: None,
                request_timeout_s,
            });
        }
    }
    Err(TurnError::NoTurnServer)
}

fn parse_ice_servers(value: Option<&Value>) -> Vec<IceServer> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut servers = Vec::new();
    for item in items.iter().take(MAX_ICE_SERVERS) {
        let Some(object) = item.as_object() else {
            continue;
        };
        let urls = match object.get("urls").or_else(|| object.get("url")) {
            Some(Value::String(url)) => vec![url.clone()],
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .take(MAX_ICE_URLS)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        };
        let urls: Vec<String> = urls
            .into_iter()
            .filter(|url| {
                url.len() <= 2048
                    && ["stun:", "stuns:", "turn:", "turns:"]
                        .iter()
                        .any(|prefix| url.starts_with(prefix))
            })
            .collect();
        if urls.is_empty() {
            continue;
        }
        let text = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_str)
                .filter(|value| value.len() <= 4096)
                .unwrap_or("")
                .to_owned()
        };
        servers.push(IceServer {
            urls,
            username: text("username"),
            credential: text("credential"),
        });
    }
    servers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::RECORD_VERSION;

    fn record_json() -> Value {
        json!({
            "version": RECORD_VERSION,
            "uu_device_id": "device-1",
            "network_id": "11".repeat(32),
            "public_key": "00".repeat(32),
            "ipv4": "10.144.0.2",
            "listen_port": 12010,
            "endpoints": ["192.0.2.10:12010"],
            "relay_addr": "198.51.100.20:50000"
        })
    }

    #[test]
    fn socket_url_maps_http_schemes() {
        assert_eq!(
            socket_url("wss://signal.example/"),
            "wss://signal.example/socket.io/?transport=websocket&EIO=4"
        );
        assert_eq!(
            socket_url("https://signal.example"),
            "wss://signal.example/socket.io/?transport=websocket&EIO=4"
        );
    }

    #[test]
    fn parses_binary_headers() {
        let packet = parse_binary_packet(r#"451-11["be-controlled",{"client_id":"c"}]"#).unwrap();
        assert_eq!(packet.count, 1);
        assert_eq!(packet.ack_id, None);
        assert_eq!(
            packet.payload.get(0).and_then(Value::as_str),
            Some("be-controlled")
        );
        let packet = parse_binary_packet(r#"461-11["success",{}]"#).unwrap();
        assert_eq!(packet.count, 1);
        assert_eq!(packet.ack_id, Some(11));
        assert!(parse_binary_packet(r#"459-11["x"]"#).is_none());
    }

    #[test]
    fn parses_text_ack() {
        let (id, payload) = parse_ack(r#"4311["success",{"client_id":"c"}]"#).unwrap();
        assert_eq!(id, 11);
        assert_eq!(payload.get(0).and_then(Value::as_str), Some("success"));
    }

    #[test]
    fn parses_viewer_control_and_offer() {
        let control = parse_control(&json!({
            "client_id": "client-1",
            "ice_id": "ice-1",
            "app_control_id": "app-1",
            "iceServers": [{
                "urls": ["turn:turn.example:3478"],
                "username": "u",
                "credential": "turn-secret"
            }]
        }))
        .unwrap();
        assert_eq!(control.client_id(), "client-1");
        assert_eq!(control.ice_id(), "ice-1");
        assert!(control.turn_server(5).is_ok());
        assert!(!format!("{control:?}").contains("turn-secret"));

        let offer = parse_offer(&json!({
            "client_id": "client-1",
            "data": {
                "type": "offer",
                "ice_id": "ice-1",
                "app_control_id": "app-1",
                "uutier_record": record_json()
            }
        }))
        .unwrap()
        .unwrap();
        assert_eq!(offer.client_id, "client-1");
        assert_eq!(offer.ice_id, "ice-1");
        assert_eq!(offer.control_id.as_deref(), Some("app-1"));
        assert_eq!(offer.record.ipv4, "10.144.0.2");

        assert!(
            parse_offer(&json!({"data": {"type": "answer"}}))
                .unwrap()
                .is_none()
        );
        assert!(
            parse_offer(&json!({
                "data": {"type": "offer", "uutier_record": {"version": 9}}
            }))
            .is_err()
        );
        assert!(parse_control(&json!({"ice_id": "ice"})).is_none());
    }

    #[test]
    fn selects_udp_turn_and_skips_tls() {
        let servers = parse_ice_servers(Some(&json!([
            {"urls": ["turns:turn.example:5349?transport=tcp"], "username": "u", "credential": "p"},
            {"urls": ["turn:turn.example:3478?transport=udp"], "username": "u", "credential": "p"},
            {"urls": ["stun:stun.example:3478"]}
        ])));
        let turn = select_turn(&servers, 5).unwrap();
        assert_eq!(turn.server, "turn:turn.example:3478?transport=udp");
        assert_eq!(turn.username, "u");
        assert_eq!(turn.password, "p");
    }

    #[tokio::test]
    async fn heartbeats_do_not_extend_the_record_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            loop {
                if ws.send(Message::Text("2".into())).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .unwrap();
        let mut session = SignalSession {
            connection: Connection { ws },
            client_id: String::new(),
            ice_id: String::new(),
            app_control_id: String::new(),
            ice_servers: Vec::new(),
        };
        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            session.next_record_until("answer", started + Duration::from_millis(200)),
        )
        .await
        .expect("record deadline did not fire while heartbeats were arriving");
        assert!(matches!(result, Err(TurnError::SignalTimeout)));
        server.abort();
    }
}
