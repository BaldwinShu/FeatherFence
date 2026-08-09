// GUI 子系统:不弹出控制台窗口。日志写 %APPDATA%\feather-fences\debug.log;
// 从终端 cargo run 启动时输出仍会显示在终端里(继承父进程句柄)。
#![windows_subsystem = "windows"]

// 轻栅栏 feather-fences:超轻量桌面分区整理工具
// Rust + Win32 原生实现,Fences 轻量版(GPL-3.0,受 Fluid Fences 概念启发,代码为原创)
mod config;
mod dragout;
mod droptarget;
mod fence;
mod icons;
mod tray;
mod utils;
mod watcher;

use std::ffi::c_void;
use std::ptr::NonNull;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError, HANDLE, HWND, LPARAM, LRESULT,
    RECT, SetLastError, WPARAM,
};
use windows::Win32::Graphics::GdiPlus::{GdiplusShutdown, GdiplusStartup, GdiplusStartupInput, GdiplusStartupOutput, Status};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_CONTROL};
use windows::Win32::UI::Shell::{
    SHBrowseForFolderW, SHGetKnownFolderPath, SHGetPathFromIDListW, BIF_NEWDIALOGSTYLE,
    BIF_RETURNONLYFSDIRS, BROWSEINFOW, FOLDERID_Desktop, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, GetWindowRect,
    HWND_MESSAGE, PostMessageW, PostQuitMessage, RegisterClassW, SetParent, SetWindowPos, ShowWindow,
    TranslateMessage, WM_APP, WM_DESTROY, WM_HOTKEY, WM_QUIT, WM_TIMER, WNDCLASSW, WNDPROC,
    WS_POPUP, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOWNA,
};
use windows::Win32::System::Ole::RegisterDragDrop;

use config::{Config, FenceCfg};
use fence::{Fence, WM_APP_DROP, WM_APP_REFRESH};
use tray::{
    TRAY_ID, WM_APP_TRAY, MENU_AUTOSTART, MENU_CONFIG_DIR, MENU_EXIT, MENU_GHOST, MENU_NEW_BOX,
    MENU_NEW_PORTAL, MENU_RELOAD, MENU_SWEEP, MENU_TOGGLE_VIS, MENU_ZEN, add_tray, make_tray_icon,
    remove_tray, show_tray_menu,
};
use utils::wstr;

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
    pub exiting: bool,
    /// 拖放 COM 对象,保持存活
    pub droptargets: Vec<windows::Win32::System::Ole::IDropTarget>,
    /// 目录监听线程
    pub watchers: Vec<watcher::DirWatcher>,
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------- 栅栏生命周期 ----------

pub fn create_fence(g: &mut Global, mut cfg: FenceCfg) -> u32 {
    if cfg.id == 0 {
        cfg.id = g.next_id;
        g.next_id += 1;
    }
    // 默认位置:屏幕右上角级联(按系统 DPI 缩放逻辑像素偏移;创建窗口前无 hwnd)
    let ms = fence::dpi_scale();
    if cfg.x == 0 && cfg.y == 0 {
        let (sw, sh) = utils::screen_size();
        let n = g.fences.len();
        cfg.x = (sw - (320.0 * ms) as i32 - (20.0 * ms) as i32 - (n as i32 % 5) * (30.0 * ms) as i32).max(0);
        cfg.y = ((80.0 * ms) as i32 + (n as i32 % 5) * (40.0 * ms) as i32).min((sh - (400.0 * ms) as i32).max(0));
    }
    if cfg.w < fence::min_w(ms) {
        cfg.w = fence::min_w(ms);
    }
    if cfg.h < fence::min_h(ms) {
        cfg.h = fence::min_h(ms);
    }

    // 不挂 Progman(分层窗口+高 alpha+Progman 父窗口会触发 DWM 命中测试 bug,
    // 导致窗口可见但点不到拖不动);改为独立顶层窗口 + 压底 Z 序(同 Fluid Fences 思路)
    let hwnd = fence::create_window(&cfg, None);
    if hwnd.is_invalid() {
        return 0;
    }
    // 注册拖放
    let dt = droptarget::FenceDropTarget::new(hwnd);
    let it: windows::Win32::System::Ole::IDropTarget = dt.into();
    unsafe { let _ = RegisterDragDrop(hwnd, &it); }
    // 保持 COM 对象存活:塞进全局集合,进程退出时释放
    g.droptargets.push(it);

    let mut f = Fence::new(cfg, hwnd);
    fence::refresh_entries(&mut f, &config::vault_dir(&g.config));
    fence::render_fence(&mut g.icons, g.config.ghost_mode, &mut f);
    let id = f.cfg.id;
    g.fences.push(f);
    // 新栅栏立即落到网格:尺寸/位置吸附 + clamp 工作区 + 消除重叠
    let new_idx = g.fences.len() - 1;
    fence::settle_fence(g, new_idx);

    // 门户目录监听
    if let Some(folder) = g.fences.last().and_then(|f| f.cfg.folder.clone()) {
        let hwnd2 = hwnd.0 as usize;
        let watcher = watcher::spawn_dir_watcher(folder, move |_names| {
            unsafe {
                PostMessageW(
                    Some(HWND(hwnd2 as *mut c_void)),
                    WM_APP_REFRESH,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        });
        g.watchers.push(watcher);
    }
    sync_config(g);
    id
}

pub fn delete_fence(g: &mut Global, idx: usize) {
    if idx >= g.fences.len() {
        return;
    }
    let f = &g.fences[idx];
    unsafe {
        windows::Win32::System::Ole::RevokeDragDrop(f.hwnd);
        DestroyWindow(f.hwnd);
    }
    g.fences.remove(idx);
    sync_config(g);
}

fn sync_config(g: &mut Global) {
    g.config.fences = fence::config_snapshot(&g.fences);
    config::save(&g.config);
}

fn apply_visibility(g: &mut Global) {
    for f in &g.fences {
        if !f.valid {
            continue;
        }
        unsafe {
            if g.zen {
                ShowWindow(f.hwnd, SW_HIDE);
            } else {
                ShowWindow(f.hwnd, SW_SHOWNA);
            }
        }
    }
}

// ---------- 桌面宿主重连(Explorer 重启防护) ----------

fn watchdog_tick(g: &mut Global) {
    // 窗口已独立于桌面层(不挂 Progman),无需宿主检测;
    // 之前 EnumWindows + SendMessageW(0x052C) 在 Progman 无响应时会卡死主线程
    for f in g.fences.iter_mut() {
        if !f.valid {
            // 窗口被 Explorer 销毁,重建
            let cfg = f.cfg.clone();
            // 不挂 Progman(分层窗口+高 alpha+Progman 父窗口会触发 DWM 命中测试 bug,
    // 导致窗口可见但点不到拖不动);改为独立顶层窗口 + 压底 Z 序(同 Fluid Fences 思路)
    let hwnd = fence::create_window(&cfg, None);
            if !hwnd.is_invalid() {
                let dt = droptarget::FenceDropTarget::new(hwnd);
                let it: windows::Win32::System::Ole::IDropTarget = dt.into();
                unsafe { let _ = RegisterDragDrop(hwnd, &it); }
                g.droptargets.push(it);
                f.hwnd = hwnd;
                f.valid = true;
                f.moving = false;
                f.resizing = None;
                if g.zen {
                    unsafe { ShowWindow(hwnd, SW_HIDE); }
                }
                fence::refresh_entries(f, &config::vault_dir(&g.config));
                fence::render_fence(&mut g.icons, g.config.ghost_mode, f);
            }
        }
    }
}

// ---------- 拖放处理 ----------

pub fn handle_drop(hwnd: HWND, paths: Vec<String>) {
    with_global(|g| {
        let Some(idx) = g.fences.iter().position(|f| f.valid && f.hwnd == hwnd) else {
            return;
        };
        let target: Option<PathBuf> = g.fences[idx].cfg.folder.clone().or_else(|| {
            let v = config::vault_dir(&g.config);
            config::ensure_dir(&v).then_some(v)
        });
        let Some(target) = target else { return };
        let mut moved = 0usize;
        for p in &paths {
            let src = PathBuf::from(p);
            if !src.exists() {
                continue;
            }
            // 已在目标目录里则跳过
            if src.parent().map(|d| d == target.as_path()).unwrap_or(false) {
                continue;
            }
            match watcher::move_to_dir(&src, &target) {
                Ok(_) => moved += 1,
                Err(e) => eprintln!("[feather] move {p} -> {} failed: {e}", target.display()),
            }
        }
        if moved > 0 {
            unsafe { PostMessageW(Some(hwnd), WM_APP_DROP, WPARAM(0), LPARAM(0)); }
        }
    });
}

// ---------- 自动归类 ----------

fn ext_of(path: &Path) -> String {
    path.extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

pub fn sweep_desktop(g: &mut Global) {
    let Some(dir) = desktop_dir() else { return };
    let rules = g.config.sweep_rules.clone();
    if rules.is_empty() {
        return;
    }
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let ext = ext_of(&p);
        if let Some(rule) = rules.iter().find(|r| r.ext.to_lowercase() == ext) {
            match watcher::move_to_dir(&p, &rule.dest) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[feather] sweep {:?}: {e}", p);
                    g.sweep_retry.push((p, rule.dest.clone()));
                }
            }
        }
    }
}

fn sweep_retry_tick(g: &mut Global) {
    let mut keep = Vec::new();
    for (src, dest) in std::mem::take(&mut g.sweep_retry) {
        if src.exists() {
            match watcher::move_to_dir(&src, &dest) {
                Ok(_) => {}
                Err(_) => keep.push((src, dest)),
            }
        }
    }
    g.sweep_retry = keep;
}

fn desktop_dir() -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Desktop, windows::Win32::UI::Shell::KNOWN_FOLDER_FLAG(0), None).ok()?;
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
            let (zen, ghost, autostart) = with_global(|g| (g.zen, g.config.ghost_mode, g.config.autostart));
            let cmd = show_tray_menu(hwnd, zen, ghost, autostart);
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
            _ => {}
        }
        return LRESULT(0);
    }
    if msg == WM_APP_SWEEP {
        with_global(|g| sweep_desktop(g));
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
                    let cfg = FenceCfg {
                        id: g.next_id,
                        title,
                        folder: Some(folder),
                        x: sw - (340.0 * s) as i32,
                        y: (100.0 * s) as i32 + (g.fences.len() as i32 % 5) * (40.0 * s) as i32,
                        w: (280.0 * s) as i32,
                        h: (340.0 * s) as i32,
                        opacity: 0.74,
                        icon: 32,
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
                        folder: Some(dir),
                        x: sw - (320.0 * s) as i32,
                        y: (100.0 * s) as i32 + (g.fences.len() as i32 % 5) * (40.0 * s) as i32,
                        w: (260.0 * s) as i32,
                        h: (340.0 * s) as i32,
                        opacity: 0.74,
                        icon: 32,
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
            unsafe { PostMessageW(
                Some(with_global(|g| g.msg_hwnd)),
                WM_APP_SWEEP,
                WPARAM(0),
                LPARAM(0),
            ) };
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
                g.config = c;
                // 先销毁全部旧窗口(避免持借用调用 DestroyWindow)
                let hwnds: Vec<HWND> = g.fences.iter().filter(|f| f.valid).map(|f| f.hwnd).collect();
                for h in hwnds {
                    unsafe {
                        windows::Win32::System::Ole::RevokeDragDrop(h);
                        DestroyWindow(h);
                    }
                }
                g.fences.clear();
                g.droptargets.clear();
                g.watchers.clear();
                for cfg in g.config.fences.clone() {
                    create_fence(g, cfg);
                }
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
            unsafe { PostMessageW(Some(with_global(|g| g.msg_hwnd)), WM_QUIT, WPARAM(0), LPARAM(0)); }
        }
        _ => {}
    }
}

// ---------- main ----------

fn main() {
    dlog("[main] start");
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
            Some(HWND_MESSAGE),
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
    fence::set_icon_px(cfg.icon);
    let vault = config::vault_dir(&cfg);
    let _ = std::fs::create_dir_all(&vault);

    G.set(Mutex::new(Global {
        config: cfg.clone(),
        next_id: cfg.fences.iter().map(|f| f.id).max().unwrap_or(0) + 1,
        fences: Vec::new(),
        msg_hwnd,
        zen: false,
        desktop_host: None,
        icons: icons::IconCache::new(),
        sweep_retry: Vec::new(),
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
            TID_SWEEP_RETRY,
            2000,
            None,
        );
    }

    // 恢复配置里的栅栏
    let fences = cfg.fences.clone();
    dlog(&format!("[main] restoring {} fences", fences.len()));
    with_global(|g| {
        for fcfg in &fences {
            create_fence(g, fcfg.clone());
        }
        // 首启:没有栅栏就建一个默认收纳箱(右侧),并保存配置
        if g.fences.is_empty() {
            let (sw, _sh) = utils::screen_size();
            let s = fence::dpi_scale();
            let box_cfg = FenceCfg {
                id: g.next_id,
                title: "收纳箱".into(),
                folder: None,
                x: sw - (320.0 * s) as i32,
                y: (100.0 * s) as i32,
                w: (260.0 * s) as i32,
                h: (340.0 * s) as i32,
                opacity: 0.74,
                icon: 32,
            };
            // 创建成功才保存,避免失败时把配置覆盖成空
            if create_fence(g, box_cfg) != 0 {
                sync_config(g);
            }
        }
        // 网格落位:恢复后把所有栅栏吸附到整数槽位、clamp 进工作区,
        // 并推挤消除重叠 —— 重启后布局也保持规整
        let n = g.fences.len();
        for i in 0..n {
            fence::settle_fence(g, i);
        }
        // 桌面自动归类监听:线程里只做扩展名粗筛,命中就通知主线程执行整理
        if let Some(dir) = desktop_dir() {
            let rules = g.config.sweep_rules.clone();
            let mhwnd = g.msg_hwnd.0 as usize;
            let watcher = watcher::spawn_dir_watcher(dir.clone(), move |names| {
                if rules.is_empty() {
                    return;
                }
                for n in names {
                    let ext = Path::new(&n)
                        .extension()
                        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
                        .unwrap_or_default();
                    if rules.iter().any(|r| r.ext.to_lowercase() == ext) {
                        unsafe {
                            PostMessageW(
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
            g.watchers.push(watcher);
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
                let cls = unsafe {
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
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // 清理
    with_global(|g| {
        g.exiting = true;
        config::save(&g.config);
        for f in g.fences.iter() {
            if f.valid {
                unsafe { windows::Win32::System::Ole::RevokeDragDrop(f.hwnd); }
            }
        }
    });
    unsafe {
        remove_tray(msg_hwnd);
        DestroyWindow(msg_hwnd);
        GdiplusShutdown(token);
        OleUninitialize();
        CloseHandle(mutex);
    }
    eprintln!("[feather] bye");
}
