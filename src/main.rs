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
mod tray;
mod utils;
mod watcher;

use std::ffi::c_void;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError, HWND, LPARAM, LRESULT,
    RECT, SetLastError, WPARAM,
};
use windows::Win32::Graphics::GdiPlus::{GdiplusShutdown, GdiplusStartup, GdiplusStartupInput, GdiplusStartupOutput};
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
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, GetWindow, GetWindowRect, HWND_TOP, IsIconic, IsWindow,
    IsWindowVisible, PostMessageW, PostQuitMessage, RegisterClassW, SetWindowPos,
    ShowWindow, GW_HWNDPREV,
    TranslateMessage, WM_APP, WM_DESTROY, WM_HOTKEY, WM_QUIT, WM_TIMER, WNDCLASSW,
    WS_POPUP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_SHOWNA,
    SW_SHOWNOACTIVATE,
};
use windows::Win32::System::Ole::RegisterDragDrop;

use config::{Config, FenceCfg};
use fence::{Fence, WM_APP_DROP};
use tray::{
    WM_APP_TRAY, MENU_AUTOSTART, MENU_CONFIG_DIR, MENU_DOWNLOAD_ENABLED,
    MENU_DOWNLOAD_VISIBLE, MENU_EXIT, MENU_GHOST, MENU_NEW_BOX, MENU_NEW_PORTAL, MENU_RELOAD,
    MENU_SWEEP, MENU_TOGGLE_VIS, MENU_ZEN, add_tray, make_tray_icon, remove_tray, show_tray_menu,
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
    /// 桌面监听线程传来的文件名；主线程等待写入稳定后移入下载收纳箱。
    pub desktop_rx: Receiver<Vec<String>>,
    pub desktop_seen: HashSet<PathBuf>,
    pub download_rx: Receiver<Vec<String>>,
    pub download_pending: HashMap<PathBuf, DownloadCandidate>,
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

pub struct DownloadCandidate {
    len: u64,
    modified: Option<std::time::SystemTime>,
    stable_ticks: u8,
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
    // 恢复配置时先按保存 DPI 钳制;窗口创建后再按实际窗口 DPI 做最终换算。
    // 若这里使用系统 DPI,主屏 200% + 副屏 100% 会在创建副屏窗口前把尺寸错误放大。
    if cfg.dpi != 0 {
        let saved_scale = cfg.dpi as f32 / 96.0;
        if cfg.w < fence::min_w(saved_scale) {
            cfg.w = fence::min_w(saved_scale);
        }
        if cfg.h < fence::min_h(saved_scale) {
            cfg.h = fence::min_h(saved_scale);
        }
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
    // v3 持久化保留物理屏幕位置,仅按保存时 DPI → 当前窗口 DPI 换算尺寸。
    // 不能用系统 DPI 统一恢复:混合缩放多屏会把副屏窗口漂回主屏坐标。
    let saved_dpi = f.cfg.dpi;
    let saved_w = f.cfg.w;
    let saved_h = f.cfg.h;
    let mut current_dpi = (f.dpi * 96.0).round() as u32;
    let mut converged = false;
    // 尺寸变化可能让跨屏窗口的主显示器切换;重新读取实际 DPI,最多再换算一次。
    for _ in 0..2 {
        f.dpi = current_dpi as f32 / 96.0;
        let restored_w = config::scale_extent_for_dpi(saved_w, saved_dpi, current_dpi)
            .max(fence::min_w(f.dpi));
        let restored_h = config::scale_extent_for_dpi(saved_h, saved_dpi, current_dpi)
            .max(fence::min_h(f.dpi));
        if restored_w != f.cfg.w || restored_h != f.cfg.h {
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    f.cfg.x,
                    f.cfg.y,
                    restored_w,
                    restored_h,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            f.cfg.w = restored_w;
            f.cfg.h = restored_h;
        }
        let observed_dpi = (fence::window_dpi(hwnd) * 96.0).round() as u32;
        if observed_dpi == current_dpi {
            converged = true;
            break;
        }
        current_dpi = observed_dpi;
    }
    if !converged {
        // 窗口卡在混合 DPI 屏幕边界时可能 A→B→A 振荡。选择当前实际显示器,
        // 按其 DPI 计算尺寸并把窗口完整钳进工作区,得到确定的终止状态。
        f.dpi = current_dpi as f32 / 96.0;
        let wa = utils::work_area(hwnd);
        let restored_w = config::scale_extent_for_dpi(saved_w, saved_dpi, current_dpi)
            .max(fence::min_w(f.dpi))
            .min(wa.right - wa.left);
        let restored_h = config::scale_extent_for_dpi(saved_h, saved_dpi, current_dpi)
            .max(fence::min_h(f.dpi))
            .min(wa.bottom - wa.top);
        let x = f.cfg.x.clamp(wa.left, (wa.right - restored_w).max(wa.left));
        let y = f.cfg.y.clamp(wa.top, (wa.bottom - restored_h).max(wa.top));
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                x,
                y,
                restored_w,
                restored_h,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
    // 边界窗口可能在两种尺寸间切换主显示器。无论是否收敛,最终都以
    // Win32 的实际 DPI 和窗口矩形为准,避免 cfg.dpi、f.dpi、w/h 互相矛盾。
    f.dpi = fence::window_dpi(hwnd);
    let mut final_rect = RECT::default();
    unsafe { let _ = GetWindowRect(hwnd, &mut final_rect); }
    f.cfg.x = final_rect.left;
    f.cfg.y = final_rect.top;
    f.cfg.w = (final_rect.right - final_rect.left).max(1);
    f.cfg.h = (final_rect.bottom - final_rect.top).max(1);
    f.cfg.dpi = (f.dpi * 96.0).round() as u32;
    fence::refresh_entries(&mut f, &config::vault_dir(&g.config));
    fence::render_fence(&mut g.icons, g.config.ghost_mode, &mut f);
    let id = f.cfg.id;
    g.fences.push(f);
    // 新栅栏立即落到网格:尺寸/位置吸附 + clamp 工作区 + 消除重叠
    let new_idx = g.fences.len() - 1;
    fence::settle_fence(g, new_idx);

    // 门户目录监听
    if let Some((folder, refresh_signal)) = g.fences.last().and_then(|f| {
        f.cfg
            .folder
            .clone()
            .map(|folder| (folder, f.refresh_signal.clone()))
    }) {
        let hwnd2 = hwnd.0 as usize;
        let watcher = watcher::spawn_dir_watcher(folder, move |_names| {
            refresh_signal.post(HWND(hwnd2 as *mut c_void));
        });
        g.watchers.push(ManagedWatcher::fence(id, watcher));
    }
    sync_config(g);
    id
}

pub fn delete_fence(g: &mut Global, idx: usize) {
    if idx >= g.fences.len() {
        return;
    }
    // 先从全局状态移除，DestroyWindow 同步派发 WM_DESTROY 时就不会把条目标成
    // “意外失效”并被 watchdog 重建。对应监听器在窗口销毁前停止，避免继续投递刷新。
    let f = g.fences.remove(idx);
    g.watchers
        .retain(|watcher| watcher.owner != WatcherOwner::Fence(f.cfg.id));
    unsafe {
        let _ = windows::Win32::System::Ole::RevokeDragDrop(f.hwnd);
        let _ = DestroyWindow(f.hwnd);
    }
    sync_config(g);
}

fn ensure_download_box(g: &mut Global) {
    let dir = config::download_box_dir();
    let exists = g
        .config
        .download_box_id
        .is_some_and(|id| g.fences.iter().any(|f| f.valid && f.cfg.id == id));
    if exists {
        return;
    }
    if let Some(id) = g
        .fences
        .iter()
        .find(|f| f.valid && f.cfg.folder.as_deref() == Some(dir.as_path()))
        .map(|f| f.cfg.id)
    {
        g.config.download_box_id = Some(id);
        sync_config(g);
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    let (sw, _sh) = utils::screen_size();
    let s = fence::dpi_scale();
    let cfg = FenceCfg {
        id: 0,
        title: "下载收纳箱".into(),
        folder: Some(dir),
        x: sw - (320.0 * s) as i32,
        y: (100.0 * s) as i32,
        w: (260.0 * s) as i32,
        h: (340.0 * s) as i32,
        dpi: (96.0 * s).round() as u32,
        opacity: 0.7,
        icon: 32,
    };
    let id = create_fence(g, cfg);
    if id != 0 {
        g.config.download_box_id = Some(id);
        sync_config(g);
    }
}

fn is_download_box(g: &Global, id: u32) -> bool {
    g.config.download_box_id == Some(id)
}

fn download_box_should_show(g: &Global, id: u32) -> bool {
    !is_download_box(g, id) || (g.config.download_enabled && g.config.download_box_visible)
}

fn downloads_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(|p| PathBuf::from(p).join("Downloads"))
}

fn reset_download_tracking(g: &mut Global) {
    while g.download_rx.try_recv().is_ok() {}
    g.download_pending.clear();
    g.desktop_seen = downloads_dir()
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
}

pub fn set_download_enabled(g: &mut Global, enabled: bool) {
    if g.config.download_enabled == enabled {
        return;
    }
    g.config.download_enabled = enabled;
    reset_download_tracking(g);
    apply_visibility(g);
    reserve_desktop_icons(g);
    config::save(&g.config);
}

pub fn set_download_box_visible(g: &mut Global, visible: bool) {
    if g.config.download_box_visible == visible {
        return;
    }
    g.config.download_box_visible = visible;
    apply_visibility(g);
    reserve_desktop_icons(g);
    config::save(&g.config);
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
            if g.zen || !download_box_should_show(g, f.cfg.id) {
                let _ = ShowWindow(f.hwnd, SW_HIDE);
            } else {
                let _ = ShowWindow(f.hwnd, SW_SHOWNA);
            }
        }
    }
}

pub fn reserve_desktop_icons(g: &Global) {
    let rects: Vec<RECT> = g
        .fences
        .iter()
        .filter(|f| f.valid && download_box_should_show(g, f.cfg.id))
        .map(|f| RECT {
            left: f.cfg.x,
            top: f.cfg.y,
            right: f.cfg.x + f.cfg.w,
            bottom: f.cfg.y + f.cfg.h,
        })
        .collect();
    desktop_icons::reserve(&rects);
}

// ---------- 桌面宿主重连(Explorer 重启防护) ----------

fn watchdog_tick(g: &mut Global) {
    // 窗口已独立于桌面层(不挂 Progman),无需宿主检测;
    // 之前 EnumWindows + SendMessageW(0x052C) 在 Progman 无响应时会卡死主线程
    let download_id = g.config.download_box_id;
    let download_shown = g.config.download_enabled && g.config.download_box_visible;
    for f in g.fences.iter_mut() {
        let intentionally_hidden = download_id == Some(f.cfg.id) && !download_shown;
        if f.valid && !g.zen && !intentionally_hidden {
            let hidden_or_minimized = unsafe {
                IsIconic(f.hwnd).as_bool() || !IsWindowVisible(f.hwnd).as_bool()
            };
            if hidden_or_minimized {
                unsafe { let _ = ShowWindow(f.hwnd, SW_SHOWNOACTIVATE); }
                fence::render_fence(&mut g.icons, g.config.ghost_mode, f);
            }
        }
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
                    unsafe { let _ = ShowWindow(hwnd, SW_HIDE); };
                }
                fence::refresh_entries(f, &config::vault_dir(&g.config));
                fence::render_fence(&mut g.icons, g.config.ghost_mode, f);
            }
        }
    }
    // Explorer 重启、用户刷新桌面或新图标出现后，再次维护禁放区。
    reserve_desktop_icons(g);
}

/// 维护严格的“所有应用窗口 > 栅栏 > Explorer 桌面”层级。
/// 栅栏永不使用 TOPMOST；Show Desktop 改写 Z 序后，也只把它插回桌面宿主正上方。
fn desktop_layer_tick(g: &mut Global) {
    if g.zen {
        return;
    }
    let host_valid = g.desktop_host.is_some_and(|h| unsafe { IsWindow(Some(h)).as_bool() });
    if !host_valid {
        g.desktop_host = utils::find_desktop_host();
    }
    let Some(host) = g.desktop_host else { return };
    let mut anchor = host;
    for f in g
        .fences
        .iter()
        .filter(|f| f.valid && download_box_should_show(g, f.cfg.id))
    {
        unsafe {
            if IsIconic(f.hwnd).as_bool() || !IsWindowVisible(f.hwnd).as_bool() {
                let _ = ShowWindow(f.hwnd, SW_SHOWNOACTIVATE);
            }
            let above = GetWindow(anchor, GW_HWNDPREV).unwrap_or(HWND_TOP);
            if above != f.hwnd {
                let _ = SetWindowPos(
                    f.hwnd,
                    Some(above),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        anchor = f.hwnd;
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
            unsafe { let _ = PostMessageW(Some(hwnd), WM_APP_DROP, WPARAM(0), LPARAM(0)); };
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
    if g.config.download_enabled {
        ingest_desktop_events(g);
    }
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
        // 新下载优先进入下载收纳箱，不被扩展名清扫规则抢走。
        if g.download_pending.contains_key(&p) {
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

fn is_download_temp(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("crdownload" | "part" | "partial" | "download" | "tmp")
    )
}

fn ingest_desktop_events(g: &mut Global) {
    let Some(downloads) = downloads_dir() else { return };
    while let Ok(names) = g.download_rx.try_recv() {
        for name in names {
            let path = downloads.join(name);
            if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case("desktop.ini")) {
                continue;
            }
            if path.is_file() && !is_download_temp(&path) && g.desktop_seen.insert(path.clone()) {
                g.download_pending.insert(path, DownloadCandidate {
                    len: u64::MAX,
                    modified: None,
                    stable_ticks: 0,
                });
            }
        }
    }
    // 删除或已移动的路径不应永远占着 seen，允许日后同名下载再次被接管。
    g.desktop_seen.retain(|p| p.exists());
}

fn download_target(g: &Global) -> PathBuf {
    g.config
        .download_box_id
        .and_then(|id| g.fences.iter().find(|f| f.valid && f.cfg.id == id))
        .and_then(|f| f.cfg.folder.clone())
        .unwrap_or_else(config::download_box_dir)
}

fn download_tick(g: &mut Global) {
    if !g.config.download_enabled {
        while g.download_rx.try_recv().is_ok() {}
        g.download_pending.clear();
        return;
    }
    ingest_desktop_events(g);
    let target = download_target(g);
    let mut completed = Vec::new();
    for (path, state) in g.download_pending.iter_mut() {
        let Ok(meta) = std::fs::metadata(path) else {
            completed.push(path.clone());
            continue;
        };
        if !meta.is_file() {
            completed.push(path.clone());
            continue;
        }
        let modified = meta.modified().ok();
        if state.len == meta.len() && state.modified == modified {
            state.stable_ticks = state.stable_ticks.saturating_add(1);
        } else {
            state.len = meta.len();
            state.modified = modified;
            state.stable_ticks = 0;
        }
        // 连续约两秒无尺寸/时间变化后再移动，避免截断仍在写入的浏览器下载。
        if state.stable_ticks >= 2 && watcher::move_to_dir(path, &target).is_ok() {
            completed.push(path.clone());
        }
    }
    if completed.is_empty() {
        return;
    }
    for path in completed {
        g.download_pending.remove(&path);
        g.desktop_seen.remove(&path);
    }
    if let Some(id) = g.config.download_box_id {
        if let Some(f) = g.fences.iter_mut().find(|f| f.valid && f.cfg.id == id) {
            fence::refresh_entries(f, &config::vault_dir(&g.config));
            fence::render_fence(&mut g.icons, g.config.ghost_mode, f);
        }
    }
    reserve_desktop_icons(g);
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
            let (zen, ghost, autostart, download_enabled, download_visible) = with_global(|g| {
                (
                    g.zen,
                    g.config.ghost_mode,
                    g.config.autostart,
                    g.config.download_enabled,
                    g.config.download_box_visible,
                )
            });
            let cmd = show_tray_menu(
                hwnd,
                zen,
                ghost,
                autostart,
                download_enabled,
                download_visible,
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
            TID_DOWNLOADS => with_global(|g| download_tick(g)),
            TID_DESKTOP_LAYER => with_global(|g| desktop_layer_tick(g)),
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
                        dpi: (96.0 * s).round() as u32,
                        opacity: 0.7,
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
                        dpi: (96.0 * s).round() as u32,
                        opacity: 0.7,
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
                g.fences.clear();
                g.droptargets.clear();
                for cfg in g.config.fences.clone() {
                    create_fence(g, cfg);
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
    fence::set_icon_px(cfg.icon);
    let vault = config::vault_dir(&cfg);
    let _ = std::fs::create_dir_all(&vault);

    let (desktop_tx, desktop_rx) = mpsc::channel::<Vec<String>>();
    let (download_tx, download_rx) = mpsc::channel::<Vec<String>>();
    let desktop_seen = downloads_dir()
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
        desktop_seen,
        download_rx,
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
        for fcfg in &fences {
            create_fence(g, fcfg.clone());
        }
        // 始终保留专用下载收纳箱；是否接管/显示由两个独立配置控制。
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
            let watcher = watcher::spawn_dir_watcher(dir.clone(), move |names| {
                let _ = tx.send(names.clone());
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
        // 下载收纳箱：单独监听 Downloads 目录，避免把桌面所有文件都当下载。
        if let Some(dir) = downloads_dir() {
            let tx = download_tx.clone();
            let watcher = watcher::spawn_dir_watcher(dir, move |names| {
                let _ = tx.send(names.clone());
            });
            g.watchers.push(ManagedWatcher::process(watcher));
        }
        reserve_desktop_icons(g);
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
