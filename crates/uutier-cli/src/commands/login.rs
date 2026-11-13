//! `uutier login` 子命令。

use std::path::Path;

use uutier_turn::{LoginOptions, login};

use super::CommandError;
use super::config;

/// 执行扫码登录。
pub async fn run(override_path: Option<&Path>, new_device: bool) -> Result<(), CommandError> {
    let path = config::credentials_path(override_path)?;
    let options = LoginOptions {
        new_device,
        ..LoginOptions::default()
    };
    login(options, &path).await.map_err(|err| {
        if uutier_node::is_configuration_error(&err) {
            CommandError::config(err.to_string())
        } else {
            CommandError::runtime(err.to_string())
        }
    })?;
    Ok(())
}
