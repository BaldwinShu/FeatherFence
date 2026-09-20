// 拖出:把栅栏里的文件/文件夹拖到桌面、资源管理器等目标(移动或复制)。
// 用 OLE DoDragDrop:自定义 IDataObject 提供 CF_HDROP(文件路径列表),
// 自定义 IDropSource 管理拖拽过程(松左键落下 / Esc 取消)。
// 目标端(桌面/文件夹窗口/其他栅栏)负责实际移动文件;拖回本栅栏由现有
// drop target 接住(文件已在自身目录则跳过 → 无操作)。
use std::cell::Cell;
use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use windows::core::{implement, Error, Ref, Result, BOOL, HRESULT};
use windows::Win32::Foundation::{
    GlobalFree, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, E_INVALIDARG,
    E_NOTIMPL, E_UNEXPECTED, HGLOBAL, POINT, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{
    IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumFORMATETC_Impl, IEnumSTATDATA,
    DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY,
    DROPEFFECT_MOVE, DROPEFFECT_NONE, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{CFSTR_PERFORMEDDROPEFFECT, DROPFILES};

/// DATA_E_FORMATETC:请求的格式不是 CF_HDROP
const DATA_E_FORMATETC: HRESULT = HRESULT(0x80040064_u32 as _);

fn final_drop_effect(ole_effect: DROPEFFECT, shell_reported: u32) -> DROPEFFECT {
    let allowed = DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0;
    let bits = if shell_reported & allowed != 0 {
        shell_reported
    } else {
        ole_effect.0
    };
    DROPEFFECT(bits & allowed)
}

/// 拖出指定路径:阻塞到拖拽结束。返回实际拖放效果(MOVE/COPY/NONE)。
pub fn start_drag(paths: Vec<String>) -> DROPEFFECT {
    crate::dlog(&format!("[dragout] start drag: {}", paths.join("; ")));
    unsafe {
        let performed_effect = Arc::new(AtomicU32::new(DROPEFFECT_NONE.0));
        let dataobj: IDataObject = FileDataObject {
            paths,
            performed_effect: Arc::clone(&performed_effect),
        }
        .into();
        let src: IDropSource = FileDropSource.into();
        let mut effect = DROPEFFECT_NONE;
        let hr = DoDragDrop(
            &dataobj,
            &src,
            DROPEFFECT_COPY | DROPEFFECT_MOVE,
            &mut effect,
        );
        // 诊断:hr 里能看到 E_UNEXPECTED/CO_E_* 等失败原因;effect 为 NONE 表示目标拒绝。
        let shell_reported = performed_effect.load(Ordering::Relaxed);
        let final_effect = final_drop_effect(effect, shell_reported);
        let name = if final_effect.0 & DROPEFFECT_COPY.0 != 0 {
            "copy"
        } else if final_effect.0 & DROPEFFECT_MOVE.0 != 0 {
            "move"
        } else {
            "none"
        };
        crate::dlog(&format!(
            "[dragout] DoDragDrop hr=0x{:08x} effect={} performed={} final={}",
            hr.0 as u32,
            effect.0,
            shell_reported,
            name
        ));
        final_effect
    }
}

/// 拖拽数据源:持有拖出文件列表,GetData 按需构造 CF_HDROP。
/// 其余 IDataObject 方法返回 E_NOTIMPL(拖出文件到资源管理器只需要 GetData)。
#[implement(IDataObject)]
pub struct FileDataObject {
    /// 拖出的绝对路径
    pub paths: Vec<String>,
    /// Explorer 可通过 CFSTR_PERFORMEDDROPEFFECT 回写真正执行的 COPY/MOVE。
    performed_effect: Arc<AtomicU32>,
}

/// 构造 CF_HDROP 全局内存块:DROPFILES 头 + 宽字符路径序列(每段 0 结尾,整体双 0 结尾)。
/// 返回的 HGLOBAL 由接收方 ReleaseStgMedium 释放。
unsafe fn build_hdrop(paths: &[String]) -> Result<HGLOBAL> {
    let mut buf: Vec<u16> = Vec::new();
    for p in paths {
        buf.extend(p.encode_utf16());
        buf.push(0);
    }
    buf.push(0); // 双空结尾
    let payload = buf.len() * 2;
    let total = size_of::<DROPFILES>() + payload;
    let hg = GlobalAlloc(GHND, total)?;
    let ptr = GlobalLock(hg);
    if ptr.is_null() {
        let _ = GlobalFree(Some(hg));
        return Err(Error::from_hresult(E_UNEXPECTED));
    }
    let df = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: BOOL(0),
        fWide: BOOL(1), // 宽字符路径
    };
    std::ptr::copy_nonoverlapping(
        &df as *const DROPFILES as *const u8,
        ptr as *mut u8,
        size_of::<DROPFILES>(),
    );
    std::ptr::copy_nonoverlapping(
        buf.as_ptr() as *const u8,
        (ptr as *mut u8).add(size_of::<DROPFILES>()),
        payload,
    );
    let _ = GlobalUnlock(hg);
    Ok(hg)
}

/// 文件路径格式:CF_HDROP / DVASPECT_CONTENT / TYMED_HGLOBAL / lindex=-1。
/// GetData/QueryGetData/EnumFormatEtc 三处必须用同一格式描述,否则目标会格式不匹配而拒绝。
fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn performed_drop_effect_format() -> FORMATETC {
    FORMATETC {
        cfFormat: unsafe { RegisterClipboardFormatW(CFSTR_PERFORMEDDROPEFFECT) as u16 },
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn format_at(direction: u32, pos: u32) -> Option<FORMATETC> {
    match (direction, pos) {
        (1, 0) => Some(hdrop_format()),
        (2, 0) => Some(performed_drop_effect_format()),
        _ => None,
    }
}

fn format_count(direction: u32) -> u32 {
    match direction {
        1 => 1,
        2 => 1,
        _ => 0,
    }
}

fn is_supported_get_format(fmt: &FORMATETC) -> bool {
    if fmt.dwAspect != DVASPECT_CONTENT.0 as u32
        || fmt.lindex != -1
        || fmt.tymed & TYMED_HGLOBAL.0 as u32 == 0
    {
        return false;
    }
    fmt.cfFormat == CF_HDROP.0
}

fn is_supported_set_format(fmt: &FORMATETC) -> bool {
    fmt.dwAspect == DVASPECT_CONTENT.0 as u32
        && fmt.lindex == -1
        && fmt.tymed & TYMED_HGLOBAL.0 as u32 != 0
        && fmt.cfFormat == performed_drop_effect_format().cfFormat
}

/// 可读格式枚举：CF_HDROP 文件路径 + Preferred DropEffect 首选移动。
/// Explorer 等目标在落点协商前会调用 EnumFormatEtc
/// 枚举数据源支持的格式;若返回 E_NOTIMPL,部分目标直接判定"无可用格式"→ 禁止光标、拒绝落下。
#[implement(IEnumFORMATETC)]
pub struct FileFormatEnum {
    /// 枚举游标(0..=1);Cell 以支持 &self 上推进游标
    pos: Cell<u32>,
    /// DATADIR_GET=1 或 DATADIR_SET=2。
    direction: u32,
}

impl IEnumFORMATETC_Impl for FileFormatEnum_Impl {
    fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
        unsafe {
            if rgelt.is_null() {
                return E_INVALIDARG;
            }
            let mut n = 0u32;
            while n < celt {
                let Some(fmt) = format_at(self.direction, self.pos.get()) else {
                    break;
                };
                *rgelt.add(n as usize) = fmt;
                self.pos.set(self.pos.get() + 1);
                n += 1;
            }
            if !pceltfetched.is_null() {
                *pceltfetched = n;
            }
            // 返回的少于请求的 → S_FALSE(枚举结束)
            if n == celt {
                S_OK
            } else {
                S_FALSE
            }
        }
    }

    fn Skip(&self, celt: u32) -> Result<()> {
        let remain = format_count(self.direction).saturating_sub(self.pos.get());
        let skipped = remain.min(celt);
        self.pos.set(self.pos.get() + skipped);
        if skipped == celt {
            Ok(())
        } else {
            // 跳过的比请求的少 → S_FALSE
            Err(Error::from_hresult(S_FALSE))
        }
    }

    fn Reset(&self) -> Result<()> {
        self.pos.set(0);
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumFORMATETC> {
        let e: IEnumFORMATETC = FileFormatEnum {
            pos: Cell::new(self.pos.get()),
            direction: self.direction,
        }
        .into();
        Ok(e)
    }
}

impl IDataObject_Impl for FileDataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> Result<STGMEDIUM> {
        unsafe {
            if pformatetcin.is_null() {
                return Err(Error::from_hresult(DATA_E_FORMATETC));
            }
            let fmt = *pformatetcin;
            if !is_supported_get_format(&fmt) {
                return Err(Error::from_hresult(DATA_E_FORMATETC));
            }
            let hg = build_hdrop(&self.paths)?;
            let mut medium = STGMEDIUM::default();
            medium.tymed = TYMED_HGLOBAL.0 as u32;
            medium.u.hGlobal = hg;
            Ok(medium)
        }
    }

    fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> Result<()> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        unsafe {
            if pformatetc.is_null() {
                return DATA_E_FORMATETC;
            }
            let fmt = *pformatetc;
            if is_supported_get_format(&fmt) {
                S_OK
            } else {
                DATA_E_FORMATETC
            }
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatetcin: *const FORMATETC,
        _pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        E_NOTIMPL
    }

    fn SetData(
        &self,
        pformatetc: *const FORMATETC,
        pmedium: *const STGMEDIUM,
        frelease: BOOL,
    ) -> Result<()> {
        unsafe {
            if pformatetc.is_null() || pmedium.is_null() {
                return Err(Error::from_hresult(E_INVALIDARG));
            }
            let fmt = *pformatetc;
            let medium = &*pmedium;
            if !is_supported_set_format(&fmt)
                || medium.tymed & TYMED_HGLOBAL.0 as u32 == 0
                || medium.u.hGlobal.is_invalid()
            {
                return Err(Error::from_hresult(DATA_E_FORMATETC));
            }
            let ptr = GlobalLock(medium.u.hGlobal);
            if ptr.is_null() {
                return Err(Error::from_hresult(E_UNEXPECTED));
            }
            let effect = std::ptr::read_unaligned(ptr as *const u32);
            let _ = GlobalUnlock(medium.u.hGlobal);
            self.performed_effect.store(effect, Ordering::Relaxed);
            if frelease.as_bool() {
                ReleaseStgMedium(pmedium as *mut STGMEDIUM);
            }
            Ok(())
        }
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> Result<IEnumFORMATETC> {
        // DATADIR_GET = 1:文件路径;
        // DATADIR_SET = 2:Explorer 回写真正执行的 COPY/MOVE。
        if format_count(dwdirection) == 0 {
            return Err(Error::from_hresult(E_NOTIMPL));
        }
        let e: IEnumFORMATETC = FileFormatEnum {
            pos: Cell::new(0),
            direction: dwdirection,
        }
        .into();
        Ok(e)
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Ref<IAdviseSink>,
    ) -> Result<u32> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn DUnadvise(&self, _dwconnection: u32) -> Result<()> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(Error::from_hresult(E_NOTIMPL))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_file_paths_and_performed_effect_feedback() {
        let paths = hdrop_format();

        assert!(is_supported_get_format(&paths));
        assert!(format_at(1, 0).is_some());
        assert!(format_at(1, 1).is_none());

        let performed = performed_drop_effect_format();
        assert!(is_supported_set_format(&performed));
        assert!(format_at(2, 0).is_some());
        assert!(format_at(2, 1).is_none());
    }

    #[test]
    fn outgoing_get_formats_leave_effect_choice_to_drop_target() {
        assert_eq!(format_count(1), 1);
        assert_eq!(format_at(1, 0).unwrap().cfFormat, CF_HDROP.0);
        assert!(format_at(1, 1).is_none());
    }

    #[test]
    fn explorer_performed_copy_overrides_the_ole_fallback() {
        assert_eq!(
            final_drop_effect(DROPEFFECT_MOVE, DROPEFFECT_COPY.0),
            DROPEFFECT_COPY
        );
        assert_eq!(
            final_drop_effect(DROPEFFECT_MOVE, DROPEFFECT_NONE.0),
            DROPEFFECT_MOVE
        );
    }
}

/// 拖拽过程控制:Esc 取消;松开左键 → 落下;GiveFeedback 用 OLE 默认光标。
#[implement(IDropSource)]
pub struct FileDropSource;

impl IDropSource_Impl for FileDropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() {
            DRAGDROP_S_CANCEL
        } else if !grfkeystate.contains(MK_LBUTTON) {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}
