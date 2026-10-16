//! Client-side daemon discovery and connection.

use forge_core::ForgePaths;
use forge_protocol::{Client, ClientError, Endpoint};

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error(
        "daemon is not running (no endpoint file at {path}); start it with `forge daemon start`"
    )]
    NotRunning { path: String },
    #[error(
        "cannot connect to the daemon at {endpoint}: {source}. Start it with `forge daemon start`"
    )]
    Connect {
        endpoint: String,
        #[source]
        source: ClientError,
    },
    #[error("endpoint file {path} is corrupt: {detail}")]
    CorruptEndpoint { path: String, detail: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Read the endpoint advertised by a running daemon.
pub fn discover_endpoint(paths: &ForgePaths) -> Result<Endpoint, IpcError> {
    let file = paths.endpoint_file();
    if !file.exists() {
        return Err(IpcError::NotRunning {
            path: file.display().to_string(),
        });
    }
    let spec = std::fs::read_to_string(&file)?;
    Endpoint::parse(spec.trim()).map_err(|e| IpcError::CorruptEndpoint {
        path: file.display().to_string(),
        detail: e.to_string(),
    })
}

pub fn read_token(paths: &ForgePaths) -> Option<String> {
    std::fs::read_to_string(paths.token_file())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Connect and complete the handshake. Token is only needed for TCP.
pub async fn connect(paths: &ForgePaths, client_name: &str) -> Result<Client, IpcError> {
    let endpoint = discover_endpoint(paths)?;
    let token = match &endpoint {
        Endpoint::Tcp { .. } => read_token(paths),
        _ => None,
    };
    match Client::connect(&endpoint, client_name, token.as_deref()).await {
        Ok(client) => Ok(client),
        Err(e) => Err(IpcError::Connect {
            endpoint: endpoint.to_spec(),
            source: e,
        }),
    }
}

/// Connect, retrying for up to `attempts` with `delay` between tries; used
/// by the TUI so it can attach before the daemon fully boots.
pub async fn connect_retry(
    paths: &ForgePaths,
    client_name: &str,
    attempts: u32,
    delay: std::time::Duration,
) -> Result<Client, IpcError> {
    let mut last = None;
    for _ in 0..attempts.max(1) {
        match connect(paths, client_name).await {
            Ok(c) => return Ok(c),
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(delay).await;
            }
        }
    }
    Err(last.unwrap_or(IpcError::NotRunning {
        path: paths.endpoint_file().display().to_string(),
    }))
}
