//! 缩略图:后台线程用 shell 的 IShellItemImageFactory 取图,主线程永不阻塞。
//!
//! 设计要点(均由 examples/thumb_probe.rs 实测得出):
//! - MTA 工作线程 + 无消息泵即可调 GetImage(不需要 STA,也不需要 IThumbnailCache);
//! - 只用 `SIIGBF_THUMBNAILONLY`:失败(0x8004B200)即"没有缩略图",回退系统图标;
//!   用 flags=0 会拿回一个图标(慢且重复),不要用;
//! - shell 返回**直通 alpha**,且**保持宽高比**(96×60 之类),所以工作线程里居中到
//!   正方形画布并预乘一次,渲染端只做一次便宜的 source-over;
//! - 冷解码 30~125ms(6000×4000 实测只涨 7MB 工作集),命中 shell 缓存仍要 2~27ms,
//!   所以必须有自己的内存缓存。
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW,
    HGDIOBJ, ReleaseDC,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_THUMBNAILONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// 缩略图就绪通知(工作线程 → 消息窗口)。+7 未被占用(+1/+2/+5/+6/+20/+0x51 已用)。
pub const WM_APP_THUMB_READY: u32 = WM_APP + 7;

/// 内存预算:预乘后的正方形画布,32px 一张 4KB、64px 一张 16KB(随图标尺寸)。
const DEFAULT_BUDGET_BYTES: usize = 24 * 1024 * 1024;
/// 工作线程数。解码是 shell 侧的重活,2 条足够且不会和 UI 抢得太凶。
const WORKERS: usize = 2;

static NOTIFY_HWND: AtomicIsize = AtomicIsize::new(0);

/// 工作线程解码完成后通知 UI 线程。窗口句柄在启动时存成原子值,
/// 工作线程绝不能碰 `with_global`(深度计数是线程局部的,会绕过锁)。
pub fn set_notify_hwnd(h: HWND) {
    NOTIFY_HWND.store(h.0 as isize, Ordering::Relaxed);
}

fn notify_ui() {
    let raw = NOTIFY_HWND.load(Ordering::Relaxed);
    if raw != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(raw as *mut _)), WM_APP_THUMB_READY, WPARAM(0), LPARAM(0));
        }
    }
}

/// 缓存键:路径 + 目标像素。
///
/// **故意不含 mtime/大小**:那需要在 UI 线程每帧 stat 每个可见文件,一旦栅栏绑到
/// 网络盘或掉线的共享目录,卡的就是主线程。失效交给目录监听(opendir 变化 → 刷新
/// → `forget_dir`),和条目重建用的是同一套信号。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ThumbKey {
    path: PathBuf,
    px: i32,
}

/// 预乘 alpha 的 BGRA 正方形画布,原图按比例居中,四周留透明。
pub struct Thumb {
    pub px: i32,
    pub data: Vec<u32>,
}

impl Thumb {
    fn bytes(&self) -> usize {
        self.data.len() * 4
    }
}

struct Job {
    key: ThumbKey,
    px: i32,
}

pub struct ThumbStore {
    map: HashMap<ThumbKey, Arc<Thumb>>,
    order: VecDeque<ThumbKey>,
    bytes: usize,
    budget: usize,
    job_tx: Option<mpsc::Sender<Job>>,
    res_rx: Option<mpsc::Receiver<(ThumbKey, Option<Thumb>)>>,
    in_flight: HashSet<ThumbKey>,
    /// 取不到缩略图的键(无预览/损坏/未知容器)。记住失败,避免每帧重复排队的无谓开销。
    failed: HashSet<ThumbKey>,
}

impl Default for ThumbStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ThumbStore {
    pub fn new() -> Self {
        ThumbStore {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            budget: DEFAULT_BUDGET_BYTES,
            job_tx: None,
            res_rx: None,
            in_flight: HashSet::new(),
            failed: HashSet::new(),
        }
    }

    /// 查缓存;未命中则排队让工作线程去取。返回 None = 本帧先用系统图标顶着。
    /// 这里**不做任何文件系统调用**,只碰内存(见 `ThumbKey` 的说明)。
    pub fn get_or_enqueue(&mut self, path: &Path, px: i32) -> Option<Arc<Thumb>> {
        if px <= 0 || !crate::fence::thumb_mode() || !is_thumbable(path) {
            return None;
        }
        let key = ThumbKey {
            path: path.to_path_buf(),
            px,
        };
        if let Some(t) = self.map.get(&key) {
            return Some(t.clone());
        }
        if self.failed.contains(&key) || self.in_flight.contains(&key) {
            return None;
        }
        self.ensure_workers();
        if let Some(tx) = &self.job_tx {
            if tx.send(Job { key: key.clone(), px }).is_ok() {
                self.in_flight.insert(key);
            }
        }
        None
    }

    /// 收取工作线程的结果;返回是否有新图可用(有则调用方重绘)。
    pub fn drain(&mut self) -> bool {
        let Some(rx) = &self.res_rx else {
            return false;
        };
        let mut changed = false;
        let mut received = 0usize;
        while let Ok((key, res)) = rx.try_recv() {
            self.in_flight.remove(&key);
            changed = true;
            received += 1;
            match res {
                Some(t) => {
                    let sz = t.bytes();
                    let is_new = self.map.insert(key.clone(), Arc::new(t)).is_none();
                    if is_new {
                        self.order.push_back(key);
                        self.bytes += sz;
                    }
                }
                None => {
                    self.failed.insert(key);
                }
            }
        }
        while self.bytes > self.budget {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(t) = self.map.remove(&old) {
                self.bytes = self.bytes.saturating_sub(t.bytes());
            }
        }
        if received > 0 {
            crate::dlog(&format!(
                "[thumb] 收到 {received} 张,缓存 {} 张 {:.1}MB",
                self.map.len(),
                self.bytes as f64 / 1048576.0
            ));
        }
        changed
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
        self.failed.clear();
    }

    /// 目录内容变化(监听器通知)时丢掉该目录下的缓存与失败记录:
    /// 文件被替换/改名后能重新取图。只算内存,不做 IO。
    pub fn forget_dir(&mut self, dir: &Path) {
        let before = self.map.len();
        self.map.retain(|k, _| !k.path.starts_with(dir));
        self.failed.retain(|k| !k.path.starts_with(dir));
        if self.map.len() != before {
            self.bytes = self.map.values().map(|t| t.bytes()).sum();
        }
    }

    fn ensure_workers(&mut self) {
        if self.job_tx.is_some() {
            return;
        }
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (res_tx, res_rx) = mpsc::channel::<(ThumbKey, Option<Thumb>)>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        for i in 0..WORKERS {
            let rx = job_rx.clone();
            let tx = res_tx.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("thumb-{i}"))
                .spawn(move || worker_loop(rx, tx));
            if let Err(e) = spawned {
                crate::dlog(&format!("[thumb] 工作线程启动失败: {e}"));
            }
        }
        self.job_tx = Some(job_tx);
        self.res_rx = Some(res_rx);
    }

    #[cfg(test)]
    fn insert_for_test(&mut self, key: ThumbKey, t: Thumb) {
        let sz = t.bytes();
        if self.map.insert(key.clone(), Arc::new(t)).is_none() {
            self.order.push_back(key);
            self.bytes += sz;
        }
        while self.bytes > self.budget {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(t) = self.map.remove(&old) {
                self.bytes = self.bytes.saturating_sub(t.bytes());
            }
        }
    }

    #[cfg(test)]
    fn key_for_test(path: &str, px: i32) -> ThumbKey {
        ThumbKey {
            path: PathBuf::from(path),
            px,
        }
    }
}

fn worker_loop(rx: Arc<Mutex<mpsc::Receiver<Job>>>, tx: mpsc::Sender<(ThumbKey, Option<Thumb>)>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    loop {
        // 锁只在 recv 期间持有:取到任务立刻放锁,两个线程仍能并行解码。
        let job = {
            let guard = match rx.lock() {
                Ok(g) => g,
                Err(_) => break,
            };
            guard.recv()
        };
        let Ok(job) = job else {
            break;
        };
        let thumb = decode(&job.key.path, job.px);
        if tx.send((job.key, thumb)).is_err() {
            break;
        }
        notify_ui();
    }
    unsafe {
        CoUninitialize();
    }
}

/// 扩展名白名单。目录、文档一律走系统图标(它们没有比图标更有用的预览);
/// `.lnk` 在 worker 里解开,指向媒体才取图(收纳箱里绝大多数条目是快捷方式)。
pub fn is_thumbable(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "lnk"
            | "jpg"
            | "jpeg"
            | "png"
            | "bmp"
            | "gif"
            | "webp"
            | "tif"
            | "tiff"
            | "heic"
            | "avif"
            | "mp4"
            | "mkv"
            | "avi"
            | "mov"
            | "webm"
            | "m4v"
            | "wmv"
            | "flv"
            | "ts"
            | "m2ts"
    )
}

fn is_lnk(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
}

/// 解开快捷方式的目标路径(worker 线程里调用,不碰 UI)。
fn resolve_lnk(path: &Path) -> Option<PathBuf> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile, STGM_READ};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::Interface;

    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let file: IPersistFile = link.cast().ok()?;
        let target = windows::core::HSTRING::from(path.to_string_lossy().as_ref());
        file.Load(&target, STGM_READ).ok()?;
        let mut buf = [0u16; 1024];
        link.GetPath(&mut buf, std::ptr::null_mut(), 0).ok()?;
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        if end == 0 {
            return None;
        }
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..end])))
    }
}

fn decode(path: &Path, px: i32) -> Option<Thumb> {
    // 快捷方式:解开目标;目标不是媒体就直接放弃(调用方回退图标),别为它再跑一趟 shell。
    let source: PathBuf = if is_lnk(path) {
        let target = resolve_lnk(path)?;
        if !is_thumbable(&target) || is_lnk(&target) {
            return None;
        }
        target
    } else {
        path.to_path_buf()
    };
    let hstr = windows::core::HSTRING::from(source.to_string_lossy().as_ref());
    let factory: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(&hstr, None) }.ok()?;
    let hbm = match unsafe { factory.GetImage(SIZE { cx: px, cy: px }, SIIGBF_THUMBNAILONLY) } {
        Ok(h) => h,
        Err(e) => {
            crate::dlog(&format!("[thumb] 无缩略图({e}): {}", source.display()));
            return None;
        }
    };
    let read = read_bgra(hbm);
    unsafe {
        let _ = DeleteObject(HGDIOBJ(hbm.0));
    }
    let (w, h, src) = read?;
    Some(center_and_premultiply(&src, w, h, px))
}

/// 取回 32bpp 顶向下的 BGRA(直通 alpha)。shell 缩略图不是正方形,宽高原样保留。
fn read_bgra(hbm: windows::Win32::Graphics::Gdi::HBITMAP) -> Option<(i32, i32, Vec<u8>)> {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(
            HGDIOBJ(hbm.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut BITMAP as *mut _),
        ) == 0
        {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        if w <= 0 || h <= 0 {
            return None;
        }
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = 0; // BI_RGB
        let mut data = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(Some(HWND::default()));
        let got = GetDIBits(
            dc,
            hbm,
            0,
            h as u32,
            Some(data.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(Some(HWND::default()), dc);
        if got == 0 {
            return None;
        }
        Some((w, h, data))
    }
}

/// 居中到 px×px 画布并预乘:源是直通 alpha,渲染端要预乘才能直接 source-over。
fn center_and_premultiply(src: &[u8], w: i32, h: i32, px: i32) -> Thumb {
    let (sw, sh) = (w, h);
    let w = sw.min(px).max(0);
    let h = sh.min(px).max(0);
    let ox = (px - w) / 2;
    let oy = (px - h) / 2;
    let mut data = vec![0u32; (px * px) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * sw + x) * 4) as usize;
            let (b, g, r, a) = (
                src[i] as u32,
                src[i + 1] as u32,
                src[i + 2] as u32,
                src[i + 3] as u32,
            );
            if a == 0 {
                continue;
            }
            let (b, g, r) = (b * a / 255, g * a / 255, r * a / 255);
            data[((oy + y) * px + ox + x) as usize] = (a << 24) | (r << 16) | (g << 8) | b;
        }
    }
    Thumb { px, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(b: u8, g: u8, r: u8, a: u8) -> u32 {
        ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32
    }

    #[test]
    fn premultiplies_and_centers_inside_square_canvas() {
        // 2×1 源图放进 4×4 画布:水平居中(ox=1),垂直居中(oy=1)
        let src = vec![
            200u8, 100, 50, 255, // 不透明
            200, 100, 50, 128, // 半透明 → 预乘约减半
        ];
        let t = center_and_premultiply(&src, 2, 1, 4);
        assert_eq!(t.px, 4);
        assert_eq!(t.data.len(), 16);
        assert_eq!(t.data[1 * 4 + 1], px(200, 100, 50, 255));
        let half = t.data[1 * 4 + 2];
        let a = (half >> 24) & 0xFF;
        assert_eq!(a, 128);
        assert_eq!((half >> 16) & 0xFF, 50 * 128 / 255);
        assert_eq!((half >> 8) & 0xFF, 100 * 128 / 255);
        assert_eq!(half & 0xFF, 200 * 128 / 255);
        // 画布四角保持透明
        assert_eq!(t.data[0], 0);
        assert_eq!(t.data[15], 0);
    }

    #[test]
    fn whitelist_includes_shortcuts_but_skips_documents_and_folders() {
        assert!(is_thumbable(Path::new("C:/a/photo.JPG")));
        assert!(is_thumbable(Path::new("C:/a/clip.mp4")));
        // 快捷方式要在 worker 里解开,指向媒体才有图;目标是应用时回退图标
        assert!(is_thumbable(Path::new("C:/a/shortcut.lnk")));
        assert!(!is_thumbable(Path::new("C:/a/readme.txt")));
        assert!(!is_thumbable(Path::new("C:/a/folder")));
    }

    #[test]
    fn key_distinguishes_path_and_target_px() {
        let a = ThumbStore::key_for_test("C:/a.jpg", 96);
        assert_ne!(a, ThumbStore::key_for_test("C:/b.jpg", 96));
        assert_ne!(a, ThumbStore::key_for_test("C:/a.jpg", 144));
        assert_eq!(a, ThumbStore::key_for_test("C:/a.jpg", 96));
    }

    #[test]
    fn forget_dir_drops_only_that_directory() {
        let mut s = ThumbStore::new();
        let t = || Thumb {
            px: 2,
            data: vec![1u32; 4],
        };
        s.insert_for_test(ThumbStore::key_for_test("C:/box/a.jpg", 2), t());
        s.insert_for_test(ThumbStore::key_for_test("C:/box/b.jpg", 2), t());
        s.insert_for_test(ThumbStore::key_for_test("C:/other/c.jpg", 2), t());
        s.forget_dir(Path::new("C:/box"));
        assert_eq!(s.map.len(), 1);
        assert!(s.map.contains_key(&ThumbStore::key_for_test("C:/other/c.jpg", 2)));
        assert_eq!(s.bytes, t().bytes());
    }

    #[test]
    fn budget_evicts_oldest_first() {
        let mut s = ThumbStore::new();
        s.budget = 3 * 4 * 4; // 3 张 2×2 的图(每张 16 字节)
        let mk = |_: u64| Thumb {
            px: 2,
            data: vec![1u32; 4],
        };
        for i in 0..4 {
            let name = format!("C:/x{i}.jpg");
            s.insert_for_test(ThumbStore::key_for_test(&name, 2), mk(i));
        }
        assert_eq!(s.map.len(), 3);
        assert!(s.bytes <= s.budget);
        assert!(!s.map.contains_key(&ThumbStore::key_for_test("C:/x0.jpg", 2)));
        assert!(s.map.contains_key(&ThumbStore::key_for_test("C:/x3.jpg", 2)));
    }
}
