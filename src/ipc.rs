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
    Down,
    Ports {
        command: PortsRequest,
    },
    Scale {
        process: String,
        replicas: u16,
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
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PortsRequest {
    List,
    Free,
    Release {
        service_name: Option<String>,
    },
    Reserve {
        port: u16,
        service_name: Option<String>,
    },
    Inspect {
        service_name: String,
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

pub async fn send_request(paths: &RuntimePaths, request: Request) -> Result<Response> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serialization_round_trips() {
        let req = Request::Ports {
            command: PortsRequest::Reserve {
                port: 5050,
                service_name: Some("api".to_string()),
            },
        };
        let encoded = serde_json::to_string(&req).expect("serialize request");
        let decoded: Request = serde_json::from_str(&encoded).expect("deserialize request");
        match decoded {
            Request::Ports {
                command: PortsRequest::Reserve { port, service_name },
            } => {
                assert_eq!(port, 5050);
                assert_eq!(service_name.as_deref(), Some("api"));
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn scale_request_round_trips() {
        let req = Request::Scale {
            process: "api".to_string(),
            replicas: 3,
        };
        let encoded = serde_json::to_string(&req).expect("serialize");
        let decoded: Request = serde_json::from_str(&encoded).expect("deserialize");
        match decoded {
            Request::Scale { process, replicas } => {
                assert_eq!(process, "api");
                assert_eq!(replicas, 3);
            }
            _ => panic!("unexpected variant"),
        }
    }
}
