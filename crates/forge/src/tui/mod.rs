//! Terminal UI: terminal lifecycle, daemon bridge and the event loop.

pub mod app;
pub mod ui;

#[cfg(test)]
mod render_tests;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use forge_core::model::{PlanFile, PlanTaskSpec};
use forge_core::ForgePaths;
use forge_protocol::messages::{Notification, Request, Response};
use forge_protocol::Client;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::cli::TuiArgs;
use crate::ipc;
use crate::CliCtx;
use app::{App, AppMsg, Effect, Screen};

pub async fn run(ctx: &CliCtx, args: &TuiArgs) -> anyhow::Result<ExitCode> {
    let handle = ClientHandle::new(ctx.paths.clone());
    let (msg_tx, msg_rx) = mpsc::channel::<AppMsg>(1024);
    if let Err(e) = handle.ensure(&msg_tx).await {
        eprintln!("forge: {e}");
        eprintln!("forge: starting the UI in offline mode; it will reconnect automatically");
    }
    let mut terminal = setup_terminal().context("cannot initialize the terminal")?;
    let result = event_loop(&mut terminal, handle, msg_tx, msg_rx, args.project.clone()).await;
    if let Err(e) = restore_terminal(&mut terminal) {
        tracing::warn!(error = %e, "failed to restore the terminal");
    }
    result
}

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

fn setup_terminal() -> anyhow::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Term) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    terminal.show_cursor()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon bridge
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ClientHandle {
    paths: ForgePaths,
    current: Arc<Mutex<Option<Arc<Client>>>>,
    connecting: Arc<Mutex<()>>,
}

impl ClientHandle {
    fn new(paths: ForgePaths) -> Self {
        Self {
            paths,
            current: Arc::new(Mutex::new(None)),
            connecting: Arc::new(Mutex::new(())),
        }
    }

    async fn current(&self) -> Option<Arc<Client>> {
        let guard = self.current.lock().await;
        match guard.as_ref() {
            Some(client) if !client.is_closed() => Some(client.clone()),
            _ => None,
        }
    }

    async fn clear(&self) {
        *self.current.lock().await = None;
    }

    async fn ensure(&self, msg_tx: &mpsc::Sender<AppMsg>) -> anyhow::Result<Arc<Client>> {
        if let Some(client) = self.current().await {
            return Ok(client);
        }
        let _guard = self.connecting.lock().await;
        if let Some(client) = self.current().await {
            return Ok(client);
        }
        let client = Arc::new(ipc::connect(&self.paths, "forge-tui").await?);
        *self.current.lock().await = Some(client.clone());
        let subscribe = client.clone();
        tokio::spawn(async move {
            let _ = subscribe
                .call(Request::Subscribe {
                    since_event_id: None,
                })
                .await;
        });
        spawn_bridge(client.clone(), msg_tx.clone());
        spawn_call(
            client.clone(),
            msg_tx.clone(),
            Request::GetState,
            |response| match response {
                Response::State(snapshot) => AppMsg::Snapshot(snapshot),
                other => AppMsg::Note(format!("unexpected response: {other:?}")),
            },
        );
        Ok(client)
    }
}

fn spawn_bridge(client: Arc<Client>, tx: mpsc::Sender<AppMsg>) {
    tokio::spawn(async move {
        let mut notes = client.notifications();
        loop {
            match notes.recv().await {
                Ok(notification) => {
                    if tx
                        .send(AppMsg::Notification(Box::new(notification)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(lagged = n, "tui lagged behind notifications");
                    let _ = tx
                        .send(AppMsg::Notification(Box::new(Notification::Resync)))
                        .await;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    let _ = tx
                        .send(AppMsg::Disconnected("notification stream closed".into()))
                        .await;
                    break;
                }
            }
        }
    });
}

fn spawn_call<F>(client: Arc<Client>, tx: mpsc::Sender<AppMsg>, request: Request, map: F)
where
    F: FnOnce(Response) -> AppMsg + Send + 'static,
{
    tokio::spawn(async move {
        match client.call(request).await {
            Ok(response) => {
                let _ = tx.send(map(response)).await;
            }
            Err(e) => {
                let _ = tx.send(AppMsg::Disconnected(e.to_string())).await;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

async fn event_loop(
    terminal: &mut Term,
    handle: ClientHandle,
    msg_tx: mpsc::Sender<AppMsg>,
    mut msg_rx: mpsc::Receiver<AppMsg>,
    project_filter: Option<String>,
) -> anyhow::Result<ExitCode> {
    let mut app = App::new(project_filter);

    // Blocking key reader thread (crossterm requires blocking reads).
    let (key_tx, mut key_rx) = mpsc::channel::<KeyEvent>(256);
    std::thread::Builder::new()
        .name("forge-tui-input".into())
        .spawn(move || loop {
            match event::read() {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    if key_tx.blocking_send(key).is_err() {
                        break;
                    }
                }
                Ok(Event::Resize(_, _)) => {
                    // Wake the loop so it redraws immediately.
                    if key_tx
                        .blocking_send(KeyEvent::new(KeyCode::Null, KeyModifiers::NONE))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        })
        .context("cannot start the TUI input thread")?;

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_state_refresh = Instant::now() - Duration::from_secs(10);
    let mut last_output_refresh = Instant::now() - Duration::from_secs(10);

    loop {
        terminal.draw(|frame| ui::render(frame, &app))?;

        tokio::select! {
            msg = msg_rx.recv() => {
                let Some(msg) = msg else { break };
                if matches!(msg, AppMsg::Disconnected(_)) {
                    handle.clear().await;
                }
                let effects = app.apply_msg(msg);
                run_effects(effects, &app, &handle, &msg_tx).await;
            }
            key = key_rx.recv() => {
                let Some(key) = key else { break };
                if key.code == KeyCode::Null && key.modifiers == KeyModifiers::NONE {
                    continue; // resize wakeup
                }
                let effects = app.handle_key(key);
                run_effects(effects, &app, &handle, &msg_tx).await;
            }
            _ = tick.tick() => {
                if app.connected {
                    if last_state_refresh.elapsed() >= Duration::from_secs(2) {
                        last_state_refresh = Instant::now();
                        if let Some(client) = handle.current().await {
                            spawn_call(client, msg_tx.clone(), Request::GetState, |response| match response {
                                Response::State(snapshot) => AppMsg::Snapshot(snapshot),
                                other => AppMsg::Note(format!("unexpected response: {other:?}")),
                            });
                        }
                    }
                    if app.screen == Screen::Main
                        && last_output_refresh.elapsed() >= Duration::from_secs(1)
                    {
                        last_output_refresh = Instant::now();
                        let target = app.selected_task().map(|view| {
                            (view.task.id, view.task.display_id(), view.task.project_id, view.task.log_path.is_some())
                        });
                        if let Some((task_id, display, project_id, has_log)) = target {
                            if has_log {
                                if let Some(client) = handle.current().await {
                                    let project = project_name(&app, project_id);
                                    spawn_call(
                                        client,
                                        msg_tx.clone(),
                                        Request::GetTaskLog { project, task: display, lines: Some(60) },
                                        move |response| match response {
                                            Response::TaskLog { text, truncated, .. } => {
                                                AppMsg::Logs { task: task_id, text, truncated }
                                            }
                                            other => AppMsg::Note(format!("log fetch failed: {other:?}")),
                                        },
                                    );
                                }
                            }
                        }
                    }
                } else if let Some(client) = handle.current().await {
                    // Connected but no snapshot yet (or it was lost): keep
                    // asking until the daemon answers.
                    if last_state_refresh.elapsed() >= Duration::from_secs(2) {
                        last_state_refresh = Instant::now();
                        spawn_call(client, msg_tx.clone(), Request::GetState, |response| {
                            match response {
                                Response::State(snapshot) => AppMsg::Snapshot(snapshot),
                                other => AppMsg::Note(format!("unexpected response: {other:?}")),
                            }
                        });
                    }
                } else if handle.ensure(&msg_tx).await.is_ok() {
                    let _ = msg_tx.send(AppMsg::Note("reconnected".into())).await;
                }
            }
        }
        if app.should_quit {
            break;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_effects(
    effects: Vec<Effect>,
    app: &App,
    handle: &ClientHandle,
    msg_tx: &mpsc::Sender<AppMsg>,
) {
    for effect in effects {
        match effect {
            Effect::Quit => break,
            Effect::Refresh => {
                if let Some(client) = handle.current().await {
                    spawn_call(
                        client,
                        msg_tx.clone(),
                        Request::GetState,
                        |response| match response {
                            Response::State(snapshot) => AppMsg::Snapshot(snapshot),
                            other => AppMsg::Note(format!("unexpected response: {other:?}")),
                        },
                    );
                }
            }
            Effect::FetchDetail(task_id) => {
                if let Some((display, project)) = task_ref(app, task_id) {
                    if let Some(client) = handle.current().await {
                        spawn_call(
                            client,
                            msg_tx.clone(),
                            Request::GetTaskDetail {
                                project,
                                task: display,
                            },
                            |response| match response {
                                Response::TaskDetail(detail) => AppMsg::Detail(detail),
                                other => AppMsg::Note(format!("detail fetch failed: {other:?}")),
                            },
                        );
                    }
                }
            }
            Effect::FetchLogs { task, lines } => {
                if let Some((display, project)) = task_ref(app, task) {
                    if let Some(client) = handle.current().await {
                        spawn_call(
                            client,
                            msg_tx.clone(),
                            Request::GetTaskLog {
                                project,
                                task: display,
                                lines: Some(lines),
                            },
                            move |response| match response {
                                Response::TaskLog {
                                    text, truncated, ..
                                } => AppMsg::Logs {
                                    task,
                                    text,
                                    truncated,
                                },
                                other => AppMsg::Note(format!("log fetch failed: {other:?}")),
                            },
                        );
                    }
                }
            }
            Effect::FetchDiff(task_id) => {
                if let Some((display, project)) = task_ref(app, task_id) {
                    if let Some(client) = handle.current().await {
                        spawn_call(
                            client,
                            msg_tx.clone(),
                            Request::GetTaskDiff {
                                project,
                                task: display,
                                max_bytes: None,
                            },
                            move |response| match response {
                                Response::TaskDiff { text, .. } => AppMsg::Diff {
                                    task: task_id,
                                    text,
                                },
                                other => AppMsg::Note(format!("diff fetch failed: {other:?}")),
                            },
                        );
                    }
                }
            }
            Effect::SubmitTask { project, title } => {
                if let Some(client) = handle.current().await {
                    let now = forge_core::model::now_ms();
                    let plan = PlanFile {
                        plan_version: Some(1),
                        project,
                        request_id: Some(format!("tui-{now}")),
                        tasks: vec![PlanTaskSpec {
                            key: format!("ui-{now}"),
                            title: title.clone(),
                            spec: title,
                            capabilities: vec![],
                            deps: vec![],
                            claims: vec![],
                            agent: None,
                            priority: 0,
                            timeout_secs: None,
                        }],
                    };
                    spawn_call(
                        client,
                        msg_tx.clone(),
                        Request::SubmitPlan {
                            request_id: plan.request_id.clone().unwrap_or_default(),
                            plan,
                        },
                        |response| match response {
                            Response::PlanAccepted { tasks, .. } => AppMsg::Note(format!(
                                "task {} submitted",
                                tasks
                                    .first()
                                    .map(|v| v.task.display_id())
                                    .unwrap_or_else(|| "?".into())
                            )),
                            other => AppMsg::Note(format!("submit failed: {other:?}")),
                        },
                    );
                } else {
                    let _ = msg_tx
                        .send(AppMsg::Note(
                            "daemon is offline; cannot create a task".into(),
                        ))
                        .await;
                }
            }
            Effect::CancelTask { task, display } => {
                if let Some(client) = handle.current().await {
                    let project = task_ref(app, task).and_then(|(_, p)| p);
                    spawn_call(
                        client,
                        msg_tx.clone(),
                        Request::CancelTask {
                            project,
                            task: display.clone(),
                            reason: Some("cancelled from TUI".into()),
                        },
                        move |response| match response {
                            Response::Ack { note } => {
                                AppMsg::Note(note.unwrap_or_else(|| {
                                    format!("{display}: cancellation requested")
                                }))
                            }
                            other => AppMsg::Note(format!("cancel failed: {other:?}")),
                        },
                    );
                }
            }
        }
    }
}

fn project_name(app: &App, project_id: i64) -> Option<String> {
    app.projects
        .iter()
        .find(|p| p.id == project_id)
        .map(|p| p.name.clone())
}

fn task_ref(app: &App, task_id: i64) -> Option<(String, Option<String>)> {
    app.tasks
        .iter()
        .find(|v| v.task.id == task_id)
        .map(|v| (v.task.display_id(), project_name(app, v.task.project_id)))
}
