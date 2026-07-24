//! Workload fingerprinting and soft labels for the episodic memory policy.
//!
//! Process windows are multi-hot encoded against a static Linux-process
//! vocabulary. The bitset becomes a bucket fingerprint for UCB memory;
//! `COMPILING|GAMING|BROWSING|IDLE` labels are UI-only heuristics.

use std::time::Instant;

use anyhow::Result;
use foverin_common::ProcessEvent;

use crate::{
    actuator,
    memory::{EpisodeMemory, resolve_memory_path},
    reward::{MetricSampler, balanced_reward},
};

/// Static vocabulary of common Linux process basenames.
/// Order is part of the fingerprint contract — do not reorder after deploy.
pub const VOCAB: &[&str] = &[
    // Build / compile toolchain
    "rustc",
    "cargo",
    "cc",
    "c++",
    "gcc",
    "g++",
    "clang",
    "clang++",
    "make",
    "gmake",
    "ninja",
    "cmake",
    "ld",
    "lld",
    "rust-lld",
    "collect2",
    "as",
    // Gaming
    "steam",
    "steamwebhelper",
    "steamos-logger",
    "csgo",
    "cs2",
    "dota2",
    "proton",
    "proton-run",
    "wine",
    "wine64",
    "wineserver",
    "gamesoverlayui",
    // Browsing / media / light desktop
    "firefox",
    "chrome",
    "chromium",
    "brave",
    "msedge",
    "spotify",
    "code",
    "electron",
    "slack",
    "discord",
    // Shell / system noise (IDLE-leaning)
    "bash",
    "zsh",
    "sh",
    "fish",
    "systemd",
    "sshd",
    "sleep",
    "login",
    "sudo",
    "pacman",
];

const COMPILE_END: usize = 17;
const GAMING_END: usize = 29;
const BROWSING_END: usize = 39;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Workload {
    Compiling = 0,
    Gaming = 1,
    Browsing = 2,
    Idle = 3,
}

impl Workload {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compiling => "COMPILING",
            Self::Gaming => "GAMING",
            Self::Browsing => "BROWSING",
            Self::Idle => "IDLE",
        }
    }

    /// Cold-start governor prior for UCB (not the decision itself).
    pub fn prior_governor(self) -> &'static str {
        match self {
            Self::Compiling | Self::Gaming => "performance",
            Self::Browsing | Self::Idle => actuator::preferred_efficiency_governor(),
        }
    }
}

/// Structured decision produced by the memory policy.
#[derive(Debug, Clone)]
pub struct PolicyDecision {
    pub detected_workload: Workload,
    /// Selection confidence percentage (0–100).
    pub confidence: f32,
    pub reason: String,
    /// Wall-clock decide latency (fingerprint + UCB), µs.
    pub inference_us: u128,
    pub governor: String,
    pub fingerprint: u64,
}

struct PendingCredit {
    fingerprint: u64,
    governor: String,
}

/// Runtime policy: episodic memory + delayed reward credit.
pub struct PolicyEngine {
    memory: EpisodeMemory,
    metrics: MetricSampler,
    pending: Option<PendingCredit>,
    primed: bool,
}

impl PolicyEngine {
    pub fn open() -> Result<Self> {
        let actions = actuator::available_governors();
        let path = resolve_memory_path();
        let memory = EpisodeMemory::open(path, actions)?;
        Ok(Self {
            memory,
            metrics: MetricSampler::new(),
            pending: None,
            primed: false,
        })
    }

    pub fn memory_path(&self) -> &std::path::Path {
        self.memory.path()
    }

    pub fn flush(&mut self) -> Result<()> {
        self.memory.save()
    }

    /// Credit the previous action (if any), then select a governor for `events`.
    pub fn decide(&mut self, events: &[ProcessEvent]) -> PolicyDecision {
        let metrics = self.metrics.sample();
        if self.primed
            && let Some(prev) = self.pending.take()
        {
            let reward = balanced_reward(metrics, &prev.governor);
            self.memory.update(prev.fingerprint, &prev.governor, reward);
        }
        self.primed = true;

        let start = Instant::now();
        let multi_hot = encode_events(events);
        let fingerprint = fingerprint_bits(&multi_hot);
        let workload = soft_label(&multi_hot);
        let prior = resolve_prior(workload, self.memory.actions());
        let selection = self.memory.select(fingerprint, &prior);
        let inference_us = start.elapsed().as_micros();

        let active: Vec<&str> = VOCAB
            .iter()
            .zip(multi_hot.iter())
            .filter(|(_, v)| **v > 0.0)
            .map(|(name, _)| *name)
            .collect();
        let reason = if active.is_empty() {
            format!(
                "memory bucket {fingerprint:#x} → {} (empty / unrecognized window)",
                selection.governor
            )
        } else {
            format!(
                "memory bucket {fingerprint:#x} hits=[{}] → {}",
                active.join(", "),
                selection.governor
            )
        };

        self.pending = Some(PendingCredit {
            fingerprint,
            governor: selection.governor.clone(),
        });

        PolicyDecision {
            detected_workload: workload,
            confidence: selection.confidence,
            reason,
            inference_us,
            governor: selection.governor,
            fingerprint,
        }
    }
}

fn resolve_prior(workload: Workload, actions: &[String]) -> String {
    let want = workload.prior_governor();
    if actions.iter().any(|a| a == want) {
        want.to_string()
    } else {
        actions
            .first()
            .cloned()
            .unwrap_or_else(|| "powersave".into())
    }
}

/// Multi-hot / bag-of-words encoding over [`VOCAB`].
pub fn encode_names(names: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<f32> {
    let mut vec = vec![0.0f32; VOCAB.len()];
    for name in names {
        let base = basename(name.as_ref()).to_ascii_lowercase();
        // Strip common suffixes like `.exe` from Wine/Proton helpers.
        let base = base.strip_suffix(".exe").unwrap_or(&base);
        if let Some(idx) = VOCAB.iter().position(|v| *v == base) {
            vec[idx] = 1.0;
        }
    }
    vec
}

/// Encode a 5-second eBPF process window into a multi-hot vector.
pub fn encode_events(events: &[ProcessEvent]) -> Vec<f32> {
    encode_names(events.iter().map(|e| filename_str(&e.filename)))
}

/// Bitset fingerprint of a multi-hot VOCAB vector (`VOCAB.len() ≤ 64`).
pub fn fingerprint_bits(multi_hot: &[f32]) -> u64 {
    debug_assert!(VOCAB.len() <= 64);
    let mut bits = 0u64;
    for (i, v) in multi_hot.iter().enumerate().take(64) {
        if *v > 0.0 {
            bits |= 1u64 << i;
        }
    }
    bits
}

/// UI-only soft label from VOCAB group hits (priority: compile > gaming > browsing > idle).
pub fn soft_label(multi_hot: &[f32]) -> Workload {
    let hit = |start: usize, end: usize| {
        multi_hot
            .get(start..end)
            .map(|s| s.iter().any(|v| *v > 0.0))
            .unwrap_or(false)
    };
    if hit(0, COMPILE_END) {
        Workload::Compiling
    } else if hit(COMPILE_END, GAMING_END) {
        Workload::Gaming
    } else if hit(GAMING_END, BROWSING_END) {
        Workload::Browsing
    } else {
        Workload::Idle
    }
}

pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub fn filename_str(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_compiling_hits() {
        let v = encode_names(["/usr/bin/cargo", "/usr/bin/rustc"]);
        assert_eq!(v[VOCAB.iter().position(|x| *x == "cargo").unwrap()], 1.0);
        assert_eq!(v[VOCAB.iter().position(|x| *x == "rustc").unwrap()], 1.0);
        assert_eq!(v.iter().filter(|&&x| x > 0.0).count(), 2);
    }

    #[test]
    fn basename_strips_path() {
        assert_eq!(basename("/usr/bin/firefox"), "firefox");
    }

    #[test]
    fn soft_label_priority() {
        let mut v = vec![0.0f32; VOCAB.len()];
        v[VOCAB.iter().position(|x| *x == "firefox").unwrap()] = 1.0;
        assert_eq!(soft_label(&v), Workload::Browsing);
        v[VOCAB.iter().position(|x| *x == "cargo").unwrap()] = 1.0;
        assert_eq!(soft_label(&v), Workload::Compiling);
        let empty = vec![0.0f32; VOCAB.len()];
        assert_eq!(soft_label(&empty), Workload::Idle);
    }

    #[test]
    fn fingerprint_stable() {
        let a = encode_names(["cargo", "rustc"]);
        let b = encode_names(["rustc", "cargo"]);
        assert_eq!(fingerprint_bits(&a), fingerprint_bits(&b));
        assert_ne!(fingerprint_bits(&a), 0);
        assert_eq!(fingerprint_bits(&vec![0.0; VOCAB.len()]), 0);
    }

    #[test]
    fn vocab_fits_u64() {
        assert!(VOCAB.len() <= 64);
        assert_eq!(COMPILE_END, 17);
        assert_eq!(GAMING_END, 29);
        assert_eq!(BROWSING_END, 39);
        assert_eq!(VOCAB.len(), 49);
    }
}
