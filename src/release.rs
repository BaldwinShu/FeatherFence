//! 删除收纳栅栏时释放箱内文件:判定(plan / decide)与执行(execute)。
//!
//! 背景:收纳箱里的多数文件是自动收纳从桌面**移动**进来的;栅栏一删,箱目录
//! 留在 `%APPDATA%\feather-fences\boxes\` 成孤儿——不桌面、不在任何栅栏、无 UI 可达。
//! 因此删除非空且未被共享的收纳栅栏前:确认 → 判定(桌面空位/重名/同卷)
//! → 放回桌面(Release)/ 整箱搬到桌面(MoveFolder)/ 保留原地(Keep)。
//!
//! 判定即使全错也只走错分支(move_to_dir 的 unique_dest 兜底重名),
//! 永不覆盖、永不删内容——这是本模块天然安全的地方。
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Foundation::RECT;
use windows::Win32::Storage::FileSystem::GetVolumePathNameW;

use crate::config::{self, FenceCfg, FenceKind};
use crate::desktop_icons;
use crate::download::download_box_should_show;
use crate::shortcut;
use crate::utils::wstr;
use crate::watcher;
use crate::Global;

/// 判定结果,同时决定确认对话框的形态(Keep 只有 OK,其余 YESNO)。
#[derive(Debug, PartialEq)]
pub(crate) enum ReleaseAction {
    /// 逐条放回桌面
    Release,
    /// 整箱(目录)搬到桌面,用户手动整理
    MoveFolder { reason: String },
    /// 保留原地:对话框告知原因与路径
    Keep { reason: String },
}

/// 判定(纯函数)。`n` 为箱顶层条目数,由调用方保证 ≥1(空箱走无确认直删)。
///
/// 顺序即优先级:① 异卷 ② 空位无法评估 ③ 空位为 0 ④ 重名 ⑤ 空位不足。
/// ①~③ 落 Keep(连"整箱搬出"都做不到/没把握);④⑤ 落 MoveFolder;其余 Release。
pub(crate) fn decide(
    volume_same: bool,
    free: Option<usize>,
    collision: bool,
    n: usize,
) -> ReleaseAction {
    if !volume_same {
        return ReleaseAction::Keep {
            reason: "收纳箱与桌面不在同一磁盘,暂不支持跨磁盘移动".into(),
        };
    }
    match free {
        None => ReleaseAction::Keep {
            reason: "无法评估桌面空间".into(),
        },
        Some(0) => ReleaseAction::Keep {
            reason: "桌面图标已占满,没有空位".into(),
        },
        Some(free) => {
            if collision {
                ReleaseAction::MoveFolder {
                    reason: "桌面上存在同名文件".into(),
                }
            } else if free < n {
                ReleaseAction::MoveFolder {
                    reason: format!("桌面空位不足({free}/{n})"),
                }
            } else {
                ReleaseAction::Release
            }
        }
    }
}

/// 删除前收集的判定材料与结果。
pub(crate) struct Plan {
    pub title: String,
    pub folder: PathBuf,
    /// 顶层条目数(判定用;Keep 提示文案也用)
    pub n: usize,
    /// 其中快捷方式数(对话框文案:这些是从桌面被搬进来的)
    pub shortcuts: usize,
    pub action: ReleaseAction,
}

impl Plan {
    /// 确认对话框正文(MessageBoxW 在 menu 侧,借用之外弹)。
    pub fn dialog_text(&self) -> String {
        let brief = format!("「{}」内有 {} 个项目", self.title, self.n);
        match &self.action {
            ReleaseAction::Release => format!(
                "{brief},其中 {} 个快捷方式。\n删除栅栏后将把它们放回桌面。是否继续?",
                self.shortcuts
            ),
            ReleaseAction::MoveFolder { reason } => format!(
                "{brief},但{reason},放不回桌面。\n将把整个文件夹搬到桌面供你手动整理。是否继续?"
            ),
            ReleaseAction::Keep { reason } => format!(
                "{brief}。\n无法放回桌面({reason}),文件保留在:\n{}\n删除栅栏仅移除窗口,文件不动。",
                self.folder.display()
            ),
        }
    }
}

/// 收纳栅栏所有权判定:目录属于本程序托管才允许释放类操作。
///
/// - Collection + `folder=None` → 有效 vault;仅当 vault 在配置目录内(默认布局
///   或用户改到配置目录下)才算自管,改到外部(如 `D:\data`)只解绑不碰文件;
/// - Collection + `folder=Some` → 必须在 `boxes/` 之下且不是 boxes 根
///   (根自身被"释放"会连锅端,防手改配置误伤);
/// - 其余(门户/手改 kind)→ None,调用方仅解绑,目录绝不碰。
pub(crate) fn owned_folder(g: &Global, cfg: &FenceCfg) -> Option<PathBuf> {
    owned_folder_in(cfg, &config::config_dir(), &config::vault_dir(&g.config))
}

fn owned_folder_in(cfg: &FenceCfg, config_dir: &Path, vault: &Path) -> Option<PathBuf> {
    if cfg.kind != FenceKind::Collection {
        return None;
    }
    match &cfg.folder {
        None => vault.starts_with(config_dir).then(|| vault.to_path_buf()),
        Some(p) => {
            let boxes = config_dir.join("boxes");
            (p.as_path() != boxes && p.starts_with(&boxes)).then(|| p.clone())
        }
    }
}

/// 共享判定(纯参数版,供单测):`folder` 是否仍被别处引用。
///
/// `others` = 其余栅栏的 (id, folder),folder=None 折算 `vault`;
/// 被引用 = 其有效目录**等于或位于本目录之下**(移动/搬走即失联)。
/// 路径比较按路径分量、大小写敏感——本程序写出的路径大小写一致;
/// 手改配置大小写不一时宁可漏判(后果只是 sweep 重建一个空目录,无损)。
pub(crate) fn in_use_refs(
    folder: &Path,
    others: &[(u32, Option<PathBuf>)],
    sweep_dests: &[PathBuf],
    vault: &Path,
    download_dir: &Path,
) -> bool {
    // 下载箱目录永不释放:下载接管与它恒在的栅栏都在用
    if folder.starts_with(download_dir) {
        return true;
    }
    // 其余栅栏的有效目录命中本目录
    others.iter().any(|(_, f)| {
        let eff = f.as_deref().unwrap_or(vault);
        eff.starts_with(folder)
    }) || sweep_dests.iter().any(|d| d.starts_with(folder))
}

/// 共享判定:`g` 中除 `exclude_id` 外的栅栏、清扫规则、下载箱目录是否引用 `folder`。
pub(crate) fn in_use(g: &Global, folder: &Path, exclude_id: u32) -> bool {
    let vault = config::vault_dir(&g.config);
    let download_dir = config::download_box_dir();
    let others: Vec<(u32, Option<PathBuf>)> = g
        .fences
        .iter()
        .filter(|f| f.cfg.id != exclude_id)
        .map(|f| (f.cfg.id, f.cfg.folder.clone()))
        .collect();
    let sweep: Vec<PathBuf> = g.config.sweep_rules.iter().map(|r| r.dest.clone()).collect();
    in_use_refs(folder, &others, &sweep, &vault, &download_dir)
}

/// 桌面顶层名字集合(用户桌面 ∪ 公共桌面),小写化供大小写不敏感比较。
fn desktop_names() -> HashSet<String> {
    let mut names = HashSet::new();
    for dir in [crate::desktop_dir(), crate::public_desktop_dir()]
        .into_iter()
        .flatten()
    {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                names.insert(e.file_name().to_string_lossy().to_lowercase());
            }
        }
    }
    names
}

/// 箱顶层条目与桌面是否重名(大小写不敏感)。重名只影响分支选择,
/// 真移动时 unique_dest 还会兜底改名,判错也不覆盖文件。
fn has_collision(entries: &[PathBuf], names: &HashSet<String>) -> bool {
    entries.iter().any(|p| {
        p.file_name()
            .is_some_and(|n| names.contains(&n.to_string_lossy().to_lowercase()))
    })
}

/// 两路径是否同一卷(GetVolumePathNameW;失败保守视为异卷)。
fn same_volume(a: &Path, b: &Path) -> bool {
    fn vol(p: &Path) -> Option<String> {
        let w = wstr(&p.to_string_lossy());
        let mut buf = [0u16; 64];
        unsafe { GetVolumePathNameW(PCWSTR(w.as_ptr()), &mut buf).ok()? };
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }
    match (vol(a), vol(b)) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(&y),
        _ => false,
    }
}

/// 收集判定材料并给出动作。仅在 `owned_folder` 有值且目录非空时由 menu 调用。
///
/// `exclude_id` = 正在删除的栅栏:它的矩形即将消失,不算进禁放区
/// (否则刚放回的图标被"为将死栅栏保留的空位"白扣格子)。
pub(crate) fn plan(g: &Global, cfg: &FenceCfg, folder: &Path, exclude_id: u32) -> Plan {
    let entries: Vec<PathBuf> = std::fs::read_dir(folder)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    let n = entries.len();
    let shortcuts = entries.iter().filter(|p| shortcut::is_shortcut(p)).count();
    let collision = has_collision(&entries, &desktop_names());
    let desktop = crate::desktop_dir();
    let volume_same = desktop.as_deref().is_some_and(|d| same_volume(folder, d));

    // 桌面空位。闸门与 reserve_desktop_icons 一致:避让关着时 Explorer 本来就
    // 允许图标落在栅栏下,判定不该多扣栅栏矩形;find 失败时 perf::safe_desktop
    // 的桌面(如 RDP)同样不扣。
    let reserved: Vec<RECT> = if g.config.desktop_avoid && !crate::perf::safe_desktop() {
        g.fences
            .iter()
            .filter(|f| f.valid && f.cfg.id != exclude_id && download_box_should_show(g, f.cfg.id))
            .map(|f| RECT {
                left: f.cfg.x,
                top: f.cfg.y,
                right: f.cfg.x + f.cfg.w,
                bottom: f.cfg.y + f.cfg.h,
            })
            .collect()
    } else {
        Vec::new()
    };
    let free = desktop_icons::free_grid_cells(&reserved);

    let action = decide(volume_same, free, collision, n);
    Plan {
        title: cfg.title.clone(),
        folder: folder.to_path_buf(),
        n,
        shortcuts,
        action,
    }
}

/// 执行判定结果。`Ok(())` 后调用方才删除栅栏;`Err` = 保留栅栏
/// (已移动的条目留在桌面且登记过防回捞,剩余的还在箱里,可见、可重试)。
pub(crate) fn execute(g: &mut Global, plan: &Plan) -> Result<(), String> {
    match &plan.action {
        ReleaseAction::Keep { .. } => Ok(()),
        ReleaseAction::MoveFolder { .. } => {
            if !plan.folder.exists() {
                return Ok(());
            }
            let desktop = crate::desktop_dir().ok_or("无法获取桌面目录")?;
            watcher::move_to_dir(&plan.folder, &desktop)
                .map(|_| ())
                .map_err(|e| format!("移动收纳箱到桌面失败:{e}"))
        }
        ReleaseAction::Release => {
            if !plan.folder.exists() {
                return Ok(());
            }
            let desktop = crate::desktop_dir().ok_or("无法获取桌面目录")?;
            let entries: Vec<PathBuf> = std::fs::read_dir(&plan.folder)
                .map(|rd| rd.flatten().map(|e| e.path()).collect())
                .map_err(|e| format!("读取收纳箱失败:{e}"))?;
            if entries.is_empty() {
                // 对话框期间被清空:算成功,顺手把空目录收掉
                let _ = std::fs::remove_dir(&plan.folder);
                return Ok(());
            }
            for src in entries {
                // 逐条移动,任一失败即停;已移的必须已登记(见下),否则会被回捞。
                let dest = watcher::move_to_dir(&src, &desktop)
                    .map_err(|e| format!("{}:{e}", src.display()))?;
                // 防自动收纳回捞:落桌 .lnk 的变更事件会被 shortcut_tick 当成
                // "新快捷方式"再收进别的箱。同线程消息泵——这里登记 seen 后,
                // 事件入队与下次 tick 处理必然排在其后,queue_new 见过即跳过。
                if shortcut::is_shortcut(&dest) {
                    g.shortcut_seen.insert(dest.clone());
                    g.shortcut_pending.remove(&dest);
                }
            }
            // 全部移完,空目录收掉;收不掉(有并发新增等)就留着,绝不删内容
            let _ = std::fs::remove_dir(&plan.folder);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn nonce() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn decide_follows_priority_order() {
        // ① 异卷 → Keep(即使空位/重名怎样)
        assert!(matches!(
            decide(false, Some(50), true, 3),
            ReleaseAction::Keep { .. }
        ));
        // ② 空位无法评估 → Keep
        assert!(matches!(
            decide(true, None, false, 3),
            ReleaseAction::Keep { .. }
        ));
        // ③ 空位为 0 → Keep(整箱搬出也要 1 格)
        assert!(matches!(
            decide(true, Some(0), true, 3),
            ReleaseAction::Keep { .. }
        ));
        // ④ 重名 → MoveFolder(即使空位充足)
        assert!(matches!(
            decide(true, Some(50), true, 3),
            ReleaseAction::MoveFolder { .. }
        ));
        // ⑤ 空位不足 → MoveFolder(带 几/几 原因)
        let action = decide(true, Some(2), false, 3);
        match action {
            ReleaseAction::MoveFolder { reason } => assert!(reason.contains("2/3")),
            other => panic!("expected MoveFolder, got {other:?}"),
        }
        // 空位恰好等于条目数 → 放得下
        assert_eq!(decide(true, Some(3), false, 3), ReleaseAction::Release);
        // 富余 → Release
        assert_eq!(decide(true, Some(50), false, 3), ReleaseAction::Release);
    }

    #[test]
    fn in_use_detects_references() {
        let folder = PathBuf::from(r"C:\cfg\boxes\B1");
        let vault = PathBuf::from(r"C:\cfg\vault");
        let dl = PathBuf::from(r"C:\cfg\boxes\下载收纳箱");
        let b2 = Some(PathBuf::from(r"C:\cfg\boxes\B2"));

        // 无人引用
        assert!(!in_use_refs(&folder, &[(9, b2.clone())], &[], &vault, &dl));
        // 其余栅栏指向同一目录 → 在用
        assert!(in_use_refs(&folder, &[(9, Some(folder.clone()))], &[], &vault, &dl));
        // 其余栅栏指向本目录之下 → 在用(搬走即失联)
        assert!(in_use_refs(
            &folder,
            &[(9, Some(folder.join("sub")))],
            &[],
            &vault,
            &dl
        ));
        // folder=None 折算 vault:vault≠folder → 不算引用
        assert!(!in_use_refs(&folder, &[(9, None)], &[], &vault, &dl));
        // 本栅栏自己也是 folder=None(有效目录就是 vault)→ 除自己外还有 None 栅栏则在用
        assert!(in_use_refs(&vault, &[(9, None)], &[], &vault, &dl));
        // sweep 目标等于/位于本目录 → 在用;指向别处 → 不算
        assert!(in_use_refs(&folder, &[], &[folder.clone()], &vault, &dl));
        assert!(in_use_refs(
            &folder,
            &[],
            &[folder.join("sub")],
            &vault,
            &dl
        ));
        assert!(!in_use_refs(
            &folder,
            &[],
            &[PathBuf::from(r"C:\cfg\boxes\B2")],
            &vault,
            &dl
        ));
        // 目录名前缀不是路径前缀:B1 不"在"B11 之下
        assert!(!in_use_refs(
            Path::new(r"C:\cfg\boxes\B1"),
            &[(9, Some(PathBuf::from(r"C:\cfg\boxes\B11")))],
            &[],
            &vault,
            &dl
        ));
        // 下载箱目录永不释放
        assert!(in_use_refs(&dl, &[], &[], &vault, &dl));
    }

    #[test]
    fn owned_folder_gates_on_boxes_and_default_vault() {
        let config_dir = Path::new(r"C:\cfg");
        let default_vault = Path::new(r"C:\cfg\vault");
        let mut cfg = FenceCfg {
            kind: FenceKind::Collection,
            folder: None,
            ..FenceCfg::default()
        };

        // 默认 vault(配置目录内)→ 自管
        assert_eq!(
            owned_folder_in(&cfg, config_dir, default_vault),
            Some(default_vault.to_path_buf())
        );
        // vault 改到外部 → 不自管(只解绑)
        assert_eq!(owned_folder_in(&cfg, config_dir, Path::new(r"D:\data")), None);

        // boxes 子目录 → 自管
        cfg.folder = Some(PathBuf::from(r"C:\cfg\boxes\收纳箱"));
        assert_eq!(
            owned_folder_in(&cfg, config_dir, default_vault),
            cfg.folder.clone()
        );
        // boxes 根自身 → 不自管(防连锅端)
        cfg.folder = Some(PathBuf::from(r"C:\cfg\boxes"));
        assert_eq!(owned_folder_in(&cfg, config_dir, default_vault), None);
        // 外部目录 → 不自管
        cfg.folder = Some(PathBuf::from(r"D:\docs"));
        assert_eq!(owned_folder_in(&cfg, config_dir, default_vault), None);

        // 非 Collection 即使指在 boxes 里也只解绑
        cfg.kind = FenceKind::Portal;
        cfg.folder = Some(PathBuf::from(r"C:\cfg\boxes\收纳箱"));
        assert_eq!(owned_folder_in(&cfg, config_dir, default_vault), None);
    }

    #[test]
    fn collision_check_is_case_insensitive() {
        let names: HashSet<String> = ["app.lnk", "notes.txt"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let dir = std::env::temp_dir().join(format!(
            "feather-fences-release-{}-{}",
            std::process::id(),
            nonce()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("APP.LNK"), b"x").unwrap();
        std::fs::write(dir.join("other.txt"), b"x").unwrap();

        let entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(has_collision(&entries, &names)); // APP.LNK ↔ app.lnk
        assert!(!has_collision(&entries, &HashSet::new()));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dialog_text_covers_all_actions() {
        let base = Plan {
            title: "工具".into(),
            folder: PathBuf::from(r"C:\cfg\boxes\收纳箱"),
            n: 3,
            shortcuts: 2,
            action: ReleaseAction::Release,
        };
        assert!(base.dialog_text().contains("3 个项目"));
        assert!(base.dialog_text().contains("2 个快捷方式"));

        let mv = Plan {
            action: ReleaseAction::MoveFolder {
                reason: "桌面上存在同名文件".into(),
            },
            ..Plan {
                title: "工具".into(),
                folder: PathBuf::from(r"C:\cfg\boxes\收纳箱"),
                n: 3,
                shortcuts: 2,
                action: ReleaseAction::Release,
            }
        };
        assert!(mv.dialog_text().contains("同名文件"));
        assert!(mv.dialog_text().contains("搬到桌面"));

        let keep = Plan {
            action: ReleaseAction::Keep {
                reason: "无法评估桌面空间".into(),
            },
            ..mv // 借结构,值不重要
        };
        assert!(keep.dialog_text().contains("保留在"));
        assert!(keep.dialog_text().contains("收纳箱"));
    }
}
