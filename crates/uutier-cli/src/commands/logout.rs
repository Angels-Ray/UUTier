//! `uutier logout` 子命令。

use std::path::Path;

use uutier_turn::remove;

use super::CommandError;
use super::config;

/// 删除 UUTier 的凭据文件。
pub fn run(override_path: Option<&Path>) -> Result<(), CommandError> {
    let path = config::credentials_path(override_path)?;
    if remove(&path).map_err(|err| CommandError::runtime(err.to_string()))? {
        outln!("removed: {}", path.display()).map_err(CommandError::output)?;
    } else {
        outln!("not found: {}", path.display()).map_err(CommandError::output)?;
    }
    Ok(())
}
