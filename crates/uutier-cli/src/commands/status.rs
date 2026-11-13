//! `uutier status` 子命令。

use uutier_node::{NodeError, default_status_path, now_unix, read_status};

use super::CommandError;

/// 输出节点状态。
pub fn run(json: bool) -> Result<(), CommandError> {
    let path = default_status_path().ok_or_else(|| {
        CommandError::config(
            "cannot resolve status path: set HOME or XDG_STATE_HOME to an absolute path",
        )
    })?;
    let status = match read_status(&path) {
        Ok(status) => status,
        Err(NodeError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CommandError::runtime(format!(
                "node is not running: no status file at {}",
                path.display()
            )));
        }
        Err(err) => {
            return Err(CommandError::runtime(format!(
                "cannot read {}: {err}",
                path.display()
            )));
        }
    };

    let now = now_unix();
    if json {
        let text = serde_json::to_string_pretty(&status)
            .map_err(|err| CommandError::runtime(err.to_string()))?;
        outln!("{text}").map_err(CommandError::output)?;
    } else {
        print_human(&status, now)?;
    }

    if status.is_stale(now) {
        let reason = if status.updated_unix > now {
            format!(
                "its timestamp {} is later than the current time",
                status.updated_unix
            )
        } else {
            format!("it is stale by {} s", status.age_s(now))
        };
        return Err(CommandError::runtime(format!(
            "status file {} is not current: {reason}; the node may not be running",
            path.display()
        )));
    }
    Ok(())
}

fn print_human(status: &uutier_core::StatusFile, now: u64) -> Result<(), CommandError> {
    outln!("pid: {}", status.pid).map_err(CommandError::output)?;
    if status.updated_unix > now {
        outln!(
            "updated: unix {} (timestamp in the future)",
            status.updated_unix
        )
        .map_err(CommandError::output)?;
    } else {
        outln!("updated: {} s ago", status.age_s(now)).map_err(CommandError::output)?;
    }
    outln!(
        "turn: enabled={} allocation={}",
        status.turn.enabled,
        status.turn.allocation.as_str()
    )
    .map_err(CommandError::output)?;
    if let Some(refreshed) = status.turn.refreshed_unix {
        outln!("turn refreshed_unix: {refreshed}").map_err(CommandError::output)?;
    }
    if let Some(error) = &status.turn.last_error {
        outln!("turn error: {error}").map_err(CommandError::output)?;
    }
    for peer in &status.peers {
        outln!(
            "peer {} ipv4={} path={} mesh_rtt_us={} turn_rtt_us={} rx_bytes={} tx_bytes={}",
            peer.peer_id,
            peer.ipv4.as_deref().unwrap_or("-"),
            peer.path.as_str(),
            optional(peer.mesh_rtt_us),
            optional(peer.turn_rtt_us),
            peer.rx_bytes,
            peer.tx_bytes,
        )
        .map_err(CommandError::output)?;
    }
    Ok(())
}

fn optional(value: Option<u32>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}
