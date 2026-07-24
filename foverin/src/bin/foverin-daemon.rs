//! Foverin daemon — eBPF sensor, episodic memory policy, cpufreq actuator, UDS server.

use std::{
    fs, mem,
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::Duration,
};

use aya::{maps::RingBuf, programs::TracePoint};
use foverin::{
    actuator::{current_governor, set_scaling_governor},
    brain::PolicyEngine,
    state::AppState,
};
use foverin_common::{ProcessEvent, SOCKET_PATH};
use log::{debug, info, warn};
use tokio::{
    io::{AsyncWriteExt, Interest, unix::AsyncFd},
    net::UnixListener,
    sync::{broadcast, oneshot},
    time::{MissedTickBehavior, interval},
};

const AGGREGATE_WINDOW: Duration = Duration::from_secs(5);
const BROADCAST_CAP: usize = 64;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Stderr)
        .init();

    // Bump the memlock rlimit. Needed for older kernels that don't use
    // memcg-based BPF accounting (see https://lwn.net/Articles/837122/).
    let rlim = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    let ret = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rlim) };
    if ret != 0 {
        debug!("remove limit on locked memory failed, ret is: {ret}");
    }

    let policy = PolicyEngine::open()?;
    let memory_path = policy.memory_path().display().to_string();

    let state = Arc::new(Mutex::new(AppState::new(current_governor())));
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.status = format!("memory policy — {memory_path}");
    }

    let (broadcast_tx, _) = broadcast::channel::<String>(BROADCAST_CAP);
    let listener = bind_uds(SOCKET_PATH)?;
    info!("UDS listening on {SOCKET_PATH}");

    let accept_tx = broadcast_tx.clone();
    let accept_state = Arc::clone(&state);
    tokio::spawn(async move {
        if let Err(err) = accept_loop(listener, accept_tx, accept_state).await {
            warn!("UDS accept loop exited: {err:#}");
        }
    });

    let mut ebpf = aya::Ebpf::load(aya::include_bytes_aligned!(concat!(
        env!("OUT_DIR"),
        "/foverin-ebpf"
    )))?;

    let program: &mut TracePoint = ebpf.program_mut("foverin").unwrap().try_into()?;
    program.load()?;
    program.attach("sched", "sched_process_exec")?;

    let events_map = ebpf
        .take_map("EVENTS")
        .expect("EVENTS ring buffer map must exist");
    let ring_buf = RingBuf::try_from(events_map)?;
    let events = AsyncFd::with_interest(ring_buf, Interest::READABLE)?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let state_bg = Arc::clone(&state);
    let bus = broadcast_tx.clone();
    let sensor = tokio::spawn(async move {
        if let Err(err) = sensor_loop(policy, events, state_bg, bus, shutdown_rx).await {
            warn!("sensor loop exited: {err:#}");
        }
        // Keep the eBPF object alive for the lifetime of the sensor task.
        drop(ebpf);
    });

    info!("foverin-daemon online — optimizing silently; attach with foverin-cli");
    publish_snapshot(&state, &broadcast_tx);

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("SIGINT — shutting down");
        }
        _ = await_sigterm() => {
            info!("SIGTERM — shutting down");
        }
    }

    let _ = shutdown_tx.send(());
    // Let the sensor flush memory, then join (abort only if it hangs).
    match tokio::time::timeout(Duration::from_secs(3), sensor).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) if err.is_cancelled() => {}
        Ok(Err(err)) => warn!("sensor join: {err}"),
        Err(_) => warn!("sensor loop did not exit in time"),
    }
    let _ = fs::remove_file(SOCKET_PATH);
    Ok(())
}

fn bind_uds(path: &str) -> anyhow::Result<UnixListener> {
    let _ = fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    // Allow unprivileged foverin-cli clients to connect.
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

async fn accept_loop(
    listener: UnixListener,
    tx: broadcast::Sender<String>,
    state: Arc<Mutex<AppState>>,
) -> anyhow::Result<()> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut rx = tx.subscribe();
        // Push current state immediately so the TUI is not blank.
        if let Ok(json) = snapshot_json(&state) {
            let _ = stream.write_all(json.as_bytes()).await;
        }
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(msg) => {
                        if stream.write_all(msg.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

async fn sensor_loop(
    mut policy: PolicyEngine,
    mut events: AsyncFd<RingBuf<aya::maps::MapData>>,
    state: Arc<Mutex<AppState>>,
    bus: broadcast::Sender<String>,
    mut shutdown: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let mut buffer: Vec<ProcessEvent> = Vec::new();
    let mut tick = interval(AGGREGATE_WINDOW);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tick.tick().await;

    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.status = "eBPF attached — aggregating every 5s".into();
    }
    publish_snapshot(&state, &bus);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                if let Err(err) = policy.flush() {
                    warn!("failed to flush memory on shutdown: {err:#}");
                }
                break;
            }
            ready = events.readable_mut() => {
                let mut guard = ready?;
                let ring_buf = guard.get_inner_mut();
                while let Some(item) = ring_buf.next() {
                    let Some(event) = parse_process_event(item.as_ref()) else {
                        continue;
                    };
                    let name = foverin::brain::filename_str(&event.filename);
                    let line = format!("pid={:<6} {name}", event.pid);
                    {
                        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                        s.push_process(line);
                    }
                    buffer.push(event);
                }
                guard.clear_ready();
                publish_snapshot(&state, &bus);
            }
            _ = tick.tick() => {
                let batch = std::mem::take(&mut buffer);
                let decision = policy.decide(&batch);

                {
                    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                    s.set_decision(
                        decision.detected_workload,
                        decision.confidence,
                        decision.inference_us as u64,
                    );
                    s.status = decision.reason.clone();
                }

                match set_scaling_governor(&decision.governor) {
                    Ok(report) => {
                        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                        s.set_governor(report.governor);
                    }
                    Err(err) => {
                        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                        s.status = format!("actuator failed: {err}");
                    }
                }

                publish_snapshot(&state, &bus);
            }
        }
    }

    Ok(())
}

fn snapshot_json(state: &Mutex<AppState>) -> anyhow::Result<String> {
    let snap = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.snapshot()
    };
    let mut line = serde_json::to_string(&snap)?;
    line.push('\n');
    Ok(line)
}

fn publish_snapshot(state: &Mutex<AppState>, bus: &broadcast::Sender<String>) {
    match snapshot_json(state) {
        Ok(line) => {
            let _ = bus.send(line);
        }
        Err(err) => warn!("failed to serialize StateSnapshot: {err:#}"),
    }
}

fn parse_process_event(bytes: &[u8]) -> Option<ProcessEvent> {
    if bytes.len() < mem::size_of::<ProcessEvent>() {
        return None;
    }
    Some(unsafe { bytes.as_ptr().cast::<ProcessEvent>().read_unaligned() })
}

async fn await_sigterm() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    }
    #[cfg(not(unix))]
    {
        std::future::pending::<()>().await
    }
}
