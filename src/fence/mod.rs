// 栅栏窗口:分层窗口(WS_EX_LAYERED)+ UpdateLayeredWindow 整幅提交(逐像素 alpha)。
// 半透明深色面板 = 真透明(直接透出桌面,无模糊);内容(标题/图标)不透明。
// 圆角由 DWM 裁(DWMWCP_ROUND 对分层窗口同样生效)。
// 注:原生 DWM 亚克力(系统背景)与 GDI 内容不兼容 —— 一画内容整窗就物化成不透明
// 表面盖死磨砂且无法还原,故放弃;分层窗口 + 逐像素 alpha 是唯一可靠方案。
// 本文件是 fence 模块树入口:只留类型/常量 + 对外重导出。
// 职责分放子模块(geometry/render/grid/refresh/window/menu),行为与拆前一致。

mod geometry;
mod grid;
mod menu;
mod refresh;
mod render;
mod window;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::config::FenceCfg;

use refresh::REFRESH_DEBOUNCE_MS;
use render::RenderCache;

// 对外 API 重导出:保持 crate::fence::X 全部调用点不变
pub use geometry::{dpi_scale, min_h, min_w, set_icon_px, set_title_font_px, window_dpi};
pub use grid::{config_snapshot, settle_fence};
pub(crate) use menu::prompt_text;
pub use refresh::refresh_entries;
pub use render::{render_fence, start_perf_animation};
pub use window::{create_window, register_class, schedule_render};

pub const WM_APP_REFRESH: u32 = WM_APP + 1;
pub const WM_APP_DROP: u32 = WM_APP + 2;
/// 目录监听按栅栏 id 通知(消息窗口处理,不持有具体 hwnd):
/// 窗口被 Explorer 销毁重建后 watcher 无需重绑,仍能按 id 找到新窗口
pub const WM_APP_REFRESH_ID: u32 = WM_APP + 6;
/// “显示桌面”会尝试最小化所有独立顶层窗口；异步恢复可避免在 WM_SIZE 内递归。
pub const WM_APP_DESKTOP_RESTORE: u32 = WM_APP + 20;

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
    /// 翻页动画起始时间 + 起始 top_row(固定时长 ease-out 用)
    pub anim_started: Instant,
    pub anim_from: f32,
    perf_anim_remaining: u32,
    /// 滚轮增量累加器(1/120 刻度):触控板/高精度滚轮的小增量先累积,满 120 再翻页
    pub wheel_acc: i32,
    pub hover: Option<usize>,
    /// 单击选中的条目；Delete 键对它执行移入回收站。
    pub selected: Option<usize>,
    pub moving: bool,
    pub move_off: (i32, i32),
    pub resizing: Option<ResizeDir>,
    /// 按下后是否真的拖动/缩放移动过(区分单击标题与拖动:单击不触发 settle)
    pub drag_moved: bool,
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
            anim_started: Instant::now(),
            anim_from: 0.0,
            perf_anim_remaining: 0,
            wheel_acc: 0,
            hover: None,
            selected: None,
            moving: false,
            move_off: (0, 0),
            resizing: None,
            drag_moved: false,
            hover_visible: false,
            drag_idx: None,
            drag_down: (0, 0),
            refresh_signal: RefreshSignal::default(),
            cache: None,
            valid: true,
        }
    }
}
