// 几何 / DPI:栅栏几何、图标/标题尺寸,全部按物理像素,随所在屏 DPI 缩放。
// 供 render/grid/window 共享的助手标 pub(crate);对外尺寸常量标 pub。
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

use windows::Win32::Foundation::HWND;

use super::Fence;

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
pub(crate) fn title_h(d: f32) -> i32 {
    ((TITLE_FONT_PX.load(AtomicOrdering::Relaxed) as f32 + 18.0) * d).round() as i32
}
pub(crate) fn edge(d: f32) -> i32 {
    (8.0 * d) as i32
}
pub(crate) fn margin(d: f32) -> i32 {
    (10.0 * d) as i32
}
/// 页面圆点轨道宽度:图标网格让出右侧竖条,圆点不压到图标上
pub(crate) fn rail(d: f32) -> i32 {
    (22.0 * d) as i32
}
/// 全局图标尺寸(逻辑像素):所有栅栏统一
static ICON_PX: AtomicU32 = AtomicU32::new(32);
/// 全局栅栏标题字号(逻辑像素):所有栅栏统一
static TITLE_FONT_PX: AtomicU32 = AtomicU32::new(12);
/// 设置全局图标尺寸(物理显示时再乘 DPI)
pub fn set_icon_px(v: u32) {
    ICON_PX.store(v.max(16).min(128), AtomicOrdering::Relaxed);
}
/// 设置全局栅栏标题字号(物理显示时再乘 DPI)
pub fn set_title_font_px(v: u32) {
    TITLE_FONT_PX.store(v.clamp(10, 32), AtomicOrdering::Relaxed);
}
/// 图标尺寸(物理像素,按所在屏 DPI 缩放);取全局值,0 时回退 32
pub(crate) fn icon(f: &Fence) -> i32 {
    let base = ICON_PX.load(AtomicOrdering::Relaxed);
    let base = if base == 0 { 32 } else { base };
    (base as f32 * f.dpi).round() as i32
}
pub(crate) fn label_h(d: f32) -> i32 {
    // 容纳 12px 原生字号的两行标签(换行) + 投影
    (38.0 * d) as i32
}
pub(crate) fn cell_w(f: &Fence) -> i32 {
    icon(f) + 12
}
pub(crate) fn cell_h(f: &Fence) -> i32 {
    icon(f) + label_h(f.dpi)
}
pub fn min_w(d: f32) -> i32 {
    (180.0 * d) as i32
}
pub fn min_h(d: f32) -> i32 {
    (100.0 * d) as i32
}
pub(crate) fn font_title(d: f32) -> f32 {
    TITLE_FONT_PX.load(AtomicOrdering::Relaxed) as f32 * d
}
pub(crate) fn font_label(d: f32) -> f32 {
    // Windows 桌面图标的标签字号:9pt = 12px(逻辑像素,随 DPI 缩放)
    12.0 * d
}
pub(crate) const FONT_NAME: &str = "Microsoft YaHei UI";
