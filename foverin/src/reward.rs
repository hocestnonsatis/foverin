//! Balanced reward from CPU busy, thermal pressure, and optional RAPL energy.

use std::{
    fs,
    path::{Path, PathBuf},
};

const T_COMFORT_MC: f32 = 70_000.0; // millidegrees C
const T_CRIT_MC: f32 = 95_000.0;

/// Snapshot of `/proc/stat` aggregate CPU counters.
#[derive(Debug, Clone, Copy, Default)]
struct CpuTimes {
    idle: u64,
    total: u64,
}

/// Optional RAPL energy reading (microjoules).
#[derive(Debug, Clone, Copy, Default)]
struct RaplSample {
    energy_uj: u64,
}

/// Samples system metrics between policy ticks for credit assignment.
#[derive(Debug, Default)]
pub struct MetricSampler {
    prev_cpu: Option<CpuTimes>,
    prev_rapl: Option<RaplSample>,
}

/// Metrics observed over one aggregation window.
#[derive(Debug, Clone, Copy)]
pub struct WindowMetrics {
    /// Fraction of CPU that was busy in `[0, 1]`.
    pub busy: f32,
    /// Thermal penalty in `[0, 1]` (0 = cool / unknown).
    pub thermal_pen: f32,
    /// Normalized energy rate proxy in `[0, 1]` when RAPL is available, else `-1`.
    pub energy_rate_norm: f32,
}

impl MetricSampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read current counters and return metrics since the previous call.
    ///
    /// The first call establishes a baseline and returns a neutral sample
    /// (`busy = 0.5`, no penalties) so credit assignment can skip it.
    pub fn sample(&mut self) -> WindowMetrics {
        let cpu = read_cpu_times().unwrap_or_default();
        let temp_mc = read_max_thermal_mc();
        let rapl = read_rapl_energy();

        let (busy, energy_rate_norm) = match self.prev_cpu {
            None => {
                self.prev_cpu = Some(cpu);
                self.prev_rapl = rapl;
                (0.5, -1.0)
            }
            Some(prev) => {
                let busy = busy_fraction(prev, cpu);
                let energy = match (self.prev_rapl, rapl) {
                    (Some(p), Some(c)) if c.energy_uj >= p.energy_uj => {
                        let delta = (c.energy_uj - p.energy_uj) as f32;
                        // ~50 J / 5s window ≈ high; normalize loosely.
                        (delta / 50_000_000.0).clamp(0.0, 1.0)
                    }
                    _ => -1.0,
                };
                self.prev_cpu = Some(cpu);
                self.prev_rapl = rapl;
                (busy, energy)
            }
        };

        let thermal_pen = match temp_mc {
            Some(t) => ((t - T_COMFORT_MC) / (T_CRIT_MC - T_COMFORT_MC)).clamp(0.0, 1.0),
            None => 0.0,
        };

        WindowMetrics {
            busy,
            thermal_pen,
            energy_rate_norm,
        }
    }
}

/// Governor "performance score" used for alignment / power proxies.
pub fn gov_perf_score(governor: &str) -> f32 {
    match governor {
        "performance" => 1.0,
        "schedutil" | "ondemand" | "conservative" => 0.5,
        "powersave" => 0.0,
        _ => 0.5,
    }
}

/// Balanced reward in `[0, 1]`.
///
/// ```text
/// reward = 0.45 * align + 0.35 * (1 - power_pen) + 0.20 * (1 - thermal_pen)
/// ```
pub fn balanced_reward(metrics: WindowMetrics, governor: &str) -> f32 {
    let gov_perf = gov_perf_score(governor);
    let align = 1.0 - (metrics.busy - gov_perf).abs();

    let power_pen = if metrics.energy_rate_norm >= 0.0 {
        // High energy while idle → waste; low energy under load → underpowered.
        let waste = metrics.energy_rate_norm * (1.0 - metrics.busy);
        let starve = (1.0 - metrics.energy_rate_norm) * metrics.busy * 0.5;
        (waste + starve).clamp(0.0, 1.0)
    } else if gov_perf >= 0.9 {
        // Performance while mostly idle.
        (1.0 - metrics.busy).clamp(0.0, 1.0)
    } else if gov_perf <= 0.1 {
        // Powersave under heavy load — light penalty.
        ((metrics.busy - 0.4).max(0.0) * 0.8).clamp(0.0, 1.0)
    } else {
        // schedutil-like: mild mismatch only.
        ((metrics.busy - gov_perf).abs() * 0.3).clamp(0.0, 1.0)
    };

    let reward = 0.45 * align + 0.35 * (1.0 - power_pen) + 0.20 * (1.0 - metrics.thermal_pen);
    reward.clamp(0.0, 1.0)
}

fn busy_fraction(prev: CpuTimes, cur: CpuTimes) -> f32 {
    let d_total = cur.total.saturating_sub(prev.total);
    let d_idle = cur.idle.saturating_sub(prev.idle);
    if d_total == 0 {
        return 0.5;
    }
    let idle_frac = d_idle as f32 / d_total as f32;
    (1.0 - idle_frac).clamp(0.0, 1.0)
}

fn read_cpu_times() -> Option<CpuTimes> {
    let data = fs::read_to_string("/proc/stat").ok()?;
    let line = data.lines().find(|l| l.starts_with("cpu "))?;
    let mut parts = line.split_whitespace().skip(1);
    // user nice system idle iowait irq softirq steal guest guest_nice
    let user: u64 = parts.next()?.parse().ok()?;
    let nice: u64 = parts.next()?.parse().ok()?;
    let system: u64 = parts.next()?.parse().ok()?;
    let idle: u64 = parts.next()?.parse().ok()?;
    let iowait: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let irq: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let softirq: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let steal: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let idle_all = idle + iowait;
    let total = user + nice + system + idle_all + irq + softirq + steal;
    Some(CpuTimes {
        idle: idle_all,
        total,
    })
}

fn read_max_thermal_mc() -> Option<f32> {
    let root = Path::new("/sys/class/thermal");
    let entries = fs::read_dir(root).ok()?;
    let mut max_t: Option<f32> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let temp_path = entry.path().join("temp");
        if let Ok(s) = fs::read_to_string(&temp_path)
            && let Ok(v) = s.trim().parse::<f32>()
        {
            max_t = Some(max_t.map_or(v, |m| m.max(v)));
        }
    }
    max_t
}

fn read_rapl_energy() -> Option<RaplSample> {
    // Prefer package RAPL; fall back to first intel-rapl:* node.
    let candidates = [
        "/sys/class/powercap/intel-rapl:0/energy_uj",
        "/sys/class/powercap/intel-rapl/intel-rapl:0/energy_uj",
    ];
    for path in candidates {
        if let Ok(s) = fs::read_to_string(path)
            && let Ok(v) = s.trim().parse::<u64>()
        {
            return Some(RaplSample { energy_uj: v });
        }
    }
    // Scan powercap for any energy_uj.
    let root = PathBuf::from("/sys/class/powercap");
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let energy = entry.path().join("energy_uj");
            if let Ok(s) = fs::read_to_string(&energy)
                && let Ok(v) = s.trim().parse::<u64>()
            {
                return Some(RaplSample { energy_uj: v });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reward_clamps_and_prefers_align() {
        let cool_busy = WindowMetrics {
            busy: 0.95,
            thermal_pen: 0.0,
            energy_rate_norm: -1.0,
        };
        let r_perf = balanced_reward(cool_busy, "performance");
        let r_save = balanced_reward(cool_busy, "powersave");
        assert!(r_perf > r_save);
        assert!((0.0..=1.0).contains(&r_perf));

        let idle = WindowMetrics {
            busy: 0.05,
            thermal_pen: 0.0,
            energy_rate_norm: -1.0,
        };
        let r_perf_idle = balanced_reward(idle, "performance");
        let r_save_idle = balanced_reward(idle, "powersave");
        assert!(r_save_idle > r_perf_idle);
    }

    #[test]
    fn thermal_penalty_lowers_reward() {
        let base = WindowMetrics {
            busy: 0.5,
            thermal_pen: 0.0,
            energy_rate_norm: -1.0,
        };
        let hot = WindowMetrics {
            busy: 0.5,
            thermal_pen: 1.0,
            energy_rate_norm: -1.0,
        };
        assert!(balanced_reward(base, "schedutil") > balanced_reward(hot, "schedutil"));
    }

    #[test]
    fn gov_perf_scores() {
        assert_eq!(gov_perf_score("performance"), 1.0);
        assert_eq!(gov_perf_score("powersave"), 0.0);
        assert_eq!(gov_perf_score("schedutil"), 0.5);
    }
}
