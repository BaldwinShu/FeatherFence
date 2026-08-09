// 配置:JSON 持久化,位于 %APPDATA%\feather-fences\config.json
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FenceCfg {
    pub id: u32,
    pub title: String,
    /// None = 收纳栅栏(空投区,拖入的文件移动到 vault)
    pub folder: Option<PathBuf>,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    /// 图标尺寸(旧版存于栅栏上;现由 Config.icon 全局统一。保留字段仅用于一次性迁移)
    #[serde(default = "default_icon")]
    pub icon: u32,
}

fn default_opacity() -> f32 {
    0.74
}

fn default_icon() -> u32 {
    32
}

impl Default for FenceCfg {
    fn default() -> Self {
        FenceCfg {
            id: 0,
            title: "栅栏".into(),
            folder: None,
            x: 0,
            y: 0,
            w: 260,
            h: 340,
            opacity: default_opacity(),
            icon: default_icon(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SweepRule {
    /// 小写带点扩展名,如 ".jpg"
    pub ext: String,
    pub dest: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(default)]
    pub fences: Vec<FenceCfg>,
    #[serde(default)]
    pub sweep_rules: Vec<SweepRule>,
    #[serde(default)]
    pub ghost_mode: bool,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub vault_dir: Option<PathBuf>,
    /// 全局图标尺寸(逻辑像素,默认 32)
    #[serde(default = "default_icon")]
    pub icon: u32,
    /// 配置格式版本:>=2 表示栅栏 x/y/w/h 存逻辑像素(读写边界按 DPI 转换)。
    /// 缺省/1 = 旧版物理像素,加载后由 normalize_dpi 一次性迁移。
    #[serde(default)]
    pub version: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            fences: Vec::new(),
            sweep_rules: Vec::new(),
            ghost_mode: false,
            autostart: false,
            vault_dir: None,
            icon: default_icon(),
            version: 2,
        }
    }
}

/// 把磁盘配置转成本会话的物理像素布局:
/// - 旧版(version < 2)配置是物理像素 → 原样保留(下次保存转逻辑像素,一次性迁移,现有布局零变化)。
/// - v2+ 配置是逻辑像素 → 按当前系统 DPI 乘回物理像素。
/// 调用点:进程启动 load() 之后、MENU_RELOAD 之后。
pub fn normalize_dpi(c: &mut Config) {
    if c.version >= 2 {
        let s = crate::fence::dpi_scale();
        if s != 1.0 {
            for f in &mut c.fences {
                f.x = (f.x as f32 * s).round() as i32;
                f.y = (f.y as f32 * s).round() as i32;
                f.w = (f.w as f32 * s).round() as i32;
                f.h = (f.h as f32 * s).round() as i32;
            }
        }
    }
    c.version = 2;
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("feather-fences")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn default_vault_dir() -> PathBuf {
    config_dir().join("vault")
}

pub fn vault_dir(c: &Config) -> PathBuf {
    c.vault_dir.clone().unwrap_or_else(default_vault_dir)
}

pub fn load() -> Config {
    let p = config_path();
    if let Ok(s) = fs::read_to_string(&p) {
        if let Ok(c) = serde_json::from_str::<Config>(&s) {
            return c;
        }
    }
    Config::default()
}

pub fn save(c: &Config) {
    if let Err(e) = fs::create_dir_all(config_dir()) {
        eprintln!("[feather] mkdir config dir failed: {e}");
        return;
    }
    match serde_json::to_string_pretty(c) {
        Ok(s) => {
            if let Err(e) = fs::write(config_path(), s) {
                eprintln!("[feather] save config failed: {e}");
            }
        }
        Err(e) => eprintln!("[feather] serialize config failed: {e}"),
    }
}

/// 确保目标目录存在,返回是否成功
pub fn ensure_dir(p: &Path) -> bool {
    if p.exists() {
        return p.is_dir();
    }
    fs::create_dir_all(p).is_ok()
}
