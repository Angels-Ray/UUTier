//! UU REST 客户端。负责请求签名、房间创建、房间加入和设备列表。

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use sha2::Sha256;
use uutier_core::{REDACTED, UuCredentials};

use crate::error::TurnError;

/// UU 云 API 地址。
pub const UU_API_BASE: &str = "https://api.nrd.nie.163.com";

/// UUTier 设备的别名前缀。只有别名以此开头的设备参与组网。
pub const DEVICE_ALIAS_PREFIX: &str = "uutier-";

/// 请求签名用到的固定头。
const FIXED_PARAMS: [(&str, &str); 7] = [
    ("x-param-plat", "1"),
    ("x-param-vn", "4.42.0.2770"),
    ("x-param-vc", "2770"),
    ("x-param-pkgn", "com.netease.gameviewer"),
    ("x-param-chn", "gwqd"),
    ("x-param-lang", "zh-CN"),
    ("x-param-cnt", "CN"),
];

const USER_AGENT: &str = "UURemote/4.42.0 (Windows NT 10.0; Win64; x64)";
const TOKEN_INVALID: i64 = 1120;
const HOST_OFFLINE: i64 = 2001;
const HOST_BUSY: i64 = 2002;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// 响应体上限，单位 byte。设备列表和房间信息远小于这个值。
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// 读取 JSON 响应，超过上限时报错，避免一次读入无界数据。
async fn read_json_body(mut response: reqwest::Response) -> Result<Value, TurnError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(TurnError::InvalidResponse);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| TurnError::InvalidResponse)?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(TurnError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| TurnError::InvalidResponse)
}

/// UU 房间。信令地址和房间 token 都是凭据，Debug 不输出。
#[derive(Clone)]
pub struct Room {
    pub signaling_list: Vec<String>,
    pub ws_connect_timeout_ms: u64,
    token: String,
}

impl Room {
    /// 返回房间 token。
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl fmt::Debug for Room {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Room")
            .field("signaling_list", &REDACTED)
            .field("ws_connect_timeout_ms", &self.ws_connect_timeout_ms)
            .field("token", &REDACTED)
            .finish()
    }
}

/// UU 设备列表中的一项。
#[derive(Clone, PartialEq, Eq)]
pub struct UuDevice {
    pub device_id: String,
    pub alias: String,
    pub platform: Option<i64>,
    pub online: bool,
}

impl fmt::Debug for UuDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UuDevice")
            .field("device_id", &REDACTED)
            .field("alias", &REDACTED)
            .field("platform", &self.platform)
            .field("online", &self.online)
            .finish()
    }
}

impl UuDevice {
    /// 设备别名是否带 UUTier 标记。
    pub fn is_uutier(&self) -> bool {
        self.alias.starts_with(DEVICE_ALIAS_PREFIX)
    }
}

/// 按账号关系分组的设备列表。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct UuDeviceList {
    /// 当前登录设备。
    pub current: Option<UuDevice>,
    /// 本账号绑定的设备。
    pub bound: Vec<UuDevice>,
    /// 其他账号分享的设备。UUTier 不加入这些设备。
    pub shared: Vec<UuDevice>,
}

impl fmt::Debug for UuDeviceList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UuDeviceList")
            .field("current", &self.current.is_some())
            .field("bound", &self.bound.len())
            .field("shared", &self.shared.len())
            .finish()
    }
}

impl UuDeviceList {
    /// 查找当前登录设备。
    pub fn local(&self, device_id: &str) -> Option<&UuDevice> {
        self.current
            .iter()
            .chain(self.bound.iter())
            .find(|device| device.device_id == device_id)
    }

    /// 返回本账号的设备，当前设备和绑定设备各出现一次。
    pub fn all(&self) -> impl Iterator<Item = &UuDevice> {
        self.current.iter().chain(self.bound.iter())
    }
}

/// 一个 UUTier 网络最多容纳的设备数。
pub const MAX_UUTIER_DEVICES: usize = 3;

/// 返回参与组网的设备。只接受本账号、别名带标记的桌面设备。
///
/// 离线设备保留位置，上线后直接接上。各节点看到相同的设备集合，角色分配
/// 不受 `status` 抖动影响。
pub fn uutier_devices(list: &UuDeviceList) -> Vec<&UuDevice> {
    let mut devices: Vec<&UuDevice> = list
        .all()
        .filter(|device| device.is_uutier() && is_desktop(device.platform))
        .collect();
    devices.sort_by(|left, right| left.device_id.cmp(&right.device_id));
    devices.dedup_by(|left, right| left.device_id == right.device_id);
    devices
}

/// 平台 2、3、5 分别是 Android、iOS 和 TV。手机和平板不参与组网。
fn is_desktop(platform: Option<i64>) -> bool {
    !matches!(platform, Some(2 | 3 | 5))
}

/// 已签名的 UU REST 客户端。
pub struct UuApi {
    credentials: UuCredentials,
    http: reqwest::Client,
}

impl UuApi {
    /// 创建客户端。凭据必须包含必填字段。
    pub fn new(credentials: UuCredentials) -> Result<Self, TurnError> {
        let missing = credentials.missing_fields();
        if !missing.is_empty() {
            return Err(TurnError::MissingCredentials { fields: missing });
        }
        Self::build(credentials)
    }

    /// 创建登录前使用的客户端。token 和 csrf 可以为空。
    pub(crate) fn anonymous(credentials: UuCredentials) -> Result<Self, TurnError> {
        Self::build(credentials)
    }

    fn build(credentials: UuCredentials) -> Result<Self, TurnError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| TurnError::InvalidResponse)?;
        Ok(Self { credentials, http })
    }

    /// 发送签名 POST，返回原始 HTTP 状态和 JSON。登录接口需要自行解释错误码。
    pub(crate) async fn post_raw(
        &self,
        path: &str,
        body: Option<&Value>,
        timeout_s: u64,
    ) -> Result<(u16, Value), TurnError> {
        let body_text = match body {
            Some(value) => serde_json::to_string(value).map_err(|_| TurnError::InvalidResponse)?,
            None => String::new(),
        };
        let headers = self.build_headers("POST", path, &body_text)?;
        let response = self
            .http
            .post(format!("{UU_API_BASE}{path}"))
            .timeout(Duration::from_secs(timeout_s.max(1)))
            .headers(headers)
            .body(body_text)
            .send()
            .await
            .map_err(|err| {
                if err.is_timeout() {
                    TurnError::Timeout
                } else {
                    TurnError::InvalidResponse
                }
            })?;
        let status = response.status().as_u16();
        let value = read_json_body(response).await?;
        Ok((status, value))
    }

    /// 返回当前账号的 UU 设备 ID。
    pub fn device_id(&self) -> &str {
        &self.credentials.device_id
    }

    /// 创建 host 房间。
    pub async fn create_room(&self) -> Result<Room, TurnError> {
        let value = self
            .request(Method::POST, "/api/v1/room/create", Some(&json!({})))
            .await?;
        parse_room(value)
    }

    /// 加入目标设备的 host 房间。
    pub async fn join_room(&self, device_id: &str) -> Result<Room, TurnError> {
        let segment = path_segment(device_id)?;
        let path = format!("/api/v1/room/join/by_device/{segment}");
        let value = self
            .request(Method::POST, &path, Some(&json!({"force_join": false})))
            .await?;
        parse_room(value)
    }

    /// 读取账号下的设备列表，保留当前设备、绑定设备和分享设备的区别。
    pub async fn device_list(&self) -> Result<UuDeviceList, TurnError> {
        let value = self
            .request(Method::GET, "/api/v1/device/list", None)
            .await?;
        let code = value.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code == TOKEN_INVALID {
            return Err(TurnError::LoginExpired);
        }
        if code != 0 {
            return Err(TurnError::Api { code });
        }
        let data = value
            .get("data")
            .and_then(Value::as_object)
            .ok_or(TurnError::InvalidResponse)?;
        Ok(UuDeviceList {
            current: data.get("current_device").and_then(parse_device),
            bound: parse_device_array(data.get("my_binded_devices")),
            shared: parse_device_array(data.get("others_shared_devices")),
        })
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, TurnError> {
        let body_text = match body {
            Some(value) => serde_json::to_string(value).map_err(|_| TurnError::InvalidResponse)?,
            None => String::new(),
        };
        let headers = self.build_headers(method.as_str(), path, &body_text)?;
        let request = self
            .http
            .request(method, format!("{UU_API_BASE}{path}"))
            .headers(headers)
            .body(body_text);
        let response = request
            .send()
            .await
            .map_err(|_| TurnError::InvalidResponse)?;
        let status = response.status().as_u16();
        let value = read_json_body(response).await?;
        let code = value.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code == TOKEN_INVALID {
            return Err(TurnError::LoginExpired);
        }
        if code == HOST_OFFLINE {
            return Err(TurnError::HostOffline);
        }
        if code == HOST_BUSY {
            return Err(TurnError::HostBusy);
        }
        if status != 200 {
            return Err(if code > 0 {
                TurnError::Api { code }
            } else {
                TurnError::HttpStatus { status }
            });
        }
        if code != 0 {
            return Err(TurnError::Api { code });
        }
        Ok(value)
    }

    fn build_headers(&self, method: &str, path: &str, body: &str) -> Result<HeaderMap, TurnError> {
        let mut params = BTreeMap::new();
        for (key, value) in FIXED_PARAMS {
            params.insert(key.to_owned(), value.to_owned());
        }
        for (key, value) in [
            ("x-param-client-id", &self.credentials.client_id),
            ("x-param-device-id", &self.credentials.device_id),
            ("x-param-system-id", &self.credentials.system_id),
            ("x-param-user-id", &self.credentials.user_id),
        ] {
            if !value.is_empty() {
                params.insert(key.to_owned(), value.clone());
            }
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| TurnError::Internal)?
            .as_secs();
        params.insert("x-param-ts".to_owned(), timestamp.to_string());
        let signature = signature(
            self.credentials.effective_sign_key(),
            method,
            path,
            &params,
            body,
        )?;
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        headers.insert("Accept", HeaderValue::from_static("application/json"));
        headers.insert("User-Agent", HeaderValue::from_static(USER_AGENT));
        if !self.credentials.token.is_empty() {
            headers.insert(
                "Authorization",
                credential_header(&format!("Bearer {}", self.credentials.token), "token")?,
            );
            headers.insert(
                "Cookie",
                credential_header(
                    &format!(
                        "nrd_access_token={}; nrd_csrf_access_token={}",
                        self.credentials.token, self.credentials.csrf
                    ),
                    "token/csrf",
                )?,
            );
        }
        if !self.credentials.csrf.is_empty() {
            headers.insert(
                "X-CSRF-TOKEN",
                credential_header(&self.credentials.csrf, "csrf")?,
            );
        }
        for (key, value) in &params {
            let name = HeaderName::from_bytes(key.as_bytes()).map_err(|_| {
                TurnError::InvalidCredentials {
                    field: "request parameter",
                }
            })?;
            headers.insert(name, credential_header(value, "request parameter")?);
        }
        headers.insert(
            "X-Param-SIGN",
            HeaderValue::from_str(&signature).map_err(|_| TurnError::Internal)?,
        );
        Ok(headers)
    }
}

impl fmt::Debug for UuApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UuApi { .. }")
    }
}

fn credential_header(value: &str, field: &'static str) -> Result<HeaderValue, TurnError> {
    HeaderValue::from_str(value).map_err(|_| TurnError::InvalidCredentials { field })
}

fn signature(
    sign_key: &str,
    method: &str,
    path: &str,
    params: &BTreeMap<String, String>,
    body: &str,
) -> Result<String, TurnError> {
    let query = params
        .iter()
        .filter(|(key, _)| key.starts_with("x-param-") && key.as_str() != "x-param-sign")
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let text = format!("{}{path}{query}{body}", method.to_uppercase());
    let mut mac =
        Hmac::<Sha256>::new_from_slice(sign_key.as_bytes()).map_err(|_| TurnError::Internal)?;
    mac.update(text.as_bytes());
    Ok(to_hex(&mac.finalize().into_bytes()))
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[usize::from(byte >> 4)] as char);
        output.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn parse_room(value: Value) -> Result<Room, TurnError> {
    let data = value
        .get("data")
        .and_then(Value::as_object)
        .ok_or(TurnError::InvalidResponse)?;
    let token = data
        .get("token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 16_384)
        .ok_or(TurnError::InvalidResponse)?
        .to_owned();
    let mut signaling_list = Vec::new();
    if let Some(value) = data.get("signaling_server").and_then(Value::as_str)
        && !value.is_empty()
    {
        signaling_list.push(value.to_owned());
    }
    if let Some(items) = data.get("signaling_list").and_then(Value::as_array) {
        for item in items.iter().filter_map(Value::as_str) {
            if !item.is_empty() && !signaling_list.iter().any(|existing| existing == item) {
                signaling_list.push(item.to_owned());
            }
        }
    }
    if signaling_list.is_empty() {
        return Err(TurnError::InvalidResponse);
    }
    let ws_connect_timeout_ms = data
        .get("ws_connect_timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Ok(Room {
        signaling_list,
        ws_connect_timeout_ms,
        token,
    })
}

fn parse_device_array(value: Option<&Value>) -> Vec<UuDevice> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_device).collect())
        .unwrap_or_default()
}

fn parse_device(value: &Value) -> Option<UuDevice> {
    let object = value.as_object()?;
    let device_id = object.get("device_id")?.as_str()?.to_owned();
    if device_id.is_empty() {
        return None;
    }
    let alias = object
        .get("alias")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let platform = object.get("platform").and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    });
    let online = match object.get("status") {
        Some(Value::String(status)) => status.eq_ignore_ascii_case("CONNECTED"),
        Some(Value::Number(status)) => status.as_i64().is_some_and(|status| status != 0),
        _ => false,
    };
    Some(UuDevice {
        device_id,
        alias,
        platform,
        online,
    })
}

fn path_segment(value: &str) -> Result<String, TurnError> {
    if value.is_empty() || value == "." || value == ".." || value.chars().any(char::is_control) {
        return Err(TurnError::InvalidRecord("invalid UU device id"));
    }
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_covers_params_path_and_body() {
        let mut params = BTreeMap::new();
        params.insert("x-param-plat".to_owned(), "1".to_owned());
        params.insert("x-param-ts".to_owned(), "1700000000".to_owned());
        let signature = signature(
            "alWiSzXZTLu3WfFnw13uBru3",
            "POST",
            "/api/v1/room/create",
            &params,
            "{}",
        )
        .unwrap();
        assert_eq!(
            signature,
            "15947b7ca13c50510fae1066675442369813124924affc3d771a19ce2662854b"
        );
    }

    #[test]
    fn room_parses_signaling_server_and_list() {
        let room = parse_room(json!({
            "code": 0,
            "data": {
                "token": "room-token",
                "signaling_server": "wss://a.example",
                "signaling_list": ["wss://a.example", "wss://b.example"],
                "ws_connect_timeout_ms": 5000
            }
        }))
        .unwrap();
        assert_eq!(
            room.signaling_list,
            vec!["wss://a.example", "wss://b.example"]
        );
        assert_eq!(room.ws_connect_timeout_ms, 5000);
        assert_eq!(room.token(), "room-token");
        assert!(!format!("{room:?}").contains("room-token"));
    }

    #[test]
    fn room_rejects_missing_token_or_signaling() {
        assert!(parse_room(json!({"code": 0, "data": {"signaling_server": "a"}})).is_err());
        assert!(parse_room(json!({"code": 0, "data": {"token": "t"}})).is_err());
        assert!(parse_room(json!({"code": 0, "data": []})).is_err());
    }

    #[test]
    fn device_ids_are_escaped_as_one_segment() {
        assert_eq!(path_segment("device-1").unwrap(), "device-1");
        assert_eq!(path_segment("a/b?c").unwrap(), "a%2Fb%3Fc");
        assert!(path_segment("").is_err());
        assert!(path_segment("..").is_err());
        assert!(path_segment("a\nb").is_err());
    }

    #[test]
    fn parses_string_status_and_string_platform() {
        let device = parse_device(&json!({
            "device_id": "d1",
            "alias": "uutier-a",
            "platform": "1",
            "status": "CONNECTED"
        }))
        .unwrap();
        assert!(device.online);
        assert_eq!(device.platform, Some(1));
        assert!(device.is_uutier());

        let device = parse_device(&json!({
            "device_id": "d2",
            "alias": "uutier-b",
            "status": "DISCONNECTED"
        }))
        .unwrap();
        assert!(!device.online);
    }

    #[test]
    fn keeps_device_groups_and_filters_by_marker() {
        let list = UuDeviceList {
            current: Some(UuDevice {
                device_id: "local".to_owned(),
                alias: "uutier-a".to_owned(),
                platform: Some(1),
                online: true,
            }),
            bound: vec![
                UuDevice {
                    device_id: "peer".to_owned(),
                    alias: "uutier-b".to_owned(),
                    platform: Some(1),
                    online: true,
                },
                UuDevice {
                    device_id: "normal".to_owned(),
                    alias: "desktop".to_owned(),
                    platform: Some(1),
                    online: true,
                },
                UuDevice {
                    device_id: "phone".to_owned(),
                    alias: "uutier-phone".to_owned(),
                    platform: Some(2),
                    online: true,
                },
                UuDevice {
                    device_id: "offline".to_owned(),
                    alias: "uutier-offline".to_owned(),
                    platform: Some(1),
                    online: false,
                },
            ],
            shared: vec![UuDevice {
                device_id: "shared".to_owned(),
                alias: "uutier-shared".to_owned(),
                platform: Some(1),
                online: true,
            }],
        };
        let devices = uutier_devices(&list);
        assert_eq!(
            devices
                .iter()
                .map(|device| device.device_id.as_str())
                .collect::<Vec<_>>(),
            vec!["local", "offline", "peer"]
        );
        assert_eq!(list.local("local").unwrap().alias, "uutier-a");
        assert_eq!(list.shared.len(), 1);
    }
}
