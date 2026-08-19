// 目录刷新:目录变化去抖(REFRESH_TICK 计时器 + 安静期),扫描目录重建条目。
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};

use crate::Global;

use super::grid::sync_page;
use super::render::render_fence;
use super::{Entry, Fence};

pub fn refresh_entries(f: &mut Fence, vault: &PathBuf) {
    let profiling = crate::perf::enabled();
    let total_started = profiling.then(Instant::now);
    let page = f.page;
    let selected_path = f.selected.and_then(|i| f.entries.get(i)).map(|e| e.path.clone());
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
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            f.entries.push(Entry { path, name, is_dir });
        }
        read_time = read_started
            .map(|started| started.elapsed())
            .unwrap_or_default();
        let sort_started = profiling.then(Instant::now);
        f.entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
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
}
/// 目录变化刷新计时器：连续事件安静一小段时间后再扫描和重绘。
pub(crate) const REFRESH_TICK: usize = 0xFE11;
pub(crate) const REFRESH_DEBOUNCE_MS: u32 = 150;

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
    let f = &mut g.fences[idx];
    refresh_entries(f, &vault);
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
