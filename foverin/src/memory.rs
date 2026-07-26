//! Episodic bucket memory with UCB1 action selection.
//!
//! Context fingerprints map to buckets; each bucket tracks per-governor
//! visit counts and EMA rewards. Selection is UCB1; updates are O(1) EMA
//! — no neural backprop.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// Default on-disk memory path (written/loaded by the daemon).
pub const DEFAULT_MEMORY_PATH: &str = "foverin_memory.json";

const MAX_BUCKETS: usize = 4096;
const UCB_C: f32 = 0.7;
const EMA_ALPHA: f32 = 0.2;
const SAVE_EVERY: u64 = 12; // ~60s at 5s ticks

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActionStats {
    visits: u32,
    ema_reward: f32,
}

impl Default for ActionStats {
    fn default() -> Self {
        Self {
            visits: 0,
            ema_reward: 0.5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bucket {
    total_visits: u32,
    last_used: u64,
    actions: HashMap<String, ActionStats>,
}

impl Bucket {
    fn new(tick: u64) -> Self {
        Self {
            total_visits: 0,
            last_used: tick,
            actions: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct MemoryFile {
    tick: u64,
    buckets: HashMap<String, Bucket>,
}

/// Persistent episodic store: fingerprint → governor stats.
#[derive(Debug)]
pub struct EpisodeMemory {
    buckets: HashMap<u64, Bucket>,
    actions: Vec<String>,
    tick: u64,
    path: PathBuf,
    dirty_since_save: u64,
}

/// Result of a UCB selection.
#[derive(Debug, Clone)]
pub struct Selection {
    pub governor: String,
    /// Confidence percentage (0–100) derived from visits / UCB margin.
    pub confidence: f32,
}

impl EpisodeMemory {
    pub fn open(path: impl Into<PathBuf>, actions: Vec<String>) -> Result<Self> {
        let path = path.into();
        let mut mem = if path.is_file() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("read memory {}", path.display()))?;
            let file: MemoryFile = serde_json::from_str(&raw)
                .with_context(|| format!("parse memory {}", path.display()))?;
            let mut buckets = HashMap::with_capacity(file.buckets.len());
            for (k, v) in file.buckets {
                let key: u64 = k.parse().with_context(|| format!("bucket key `{k}`"))?;
                buckets.insert(key, v);
            }
            Self {
                buckets,
                actions,
                tick: file.tick,
                path,
                dirty_since_save: 0,
            }
        } else {
            Self {
                buckets: HashMap::new(),
                actions,
                tick: 0,
                path,
                dirty_since_save: 0,
            }
        };
        if mem.actions.is_empty() {
            mem.actions = vec!["performance".into(), "powersave".into()];
        }
        Ok(mem)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn actions(&self) -> &[String] {
        &self.actions
    }

    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// UCB1 select a governor for `fingerprint`.
    ///
    /// `prior_governor` seeds unseen actions (cold-start bias) without writing.
    pub fn select(&mut self, fingerprint: u64, prior_governor: &str) -> Selection {
        self.tick = self.tick.saturating_add(1);
        if !self.buckets.contains_key(&fingerprint) {
            self.evict_if_needed();
        }

        let bucket = self
            .buckets
            .entry(fingerprint)
            .or_insert_with(|| Bucket::new(self.tick));
        bucket.last_used = self.tick;

        let n = bucket.total_visits.max(1) as f32;
        let ln_n = (n + 1.0).ln();

        let mut best_gov = self.actions[0].clone();
        let mut best_ucb = f32::NEG_INFINITY;
        let mut second_ucb = f32::NEG_INFINITY;
        let mut best_visits = 0u32;

        for action in &self.actions {
            let stats = bucket.actions.get(action);
            let (visits, ema) = match stats {
                Some(s) => (s.visits, s.ema_reward),
                None => {
                    let prior = if action == prior_governor { 0.72 } else { 0.45 };
                    (0, prior)
                }
            };
            let bonus = UCB_C * (ln_n / (visits as f32 + 1.0)).sqrt();
            let ucb = ema + bonus;
            if ucb > best_ucb {
                second_ucb = best_ucb;
                best_ucb = ucb;
                best_gov = action.clone();
                best_visits = visits;
            } else if ucb > second_ucb {
                second_ucb = ucb;
            }
        }

        let margin = if second_ucb.is_finite() {
            (best_ucb - second_ucb).max(0.0)
        } else {
            1.0
        };
        let visit_conf = (best_visits as f32 * 5.0).min(100.0);
        let margin_conf = (margin * 80.0).min(100.0);
        let confidence = visit_conf.max(margin_conf).clamp(5.0, 100.0);

        Selection {
            governor: best_gov,
            confidence,
        }
    }

    /// Credit `(fingerprint, governor)` with a `[0, 1]` reward.
    pub fn update(&mut self, fingerprint: u64, governor: &str, reward: f32) {
        let reward = reward.clamp(0.0, 1.0);
        if !self.buckets.contains_key(&fingerprint) {
            self.evict_if_needed();
        }

        let tick = self.tick;
        let bucket = self
            .buckets
            .entry(fingerprint)
            .or_insert_with(|| Bucket::new(tick));
        bucket.last_used = tick;
        bucket.total_visits = bucket.total_visits.saturating_add(1);

        let stats = bucket.actions.entry(governor.to_string()).or_default();
        if stats.visits == 0 {
            stats.ema_reward = reward;
        } else {
            stats.ema_reward = (1.0 - EMA_ALPHA) * stats.ema_reward + EMA_ALPHA * reward;
        }
        stats.visits = stats.visits.saturating_add(1);

        self.dirty_since_save = self.dirty_since_save.saturating_add(1);
        if self.dirty_since_save >= SAVE_EVERY {
            let _ = self.save();
        }
    }

    pub fn save(&mut self) -> Result<()> {
        let mut file = MemoryFile {
            tick: self.tick,
            buckets: HashMap::with_capacity(self.buckets.len()),
        };
        for (k, v) in &self.buckets {
            file.buckets.insert(k.to_string(), v.clone());
        }
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(&file).context("serialize memory")?;
        fs::write(&self.path, json)
            .with_context(|| format!("write memory {}", self.path.display()))?;
        self.dirty_since_save = 0;
        Ok(())
    }

    fn evict_if_needed(&mut self) {
        while self.buckets.len() >= MAX_BUCKETS {
            let victim = self
                .buckets
                .iter()
                .min_by(|(_, a), (_, b)| {
                    a.total_visits
                        .cmp(&b.total_visits)
                        .then_with(|| a.last_used.cmp(&b.last_used))
                })
                .map(|(k, _)| *k);
            if let Some(k) = victim {
                self.buckets.remove(&k);
            } else {
                break;
            }
        }
    }
}

/// Resolve where to load/save memory (env override → CWD → next to exe).
pub fn resolve_memory_path() -> PathBuf {
    if let Ok(p) = std::env::var("FOVERIN_MEMORY") {
        return PathBuf::from(p);
    }
    let cwd = PathBuf::from(DEFAULT_MEMORY_PATH);
    if cwd.is_file() {
        return cwd;
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(DEFAULT_MEMORY_PATH);
        if sibling.is_file() {
            return sibling;
        }
    }
    cwd
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("foverin-mem-{name}-{nanos}.json"))
    }

    #[test]
    fn ucb_explores_unseen_then_exploits() {
        let path = tmp_path("ucb");
        let actions = vec!["performance".into(), "schedutil".into(), "powersave".into()];
        let mut mem = EpisodeMemory::open(&path, actions).unwrap();

        // Strong rewards for performance on fingerprint 1.
        for _ in 0..20 {
            let sel = mem.select(1, "performance");
            mem.update(
                1,
                &sel.governor,
                if sel.governor == "performance" {
                    0.95
                } else {
                    0.1
                },
            );
        }
        let sel = mem.select(1, "performance");
        assert_eq!(sel.governor, "performance");
        assert!(sel.confidence >= 5.0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn eviction_caps_buckets() {
        let path = tmp_path("evict");
        let mut mem = EpisodeMemory::open(&path, vec!["performance".into()]).unwrap();
        for i in 0..(MAX_BUCKETS + 10) as u64 {
            let _ = mem.select(i, "performance");
            mem.update(i, "performance", 0.5);
        }
        assert!(mem.bucket_count() <= MAX_BUCKETS);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn persist_roundtrip() {
        let path = tmp_path("persist");
        {
            let mut mem =
                EpisodeMemory::open(&path, vec!["performance".into(), "powersave".into()]).unwrap();
            mem.update(42, "performance", 0.8);
            mem.save().unwrap();
        }
        let mem =
            EpisodeMemory::open(&path, vec!["performance".into(), "powersave".into()]).unwrap();
        assert!(mem.buckets.contains_key(&42));
        let _ = fs::remove_file(path);
    }
}
