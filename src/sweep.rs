
// 桌面清扫:扩展名规则把桌面杂项文件搬到目标目录;失败项定时重试。
use crate::download::ingest_desktop_events;
use crate::shortcut::ext_of;
use crate::watcher;
use crate::Global;

use super::desktop_dir;

pub(crate) fn sweep_desktop(g: &mut Global) {
    if g.config.download_enabled {
        ingest_desktop_events(g);
    }
    let Some(dir) = desktop_dir() else { return };
    let rules = g.config.sweep_rules.clone();
    if rules.is_empty() {
        return;
    }
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        // 新下载优先进入下载收纳箱，不被扩展名清扫规则抢走。
        if g.download_pending.contains_key(&p) {
            continue;
        }
        let ext = ext_of(&p);
        if let Some(rule) = rules.iter().find(|r| r.ext.to_lowercase() == ext) {
            match watcher::move_to_dir(&p, &rule.dest) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[feather] sweep {:?}: {e}", p);
                    g.sweep_retry.push((p, rule.dest.clone()));
                }
            }
        }
    }
}
pub(crate) fn sweep_retry_tick(g: &mut Global) {
    let mut keep = Vec::new();
    for (src, dest) in std::mem::take(&mut g.sweep_retry) {
        if src.exists() {
            match watcher::move_to_dir(&src, &dest) {
                Ok(_) => {}
                Err(_) => keep.push((src, dest)),
            }
        }
    }
    g.sweep_retry = keep;
}
