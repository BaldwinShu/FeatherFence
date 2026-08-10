//! 避让栅栏：把 Explorer 桌面 ListView 中落在栅栏矩形下的图标移到空闲网格。
use std::ffi::c_void;
use std::mem::size_of;

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

/// `reserved_screen` 使用物理屏幕坐标，与顶层栅栏窗口的 cfg 坐标一致。
pub fn reserve(reserved_screen: &[RECT]) {
    if reserved_screen.is_empty() {
        return;
    }
    let Some(list) = crate::utils::find_desktop_listview() else {
        return;
    };
    unsafe {
        // 自动排列会立刻把移开的图标重新填回禁放区，因此启用栅栏避让时关闭该样式。
        let style = GetWindowLongPtrW(list, GWL_STYLE);
        if style & LVS_AUTOARRANGE as isize != 0 {
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
            // Windows 默认按列排桌面图标；从左上开始找第一个未占用且不在栅栏下的格子。
            let max_x = (client.right - cell_w).max(0);
            let max_y = (client.bottom - cell_h).max(0);
            'slots: for x in (0..=max_x).step_by(cell_w as usize) {
                for y in (0..=max_y).step_by(cell_h as usize) {
                    let p = POINT { x, y };
                    if !blocked(p) && !collides(p, idx, &positions) {
                        chosen = Some(p);
                        break 'slots;
                    }
                }
            }
            if let Some(p) = chosen {
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
