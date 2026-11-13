//! 公共错误类型。

/// `uutier-core` 产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// 配置字段无效。
    #[error("invalid configuration: {0}")]
    ConfigInvalid(String),
}
