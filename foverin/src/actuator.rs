use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use log::debug;

const CPUFREQ_ROOT: &str = "/sys/devices/system/cpu";

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
