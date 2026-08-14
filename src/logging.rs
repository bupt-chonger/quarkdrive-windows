use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::PathBuf,
    sync::OnceLock,
};

use anyhow::{Context, Result};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

struct FileMakeWriter {
    path: PathBuf,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileMakeWriter {
    type Writer = File;

    fn make_writer(&'a self) -> Self::Writer {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .unwrap_or_else(|_| File::create(&self.path).expect("无法创建夸克网盘日志文件"))
    }
}

pub fn path() -> PathBuf {
    LOG_PATH
        .get_or_init(|| {
            dirs::data_local_dir()
                .or_else(dirs::config_dir)
                .unwrap_or_else(|| PathBuf::from("."))
                .join("QuarkDrive")
                .join("quarkdrive.log")
        })
        .clone()
}

pub fn init() {
    let path = path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "quarkdrive=info".into());
    let layer = fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_writer(FileMakeWriter { path });
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .try_init();
}

pub fn read_tail(max_bytes: usize) -> Result<String> {
    let mut file = match File::open(path()) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(err) => return Err(err).context("无法读取日志文件"),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).context("无法读取日志文件")?;
    if bytes.len() > max_bytes {
        bytes = bytes.split_off(bytes.len() - max_bytes);
        if let Some(index) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes = bytes.split_off(index + 1);
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn clear() -> Result<()> {
    let log_path = path();
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).context("无法创建日志目录")?;
    }
    fs::write(log_path, []).context("无法清理日志文件")
}
