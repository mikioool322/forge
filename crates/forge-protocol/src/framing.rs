//! Newline-delimited JSON framing with a hard size cap.
//!
//! Frames are single-line JSON objects terminated by `\n`. Readers must use
//! [`FrameReader`] (which buffers), so that multiple frames arriving in one
//! TCP segment / pipe read are handled correctly.

use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::messages::Frame;

/// Maximum size of a single frame (including the newline).
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("frame exceeds maximum size of {MAX_FRAME_BYTES} bytes")]
    TooLarge,
    #[error("invalid frame: {0}")]
    Invalid(#[from] serde_json::Error),
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> io::Result<()> {
    let mut buf = serde_json::to_vec(frame)?;
    buf.push(b'\n');
    w.write_all(&buf).await?;
    w.flush().await
}

/// Buffered frame reader; owns the read half and keeps unconsumed bytes.
pub struct FrameReader<R: AsyncBufRead + Unpin> {
    inner: R,
}

impl<R: AsyncBufRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    pub async fn read(&mut self) -> Result<Option<Frame>, FrameError> {
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        let n = self.inner.read_until(b'\n', &mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        if buf.len() > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge);
        }
        let frame = serde_json::from_slice(&buf)?;
        Ok(Some(frame))
    }
}

/// Frame writer over any async writer.
pub struct FrameWriter<W: AsyncWrite + Unpin> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    pub async fn write(&mut self, frame: &Frame) -> io::Result<()> {
        write_frame(&mut self.inner, frame).await
    }

    pub async fn response(
        &mut self,
        id: u64,
        response: crate::messages::Response,
    ) -> io::Result<()> {
        self.write(&Frame::Response { id, response }).await
    }

    pub async fn notify(&mut self, notification: crate::messages::Notification) -> io::Result<()> {
        self.write(&Frame::Notify { notification }).await
    }

    pub async fn into_inner(self) -> W {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{Notification, Request};
    use tokio::io::BufReader;

    #[tokio::test]
    async fn roundtrip_multiple_frames() {
        let mut buf: Vec<u8> = Vec::new();
        let f1 = Frame::Request {
            id: 1,
            request: Request::Ping,
        };
        let f2 = Frame::Notify {
            notification: Notification::DaemonNote {
                message: "x".into(),
            },
        };
        write_frame(&mut buf, &f1).await.unwrap();
        write_frame(&mut buf, &f2).await.unwrap();

        let mut reader = FrameReader::new(BufReader::new(std::io::Cursor::new(buf)));
        let g1 = reader.read().await.unwrap().unwrap();
        let g2 = reader.read().await.unwrap().unwrap();
        assert!(matches!(g1, Frame::Request { id: 1, .. }));
        assert!(matches!(g2, Frame::Notify { .. }));
        assert!(reader.read().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversized_frame_rejected() {
        let mut payload = vec![b'{'; MAX_FRAME_BYTES + 10];
        payload.push(b'\n');
        let mut reader = FrameReader::new(BufReader::new(std::io::Cursor::new(payload)));
        let err = reader.read().await.unwrap_err();
        assert!(matches!(err, FrameError::TooLarge), "{err:?}");
    }

    #[tokio::test]
    async fn invalid_json_rejected() {
        let payload = b"not json\n".to_vec();
        let mut reader = FrameReader::new(BufReader::new(std::io::Cursor::new(payload)));
        let err = reader.read().await.unwrap_err();
        assert!(matches!(err, FrameError::Invalid(_)), "{err:?}");
    }

    #[tokio::test]
    async fn truncated_frame_is_error() {
        let mut payload = serde_json::to_vec(&Frame::Request {
            id: 1,
            request: Request::Ping,
        })
        .unwrap();
        payload.truncate(payload.len() / 2);
        let mut reader = FrameReader::new(BufReader::new(std::io::Cursor::new(payload)));
        let err = reader.read().await.unwrap_err();
        assert!(matches!(err, FrameError::Invalid(_)), "{err:?}");
    }

    #[tokio::test]
    async fn multiple_frames_with_partial_reads() {
        let mut data = Vec::new();
        for id in 0..3u64 {
            write_frame(
                &mut data,
                &Frame::Request {
                    id,
                    request: Request::Ping,
                },
            )
            .await
            .unwrap();
        }
        // A tiny buffer forces several underlying reads per frame.
        let mut reader = FrameReader::new(BufReader::with_capacity(16, std::io::Cursor::new(data)));
        for expected in 0..3u64 {
            match reader.read().await.unwrap().unwrap() {
                Frame::Request { id, .. } => assert_eq!(id, expected),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(reader.read().await.unwrap().is_none());
    }
}
