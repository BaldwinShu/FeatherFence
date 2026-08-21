// GUI 子系统:不弹出控制台窗口。日志写 %APPDATA%\feather-fences\debug.log;
// 从终端 cargo run 启动时输出仍会显示在终端里(继承父进程句柄)。
#![windows_subsystem = "windows"]

// unsafe_op_in_unsafe_fn:本库以 `unsafe fn` 作为 Win32 FFI 的安全契约(每个调用点都
// 在 unsafe fn 体内),再逐调用包 unsafe{} 属于重复标注,徒增噪音。函数签名已声明 unsafe。
#![allow(unsafe_op_in_unsafe_fn)]

// 轻栅栏 feather-fences:超轻量桌面分区整理工具
// Rust + Win32 原生实现,Fences 轻量版(GPL-3.0,受 Fluid Fences 概念启发,代码为原创)
mod config;
mod desktop_icons;
mod dragout;
mod droptarget;
mod fence;
mod icons;
mod perf;
mod tray;
mod utils;
mod watcher;
mod download;
mod fencelife;
mod shortcut;
mod sweep;

use std::ffi::c_void;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError, HWND, LPARAM, LRESULT,
    SetLastError, WPARAM,
};
use windows::Win32::Graphics::GdiPlus::{GdiplusShutdown, GdiplusStartup, GdiplusStartupInput, GdiplusStartupOutput};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_CONTROL};
use windows::Win32::UI::Shell::{
    SHBrowseForFolderW, SHGetKnownFolderPath, SHGetPathFromIDListW, BIF_NEWDIALOGSTYLE,
    BIF_RETURNONLYFSDIRS, BROWSEINFOW, FOLDERID_Desktop, FOLDERID_PublicDesktop, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, PostMessageW, PostQuitMessage, RegisterClassW,
    TranslateMessage, WM_APP, WM_DESTROY, WM_HOTKEY, WM_QUIT, WM_TIMER, WNDCLASSW,
    WS_POPUP,
};

use config::{Config, FenceCfg, FenceKind};
use fence::Fence;
use tray::{
    WM_APP_TRAY, MENU_AUTOSTART, MENU_CONFIG_DIR, MENU_DESKTOP_AVOID, MENU_DESKTOP_ROLLBACK,
    MENU_DOWNLOAD_ENABLED, MENU_DOWNLOAD_VISIBLE, MENU_EXIT, MENU_GHOST, MENU_NEW_BOX,
    MENU_NEW_PORTAL, MENU_RELOAD, MENU_SWEEP, MENU_TOGGLE_VIS, MENU_ZEN, add_tray,
    make_tray_icon, remove_tray, show_tray_menu,
};
use utils::wstr;

use download::*;
use fencelife::*;
use shortcut::*;
use sweep::*;

unsafe impl Send for Global {}

pub struct Global {
    pub config: Config,
    pub next_id: u32,
    pub fences: Vec<Fence>,
    pub msg_hwnd: HWND,
    pub zen: bool,
    pub desktop_host: Option<HWND>,
    pub icons: icons::IconCache,
    pub sweep_retry: Vec<(PathBuf, PathBuf)>,
    /// 桌面监听线程传来的文件名；主线程等待新增快捷方式写入稳定后自动收纳。
    pub desktop_rx: Receiver<Vec<PathBuf>>,
    pub shortcut_seen: HashSet<PathBuf>,
    pub shortcut_pending: HashMap<PathBuf, FileCandidate>,
    pub(crate) shortcut_dragout: Option<ShortcutDragoutState>,
    pub download_rx: Receiver<Vec<String>>,
    pub download_seen: HashSet<PathBuf>,
    pub download_pending: HashMap<PathBuf, FileCandidate>,
    pub exiting: bool,
    /// 拖放 COM 对象,保持存活
    pub droptargets: Vec<windows::Win32::System::Ole::IDropTarget>,
    /// 目录监听线程
    pub watchers: Vec<ManagedWatcher>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WatcherOwner {
    Process,
    Fence(u32),
}

pub struct ManagedWatcher {
    owner: WatcherOwner,
    _watcher: watcher::DirWatcher,
}

impl ManagedWatcher {
    fn process(watcher: watcher::DirWatcher) -> Self {
        Self {
            owner: WatcherOwner::Process,
            _watcher: watcher,
        }
    }

    fn fence(id: u32, watcher: watcher::DirWatcher) -> Self {
        Self {
            owner: WatcherOwner::Fence(id),
            _watcher: watcher,
        }
    }
}

pub struct FileCandidate {
    len: u64,
    modified: Option<std::time::SystemTime>,
    stable_ticks: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CollectionStats {
    id: u32,
    shortcuts: u64,
    files: u64,
}

static G: OnceLock<Mutex<Global>> = OnceLock::new();
static G_PTR: OnceLock<usize> = OnceLock::new();
static HINSTANCE: OnceLock<usize> = OnceLock::new();

thread_local! {
    static G_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// 调试日志:写 %APPDATA%eather-fences\debug.log + stderr
pub fn dlog(msg: &str) {
    use std::io::Write;
    let p = config::config_dir().join("debug.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{}", msg);
    }
    eprintln!("{msg}");
}

pub fn hinstance() -> windows::Win32::Foundation::HINSTANCE {
    let ptr = *HINSTANCE.get_or_init(|| {
        let h = unsafe { GetModuleHandleW(None).unwrap_or_default() };
        h.0 as usize
    });
    windows::Win32::Foundation::HINSTANCE(ptr as *mut c_void)
}

/// 可重入全局访问:模态调用(TrackPopupMenu/DestroyWindow/文件夹对话框)会在持锁时
/// 派发窗口消息 → 再次进入本函数。深度>0 时直接走裸指针(仅主线程可达,安全)。
pub fn with_global<R>(f: impl FnOnce(&mut Global) -> R) -> R {
    let depth = G_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    let result = if depth == 0 {
        let mut guard = G.get().expect("global not init").lock().unwrap();
        f(&mut guard)
    } else {
        unsafe {
            let ptr = *G_PTR.get().expect("global ptr not set") as *mut Global;
            f(&mut *ptr)
        }
    };
    G_DEPTH.with(|d| d.set(depth));
    result
}












fn desktop_dir() -> Option<PathBuf> {
    known_folder_dir(&FOLDERID_Desktop)
}

fn public_desktop_dir() -> Option<PathBuf> {
    known_folder_dir(&FOLDERID_PublicDesktop)
}

fn known_folder_dir(folder_id: &windows::core::GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(
            folder_id,
            windows::Win32::UI::Shell::KNOWN_FOLDER_FLAG(0),
            None,
        )
        .ok()?;
        let s = String::from_utf16_lossy(p.as_wide());
        CoTaskMemFree(Some(p.as_ptr() as *const c_void));
        Some(PathBuf::from(s))
    }
}

fn pick_folder(owner: HWND, title: &str) -> Option<PathBuf> {
    unsafe {
        let mut display = [0u16; 260];
        let title_w = wstr(title);
        let mut bi = BROWSEINFOW {
            hwndOwner: owner,
            pidlRoot: std::ptr::null_mut(),
            pszDisplayName: windows::core::PWSTR(display.as_mut_ptr()),
            lpszTitle: PCWSTR(title_w.as_ptr()),
            ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
            lpfn: None,
            lParam: LPARAM(0),
            iImage: 0,
        };
        let pidl = SHBrowseForFolderW(&mut bi);
        if pidl.is_null() {
            return None;
        }
        let mut buf = [0u16; 260];
        let ok = SHGetPathFromIDListW(pidl, &mut buf);
        CoTaskMemFree(Some(pidl as *const c_void));
        if ok.as_bool() {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(260);
            Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
        } else {
            None
        }
    }
}

// ---------- 开机自启 ----------

fn set_autostart(enabled: bool) {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
    use winreg::RegKey;
    let path = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    match RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(path, KEY_READ | KEY_WRITE) {
        Ok(key) => {
            let _ = if enabled {
                match std::env::current_exe() {
                    Ok(exe) => key.set_value("feather-fences", &exe.to_string_lossy().to_string()),
                    Err(_) => Ok(()),
                }
            } else {
                key.delete_value("feather-fences")
            };
        }
        Err(e) => eprintln!("[feather] autostart registry: {e}"),
    }
}

// ---------- 消息窗口 ----------

const TID_WATCHDOG: usize = 1;
const TID_SWEEP_RETRY: usize = 3;
const TID_DOWNLOADS: usize = 4;
const TID_DESKTOP_LAYER: usize = 5;
const WM_APP_SWEEP: u32 = WM_APP + 5;

unsafe extern "system" fn msg_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_APP_TRAY {
        let action = (lparam.0 & 0xFFFF) as u32;
        if action == windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONUP as u32
            || action == windows::Win32::UI::WindowsAndMessaging::WM_CONTEXTMENU as u32
        {
            let (zen, ghost, autostart, download_enabled, download_visible, desktop_avoid) =
                with_global(|g| {
                    (
                        g.zen,
                        g.config.ghost_mode,
                        g.config.autostart,
                        g.config.download_enabled,
                        g.config.download_box_visible,
                        g.config.desktop_avoid,
                    )
                });
            let cmd = show_tray_menu(
                hwnd,
                zen,
                ghost,
                autostart,
                download_enabled,
                download_visible,
                desktop_avoid,
            );
            dispatch_menu(cmd);
        } else if action == windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONDBLCLK as u32 {
            with_global(|g| {
                g.zen = !g.zen;
                apply_visibility(g);
            });
        }
        return LRESULT(0);
    }
    if msg == WM_HOTKEY {
        with_global(|g| {
            g.zen = !g.zen;
            apply_visibility(g);
        });
        return LRESULT(0);
    }
    if msg == WM_TIMER {
        match wparam.0 {
            TID_WATCHDOG => with_global(|g| watchdog_tick(g)),
            TID_SWEEP_RETRY => with_global(|g| sweep_retry_tick(g)),
            TID_DOWNLOADS => with_global(|g| {
                download_tick(g);
                shortcut_tick(g);
            }),
            TID_DESKTOP_LAYER => with_global(|g| desktop_layer_tick(g)),
            _ => {}
        }
        return LRESULT(0);
    }
    if msg == WM_APP_SWEEP {
        with_global(|g| sweep_desktop(g));
        return LRESULT(0);
    }
    // 目录监听按栅栏 id 通知:窗口重建后依然有效(不依赖具体 hwnd)
    if msg == fence::WM_APP_REFRESH_ID {
        let id = wparam.0 as u32;
        with_global(|g| {
            if let Some(idx) = g.fences.iter().position(|f| f.valid && f.cfg.id == id) {
                let ghost = g.config.ghost_mode;
                let f = &mut g.fences[idx];
                fence::refresh_entries(f, &config::vault_dir(&g.config));
                fence::render_fence(&mut g.icons, ghost, f);
            }
        });
        return LRESULT(0);
    }
    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn dispatch_menu(cmd: u32) {
    match cmd {
        MENU_NEW_PORTAL => {
            with_global(|g| {
                if let Some(folder) = pick_folder(g.msg_hwnd, "选择栅栏要显示的文件夹") {
                    let title = folder
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "文件夹栅栏".into());
                    let (sw, _sh) = utils::screen_size();
                    let s = fence::dpi_scale();
                    // id 传 0,由 create_fence 统一分配并递增 next_id(避免重复 id)
                    let cfg = FenceCfg {
                        id: 0,
                        title,
                        kind: FenceKind::Portal,
                        folder: Some(folder),
                        x: sw - (340.0 * s) as i32,
                        y: (100.0 * s) as i32 + (g.fences.len() as i32 % 5) * (40.0 * s) as i32,
                        w: (280.0 * s) as i32,
                        h: (340.0 * s) as i32,
                        dpi: (96.0 * s).round() as u32,
                        opacity: 0.7,
                        icon: 32,
                        pos_set: None,
                    };
                    create_fence(g, cfg);
                }
            });
        }
        MENU_NEW_BOX => {
            with_global(|g| {
                // 每个收纳栅栏 = 新建一个专属空目录(不再共享 vault)。
                // 目录放 config_dir/boxes/ 下,名字自动取"收纳箱/收纳箱 2/…"去重。
                let boxes_root = config::config_dir().join("boxes");
                let dir = {
                    let mut n = 1u32;
                    loop {
                        let name = if n == 1 {
                            "收纳箱".to_string()
                        } else {
                            format!("收纳箱 {}", n)
                        };
                        let d = boxes_root.join(&name);
                        if !d.exists() {
                            break d;
                        }
                        n += 1;
                    }
                };
                if std::fs::create_dir_all(&dir).is_ok() {
                    let (sw, _sh) = utils::screen_size();
                    let title = dir
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "收纳箱".into());
                    // id 传 0,由 create_fence 分配新 id 并递增
                    let s = fence::dpi_scale();
                    let cfg = FenceCfg {
                        id: 0,
                        title,
                        kind: FenceKind::Collection,
                        folder: Some(dir),
                        x: sw - (320.0 * s) as i32,
                        y: (100.0 * s) as i32 + (g.fences.len() as i32 % 5) * (40.0 * s) as i32,
                        w: (260.0 * s) as i32,
                        h: (340.0 * s) as i32,
                        dpi: (96.0 * s).round() as u32,
                        opacity: 0.7,
                        icon: 32,
                        pos_set: None,
                    };
                    create_fence(g, cfg);
                }
            });
        }
        MENU_TOGGLE_VIS => {
            with_global(|g| {
                g.zen = !g.zen;
                apply_visibility(g);
            });
        }
        MENU_ZEN => {
            with_global(|g| {
                g.zen = !g.zen;
                apply_visibility(g);
            });
        }
        MENU_GHOST => {
            with_global(|g| {
                g.config.ghost_mode = !g.config.ghost_mode;
                config::save(&g.config);
                for f in g.fences.iter() {
                    if f.valid {
                        fence::schedule_render(f.hwnd);
                    }
                }
            });
        }
        MENU_SWEEP => {
            let _ = unsafe { PostMessageW(
                Some(with_global(|g| g.msg_hwnd)),
                WM_APP_SWEEP,
                WPARAM(0),
                LPARAM(0),
            ) };
        }
        MENU_DOWNLOAD_ENABLED => {
            with_global(|g| set_download_enabled(g, !g.config.download_enabled));
        }
        MENU_DOWNLOAD_VISIBLE => {
            with_global(|g| {
                if g.config.download_enabled {
                    set_download_box_visible(g, !g.config.download_box_visible);
                }
            });
        }
        MENU_DESKTOP_AVOID => {
            with_global(|g| {
                g.config.desktop_avoid = !g.config.desktop_avoid;
                if g.config.desktop_avoid {
                    reserve_desktop_icons(g);
                } else {
                    // 关闭避让:不回退图标(由「撤销并关闭避让」负责),
                    // 只恢复自动排列样式并清空历史。
                    desktop_icons::restore_autoarrange();
                    desktop_icons::clear_history();
                }
                config::save(&g.config);
            });
        }
        MENU_DESKTOP_ROLLBACK => {
            with_global(|g| rollback_desktop(g));
        }
        MENU_AUTOSTART => {
            with_global(|g| {
                g.config.autostart = !g.config.autostart;
                set_autostart(g.config.autostart);
                config::save(&g.config);
            });
        }
        MENU_RELOAD => {
            with_global(|g| {
                let mut c = config::load();
                config::normalize_dpi(&mut c);
                c.title_font_size = config::normalize_title_font_size(c.title_font_size);
                fence::set_icon_px(c.icon);
                fence::set_title_font_px(c.title_font_size);
                g.config = c;
                // 保留进程级监听（桌面清扫和 Downloads 接管）；先停止所有栅栏监听，
                // 避免窗口销毁期间仍收到刷新。
                g.watchers
                    .retain(|watcher| watcher.owner == WatcherOwner::Process);
                // 先销毁全部旧窗口(避免持借用调用 DestroyWindow)
                let hwnds: Vec<HWND> = g.fences.iter().filter(|f| f.valid).map(|f| f.hwnd).collect();
                for h in hwnds {
                    unsafe {
                        let _ = windows::Win32::System::Ole::RevokeDragDrop(h);
                        let _ = DestroyWindow(h);
                    }
                }
                g.fences.clear(); // Fence 析构 → 各自目录监听自动停止
                g.droptargets.clear();
                // 与启动恢复一致:重复/缺失 id 重新分配,保证按 id 刷新全部生效
                let mut seen = std::collections::HashSet::new();
                for cfg in g.config.fences.clone() {
                    let mut c = cfg;
                    if c.id == 0 || !seen.insert(c.id) {
                        c.id = g.next_id;
                        g.next_id += 1;
                    }
                    create_fence(g, c);
                }
                ensure_download_box(g);
                apply_visibility(g);
            });
        }
        MENU_CONFIG_DIR => {
            let dir = config::config_dir();
            let _ = std::fs::create_dir_all(&dir);
            let w = wstr(&dir.to_string_lossy());
            unsafe {
                let _ = ShellExecuteW(
                    None,
                    PCWSTR(w!("explore").as_ptr()),
                    PCWSTR(w.as_ptr()),
                    None,
                    None,
                    windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
                );
            }
        }
        MENU_EXIT => {
            unsafe { let _ = PostMessageW(Some(with_global(|g| g.msg_hwnd)), WM_QUIT, WPARAM(0), LPARAM(0)); };
        }
        _ => {}
    }
}

// ---------- main ----------

fn main() {
    dlog("[main] start");
    perf::init();
    utils::set_dpi_awareness();
    dlog("[main] dpi set");

    // 单实例
    // 单实例:先清零错误码再创建互斥体(CreateMutexW 成功时不保证清除 GetLastError,
    // 残留值会导致误判"已在运行"而弹框退出)
    unsafe {
        SetLastError(ERROR_SUCCESS);
    }
    let mutex = unsafe { CreateMutexW(None, false, w!("feather-fences-singleton")).unwrap_or_default() };
    let last_err = unsafe { GetLastError() };
    dlog(&format!(
        "[main] mutex handle valid={} last_error={} (183=ALREADY_EXISTS)",
        !mutex.is_invalid(),
        last_err.0
    ));
    if last_err == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                None,
                w!("轻栅栏已在运行(见系统托盘)"),
                w!("轻栅栏"),
                windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE(0x10),
            );
        }
        return;
    }

    // OLE(拖放需要)
    unsafe {
        let _ = OleInitialize(None);
    }
    dlog("[main] ole ok");

    // GDI+
    let mut token: usize = 0;
    let input = GdiplusStartupInput {
        GdiplusVersion: 1,
        DebugEventCallback: 0,
        SuppressBackgroundThread: windows::core::BOOL(0),
        SuppressExternalCodecs: windows::core::BOOL(0),
    };
    let mut output = GdiplusStartupOutput::default();
    let status = unsafe { GdiplusStartup(&mut token, &input, &mut output) };
    if status.0 != 0 {
        eprintln!("[feather] GdiplusStartup failed: {status:?}");
        return;
    }

    let hinst = hinstance();
    dlog("[main] gdiplus+msg window prep");
    unsafe {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(msg_wndproc),
            hInstance: hinst,
            lpszClassName: PCWSTR(w!("FeatherMsg").as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
    }

    let msg_hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            w!("FeatherMsg"),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            // 托盘弹出菜单需要一个可成为前台窗口的隐藏顶层 owner；
            // HWND_MESSAGE 消息窗口无法可靠触发“点击外部关闭菜单”。
            None,
            None,
            Some(hinst),
            None,
        )
        .unwrap_or_default()
    };

    fence::register_class();
    dlog("[main] class registered");
    let mut cfg = config::load();
    // 磁盘配置是逻辑像素 → 乘回当前系统 DPI 变物理像素;旧版物理像素原样保留(一次性迁移)
    config::normalize_dpi(&mut cfg);
    // 一次性迁移:旧版图标尺寸存在栅栏上,现在全局统一。
    // 若全局未设,取第一个非零栅栏值;否则默认 32。
    if cfg.icon == 0 {
        cfg.icon = cfg
            .fences
            .iter()
            .find(|f| f.icon != 0)
            .map(|f| f.icon)
            .unwrap_or(32);
    }
    cfg.title_font_size = config::normalize_title_font_size(cfg.title_font_size);
    fence::set_icon_px(cfg.icon);
    fence::set_title_font_px(cfg.title_font_size);
    let vault = config::vault_dir(&cfg);
    let _ = std::fs::create_dir_all(&vault);

    let (desktop_tx, desktop_rx) = mpsc::channel::<Vec<PathBuf>>();
    let (download_tx, download_rx) = mpsc::channel::<Vec<String>>();
    let mut shortcut_seen = HashSet::new();
    for dir in [desktop_dir(), public_desktop_dir()].into_iter().flatten() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            shortcut_seen.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| is_shortcut(path)),
            );
        }
    }
    let download_seen = downloads_dir()
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();

    G.set(Mutex::new(Global {
        config: cfg.clone(),
        next_id: cfg.fences.iter().map(|f| f.id).max().unwrap_or(0) + 1,
        fences: Vec::new(),
        msg_hwnd,
        zen: false,
        desktop_host: None,
        icons: icons::IconCache::new(),
        sweep_retry: Vec::new(),
        desktop_rx,
        shortcut_seen,
        shortcut_pending: HashMap::new(),
        shortcut_dragout: None,
        download_rx,
        download_seen,
        download_pending: HashMap::new(),
        exiting: false,
        droptargets: Vec::new(),
        watchers: Vec::new(),
    }))
    .ok();
    G_PTR
        .set(&*G.get().expect("global").lock().unwrap() as *const Global as usize)
        .ok();

    // 托盘
    let ticon = make_tray_icon();
    add_tray(msg_hwnd, ticon);
    dlog("[main] tray ok");

    // 热键 Ctrl+Alt+Z = Zen
    unsafe {
        let _ = RegisterHotKey(Some(msg_hwnd), 1, MOD_CONTROL | MOD_ALT, 'Z' as u32);
    }

    // 定时器
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
            Some(msg_hwnd),
            TID_WATCHDOG,
            3000,
            None,
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
            Some(msg_hwnd),
            TID_DOWNLOADS,
            1000,
            None,
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
            Some(msg_hwnd),
            TID_DESKTOP_LAYER,
            150,
            None,
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
            Some(msg_hwnd),
            TID_SWEEP_RETRY,
            2000,
            None,
        );
    }

    // 恢复配置里的栅栏
    let fences = cfg.fences.clone();
    dlog(&format!("[main] restoring {} fences", fences.len()));
    with_global(|g| {
        // 旧版 bug 曾产生重复 id(如多个文件夹栅栏 id 全为 1);按 id 通知的
        // watcher 只命中第一个,其余栅栏失去自动刷新 —— 恢复时重新分配唯一 id
        let mut seen = std::collections::HashSet::new();
        for fcfg in &fences {
            let mut c = fcfg.clone();
            if c.id == 0 || !seen.insert(c.id) {
                c.id = g.next_id;
                g.next_id += 1;
            }
            create_fence(g, c);
        }
        // 首启:没有栅栏就建一个默认收纳箱(右侧),并保存配置
        if g.fences.is_empty() {
            let (sw, _sh) = utils::screen_size();
            let s = fence::dpi_scale();
            let box_cfg = FenceCfg {
                id: 0, // 由 create_fence 分配;直接传 next_id 不会递增,会与后续新建栅栏撞 id
                title: "收纳箱".into(),
                kind: config::FenceKind::Collection,
                folder: None,
                x: sw - (320.0 * s) as i32,
                y: (100.0 * s) as i32,
                w: (260.0 * s) as i32,
                h: (340.0 * s) as i32,
                dpi: (96.0 * s).round() as u32,
                opacity: 0.74,
                icon: 32,
                pos_set: None,
            };
            // 创建成功才保存,避免失败时把配置覆盖成空
            if create_fence(g, box_cfg) != 0 {
                sync_config(g);
            }
        }
        // 始终保留专用下载收纳箱;是否接管/显示由两个独立配置控制。
        ensure_download_box(g);
        // 网格落位:恢复后把所有栅栏吸附到整数槽位、clamp 进工作区,
        // 并推挤消除重叠 —— 重启后布局也保持规整
        let n = g.fences.len();
        for i in 0..n {
            fence::settle_fence(g, i);
        }
        apply_visibility(g);
        // 桌面自动归类监听:线程里只做扩展名粗筛,命中就通知主线程执行整理
        if let Some(dir) = desktop_dir() {
            let rules = g.config.sweep_rules.clone();
            let mhwnd = g.msg_hwnd.0 as usize;
            let tx = desktop_tx.clone();
            let watched_dir = dir.clone();
            let watcher = watcher::spawn_dir_watcher(dir.clone(), move |names| {
                let paths = names.iter().map(|name| watched_dir.join(name)).collect();
                let _ = tx.send(paths);
                for n in &names {
                    let ext = Path::new(&n)
                        .extension()
                        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
                        .unwrap_or_default();
                    if rules.iter().any(|r| r.ext.to_lowercase() == ext) {
                        unsafe {
                            let _ = PostMessageW(
                                Some(HWND(mhwnd as *mut c_void)),
                                WM_APP_SWEEP,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                        break;
                    }
                }
            });
            g.watchers.push(ManagedWatcher::process(watcher));
        }
        // 安装器也可能把快捷方式写入所有用户共享的公共桌面。
        if let Some(dir) =
            public_desktop_dir().filter(|public| desktop_dir().as_deref() != Some(public.as_path()))
        {
            let tx = desktop_tx.clone();
            let watched_dir = dir.clone();
            let watcher = watcher::spawn_dir_watcher(dir, move |names| {
                let paths = names.iter().map(|name| watched_dir.join(name)).collect();
                let _ = tx.send(paths);
            });
            g.watchers.push(ManagedWatcher::process(watcher));
        }
        // 下载收纳箱：单独监听 Downloads 目录，避免把桌面所有文件都当下载。
        if let Some(dir) = downloads_dir() {
            let tx = download_tx.clone();
            let watcher = watcher::spawn_dir_watcher(dir, move |names| {
                let _ = tx.send(names.clone());
            });
            g.watchers.push(ManagedWatcher::process(watcher));
        }
        reserve_desktop_icons(g);
        if let Some(id) = perf::animation_fence_id() {
            if let Some(f) = g.fences.iter_mut().find(|f| f.valid && f.cfg.id == id) {
                if fence::start_perf_animation(f) {
                    fence::render_fence(&mut g.icons, g.config.ghost_mode, f);
                }
            }
        }
    });

    dlog(&format!("[main] started, fences: {}", fences.len()));

    // 消息循环
    dlog("[main] message loop start");
    unsafe {
        let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
        let mut count: u64 = 0;
        let mut last = std::time::Instant::now();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            count += 1;
            if count % 2000 == 0 {
                let hw = msg.hwnd;
                let cls = {
                    let mut b = [0u16; 64];
                    windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hw, &mut b);
                    String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(64)])
                };
                dlog(&format!(
                    "[main] processed {count} msgs in {}ms (msg=0x{:x} hwnd=0x{:x} class={})",
                    last.elapsed().as_millis(),
                    msg.message,
                    hw.0 as usize,
                    cls
                ));
                last = std::time::Instant::now();
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // 清理
    with_global(|g| {
        g.exiting = true;
        // 先停止所有目录监听，确保清理窗口和 COM/GDI+ 后不再有后台通知。
        g.watchers.clear();
        config::save(&g.config);
        for f in g.fences.iter() {
            if f.valid {
                unsafe { let _ = windows::Win32::System::Ole::RevokeDragDrop(f.hwnd); };
            }
        }
    });
    unsafe {
        remove_tray(msg_hwnd);
        let _ = DestroyWindow(msg_hwnd);
        GdiplusShutdown(token);
        OleUninitialize();
        let _ = CloseHandle(mutex);
    }
    eprintln!("[feather] bye");
}

