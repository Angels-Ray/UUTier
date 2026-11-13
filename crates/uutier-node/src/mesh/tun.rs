//! 虚拟网卡。

use std::io;
use std::net::Ipv4Addr;

use tun::{AsyncDevice, Configuration, create_as_async};

use super::{DEFAULT_TUN_NAME, MeshError, MeshParams};

/// 虚拟网络的掩码。所有节点使用同一个 /24 网段。
pub const VIRTUAL_NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);

/// 运行中的 TUN 设备。
pub struct TunDevice {
    inner: AsyncDevice,
    ipv4: Ipv4Addr,
    mtu_bytes: u16,
}

impl TunDevice {
    /// 创建并启用 TUN 设备。
    pub fn create(params: &MeshParams) -> Result<Self, MeshError> {
        let mut config = Configuration::default();
        config
            .tun_name(DEFAULT_TUN_NAME)
            .address(params.ipv4)
            .netmask(VIRTUAL_NETMASK)
            .mtu(params.mtu_bytes)
            .up();
        let inner =
            create_as_async(&config).map_err(|error| MeshError::Tun(describe_error(&error)))?;
        Ok(Self {
            inner,
            ipv4: params.ipv4,
            mtu_bytes: params.mtu_bytes,
        })
    }

    /// 返回虚拟 IPv4 地址。
    pub fn ipv4(&self) -> Ipv4Addr {
        self.ipv4
    }

    /// 返回 MTU，单位 byte。
    pub fn mtu_bytes(&self) -> u16 {
        self.mtu_bytes
    }

    /// 读取一个 IP 包。
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.recv(buf).await
    }

    /// 写入一个 IP 包。
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.inner.send(buf).await
    }
}

/// 给常见的 TUN 创建失败附上处理办法。
fn describe_error(error: &::tun::Error) -> String {
    let hint = match error {
        ::tun::Error::Io(source) => match source.kind() {
            io::ErrorKind::PermissionDenied => {
                Some("run as root or grant CAP_NET_ADMIN with `setcap cap_net_admin+ep`")
            }
            io::ErrorKind::NotFound => Some("load the tun module and check /dev/net/tun"),
            io::ErrorKind::AlreadyExists | io::ErrorKind::ResourceBusy => {
                Some("another process is using uutier0")
            }
            _ => None,
        },
        _ => None,
    };
    match hint {
        Some(hint) => format!("{error}; {hint}"),
        None => error.to_string(),
    }
}

impl std::fmt::Debug for TunDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunDevice")
            .field("name", &DEFAULT_TUN_NAME)
            .field("ipv4", &self.ipv4)
            .field("mtu_bytes", &self.mtu_bytes)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires root or CAP_NET_ADMIN"]
    async fn creates_and_removes_device() {
        let params = MeshParams {
            name: "uutier".to_owned(),
            secret: "secret".to_owned(),
            ipv4: Ipv4Addr::new(10, 144, 0, 2),
            mtu_bytes: 1280,
            listen_port: 12010,
        };
        let device = TunDevice::create(&params).unwrap();
        assert_eq!(device.ipv4(), params.ipv4);
        assert_eq!(device.mtu_bytes(), params.mtu_bytes);
        let mut buf = [0u8; 1500];
        let _ =
            tokio::time::timeout(std::time::Duration::from_millis(10), device.recv(&mut buf)).await;
    }
}
