//! UUTier 配置模型和校验。

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// 默认的网络名。
pub const DEFAULT_NETWORK_NAME: &str = "uutier";

/// 默认的 UDP 监听端口。
pub const DEFAULT_LISTEN_PORT: u16 = 12010;

/// 默认 MTU，单位 byte。
pub const DEFAULT_MTU_BYTES: u16 = 1280;

/// 允许的最小 MTU，单位 byte。
pub const MIN_MTU_BYTES: u16 = 64;

/// 允许的最大 MTU，单位 byte。
///
/// UDP 载荷上限 65507 byte，减去 40 byte 线上头部和 40 byte TURN 开销。
pub const MAX_MTU_BYTES: u16 = 65_427;

/// 允许的最长 TURN 请求超时，单位 s。
pub const MAX_REQUEST_TIMEOUT_S: u64 = 3600;

/// `network.secret` 的最小长度，单位 byte。网络标识是密钥的快速哈希，
/// 短密钥可以被离线字典攻击还原。
pub const MIN_SECRET_BYTES: usize = 16;

/// 输出配置时替换秘密字段的文本。
pub const REDACTED: &str = "***";

/// UUTier 配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UutierConfig {
    #[serde(default)]
    pub uu: UuConfig,
    #[serde(default)]
    pub turn: TurnConfig,
    #[serde(default)]
    pub network: NetworkConfig,
}

impl UutierConfig {
    /// 从 TOML 文本解析配置。
    pub fn parse(text: &str) -> Result<Self, CoreError> {
        toml::from_str(text)
            .map_err(|err| CoreError::ConfigInvalid(describe_parse_error(text, &err)))
    }

    /// 校验配置。
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.network.name.trim().is_empty() {
            return Err(CoreError::ConfigInvalid(
                "network.name is required".to_owned(),
            ));
        }
        if self.network.secret.trim().is_empty() {
            return Err(CoreError::ConfigInvalid(
                "network.secret is required, generate one with openssl rand -hex 32".to_owned(),
            ));
        }
        if self.network.secret.len() < MIN_SECRET_BYTES {
            return Err(CoreError::ConfigInvalid(format!(
                "network.secret is {} bytes, need at least {MIN_SECRET_BYTES}; generate one with openssl rand -hex 32",
                self.network.secret.len()
            )));
        }
        if self.network.ipv4.trim().is_empty() {
            return Err(CoreError::ConfigInvalid(
                "network.ipv4 is required".to_owned(),
            ));
        }
        let ipv4 = self.network_ipv4()?;
        if ipv4.is_unspecified()
            || ipv4.is_loopback()
            || ipv4.is_multicast()
            || ipv4.is_broadcast()
            || ipv4.octets()[3] == 0
            || ipv4.octets()[3] == 255
        {
            return Err(CoreError::ConfigInvalid(format!(
                "network.ipv4 {ipv4} is not a usable host address in a /24"
            )));
        }
        if !(MIN_MTU_BYTES..=MAX_MTU_BYTES).contains(&self.network.mtu_bytes) {
            return Err(CoreError::ConfigInvalid(format!(
                "network.mtu_bytes is {}, expected {MIN_MTU_BYTES}..={MAX_MTU_BYTES}",
                self.network.mtu_bytes
            )));
        }
        if self.network.listen_port == 0 {
            return Err(CoreError::ConfigInvalid(
                "network.listen_port is 0, expected 1..=65535".to_owned(),
            ));
        }
        if let Some(path) = &self.uu.path
            && !path.is_absolute()
        {
            return Err(CoreError::ConfigInvalid(format!(
                "uu.path {} is not an absolute path",
                path.display()
            )));
        }
        if !(1..=MAX_REQUEST_TIMEOUT_S).contains(&self.turn.request_timeout_s) {
            return Err(CoreError::ConfigInvalid(format!(
                "turn.request_timeout_s is {}, expected 1..={MAX_REQUEST_TIMEOUT_S}",
                self.turn.request_timeout_s
            )));
        }
        if !self.turn.enabled && self.turn.mode == TurnMode::Always {
            return Err(CoreError::ConfigInvalid(
                "turn.mode \"always\" requires turn.enabled = true".to_owned(),
            ));
        }

        Ok(())
    }

    /// 返回网络名。
    pub fn network_name(&self) -> &str {
        &self.network.name
    }

    /// 解析本机的虚拟 IPv4 地址。
    pub fn network_ipv4(&self) -> Result<std::net::Ipv4Addr, CoreError> {
        self.network
            .ipv4
            .parse::<std::net::Ipv4Addr>()
            .map_err(|err| {
                CoreError::ConfigInvalid(format!(
                    "network.ipv4 {} is invalid: {err}",
                    self.network.ipv4
                ))
            })
    }

    /// 返回一份可输出的配置副本，秘密字段被替换。
    pub fn redacted(&self) -> Self {
        let mut redacted = self.clone();
        if !redacted.network.secret.is_empty() {
            redacted.network.secret = REDACTED.to_owned();
        }
        redacted
    }

    /// 序列化配置，秘密字段被替换。
    pub fn to_redacted_toml(&self) -> Result<String, CoreError> {
        toml::to_string_pretty(&self.redacted())
            .map_err(|err| CoreError::ConfigInvalid(err.to_string()))
    }
}

/// UU 凭据配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UuConfig {
    /// 凭据文件路径。不填时使用 `~/.config/uutier/uu.json`。
    #[serde(default)]
    pub path: Option<PathBuf>,
}

/// TURN 配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TurnConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub mode: TurnMode,
    /// TURN 请求超时，单位 s。
    #[serde(default = "default_request_timeout_s")]
    pub request_timeout_s: u64,
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: TurnMode::default(),
            request_timeout_s: default_request_timeout_s(),
        }
    }
}

/// TURN 通道的使用方式。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum TurnMode {
    /// 先尝试直连。5 s 内未完成握手的 peer 改用 TURN。
    #[default]
    #[serde(rename = "direct-first")]
    DirectFirst,
    /// 启动时就使用 TURN，不尝试直连。
    #[serde(rename = "always")]
    Always,
}

impl TurnMode {
    /// 返回稳定的字符串表示。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectFirst => "direct-first",
            Self::Always => "always",
        }
    }
}

/// 解析 32 byte 的十六进制公钥。
pub fn parse_public_key(value: &str) -> Result<[u8; 32], CoreError> {
    let bytes = value.as_bytes();
    if bytes.len() != 64 {
        return Err(CoreError::ConfigInvalid(format!(
            "public_key is {} characters, need 64",
            bytes.len()
        )));
    }
    let mut key = [0u8; 32];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0])
            .ok_or_else(|| CoreError::ConfigInvalid("public_key is not hexadecimal".to_owned()))?;
        let low = hex_nibble(pair[1])
            .ok_or_else(|| CoreError::ConfigInvalid("public_key is not hexadecimal".to_owned()))?;
        key[index] = (high << 4) | low;
    }
    Ok(key)
}

/// 生成不含源码行的解析错误文本，避免回显秘密字段。
fn describe_parse_error(text: &str, error: &toml::de::Error) -> String {
    // 类型错误的消息会带回字段值，值可能是 network.secret。
    let message = if error.message().starts_with("invalid type:") {
        "invalid value type"
    } else {
        error.message()
    };
    let Some(span) = error.span() else {
        return message.to_owned();
    };
    let Some(before) = text.get(..span.start.min(text.len())) else {
        return message.to_owned();
    };
    let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map_or(1, |tail| tail.chars().count() + 1);
    format!("{message} at line {line}, column {column}")
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 虚拟网络配置。
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    #[serde(default = "default_network_name")]
    pub name: String,
    /// 网络密钥，至少 16 byte。同一网络的所有节点相同。
    #[serde(default)]
    pub secret: String,
    /// 本机在虚拟网络中的 IPv4 地址。
    #[serde(default)]
    pub ipv4: String,
    /// TUN 的 MTU，单位 byte。
    #[serde(default = "default_mtu_bytes")]
    pub mtu_bytes: u16,
    /// UDP 监听端口。
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            name: default_network_name(),
            secret: String::new(),
            ipv4: String::new(),
            mtu_bytes: default_mtu_bytes(),
            listen_port: default_listen_port(),
        }
    }
}

impl fmt::Debug for NetworkConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetworkConfig")
            .field("name", &self.name)
            .field("secret", &REDACTED)
            .field("ipv4", &self.ipv4)
            .field("mtu_bytes", &self.mtu_bytes)
            .field("listen_port", &self.listen_port)
            .finish()
    }
}

/// 返回配置文件路径。
pub fn config_path(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let base = xdg_config_home
        .filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| {
            home.filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
                .map(|home| home.join(".config"))
        })?;
    Some(base.join("uutier").join("config.toml"))
}

fn default_network_name() -> String {
    DEFAULT_NETWORK_NAME.to_owned()
}

fn default_mtu_bytes() -> u16 {
    DEFAULT_MTU_BYTES
}

fn default_listen_port() -> u16 {
    DEFAULT_LISTEN_PORT
}

fn default_true() -> bool {
    true
}

fn default_request_timeout_s() -> u64 {
    5
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[turn]
enabled = true
mode = "direct-first"

[network]
name = "uutier"
secret = "hidden-secret-16"
ipv4 = "10.144.0.2"
mtu_bytes = 1280
listen_port = 12010
"#;

    #[test]
    fn parses_config_with_defaults() {
        let config = UutierConfig::parse(SAMPLE).unwrap();
        assert_eq!(config.uu.path, None);
        assert_eq!(config.turn.mode, TurnMode::DirectFirst);
        assert_eq!(config.turn.request_timeout_s, 5);
        assert_eq!(config.network.mtu_bytes, DEFAULT_MTU_BYTES);
        assert_eq!(config.network.listen_port, DEFAULT_LISTEN_PORT);
        assert_eq!(config.network_name(), "uutier");
        config.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(UutierConfig::parse("foo = 1\n").is_err());
        assert!(UutierConfig::parse("[turn]\nfoo = 1\n").is_err());
        assert!(UutierConfig::parse("[network]\nfoo = 1\n").is_err());
        assert!(UutierConfig::parse("[easytier]\nfoo = 1\n").is_err());
        assert!(UutierConfig::parse("[relay]\nfoo = 1\n").is_err());
    }

    #[test]
    fn requires_network_fields() {
        let config = UutierConfig::parse("").unwrap();
        assert!(config.validate().is_err());

        let config = UutierConfig::parse("[network]\nsecret = \"sixteen-byte-key\"\n").unwrap();
        assert!(config.validate().is_err());

        let config =
            UutierConfig::parse("[network]\nsecret = \"sixteen-byte-key\"\nipv4 = \"10.0.0.2\"\n")
                .unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn rejects_short_network_secret() {
        for secret in ["", "change-me"] {
            let text = SAMPLE.replace("hidden-secret-16", secret);
            assert!(
                UutierConfig::parse(&text).unwrap().validate().is_err(),
                "secret {secret:?} was accepted"
            );
        }
        let text = SAMPLE.replace("hidden-secret-16", "sixteen-byte-key");
        UutierConfig::parse(&text).unwrap().validate().unwrap();
    }

    #[test]
    fn rejects_invalid_ip_and_mtu() {
        let text = SAMPLE.replace("10.144.0.2", "10.144.0");
        assert!(UutierConfig::parse(&text).unwrap().validate().is_err());

        let text = SAMPLE.replace("mtu_bytes = 1280", "mtu_bytes = 63");
        assert!(UutierConfig::parse(&text).unwrap().validate().is_err());

        let text = SAMPLE.replace("mtu_bytes = 1280", "mtu_bytes = 70000");
        assert!(UutierConfig::parse(&text).is_err());

        let text = SAMPLE.replace("mtu_bytes = 1280", "mtu_bytes = 65428");
        assert!(UutierConfig::parse(&text).unwrap().validate().is_err());
    }

    #[test]
    fn rejects_unusable_ipv4_addresses() {
        for address in [
            "0.0.0.0",
            "127.0.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "10.144.0.0",
            "10.144.0.255",
        ] {
            let text = SAMPLE.replace("10.144.0.2", address);
            assert!(
                UutierConfig::parse(&text).unwrap().validate().is_err(),
                "{address} was accepted"
            );
        }
    }

    #[test]
    fn rejects_zero_listen_port() {
        let text = SAMPLE.replace("listen_port = 12010", "listen_port = 0");
        assert!(UutierConfig::parse(&text).unwrap().validate().is_err());
    }

    #[test]
    fn always_mode_requires_turn() {
        let text = SAMPLE
            .replace("enabled = true", "enabled = false")
            .replace("mode = \"direct-first\"", "mode = \"always\"");
        assert!(UutierConfig::parse(&text).unwrap().validate().is_err());
    }

    #[test]
    fn accepts_request_timeout_boundaries() {
        for timeout in ["1", "3600"] {
            let text = SAMPLE.replace(
                "[turn]\nenabled = true",
                &format!("[turn]\nenabled = true\nrequest_timeout_s = {timeout}"),
            );
            UutierConfig::parse(&text).unwrap().validate().unwrap();
        }
    }

    #[test]
    fn rejects_out_of_range_request_timeout() {
        for timeout in ["0", "3601", "9223372036854775807"] {
            let text = SAMPLE.replace(
                "[turn]\nenabled = true",
                &format!("[turn]\nenabled = true\nrequest_timeout_s = {timeout}"),
            );
            assert!(
                UutierConfig::parse(&text).unwrap().validate().is_err(),
                "timeout {timeout} was accepted"
            );
        }
    }

    #[test]
    fn parses_credentials_path() {
        let config = UutierConfig::parse("[uu]\npath = \"/tmp/uu.json\"\n").unwrap();
        assert_eq!(config.uu.path, Some(PathBuf::from("/tmp/uu.json")));
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_relative_credentials_path() {
        let text = "[uu]\npath = \"uu.json\"\n";
        let config = UutierConfig::parse(text).unwrap();
        assert!(config.validate().is_err());

        let text = "[uu]\npath = \"\"\n";
        let config = UutierConfig::parse(text).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn parse_errors_do_not_echo_source_lines() {
        let text = "[network]\nsecret = \"leaked-secret\n";
        let error = UutierConfig::parse(text).unwrap_err().to_string();
        assert!(!error.contains("leaked-secret"), "{error}");

        let text = "[network]\nsecret = 123456\n";
        let error = UutierConfig::parse(text).unwrap_err().to_string();
        assert!(!error.contains("123456"), "{error}");
        assert!(error.contains("invalid value type"), "{error}");
    }

    #[test]
    fn parses_public_keys_case_insensitively() {
        let key =
            parse_public_key("ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789")
                .unwrap();
        assert_eq!(key[0], 0xab);
        assert_eq!(key[31], 0x89);
        assert!(parse_public_key("0x00").is_err());
    }

    #[test]
    fn redacts_secrets() {
        let config = UutierConfig::parse(SAMPLE).unwrap();
        let text = config.to_redacted_toml().unwrap();
        assert!(text.contains("secret = \"***\""));
        assert!(!text.contains("hidden"));

        let debug = format!("{config:?}");
        assert!(!debug.contains("hidden"));
    }

    #[test]
    fn resolves_config_path() {
        let path = config_path(
            Some(Path::new("/xdg/config")),
            Some(Path::new("/home/user")),
        );
        assert_eq!(path, Some(PathBuf::from("/xdg/config/uutier/config.toml")));
        let path = config_path(None, Some(Path::new("/home/user")));
        assert_eq!(
            path,
            Some(PathBuf::from("/home/user/.config/uutier/config.toml"))
        );
        assert_eq!(config_path(None, None), None);
        assert_eq!(
            config_path(Some(Path::new("")), Some(Path::new("/home/user"))),
            Some(PathBuf::from("/home/user/.config/uutier/config.toml"))
        );
        assert_eq!(config_path(Some(Path::new("relative")), None), None);
    }
}
