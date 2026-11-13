//! `uutier config` 子命令。

use std::io::Write as _;
use std::path::{Path, PathBuf};

use uutier_core::UutierConfig;

use super::CommandError;

/// 读取并校验配置。
pub fn load(override_path: Option<&Path>) -> Result<(PathBuf, UutierConfig), CommandError> {
    let path = resolve_path(override_path)?;
    let metadata = std::fs::metadata(&path).map_err(|err| cannot_read(&path, err))?;
    check_regular_file(&path, &metadata)?;
    let text = std::fs::read_to_string(&path).map_err(|err| cannot_read(&path, err))?;
    let config = UutierConfig::parse(&text)
        .map_err(|err| CommandError::config(format!("{}: {err}", path.display())))?;
    config
        .validate()
        .map_err(|err| CommandError::config(format!("{}: {err}", path.display())))?;
    Ok((path, config))
}

/// 读取 `[uu] path`，不校验其他配置段。`login` 和 `logout` 只需要凭据路径。
fn load_uu_path(override_path: Option<&Path>) -> Result<Option<PathBuf>, CommandError> {
    let explicit = override_path.is_some();
    let path = resolve_path(override_path)?;
    let text = match std::fs::metadata(&path) {
        Ok(metadata) => {
            check_regular_file(&path, &metadata)?;
            std::fs::read_to_string(&path).map_err(|err| cannot_read(&path, err))?
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !explicit => return Ok(None),
        Err(err) => return Err(cannot_read(&path, err)),
    };
    let config = UutierConfig::parse(&text)
        .map_err(|err| CommandError::config(format!("{}: {err}", path.display())))?;
    if let Some(configured) = &config.uu.path
        && !configured.is_absolute()
    {
        return Err(CommandError::config(format!(
            "{}: uu.path {} is not an absolute path",
            path.display(),
            configured.display()
        )));
    }
    Ok(config.uu.path)
}

/// 返回凭据文件路径。`uu.path` 优先，否则使用默认路径。
pub fn credentials_path(override_path: Option<&Path>) -> Result<PathBuf, CommandError> {
    let configured = load_uu_path(override_path)?;
    configured
        .or_else(uutier_turn::default_credentials_path)
        .ok_or_else(|| {
            CommandError::config(
                "cannot resolve credentials path: set HOME or XDG_CONFIG_HOME to an absolute path",
            )
        })
}

/// 返回配置文件路径。
pub fn resolve_path(override_path: Option<&Path>) -> Result<PathBuf, CommandError> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    uutier_core::config_path(xdg.as_deref(), home.as_deref()).ok_or_else(|| {
        CommandError::config(
            "cannot resolve config path: set HOME or XDG_CONFIG_HOME to an absolute path",
        )
    })
}

fn cannot_read(path: &Path, err: std::io::Error) -> CommandError {
    CommandError::config(format!("cannot read {}: {err}", path.display()))
}

/// 配置文件必须是普通文件，避免把目录或 FIFO 当配置读取。
fn check_regular_file(path: &Path, metadata: &std::fs::Metadata) -> Result<(), CommandError> {
    if !metadata.is_file() {
        return Err(CommandError::config(format!(
            "cannot read {}: not a regular file",
            path.display()
        )));
    }
    warn_if_readable_by_others(path, metadata);
    Ok(())
}

/// 配置文件包含 `network.secret`，组或其他用户可读时直接写 stderr。
/// 这条警告不受 `--log-level` 和 `RUST_LOG` 过滤。
fn warn_if_readable_by_others(path: &Path, metadata: &std::fs::Metadata) {
    use std::os::unix::fs::PermissionsExt as _;

    if metadata.permissions().mode() & 0o077 != 0 {
        let _ = writeln!(
            std::io::stderr().lock(),
            "uutier: warning: config file is readable by other users, run chmod 600: {}",
            path.display()
        );
    }
}

/// 校验配置。
pub fn check(override_path: Option<&Path>) -> Result<(), CommandError> {
    let (path, _) = load(override_path)?;
    outln!("ok: {}", path.display()).map_err(CommandError::output)?;
    Ok(())
}

/// 输出配置，秘密字段被替换。
pub fn show(override_path: Option<&Path>) -> Result<(), CommandError> {
    let (_, config) = load(override_path)?;
    let text = config
        .to_redacted_toml()
        .map_err(|err| CommandError::runtime(err.to_string()))?;
    out!("{text}").map_err(CommandError::output)?;
    Ok(())
}
