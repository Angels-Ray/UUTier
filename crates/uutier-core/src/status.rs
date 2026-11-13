//! 运行状态文件。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 状态文件格式版本。
pub const STATUS_SCHEMA_VERSION: u32 = 1;

/// 状态文件超过该时间未更新时，视为节点未运行，单位 s。
pub const STATUS_MAX_AGE_S: u64 = 5;

/// 状态文件。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusFile {
    pub schema_version: u32,
    pub pid: u32,
    /// 写入时间，Unix 时间戳，单位 s。
    pub updated_unix: u64,
    #[serde(default)]
    pub peers: Vec<PeerStatus>,
    #[serde(default)]
    pub turn: TurnStatus,
}

impl StatusFile {
    /// 创建状态文件。
    pub fn new(pid: u32, updated_unix: u64) -> Self {
        Self {
            schema_version: STATUS_SCHEMA_VERSION,
            pid,
            updated_unix,
            peers: Vec::new(),
            turn: TurnStatus::default(),
        }
    }

    /// 返回状态文件的存在时间，单位 s。
    pub fn age_s(&self, now_unix: u64) -> u64 {
        now_unix.saturating_sub(self.updated_unix)
    }

    /// 判断状态文件是否过期。
    pub fn is_stale(&self, now_unix: u64) -> bool {
        self.is_older_than(now_unix, STATUS_MAX_AGE_S)
    }

    /// 判断状态文件的存在时间是否超过 `max_age_s`。
    ///
    /// 写入时间晚于当前时间时视为过期，避免被修改过的文件一直显示为最新。
    pub fn is_older_than(&self, now_unix: u64, max_age_s: u64) -> bool {
        self.updated_unix > now_unix || self.age_s(now_unix) > max_age_s
    }
}

/// 一个 peer 的状态。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerStatus {
    pub peer_id: u64,
    pub ipv4: Option<String>,
    pub path: PathKind,
    /// mesh 层测量到的 RTT，单位 µs。
    pub mesh_rtt_us: Option<u32>,
    /// TURN bridge 测量到的 RTT，单位 µs。
    pub turn_rtt_us: Option<u32>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// 实际路径。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PathKind {
    #[serde(rename = "direct")]
    Direct,
    #[serde(rename = "turn")]
    Turn,
}

impl PathKind {
    /// 返回稳定的字符串表示。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Turn => "turn",
        }
    }
}

/// TURN 状态。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnStatus {
    pub enabled: bool,
    pub allocation: AllocationState,
    /// 最近一次分配刷新时间，Unix 时间戳，单位 s。
    pub refreshed_unix: Option<u64>,
    /// 最近的错误。文本不包含凭据和完整公网地址。
    pub last_error: Option<String>,
}

/// TURN 分配状态。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum AllocationState {
    #[default]
    #[serde(rename = "inactive")]
    Inactive,
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "failed")]
    Failed,
}

impl AllocationState {
    /// 返回稳定的字符串表示。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inactive => "inactive",
            Self::Active => "active",
            Self::Failed => "failed",
        }
    }
}

/// 返回状态文件路径。
pub fn status_path(xdg_state_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let base = xdg_state_home
        .filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| {
            home.filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
                .map(|home| home.join(".local").join("state"))
        })?;
    Some(base.join("uutier").join("status.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let mut status = StatusFile::new(42, 1_760_000_000);
        status.peers.push(PeerStatus {
            peer_id: 7,
            ipv4: Some("10.144.0.3".to_owned()),
            path: PathKind::Turn,
            mesh_rtt_us: Some(1_200),
            turn_rtt_us: Some(4_500),
            rx_bytes: 10,
            tx_bytes: 20,
        });
        status.turn = TurnStatus {
            enabled: true,
            allocation: AllocationState::Active,
            refreshed_unix: Some(1_760_000_001),
            last_error: None,
        };
        let text = serde_json::to_string(&status).unwrap();
        let decoded: StatusFile = serde_json::from_str(&text).unwrap();
        assert_eq!(decoded, status);
        assert!(text.contains("\"path\":\"turn\""));
        assert!(text.contains("\"allocation\":\"active\""));
    }

    #[test]
    fn detects_stale_status() {
        let status = StatusFile::new(1, 100);
        assert!(!status.is_stale(105));
        assert!(status.is_stale(106));
        assert_eq!(status.age_s(105), 5);
    }

    #[test]
    fn future_timestamps_are_stale() {
        let status = StatusFile::new(1, 200);
        assert!(status.is_stale(100));
        assert!(status.is_older_than(100, u64::MAX));
        assert_eq!(status.age_s(100), 0);
    }

    #[test]
    fn resolves_status_path() {
        let path = status_path(Some(Path::new("/xdg/state")), Some(Path::new("/home/user")));
        assert_eq!(path, Some(PathBuf::from("/xdg/state/uutier/status.json")));
        let path = status_path(None, Some(Path::new("/home/user")));
        assert_eq!(
            path,
            Some(PathBuf::from("/home/user/.local/state/uutier/status.json"))
        );
        assert_eq!(status_path(None, None), None);
        assert_eq!(
            status_path(Some(Path::new("")), Some(Path::new("/home/user"))),
            Some(PathBuf::from("/home/user/.local/state/uutier/status.json"))
        );
        assert_eq!(status_path(Some(Path::new("relative")), None), None);
    }

    #[test]
    fn path_kinds_have_stable_names() {
        assert_eq!(PathKind::Direct.as_str(), "direct");
        assert_eq!(PathKind::Turn.as_str(), "turn");
        assert_eq!(AllocationState::Inactive.as_str(), "inactive");
        assert_eq!(AllocationState::Active.as_str(), "active");
        assert_eq!(AllocationState::Failed.as_str(), "failed");
    }
}
