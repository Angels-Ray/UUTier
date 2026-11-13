//! 状态文件的读写。

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use uutier_core::StatusFile;

use crate::NodeError;

const STATUS_DIR_MODE: u32 = 0o700;

/// 返回当前 Unix 时间戳，单位 s。
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// 返回当前用户的状态文件路径。
pub fn default_status_path() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    uutier_core::status_path(xdg.as_deref(), home.as_deref())
}

/// 读取状态文件。
pub fn read(path: &Path) -> Result<StatusFile, NodeError> {
    let text = fs::read_to_string(path)?;
    let status: StatusFile =
        serde_json::from_str(&text).map_err(|source| NodeError::StatusJson {
            path: path.to_path_buf(),
            source,
        })?;
    if status.schema_version != uutier_core::STATUS_SCHEMA_VERSION {
        return Err(NodeError::UnsupportedStatusSchema {
            version: status.schema_version,
            expected: uutier_core::STATUS_SCHEMA_VERSION,
        });
    }
    Ok(status)
}

/// 原子写入状态文件，权限 0600。
pub fn write(path: &Path, status: &StatusFile) -> Result<(), NodeError> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let existed = parent.exists();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(STATUS_DIR_MODE)
        .create(parent)?;
    // 只收紧新建目录和默认布局下的 uutier 目录，避免改动调用方提供的共享目录。
    if (!existed || parent.file_name().is_some_and(|name| name == "uutier"))
        && fs::metadata(parent)?.permissions().mode() & 0o777 != STATUS_DIR_MODE
    {
        fs::set_permissions(parent, fs::Permissions::from_mode(STATUS_DIR_MODE))?;
    }

    let text = serde_json::to_string_pretty(status)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".status-")
        .suffix(".json.tmp")
        .tempfile_in(parent)?;
    tmp.as_file_mut().write_all(text.as_bytes())?;
    // 状态文件每 2 s 重写，掉电后重新生成即可，不调用 fsync。
    tmp.persist(path).map_err(|err| NodeError::Io(err.error))?;
    Ok(())
}

/// 状态文件写入器。
#[derive(Debug, Clone)]
pub struct StatusWriter {
    path: PathBuf,
}

impl StatusWriter {
    /// 创建写入器。
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// 返回状态文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 写入状态。
    pub fn write(&self, status: &StatusFile) -> Result<(), NodeError> {
        write(&self.path, status)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;
    use uutier_core::{PathKind, StatusFile};

    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn writes_and_reads_status() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("uutier").join("status.json");
        let mut status = StatusFile::new(std::process::id(), now_unix());
        status.peers.push(uutier_core::PeerStatus {
            peer_id: 1,
            ipv4: Some("10.144.0.3".to_owned()),
            path: PathKind::Direct,
            mesh_rtt_us: Some(800),
            turn_rtt_us: None,
            rx_bytes: 1,
            tx_bytes: 2,
        });

        write(&path, &status).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(read(&path).unwrap(), status);
    }

    #[test]
    fn tightens_existing_status_directory() {
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("uutier");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        write(&parent.join("status.json"), &StatusFile::new(1, 100)).unwrap();
        assert_eq!(mode(&parent), 0o700);
    }

    #[test]
    fn leaves_caller_directory_mode_unchanged() {
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("state");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        write(&parent.join("status.json"), &StatusFile::new(1, 100)).unwrap();
        assert_eq!(mode(&parent), 0o755);
        assert_eq!(mode(&parent.join("status.json")), 0o600);
    }

    #[test]
    fn overwrites_status_atomically() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        let writer = StatusWriter::new(path.clone());
        writer.write(&StatusFile::new(1, 100)).unwrap();
        writer.write(&StatusFile::new(2, 200)).unwrap();
        let status = read(&path).unwrap();
        assert_eq!(status.pid, 2);
        assert_eq!(status.updated_unix, 200);
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        let mut status = StatusFile::new(1, 100);
        status.schema_version = 2;
        write(&path, &status).unwrap();
        match read(&path) {
            Err(NodeError::UnsupportedStatusSchema {
                version: 2,
                expected: 1,
            }) => {}
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn reports_invalid_status_with_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("status.json");
        fs::write(&path, "not json").unwrap();
        match read(&path) {
            Err(NodeError::StatusJson { path: reported, .. }) => assert_eq!(reported, path),
            other => panic!("unexpected result: {other:?}"),
        }
    }
}
