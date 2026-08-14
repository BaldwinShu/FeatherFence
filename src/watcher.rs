// 目录监听:ReadDirectoryChangesW,文件夹门户实时刷新 + 桌面自动归类
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED,
    FILE_ACTION_RENAMED_NEW_NAME, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_INFORMATION,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::utils::wstr;

pub struct DirWatcher {
    thread: Option<JoinHandle<()>>,
    /// 停止信号:置位后线程尽快退出
    stop: Arc<AtomicBool>,
    /// 线程当前持有的目录句柄(裸指针以 usize 存储,避免 Send 问题);
    /// stop() 关闭它以唤醒阻塞中的 ReadDirectoryChangesW
    handle: Arc<Mutex<Option<usize>>>,
}

impl DirWatcher {
    /// 请求停止:置位信号并关闭目录句柄(阻塞读立即返回错误,线程随之退出)。
    /// 句柄由"谁从共享槽取走谁关闭"保证不重复关闭。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.lock().unwrap().take() {
            unsafe { let _ = CloseHandle(HANDLE(h as *mut c_void)); }
        }
    }
}

impl Drop for DirWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn spawn_dir_watcher<F>(dir: PathBuf, notify: F) -> DirWatcher
where
    F: Fn(Vec<String>) + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let handle: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
    let t_stop = stop.clone();
    let t_handle = handle.clone();
    let thread = std::thread::spawn(move || {
        let mut local: Option<HANDLE> = None;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            // 停止检查:退出前把句柄从共享槽取走关闭(若 stop() 已取走则无需再关)
            if t_stop.load(Ordering::SeqCst) {
                if let Some(h) = local.take() {
                    let mut sh = t_handle.lock().unwrap();
                    if *sh == Some(h.0 as usize) {
                        *sh = None;
                        unsafe { let _ = CloseHandle(h); }
                    }
                }
                break;
            }
            if local.is_none() {
                let wdir = wstr(&dir.to_string_lossy());
                local = unsafe {
                    CreateFileW(
                        PCWSTR(wdir.as_ptr()),
                        FILE_LIST_DIRECTORY.0,
                        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                        None,
                        OPEN_EXISTING,
                        FILE_FLAG_BACKUP_SEMANTICS,
                        None,
                    )
                    .ok()
                };
                match local {
                    Some(h) => *t_handle.lock().unwrap() = Some(h.0 as usize),
                    None => {
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        continue;
                    }
                }
                // 重新进入循环顶部:补查 stop,避免"打开句柄后 stop() 才置位"时
                // 线程带着无人关闭的新句柄阻塞在读上
                continue;
            }
            let h = local.unwrap();
            let mut returned: u32 = 0;
            let ok = unsafe {
                ReadDirectoryChangesW(
                    h,
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as u32,
                    false,
                    FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME,
                    Some(&mut returned),
                    None,
                    None,
                )
            };
            if ok.is_err() || returned == 0 {
                // 目录失效,或 stop() 已关闭句柄:归还共享槽并关闭(取走者负责关闭)
                let mut sh = t_handle.lock().unwrap();
                if *sh == Some(h.0 as usize) {
                    *sh = None;
                    unsafe { let _ = CloseHandle(h); }
                }
                local = None;
                std::thread::sleep(std::time::Duration::from_secs(3));
                continue;
            }
            let mut names = Vec::new();
            let mut off = 0usize;
            loop {
                if off + 16 > returned as usize {
                    break;
                }
                let fni = unsafe { &*(buf.as_ptr().add(off) as *const FILE_NOTIFY_INFORMATION) };
                let name_len = fni.FileNameLength as usize;
                if off + 16 + name_len > returned as usize {
                    break;
                }
                let action = fni.Action;
                if action == FILE_ACTION_ADDED || action == FILE_ACTION_MODIFIED || action == FILE_ACTION_RENAMED_NEW_NAME {
                    let name_u16 = unsafe { std::slice::from_raw_parts(fni.FileName.as_ptr(), name_len / 2) };
                    names.push(String::from_utf16_lossy(name_u16));
                }
                if fni.NextEntryOffset == 0 {
                    break;
                }
                off += fni.NextEntryOffset as usize;
            }
            if !names.is_empty() {
                notify(names);
            }
        }
    });
    DirWatcher {
        thread: Some(thread),
        stop,
        handle,
    }
}

/// 移动文件到目标目录(同卷 rename,跨卷 copy+delete),自动避免重名
pub fn move_to_dir(src: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    if !dest_dir.exists() {
        std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
    }
    let name = src.file_name().ok_or("no file name")?.to_os_string();
    let dest = unique_dest(dest_dir, &name);
    match std::fs::rename(src, &dest) {
        Ok(()) => Ok(dest),
        Err(e) => {
            // 跨卷或失败 → copy + delete
            match std::fs::copy(src, &dest) {
                Ok(_) => {
                    let _ = std::fs::remove_file(src);
                    Ok(dest)
                }
                Err(e2) => Err(format!("rename: {e}; copy: {e2}")),
            }
        }
    }
}

/// 目标已存在则加 "(1)"/"(2)" 后缀
pub fn unique_dest(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let cand = dir.join(name);
    if !cand.exists() {
        return cand;
    }
    let stem = Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let ext = Path::new(name)
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    for i in 1..1000 {
        let c = dir.join(format!("{stem} ({i}){ext}"));
        if !c.exists() {
            return c;
        }
    }
    dir.join(format!("{stem} ({}){ext}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)))
}
