// 系统托盘:图标 + 右键菜单
use std::mem::{size_of, zeroed};

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyMenu, PostMessageW,
    SetForegroundWindow, HICON, ICONINFO, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING,
    MF_UNCHECKED, TrackPopupMenu, TPM_NONOTIFY, TPM_RETURNCMD, WM_NULL,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, Shell_NotifyIconW,
};

use crate::utils::wstr;

pub const WM_APP_TRAY: u32 = 0x8000 + 10;
pub const TRAY_ID: u32 = 1;

// 菜单项 ID
pub const MENU_NEW_PORTAL: u32 = 2001;
pub const MENU_NEW_BOX: u32 = 2002;
pub const MENU_TOGGLE_VIS: u32 = 2003;
pub const MENU_ZEN: u32 = 2004;
pub const MENU_GHOST: u32 = 2005;
pub const MENU_SWEEP: u32 = 2006;
pub const MENU_AUTOSTART: u32 = 2007;
pub const MENU_CONFIG_DIR: u32 = 2008;
pub const MENU_EXIT: u32 = 2009;
pub const MENU_RELOAD: u32 = 2010;
pub const MENU_DOWNLOAD_ENABLED: u32 = 2011;
pub const MENU_DOWNLOAD_VISIBLE: u32 = 2012;
pub const MENU_DESKTOP_AVOID: u32 = 2013;
pub const MENU_DESKTOP_ROLLBACK: u32 = 2014;
pub const MENU_ZEN_HOTKEY: u32 = 2015;

pub fn make_tray_icon() -> HICON {
    // 16x16 三横条"栅栏"图标,带 alpha
    let mut px = [0u32; 16 * 16];
    let bars: [(u32, u32); 3] = [(2, 5), (7, 10), (12, 15)];
    for (r0, r1) in bars {
        for r in r0..=r1 {
            for c in 2..14u32 {
                let corner = (r == r0 || r == r1) && (c == 2 || c == 13);
                if !corner {
                    px[(r * 16 + c) as usize] = 0xE8FFFFFF; // 半透明白
                }
            }
        }
    }
    unsafe {
        let dc = CreateCompatibleDC(None);
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = 16;
        bmi.bmiHeader.biHeight = -16;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
        if !bits.is_null() {
            std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, 256);
        }
        let zero_mask = [0u8; 32];
        let mask = CreateBitmap(16, 16, 1, 1, Some(zero_mask.as_ptr() as *const std::ffi::c_void));
        let ii = ICONINFO {
            fIcon: BOOL(1),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: hbmp,
        };
        let hicon = CreateIconIndirect(&ii).unwrap_or_default();
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        let _ = DeleteDC(dc);
        hicon
    }
}

fn set_tip(nid: &mut NOTIFYICONDATAW, tip: &str) {
    let w = wstr(tip);
    for (i, c) in w.iter().take(127).enumerate() {
        nid.szTip[i] = *c;
    }
    nid.szTip[127] = 0;
}

pub fn add_tray(hwnd: HWND, hicon: HICON) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = zeroed();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ID;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_APP_TRAY;
        nid.hIcon = hicon;
        set_tip(&mut nid, "轻栅栏 Feather Fences");
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

pub fn remove_tray(hwnd: HWND) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = zeroed();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ID;
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// 托盘气泡通知(移入失败等一次性提示)
pub fn notify_tip(hwnd: HWND, title: &str, msg: &str) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = zeroed();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ID;
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_INFO;
        nid.Anonymous.uTimeout = 4000;
        let tw = wstr(title);
        for (i, c) in tw.iter().take(63).enumerate() {
            nid.szInfoTitle[i] = *c;
        }
        nid.szInfoTitle[63] = 0;
        let mw = wstr(msg);
        for (i, c) in mw.iter().take(255).enumerate() {
            nid.szInfo[i] = *c;
        }
        nid.szInfo[255] = 0;
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// 弹出托盘菜单,返回用户选择的命令 ID(0 = 无)
pub fn show_tray_menu(
    hwnd: HWND,
    zen: bool,
    zen_hotkey: Option<&str>,
    ghost: bool,
    autostart: bool,
    download_enabled: bool,
    download_visible: bool,
    desktop_avoid: bool,
) -> u32 {
    unsafe {
        let menu = CreatePopupMenu().unwrap_or_default();
        let zen_label = zen_hotkey
            .map(|hotkey| format!("Zen 模式\t{hotkey}"))
            .unwrap_or_else(|| "Zen 模式".into());
        let zen_label_w = wstr(&zen_label);
        let _ = AppendMenuW(menu, MF_STRING, MENU_NEW_PORTAL as usize, PCWSTR(w!("新建文件夹栅栏…").as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, MENU_NEW_BOX as usize, PCWSTR(w!("新建收纳栅栏").as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_TOGGLE_VIS as usize, PCWSTR(w!("隐藏/显示全部栅栏").as_ptr()));
        let _ = AppendMenuW(
            menu,
            if zen { MF_STRING | MF_CHECKED } else { MF_STRING | MF_UNCHECKED },
            MENU_ZEN as usize,
            PCWSTR(zen_label_w.as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_ZEN_HOTKEY as usize,
            PCWSTR(w!("设置 Zen 快捷键…").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            if ghost { MF_STRING | MF_CHECKED } else { MF_STRING | MF_UNCHECKED },
            MENU_GHOST as usize,
            PCWSTR(w!("Ghost 模式(悬停显现)").as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_STRING, MENU_SWEEP as usize, PCWSTR(w!("立即整理桌面").as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            menu,
            if download_enabled { MF_STRING | MF_CHECKED } else { MF_STRING | MF_UNCHECKED },
            MENU_DOWNLOAD_ENABLED as usize,
            PCWSTR(w!("下载接管").as_ptr()),
        );
        let visible_flags = if download_visible { MF_STRING | MF_CHECKED } else { MF_STRING | MF_UNCHECKED };
        let _ = AppendMenuW(
            menu,
            if download_enabled { visible_flags } else { visible_flags | MF_GRAYED },
            MENU_DOWNLOAD_VISIBLE as usize,
            PCWSTR(w!("显示下载收纳箱").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            if desktop_avoid { MF_STRING | MF_CHECKED } else { MF_STRING | MF_UNCHECKED },
            MENU_DESKTOP_AVOID as usize,
            PCWSTR(w!("桌面图标避让").as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_STRING, MENU_DESKTOP_ROLLBACK as usize, PCWSTR(w!("撤销并关闭避让").as_ptr()));
        let _ = AppendMenuW(menu, MF_GRAYED, 0, PCWSTR(w!("搬移后 1 分钟内可撤销").as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            menu,
            if autostart { MF_STRING | MF_CHECKED } else { MF_STRING | MF_UNCHECKED },
            MENU_AUTOSTART as usize,
            PCWSTR(w!("开机自启").as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_STRING, MENU_RELOAD as usize, PCWSTR(w!("重新加载配置").as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, MENU_CONFIG_DIR as usize, PCWSTR(w!("打开配置目录").as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_EXIT as usize, PCWSTR(w!("退出").as_ptr()));

        let mut pt = windows::Win32::Foundation::POINT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);

        // 托盘菜单必须由一个前台顶层窗口拥有，否则 Windows 不会可靠地在用户
        // 点击菜单外部时结束 TrackPopupMenu（菜单会一直留在屏幕上）。
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        // 按照 Win32 托盘菜单约定，在 TrackPopupMenu 返回后投递一条消息，
        // 确保系统完成菜单的关闭与前台切换。
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd.0 as u32
    }
}
