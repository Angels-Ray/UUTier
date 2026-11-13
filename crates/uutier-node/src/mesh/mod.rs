//! 自研组网层。

use std::fmt;
use std::io;
use std::net::Ipv4Addr;

use blake2::{Blake2s256, Digest};
use thiserror::Error;
use uutier_core::{CoreError, REDACTED, UutierConfig};

pub mod identity;
pub mod noise;
pub mod runtime;
pub mod tun;
pub mod wire;

pub use identity::{
    IDENTITY_KEY_LEN, Identity, IdentityError, default_identity_path, identity_path,
};
pub use noise::{
    HANDSHAKE_PAYLOAD_LEN, HandshakePayload, InitiatorHandshake, NoiseError, NoiseSession, PSK_LEN,
    ResponderHandshake, psk_from_secret,
};
pub use runtime::{
    Action, HANDSHAKE_RETRY_S, MAX_PEERS, Mesh, PEER_TIMEOUT_S, PING_INTERVAL_S, PeerRecord,
};
pub use tun::{TunDevice, VIRTUAL_NETMASK};
pub use wire::{FrameKind, Header, WIRE_VERSION, WireError};

/// TUN 设备名。
pub const DEFAULT_TUN_NAME: &str = "uutier0";

/// 198.18.0.0/15。Clash 等代理软件分配虚拟地址，不能作为直连候选。
pub(crate) fn is_benchmarking(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    octets[0] == 198 && (octets[1] & 0xfe) == 18
}

/// 判断 IPv4 地址能否作为直连候选。
///
/// 排除回环、未指定、组播、广播、链路本地、代理虚拟地址和文档地址。
pub(crate) fn is_usable_endpoint(address: Ipv4Addr) -> bool {
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || address.is_broadcast()
        || address.is_link_local()
        || is_benchmarking(address)
        || address.is_documentation())
}

/// 组网层参数。字段从 `[network]` 派生。
#[derive(Clone, PartialEq, Eq)]
pub struct MeshParams {
    pub name: String,
    pub secret: String,
    pub ipv4: Ipv4Addr,
    pub mtu_bytes: u16,
    pub listen_port: u16,
}

impl MeshParams {
    /// 从配置生成组网参数。
    pub fn from_config(config: &UutierConfig) -> Result<Self, CoreError> {
        config.validate()?;
        Ok(Self {
            name: config.network.name.clone(),
            secret: config.network.secret.clone(),
            ipv4: config.network_ipv4()?,
            mtu_bytes: config.network.mtu_bytes,
            listen_port: config.network.listen_port,
        })
    }

    /// 派生网络标识。网络名或密钥不同的节点不能互通。
    pub fn network_id(&self) -> String {
        let mut hasher = Blake2s256::new();
        hasher.update(self.name.as_bytes());
        hasher.update([0]);
        hasher.update(self.secret.as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

impl fmt::Debug for MeshParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MeshParams")
            .field("name", &self.name)
            .field("secret", &REDACTED)
            .field("ipv4", &self.ipv4)
            .field("mtu_bytes", &self.mtu_bytes)
            .field("listen_port", &self.listen_port)
            .finish()
    }
}

/// 组网层错误。
#[derive(Debug, Error)]
pub enum MeshError {
    #[error(transparent)]
    Identity(#[from] IdentityError),

    #[error(transparent)]
    Noise(#[from] NoiseError),

    #[error(transparent)]
    Wire(#[from] WireError),

    #[error("tun error: {0}")]
    Tun(String),

    #[error("i/o error: {0}")]
    Io(#[from] io::Error),

    #[error("peer record matches the local identity or IPv4 address")]
    PeerIsLocal,

    #[error("duplicate peer ID {0}")]
    DuplicatePeerId(u64),

    #[error("peer {0} changed its IPv4 address")]
    PeerIpv4Changed(u64),

    #[error("duplicate peer IPv4 {0}")]
    DuplicatePeerIpv4(Ipv4Addr),

    #[error("peer IPv4 {0} is outside the local /24")]
    PeerOutsideSubnet(Ipv4Addr),

    #[error("peer IPv4 {0} is not a usable host address in a /24")]
    PeerIpv4Unusable(Ipv4Addr),

    #[error("peer limit {max} reached")]
    TooManyPeers { max: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> UutierConfig {
        UutierConfig::parse(
            r#"
[network]
name = "uutier"
secret = "hidden-secret-16"
ipv4 = "10.144.0.2"
mtu_bytes = 1280
listen_port = 12010
"#,
        )
        .unwrap()
    }

    #[test]
    fn derives_mesh_params() {
        let params = MeshParams::from_config(&config()).unwrap();
        assert_eq!(params.name, "uutier");
        assert_eq!(params.secret, "hidden-secret-16");
        assert_eq!(params.ipv4, Ipv4Addr::new(10, 144, 0, 2));
        assert_eq!(params.mtu_bytes, 1280);
        assert_eq!(params.listen_port, 12010);
        assert_eq!(WIRE_VERSION, 1);
        assert_eq!(DEFAULT_TUN_NAME, "uutier0");
    }

    #[test]
    fn debug_redacts_secret() {
        let params = MeshParams::from_config(&config()).unwrap();
        let text = format!("{params:?}");
        assert!(!text.contains("hidden"));
        assert!(text.contains("10.144.0.2"));
    }

    #[test]
    fn rejects_invalid_config() {
        assert!(MeshParams::from_config(&UutierConfig::parse("").unwrap()).is_err());
    }

    #[test]
    fn network_id_depends_on_name_and_secret() {
        let params = MeshParams::from_config(&config()).unwrap();
        let same = MeshParams::from_config(&config()).unwrap();
        assert_eq!(params.network_id(), same.network_id());

        let mut changed = MeshParams::from_config(&config()).unwrap();
        changed.secret = "other".to_owned();
        assert_ne!(params.network_id(), changed.network_id());
    }

    #[test]
    fn unusable_endpoints_are_rejected() {
        for address in [
            Ipv4Addr::new(127, 0, 0, 1),
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(224, 0, 0, 1),
            Ipv4Addr::BROADCAST,
            Ipv4Addr::new(169, 254, 1, 1),
            Ipv4Addr::new(198, 18, 0, 1),
            Ipv4Addr::new(192, 0, 2, 1),
        ] {
            assert!(!is_usable_endpoint(address), "{address}");
        }
        assert!(is_usable_endpoint(Ipv4Addr::new(192, 168, 1, 2)));
    }

    #[test]
    fn mesh_rejects_a_third_peer_and_unusable_addresses() {
        let params = MeshParams::from_config(&config()).unwrap();
        let local = Identity::generate();
        let now = std::time::Instant::now();
        let mut mesh = Mesh::new(params, local, Vec::new(), now).unwrap();
        for last_octet in 3..=4 {
            let peer = Identity::generate();
            let record = PeerRecord::new(
                *peer.public_key(),
                Ipv4Addr::new(10, 144, 0, last_octet),
                None,
            );
            assert!(mesh.add_peer(record, now).unwrap());
        }
        assert_eq!(mesh.peer_count(), MAX_PEERS);
        let peer = Identity::generate();
        let record = PeerRecord::new(*peer.public_key(), Ipv4Addr::new(10, 144, 0, 5), None);
        assert!(matches!(
            mesh.add_peer(record, now),
            Err(MeshError::TooManyPeers { .. })
        ));

        let mut other = Mesh::new(
            MeshParams::from_config(&config()).unwrap(),
            Identity::generate(),
            Vec::new(),
            now,
        )
        .unwrap();
        let peer = Identity::generate();
        let record = PeerRecord::new(*peer.public_key(), Ipv4Addr::new(10, 144, 0, 255), None);
        assert!(matches!(
            other.add_peer(record, now),
            Err(MeshError::PeerIpv4Unusable(_))
        ));
    }

    #[test]
    fn mesh_adds_and_removes_discovered_peers() {
        let params = MeshParams::from_config(&config()).unwrap();
        let local = Identity::generate();
        let peer = Identity::generate();
        let now = std::time::Instant::now();
        let mut mesh = Mesh::new(params, local, Vec::new(), now).unwrap();
        let record = PeerRecord::new(*peer.public_key(), Ipv4Addr::new(10, 144, 0, 3), None);
        let peer_id = record.peer_id;
        assert!(mesh.add_peer(record.clone(), now).unwrap());
        assert_eq!(mesh.peer_count(), 1);
        assert!(!mesh.add_peer(record, now).unwrap());
        assert!(mesh.remove_peer(peer_id));
        assert_eq!(mesh.peer_count(), 0);
        assert!(!mesh.remove_peer(peer_id));
    }
}
