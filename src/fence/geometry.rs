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
/// 缩略图模式:媒体文件用缩略图替代图标。**只换绘制内容,不改网格槽位**——
/// 缩略图就画在当前图标尺寸上,容量与图标模式完全一致。
static THUMB_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 隐藏扩展名:只影响标签的显示文本。`Entry.name`、排序、路径映射、
/// 双击/拖出/系统菜单全部仍用真实文件名。
static HIDE_EXT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 全局栅栏标题字号(逻辑像素):所有栅栏统一
static TITLE_FONT_PX: AtomicU32 = AtomicU32::new(12);
/// 设置全局图标尺寸(物理显示时再乘 DPI)
pub fn set_icon_px(v: u32) {
    ICON_PX.store(v.max(16).min(128), AtomicOrdering::Relaxed);
}
/// 切换缩略图模式(只影响绘制内容与网格裁剪,**不重排栅栏尺寸**)。
pub fn set_thumb_mode(on: bool) {
    THUMB_MODE.store(on, AtomicOrdering::Relaxed);
}
pub fn thumb_mode() -> bool {
    THUMB_MODE.load(AtomicOrdering::Relaxed)
}
/// 切换"隐藏扩展名"(只换标签显示文本,不改任何身份/排序信息)
pub fn set_hide_extensions(on: bool) {
    HIDE_EXT.store(on, AtomicOrdering::Relaxed);
}
pub fn hide_extensions() -> bool {
    HIDE_EXT.load(AtomicOrdering::Relaxed)
}
/// 设置全局栅栏标题字号(物理显示时再乘 DPI)
pub fn set_title_font_px(v: u32) {
    TITLE_FONT_PX.store(v.clamp(10, 32), AtomicOrdering::Relaxed);
}
/// 图标/缩略图边长(物理像素,按所在屏 DPI 缩放);取全局值,0 时回退 32。
/// 缩略图模式复用同一尺寸 —— 格子仍是 44×70,容量不变。
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
