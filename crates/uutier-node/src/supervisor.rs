//! 节点生命周期。

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tracing::{debug, info, warn};
use uutier_core::{
    AllocationState, PathKind, StatusFile, TurnMode, UutierConfig, parse_public_key,
};

use crate::NodeError;
use crate::mesh::wire::{DATA_NONCE_LEN, HEADER_LEN, TAG_LEN};
use crate::mesh::{
    Action, Identity, Mesh, MeshError, MeshParams, PeerRecord, TunDevice, default_identity_path,
};
use crate::status::StatusWriter;
use crate::uu_link::{DIRECT_PROBE_S, UuEvent, UuRuntime};

/// 启动参数。
#[derive(Debug, Clone)]
pub struct NodeOptions {
    pub config: UutierConfig,
    /// 状态文件路径。
    pub status_path: PathBuf,
}

/// 一个 peer 的 UU 链路状态。
struct PeerLink {
    /// 对端的 UU 设备 ID。设备离开计划后按它移除 peer。
    uu_device_id: Option<String>,
    /// 最近一次可用的直连端点，用于 TURN 断开后回退。
    direct_endpoint: Option<SocketAddr>,
    bridge: Option<uutier_turn::TurnBridge>,
    /// direct-first 模式下等待直连握手的截止时间。
    deadline: Option<Instant>,
}

impl PeerLink {
    fn new() -> Self {
        Self {
            uu_device_id: None,
            direct_endpoint: None,
            bridge: None,
            deadline: None,
        }
    }

    fn has_direct(&self) -> bool {
        self.direct_endpoint.is_some()
    }
}

/// 运行中的 UUTier 节点。
pub struct Node {
    mesh: Mesh,
    tun: TunDevice,
    socket: UdpSocket,
    status: StatusWriter,
    turn_enabled: bool,
    uu: Option<UuRuntime>,
    links: HashMap<u64, PeerLink>,
    turn_last_error: Option<String>,
    status_write_failed: bool,
    /// 数据路径的 I/O 错误只在状态变化时记录，避免每个包都写日志。
    udp_send_failed: bool,
    tun_write_failed: bool,
    /// 上一次状态写入。文件系统阻塞时不叠加新的写入任务。
    status_write: Option<tokio::task::JoinHandle<Result<(), NodeError>>>,
}

enum Input {
    Tun(usize),
    Udp(usize, SocketAddr),
    Tick,
    Uu(Option<UuEvent>),
}

impl Node {
    /// 创建 TUN、绑定 UDP 端口并加载身份。
    pub async fn start(options: NodeOptions) -> Result<Self, NodeError> {
        options.config.validate()?;
        let params = MeshParams::from_config(&options.config)?;
        let identity_path = default_identity_path().ok_or(NodeError::IdentityPath)?;
        let identity = Identity::create_or_load(&identity_path)?;

        let tun = TunDevice::create(&params)?;
        let socket = match UdpSocket::bind(("0.0.0.0", params.listen_port)).await {
            Ok(socket) => socket,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(NodeError::ListenPermissionDenied {
                    port: params.listen_port,
                });
            }
            Err(err) => return Err(NodeError::Io(err)),
        };
        let listen_port = socket.local_addr()?.port();
        let uu = UuRuntime::start(&options.config, &identity, &params, listen_port).await?;
        let mesh = Mesh::new(params, identity, Vec::new(), Instant::now())?;
        let node = Self {
            mesh,
            tun,
            socket,
            status: StatusWriter::new(options.status_path),
            turn_enabled: options.config.turn.enabled,
            uu: Some(uu),
            links: HashMap::new(),
            turn_last_error: None,
            status_write_failed: false,
            udp_send_failed: false,
            tun_write_failed: false,
            status_write: None,
        };
        node.log_start();
        Ok(node)
    }

    /// 返回状态写入器。
    pub fn status_writer(&self) -> &StatusWriter {
        &self.status
    }

    /// 运行节点，直到出错或调用方丢弃 future。
    pub async fn run(mut self) -> Result<(), NodeError> {
        let mtu_bytes = usize::from(self.mesh.params().mtu_bytes);
        let mut tun_buf = vec![0u8; mtu_bytes + 64];
        let mut udp_buf = vec![0u8; mtu_bytes + HEADER_LEN + DATA_NONCE_LEN + TAG_LEN + 64];
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // 状态文件每 2 s 重写一次，validity 窗口为 5 s。
        let mut status_tick = 0u8;

        loop {
            let input = tokio::select! {
                result = self.tun.recv(&mut tun_buf) => Input::Tun(result?),
                result = self.socket.recv_from(&mut udp_buf) => {
                    let (len, from) = result?;
                    Input::Udp(len, from)
                }
                _ = ticker.tick() => Input::Tick,
                event = async {
                    match self.uu.as_mut() {
                        Some(uu) => uu.events.recv().await,
                        None => std::future::pending::<Option<UuEvent>>().await,
                    }
                } => Input::Uu(event),
            };

            match input {
                Input::Tun(len) => {
                    let actions = self.mesh.handle_ip(&tun_buf[..len]);
                    self.execute(actions).await;
                }
                Input::Udp(len, from) => {
                    let actions = self
                        .mesh
                        .handle_datagram(Instant::now(), from, &udp_buf[..len]);
                    self.execute(actions).await;
                }
                Input::Tick => {
                    let now = Instant::now();
                    self.check_links(now);
                    let actions = self.mesh.tick(now);
                    self.execute(actions).await;
                    status_tick = status_tick.wrapping_add(1);
                    if status_tick % 2 == 1 {
                        self.write_status().await;
                    }
                }
                Input::Uu(Some(event)) => {
                    self.handle_uu_event(event, Instant::now());
                }
                Input::Uu(None) => {
                    // 所有 UU 链路任务已结束。接收端关闭后 `recv` 立即返回
                    // `None`，继续在 select 中轮询会让主循环空转。
                    warn!("UU event channel closed, continuing without UU signaling");
                    self.uu = None;
                }
            }
        }
    }

    fn log_start(&self) {
        let params = self.mesh.params();
        let listen_port = self
            .socket
            .local_addr()
            .map(|address| address.port())
            .unwrap_or(params.listen_port);
        info!(
            peer_id = self.mesh.local_peer_id(),
            ipv4 = %params.ipv4,
            mtu_bytes = params.mtu_bytes,
            listen_port,
            peers = self.mesh.peer_count(),
            "mesh started"
        );
    }

    fn handle_uu_event(&mut self, event: UuEvent, now: Instant) {
        match event {
            UuEvent::Discovered { record } => {
                if !record
                    .network_id
                    .eq_ignore_ascii_case(&self.mesh.params().network_id())
                {
                    warn!("peer record belongs to another network");
                    return;
                }
                let public_key = match parse_public_key(&record.public_key) {
                    Ok(public_key) => public_key,
                    Err(error) => {
                        warn!(error = %error, "peer record has an invalid public key");
                        return;
                    }
                };
                let ipv4 = match record.ipv4.parse::<Ipv4Addr>() {
                    Ok(ipv4) => ipv4,
                    Err(error) => {
                        warn!(error = %error, "peer record has an invalid IPv4 address");
                        return;
                    }
                };
                let always = self
                    .uu
                    .as_ref()
                    .is_some_and(|uu| uu.mode == TurnMode::Always);
                let endpoints: Vec<SocketAddr> = if always {
                    Vec::new()
                } else {
                    record
                        .endpoints()
                        .into_iter()
                        .filter(|endpoint| match endpoint.ip() {
                            std::net::IpAddr::V4(address) => {
                                crate::mesh::is_usable_endpoint(address)
                            }
                            std::net::IpAddr::V6(_) => false,
                        })
                        .collect()
                };
                let mesh_record = PeerRecord::new(public_key, ipv4, endpoints.first().copied());
                let peer_id = mesh_record.peer_id;
                // 对端重新生成身份密钥后 peer ID 变化，按设备 ID 移除旧条目。
                let mut stale = Vec::new();
                for (&id, link) in &self.links {
                    if id != peer_id
                        && link.uu_device_id.as_deref() == Some(record.uu_device_id.as_str())
                    {
                        stale.push(id);
                    }
                }
                for id in stale {
                    info!(
                        peer_id = id,
                        "peer identity changed, removing the old entry"
                    );
                    self.links.remove(&id);
                    self.mesh.remove_peer(id);
                }
                match self.mesh.add_peer(mesh_record, now) {
                    Ok(_) => {}
                    Err(MeshError::PeerIpv4Changed(id)) => {
                        // 对端改了虚拟 IPv4，旧记录的会话和地址都已失效。
                        info!(peer_id = id, "peer IPv4 changed, rebuilding the peer entry");
                        self.links.remove(&id);
                        self.mesh.remove_peer(id);
                        let record = PeerRecord::new(public_key, ipv4, endpoints.first().copied());
                        if let Err(error) = self.mesh.add_peer(record, now) {
                            warn!(peer_id, error = %error, "peer record rejected");
                            return;
                        }
                    }
                    Err(error) => {
                        warn!(peer_id, error = %error, "peer record rejected");
                        return;
                    }
                }
                let link = self.links.entry(peer_id).or_insert_with(PeerLink::new);
                link.uu_device_id = Some(record.uu_device_id.clone());
                tracing::debug!(peer_id, endpoints = endpoints.len(), "peer record received");
                if always {
                    link.deadline = None;
                    if link.bridge.is_some() {
                        self.activate_turn(peer_id, now);
                    }
                    return;
                }
                if !endpoints.is_empty() {
                    link.direct_endpoint = endpoints.first().copied();
                    self.mesh.set_peer_candidates(peer_id, &endpoints, now);
                }
                // 已有会话时重复的记录不重置直连探测窗口。
                if !self.mesh.peer_has_session(peer_id) {
                    link.deadline = Some(now + Duration::from_secs(DIRECT_PROBE_S));
                    if link.bridge.is_some() && !link.has_direct() {
                        self.activate_turn(peer_id, now);
                    }
                }
            }
            UuEvent::Devices { device_ids } => {
                let keep: HashSet<String> = device_ids.into_iter().collect();
                let stale: Vec<u64> = self
                    .links
                    .iter()
                    .filter(|(_, link)| {
                        link.uu_device_id
                            .as_ref()
                            .is_some_and(|id| !keep.contains(id))
                    })
                    .map(|(&peer_id, _)| peer_id)
                    .collect();
                for peer_id in stale {
                    info!(peer_id, "peer left the marked UU device set");
                    self.links.remove(&peer_id);
                    self.mesh.remove_peer(peer_id);
                }
            }
            UuEvent::Bridge { peer_id, bridge } => {
                let Some(path) = self.mesh.peer_path(peer_id) else {
                    tracing::debug!(peer_id, "dropping TURN bridge for an unknown peer");
                    return;
                };
                let always = self
                    .uu
                    .as_ref()
                    .is_some_and(|uu| uu.mode == TurnMode::Always);
                let has_session = self.mesh.peer_has_session(peer_id);
                // 只有已证实走直连的会话才不需要 bridge。路径为 TURN 时替换旧 bridge。
                if !always && has_session && path == PathKind::Direct {
                    info!(peer_id, "direct path established, releasing TURN bridge");
                    return;
                }
                let link = self.links.entry(peer_id).or_insert_with(PeerLink::new);
                let local_addr = bridge.local_addr;
                link.bridge = Some(bridge);
                self.mesh.set_turn_endpoint(peer_id, local_addr);
                self.turn_last_error = None;
                if always || (has_session && path == PathKind::Turn) {
                    self.activate_turn(peer_id, now);
                }
            }
            UuEvent::Failed { peer_id, error } => {
                match peer_id {
                    Some(peer_id) => warn!(peer_id, error = %error, "UU link failed"),
                    None => warn!(error = %error, "UU link failed"),
                }
                self.turn_last_error = Some(error);
            }
            UuEvent::Ended { peer_id, error } => {
                // 信令连接关闭后 TURN 分配仍按自己的生命周期工作，bridge 保留。
                tracing::debug!(peer_id, "UU signaling ended, keeping the TURN allocation");
                if let Some(error) = error {
                    self.turn_last_error = Some(error);
                }
            }
        }
    }

    /// 在 direct-first 超时后启用 TURN，并清理已无用途的 bridge。
    fn check_links(&mut self, now: Instant) {
        let mut activate = Vec::new();
        let mut release = Vec::new();
        let mut stopped = Vec::new();
        let always = self
            .uu
            .as_ref()
            .is_some_and(|uu| uu.mode == TurnMode::Always);
        for (&peer_id, link) in &self.links {
            if link
                .bridge
                .as_ref()
                .is_some_and(|bridge| bridge.is_finished())
            {
                stopped.push(peer_id);
                continue;
            }
            if self.mesh.peer_has_session(peer_id) {
                // 会话已确认走直连时 bridge 没有用途，不必等探测截止时间。
                // TURN 会话依赖 bridge，不能释放。
                if !always
                    && link.bridge.is_some()
                    && self.mesh.peer_path(peer_id) != Some(PathKind::Turn)
                {
                    release.push(peer_id);
                }
                continue;
            }
            if let Some(deadline) = link.deadline
                && now >= deadline
                && link.bridge.is_some()
            {
                activate.push(peer_id);
            }
        }

        for peer_id in activate {
            self.activate_turn(peer_id, now);
        }
        for peer_id in release {
            if let Some(link) = self.links.get_mut(&peer_id) {
                link.bridge = None;
                link.deadline = None;
            }
        }
        for peer_id in stopped {
            warn!(peer_id, "TURN bridge stopped");
            self.turn_last_error = Some("TURN bridge stopped".to_owned());
            let fallback = self.links.get_mut(&peer_id).and_then(|link| {
                link.bridge = None;
                link.deadline = None;
                link.direct_endpoint
            });
            if self.mesh.peer_path(peer_id) == Some(PathKind::Turn)
                && let Some(endpoint) = fallback
            {
                self.mesh.set_peer_transport(peer_id, endpoint, false, now);
            }
        }
    }

    fn activate_turn(&mut self, peer_id: u64, now: Instant) {
        let Some(link) = self.links.get_mut(&peer_id) else {
            return;
        };
        let Some(local_addr) = link.bridge.as_ref().map(|bridge| bridge.local_addr) else {
            return;
        };
        link.deadline = None;
        self.mesh.set_peer_transport(peer_id, local_addr, true, now);
        info!(peer_id, "using TURN");
    }

    async fn execute(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Send { to, datagram } => {
                    let result = self.socket.send_to(&datagram, to).await.map(|_| ());
                    self.mesh.recycle(datagram);
                    note_io_result(&mut self.udp_send_failed, result, "udp send");
                }
                Action::Deliver(packet) => {
                    let result = match self.tun.send(&packet).await {
                        Ok(written) if written == packet.len() => Ok(()),
                        Ok(_) => Err(std::io::Error::new(
                            std::io::ErrorKind::WriteZero,
                            "short TUN write",
                        )),
                        Err(err) => Err(err),
                    };
                    self.mesh.recycle(packet);
                    note_io_result(&mut self.tun_write_failed, result, "tun write");
                }
                Action::PeerUp { peer_id } => {
                    let waiting = self
                        .links
                        .get(&peer_id)
                        .is_some_and(|link| link.deadline.is_some());
                    if waiting && self.mesh.peer_path(peer_id) == Some(PathKind::Turn) {
                        self.activate_turn(peer_id, Instant::now());
                    }
                    let path = self
                        .mesh
                        .peer_path(peer_id)
                        .unwrap_or(PathKind::Direct)
                        .as_str();
                    info!(peer_id, path_kind = path, "peer connected");
                }
                Action::PeerDown { peer_id } => {
                    warn!(peer_id, "peer timed out");
                }
            }
        }
    }

    async fn write_status(&mut self) {
        let mut status = StatusFile::new(std::process::id(), crate::status::now_unix());
        status.peers = self.mesh.peer_status();
        status.turn.enabled = self.turn_enabled;
        status.turn.allocation = AllocationState::Inactive;
        status.turn.last_error = self.turn_last_error.clone();
        let mut refreshed_unix = None;
        for link in self.links.values() {
            let Some(bridge) = &link.bridge else {
                continue;
            };
            if bridge.is_finished() {
                continue;
            }
            status.turn.allocation = AllocationState::Active;
            refreshed_unix = refreshed_unix.max(Some(bridge.refreshed_unix()));
        }
        if status.turn.allocation == AllocationState::Inactive && self.turn_last_error.is_some() {
            status.turn.allocation = AllocationState::Failed;
        }
        status.turn.refreshed_unix = refreshed_unix;

        if let Some(handle) = self.status_write.take() {
            if !handle.is_finished() {
                // 上一次写入仍未完成，跳过本次，避免阻塞主循环或堆积任务。
                self.status_write = Some(handle);
                return;
            }
            self.finish_status_write(handle.await);
        }
        let writer = self.status.clone();
        self.status_write = Some(tokio::task::spawn_blocking(move || writer.write(&status)));
    }

    fn finish_status_write(
        &mut self,
        result: Result<Result<(), NodeError>, tokio::task::JoinError>,
    ) {
        match result {
            Ok(Ok(())) => {
                if self.status_write_failed {
                    info!(path = %self.status.path().display(), "status file writable again");
                    self.status_write_failed = false;
                }
            }
            Ok(Err(err)) => {
                if self.status_write_failed {
                    tracing::debug!(error = %err, "status write still failing");
                } else {
                    warn!(
                        error = %err,
                        path = %self.status.path().display(),
                        "cannot write status file"
                    );
                    self.status_write_failed = true;
                }
            }
            Err(err) => {
                if !self.status_write_failed {
                    warn!(error = %err, "status writer task failed");
                    self.status_write_failed = true;
                }
            }
        }
    }
}

/// 数据路径的 I/O 错误只在状态变化时记录。
fn note_io_result(failed: &mut bool, result: std::io::Result<()>, action: &'static str) {
    match result {
        Ok(()) => {
            if *failed {
                info!(action, "I/O recovered");
                *failed = false;
            }
        }
        Err(err) => {
            if *failed {
                debug!(action, error = %err, "I/O still failing");
            } else {
                warn!(action, error = %err, "I/O failed");
                *failed = true;
            }
        }
    }
}
