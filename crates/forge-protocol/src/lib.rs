//! Forge daemon/client IPC protocol.
//!
//! * `messages`   — typed requests, responses and notifications
//! * `framing`    — newline-delimited JSON frames with a size cap
//! * `transport`  — Unix domain sockets (Unix) / loopback TCP (Windows)
//! * `client`     — multiplexing async client with notification stream

pub mod client;
pub mod framing;
pub mod messages;
pub mod transport;

pub use client::{Client, ClientError};
pub use framing::{write_frame, FrameError, FrameReader, FrameWriter, MAX_FRAME_BYTES};
pub use messages::{
    DaemonInfo, ErrorCode, Frame, Notification, Request, Response, StateSnapshot, TaskDetail,
    TaskView, PROTOCOL_VERSION,
};
pub use transport::{Endpoint, Listener, Stream, TransportError};
