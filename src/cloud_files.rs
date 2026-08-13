use std::{
    fs,
    mem::size_of,
    path::{Path, PathBuf},
    ptr,
    sync::Arc,
};

use anyhow::{Context, Result};
use tracing::{error, info};
use windows::{
    Storage::{
        Provider::{
            StorageProviderHardlinkPolicy, StorageProviderHydrationPolicy,
            StorageProviderHydrationPolicyModifier, StorageProviderInSyncPolicy,
            StorageProviderPopulationPolicy, StorageProviderSyncRootInfo,
            StorageProviderSyncRootManager,
        },
        StorageFolder,
    },
    Win32::{
        Foundation::{
            NTSTATUS, STATUS_CLOUD_FILE_UNSUCCESSFUL, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
        },
        Storage::{
            CloudFilters::*,
            FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_BASIC_INFO},
        },
        System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
    },
    core::{GUID, HSTRING, PCWSTR},
};

use crate::{
    config::Config,
    quark::{QuarkClient, RemoteItem},
};

const PROVIDER_ID: GUID = GUID::from_u128(0x7f86d767_7fe2_4cfb_93f8_84c0f279116d);

struct ProviderContext {
    client: Arc<RemoteBackend>,
}

enum RemoteBackend {
    Web(QuarkClient),
}

impl RemoteBackend {
    fn from_config(config: &Config) -> Result<Self> {
        Ok(Self::Web(QuarkClient::new(&config.cookie)?))
    }

    fn list_children(&self, parent_id: &str) -> Result<Vec<RemoteItem>, crate::quark::QuarkError> {
        match self {
            Self::Web(client) => client.list_children(parent_id),
        }
    }

    fn download_range(
        &self,
        file_id: &str,
        range: std::ops::Range<u64>,
    ) -> Result<Vec<u8>, crate::quark::QuarkError> {
        match self {
            Self::Web(client) => client.download_range(file_id, range),
        }
    }

    fn delete_file(&self, file_id: &str) -> Result<(), crate::quark::QuarkError> {
        match self {
            Self::Web(client) => client.delete_file(file_id),
        }
    }
}

pub struct Connection {
    key: CF_CONNECTION_KEY,
    _context: Box<ProviderContext>,
    mount_path: PathBuf,
    com_initialized: bool,
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Err(err) = unsafe { CfDisconnectSyncRoot(self.key) } {
            error!(?err, "断开 Cloud Files 同步根失败");
        }
        if self.com_initialized {
            unsafe { CoUninitialize() };
        }
    }
}

impl Connection {
    pub fn wait(self) -> Result<()> {
        info!(path = %self.mount_path.display(), "夸克网盘已挂载；按 Ctrl+C 停止");
        loop {
            std::thread::park();
        }
    }
}

pub fn register_and_connect(config: &Config) -> Result<Connection> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .context("初始化 Windows Runtime 失败")?;
    fs::create_dir_all(&config.mount_path)
        .with_context(|| format!("无法创建挂载目录 {}", config.mount_path.display()))?;
    register_sync_root(config)?;

    let mut context = Box::new(ProviderContext {
        client: Arc::new(RemoteBackend::from_config(config)?),
    });
    let callbacks = [
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
            Callback: Some(fetch_placeholders),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_DATA,
            Callback: Some(fetch_data),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_DELETE,
            Callback: Some(notify_delete),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NONE,
            Callback: None,
        },
    ];
    let path = wide(&config.mount_path);
    let key = unsafe {
        CfConnectSyncRoot(
            PCWSTR(path.as_ptr()),
            callbacks.as_ptr(),
            Some((&mut *context as *mut ProviderContext).cast()),
            CF_CONNECT_FLAG_NONE,
        )
    }
    .context("无法连接 Windows Cloud Files 同步根")?;
    // Existing mounts may have been populated by an older build that marked
    // every directory as permanently full. Re-enable on-demand population so
    // Explorer Refresh can issue FETCH_PLACEHOLDERS again.
    if let Err(err) = enable_on_demand_population_tree(&config.mount_path) {
        tracing::warn!(?err, "重新启用目录按需填充失败");
    }
    let _ = std::process::Command::new("explorer.exe")
        .arg(&config.mount_path)
        .spawn();
    Ok(Connection {
        key,
        _context: context,
        mount_path: config.mount_path.clone(),
        com_initialized: true,
    })
}

pub fn unregister(path: &Path) -> Result<()> {
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let id = shell_sync_root_id();
    let id = HSTRING::from(id);
    let was_shell_registered =
        StorageProviderSyncRootManager::GetSyncRootInformationForId(&id).is_ok();
    let shell_result = if was_shell_registered {
        StorageProviderSyncRootManager::Unregister(&id)
    } else {
        Ok(())
    };
    let path = wide(path);
    let cfapi_result = unsafe { CfUnregisterSyncRoot(PCWSTR(path.as_ptr())) };
    if initialized {
        unsafe { CoUninitialize() };
    }
    // StorageProviderSyncRootManager::Unregister also removes the underlying
    // CFAPI registration. Consequently the direct CFAPI call normally reports
    // ERROR_NOT_A_CLOUD_SYNC_ROOT; a successful Shell unregister is sufficient.
    if shell_result.is_ok() || cfapi_result.is_ok() {
        Ok(())
    } else {
        shell_result
            .context("从资源管理器注销夸克网盘失败")
            .and_then(|_| cfapi_result.context("注销 Cloud Files 同步根失败"))
    }
}

fn register_sync_root(config: &Config) -> Result<()> {
    register_with_shell(config)?;
    register_cfapi(config)
}

fn register_with_shell(config: &Config) -> Result<()> {
    anyhow::ensure!(
        StorageProviderSyncRootManager::IsSupported().unwrap_or(false),
        "当前 Windows 版本不支持 Cloud Files 同步根"
    );
    let folder = StorageFolder::GetFolderFromPathAsync(&HSTRING::from(
        config.mount_path.to_string_lossy().as_ref(),
    ))?
    .join()
    .context("无法打开挂载目录的 WinRT StorageFolder")?;
    let info = StorageProviderSyncRootInfo::new()?;
    info.SetId(&HSTRING::from(shell_sync_root_id()))?;
    info.SetPath(&folder)?;
    info.SetDisplayNameResource(&HSTRING::from(&config.remote_root_name))?;
    let executable = std::env::current_exe().context("无法确定程序路径")?;
    info.SetIconResource(&HSTRING::from(format!("{},-101", executable.display())))?;
    info.SetHydrationPolicy(StorageProviderHydrationPolicy::Partial)?;
    info.SetHydrationPolicyModifier(
        StorageProviderHydrationPolicyModifier::AutoDehydrationAllowed,
    )?;
    info.SetPopulationPolicy(StorageProviderPopulationPolicy::Full)?;
    info.SetInSyncPolicy(StorageProviderInSyncPolicy::Default)?;
    info.SetHardlinkPolicy(StorageProviderHardlinkPolicy::None)?;
    info.SetShowSiblingsAsGroup(false)?;
    info.SetAllowPinning(true)?;
    info.SetVersion(&HSTRING::from(env!("CARGO_PKG_VERSION")))?;
    info.SetProviderId(PROVIDER_ID)?;
    StorageProviderSyncRootManager::Register(&info).context("向资源管理器注册夸克网盘失败")
}

pub fn registration_status(path: &Path) -> RegistrationStatus {
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let id = HSTRING::from(shell_sync_root_id());
    let shell_registered = StorageProviderSyncRootManager::GetSyncRootInformationForId(&id).is_ok();
    let result = RegistrationStatus {
        shell_registered,
        directory_exists: path.is_dir(),
        sync_root_id: id.to_string(),
    };
    if initialized {
        unsafe { CoUninitialize() };
    }
    result
}

pub fn is_registered_sync_root_path(path: &Path) -> bool {
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let id = HSTRING::from(shell_sync_root_id());
    let matches = StorageProviderSyncRootManager::GetSyncRootInformationForId(&id)
        .ok()
        .and_then(|info| info.Path().ok())
        .and_then(|folder| folder.Path().ok())
        .map(|registered| {
            PathBuf::from(registered.to_string())
                .to_string_lossy()
                .eq_ignore_ascii_case(&path.to_string_lossy())
        })
        .unwrap_or(false);
    if initialized {
        unsafe { CoUninitialize() };
    }
    matches
}

#[derive(Debug)]
pub struct RegistrationStatus {
    pub shell_registered: bool,
    pub directory_exists: bool,
    pub sync_root_id: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalStorageStats {
    pub logical_bytes: u64,
    pub local_bytes: u64,
    pub releasable_bytes: u64,
    pub file_count: u64,
    pub releasable_files: u64,
    pub skipped_files: u64,
}

/// Measures the logical cloud size and the bytes physically present on disk.
/// Directory enumeration can populate placeholder names, but never hydrates
/// file contents. Pinned files are excluded from `releasable_bytes`.
pub fn scan_local_storage(root: &Path) -> Result<LocalStorageStats> {
    let mut stats = LocalStorageStats::default();
    scan_directory(root, &mut stats)?;
    Ok(stats)
}

/// Dehydrates eligible placeholder files while preserving their identities,
/// names, metadata and cloud content. Pinned files and ordinary local files
/// are never changed.
pub fn release_local_storage(root: &Path) -> Result<LocalStorageStats> {
    release_directory(root)?;
    scan_local_storage(root)
}

/// Releases one known file for diagnostics. The target must be a descendant
/// of the configured sync root and is subject to the same placeholder/pin
/// safety checks as the bulk operation.
pub fn release_local_file(root: &Path, target: &Path) -> Result<()> {
    anyhow::ensure!(target.starts_with(root), "目标文件不在夸克挂载目录中");
    release_file(target)
}

fn scan_directory(path: &Path, stats: &mut LocalStorageStats) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("无法扫描 {}", path.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if let Err(err) = scan_directory(&entry.path(), stats) {
                tracing::warn!(?err, path = %entry.path().display(), "跳过无法扫描的目录");
            }
        } else if file_type.is_file() {
            stats.file_count += 1;
            let metadata = entry.metadata()?;
            stats.logical_bytes = stats.logical_bytes.saturating_add(metadata.len());
            match placeholder_storage_info(&entry.path()) {
                Ok(Some(info)) => {
                    let on_disk = info.OnDiskDataSize.max(0) as u64;
                    stats.local_bytes = stats.local_bytes.saturating_add(on_disk);
                    if info.PinState != CF_PIN_STATE_PINNED && on_disk > 0 {
                        stats.releasable_bytes = stats.releasable_bytes.saturating_add(on_disk);
                        stats.releasable_files += 1;
                    }
                }
                Ok(None) | Err(_) => stats.skipped_files += 1,
            }
        }
    }
    Ok(())
}

fn release_directory(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("无法扫描 {}", path.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if let Err(err) = release_directory(&entry.path()) {
                tracing::warn!(?err, path = %entry.path().display(), "跳过无法释放的目录");
            }
        } else if file_type.is_file()
            && let Err(err) = release_file(&entry.path())
        {
            tracing::warn!(?err, path = %entry.path().display(), "文件正在使用或无法释放");
        }
    }
    Ok(())
}

fn release_file(path: &Path) -> Result<()> {
    let Some(info) = placeholder_storage_info(path)? else {
        return Ok(());
    };
    if info.PinState == CF_PIN_STATE_PINNED || info.OnDiskDataSize <= 0 {
        return Ok(());
    }
    let path_wide = wide(path);
    let handle = unsafe {
        CfOpenFileWithOplock(
            PCWSTR(path_wide.as_ptr()),
            CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_WRITE_ACCESS,
        )
    }
    .context("无法取得文件独占锁")?;
    let result =
        unsafe { CfDehydratePlaceholder(handle, 0, -1, CF_DEHYDRATE_FLAG_BACKGROUND, None) };
    unsafe { CfCloseHandle(handle) };
    result.context("无法释放文件内容")
}

fn placeholder_storage_info(path: &Path) -> Result<Option<CF_PLACEHOLDER_STANDARD_INFO>> {
    let path_wide = wide(path);
    let handle =
        match unsafe { CfOpenFileWithOplock(PCWSTR(path_wide.as_ptr()), CF_OPEN_FILE_FLAG_NONE) } {
            Ok(handle) => handle,
            Err(_) => return Ok(None),
        };
    // STANDARD_INFO is followed by the variable-length FileIdentity blob.
    // Supplying only size_of::<CF_PLACEHOLDER_STANDARD_INFO>() makes CFAPI
    // return ERROR_MORE_DATA for every real placeholder with an identity.
    let buffer_bytes = size_of::<CF_PLACEHOLDER_STANDARD_INFO>()
        + CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH as usize;
    let mut buffer = vec![0_u64; buffer_bytes.div_ceil(size_of::<u64>())];
    let result = unsafe {
        CfGetPlaceholderInfo(
            handle,
            CF_PLACEHOLDER_INFO_STANDARD,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * size_of::<u64>()) as u32,
            None,
        )
    };
    unsafe { CfCloseHandle(handle) };
    match result {
        Ok(()) => Ok(Some(unsafe {
            buffer
                .as_ptr()
                .cast::<CF_PLACEHOLDER_STANDARD_INFO>()
                .read()
        })),
        Err(_) => Ok(None),
    }
}

fn shell_sync_root_id() -> String {
    "QuarkDrive!Personal".to_string()
}

fn register_cfapi(config: &Config) -> Result<()> {
    let path = wide(&config.mount_path);
    let provider_name = wide_str("Quark Drive for Windows");
    let provider_version = wide_str(env!("CARGO_PKG_VERSION"));
    let sync_identity = b"quarkdrive-windows-v1";
    let root_identity = config.remote_root_id.as_bytes();
    let registration = CF_SYNC_REGISTRATION {
        StructSize: size_of::<CF_SYNC_REGISTRATION>() as u32,
        ProviderName: PCWSTR(provider_name.as_ptr()),
        ProviderVersion: PCWSTR(provider_version.as_ptr()),
        SyncRootIdentity: sync_identity.as_ptr().cast(),
        SyncRootIdentityLength: sync_identity.len() as u32,
        FileIdentity: root_identity.as_ptr().cast(),
        FileIdentityLength: root_identity.len() as u32,
        ProviderId: PROVIDER_ID,
    };
    let policies = CF_SYNC_POLICIES {
        StructSize: size_of::<CF_SYNC_POLICIES>() as u32,
        Hydration: CF_HYDRATION_POLICY {
            Primary: CF_HYDRATION_POLICY_PARTIAL,
            Modifier: CF_HYDRATION_POLICY_MODIFIER_AUTO_DEHYDRATION_ALLOWED,
        },
        Population: CF_POPULATION_POLICY {
            Primary: CF_POPULATION_POLICY_FULL,
            Modifier: CF_POPULATION_POLICY_MODIFIER_NONE,
        },
        InSync: CF_INSYNC_POLICY_NONE,
        HardLink: CF_HARDLINK_POLICY_NONE,
        PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
    };
    unsafe {
        CfRegisterSyncRoot(
            PCWSTR(path.as_ptr()),
            &registration,
            &policies,
            CF_REGISTER_FLAG_UPDATE,
        )
    }
    .context("注册 Windows Cloud Files 同步根失败")
}

unsafe extern "system" fn fetch_placeholders(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    let result = unsafe { do_fetch_placeholders(&*info) };
    if let Err(err) = result {
        error!(?err, "加载远端目录失败");
        unsafe { complete_placeholders(&*info, &mut [], STATUS_UNSUCCESSFUL) };
    }
}

unsafe fn do_fetch_placeholders(info: &CF_CALLBACK_INFO) -> Result<()> {
    let context = unsafe { &*(info.CallbackContext as *const ProviderContext) };
    let parent_id = unsafe { identity(info) }?;
    let items = context.client.list_children(&parent_id)?;
    let names: Vec<Vec<u16>> = items
        .iter()
        .map(|item| wide_str(&windows_name(&item.name)))
        .collect();
    let identities: Vec<&[u8]> = items.iter().map(|item| item.id.as_bytes()).collect();
    let mut placeholders: Vec<CF_PLACEHOLDER_CREATE_INFO> = items
        .iter()
        .enumerate()
        .map(|(index, item)| CF_PLACEHOLDER_CREATE_INFO {
            RelativeFileName: PCWSTR(names[index].as_ptr()),
            FsMetadata: metadata(item),
            FileIdentity: identities[index].as_ptr().cast(),
            FileIdentityLength: identities[index].len() as u32,
            // Supersede metadata on a repeated population request so an
            // Explorer refresh can observe remote renames/size changes too.
            Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC | CF_PLACEHOLDER_CREATE_FLAG_SUPERSEDE,
            ..Default::default()
        })
        .collect();
    unsafe { complete_placeholders(info, &mut placeholders, STATUS_SUCCESS) };
    if let Err(err) = unsafe { enable_on_demand_population_for_info(info) } {
        tracing::warn!(?err, "重新启用当前目录按需填充失败");
    }
    Ok(())
}

unsafe fn complete_placeholders(
    info: &CF_CALLBACK_INFO,
    placeholders: &mut [CF_PLACEHOLDER_CREATE_INFO],
    status: NTSTATUS,
) {
    let op_info = operation_info(info, CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS);
    let transfer = CF_OPERATION_PARAMETERS_0_4 {
        // The directory is fully listed in one response. Re-enable on-demand
        // population after the transfer so a later Explorer Refresh can ask
        // for a fresh remote listing without causing repeated callbacks.
        Flags: CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION,
        CompletionStatus: status,
        PlaceholderTotalCount: placeholders.len() as i64,
        PlaceholderArray: placeholders.as_mut_ptr(),
        PlaceholderCount: placeholders.len() as u32,
        EntriesProcessed: 0,
    };
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: size_of::<CF_OPERATION_PARAMETERS>() as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferPlaceholders: transfer,
        },
    };
    if let Err(err) = unsafe { CfExecute(&op_info, &mut params) } {
        error!(?err, "提交占位文件失败");
    }
}

unsafe fn enable_on_demand_population_for_info(info: &CF_CALLBACK_INFO) -> Result<()> {
    anyhow::ensure!(!info.NormalizedPath.is_null(), "目录回调缺少标准化路径");
    let path = unsafe { info.NormalizedPath.to_string() }.context("无法读取目录回调路径")?;
    enable_on_demand_population(Path::new(&path))
}

fn enable_on_demand_population_tree(root: &Path) -> Result<()> {
    enable_on_demand_population(root)?;
    let entries =
        fs::read_dir(root).with_context(|| format!("无法遍历挂载目录 {}", root.display()))?;
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Err(err) = enable_on_demand_population_tree(&entry.path())
        {
            tracing::debug!(?err, path = %entry.path().display(), "跳过无法更新的子目录");
        }
    }
    Ok(())
}

fn enable_on_demand_population(path: &Path) -> Result<()> {
    let path_wide = wide(path);
    let handle =
        unsafe { CfOpenFileWithOplock(PCWSTR(path_wide.as_ptr()), CF_OPEN_FILE_FLAG_NONE) }
            .with_context(|| format!("无法打开目录占位符 {}", path.display()))?;
    let result = unsafe {
        CfUpdatePlaceholder(
            handle,
            None,
            None,
            0,
            None,
            CF_UPDATE_FLAG_ENABLE_ON_DEMAND_POPULATION,
            None,
            None,
        )
    };
    unsafe { CfCloseHandle(handle) };
    result.with_context(|| format!("无法启用目录按需填充 {}", path.display()))
}

unsafe extern "system" fn notify_delete(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    let result = unsafe { do_notify_delete(&*info, &*params) };
    let status = match result {
        Ok(()) => STATUS_SUCCESS,
        Err(err) => {
            error!(?err, "删除夸克网盘远端文件失败");
            STATUS_CLOUD_FILE_UNSUCCESSFUL
        }
    };
    unsafe { complete_delete(&*info, status) };
}

unsafe fn do_notify_delete(info: &CF_CALLBACK_INFO, params: &CF_CALLBACK_PARAMETERS) -> Result<()> {
    let flags = unsafe { params.Anonymous.Delete.Flags };
    anyhow::ensure!(
        !flags.contains(CF_CALLBACK_DELETE_FLAG_IS_UNDELETE),
        "夸克网盘暂不支持从资源管理器撤销删除"
    );
    let context = unsafe { &*(info.CallbackContext as *const ProviderContext) };
    let file_id = unsafe { identity(info) }?;
    context.client.delete_file(&file_id)?;
    Ok(())
}

unsafe fn complete_delete(info: &CF_CALLBACK_INFO, status: NTSTATUS) {
    let op_info = operation_info(info, CF_OPERATION_TYPE_ACK_DELETE);
    let ack = CF_OPERATION_PARAMETERS_0_7 {
        Flags: CF_OPERATION_ACK_DELETE_FLAG_NONE,
        CompletionStatus: status,
    };
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: size_of::<CF_OPERATION_PARAMETERS>() as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 { AckDelete: ack },
    };
    if let Err(err) = unsafe { CfExecute(&op_info, &mut params) } {
        error!(?err, "确认夸克网盘删除操作失败");
    }
}

unsafe extern "system" fn fetch_data(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    let result = unsafe { do_fetch_data(&*info, &*params) };
    if let Err(err) = result {
        error!(?err, "下载文件数据失败");
        unsafe { complete_data(&*info, &[], 0, STATUS_UNSUCCESSFUL) };
    }
}

unsafe fn do_fetch_data(info: &CF_CALLBACK_INFO, params: &CF_CALLBACK_PARAMETERS) -> Result<()> {
    let context = unsafe { &*(info.CallbackContext as *const ProviderContext) };
    let file_id = unsafe { identity(info) }?;
    let fetch = unsafe { params.Anonymous.FetchData };
    let start = u64::try_from(fetch.RequiredFileOffset).context("无效的数据偏移")?;
    let length = u64::try_from(fetch.RequiredLength).context("无效的数据长度")?;
    let data = context
        .client
        .download_range(&file_id, start..start.saturating_add(length))?;
    unsafe { complete_data(info, &data, fetch.RequiredFileOffset, STATUS_SUCCESS) };
    Ok(())
}

unsafe fn complete_data(info: &CF_CALLBACK_INFO, data: &[u8], offset: i64, status: NTSTATUS) {
    let op_info = operation_info(info, CF_OPERATION_TYPE_TRANSFER_DATA);
    let transfer = CF_OPERATION_PARAMETERS_0_0 {
        Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
        CompletionStatus: status,
        Buffer: if data.is_empty() {
            ptr::null()
        } else {
            data.as_ptr().cast()
        },
        Offset: offset,
        Length: data.len() as i64,
    };
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: size_of::<CF_OPERATION_PARAMETERS>() as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferData: transfer,
        },
    };
    if let Err(err) = unsafe { CfExecute(&op_info, &mut params) } {
        error!(?err, "提交文件数据失败");
    }
}

fn operation_info(info: &CF_CALLBACK_INFO, kind: CF_OPERATION_TYPE) -> CF_OPERATION_INFO {
    CF_OPERATION_INFO {
        StructSize: size_of::<CF_OPERATION_INFO>() as u32,
        Type: kind,
        ConnectionKey: info.ConnectionKey,
        TransferKey: info.TransferKey,
        CorrelationVector: ptr::null(),
        SyncStatus: ptr::null(),
        RequestKey: info.RequestKey,
    }
}

unsafe fn identity(info: &CF_CALLBACK_INFO) -> Result<String> {
    anyhow::ensure!(
        !info.FileIdentity.is_null() && info.FileIdentityLength > 0,
        "占位文件没有远端 ID"
    );
    let bytes = unsafe {
        std::slice::from_raw_parts(
            info.FileIdentity.cast::<u8>(),
            info.FileIdentityLength as usize,
        )
    };
    String::from_utf8(bytes.to_vec()).context("远端 ID 不是 UTF-8")
}

fn metadata(item: &RemoteItem) -> CF_FS_METADATA {
    let attributes = if item.is_directory {
        FILE_ATTRIBUTE_DIRECTORY.0
    } else {
        FILE_ATTRIBUTE_NORMAL.0
    };
    CF_FS_METADATA {
        BasicInfo: FILE_BASIC_INFO {
            CreationTime: unix_ms_to_filetime(item.created_at_ms),
            LastAccessTime: unix_ms_to_filetime(item.modified_at_ms),
            LastWriteTime: unix_ms_to_filetime(item.modified_at_ms),
            ChangeTime: unix_ms_to_filetime(item.modified_at_ms),
            FileAttributes: attributes,
        },
        FileSize: item.size.min(i64::MAX as u64) as i64,
    }
}

fn unix_ms_to_filetime(ms: i64) -> i64 {
    ms.saturating_mul(10_000)
        .saturating_add(116_444_736_000_000_000)
}

fn windows_name(name: &str) -> String {
    let invalid = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let mut value: String = name
        .chars()
        .map(|c| {
            if invalid.contains(&c) || c < ' ' {
                '＿'
            } else {
                c
            }
        })
        .collect();
    while value.ends_with([' ', '.']) {
        value.pop();
    }
    if value.is_empty() {
        value = "未命名".into();
    }
    let stem = value
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        value.insert(0, '＿');
    }
    value
}

fn wide(path: &Path) -> Vec<u16> {
    wide_str(&path.to_string_lossy())
}
fn wide_str(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sanitizes_windows_names() {
        assert_eq!(windows_name("CON.txt"), "＿CON.txt");
        assert_eq!(windows_name("a:b. "), "a＿b");
    }
    #[test]
    fn converts_epoch() {
        assert_eq!(unix_ms_to_filetime(0), 116_444_736_000_000_000);
    }
}
