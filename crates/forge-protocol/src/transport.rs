//! IPC transport.
//!
//! Unix domain sockets are used on Unix platforms (with 0600 permissions on
//! the socket file). Windows does not expose AF_UNIX through the Rust
//! standard library / Tokio, so the daemon falls back to a loopback TCP
//! listener with a mandatory bearer token on a random port; the port is
//! advertised through an endpoint file in the runtime directory (0600 on
//! Unix, user-profile protected on Windows). This is a documented deviation:
//! see docs/DECISIONS.md.

use std::fmt;
use std::path::PathBuf;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("invalid endpoint {0:?}: expected `unix:/path` or `tcp:host:port`")]
    Invalid(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("connection closed")]
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Unix(PathBuf),
    Tcp { host: String, port: u16 },
}

impl Endpoint {
    pub fn parse(spec: &str) -> Result<Self, TransportError> {
        let spec = spec.trim();
        if let Some(path) = spec.strip_prefix("unix:") {
            if path.is_empty() {
                return Err(TransportError::Invalid(spec.to_string()));
            }
            return Ok(Endpoint::Unix(PathBuf::from(path)));
        }
        if let Some(rest) = spec.strip_prefix("tcp:") {
            let (host, port) = rest
                .rsplit_once(':')
                .ok_or_else(|| TransportError::Invalid(spec.to_string()))?;
            let port: u16 = port
                .parse()
                .map_err(|_| TransportError::Invalid(spec.to_string()))?;
            return Ok(Endpoint::Tcp {
                host: host.to_string(),
                port,
            });
        }
        Err(TransportError::Invalid(spec.to_string()))
    }

    pub fn to_spec(&self) -> String {
        match self {
            Endpoint::Unix(p) => format!("unix:{}", p.display()),
            Endpoint::Tcp { host, port } => format!("tcp:{host}:{port}"),
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_spec())
    }
}

/// Listening socket.
#[derive(Debug)]
pub struct Listener {
    inner: ListenerInner,
}

#[derive(Debug)]
enum ListenerInner {
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    Tcp(TcpListener),
}

impl Listener {
    pub fn local_endpoint(&self) -> Endpoint {
        match &self.inner {
            #[cfg(unix)]
            ListenerInner::Unix(_) => unreachable!("local_endpoint for unix sockets"),
            ListenerInner::Tcp(l) => {
                let addr = l.local_addr().expect("tcp local addr");
                Endpoint::Tcp {
                    host: addr.ip().to_string(),
                    port: addr.port(),
                }
            }
        }
    }

    pub async fn accept(&self) -> Result<Stream, std::io::Error> {
        match &self.inner {
            #[cfg(unix)]
            ListenerInner::Unix(l) => Ok(Stream::Unix(l.accept().await?.0)),
            ListenerInner::Tcp(l) => {
                let (stream, _peer) = l.accept().await?;
                stream.set_nodelay(true)?;
                Ok(Stream::Tcp(stream))
            }
        }
    }
}

/// Bind to an endpoint. For Unix sockets the socket file is created with
/// 0600 permissions; a stale socket file is removed first.
pub async fn bind(ep: &Endpoint) -> Result<Listener, TransportError> {
    match ep {
        #[cfg(unix)]
        Endpoint::Unix(path) => {
            if path.exists() {
                // Refuse to remove something that is not a socket.
                let meta = std::fs::symlink_metadata(path)?;
                if !meta.file_type().is_socket() {
                    return Err(TransportError::Io(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not a socket", path.display()),
                    )));
                }
                std::fs::remove_file(path)?;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let listener = tokio::net::UnixListener::bind(path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(Listener {
                inner: ListenerInner::Unix(listener),
            })
        }
        #[cfg(not(unix))]
        Endpoint::Unix(path) => Err(TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "unix sockets are not supported on this platform (endpoint {})",
                path.display()
            ),
        ))),
        Endpoint::Tcp { host, port } => {
            if host != "127.0.0.1" && host != "localhost" && host != "::1" {
                return Err(TransportError::Invalid(format!(
                    "refusing to bind non-loopback address {host}"
                )));
            }
            let listener = TcpListener::bind((host.as_str(), *port)).await?;
            Ok(Listener {
                inner: ListenerInner::Tcp(listener),
            })
        }
    }
}

/// Connected stream (client or accepted server side).
pub enum Stream {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    Tcp(tokio::net::TcpStream),
}

impl Stream {
    #[cfg(unix)]
    pub fn is_unix(&self) -> bool {
        matches!(self, Stream::Unix(_))
    }

    #[cfg(not(unix))]
    pub fn is_unix(&self) -> bool {
        false
    }
}

pub async fn connect(ep: &Endpoint) -> Result<Stream, TransportError> {
    match ep {
        #[cfg(unix)]
        Endpoint::Unix(path) => Ok(Stream::Unix(tokio::net::UnixStream::connect(path).await?)),
        #[cfg(not(unix))]
        Endpoint::Unix(path) => Err(TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "unix sockets are not supported on this platform (endpoint {})",
                path.display()
            ),
        ))),
        Endpoint::Tcp { host, port } => {
            let stream = tokio::net::TcpStream::connect((host.as_str(), *port)).await?;
            stream.set_nodelay(true)?;
            Ok(Stream::Tcp(stream))
        }
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Stream::Unix(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Stream::Tcp(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            #[cfg(unix)]
            Stream::Unix(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            Stream::Tcp(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Stream::Unix(s) => std::pin::Pin::new(s).poll_flush(cx),
            Stream::Tcp(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Stream::Unix(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Stream::Tcp(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn endpoint_parsing() {
        assert_eq!(
            Endpoint::parse("unix:/tmp/forge.sock").unwrap(),
            Endpoint::Unix(PathBuf::from("/tmp/forge.sock"))
        );
        assert_eq!(
            Endpoint::parse("tcp:127.0.0.1:4510").unwrap(),
            Endpoint::Tcp {
                host: "127.0.0.1".into(),
                port: 4510
            }
        );
        assert!(Endpoint::parse("http://x").is_err());
        assert!(Endpoint::parse("unix:").is_err());
        assert!(Endpoint::parse("tcp:127.0.0.1").is_err());
        assert!(Endpoint::parse("tcp:127.0.0.1:notaport").is_err());
    }

    #[test]
    fn endpoint_spec_roundtrip() {
        for spec in ["tcp:127.0.0.1:8080", "unix:/run/forge.sock"] {
            let ep = Endpoint::parse(spec).unwrap();
            assert_eq!(ep.to_spec(), spec);
        }
    }

    #[tokio::test]
    async fn tcp_roundtrip() {
        let listener = bind(&Endpoint::Tcp {
            host: "127.0.0.1".into(),
            port: 0,
        })
        .await
        .unwrap();
        let local = listener.local_endpoint();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"hello");
            stream.write_all(b"world").await.unwrap();
            stream.flush().await.unwrap();
        });
        let mut client = connect(&local).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        client.flush().await.unwrap();
        let mut buf = [0u8; 5];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn refuses_non_loopback_bind() {
        let err = bind(&Endpoint::Tcp {
            host: "0.0.0.0".into(),
            port: 0,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, TransportError::Invalid(_)), "{err:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forge.sock");
        let listener = bind(&Endpoint::Unix(path.clone())).await.unwrap();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut buf = [0u8; 2];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"hi");
        });
        let mut client = connect(&Endpoint::Unix(path.clone())).await.unwrap();
        client.write_all(b"hi").await.unwrap();
        client.flush().await.unwrap();
        server.await.unwrap();

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket must be owner-only");
    }
}
