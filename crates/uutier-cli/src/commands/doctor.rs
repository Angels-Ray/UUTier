//! `uutier doctor` 子命令。

use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use uutier_core::{UuCredentials, UutierConfig};
use uutier_node::{Identity, default_identity_path};
use uutier_turn::{MAX_UUTIER_DEVICES, UuApi, uutier_devices};

use super::CommandError;
use super::config;

/// 检查配置、凭据、身份、UU API 和 TUN。
pub async fn run(override_path: Option<&Path>) -> Result<(), CommandError> {
    let (path, config) = config::load(override_path)?;
    outln!("ok   config: {}", path.display()).map_err(CommandError::output)?;

    let mut failures = Vec::new();
    match check_credentials(&config) {
        Ok((credentials_path, credentials)) => {
            let missing = credentials.missing_fields();
            if missing.is_empty() {
                outln!("ok   credentials: {}", credentials_path.display())
                    .map_err(CommandError::output)?;
                match check_uu(&credentials).await {
                    Ok(detail) => outln!("ok   uu api: {detail}").map_err(CommandError::output)?,
                    Err(UuCheckError::Api(err)) => {
                        outln!("fail uu api: {err}").map_err(CommandError::output)?;
                        failures.push("uu api");
                    }
                    Err(UuCheckError::Devices(err)) => {
                        outln!("fail uu devices: {err}").map_err(CommandError::output)?;
                        failures.push("uu devices");
                    }
                }
            } else {
                outln!(
                    "fail credentials: {} is missing {}",
                    credentials_path.display(),
                    missing.join(", ")
                )
                .map_err(CommandError::output)?;
                failures.push("credentials");
            }
        }
        Err(err) => {
            outln!("fail credentials: {err}").map_err(CommandError::output)?;
            failures.push("credentials");
        }
    }

    match check_identity() {
        Ok(detail) => outln!("ok   identity: {detail}").map_err(CommandError::output)?,
        Err(err) => {
            outln!("fail identity: {err}").map_err(CommandError::output)?;
            failures.push("identity");
        }
    }
    match check_tun() {
        Ok(detail) => outln!("ok   tun: {detail}").map_err(CommandError::output)?,
        Err(err) => {
            outln!("fail tun: {err}").map_err(CommandError::output)?;
            failures.push("tun");
        }
    }
    if !config.turn.enabled {
        outln!("skip turn: disabled").map_err(CommandError::output)?;
    } else {
        outln!("skip turn: allocation is created during a peer session")
            .map_err(CommandError::output)?;
    }

    if !failures.is_empty() {
        return Err(CommandError::runtime(format!(
            "checks failed: {}",
            failures.join(", ")
        )));
    }
    Ok(())
}

fn check_credentials(config: &UutierConfig) -> Result<(PathBuf, UuCredentials), String> {
    let path = config
        .uu
        .path
        .clone()
        .or_else(uutier_turn::default_credentials_path)
        .ok_or_else(|| "cannot resolve the credentials path".to_owned())?;

    let credentials = uutier_turn::load_from(&path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    Ok((path, credentials))
}

/// `check_uu` 的失败来源：API 调用，或本机设备状态。
enum UuCheckError {
    Api(String),
    Devices(String),
}

async fn check_uu(credentials: &UuCredentials) -> Result<String, UuCheckError> {
    let api = UuApi::new(credentials.clone()).map_err(|err| UuCheckError::Api(err.to_string()))?;
    let list = api
        .device_list()
        .await
        .map_err(|err| UuCheckError::Api(err.to_string()))?;
    let local = list.local(api.device_id()).ok_or_else(|| {
        UuCheckError::Devices("the logged-in device is not in the account device list".to_owned())
    })?;
    if !local.is_uutier() {
        return Err(UuCheckError::Devices(format!(
            "local device alias must start with {}",
            uutier_turn::DEVICE_ALIAS_PREFIX
        )));
    }
    let devices = uutier_devices(&list);
    if !devices
        .iter()
        .any(|device| device.device_id == api.device_id())
    {
        return Err(UuCheckError::Devices(
            "the local device is not a marked desktop device".to_owned(),
        ));
    }
    if devices.len() > MAX_UUTIER_DEVICES {
        return Err(UuCheckError::Devices(format!(
            "{} marked UUTier devices, at most {} supported",
            devices.len(),
            MAX_UUTIER_DEVICES
        )));
    }
    let online = devices.iter().filter(|device| device.online).count();
    let peers = devices.len().saturating_sub(1);
    Ok(format!(
        "{} marked UUTier device(s), {online} online, {peers} peer(s)",
        devices.len()
    ))
}

fn check_identity() -> Result<String, String> {
    let path =
        default_identity_path().ok_or_else(|| "cannot resolve the identity path".to_owned())?;
    if !path.exists() {
        return Ok(format!(
            "{} (not created yet; run `uutier key show`)",
            path.display()
        ));
    }
    let identity = Identity::load(&path).map_err(|err| err.to_string())?;
    Ok(format!("{} peer_id={}", path.display(), identity.peer_id()))
}

fn check_tun() -> Result<String, String> {
    let metadata = std::fs::metadata("/dev/net/tun")
        .map_err(|err| format!("cannot read /dev/net/tun: {err}"))?;
    if !metadata.file_type().is_char_device() {
        return Err("/dev/net/tun is not a character device".to_owned());
    }
    match has_net_admin() {
        Some(true) => {}
        Some(false) => {
            return Err(
                "missing CAP_NET_ADMIN; run `sudo setcap cap_net_admin+ep \"$(command -v uutier)\"`"
                    .to_owned(),
            );
        }
        None => {
            return Err(
                "cannot read CapEff from /proc/self/status, cannot verify CAP_NET_ADMIN".to_owned(),
            );
        }
    }
    if Path::new("/sys/class/net/uutier0").exists() {
        return Err("uutier0 already exists, stop the running node first".to_owned());
    }
    Ok("/dev/net/tun".to_owned())
}

/// 读取 `/proc/self/status` 的有效能力位，判断是否包含 CAP_NET_ADMIN。
fn has_net_admin() -> Option<bool> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let mask = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))?
        .trim();
    let mask = u64::from_str_radix(mask, 16).ok()?;
    Some(mask & (1 << 12) != 0)
}
