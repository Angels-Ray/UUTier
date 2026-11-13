//! 通过 UU 设备列表发现 peer，并用 UU 信令建立 TURN 路径。
//!
//! 只有别名以 `uutier-` 开头的本账号桌面设备参与组网。设备按 ID 排序：
//! 两台时 ID 较大的一方创建房间，较小的一方加入；三台时每台给下一台创建
//! 房间，并加入上一台的房间。一个房间同时只接受一个 viewer。

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tracing::{info, warn};
use uutier_core::{TurnConfig, TurnMode, UuCredentials, UutierConfig};
use uutier_turn::{
    HostEvent, HostOffer, HostRoom, MAX_UUTIER_DEVICES, PeerRecord, RECORD_VERSION, SignalRecord,
    SignalSession, TurnAllocation, TurnBridge, TurnError, UuApi, UuDeviceList, ViewerControl,
    start_bridge_with_allocation, uutier_devices,
};

use crate::NodeError;
use crate::mesh::{Identity, MeshParams};

/// 直连探测时间，单位 s。超时后 direct-first 模式改用 TURN。
pub const DIRECT_PROBE_S: u64 = 5;

const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(30);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(10);

/// 发给 supervisor 的事件。
pub enum UuEvent {
    /// 已收到对端记录，可以开始直连握手。
    Discovered { record: PeerRecord },
    /// 当前带标记的设备集合。与上一次相同时不发送。
    Devices { device_ids: Vec<String> },
    /// TURN bridge 已就绪。
    Bridge { peer_id: u64, bridge: TurnBridge },
    /// 链路在给出 TURN bridge 之前失败。`peer_id` 未知时为 `None`。
    Failed { peer_id: Option<u64>, error: String },
    /// 信令连接已关闭。bridge 是否继续由上层决定。
    Ended { peer_id: u64, error: Option<String> },
}

/// UU 链路任务集合。
pub(crate) struct UuRuntime {
    pub events: mpsc::Receiver<UuEvent>,
    pub tasks: Vec<JoinHandle<()>>,
    pub mode: TurnMode,
}

/// 发现循环使用的固定参数。
struct RuntimeRequest {
    local: PeerRecord,
    turn: TurnConfig,
    local_peer_addr: SocketAddr,
    network_id: String,
    local_ipv4: Ipv4Addr,
    local_device_id: String,
    /// 登录过期后置位，发现循环停止重启信令任务。
    expired: Arc<AtomicBool>,
}

/// 一个 host 房间任务。
struct HostRequest {
    /// 只接受这个设备的 offer。
    expected_device_id: String,
    local: PeerRecord,
    turn: TurnConfig,
    local_peer_addr: SocketAddr,
    network_id: String,
    local_ipv4: Ipv4Addr,
    expired: Arc<AtomicBool>,
}

/// 一个 viewer 链路任务。
struct ViewerRequest {
    target_device_id: String,
    local: PeerRecord,
    turn: TurnConfig,
    local_peer_addr: SocketAddr,
    network_id: String,
    local_ipv4: Ipv4Addr,
    expired: Arc<AtomicBool>,
}

struct ManagedTask {
    target: String,
    handle: AbortHandle,
}

/// 当前设备集合下本机要建立的两条信令关系。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Plan {
    /// 本机为谁创建 host 房间。
    host: Option<String>,
    /// 本机加入谁的 host 房间。
    viewer: Option<String>,
}

impl UuRuntime {
    /// 读取凭据和设备列表，校验本机标记，然后启动发现循环。
    pub(crate) async fn start(
        config: &UutierConfig,
        identity: &Identity,
        params: &MeshParams,
        listen_port: u16,
    ) -> Result<Self, NodeError> {
        let credentials = load_credentials(config)?;
        let local_device_id = credentials.device_id.clone();
        let api = Arc::new(UuApi::new(credentials)?);
        let list = api.device_list().await?;
        let local = list
            .local(&local_device_id)
            .ok_or(NodeError::UuDeviceMissing)?;
        if !local.is_uutier() {
            return Err(NodeError::UuDeviceUnmarked);
        }
        let devices = uutier_devices(&list);
        if !devices
            .iter()
            .any(|device| device.device_id == local_device_id)
        {
            return Err(NodeError::UuDeviceUnusable);
        }
        if devices.len() > MAX_UUTIER_DEVICES {
            return Err(NodeError::TooManyUuDevices {
                count: devices.len(),
                max: MAX_UUTIER_DEVICES,
            });
        }

        let network_id = params.network_id();
        // always 模式不公布直连候选，对端不会尝试直连。
        let endpoints = if config.turn.mode == TurnMode::Always {
            Vec::new()
        } else {
            local_endpoints(params.ipv4, listen_port)
        };
        let local_record = PeerRecord {
            version: RECORD_VERSION,
            uu_device_id: local_device_id.clone(),
            network_id: network_id.clone(),
            public_key: identity.public_key_hex(),
            ipv4: params.ipv4.to_string(),
            listen_port,
            endpoints,
            relay_addr: None,
        };
        let request = RuntimeRequest {
            local: local_record,
            turn: config.turn.clone(),
            local_peer_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, listen_port)),
            network_id,
            local_ipv4: params.ipv4,
            local_device_id,
            expired: Arc::new(AtomicBool::new(false)),
        };
        let (sender, events) = mpsc::channel(32);
        let task = tokio::spawn(run_discovery(Arc::clone(&api), request, sender));
        Ok(Self {
            events,
            tasks: vec![task],
            mode: config.turn.mode,
        })
    }
}

impl Drop for UuRuntime {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// 计划本机在给定设备集合中的角色。
fn plan(list: &UuDeviceList, local_device_id: &str) -> Result<Plan, String> {
    let local = list
        .local(local_device_id)
        .ok_or_else(|| "local UU device is not in the device list".to_owned())?;
    if !local.is_uutier() {
        return Err(format!(
            "local UU device alias must start with {}",
            uutier_turn::DEVICE_ALIAS_PREFIX
        ));
    }
    let devices = uutier_devices(list);
    if devices.len() > MAX_UUTIER_DEVICES {
        return Err(format!(
            "{} UUTier devices, at most {} supported",
            devices.len(),
            MAX_UUTIER_DEVICES
        ));
    }
    let index = devices
        .iter()
        .position(|device| device.device_id == local_device_id)
        .ok_or_else(|| "local UU device is not a marked desktop device".to_owned())?;
    let plan = match devices.len() {
        0 | 1 => Plan {
            host: None,
            viewer: None,
        },
        2 => {
            if index == 0 {
                Plan {
                    host: None,
                    viewer: Some(devices[1].device_id.clone()),
                }
            } else {
                Plan {
                    host: Some(devices[0].device_id.clone()),
                    viewer: None,
                }
            }
        }
        3 => Plan {
            host: Some(devices[(index + 1) % 3].device_id.clone()),
            viewer: Some(devices[(index + 2) % 3].device_id.clone()),
        },
        _ => {
            return Err(format!(
                "unsupported UUTier device count: {}",
                devices.len()
            ));
        }
    };
    Ok(plan)
}

async fn run_discovery(api: Arc<UuApi>, request: RuntimeRequest, events: mpsc::Sender<UuEvent>) {
    let mut tasks = JoinSet::new();
    let mut host: Option<ManagedTask> = None;
    let mut viewer: Option<ManagedTask> = None;
    let mut problem: Option<String> = None;
    let mut list_error: Option<String> = None;
    let mut last_devices: Option<Vec<String>> = None;

    loop {
        if request.expired.load(Ordering::Relaxed) {
            abort_task(&mut host);
            abort_task(&mut viewer);
            return;
        }
        match api.device_list().await {
            Ok(list) => {
                if list_error.take().is_some() {
                    info!("UU device list readable again");
                }
                let device_ids: Vec<String> = uutier_devices(&list)
                    .iter()
                    .map(|device| device.device_id.clone())
                    .collect();
                if last_devices.as_ref() != Some(&device_ids) {
                    if events
                        .send(UuEvent::Devices {
                            device_ids: device_ids.clone(),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    last_devices = Some(device_ids);
                }
                match plan(&list, &request.local_device_id) {
                    Ok(plan) => {
                        if problem.take().is_some() {
                            info!("UU discovery active");
                        }
                        update_host(&mut tasks, &mut host, plan.host, &api, &request, &events);
                        update_viewer(
                            &mut tasks,
                            &mut viewer,
                            plan.viewer,
                            &api,
                            &request,
                            &events,
                        );
                    }
                    Err(error) => {
                        abort_task(&mut host);
                        abort_task(&mut viewer);
                        if problem.as_deref() != Some(error.as_str()) {
                            warn!(error = %error, "UU discovery stopped");
                            let _ = events
                                .send(UuEvent::Failed {
                                    peer_id: None,
                                    error: error.clone(),
                                })
                                .await;
                            problem = Some(error);
                        }
                    }
                }
            }
            Err(error) => {
                let text = error.to_string();
                if list_error.as_deref() != Some(text.as_str()) {
                    warn!(error = %text, "cannot read the UU device list, keeping current links");
                    list_error = Some(text);
                }
            }
        }
        while tasks.try_join_next().is_some() {}
        clear_finished(&mut host);
        clear_finished(&mut viewer);
        if events.is_closed() {
            return;
        }
        tokio::time::sleep(DISCOVERY_INTERVAL).await;
    }
}

fn update_host(
    tasks: &mut JoinSet<()>,
    slot: &mut Option<ManagedTask>,
    target: Option<String>,
    api: &Arc<UuApi>,
    request: &RuntimeRequest,
    events: &mpsc::Sender<UuEvent>,
) {
    match (
        slot.as_ref().map(|task| task.target.as_str()),
        target.as_deref(),
    ) {
        (Some(current), Some(next)) if current == next => {}
        (_, None) => abort_task(slot),
        (_, Some(next)) => {
            abort_task(slot);
            let handle = tasks.spawn(run_host(
                Arc::clone(api),
                HostRequest {
                    expected_device_id: next.to_owned(),
                    local: request.local.clone(),
                    turn: request.turn.clone(),
                    local_peer_addr: request.local_peer_addr,
                    network_id: request.network_id.clone(),
                    local_ipv4: request.local_ipv4,
                    expired: Arc::clone(&request.expired),
                },
                events.clone(),
            ));
            *slot = Some(ManagedTask {
                target: next.to_owned(),
                handle,
            });
        }
    }
}

fn update_viewer(
    tasks: &mut JoinSet<()>,
    slot: &mut Option<ManagedTask>,
    target: Option<String>,
    api: &Arc<UuApi>,
    request: &RuntimeRequest,
    events: &mpsc::Sender<UuEvent>,
) {
    match (
        slot.as_ref().map(|task| task.target.as_str()),
        target.as_deref(),
    ) {
        (Some(current), Some(next)) if current == next => {}
        (_, None) => abort_task(slot),
        (_, Some(next)) => {
            abort_task(slot);
            let handle = tasks.spawn(run_viewer(
                Arc::clone(api),
                ViewerRequest {
                    target_device_id: next.to_owned(),
                    local: request.local.clone(),
                    turn: request.turn.clone(),
                    local_peer_addr: request.local_peer_addr,
                    network_id: request.network_id.clone(),
                    local_ipv4: request.local_ipv4,
                    expired: Arc::clone(&request.expired),
                },
                events.clone(),
            ));
            *slot = Some(ManagedTask {
                target: next.to_owned(),
                handle,
            });
        }
    }
}

fn abort_task(slot: &mut Option<ManagedTask>) {
    if let Some(task) = slot.take() {
        task.handle.abort();
    }
}

/// 任务自行结束时清空槽位，让下一轮发现重新启动它。
fn clear_finished(slot: &mut Option<ManagedTask>) {
    if slot.as_ref().is_some_and(|task| task.handle.is_finished()) {
        *slot = None;
    }
}

async fn run_host(api: Arc<UuApi>, request: HostRequest, events: mpsc::Sender<UuEvent>) {
    let mut delay = RETRY_MIN;
    let mut reported = false;
    loop {
        let started = tokio::time::Instant::now();
        match host_room(&api, &request, &events).await {
            Ok(()) => return,
            Err(TurnError::LoginExpired) => {
                warn!("UU login expired, host room stopped");
                request.expired.store(true, Ordering::Relaxed);
                let _ = events
                    .send(UuEvent::Failed {
                        peer_id: None,
                        error: "UU login expired".to_owned(),
                    })
                    .await;
                return;
            }
            Err(error) => {
                // 房间运行超过最长重试间隔后再断开，说明这次失败是新故障，
                // 下一次重试从最短间隔开始。
                if started.elapsed() >= RETRY_MAX {
                    delay = RETRY_MIN;
                    reported = false;
                }
                if reported {
                    tracing::debug!(error = %error, "UU host room still stopped");
                } else {
                    warn!(
                        error = %error,
                        "UU host room stopped"
                    );
                    reported = true;
                }
                if events
                    .send(UuEvent::Failed {
                        peer_id: None,
                        error: error.to_string(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(RETRY_MAX);
            }
        }
    }
}

async fn host_room(
    api: &UuApi,
    request: &HostRequest,
    events: &mpsc::Sender<UuEvent>,
) -> Result<(), TurnError> {
    let mut room = HostRoom::create(api).await?;
    info!("UU host room created");
    let mut viewers: HashMap<String, ViewerControl> = HashMap::new();
    loop {
        match room.next_event().await? {
            HostEvent::Control(viewer) => {
                // 一个 host 房间同时只接受一个 viewer，旧 control 不再使用。
                viewers.clear();
                viewers.insert(viewer.client_id().to_owned(), viewer);
            }
            HostEvent::Offer(offer) => {
                if offer.record.uu_device_id != request.expected_device_id {
                    warn!("offer from an unexpected UU device");
                    continue;
                }
                if let Err(error) = validate_common(
                    &offer.record,
                    &request.network_id,
                    request.local_ipv4,
                    &request.local.uu_device_id,
                ) {
                    warn!(error = %error, "peer record rejected");
                    continue;
                }
                let Some(viewer) = find_viewer(&viewers, &offer) else {
                    warn!("offer has no matching control session");
                    continue;
                };
                let peer_id = peer_id_of(&offer.record)?;
                let remote_relay = offer.record.relay_addr();
                // 对端没有 relay 地址时只走直连，本端分配没有用处。
                let mut failure = None;
                let allocation = if remote_relay.is_none() {
                    Ok(None)
                } else {
                    allocate_turn(viewer, &request.turn).await
                };
                let allocation = match allocation {
                    Ok(allocation) => allocation,
                    Err(error) => {
                        failure = Some(error.to_string());
                        None
                    }
                };
                let mut local = request.local.clone();
                local.relay_addr = allocation
                    .as_ref()
                    .map(|allocation| allocation.relay_addr().to_string());
                let mut bridge = None;
                if remote_relay.is_some() {
                    match start_bridge(peer_id, allocation, remote_relay, request.local_peer_addr)
                        .await
                    {
                        Ok(started) => bridge = started,
                        Err(error) => {
                            failure = Some(error.to_string());
                            local.relay_addr = None;
                        }
                    }
                }
                room.send_answer(viewer, offer.control_id.as_deref().unwrap_or(""), &local)
                    .await?;

                if events
                    .send(UuEvent::Discovered {
                        record: offer.record.clone(),
                    })
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                if let Some(error) = failure
                    && events
                        .send(UuEvent::Failed {
                            peer_id: Some(peer_id),
                            error,
                        })
                        .await
                        .is_err()
                {
                    return Ok(());
                }
                if let Some(bridge) = bridge
                    && events
                        .send(UuEvent::Bridge { peer_id, bridge })
                        .await
                        .is_err()
                {
                    return Ok(());
                }
            }
        }
    }
}

async fn run_viewer(api: Arc<UuApi>, request: ViewerRequest, events: mpsc::Sender<UuEvent>) {
    let mut delay = RETRY_MIN;
    let mut reported = false;
    loop {
        match viewer_session(&api, &request, &events).await {
            Ok(()) => {
                delay = RETRY_MIN;
                reported = false;
            }
            Err(TurnError::LoginExpired) => {
                warn!("UU login expired, viewer link stopped");
                request.expired.store(true, Ordering::Relaxed);
                let _ = events
                    .send(UuEvent::Failed {
                        peer_id: None,
                        error: "UU login expired".to_owned(),
                    })
                    .await;
                return;
            }
            Err(error) => {
                if reported {
                    tracing::debug!(error = %error, "UU viewer link still stopped");
                } else {
                    warn!(
                        error = %error,
                        "UU viewer link stopped"
                    );
                    reported = true;
                }
                if events
                    .send(UuEvent::Failed {
                        peer_id: None,
                        error: error.to_string(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        if events.is_closed() {
            return;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RETRY_MAX);
    }
}

async fn viewer_session(
    api: &UuApi,
    request: &ViewerRequest,
    events: &mpsc::Sender<UuEvent>,
) -> Result<(), TurnError> {
    let (mut session, remote, allocation) = attempt(api, request).await?;
    let peer_id = peer_id_of(&remote.record)?;
    tracing::debug!(
        peer_id,
        endpoints = remote.record.endpoints().len(),
        "peer record exchanged"
    );
    if events
        .send(UuEvent::Discovered {
            record: remote.record.clone(),
        })
        .await
        .is_err()
    {
        return Ok(());
    }

    // 对端没有 relay 地址时 TURN 无法双向工作，只走直连。
    let allocation = if remote.record.relay_addr().is_some() {
        allocation
    } else {
        None
    };
    let bridge = match start_bridge(
        peer_id,
        allocation,
        remote.record.relay_addr(),
        request.local_peer_addr,
    )
    .await
    {
        Ok(bridge) => bridge,
        Err(error) => {
            session.close().await;
            return Err(error);
        }
    };
    if let Some(bridge) = bridge
        && events
            .send(UuEvent::Bridge { peer_id, bridge })
            .await
            .is_err()
    {
        return Ok(());
    }

    let error = session.pump().await.err().map(|error| error.to_string());
    if let Some(error) = &error {
        warn!(peer_id, error = %error, "UU signaling closed");
    }
    let _ = events.send(UuEvent::Ended { peer_id, error }).await;
    Ok(())
}

async fn attempt(
    api: &UuApi,
    request: &ViewerRequest,
) -> Result<(SignalSession, SignalRecord, Option<TurnAllocation>), TurnError> {
    let mut session = SignalSession::viewer(api, request.target_device_id.as_str()).await?;
    let allocation = match allocate_viewer_allocation(&session, request).await {
        Ok(allocation) => allocation,
        Err(error) if request.turn.mode == TurnMode::Always => return Err(error),
        Err(error) => {
            // direct-first 模式下 TURN 失败不应挡住直连，记录里不带 relay 地址即可。
            warn!(error = %error, "cannot allocate TURN relay, continuing without it");
            None
        }
    };
    let mut local = request.local.clone();
    local.relay_addr = allocation
        .as_ref()
        .map(|allocation| allocation.relay_addr().to_string());

    session.send_offer(&local).await?;
    let answer = session.next_record("answer").await?;
    if answer.record.uu_device_id != request.target_device_id {
        return Err(TurnError::InvalidRecord("UU device id mismatch"));
    }
    validate_common(
        &answer.record,
        &request.network_id,
        request.local_ipv4,
        &request.local.uu_device_id,
    )?;
    Ok((session, answer, allocation))
}

async fn allocate_viewer_allocation(
    session: &SignalSession,
    request: &ViewerRequest,
) -> Result<Option<TurnAllocation>, TurnError> {
    if !request.turn.enabled {
        return Ok(None);
    }
    let server = session.turn_server(request.turn.request_timeout_s)?;
    TurnAllocation::allocate(&server).await.map(Some)
}

async fn allocate_turn(
    viewer: &ViewerControl,
    turn: &TurnConfig,
) -> Result<Option<TurnAllocation>, TurnError> {
    if !turn.enabled {
        return Ok(None);
    }
    let server = viewer.turn_server(turn.request_timeout_s)?;
    TurnAllocation::allocate(&server).await.map(Some)
}

async fn start_bridge(
    peer_id: u64,
    allocation: Option<TurnAllocation>,
    remote_relay: Option<SocketAddr>,
    local_peer_addr: SocketAddr,
) -> Result<Option<TurnBridge>, TurnError> {
    let Some(allocation) = allocation else {
        return Ok(None);
    };
    let Some(remote_relay) = remote_relay else {
        return Err(TurnError::InvalidRecord("peer record has no relay address"));
    };
    let bridge = start_bridge_with_allocation(allocation, remote_relay, local_peer_addr).await?;
    info!(peer_id, "TURN bridge ready");
    Ok(Some(bridge))
}

/// 检查记录是否属于同一网络，地址是否在本机虚拟网段内。
fn validate_common(
    record: &PeerRecord,
    network_id: &str,
    local_ipv4: Ipv4Addr,
    local_device_id: &str,
) -> Result<(), TurnError> {
    record.validate()?;
    if !record.network_id.eq_ignore_ascii_case(network_id) {
        return Err(TurnError::InvalidRecord("network id mismatch"));
    }
    if record.uu_device_id == local_device_id {
        return Err(TurnError::InvalidRecord("record uses the local UU device"));
    }
    let ipv4 = record
        .ipv4
        .parse::<Ipv4Addr>()
        .map_err(|_| TurnError::InvalidRecord("invalid IPv4 address"))?;
    if ipv4 == local_ipv4 || ipv4.octets()[..3] != local_ipv4.octets()[..3] {
        return Err(TurnError::InvalidRecord(
            "peer IPv4 is outside the local /24",
        ));
    }
    if matches!(ipv4.octets()[3], 0 | 255) {
        return Err(TurnError::InvalidRecord(
            "peer IPv4 is not a usable host address",
        ));
    }
    if let Some(relay) = record.relay_addr() {
        let SocketAddr::V4(relay) = relay else {
            return Err(TurnError::InvalidRecord("relay address is not IPv4"));
        };
        if !is_public_relay(*relay.ip()) {
            return Err(TurnError::InvalidRecord(
                "relay address is not a public IPv4 address",
            ));
        }
    }
    Ok(())
}

/// UU 云 TURN 的 relay 地址应当是公网单播 IPv4。
fn is_public_relay(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    crate::mesh::is_usable_endpoint(address)
        && !address.is_private()
        // 100.64.0.0/10 是运营商级 NAT 地址。
        && !(octets[0] == 100 && (octets[1] & 0xc0) == 64)
        && octets[0] != 0
        && octets[0] < 240
        && !(octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        && !(octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
}

fn peer_id_of(record: &PeerRecord) -> Result<u64, TurnError> {
    let public_key = uutier_core::parse_public_key(&record.public_key)
        .map_err(|_| TurnError::InvalidRecord("invalid public key"))?;
    Ok(crate::mesh::identity::peer_id(&public_key))
}

fn find_viewer<'a>(
    viewers: &'a HashMap<String, ViewerControl>,
    offer: &HostOffer,
) -> Option<&'a ViewerControl> {
    if let Some(viewer) = viewers.get(&offer.client_id) {
        return Some(viewer);
    }
    if !offer.ice_id.is_empty()
        && let Some(viewer) = viewers
            .values()
            .find(|viewer| viewer.ice_id() == offer.ice_id)
    {
        return Some(viewer);
    }
    if viewers.len() == 1 {
        return viewers.values().next();
    }
    None
}

fn load_credentials(config: &UutierConfig) -> Result<UuCredentials, TurnError> {
    uutier_turn::load(config.uu.path.as_deref())
}

/// 返回可公布的直连候选地址，物理网卡优先。
fn local_endpoints(local_ipv4: Ipv4Addr, listen_port: u16) -> Vec<String> {
    let mut addresses = interface_ipv4s(local_ipv4);
    addresses.sort_by_key(|(name, address)| endpoint_score(name, *address));
    addresses.dedup_by_key(|(_, address)| *address);
    addresses.truncate(uutier_turn::MAX_ENDPOINTS);
    addresses
        .into_iter()
        .map(|(_, address)| SocketAddr::new(address.into(), listen_port).to_string())
        .collect()
}

/// 读取本机接口的 IPv4 地址，去掉回环、链路本地和本机虚拟网段。
fn interface_ipv4s(local_ipv4: Ipv4Addr) -> Vec<(String, Ipv4Addr)> {
    let interfaces = match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces,
        Err(error) => {
            warn!(error = %error, "cannot read network interfaces");
            return Vec::new();
        }
    };
    interfaces
        .into_iter()
        .filter_map(|interface| {
            let std::net::IpAddr::V4(address) = interface.ip() else {
                return None;
            };
            if interface.is_loopback()
                || address.is_link_local()
                || address.is_unspecified()
                || crate::mesh::is_benchmarking(address)
                || address.is_documentation()
                || address.is_multicast()
                || address.is_broadcast()
                || address.octets()[..3] == local_ipv4.octets()[..3]
            {
                return None;
            }
            Some((interface.name, address))
        })
        .collect()
}

/// 物理网卡上的私有地址优先，虚拟网卡上的地址靠后。
fn endpoint_score(name: &str, address: Ipv4Addr) -> u8 {
    let physical = ["en", "eth", "wl"]
        .iter()
        .any(|prefix| name.starts_with(prefix));
    match (physical, address.is_private()) {
        (true, true) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (false, false) => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uutier_turn::UuDevice;

    fn device(id: &str, alias: &str, online: bool) -> UuDevice {
        UuDevice {
            device_id: id.to_owned(),
            alias: alias.to_owned(),
            platform: Some(1),
            online,
        }
    }

    fn list(devices: &[UuDevice], local: &str) -> UuDeviceList {
        UuDeviceList {
            current: devices
                .iter()
                .find(|device| device.device_id == local)
                .cloned(),
            bound: devices
                .iter()
                .filter(|device| device.device_id != local)
                .cloned()
                .collect(),
            shared: Vec::new(),
        }
    }

    #[test]
    fn two_devices_let_the_larger_id_host() {
        let devices = [device("a", "uutier-a", true), device("b", "uutier-b", true)];
        assert_eq!(
            plan(&list(&devices, "a"), "a").unwrap(),
            Plan {
                host: None,
                viewer: Some("b".to_owned())
            }
        );
        assert_eq!(
            plan(&list(&devices, "b"), "b").unwrap(),
            Plan {
                host: Some("a".to_owned()),
                viewer: None
            }
        );
    }

    #[test]
    fn three_devices_form_a_ring() {
        let devices = [
            device("a", "uutier-a", true),
            device("b", "uutier-b", true),
            device("c", "uutier-c", true),
        ];
        let expected = [("a", "b", "c"), ("b", "c", "a"), ("c", "a", "b")];
        for (local, host, viewer) in expected {
            assert_eq!(
                plan(&list(&devices, local), local).unwrap(),
                Plan {
                    host: Some(host.to_owned()),
                    viewer: Some(viewer.to_owned())
                }
            );
        }
    }

    #[test]
    fn discovery_ignores_unmarked_devices_and_rejects_oversized_sets() {
        let devices = [device("a", "uutier-a", true), device("b", "desktop", true)];
        assert_eq!(
            plan(&list(&devices, "a"), "a").unwrap(),
            Plan {
                host: None,
                viewer: None
            }
        );

        let devices = [device("a", "desktop", true), device("b", "uutier-b", true)];
        assert!(plan(&list(&devices, "a"), "a").is_err());

        let devices = [
            device("a", "uutier-a", true),
            device("b", "uutier-b", true),
            device("c", "uutier-c", true),
            device("d", "uutier-d", true),
        ];
        assert!(plan(&list(&devices, "a"), "a").is_err());
    }

    #[test]
    fn offline_peers_keep_their_place_in_the_ring() {
        let devices = [
            device("a", "uutier-a", true),
            device("b", "uutier-b", true),
            device("c", "uutier-c", false),
        ];
        let plan = plan(&list(&devices, "a"), "a").unwrap();
        assert_eq!(
            plan,
            Plan {
                host: Some("b".to_owned()),
                viewer: Some("c".to_owned())
            }
        );
    }

    #[test]
    fn record_must_match_network_and_subnet() {
        let record = PeerRecord {
            version: RECORD_VERSION,
            uu_device_id: "b".to_owned(),
            network_id: "11".repeat(32),
            public_key: "00".repeat(32),
            ipv4: "10.144.0.3".to_owned(),
            listen_port: 12010,
            endpoints: Vec::new(),
            relay_addr: None,
        };
        let network_id = "11".repeat(32);
        let local = Ipv4Addr::new(10, 144, 0, 2);
        assert!(validate_common(&record, &network_id, local, "a").is_ok());
        assert!(validate_common(&record, &"22".repeat(32), local, "a").is_err());

        let mut other = record.clone();
        other.ipv4 = "10.145.0.3".to_owned();
        assert!(validate_common(&other, &network_id, local, "a").is_err());

        let mut own = record;
        own.uu_device_id = "a".to_owned();
        assert!(validate_common(&own, &network_id, local, "a").is_err());
    }

    #[test]
    fn record_rejects_unusable_addresses() {
        let record = PeerRecord {
            version: RECORD_VERSION,
            uu_device_id: "b".to_owned(),
            network_id: "11".repeat(32),
            public_key: "00".repeat(32),
            ipv4: "10.144.0.3".to_owned(),
            listen_port: 12010,
            endpoints: Vec::new(),
            relay_addr: None,
        };
        let network_id = "11".repeat(32);
        let local = Ipv4Addr::new(10, 144, 0, 2);
        for ipv4 in ["10.144.0.0", "10.144.0.255"] {
            let mut value = record.clone();
            value.ipv4 = ipv4.to_owned();
            assert!(validate_common(&value, &network_id, local, "a").is_err());
        }
        for relay in ["8.8.8.8:3478", "1.1.1.1:3478"] {
            let mut value = record.clone();
            value.relay_addr = Some(relay.to_owned());
            assert!(validate_common(&value, &network_id, local, "a").is_ok());
        }
        for relay in [
            "10.0.0.1:3478",
            "127.0.0.1:3478",
            "100.64.0.1:3478",
            "169.254.1.1:3478",
            "192.0.2.1:3478",
            "0.1.2.3:3478",
            "240.0.0.1:3478",
            "192.0.0.1:3478",
            "192.88.99.1:3478",
            "[2001:db8::1]:3478",
        ] {
            let mut value = record.clone();
            value.relay_addr = Some(relay.to_owned());
            assert!(
                validate_common(&value, &network_id, local, "a").is_err(),
                "{relay}"
            );
        }
    }
}
