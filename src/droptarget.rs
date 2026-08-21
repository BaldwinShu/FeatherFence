// 文件拖放:IDropTarget 实现,拖文件进栅栏 → 移动到栅栏目录/收纳箱
use std::cell::Cell;
use std::ffi::c_void;

use windows::core::{implement, Ref, Result, w};
use windows::Win32::Foundation::{HWND, MAX_PATH, POINTL};
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::ReleaseStgMedium;
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl,
};
use windows::Win32::System::SystemServices::{MK_CONTROL, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{DragQueryFileW, ILCombine, ILFree, SHGetPathFromIDListW, HDROP};

#[implement(IDropTarget)]
pub struct FenceDropTarget {
    pub hwnd: HWND,
    accepts: Cell<bool>,
}

impl FenceDropTarget {
    pub fn new(hwnd: HWND) -> Self {
        FenceDropTarget {
            hwnd,
            accepts: Cell::new(false),
        }
    }
}

fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// CFSTR_SHELLIDLIST("Shell IDList Array")的注册剪贴板格式号。
/// 桌面多选、或选区里混入快捷方式/命名空间项(如“控制面板”)时,Explorer 往往
/// 只提供 CIDA 而不给 CF_HDROP,单查 CF_HDROP 会整批取不到路径。
fn shellidlist_format() -> FORMATETC {
    let cf = unsafe { RegisterClipboardFormatW(w!("Shell IDList Array")) as u16 };
    FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// 从数据源允许的效果里挑一个我们能兑现的:我们本就是把文件移进栅栏目录,故优先 MOVE,
/// 其次 COPY,都不允许则 NONE。返回值必须是 allowed 的子集,否则光标显示禁止且 Drop 不触发。
/// 桌面拖快捷方式/含命名空间项时 allowed 常是 MOVE|LINK(0x6)——不含 COPY,若硬编码
/// 返回 COPY 会被判非法,这正是多选拖不进去的根因。
fn pick_effect(allowed: DROPEFFECT, keys: MODIFIERKEYS_FLAGS) -> DROPEFFECT {
    if keys.contains(MK_CONTROL) && allowed.0 & DROPEFFECT_COPY.0 != 0 {
        DROPEFFECT_COPY
    } else if allowed.0 & DROPEFFECT_MOVE.0 != 0 {
        DROPEFFECT_MOVE
    } else if allowed.0 & DROPEFFECT_COPY.0 != 0 {
        DROPEFFECT_COPY
    } else {
        DROPEFFECT_NONE
    }
}

/// 拖入阶段只询问数据源支不支持某格式。不能在此调用 GetData:Explorer 对较大的多选
/// 集合可能延迟生成数据,过早取数会失败并被误判为不支持拖放。
fn supports_paths(dataobj: Option<&IDataObject>) -> bool {
    let Some(dataobj) = dataobj else {
        return false;
    };
    unsafe {
        dataobj.QueryGetData(&hdrop_format()).is_ok()
            || dataobj.QueryGetData(&shellidlist_format()).is_ok()
    }
}

/// 从 CF_HDROP 取路径。
fn paths_from_hdrop(dataobj: &IDataObject) -> Vec<String> {
    unsafe {
        let mut medium = match dataobj.GetData(&hdrop_format()) {
            Ok(m) => m,
            Err(e) => {
                crate::dlog(&format!("[drop] CF_HDROP GetData failed: {e}"));
                return Vec::new();
            }
        };
        let hdrop = HDROP(medium.u.hGlobal.0 as *mut c_void);
        let n = DragQueryFileW(hdrop, 0xFFFFFFFF, None);
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let len = DragQueryFileW(hdrop, i, None);
            let mut buf = vec![0u16; (len + 1) as usize];
            DragQueryFileW(hdrop, i, Some(&mut buf));
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        ReleaseStgMedium(&mut medium);
        out
    }
}

/// 从 CFSTR_SHELLIDLIST(CIDA)取路径。CIDA 布局:u32 cidl + 偏移数组,偏移[0] 指向
/// 父文件夹 PIDL,偏移[1..=cidl] 指向各子项相对 PIDL(偏移量均相对 CIDA 起始)。
/// 父、子 PIDL 用 ILCombine 拼成绝对 PIDL,再 SHGetPathFromIDList 落成文件系统路径;
/// 纯命名空间项(如“控制面板”)没有路径,解析失败会被跳过。
fn paths_from_shellidlist(dataobj: &IDataObject) -> Vec<String> {
    unsafe {
        let mut medium = match dataobj.GetData(&shellidlist_format()) {
            Ok(m) => m,
            Err(e) => {
                crate::dlog(&format!("[drop] CIDA GetData failed: {e}"));
                return Vec::new();
            }
        };
        let hglobal = medium.u.hGlobal;
        let base = GlobalLock(hglobal) as *const u8;
        let mut out = Vec::new();
        if !base.is_null() {
            let offsets = base as *const u32;
            let cidl = *offsets as usize;
            let parent = base.add(*offsets.add(1) as usize) as *const ITEMIDLIST;
            for i in 0..cidl {
                let child = base.add(*offsets.add(2 + i) as usize) as *const ITEMIDLIST;
                let full = ILCombine(Some(parent), Some(child));
                if !full.is_null() {
                    let mut path = [0u16; MAX_PATH as usize];
                    if SHGetPathFromIDListW(full, &mut path).as_bool() {
                        let len = path.iter().position(|&c| c == 0).unwrap_or(path.len());
                        if len > 0 {
                            out.push(String::from_utf16_lossy(&path[..len]));
                        }
                    }
                    ILFree(Some(full));
                }
            }
            let _ = GlobalUnlock(hglobal);
        }
        ReleaseStgMedium(&mut medium);
        out
    }
}

/// 提取被拖入的路径:优先 CF_HDROP;缺失或为空(多选/含命名空间项时常见)时回退 CIDA。
/// 只在 QueryGetData 报告可用时才真正取数,避免对不存在的格式做无谓的失败取数。
fn extract_paths(dataobj: Option<&IDataObject>) -> Vec<String> {
    let Some(dataobj) = dataobj else {
        return Vec::new();
    };
    unsafe {
        if dataobj.QueryGetData(&hdrop_format()).is_ok() {
            let paths = paths_from_hdrop(dataobj);
            if !paths.is_empty() {
                return paths;
            }
        }
        if dataobj.QueryGetData(&shellidlist_format()).is_ok() {
            return paths_from_shellidlist(dataobj);
        }
    }
    Vec::new()
}

impl IDropTarget_Impl for FenceDropTarget_Impl {
    fn DragEnter(
        &self,
        dataobj: Ref<IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> Result<()> {
        unsafe {
            let accepts = supports_paths(dataobj.as_ref());
            self.accepts.set(accepts);
            *pdweffect = if accepts {
                pick_effect(*pdweffect, keys)
            } else {
                DROPEFFECT_NONE
            };
        }
        Ok(())
    }

    fn DragOver(
        &self,
        keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> Result<()> {
        unsafe {
            *pdweffect = if self.accepts.get() {
                pick_effect(*pdweffect, keys)
            } else {
                DROPEFFECT_NONE
            };
        }
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        self.accepts.set(false);
        Ok(())
    }

    fn Drop(
        &self,
        dataobj: Ref<IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> Result<()> {
        unsafe {
            self.accepts.set(false);
            let paths = extract_paths(dataobj.as_ref());
            if paths.is_empty() {
                *pdweffect = DROPEFFECT_NONE;
                return Ok(());
            }
            let selected = pick_effect(*pdweffect, keys);
            let copy_requested = selected == DROPEFFECT_COPY;
            *pdweffect = if crate::handle_drop(self.hwnd, paths, copy_requested) {
                selected
            } else {
                DROPEFFECT_NONE
            };
        }
        Ok(())
    }
}

#[cfg(test)]
mod effect_tests {
    use super::*;

    #[test]
    fn ctrl_prefers_copy_and_plain_drag_prefers_move() {
        let both = DROPEFFECT_COPY | DROPEFFECT_MOVE;

        assert_eq!(pick_effect(both, MK_CONTROL), DROPEFFECT_COPY);
        assert_eq!(
            pick_effect(both, MODIFIERKEYS_FLAGS::default()),
            DROPEFFECT_MOVE
        );
    }

    #[test]
    fn requested_effect_never_exceeds_source_permissions() {
        assert_eq!(pick_effect(DROPEFFECT_MOVE, MK_CONTROL), DROPEFFECT_MOVE);
        assert_eq!(
            pick_effect(DROPEFFECT_COPY, MODIFIERKEYS_FLAGS::default()),
            DROPEFFECT_COPY
        );
    }
}
