// 文件拖放:IDropTarget 实现,拖文件进栅栏 → 移动到栅栏目录/收纳箱
use std::ffi::c_void;
use std::mem::size_of;

use windows::core::{implement, Interface, Ref, Result};
use windows::Win32::Foundation::{HWND, POINTL};
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, STGMEDIUM, TYMED_HGLOBAL};
use windows::Win32::System::Ole::ReleaseStgMedium;
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::System::Ole::{DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

#[implement(IDropTarget)]
pub struct FenceDropTarget {
    pub hwnd: HWND,
}

impl FenceDropTarget {
    pub fn new(hwnd: HWND) -> Self {
        FenceDropTarget { hwnd }
    }
}

fn extract_paths(dataobj: Option<&IDataObject>) -> Vec<String> {
    let Some(dataobj) = dataobj else {
        return Vec::new();
    };
    unsafe {
        let fmt = FORMATETC {
            cfFormat: CF_HDROP.0,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let mut medium = match dataobj.GetData(&fmt) {
            Ok(m) => m,
            Err(_) => return Vec::new(),
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

fn first_effect(paths: &[String]) -> DROPEFFECT {
    if paths.is_empty() {
        return DROPEFFECT_NONE;
    }
    // 默认 COPY(安全),drop 时再按目标实际移动
    DROPEFFECT_COPY
}

impl IDropTarget_Impl for FenceDropTarget_Impl {
    fn DragEnter(
        &self,
        dataobj: Ref<IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> Result<()> {
        unsafe {
            let paths = extract_paths(dataobj.as_ref());
            *pdweffect = first_effect(&paths);
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> Result<()> {
        unsafe {
            *pdweffect = DROPEFFECT_COPY;
        }
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        Ok(())
    }

    fn Drop(
        &self,
        dataobj: Ref<IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> Result<()> {
        unsafe {
            let paths = extract_paths(dataobj.as_ref());
            if paths.is_empty() {
                *pdweffect = DROPEFFECT_NONE;
                return Ok(());
            }
            *pdweffect = DROPEFFECT_MOVE;
            crate::handle_drop(self.hwnd, paths);
        }
        Ok(())
    }
}
