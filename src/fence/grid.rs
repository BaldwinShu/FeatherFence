// 网格布局:翻页/动画 + 磁吸吸附/防重叠/防溢出 + 命中测试。
use std::time::{Duration, Instant};

use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};

use crate::config::FenceCfg;
use crate::utils::work_area;

use super::geometry::{cell_h, cell_w, edge, margin, min_h, min_w, rail, title_h};
use super::render::render_fence;
use super::{Fence, ResizeDir};

pub(crate) fn grid_dims(f: &Fence) -> (i32, i32) {
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
pub(crate) fn total_pages(f: &Fence) -> usize {
    let ps = page_size(f);
    if ps == 0 {
        return 1;
    }
    ((f.entries.len() + ps - 1) / ps).max(1)
}

/// 翻页动画计时器 ID
pub(crate) const ANIM_TICK: usize = 0xFE10;
/// 页号收敛到合法范围,顶部行吸附到页首(尺寸/条目变化后调用)
pub(crate) fn sync_page(f: &mut Fence) {
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

/// 翻页目标时长；按真实经过时间推进，慢帧会自然跳过中间位置。
pub(crate) const ANIM_DURATION: Duration = Duration::from_millis(200);
const ANIM_TICK_MS: u32 = 16;

pub(crate) fn animation_progress(elapsed: Duration) -> f32 {
    (elapsed.as_secs_f32() / ANIM_DURATION.as_secs_f32()).min(1.0)
}

/// 启动翻页动画(定时器驱动重绘):记录起始位置,固定时长 cubic ease-out
pub(crate) fn start_page_anim(f: &mut Fence) {
    if !f.animating && !f.hwnd.is_invalid() {
        f.animating = true;
        f.anim_from = f.top_row;
        f.anim_started = Instant::now()
            .checked_sub(Duration::from_millis(ANIM_TICK_MS as u64))
            .unwrap_or_else(Instant::now);
        crate::perf::begin_animation(f.cfg.id);
        unsafe {
            let _ = SetTimer(Some(f.hwnd), ANIM_TICK, ANIM_TICK_MS, None);
        }
    }
}

/// 推进一帧动画:top_row 按固定时长从 anim_from 插值到目标页(cubic ease-out,
/// 起步快、落点稳)。到点吸附并停表。返回是否仍在动画中。
pub(crate) fn step_page_anim(f: &mut Fence) -> bool {
    let (_, rows) = grid_dims(f);
    let target = f.page as f32 * rows as f32;
    let t = animation_progress(f.anim_started.elapsed());
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
pub(crate) fn magnet_smooth(v: f32, step: i32, origin: i32, range: f32) -> f32 {
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
pub(crate) fn magnet_size_smooth(v: f32, step: i32, base: i32, range: f32) -> f32 {
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

pub(crate) fn hit_item(f: &Fence, x: i32, y: i32, cols: i32) -> Option<usize> {
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

pub(crate) fn resize_dir_at(f: &Fence, x: i32, y: i32) -> Option<ResizeDir> {
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

