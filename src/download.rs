
// 下载接管:浏览器下载归入专用收纳箱(独立 Downloads 监听 + 尺寸/时间稳定判定)。
use std::path::{Path, PathBuf};

use crate::config::{self, FenceCfg, FenceKind};
use crate::fence;
use crate::fencelife::{apply_visibility, create_fence, reserve_desktop_icons, sync_config};
use crate::utils;
use crate::watcher;
use crate::Global;

use super::FileCandidate;

pub(crate) fn ensure_download_box(g: &mut Global) {
    let dir = config::download_box_dir();
    let existing_idx = g
        .config
        .download_box_id
        .and_then(|id| g.fences.iter().position(|f| f.valid && f.cfg.id == id));
    if let Some(idx) = existing_idx {
        if g.fences[idx].cfg.kind != FenceKind::Download {
            g.fences[idx].cfg.kind = FenceKind::Download;
            sync_config(g);
        }
        return;
    }
    if let Some(idx) = g
        .fences
        .iter()
        .position(|f| f.valid && f.cfg.folder.as_deref() == Some(dir.as_path()))
    {
        let id = g.fences[idx].cfg.id;
        g.fences[idx].cfg.kind = FenceKind::Download;
        g.config.download_box_id = Some(id);
        sync_config(g);
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    let (sw, _sh) = utils::screen_size();
    let s = fence::dpi_scale();
    let cfg = FenceCfg {
        id: 0,
        title: "下载收纳箱".into(),
        kind: FenceKind::Download,
        folder: Some(dir),
        x: sw - (320.0 * s) as i32,
        y: (100.0 * s) as i32,
        w: (260.0 * s) as i32,
        h: (340.0 * s) as i32,
        dpi: (96.0 * s).round() as u32,
        opacity: 0.7,
        icon: 32,
        pos_set: None,
    };
    let id = create_fence(g, cfg);
    if id != 0 {
        g.config.download_box_id = Some(id);
        sync_config(g);
    }
}

fn is_download_box(g: &Global, id: u32) -> bool {
    g.config.download_box_id == Some(id)
}

pub(crate) fn download_box_should_show(g: &Global, id: u32) -> bool {
    !is_download_box(g, id) || (g.config.download_enabled && g.config.download_box_visible)
}

pub(crate) fn downloads_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(|p| PathBuf::from(p).join("Downloads"))
}

fn reset_download_tracking(g: &mut Global) {
    while g.download_rx.try_recv().is_ok() {}
    g.download_pending.clear();
    g.download_seen = downloads_dir()
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
}

pub(crate) fn set_download_enabled(g: &mut Global, enabled: bool) {
    if g.config.download_enabled == enabled {
        return;
    }
    g.config.download_enabled = enabled;
    reset_download_tracking(g);
    apply_visibility(g);
    reserve_desktop_icons(g);
    config::save(&g.config);
}

pub(crate) fn set_download_box_visible(g: &mut Global, visible: bool) {
    if g.config.download_box_visible == visible {
        return;
    }
    g.config.download_box_visible = visible;
    apply_visibility(g);
    reserve_desktop_icons(g);
    config::save(&g.config);
}
fn is_download_temp(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("crdownload" | "part" | "partial" | "download" | "tmp")
    )
}

pub(crate) fn ingest_desktop_events(g: &mut Global) {
    let Some(downloads) = downloads_dir() else { return };
    while let Ok(names) = g.download_rx.try_recv() {
        for name in names {
            let path = downloads.join(name);
            if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case("desktop.ini")) {
                continue;
            }
            if path.is_file() && !is_download_temp(&path) && g.download_seen.insert(path.clone()) {
                g.download_pending.insert(path, FileCandidate {
                    len: u64::MAX,
                    modified: None,
                    stable_ticks: 0,
                });
            }
        }
    }
    // 删除或已移动的路径不应永远占着 seen，允许日后同名下载再次被接管。
    g.download_seen.retain(|p| p.exists());
}

fn download_target(g: &Global) -> PathBuf {
    g.config
        .download_box_id
        .and_then(|id| g.fences.iter().find(|f| f.valid && f.cfg.id == id))
        .and_then(|f| f.cfg.folder.clone())
        .unwrap_or_else(config::download_box_dir)
}

pub(crate) fn download_tick(g: &mut Global) {
    if !g.config.download_enabled {
        while g.download_rx.try_recv().is_ok() {}
        g.download_pending.clear();
        return;
    }
    ingest_desktop_events(g);
    let target = download_target(g);
    let mut completed = Vec::new();
    for (path, state) in g.download_pending.iter_mut() {
        let Ok(meta) = std::fs::metadata(path) else {
            completed.push(path.clone());
            continue;
        };
        if !meta.is_file() {
            completed.push(path.clone());
            continue;
        }
        let modified = meta.modified().ok();
        if state.len == meta.len() && state.modified == modified {
            state.stable_ticks = state.stable_ticks.saturating_add(1);
        } else {
            state.len = meta.len();
            state.modified = modified;
            state.stable_ticks = 0;
        }
        // 连续约两秒无尺寸/时间变化后再移动，避免截断仍在写入的浏览器下载。
        if state.stable_ticks >= 2 && watcher::move_to_dir(path, &target).is_ok() {
            completed.push(path.clone());
        }
    }
    if completed.is_empty() {
        return;
    }
    for path in completed {
        g.download_pending.remove(&path);
        g.download_seen.remove(&path);
    }
    if let Some(id) = g.config.download_box_id {
        if let Some(f) = g.fences.iter_mut().find(|f| f.valid && f.cfg.id == id) {
            fence::refresh_entries(f, &config::vault_dir(&g.config));
            fence::render_fence(&mut g.icons, g.config.ghost_mode, f);
        }
    }
    reserve_desktop_icons(g);
}
