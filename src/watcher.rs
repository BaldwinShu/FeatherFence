// 目录监听:ReadDirectoryChangesW,文件夹门户实时刷新 + 桌面自动归类
use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED,
    FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::IO::CancelSynchronousIo;

use crate::utils::wstr;

#[derive(Default)]
struct StopSignal {
    stopped: Mutex<bool>,
    changed: Condvar,
}

impl StopSignal {
    fn is_stopped(&self) -> bool {
        *self
            .stopped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn request_stop(&self) {
        let mut stopped = self
            .stopped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *stopped = true;
        self.changed.notify_all();
    }

    /// 可中断的重试等待。返回 true 表示已请求停止。
    fn wait_timeout(&self, timeout: Duration) -> bool {
        let stopped = self
            .stopped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *stopped {
            return true;
        }
        let (stopped, _) = self
            .changed
            .wait_timeout_while(stopped, timeout, |stopped| !*stopped)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *stopped
    }
}

pub struct DirWatcher {
    stop: Arc<StopSignal>,
    thread: Option<JoinHandle<()>>,
}

impl DirWatcher {
    /// 停止阻塞中的 ReadDirectoryChangesW 并回收监听线程。可重复调用。
    pub fn stop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.stop.request_stop();

        // CancelSynchronousIo 只取消调用瞬间已经挂起的同步 I/O。工作线程可能正好
        // 位于“检查 stop → 发起 ReadDirectoryChangesW”的短窗口内，因此在它退出前
        // 重试取消；线程每次 I/O 返回后也会检查 stop，不会再次进入阻塞读取。
        while !thread.is_finished() {
            unsafe {
                let _ = CancelSynchronousIo(HANDLE(thread.as_raw_handle()));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let _ = thread.join();
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
    let stop = Arc::new(StopSignal::default());
    let thread_stop = stop.clone();
    let thread = std::thread::spawn(move || {
        let mut handle: Option<HANDLE> = None;
        let mut buf = vec![0u8; 64 * 1024];
        while !thread_stop.is_stopped() {
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
                    if thread_stop.wait_timeout(Duration::from_secs(3)) {
                        break;
                    }
                    continue;
                }
            }
            if thread_stop.is_stopped() {
                break;
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
            if thread_stop.is_stopped() {
                break;
            }
            if ok.is_err() || returned == 0 {
                // 目录失效,关掉重来
                unsafe { let _ = windows::Win32::Foundation::CloseHandle(h); }
                handle = None;
                if thread_stop.wait_timeout(Duration::from_secs(3)) {
                    break;
                }
                continue;
            }
            let names = parse_notify_names(&buf, returned as usize);
            if !names.is_empty() && !thread_stop.is_stopped() {
                notify(names);
            }
        }
        if let Some(handle) = handle {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
        }
    });
    DirWatcher {
        stop,
        thread: Some(thread),
    }
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
            || action == FILE_ACTION_REMOVED.0
            || action == FILE_ACTION_MODIFIED.0
            || action == FILE_ACTION_RENAMED_OLD_NAME.0
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
    use super::spawn_dir_watcher;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use windows::Win32::Storage::FileSystem::{
        FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME,
    };

    fn notify_record(action: u32, name: &str) -> Vec<u8> {
        let name: Vec<u16> = name.encode_utf16().collect();
        let mut record = Vec::new();
        record.extend_from_slice(&0u32.to_le_bytes());
        record.extend_from_slice(&action.to_le_bytes());
        record.extend_from_slice(&((name.len() * 2) as u32).to_le_bytes());
        for ch in name {
            record.extend_from_slice(&ch.to_le_bytes());
        }
        record
    }

    #[test]
    fn parses_single_unpadded_final_record() {
        let record = notify_record(FILE_ACTION_RENAMED_NEW_NAME.0, "Notion-7.29.0.msix");
        assert_eq!(
            parse_notify_names(&record, record.len()),
            ["Notion-7.29.0.msix"]
        );
    }

    #[test]
    fn parses_removed_name() {
        let record = notify_record(FILE_ACTION_REMOVED.0, "~$设备报警.xlsx");

        assert_eq!(
            parse_notify_names(&record, record.len()),
            ["~$设备报警.xlsx"]
        );
    }

    #[test]
    fn parses_renamed_old_name() {
        let record = notify_record(FILE_ACTION_RENAMED_OLD_NAME.0, "before.txt");

        assert_eq!(parse_notify_names(&record, record.len()), ["before.txt"]);
    }

    #[test]
    fn stopped_watcher_does_not_deliver_later_changes() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "feather-fences-watcher-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let (tx, rx) = mpsc::channel();
        let mut watcher = spawn_dir_watcher(dir.clone(), move |names| {
            let _ = tx.send(names);
        });

        // The worker opens the directory asynchronously. Probe until the first
        // notification proves the watcher is ready instead of relying on sleep.
        let mut ready = false;
        for attempt in 0..30 {
            let name = format!("probe-{attempt}.txt");
            std::fs::write(dir.join(&name), b"probe").unwrap();
            if let Ok(names) = rx.recv_timeout(Duration::from_millis(100)) {
                if names.iter().any(|seen| seen == &name) {
                    ready = true;
                    break;
                }
            }
        }
        assert!(ready, "watcher did not report a change within 3 seconds");

        watcher.stop();
        while rx.try_recv().is_ok() {}
        std::fs::write(dir.join("second.txt"), b"second").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(250)).is_err());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn watcher_reports_removed_file() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "feather-fences-watcher-remove-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let (tx, rx) = mpsc::channel();
        let mut watcher = spawn_dir_watcher(dir.clone(), move |names| {
            let _ = tx.send(names);
        });

        let mut created = None;
        for attempt in 0..30 {
            let name = format!("remove-{attempt}.txt");
            std::fs::write(dir.join(&name), b"temporary").unwrap();
            if let Ok(names) = rx.recv_timeout(Duration::from_millis(100)) {
                if names.iter().any(|seen| seen == &name) {
                    created = Some(name);
                    break;
                }
            }
        }
        let name = created.expect("watcher did not report file creation within 3 seconds");
        while rx.try_recv().is_ok() {}

        std::fs::remove_file(dir.join(&name)).unwrap();
        let mut removed = false;
        for _ in 0..10 {
            if let Ok(names) = rx.recv_timeout(Duration::from_millis(100)) {
                if names.iter().any(|seen| seen == &name) {
                    removed = true;
                    break;
                }
            }
        }
        assert!(
            removed,
            "watcher did not report the removed file within 1 second"
        );

        watcher.stop();
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// 跨卷移动文件：先复制，再删除源文件。
///
/// 删除源文件失败时，不能把操作报告为成功，否则调用方会误以为 MOVE 已完成。
/// 此时尽量删除刚创建的目标副本，把操作回滚到“源文件仍在、目标文件不存在”。
fn copy_then_remove_file(
    src: &Path,
    dest: &Path,
    rename_error: &std::io::Error,
) -> Result<(), String> {
    let mut destination_created = false;
    let copy_result = (|| -> std::io::Result<()> {
        let mut source = std::fs::File::open(src)?;
        // create_new 保证即使目标名称在检查后被其他程序抢先创建，也不会覆盖它。
        let mut target = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)?;
        destination_created = true;
        std::io::copy(&mut source, &mut target)?;
        target.sync_all()?;
        if let Ok(metadata) = source.metadata() {
            std::fs::set_permissions(dest, metadata.permissions())?;
        }
        Ok(())
    })();
    if let Err(copy_error) = copy_result {
        // 只有本次 create_new 确实创建了 dest 才清理，绝不能删除竞态中由别人创建的文件。
        if destination_created {
            let _ = std::fs::remove_file(dest);
        }
        return Err(format!(
            "重命名失败：{rename_error}；复制失败：{copy_error}"
        ));
    }

    if let Err(remove_error) = std::fs::remove_file(src) {
        return match std::fs::remove_file(dest) {
            Ok(()) => Err(format!(
                "重命名失败：{rename_error}；复制完成但无法删除源文件：{remove_error}；已撤销目标副本"
            )),
            Err(rollback_error) => Err(format!(
                "重命名失败：{rename_error}；复制完成但无法删除源文件：{remove_error}；目标副本也无法回滚：{rollback_error}"
            )),
        };
    }

    Ok(())
}

fn copy_file_new(src: &Path, dest: &Path) -> Result<(), String> {
    let mut source = std::fs::File::open(src).map_err(|e| e.to_string())?;
    let mut target = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| e.to_string())?;
    std::io::copy(&mut source, &mut target).map_err(|e| e.to_string())?;
    target.sync_all().map_err(|e| e.to_string())?;
    if let Ok(metadata) = source.metadata() {
        std::fs::set_permissions(dest, metadata.permissions()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn copy_dir_tree(src: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir(dest).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let source = entry.path();
        let target = dest.join(entry.file_name());
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            copy_dir_tree(&source, &target)?;
        } else {
            copy_file_new(&source, &target)?;
        }
    }
    if let Ok(metadata) = std::fs::metadata(src) {
        std::fs::set_permissions(dest, metadata.permissions()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 复制项目到目标目录，保留源项目，并像 Explorer 一样为重名目标生成新名称。
pub fn copy_to_dir(src: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    if !dest_dir.exists() {
        std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
    }
    let name = src.file_name().ok_or("no file name")?.to_os_string();
    let dest = unique_dest(dest_dir, &name);
    let result = if src.is_dir() {
        copy_dir_tree(src, &dest)
    } else {
        copy_file_new(src, &dest)
    };
    if let Err(error) = result {
        // `dest` was chosen as a unique name and created only by this copy attempt.
        if dest.is_dir() {
            let _ = std::fs::remove_dir_all(&dest);
        } else {
            let _ = std::fs::remove_file(&dest);
        }
        return Err(error);
    }
    Ok(dest)
}

/// 移动项目到目标目录（同卷 rename，跨卷文件 copy+delete），自动避免重名。
pub fn move_to_dir(src: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    if !dest_dir.exists() {
        std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
    }
    let name = src.file_name().ok_or("no file name")?.to_os_string();
    let dest = unique_dest(dest_dir, &name);
    match std::fs::rename(src, &dest) {
        Ok(()) => Ok(dest),
        Err(rename_error) => {
            // std::fs::copy 不支持目录。目录同卷可由 rename 完成；跨卷失败时明确报错，
            // 交给上层提示用户，不做可能产生半成品的递归复制。
            if src.is_dir() {
                return Err(format!(
                    "无法移动文件夹：{rename_error}；暂不支持跨磁盘移动文件夹"
                ));
            }
            copy_then_remove_file(src, &dest, &rename_error)?;
            Ok(dest)
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

#[cfg(test)]
mod move_tests {
    use super::{copy_then_remove_file, copy_to_dir};
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    fn test_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "feather-fences-move-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn copy_then_remove_reports_a_completed_move() {
        let dir = test_dir();
        let src = dir.join("source.txt");
        let dest = dir.join("dest.txt");
        std::fs::write(&src, b"content").unwrap();
        let rename_error = std::io::Error::other("forced cross-volume fallback");

        copy_then_remove_file(&src, &dest, &rename_error).unwrap();

        assert!(!src.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"content");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copy_then_remove_rolls_back_when_the_source_cannot_be_deleted() {
        let dir = test_dir();
        let src = dir.join("source.txt");
        let dest = dir.join("dest.txt");
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .open(&src)
            .unwrap();
        file.write_all(b"content").unwrap();
        file.flush().unwrap();
        let rename_error = std::io::Error::other("forced cross-volume fallback");

        let error = copy_then_remove_file(&src, &dest, &rename_error).unwrap_err();

        assert!(error.contains("已撤销目标副本"));
        assert!(src.exists());
        assert!(!dest.exists());
        drop(file);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copy_then_remove_never_overwrites_a_raced_destination() {
        let dir = test_dir();
        let src = dir.join("source.txt");
        let dest = dir.join("dest.txt");
        std::fs::write(&src, b"source content").unwrap();
        std::fs::write(&dest, b"existing content").unwrap();
        let rename_error = std::io::Error::other("forced cross-volume fallback");

        let error = copy_then_remove_file(&src, &dest, &rename_error).unwrap_err();

        assert!(error.contains("复制失败"));
        assert!(src.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"existing content");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copy_to_dir_keeps_source_and_copies_nested_folders() {
        let dir = test_dir();
        let source_root = dir.join("source-root");
        let source = source_root.join("folder");
        let destination = dir.join("destination");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("nested").join("file.txt"), b"content").unwrap();

        let copied = copy_to_dir(&source, &destination).unwrap();

        assert!(source.join("nested").join("file.txt").exists());
        assert_eq!(
            std::fs::read(copied.join("nested").join("file.txt")).unwrap(),
            b"content"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
