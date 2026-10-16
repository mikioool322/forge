//! Asynchronous, multiplexing IPC client.
//!
//! One connection carries many concurrent requests (matched by id) plus a
//! stream of server-push notifications.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::BufReader;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

use crate::framing::{FrameReader, FrameWriter};
use crate::messages::{Frame, Notification, Request, Response, PROTOCOL_VERSION};
use crate::transport::{connect, Endpoint};

/// Default per-request timeout.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);
const NOTIFY_CHANNEL_CAPACITY: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("transport error: {0}")]
    Transport(#[from] crate::transport::TransportError),
    #[error("connection closed by daemon")]
    Closed,
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("protocol mismatch: daemon speaks v{daemon}, client speaks v{client}")]
    ProtocolMismatch { daemon: u32, client: u32 },
    #[error("daemon rejected the session: {0}")]
    Handshake(String),
    #[error("unexpected frame from daemon")]
    UnexpectedFrame,
}

pub struct Client {
    tx: mpsc::Sender<Frame>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>>,
    notify_tx: broadcast::Sender<Notification>,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    call_timeout: Duration,
}

impl Client {
    /// Connect and perform the protocol handshake.
    pub async fn connect(
        endpoint: &Endpoint,
        client_name: &str,
        token: Option<&str>,
    ) -> Result<Self, ClientError> {
        let stream = connect(endpoint).await?;
        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = FrameReader::new(BufReader::new(read_half));
        let mut writer = FrameWriter::new(write_half);

        let (tx, mut rx) = mpsc::channel::<Frame>(1024);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (notify_tx, _) = broadcast::channel::<Notification>(NOTIFY_CHANNEL_CAPACITY);
        let closed = Arc::new(AtomicBool::new(false));

        // Writer task: serializes outgoing frames.
        let closed_w = closed.clone();
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if writer.write(&frame).await.is_err() {
                    break;
                }
            }
            closed_w.store(true, Ordering::SeqCst);
        });

        // Reader task: dispatches responses to waiters and notifications to
        // subscribers.
        let pending_r = pending.clone();
        let closed_r = closed.clone();
        let notify_tx_r = notify_tx.clone();
        tokio::spawn(async move {
            loop {
                match reader.read().await {
                    Ok(Some(Frame::Response { id, response })) => {
                        let waiter = pending_r.lock().await.remove(&id);
                        if let Some(w) = waiter {
                            let _ = w.send(response);
                        }
                    }
                    Ok(Some(Frame::Notify { notification })) => {
                        let _ = notify_tx_r.send(notification);
                    }
                    Ok(Some(Frame::Request { .. })) => {
                        // Servers never send requests.
                        break;
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            closed_r.store(true, Ordering::SeqCst);
            // Wake all waiters with "closed": dropping the oneshot senders.
            pending_r.lock().await.clear();
        });

        let client = Self {
            tx,
            pending,
            notify_tx,
            next_id: AtomicU64::new(1),
            closed,
            call_timeout: DEFAULT_CALL_TIMEOUT,
        };

        // Handshake.
        let response = client
            .call(Request::Hello {
                protocol: PROTOCOL_VERSION,
                client: client_name.to_string(),
                token: token.map(|t| t.to_string()),
            })
            .await?;
        match response {
            Response::Hello {
                protocol,
                daemon_version,
                ..
            } => {
                if protocol != PROTOCOL_VERSION {
                    return Err(ClientError::ProtocolMismatch {
                        daemon: protocol,
                        client: PROTOCOL_VERSION,
                    });
                }
                tracing::debug!(daemon_version, "connected to daemon");
                Ok(client)
            }
            Response::Error { message, .. } => Err(ClientError::Handshake(message)),
            _ => Err(ClientError::UnexpectedFrame),
        }
    }

    pub fn set_call_timeout(&mut self, timeout: Duration) {
        self.call_timeout = timeout;
    }

    /// Send a request and await its response.
    pub async fn call(&self, request: Request) -> Result<Response, ClientError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ClientError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if self.tx.send(Frame::Request { id, request }).await.is_err() {
            self.pending.lock().await.remove(&id);
            return Err(ClientError::Closed);
        }
        match tokio::time::timeout(self.call_timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(ClientError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(ClientError::Timeout(self.call_timeout))
            }
        }
    }

    /// Convenience: `Ping`.
    pub async fn ping(&self) -> Result<(), ClientError> {
        match self.call(Request::Ping).await? {
            Response::Pong { .. } => Ok(()),
            Response::Error { message, .. } => Err(ClientError::Handshake(message)),
            _ => Err(ClientError::UnexpectedFrame),
        }
    }

    /// Subscribe to daemon notifications (at most one subscription per
    /// client; call `Request::Subscribe` separately if needed).
    pub fn notifications(&self) -> broadcast::Receiver<Notification> {
        self.notify_tx.subscribe()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("closed", &self.is_closed())
            .finish()
    }
}
