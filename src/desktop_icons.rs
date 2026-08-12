//! 避让栅栏：把 Explorer 桌面 ListView 中落在栅栏矩形下的图标移到空闲网格。
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 撤销窗口：图标/栅栏被搬移后 1 分钟内可「撤销并关闭避让」,超时记录作废,
/// 防止长期开启避让时 static 历史无限累积。
const ROLLBACK_TTL_SECS: u64 = 60;

/// 避让开启前桌面 ListView 的自动排列状态;关闭避让时按它原样恢复,
/// 避免把用户手动关掉自动排列的自定义布局强制打开。
static AUTOARRANGE_WAS_ON: AtomicBool = AtomicBool::new(false);
/// 避让期间被搬走的图标:index -> (搬移前坐标, 搬移时刻)。撤销时写回原位。
static ICON_HISTORY: Mutex<Option<HashMap<u32, (POINT, u64)>>> = Mutex::new(None);
/// 避让期间被移动/缩放的栅栏:id -> (移动前完整配置, 移动时刻)。撤销时恢复原状。
static FENCE_HISTORY: Mutex<Option<HashMap<u32, (crate::config::FenceCfg, u64)>>> =
    Mutex::new(None);

use windows::Win32::Foundation::{CloseHandle, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_VM_OPERATION, PROCESS_VM_READ};
use windows::Win32::UI::Controls::{
    LVM_GETITEMCOUNT, LVM_GETITEMPOSITION, LVM_GETITEMSPACING, LVM_SETITEMPOSITION,
    LVS_AUTOARRANGE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetWindowLongPtrW, GetWindowThreadProcessId, SendMessageW, SetWindowLongPtrW,
    GWL_STYLE,
};

fn overlaps(a: RECT, b: RECT) -> bool {
    a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `reserved_screen` 使用物理屏幕坐标，与顶层栅栏窗口的 cfg 坐标一致。
pub fn reserve(reserved_screen: &[RECT]) {
    if reserved_screen.is_empty() {
        return;
    }
    let Some(list) = crate::utils::find_desktop_listview() else {
        return;
    };
    unsafe {
        // 自动排列会立刻把移开的图标重新填回禁放区，因此启用栅栏避让时关闭该样式；
        // 同时记住原始状态，供关闭避让时原样恢复。
        let style = GetWindowLongPtrW(list, GWL_STYLE);
        if style & LVS_AUTOARRANGE as isize != 0 {
            AUTOARRANGE_WAS_ON.store(true, Ordering::Relaxed);
            SetWindowLongPtrW(list, GWL_STYLE, style & !(LVS_AUTOARRANGE as isize));
        }
        let mut reserved = Vec::with_capacity(reserved_screen.len());
        for r in reserved_screen {
            let mut tl = POINT {
                x: r.left,
                y: r.top,
            };
            let mut br = POINT {
                x: r.right,
                y: r.bottom,
            };
            if ScreenToClient(list, &mut tl).as_bool() && ScreenToClient(list, &mut br).as_bool() {
                reserved.push(RECT {
                    left: tl.x,
                    top: tl.y,
                    right: br.x,
                    bottom: br.y,
                });
            }
        }
        if reserved.is_empty() {
            return;
        }

        let count = SendMessageW(list, LVM_GETITEMCOUNT, Some(WPARAM(0)), Some(LPARAM(0))).0 as i32;
        if count <= 0 {
            return;
        }
        let packed =
            SendMessageW(list, LVM_GETITEMSPACING, Some(WPARAM(0)), Some(LPARAM(0))).0 as u32;
        let cell_w = ((packed & 0xffff) as i32).max(48);
        let cell_h = ((packed >> 16) as i32).max(48);
        let mut client = RECT::default();
        if GetClientRect(list, &mut client).is_err() {
            return;
        }

        let mut pid = 0u32;
        GetWindowThreadProcessId(list, Some(&mut pid));
        let Ok(process) = OpenProcess(PROCESS_VM_OPERATION | PROCESS_VM_READ, false, pid) else {
            return;
        };
        let remote = VirtualAllocEx(
            process,
            None,
            size_of::<POINT>(),
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if remote.is_null() {
            let _ = CloseHandle(process);
            return;
        }

        let mut positions = Vec::with_capacity(count as usize);
        for i in 0..count {
            let ok = SendMessageW(
                list,
                LVM_GETITEMPOSITION,
                Some(WPARAM(i as usize)),
                Some(LPARAM(remote as isize)),
            )
            .0 != 0;
            let mut p = POINT::default();
            if ok
                && ReadProcessMemory(
                    process,
                    remote,
                    &mut p as *mut POINT as *mut c_void,
                    size_of::<POINT>(),
                    None,
                )
                .is_ok()
            {
                positions.push(p);
            } else {
                positions.push(POINT {
                    x: -10000,
                    y: -10000,
                });
            }
        }

        let item_rect = |p: POINT| RECT {
            left: p.x,
            top: p.y,
            right: p.x + cell_w,
            bottom: p.y + cell_h,
        };
        let blocked = |p: POINT| reserved.iter().any(|r| overlaps(item_rect(p), *r));
        let collides = |p: POINT, skip: usize, all: &[POINT]| {
            all.iter().enumerate().any(|(i, q)| {
                i != skip && (p.x - q.x).abs() < cell_w / 2 && (p.y - q.y).abs() < cell_h / 2
            })
        };

        for idx in 0..positions.len() {
            if !blocked(positions[idx]) {
                continue;
            }
            let mut chosen = None;
            // 就近搬移：以图标当前位置为圆心,找最近的空闲网格(不在栅栏下、
            // 也不与任何图标重叠),尽量不打乱既有布局。仅搬走被栅栏盖住的图标。
            let ox = positions[idx].x;
            let oy = positions[idx].y;
            let mut best_d2 = i64::MAX;
            let max_x = (client.right - cell_w).max(0);
            let max_y = (client.bottom - cell_h).max(0);
            let mut x = 0;
            while x <= max_x {
                let mut y = 0;
                while y <= max_y {
                    let p = POINT { x, y };
                    if !blocked(p) && !collides(p, idx, &positions) {
                        let d2 = (x as i64 - ox as i64).pow(2) + (y as i64 - oy as i64).pow(2);
                        if d2 < best_d2 {
                            best_d2 = d2;
                            chosen = Some(p);
                        }
                    }
                    y += cell_h;
                }
                x += cell_w;
            }
            if let Some(p) = chosen {
                // 记录搬移前的原始位置(每个图标只记一次),供「撤销并关闭避让」写回;
                // 同时按存活时间清理过期记录,防止长期使用无限累积。
                {
                    let mut guard = ICON_HISTORY.lock().unwrap_or_else(|e| e.into_inner());
                    let map = guard.get_or_insert_with(HashMap::new);
                    let now = now_unix();
                    map.retain(|_, (_, t)| now.saturating_sub(*t) < ROLLBACK_TTL_SECS);
                    map.entry(idx as u32).or_insert((positions[idx], now));
                }
                SendMessageW(
                    list,
                    LVM_SETITEMPOSITION,
                    Some(WPARAM(idx)),
                    Some(LPARAM(
                        (((p.y as u32) & 0xffff) << 16 | ((p.x as u32) & 0xffff)) as isize,
                    )),
                );
                positions[idx] = p;
            }
        }

        let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
        let _ = CloseHandle(process);
    }
}

/// 记录栅栏移动/缩放前的配置快照(每个 id 只记一次)。由栅栏 WM_LBUTTONDOWN
/// 进入拖动/缩放时调用——此时窗口还在原位。
pub fn record_fence(cfg: &crate::config::FenceCfg) {
    let now = now_unix();
    let mut guard = FENCE_HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    map.retain(|_, (_, t)| now.saturating_sub(*t) < ROLLBACK_TTL_SECS);
    map.entry(cfg.id).or_insert((cfg.clone(), now));
}

/// 撤销图标搬移:把存活期内被搬走的图标写回原位。返回成功写回的数量。
pub fn rollback_icons() -> usize {
    let Some(list) = crate::utils::find_desktop_listview() else {
        clear_history();
        return 0;
    };
    unsafe {
        let now = now_unix();
        let mut restored = 0;
        let history = ICON_HISTORY.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(map) = history {
            for (idx, (p, t)) in &map {
                if now.saturating_sub(*t) >= ROLLBACK_TTL_SECS {
                    continue; // 已过期,视为在新位置稳定,不回退
                }
                let lp = (((p.y as u32) & 0xffff) << 16 | ((p.x as u32) & 0xffff)) as isize;
                let _ = SendMessageW(
                    list,
                    LVM_SETITEMPOSITION,
                    Some(WPARAM(*idx as usize)),
                    Some(LPARAM(lp)),
                );
                restored += 1;
            }
        }
        restored
    }
}

/// 取出存活期内的栅栏移动前快照(供恢复几何),并清空历史。
pub fn take_fence_history() -> Vec<(u32, crate::config::FenceCfg)> {
    let now = now_unix();
    let map = FENCE_HISTORY.lock().unwrap_or_else(|e| e.into_inner()).take();
    match map {
        Some(m) => m
            .into_iter()
            .filter(|(_, (_, t))| now.saturating_sub(*t) < ROLLBACK_TTL_SECS)
            .map(|(id, (cfg, _))| (id, cfg))
            .collect(),
        None => Vec::new(),
    }
}

/// 清空所有搬移/移动历史(关闭避让但不撤销时调用,图标保持当前位置)。
pub fn clear_history() {
    let _ = ICON_HISTORY.lock().unwrap_or_else(|e| e.into_inner()).take();
    let _ = FENCE_HISTORY.lock().unwrap_or_else(|e| e.into_inner()).take();
}

/// 关闭避让时调用：仅恢复桌面 ListView 在避让开启前的自动排列状态
/// (只当避让开启时由我们关掉自动排列才重新打开;用户原本手动关掉的
/// 自定义布局保持原样,不被强制吸附)。图标位置不做任何改动。
pub fn restore_autoarrange() {
    let Some(list) = crate::utils::find_desktop_listview() else {
        return;
    };
    unsafe {
        let style = GetWindowLongPtrW(list, GWL_STYLE);
        if AUTOARRANGE_WAS_ON.swap(false, Ordering::Relaxed)
            && style & LVS_AUTOARRANGE as isize == 0
        {
            let _ = SetWindowLongPtrW(list, GWL_STYLE, style | (LVS_AUTOARRANGE as isize));
        }
    }
}
