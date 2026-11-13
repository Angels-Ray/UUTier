//! UUTier 命令行入口。

#![forbid(unsafe_code)]

/// 向 stdout 写一行。下游关闭管道时返回成功，其余写入错误交给调用方。
macro_rules! outln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        crate::stdout_result(writeln!(std::io::stdout().lock(), $($arg)*))
    }};
}

/// 向 stdout 写文本。下游关闭管道时返回成功，其余写入错误交给调用方。
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        crate::stdout_result(write!(std::io::stdout().lock(), $($arg)*))
    }};
}

/// 下游关闭管道不算错误，`uutier status | head` 正常退出。
fn stdout_result(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

mod commands;

use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use commands::CommandError;

/// 命令行参数。
#[derive(Debug, Parser)]
#[command(
    name = "uutier",
    version,
    about = "Mesh networking with UU TURN fallback"
)]
struct Cli {
    /// 配置文件路径。
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// 日志级别，默认 info。未指定时读取 `RUST_LOG`。
    #[arg(
        long,
        global = true,
        value_parser = ["trace", "debug", "info", "warn", "error"]
    )]
    log_level: Option<String>,

    #[command(subcommand)]
    command: Command,
}

/// 子命令。
#[derive(Debug, Subcommand)]
enum Command {
    /// 扫码登录 UU 账号，写入凭据文件。二维码只显示在终端。
    Login {
        /// 注册一个带 `uutier-` 标记的新设备，不复用已保存的设备 ID。
        #[arg(long)]
        new_device: bool,
    },
    /// 删除 UUTier 的凭据文件。
    Logout,
    /// 前台运行节点，需要创建 TUN 的权限。
    Up,
    /// 输出节点状态。
    Status {
        /// 输出 JSON。
        #[arg(long)]
        json: bool,
    },
    /// 检查配置、凭据、身份、UU 设备列表和 TUN。
    Doctor,
    /// 配置相关的命令。
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// 身份相关的命令。
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
}

/// 身份子命令。
#[derive(Debug, Subcommand)]
enum KeyCommand {
    /// 创建或读取本机身份密钥，输出 peer ID 和公钥。
    Show,
}

/// 配置子命令。
#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// 校验配置。
    Check,
    /// 输出配置，秘密字段被替换。
    Show,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.log_level.as_deref());

    let explicit_config = cli.config;
    let config_path = explicit_config.clone().or_else(config_from_env);
    match run(
        explicit_config.as_deref(),
        config_path.as_deref(),
        cli.command,
    )
    .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let _ = writeln!(std::io::stderr().lock(), "uutier: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}

/// 读取 `UUTIER_CONFIG`。空值视为未设置。
fn config_from_env() -> Option<PathBuf> {
    std::env::var_os("UUTIER_CONFIG")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

async fn run(
    explicit_config: Option<&Path>,
    config_path: Option<&Path>,
    command: Command,
) -> Result<(), CommandError> {
    match command {
        Command::Login { new_device } => commands::login::run(config_path, new_device).await,
        Command::Logout => commands::logout::run(config_path),
        Command::Up => commands::up::run(config_path).await,
        Command::Status { json } => {
            reject_config(explicit_config, "status")?;
            commands::status::run(json)
        }
        Command::Doctor => commands::doctor::run(config_path).await,
        Command::Config { command } => match command {
            ConfigCommand::Check => commands::config::check(config_path),
            ConfigCommand::Show => commands::config::show(config_path),
        },
        Command::Key { command } => match command {
            KeyCommand::Show => {
                reject_config(explicit_config, "key show")?;
                commands::key::show()
            }
        },
    }
}

/// 不读取配置的命令拒绝显式 `--config`，避免路径写错却没有任何效果。
fn reject_config(config_path: Option<&Path>, command: &str) -> Result<(), CommandError> {
    match config_path {
        Some(_) => Err(CommandError::config(format!(
            "{command} does not read the config file"
        ))),
        None => Ok(()),
    }
}

fn init_logging(level: Option<&str>) {
    let filter = match level {
        Some(level) => EnvFilter::try_new(level).ok(),
        None => env_filter(),
    }
    .unwrap_or_else(|| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

/// 读取 `RUST_LOG`。空值视为未设置，无效值给出警告。
fn env_filter() -> Option<EnvFilter> {
    let value = std::env::var_os("RUST_LOG")?;
    let text = value.to_string_lossy();
    if text.trim().is_empty() {
        return None;
    }
    match EnvFilter::try_new(text.as_ref()) {
        Ok(filter) => Some(filter),
        Err(err) => {
            let _ = writeln!(std::io::stderr().lock(), "uutier: ignoring RUST_LOG: {err}");
            None
        }
    }
}
