
// 快捷方式自动收纳:桌面新增 .lnk 按"快捷方式占比/数量"选目标收纳栅栏。
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::{self, FenceKind};
use crate::fence;
use crate::fencelife::reserve_desktop_icons;
use crate::watcher;
use crate::{with_global, Global};

use super::{CollectionStats, FileCandidate};

// ---------- 自动归类 ----------

pub(crate) fn ext_of(path: &Path) -> String {
    path.extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

pub(crate) fn is_shortcut(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"))
}

fn scan_collection(id: u32, dir: &Path) -> Option<CollectionStats> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut stats = CollectionStats {
        id,
        shortcuts: 0,
        files: 0,
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        if is_shortcut(&entry.path()) {
            stats.shortcuts = stats.shortcuts.saturating_add(1);
        } else {
            stats.files = stats.files.saturating_add(1);
        }
    }
    Some(stats)
}

fn choose_collection(stats: &[CollectionStats]) -> Option<u32> {
    if stats.iter().all(|stats| stats.shortcuts == 0) {
        return stats
            .iter()
            .filter(|stats| stats.files == 0)
            .min_by_key(|stats| stats.id)
            .or_else(|| stats.iter().min_by_key(|stats| stats.id))
            .map(|stats| stats.id);
    }

    stats
        .iter()
        .max_by(|a, b| {
            let a_total = a.shortcuts.saturating_add(a.files).max(1) as u128;
            let b_total = b.shortcuts.saturating_add(b.files).max(1) as u128;
            ((a.shortcuts as u128) * b_total)
                .cmp(&((b.shortcuts as u128) * a_total))
                .then_with(|| a.shortcuts.cmp(&b.shortcuts))
                // max_by 应把较小 ID 视为更优。
                .then_with(|| b.id.cmp(&a.id))
        })
        .map(|stats| stats.id)
}

fn choose_collection_target(g: &Global) -> Option<(u32, PathBuf)> {
    let vault = config::vault_dir(&g.config);
    let candidates: Vec<(CollectionStats, PathBuf)> = g
        .fences
        .iter()
        .filter(|f| f.valid && f.cfg.kind == FenceKind::Collection)
        .filter_map(|f| {
            let dir = f.cfg.folder.clone().unwrap_or_else(|| vault.clone());
            scan_collection(f.cfg.id, &dir).map(|stats| (stats, dir))
        })
        .collect();
    let stats: Vec<CollectionStats> = candidates.iter().map(|(stats, _)| *stats).collect();
    let id = choose_collection(&stats)?;
    candidates
        .into_iter()
        .find(|(stats, _)| stats.id == id)
        .map(|(_, dir)| (id, dir))
}

fn queue_shortcut_candidate(pending: &mut HashMap<PathBuf, FileCandidate>, path: PathBuf) {
    if !is_shortcut(&path) {
        return;
    }
    pending.entry(path).or_insert(FileCandidate {
        len: u64::MAX,
        modified: None,
        stable_ticks: 0,
    });
}

fn queue_new_shortcut_candidate(
    seen: &mut HashSet<PathBuf>,
    pending: &mut HashMap<PathBuf, FileCandidate>,
    path: PathBuf,
) {
    if is_shortcut(&path) && seen.insert(path.clone()) {
        queue_shortcut_candidate(pending, path);
    }
}

const DRAGOUT_EVENT_TAIL: Duration = Duration::from_secs(5);

/// State held while OLE is running its nested drag loop. Desktop notifications can arrive while
/// `DoDragDrop` is still active, so they must be classified before the normal collector sees them.
#[derive(Debug)]
pub(crate) struct ShortcutDragoutState {
    active: bool,
    source_stem: String,
    desktop_dirs: Vec<PathBuf>,
    held: HashSet<PathBuf>,
    ignore_until: Option<Instant>,
}

impl ShortcutDragoutState {
    fn begin(source: &Path) -> Option<Self> {
        if !is_shortcut(source) {
            return None;
        }
        let source_stem = source.file_stem()?.to_string_lossy().to_lowercase();
        let desktop_dirs = [crate::desktop_dir(), crate::public_desktop_dir()]
            .into_iter()
            .flatten()
            .collect();
        Some(Self {
            active: true,
            source_stem,
            desktop_dirs,
            held: HashSet::new(),
            ignore_until: None,
        })
    }

    fn is_desktop_path(&self, path: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        self.desktop_dirs.iter().any(|dir| same_path(parent, dir))
    }

    fn is_dragged_name(&self, path: &Path) -> bool {
        let Some(stem) = path.file_stem().map(|value| value.to_string_lossy().to_lowercase())
        else {
            return false;
        };
        if stem == self.source_stem {
            return true;
        }
        let prefix = format!("{} (", self.source_stem);
        stem.strip_prefix(&prefix)
            .and_then(|suffix| suffix.strip_suffix(')'))
            .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()))
    }

    fn should_suppress(&self, path: &Path, now: Instant) -> bool {
        self.is_desktop_path(path)
            && is_shortcut(path)
            && self.is_dragged_name(path)
            && self.ignore_until.is_some_and(|until| now <= until)
    }

    fn hold(&mut self, path: PathBuf) {
        if is_shortcut(&path) {
            self.held.insert(path);
        }
    }

    fn finish(&mut self) -> Vec<PathBuf> {
        self.active = false;
        self.ignore_until = Some(Instant::now() + DRAGOUT_EVENT_TAIL);
        self.held.drain().collect()
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
}

/// Begin suppressing only the desktop shortcut names that can be produced by this drag.
pub(crate) fn begin_shortcut_dragout(source: &Path) -> bool {
    let Some(state) = ShortcutDragoutState::begin(source) else {
        return false;
    };
    with_global(|g| g.shortcut_dragout = Some(state));
    true
}

/// End the nested OLE loop and replay unrelated notifications. Matching desktop notifications
/// are marked as already seen, then filtered for a short tail in case Explorer delivers them late.
pub(crate) fn finish_shortcut_dragout() {
    with_global(|g| {
        let Some(mut state) = g.shortcut_dragout.take() else {
            return;
        };
        let held = state.finish();
        let now = Instant::now();
        for path in held {
            if state.is_desktop_path(&path) && state.is_dragged_name(&path) {
                g.shortcut_pending.remove(&path);
                g.shortcut_seen.insert(path);
            } else {
                queue_new_shortcut_candidate(
                    &mut g.shortcut_seen,
                    &mut g.shortcut_pending,
                    path,
                );
            }
        }
        state.ignore_until = Some(now + DRAGOUT_EVENT_TAIL);
        g.shortcut_dragout = Some(state);
    });
}

fn ingest_shortcut_events(g: &mut Global) {
    let now = Instant::now();
    while let Ok(paths) = g.desktop_rx.try_recv() {
        for path in paths {
            if let Some(state) = g.shortcut_dragout.as_mut() {
                if state.active {
                    state.hold(path);
                    continue;
                }
                if state.should_suppress(&path, now) {
                    g.shortcut_pending.remove(&path);
                    g.shortcut_seen.insert(path);
                    continue;
                }
            }
            queue_new_shortcut_candidate(&mut g.shortcut_seen, &mut g.shortcut_pending, path);
        }
    }
    if g
        .shortcut_dragout
        .as_ref()
        .is_some_and(|state| !state.active && state.ignore_until.is_some_and(|until| now > until))
    {
        g.shortcut_dragout = None;
    }
}

pub(crate) fn shortcut_tick(g: &mut Global) {
    ingest_shortcut_events(g);
    let paths: Vec<PathBuf> = g.shortcut_pending.keys().cloned().collect();
    let mut completed = Vec::new();
    let mut moved_to = HashSet::new();

    for path in paths {
        let Ok(meta) = std::fs::metadata(&path) else {
            completed.push(path);
            continue;
        };
        if !meta.is_file() || !is_shortcut(&path) {
            completed.push(path);
            continue;
        }
        let modified = meta.modified().ok();
        let ready = if let Some(state) = g.shortcut_pending.get_mut(&path) {
            if state.len == meta.len() && state.modified == modified {
                state.stable_ticks = state.stable_ticks.saturating_add(1);
            } else {
                state.len = meta.len();
                state.modified = modified;
                state.stable_ticks = 0;
            }
            state.stable_ticks >= 2
        } else {
            false
        };
        if !ready {
            continue;
        }

        let Some((id, target)) = choose_collection_target(g) else {
            completed.push(path);
            continue;
        };
        match watcher::move_to_dir(&path, &target) {
            Ok(_) => {
                completed.push(path);
                moved_to.insert(id);
            }
            Err(e) => eprintln!("[feather] shortcut {:?} -> {}: {e}", path, target.display()),
        }
    }

    for path in completed {
        g.shortcut_pending.remove(&path);
    }
    g.shortcut_seen.retain(|path| path.exists());
    if moved_to.is_empty() {
        return;
    }
    for id in moved_to {
        if let Some(f) = g.fences.iter_mut().find(|f| f.valid && f.cfg.id == id) {
            fence::refresh_entries(f, &config::vault_dir(&g.config));
            fence::render_fence(&mut g.icons, g.config.ghost_mode, f);
        }
    }
    reserve_desktop_icons(g);
}
#[cfg(test)]
mod shortcut_collection_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn stats(id: u32, shortcuts: u64, files: u64) -> CollectionStats {
        CollectionStats {
            id,
            shortcuts,
            files,
        }
    }

    #[test]
    fn empty_box_wins_when_no_box_has_shortcuts() {
        let boxes = [stats(9, 0, 5), stats(4, 0, 0), stats(2, 0, 3)];

        assert_eq!(choose_collection(&boxes), Some(4));
    }

    #[test]
    fn lowest_id_wins_when_no_box_has_shortcuts_or_is_empty() {
        let boxes = [stats(9, 0, 5), stats(2, 0, 3)];

        assert_eq!(choose_collection(&boxes), Some(2));
    }

    #[test]
    fn highest_shortcut_ratio_wins() {
        let boxes = [stats(1, 3, 1), stats(2, 4, 2), stats(3, 0, 0)];

        assert_eq!(choose_collection(&boxes), Some(1));
    }

    #[test]
    fn more_shortcuts_win_when_ratios_are_equal() {
        let boxes = [stats(1, 1, 1), stats(2, 3, 3)];

        assert_eq!(choose_collection(&boxes), Some(2));
    }

    #[test]
    fn lowest_id_breaks_a_complete_tie() {
        let boxes = [stats(8, 3, 3), stats(2, 3, 3)];

        assert_eq!(choose_collection(&boxes), Some(2));
        assert_eq!(choose_collection(&[]), None);
    }

    #[test]
    fn collection_scan_counts_files_only_and_matches_lnk_case_insensitively() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "feather-fences-shortcuts-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        std::fs::write(dir.join("app.LNK"), b"shortcut").unwrap();
        std::fs::write(dir.join("notes.txt"), b"file").unwrap();
        std::fs::write(dir.join("folder").join("nested.lnk"), b"nested").unwrap();

        let actual = scan_collection(7, &dir);

        assert_eq!(actual, Some(stats(7, 1, 1)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn duplicate_notifications_share_one_pending_candidate() {
        let mut seen = HashSet::new();
        let mut pending = HashMap::new();
        let path = PathBuf::from(r"C:\Users\test\Desktop\App.lnk");

        queue_new_shortcut_candidate(&mut seen, &mut pending, path.clone());
        queue_new_shortcut_candidate(&mut seen, &mut pending, path);
        queue_new_shortcut_candidate(
            &mut seen,
            &mut pending,
            PathBuf::from(r"C:\Users\test\Desktop\notes.txt"),
        );

        assert_eq!(pending.len(), 1);
        assert_eq!(seen.len(), 1);
    }

    fn dragout_state() -> ShortcutDragoutState {
        ShortcutDragoutState {
            active: true,
            source_stem: "app".to_string(),
            desktop_dirs: vec![PathBuf::from(r"C:\Users\test\Desktop")],
            held: HashSet::new(),
            ignore_until: None,
        }
    }

    #[test]
    fn dragout_state_accepts_only_lnk_source() {
        assert!(ShortcutDragoutState::begin(Path::new(r"C:\Fence\App.lnk")).is_some());
        assert!(ShortcutDragoutState::begin(Path::new(r"C:\Fence\App.txt")).is_none());
    }

    #[test]
    fn dragout_destination_matches_source_and_explorer_collision_name() {
        let mut state = dragout_state();
        state.ignore_until = Some(Instant::now() + Duration::from_secs(1));

        assert!(state.should_suppress(
            Path::new(r"C:\Users\TEST\Desktop\app.lnk"),
            Instant::now()
        ));
        assert!(state.should_suppress(
            Path::new(r"C:\Users\test\Desktop\App (2).lnk"),
            Instant::now()
        ));
        assert!(!state.should_suppress(
            Path::new(r"C:\Users\test\Desktop\Other.lnk"),
            Instant::now()
        ));
        assert!(!state.should_suppress(
            Path::new(r"C:\Users\test\Downloads\App.lnk"),
            Instant::now()
        ));
    }

    #[test]
    fn dragout_tail_expires_without_blocking_future_shortcuts() {
        let mut state = dragout_state();
        state.ignore_until = Some(Instant::now() - Duration::from_secs(1));

        assert!(!state.should_suppress(
            Path::new(r"C:\Users\test\Desktop\App.lnk"),
            Instant::now()
        ));
    }

    #[test]
    fn finish_replays_unrelated_held_notifications() {
        let mut state = dragout_state();
        state.hold(PathBuf::from(r"C:\Users\test\Desktop\Other.lnk"));
        state.hold(PathBuf::from(r"C:\Users\test\Desktop\App.lnk"));

        let held = state.finish();

        assert!(!state.active);
        assert_eq!(held.len(), 2);
        assert!(state.ignore_until.is_some());
    }
}
