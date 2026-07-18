use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use log::{debug, warn};
use sysinfo::{ProcessesToUpdate, System};

use crate::brain::Workload;

const CPUFREQ_ROOT: &str = "/sys/devices/system/cpu";
const CGROUP_DIR: &str = "/sys/fs/cgroup/foverin_background";
const CGROUP_PARENT_SUBTREE: &str = "/sys/fs/cgroup/cgroup.subtree_control";

/// Known background resource hogs to isolate under COMPILING / GAMING.
pub const BACKGROUND_HOGS: &[&str] = &[
    "spotify",
    "discord",
    "slack",
    "chrome",
    "firefox",
    "telegram-desktop",
];

/// Read the current scaling governor from cpu0 (best-effort).
pub fn current_governor() -> String {
    let path = Path::new(CPUFREQ_ROOT).join("cpu0/cpufreq/scaling_governor");
    fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

/// Apply a CPU scaling profile derived from an AI workload label.
pub fn apply_system_profile(workload: &str) -> io::Result<ActuatorReport> {
    let governor = governor_for_workload(workload);
    set_all_governors(governor)?;
    Ok(ActuatorReport {
        workload: workload.to_string(),
        governor: governor.to_string(),
    })
}

/// Isolate (or release) known background hogs via cgroup v2 sysfs.
///
/// Missing cgroup files are handled gracefully — returns a disabled report
/// rather than failing the whole actuator pipeline.
pub fn manage_cgroups(workload: &Workload) -> io::Result<CgroupReport> {
    if let Err(err) = ensure_background_cgroup() {
        warn!("cgroup v2 init skipped: {err}");
        return Ok(CgroupReport {
            active: false,
            pid_count: 0,
        });
    }

    match workload {
        Workload::Compiling | Workload::Gaming => apply_cgroup_throttle(),
        Workload::Browsing | Workload::Idle => lift_cgroup_throttle(),
    }
}

#[derive(Debug, Clone)]
pub struct ActuatorReport {
    #[allow(dead_code)]
    pub workload: String,
    pub governor: String,
}

impl ActuatorReport {
    pub fn log_line(&self) -> String {
        match self.governor.as_str() {
            "performance" => "[ACTUATOR] Setting all CPU cores to performance!".to_string(),
            other => format!("[ACTUATOR] Setting governor to {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CgroupReport {
    /// True when COMPILING/GAMING throttle is applied.
    pub active: bool,
    /// Number of background PIDs currently moved into the cgroup (throttle path).
    pub pid_count: usize,
}

fn ensure_background_cgroup() -> io::Result<()> {
    let dir = Path::new(CGROUP_DIR);
    if !Path::new("/sys/fs/cgroup").is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "cgroup v2 mount /sys/fs/cgroup not found",
        ));
    }

    // Best-effort: enable cpu + memory on the root so the child gets the controllers.
    let _ = write_sysfs_str(Path::new(CGROUP_PARENT_SUBTREE), "+cpu +memory");

    if !dir.is_dir() {
        fs::create_dir(dir).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("create {CGROUP_DIR}: {err} (need root / cgroup write access?)"),
            )
        })?;
    }
    Ok(())
}

fn apply_cgroup_throttle() -> io::Result<CgroupReport> {
    let pids = scan_background_hog_pids();
    let procs = Path::new(CGROUP_DIR).join("cgroup.procs");
    let mut moved = 0usize;

    for pid in &pids {
        match write_sysfs_str(&procs, &pid.to_string()) {
            Ok(()) => moved += 1,
            Err(err) => debug!("cgroup.procs pid={pid}: {err}"),
        }
    }

    // 10ms quota / 100ms period ≈ 10% of one core for the whole group.
    write_sysfs_optional(Path::new(CGROUP_DIR).join("cpu.max"), "10000 100000");
    // Aggressive reclaim / swap pressure for background apps.
    write_sysfs_optional(Path::new(CGROUP_DIR).join("memory.high"), "1G");

    Ok(CgroupReport {
        active: true,
        pid_count: moved,
    })
}

fn lift_cgroup_throttle() -> io::Result<CgroupReport> {
    write_sysfs_optional(Path::new(CGROUP_DIR).join("cpu.max"), "max 100000");
    write_sysfs_optional(Path::new(CGROUP_DIR).join("memory.high"), "max");

    Ok(CgroupReport {
        active: false,
        pid_count: 0,
    })
}

fn scan_background_hog_pids() -> Vec<u32> {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);

    let mut pids = Vec::new();
    for (pid, process) in sys.processes() {
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        if BACKGROUND_HOGS
            .iter()
            .any(|hog| process_matches_hog(&name, hog))
        {
            pids.push(pid.as_u32());
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn process_matches_hog(name: &str, hog: &str) -> bool {
    name == hog || name.starts_with(&format!("{hog}.")) || name.starts_with(&format!("{hog}-"))
}

fn write_sysfs_optional(path: PathBuf, value: &str) {
    if let Err(err) = write_sysfs_str(&path, value) {
        debug!("skip {}: {err}", path.display());
    }
}

fn write_sysfs_str(path: &Path, value: &str) -> io::Result<()> {
    let mut file = fs::OpenOptions::new().write(true).open(path)?;
    file.write_all(value.as_bytes())?;
    let _ = file.write_all(b"\n");
    Ok(())
}

fn governor_for_workload(workload: &str) -> &'static str {
    match workload.trim().to_ascii_uppercase().as_str() {
        "COMPILING" | "GAMING" => "performance",
        // Prefer schedutil when the kernel exposes it; fall back to powersave.
        "BROWSING" | "IDLE" => preferred_efficiency_governor(),
        other => {
            debug!("unknown workload `{other}`, defaulting to efficiency governor");
            preferred_efficiency_governor()
        }
    }
}

fn preferred_efficiency_governor() -> &'static str {
    if governor_is_available("schedutil") {
        "schedutil"
    } else if governor_is_available("powersave") {
        "powersave"
    } else {
        // Last resort — leave a writable known name; write may still fail per-CPU.
        "powersave"
    }
}

fn governor_is_available(name: &str) -> bool {
    let path = Path::new(CPUFREQ_ROOT).join("cpu0/cpufreq/scaling_available_governors");
    fs::read_to_string(path)
        .map(|s| s.split_whitespace().any(|g| g == name))
        .unwrap_or(false)
}

fn set_all_governors(governor: &str) -> io::Result<()> {
    let mut attempted = 0usize;
    let mut written = 0usize;
    let mut last_err: Option<io::Error> = None;

    for path in cpufreq_governor_paths()? {
        attempted += 1;
        match write_governor(&path, governor) {
            Ok(()) => written += 1,
            Err(err) => {
                // Offline cores / missing cpufreq: skip, keep going.
                debug!("skip {}: {err}", path.display());
                last_err = Some(err);
            }
        }
    }

    if attempted == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no cpu*/cpufreq/scaling_governor paths found",
        ));
    }
    if written == 0 {
        return Err(last_err
            .unwrap_or_else(|| io::Error::other("failed to write scaling_governor on any CPU")));
    }
    Ok(())
}

fn cpufreq_governor_paths() -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(CPUFREQ_ROOT)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("cpu") {
            continue;
        }
        // Match cpu0, cpu12, … — skip cpufreq, cpuidle, etc.
        if !name[3..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let gov = entry.path().join("cpufreq/scaling_governor");
        if gov.is_file() {
            paths.push(gov);
        }
    }
    paths.sort();
    Ok(paths)
}

fn write_governor(path: &Path, governor: &str) -> io::Result<()> {
    write_sysfs_str(path, governor)
}
