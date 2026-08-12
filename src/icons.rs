// 图标缓存:优先从系统图像列表(SHGetImageList)抽取 32bpp alpha 图标,
// 根治旧版 SHGetFileInfoW(SHGFI_ICON) 对 1-bit 掩码图标渲染出的透明/色块问题。
// 渲染直接用 GDI DrawIconEx 画 HICON(而非 GDI+ 位图):GdipCreateBitmapFromHICON
// 会把图标透明区转成不透明黑块,DrawIconEx 走系统原生掩码/alpha 处理,透明区正确。
use std::collections::{HashMap, VecDeque};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use windows::Win32::UI::Controls::IImageList;
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON,
    SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHIL_EXTRALARGE, SHIL_LARGE,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::core::PCWSTR;

use crate::utils::wstr;

const CACHE_CAP: usize = 512;

unsafe impl Send for IconCache {}

pub struct IconCache {
    map: HashMap<PathBuf, HICON>,
    lru: VecDeque<PathBuf>,
}

/// 从系统图像列表按索引拿图标:EXTRALARGE(48)→LARGE(32) 依次尝试。
/// 不要用 SHIL_JUMBO(256):实测 DrawIconEx 把 256 图标缩到任意更小尺寸时,
/// 某些图标只画出左上 1/3~1/4(如 Neat Download Manager 只剩 48×48),
/// 而 EXTRALARGE/LARGE 在 48~192 双向缩放全部正常。失败返回无效句柄。
fn syslist_icon(index: i32) -> HICON {
    let tries: [(i32, &str); 2] = [
        (SHIL_EXTRALARGE as i32, "extra"),
        (SHIL_LARGE as i32, "large"),
    ];
    for (id, _name) in tries {
        if let Ok(il) = unsafe { SHGetImageList::<IImageList>(id) } {
            if let Ok(h) = unsafe { il.GetIcon(index, 0) } {
                if !h.is_invalid() {
                    return h;
                }
            }
        }
    }
    HICON::default()
}

/// SHGetFileInfoW 抽取(旧路径,仅作兜底:图标源/图像列表都失败时)
fn extract_icon(wpath: &[u16], exists: bool) -> HICON {
    let mut flags = SHGFI_ICON | SHGFI_LARGEICON;
    if !exists {
        flags |= SHGFI_USEFILEATTRIBUTES;
    }
    let mut info = SHFILEINFOW::default();
    let r = unsafe {
        SHGetFileInfoW(
            PCWSTR(wpath.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut info as *mut _),
            size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };
    if r != 0 && !info.hIcon.is_invalid() {
        return info.hIcon;
    }
    // 兜底:按扩展名拿通用图标
    let mut info2 = SHFILEINFOW::default();
    let fallback = unsafe {
        SHGetFileInfoW(
            PCWSTR(wpath.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut info2 as *mut _),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
        )
    };
    if fallback != 0 && !info2.hIcon.is_invalid() {
        info2.hIcon
    } else {
        HICON::default()
    }
}

impl IconCache {
    pub fn new() -> Self {
        IconCache {
            map: HashMap::new(),
            lru: VecDeque::new(),
        }
    }

    pub fn get(&mut self, path: &Path) -> HICON {
        if let Some(&h) = self.map.get(path) {
            return h;
        }
        let exists = path.exists();
        let wpath = wstr(&path.to_string_lossy());
        // 第一步:SHGFI_SYSICONINDEX 拿系统图像列表索引(带 32bpp alpha)
        let mut flags = SHGFI_SYSICONINDEX;
        if !exists {
            flags |= SHGFI_USEFILEATTRIBUTES;
        }
        let mut info = SHFILEINFOW::default();
        let r = unsafe {
            SHGetFileInfoW(
                PCWSTR(wpath.as_ptr()),
                FILE_ATTRIBUTE_NORMAL,
                Some(&mut info as *mut _),
                size_of::<SHFILEINFOW>() as u32,
                flags,
            )
        };
        let hicon = if r != 0 && info.iIcon >= 0 {
            syslist_icon(info.iIcon)
        } else {
            HICON::default()
        };
        let hicon = if hicon.is_invalid() {
            extract_icon(&wpath, exists)
        } else {
            hicon
        };
        if hicon.is_invalid() {
            return hicon;
        }
        self.map.insert(path.to_path_buf(), hicon);
        self.lru.push_back(path.to_path_buf());
        while self.lru.len() > CACHE_CAP {
            if let Some(old) = self.lru.pop_front() {
                if let Some(h) = self.map.remove(&old) {
                    let _ = unsafe { DestroyIcon(h) };
                }
            }
        }
        hicon
    }

    pub fn clear(&mut self) {
        for (_, h) in self.map.drain() {
            let _ = unsafe { DestroyIcon(h) };
        }
        self.lru.clear();
    }
}

impl Drop for IconCache {
    fn drop(&mut self) {
        self.clear();
    }
}

impl Default for IconCache {
    fn default() -> Self {
        Self::new()
    }
}
