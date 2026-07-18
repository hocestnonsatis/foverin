#![cfg_attr(not(feature = "user"), no_std)]

/// Maximum path bytes captured from `sched_process_exec` (null-terminated).
pub const FILENAME_LEN: usize = 128;

/// Event emitted when a process successfully executes a new image.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ProcessEvent {
    /// Thread-group ID (the userspace "PID").
    pub pid: u32,
    /// Absolute (or relative) filename from `sched_process_exec`.
    pub filename: [u8; FILENAME_LEN],
}

#[cfg(feature = "user")]
unsafe impl aya::Pod for ProcessEvent {}

#[cfg(feature = "user")]
mod ipc {
    use serde::{Deserialize, Serialize};

    /// Unix domain socket path for daemon ↔ CLI IPC.
    pub const SOCKET_PATH: &str = "/tmp/foverin.sock";

    /// Serializable mirror of the daemon's live `AppState` for UDS clients.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    pub struct StateSnapshot {
        /// Recent process exec lines (`pid=N  name`), oldest → newest.
        pub process_stream: Vec<String>,
        /// Latest classified workload label, if any.
        pub workload: Option<String>,
        /// Softmax confidence percentage (0–100).
        pub confidence: f32,
        /// Last inference latency in microseconds.
        pub latency_us: u64,
        /// Active CPU scaling governor.
        pub active_governor: String,
        /// Whether background-hog cgroup throttle is engaged.
        pub cgroup_active: bool,
        /// Number of PIDs currently in the throttle cgroup.
        pub cgroup_pid_count: usize,
        /// Status / reason line.
        pub status: String,
    }

    impl Default for StateSnapshot {
        fn default() -> Self {
            Self {
                process_stream: Vec::new(),
                workload: None,
                confidence: 0.0,
                latency_us: 0,
                active_governor: "unknown".into(),
                cgroup_active: false,
                cgroup_pid_count: 0,
                status: String::new(),
            }
        }
    }
}

#[cfg(feature = "user")]
pub use ipc::{SOCKET_PATH, StateSnapshot};
