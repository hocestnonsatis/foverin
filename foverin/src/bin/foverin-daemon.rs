//! Foverin daemon — eBPF sensor, nano-NN inference, cpufreq actuator, UDS server.

use std::{
    fs, mem,
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::Duration,
};

use aya::{maps::RingBuf, programs::TracePoint};
use foverin::{
    actuator::{apply_system_profile, current_governor},
    brain::{Classifier, Workload, resolve_weights_path},
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

    let weights_path = resolve_weights_path();
    let classifier = Classifier::load(&weights_path).map_err(|err| {
        anyhow::anyhow!(
            "failed to load weights from {}: {err:#}\n\
             Run `cargo build --release --bin forge && ./target/release/forge` first.",
            weights_path.display()
        )
    })?;

    let state = Arc::new(Mutex::new(AppState::new(current_governor())));
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.status = format!("weights loaded — {}", weights_path.display());
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
        if let Err(err) = sensor_loop(classifier, events, state_bg, bus, shutdown_rx).await {
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
    sensor.abort();
    let _ = sensor.await;
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
    classifier: Classifier,
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
                let decision = if batch.is_empty() {
                    foverin::brain::AiDecision {
                        detected_workload: Workload::Idle,
                        confidence: 100.0,
                        reason: "no process exec events in the aggregation window".into(),
                        inference_us: 0,
                    }
                } else {
                    match classifier.classify_events(&batch) {
                        Ok(decision) => decision,
                        Err(err) => {
                            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                            s.status = format!("inference error: {err:#}");
                            publish_snapshot(&state, &bus);
                            continue;
                        }
                    }
                };

                {
                    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                    s.set_decision(
                        decision.detected_workload,
                        decision.confidence,
                        decision.inference_us as u64,
                    );
                    s.status = decision.reason.clone();
                }

                match apply_system_profile(decision.detected_workload.as_str()) {
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
