//! 节点通过 UU 信令交换的记录。

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};

use serde::{Deserialize, Serialize};
use uutier_core::REDACTED;

use crate::error::TurnError;

/// 记录格式版本。
pub const RECORD_VERSION: u32 = 2;
/// 记录里最多携带的候选地址数量。
pub const MAX_ENDPOINTS: usize = 8;
/// SOAC 中占位用的最小 SDP。UUTier 不使用 WebRTC，只要求字段存在。
pub const MINIMAL_SDP: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=sctp-port:5000\r\n";

/// 一个节点在 UU 信令里公布的信息。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRecord {
    pub version: u32,
    pub uu_device_id: String,
    /// 网络名和网络密钥的 BLAKE2s-256 哈希，用来拒绝同一账号下的其他网络。
    pub network_id: String,
    pub public_key: String,
    pub ipv4: String,
    pub listen_port: u16,
    #[serde(default)]
    pub endpoints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_addr: Option<String>,
}

impl fmt::Debug for PeerRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerRecord")
            .field("version", &self.version)
            .field("uu_device_id", &REDACTED)
            .field("network_id", &REDACTED)
            .field("public_key", &REDACTED)
            .field("ipv4", &self.ipv4)
            .field("listen_port", &self.listen_port)
            .field("endpoints", &self.endpoints.len())
            .field("relay_addr", &self.relay_addr.as_ref().map(|_| REDACTED))
            .finish()
    }
}

impl PeerRecord {
    /// 检查字段范围。对端身份和虚拟地址由调用方与配置比对。
    pub fn validate(&self) -> Result<(), TurnError> {
        if self.version != RECORD_VERSION {
            return Err(TurnError::InvalidRecord("unsupported record version"));
        }
        if self.uu_device_id.is_empty()
            || self.uu_device_id.len() > 256
            || self.uu_device_id.chars().any(char::is_control)
        {
            return Err(TurnError::InvalidRecord("invalid UU device id"));
        }
        if self.network_id.len() != 64
            || !self.network_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(TurnError::InvalidRecord("invalid network id"));
        }
        if self.public_key.len() != 64
            || !self.public_key.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(TurnError::InvalidRecord("invalid public key"));
        }
        if self.ipv4.parse::<Ipv4Addr>().is_err() {
            return Err(TurnError::InvalidRecord("invalid IPv4 address"));
        }
        if self.listen_port == 0 {
            return Err(TurnError::InvalidRecord("invalid listen port"));
        }
        if self.endpoints.len() > MAX_ENDPOINTS {
            return Err(TurnError::InvalidRecord("too many endpoints"));
        }
        for endpoint in &self.endpoints {
            if endpoint.len() > 64 || endpoint.parse::<SocketAddr>().is_err() {
                return Err(TurnError::InvalidRecord("invalid endpoint"));
            }
        }
        if let Some(relay) = &self.relay_addr
            && relay.parse::<SocketAddr>().is_err()
        {
            return Err(TurnError::InvalidRecord("invalid relay address"));
        }
        Ok(())
    }

    /// 返回解析后的候选地址。
    pub fn endpoints(&self) -> Vec<SocketAddr> {
        self.endpoints
            .iter()
            .filter_map(|endpoint| endpoint.parse().ok())
            .collect()
    }

    /// 返回解析后的 TURN relay 地址。
    pub fn relay_addr(&self) -> Option<SocketAddr> {
        self.relay_addr
            .as_deref()
            .and_then(|relay| relay.parse().ok())
    }
}

/// SOAC 的 `data` 字段。UUTier 只读取 `type` 和 `uutier_record`。
#[derive(Clone, Serialize, Deserialize)]
pub struct SoacData {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ice_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_control_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uutier_record: Option<PeerRecord>,
}

impl fmt::Debug for SoacData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SoacData")
            .field("kind", &self.kind)
            .field("sdp_bytes", &self.sdp.as_ref().map(String::len))
            .field("ice_id", &self.ice_id.as_ref().map(|_| REDACTED))
            .field(
                "app_control_id",
                &self.app_control_id.as_ref().map(|_| REDACTED),
            )
            .field("uutier_record", &self.uutier_record)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> PeerRecord {
        PeerRecord {
            version: RECORD_VERSION,
            uu_device_id: "device-1".to_owned(),
            network_id: "11".repeat(32),
            public_key: "00".repeat(32),
            ipv4: "10.144.0.2".to_owned(),
            listen_port: 12010,
            endpoints: vec!["192.0.2.10:12010".to_owned()],
            relay_addr: Some("198.51.100.20:50000".to_owned()),
        }
    }

    #[test]
    fn round_trips_through_json() {
        let text = serde_json::to_string(&record()).unwrap();
        let decoded: PeerRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(decoded, record());
        assert_eq!(decoded.endpoints().len(), 1);
        assert!(decoded.relay_addr().is_some());
        assert!(decoded.validate().is_ok());
    }

    #[test]
    fn debug_hides_device_id_and_addresses() {
        let text = format!("{:?}", record());
        assert!(!text.contains("device-1"), "{text}");
        assert!(!text.contains("192.0.2.10"), "{text}");
        assert!(!text.contains("198.51.100.20"), "{text}");
        assert!(!text.contains(&"00".repeat(32)), "{text}");
        assert!(text.contains("10.144.0.2"), "{text}");
    }

    #[test]
    fn rejects_out_of_range_fields() {
        for mutate in [
            |record: &mut PeerRecord| record.version = 3,
            |record: &mut PeerRecord| record.uu_device_id.clear(),
            |record: &mut PeerRecord| record.network_id.clear(),
            |record: &mut PeerRecord| record.public_key = "zz".repeat(32),
            |record: &mut PeerRecord| record.ipv4 = "10.144.0.300".to_owned(),
            |record: &mut PeerRecord| record.listen_port = 0,
            |record: &mut PeerRecord| record.endpoints = vec!["bad".to_owned()],
            |record: &mut PeerRecord| record.relay_addr = Some("bad".to_owned()),
            |record: &mut PeerRecord| {
                record.endpoints = (0..=MAX_ENDPOINTS)
                    .map(|index| format!("192.0.2.{}:12010", index + 1))
                    .collect();
            },
        ] {
            let mut value = record();
            mutate(&mut value);
            assert!(value.validate().is_err(), "{value:?}");
        }
    }

    #[test]
    fn soac_data_keeps_unknown_fields_out_of_the_way() {
        let value: SoacData = serde_json::from_value(serde_json::json!({
            "type": "offer",
            "sdp": MINIMAL_SDP,
            "ice_id": "ice",
            "app_control_id": "control",
            "uutier_record": record(),
            "vendor_extra": 1
        }))
        .unwrap();
        assert_eq!(value.kind, "offer");
        assert_eq!(value.uutier_record, Some(record()));
    }
}
