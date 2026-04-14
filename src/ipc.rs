use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

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
    Down,
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

/// Default timeout for a single IPC round-trip.  Local sockets are fast;
/// if the daemon hasn't responded in 5 seconds it's almost certainly hung.
const IPC_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn send_request(paths: &RuntimePaths, request: Request) -> Result<Response> {
    tokio::time::timeout(IPC_TIMEOUT, send_request_inner(paths, request))
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
