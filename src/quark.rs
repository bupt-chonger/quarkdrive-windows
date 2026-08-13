use std::{
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use qrcode::{QrCode, types::Color};
use reqwest::{
    Url,
    blocking::Client,
    cookie::{CookieStore, Jar},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use thiserror::Error;

const BASE_URL: &str = "https://drive.quark.cn";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RemoteItem {
    pub id: String,
    pub parent_id: String,
    pub name: String,
    pub is_directory: bool,
    pub size: u64,
    pub created_at_ms: i64,
    pub modified_at_ms: i64,
    pub version: String,
}

#[derive(Debug, Error)]
pub enum QuarkError {
    #[error("网络请求失败: {0}")]
    Http(#[from] reqwest::Error),
    #[error("夸克接口返回错误 {code}: {message}")]
    Api { code: i64, message: String },
    #[error("夸克接口响应缺少字段: {0}")]
    Malformed(&'static str),
    #[error("二维码登录失败: {0}")]
    Login(String),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AccountInfo {
    pub nickname: String,
    pub user_id: String,
    pub avatar_url: String,
}

#[derive(Clone, Debug)]
pub struct QrCodeData {
    pub token: String,
    pub url: String,
    pub width: usize,
    pub modules: Vec<bool>,
}

#[derive(Clone, Debug)]
pub struct QrLoginResult {
    pub cookie: String,
    pub account: AccountInfo,
}

pub struct QuarkQrLogin {
    client: Client,
    jar: Arc<Jar>,
}

impl QuarkQrLogin {
    pub fn new() -> Result<Self, QuarkError> {
        let jar = Arc::new(Jar::default());
        let client = Client::builder()
            .user_agent("QuarkDriveWindows/0.1")
            .timeout(Duration::from_secs(30))
            .cookie_provider(jar.clone())
            .build()?;
        Ok(Self { client, jar })
    }

    pub fn get_qr_code(&self) -> Result<QrCodeData, QuarkError> {
        let response: serde_json::Value = self
            .client
            .get("https://uop.quark.cn/cas/ajax/getTokenForQrcodeLogin")
            .query(&[("client_id", "532"), ("v", "1.2"), ("request_id", &uuid())])
            .send()?
            .error_for_status()?
            .json()?;
        let status = value_i64(&response, &["status"]).unwrap_or_default();
        if status != 2_000_000 {
            return Err(QuarkError::Login(format!(
                "获取二维码失败：{}",
                value_string(&response, &["message"]).unwrap_or_else(|| "未知错误".into())
            )));
        }
        let token = value_string(&response, &["data", "members", "token"])
            .ok_or(QuarkError::Malformed("data.members.token"))?;
        let url = format!(
            "https://su.quark.cn/4_eMHBJ?token={}&client_id=532&ssb=weblogin&uc_param_str=&uc_biz_str=S%3Acustom%7COPT%3ASAREA%400%7COPT%3AIMMERSIVE%401%7COPT%3ABACK_BTN_STYLE%400",
            percent_encode(&token)
        );
        let code = QrCode::new(url.as_bytes())
            .map_err(|err| QuarkError::Login(format!("生成二维码失败：{err}")))?;
        let modules = code
            .to_colors()
            .into_iter()
            .map(|color| color == Color::Dark)
            .collect();
        Ok(QrCodeData {
            token,
            url,
            width: code.width(),
            modules,
        })
    }

    pub fn wait_for_login(
        &self,
        token: &str,
        timeout: Duration,
        cancelled: impl Fn() -> bool,
    ) -> Result<QrLoginResult, QuarkError> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cancelled() {
                return Err(QuarkError::Login("登录已取消".into()));
            }
            let response: serde_json::Value = self
                .client
                .get("https://uop.quark.cn/cas/ajax/getServiceTicketByQrcodeToken")
                .query(&[
                    ("client_id", "532"),
                    ("v", "1.2"),
                    ("token", token),
                    ("request_id", &uuid()),
                ])
                .send()?
                .error_for_status()?
                .json()?;
            let status = value_i64(&response, &["status"]).unwrap_or_default();
            if status == 2_000_000
                && let Some(ticket) =
                    value_string(&response, &["data", "members", "service_ticket"])
            {
                return self.finish_login(&ticket);
            }
            if matches!(status, 50_004_002 | 50_004_003 | 50_004_004) {
                return Err(QuarkError::Login(
                    value_string(&response, &["message"]).unwrap_or_else(|| "二维码已失效".into()),
                ));
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        Err(QuarkError::Login("二维码登录超时，请重新扫码".into()))
    }

    fn finish_login(&self, service_ticket: &str) -> Result<QrLoginResult, QuarkError> {
        let response: serde_json::Value = self
            .client
            .get("https://pan.quark.cn/account/info")
            .query(&[("st", service_ticket), ("lw", "scan")])
            .send()?
            .error_for_status()?
            .json()?;
        let account = AccountInfo {
            nickname: first_string(
                &response,
                &[&["data", "nickname"], &["data", "user_info", "nickname"]],
            )
            .unwrap_or_default(),
            user_id: first_string(
                &response,
                &[
                    &["data", "user_id"],
                    &["data", "userId"],
                    &["data", "uid"],
                    &["data", "id"],
                    &["data", "user_info", "user_id"],
                    &["data", "user_info", "uid"],
                ],
            )
            .unwrap_or_default(),
            avatar_url: first_string(
                &response,
                &[
                    &["data", "avatar"],
                    &["data", "avatar_url"],
                    &["data", "avatarUrl"],
                    &["data", "user_info", "avatar"],
                    &["data", "user_info", "avatar_url"],
                ],
            )
            .unwrap_or_default(),
        };
        if account.nickname.is_empty() && account.user_id.is_empty() {
            return Err(QuarkError::Malformed("data.nickname"));
        }
        let cookie = [
            "https://pan.quark.cn/",
            "https://drive.quark.cn/",
            "https://drive-pc.quark.cn/",
            "https://uop.quark.cn/",
        ]
        .into_iter()
        .filter_map(|address| Url::parse(address).ok())
        .filter_map(|url| self.jar.cookies(&url))
        .filter_map(|value| value.to_str().ok().map(str::to_owned))
        .flat_map(|value| {
            value
                .split(';')
                .map(str::trim)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|value| !value.is_empty())
        .fold(Vec::<String>::new(), |mut cookies, value| {
            if !cookies.iter().any(|existing| existing == &value) {
                cookies.push(value);
            }
            cookies
        })
        .join("; ");
        if cookie.is_empty() {
            return Err(QuarkError::Malformed("登录 Cookie"));
        }
        Ok(QrLoginResult { cookie, account })
    }
}

fn uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{timestamp:032x}-{:04x}",
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn json_at<'a>(value: &'a serde_json::Value, path: &[&str]) -> Option<&'a serde_json::Value> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
}

fn value_string(value: &serde_json::Value, path: &[&str]) -> Option<String> {
    json_at(value, path).and_then(|item| {
        item.as_str()
            .map(str::to_owned)
            .or_else(|| item.as_i64().map(|number| number.to_string()))
    })
}

fn value_i64(value: &serde_json::Value, path: &[&str]) -> Option<i64> {
    json_at(value, path).and_then(|item| {
        item.as_i64()
            .or_else(|| item.as_str().and_then(|text| text.parse().ok()))
    })
}

fn first_string(value: &serde_json::Value, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|path| value_string(value, path))
}

#[derive(Clone)]
pub struct QuarkClient {
    client: Client,
    cookie: String,
}

impl QuarkClient {
    pub fn new(cookie: impl Into<String>) -> Result<Self, QuarkError> {
        let client = Client::builder()
            .user_agent("QuarkDriveWindows/0.1")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            cookie: normalize_cookie(&cookie.into()),
        })
    }

    pub fn list_children(&self, parent_id: &str) -> Result<Vec<RemoteItem>, QuarkError> {
        let mut result = Vec::new();
        let mut page = 1_u32;
        loop {
            let envelope: Envelope<ListData> = self
                .client
                .get(format!("{BASE_URL}/1/clouddrive/file/sort"))
                .query(&[
                    ("pr", "ucpro"),
                    ("fr", "pc"),
                    ("pdir_fid", parent_id),
                    ("_page", &page.to_string()),
                    ("_size", "500"),
                    ("_fetch_total", "1"),
                    ("_fetch_sub_dirs", "0"),
                    ("_sort", "file_type:asc,updated_at:desc"),
                ])
                .header("Cookie", &self.cookie)
                .header("Referer", "https://pan.quark.cn/")
                .send()?
                .error_for_status()?
                .json()?;
            let data = envelope.into_data()?;
            let count = data.list.len();
            result.extend(
                data.list
                    .into_iter()
                    .filter_map(|raw| raw.into_item(parent_id)),
            );
            if count < 500 {
                break;
            }
            page += 1;
        }
        result.sort_by_key(|item| item.name.to_lowercase());
        Ok(result)
    }

    pub fn download_range(&self, file_id: &str, range: Range<u64>) -> Result<Vec<u8>, QuarkError> {
        let envelope: Envelope<Vec<DownloadData>> = self
            .client
            .post(format!(
                "{BASE_URL}/1/clouddrive/file/download?pr=ucpro&fr=pc"
            ))
            .header("Cookie", &self.cookie)
            .header("Referer", "https://pan.quark.cn/")
            .json(&json!({"fids": [file_id]}))
            .send()?
            .error_for_status()?
            .json()?;
        let url = envelope
            .into_data()?
            .into_iter()
            .next()
            .ok_or(QuarkError::Malformed("data[0].download_url"))?
            .download_url;
        let end = range.end.saturating_sub(1);
        let response = self
            .client
            .get(url)
            .header("Range", format!("bytes={}-{}", range.start, end))
            .send()?
            .error_for_status()?;
        let status = response.status();
        let bytes = response.bytes()?.to_vec();
        if status.as_u16() == 200 {
            let start = usize::try_from(range.start)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            let length = usize::try_from(range.end - range.start).unwrap_or(usize::MAX);
            return Ok(bytes[start..bytes.len().min(start.saturating_add(length))].to_vec());
        }
        Ok(bytes)
    }

    /// Deletes a file or directory from the cloud drive.
    ///
    /// The web-cookie API accepts the request asynchronously and returns a
    /// task id. The provider treats a successful task submission as the
    /// point at which the local delete may proceed; the next directory poll
    /// reconciles the final remote state.
    pub fn delete_file(&self, file_id: &str) -> Result<(), QuarkError> {
        let envelope: Envelope<serde_json::Value> = self
            .client
            .post(format!(
                "{BASE_URL}/1/clouddrive/file/delete?pr=ucpro&fr=pc&uc_param_str="
            ))
            .header("Cookie", &self.cookie)
            .header("Referer", "https://pan.quark.cn/")
            .json(&json!({
                "action_type": 1,
                "filelist": [file_id],
                "exclude_fids": [],
            }))
            .send()?
            .error_for_status()?
            .json()?;
        envelope.into_data().map(|_| ())
    }

    pub fn check_account(&self, root_id: &str) -> Result<usize, QuarkError> {
        Ok(self.list_children(root_id)?.len())
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    status: Option<i64>,
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    data: Option<T>,
}

impl<T: DeserializeOwned> Envelope<T> {
    fn into_data(self) -> Result<T, QuarkError> {
        if self.status.is_some_and(|status| status != 200) || self.code != 0 {
            return Err(QuarkError::Api {
                code: if self.code != 0 {
                    self.code
                } else {
                    self.status.unwrap_or_default()
                },
                message: self.message,
            });
        }
        self.data.ok_or(QuarkError::Malformed("data"))
    }
}

#[derive(Deserialize)]
struct ListData {
    #[serde(default)]
    list: Vec<RawItem>,
}

#[derive(Deserialize)]
struct RawItem {
    fid: Option<String>,
    file_name: Option<String>,
    #[serde(default)]
    dir: bool,
    #[serde(default, deserialize_with = "number_or_string_u64")]
    size: u64,
    #[serde(default, deserialize_with = "number_or_string_i64")]
    created_at: i64,
    #[serde(default, deserialize_with = "number_or_string_i64")]
    updated_at: i64,
    content_hash: Option<String>,
}

impl RawItem {
    fn into_item(self, parent_id: &str) -> Option<RemoteItem> {
        let id = self.fid?;
        let name = self.file_name?;
        let version = format!(
            "{}:{}:{}",
            self.updated_at,
            self.size,
            self.content_hash.unwrap_or_default()
        );
        Some(RemoteItem {
            id,
            parent_id: parent_id.into(),
            name,
            is_directory: self.dir,
            size: if self.dir { 0 } else { self.size },
            created_at_ms: self.created_at,
            modified_at_ms: self.updated_at,
            version,
        })
    }
}

#[derive(Deserialize)]
struct DownloadData {
    download_url: String,
}

fn normalize_cookie(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("Cookie:")
        .trim()
        .replace(['\r', '\n'], "")
}

fn number_or_string_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(value
        .as_u64()
        .or_else(|| value.as_str()?.parse().ok())
        .unwrap_or_default())
}

fn number_or_string_i64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(value
        .as_i64()
        .or_else(|| value.as_str()?.parse().ok())
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_is_normalized() {
        assert_eq!(normalize_cookie(" Cookie: a=b; c=d\r\n"), "a=b; c=d");
    }

    #[test]
    fn raw_item_maps_stable_identity() {
        let raw: RawItem = serde_json::from_value(
            json!({"fid":"42","file_name":"a.txt","dir":false,"size":"12","updated_at":9}),
        )
        .unwrap();
        let item = raw.into_item("0").unwrap();
        assert_eq!(
            (item.id.as_str(), item.size, item.version.as_str()),
            ("42", 12, "9:12:")
        );
    }
}
