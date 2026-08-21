
// 栅栏生命周期:创建/删除/可见性 + 桌面图标避让编排 + Explorer 重启看门狗 + 拖放处理。
use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::System::Ole::RegisterDragDrop;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, GetWindow, GetWindowRect, GW_HWNDPREV, HWND_TOP, IsIconic, IsWindow,
    IsWindowVisible, PostMessageW, SetWindowPos, ShowWindow, SW_HIDE, SW_SHOWNA, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
};

use crate::config::{self, FenceCfg};
use crate::desktop_icons;
use crate::download::download_box_should_show;
use crate::droptarget;
use crate::fence::{self, Fence, WM_APP_DROP};
use crate::perf;
use crate::tray;
use crate::utils::{self, wstr};
use crate::watcher;
use crate::{with_global, Global};

use super::{ManagedWatcher, WatcherOwner};

// ---------- 栅栏生命周期 ----------

pub(crate) fn create_fence(g: &mut Global, mut cfg: FenceCfg) -> u32 {
    if cfg.id == 0 {
        cfg.id = g.next_id;
        g.next_id += 1;
    }
    // 默认位置:屏幕右上角级联(按系统 DPI 缩放逻辑像素偏移;创建窗口前无 hwnd)。
    // 判定"未放置"用 pos_set:旧配置缺字段且 x/y 恰为 (0,0) 时视为未放置;
    // 本版本保存过的位置(含真正的 (0,0))一律原样恢复。
    let ms = fence::dpi_scale();
    if cfg.pos_set != Some(true) && cfg.x == 0 && cfg.y == 0 {
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
    f.cfg.pos_set = Some(true); // 位置已确定(含恰好 (0,0) 的情况)
    fence::refresh_entries(&mut f, &config::vault_dir(&g.config));
    fence::render_fence(&mut g.icons, g.config.ghost_mode, &mut f);
    let id = f.cfg.id;
    // 目录监听(所有栅栏):文件夹栅栏监听门户目录,收纳栅栏监听收纳箱目录。
    // 通知按栅栏 id 发给消息窗口——不持有具体 hwnd,窗口被 Explorer 销毁重建后
    // watcher 无需重绑仍能按 id 找到新窗口;删除栅栏/重载时随 ManagedWatcher 停止。
    let watch_dir = f.cfg.folder.clone().unwrap_or_else(|| config::vault_dir(&g.config));
    let fid = id;
    let mhwnd = g.msg_hwnd.0 as usize;
    g.fences.push(f);
    // 新栅栏立即落到网格:尺寸/位置吸附 + clamp 工作区 + 消除重叠
    let new_idx = g.fences.len() - 1;
    fence::settle_fence(g, new_idx);

    let watcher = watcher::spawn_dir_watcher(watch_dir, move |_names| {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(mhwnd as *mut c_void)),
                fence::WM_APP_REFRESH_ID,
                WPARAM(fid as usize),
                LPARAM(0),
            );
        }
    });
    g.watchers.push(ManagedWatcher::fence(fid, watcher));
    sync_config(g);
    id
}

pub(crate) fn delete_fence(g: &mut Global, idx: usize) {
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
pub(crate) fn sync_config(g: &mut Global) {
    g.config.fences = fence::config_snapshot(&g.fences);
    config::save(&g.config);
}

pub(crate) fn apply_visibility(g: &mut Global) {
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

pub(crate) fn reserve_desktop_icons(g: &Global) {
    if !g.config.desktop_avoid || perf::safe_desktop() {
        return;
    }
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

/// 「撤销并关闭避让」:把避让期间被搬走的图标写回原位、被移动的栅栏恢复原状,
/// 然后关闭避让并恢复自动排列样式。
pub(crate) fn rollback_desktop(g: &mut Global) {
    g.config.desktop_avoid = false; // 先关闭,后续 settle 内部的 reserve 会被 gate 住
    desktop_icons::rollback_icons();
    for (id, cfg) in desktop_icons::take_fence_history() {
        if let Some(idx) = g.fences.iter().position(|f| f.cfg.id == id) {
            // 恢复移动前的几何,再由 settle 磁吸回网格、同步分页并保存
            g.fences[idx].cfg.x = cfg.x;
            g.fences[idx].cfg.y = cfg.y;
            g.fences[idx].cfg.w = cfg.w;
            g.fences[idx].cfg.h = cfg.h;
            fence::settle_fence(g, idx);
        }
    }
    desktop_icons::restore_autoarrange();
    g.config.fences = fence::config_snapshot(&g.fences);
    config::save(&g.config);
}
// ---------- 桌面宿主重连(Explorer 重启防护) ----------

pub(crate) fn watchdog_tick(g: &mut Global) {
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
                // watcher 仍挂在 f 上且按栅栏 id 通知,新窗口自动恢复实时刷新
                fence::refresh_entries(f, &config::vault_dir(&g.config));
                fence::render_fence(&mut g.icons, g.config.ghost_mode, f);
            }
        }
        // 周期回位:任何原因把栅栏从桌面层顶起时,3s 内插回桌面层之上。
        // 用 desktop_insert_host(Progman 之后)而非 HWND_BOTTOM ——
        // HWND_BOTTOM 会把窗口压进 Progman 之下的 DWM 隐藏区域(不可见)。
        if f.valid {
            if let Some(host) = utils::desktop_insert_host() {
                unsafe {
                    let _ = SetWindowPos(
                        f.hwnd,
                        Some(host),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
        }
    }
    // Explorer 重启、用户刷新桌面或新图标出现后，再次维护禁放区。
    reserve_desktop_icons(g);
}

/// 维护严格的“所有应用窗口 > 栅栏 > Explorer 桌面”层级。
/// 栅栏永不使用 TOPMOST；Show Desktop 改写 Z 序后，也只把它插回桌面宿主正上方。
pub(crate) fn desktop_layer_tick(g: &mut Global) {
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

fn format_drop_failures(
    target: &Path,
    succeeded: usize,
    failures: &[(String, String)],
    copy_requested: bool,
) -> String {
    const MAX_DETAILS: usize = 5;
    let operation = if copy_requested { "复制" } else { "移动" };
    let mut message = format!(
        "有 {} 个项目未能{}到：\n{}\n\n",
        failures.len(),
        operation,
        target.display()
    );
    if succeeded > 0 {
        message.push_str(&format!("已成功{operation} {succeeded} 个项目。\n\n"));
    }
    for (path, error) in failures.iter().take(MAX_DETAILS) {
        let name = Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| path.into());
        message.push_str(&format!("• {name}：{error}\n"));
    }
    if failures.len() > MAX_DETAILS {
        message.push_str(&format!("• 另有 {} 个项目未列出\n", failures.len() - MAX_DETAILS));
    }
    message.push_str("\n请检查原位置和目标目录后重试。程序不会主动删除未成功移动的源项目。");
    message
}

/// 处理拖入并返回是否至少复制/移动了一个项目，供 OLE 向数据源报告真实结果。
pub(crate) fn handle_drop(hwnd: HWND, paths: Vec<String>, copy_requested: bool) -> bool {
    let result = with_global(|g| {
        let Some(idx) = g.fences.iter().position(|f| f.valid && f.hwnd == hwnd) else {
            return None;
        };
        let target = if let Some(folder) = g.fences[idx].cfg.folder.clone() {
            folder
        } else {
            let vault = config::vault_dir(&g.config);
            if !config::ensure_dir(&vault) {
                let failures = paths
                    .iter()
                    .map(|path| (path.clone(), "无法创建或访问栅栏存储目录".to_string()))
                    .collect();
                return Some((vault, 0, failures));
            }
            vault
        };
        let mut succeeded = 0usize;
        let mut failures = Vec::new();
        for p in &paths {
            let src = PathBuf::from(p);
            if !src.exists() {
                failures.push((p.clone(), "源项目不存在或已被移动".to_string()));
                continue;
            }
            // MOVE 到自身目录没有意义；COPY 到自身目录则应生成一个重名副本。
            if !copy_requested
                && src.parent().map(|d| d == target.as_path()).unwrap_or(false)
            {
                continue;
            }
            let operation = if copy_requested {
                watcher::copy_to_dir(&src, &target)
            } else {
                watcher::move_to_dir(&src, &target)
            };
            match operation {
                Ok(_) => succeeded += 1,
                Err(e) => {
                    let name = if copy_requested { "copy" } else { "move" };
                    eprintln!("[feather] {name} {p} -> {} failed: {e}", target.display());
                    failures.push((p.clone(), e));
                }
            }
        }
        if succeeded > 0 {
            unsafe { let _ = PostMessageW(Some(hwnd), WM_APP_DROP, WPARAM(0), LPARAM(0)); };
        }
        if !failures.is_empty() {
            // 拖放失败静默是历史缺陷:至少给用户一个托盘气泡提示
            tray::notify_tip(
                g.msg_hwnd,
                "轻栅栏",
                &format!(
                    "{} 个文件未能移入目标目录(可能被占用或跨卷移动文件夹)",
                    failures.len()
                ),
            );
        }
        Some((target, succeeded, failures))
    });

    let Some((target, succeeded, failures)) = result else {
        return false;
    };
    if !failures.is_empty() {
        let message = format_drop_failures(&target, succeeded, &failures, copy_requested);
        let message_w = wstr(&message);
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                Some(hwnd),
                PCWSTR(message_w.as_ptr()),
                w!("FeatherFence - 文件移动失败"),
                windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE(0x10),
            );
        }
    }
    succeeded > 0
}
#[cfg(test)]
mod drop_feedback_tests {
    use super::format_drop_failures;
    use std::path::Path;

    #[test]
    fn failure_message_limits_details_and_reports_partial_success() {
        let failures: Vec<_> = (0..7)
            .map(|i| (format!("C:\\source\\file-{i}.txt"), "拒绝访问".to_string()))
            .collect();

        let message = format_drop_failures(Path::new("D:\\target"), 2, &failures, false);

        assert!(message.contains("有 7 个项目未能移动"));
        assert!(message.contains("已成功移动 2 个项目"));
        assert!(message.contains("file-0.txt"));
        assert!(!message.contains("file-5.txt"));
        assert!(message.contains("另有 2 个项目未列出"));
    }
}
