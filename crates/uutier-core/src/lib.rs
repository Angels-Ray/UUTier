//! UUTier 的共享类型。

#![forbid(unsafe_code)]

pub mod config;
pub mod credentials;
pub mod error;
pub mod status;

pub use config::{
    DEFAULT_LISTEN_PORT, DEFAULT_MTU_BYTES, DEFAULT_NETWORK_NAME, MIN_MTU_BYTES, NetworkConfig,
    REDACTED, TurnConfig, TurnMode, UuConfig, UutierConfig, config_path, parse_public_key,
};
pub use credentials::{DEFAULT_UU_SIGN_KEY, UuCredentials};
pub use error::CoreError;
pub use status::{
    AllocationState, PathKind, PeerStatus, STATUS_MAX_AGE_S, STATUS_SCHEMA_VERSION, StatusFile,
    TurnStatus, status_path,
};
