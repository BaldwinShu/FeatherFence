use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static ENABLED: OnceLock<bool> = OnceLock::new();
static SAFE_DESKTOP: OnceLock<bool> = OnceLock::new();
static LOG_TX: OnceLock<Sender<String>> = OnceLock::new();
static ANIMATIONS: OnceLock<Mutex<HashMap<u32, AnimationSession>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default)]
pub struct RenderSample {
    pub width: i32,
    pub height: i32,
    pub entries: usize,
    pub ensure_cache: Duration,
    pub clear: Duration,
    pub gdi_plus: Duration,
    pub icon_hits: u64,
    pub icon_misses: u64,
    pub icon_hit_time: Duration,
    pub icon_miss_time: Duration,
    pub label_count: u64,
    pub label_time: Duration,
    pub gdi_icons: Duration,
    pub premultiply: Duration,
    pub update_layered_window: Duration,
    pub total: Duration,
}

struct AnimationSession {
    started: Instant,
    last_frame: Option<Instant>,
    frame_times: Vec<Duration>,
    frame_intervals: Vec<Duration>,
    stage_totals: RenderSample,
}

impl AnimationSession {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last_frame: None,
            frame_times: Vec::with_capacity(16),
            frame_intervals: Vec::with_capacity(16),
            stage_totals: RenderSample::default(),
        }
    }

    fn push(&mut self, sample: RenderSample, now: Instant) {
        if let Some(previous) = self.last_frame.replace(now) {
            self.frame_intervals
                .push(now.saturating_duration_since(previous));
        }
        self.frame_times.push(sample.total);
        add_render_sample(&mut self.stage_totals, sample);
    }
}

pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| env_flag("FEATHER_PERF"))
}

/// 性能测试时可关闭桌面图标避让，避免测试窗口改变用户真实桌面布局。
pub fn safe_desktop() -> bool {
    enabled() && *SAFE_DESKTOP.get_or_init(|| env_flag("FEATHER_PERF_SAFE_DESKTOP"))
}

pub fn animation_fence_id() -> Option<u32> {
    enabled()
        .then(|| std::env::var("FEATHER_PERF_ANIMATE_FENCE").ok())
        .flatten()
        .and_then(|value| value.parse().ok())
}

pub fn animation_repeats() -> u32 {
    if !enabled() {
        return 1;
    }
    std::env::var("FEATHER_PERF_ANIMATION_REPEATS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1)
        .clamp(1, 10)
}

pub fn init() {
    if enabled() {
        emit(format!(
            "[perf][session] pid={} profiling=enabled",
            std::process::id()
        ));
    }
}

pub fn begin_animation(fence_id: u32) {
    if !enabled() {
        return;
    }
    animations()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(fence_id, AnimationSession::new());
}

pub fn record_refresh(
    fence_id: u32,
    dir: &Path,
    entries: usize,
    read: Duration,
    sort: Duration,
    total: Duration,
    succeeded: bool,
) {
    if !enabled() {
        return;
    }
    emit(format!(
        "[perf][refresh] fence={fence_id} entries={entries} read_us={} sort_us={} total_us={} ok={succeeded} dir={}",
        micros(read),
        micros(sort),
        micros(total),
        dir.display()
    ));
}

pub fn record_render(fence_id: u32, animating: bool, sample: RenderSample) {
    if !enabled() {
        return;
    }

    let now = Instant::now();
    let (completed, belongs_to_animation) = {
        let mut sessions = animations()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(session) = sessions.get_mut(&fence_id) {
            session.push(sample, now);
            if animating {
                (None, true)
            } else {
                (sessions.remove(&fence_id), true)
            }
        } else {
            (None, false)
        }
    };

    if let Some(session) = completed {
        emit_animation(fence_id, session);
    } else if !belongs_to_animation {
        emit_render("render", fence_id, sample);
    }
}

fn emit_render(kind: &str, fence_id: u32, sample: RenderSample) {
    emit(format!(
        "[perf][{kind}] fence={fence_id} size={}x{} entries={} total_us={} cpu_draw_us={} gdi_plus_wall_us={} labels={} label_us={} label_avg_us={} gdi_icons_us={} icon_hits={} icon_misses={} icon_hit_us={} icon_miss_us={} clear_us={} premultiply_us={} ulw_us={} cache_us={}",
        sample.width,
        sample.height,
        sample.entries,
        micros(sample.total),
        micros(cpu_draw_time(sample)),
        micros(sample.gdi_plus),
        sample.label_count,
        micros(sample.label_time),
        average_for_count(sample.label_time, sample.label_count),
        micros(sample.gdi_icons),
        sample.icon_hits,
        sample.icon_misses,
        micros(sample.icon_hit_time),
        micros(sample.icon_miss_time),
        micros(sample.clear),
        micros(sample.premultiply),
        micros(sample.update_layered_window),
        micros(sample.ensure_cache),
    ));
}

fn emit_animation(fence_id: u32, session: AnimationSession) {
    let frames = session.frame_times.len();
    if frames == 0 {
        return;
    }
    let total_render: Duration = session.frame_times.iter().copied().sum();
    let over_budget = session
        .frame_times
        .iter()
        .filter(|duration| **duration > Duration::from_millis(16))
        .count();
    let avg = total_render / frames as u32;
    let frame_p95 = percentile(&session.frame_times, 95);
    let frame_max = session
        .frame_times
        .iter()
        .copied()
        .max()
        .unwrap_or_default();
    let interval_avg = average(&session.frame_intervals);
    let interval_p95 = percentile(&session.frame_intervals, 95);
    let interval_max = session
        .frame_intervals
        .iter()
        .copied()
        .max()
        .unwrap_or_default();
    let elapsed = session.started.elapsed();
    let stages = session.stage_totals;

    emit(format!(
        "[perf][animation] fence={fence_id} frames={frames} elapsed_ms={} frame_avg_us={} frame_p95_us={} frame_max_us={} over_16ms={} interval_avg_us={} interval_p95_us={} interval_max_us={} cpu_draw_avg_us={} gdi_plus_wall_avg_us={} labels_total={} label_avg_frame_us={} label_avg_item_us={} gdi_icons_avg_us={} icon_hits_total={} icon_miss_total={} icon_miss_avg_us={} premultiply_avg_us={} ulw_avg_us={}",
        elapsed.as_millis(),
        micros(avg),
        micros(frame_p95),
        micros(frame_max),
        over_budget,
        micros(interval_avg),
        micros(interval_p95),
        micros(interval_max),
        micros(cpu_draw_time(stages) / frames as u32),
        micros(stages.gdi_plus / frames as u32),
        stages.label_count,
        micros(stages.label_time / frames as u32),
        average_for_count(stages.label_time, stages.label_count),
        micros(stages.gdi_icons / frames as u32),
        stages.icon_hits,
        stages.icon_misses,
        average_for_count(stages.icon_miss_time, stages.icon_misses),
        micros(stages.premultiply / frames as u32),
        micros(stages.update_layered_window / frames as u32),
    ));
}

fn cpu_draw_time(sample: RenderSample) -> Duration {
    let icon_lookup = sample.icon_hit_time + sample.icon_miss_time;
    sample.clear
        + sample.gdi_plus.saturating_sub(icon_lookup)
        + sample.gdi_icons
        + sample.premultiply
}

fn add_render_sample(total: &mut RenderSample, sample: RenderSample) {
    total.width = sample.width;
    total.height = sample.height;
    total.entries = sample.entries;
    total.ensure_cache += sample.ensure_cache;
    total.clear += sample.clear;
    total.gdi_plus += sample.gdi_plus;
    total.icon_hits += sample.icon_hits;
    total.icon_misses += sample.icon_misses;
    total.icon_hit_time += sample.icon_hit_time;
    total.icon_miss_time += sample.icon_miss_time;
    total.label_count += sample.label_count;
    total.label_time += sample.label_time;
    total.gdi_icons += sample.gdi_icons;
    total.premultiply += sample.premultiply;
    total.update_layered_window += sample.update_layered_window;
    total.total += sample.total;
}

fn average(values: &[Duration]) -> Duration {
    if values.is_empty() {
        Duration::default()
    } else {
        values.iter().copied().sum::<Duration>() / values.len() as u32
    }
}

fn percentile(values: &[Duration], percentile: usize) -> Duration {
    if values.is_empty() {
        return Duration::default();
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() - 1) * percentile).div_ceil(100);
    sorted[index]
}

fn average_for_count(total: Duration, count: u64) -> u128 {
    if count == 0 {
        0
    } else {
        total.as_micros() / count as u128
    }
}

fn micros(duration: Duration) -> u128 {
    duration.as_micros()
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn animations() -> &'static Mutex<HashMap<u32, AnimationSession>> {
    ANIMATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn emit(message: String) {
    if let Some(sender) = log_sender() {
        let _ = sender.send(message);
    }
}

fn log_sender() -> Option<&'static Sender<String>> {
    if !enabled() {
        return None;
    }
    Some(LOG_TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<String>();
        let log_dir = crate::config::config_dir();
        let _ = thread::Builder::new()
            .name("feather-perf-log".into())
            .spawn(move || {
                let _ = std::fs::create_dir_all(&log_dir);
                let path = log_dir.join("perf.log");
                let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
                    return;
                };
                for message in rx {
                    let timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis();
                    let _ = writeln!(file, "{timestamp} {message}");
                }
            });
        tx
    }))
}

#[cfg(test)]
mod tests {
    use super::{average, percentile};
    use std::time::Duration;

    #[test]
    fn percentile_uses_nearest_rank_and_handles_empty_input() {
        let values = [1, 2, 3, 4, 100].map(Duration::from_millis);
        assert_eq!(percentile(&values, 95), Duration::from_millis(100));
        assert_eq!(percentile(&values, 50), Duration::from_millis(3));
        assert_eq!(percentile(&[], 95), Duration::default());
    }

    #[test]
    fn average_handles_empty_input() {
        let values = [Duration::from_millis(2), Duration::from_millis(4)];
        assert_eq!(average(&values), Duration::from_millis(3));
        assert_eq!(average(&[]), Duration::default());
    }
}
