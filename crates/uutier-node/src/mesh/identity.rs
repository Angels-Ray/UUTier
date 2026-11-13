//! 节点身份密钥。

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use blake2::{Blake2s256, Digest};
use tempfile::NamedTempFile;
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};

/// 身份密钥长度，单位 byte。
pub const IDENTITY_KEY_LEN: usize = 32;

/// 节点身份。静态 X25519 密钥对。
#[derive(Clone)]
pub struct Identity {
    secret: StaticSecret,
    public: PublicKey,
}

impl Identity {
    /// 生成新身份。
    pub fn generate() -> Self {
        let secret = StaticSecret::random();
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// 从 32 byte 私钥构造身份。
    pub fn from_secret_bytes(bytes: [u8; IDENTITY_KEY_LEN]) -> Self {
        let secret = StaticSecret::from(bytes);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// 读取身份文件，不存在时生成并保存。
    pub fn create_or_load(path: &Path) -> Result<Self, IdentityError> {
        match fs::metadata(path) {
            Ok(_) => {
                create_private_dir(private_parent(path))?;
                Self::load(path)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let identity = Self::generate();
                identity.save(path)?;
                Ok(identity)
            }
            Err(source) => Err(IdentityError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// 读取身份文件。
    pub fn load(path: &Path) -> Result<Self, IdentityError> {
        let mut file = fs::File::open(path).map_err(|source| IdentityError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // 文件类型和权限取自已打开的句柄，检查和读取针对同一个文件。
        let metadata = file.metadata().map_err(|source| IdentityError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(IdentityError::NotAFile(path.to_path_buf()));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(IdentityError::InsecurePermissions(path.to_path_buf()));
        }
        let len = metadata.len();
        if len != IDENTITY_KEY_LEN as u64 {
            return Err(IdentityError::InvalidKeyLength(
                usize::try_from(len).unwrap_or(usize::MAX),
            ));
        }
        let mut bytes = [0u8; IDENTITY_KEY_LEN];
        file.read_exact(&mut bytes)
            .map_err(|source| IdentityError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(Self::from_secret_bytes(bytes))
    }

    /// 保存身份文件。文件权限 0600，目录权限 0700。
    pub fn save(&self, path: &Path) -> Result<(), IdentityError> {
        let parent = private_parent(path);
        create_private_dir(parent)?;
        let mut temp = NamedTempFile::new_in(parent).map_err(|source| IdentityError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        temp.write_all(&self.secret.to_bytes())
            .and_then(|()| {
                temp.as_file()
                    .set_permissions(fs::Permissions::from_mode(0o600))
            })
            .and_then(|()| temp.as_file().sync_all())
            .map_err(|source| IdentityError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        temp.persist(path).map_err(|err| IdentityError::Io {
            path: path.to_path_buf(),
            source: err.error,
        })?;
        Ok(())
    }

    /// 返回公钥。
    pub fn public_key(&self) -> &[u8; IDENTITY_KEY_LEN] {
        self.public.as_bytes()
    }

    /// 返回公钥的十六进制文本。
    pub fn public_key_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(IDENTITY_KEY_LEN * 2);
        for &byte in self.public.as_bytes() {
            text.push(char::from(HEX[usize::from(byte >> 4)]));
            text.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        text
    }

    /// 返回 peer ID。
    pub fn peer_id(&self) -> u64 {
        peer_id(self.public_key())
    }

    /// 返回私钥字节。只供 Noise 握手使用。
    pub(crate) fn secret_bytes(&self) -> [u8; IDENTITY_KEY_LEN] {
        self.secret.to_bytes()
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("peer_id", &self.peer_id())
            .finish_non_exhaustive()
    }
}

/// 计算公钥的 peer ID。
pub fn peer_id(public_key: &[u8; IDENTITY_KEY_LEN]) -> u64 {
    let digest = Blake2s256::digest(public_key);
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

/// 返回身份文件路径。
pub fn identity_path(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let base = xdg_config_home
        .filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| {
            home.filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
                .map(|home| home.join(".config"))
        })?;
    Some(base.join("uutier").join("identity.key"))
}

/// 从环境变量解析身份文件路径。
pub fn default_identity_path() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    identity_path(xdg.as_deref(), home.as_deref())
}

/// 身份文件错误。
#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{0} is not a regular file")]
    NotAFile(PathBuf),

    #[error("{0} is accessible by group or others")]
    InsecurePermissions(PathBuf),

    #[error("identity key is {0} bytes, need {IDENTITY_KEY_LEN}")]
    InvalidKeyLength(usize),
}

fn private_parent(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn create_private_dir(dir: &Path) -> Result<(), IdentityError> {
    if dir == Path::new(".") {
        return Ok(());
    }
    fs::create_dir_all(dir).map_err(|source| IdentityError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|source| {
        IdentityError::Io {
            path: dir.to_path_buf(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_identity_file_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("identity.key");
        let identity = Identity::create_or_load(&path).unwrap();
        let loaded = Identity::load(&path).unwrap();

        assert_eq!(identity.public_key(), loaded.public_key());
        assert_eq!(identity.peer_id(), loaded.peer_id());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn tightens_existing_directory_before_loading() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.path().join("identity.key");
        fs::write(&path, [1u8; IDENTITY_KEY_LEN]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        Identity::create_or_load(&path).unwrap();
        assert_eq!(
            fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn rejects_short_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        fs::write(&path, [1u8; 3]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(matches!(
            Identity::load(&path),
            Err(IdentityError::InvalidKeyLength(3))
        ));
    }

    #[test]
    fn rejects_key_with_extra_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        fs::write(&path, [1u8; IDENTITY_KEY_LEN + 1]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(matches!(
            Identity::load(&path),
            Err(IdentityError::InvalidKeyLength(33))
        ));
    }

    #[test]
    fn rejects_group_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        fs::write(&path, [1u8; IDENTITY_KEY_LEN]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        assert!(matches!(
            Identity::load(&path),
            Err(IdentityError::InsecurePermissions(_))
        ));
    }

    #[test]
    fn debug_hides_key_material() {
        let identity = Identity::from_secret_bytes([7u8; IDENTITY_KEY_LEN]);
        let text = format!("{identity:?}");
        assert!(!text.contains("070707"));
        assert!(text.contains("peer_id"));
    }

    #[test]
    fn peer_id_is_stable_and_keyed_by_public_key() {
        let first = Identity::from_secret_bytes([1u8; IDENTITY_KEY_LEN]);
        let second = Identity::from_secret_bytes([2u8; IDENTITY_KEY_LEN]);
        let again = Identity::from_secret_bytes([1u8; IDENTITY_KEY_LEN]);

        assert_eq!(first.peer_id(), again.peer_id());
        assert_ne!(first.peer_id(), second.peer_id());
    }

    #[test]
    fn resolves_identity_path() {
        let path = identity_path(
            Some(Path::new("/xdg/config")),
            Some(Path::new("/home/user")),
        );
        assert_eq!(path, Some(PathBuf::from("/xdg/config/uutier/identity.key")));
        let path = identity_path(None, Some(Path::new("/home/user")));
        assert_eq!(
            path,
            Some(PathBuf::from("/home/user/.config/uutier/identity.key"))
        );
        assert_eq!(identity_path(None, None), None);
        assert_eq!(
            identity_path(Some(Path::new("")), Some(Path::new("/home/user"))),
            Some(PathBuf::from("/home/user/.config/uutier/identity.key"))
        );
        assert_eq!(identity_path(Some(Path::new("relative")), None), None);
    }
}
