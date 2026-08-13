use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub cookie: String,
    #[serde(default)]
    pub account_nickname: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub account_avatar: String,
    #[serde(default = "default_root_id")]
    pub remote_root_id: String,
    #[serde(default = "default_root_name")]
    pub remote_root_name: String,
    #[serde(default = "default_root_label")]
    pub remote_root_label: String,
    #[serde(default = "default_start_on_login")]
    pub start_on_login: bool,
    pub mount_path: PathBuf,
}

fn default_root_id() -> String {
    "0".into()
}
fn default_root_name() -> String {
    "夸克网盘".into()
}
fn default_root_label() -> String {
    "全部文件".into()
}
fn default_start_on_login() -> bool {
    true
}

impl Config {
    pub fn load_unchecked(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("无法读取配置文件 {}", path.display()))?;
        serde_json::from_str(&text).context("配置文件不是有效 JSON")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let config = Self::load_unchecked(path)?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        fs::write(path, text).with_context(|| format!("无法写入配置文件 {}", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        if self.cookie.trim().is_empty() {
            bail!("尚未登录夸克网盘：请打开设置页扫码登录，或使用兼容性的 login --cookie 命令");
        }
        if self.remote_root_id.trim().is_empty() {
            bail!("远端根目录 ID 不能为空");
        }
        if !self.mount_path.is_absolute() {
            bail!("挂载目录必须是绝对路径");
        }
        Ok(())
    }
}

pub fn default_config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("无法确定当前用户的配置目录")?;
    Ok(base.join("QuarkDrive").join("config.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let config = Config {
            cookie: "k=v".into(),
            account_nickname: "测试账号".into(),
            account_id: "1001".into(),
            account_avatar: String::new(),
            remote_root_id: "0".into(),
            remote_root_name: "网盘".into(),
            remote_root_label: "全部文件".into(),
            start_on_login: true,
            mount_path: dir.path().join("mount"),
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().cookie, "k=v");
    }
}
