// 栅栏窗口:创建(分层窗口)+ fence_wndproc 消息循环(拖动/缩放/翻页/删除/重命名)。
use std::cell::Cell;
use std::mem::size_of;
use std::path::PathBuf;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmFlush, DwmSetWindowAttribute, DWMWA_SYSTEMBACKDROP_TYPE,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DWMSBT_TRANSIENTWINDOW, DWM_SYSTEMBACKDROP_TYPE,
};
use windows::Win32::Graphics::Gdi::{BeginPaint, ClientToScreen, EndPaint, PAINTSTRUCT};
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetActiveWindow, SetCapture, SetFocus, TrackMouseEvent, VK_DELETE, TME_LEAVE,
    TRACKMOUSEEVENT, TRACKMOUSEEVENT_FLAGS,
};
use windows::Win32::System::SystemServices::MK_LBUTTON;
use windows::Win32::UI::Shell::{
    ShellExecuteW, SHFileOperationW, SHFILEOPSTRUCTW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION,
    FOF_NOERRORUI, FO_DELETE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetCursorPos, GetSystemMetrics, GetWindowRect, KillTimer,
    LoadCursorW, PostMessageW, RegisterClassW, SendMessageW, SetCursor, SetForegroundWindow,
    SetTimer, SetWindowPos, ShowWindow, CS_DBLCLKS, HTCLIENT, IDC_ARROW, IDC_SIZENESW, IDC_SIZENS,
    IDC_SIZENWSE, IDC_SIZEWE, IDC_SIZEALL, SM_CXDRAG, SM_CYDRAG, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNA, SW_SHOWNOACTIVATE, SW_SHOWNORMAL, SC_MINIMIZE,
    SIZE_MINIMIZED, WNDCLASSW, WM_CANCELMODE, WM_CAPTURECHANGED, WM_DESTROY, WM_ERASEBKGND,
    WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCACTIVATE, WM_NCCALCSIZE, WM_NCHITTEST, WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR, WM_SIZE,
    WM_SYSCOMMAND, WM_TIMER, WM_DISPLAYCHANGE, WM_DPICHANGED, WS_CAPTION, WS_EX_LAYERED,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::config::{scale_extent_for_dpi, FenceCfg, RenderMode};
use crate::utils::{work_area, wstr};
use crate::{with_global, Global};

use super::geometry::{
    cell_h, cell_w, margin, min_h, min_w, rail, title_h, window_dpi,
};
use super::grid::{
    config_snapshot, grid_dims, hit_item, magnet_size_smooth, magnet_smooth, resize_dir_at,
    settle_fence, start_page_anim, step_page_anim, sync_page, total_pages, ANIM_TICK,
};
use super::menu::{fence_menu, rename_fence};
use super::refresh::{
    refresh_entries, refresh_fence_now, restart_refresh_timer, stop_refresh_timer,
    REFRESH_DEBOUNCE_MS, REFRESH_TICK,
};
use super::render::{continue_perf_animation, render_fence};
use super::{RefreshTimerAction, ResizeDir, WM_APP_DESKTOP_RESTORE, WM_APP_DROP, WM_APP_REFRESH};

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
/// 亚克力触发定时器(一次性,500ms 后补打)。避开 ANIM_TICK=0xFE10 / REFRESH_TICK=0xFE11。
const BACKDROP_NUDGE_TICK: usize = 0xFE12;

/// 触发链里每次 `DwmExtendFrameIntoClientArea` 之后必须补的一发:POC 配方里就在,
/// 移植时漏了。`SWP_FRAMECHANGED` 才让 USER32 失效窗口的非客户区缓存、重算 frame,
/// 没有它 DWM 侧看不到这次 margins 变化 —— 材质建立不起来(实测就是"纯色回退填充")。
fn frame_change(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED | SWP_NOACTIVATE,
        );
    }
}

thread_local! {
    /// WM_NCACTIVATE 重声明的重入保护:内层那发 SendMessageW 会再次进入本分支
    static NCACTIVATE_REASSERTING: Cell<bool> = const { Cell::new(false) };
}

/// 当前是否亚克力渲染模式(全局配置)。
fn acrylic_mode() -> bool {
    with_global(|g| g.config.render_mode == RenderMode::AcrylicBackdrop)
}

/// 亚克力触发链(POC 验证配方):fresh 初设永不渲染 — extend 取值本身无关,必须发生
/// 一次 margins 值变化(-1→0)才触发 DWM 重建客户区材质;建立后锁存、失焦不退化。
/// 创建时连打两发 + BACKDROP_NUDGE_TICK 在 500ms(首次合成后)补第三发,覆盖时序假设。
/// 顺序与 POC 一致:先设材质类型,再打 delta;每次 extend 后 frame_change,末尾 DwmFlush
/// 等一次合成(缺这三样中的任何一样,POC 里都观察不到材质)。
fn apply_acrylic_backdrop(hwnd: HWND) {
    unsafe {
        let bd = DWMSBT_TRANSIENTWINDOW;
        let attr = match DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &bd as *const DWM_SYSTEMBACKDROP_TYPE as *const _,
            size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
        ) {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("ERR {e:?}"),
        };
        let m1 = MARGINS {
            cxLeftWidth: -1,
            cxRightWidth: -1,
            cyTopHeight: -1,
            cyBottomHeight: -1,
        };
        let m0 = MARGINS {
            cxLeftWidth: 0,
            cxRightWidth: 0,
            cyTopHeight: 0,
            cyBottomHeight: 0,
        };
        let e1 = DwmExtendFrameIntoClientArea(hwnd, &m1).is_ok();
        frame_change(hwnd);
        let e2 = DwmExtendFrameIntoClientArea(hwnd, &m0).is_ok();
        frame_change(hwnd);
        let flushed = DwmFlush().is_ok();
        // 材质跟随窗口深浅色:系统在浅色模式下给的是浅色亚克力,而这套皮肤的面板
        // 本来就是深色(#1A1C20),必须显式压成深色材质(PowerToys 同款)。
        let dark = windows::core::BOOL(1);
        let dark_attr = match DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const _,
            size_of::<windows::core::BOOL>() as u32,
        ) {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("ERR {e:?}"),
        };
        // **关键**:DWM 会把"非激活"窗口的材质压平成一块实色(活模糊只给激活窗口),
        // 而栅栏从不被激活 —— 所以材质永远是那块灰色。这一发让它"只为渲染"按激活窗口
        // 画材质,不抢真实焦点、不改变任何交互(PowerToys 同款修法)。
        let _ = SendMessageW(hwnd, WM_NCACTIVATE, Some(WPARAM(1)), Some(LPARAM(0)));
        // 全程留痕:区分"触发链没执行/API失败"与"执行了但系统不渲染"
        crate::dlog(&format!(
            "[acrylic] trigger hwnd=0x{:x} attr={attr} dark={dark_attr} extend(-1)={e1} extend(0)={e2} flush={flushed}",
            hwnd.0 as usize
        ));
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
        // 亚克力模式 = 非分层窗口 + 系统材质(触发链见 apply_acrylic_backdrop);
        // 分层模式(现状)保持 WS_EX_LAYERED + ULW 管线,零改动。
        let acrylic = acrylic_mode();
        let r = CreateWindowExW(
            // 分层:逐像素 alpha,半透明面板真透明透出桌面;圆角由 DWM 裁。
            // 亚克力:不带 WS_EX_LAYERED(分层与系统材质互斥),内容全部走 DirectComposition
            // 表面(见 fence::dcomp)。WS_EX_NOREDIRECTIONBITMAP 让窗口不再有重定向表面 ——
            // 那块表面是 GDI 画布,会把半透明像素混成不透明(用户报告的"没有,不透明"),
            // 也盖住 DWM 材质;DComp-only 窗口(Chromium 同款)不能留着它。
            // 启动时用 SW_SHOWNA 避免抢焦点；用户点击后允许激活，才能接收 Delete。
            if acrylic {
                WS_EX_TOOLWINDOW | WS_EX_NOREDIRECTIONBITMAP
            } else {
                WS_EX_TOOLWINDOW | WS_EX_LAYERED
            },
            w!("FeatherFence"),
            PCWSTR(title_w.as_ptr()),
            // 亚克力必须带 WS_CAPTION:DWM 只对"有框"的窗口合成系统材质,光秃秃的
            // WS_POPUP 会被静默忽略(材质永远不出来)。外观仍无边框 —— WM_NCCALCSIZE
            // 返回 0 把非客户区收成 0(PowerToys 同款修法)。分层模式维持 WS_POPUP。
            if acrylic {
                WS_POPUP | WS_CAPTION
            } else {
                WS_POPUP
            },
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
            if acrylic {
                apply_acrylic_backdrop(hwnd);
            }
            // 插层(R1 实验,只动亚克力):
            // - 分层(现状):Progman 之上、图标列表层之下 —— 保持原 z 序,零改动。
            //   不用 HWND_BOTTOM(会压到 Progman 之下 DWM 隐藏区域);不挂 Progman 父窗口
            //   (分层+高 alpha+Progman 父窗口触发 DWM 命中测试 bug)。
            // - 亚克力:插到全屏图标列表层(SysListView32)之上 —— 疑似栅栏被该全屏窗口
            //   盖住 → DWM 跳过 backdrop 渲染(白面);POC 中"被 ULW 全盖=白"同机制。
            let z_anchor = if acrylic {
                crate::utils::find_desktop_listview()
            } else {
                crate::utils::desktop_insert_host()
            };
            if let Some(host) = z_anchor {
                let _ = SetWindowPos(
                    hwnd,
                    Some(host),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            // 分层窗口:显示后整幅 ULW 提交(逐像素 alpha,透明面板透出桌面)。
            let _ = ShowWindow(hwnd, SW_SHOWNA);
            // 圆角由 DWM 裁
            enable_round(hwnd);
            if acrylic {
                // show 之后再打一发 + 500ms 定时器补发(POC 配方:初设不渲染,靠 delta 触发)
                apply_acrylic_backdrop(hwnd);
                let _ = SetTimer(Some(hwnd), BACKDROP_NUDGE_TICK, 500, None);
            }
            // 首帧渲染(画进缓存 + ULW 提交)
            schedule_render(hwnd);
            // 自检:程序自己测命中(对比外部诊断,区分桌面/进程视角问题)
            let mut rc = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rc);
            let cx = (rc.left + rc.right) / 2;
            let cy = (rc.top + rc.bottom) / 2;
            let _hit = windows::Win32::UI::WindowsAndMessaging::WindowFromPoint(POINT { x: cx, y: cy });
            crate::dlog(&format!(
                "[feather] created hwnd=0x{:x} at ({},{},{},{}) mode={}",
                hwnd.0 as usize,
                rc.left,
                rc.top,
                rc.right,
                rc.bottom,
                if acrylic { "acrylic" } else { "layered" }
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

pub(crate) fn fence_idx(g: &Global, hwnd: HWND) -> Option<usize> {
    g.fences.iter().position(|f| f.valid && f.hwnd == hwnd)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CancelledPointerInteraction {
    geometry_changed: bool,
    visual_changed: bool,
}

/// Clear every state that depends on owning the mouse capture. Windows can revoke capture
/// without delivering WM_LBUTTONUP (for example when another window starts a modal action).
/// Leaving `moving` set in that case makes the fence jump to a later, unrelated mouse move.
fn reset_pointer_interaction(f: &mut super::Fence) -> CancelledPointerInteraction {
    let geometry_changed = (f.moving || f.resizing.is_some()) && f.drag_moved;
    let visual_changed = f.drag_idx.is_some() || f.hover.is_some();
    f.moving = false;
    f.resizing = None;
    f.drag_moved = false;
    f.drag_idx = None;
    f.hover = None;
    CancelledPointerInteraction {
        geometry_changed,
        visual_changed,
    }
}

fn cancel_pointer_interaction(g: &mut Global, idx: usize, reason: &str) {
    if idx >= g.fences.len() {
        return;
    }
    let outcome = reset_pointer_interaction(&mut g.fences[idx]);
    if !outcome.geometry_changed && !outcome.visual_changed {
        return;
    }
    crate::dlog(&format!(
        "[fence] pointer interaction cancelled: id={} reason={} geometry_changed={}",
        g.fences[idx].cfg.id, reason, outcome.geometry_changed
    ));
    if outcome.visual_changed {
        let ghost = g.config.ghost_mode;
        render_fence(&mut g.icons, ghost, &mut g.fences[idx]);
    }
    if outcome.geometry_changed {
        // Keep the last continuously-followed rectangle. Cancellation must not trigger the
        // release-time grid/overlap snap, which is exactly what would look like another jump.
        g.config.fences = config_snapshot(&g.fences);
        crate::config::save(&g.config);
        crate::reserve_desktop_icons(g);
    }
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
        // DWM 只给"激活"窗口画活模糊,非激活窗口的材质被压平成一块实色。栅栏是常驻
        // 组件(点一下/点走一次就闪一次灰,不可接受),所以收到失活通知立刻重新声明
        // "激活" —— 仅影响材质渲染,不改变系统真实的激活窗口、不抢焦点。
        //
        // 注意:**激活那一发必须交给 DefWindowProc**。DWM 的激活态是 DefWindowProc 里
        // 更新的;把它吞掉(return 1)连启动时的材质都建立不起来(踩过)。只有失活那一发
        // 不能落地,否则窗口又被标成非激活、材质压平。
        WM_NCACTIVATE if acrylic_mode() => {
            if wparam.0 == 0 && !NCACTIVATE_REASSERTING.with(|c| c.get()) {
                NCACTIVATE_REASSERTING.with(|c| c.set(true));
                let r = SendMessageW(hwnd, WM_NCACTIVATE, Some(WPARAM(1)), Some(LPARAM(0)));
                NCACTIVATE_REASSERTING.with(|c| c.set(false));
                // 若材质已被压平,重走一遍触发链把它拉回活模糊
                apply_acrylic_backdrop(hwnd);
                return r;
            }
            // 激活/重声明那一发交给 DefWindowProc 并直接返回(不落到函数末尾二次调用)
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        // 亚克力窗带 WS_CAPTION(见 create_window 注释)但不要真的长出标题栏/边框:
        // 返回 0 让客户区 = 整个窗口。分层模式没有 WS_CAPTION,不拦截,走 DefWindowProc。
        WM_NCCALCSIZE if acrylic_mode() => {
            return LRESULT(0);
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
            // 亚克力窗口内容同样不被系统保留,遮挡/还原后需重绘(画到窗口 DC)。
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            }
            let acrylic =
                with_global(|g| g.config.render_mode == RenderMode::AcrylicBackdrop);
            if acrylic {
                schedule_render(hwnd);
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
        WM_CANCELMODE | WM_CAPTURECHANGED => {
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    let reason = if msg == WM_CAPTURECHANGED {
                        "capture changed"
                    } else {
                        "cancel mode"
                    };
                    cancel_pointer_interaction(g, idx, reason);
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
            // A captured drag should always report MK_LBUTTON. Defensively stop stale state if
            // Windows did not deliver the expected button-up/capture-change sequence.
            let has_left_button = wparam.0 & MK_LBUTTON.0 as usize != 0;
            let cancelled = with_global(|g| {
                let Some(idx) = fence_idx(g, hwnd) else {
                    return false;
                };
                let f = &g.fences[idx];
                let active = f.moving || f.resizing.is_some() || f.drag_idx.is_some();
                if active && !has_left_button {
                    cancel_pointer_interaction(g, idx, "mouse move without left button");
                    true
                } else {
                    false
                }
            });
            if cancelled {
                return LRESULT(0);
            }
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
                            f.drag_moved = true;
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
                            f.drag_moved = true;
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
                crate::dragout::start_drag(vec![path.clone()]);
                with_global(|g| {
                    // issue #24 ①:拖出结束后(文件此时已落到桌面)把桌面路径登记为"已知",
                    // 避免自动收纳把用户刚拖到桌面的快捷方式又抓回栅栏。必须在拖出之后登记,
                    // 否则会被 shortcut_tick 末尾的存在性回收提前删除。
                    crate::shortcut::suppress_autocollect_after_dragout(g, std::path::Path::new(&path));
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
                    let avoid = g.config.desktop_avoid;
                    let f = &mut g.fences[idx];
                    // 本按下周期内是否真实移动过(松手时决定要不要 settle)
                    f.drag_moved = false;
                    if y < title_h(f.dpi) {
                        if avoid {
                            crate::desktop_icons::record_fence(&f.cfg);
                        }
                        f.moving = true;
                        let mut cur = POINT::default();
                        let _ = GetCursorPos(&mut cur);
                        let mut rc = RECT::default();
                        let _ = GetWindowRect(hwnd, &mut rc);
                        f.move_off = (cur.x - rc.left, cur.y - rc.top);
                        SetCapture(hwnd);
                    } else if let Some(dir) = resize_dir_at(f, x, y) {
                        if avoid {
                            crate::desktop_icons::record_fence(&f.cfg);
                        }
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
                    // 仅当真的拖动/缩放移动过才整理吸附;单击标题/边缘不触发
                    // settle(否则点一下标题栅栏就跳到最近格点并改变尺寸)
                    let was_drag = (g.fences[idx].moving || g.fences[idx].resizing.is_some())
                        && g.fences[idx].drag_moved;
                    let had_item_press = g.fences[idx].drag_idx.is_some();
                    g.fences[idx].moving = false;
                    g.fences[idx].resizing = None;
                    g.fences[idx].drag_moved = false;
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
            // 右键落在图标上:选中该图标并弹出与桌面一致的系统 Shell 右键菜单;
            // 右键标题栏/空白处:仍打开栅栏菜单(删除/重命名/透明度/图标大小)。
            let x = low16(lparam.0 as usize);
            let y = high16(lparam.0 as usize);
            let mut target: Option<std::path::PathBuf> = None;
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    let ghost = g.config.ghost_mode;
                    let f = &mut g.fences[idx];
                    if y >= title_h(f.dpi) {
                        let (cols, _) = grid_dims(f);
                        if let Some(idx2) = hit_item(f, x, y, cols) {
                            f.selected = Some(idx2);
                            render_fence(&mut g.icons, ghost, f);
                            target = f.entries.get(idx2).map(|e| e.path.clone());
                        }
                    }
                }
            });
            match target {
                Some(path) => {
                    // 客户区 → 屏幕坐标;系统菜单在常驻后台线程构建+弹出,主线程不冻结。
                    let mut pt = POINT { x, y };
                    let _ = ClientToScreen(hwnd, &mut pt);
                    crate::shellmenu::show_for_path_async(path, pt.x, pt.y);
                }
                None => fence_menu(hwnd),
            }
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
                        let finished = f.animating && !step_page_anim(f);
                        render_fence(&mut g.icons, g.config.ghost_mode, f);
                        if finished {
                            continue_perf_animation(f);
                        }
                    }
                });
            } else if wparam.0 == BACKDROP_NUDGE_TICK {
                // 亚克力:500ms 后(DWM 首次合成完)补打一次触发链,一次性不重臂
                let _ = KillTimer(Some(hwnd), BACKDROP_NUDGE_TICK);
                crate::dlog(&format!(
                    "[acrylic] nudge fired hwnd=0x{:x}",
                    hwnd.0 as usize
                ));
                apply_acrylic_backdrop(hwnd);
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
            // 分辨率 / 显示器插拔变化:只把 w/h 从 cfg.dpi 保逻辑换算到新 DPI,
            // 位置(x/y)保持不动。x/y 是用户摆放的物理基准——一旦被 clamp 进新
            // 工作区并落盘就永久污染,分辨率往返(如游戏全屏→退出)后无法还原、
            // 互相挤压。小分辨率期间部分栅栏会处于屏外,分辨率恢复后原样还原;
            // 全屏游戏本来盖住桌面,不需要栅栏可见。
            with_global(|g| {
                if let Some(idx) = fence_idx(g, hwnd) {
                    let f = &mut g.fences[idx];
                    let d = window_dpi(hwnd);
                    let new_dpi = (d.max(1.0) * 96.0).round() as u32;
                    let nw = scale_extent_for_dpi(f.cfg.w, f.cfg.dpi, new_dpi).max(min_w(d));
                    let nh = scale_extent_for_dpi(f.cfg.h, f.cfg.dpi, new_dpi).max(min_h(d));
                    if nw != f.cfg.w || nh != f.cfg.h || (f.dpi - d).abs() > 0.01 {
                        f.dpi = d;
                        f.cfg.w = nw;
                        f.cfg.h = nh;
                        f.cfg.dpi = new_dpi;
                        // f.cfg.x / f.cfg.y 保持不动
                        unsafe {
                            let _ = SetWindowPos(
                                hwnd,
                                None,
                                f.cfg.x,
                                f.cfg.y,
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

#[cfg(test)]
mod pointer_interaction_tests {
    use super::{reset_pointer_interaction, ResizeDir};
    use crate::config::FenceCfg;
    use crate::fence::Fence;
    use windows::Win32::Foundation::HWND;

    #[test]
    fn cancelled_capture_clears_geometry_drag_without_requesting_a_snap() {
        let mut fence = Fence::new(FenceCfg::default(), HWND::default());
        fence.moving = true;
        fence.resizing = Some(ResizeDir::SE);
        fence.drag_moved = true;

        let outcome = reset_pointer_interaction(&mut fence);

        assert!(outcome.geometry_changed);
        assert!(!fence.moving);
        assert!(fence.resizing.is_none());
        assert!(!fence.drag_moved);
    }

    #[test]
    fn cancelled_capture_clears_pending_item_drag() {
        let mut fence = Fence::new(FenceCfg::default(), HWND::default());
        fence.drag_idx = Some(3);
        fence.hover = Some(3);

        let outcome = reset_pointer_interaction(&mut fence);

        assert!(!outcome.geometry_changed);
        assert!(outcome.visual_changed);
        assert!(fence.drag_idx.is_none());
        assert!(fence.hover.is_none());
    }
}
