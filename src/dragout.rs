// 拖出:把栅栏里的文件/文件夹拖到桌面、资源管理器等目标(移动或复制)。
// 用 OLE DoDragDrop:自定义 IDataObject 提供 CF_HDROP(文件路径列表),
// 自定义 IDropSource 管理拖拽过程(松左键落下 / Esc 取消)。
// 目标端(桌面/文件夹窗口/其他栅栏)负责实际移动文件;拖回本栅栏由现有
// drop target 接住(文件已在自身目录则跳过 → 无操作)。
use std::cell::Cell;
use std::mem::size_of;

use windows::core::{implement, BOOL, Error, HRESULT, Ref, Result};
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, E_INVALIDARG, E_NOTIMPL,
    E_UNEXPECTED, GlobalFree, HGLOBAL, POINT, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{
    DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC,
    IEnumFORMATETC_Impl, IEnumSTATDATA, STGMEDIUM, TYMED_HGLOBAL,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows::Win32::System::Ole::{
    CF_HDROP, DoDragDrop, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE,
    IDropSource, IDropSource_Impl,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::DROPFILES;

/// DATA_E_FORMATETC:请求的格式不是 CF_HDROP
const DATA_E_FORMATETC: HRESULT = HRESULT(0x80040064_u32 as _);

/// 拖出指定路径:阻塞到拖拽结束。返回实际拖放效果(MOVE/COPY/NONE)。
pub fn start_drag(paths: Vec<String>) -> DROPEFFECT {
    crate::dlog(&format!("[dragout] start drag: {}", paths.join("; ")));
    unsafe {
        let dataobj: IDataObject = FileDataObject { paths }.into();
        let src: IDropSource = FileDropSource.into();
        let mut effect = DROPEFFECT_NONE;
        let hr = DoDragDrop(&dataobj, &src, DROPEFFECT_COPY | DROPEFFECT_MOVE, &mut effect);
        // 诊断:hr 里能看到 E_UNEXPECTED/CO_E_* 等失败原因;effect 为 NONE 表示目标拒绝。
        crate::dlog(&format!(
            "[dragout] DoDragDrop hr=0x{:08x} effect={}",
            hr.0 as u32, effect.0
        ));
        effect
    }
}

/// 拖拽数据源:持有拖出文件列表,GetData 按需构造 CF_HDROP。
/// 其余 IDataObject 方法返回 E_NOTIMPL(拖出文件到资源管理器只需要 GetData)。
#[implement(IDataObject)]
pub struct FileDataObject {
    /// 拖出的绝对路径
    pub paths: Vec<String>,
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

/// 数据源支持的唯一格式:CF_HDROP / DVASPECT_CONTENT / TYMED_HGLOBAL / lindex=-1。
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

/// CF_HDROP 格式枚举(单元素)。Explorer 等目标在落点协商前会调用 EnumFormatEtc
/// 枚举数据源支持的格式;若返回 E_NOTIMPL,部分目标直接判定"无可用格式"→ 禁止光标、拒绝落下。
#[implement(IEnumFORMATETC)]
pub struct FileFormatEnum {
    /// 枚举游标(0..=1);Cell 以支持 &self 上推进游标
    pos: Cell<u32>,
}

impl IEnumFORMATETC_Impl for FileFormatEnum_Impl {
    fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
        unsafe {
            if rgelt.is_null() {
                return E_INVALIDARG;
            }
            let mut n = 0u32;
            while n < celt && self.pos.get() < 1 {
                *rgelt.add(n as usize) = hdrop_format();
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
        let remain = 1u32.saturating_sub(self.pos.get());
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
        let e: IEnumFORMATETC = FileFormatEnum { pos: Cell::new(self.pos.get()) }.into();
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
            if fmt.cfFormat != CF_HDROP.0
                || fmt.dwAspect != DVASPECT_CONTENT.0 as u32
                || fmt.lindex != -1
            {
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
            if fmt.cfFormat == CF_HDROP.0
                && fmt.dwAspect == DVASPECT_CONTENT.0 as u32
                && fmt.lindex == -1
            {
                S_OK
            } else {
                DATA_E_FORMATETC
            }
        }
    }

    fn GetCanonicalFormatEtc(&self, _pformatetcin: *const FORMATETC, _pformatetcout: *mut FORMATETC) -> HRESULT {
        E_NOTIMPL
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: BOOL,
    ) -> Result<()> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> Result<IEnumFORMATETC> {
        // DATADIR_GET = 1(读数据):返回 CF_HDROP 单格式枚举;
        // DATADIR_SET = 2(写数据):只读数据源不支持,返回 E_NOTIMPL(标准行为)。
        if dwdirection != 1 {
            return Err(Error::from_hresult(E_NOTIMPL));
        }
        let e: IEnumFORMATETC = FileFormatEnum { pos: Cell::new(0) }.into();
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
