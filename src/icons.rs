// 图标缓存:优先从系统图像列表(SHGetImageList)抽取 32bpp alpha 图标,
// 根治旧版 SHGetFileInfoW(SHGFI_ICON) 对 1-bit 掩码图标渲染出的透明/色块问题。
// 渲染直接用 GDI DrawIconEx 画 HICON(而非 GDI+ 位图):GdipCreateBitmapFromHICON
// 会把图标透明区转成不透明黑块,DrawIconEx 走系统原生掩码/alpha 处理,透明区正确。
use std::collections::{HashMap, VecDeque};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows::Win32::UI::Controls::IImageList;
use windows::Win32::UI::Shell::{
    SHChangeNotify, SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHCNE_UPDATEITEM, SHCNF_PATHW,
    SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHIL_EXTRALARGE,
    SHIL_LARGE,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::core::PCWSTR;

use crate::utils::wstr;

const CACHE_CAP: usize = 512;

unsafe impl Send for IconCache {}

/// 文件指纹(修改时间 + 大小),图标缓存失效判断用。
/// 由目录枚举(ReadDir)顺带取出:Windows 上 `DirEntry::metadata` 用的是
/// 枚举结果里的缓存数据,不产生额外 IO,所以每个条目都能免费带上。
/// 刻意不在渲染时 stat —— 栅栏绑到网络盘或掉线共享目录时,卡的是主线程。
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct FileStamp {
    mtime: Option<std::time::SystemTime>,
    len: u64,
}

impl FileStamp {
    pub fn from_metadata(meta: &std::fs::Metadata) -> Self {
        FileStamp {
            mtime: meta.modified().ok(),
            len: meta.len(),
        }
    }

    /// 两边都有修改时间才判断;任一未知就沿用缓存
    /// (宁可图标旧一拍,也不要在每帧里重取)。
    fn matches(self, other: Self) -> bool {
        match (self.mtime, other.mtime) {
            (Some(a), Some(b)) => a == b && self.len == other.len,
            _ => true,
        }
    }
}

struct CachedIcon {
    hicon: HICON,
    stamp: FileStamp,
}

/// 让 shell 重读这些条目的图标(丢它自己的条目缓存)。
/// 文件刚落地/刚被移动时,`SHGetFileInfo` 可能把通用图标登记进本进程的图标
/// 映射;之后重取仍是通用图标,只有重启进程才恢复(issue #41)。
/// `SHCNE_UPDATEITEM` 是通知 shell"该条目变了"的标准方式,调用方只需在
/// 新出现条目的延迟复核时用,量很小。
pub fn notify_shell_item_changed(paths: &[PathBuf]) {
    for path in paths {
        let wide = wstr(&path.to_string_lossy());
        unsafe {
            SHChangeNotify(
                SHCNE_UPDATEITEM,
                SHCNF_PATHW,
                Some(wide.as_ptr() as *const std::ffi::c_void),
                None,
            );
        }
    }
}

pub struct IconCache {
    map: HashMap<PathBuf, CachedIcon>,
    lru: VecDeque<PathBuf>,
    perf: IconPerfStats,
    /// 缩略图(视依赖媒体文件时替代图标)。独立存储、独立预算,
    /// 未命中/不支持的扩展名仍走上面的图标缓存。
    thumbs: crate::thumbs::ThumbStore,
}

#[derive(Clone, Copy, Default)]
pub struct IconPerfStats {
    pub hits: u64,
    pub misses: u64,
    pub hit_time: Duration,
    pub miss_time: Duration,
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

/// SHGetFileInfoW 抽取(旧路径,仅作兜底:图标源/图像列表都失败时)。
/// 返回 (图标, 是否可入缓存):按扩展名给出的通用图标可能只是一次瞬态失败的
/// 产物,调用方不能用它长期占住缓存。
fn extract_icon(wpath: &[u16], exists: bool) -> (HICON, bool) {
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
        return (info.hIcon, exists);
    }
    // 兜底:按扩展名拿通用图标(文件不存在或 shell 暂时解析不了时才走到这里)
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
        (info2.hIcon, false)
    } else {
        (HICON::default(), false)
    }
}

impl IconCache {
    pub fn new() -> Self {
        IconCache {
            map: HashMap::new(),
            lru: VecDeque::new(),
            perf: IconPerfStats::default(),
            thumbs: crate::thumbs::ThumbStore::new(),
        }
    }

    /// 缩略图查询:命中直接返回;未命中时排队给后台线程,本帧返回 None(调用方画图标)。
    pub fn thumb(&mut self, path: &Path, px: i32) -> Option<std::sync::Arc<crate::thumbs::Thumb>> {
        self.thumbs.get_or_enqueue(path, px)
    }

    /// 收后台解码结果。返回 true 表示有新缩略图可用,调用方应重绘。
    pub fn drain_thumbs(&mut self) -> bool {
        self.thumbs.drain()
    }

    /// 目录内容变化时作废该目录的缩略图(只动内存,不做 IO)。
    pub fn forget_thumbs_in(&mut self, dir: &Path) {
        self.thumbs.forget_dir(dir);
    }

    /// 作废若干路径的图标缓存(只动内存)。用于新出现条目的延迟复核:
    /// 落地瞬间取到的通用图标,一秒后丢掉重取一次。
    ///
    /// 注意:光清我们自己的 map 不一定够 —— 文件刚落地时 shell 可能已经把
    /// "通用图标"记进了本进程的图标映射,重取 `SHGetFileInfo` 还是旧结果。
    /// 复核前先调 [`notify_shell_item_changed`] 让 shell 也丢掉该条目。
    pub fn forget_paths(&mut self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        for path in paths {
            if let Some(old) = self.map.remove(path) {
                let _ = unsafe { DestroyIcon(old.hicon) };
            }
        }
        self.lru.retain(|p| !paths.contains(p));
    }

    /// 取图标。`stamp` 是目录扫描时顺带记录的指纹:和缓存里的不一致就重取
    /// (文件被替换/同名覆盖);一致或无法比对时直接命中。
    pub fn get(&mut self, path: &Path, stamp: FileStamp) -> HICON {
        let started = crate::perf::enabled().then(Instant::now);
        if let Some(cached) = self.map.get(path) {
            if cached.stamp.matches(stamp) {
                if let Some(started) = started {
                    self.perf.hits += 1;
                    self.perf.hit_time += started.elapsed();
                }
                return cached.hicon;
            }
        }
        // 指纹变了:丢掉旧句柄重取(lru 里那条留着,重取后不再重复入队)
        let replaced = if let Some(old) = self.map.remove(path) {
            let _ = unsafe { DestroyIcon(old.hicon) };
            true
        } else {
            false
        };
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
        // 只有"文件确实存在 + 不是按扩展名的兜底图标"才进缓存:
        // 文件刚落地/正在写入/快捷方式目标暂时解析不了时,shell 会给出一个
        // 有效但通用的图标,缓存住就再也不会更新(issue #41)。
        // 这类结果只用于当前这一帧,下一帧自然重试。
        let (hicon, cacheable) = if r != 0 && info.iIcon >= 0 {
            let h = syslist_icon(info.iIcon);
            if h.is_invalid() {
                extract_icon(&wpath, exists)
            } else {
                (h, exists)
            }
        } else {
            extract_icon(&wpath, exists)
        };
        if let Some(started) = started {
            self.perf.misses += 1;
            self.perf.miss_time += started.elapsed();
        }
        if hicon.is_invalid() || !cacheable {
            self.lru.retain(|p| p != path);
            return hicon;
        }
        if !replaced {
            self.lru.push_back(path.to_path_buf());
        }
        self.map
            .insert(path.to_path_buf(), CachedIcon { hicon, stamp });
        while self.lru.len() > CACHE_CAP {
            if let Some(old) = self.lru.pop_front() {
                if let Some(c) = self.map.remove(&old) {
                    let _ = unsafe { DestroyIcon(c.hicon) };
                }
            }
        }
        hicon
    }

    pub fn take_perf_stats(&mut self) -> IconPerfStats {
        std::mem::take(&mut self.perf)
    }

    pub fn clear(&mut self) {
        for (_, c) in self.map.drain() {
            let _ = unsafe { DestroyIcon(c.hicon) };
        }
        self.lru.clear();
        self.thumbs.clear();
    }
}

impl Drop for IconCache {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn stamp(secs: u64, len: u64) -> FileStamp {
        FileStamp {
            mtime: Some(UNIX_EPOCH + Duration::from_secs(secs)),
            len,
        }
    }

    #[test]
    fn stamp_match_requires_same_mtime_and_size() {
        let a = stamp(10, 100);
        assert!(a.matches(stamp(10, 100)));
        assert!(!a.matches(stamp(11, 100)));
        assert!(!a.matches(stamp(10, 101)));
    }

    #[test]
    fn unknown_stamp_never_invalidates_the_cache() {
        // 取不到元数据时(权限/网络盘瞬断)保守沿用缓存,避免每帧重取。
        let unknown = FileStamp::default();
        assert!(stamp(10, 100).matches(unknown));
        assert!(unknown.matches(stamp(10, 100)));
    }

    fn insert(cache: &mut IconCache, name: &str) {
        let path = PathBuf::from(name);
        cache
            .map
            .insert(path.clone(), CachedIcon { hicon: HICON::default(), stamp: FileStamp::default() });
        cache.lru.push_back(path);
    }

    #[test]
    fn forget_paths_drops_only_listed_entries() {
        let mut cache = IconCache::new();
        insert(&mut cache, "C:/box/a.lnk");
        insert(&mut cache, "C:/box/b.lnk");

        cache.forget_paths(&[PathBuf::from("C:/box/a.lnk")]);

        assert_eq!(cache.map.len(), 1);
        assert!(cache.map.contains_key(&PathBuf::from("C:/box/b.lnk")));
        assert_eq!(cache.lru.len(), 1);
        assert_eq!(cache.lru[0], PathBuf::from("C:/box/b.lnk"));
    }
}

impl Default for IconCache {
    fn default() -> Self {
        Self::new()
    }
}
