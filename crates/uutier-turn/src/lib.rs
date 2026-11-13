//! UU 登录、凭据存储、TURN 客户端和本地 UDP bridge。

#![forbid(unsafe_code)]

pub mod bridge;
pub mod client;
pub mod error;
pub mod login;
pub mod record;
pub mod signaling;
pub mod store;
pub mod uu;

pub use bridge::{
    BRIDGE_BUFFER_BYTES, BridgeConfig, TurnBridge, start as start_bridge,
    start_with_allocation as start_bridge_with_allocation,
};
pub use client::{TurnAllocation, TurnServerConfig, allocate};
pub use error::TurnError;
pub use login::{LoginOptions, LoginOutcome, login};
pub use record::{MAX_ENDPOINTS, MINIMAL_SDP, PeerRecord, RECORD_VERSION, SoacData};
pub use signaling::{HostEvent, HostOffer, HostRoom, SignalRecord, SignalSession, ViewerControl};
pub use store::{default_credentials_path, load, load_from, remove, save, uutier_credentials_path};
pub use uu::{
    DEVICE_ALIAS_PREFIX, MAX_UUTIER_DEVICES, Room, UU_API_BASE, UuApi, UuDevice, UuDeviceList,
    uutier_devices,
};
