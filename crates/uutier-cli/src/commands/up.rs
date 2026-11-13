//! `uutier up` 子命令。

use std::path::Path;

use tokio::signal::unix::{SignalKind, signal};
use uutier_node::{Node, NodeOptions, default_status_path};

use super::CommandError;
use super::config;

async fn shutdown_signal() -> Result<(), CommandError> {
    let mut terminate = signal(SignalKind::terminate())
        .map_err(|err| CommandError::runtime(format!("cannot listen for SIGTERM: {err}")))?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            result.map_err(|err| CommandError::runtime(format!("cannot wait for Ctrl+C: {err}")))?;
        }
        _ = terminate.recv() => {}
    }
    Ok(())
}

/// 前台运行节点。
pub async fn run(override_path: Option<&Path>) -> Result<(), CommandError> {
    let (config_path, config) = config::load(override_path)?;
    let status_path = default_status_path().ok_or_else(|| {
        CommandError::config(
            "cannot resolve status path: set HOME or XDG_STATE_HOME to an absolute path",
        )
    })?;
    let node = Node::start(NodeOptions {
        config,
        status_path,
    })
    .await
    .map_err(|err| {
        if err.is_configuration() {
            CommandError::config(err.to_string())
        } else {
            CommandError::runtime(err.to_string())
        }
    })?;

    tracing::info!(
        config = %config_path.display(),
        status = %node.status_writer().path().display(),
        "node starting"
    );
    tokio::select! {
        result = node.run() => result.map_err(|err| CommandError::runtime(err.to_string()))?,
        result = shutdown_signal() => {
            result?;
            tracing::info!("stopping");
        }
    }
    Ok(())
}
