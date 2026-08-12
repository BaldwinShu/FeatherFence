// 系统工具:DPI、桌面宿主窗口(WorkerW)、宽字符串等
use std::mem::size_of;
use windows::core::{w, BOOL};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITOR_DEFAULTTONEAREST, MONITORINFO};
use windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, FindWindowW, GetClassNameW, GetSystemMetrics,
    SendMessageW, SetProcessDPIAware, SM_CXSCREEN, SM_CYSCREEN,
};

/// UTF-8 -> 以 0 结尾的 UTF-16
pub fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn set_dpi_awareness() {
    unsafe {
        // 尽力而为:新 API 失败就退回旧 API
        if SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err() {
            let _ = SetProcessDPIAware();
        }
    }
}

pub fn screen_size() -> (i32, i32) {
    unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
}

pub fn work_area(hwnd: HWND) -> RECT {
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            let (w, h) = screen_size();
            RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            }
        }
    }
}

static mut FOUND_HOST: Option<HWND> = None;
static mut FOUND_LIST: Option<HWND> = None;
static SPIF_SENT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

unsafe extern "system" fn enum_child_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let mut cls = [0u16; 64];
    let n = GetClassNameW(hwnd, &mut cls);
    if n > 0 {
        let name = String::from_utf16_lossy(&cls[..n as usize]);
        if name == "SHELLDLL_DefView" {
            unsafe { FOUND_HOST = Some(hwnd) };
            return BOOL(0);
        }
    }
    BOOL(1)
}

unsafe extern "system" fn enum_list_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let mut cls = [0u16; 64];
    let n = GetClassNameW(hwnd, &mut cls);
    if n > 0 && String::from_utf16_lossy(&cls[..n as usize]) == "SysListView32" {
        unsafe { FOUND_LIST = Some(hwnd) };
        return BOOL(0);
    }
    BOOL(1)
}

unsafe extern "system" fn enum_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let mut cls = [0u16; 64];
    let n = GetClassNameW(hwnd, &mut cls);
    if n > 0 {
        let name = String::from_utf16_lossy(&cls[..n as usize]);
        if name == "WorkerW" {
            unsafe { FOUND_HOST = None };
            let _ = EnumChildWindows(Some(hwnd), Some(enum_child_proc), LPARAM(0));
            if (unsafe { FOUND_HOST }).is_some() {
                unsafe { FOUND_HOST = Some(hwnd) }; // 返回持有 SHELLDLL_DefView 的 WorkerW 本身
                return BOOL(0);
            }
        }
    }
    BOOL(1)
}

/// 找到桌面图标宿主窗口(WorkerW),栅栏挂到它下面才能随桌面常驻
pub fn find_desktop_host() -> Option<HWND> {
    unsafe {
        FOUND_HOST = None;
        let progman = FindWindowW(w!("Progman"), None).ok();
        if let Some(progman) = progman.filter(|h| !h.is_invalid()) {
            // 先直接找 WorkerW(不要每次发 0x052C —— 它会触发 Progman 重建 WorkerW,
            // 把挂在下面的栅栏窗口级联销毁/移动)
            let _ = EnumWindows(Some(enum_proc), LPARAM(0));
            if let Some(h) = FOUND_HOST {
                return Some(h);
            }
            // 找不到再发一次 0x052C(Win10+ 让 Progman 生成 WorkerW);只发一次,
            // 发完 WorkerW 就存在了,后续直接枚举找到
            let first = SPIF_SENT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            if first {
                SendMessageW(progman, 0x052C, Some(WPARAM(0)), Some(LPARAM(0)));
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            FOUND_HOST = None;
            let _ = EnumWindows(Some(enum_proc), LPARAM(0));
            if let Some(h) = FOUND_HOST {
                return Some(h);
            }
            // 兜底:WorkerW 没找到就用 Progman
            return Some(progman);
        }
        None
    }
}

/// 找到 Explorer 中承载桌面原生图标的 SysListView32。
pub fn find_desktop_listview() -> Option<HWND> {
    unsafe {
        FOUND_LIST = None;
        let host = find_desktop_host()?;
        let _ = EnumChildWindows(Some(host), Some(enum_list_proc), LPARAM(0));
        let first = FOUND_LIST;
        if first.is_none() {
            // Progman 作为兜底宿主时，图标列表可能多嵌套一层。
            if let Ok(progman) = FindWindowW(w!("Progman"), None) {
                let _ = EnumChildWindows(Some(progman), Some(enum_list_proc), LPARAM(0));
            }
        }
        FOUND_LIST
    }
}
