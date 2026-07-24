//! Shared application state bridging the eBPF/AI loop and UDS clients.

use std::collections::VecDeque;

use foverin_common::StateSnapshot;

use crate::brain::Workload;

pub const PROCESS_STREAM_CAP: usize = 50;

/// Snapshot of the latest memory-policy decision for the UI.
#[derive(Debug, Clone, Copy)]
pub struct DecisionSnapshot {
    pub workload: Workload,
    /// Selection confidence percentage (0–100).
    pub confidence: f32,
    /// Decision latency in microseconds.
    pub latency_us: u64,
}

#[derive(Debug)]
pub struct AppState {
    /// Last ~50 intercepted process lines (`pid=N  name`).
    pub process_stream: VecDeque<String>,
    pub last_decision: Option<DecisionSnapshot>,
    pub active_governor: String,
    /// Optional status / error line for the footer.
    pub status: String,
}

impl AppState {
    pub fn new(active_governor: impl Into<String>) -> Self {
        Self {
            process_stream: VecDeque::with_capacity(PROCESS_STREAM_CAP),
            last_decision: None,
            active_governor: active_governor.into(),
            status: "sensors online — watching sched_process_exec".into(),
        }
    }

    pub fn push_process(&mut self, line: String) {
        if self.process_stream.len() >= PROCESS_STREAM_CAP {
            self.process_stream.pop_front();
        }
        self.process_stream.push_back(line);
    }

    pub fn set_decision(&mut self, workload: Workload, confidence: f32, latency_us: u64) {
        self.last_decision = Some(DecisionSnapshot {
            workload,
            confidence,
            latency_us,
        });
    }

    pub fn set_governor(&mut self, governor: impl Into<String>) {
        self.active_governor = governor.into();
    }

    /// Serialize-ready DTO for UDS broadcast to CLI clients.
    pub fn snapshot(&self) -> StateSnapshot {
        let (workload, confidence, latency_us) = match self.last_decision {
            Some(d) => (
                Some(d.workload.as_str().to_string()),
                d.confidence,
                d.latency_us,
            ),
            None => (None, 0.0, 0),
        };
        StateSnapshot {
            process_stream: self.process_stream.iter().cloned().collect(),
            workload,
            confidence,
            latency_us,
            active_governor: self.active_governor.clone(),
            status: self.status.clone(),
        }
    }
}
