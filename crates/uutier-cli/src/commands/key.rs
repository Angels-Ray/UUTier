//! `uutier key` 子命令。

use uutier_node::{Identity, default_identity_path};

use super::CommandError;

/// 输出本机 peer ID 和公钥。
pub fn show() -> Result<(), CommandError> {
    let path = default_identity_path().ok_or_else(|| {
        CommandError::config(
            "cannot resolve identity path: set HOME or XDG_CONFIG_HOME to an absolute path",
        )
    })?;
    let identity =
        Identity::create_or_load(&path).map_err(|err| CommandError::config(err.to_string()))?;
    outln!("identity: {}", path.display()).map_err(CommandError::output)?;
    outln!("peer_id: {}", identity.peer_id()).map_err(CommandError::output)?;
    outln!("public_key: {}", identity.public_key_hex()).map_err(CommandError::output)?;
    Ok(())
}
