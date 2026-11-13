//! UUTier 节点：组网层运行时、TURN bridge 和状态文件。

#![forbid(unsafe_code)]

pub mod mesh;
pub mod status;
pub mod supervisor;
mod uu_link;

use std::io;
use std::path::PathBuf;

pub use mesh::{
    Action, DEFAULT_TUN_NAME, FrameKind, HANDSHAKE_RETRY_S, HandshakePayload, Identity,
    IdentityError, InitiatorHandshake, Mesh, MeshError, MeshParams, NoiseError, NoiseSession,
    PEER_TIMEOUT_S, PING_INTERVAL_S, PeerRecord, ResponderHandshake, TunDevice, WIRE_VERSION,
    default_identity_path, psk_from_secret,
};
pub use status::{
    StatusWriter, default_status_path, now_unix, read as read_status, write as write_status,
};
pub use supervisor::{Node, NodeOptions};

use uutier_core::CoreError;
use uutier_turn::TurnError;

/// `uutier-node` 产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error(transparent)]
    Config(#[from] CoreError),

    #[error(transparent)]
    Turn(#[from] TurnError),

    #[error(transparent)]
    Mesh(#[from] MeshError),

    #[error(transparent)]
    Identity(#[from] IdentityError),

    #[error("i/o error: {0}")]
    Io(#[from] io::Error),

    #[error(
        "cannot bind UDP port {port}: permission denied; use a port above 1023 or grant CAP_NET_BIND_SERVICE"
    )]
    ListenPermissionDenied { port: u16 },

    #[error("invalid status JSON in {path}: {source}")]
    StatusJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("unsupported status schema_version {version}, expected {expected}")]
    UnsupportedStatusSchema { version: u32, expected: u32 },

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("cannot resolve the identity path: $HOME is not set")]
    IdentityPath,

    #[error("the logged-in UU device is not in the account device list")]
    UuDeviceMissing,

    #[error("the local UU device alias must start with uutier-")]
    UuDeviceUnmarked,

    #[error("the local UU device is not a desktop device")]
    UuDeviceUnusable,

    #[error("{count} marked UUTier devices, at most {max} supported")]
    TooManyUuDevices { count: usize, max: usize },
}

impl NodeError {
    /// 判断启动错误是否由配置、凭据或设备标记引起。
    ///
    /// 调用方据此返回退出码 2，让 systemd 不再自动重启。
    pub fn is_configuration(&self) -> bool {
        match self {
            Self::Config(_)
            | Self::Mesh(_)
            | Self::Identity(_)
            | Self::IdentityPath
            | Self::UuDeviceMissing
            | Self::UuDeviceUnmarked
            | Self::UuDeviceUnusable
            | Self::TooManyUuDevices { .. }
            | Self::ListenPermissionDenied { .. } => true,
            Self::Turn(error) => is_configuration_error(error),
            Self::Io(_)
            | Self::StatusJson { .. }
            | Self::UnsupportedStatusSchema { .. }
            | Self::Json(_) => false,
        }
    }
}

/// 判断 TURN 或凭据错误是否属于配置错误。
pub fn is_configuration_error(error: &TurnError) -> bool {
    matches!(
        error,
        TurnError::Json { .. }
            | TurnError::CredentialsIo { .. }
            | TurnError::InvalidCredentials { .. }
            | TurnError::MissingCredentials { .. }
            | TurnError::InsecurePermissions(_)
            | TurnError::InsecureDirectory(_)
            | TurnError::NotAFile(_)
            | TurnError::CredentialsTooLarge(_)
            | TurnError::MissingPath
            | TurnError::LoginExpired
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_startup_errors_for_exit_codes() {
        assert!(NodeError::Config(CoreError::ConfigInvalid("bad".to_owned())).is_configuration());
        assert!(NodeError::IdentityPath.is_configuration());
        assert!(NodeError::TooManyUuDevices { count: 4, max: 3 }.is_configuration());
        assert!(NodeError::Turn(TurnError::LoginExpired).is_configuration());
        assert!(NodeError::ListenPermissionDenied { port: 443 }.is_configuration());
        assert!(
            NodeError::Turn(TurnError::Json {
                path: PathBuf::from("uu.json"),
                source: serde_json::from_str::<serde_json::Value>("").unwrap_err(),
            })
            .is_configuration()
        );
        assert!(
            NodeError::Turn(TurnError::InvalidCredentials { field: "token" }).is_configuration()
        );
        assert!(
            NodeError::Turn(TurnError::InsecureDirectory(PathBuf::from("credentials")))
                .is_configuration()
        );
        assert!(!NodeError::Turn(TurnError::Timeout).is_configuration());
        assert!(
            !NodeError::Io(io::Error::new(io::ErrorKind::AddrInUse, "port in use"))
                .is_configuration()
        );
    }
}
