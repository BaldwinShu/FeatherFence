// 右键菜单:删除/重命名/透明度/图标大小/标题字号 + 重命名输入对话框。
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
    CLEARTYPE_QUALITY, HBRUSH, HGDIOBJ,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetActiveWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetWindowRect, GetWindowTextW, IsDialogMessageW,
    IsWindow, LoadCursorW, RegisterClassW, SendMessageW, SetForegroundWindow, ShowWindow,
    TrackPopupMenu, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CS_DBLCLKS,
    ES_AUTOHSCROLL, HMENU, IDC_ARROW, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, SW_SHOW,
    SW_SHOWNORMAL, TPM_NONOTIFY, TPM_RETURNCMD, TranslateMessage, WM_CLOSE, WM_COMMAND, WM_SETFONT,
    WINDOW_STYLE, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_DLGMODALFRAME,
    WS_EX_TOOLWINDOW, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

use crate::utils::wstr;
use crate::with_global;

use super::geometry::{set_icon_px, set_title_font_px};
use super::grid::{config_snapshot, settle_fence};
use super::render::render_fence;
use super::window::fence_idx;

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
        // 标题字号子菜单(全局统一)
        let cur_title_font = with_global(|g| g.config.title_font_size.max(1));
        let title_font_menu = CreatePopupMenu().unwrap_or_default();
        for (id, size) in [
            (1013u32, 12u32),
            (1014, 14),
            (1015, 16),
            (1016, 18),
            (1017, 20),
            (1018, 24),
        ] {
            let flags = if cur_title_font == size {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            };
            let _ = AppendMenuW(
                title_font_menu,
                flags,
                id as usize,
                PCWSTR(wstr(&format!("{} px", size)).as_ptr()),
            );
        }
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            title_font_menu.0 as usize,
            PCWSTR(w!("标题字号").as_ptr()),
        );
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
        } else if (1013..=1018).contains(&cmd) {
            let size = match cmd {
                1013 => 12,
                1014 => 14,
                1015 => 16,
                1016 => 18,
                1017 => 20,
                _ => 24,
            };
            with_global(|g| {
                set_title_font_px(size);
                g.config.title_font_size = size;
                // 标题栏高度随字号变化;重新吸附可保留完整图标行并避免文字被裁切。
                let n = g.fences.len();
                for i in 0..n {
                    settle_fence(g, i);
                }
            });
        }
    }
}

/// 重命名栅栏:弹输入框,输入非空则更新标题并持久化
pub(crate) fn rename_fence(hwnd: HWND) {
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
pub(crate) fn prompt_text(parent: HWND, title: &str, initial: &str) -> Option<String> {
    static REG: std::sync::Once = std::sync::Once::new();
    REG.call_once(|| unsafe {
        let wc = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(input_wndproc),
            hInstance: crate::hinstance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(6 as *mut std::ffi::c_void), // COLOR_WINDOW+1 = 标准对话框底色
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
