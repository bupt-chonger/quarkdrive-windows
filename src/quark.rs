use std::{
    fs::File,
    io::Read,
    ops::Range,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use md5::{Digest, Md5};
use qrcode::{QrCode, types::Color};
use reqwest::{
    Url,
    blocking::Client,
    cookie::{CookieStore, Jar},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use sha1::Sha1;
use thiserror::Error;

const BASE_URL: &str = "https://drive.quark.cn";
const API_PARAMS: &[(&str, &str)] = &[("pr", "ucpro"), ("fr", "pc"), ("uc_param_str", "")];
const OSS_USER_AGENT: &str = "aliyun-sdk-js/6.6.1 Chrome 98.0.4758.80 on Windows 10 64-bit";

/// Names reserved by the sync implementation for recycle-bin and staged
/// upload objects. They are implementation details, not user files, and must
/// never be exposed as remote placeholders or uploaded from the local mount.
pub fn is_internal_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with(".quarkdrive-trash-")
        || name.starts_with("_quarkdrive_trash_")
        || name.starts_with(".quarkdrive-upload-")
        || name.starts_with(".quarkdrive-backup-")
}

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
    #[error("本地文件操作失败: {0}")]
    Io(#[from] std::io::Error),
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
            if matches!(status, 50_004_002..=50_004_004) {
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
        let started = Instant::now();
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
                    .filter(|raw| {
                        raw.file_name
                            .as_deref()
                            .map(|name| !is_internal_name(name))
                            .unwrap_or(true)
                    })
                    .filter_map(|raw| raw.into_item(parent_id)),
            );
            if count < 500 {
                break;
            }
            page += 1;
        }
        result.sort_by_key(|item| item.name.to_lowercase());
        tracing::info!(
            target: "quarkdrive::api",
            operation = "list_children",
            endpoint = "/1/clouddrive/file/sort",
            parent_id,
            elapsed_ms = started.elapsed().as_millis() as u64,
            returned = result.len(),
            result = "ok",
            "夸克 API 调用完成"
        );
        Ok(result)
    }

    pub fn download_range(&self, file_id: &str, range: Range<u64>) -> Result<Vec<u8>, QuarkError> {
        let started = Instant::now();
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
            let data = bytes[start..bytes.len().min(start.saturating_add(length))].to_vec();
            tracing::info!(
                target: "quarkdrive::api",
                operation = "download_range",
                endpoint = "/1/clouddrive/file/download",
                file_id,
                requested_bytes = range.end.saturating_sub(range.start),
                returned = data.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                result = "ok",
                "夸克 API 调用完成"
            );
            return Ok(data);
        }
        tracing::info!(
            target: "quarkdrive::api",
            operation = "download_range",
            endpoint = "/1/clouddrive/file/download",
            file_id,
            requested_bytes = range.end.saturating_sub(range.start),
            returned = bytes.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            result = "ok",
            "夸克 API 调用完成"
        );
        Ok(bytes)
    }

    pub fn create_folder(&self, parent_id: &str, name: &str) -> Result<String, QuarkError> {
        let started = Instant::now();
        let response: Envelope<serde_json::Value> = self
            .client
            .post(format!("{BASE_URL}/1/clouddrive/file"))
            .query(API_PARAMS)
            .header("Cookie", &self.cookie)
            .header("Referer", "https://pan.quark.cn/")
            .json(&json!({
                "pdir_fid": parent_id,
                "file_name": name,
                "dir_path": "",
                "dir_init_lock": false,
            }))
            .send()?
            .error_for_status()?
            .json()?;
        let data = response.into_data()?;
        let id = first_json_string(&data, &[&["fid"], &["data", "fid"]])
            .ok_or(QuarkError::Malformed("data.fid"))?;
        tracing::info!(
            target: "quarkdrive::api",
            operation = "create_folder",
            endpoint = "/1/clouddrive/file",
            parent_id,
            name,
            elapsed_ms = started.elapsed().as_millis() as u64,
            returned = 1,
            result = "ok",
            "夸克 API 调用完成"
        );
        Ok(id)
    }

    pub fn move_file(&self, file_id: &str, target_parent_id: &str) -> Result<(), QuarkError> {
        self.file_operation(
            "move_file",
            "/1/clouddrive/file/move",
            json!({
                "filelist": [file_id],
                "to_pdir_fid": target_parent_id,
            }),
        )
    }

    pub fn rename_file(&self, file_id: &str, name: &str) -> Result<(), QuarkError> {
        self.file_operation(
            "rename_file",
            "/1/clouddrive/file/rename",
            json!({"fid": file_id, "file_name": name}),
        )
    }

    fn file_operation(
        &self,
        operation: &'static str,
        endpoint: &'static str,
        body: serde_json::Value,
    ) -> Result<(), QuarkError> {
        let started = Instant::now();
        let envelope: Envelope<serde_json::Value> = self
            .client
            .post(format!("{BASE_URL}{endpoint}"))
            .query(API_PARAMS)
            .header("Cookie", &self.cookie)
            .header("Referer", "https://pan.quark.cn/")
            .json(&body)
            .send()?
            .error_for_status()?
            .json()?;
        envelope.into_data()?;
        tracing::info!(
            target: "quarkdrive::api",
            operation,
            endpoint,
            elapsed_ms = started.elapsed().as_millis() as u64,
            returned = 0,
            result = "ok",
            "夸克 API 调用完成"
        );
        Ok(())
    }

    pub fn upload_file(&self, path: &Path, parent_id: &str) -> Result<RemoteItem, QuarkError> {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or(QuarkError::Malformed("local file name"))?;
        self.upload_file_named(path, parent_id, name)
    }

    fn upload_file_named(
        &self,
        path: &Path,
        parent_id: &str,
        name: &str,
    ) -> Result<RemoteItem, QuarkError> {
        let started = Instant::now();
        let metadata = std::fs::metadata(path)?;
        let size = metadata.len();
        let (md5, sha1) = file_hashes(path)?;
        let now_ms = unix_time_ms(std::time::SystemTime::now()).unwrap_or_default();
        let created_at_ms = metadata
            .created()
            .ok()
            .and_then(unix_time_ms)
            .unwrap_or(now_ms);
        let modified_at_ms = metadata
            .modified()
            .ok()
            .and_then(unix_time_ms)
            .unwrap_or(now_ms);
        let pre_response: serde_json::Value = self
            .client
            .post(format!("{BASE_URL}/1/clouddrive/file/upload/pre"))
            .query(API_PARAMS)
            .header("Cookie", &self.cookie)
            .header("Referer", "https://pan.quark.cn/")
            .json(&json!({
                "ccp_hash_update": true,
                "parallel_upload": false,
                "dir_name": "",
                "file_name": name,
                "format_type": "application/octet-stream",
                "l_created_at": metadata.created().ok().and_then(unix_time_ms),
                "l_updated_at": metadata.modified().ok().and_then(unix_time_ms),
                "pdir_fid": parent_id,
                "size": size,
            }))
            .send()?
            .error_for_status()?
            .json()?;
        ensure_success(&pre_response)?;
        let data = pre_response
            .get("data")
            .cloned()
            .ok_or(QuarkError::Malformed("data"))?;
        let fid = json_string(&data, "fid")?;
        let task_id = json_string(&data, "task_id")?;
        let item = RemoteItem {
            id: fid,
            parent_id: parent_id.to_string(),
            name: name.to_string(),
            is_directory: false,
            size,
            created_at_ms,
            modified_at_ms,
            version: format!("{modified_at_ms}:{size}:{sha1}"),
        };
        if data.get("finish").and_then(|value| value.as_bool()) == Some(true) {
            tracing::info!(
                target: "quarkdrive::api",
                operation = "upload_file",
                endpoint = "/1/clouddrive/file/upload/pre",
                parent_id,
                name,
                bytes = size,
                elapsed_ms = started.elapsed().as_millis() as u64,
                returned = 1,
                result = "ok",
                "夸克 API 快速上传完成"
            );
            return Ok(item);
        }
        let hash_response: serde_json::Value = self
            .client
            .post(format!("{BASE_URL}/1/clouddrive/file/update/hash"))
            .query(API_PARAMS)
            .header("Cookie", &self.cookie)
            .header("Referer", "https://pan.quark.cn/")
            .json(&json!({"md5": md5, "sha1": sha1, "task_id": task_id}))
            .send()?
            .error_for_status()?
            .json()?;
        ensure_success(&hash_response)?;
        let rapid = hash_response
            .get("data")
            .and_then(|value| value.get("finish"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if rapid {
            tracing::info!(
                target: "quarkdrive::api",
                operation = "upload_file",
                endpoint = "/1/clouddrive/file/update/hash",
                parent_id,
                name,
                bytes = size,
                elapsed_ms = started.elapsed().as_millis() as u64,
                returned = 1,
                result = "ok",
                "夸克 API 哈希秒传完成"
            );
            return Ok(item);
        }
        if size == 0 {
            return Err(QuarkError::Api {
                code: 500,
                message: "夸克接口未完成零字节文件上传".into(),
            });
        }
        let obj_key = json_string(&data, "obj_key")?;
        let upload_id = json_string(&data, "upload_id")?;
        let bucket = json_string(&data, "bucket")?;
        let upload_url = json_string(&data, "upload_url")?;
        let auth_info = data
            .get("auth_info")
            .cloned()
            .ok_or(QuarkError::Malformed("data.auth_info"))?;
        let callback = data.get("callback").cloned().unwrap_or_else(|| json!({}));
        if !rapid {
            let part_size = pre_response
                .get("metadata")
                .and_then(|value| value.get("part_size"))
                .and_then(value_u64)
                .unwrap_or(8 * 1024 * 1024) as usize;
            let mut file = File::open(path)?;
            let mut part_number = 1_u32;
            let mut etags = Vec::new();
            loop {
                let mut part = vec![0_u8; part_size];
                let count = file.read(&mut part)?;
                if count == 0 {
                    break;
                }
                part.truncate(count);
                let etag = self.upload_part(
                    &upload_url,
                    &bucket,
                    &obj_key,
                    &upload_id,
                    &task_id,
                    &auth_info,
                    part_number,
                    part,
                )?;
                etags.push(etag);
                part_number += 1;
            }
            self.commit_upload(
                &upload_url,
                &bucket,
                &obj_key,
                &upload_id,
                &task_id,
                &auth_info,
                &callback,
                &etags,
            )?;
        }
        let finish_response: serde_json::Value = self
            .client
            .post(format!("{BASE_URL}/1/clouddrive/file/upload/finish"))
            .query(API_PARAMS)
            .header("Cookie", &self.cookie)
            .header("Referer", "https://pan.quark.cn/")
            .json(&json!({"obj_key": obj_key, "task_id": task_id}))
            .send()?
            .error_for_status()?
            .json()?;
        ensure_success(&finish_response)?;
        tracing::info!(
            target: "quarkdrive::api",
            operation = "upload_file",
            endpoint = "/1/clouddrive/file/upload",
            parent_id,
            name,
            bytes = size,
            elapsed_ms = started.elapsed().as_millis() as u64,
            returned = 1,
            result = "ok",
            "夸克 API 调用完成"
        );
        Ok(item)
    }

    pub fn replace_file(
        &self,
        path: &Path,
        parent_id: &str,
        old_id: &str,
        name: &str,
    ) -> Result<RemoteItem, QuarkError> {
        let token = uuid();
        let staged_name = format!(".quarkdrive-upload-{token}");
        let backup_name = format!(".quarkdrive-backup-{token}");
        let staged = self.upload_file_named(path, parent_id, &staged_name)?;
        if let Err(err) = self.rename_file(old_id, &backup_name) {
            let _ = self.delete_file(&staged.id);
            return Err(err);
        }
        if let Err(err) = self.rename_file(&staged.id, name) {
            let _ = self.rename_file(old_id, name);
            let _ = self.delete_file(&staged.id);
            return Err(err);
        }
        if let Err(err) = self.delete_file(old_id) {
            let _ = self.rename_file(&staged.id, &staged_name);
            let _ = self.rename_file(old_id, name);
            let _ = self.delete_file(&staged.id);
            return Err(err);
        }
        Ok(RemoteItem {
            id: staged.id,
            parent_id: parent_id.to_string(),
            name: name.to_string(),
            is_directory: false,
            size: staged.size,
            created_at_ms: staged.created_at_ms,
            modified_at_ms: staged.modified_at_ms,
            version: staged.version,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn upload_part(
        &self,
        upload_url: &str,
        bucket: &str,
        obj_key: &str,
        upload_id: &str,
        task_id: &str,
        auth_info: &serde_json::Value,
        part_number: u32,
        part: Vec<u8>,
    ) -> Result<String, QuarkError> {
        let date = http_date();
        let auth_meta = format!(
            "PUT\\n\\napplication/octet-stream\\n{date}\\nx-oss-date:{date}\\nx-oss-user-agent:{OSS_USER_AGENT}\\n/{bucket}/{obj_key}?partNumber={part_number}&uploadId={upload_id}"
        );
        let auth = self.upload_auth(task_id, auth_info, &auth_meta)?;
        let response = self
            .client
            .put(oss_endpoint(upload_url, bucket, obj_key))
            .query(&[
                ("partNumber", part_number.to_string()),
                ("uploadId", upload_id.to_string()),
            ])
            .header("Authorization", auth)
            .header("Content-Type", "application/octet-stream")
            .header("Referer", "https://pan.quark.cn/")
            .header("x-oss-date", date)
            .header("x-oss-user-agent", OSS_USER_AGENT)
            .body(part)
            .send()?
            .error_for_status()?;
        response
            .headers()
            .get("ETag")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .ok_or(QuarkError::Malformed("upload part ETag"))
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_upload(
        &self,
        upload_url: &str,
        bucket: &str,
        obj_key: &str,
        upload_id: &str,
        task_id: &str,
        auth_info: &serde_json::Value,
        callback: &serde_json::Value,
        etags: &[String],
    ) -> Result<(), QuarkError> {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><CompleteMultipartUpload>{}</CompleteMultipartUpload>",
            etags
                .iter()
                .enumerate()
                .map(|(index, etag)| format!(
                    "<Part><PartNumber>{}</PartNumber><ETag>{etag}</ETag></Part>",
                    index + 1
                ))
                .collect::<String>()
        );
        let content_md5 = BASE64.encode(Md5::digest(xml.as_bytes()));
        let callback_base64 = BASE64.encode(
            serde_json::to_vec(callback).map_err(|err| QuarkError::Login(err.to_string()))?,
        );
        let date = http_date();
        let auth_meta = format!(
            "POST\\n{content_md5}\\napplication/xml\\n{date}\\nx-oss-callback:{callback_base64}\\nx-oss-date:{date}\\nx-oss-user-agent:{OSS_USER_AGENT}\\n/{bucket}/{obj_key}?uploadId={upload_id}"
        );
        let auth = self.upload_auth(task_id, auth_info, &auth_meta)?;
        self.client
            .post(oss_endpoint(upload_url, bucket, obj_key))
            .query(&[("uploadId", upload_id)])
            .header("Authorization", auth)
            .header("Content-MD5", content_md5)
            .header("Content-Type", "application/xml")
            .header("x-oss-callback", callback_base64)
            .header("x-oss-date", date)
            .header("x-oss-user-agent", OSS_USER_AGENT)
            .body(xml)
            .send()?
            .error_for_status()?;
        Ok(())
    }

    fn upload_auth(
        &self,
        task_id: &str,
        auth_info: &serde_json::Value,
        auth_meta: &str,
    ) -> Result<String, QuarkError> {
        let response: Envelope<UploadAuthData> = self
            .client
            .post(format!("{BASE_URL}/1/clouddrive/file/upload/auth"))
            .query(API_PARAMS)
            .header("Cookie", &self.cookie)
            .header("Referer", "https://pan.quark.cn/")
            .json(&json!({"auth_info": auth_info, "auth_meta": auth_meta, "task_id": task_id}))
            .send()?
            .error_for_status()?
            .json()?;
        Ok(response.into_data()?.auth_key)
    }

    /// Deletes a file or directory from the cloud drive.
    ///
    /// The web-cookie API accepts the request asynchronously and returns a
    /// task id. The provider treats a successful task submission as the
    /// point at which the local delete may proceed; the next directory poll
    /// reconciles the final remote state.
    pub fn delete_file(&self, file_id: &str) -> Result<(), QuarkError> {
        let started = Instant::now();
        let envelope: Envelope<serde_json::Value> = self
            .client
            .post(format!(
                "{BASE_URL}/1/clouddrive/file/delete?pr=ucpro&fr=pc&uc_param_str="
            ))
            .header("Cookie", &self.cookie)
            .header("Referer", "https://pan.quark.cn/")
            .json(&json!({
                "action_type": 2,
                "filelist": [file_id],
                "exclude_fids": [],
            }))
            .send()?
            .error_for_status()?
            .json()?;
        envelope.into_data().map(|_| ())?;
        tracing::info!(
            target: "quarkdrive::api",
            operation = "delete_file",
            endpoint = "/1/clouddrive/file/delete",
            file_id,
            elapsed_ms = started.elapsed().as_millis() as u64,
            returned = 0,
            result = "ok",
            "夸克 API 调用完成"
        );
        Ok(())
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

#[derive(Deserialize)]
struct UploadAuthData {
    auth_key: String,
}

fn ensure_success(value: &serde_json::Value) -> Result<(), QuarkError> {
    let status = value
        .get("status")
        .and_then(|value| value_i64(value, &[]))
        .unwrap_or(200);
    let code = value
        .get("code")
        .and_then(|value| value_i64(value, &[]))
        .unwrap_or(0);
    if status != 200 || code != 0 {
        return Err(QuarkError::Api {
            code: if code != 0 { code } else { status },
            message: value_string(value, &["message"]).unwrap_or_else(|| "未知错误".into()),
        });
    }
    Ok(())
}

fn json_string(value: &serde_json::Value, key: &'static str) -> Result<String, QuarkError> {
    value_string(value, &[key]).ok_or(QuarkError::Malformed(key))
}

fn first_json_string(value: &serde_json::Value, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|path| value_string(value, path))
}

fn value_u64(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn file_hashes(path: &Path) -> Result<(String, String), QuarkError> {
    let mut file = File::open(path).map_err(|err| QuarkError::Login(err.to_string()))?;
    let mut md5 = Md5::new();
    let mut sha1 = Sha1::new();
    let mut buffer = vec![0_u8; 8 * 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        md5.update(&buffer[..count]);
        sha1.update(&buffer[..count]);
    }
    Ok((
        format!("{:x}", md5.finalize()),
        format!("{:x}", sha1.finalize()),
    ))
}

fn oss_endpoint(upload_url: &str, bucket: &str, obj_key: &str) -> String {
    let host = upload_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    format!("https://{bucket}.{host}/{obj_key}")
}

fn http_date() -> String {
    httpdate::fmt_http_date(std::time::SystemTime::now())
}

fn unix_time_ms(value: std::time::SystemTime) -> Option<i64> {
    value
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
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

    #[test]
    fn internal_names_are_hidden_from_sync() {
        assert!(is_internal_name(
            ".quarkdrive-trash-1f6b96db-4f6b-41de-87d3-0d32e2676b0b"
        ));
        assert!(is_internal_name(".quarkdrive-upload-123"));
        assert!(is_internal_name(".quarkdrive-backup-123"));
        assert!(is_internal_name("_quarkdrive_trash_123"));
        assert!(!is_internal_name("我的文件夹"));
    }
}
