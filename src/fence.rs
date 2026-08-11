// 栅栏窗口:分层窗口(WS_EX_LAYERED)+ UpdateLayeredWindow 整幅提交(逐像素 alpha)。
// 半透明深色面板 = 真透明(直接透出桌面,无模糊);内容(标题/图标)不透明。
// 圆角由 DWM 裁(DWMWCP_ROUND 对分层窗口同样生效)。
// 注:原生 DWM 亚克力(系统背景)与 GDI 内容不兼容 —— 一画内容整窗就物化成不透明
// 表面盖死磨砂且无法还原,故放弃;分层窗口 + 逐像素 alpha 是唯一可靠方案。
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateRectRgn, DeleteDC,
    DeleteObject, EndPaint, SelectClipRgn, SelectObject, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, HBRUSH, HBITMAP,
    HDC, HGDIOBJ, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
};
use windows::Win32::Graphics::GdiPlus::{
    GdipAddPathArc, GdipAddPathEllipse, GdipClosePathFigure, GdipCreateFont,
    GdipCreateFontFamilyFromName, GdipCreateFromHDC, GdipCreatePath, GdipCreateSolidFill,
    GdipCreateStringFormat, GdipDeleteBrush, GdipDeleteFont, GdipDeleteFontFamily,
    GdipDeleteGraphics, GdipDeletePath, GdipDeleteStringFormat, GdipDrawString, GdipFillPath,
    GdipFillRectangle, GdipFlush, GdipMeasureString, GdipResetClip, GdipSetClipRect,
    GdipSetSmoothingMode, GdipSetStringFormatAlign, GdipSetStringFormatFlags,
    GdipSetStringFormatLineAlign, GdipSetStringFormatTrimming, GdipSetTextRenderingHint,
    CombineModeReplace, FillModeAlternate, FlushIntentionSync, FontStyleRegular, GpBrush, GpFont,
    GpFontFamily, GpGraphics, GpPath, GpSolidFill, GpStringFormat, RectF, SmoothingModeAntiAlias,
    StringAlignmentCenter, StringAlignmentNear, StringFormatFlagsNoWrap,
    StringTrimmingEllipsisCharacter, TextRenderingHintAntiAliasGridFit, UnitPixel,
};
use windows::Win32::UI::Shell::{
    ShellExecuteW, SHFileOperationW, SHFILEOPSTRUCTW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION,
    FOF_NOERRORUI, FO_DELETE,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetActiveWindow, SetCapture, SetFocus, TrackMouseEvent, VK_DELETE, TME_LEAVE,
    TRACKMOUSEEVENT, TRACKMOUSEEVENT_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, DrawIconEx, GetCursorPos, GetMessageW, GetWindowRect, GetSystemMetrics,
    GetWindowTextW, IsDialogMessageW, IsWindow, KillTimer, LoadCursorW, PostMessageW, RegisterClassW,
    SetCursor, SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow, TrackPopupMenu, BS_DEFPUSHBUTTON,
    BS_PUSHBUTTON, CS_DBLCLKS, ES_AUTOHSCROLL, HICON, HMENU, HTCLIENT, IDC_ARROW, IDC_SIZENESW,
    IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_SIZEALL, MF_CHECKED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, MSG, SendMessageW, SM_CXDRAG, SM_CYDRAG, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOW,
    SW_SHOWNA, SW_SHOWNOACTIVATE, SW_SHOWNORMAL, DI_NORMAL, SC_MINIMIZE, SIZE_MINIMIZED,
    TPM_NONOTIFY, TPM_RETURNCMD, TranslateMessage,
    ULW_ALPHA, UpdateLayeredWindow, WINDOW_STYLE, WNDCLASSW, WM_APP,
    WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCHITTEST, WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR, WM_SETFONT,
    WM_SIZE, WM_SYSCOMMAND, WM_TIMER,
    WM_DISPLAYCHANGE, WM_DPICHANGED,
    WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_DLGMODALFRAME, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

use crate::{with_global, Global};
use windows::core::w;
use crate::config::FenceCfg;
use crate::utils::{wstr, work_area};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND};

pub const WM_APP_REFRESH: u32 = WM_APP + 1;
pub const WM_APP_DROP: u32 = WM_APP + 2;
/// “显示桌面”会尝试最小化所有独立顶层窗口；异步恢复可避免在 WM_SIZE 内递归。
pub const WM_APP_DESKTOP_RESTORE: u32 = WM_APP + 20;

// --- 圆角:DWM 裁(DWMWCP_ROUND 对分层窗口同样生效) ---
fn enable_round(hwnd: HWND) {
    unsafe {
        let r = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &r as *const _ as *const std::ffi::c_void,
            size_of::<windows::Win32::Graphics::Dwm::DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
    }
}

/// 系统 DPI 缩放因子(200% 缩放 = 2.0);窗口/字体按物理像素工作,必须乘这个因子。
/// 用于无窗口场景(重命名对话框、新建栅栏的 min 钳制);
/// 窗口相关几何一律用 window_dpi(hwnd) / f.dpi,按窗口所在显示器缩放。
pub fn dpi_scale() -> f32 {
    unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem() as f32 / 96.0 }.max(1.0)
}
/// 窗口所在显示器 DPI 缩放因子(Per-Monitor):
/// 副屏与主屏缩放不同时,按窗口实际所在屏缩放,而非系统(主屏)DPI
pub fn window_dpi(hwnd: HWND) -> f32 {
    unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) as f32 / 96.0 }.max(1.0)
}
pub fn title_h(d: f32) -> i32 {
    (30.0 * d) as i32
}
fn edge(d: f32) -> i32 {
    (8.0 * d) as i32
}
fn margin(d: f32) -> i32 {
    (10.0 * d) as i32
}
/// 页面圆点轨道宽度:图标网格让出右侧竖条,圆点不压到图标上
fn rail(d: f32) -> i32 {
    (22.0 * d) as i32
}
/// 全局图标尺寸(逻辑像素):所有栅栏统一
static ICON_PX: AtomicU32 = AtomicU32::new(32);
/// 设置全局图标尺寸(物理显示时再乘 DPI)
pub fn set_icon_px(v: u32) {
    ICON_PX.store(v.max(16).min(128), AtomicOrdering::Relaxed);
}
/// 图标尺寸(物理像素,按所在屏 DPI 缩放);取全局值,0 时回退 32
fn icon(f: &Fence) -> i32 {
    let base = ICON_PX.load(AtomicOrdering::Relaxed);
    let base = if base == 0 { 32 } else { base };
    (base as f32 * f.dpi).round() as i32
}
fn label_h(d: f32) -> i32 {
    // 容纳 12px 原生字号的两行标签(换行) + 投影
    (38.0 * d) as i32
}
fn cell_w(f: &Fence) -> i32 {
    icon(f) + 12
}
fn cell_h(f: &Fence) -> i32 {
    icon(f) + label_h(f.dpi)
}
pub fn min_w(d: f32) -> i32 {
    (180.0 * d) as i32
}
pub fn min_h(d: f32) -> i32 {
    (100.0 * d) as i32
}
fn font_title(d: f32) -> f32 {
    11.5 * d
}
fn font_label(d: f32) -> f32 {
    // Windows 桌面图标的标签字号:9pt = 12px(逻辑像素,随 DPI 缩放)
    12.0 * d
}
const FONT_NAME: &str = "Microsoft YaHei UI";

#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

struct RefreshState {
    queued: bool,
    last_event: Instant,
}

impl Default for RefreshState {
    fn default() -> Self {
        Self {
            queued: false,
            last_event: Instant::now(),
        }
    }
}

impl RefreshState {
    fn record_event(&mut self, now: Instant) -> bool {
        self.last_event = now;
        if self.queued {
            false
        } else {
            self.queued = true;
            true
        }
    }

    fn timer_action(&mut self, now: Instant, delay: Duration) -> RefreshTimerAction {
        if !self.queued {
            return RefreshTimerAction::Idle;
        }
        let elapsed = now.saturating_duration_since(self.last_event);
        if elapsed < delay {
            let remaining = delay - elapsed;
            return RefreshTimerAction::Wait(
                remaining.as_millis().clamp(1, u32::MAX as u128) as u32,
            );
        }
        self.queued = false;
        RefreshTimerAction::Refresh
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefreshTimerAction {
    Idle,
    Wait(u32),
    Refresh,
}

#[derive(Clone, Default)]
pub struct RefreshSignal {
    state: Arc<Mutex<RefreshState>>,
}

impl RefreshSignal {
    /// 同一栅栏最多排队一条刷新消息，避免目录事件风暴淹没 UI 消息队列。
    pub fn post(&self, hwnd: HWND) {
        let should_post = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record_event(Instant::now());
        if !should_post {
            return;
        }
        let posted = unsafe {
            PostMessageW(Some(hwnd), WM_APP_REFRESH, WPARAM(0), LPARAM(0))
        };
        if posted.is_err() {
            self.cancel();
        }
    }

    fn timer_action(&self) -> RefreshTimerAction {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .timer_action(
                Instant::now(),
                Duration::from_millis(REFRESH_DEBOUNCE_MS as u64),
            )
    }

    fn cancel(&self) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .queued = false;
    }
}

unsafe impl Send for Fence {}

#[derive(Clone, Copy, PartialEq)]
pub enum ResizeDir {
    N,
    S,
    E,
    W,
    NW,
    NE,
    SW,
    SE,
}

pub struct Fence {
    pub cfg: FenceCfg,
    pub hwnd: HWND,
    /// 所在显示器的 DPI 缩放因子(Per-Monitor)。窗口跨屏/缩放变化时由 WM_DPICHANGED 更新。
    pub dpi: f32,
    pub entries: Vec<Entry>,
    /// 当前页(0 基);滚动按整页切换
    pub page: usize,
    /// 网格顶部行号(浮点):翻页动画中平滑变化,静止时 = page × rows
    pub top_row: f32,
    /// 翻页动画计时器是否在跑
    pub animating: bool,
    /// 翻页动画已推进帧数 + 起始 top_row(固定时长 ease-out 用)
    pub anim_frames: u32,
    pub anim_from: f32,
    /// 滚轮增量累加器(1/120 刻度):触控板/高精度滚轮的小增量先累积,满 120 再翻页
    pub wheel_acc: i32,
    pub hover: Option<usize>,
    /// 单击选中的条目；Delete 键对它执行移入回收站。
    pub selected: Option<usize>,
    pub moving: bool,
    pub move_off: (i32, i32),
    pub resizing: Option<ResizeDir>,
    pub hover_visible: bool,
    /// 拖出:按下的条目索引(移动超阈值后启动 OLE 拖拽)
    pub drag_idx: Option<usize>,
    /// 拖出:按下时的客户区坐标(拖拽阈值判断用)
    pub drag_down: (i32, i32),
    /// 目录监听线程与窗口消息之间的刷新合并信号。
    pub refresh_signal: RefreshSignal,
    /// 已渲染 DIB 缓存:ULW 整幅提交的源(内容不保留,必须自己存)
    cache: Option<RenderCache>,
    pub valid: bool,
}

impl Fence {
    pub fn new(cfg: FenceCfg, hwnd: HWND) -> Self {
        Fence {
            cfg,
            hwnd,
            dpi: window_dpi(hwnd),
            entries: Vec::new(),
            page: 0,
            top_row: 0.0,
            animating: false,
            anim_frames: 0,
            anim_from: 0.0,
            wheel_acc: 0,
            hover: None,
            selected: None,
            moving: false,
            move_off: (0, 0),
            resizing: None,
            hover_visible: false,
            drag_idx: None,
            drag_down: (0, 0),
            refresh_signal: RefreshSignal::default(),
            cache: None,
            valid: true,
        }
    }
}

/// 已渲染的窗口位图缓存(分层窗口的"内容保留"靠它)。每栅栏一个,
/// 渲染时重画、UpdateLayeredWindow 整幅提交。尺寸变化时重建。
struct RenderCache {
    /// 内存 DC,选中了 hbmp
    mdc: HDC,
    hbmp: HBITMAP,
    /// 位图像素(预乘 alpha 通道就地改)
    bits: *mut u8,
    w: i32,
    h: i32,
}

// Fence 已手动标注 Send(HWND 等裸句柄);RenderCache 随 Fence 走,同样仅主线程访问
unsafe impl Send for RenderCache {}

/// 取/建栅栏的渲染缓存(尺寸匹配则复用,否则重建)。返回像素指针;失败返回 null。
fn ensure_cache(f: &mut Fence, w: i32, h: i32) -> *mut u8 {
    let need_new = match &f.cache {
        Some(c) => c.w != w || c.h != h,
        None => true,
    };
    if need_new {
        if let Some(c) = f.cache.take() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(c.hbmp.0));
                let _ = DeleteDC(c.mdc);
            }
        }
        let mdc = unsafe { CreateCompatibleDC(None) };
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbmp = unsafe { CreateDIBSection(Some(mdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) };
        match hbmp {
            Ok(b) => {
                let _ = unsafe { SelectObject(mdc, HGDIOBJ(b.0)) };
                f.cache = Some(RenderCache {
                    mdc,
                    hbmp: b,
                    bits: bits as *mut u8,
                    w,
                    h,
                });
            }
            Err(_) => {
                let _ = unsafe { DeleteDC(mdc) };
                f.cache = None;
            }
        }
    }
    f.cache.as_ref().map_or(std::ptr::null_mut(), |c| c.bits)
}

/// 把预乘 alpha 的缓存整幅提交(UpdateLayeredWindow)。逐像素 alpha:
/// 透明面板直接透出桌面(无模糊),内容不透明。整幅替换,不会残留旧帧。
unsafe fn submit_ulw(hwnd: HWND, cache: &RenderCache) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rc);
        let mut pos = POINT { x: rc.left, y: rc.top };
        let size = SIZE { cx: cache.w, cy: cache.h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            None,
            Some(&mut pos),
            Some(&size),
            Some(cache.mdc),
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }
}

pub fn register_class() {
    unsafe {
        let wc = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(fence_wndproc),
            hInstance: crate::hinstance(),
            lpszClassName: PCWSTR(w!("FeatherFence").as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        let atom = RegisterClassW(&wc);
        if atom == 0 {
            eprintln!(
                "[feather] RegisterClassW failed: {:?}",
                windows::Win32::Foundation::GetLastError()
            );
        }
    }
}

pub fn create_window(cfg: &FenceCfg, parent: Option<HWND>) -> HWND {
    unsafe {
        let title_w = wstr(&cfg.title);
        let r = CreateWindowExW(
            // 分层窗口 + ULW 整幅提交:逐像素 alpha,半透明面板真透明透出桌面。
            // 圆角由 DWM 裁(DWMWCP_ROUND 对分层窗口同样生效)。
            // 启动时用 SW_SHOWNA 避免抢焦点；用户点击后允许激活，才能接收 Delete。
            WS_EX_TOOLWINDOW | WS_EX_LAYERED,
            w!("FeatherFence"),
            PCWSTR(title_w.as_ptr()),
            WS_POPUP,
            cfg.x,
            cfg.y,
            cfg.w,
            cfg.h,
            parent,
            None,
            Some(crate::hinstance()),
            None,
        );
        let hwnd = match r {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[feather] CreateWindowExW error: {e:?}");
                HWND::default()
            }
        };
        if !hwnd.is_invalid() {
            // 分层窗口:显示后整幅 ULW 提交(逐像素 alpha,透明面板透出桌面)。
            let _ = ShowWindow(hwnd, SW_SHOWNA);
            // 圆角由 DWM 裁
            enable_round(hwnd);
            // 首帧渲染(画进缓存 + ULW 提交)
            schedule_render(hwnd);
            // 自检:程序自己测命中(对比外部诊断,区分桌面/进程视角问题)
            let mut rc = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rc);
            let cx = (rc.left + rc.right) / 2;
            let cy = (rc.top + rc.bottom) / 2;
            let _hit = windows::Win32::UI::WindowsAndMessaging::WindowFromPoint(POINT { x: cx, y: cy });
            crate::dlog(&format!(
                "[feather] created hwnd=0x{:x} at ({},{},{},{})",
                hwnd.0 as usize, rc.left, rc.top, rc.right, rc.bottom
            ));
        }
        hwnd
    }
}

fn low16(v: usize) -> i32 {
    (v & 0xFFFF) as u16 as i16 as i32
}

fn high16(v: usize) -> i32 {
    ((v >> 16) & 0xFFFF) as u16 as i16 as i32
}

fn fence_idx(g: &Global, hwnd: HWND) -> Option<usize> {
    g.fences.iter().position(|f| f.valid && f.hwnd == hwnd)
}

pub fn schedule_render(hwnd: HWND) {
    // 直接渲染(渲染是纯函数,开销毫秒级)
    with_global(|g| {
        if let Some(idx) = fence_idx(g, hwnd) {
            let ghost = g.config.ghost_mode;
            render_fence(&mut g.icons, ghost, &mut g.fences[idx]);
        }
    });
}

/// 刷新栅栏条目:folder 指定则显示该文件夹;否则显示收纳箱(vault)目录。
/// 这样拖入文件(handle_drop 把无 folder 的栅栏文件移进 vault)会立即显示,重启后也持久。
pub fn refresh_entries(f: &mut Fence, vault: &PathBuf) {
    let page = f.page;
    let selected_path = f.selected.and_then(|i| f.entries.get(i)).map(|e| e.path.clone());
    f.entries.clear();
    let dir = f.cfg.folder.clone().unwrap_or_else(|| vault.clone());
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            f.entries.push(Entry { path, name, is_dir });
        }
        f.entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    }
    f.selected = selected_path.and_then(|p| f.entries.iter().position(|e| e.path == p));
    f.page = page;
    f.wheel_acc = 0;
    sync_page(f);
}

#[cfg(test)]
mod refresh_tests {
    use super::{
        grid_dims, refresh_entries, Fence, RefreshState, RefreshTimerAction,
        REFRESH_DEBOUNCE_MS,
    };
    use crate::config::FenceCfg;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use windows::Win32::Foundation::HWND;

    #[test]
    fn refresh_state_coalesces_events_until_the_quiet_period() {
        let mut state = RefreshState::default();
        let start = Instant::now();
        let delay = Duration::from_millis(REFRESH_DEBOUNCE_MS as u64);

        assert!(state.record_event(start));
        assert!(!state.record_event(start + Duration::from_millis(50)));
        assert_eq!(
            state.timer_action(start + Duration::from_millis(150), delay),
            RefreshTimerAction::Wait(50)
        );
        assert_eq!(
            state.timer_action(start + Duration::from_millis(200), delay),
            RefreshTimerAction::Refresh
        );
        assert_eq!(
            state.timer_action(start + Duration::from_millis(201), delay),
            RefreshTimerAction::Idle
        );
        assert!(state.record_event(start + Duration::from_millis(202)));
    }

    #[test]
    fn refresh_preserves_page_and_clamps_when_contents_shrink() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "feather-fences-refresh-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..40 {
            std::fs::write(dir.join(format!("item-{i:02}.txt")), b"item").unwrap();
        }

        let cfg = FenceCfg {
            folder: Some(dir.clone()),
            ..FenceCfg::default()
        };
        let mut fence = Fence::new(cfg, HWND::default());
        fence.page = 1;
        refresh_entries(&mut fence, &dir);
        assert_eq!(fence.page, 1);
        assert_eq!(fence.top_row, grid_dims(&fence).1 as f32);

        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            std::fs::remove_file(entry.path()).unwrap();
        }
        refresh_entries(&mut fence, &dir);
        assert_eq!(fence.page, 0);
        assert_eq!(fence.top_row, 0.0);

        std::fs::remove_dir_all(dir).unwrap();
    }
}

fn recycle_path(hwnd: HWND, path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    // SHFileOperationW 的 pFrom 是双 NUL 结尾的路径列表。
    let mut from: Vec<u16> = path.as_os_str().encode_wide().collect();
    from.push(0);
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        hwnd,
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        pTo: PCWSTR::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    let code = unsafe { SHFileOperationW(&mut op) };
    if code == 0 && !op.fAnyOperationsAborted.as_bool() {
        Ok(())
    } else {
        Err(format!("SHFileOperationW code={code}, aborted={}", op.fAnyOperationsAborted.as_bool()))
    }
}

pub fn fence_menu(hwnd: HWND) {
    unsafe {
        let menu = CreatePopupMenu().unwrap_or_default();
        let is_download = with_global(|g| {
            fence_idx(g, hwnd).is_some_and(|i| g.config.download_box_id == Some(g.fences[i].cfg.id))
        });
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            1001,
            if is_download {
                PCWSTR(w!("关闭下载接管").as_ptr())
            } else {
                PCWSTR(w!("删除此栅栏").as_ptr())
            },
        );
        if is_download {
            let _ = AppendMenuW(menu, MF_STRING, 1010, PCWSTR(w!("隐藏下载收纳箱").as_ptr()));
        }
        let _ = AppendMenuW(menu, MF_STRING, 1011, PCWSTR(w!("打开收纳箱").as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, 1005, PCWSTR(w!("重命名...").as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let cur_opacity = with_global(|g| {
            fence_idx(g, hwnd)
                .map(|i| g.fences[i].cfg.opacity)
                .unwrap_or_default()
        });
        let opacity_presets = [
            (1002usize, 1.0f32, w!("100%")),
            (1003, 0.7, w!("70%")),
            (1004, 0.45, w!("45%")),
            (1012, 0.3, w!("30%")),
        ];
        let selected_opacity_id = opacity_presets
            .iter()
            .min_by(|(_, a, _), (_, b, _)| {
                (cur_opacity - *a)
                    .abs()
                    .total_cmp(&(cur_opacity - *b).abs())
            })
            .map(|(id, _, _)| *id)
            .unwrap_or_default();
        let opacity_menu = CreatePopupMenu().unwrap_or_default();
        for (id, _, label) in opacity_presets {
            let flags = if id == selected_opacity_id {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            };
            let _ = AppendMenuW(opacity_menu, flags, id, PCWSTR(label.as_ptr()));
        }
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            opacity_menu.0 as usize,
            PCWSTR(w!("透明度").as_ptr()),
        );
        // 图标大小子菜单(全局统一)
        let cur_icon = with_global(|g| g.config.icon.max(1));
        let icon_menu = CreatePopupMenu().unwrap_or_default();
        for (id, size) in [(1006u32, 24u32), (1007, 32), (1008, 48), (1009, 64)] {
            let flags = if cur_icon == size { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = AppendMenuW(
                icon_menu,
                flags,
                id as usize,
                PCWSTR(wstr(&format!("{} px", size)).as_ptr()),
            );
        }
        let _ = AppendMenuW(menu, MF_POPUP, icon_menu.0 as usize, PCWSTR(w!("图标大小").as_ptr()));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        let cmd = cmd.0 as u32;
        if cmd == 1001 {
            with_global(|g| {
                if let Some(idx) = g.fences.iter().position(|f| f.hwnd == hwnd) {
                    if g.config.download_box_id == Some(g.fences[idx].cfg.id) {
                        crate::set_download_enabled(g, false);
                        return;
                    }
                    crate::delete_fence(g, idx);
                }
            });
        } else if matches!(cmd, 1002..=1004 | 1012) {
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    let ghost = g.config.ghost_mode;
                    g.fences[idx].cfg.opacity = match cmd {
                        1002 => 1.0,
                        1003 => 0.7,
                        1004 => 0.45,
                        _ => 0.3,
                    };
                    render_fence(&mut g.icons, ghost, &mut g.fences[idx]);
                    g.config.fences = config_snapshot(&g.fences);
                    crate::config::save(&g.config);
                }
            });
        } else if cmd == 1005 {
            // 重命名:弹输入框 → 改 title → 存配置
            rename_fence(hwnd);
        } else if cmd == 1010 {
            with_global(|g| crate::set_download_box_visible(g, false));
        } else if cmd == 1011 {
            let folder = with_global(|g| {
                fence_idx(g, hwnd)
                    .and_then(|i| g.fences[i].cfg.folder.clone())
                    .unwrap_or_else(|| crate::config::vault_dir(&g.config))
            });
            let _ = std::fs::create_dir_all(&folder);
            let folder_w = wstr(&folder.to_string_lossy());
            let _ = ShellExecuteW(
                None,
                PCWSTR(w!("explore").as_ptr()),
                PCWSTR(folder_w.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            );
        } else if (1006..=1009).contains(&cmd) {
            let size = match cmd {
                1006 => 24,
                1007 => 32,
                1008 => 48,
                _ => 64,
            };
            with_global(|g| {
                set_icon_px(size);
                g.config.icon = size;
                // 图标尺寸变化 → 网格槽位/页数全变:所有栅栏重新吸附到新网格
                let n = g.fences.len();
                for i in 0..n {
                    settle_fence(g, i);
                }
            });
        }
    }
}

/// 重命名栅栏:弹输入框,输入非空则更新标题并持久化
fn rename_fence(hwnd: HWND) {
    let current = with_global(|g| {
        g.fences
            .iter()
            .find(|f| f.valid && f.hwnd == hwnd)
            .map(|f| f.cfg.title.clone())
            .unwrap_or_default()
    });
    if let Some(name) = prompt_text(hwnd, "重命名栅栏", &current) {
        let name = name.trim().to_string();
        if !name.is_empty() {
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    let ghost = g.config.ghost_mode;
                    g.fences[idx].cfg.title = name.clone();
                    render_fence(&mut g.icons, ghost, &mut g.fences[idx]);
                    g.config.fences = config_snapshot(&g.fences);
                    crate::config::save(&g.config);
                }
            });
        }
    }
}

// ---- 文本输入对话框(重命名栅栏用)----
static PROMPT_EDIT: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
static PROMPT_RESULT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

unsafe extern "system" fn input_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = (wparam.0 as usize) & 0xFFFF;
                if id == 1 {
                    // 确定:读编辑框文本存入结果
                    let edit = HWND(*PROMPT_EDIT.lock().unwrap() as *mut std::ffi::c_void);
                    let mut buf = [0u16; 512];
                    let n = GetWindowTextW(edit, &mut buf);
                    *PROMPT_RESULT.lock().unwrap() =
                        Some(String::from_utf16_lossy(&buf[..(n.max(0) as usize).min(buf.len())]));
                    let _ = DestroyWindow(hwnd);
                } else if id == 2 {
                    let _ = DestroyWindow(hwnd);
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 弹出单行文本输入对话框,返回输入内容;取消返回 None
fn prompt_text(parent: HWND, title: &str, initial: &str) -> Option<String> {
    static REG: std::sync::Once = std::sync::Once::new();
    REG.call_once(|| unsafe {
        let wc = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(input_wndproc),
            hInstance: crate::hinstance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(16 as *mut std::ffi::c_void),
            lpszClassName: PCWSTR(w!("FeatherInput").as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
    });
    unsafe {
        // 对话框随父栅栏所在显示器的 DPI 缩放(Per-Monitor)
        let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(parent).max(96);
        let s = (dpi as f32 / 96.0).max(1.0);
        let px = |v: f32| (v * s) as i32;

        // —— 客户区布局(所有子控件坐标都相对客户区左上角)——
        let pad = px(18.0); // 四周内边距
        let cw = px(360.0); // 客户区宽
        let edit_y = pad;
        let edit_h = px(30.0);
        let bw = px(88.0); // 按钮宽
        let bh = px(32.0); // 按钮高
        let bgap = px(12.0); // 两按钮间距
        let by = edit_y + edit_h + px(22.0); // 按钮行 Y
        let ch = by + bh + pad; // 客户区高

        // 由客户区尺寸反推整窗尺寸(含标题栏/边框),否则底部按钮会被裁掉
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let exstyle = WS_EX_DLGMODALFRAME | WS_EX_TOOLWINDOW;
        let mut wr = RECT { left: 0, top: 0, right: cw, bottom: ch };
        let _ = windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi(&mut wr, style, false, exstyle, dpi);
        let dw = wr.right - wr.left;
        let dh = wr.bottom - wr.top;

        let mut prc = RECT::default();
        let _ = GetWindowRect(parent, &mut prc);
        // 定位到栅栏附近,但不出屏幕工作区
        let wa = crate::utils::work_area(parent);
        let dx = (prc.left + (prc.right - prc.left - dw) / 2).clamp(wa.left, wa.right - dw);
        let dy = (prc.top + (prc.bottom - prc.top - dh) / 3).clamp(wa.top, wa.bottom - dh);

        let dlg = CreateWindowExW(
            exstyle,
            w!("FeatherInput"),
            PCWSTR(wstr(title).as_ptr()),
            style,
            dx,
            dy,
            dw,
            dh,
            Some(parent),
            None,
            Some(crate::hinstance()),
            None,
        )
        .ok()?;

        // 按 DPI 缩放的界面字体(默认 SYSTEM_FONT 老旧且不缩放,换成雅黑更清晰)
        let font = CreateFontW(
            -px(15.0),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            PCWSTR(wstr("Microsoft YaHei UI").as_ptr()),
        );
        let set_font = |h: HWND| {
            if !font.is_invalid() {
                let _ = SendMessageW(h, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            }
        };

        // 单行编辑框(初始文本由创建时窗口名带入)
        let edit = CreateWindowExW(
            Default::default(),
            w!("EDIT"),
            PCWSTR(wstr(initial).as_ptr()),
            WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32,
            ),
            pad,
            edit_y,
            cw - pad * 2,
            edit_h,
            Some(dlg),
            None,
            Some(crate::hinstance()),
            None,
        )
        .ok()?;
        set_font(edit);
        // 确定 / 取消:右对齐排布
        let ok = CreateWindowExW(
            Default::default(),
            w!("BUTTON"),
            PCWSTR(wstr("确定").as_ptr()),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32),
            cw - pad - bw * 2 - bgap,
            by,
            bw,
            bh,
            Some(dlg),
            Some(HMENU(1 as *mut std::ffi::c_void)),
            Some(crate::hinstance()),
            None,
        )
        .ok()?;
        set_font(ok);
        let cancel = CreateWindowExW(
            Default::default(),
            w!("BUTTON"),
            PCWSTR(wstr("取消").as_ptr()),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32),
            cw - pad - bw,
            by,
            bw,
            bh,
            Some(dlg),
            Some(HMENU(2 as *mut std::ffi::c_void)),
            Some(crate::hinstance()),
            None,
        )
        .ok()?;
        set_font(cancel);
        *PROMPT_EDIT.lock().unwrap() = edit.0 as usize;
        *PROMPT_RESULT.lock().unwrap() = None;
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetForegroundWindow(dlg);
        let _ = SetActiveWindow(dlg);
        let _ = SetFocus(Some(edit));
        // 全选编辑框内容,方便直接改名
        let _ = SendMessageW(edit, 0x00B1 /* EM_SETSEL */, Some(WPARAM(0)), Some(LPARAM(-1)));
        // 模态消息循环:直到对话框被销毁
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(dlg, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
            if !IsWindow(Some(dlg)).as_bool() {
                break;
            }
        }
        if !font.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
        PROMPT_RESULT.lock().unwrap().take()
    }
}

unsafe extern "system" fn fence_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // WM_NCCREATE 显式走 DefWindowProc 并返回其结果(避免创建被系统中止)
    if msg == 0x0081 {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    match msg {
        WM_SYSCOMMAND if (wparam.0 as u32 & 0xfff0) == SC_MINIMIZE => {
            // 栅栏是桌面组件，不参与 Win+D / 任务栏“显示桌面”的最小化集合。
            return LRESULT(0);
        }
        WM_SIZE if wparam.0 as u32 == SIZE_MINIMIZED => {
            let _ = PostMessageW(Some(hwnd), WM_APP_DESKTOP_RESTORE, WPARAM(0), LPARAM(0));
            return LRESULT(0);
        }
        WM_APP_DESKTOP_RESTORE => {
            let should_show = with_global(|g| !g.zen && fence_idx(g, hwnd).is_some());
            if should_show {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                schedule_render(hwnd);
            }
            return LRESULT(0);
        }
        WM_ERASEBKGND => {
            // 背景由我们全量重绘(ULW 整幅替换),不做系统擦除 → 无闪烁
            return LRESULT(1);
        }
        WM_NCHITTEST => {
            // 命中测试统一返回 HTCLIENT:无边框 + WS_EX_NOACTIVATE 下系统拖动/拉伸不可用
            // (实测:点击标题栏 WM_NCLBUTTONDOWN(HTCAPTION) 到达,但 DefWindowProc 不移动窗口)。
            // 拖动/拉伸改由手动实现:WM_LBUTTONDOWN 判定区域并 SetCapture,WM_MOUSEMOVE 里 SetWindowPos。
            // 光标形状仍由 WM_SETCURSOR 独立判定(标题/边缘/主体)。
            return LRESULT(HTCLIENT as isize);
        }
        WM_PAINT => {
            // 分层窗口内容不保留:系统发 WM_PAINT 仅用于验证(清空更新区域)。
            // 整幅内容由 render_fence 画进缓存后 UpdateLayeredWindow 提交。
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            }
            return LRESULT(0);
        }
        WM_DESTROY => {
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    g.fences[idx].valid = false;
                }
            });
            return LRESULT(0);
        }
        WM_APP_REFRESH => {
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    // 后续事件只更新时间戳，不再投递消息；计时器到期时检查安静期。
                    if !restart_refresh_timer(hwnd, REFRESH_DEBOUNCE_MS) {
                        g.fences[idx].refresh_signal.cancel();
                        refresh_fence_now(g, idx);
                    }
                }
            });
            return LRESULT(0);
        }
        WM_APP_DROP => {
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    g.fences[idx].refresh_signal.cancel();
                    stop_refresh_timer(hwnd);
                    refresh_fence_now(g, idx);
                }
            });
            return LRESULT(0);
        }
        WM_KEYDOWN if wparam.0 == VK_DELETE.0 as usize => {
            let path = with_global(|g| {
                let idx = fence_idx(g, hwnd)?;
                let f = &g.fences[idx];
                f.selected.and_then(|i| f.entries.get(i)).map(|e| e.path.clone())
            });
            if let Some(path) = path {
                match recycle_path(hwnd, &path) {
                    Ok(()) => with_global(|g| {
                        if let Some(idx) = fence_idx(g, hwnd) {
                            let ghost = g.config.ghost_mode;
                            let f = &mut g.fences[idx];
                            f.selected = None;
                            refresh_entries(f, &crate::config::vault_dir(&g.config));
                            render_fence(&mut g.icons, ghost, f);
                        }
                    }),
                    Err(e) => crate::dlog(&format!("[delete] {}: {e}", path.display())),
                }
            }
            return LRESULT(0);
        }
        WM_MOUSEMOVE => {
            let x = low16(lparam.0 as usize);
            let y = high16(lparam.0 as usize);
            // 达到拖拽阈值后要启动的拖出(路径 + 目标目录),在 with_global 之外执行
            let mut drag_path: Option<(String, PathBuf)> = None;
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    let ghost = g.config.ghost_mode;
                    let mut need_render = false;
                    {
                        let f = &mut g.fences[idx];
                        let d = f.dpi;
                        if ghost && !f.hover_visible {
                            f.hover_visible = true;
                            let mut tme = TRACKMOUSEEVENT {
                                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                                dwFlags: TRACKMOUSEEVENT_FLAGS(TME_LEAVE.0),
                                hwndTrack: hwnd,
                                dwHoverTime: 0,
                            };
                            let _ = TrackMouseEvent(&mut tme);
                            need_render = true;
                        }
                        if f.moving {
                            let mut cur = POINT::default();
                            let _ = GetCursorPos(&mut cur);
                            // 连续磁吸:平滑拉向最近格点,越近拉得越紧(无瞬移跳变);
                            // 同时 clamp 进工作区,防拖出屏幕
                            let wa = work_area(hwnd);
                            let rx = magnet_smooth((cur.x - f.move_off.0) as f32, cell_w(f), wa.left, 0.5);
                            let ry = magnet_smooth((cur.y - f.move_off.1) as f32, cell_h(f), wa.top, 0.5);
                            let mut nx = rx.round() as i32;
                            let mut ny = ry.round() as i32;
                            nx = nx.clamp(wa.left, (wa.right - f.cfg.w).max(wa.left));
                            ny = ny.clamp(wa.top, (wa.bottom - f.cfg.h).max(wa.top));
                            let _ = SetWindowPos(
                                hwnd,
                                None,
                                nx,
                                ny,
                                0,
                                0,
                                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                            // 同步 cfg:松手时 settle_fence 从实际拖动位置吸附,
                            // 否则会用旧的 cfg 位置,把窗口弹回原位
                            f.cfg.x = nx;
                            f.cfg.y = ny;
                            // 拖动中不重绘(内容没变;避免每帧全量重绘导致窗口忙/转圈)
                        } else if let Some(dir) = f.resizing {
                            let mut cur = POINT::default();
                            let _ = GetCursorPos(&mut cur);
                            let mut rc = RECT::default();
                            let _ = GetWindowRect(hwnd, &mut rc);
                            let (mut nx, mut ny, mut nw, mut nh) = (rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top);
                            let apply = |nx: &mut i32, ny: &mut i32, nw: &mut i32, nh: &mut i32, dir: ResizeDir| {
                                match dir {
                                    ResizeDir::E | ResizeDir::NE | ResizeDir::SE => *nw = (cur.x - *nx).max(min_w(d)),
                                    ResizeDir::W | ResizeDir::NW | ResizeDir::SW => {
                                        let right = *nx + *nw;
                                        *nx = cur.x.min(right - min_w(d));
                                        *nw = right - *nx;
                                    }
                                    _ => {}
                                }
                                match dir {
                                    ResizeDir::S | ResizeDir::SE | ResizeDir::SW => *nh = (cur.y - *ny).max(min_h(d)),
                                    ResizeDir::N | ResizeDir::NE | ResizeDir::NW => {
                                        let bottom = *ny + *nh;
                                        *ny = cur.y.min(bottom - min_h(d));
                                        *nh = bottom - *ny;
                                    }
                                    _ => {}
                                }
                            };
                            apply(&mut nx, &mut ny, &mut nw, &mut nh, dir);
                            // 连续尺寸磁吸(平滑拉向整数格子,无跳变)+ clamp 工作区(防溢出)
                            let wa = work_area(hwnd);
                            let nw2 = magnet_size_smooth(nw as f32, cell_w(f), 2 * margin(d) + rail(d), 0.5).round() as i32;
                            let nh2 = magnet_size_smooth(nh as f32, cell_h(f), title_h(d) + 2 * margin(d), 0.5).round() as i32;
                            let nw = nw2.min((wa.right - nx).max(min_w(d)));
                            let nh = nh2.min((wa.bottom - ny).max(min_h(d)));
                            let _ = SetWindowPos(hwnd, None, nx, ny, nw, nh, SWP_NOZORDER | SWP_NOACTIVATE);
                            // 实时跟随:同步 cfg 尺寸并重绘,内容平滑缩放(而非松手后瞬间刷新)。
                            // 每帧重新提交 ULW 表面,尺寸与窗口矩形保持一致。
                            f.cfg.x = nx;
                            f.cfg.y = ny;
                            f.cfg.w = nw;
                            f.cfg.h = nh;
                            // 窗口尺寸实时变化 → 页/行重算,顶部行吸附到当前页首
                            sync_page(f);
                            need_render = true;
                        } else if f.drag_idx.is_some() {
                            // 拖出阈值:按下后鼠标移过系统拖拽阈值 → 启动 OLE 拖出。
                            // 实际 DoDragDrop 在 with_global 之外执行(避免持锁进入模态循环)。
                            let t = unsafe {
                                GetSystemMetrics(SM_CXDRAG).max(GetSystemMetrics(SM_CYDRAG))
                            }
                            .max(4);
                            if (x - f.drag_down.0).abs() >= t || (y - f.drag_down.1).abs() >= t {
                                let didx = f.drag_idx.take();
                                f.hover = None;
                                if let Some(didx) = didx {
                                    if let Some(p) = f.entries.get(didx).map(|e| e.path.clone()) {
                                        unsafe { let _ = ReleaseCapture(); };
                                        let vault = crate::config::vault_dir(&g.config);
                                        drag_path = Some((p.to_string_lossy().to_string(), vault));
                                    }
                                }
                                need_render = true;
                            }
                        } else {
                            // hover 高亮
                            let (cols, _) = grid_dims(f);
                            let new_hover = hit_item(f, x, y, cols);
                            if new_hover != f.hover {
                                f.hover = new_hover;
                                need_render = true;
                            }
                        }
                    }
                    if need_render {
                        render_fence(&mut g.icons, ghost, &mut g.fences[idx]);
                    }
                }
            });
            // 在锁外启动 OLE 拖出(阻塞到松手);拖出后文件可能被移动/删除 → 重扫目录刷新
            if let Some((path, vault)) = drag_path {
                crate::dragout::start_drag(vec![path]);
                with_global(|g| {
                    if let Some(idx) = fence_idx(g, hwnd) {
                        let f = &mut g.fences[idx];
                        let keep_page = f.page;
                        refresh_entries(f, &vault);
                        // 拖出后尽量留在原页(条目减少时收敛到最后一页)
                        f.page = keep_page.min(total_pages(f).saturating_sub(1));
                        f.top_row = f.page as f32 * grid_dims(f).1 as f32;
                        render_fence(&mut g.icons, g.config.ghost_mode, f);
                    }
                });
            }
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            let x = low16(lparam.0 as usize);
            let y = high16(lparam.0 as usize);
            let _ = SetForegroundWindow(hwnd);
            let _ = SetActiveWindow(hwnd);
            let _ = SetFocus(Some(hwnd));
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    let ghost = g.config.ghost_mode;
                    let f = &mut g.fences[idx];
                    if y < title_h(f.dpi) {
                        f.moving = true;
                        let mut cur = POINT::default();
                        let _ = GetCursorPos(&mut cur);
                        let mut rc = RECT::default();
                        let _ = GetWindowRect(hwnd, &mut rc);
                        f.move_off = (cur.x - rc.left, cur.y - rc.top);
                        SetCapture(hwnd);
                    } else if let Some(dir) = resize_dir_at(f, x, y) {
                        f.resizing = Some(dir);
                        SetCapture(hwnd);
                    } else {
                        // 按在图标上:记录潜在拖出,移动超阈值后由 WM_MOUSEMOVE 启动 OLE 拖拽
                        let (cols, _) = grid_dims(f);
                        if let Some(idx2) = hit_item(f, x, y, cols) {
                            f.selected = Some(idx2);
                            f.drag_idx = Some(idx2);
                            f.drag_down = (x, y);
                            SetCapture(hwnd);
                            render_fence(&mut g.icons, ghost, f);
                        } else if f.selected.take().is_some() {
                            render_fence(&mut g.icons, ghost, f);
                        }
                    }
                }
            });
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    let was_drag = g.fences[idx].moving || g.fences[idx].resizing.is_some();
                    let had_item_press = g.fences[idx].drag_idx.is_some();
                    g.fences[idx].moving = false;
                    g.fences[idx].resizing = None;
                    // 普通单击(未达拖拽阈值)也会到这里:清除潜在拖出
                    g.fences[idx].drag_idx = None;
                    if was_drag || had_item_press {
                        let _ = ReleaseCapture();
                    }
                    if was_drag {
                        // 松手整理:吸附网格尺寸/位置 + clamp 工作区 + 重叠推挤到空闲槽位 + 保存
                        settle_fence(g, idx);
                    }
                }
            });
            return LRESULT(0);
        }
        WM_LBUTTONDBLCLK => {
            let x = low16(lparam.0 as usize);
            let y = high16(lparam.0 as usize);
            if y < title_h(window_dpi(hwnd)) {
                // 双击顶部栅栏名 → 重命名
                rename_fence(hwnd);
                return LRESULT(0);
            }
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    let f = &mut g.fences[idx];
                    let (cols, _) = grid_dims(f);
                    if let Some(idx2) = hit_item(f, x, y, cols) {
                        if let Some(e) = f.entries.get(idx2) {
                            let w = wstr(&e.path.to_string_lossy());
                            let _ = ShellExecuteW(
                                None,
                                PCWSTR(w!("open").as_ptr()),
                                PCWSTR(w.as_ptr()),
                                None,
                                None,
                                SW_SHOWNORMAL,
                            );
                        }
                    }
                }
            });
            return LRESULT(0);
        }
        WM_RBUTTONUP => {
            // 右键任意位置都打开栅栏菜单(删除/重命名/透明度/图标大小)。
            // 之前只认标题栏,右键内容区没反应 = 用户"无法删除"。改到任意位置。
            fence_menu(hwnd);
            return LRESULT(0);
        }
        WM_MOUSEWHEEL => {
            let raw = high16(wparam.0);
            if raw == 0 {
                return LRESULT(0);
            }
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    let ghost = g.config.ghost_mode;
                    let f = &mut g.fences[idx];
                    // 增量先累加,满 120(一次滚轮刻度)翻一页;触控板小增量累积后同样翻页
                    f.wheel_acc += raw;
                    let steps = f.wheel_acc / 120;
                    if steps == 0 {
                        return;
                    }
                    f.wheel_acc -= steps * 120;
                    let pages = total_pages(f);
                    let dir = if steps < 0 { 1 } else { -1 };
                    let np = (f.page as i32 + dir * steps.abs()).clamp(0, pages as i32 - 1) as usize;
                    if np != f.page {
                        f.page = np;
                        start_page_anim(f);
                        // 立即推进一帧,滚动响应更跟手(剩余动画由 WM_TIMER 平滑补完)
                        step_page_anim(f);
                    }
                    render_fence(&mut g.icons, ghost, f);
                }
            });
            return LRESULT(0);
        }
        WM_TIMER => {
            if wparam.0 == REFRESH_TICK {
                with_global(|g| {
                    if let Some(idx) = fence_idx(g, hwnd) {
                        match g.fences[idx].refresh_signal.timer_action() {
                            RefreshTimerAction::Idle => stop_refresh_timer(hwnd),
                            RefreshTimerAction::Wait(delay_ms) => {
                                if !restart_refresh_timer(hwnd, delay_ms) {
                                    g.fences[idx].refresh_signal.cancel();
                                    refresh_fence_now(g, idx);
                                }
                            }
                            RefreshTimerAction::Refresh => {
                                stop_refresh_timer(hwnd);
                                refresh_fence_now(g, idx);
                            }
                        }
                    }
                });
            } else if wparam.0 == ANIM_TICK {
                with_global(|g| {
                    if let Some(idx) = fence_idx(g, hwnd) {
                        let f = &mut g.fences[idx];
                        if f.animating {
                            step_page_anim(f);
                        }
                        render_fence(&mut g.icons, g.config.ghost_mode, f);
                    }
                });
            }
            return LRESULT(0);
        }
        WM_MOUSELEAVE => {
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    let ghost = g.config.ghost_mode;
                    let f = &mut g.fences[idx];
                    f.hover_visible = false;
                    f.hover = None;
                    render_fence(&mut g.icons, ghost, f);
                }
            });
            return LRESULT(0);
        }
        WM_SETCURSOR => {
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    let f = &g.fences[idx];
                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    let mut cpt = pt;
                    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut cpt);
                    let cursor = if cpt.y < title_h(f.dpi) {
                        IDC_SIZEALL
                    } else if let Some(d) = resize_dir_at(f, cpt.x, cpt.y) {
                        match d {
                            ResizeDir::N | ResizeDir::S => IDC_SIZENS,
                            ResizeDir::E | ResizeDir::W => IDC_SIZEWE,
                            ResizeDir::NW | ResizeDir::SE => IDC_SIZENWSE,
                            _ => IDC_SIZENESW,
                        }
                    } else {
                        IDC_ARROW
                    };
                    let hc = LoadCursorW(None, cursor).unwrap_or_default();
                    SetCursor(Some(hc));
                }
            });
            return LRESULT(1);
        }
        WM_DPICHANGED => {
            // Per-Monitor V2 下,窗口被拖到不同 DPI 的显示器 / 系统缩放变化时,
            // 系统把窗口矩形缩放到建议矩形(并钳进新显示器工作区)。
            // 按建议矩形应用,并把 f.dpi 切到新值 → 几何/渲染随新屏比例重算。
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    let newdpi = (wparam.0 & 0xFFFF) as u32;
                    if newdpi == 0 {
                        return;
                    }
                    let rect = unsafe { *(lparam.0 as *const RECT) };
                    let nw = (rect.right - rect.left).max(1);
                    let nh = (rect.bottom - rect.top).max(1);
                    let f = &mut g.fences[idx];
                    f.dpi = newdpi as f32 / 96.0;
                    f.cfg.x = rect.left;
                    f.cfg.y = rect.top;
                    f.cfg.w = nw;
                    f.cfg.h = nh;
                    f.cfg.dpi = newdpi;
                    unsafe {
                        let _ = SetWindowPos(
                            hwnd,
                            None,
                            rect.left,
                            rect.top,
                            nw,
                            nh,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                    sync_page(f);
                    render_fence(&mut g.icons, g.config.ghost_mode, f);
                    g.config.fences = config_snapshot(&g.fences);
                    crate::config::save(&g.config);
                }
            });
            return LRESULT(0);
        }
        WM_DISPLAYCHANGE => {
            // 分辨率 / 显示器插拔变化:把本栅栏 clamp 回(新)工作区。
            // 只做钳制不磁吸,避免分辨率变化时全部栅栏被吸附乱移。
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    let f = &mut g.fences[idx];
                    f.dpi = window_dpi(hwnd);
                    let d = f.dpi;
                    let wa = work_area(hwnd);
                    let nw = f.cfg.w.min(wa.right - wa.left).max(min_w(d));
                    let nh = f.cfg.h.min(wa.bottom - wa.top).max(min_h(d));
                    let nx = f.cfg.x.clamp(wa.left, (wa.right - nw).max(wa.left));
                    let ny = f.cfg.y.clamp(wa.top, (wa.bottom - nh).max(wa.top));
                    if nx != f.cfg.x || ny != f.cfg.y || nw != f.cfg.w || nh != f.cfg.h {
                        f.cfg.x = nx;
                        f.cfg.y = ny;
                        f.cfg.w = nw;
                        f.cfg.h = nh;
                        unsafe {
                            let _ = SetWindowPos(
                                hwnd,
                                None,
                                nx,
                                ny,
                                nw,
                                nh,
                                SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                        }
                        sync_page(f);
                        render_fence(&mut g.icons, g.config.ghost_mode, f);
                        g.config.fences = config_snapshot(&g.fences);
                        crate::config::save(&g.config);
                    }
                }
            });
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn grid_dims(f: &Fence) -> (i32, i32) {
    let w = f.cfg.w;
    let h = f.cfg.h;
    let d = f.dpi;
    // 宽度让出右侧圆点轨道
    let cols = ((w - 2 * margin(d) - rail(d)) / cell_w(f)).max(1);
    let rows = ((h - title_h(d) - 2 * margin(d)) / cell_h(f)).max(0);
    (cols, rows)
}

/// 每页条数 = 当前窗口尺寸下的完整网格(cols × rows),随窗口大小实时变化
fn page_size(f: &Fence) -> usize {
    let (cols, rows) = grid_dims(f);
    (cols.max(1) as usize) * (rows.max(0) as usize)
}

/// 总页数(至少 1 页)
fn total_pages(f: &Fence) -> usize {
    let ps = page_size(f);
    if ps == 0 {
        return 1;
    }
    ((f.entries.len() + ps - 1) / ps).max(1)
}

/// 翻页动画计时器 ID
const ANIM_TICK: usize = 0xFE10;
/// 目录变化刷新计时器：连续事件安静一小段时间后再扫描和重绘。
const REFRESH_TICK: usize = 0xFE11;
const REFRESH_DEBOUNCE_MS: u32 = 150;

fn stop_refresh_timer(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), REFRESH_TICK);
    }
}

fn restart_refresh_timer(hwnd: HWND, delay_ms: u32) -> bool {
    stop_refresh_timer(hwnd);
    unsafe { SetTimer(Some(hwnd), REFRESH_TICK, delay_ms.max(1), None) != 0 }
}

fn refresh_fence_now(g: &mut Global, idx: usize) {
    let ghost = g.config.ghost_mode;
    let vault = crate::config::vault_dir(&g.config);
    let f = &mut g.fences[idx];
    refresh_entries(f, &vault);
    render_fence(&mut g.icons, ghost, f);
}

/// 页号收敛到合法范围,顶部行吸附到页首(尺寸/条目变化后调用)
fn sync_page(f: &mut Fence) {
    let pages = total_pages(f);
    if f.page >= pages {
        f.page = pages.saturating_sub(1);
    }
    let (_, rows) = grid_dims(f);
    f.top_row = f.page as f32 * rows as f32;
    stop_page_anim(f);
}

/// 停掉翻页动画计时器
fn stop_page_anim(f: &mut Fence) {
    f.animating = false;
    if !f.hwnd.is_invalid() {
        unsafe {
            let _ = KillTimer(Some(f.hwnd), ANIM_TICK);
        }
    }
}

/// 翻页动画时长(帧,16ms/帧 → 约 200ms)
const ANIM_FRAMES: u32 = 13;

/// 启动翻页动画(定时器驱动重绘):记录起始位置,固定时长 cubic ease-out
fn start_page_anim(f: &mut Fence) {
    if !f.animating && !f.hwnd.is_invalid() {
        f.animating = true;
        f.anim_frames = 0;
        f.anim_from = f.top_row;
        unsafe {
            let _ = SetTimer(Some(f.hwnd), ANIM_TICK, 16, None);
        }
    }
}

/// 推进一帧动画:top_row 按固定时长从 anim_from 插值到目标页(cubic ease-out,
/// 起步快、落点稳)。到点吸附并停表。返回是否仍在动画中。
fn step_page_anim(f: &mut Fence) -> bool {
    let (_, rows) = grid_dims(f);
    let target = f.page as f32 * rows as f32;
    f.anim_frames += 1;
    let t = (f.anim_frames as f32 / ANIM_FRAMES as f32).min(1.0);
    let e = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t);
    f.top_row = f.anim_from + (target - f.anim_from) * e;
    if t >= 1.0 {
        f.top_row = target;
        stop_page_anim(f);
        false
    } else {
        true
    }
}

// ---------- 网格布局:磁吸吸附 / 网格尺寸 / 防重叠 / 防溢出 ----------

/// 磁吸:r 距最近网格格点(origin + n*step)在容差(factor*step)内 → 吸附到该格点。
/// factor >= 0.5 即"始终吸附最近格点",factor 更小则只有靠近时才吸附。
/// 用于松手/创建/恢复时"必落网格"。
fn magnet(v: i32, step: i32, origin: i32, factor: f32) -> i32 {
    if step <= 0 {
        return v;
    }
    let rel = (v - origin) as f32;
    let n = (rel / step as f32).round();
    let target = origin + (n as i32) * step;
    if ((target - v).abs() as f32) <= step as f32 * factor {
        target
    } else {
        v
    }
}

/// 连续磁吸(拖动中用):距最近格点越近拉力越大,全程平滑,无离散跳变(瞬移)。
/// 超出 range(以步长比例计)时完全跟随鼠标。distance 用 f32 保留亚像素。
fn magnet_smooth(v: f32, step: i32, origin: i32, range: f32) -> f32 {
    if step <= 0 {
        return v;
    }
    let rel = v - origin as f32;
    let n = (rel / step as f32).round();
    let target = origin as f32 + n * step as f32;
    let dist = (target - v).abs();
    let max_range = step as f32 * range;
    if max_range <= 0.0 || dist >= max_range {
        return v;
    }
    // 接近程度 0..1,平方 easing:远时几乎不拉、近时贴紧格点
    let t = 1.0 - dist / max_range;
    let pull = t * t;
    v + (target - v) * pull
}

/// 连续尺寸磁吸(拖动缩放中用):平滑拉向整数格子,无跳变
fn magnet_size_smooth(v: f32, step: i32, base: i32, range: f32) -> f32 {
    magnet_smooth(v - base as f32, step, 0, range) + base as f32
}

/// 网格吸附后的完整尺寸:w = 2margin + rail + cols*cell, h = title + 2margin + rows*cell
fn snap_size(f: &Fence, w: i32, h: i32) -> (i32, i32) {
    let d = f.dpi;
    let cw = cell_w(f);
    let ch = cell_h(f);
    let cols = (((w - 2 * margin(d) - rail(d)) as f32 / cw as f32).round().max(1.0)) as i32;
    let rows = (((h - title_h(d) - 2 * margin(d)) as f32 / ch as f32).round().max(1.0)) as i32;
    (
        (2 * margin(d) + rail(d) + cols * cw).max(min_w(d)),
        (title_h(d) + 2 * margin(d) + rows * ch).max(min_h(d)),
    )
}

fn rects_overlap(ax: i32, ay: i32, aw: i32, ah: i32, bx: i32, by: i32, bw: i32, bh: i32) -> bool {
    ax < bx + bw && ax + aw > bx && ay < by + bh && ay + ah > by
}

/// 是否与除 self_idx 外的其他栅栏重叠
fn overlaps_any(g: &crate::Global, self_idx: usize, x: i32, y: i32, w: i32, h: i32) -> bool {
    g.fences
        .iter()
        .enumerate()
        .any(|(i, o)| i != self_idx && o.valid && rects_overlap(x, y, w, h, o.cfg.x, o.cfg.y, o.cfg.w, o.cfg.h))
}

/// 松手整理:把 idx 栅栏吸附到网格尺寸/位置,clamp 进工作区,若有重叠沿螺旋挪到最近空闲槽位。
/// 用于拖动/缩放松手、创建、启动恢复、图标大小变更后。
pub fn settle_fence(g: &mut crate::Global, idx: usize) {
    if idx >= g.fences.len() {
        return;
    }
    let hwnd = g.fences[idx].hwnd;
    let wa = work_area(hwnd);
    // 1. 网格尺寸
    let (nw, nh) = snap_size(&g.fences[idx], g.fences[idx].cfg.w, g.fences[idx].cfg.h);
    let slot_w = cell_w(&g.fences[idx]);
    let slot_h = cell_h(&g.fences[idx]);
    // 2. 位置吸附(松手必落网格)+ clamp 工作区
    let mut nx = magnet(g.fences[idx].cfg.x, slot_w, wa.left, 0.5);
    let mut ny = magnet(g.fences[idx].cfg.y, slot_h, wa.top, 0.5);
    nx = nx.clamp(wa.left, (wa.right - nw).max(wa.left));
    ny = ny.clamp(wa.top, (wa.bottom - nh).max(wa.top));
    // 3. 重叠 → 从当前位置沿四个方向螺旋找最近空闲槽位
    if overlaps_any(g, idx, nx, ny, nw, nh) {
        let (bx, by) = (nx, ny);
        'outer: for d in 1..96 {
            for (dx, dy) in [(-d, 0), (d, 0), (0, -d), (0, d)] {
                let tx = bx + dx * slot_w;
                let ty = by + dy * slot_h;
                if tx < wa.left || ty < wa.top || tx + nw > wa.right || ty + nh > wa.bottom {
                    continue;
                }
                if !overlaps_any(g, idx, tx, ty, nw, nh) {
                    nx = tx;
                    ny = ty;
                    break 'outer;
                }
            }
        }
    }
    // 4. 应用 + 重绘 + 保存
    let ghost = g.config.ghost_mode;
    let f = &mut g.fences[idx];
    f.cfg.x = nx;
    f.cfg.y = ny;
    f.cfg.w = nw;
    f.cfg.h = nh;
    unsafe {
        let _ = SetWindowPos(hwnd, None, nx, ny, nw, nh, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    sync_page(f);
    render_fence(&mut g.icons, ghost, f);
    g.config.fences = config_snapshot(&g.fences);
    crate::config::save(&g.config);
    crate::reserve_desktop_icons(g);
}

/// 持久化运行时物理矩形及其窗口 DPI。
/// 屏幕 x/y 不能除以窗口 DPI:它们位于跨显示器的全局坐标空间,
/// 混合缩放时再统一乘系统 DPI不可逆。启动恢复只换算 w/h。
pub fn config_snapshot(fences: &[Fence]) -> Vec<FenceCfg> {
    fences
        .iter()
        .map(|f| {
            let mut c = f.cfg.clone();
            c.dpi = (f.dpi.max(1.0) * 96.0).round() as u32;
            c
        })
        .collect()
}

fn hit_item(f: &Fence, x: i32, y: i32, cols: i32) -> Option<usize> {
    if f.entries.is_empty() {
        return None;
    }
    let d = f.dpi;
    if x < margin(d) || y < title_h(d) + margin(d) {
        return None;
    }
    let col = (x - margin(d)) / cell_w(f);
    let row = (y - title_h(d) - margin(d)) / cell_h(f);
    if col >= cols || row < 0 {
        return None;
    }
    // 屏幕行 → 绝对行(加上当前页顶部行,动画中取最近的整数行)
    let row_abs = (row + f.top_row.round() as i32) as i64;
    let idx = row_abs * cols as i64 + col as i64;
    if idx >= 0 && (idx as usize) < f.entries.len() {
        Some(idx as usize)
    } else {
        None
    }
}

fn resize_dir_at(f: &Fence, x: i32, y: i32) -> Option<ResizeDir> {
    let (w, h) = (f.cfg.w, f.cfg.h);
    let e = edge(f.dpi);
    let left = x < e;
    let right = x >= w - e;
    let top = y < e;
    let bottom = y >= h - e;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(ResizeDir::NW),
        (true, _, false, true) => Some(ResizeDir::SW),
        (false, true, true, _) => Some(ResizeDir::NE),
        (false, true, false, true) => Some(ResizeDir::SE),
        (true, _, _, _) => Some(ResizeDir::W),
        (false, true, _, _) => Some(ResizeDir::E),
        (_, _, true, _) => Some(ResizeDir::N),
        (_, _, false, true) => Some(ResizeDir::S),
        _ => None,
    }
}

/// 圆角矩形路径
unsafe fn add_rounded_path(path: *mut GpPath, x: f32, y: f32, w: f32, h: f32, r: f32) {
    let r = r.min(w / 2.0).min(h / 2.0);
    GdipAddPathArc(path, x, y, r * 2.0, r * 2.0, 180.0, 90.0);
    GdipAddPathArc(path, x + w - r * 2.0, y, r * 2.0, r * 2.0, 270.0, 90.0);
    GdipAddPathArc(path, x + w - r * 2.0, y + h - r * 2.0, r * 2.0, r * 2.0, 0.0, 90.0);
    GdipAddPathArc(path, x, y + h - r * 2.0, r * 2.0, r * 2.0, 90.0, 90.0);
    GdipClosePathFigure(path);
}

unsafe fn fill_rounded(g: *mut GpGraphics, x: f32, y: f32, w: f32, h: f32, r: f32, argb: u32) {
    let mut path: *mut GpPath = std::ptr::null_mut();
    GdipCreatePath(FillModeAlternate, &mut path);
    add_rounded_path(path, x, y, w, h, r);
    let mut brush: *mut GpSolidFill = std::ptr::null_mut();
    GdipCreateSolidFill(argb, &mut brush);
    GdipFillPath(g, brush as *mut GpBrush, path);
    GdipDeleteBrush(brush as *mut GpBrush);
    GdipDeletePath(path);
}

unsafe fn draw_text(
    g: *mut GpGraphics,
    font: *const GpFont,
    fmt: *const GpStringFormat,
    brush: *const GpBrush,
    text: &str,
    rect: RectF,
) {
    let w = wstr(text);
    GdipDrawString(
        g,
        PCWSTR(w.as_ptr()),
        -1,
        font,
        &rect,
        fmt,
        brush,
    );
}

/// 绘制桌面图标式标签:先在八个方向各偏移 stroke 画一圈暗色描边,再画白色正文。
/// 八方向(含对角)覆盖均匀,字周描边等宽、不偏侧;stroke 取 1 物理像素时描边纤细
/// 不臃肿,又能在明暗壁纸上给白字衬出清晰边界——比只向一侧偏移的软投影更“实体”。
unsafe fn draw_outlined_text(
    g: *mut GpGraphics,
    font: *const GpFont,
    fmt: *const GpStringFormat,
    outline: *const GpBrush,
    white: *const GpBrush,
    text: &str,
    rect: RectF,
    stroke: f32,
) {
    for (dx, dy) in [
        (-stroke, 0.0),
        (stroke, 0.0),
        (0.0, -stroke),
        (0.0, stroke),
        (-stroke, -stroke),
        (stroke, -stroke),
        (-stroke, stroke),
        (stroke, stroke),
    ] {
        let edge = RectF {
            X: rect.X + dx,
            Y: rect.Y + dy,
            Width: rect.Width,
            Height: rect.Height,
        };
        unsafe { draw_text(g, font, fmt, outline, text, edge) };
    }
    unsafe { draw_text(g, font, fmt, white, text, rect) };
}

/// 文件名称:仿 Windows 桌面图标标签 —— 白色文字 + 紧实深色描边,
/// 单行放得下就单行,放不下自动两行,末行超长由 fmt 的省略号裁剪。
unsafe fn draw_label(
    g: *mut GpGraphics,
    font: *const GpFont,
    fmt: *const GpStringFormat,
    meas: *const GpStringFormat,
    shadow: *const GpBrush,
    white: *const GpBrush,
    text: &str,
    rect: RectF,
) {
    let w = wstr(text);
    let units: Vec<u16> = text.encode_utf16().collect();
    let total = units.len();
    // 单行宽度测量(NoWrap + 无裁剪):codepointsfitted = 能放下的字符数
    let mut bbox = RectF::default();
    let mut fitted = 0i32;
    let mut lines = 0i32;
    unsafe {
        GdipMeasureString(
            g,
            PCWSTR(w.as_ptr()),
            -1,
            font,
            &rect,
            meas,
            &mut bbox,
            &mut fitted,
            &mut lines,
        );
    }
    // 描边固定 1 物理像素:整数偏移不会二次软化字形,八方向合起来仍是纤细一圈,
    // 不随 DPI 变粗(此前 round(dpi) 在 1.5× 下取到 2px,把标签撑得又粗又糊)。
    let stroke = 1.0_f32;
    if (fitted as usize) >= total && bbox.Width <= rect.Width + 0.5 {
        unsafe { draw_outlined_text(g, font, fmt, shadow, white, text, rect, stroke) };
        return;
    }
    // 两行:第 1 行取能放下的字符数;若切点落在代理对中间(前一个码元是高代理)则前移
    let mut cut = (fitted as usize).clamp(1, total);
    if (0xD800..0xDC00).contains(&units[cut - 1]) {
        cut -= 1;
    }
    if cut == 0 {
        cut = 1;
    }
    let line1 = String::from_utf16_lossy(&units[..cut]);
    let line2 = String::from_utf16_lossy(&units[cut..]);
    let half = rect.Height / 2.0;
    let r1 = RectF { X: rect.X, Y: rect.Y, Width: rect.Width, Height: half };
    let r2 = RectF { X: rect.X, Y: rect.Y + half, Width: rect.Width, Height: half };
    unsafe {
        draw_outlined_text(g, font, fmt, shadow, white, &line1, r1, stroke);
        draw_outlined_text(g, font, fmt, shadow, white, &line2, r2, stroke);
    }
}

unsafe fn fill_circle(g: *mut GpGraphics, cx: f32, cy: f32, r: f32, argb: u32) {
    let mut path: *mut GpPath = std::ptr::null_mut();
    GdipCreatePath(FillModeAlternate, &mut path);
    GdipAddPathEllipse(path, cx - r, cy - r, r * 2.0, r * 2.0);
    let mut brush: *mut GpSolidFill = std::ptr::null_mut();
    GdipCreateSolidFill(argb, &mut brush);
    GdipFillPath(g, brush as *mut GpBrush, path);
    GdipDeleteBrush(brush as *mut GpBrush);
    GdipDeletePath(path);
}

/// 侧边竖直页面指示点:当前页微放大 + 高亮。亮度/大小按与当前页的接近程度连续过渡,
/// 翻页动画里小圆随之平滑长大/缩小。
unsafe fn draw_page_dots(g: *mut GpGraphics, f: &Fence, w: i32, h: i32) {
    let pages = total_pages(f);
    if pages <= 1 {
        return;
    }
    let (_, rows) = grid_dims(f);
    if rows <= 0 {
        return;
    }
    // 连续页位置(0..pages-1),翻页动画中平滑移动
    let pfrac = f.top_row / rows as f32;
    let d = f.dpi;
    let dot_r = 2.5 * d;
    let spacing = 15.0 * d;
    let cy0 = (title_h(d) as f32 + h as f32) / 2.0 - spacing * (pages as f32 - 1.0) / 2.0;
    // 圆点在右侧独立轨道内居中,不与图标网格重叠
    let cx = w as f32 - margin(d) as f32 - rail(d) as f32 / 2.0;
    for p in 0..pages {
        let cy = cy0 + p as f32 * spacing;
        // 距当前页越近越亮越大
        let act = (1.0 - (pfrac - p as f32).abs()).clamp(0.0, 1.0);
        let r = dot_r + dot_r * 0.8 * act;
        // 颜色:半透明白(0x4D) ↔ 纯白 按 act 插值(深色背景上可见)
        let a = (0x4D as f32 + (0xFF - 0x4D) as f32 * act) as u32;
        let col = (a << 24) | 0x00FFFFFF;
        if act > 0.01 {
            // 当前页外圈柔光(白色)
            fill_circle(g, cx, cy, r * 2.0, (((0x40 as f32) * act) as u32) << 24 | 0x00FFFFFF);
        }
        fill_circle(g, cx, cy, r, col);
    }
}

/// 渲染一帧:背景每像素透明度(opacity)+ 幽灵淡出(global),画进缓存并 ULW 整幅提交。
/// 半透明像素真透明透出桌面,内容画满矩形,圆角由 DWM 裁。
pub fn render_fence(icons: &mut crate::icons::IconCache, ghost_mode: bool, f: &mut Fence) {
    let w = f.cfg.w;
    let h = f.cfg.h;
    if w <= 0 || h <= 0 || f.hwnd.is_invalid() {
        return;
    }
    // 幽灵态(未悬停):整体 alpha 缩到 16%(逐像素 alpha 直接透出桌面,无需开关背景)。
    let ghost_active = ghost_mode && !f.hover_visible;
    let bg_alpha = (255.0 * f.cfg.opacity.clamp(0.1, 1.0)) as u8;
    let mut global = 255u8;
    if ghost_active {
        global = (255.0 * 0.16) as u8;
    }
    // 直接绘制+提交(不走 WM_PAINT;直接用 f,不查表——创建时 fence 还没进全局列表)
    paint_core(icons, f, bg_alpha, global);
}

/// 核心绘制:画进每栅栏缓存 DIB(GDI+ 文字/图形 + GDI 图标),预乘 alpha 后
/// UpdateLayeredWindow 整幅提交。半透明像素真透明透出桌面,内容画满矩形,圆角由 DWM 裁。
fn paint_core(icons: &mut crate::icons::IconCache, f: &mut Fence, bg_alpha: u8, global: u8) {
    let w = f.cfg.w;
    let h = f.cfg.h;
    if w <= 0 || h <= 0 {
        return;
    }
    unsafe {
        // 取/建缓存 DIB(尺寸不变则复用,避免每次重建);bits 为空 = 创建失败
        let bits = ensure_cache(f, w, h);
        if bits.is_null() {
            return;
        }
        let memdc = match f.cache.as_ref() {
            Some(c) => c.mdc,
            None => return,
        };
        // 整幅清成全透明(0),重画当前帧
        std::ptr::write_bytes(bits, 0, (w as usize) * (h as usize) * 4);
        let mut gfx: *mut GpGraphics = std::ptr::null_mut();
        if GdipCreateFromHDC(memdc, &mut gfx).0 != 0 {
            return;
        }
        GdipSetSmoothingMode(gfx, SmoothingModeAntiAlias);
        // GridFit 把字形笔画对齐到物理像素网格,比普通灰阶抗锯齿更接近
        // Windows 桌面标签的紧实观感。ClearType 不适用于逐像素透明分层窗口。
        GdipSetTextRenderingHint(gfx, TextRenderingHintAntiAliasGridFit);
        // 本帧按窗口所在显示器 DPI 缩放几何(Per-Monitor)
        let d = f.dpi;

        // 半透明深色面板:分层窗口走逐像素 alpha,ULW 整幅提交,半透明像素直接透出
        // 桌面(真透明,无磨砂)。面板 = 透明度随 bg_alpha 缩放的深色盖层。
        {
            let tint_a = ((bg_alpha as u32) * 170) / 255;
            let mut bg_brush: *mut GpSolidFill = std::ptr::null_mut();
            GdipCreateSolidFill((tint_a << 24) | 0x001A1C20, &mut bg_brush);
            GdipFillRectangle(gfx, bg_brush as *mut GpBrush, 0.0, 0.0, w as f32, h as f32);
            GdipDeleteBrush(bg_brush as *mut GpBrush);
        }

        // 标题栏
        let mut fam: *mut GpFontFamily = std::ptr::null_mut();
        GdipCreateFontFamilyFromName(PCWSTR(wstr(FONT_NAME).as_ptr()), std::ptr::null_mut(), &mut fam);
        let mut font: *mut GpFont = std::ptr::null_mut();
        GdipCreateFont(fam, font_title(d), FontStyleRegular.0, UnitPixel, &mut font);
        let mut fmt: *mut GpStringFormat = std::ptr::null_mut();
        GdipCreateStringFormat(0, 0, &mut fmt);
        GdipSetStringFormatAlign(fmt, StringAlignmentCenter);
        GdipSetStringFormatLineAlign(fmt, StringAlignmentCenter);
        // 标题白色(深色毛玻璃面板上可读)
        let mut title_brush: *mut GpSolidFill = std::ptr::null_mut();
        GdipCreateSolidFill(0xFFFFFFFF, &mut title_brush);

        // 居中:横向铺满内容区(右侧留出页面圆点轨道),StringAlignmentCenter 使文字居中
        let title_rect = RectF {
            X: 0.0,
            Y: 0.0,
            Width: (w - rail(d)).max(1) as f32,
            Height: title_h(d) as f32,
        };
        let display_title = if f.cfg.folder.is_some() && f.cfg.title.is_empty() {
            f.cfg
                .folder
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            f.cfg.title.clone()
        };
        draw_text(gfx, font, fmt, title_brush as *const GpBrush, &display_title, title_rect);

        // 图标网格
        // 待画的图标(位置 + HICON):GDI DrawIconEx 在 GDI+ 绘制完成后统一直绘。
        // 用 GDI 而非 GDI+ 位图的原因:GdipCreateBitmapFromHICON 会把图标透明区
        // 变成不透明黑块(实测整个 256² 位图 trans=0),DrawIconEx 走系统原生掩码/alpha,
        // 对 1-bit、32bpp、PNG 压缩图标都正确。
        let mut icons_to_draw: Vec<(i32, i32, HICON)> = Vec::new();
        // 图标 GDI 裁剪区用到的行数(网格块内的 rows 出块即失效,提到函数级)
        let mut grid_rows: i32 = 0;
        if !f.entries.is_empty() {
            let (cols, rows) = grid_dims(f);
            grid_rows = rows;
            if rows > 0 {
                let cell_w = (w - 2 * margin(d) - rail(d)) as f32 / cols.max(1) as f32;
                let mut hover_brush: *mut GpSolidFill = std::ptr::null_mut();
                GdipCreateSolidFill(0x22FFFFFF, &mut hover_brush);
                let mut label_font: *mut GpFont = std::ptr::null_mut();
                GdipCreateFont(fam, font_label(d), FontStyleRegular.0, UnitPixel, &mut label_font);
                let mut label_fmt: *mut GpStringFormat = std::ptr::null_mut();
                GdipCreateStringFormat(0, 0, &mut label_fmt);
                GdipSetStringFormatAlign(label_fmt, StringAlignmentCenter);
                GdipSetStringFormatLineAlign(label_fmt, StringAlignmentNear);
                // 末行超长以省略号裁剪(仿桌面图标)
                GdipSetStringFormatTrimming(label_fmt, StringTrimmingEllipsisCharacter);
                // 测量格式:NoWrap(不换行),用于判断"单行放得下 / 需要两行"
                let mut meas_fmt: *mut GpStringFormat = std::ptr::null_mut();
                GdipCreateStringFormat(0, 0, &mut meas_fmt);
                GdipSetStringFormatFlags(meas_fmt, StringFormatFlagsNoWrap.0);
                GdipSetStringFormatAlign(meas_fmt, StringAlignmentCenter);
                GdipSetStringFormatLineAlign(meas_fmt, StringAlignmentNear);
                let mut label_brush: *mut GpSolidFill = std::ptr::null_mut();
                // 深色面板上用白色文字 + 深色投影(仿桌面图标标签)
                GdipCreateSolidFill(0xFFFFFFFF, &mut label_brush);
                // 高不透明度暗色细描边:避免半透明面板把抗锯齿边缘衬得发灰、虚浮。
                let mut shadow_brush: *mut GpSolidFill = std::ptr::null_mut();
                GdipCreateSolidFill(0xD9000000, &mut shadow_brush);

                // 网格裁剪到精确内容区:[title_h+margin, +rows*cell_h]。
                // 不含上下 margin:静止时相邻页的行恰好被完全裁掉(上一页最后一行
                // 结束于裁剪区上沿,下一页第一行始于裁剪区下沿),动画中平滑进出;
                // 若裁到 title_h 会把上一页标签/下一页图标漏进 margin 带 → 串页。
                let clip_top = (title_h(d) + margin(d)) as f32;
                GdipSetClipRect(
                    gfx,
                    0.0,
                    clip_top,
                    w as f32,
                    (rows as f32 * cell_h(f) as f32).max(0.0),
                    CombineModeReplace,
                );
                // 按浮点顶部行绘制:静止时 top_row = page*rows,翻页动画中平滑过渡
                let row0 = f.top_row.floor() as i32 - 1;
                for row in row0..(row0 + rows + 2) {
                    if row < 0 {
                        continue;
                    }
                    let y = title_h(d) as f32 + margin(d) as f32 + (row as f32 - f.top_row) * cell_h(f) as f32;
                    if y >= h as f32 {
                        break;
                    }
                    for col in 0..cols {
                        let idx2 = (row * cols + col) as usize;
                        if idx2 >= f.entries.len() {
                            break;
                        }
                        let e = &f.entries[idx2];
                        let x = margin(d) as f32 + col as f32 * cell_w;
                        if f.selected == Some(idx2) || f.hover == Some(idx2) {
                            fill_rounded(
                                gfx,
                                x - 3.0,
                                y - 2.0,
                                cell_w + 6.0,
                                (icon(f) + label_h(d)) as f32 + 4.0,
                                8.0,
                                if f.selected == Some(idx2) { 0x55FFFFFF } else { 0x22FFFFFF },
                            );
                        }
                        // 图标:收集位置,稍后由 GDI DrawIconEx 直绘(原生 alpha,透明区正确)
                        let hicon = icons.get(&e.path);
                        if !hicon.is_invalid() {
                            let ix = (x + (cell_w - icon(f) as f32) / 2.0).round() as i32;
                            let iy = y.round() as i32;
                            icons_to_draw.push((ix, iy, hicon));
                        }
                        // 名称(仿桌面图标:白字 + 投影 + 省略号,放不下两行)
                        let label_rect = RectF {
                            X: x - 2.0,
                            Y: y + icon(f) as f32 + 3.0,
                            Width: cell_w + 4.0,
                            Height: label_h(d) as f32 - 4.0,
                        };
                        draw_label(
                            gfx,
                            label_font,
                            label_fmt,
                            meas_fmt,
                            shadow_brush as *const GpBrush,
                            label_brush as *const GpBrush,
                            &e.name,
                            label_rect,
                        );
                    }
                }
                GdipResetClip(gfx);
                // 侧边页面指示点:当前页微放大 + 高亮(随滚动位置连续过渡)
                draw_page_dots(gfx, f, w, h);
                GdipDeleteBrush(hover_brush as *mut GpBrush);
                GdipDeleteFont(label_font);
                GdipDeleteStringFormat(label_fmt);
                GdipDeleteStringFormat(meas_fmt);
                GdipDeleteBrush(label_brush as *mut GpBrush);
                GdipDeleteBrush(shadow_brush as *mut GpBrush);
            }
        } else if f.entries.is_empty() {
            // 空栅栏提示
            let hint = if f.cfg.folder.is_some() { "空文件夹" } else { "将文件拖入此处收纳" };
            let mut hint_fmt: *mut GpStringFormat = std::ptr::null_mut();
            GdipCreateStringFormat(0, 0, &mut hint_fmt);
            GdipSetStringFormatAlign(hint_fmt, StringAlignmentCenter);
            GdipSetStringFormatLineAlign(hint_fmt, StringAlignmentCenter);
            let mut hint_brush: *mut GpSolidFill = std::ptr::null_mut();
            GdipCreateSolidFill(0x99FFFFFF, &mut hint_brush);
            let hint_rect = RectF {
                X: 10.0,
                Y: title_h(d) as f32 + 10.0,
                Width: (w - 20).max(1) as f32,
                Height: (h - title_h(d) - 20).max(1) as f32,
            };
            draw_text(gfx, font, hint_fmt, hint_brush as *const GpBrush, hint, hint_rect);
            GdipDeleteStringFormat(hint_fmt);
            GdipDeleteBrush(hint_brush as *mut GpBrush);
        }

        GdipDeleteBrush(title_brush as *mut GpBrush);
        GdipDeleteStringFormat(fmt);
        GdipDeleteFont(font);
        GdipDeleteFontFamily(fam);
        GdipFlush(gfx, FlushIntentionSync);
        GdipDeleteGraphics(gfx);

        // 图标:GDI DrawIconEx 直绘进 DIB(背景/文字已由 GDI+ 画好)。
        // DrawIconEx 对 32bpp 图标做原生 alpha 合成,对掩码图标套 AND 掩码,
        // 透明区域保持面板颜色,不再出现不透明黑块。
        // GDI 不认 GDI+ 的裁剪区,这里单独给图标套一层 GDI 裁剪区(精确网格内容区),
        // 否则翻页动画中相邻页的图标会飘进 title/margin。
        let icol = icon(f);
        let ctop = (title_h(d) + margin(d)) as i32;
        let cbot = ctop + grid_rows.max(0) * cell_h(f);
        let rgn = CreateRectRgn(0, ctop, w, cbot);
        if !rgn.is_invalid() {
            SelectClipRgn(memdc, Some(rgn));
            for (ix, iy, hicon) in &icons_to_draw {
                let _ = DrawIconEx(memdc, *ix, *iy, *hicon, icol, icol, 0, None, DI_NORMAL);
            }
            SelectClipRgn(memdc, None);
            let _ = DeleteObject(HGDIOBJ(rgn.0));
        } else {
            for (ix, iy, hicon) in &icons_to_draw {
                let _ = DrawIconEx(memdc, *ix, *iy, *hicon, icol, icol, 0, None, DI_NORMAL);
            }
        }

        // GDI+ 输出的是直通(straight)alpha,而 AlphaBlend 的 AC_SRC_ALPHA 要求
        // 颜色已按 alpha 预乘。逐像素转预乘(同时乘上 global 做幽灵淡出),否则半透明像素
        // 会被按预乘假定错误合成 → 图标透明处/圆角边缘出现色块、发暗。
        let px = bits as *mut u32;
        let n = (w as usize) * (h as usize);
        let g = global as u32;
        for i in 0..n {
            let p = *px.add(i);
            let a = (p >> 24) & 0xFF;
            if a == 0 {
                *px.add(i) = 0;
                continue;
            }
            let a2 = a * g / 255; // 整体透明度(幽灵淡出)叠加到 alpha
            let b = ((p & 0xFF) * a2) / 255;
            let gr = (((p >> 8) & 0xFF) * a2) / 255;
            let r = (((p >> 16) & 0xFF) * a2) / 255;
            *px.add(i) = (a2 << 24) | (r << 16) | (gr << 8) | b;
        }

        // 提交:UpdateLayeredWindow 整幅替换窗口表面。透明像素透出桌面(真透明),
        // 不透明像素直接显示——没有磨砂,内容移动也不会留残影。缓存保留供重建。
        if let Some(c) = &f.cache {
            submit_ulw(f.hwnd, c);
        }
    }
}
