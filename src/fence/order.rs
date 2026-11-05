// 自定义排序(每个栅栏独立):按 FenceCfg.order 排条目,以及上移/下移/置顶/恢复默认。
//
// 语义:order 是文件名列表(大小写不敏感,空 = 默认排序 = 文件夹优先 + 名称)。
// 列表里有的按记录位置;不在列表里的(新拖入、改名)排到末尾,彼此按默认规则,
// 所以拖入新文件不会打乱已排好的顺序。旧名字留在列表里无害,下次调整时顺带清掉。
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::Global;

use super::grid::{config_snapshot, grid_dims, sync_page};
use super::render::render_fence;
use super::Entry;

#[derive(Clone, Copy)]
pub(crate) enum OrderAction {
    Up,
    Down,
    Top,
}

fn default_cmp(a: &Entry, b: &Entry) -> Ordering {
    b.is_dir
        .cmp(&a.is_dir)
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
}

/// 按 `order` 重排条目。空列表 = 默认排序。
pub(crate) fn sort_entries(entries: &mut [Entry], order: &[String]) {
    if order.is_empty() {
        entries.sort_by(default_cmp);
        return;
    }
    let pos: HashMap<String, usize> = order
        .iter()
        .enumerate()
        .map(|(i, name)| (name.to_lowercase(), i))
        .collect();
    entries.sort_by(
        |a, b| match (pos.get(&a.name.to_lowercase()), pos.get(&b.name.to_lowercase())) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => default_cmp(a, b),
        },
    );
}

/// 把当前显示顺序固化成 order 列表,再按动作挪动 `sel` 那一项。
/// 超出边界的动作保持原顺序(菜单里对应项已置灰,正常点不到)。
fn reordered(entries: &[Entry], sel: usize, action: OrderAction) -> Vec<String> {
    let mut order: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    if sel >= order.len() {
        return order;
    }
    match action {
        OrderAction::Up if sel > 0 => order.swap(sel, sel - 1),
        OrderAction::Down if sel + 1 < order.len() => order.swap(sel, sel + 1),
        OrderAction::Top if sel > 0 => {
            let name = order.remove(sel);
            order.insert(0, name);
        }
        _ => {}
    }
    order
}

/// 按当前 order 重排 → 选中项按路径跟随 → 翻到它所在页 → 重绘并持久化。
fn apply(g: &mut Global, idx: usize, selected_path: Option<PathBuf>) {
    let ghost = g.config.ghost_mode;
    {
        let f = &mut g.fences[idx];
        sort_entries(&mut f.entries, &f.cfg.order);
        f.selected = selected_path
            .as_ref()
            .and_then(|p| f.entries.iter().position(|e| &e.path == p));
        if let Some(sel) = f.selected {
            // 置顶/跨页上移后立刻能看到结果
            let (cols, rows) = grid_dims(f);
            let per_page = (cols.max(1) as usize) * (rows.max(0) as usize);
            if per_page > 0 {
                f.page = sel / per_page;
            }
            sync_page(f);
        }
    }
    render_fence(&mut g.icons, ghost, &mut g.fences[idx]);
    g.config.fences = config_snapshot(&g.fences);
    crate::config::save(&g.config);
}

/// 上移/下移/置顶当前选中的条目。没有选中项时不动。
pub(crate) fn move_selected(g: &mut Global, idx: usize, action: OrderAction) {
    let (sel, path) = {
        let f = &g.fences[idx];
        let Some(sel) = f.selected.filter(|i| *i < f.entries.len()) else {
            return;
        };
        (sel, f.entries[sel].path.clone())
    };
    g.fences[idx].cfg.order = reordered(&g.fences[idx].entries, sel, action);
    apply(g, idx, Some(path));
}

/// 恢复默认排序:清空自定义顺序。
pub(crate) fn reset_order(g: &mut Global, idx: usize) {
    let selected_path = {
        let f = &g.fences[idx];
        f.selected
            .and_then(|i| f.entries.get(i))
            .map(|e| e.path.clone())
    };
    g.fences[idx].cfg.order.clear();
    apply(g, idx, selected_path);
}

/// 菜单用:当前栅栏的选中状态 → (可上移/置顶, 可下移, 有自定义顺序)。
pub(crate) fn menu_state(g: &Global, idx: usize) -> (bool, bool, bool) {
    let f = &g.fences[idx];
    let sel = f.selected.filter(|i| *i < f.entries.len());
    let can_up = sel.is_some_and(|s| s > 0);
    let can_down = sel.is_some_and(|s| s + 1 < f.entries.len());
    (can_up, can_down, !f.cfg.order.is_empty())
}

#[cfg(test)]
mod order_tests {
    use super::{reordered, sort_entries, OrderAction};
    use crate::fence::Entry;
    use std::path::PathBuf;

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            path: PathBuf::from(name),
            name: name.into(),
            is_dir,
            stamp: crate::icons::FileStamp::default(),
        }
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn empty_order_keeps_default_folders_first_then_name() {
        let mut entries = vec![entry("b.txt", false), entry("A", true), entry("a.txt", false)];
        sort_entries(&mut entries, &[]);
        assert_eq!(names(&entries), ["A", "a.txt", "b.txt"]);
    }

    #[test]
    fn custom_order_wins_and_unknown_entries_go_last_by_default_rule() {
        let mut entries = vec![
            entry("a", false),
            entry("b", false),
            entry("new", false),
            entry("dir", true),
        ];
        sort_entries(&mut entries, &["b".into(), "a".into()]);
        // 新条目不在列表里 → 排末尾,彼此按默认规则(文件夹优先)
        assert_eq!(names(&entries), ["b", "a", "dir", "new"]);
    }

    #[test]
    fn stale_names_and_case_differences_are_tolerated() {
        let mut entries = vec![entry("A", false), entry("b", false)];
        sort_entries(&mut entries, &["B".into(), "已删除的".into(), "a".into()]);
        assert_eq!(names(&entries), ["b", "A"]);
    }

    #[test]
    fn reorder_freezes_display_order_and_moves_the_selected_item() {
        let entries = vec![entry("a", false), entry("b", false), entry("c", false)];

        assert_eq!(reordered(&entries, 1, OrderAction::Up), ["b", "a", "c"]);
        assert_eq!(reordered(&entries, 1, OrderAction::Down), ["a", "c", "b"]);
        assert_eq!(reordered(&entries, 2, OrderAction::Top), ["c", "a", "b"]);
        // 边界动作保持原顺序(菜单里已置灰)
        assert_eq!(reordered(&entries, 0, OrderAction::Up), ["a", "b", "c"]);
        assert_eq!(reordered(&entries, 2, OrderAction::Down), ["a", "b", "c"]);
        assert_eq!(reordered(&entries, 0, OrderAction::Top), ["a", "b", "c"]);
    }
}
