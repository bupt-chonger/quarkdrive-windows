use std::path::PathBuf;

use anyhow::Result;
#[cfg(not(windows))]
use anyhow::bail;
use clap::{CommandFactory, Parser, Subcommand};
use quarkdrive_windows::{
    config::{Config, default_config_path},
    logging,
    quark::QuarkClient,
};

#[derive(Parser)]
#[command(
    name = "quarkdrive",
    version,
    about = "将夸克网盘挂载到 Windows 资源管理器"
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    #[command(hide = true)]
    Start,
    Init {
        #[arg(long, default_value = "")]
        cookie: String,
        #[arg(long)]
        mount: PathBuf,
        #[arg(long, default_value = "0")]
        root_id: String,
        #[arg(long, default_value = "夸克网盘")]
        root_name: String,
    },
    /// 使用已有的夸克网盘网页 Cookie 兼容登录；设置页推荐使用二维码登录
    Login {
        /// 从 pan.quark.cn 浏览器会话中复制的完整 Cookie（兼容模式）
        #[arg(long)]
        cookie: String,
        #[arg(long)]
        mount: Option<PathBuf>,
        /// 只更新登录会话，不启动挂载
        #[arg(long)]
        no_mount: bool,
    },
    Doctor,
    List {
        #[arg(long)]
        parent_id: Option<String>,
    },
    Mount,
    Status,
    /// 检测挂载目录的逻辑容量、本地占用和可释放容量
    Storage,
    #[command(hide = true)]
    ReleaseOne {
        path: PathBuf,
    },
    Unregister,
}

fn main() -> Result<()> {
    let raw_args: Vec<_> = std::env::args_os().collect();
    if raw_args
        .iter()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h" || arg == "help")
    {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }
    if raw_args
        .iter()
        .skip(1)
        .any(|arg| arg == "--version" || arg == "-V")
    {
        println!("quarkdrive {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    logging::init();
    let cli = Cli::parse();
    let path = cli.config.map(Ok).unwrap_or_else(default_config_path)?;
    #[cfg(windows)]
    if matches!(
        cli.command.as_ref(),
        None | Some(Command::Start) | Some(Command::Mount)
    ) {
        use windows::Win32::{
            System::Console::GetConsoleWindow,
            UI::WindowsAndMessaging::{SW_HIDE, ShowWindow},
        };
        let console = unsafe { GetConsoleWindow() };
        if !console.is_invalid() {
            let _ = unsafe { ShowWindow(console, SW_HIDE) };
        }
    }
    match cli.command.unwrap_or(Command::Start) {
        Command::Start => start(&path)?,
        Command::Init {
            cookie,
            mount,
            root_id,
            root_name,
        } => {
            let config = Config {
                cookie,
                account_nickname: String::new(),
                account_id: String::new(),
                account_avatar: String::new(),
                remote_root_id: root_id,
                remote_root_name: root_name,
                remote_root_label: "全部文件".into(),
                start_on_login: true,
                mount_path: mount,
            };
            config.save(&path)?;
            println!("配置已保存：{}", path.display());
        }
        Command::Login {
            cookie,
            mount: requested_mount,
            no_mount,
        } => {
            let config = save_cookie(&path, cookie, requested_mount)?;
            if no_mount {
                println!("夸克网盘登录会话已更新。");
            } else {
                println!("登录会话已更新，正在挂载夸克网盘……");
                mount(config, &path)?;
            }
        }
        Command::Doctor => {
            let config = Config::load(&path)?;
            let count = QuarkClient::new(&config.cookie)?.check_account(&config.remote_root_id)?;
            println!(
                "登录有效；远端根目录包含 {count} 个项目。\n挂载目录：{}",
                config.mount_path.display()
            );
        }
        Command::List { parent_id } => {
            let config = Config::load(&path)?;
            let parent = parent_id.unwrap_or_else(|| config.remote_root_id.clone());
            let items = QuarkClient::new(&config.cookie)?.list_children(&parent)?;
            for item in items {
                println!(
                    "{}\t{}\t{}\t{}",
                    if item.is_directory { "DIR " } else { "FILE" },
                    item.size,
                    item.id,
                    item.name
                );
            }
        }
        Command::Mount => {
            let config = Config::load(&path)?;
            mount(config, &path)?;
        }
        Command::Status => {
            let config = Config::load(&path)?;
            status(&config)?;
        }
        Command::Storage => {
            let config = Config::load(&path)?;
            #[cfg(windows)]
            {
                let stats =
                    quarkdrive_windows::cloud_files::scan_local_storage(&config.mount_path)?;
                println!(
                    "逻辑容量：{} 字节\n本地占用：{} 字节\n可释放：{} 字节\n文件数：{}\n可释放文件数：{}",
                    stats.logical_bytes,
                    stats.local_bytes,
                    stats.releasable_bytes,
                    stats.file_count,
                    stats.releasable_files
                );
            }
            #[cfg(not(windows))]
            bail!("本地空间检测仅支持 Windows");
        }
        Command::ReleaseOne { path: target } => {
            let config = Config::load(&path)?;
            #[cfg(windows)]
            quarkdrive_windows::cloud_files::release_local_file(&config.mount_path, &target)?;
            #[cfg(not(windows))]
            bail!("本地空间释放仅支持 Windows");
        }
        Command::Unregister => {
            let config = Config::load(&path)?;
            unregister(&config.mount_path)?;
            println!(
                "已注销同步根：{}（本地文件未删除）",
                config.mount_path.display()
            );
        }
    }
    Ok(())
}

fn start(path: &std::path::Path) -> Result<()> {
    match Config::load(path) {
        Ok(config) => mount(config, path),
        Err(err) => {
            tracing::warn!(?err, "夸克网盘配置不可用，打开设置页扫码登录");
            #[cfg(windows)]
            {
                let config = Config::load_unchecked(path).unwrap_or_else(|_| Config {
                    cookie: String::new(),
                    account_nickname: String::new(),
                    account_id: String::new(),
                    account_avatar: String::new(),
                    remote_root_id: "0".into(),
                    remote_root_name: "夸克网盘".into(),
                    remote_root_label: "全部文件".into(),
                    start_on_login: true,
                    mount_path: default_mount_path(),
                });
                quarkdrive_windows::windows_app::run(config, path.to_path_buf(), None)
            }
            #[cfg(not(windows))]
            return Err(err);
        }
    }
}

fn save_cookie(
    path: &std::path::Path,
    cookie: String,
    requested_mount: Option<PathBuf>,
) -> Result<Config> {
    let existing = Config::load_unchecked(path).ok();
    let config = Config {
        cookie,
        account_nickname: existing
            .as_ref()
            .map(|c| c.account_nickname.clone())
            .unwrap_or_default(),
        account_id: existing
            .as_ref()
            .map(|c| c.account_id.clone())
            .unwrap_or_default(),
        account_avatar: existing
            .as_ref()
            .map(|c| c.account_avatar.clone())
            .unwrap_or_default(),
        remote_root_id: existing
            .as_ref()
            .map(|c| c.remote_root_id.clone())
            .unwrap_or_else(|| "0".into()),
        remote_root_name: existing
            .as_ref()
            .map(|c| c.remote_root_name.clone())
            .unwrap_or_else(|| "夸克网盘".into()),
        remote_root_label: existing
            .as_ref()
            .map(|c| c.remote_root_label.clone())
            .unwrap_or_else(|| "全部文件".into()),
        start_on_login: existing.as_ref().map(|c| c.start_on_login).unwrap_or(true),
        mount_path: requested_mount
            .or_else(|| existing.map(|c| c.mount_path))
            .unwrap_or_else(default_mount_path),
    };
    config.save(path)?;
    Ok(config)
}

fn default_mount_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("C:\\"))
        .join("QuarkDrive")
}

#[cfg(windows)]
fn status(config: &Config) -> Result<()> {
    let value = quarkdrive_windows::cloud_files::registration_status(&config.mount_path);
    println!(
        "资源管理器注册：{}\n挂载目录存在：{}\n同步根 ID：{}\n挂载目录：{}",
        if value.shell_registered {
            "正常"
        } else {
            "未注册（请运行 mount）"
        },
        value.directory_exists,
        value.sync_root_id,
        config.mount_path.display()
    );
    Ok(())
}
#[cfg(not(windows))]
fn status(_: &Config) -> Result<()> {
    bail!("仅支持 Windows")
}

#[cfg(windows)]
fn mount(config: Config, path: &std::path::Path) -> Result<()> {
    let mut config = config;
    quarkdrive_windows::cloud_files::normalize_remote_root(&mut config)?;
    config.save(path)?;
    let connection = quarkdrive_windows::cloud_files::register_and_connect(&config)?;
    quarkdrive_windows::windows_app::run(config, path.to_path_buf(), Some(connection))
}
#[cfg(not(windows))]
fn mount(_: Config, _: &std::path::Path) -> Result<()> {
    bail!("Cloud Files 挂载仅支持 Windows 10 1709 或更高版本")
}

#[cfg(windows)]
fn unregister(path: &std::path::Path) -> Result<()> {
    quarkdrive_windows::cloud_files::unregister(path)
}
#[cfg(not(windows))]
fn unregister(_: &std::path::Path) -> Result<()> {
    bail!("Cloud Files 挂载仅支持 Windows")
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn no_arguments_selects_automatic_start() {
        let cli = Cli::try_parse_from(["quarkdrive"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn explicit_status_is_parsed_without_starting() {
        let cli = Cli::try_parse_from(["quarkdrive", "status"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Status)));
    }
}
