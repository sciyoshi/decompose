use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::traits::tokio::Stream as _;
use interprocess::local_socket::{GenericFilePath, ToFsName};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::model::{ProcessSnapshot, RuntimePaths};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Ps,
    Down {
        timeout_seconds: Option<u64>,
    },
    /// Stop the listed services. Empty list = stop all.
    Stop {
        services: Vec<String>,
    },
    /// Start the listed services. Empty list = start all.
    Start {
        services: Vec<String>,
    },
    /// Restart the listed services. Empty list = restart all.
    Restart {
        services: Vec<String>,
    },
    /// Send a signal to the listed services. Empty list = all.
    Kill {
        services: Vec<String>,
        signal: i32,
    },
    /// Remove processes not in the keep list.
    RemoveOrphans {
        keep: Vec<String>,
    },
    /// Re-read the daemon's config files from disk and reconcile running
    /// processes against the new definition. Stops and re-spawns services
    /// whose `config_hash` has changed, spawns newly-added services, and —
    /// when `remove_orphans` is set — stops and drops services that have
    /// been removed from the config. Without `remove_orphans`, removed
    /// services are left running and logged as orphans. `force_recreate`
    /// classifies every still-present service as `changed` regardless of
    /// hash; `no_recreate` does the opposite, keeping hash-diverged
    /// services untouched. `no_start` inserts new/changed process entries
    /// but leaves them in `NotStarted` instead of `Pending` so the
    /// supervisor won't auto-spawn them.
    Reload {
        force_recreate: bool,
        no_recreate: bool,
        remove_orphans: bool,
        no_start: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Pong {
        pid: u32,
        instance: String,
    },
    Ps {
        pid: u32,
        instance: String,
        processes: Vec<ProcessSnapshot>,
    },
    Ack {
        message: String,
    },
    Error {
        message: String,
    },
}

/// Default timeout for a single IPC round-trip. Local sockets are fast; if
/// the daemon hasn't responded in a few seconds it's almost certainly hung.
/// Override via `DECOMPOSE_IPC_TIMEOUT_MS` (default 5000ms) — see
/// [`crate::tuning`].
pub async fn send_request(paths: &RuntimePaths, request: Request) -> Result<Response> {
    tokio::time::timeout(
        crate::tuning::ipc_timeout(),
        send_request_inner(paths, request),
    )
    .await
    .context("IPC request timed out — daemon may be unresponsive")?
}

async fn send_request_inner(paths: &RuntimePaths, request: Request) -> Result<Response> {
    let socket_name = to_socket_name(&paths.socket)?;
    let stream = Stream::connect(socket_name)
        .await
        .with_context(|| format!("failed to connect to {}", paths.socket.display()))?;

    let (read_half, mut write_half) = tokio::io::split(stream);
    let payload = serde_json::to_string(&request)?;
    write_half.write_all(payload.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        bail!("daemon closed the connection");
    }

    let response: Response = serde_json::from_str(line.trim())?;
    Ok(response)
}

pub fn to_socket_name(path: &Path) -> Result<interprocess::local_socket::Name<'static>> {
    let raw: OsString = path.as_os_str().to_os_string();
    let utf = raw
        .into_string()
        .map_err(|_| anyhow!("socket path contains invalid UTF-8: {}", path.display()))?;
    utf.to_fs_name::<GenericFilePath>()
        .context("failed to create local socket name")
}
