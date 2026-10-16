//! End-to-end tests of the IPC protocol over a real (loopback TCP) socket:
//! handshake, protocol version checks, multiplexed requests, out-of-order
//! responses, notifications and disconnects.

use std::time::Duration;

use forge_protocol::framing::{FrameReader, FrameWriter};
use forge_protocol::messages::{
    ErrorCode, Frame, Notification, Request, Response, PROTOCOL_VERSION,
};
use forge_protocol::transport::{bind, Endpoint};
use forge_protocol::Client;
use tokio::io::BufReader;

/// Run a scripted server on a loopback socket; returns its endpoint.
async fn spawn_server<F, Fut>(script: F) -> Endpoint
where
    F: FnOnce(
            tokio::io::ReadHalf<forge_protocol::transport::Stream>,
            tokio::io::WriteHalf<forge_protocol::transport::Stream>,
        ) -> Fut
        + Send
        + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = bind(&Endpoint::Tcp {
        host: "127.0.0.1".into(),
        port: 0,
    })
    .await
    .unwrap();
    let endpoint = listener.local_endpoint();
    tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let (r, w) = tokio::io::split(stream);
        script(r, w).await;
    });
    endpoint
}

async fn expect_hello(
    reader: &mut FrameReader<BufReader<tokio::io::ReadHalf<forge_protocol::transport::Stream>>>,
) -> u64 {
    match reader.read().await.unwrap().unwrap() {
        Frame::Request {
            id,
            request: Request::Hello { protocol, .. },
        } => {
            assert_eq!(protocol, PROTOCOL_VERSION);
            id
        }
        other => panic!("expected hello, got {other:?}"),
    }
}

#[tokio::test]
async fn handshake_multiplexing_and_notifications() {
    let endpoint = spawn_server(|r, w| async move {
        let mut reader = FrameReader::new(BufReader::new(r));
        let mut writer = FrameWriter::new(w);
        let hello_id = expect_hello(&mut reader).await;
        writer
            .response(
                hello_id,
                Response::Hello {
                    protocol: PROTOCOL_VERSION,
                    daemon_version: "0.0.0-test".into(),
                    instance_id: "itest".into(),
                    uptime_ms: 5,
                },
            )
            .await
            .unwrap();

        // Expect a Subscribe, then push a notification.
        match reader.read().await.unwrap().unwrap() {
            Frame::Request {
                id,
                request: Request::Subscribe { .. },
            } => {
                writer.response(id, Response::Subscribed).await.unwrap();
            }
            other => panic!("expected subscribe, got {other:?}"),
        }
        writer
            .notify(Notification::DaemonNote {
                message: "boot".into(),
            })
            .await
            .unwrap();

        // Read two pipelined requests and answer them in reverse order.
        let mut reqs = vec![];
        for _ in 0..2 {
            match reader.read().await.unwrap().unwrap() {
                Frame::Request { id, request } => reqs.push((id, request)),
                other => panic!("expected request, got {other:?}"),
            }
        }
        for (id, request) in reqs.into_iter().rev() {
            let response = match request {
                Request::Ping => Response::Pong {
                    protocol: PROTOCOL_VERSION,
                    daemon_version: "0.0.0-test".into(),
                    uptime_ms: 42,
                },
                other => Response::error(ErrorCode::BadRequest, format!("unexpected {other:?}")),
            };
            writer.response(id, response).await.unwrap();
        }
    })
    .await;

    let client = Client::connect(&endpoint, "itest-client", None)
        .await
        .unwrap();
    let mut notes = client.notifications();
    match client
        .call(Request::Subscribe {
            since_event_id: None,
        })
        .await
        .unwrap()
    {
        Response::Subscribed => {}
        other => panic!("unexpected {other:?}"),
    }
    let note = tokio::time::timeout(Duration::from_secs(5), notes.recv())
        .await
        .expect("notification timeout")
        .expect("notification channel");
    assert!(matches!(note, Notification::DaemonNote { .. }));

    // Two concurrent calls, answers arrive in reverse order.
    let (a, b) = tokio::join!(client.call(Request::Ping), client.call(Request::Ping));
    assert!(matches!(a.unwrap(), Response::Pong { uptime_ms: 42, .. }));
    assert!(matches!(b.unwrap(), Response::Pong { uptime_ms: 42, .. }));
}

#[tokio::test]
async fn protocol_mismatch_is_rejected() {
    let endpoint = spawn_server(|r, w| async move {
        let mut reader = FrameReader::new(BufReader::new(r));
        let mut writer = FrameWriter::new(w);
        let hello_id = expect_hello(&mut reader).await;
        writer
            .response(
                hello_id,
                Response::Hello {
                    protocol: PROTOCOL_VERSION + 99,
                    daemon_version: "future".into(),
                    instance_id: "itest".into(),
                    uptime_ms: 0,
                },
            )
            .await
            .unwrap();
    })
    .await;

    let err = Client::connect(&endpoint, "itest-client", None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, forge_protocol::ClientError::ProtocolMismatch { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn server_disconnect_wakes_pending_calls() {
    let endpoint = spawn_server(|r, w| async move {
        let mut reader = FrameReader::new(BufReader::new(r));
        let mut writer = FrameWriter::new(w);
        let hello_id = expect_hello(&mut reader).await;
        writer
            .response(
                hello_id,
                Response::Hello {
                    protocol: PROTOCOL_VERSION,
                    daemon_version: "0.0.0-test".into(),
                    instance_id: "itest".into(),
                    uptime_ms: 0,
                },
            )
            .await
            .unwrap();
        // Drop the connection without answering the next request.
    })
    .await;

    let client = Client::connect(&endpoint, "itest-client", None)
        .await
        .unwrap();
    let result = client.call(Request::Ping).await;
    assert!(
        matches!(result, Err(forge_protocol::ClientError::Closed)),
        "{result:?}"
    );
    assert!(client.is_closed());
}

#[tokio::test]
async fn hello_rejection_surfaces_reason() {
    let endpoint = spawn_server(|r, w| async move {
        let mut reader = FrameReader::new(BufReader::new(r));
        let mut writer = FrameWriter::new(w);
        let hello_id = expect_hello(&mut reader).await;
        writer
            .response(
                hello_id,
                Response::error(ErrorCode::Unauthorized, "bad token"),
            )
            .await
            .unwrap();
    })
    .await;

    let err = Client::connect(&endpoint, "itest-client", Some("wrong"))
        .await
        .unwrap_err();
    match err {
        forge_protocol::ClientError::Handshake(msg) => assert!(msg.contains("bad token")),
        other => panic!("unexpected {other:?}"),
    }
}
