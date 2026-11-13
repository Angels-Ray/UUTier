//! 子命令实现。

pub mod config;
pub mod doctor;
pub mod key;
pub mod login;
pub mod logout;
pub mod status;
pub mod up;

/// 命令错误和退出码。
#[derive(Debug)]
pub struct CommandError {
    message: String,
    exit_code: u8,
}

impl CommandError {
    /// 运行错误，退出码 1。
    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 1,
        }
    }

    /// 配置错误，退出码 2。
    pub fn config(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 2,
        }
    }

    /// stdout 写入失败，退出码 1。
    pub fn output(err: std::io::Error) -> Self {
        Self::runtime(format!("cannot write to stdout: {err}"))
    }

    /// 返回退出码。
    pub fn exit_code(&self) -> u8 {
        self.exit_code
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CommandError {}
