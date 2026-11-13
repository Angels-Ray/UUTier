//! 本地 UDP 与 TURN 之间的转发。

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use uutier_core::REDACTED;

use crate::client::{TurnAllocation, TurnEvent, TurnServerConfig};
use crate::error::TurnError;

/// 单次转发的缓冲区大小，单位 byte。
pub const BRIDGE_BUFFER_BYTES: usize = 64 * 1024;

const TURN_PACKET_BYTES: usize = 64 * 1024;
const MAX_REFRESH_INTERVAL: Duration = Duration::from_secs(240);
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const PERMISSION_INTERVAL: Duration = Duration::from_secs(240);

/// TURN bridge 配置。
#[derive(Clone)]
pub struct BridgeConfig {
    /// 对端的 TURN 分配地址。
    pub remote_relay_addr: SocketAddr,
    /// 组网层 UDP socket 在 loopback 上的地址。
    pub local_peer_addr: SocketAddr,
    pub turn: TurnServerConfig,
}

impl fmt::Debug for BridgeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BridgeConfig")
            .field("remote_relay_addr", &REDACTED)
            .field("local_peer_addr", &self.local_peer_addr)
            .field("turn", &self.turn)
            .finish()
    }
}

/// 运行中的 TURN bridge。
pub struct TurnBridge {
    /// 本地 UDP 地址。组网层把该地址当作对端端点。
    pub local_addr: SocketAddr,
    refreshed_unix: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl TurnBridge {
    /// 返回 bridge 是否已经退出。
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// 返回最近一次 TURN 分配刷新时间，Unix 时间戳，单位 s。
    pub fn refreshed_unix(&self) -> u64 {
        self.refreshed_unix.load(Ordering::Relaxed)
    }
}

impl fmt::Debug for TurnBridge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnBridge")
            .field("local_addr", &self.local_addr)
            .field("finished", &self.task.is_finished())
            .finish()
    }
}

impl Drop for TurnBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 启动 TURN bridge。先做 TURN 分配，再创建到对端 relay 的权限。
pub async fn start(config: BridgeConfig) -> Result<TurnBridge, TurnError> {
    let allocation = TurnAllocation::allocate(&config.turn).await?;
    start_with_allocation(allocation, config.remote_relay_addr, config.local_peer_addr).await
}

/// 用已经建立的 TURN 分配启动 bridge。分配里的 relay 地址由调用方通过信令公布。
pub async fn start_with_allocation(
    mut allocation: TurnAllocation,
    remote_relay_addr: SocketAddr,
    local_peer_addr: SocketAddr,
) -> Result<TurnBridge, TurnError> {
    let local = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let local_addr = local.local_addr()?;
    allocation.create_permission(remote_relay_addr).await?;
    let refreshed_unix = Arc::new(AtomicU64::new(unix_now()));
    let refreshed = Arc::clone(&refreshed_unix);
    let task = tokio::spawn(async move {
        if let Err(error) = run(
            local,
            allocation,
            remote_relay_addr,
            local_peer_addr,
            refreshed,
        )
        .await
        {
            warn!(%error, "TURN bridge stopped");
        }
    });
    Ok(TurnBridge {
        local_addr,
        refreshed_unix,
        task,
    })
}

async fn run(
    local: UdpSocket,
    mut allocation: TurnAllocation,
    remote_relay_addr: SocketAddr,
    local_peer_addr: SocketAddr,
    refreshed: Arc<AtomicU64>,
) -> Result<(), TurnError> {
    let mut next_refresh = tokio::time::Instant::now() + refresh_interval(allocation.lifetime_s());
    let mut next_permission = tokio::time::Instant::now() + PERMISSION_INTERVAL;
    let mut local_buffer = vec![0u8; BRIDGE_BUFFER_BYTES];
    let mut turn_packet = vec![0u8; TURN_PACKET_BYTES];
    // 定时器只注册一次，避免每个数据包都创建并取消 timeout。
    let timer = tokio::time::sleep_until(wake_at(&allocation, next_refresh, next_permission));
    tokio::pin!(timer);
    loop {
        tokio::select! {
            result = local.recv_from(&mut local_buffer) => {
                let (len, from) = result?;
                if from != local_peer_addr {
                    debug!("ignoring unexpected TURN bridge source");
                    continue;
                }
                allocation.send_to(remote_relay_addr, &local_buffer[..len]).await?;
            }
            result = allocation.recv_into(&mut turn_packet) => {
                match result? {
                    TurnEvent::Data { from, payload } => {
                        // TURN 权限按 IP 授予，同一 TURN 服务器上的其他分配也能送到这里。
                        if from != remote_relay_addr {
                            debug!("ignoring TURN data from an unexpected peer");
                            continue;
                        }
                        if payload.len() > BRIDGE_BUFFER_BYTES {
                            debug!("dropping oversized TURN payload");
                            continue;
                        }
                        local.send_to(&turn_packet[payload], local_peer_addr).await?;
                    }
                    TurnEvent::Refreshed => {
                        refreshed.store(unix_now(), Ordering::Relaxed);
                        next_refresh = tokio::time::Instant::now()
                            + refresh_interval(allocation.lifetime_s());
                    }
                    TurnEvent::PermissionCreated => {
                        next_permission = tokio::time::Instant::now() + PERMISSION_INTERVAL;
                    }
                    TurnEvent::ControlFailed(error) => return Err(error),
                    TurnEvent::None => {}
                }
            }
            _ = &mut timer => {
                let now = tokio::time::Instant::now();
                if let Some(deadline) = allocation.control_deadline() {
                    if now >= deadline {
                        return Err(TurnError::Timeout);
                    }
                    allocation.resend_control().await?;
                } else if now >= next_refresh {
                    allocation.begin_refresh().await?;
                } else if now >= next_permission {
                    allocation.begin_permission(remote_relay_addr).await?;
                }
                timer
                    .as_mut()
                    .reset(wake_at(&allocation, next_refresh, next_permission));
            }
        }
    }
}

/// 下一次定时器唤醒时间。控制请求未完成时只按请求的重发和超时时间唤醒。
fn wake_at(
    allocation: &TurnAllocation,
    next_refresh: tokio::time::Instant,
    next_permission: tokio::time::Instant,
) -> tokio::time::Instant {
    match allocation.control_wake() {
        Some(wake) => wake,
        None => next_refresh.min(next_permission),
    }
}

/// 分配有效期的一半，限制在 30 s 到 240 s 之间。
fn refresh_interval(lifetime_s: u32) -> Duration {
    Duration::from_secs(u64::from(lifetime_s) / 2).clamp(MIN_REFRESH_INTERVAL, MAX_REFRESH_INTERVAL)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_interval_tracks_lifetime() {
        assert_eq!(refresh_interval(600), Duration::from_secs(240));
        assert_eq!(refresh_interval(480), Duration::from_secs(240));
        assert_eq!(refresh_interval(300), Duration::from_secs(150));
        assert_eq!(refresh_interval(60), Duration::from_secs(30));
        assert_eq!(refresh_interval(40), Duration::from_secs(30));
        assert_eq!(refresh_interval(0), Duration::from_secs(30));
    }
}
