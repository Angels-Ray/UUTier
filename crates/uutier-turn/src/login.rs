//! UU 扫码登录。终端显示二维码，确认后把凭据写入 `uu.json`。

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use tracing::warn;
use uuid::Uuid;

use uutier_core::UuCredentials;

use crate::error::TurnError;
use crate::store;
use crate::uu::UuApi;

const QR_EXPIRED: i64 = 1190;
const STATUS_TIMEOUT_S: u64 = 60;
const GENERATE_TIMEOUT_S: u64 = 15;

/// 登录选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginOptions {
    /// 等待扫码的超时时间，单位 s。
    pub timeout_s: u64,
    /// 忽略已保存的设备 ID，注册一个带 UUTier 标记的新设备。
    pub new_device: bool,
}

impl Default for LoginOptions {
    fn default() -> Self {
        Self {
            timeout_s: 300,
            new_device: false,
        }
    }
}

/// 登录结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginOutcome {
    /// 写入的凭据文件路径。
    pub credentials_path: PathBuf,
}

/// 执行 UU 扫码登录，成功后写入 `credentials_path`。
pub async fn login(
    options: LoginOptions,
    credentials_path: &Path,
) -> Result<LoginOutcome, TurnError> {
    if !std::io::stdout().is_terminal() {
        return Err(TurnError::QrNotTerminal);
    }
    // `new_device` 会清空旧设备 ID。登录失败时恢复原文件，避免旧身份丢失。
    let previous = if options.new_device {
        match store::load_from(credentials_path) {
            Ok(credentials) => Some(credentials),
            Err(TurnError::CredentialsIo { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                None
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let result = login_inner(options, credentials_path).await;
    if result.is_err()
        && let Some(previous) = previous
        && let Err(error) = store::save(credentials_path, &previous)
    {
        // 登录本身已经失败，恢复失败只记录，保留原始错误。
        warn!(error = %error, "cannot restore the previous credentials");
    }
    result
}

async fn login_inner(
    options: LoginOptions,
    credentials_path: &Path,
) -> Result<LoginOutcome, TurnError> {
    let mut credentials = load_credentials(credentials_path)?;
    if options.new_device {
        credentials.device_id.clear();
        credentials.client_id = Uuid::new_v4().to_string();
        credentials.system_id = Uuid::new_v4().to_string().to_uppercase();
    }
    if credentials.client_id.is_empty() {
        credentials.client_id = Uuid::new_v4().to_string();
    }
    if credentials.system_id.is_empty() {
        credentials.system_id = Uuid::new_v4().to_string().to_uppercase();
    }
    if credentials.machine_guid.is_empty() {
        credentials.machine_guid = machine_guid();
    }

    if credentials.device_id.is_empty() {
        let api = UuApi::anonymous(credentials.clone())?;
        let identity = credentials.clone();
        let body = tokio::task::spawn_blocking(move || device_body(&identity))
            .await
            .map_err(|_| TurnError::Internal)?;
        let (status, value) = api
            .post_raw(
                "/api/v1/device/windows/init",
                Some(&body),
                GENERATE_TIMEOUT_S,
            )
            .await?;
        let data = ok_data(status, &value)?;
        credentials.device_id = data
            .get("device_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(TurnError::InvalidResponse)?
            .to_owned();
    }
    // 先保存设备身份。中途退出后，下次登录沿用同一个设备。
    store::save(credentials_path, &credentials)?;

    let api = UuApi::anonymous(credentials.clone())?;
    let Some(deadline) =
        tokio::time::Instant::now().checked_add(Duration::from_secs(options.timeout_s.max(1)))
    else {
        return Err(TurnError::LoginTimeout);
    };

    loop {
        let (status, value) = api
            .post_raw("/api/v1/qrcode/gen/login", None, GENERATE_TIMEOUT_S)
            .await?;
        let data = ok_data(status, &value)?;
        let url = data
            .get("qrcode_jump_url")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(TurnError::InvalidResponse)?
            .to_owned();
        let ticket = data
            .get("status_query_ticket")
            .or_else(|| data.get("token"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(TurnError::InvalidResponse)?
            .to_owned();

        print_qr(&url)?;
        print_line(format_args!("Waiting for scan..."));

        let mut last_status = 1i64;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(TurnError::LoginTimeout);
            }
            let query = json!({
                "token": ticket,
                "qrcode_jump_url": url,
                "login_status": last_status,
            });
            let (status, value) = match api
                .post_raw(
                    "/api/v1/qrcode/login/status",
                    Some(&query),
                    STATUS_TIMEOUT_S,
                )
                .await
            {
                Ok(result) => result,
                // 一次查询失败或超时不代表二维码失效。
                Err(TurnError::Timeout) | Err(TurnError::InvalidResponse) => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                Err(error) => return Err(error),
            };

            match qr_action(last_status, status, &value) {
                QrAction::Wait(next) => last_status = next,
                QrAction::Scanned => {
                    last_status = 2;
                    print_line(format_args!("Scanned, confirm the sign-in on your phone."));
                }
                QrAction::Refresh(delay) => {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    break;
                }
                QrAction::Expired => {
                    print_line(format_args!("QR code expired, generating a new one."));
                    break;
                }
                QrAction::Confirmed => {
                    print_line(format_args!("Confirmed, completing sign-in..."));
                    let body = json!({"token": ticket, "qrcode_jump_url": url});
                    let (status, value) = api
                        .post_raw("/api/v1/login/by_qrcode", Some(&body), STATUS_TIMEOUT_S)
                        .await?;
                    let data = ok_data(status, &value)?;
                    let token = data
                        .get("token")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .ok_or(TurnError::InvalidResponse)?;
                    let user_id = data
                        .get("user_id")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .ok_or(TurnError::InvalidResponse)?;
                    let csrf = csrf_from_token(token).ok_or(TurnError::InvalidResponse)?;
                    credentials.token = token.to_owned();
                    credentials.user_id = user_id.to_owned();
                    credentials.csrf = csrf;
                    store::save(credentials_path, &credentials)?;
                    print_line(format_args!(
                        "Login succeeded: user {user_id}, credentials {}",
                        credentials_path.display()
                    ));
                    return Ok(LoginOutcome {
                        credentials_path: credentials_path.to_path_buf(),
                    });
                }
                QrAction::Failed(error) => return Err(error),
            }
        }
    }
}

fn load_credentials(path: &Path) -> Result<UuCredentials, TurnError> {
    match store::load_from(path) {
        Ok(credentials) => Ok(credentials),
        Err(TurnError::CredentialsIo { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(UuCredentials::default())
        }
        Err(error) => Err(error),
    }
}

fn ok_data(status: u16, value: &Value) -> Result<&Value, TurnError> {
    let code = value.get("code").and_then(Value::as_i64);
    if status == 200 && code == Some(0) {
        return Ok(value.get("data").unwrap_or(&Value::Null));
    }
    match code {
        Some(1120) => Err(TurnError::LoginExpired),
        Some(code) => Err(TurnError::Api { code }),
        None => Err(TurnError::HttpStatus { status }),
    }
}

enum QrAction {
    Wait(i64),
    Scanned,
    Confirmed,
    Refresh(Duration),
    Expired,
    Failed(TurnError),
}

fn qr_action(last_status: i64, http_status: u16, value: &Value) -> QrAction {
    let code = value.get("code").and_then(Value::as_i64);
    if code == Some(QR_EXPIRED) {
        return QrAction::Expired;
    }
    if http_status != 200 || code != Some(0) {
        return QrAction::Failed(match code {
            Some(code) => TurnError::Api { code },
            None => TurnError::HttpStatus {
                status: http_status,
            },
        });
    }
    let status = value
        .get("data")
        .and_then(|data| data.get("status"))
        .and_then(Value::as_i64);
    match status {
        Some(1) => QrAction::Wait(1),
        Some(2) if last_status != 2 => QrAction::Scanned,
        Some(2) => QrAction::Wait(2),
        Some(0) => QrAction::Refresh(Duration::from_millis(350)),
        Some(3) => QrAction::Refresh(Duration::ZERO),
        Some(4) => QrAction::Confirmed,
        _ => QrAction::Failed(TurnError::InvalidResponse),
    }
}

fn print_qr(url: &str) -> Result<(), TurnError> {
    // 二维码编码了登录地址，重定向到文件或日志时不能输出。
    if !std::io::stdout().is_terminal() {
        return Err(TurnError::QrNotTerminal);
    }
    let code = qrcode::QrCode::new(url.as_bytes()).map_err(|_| TurnError::Internal)?;
    let image = code.render::<qrcode::render::unicode::Dense1x2>().build();
    print_line(format_args!(
        "\nScan this QR code with the NetEase UU Remote app:\n\n{image}\n"
    ));
    Ok(())
}

/// 写一行终端提示。终端关闭后继续轮询只会失败，忽略写入错误即可。
fn print_line(args: std::fmt::Arguments<'_>) {
    use std::io::Write as _;

    let _ = writeln!(std::io::stdout().lock(), "{args}");
}

fn csrf_from_token(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("csrf")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn device_body(credentials: &UuCredentials) -> Value {
    json!({
        "name": device_name(),
        "client_id": credentials.client_id,
        "system_id": credentials.system_id,
        "machine_guid": credentials.machine_guid,
        "os": os_name(),
        "base_board": base_board(),
        "cpu": cpu_name(),
        "video": video(),
        "mac": mac_address(),
        "memory": memory_mb(),
        "screen": "1920x1080",
    })
}

fn device_name() -> String {
    let hostname = hostname();
    let name: String = hostname.chars().take(48).collect();
    format!("{}{name}", crate::uu::DEVICE_ALIAS_PREFIX)
}

fn hostname() -> String {
    let name = read_trimmed("/proc/sys/kernel/hostname");
    if name.is_empty() {
        "Linux".to_owned()
    } else {
        name
    }
}

fn os_name() -> String {
    read_trimmed("/etc/os-release")
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|value| value.trim_matches('"').to_owned())
        .unwrap_or_else(|| "Linux".to_owned())
}

fn cpu_name() -> String {
    read_trimmed("/proc/cpuinfo")
        .lines()
        .find_map(|line| {
            line.strip_prefix("model name")
                .and_then(|rest| rest.split_once(':'))
                .map(|(_, value)| value.trim().to_owned())
        })
        .unwrap_or_else(|| std::env::consts::ARCH.to_owned())
}

fn base_board() -> String {
    let value = format!(
        "{} {}",
        read_trimmed("/sys/class/dmi/id/board_vendor"),
        read_trimmed("/sys/class/dmi/id/board_name")
    );
    let value = value.trim();
    if value.is_empty() {
        "Unknown".to_owned()
    } else {
        value.to_owned()
    }
}

fn video() -> Vec<String> {
    let mut cards = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let device = entry.path().join("device");
            let vendor = read_trimmed(&device.join("vendor").to_string_lossy());
            let id = read_trimmed(&device.join("device").to_string_lossy());
            if vendor.is_empty() && id.is_empty() {
                continue;
            }
            cards.push(format!(
                "PCI {}:{}",
                vendor.trim_start_matches("0x"),
                id.trim_start_matches("0x")
            ));
        }
    }
    if cards.is_empty() {
        cards.push("Unknown".to_owned());
    }
    cards
}

fn mac_address() -> String {
    let mut names: Vec<String> = fs::read_dir("/sys/class/net")
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().join("device").exists())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
        .iter()
        .map(|name| read_trimmed(&format!("/sys/class/net/{name}/address")))
        .find(|address| !address.is_empty() && address != "00:00:00:00:00:00")
        .map(|address| address.to_uppercase().replace(':', "-"))
        .unwrap_or_default()
}

fn memory_mb() -> u64 {
    read_trimmed("/proc/meminfo")
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|value| {
            value
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .ok()
        })
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}

fn machine_guid() -> String {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        let value = read_trimmed(path);
        if !value.is_empty() {
            return value;
        }
    }
    Uuid::new_v4().to_string()
}

fn read_trimmed(path: &str) -> String {
    fs::read_to_string(path)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_csrf_from_jwt_payload() {
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"u","csrf":"c-1"}"#);
        assert_eq!(
            csrf_from_token(&format!("header.{payload}.signature")),
            Some("c-1".to_owned())
        );
        assert_eq!(csrf_from_token("bad"), None);
        assert_eq!(csrf_from_token("a.e30.c"), None);
    }

    #[test]
    fn qr_status_maps_to_actions() {
        let value = |status: i64| json!({"code": 0, "data": {"status": status}});
        assert!(matches!(qr_action(1, 200, &value(1)), QrAction::Wait(1)));
        assert!(matches!(qr_action(1, 200, &value(2)), QrAction::Scanned));
        assert!(matches!(qr_action(2, 200, &value(2)), QrAction::Wait(2)));
        assert!(matches!(qr_action(1, 200, &value(4)), QrAction::Confirmed));
        assert!(matches!(
            qr_action(1, 200, &value(3)),
            QrAction::Refresh(delay) if delay.is_zero()
        ));
        assert!(matches!(
            qr_action(1, 200, &value(0)),
            QrAction::Refresh(delay) if !delay.is_zero()
        ));
        assert!(matches!(
            qr_action(1, 400, &json!({"code": 1190})),
            QrAction::Expired
        ));
        assert!(matches!(
            qr_action(1, 500, &json!({})),
            QrAction::Failed(TurnError::HttpStatus { status: 500 })
        ));
        assert!(matches!(
            qr_action(1, 200, &json!({"code": 0, "data": {"status": 9}})),
            QrAction::Failed(TurnError::InvalidResponse)
        ));
    }

    #[test]
    fn device_body_has_required_fields() {
        let credentials = UuCredentials {
            client_id: "client".to_owned(),
            system_id: "system".to_owned(),
            machine_guid: "machine".to_owned(),
            ..UuCredentials::default()
        };
        let body = device_body(&credentials);
        assert_eq!(body["client_id"], "client");
        assert_eq!(body["system_id"], "system");
        assert_eq!(body["machine_guid"], "machine");
        assert!(
            body["name"]
                .as_str()
                .unwrap()
                .starts_with(crate::uu::DEVICE_ALIAS_PREFIX)
        );
        for key in [
            "name",
            "client_id",
            "system_id",
            "machine_guid",
            "os",
            "base_board",
            "cpu",
            "video",
            "mac",
            "memory",
            "screen",
        ] {
            assert!(body.get(key).is_some(), "{key}");
        }
    }
}
