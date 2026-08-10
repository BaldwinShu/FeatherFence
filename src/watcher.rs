// 目录监听:ReadDirectoryChangesW,文件夹门户实时刷新 + 桌面自动归类
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED,
    FILE_ACTION_RENAMED_NEW_NAME, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::utils::wstr;

pub struct DirWatcher {
    pub thread: Option<JoinHandle<()>>,
}

pub fn spawn_dir_watcher<F>(dir: PathBuf, notify: F) -> DirWatcher
where
    F: Fn(Vec<String>) + Send + 'static,
{
    let thread = std::thread::spawn(move || {
        let mut handle: Option<HANDLE> = None;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if handle.is_none() {
                let wdir = wstr(&dir.to_string_lossy());
                handle = unsafe {
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
                if handle.is_none() {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    continue;
                }
            }
            let h = handle.unwrap();
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
                // 目录失效,关掉重来
                unsafe { let _ = windows::Win32::Foundation::CloseHandle(h); }
                handle = None;
                std::thread::sleep(std::time::Duration::from_secs(3));
                continue;
            }
            let names = parse_notify_names(&buf, returned as usize);
            if !names.is_empty() {
                notify(names);
            }
        }
    });
    DirWatcher { thread: Some(thread) }
}

fn parse_notify_names(buf: &[u8], returned: usize) -> Vec<String> {
    // FILE_NOTIFY_INFORMATION 的可变长文件名从第 12 字节开始。按 Rust 结构体
    // 大小(16 字节)检查会把没有尾部填充的最后一条通知误判为不完整。
    const HEADER_LEN: usize = 12;
    let end = returned.min(buf.len());
    let mut names = Vec::new();
    let mut off = 0usize;
    loop {
        if off.checked_add(HEADER_LEN).is_none_or(|n| n > end) {
            break;
        }
        let next = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        let action = u32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap());
        let name_len = u32::from_le_bytes(buf[off + 8..off + 12].try_into().unwrap()) as usize;
        let Some(name_end) = off.checked_add(HEADER_LEN).and_then(|n| n.checked_add(name_len)) else {
            break;
        };
        if name_len % 2 != 0 || name_end > end {
            break;
        }
        if action == FILE_ACTION_ADDED.0
            || action == FILE_ACTION_MODIFIED.0
            || action == FILE_ACTION_RENAMED_NEW_NAME.0
        {
            let name_u16: Vec<u16> = buf[off + HEADER_LEN..name_end]
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect();
            names.push(String::from_utf16_lossy(&name_u16));
        }
        if next == 0 {
            break;
        }
        let Some(new_off) = off.checked_add(next) else { break };
        if new_off <= off || new_off > end {
            break;
        }
        off = new_off;
    }
    names
}

#[cfg(test)]
mod tests {
    use super::parse_notify_names;
    use windows::Win32::Storage::FileSystem::FILE_ACTION_RENAMED_NEW_NAME;

    #[test]
    fn parses_single_unpadded_final_record() {
        let name: Vec<u16> = "Notion-7.29.0.msix".encode_utf16().collect();
        let mut record = Vec::new();
        record.extend_from_slice(&0u32.to_le_bytes());
        record.extend_from_slice(&FILE_ACTION_RENAMED_NEW_NAME.0.to_le_bytes());
        record.extend_from_slice(&((name.len() * 2) as u32).to_le_bytes());
        for ch in name {
            record.extend_from_slice(&ch.to_le_bytes());
        }
        assert_eq!(parse_notify_names(&record, record.len()), ["Notion-7.29.0.msix"]);
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
