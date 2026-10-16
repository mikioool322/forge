//! Daemon lifecycle: runtime directories, single-instance lock, endpoint
//! advertisement, accept loop, graceful shutdown.

pub mod conn;
pub mod engine;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use forge_core::model::now_ms;
use forge_core::{ForgeConfig, ForgePaths};
use forge_protocol::transport::{self, Endpoint};
use forge_protocol::PROTOCOL_VERSION;
use forge_runtime::platform;
use forge_store::Store;
use tokio::sync::Semaphore;

use engine::EngineStartup;

/// Start the daemon. Returns when the daemon has shut down cleanly.
pub async fn run(cfg: ForgeConfig, paths: ForgePaths) -> anyhow::Result<()> {
    ensure_directories(&paths)?;
    let _lock = LockGuard::acquire(&paths)?;

    let instance_id = random_hex(8);
    let started_at = now_ms();

    // Database worker (runs migrations).
    let store = Arc::new(Store::open(paths.db_path.clone())?);

    // Session token: required for the TCP fallback used on Windows.
    let token = random_hex(32);
    write_private(&paths.token_file(), &token)?;

    // Bind IPC.
    let requested = Endpoint::parse(&paths.endpoint_spec(&cfg))
        .context("invalid daemon endpoint in configuration")?;
    let listener = transport::bind(&requested)
        .await
        .with_context(|| format!("cannot bind IPC endpoint {}", requested.to_spec()))?;
    let actual = match &requested {
        Endpoint::Tcp { .. } => listener.local_endpoint(),
        other => other.clone(),
    };
    let endpoint_spec = actual.to_spec();
    write_private(&paths.endpoint_file(), &endpoint_spec)?;
    write_private(
        &paths.pid_file(),
        &serde_json::json!({
            "pid": std::process::id(),
            "started_at": started_at,
            "version": env!("CARGO_PKG_VERSION"),
            "endpoint": endpoint_spec,
        })
        .to_string(),
    )?;

    // Engine.
    let handles = engine::spawn_engine(EngineStartup {
        cfg: cfg.clone(),
        paths: paths.clone(),
        store: store.clone(),
        instance_id: instance_id.clone(),
        endpoint_spec: endpoint_spec.clone(),
        started_at,
    });
    let ctrl = handles.ctrl.clone();
    let notify = handles.notify.clone();
    let shutdown = handles.shutdown.clone();
    let engine_join = handles.join;

    let ctx = Arc::new(conn::ConnCtx {
        engine: ctrl,
        notify,
        token: Arc::new(token),
        require_token: matches!(actual, Endpoint::Tcp { .. }),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        instance_id: instance_id.clone(),
        started_at,
    });

    tracing::info!(
        endpoint = %endpoint_spec,
        pid = std::process::id(),
        protocol = PROTOCOL_VERSION,
        "daemon listening"
    );

    let conn_limit = Arc::new(Semaphore::new(cfg.daemon.connection_limit.max(1)));
    let mut shutdown_rx = shutdown.subscribe();

    #[cfg(unix)]
    let mut term_slot: Option<tokio::signal::unix::Signal> = {
        use tokio::signal::unix::{signal, SignalKind};
        Some(signal(SignalKind::terminate()).context("cannot install SIGTERM handler")?)
    };
    #[cfg(not(unix))]
    let mut term_slot: Option<()> = None;

    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("ctrl-c received; shutting down");
                let _ = shutdown.send(true);
                break;
            }
            _ = wait_signal(&mut term_slot) => {
                tracing::info!("SIGTERM received; shutting down");
                let _ = shutdown.send(true);
                break;
            }
            accept = listener.accept() => {
                match accept {
                    Ok(stream) => {
                        let permit = match conn_limit.clone().try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => {
                                tracing::warn!("connection limit reached; rejecting client");
                                continue;
                            }
                        };
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(e) = conn::handle(stream, ctx, "client".to_string()).await {
                                tracing::debug!(error = %e, "connection handler error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }

    // Ask the engine to stop (in case shutdown came from ctrl-c) and wait.
    let _ = shutdown.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(30), engine_join).await;
    drop(store);

    // Remove runtime advertisement files.
    let _ = std::fs::remove_file(paths.endpoint_file());
    let _ = std::fs::remove_file(paths.pid_file());
    tracing::info!("daemon stopped");
    Ok(())
}

fn ensure_directories(paths: &ForgePaths) -> anyhow::Result<()> {
    for dir in [
        &paths.data_dir,
        &paths.run_dir,
        &paths.logs_dir,
        &paths.worktrees_dir,
    ] {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create directory {}", dir.display()))?;
    }
    #[cfg(unix)]
    {
        platform::set_owner_only_permissions(&paths.data_dir)?;
        platform::set_owner_only_permissions(&paths.run_dir)?;
        platform::set_owner_only_permissions(&paths.logs_dir)?;
        platform::set_owner_only_permissions(&paths.worktrees_dir)?;
    }
    Ok(())
}

/// Await SIGTERM on Unix; never completes on other platforms.
#[cfg(unix)]
async fn wait_signal(slot: &mut Option<tokio::signal::unix::Signal>) {
    match slot.as_mut() {
        Some(sig) => {
            let _ = sig.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}

#[cfg(not(unix))]
async fn wait_signal(_slot: &mut Option<()>) {
    std::future::pending::<()>().await
}

fn write_private(path: &PathBuf, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents).with_context(|| format!("cannot write {}", path.display()))?;
    platform::set_owner_only_permissions(path)?;
    Ok(())
}

fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Exclusive daemon lock; stale locks (dead pid) are reclaimed.
struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    fn acquire(paths: &ForgePaths) -> anyhow::Result<Self> {
        let path = paths.lock_file();
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    let payload = format!(
                        "{{\"pid\":{},\"started_at\":{}}}",
                        std::process::id(),
                        now_ms()
                    );
                    let _ = file.write_all(payload.as_bytes());
                    let _ = file.flush();
                    drop(file);
                    platform::set_owner_only_permissions(&path)?;
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                    let content = std::fs::read_to_string(&path).unwrap_or_default();
                    let pid = serde_json::from_str::<serde_json::Value>(&content)
                        .ok()
                        .and_then(|v| v.get("pid").and_then(|p| p.as_u64()))
                        .map(|p| p as u32);
                    if let Some(pid) = pid {
                        if platform::process_alive(pid) {
                            anyhow::bail!(
                                "another daemon is already running (pid {pid}); use `forge daemon stop` first"
                            );
                        }
                        tracing::warn!(stale_pid = pid, "removing stale daemon lock");
                    } else {
                        tracing::warn!("removing unreadable daemon lock file");
                    }
                    let _ = std::fs::remove_file(&path);
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("cannot create lock {}", path.display()))
                }
            }
        }
        anyhow::bail!("could not acquire the daemon lock at {}", path.display());
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
