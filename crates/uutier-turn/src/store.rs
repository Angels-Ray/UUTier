//! UU 凭据的读取、写入和删除。

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use uutier_core::UuCredentials;

use crate::error::TurnError;

const CREDENTIALS_FILE: &str = "uu.json";
const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
/// 凭据文件上限，单位 byte。正常文件约 1 KiB。
const MAX_CREDENTIALS_BYTES: u64 = 64 * 1024;

/// 返回 UUTier 的 `uu.json` 路径。
pub fn uutier_credentials_path(
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let base = xdg_config_home
        .filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| {
            home.filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
                .map(|home| home.join(".config"))
        })?;
    Some(base.join("uutier").join(CREDENTIALS_FILE))
}

/// 返回当前用户的 `uu.json` 路径。
pub fn default_credentials_path() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    uutier_credentials_path(xdg.as_deref(), home.as_deref())
}

/// 从指定文件读取凭据。
pub fn load_from(path: &Path) -> Result<UuCredentials, TurnError> {
    let file = fs::File::open(path).map_err(|source| credentials_io(path, source))?;
    // 文件类型和权限取自已打开的句柄，检查和读取针对同一个文件。
    let metadata = file
        .metadata()
        .map_err(|source| credentials_io(path, source))?;
    if !metadata.is_file() {
        return Err(TurnError::NotAFile(path.to_path_buf()));
    }
    let parent = parent_dir(path);
    let parent_metadata = fs::metadata(parent).map_err(|source| credentials_io(path, source))?;
    if !parent_metadata.is_dir() || parent_metadata.permissions().mode() & 0o077 != 0 {
        return Err(TurnError::InsecureDirectory(parent.to_path_buf()));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(TurnError::InsecurePermissions(path.to_path_buf()));
    }
    if metadata.len() > MAX_CREDENTIALS_BYTES {
        return Err(TurnError::CredentialsTooLarge(path.to_path_buf()));
    }
    let mut text = String::new();
    file.take(MAX_CREDENTIALS_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|source| credentials_io(path, source))?;
    if text.len() as u64 > MAX_CREDENTIALS_BYTES {
        return Err(TurnError::CredentialsTooLarge(path.to_path_buf()));
    }
    let credentials: UuCredentials =
        serde_json::from_str(&text).map_err(|source| TurnError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(credentials)
}

/// 从 `path` 或默认路径读取凭据。
pub fn load(path: Option<&Path>) -> Result<UuCredentials, TurnError> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => default_credentials_path().ok_or(TurnError::MissingPath)?,
    };
    load_from(&path)
}

/// 原子写入凭据文件，权限 0600。
pub fn save(path: &Path, credentials: &UuCredentials) -> Result<(), TurnError> {
    let parent = parent_dir(path);
    create_private_dir(parent).map_err(|source| credentials_io(path, source))?;

    let text = serde_json::to_string_pretty(credentials).map_err(|source| TurnError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".uu-")
        .suffix(".json.tmp")
        .tempfile_in(parent)
        .map_err(|source| credentials_io(path, source))?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(FILE_MODE))
        .map_err(|source| credentials_io(path, source))?;
    tmp.as_file_mut()
        .write_all(text.as_bytes())
        .map_err(|source| credentials_io(path, source))?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|source| credentials_io(path, source))?;
    tmp.persist(path)
        .map_err(|err| credentials_io(path, err.error))?;
    Ok(())
}

/// 删除凭据文件，返回是否真的删除了文件。
pub fn remove(path: &Path) -> Result<bool, TurnError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(credentials_io(path, source)),
    }
}

/// 创建目录并设为 0700。已存在的目录必须没有组或其他用户的权限位。
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    match fs::metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    "credentials parent is not a directory",
                ));
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "credentials directory is accessible by group or others, run chmod 700",
                ));
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(DIRECTORY_MODE))
        }
        Err(source) => Err(source),
    }
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn credentials_io(path: &Path, source: std::io::Error) -> TurnError {
    TurnError::CredentialsIo {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    fn credentials() -> UuCredentials {
        UuCredentials {
            user_id: "user".to_owned(),
            device_id: "device".to_owned(),
            client_id: "client".to_owned(),
            system_id: "system".to_owned(),
            machine_guid: "machine".to_owned(),
            token: "secret-token".to_owned(),
            csrf: "secret-csrf".to_owned(),
            sign_key: String::new(),
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn private_dir(dir: &TempDir) -> PathBuf {
        let path = dir.path().join("uutier");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn saves_and_loads_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("uutier").join("uu.json");
        save(&path, &credentials()).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);

        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded, credentials());
    }

    #[test]
    fn rejects_group_or_other_access() {
        let dir = TempDir::new().unwrap();
        let path = private_dir(&dir).join("uu.json");
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            load_from(&path),
            Err(TurnError::InsecurePermissions(_))
        ));
    }

    #[test]
    fn rejects_directories() {
        let dir = TempDir::new().unwrap();
        assert!(matches!(load_from(dir.path()), Err(TurnError::NotAFile(_))));
    }

    #[test]
    fn rejects_oversized_credentials_files() {
        let dir = TempDir::new().unwrap();
        let path = private_dir(&dir).join("uu.json");
        let size = usize::try_from(MAX_CREDENTIALS_BYTES).unwrap() + 1;
        fs::write(&path, vec![b' '; size]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            load_from(&path),
            Err(TurnError::CredentialsTooLarge(_))
        ));
    }

    #[test]
    fn reports_json_errors_with_path() {
        let dir = TempDir::new().unwrap();
        let path = private_dir(&dir).join("uu.json");
        fs::write(&path, "not json").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        match load_from(&path) {
            Err(TurnError::Json { path: reported, .. }) => assert_eq!(reported, path),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn remove_reports_whether_the_file_existed() {
        let dir = TempDir::new().unwrap();
        let path = private_dir(&dir).join("uu.json");
        assert!(!remove(&path).unwrap());
        save(&path, &credentials()).unwrap();
        assert!(remove(&path).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn rejects_existing_directory_with_group_access() {
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("uutier");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        let path = parent.join("uu.json");
        assert!(save(&path, &credentials()).is_err());
        assert!(!path.exists());

        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        save(&path, &credentials()).unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn load_rejects_insecure_parent_directory() {
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("uutier");
        fs::create_dir(&parent).unwrap();
        let path = parent.join("uu.json");
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            load_from(&path),
            Err(TurnError::InsecureDirectory(_))
        ));
    }

    #[test]
    fn save_overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = private_dir(&dir).join("uu.json");
        save(&path, &credentials()).unwrap();
        let mut updated = credentials();
        updated.user_id = "other".to_owned();
        save(&path, &updated).unwrap();
        assert_eq!(load_from(&path).unwrap().user_id, "other");
    }

    #[test]
    fn resolves_credentials_path() {
        let path = uutier_credentials_path(
            Some(Path::new("/xdg/config")),
            Some(Path::new("/home/user")),
        );
        assert_eq!(path, Some(PathBuf::from("/xdg/config/uutier/uu.json")));
        let path = uutier_credentials_path(None, Some(Path::new("/home/user")));
        assert_eq!(
            path,
            Some(PathBuf::from("/home/user/.config/uutier/uu.json"))
        );
        assert_eq!(uutier_credentials_path(None, None), None);
        assert_eq!(
            uutier_credentials_path(Some(Path::new("")), Some(Path::new("/home/user"))),
            Some(PathBuf::from("/home/user/.config/uutier/uu.json"))
        );
        assert_eq!(
            uutier_credentials_path(Some(Path::new("relative")), None),
            None
        );
    }
}
