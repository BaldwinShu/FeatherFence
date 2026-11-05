// 目录刷新:目录变化去抖(REFRESH_TICK 计时器 + 安静期),扫描目录重建条目。
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};

use crate::Global;

use super::grid::sync_page;
use super::render::render_fence;
use super::{Entry, Fence};

/// 重扫目录并重建条目。返回**本次新出现**的路径(供调用方安排图标复核);
/// 路径集合没变、或这是该栅栏的首次装载时返回空。
pub fn refresh_entries(f: &mut Fence, vault: &PathBuf) -> Vec<PathBuf> {
    let profiling = crate::perf::enabled();
    let total_started = profiling.then(Instant::now);
    let page = f.page;
    let selected_path = f.selected.and_then(|i| f.entries.get(i)).map(|e| e.path.clone());
    let first_scan = !f.scanned_once;
    f.scanned_once = true;
    let known: HashSet<PathBuf> = f.entries.iter().map(|e| e.path.clone()).collect();
    f.entries.clear();
    let dir = f.cfg.folder.clone().unwrap_or_else(|| vault.clone());
    let read_started = profiling.then(Instant::now);
    let read_time;
    let mut sort_time = Duration::default();
    let mut succeeded = false;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        succeeded = true;
        for e in rd.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            // 指纹从枚举结果里取(Windows 上不额外 IO),图标缓存据此判断过期。
            let stamp = e
                .metadata()
                .map(|m| crate::icons::FileStamp::from_metadata(&m))
                .unwrap_or_default();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            f.entries.push(Entry { path, name, is_dir, stamp });
        }
        read_time = read_started
            .map(|started| started.elapsed())
            .unwrap_or_default();
        let sort_started = profiling.then(Instant::now);
        // 自定义排序(每个栅栏独立);order 为空时等价于文件夹优先 + 名称
        super::order::sort_entries(&mut f.entries, &f.cfg.order);
        sort_time = sort_started
            .map(|started| started.elapsed())
            .unwrap_or_default();
    } else {
        read_time = read_started
            .map(|started| started.elapsed())
            .unwrap_or_default();
    }
    f.selected = selected_path.and_then(|p| f.entries.iter().position(|e| e.path == p));
    f.page = page;
    f.wheel_acc = 0;
    sync_page(f);
    if let Some(started) = total_started {
        crate::perf::record_refresh(
            f.cfg.id,
            &dir,
            f.entries.len(),
            read_time,
            sort_time,
            started.elapsed(),
            succeeded,
        );
    }
    if first_scan {
        return Vec::new();
    }
    f.entries
        .iter()
        .filter(|e| !known.contains(&e.path))
        .map(|e| e.path.clone())
        .collect()
}
/// 目录变化刷新计时器：连续事件安静一小段时间后再扫描和重绘。
pub(crate) const REFRESH_TICK: usize = 0xFE11;
pub(crate) const REFRESH_DEBOUNCE_MS: u32 = 150;

/// 新条目图标复核计时器(一次性)。避开 ANIM_TICK=0xFE10 / REFRESH_TICK=0xFE11 /
/// BACKDROP_NUDGE_TICK=0xFE12。
pub(crate) const ICON_RECHECK_TICK: usize = 0xFE13;
/// 文件落地后 shell 解析图标(尤其 .lnk 的目标图标)可能有一拍瞬态,
/// 等它稳定再重取一次,而不是把通用图标永久缓存(issue #41)。
const ICON_RECHECK_MS: u32 = 1000;

/// 登记本次刷新新出现的路径,并在一秒后统一作废这些路径的图标缓存、重绘一次。
/// 正常情况重取结果不变,最多一次整幅提交;瞬态取到的通用图标会被纠正。
pub(crate) fn arm_icon_recheck(g: &mut Global, idx: usize, fresh: Vec<PathBuf>) {
    if fresh.is_empty() {
        return;
    }
    let f = &mut g.fences[idx];
    if f.hwnd.is_invalid() {
        return;
    }
    f.icon_recheck.extend(fresh);
    unsafe {
        let _ = SetTimer(Some(f.hwnd), ICON_RECHECK_TICK, ICON_RECHECK_MS, None);
    }
}

pub(crate) fn stop_refresh_timer(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), REFRESH_TICK);
    }
}

pub(crate) fn restart_refresh_timer(hwnd: HWND, delay_ms: u32) -> bool {
    stop_refresh_timer(hwnd);
    unsafe { SetTimer(Some(hwnd), REFRESH_TICK, delay_ms.max(1), None) != 0 }
}

pub(crate) fn refresh_fence_now(g: &mut Global, idx: usize) {
    let ghost = g.config.ghost_mode;
    let vault = crate::config::vault_dir(&g.config);
    let dir = g.fences[idx].cfg.folder.clone().unwrap_or_else(|| vault.clone());
    // 目录变了 → 该目录下的缩略图缓存作废(被替换的图片要显示新内容)
    g.icons.forget_thumbs_in(&dir);
    let fresh = {
        let f = &mut g.fences[idx];
        refresh_entries(f, &vault)
    };
    arm_icon_recheck(g, idx, fresh);
    let f = &mut g.fences[idx];
    render_fence(&mut g.icons, ghost, f);
}
#[cfg(test)]
mod refresh_tests {
    use super::{refresh_entries, REFRESH_DEBOUNCE_MS};
    use crate::config::FenceCfg;
    use crate::fence::grid::{animation_progress, grid_dims, ANIM_DURATION};
    use crate::fence::{Fence, RefreshState, RefreshTimerAction};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use windows::Win32::Foundation::HWND;

    #[test]
    fn refresh_state_coalesces_events_until_the_quiet_period() {
        let mut state = RefreshState::default();
        let start = Instant::now();
        let delay = Duration::from_millis(REFRESH_DEBOUNCE_MS as u64);

        assert!(state.record_event(start));
        assert!(!state.record_event(start + Duration::from_millis(50)));
        assert_eq!(
            state.timer_action(start + Duration::from_millis(150), delay),
            RefreshTimerAction::Wait(50)
        );
        assert_eq!(
            state.timer_action(start + Duration::from_millis(200), delay),
            RefreshTimerAction::Refresh
        );
        assert_eq!(
            state.timer_action(start + Duration::from_millis(201), delay),
            RefreshTimerAction::Idle
        );
        assert!(state.record_event(start + Duration::from_millis(202)));
    }

    #[test]
    fn animation_progress_tracks_elapsed_time_and_clamps_at_the_end() {
        assert_eq!(animation_progress(Duration::ZERO), 0.0);
        assert_eq!(animation_progress(ANIM_DURATION / 2), 0.5);
        assert_eq!(animation_progress(ANIM_DURATION), 1.0);
        assert_eq!(animation_progress(ANIM_DURATION * 2), 1.0);
    }

    #[test]
    fn refresh_reports_only_newly_appeared_entries() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "feather-fences-fresh-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();

        let cfg = FenceCfg {
            folder: Some(dir.clone()),
            ..FenceCfg::default()
        };
        let mut fence = Fence::new(cfg, HWND::default());

        // 首次装载不算"新出现":不做整箱图标复核
        assert!(refresh_entries(&mut fence, &dir).is_empty());
        // 内容没变 → 没有新条目(不会触发图标复核)
        assert!(refresh_entries(&mut fence, &dir).is_empty());

        std::fs::write(dir.join("c.txt"), b"c").unwrap();
        let added = refresh_entries(&mut fence, &dir);
        assert_eq!(added, vec![dir.join("c.txt")]);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refresh_preserves_page_and_clamps_when_contents_shrink() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "feather-fences-refresh-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..40 {
            std::fs::write(dir.join(format!("item-{i:02}.txt")), b"item").unwrap();
        }

        let cfg = FenceCfg {
            folder: Some(dir.clone()),
            ..FenceCfg::default()
        };
        let mut fence = Fence::new(cfg, HWND::default());
        fence.page = 1;
        refresh_entries(&mut fence, &dir);
        assert_eq!(fence.page, 1);
        assert_eq!(fence.top_row, grid_dims(&fence).1 as f32);

        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            std::fs::remove_file(entry.path()).unwrap();
        }
        refresh_entries(&mut fence, &dir);
        assert_eq!(fence.page, 0);
        assert_eq!(fence.top_row, 0.0);

        std::fs::remove_dir_all(dir).unwrap();
    }
}
