//! `uutier-turn` 的错误类型。

use std::io;
use std::path::PathBuf;

/// `uutier-turn` 产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),

    #[error("invalid JSON in {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("cannot access credentials file {path}: {source}")]
    CredentialsIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("missing credentials: {}", fields.join(", "))]
    MissingCredentials { fields: Vec<&'static str> },

    #[error("credentials field {field} cannot be sent in an HTTP header")]
    InvalidCredentials { field: &'static str },

    #[error("credentials file {0} is accessible by group or others, expected 0600")]
    InsecurePermissions(PathBuf),

    #[error("credentials directory {0} is accessible by group or others, expected 0700")]
    InsecureDirectory(PathBuf),

    #[error("{0} is not a regular file")]
    NotAFile(PathBuf),

    #[error("credentials file {0} is too large")]
    CredentialsTooLarge(PathBuf),

    #[error("cannot resolve the credentials path: $HOME is not set")]
    MissingPath,

    #[error("UU login token is invalid or expired")]
    LoginExpired,

    #[error("waiting for the QR code scan timed out")]
    LoginTimeout,

    #[error("login QR code requires a terminal on stdout")]
    QrNotTerminal,

    #[error("UU host is offline or has no signaling connection")]
    HostOffline,

    #[error("UU host room already has a viewer")]
    HostBusy,

    #[error("UU API returned error code {code}")]
    Api { code: i64 },

    #[error("UU API returned HTTP status {status}")]
    HttpStatus { status: u16 },

    #[error("UU API response is invalid")]
    InvalidResponse,

    #[error("UU signaling is disconnected")]
    SignalDisconnected,

    #[error("UU signaling timed out")]
    SignalTimeout,

    #[error("UU signaling control request was refused")]
    ControlRejected,

    #[error("UU signaling returned no UDP TURN server")]
    NoTurnServer,

    #[error("UU peer record is invalid: {0}")]
    InvalidRecord(&'static str),

    #[error("cannot resolve the TURN server address: {0}")]
    Resolve(#[source] io::Error),

    #[error("invalid TURN server URL")]
    InvalidServer,

    #[error("TURN transport {0} is not supported")]
    UnsupportedTransport(&'static str),

    #[error("TURN address family is not supported")]
    UnsupportedAddressFamily,

    #[error("TURN protocol error: {0}")]
    Protocol(&'static str),

    #[error("TURN server returned error code {code}")]
    ServerError { code: u16 },

    #[error("TURN request timed out")]
    Timeout,

    #[error("cannot obtain random bytes")]
    Random,

    #[error("internal state error")]
    Internal,
}
