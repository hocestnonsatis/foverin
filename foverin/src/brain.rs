//! Custom micro-neural network for OS workload classification.
//!
//! Process windows are multi-hot encoded against a static Linux-process
//! vocabulary, then classified by a tiny feed-forward net:
//! `VOCAB → 64 (ReLU) → 32 (ReLU) → 4` (`COMPILING|GAMING|BROWSING|IDLE`).

use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context as _, Result, bail};
use candle_core::{D, DType, Device, Tensor};
use candle_nn::{Linear, Module, VarBuilder, ops};
use foverin_common::ProcessEvent;

/// Default weights path (written by `forge`, loaded by the daemon).
pub const DEFAULT_WEIGHTS_PATH: &str = "foverin_weights.safetensors";

/// Static vocabulary of common Linux process basenames.
/// Order is part of the model contract — do not reorder after training.
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

pub const HIDDEN1: usize = 64;
pub const HIDDEN2: usize = 32;
pub const NUM_CLASSES: usize = 4;

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

    pub fn from_index(idx: usize) -> Result<Self> {
        match idx {
            0 => Ok(Self::Compiling),
            1 => Ok(Self::Gaming),
            2 => Ok(Self::Browsing),
            3 => Ok(Self::Idle),
            _ => bail!("invalid workload class index {idx}"),
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

/// Structured decision produced by the nano-network.
#[derive(Debug, Clone)]
pub struct AiDecision {
    pub detected_workload: Workload,
    /// Softmax confidence of the winning class, as a percentage (0–100).
    pub confidence: f32,
    pub reason: String,
    /// Wall-clock forward-pass duration (encode + infer + argmax), µs.
    pub inference_us: u128,
}

/// Tiny feed-forward classifier shared by Forge (train) and Foverin (infer).
pub struct WorkloadNet {
    fc1: Linear,
    fc2: Linear,
    fc3: Linear,
}

impl WorkloadNet {
    pub fn new(vb: VarBuilder) -> candle_core::Result<Self> {
        Ok(Self {
            fc1: candle_nn::linear(VOCAB.len(), HIDDEN1, vb.pp("fc1"))?,
            fc2: candle_nn::linear(HIDDEN1, HIDDEN2, vb.pp("fc2"))?,
            fc3: candle_nn::linear(HIDDEN2, NUM_CLASSES, vb.pp("fc3"))?,
        })
    }

    pub fn forward(&self, xs: &Tensor) -> candle_core::Result<Tensor> {
        let xs = self.fc1.forward(xs)?.relu()?;
        let xs = self.fc2.forward(&xs)?.relu()?;
        self.fc3.forward(&xs)
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

pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub fn filename_str(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).unwrap_or("?")
}

/// Resolve where to load/save weights (env override → CWD → next to exe).
pub fn resolve_weights_path() -> PathBuf {
    if let Ok(p) = std::env::var("FOVERIN_WEIGHTS") {
        return PathBuf::from(p);
    }
    let cwd = PathBuf::from(DEFAULT_WEIGHTS_PATH);
    if cwd.is_file() {
        return cwd;
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(DEFAULT_WEIGHTS_PATH);
        if sibling.is_file() {
            return sibling;
        }
    }
    cwd
}

/// Loaded inference engine: model + device, ready for sub-millisecond forward passes.
pub struct Classifier {
    model: WorkloadNet,
    device: Device,
}

impl Classifier {
    /// Load `foverin_weights.safetensors` into memory at startup.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let device = Device::Cpu;
        // SAFETY: weights file is trusted (produced by our own `forge` binary).
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[path], DType::F32, &device)
                .with_context(|| format!("mmap safetensors {}", path.display()))?
        };
        let model = WorkloadNet::new(vb).context("construct WorkloadNet from weights")?;
        Ok(Self { model, device })
    }

    /// Encode + forward + softmax + argmax.
    ///
    /// Returns `(workload, confidence_percent)` where confidence is the winning
    /// Softmax probability × 100 (e.g. `98.5`). Target: ≪ 1 ms on CPU.
    pub fn classify_vector(&self, multi_hot: &[f32]) -> Result<(Workload, f32)> {
        let input = Tensor::from_slice(multi_hot, (1, VOCAB.len()), &self.device)
            .context("build input tensor")?;
        let logits = self.model.forward(&input).context("forward pass")?;
        let probs = ops::softmax(&logits, D::Minus1).context("softmax")?;
        let class = probs
            .argmax(D::Minus1)?
            .to_dtype(DType::U32)?
            .to_vec1::<u32>()?[0] as usize;
        let confidence = probs
            .get(0)?
            .get(class)?
            .to_scalar::<f32>()
            .context("read softmax confidence")?
            * 100.0;
        Ok((Workload::from_index(class)?, confidence))
    }

    pub fn classify_events(&self, events: &[ProcessEvent]) -> Result<AiDecision> {
        let multi_hot = encode_events(events);
        let active: Vec<&str> = VOCAB
            .iter()
            .zip(multi_hot.iter())
            .filter(|(_, v)| **v > 0.0)
            .map(|(name, _)| *name)
            .collect();
        let start = Instant::now();
        let (workload, confidence) = self.classify_vector(&multi_hot)?;
        let inference_us = start.elapsed().as_micros();
        let reason = if active.is_empty() {
            "empty / unrecognized process window → IDLE".into()
        } else {
            format!("multi-hot hits: [{}]", active.join(", "))
        };
        Ok(AiDecision {
            detected_workload: workload,
            confidence,
            reason,
            inference_us,
        })
    }
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
}
