use std::{
    fs,
    mem::size_of,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HINSTANCE, HWND, LPARAM,
            LRESULT, POINT, WPARAM,
        },
        Graphics::Gdi::{
            BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreateSolidBrush,
            DEFAULT_CHARSET, DEFAULT_PITCH, DeleteObject, EndPaint, FF_DONTCARE, FW_NORMAL,
            FillRect, GetStockObject, InvalidateRect, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SetBkMode,
            TRANSPARENT, WHITE_BRUSH,
        },
        System::{LibraryLoader::GetModuleHandleW, Threading::CreateMutexW},
        UI::{
            Input::KeyboardAndMouse::EnableWindow,
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, w},
};
use winreg::{RegKey, enums::HKEY_CURRENT_USER};

use crate::{
    cloud_files::{
        Connection, LocalStorageEntry, LocalStorageStats, release_local_paths, scan_local_storage,
        scan_local_storage_entries,
    },
    config::Config,
    logging,
    quark::{QrCodeData, QrLoginResult, QuarkClient, QuarkQrLogin, is_internal_name},
};

const WM_TRAY: u32 = WM_APP + 1;
const WM_STORAGE_DONE: u32 = WM_APP + 3;
const WM_QR_READY: u32 = WM_APP + 4;
const WM_LOGIN_DONE: u32 = WM_APP + 5;
const ID_OPEN: usize = 1001;
const ID_SETTINGS: usize = 1002;
const ID_EXIT: usize = 1003;
const ID_PATH: i32 = 2001;
const ID_NAME: i32 = 2002;
const ID_STARTUP: i32 = 2003;
const ID_SAVE: usize = 2004;
const ID_CANCEL: usize = 2005;
const ID_OPEN_FOLDER: usize = 2006;
const ID_REMOTE_ROOT: i32 = 2008;
const ID_STORAGE_SCAN: usize = 2009;
const ID_STORAGE_RELEASE: usize = 2010;
const ID_LOGIN: usize = 2011;
const ID_QR_CANCEL: usize = 2012;
const ID_TAB_SETTINGS: usize = 2013;
const ID_TAB_LOG: usize = 2014;
const ID_LOG_REFRESH: usize = 2015;
const ID_LOG_CLEAR: usize = 2016;
const ICON_ID: u16 = 101;
const SINGLE_INSTANCE_NAME: PCWSTR = w!("Local\\QuarkDriveWindows.SingleInstance");

struct SingleInstance(HANDLE);

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct AppState {
    config: Config,
    config_path: PathBuf,
    connection: Option<Connection>,
    path_edit: HWND,
    name_edit: HWND,
    account_status: HWND,
    account_detail: HWND,
    login_button: HWND,
    qr_dialog: HWND,
    startup_check: HWND,
    root_combo: HWND,
    storage_status: HWND,
    storage_detail: HWND,
    storage_scan_button: HWND,
    storage_release_button: HWND,
    storage_list: HWND,
    log_edit: HWND,
    log_refresh_button: HWND,
    log_clear_button: HWND,
    open_button: HWND,
    save_button: HWND,
    cancel_button: HWND,
    settings_controls: Vec<HWND>,
    storage_stats: LocalStorageStats,
    storage_entries: Vec<LocalStorageEntry>,
    remote_roots: Vec<(String, String)>,
}

#[derive(Clone)]
enum StorageTask {
    Scan,
    Release {
        selected: Vec<PathBuf>,
        expected_bytes: u64,
    },
}

struct StorageTaskResult {
    task: StorageTask,
    result: std::result::Result<(LocalStorageStats, Vec<LocalStorageEntry>), String>,
}

struct LoginDone {
    result: std::result::Result<QrLoginResult, String>,
}

struct QrLoginState {
    status: HWND,
    qr_view: HWND,
    qr: Option<QrCodeData>,
    cancel_button: HWND,
    cancelled: Arc<AtomicBool>,
}

pub fn run(config: Config, config_path: PathBuf, connection: Option<Connection>) -> Result<()> {
    let remote_roots = load_remote_roots(&config);
    let show_on_start = connection.is_none();
    unsafe {
        let Some(_single_instance) = acquire_single_instance()? else {
            tracing::warn!("夸克网盘挂载器已在运行，忽略重复启动");
            return Ok(());
        };
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let icon = LoadIconW(Some(instance), PCWSTR(ICON_ID as usize as *const u16))?;
        let class_name = w!("QuarkDriveWindow");
        let class = WNDCLASSW {
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: icon,
            hInstance: instance,
            lpszClassName: class_name,
            lpfnWndProc: Some(window_proc),
            // COLOR_WINDOW (5) plus one is the conventional system-color brush.
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(6 as *mut _),
            ..Default::default()
        };
        anyhow::ensure!(RegisterClassW(&class) != 0, "无法注册配置窗口");

        let state = Box::new(AppState {
            config,
            config_path,
            connection,
            path_edit: HWND::default(),
            name_edit: HWND::default(),
            account_status: HWND::default(),
            account_detail: HWND::default(),
            login_button: HWND::default(),
            qr_dialog: HWND::default(),
            startup_check: HWND::default(),
            root_combo: HWND::default(),
            storage_status: HWND::default(),
            storage_detail: HWND::default(),
            storage_scan_button: HWND::default(),
            storage_release_button: HWND::default(),
            storage_list: HWND::default(),
            log_edit: HWND::default(),
            log_refresh_button: HWND::default(),
            log_clear_button: HWND::default(),
            open_button: HWND::default(),
            save_button: HWND::default(),
            cancel_button: HWND::default(),
            settings_controls: Vec::new(),
            storage_stats: LocalStorageStats::default(),
            storage_entries: Vec::new(),
            remote_roots,
        });
        let state_ptr = Box::into_raw(state);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("夸克网盘设置"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            720,
            862,
            None,
            None,
            Some(instance),
            Some(state_ptr.cast()),
        )?;
        add_tray_icon(hwnd, icon, !show_on_start)?;
        if show_on_start {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            if config_needs_login(state_ptr) {
                begin_qr_login(hwnd, &mut *state_ptr);
            }
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = Box::from_raw(state_ptr);
        Ok(())
    }
}

unsafe fn acquire_single_instance() -> Result<Option<SingleInstance>> {
    let mutex = unsafe { CreateMutexW(None, true, SINGLE_INSTANCE_NAME)? };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        let _ = unsafe { CloseHandle(mutex) };
        return Ok(None);
    }
    Ok(Some(SingleInstance(mutex)))
}

fn config_needs_login(state_ptr: *mut AppState) -> bool {
    unsafe { (*state_ptr).config.cookie.trim().is_empty() }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
    }
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AppState };
    match msg {
        WM_CREATE if !state_ptr.is_null() => {
            if let Err(err) = unsafe { create_controls(hwnd, &mut *state_ptr) } {
                unsafe { show_error(hwnd, &err.to_string()) };
                return LRESULT(-1);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
            LRESULT(0)
        }
        WM_COMMAND if !state_ptr.is_null() => {
            let id = wparam.0 & 0xffff;
            unsafe { handle_command(hwnd, &mut *state_ptr, id) };
            LRESULT(0)
        }
        WM_TRAY => {
            let event = lparam.0 as u32;
            if event == WM_LBUTTONDBLCLK {
                unsafe { show_settings(hwnd) };
            } else if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                unsafe { show_tray_menu(hwnd) };
            }
            LRESULT(0)
        }
        WM_STORAGE_DONE if !state_ptr.is_null() => {
            let state = unsafe { &mut *state_ptr };
            let result = unsafe { Box::from_raw(lparam.0 as *mut StorageTaskResult) };
            unsafe { finish_storage_task(hwnd, state, *result) };
            LRESULT(0)
        }
        WM_LOGIN_DONE if !state_ptr.is_null() => {
            let state = unsafe { &mut *state_ptr };
            let result = unsafe { Box::from_raw(lparam.0 as *mut LoginDone) };
            unsafe { finish_login(hwnd, state, result.result) };
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut _);
            unsafe { SetBkMode(hdc, TRANSPARENT) };
            LRESULT(unsafe { GetStockObject(WHITE_BRUSH).0 as isize })
        }
        WM_DESTROY => {
            unsafe { remove_tray_icon(hwnd) };
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn begin_qr_login(hwnd: HWND, state: &mut AppState) {
    if !state.qr_dialog.is_invalid() {
        let _ = unsafe { ShowWindow(state.qr_dialog, SW_RESTORE) };
        let _ = unsafe { SetForegroundWindow(state.qr_dialog) };
        return;
    }
    let _ = unsafe { EnableWindow(state.login_button, false) };
    let cancelled = Arc::new(AtomicBool::new(false));
    let dialog_state = Box::new(QrLoginState {
        status: HWND::default(),
        qr_view: HWND::default(),
        qr: None,
        cancel_button: HWND::default(),
        cancelled: cancelled.clone(),
    });
    let instance: HINSTANCE = match unsafe { GetModuleHandleW(None) } {
        Ok(value) => value.into(),
        Err(err) => {
            let _ = unsafe { EnableWindow(state.login_button, true) };
            unsafe { show_error(hwnd, &format!("无法打开二维码登录：{err}")) };
            return;
        }
    };
    unsafe { register_qr_window_class(instance) };
    let dialog_state_ptr = Box::into_raw(dialog_state);
    let dialog = unsafe {
        CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            w!("QuarkDriveQrLogin"),
            w!("二维码登录夸克网盘"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            720,
            820,
            Some(hwnd),
            None,
            Some(instance),
            Some(dialog_state_ptr.cast()),
        )
    };
    let dialog = match dialog {
        Ok(value) if !value.is_invalid() => value,
        _ => {
            unsafe { drop(Box::from_raw(dialog_state_ptr)) };
            let _ = unsafe { EnableWindow(state.login_button, true) };
            unsafe { show_error(hwnd, "无法打开二维码登录窗口") };
            return;
        }
    };
    state.qr_dialog = dialog;
    let _ = unsafe { ShowWindow(dialog, SW_SHOW) };
    let _ = unsafe { SetForegroundWindow(dialog) };
    let parent_value = hwnd.0 as isize;
    let dialog_value = dialog.0 as isize;
    std::thread::spawn(move || {
        let result = (|| {
            let login = QuarkQrLogin::new().map_err(|err| err.to_string())?;
            let qr = login.get_qr_code().map_err(|err| err.to_string())?;
            let token = qr.token.clone();
            let qr_payload = Box::new(qr);
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(dialog_value as *mut _)),
                    WM_QR_READY,
                    WPARAM(0),
                    LPARAM(Box::into_raw(qr_payload) as isize),
                )
            };
            login
                .wait_for_login(&token, Duration::from_secs(300), || {
                    cancelled.load(Ordering::Relaxed)
                })
                .map_err(|err| err.to_string())
        })();
        let payload = Box::new(LoginDone { result });
        let _ = unsafe {
            PostMessageW(
                Some(HWND(parent_value as *mut _)),
                WM_LOGIN_DONE,
                WPARAM(0),
                LPARAM(Box::into_raw(payload) as isize),
            )
        };
    });
}

unsafe fn create_controls(hwnd: HWND, state: &mut AppState) -> Result<()> {
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None)? }.into();
    unsafe {
        create(
            hwnd,
            instance,
            w!("STATIC"),
            "夸克网盘",
            28,
            20,
            210,
            30,
            0,
            0,
        )?;
        create(
            hwnd,
            instance,
            w!("BUTTON"),
            "设置",
            494,
            20,
            78,
            30,
            BS_PUSHBUTTON as u32,
            ID_TAB_SETTINGS,
        )?;
        create(
            hwnd,
            instance,
            w!("BUTTON"),
            "日志",
            580,
            20,
            78,
            30,
            BS_PUSHBUTTON as u32,
            ID_TAB_LOG,
        )?;
        create(
            hwnd,
            instance,
            w!("STATIC"),
            "挂载与账号设置",
            28,
            51,
            240,
            22,
            0,
            0,
        )?;

        create(
            hwnd,
            instance,
            w!("BUTTON"),
            "夸克账号",
            24,
            88,
            654,
            112,
            BS_GROUPBOX as u32,
            0,
        )?;
        let icon_view = create(hwnd, instance, w!("STATIC"), "", 48, 120, 48, 48, 3, 0)?; // SS_ICON
        let account_icon = LoadIconW(Some(instance), PCWSTR(ICON_ID as usize as *const u16))?;
        SendMessageW(
            icon_view,
            STM_SETICON,
            Some(WPARAM(account_icon.0 as usize)),
            Some(LPARAM(0)),
        );
        let account_text = if state.config.cookie.trim().is_empty() {
            "未登录夸克网盘"
        } else if state.config.account_nickname.trim().is_empty() {
            "已登录夸克网盘"
        } else {
            &state.config.account_nickname
        };
        state.account_status = create(
            hwnd,
            instance,
            w!("STATIC"),
            account_text,
            112,
            120,
            400,
            24,
            0,
            0,
        )?;
        let account_detail = if state.config.cookie.trim().is_empty() {
            "使用夸克网盘 APP 扫码登录，登录信息会自动保存".to_string()
        } else if state.config.account_id.trim().is_empty() {
            "已保存登录会话，可重新扫码切换账号".to_string()
        } else {
            format!("账号 ID：{} · 登录会话已保存", state.config.account_id)
        };
        state.account_detail = create(
            hwnd,
            instance,
            w!("STATIC"),
            &account_detail,
            112,
            148,
            420,
            22,
            0,
            0,
        )?;
        state.login_button = create(
            hwnd,
            instance,
            w!("BUTTON"),
            "二维码登录",
            542,
            130,
            108,
            34,
            BS_PUSHBUTTON as u32,
            ID_LOGIN,
        )?;

        create(
            hwnd,
            instance,
            w!("BUTTON"),
            "云端挂载范围",
            24,
            214,
            654,
            104,
            BS_GROUPBOX as u32,
            0,
        )?;
        create(
            hwnd,
            instance,
            w!("STATIC"),
            "云盘根目录",
            48,
            250,
            100,
            24,
            0,
            0,
        )?;
        state.root_combo = create(
            hwnd,
            instance,
            w!("COMBOBOX"),
            "",
            164,
            246,
            486,
            180,
            WS_BORDER.0 | CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0,
            ID_REMOTE_ROOT as usize,
        )?;
        let mut selected = 0;
        for (index, (_, label)) in state.remote_roots.iter().enumerate() {
            let label = wide(label);
            SendMessageW(
                state.root_combo,
                CB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(label.as_ptr() as isize)),
            );
            if state.remote_roots[index].0 == state.config.remote_root_id {
                selected = index;
            }
        }
        SendMessageW(
            state.root_combo,
            CB_SETCURSEL,
            Some(WPARAM(selected)),
            Some(LPARAM(0)),
        );

        create(
            hwnd,
            instance,
            w!("BUTTON"),
            "Windows 挂载",
            24,
            332,
            654,
            168,
            BS_GROUPBOX as u32,
            0,
        )?;
        create(
            hwnd,
            instance,
            w!("STATIC"),
            "挂载位置",
            48,
            370,
            100,
            24,
            0,
            0,
        )?;
        state.path_edit = create(
            hwnd,
            instance,
            w!("EDIT"),
            &state.config.mount_path.to_string_lossy(),
            164,
            366,
            486,
            27,
            WS_BORDER.0,
            ID_PATH as usize,
        )?;
        create(
            hwnd,
            instance,
            w!("STATIC"),
            "显示名称",
            48,
            414,
            100,
            24,
            0,
            0,
        )?;
        state.name_edit = create(
            hwnd,
            instance,
            w!("EDIT"),
            &state.config.remote_root_name,
            164,
            410,
            486,
            27,
            WS_BORDER.0,
            ID_NAME as usize,
        )?;
        state.startup_check = create(
            hwnd,
            instance,
            w!("BUTTON"),
            "登录 Windows 后自动启动",
            164,
            452,
            250,
            28,
            BS_AUTOCHECKBOX as u32,
            ID_STARTUP as usize,
        )?;
        if state.config.start_on_login {
            SendMessageW(
                state.startup_check,
                BM_SETCHECK,
                Some(WPARAM(1)), // BST_CHECKED
                Some(LPARAM(0)),
            );
        }
        create(
            hwnd,
            instance,
            w!("BUTTON"),
            "本地空间",
            24,
            514,
            654,
            236,
            BS_GROUPBOX as u32,
            0,
        )?;
        state.storage_status = create(
            hwnd,
            instance,
            w!("STATIC"),
            "正在检测本地文件容量…",
            48,
            548,
            600,
            24,
            0,
            0,
        )?;
        state.storage_detail = create(
            hwnd,
            instance,
            w!("STATIC"),
            "仅统计已下载到本机的网盘文件",
            48,
            704,
            390,
            22,
            0,
            0,
        )?;
        state.storage_list = create(
            hwnd,
            instance,
            w!("LISTBOX"),
            "",
            48,
            578,
            602,
            116,
            WS_BORDER.0 | WS_VSCROLL.0 | LBS_EXTENDEDSEL as u32 | LBS_NOINTEGRALHEIGHT as u32,
            0,
        )?;
        state.storage_scan_button = create(
            hwnd,
            instance,
            w!("BUTTON"),
            "重新检测",
            48,
            738,
            94,
            34,
            BS_PUSHBUTTON as u32,
            ID_STORAGE_SCAN,
        )?;
        state.storage_release_button = create(
            hwnd,
            instance,
            w!("BUTTON"),
            "释放空间",
            154,
            738,
            100,
            34,
            BS_PUSHBUTTON as u32,
            ID_STORAGE_RELEASE,
        )?;
        let _ = EnableWindow(state.storage_release_button, false);
        state.open_button = create(
            hwnd,
            instance,
            w!("BUTTON"),
            "打开网盘",
            24,
            774,
            100,
            32,
            BS_PUSHBUTTON as u32,
            ID_OPEN_FOLDER,
        )?;
        state.save_button = create(
            hwnd,
            instance,
            w!("BUTTON"),
            "保存并重启",
            436,
            774,
            112,
            32,
            BS_DEFPUSHBUTTON as u32,
            ID_SAVE,
        )?;
        state.cancel_button = create(
            hwnd,
            instance,
            w!("BUTTON"),
            "取消",
            562,
            774,
            100,
            32,
            BS_PUSHBUTTON as u32,
            ID_CANCEL,
        )?;
        state.log_edit = create(
            hwnd,
            instance,
            w!("EDIT"),
            "",
            24,
            88,
            654,
            654,
            WS_BORDER.0
                | WS_VSCROLL.0
                | WS_HSCROLL.0
                | ES_MULTILINE as u32
                | ES_AUTOVSCROLL as u32
                | ES_AUTOHSCROLL as u32
                | ES_READONLY as u32,
            0,
        )?;
        state.log_refresh_button = create(
            hwnd,
            instance,
            w!("BUTTON"),
            "刷新日志",
            436,
            774,
            112,
            32,
            BS_PUSHBUTTON as u32,
            ID_LOG_REFRESH,
        )?;
        state.log_clear_button = create(
            hwnd,
            instance,
            w!("BUTTON"),
            "清理日志",
            562,
            774,
            100,
            32,
            BS_PUSHBUTTON as u32,
            ID_LOG_CLEAR,
        )?;
        state.settings_controls = vec![
            state.account_status,
            state.account_detail,
            state.login_button,
            state.root_combo,
            state.path_edit,
            state.name_edit,
            state.startup_check,
            state.storage_status,
            state.storage_detail,
            state.storage_list,
            state.storage_scan_button,
            state.storage_release_button,
            state.open_button,
            state.save_button,
            state.cancel_button,
        ];
        let _ = ShowWindow(state.log_edit, SW_HIDE);
        let _ = ShowWindow(state.log_refresh_button, SW_HIDE);
        let _ = ShowWindow(state.log_clear_button, SW_HIDE);
        begin_storage_task(hwnd, state, StorageTask::Scan);
    }
    Ok(())
}

unsafe fn register_qr_window_class(instance: HINSTANCE) {
    let class = WNDCLASSW {
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
        hInstance: instance,
        lpszClassName: w!("QuarkDriveQrLogin"),
        lpfnWndProc: Some(qr_window_proc),
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(6 as *mut _),
        ..Default::default()
    };
    let _ = unsafe { RegisterClassW(&class) };
}

unsafe extern "system" fn qr_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
    }
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut QrLoginState };
    match msg {
        WM_CREATE if !state_ptr.is_null() => {
            let state = unsafe { &mut *state_ptr };
            if let Err(err) = unsafe { create_qr_controls(hwnd, state) } {
                unsafe { show_error(hwnd, &err.to_string()) };
                return LRESULT(-1);
            }
            LRESULT(0)
        }
        WM_QR_READY if !state_ptr.is_null() => {
            let state = unsafe { &mut *state_ptr };
            let qr = unsafe { Box::from_raw(lparam.0 as *mut QrCodeData) };
            state.qr = Some(*qr);
            let _ = unsafe { InvalidateRect(Some(hwnd), None, true) };
            let status = wide("请使用夸克网盘 APP 扫码登录，登录成功后会自动获取账号信息");
            let _ = unsafe { SetWindowTextW(state.status, PCWSTR(status.as_ptr())) };
            LRESULT(0)
        }
        WM_COMMAND if !state_ptr.is_null() && (wparam.0 & 0xffff) == ID_QR_CANCEL => {
            let state = unsafe { &*state_ptr };
            state.cancelled.store(true, Ordering::Relaxed);
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_PAINT if !state_ptr.is_null() => {
            let state = unsafe { &*state_ptr };
            unsafe { paint_qr(hwnd, state) };
            LRESULT(0)
        }
        WM_CLOSE if !state_ptr.is_null() => {
            let state = unsafe { &*state_ptr };
            state.cancelled.store(true, Ordering::Relaxed);
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            if !state_ptr.is_null() {
                unsafe { drop(Box::from_raw(state_ptr)) };
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn create_qr_controls(hwnd: HWND, state: &mut QrLoginState) -> Result<()> {
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None)? }.into();
    state.status = unsafe {
        create(
            hwnd,
            instance,
            w!("STATIC"),
            "正在获取二维码…",
            44,
            24,
            610,
            48,
            0,
            0,
        )?
    };
    // The dialog itself paints the QR bitmap. A text/static control would
    // distort the modules through font metrics or DPI scaling.
    state.qr_view = hwnd;
    state.cancel_button = unsafe {
        create(
            hwnd,
            instance,
            w!("BUTTON"),
            "取消登录",
            300,
            730,
            120,
            34,
            BS_PUSHBUTTON as u32,
            ID_QR_CANCEL,
        )?
    };
    Ok(())
}

unsafe fn paint_qr(hwnd: HWND, state: &QrLoginState) {
    let mut paint = PAINTSTRUCT::default();
    let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
    if hdc.is_invalid() {
        return;
    }
    let white = unsafe { CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x00FFFFFF)) };
    let black = unsafe { CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x00000000)) };
    let client = windows::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: 698,
        bottom: 780,
    };
    let _ = unsafe { FillRect(hdc, &client, white) };
    if let Some(qr) = &state.qr {
        let area_left = 44;
        let area_top = 82;
        let area_width = 610;
        let area_height = 620;
        let module_count = qr.width.saturating_add(8) as i32;
        let module_size = (area_width.min(area_height) / module_count).max(1);
        let total = module_count * module_size;
        let left = area_left + (area_width - total) / 2;
        let top = area_top + (area_height - total) / 2;
        let quiet = 4_i32;
        for y in 0..qr.width {
            for x in 0..qr.width {
                if !qr.modules[y * qr.width + x] {
                    continue;
                }
                let rect = windows::Win32::Foundation::RECT {
                    left: left + (x as i32 + quiet) * module_size,
                    top: top + (y as i32 + quiet) * module_size,
                    right: left + (x as i32 + quiet + 1) * module_size,
                    bottom: top + (y as i32 + quiet + 1) * module_size,
                };
                let _ = unsafe { FillRect(hdc, &rect, black) };
            }
        }
    }
    unsafe {
        let _ = DeleteObject(white.into());
        let _ = DeleteObject(black.into());
        let _ = EndPaint(hwnd, &paint);
    }
}

unsafe fn finish_login(
    _hwnd: HWND,
    state: &mut AppState,
    result: std::result::Result<QrLoginResult, String>,
) {
    let _ = unsafe { EnableWindow(state.login_button, true) };
    if !state.qr_dialog.is_invalid() {
        let _ = unsafe { DestroyWindow(state.qr_dialog) };
        state.qr_dialog = HWND::default();
    }
    let login = match result {
        Ok(value) => value,
        Err(err) => {
            if err != "二维码登录失败: 登录已取消" {
                unsafe { show_error(state.account_status, &err) };
            }
            return;
        }
    };
    state.config.cookie = login.cookie;
    state.config.account_nickname = if login.account.nickname.trim().is_empty() {
        "夸克网盘账号".into()
    } else {
        login.account.nickname
    };
    state.config.account_id = login.account.user_id;
    state.config.account_avatar = login.account.avatar_url;
    if let Err(err) = state.config.save(&state.config_path) {
        unsafe {
            show_error(
                state.account_status,
                &format!("登录成功，但保存登录信息失败：{err}"),
            )
        };
        return;
    }
    state.remote_roots = load_remote_roots(&state.config);
    let _ = unsafe { SendMessageW(state.root_combo, CB_RESETCONTENT, None, None) };
    let mut selected = 0;
    for (index, (id, label)) in state.remote_roots.iter().enumerate() {
        let label = wide(label);
        let _ = unsafe {
            SendMessageW(
                state.root_combo,
                CB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(label.as_ptr() as isize)),
            )
        };
        if id == &state.config.remote_root_id {
            selected = index;
        }
    }
    let _ = unsafe {
        SendMessageW(
            state.root_combo,
            CB_SETCURSEL,
            Some(WPARAM(selected)),
            Some(LPARAM(0)),
        )
    };
    let account = wide(&state.config.account_nickname);
    let detail = if state.config.account_id.is_empty() {
        wide("登录会话已保存，可点击保存并重启完成挂载")
    } else {
        wide(&format!(
            "账号 ID：{} · 登录会话已保存",
            state.config.account_id
        ))
    };
    let _ = unsafe { SetWindowTextW(state.account_status, PCWSTR(account.as_ptr())) };
    let _ = unsafe { SetWindowTextW(state.account_detail, PCWSTR(detail.as_ptr())) };
    unsafe {
        show_info(
            state.account_status,
            "登录成功，账号信息已更新。点击“保存并重启”完成挂载。",
        )
    };
}

#[allow(clippy::too_many_arguments)]
unsafe fn create(
    hwnd: HWND,
    instance: HINSTANCE,
    class: PCWSTR,
    text: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    extra_style: u32,
    id: usize,
) -> Result<HWND> {
    let text = wide(text);
    let control = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            PCWSTR(text.as_ptr()),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(extra_style),
            x,
            y,
            width,
            height,
            Some(hwnd),
            Some(HMENU(id as *mut _)),
            Some(instance),
            None,
        )
    }
    .context("无法创建配置控件")?;
    let font = ui_font();
    unsafe {
        SendMessageW(
            control,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    }
    Ok(control)
}

unsafe fn handle_command(hwnd: HWND, state: &mut AppState, id: usize) {
    match id {
        ID_OPEN | ID_OPEN_FOLDER => open_folder(&state.config.mount_path),
        ID_SETTINGS => unsafe { show_settings(hwnd) },
        ID_TAB_SETTINGS => unsafe { set_tab(state, false) },
        ID_TAB_LOG => unsafe { set_tab(state, true) },
        ID_LOG_REFRESH => unsafe { refresh_log_view(state) },
        ID_LOG_CLEAR => {
            if let Err(err) = logging::clear() {
                unsafe { show_error(hwnd, &err.to_string()) };
            }
            unsafe { refresh_log_view(state) };
        }
        ID_CANCEL => {
            let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
        }
        ID_EXIT => {
            state.connection.take();
            unsafe { DestroyWindow(hwnd) }.ok();
        }
        ID_SAVE => {
            if let Err(err) = unsafe { save_and_restart(hwnd, state) } {
                unsafe { show_error(hwnd, &err.to_string()) };
            }
        }
        ID_STORAGE_SCAN => unsafe { begin_storage_task(hwnd, state, StorageTask::Scan) },
        ID_LOGIN => unsafe { begin_qr_login(hwnd, state) },
        ID_STORAGE_RELEASE => {
            let selected = unsafe { selected_storage_paths(state) };
            let bytes = selected
                .iter()
                .filter_map(|path| {
                    state
                        .storage_entries
                        .iter()
                        .find(|entry| &entry.path == path)
                })
                .map(|entry| entry.releasable_bytes)
                .sum::<u64>();
            if selected.is_empty() || bytes == 0 {
                unsafe { show_info(hwnd, "当前没有可释放的本地网盘文件。") };
            } else {
                let message = format!(
                    "将释放 {} 本地空间（{} 个目录/文件）。\n\n文件仍会保留在夸克网盘和资源管理器中，再次打开时会按需下载。标记为“始终保留在此设备上”的文件不会被释放。",
                    format_bytes(bytes),
                    selected.len()
                );
                if unsafe { confirm(hwnd, &message) } {
                    unsafe {
                        begin_storage_task(
                            hwnd,
                            state,
                            StorageTask::Release {
                                selected,
                                expected_bytes: bytes,
                            },
                        )
                    };
                }
            }
        }
        _ => {}
    }
}

unsafe fn begin_storage_task(hwnd: HWND, state: &mut AppState, task: StorageTask) {
    let _ = unsafe { EnableWindow(state.storage_scan_button, false) };
    let _ = unsafe { EnableWindow(state.storage_release_button, false) };
    let status = match &task {
        StorageTask::Scan => "正在检测本地文件容量…",
        StorageTask::Release { .. } => "正在按选择安全释放本地空间…",
    };
    let status = wide(status);
    let _ = unsafe { SetWindowTextW(state.storage_status, PCWSTR(status.as_ptr())) };
    let path = PathBuf::from(unsafe { control_text(state.path_edit) });
    let hwnd_value = hwnd.0 as isize;
    std::thread::spawn(move || {
        let result = (|| -> Result<(LocalStorageStats, Vec<LocalStorageEntry>)> {
            let stats = match &task {
                StorageTask::Scan => scan_local_storage(&path)?,
                StorageTask::Release { selected, .. } => release_local_paths(&path, selected)?,
            };
            let entries = scan_local_storage_entries(&path)?;
            Ok((stats, entries))
        })()
        .map_err(|err| err.to_string());
        let payload = Box::new(StorageTaskResult { task, result });
        let hwnd = HWND(hwnd_value as *mut _);
        let _ = unsafe {
            PostMessageW(
                Some(hwnd),
                WM_STORAGE_DONE,
                WPARAM(0),
                LPARAM(Box::into_raw(payload) as isize),
            )
        };
    });
}

unsafe fn finish_storage_task(hwnd: HWND, state: &mut AppState, result: StorageTaskResult) {
    let _ = unsafe { EnableWindow(state.storage_scan_button, true) };
    match result.result {
        Ok((stats, entries)) => {
            state.storage_stats = stats;
            state.storage_entries = entries;
            unsafe { populate_storage_list(state) };
            let status = format!(
                "本地占用 {} · 可释放 {}",
                format_bytes(stats.local_bytes),
                format_bytes(stats.releasable_bytes)
            );
            let detail = format!(
                "云端文件总容量 {} · {} 个文件已占用本地空间",
                format_bytes(stats.logical_bytes),
                stats.releasable_files
            );
            let status_wide = wide(&status);
            let detail_wide = wide(&detail);
            let _ = unsafe { SetWindowTextW(state.storage_status, PCWSTR(status_wide.as_ptr())) };
            let _ = unsafe { SetWindowTextW(state.storage_detail, PCWSTR(detail_wide.as_ptr())) };
            let _ =
                unsafe { EnableWindow(state.storage_release_button, stats.releasable_bytes > 0) };
            if let StorageTask::Release { expected_bytes, .. } = result.task {
                let released = expected_bytes.saturating_sub(stats.releasable_bytes);
                unsafe {
                    show_info(
                        hwnd,
                        &format!(
                            "已释放 {} 本地空间。云端文件保持不变。",
                            format_bytes(released)
                        ),
                    )
                };
            }
        }
        Err(err) => {
            let text = wide("容量检测失败");
            let _ = unsafe { SetWindowTextW(state.storage_status, PCWSTR(text.as_ptr())) };
            unsafe { show_error(hwnd, &err) };
        }
    }
}

unsafe fn selected_storage_paths(state: &AppState) -> Vec<PathBuf> {
    let count = unsafe { SendMessageW(state.storage_list, LB_GETSELCOUNT, None, None).0 };
    if count <= 0 {
        return Vec::new();
    }
    let mut indexes = vec![0_i32; count as usize];
    let selected = unsafe {
        SendMessageW(
            state.storage_list,
            LB_GETSELITEMS,
            Some(WPARAM(indexes.len())),
            Some(LPARAM(indexes.as_mut_ptr() as isize)),
        )
        .0
    };
    indexes
        .into_iter()
        .take(selected.max(0) as usize)
        .filter_map(|index| state.storage_entries.get(index as usize))
        .map(|entry| entry.path.clone())
        .collect()
}

unsafe fn populate_storage_list(state: &mut AppState) {
    let _ = unsafe { SendMessageW(state.storage_list, LB_RESETCONTENT, None, None) };
    for entry in &state.storage_entries {
        let name = entry
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| entry.path.display().to_string());
        let label = format!(
            "{}   {}   ({} 个文件)",
            name,
            format_bytes(entry.releasable_bytes),
            entry.file_count
        );
        let label = wide(&label);
        let _ = unsafe {
            SendMessageW(
                state.storage_list,
                LB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(label.as_ptr() as isize)),
            )
        };
    }
    // The safe default is to select every detected entry; users can clear
    // individual rows before pressing “释放空间”.
    for index in 0..state.storage_entries.len() {
        let _ = unsafe {
            SendMessageW(
                state.storage_list,
                LB_SETSEL,
                Some(WPARAM(1)),
                Some(LPARAM(index as isize)),
            )
        };
    }
}

unsafe fn set_tab(state: &mut AppState, log_tab: bool) {
    for control in &state.settings_controls {
        let _ = unsafe { ShowWindow(*control, if log_tab { SW_HIDE } else { SW_SHOW }) };
    }
    let _ = unsafe { ShowWindow(state.log_edit, if log_tab { SW_SHOW } else { SW_HIDE }) };
    let _ = unsafe {
        ShowWindow(
            state.log_refresh_button,
            if log_tab { SW_SHOW } else { SW_HIDE },
        )
    };
    let _ = unsafe {
        ShowWindow(
            state.log_clear_button,
            if log_tab { SW_SHOW } else { SW_HIDE },
        )
    };
    if log_tab {
        unsafe { refresh_log_view(state) };
    }
}

unsafe fn refresh_log_view(state: &mut AppState) {
    let text = logging::read_tail(512 * 1024).unwrap_or_else(|err| format!("日志读取失败：{err}"));
    let text = wide(&text);
    let _ = unsafe { SetWindowTextW(state.log_edit, PCWSTR(text.as_ptr())) };
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

unsafe fn save_and_restart(hwnd: HWND, state: &mut AppState) -> Result<()> {
    let mount = PathBuf::from(unsafe { control_text(state.path_edit) });
    let name = unsafe { control_text(state.name_edit) };
    anyhow::ensure!(!name.trim().is_empty(), "资源管理器显示名称不能为空");
    let startup = unsafe {
        SendMessageW(
            state.startup_check,
            BM_GETCHECK,
            Some(WPARAM(0)),
            Some(LPARAM(0)),
        )
        .0 as u32
            == 1 // BST_CHECKED
    };
    let old_config = state.config.clone();
    let old_mount = old_config.mount_path.clone();
    let mount_changed = !paths_equal(&old_mount, &mount);
    if mount_changed {
        validate_mount_migration(&old_mount, &mount)?;
        let message = format!(
            "将把夸克网盘挂载位置从：\n{}\n\n切换到：\n{}\n\n旧挂载目录中的全部文件和文件夹会从本机删除，然后在新目录重新同步。云端文件不会被删除。\n\n是否继续？",
            old_mount.display(),
            mount.display()
        );
        if !unsafe { confirm_mount_migration(hwnd, &message) } {
            return Ok(());
        }
    }
    anyhow::ensure!(
        !state.config.cookie.trim().is_empty(),
        "请先点击“二维码登录”，使用夸克网盘 APP 扫码完成登录"
    );
    state.config.mount_path = mount;
    state.config.remote_root_name = name.trim().to_string();
    let selection = unsafe { SendMessageW(state.root_combo, CB_GETCURSEL, None, None).0 };
    if selection >= 0
        && let Some((id, label)) = state.remote_roots.get(selection as usize)
    {
        state.config.remote_root_id = id.clone();
        state.config.remote_root_label = label.clone();
    }
    state.config.start_on_login = startup;
    update_startup(startup)?;
    if mount_changed {
        state.connection.take();
        if let Err(err) = migrate_mount_directory(&old_config, &state.config.mount_path) {
            if let Ok(connection) = crate::cloud_files::register_and_connect(&old_config) {
                state.connection = Some(connection);
            }
            return Err(err);
        }
    }
    state.config.save(&state.config_path)?;
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("--config")
        .arg(&state.config_path)
        .spawn()?;
    unsafe { DestroyWindow(hwnd) }?;
    Ok(())
}

fn validate_mount_migration(old: &Path, new: &Path) -> Result<()> {
    anyhow::ensure!(
        old.is_absolute() && new.is_absolute(),
        "挂载目录必须是绝对路径"
    );
    anyhow::ensure!(old.is_dir(), "旧挂载目录不存在：{}", old.display());
    anyhow::ensure!(
        is_safe_mount_directory(old),
        "拒绝删除不安全的旧挂载目录：{}",
        old.display()
    );
    anyhow::ensure!(
        crate::cloud_files::is_registered_sync_root_path(old),
        "旧目录不是当前注册的夸克同步根，已阻止删除：{}",
        old.display()
    );
    anyhow::ensure!(
        !is_ancestor_or_same(old, new) && !is_ancestor_or_same(new, old),
        "新旧挂载目录不能互相包含"
    );
    if new.exists() {
        anyhow::ensure!(new.is_dir(), "新挂载位置不是文件夹：{}", new.display());
        anyhow::ensure!(
            fs::read_dir(new)?.next().is_none(),
            "新挂载目录必须为空：{}",
            new.display()
        );
    }
    Ok(())
}

fn migrate_mount_directory(old_config: &Config, new: &Path) -> Result<()> {
    crate::cloud_files::unregister(&old_config.mount_path)?;
    let mut last_error = None;
    for _ in 0..4 {
        match fs::remove_dir_all(&old_config.mount_path) {
            Ok(()) => {
                if let Some(parent) = new.parent() {
                    fs::create_dir_all(parent)?;
                }
                return Ok(());
            }
            Err(err) => {
                last_error = Some(err);
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
    }
    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("无法删除旧挂载目录")))
    .with_context(|| format!("无法删除旧挂载目录 {}", old_config.mount_path.display()))
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn is_ancestor_or_same(parent: &Path, child: &Path) -> bool {
    let parent = parent.to_string_lossy().replace('/', "\\").to_lowercase();
    let child = child.to_string_lossy().replace('/', "\\").to_lowercase();
    child == parent
        || child
            .strip_prefix(&parent)
            .is_some_and(|rest| rest.starts_with('\\'))
}

fn is_safe_mount_directory(path: &Path) -> bool {
    let meaningful = path
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count();
    if meaningful < 1 || path.parent().is_none() {
        return false;
    }
    let protected = [
        dirs::home_dir(),
        dirs::desktop_dir(),
        dirs::document_dir(),
        dirs::download_dir(),
        dirs::data_dir(),
        dirs::config_dir(),
    ];
    !protected
        .into_iter()
        .flatten()
        .any(|item| paths_equal(path, &item))
}

fn load_remote_roots(config: &Config) -> Vec<(String, String)> {
    let mut roots = vec![("0".to_string(), "全部文件".to_string())];
    if config.cookie.trim().is_empty() {
        return roots;
    }
    let result = QuarkClient::new(&config.cookie).and_then(|client| client.list_children("0"));
    if let Ok(items) = result {
        roots.extend(
            items
                .into_iter()
                .filter(|item| item.is_directory && !is_internal_name(&item.name))
                .map(|item| (item.id, item.name)),
        );
    }
    if config.remote_root_id != "0" && !roots.iter().any(|(id, _)| id == &config.remote_root_id) {
        roots.push((
            config.remote_root_id.clone(),
            config.remote_root_label.clone(),
        ));
    }
    roots
}

fn ui_font() -> windows::Win32::Graphics::Gdi::HFONT {
    static FONT: OnceLock<isize> = OnceLock::new();
    let raw = *FONT.get_or_init(|| unsafe {
        CreateFontW(
            -16,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
            w!("Segoe UI"),
        )
        .0 as isize
    });
    windows::Win32::Graphics::Gdi::HFONT(raw as *mut _)
}

fn update_startup(enabled: bool) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")?;
    if enabled {
        key.set_value(
            "QuarkDrive",
            &format!("\"{}\"", std::env::current_exe()?.display()),
        )?;
    } else {
        let _ = key.delete_value("QuarkDrive");
    }
    Ok(())
}

unsafe fn show_settings(hwnd: HWND) {
    let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
    let _ = unsafe { SetForegroundWindow(hwnd) };
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    unsafe {
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, w!("打开夸克网盘"));
        let _ = AppendMenuW(menu, MF_STRING, ID_SETTINGS, w!("设置"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("退出"));
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, point.x, point.y, None, hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

unsafe fn add_tray_icon(hwnd: HWND, icon: HICON, connected: bool) -> Result<()> {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    copy_wide(
        &mut data.szTip,
        if connected {
            "夸克网盘 - 已挂载"
        } else {
            "夸克网盘 - 需要扫码登录"
        },
    );
    anyhow::ensure!(
        unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool(),
        "无法创建任务栏托盘图标"
    );
    Ok(())
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
}

fn open_folder(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
}

unsafe fn control_text(hwnd: HWND) -> String {
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    let mut buffer = vec![0_u16; length as usize + 1];
    unsafe { GetWindowTextW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..length as usize])
}

unsafe fn show_error(hwnd: HWND, text: &str) {
    let text = wide(text);
    unsafe {
        MessageBoxW(
            Some(hwnd),
            PCWSTR(text.as_ptr()),
            w!("夸克网盘"),
            MB_OK | MB_ICONERROR,
        )
    };
}

unsafe fn show_info(hwnd: HWND, text: &str) {
    let text = wide(text);
    unsafe {
        MessageBoxW(
            Some(hwnd),
            PCWSTR(text.as_ptr()),
            w!("夸克网盘"),
            MB_OK | MB_ICONINFORMATION,
        )
    };
}

unsafe fn confirm(hwnd: HWND, text: &str) -> bool {
    let text = wide(text);
    unsafe {
        MessageBoxW(
            Some(hwnd),
            PCWSTR(text.as_ptr()),
            w!("释放本地空间"),
            MB_OKCANCEL | MB_ICONINFORMATION | MB_DEFBUTTON2,
        ) == IDOK
    }
}

unsafe fn confirm_mount_migration(hwnd: HWND, text: &str) -> bool {
    let text = wide(text);
    unsafe {
        MessageBoxW(
            Some(hwnd),
            PCWSTR(text.as_ptr()),
            w!("切换挂载目录"),
            MB_OKCANCEL | MB_ICONWARNING | MB_DEFBUTTON2,
        ) == IDOK
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn copy_wide<const N: usize>(target: &mut [u16; N], value: &str) {
    for (to, from) in target.iter_mut().zip(value.encode_utf16()) {
        *to = from;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_storage_sizes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn rejects_nested_mount_migrations() {
        assert!(is_ancestor_or_same(
            Path::new(r"C:\Users\me\QuarkDrive"),
            Path::new(r"C:\Users\me\QuarkDrive\new")
        ));
        assert!(is_ancestor_or_same(
            Path::new(r"C:\Users\me"),
            Path::new(r"C:\Users\me\QuarkDrive")
        ));
        assert!(!is_ancestor_or_same(
            Path::new(r"C:\Users\me\QuarkDrive"),
            Path::new(r"D:\QuarkDrive")
        ));
    }

    #[test]
    fn rejects_drive_roots_as_mount_directory() {
        assert!(!is_safe_mount_directory(Path::new(r"C:\")));
        assert!(is_safe_mount_directory(Path::new(r"D:\QuarkDrive")));
    }
}
