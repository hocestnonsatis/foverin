//! Foverin CLI — Ratatui Matrix dashboard over the daemon UDS.

use std::sync::{Arc, Mutex};

use foverin::ui;
use foverin_common::{SOCKET_PATH, StateSnapshot};
use log::warn;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixStream,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Stderr)
        .init();

    let stream = match UnixStream::connect(SOCKET_PATH).await {
        Ok(s) => s,
        Err(err) => {
            warn!("cannot connect to {SOCKET_PATH}: {err}");
            ui::run_fatal_unreachable()?;
            return Ok(());
        }
    };

    let state = Arc::new(Mutex::new(StateSnapshot::default()));
    let state_reader = Arc::clone(&state);

    let reader_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<StateSnapshot>(&line) {
                Ok(snap) => {
                    let mut s = state_reader.lock().unwrap_or_else(|e| e.into_inner());
                    *s = snap;
                }
                Err(err) => warn!("bad StateSnapshot JSON: {err}"),
            }
        }
    });

    // Ratatui is blocking; keep the async reader alive on the runtime.
    let ui_result = tokio::task::spawn_blocking(move || ui::run(state)).await?;

    reader_task.abort();
    let _ = reader_task.await;
    ui_result?;
    Ok(())
}
