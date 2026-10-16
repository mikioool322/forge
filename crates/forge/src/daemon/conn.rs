//! Server-side IPC connection handling.
//!
//! One task per connection. Writes are serialized through a single writer;
//! request/response handling for slow queries is spawned with a per-connection
//! permit limit so notifications keep flowing.

use std::sync::Arc;

use forge_protocol::framing::{FrameReader, FrameWriter};
use forge_protocol::messages::{
    ErrorCode, Frame, Notification, Request, Response, PROTOCOL_VERSION,
};
use forge_protocol::transport::Stream;
use tokio::io::BufReader;
use tokio::sync::{broadcast, mpsc, oneshot, Semaphore};

use crate::daemon::engine::{Ctrl, CtrlReq};

/// Per-connection request concurrency (bounded: no unbounded spawning).
const INFLIGHT_PER_CONNECTION: usize = 8;

pub struct ConnCtx {
    pub engine: mpsc::Sender<Ctrl>,
    pub notify: broadcast::Sender<Notification>,
    pub token: Arc<String>,
    pub require_token: bool,
    pub daemon_version: String,
    pub instance_id: String,
    pub started_at: i64,
}

pub async fn handle(stream: Stream, ctx: Arc<ConnCtx>, peer: String) -> anyhow::Result<()> {
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = FrameReader::new(BufReader::new(read_half));
    let mut writer = FrameWriter::new(write_half);

    // --- handshake --------------------------------------------------------
    let hello = match reader.read().await? {
        Some(Frame::Request {
            id,
            request:
                Request::Hello {
                    protocol,
                    client,
                    token,
                },
        }) => (id, protocol, client, token),
        Some(_) => {
            let _ = writer
                .response(
                    0,
                    Response::error(ErrorCode::BadRequest, "first frame must be `hello`"),
                )
                .await;
            return Ok(());
        }
        None => return Ok(()),
    };
    let (hello_id, client_protocol, client_name, client_token) = hello;
    if client_protocol != PROTOCOL_VERSION {
        let _ = writer
            .response(
                hello_id,
                Response::error(
                    ErrorCode::Conflict,
                    format!(
                        "protocol mismatch: daemon speaks v{PROTOCOL_VERSION}, client speaks v{client_protocol}; upgrade forge"
                    ),
                ),
            )
            .await;
        return Ok(());
    }
    if ctx.require_token && client_token.as_deref() != Some(ctx.token.as_str()) {
        tracing::warn!(peer = %peer, "rejected client with invalid token");
        let _ = writer
            .response(
                hello_id,
                Response::error(ErrorCode::Unauthorized, "invalid or missing session token"),
            )
            .await;
        return Ok(());
    }
    writer
        .response(
            hello_id,
            Response::Hello {
                protocol: PROTOCOL_VERSION,
                daemon_version: ctx.daemon_version.clone(),
                instance_id: ctx.instance_id.clone(),
                uptime_ms: (forge_core::model::now_ms() - ctx.started_at).max(0) as u64,
            },
        )
        .await?;
    tracing::debug!(peer = %peer, client = %client_name, "client connected");

    // --- request loop -----------------------------------------------------
    let mut notify_rx = ctx.notify.subscribe();
    let mut subscribed = false;
    let (reply_tx, mut reply_rx) = mpsc::channel::<(u64, Response)>(16);
    let inflight = Arc::new(Semaphore::new(INFLIGHT_PER_CONNECTION));

    loop {
        tokio::select! {
            frame = reader.read() => {
                match frame {
                    Ok(Some(Frame::Request { id, request })) => {
                        match request {
                            Request::Ping => {
                                let response = Response::Pong {
                                    protocol: PROTOCOL_VERSION,
                                    daemon_version: ctx.daemon_version.clone(),
                                    uptime_ms: (forge_core::model::now_ms() - ctx.started_at).max(0) as u64,
                                };
                                writer.response(id, response).await?;
                            }
                            Request::Hello { .. } => {
                                writer.response(id, Response::error(
                                    ErrorCode::BadRequest, "already greeted on this connection")).await?;
                            }
                            Request::Subscribe { since_event_id } => {
                                subscribed = true;
                                writer.response(id, Response::Subscribed).await?;
                                if let Some(since) = since_event_id {
                                    let (tx, rx) = oneshot::channel();
                                    let ctrl = Ctrl { req: CtrlReq::EventsSince { since_id: since, limit: 500 }, reply: tx };
                                    if ctx.engine.send(ctrl).await.is_ok() {
                                        if let Ok(Response::Events { events }) = rx.await {
                                            for event in events {
                                                writer.notify(Notification::Event(Box::new(event))).await?;
                                            }
                                        }
                                    }
                                }
                            }
                            other => {
                                let Some(req) = to_ctrl_req(other) else {
                                    writer.response(id, Response::error(
                                        ErrorCode::BadRequest, "unsupported request")).await?;
                                    continue;
                                };
                                let permit = match inflight.clone().acquire_owned().await {
                                    Ok(p) => p,
                                    Err(_) => break,
                                };
                                let (tx, rx) = oneshot::channel();
                                if ctx.engine.send(Ctrl { req, reply: tx }).await.is_err() {
                                    writer.response(id, Response::error(
                                        ErrorCode::Unavailable, "daemon engine is stopping")).await?;
                                    continue;
                                }
                                let reply_tx = reply_tx.clone();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    if let Ok(response) = rx.await {
                                        let _ = reply_tx.send((id, response)).await;
                                    }
                                });
                            }
                        }
                    }
                    Ok(Some(_)) => {
                        // Clients only ever send requests.
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!(peer = %peer, error = %e, "frame decode error");
                        break;
                    }
                }
            }
            Some((id, response)) = reply_rx.recv() => {
                writer.response(id, response).await?;
            }
            note = notify_rx.recv(), if subscribed => {
                match note {
                    Ok(notification) => writer.notify(notification).await?,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(peer = %peer, lagged = n, "client lagged; sending resync");
                        writer.notify(Notification::Resync).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    tracing::debug!(peer = %peer, "client disconnected");
    Ok(())
}

fn to_ctrl_req(request: Request) -> Option<CtrlReq> {
    Some(match request {
        Request::Ping | Request::Hello { .. } | Request::Subscribe { .. } => return None,
        Request::GetState => CtrlReq::GetState,
        Request::SubmitPlan { request_id, plan } => CtrlReq::SubmitPlan { request_id, plan },
        Request::CancelTask {
            project,
            task,
            reason,
        } => CtrlReq::CancelTask {
            project,
            task,
            reason,
        },
        Request::RetryTask { project, task } => CtrlReq::RetryTask { project, task },
        Request::GetTaskDetail { project, task } => CtrlReq::GetTaskDetail { project, task },
        Request::GetTaskLog {
            project,
            task,
            lines,
        } => CtrlReq::GetTaskLog {
            project,
            task,
            lines: lines.unwrap_or(0),
        },
        Request::GetTaskDiff {
            project,
            task,
            max_bytes,
        } => CtrlReq::GetTaskDiff {
            project,
            task,
            max_bytes: max_bytes.unwrap_or(0),
        },
        Request::ListProjects => CtrlReq::ListProjects,
        Request::RegisterProject { root, name } => CtrlReq::RegisterProject { root, name },
        Request::Shutdown { drain, .. } => CtrlReq::Shutdown {
            drain: drain.unwrap_or(false),
        },
    })
}
