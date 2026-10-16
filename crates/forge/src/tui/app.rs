//! TUI application state, messages and input handling.
//!
//! This module is pure: it never performs I/O. The event loop in `mod.rs`
//! translates [`Effect`]s into daemon requests and feeds
//! [`AppMsg`]/notifications back in. That keeps keyboard behaviour and
//! rendering fully unit-testable.

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use forge_core::model::{AgentInfo, EventRecord, Project, TaskState};
use forge_protocol::{DaemonInfo, Notification, StateSnapshot, TaskDetail, TaskView};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tasks,
    Agents,
    Output,
}

impl Focus {
    pub fn next(self) -> Self {
        match self {
            Focus::Tasks => Focus::Agents,
            Focus::Agents => Focus::Output,
            Focus::Output => Focus::Tasks,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Main,
    Help,
    Detail,
    Logs,
    Diff,
    NewTask,
}

impl Screen {
    pub fn label(self) -> &'static str {
        match self {
            Screen::Main => "OVERVIEW",
            Screen::Help => "HELP",
            Screen::Detail => "TASK",
            Screen::Logs => "LOG",
            Screen::Diff => "DIFF",
            Screen::NewTask => "NEW TASK",
        }
    }
}

/// Side effects requested by the UI (executed by the event loop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Refresh,
    FetchDetail(i64),
    FetchLogs { task: i64, lines: usize },
    FetchDiff(i64),
    SubmitTask { project: String, title: String },
    CancelTask { task: i64, display: String },
    Quit,
}

/// Messages pushed into the event loop from async client work.
#[derive(Debug)]
pub enum AppMsg {
    Snapshot(Box<StateSnapshot>),
    Notification(Box<Notification>),
    Detail(Box<TaskDetail>),
    Logs {
        task: i64,
        text: String,
        truncated: bool,
    },
    Diff {
        task: i64,
        text: String,
    },
    Disconnected(String),
    Note(String),
}

pub struct App {
    pub connected: bool,
    pub daemon: Option<DaemonInfo>,
    pub projects: Vec<Project>,
    pub tasks: Vec<TaskView>,
    pub agents: Vec<AgentInfo>,
    pub events: VecDeque<EventRecord>,

    pub focus: Focus,
    pub screen: Screen,
    pub task_index: usize,
    pub agent_index: usize,

    /// Cached log tail for the selected task (overview pane).
    pub output_text: String,
    pub output_task: Option<i64>,

    pub detail: Option<TaskDetail>,
    pub log_text: Option<String>,
    pub log_task: Option<i64>,
    pub log_truncated: bool,
    pub diff_text: Option<String>,
    pub diff_task: Option<i64>,
    pub screen_scroll: u16,

    pub input: String,
    pub confirm_cancel: Option<(i64, String)>,

    pub project_filter: Option<String>,
    pub status: String,
    pub should_quit: bool,
}

impl App {
    pub fn new(project_filter: Option<String>) -> Self {
        Self {
            connected: false,
            daemon: None,
            projects: Vec::new(),
            tasks: Vec::new(),
            agents: Vec::new(),
            events: VecDeque::new(),
            focus: Focus::Tasks,
            screen: Screen::Main,
            task_index: 0,
            agent_index: 0,
            output_text: String::new(),
            output_task: None,
            detail: None,
            log_text: None,
            log_task: None,
            log_truncated: false,
            diff_text: None,
            diff_task: None,
            screen_scroll: 0,
            input: String::new(),
            confirm_cancel: None,
            project_filter,
            status: String::new(),
            should_quit: false,
        }
    }

    // ------------------------------------------------------------------
    // Data ingestion
    // ------------------------------------------------------------------

    pub fn apply_snapshot(&mut self, snapshot: StateSnapshot) {
        let previous = self.selected_task().map(|v| v.task.id);
        let was_connected = self.connected;
        let endpoint = snapshot.daemon.endpoint.clone();
        self.daemon = Some(snapshot.daemon);
        self.projects = snapshot.projects;
        self.tasks = snapshot.tasks;
        self.tasks
            .sort_by_key(|v| (v.task.project_id, v.task.seq_no));
        self.agents = snapshot.agents;
        self.events = snapshot.events.into_iter().collect();
        if let Some(id) = previous {
            if let Some(idx) = self.visible_tasks().iter().position(|v| v.task.id == id) {
                self.task_index = idx;
            }
        }
        self.connected = true;
        if !was_connected {
            self.status = format!("attached to {endpoint}");
        }
        self.clamp_selection();
    }

    pub fn apply_notification(&mut self, notification: Notification) -> Vec<Effect> {
        match notification {
            Notification::TaskChanged(view) => {
                let task_id = view.task.id;
                match self.tasks.iter_mut().find(|v| v.task.id == task_id) {
                    Some(existing) => *existing = *view,
                    None => self.tasks.push(*view),
                }
                self.tasks
                    .sort_by_key(|v| (v.task.project_id, v.task.seq_no));
                self.clamp_selection();
                vec![]
            }
            Notification::AgentsChanged { agents } => {
                self.agents = agents;
                vec![]
            }
            Notification::Event(event) => {
                self.events.push_back(*event);
                while self.events.len() > 500 {
                    self.events.pop_front();
                }
                vec![]
            }
            Notification::DaemonNote { message } => {
                self.status = message;
                vec![]
            }
            Notification::Resync => vec![Effect::Refresh],
        }
    }

    pub fn apply_msg(&mut self, msg: AppMsg) -> Vec<Effect> {
        match msg {
            AppMsg::Snapshot(snapshot) => {
                self.apply_snapshot(*snapshot);
                vec![]
            }
            AppMsg::Notification(note) => self.apply_notification(*note),
            AppMsg::Detail(detail) => {
                self.detail = Some(*detail);
                vec![]
            }
            AppMsg::Logs {
                task,
                text,
                truncated,
            } => {
                self.log_task = Some(task);
                self.log_text = Some(text);
                self.log_truncated = truncated;
                vec![]
            }
            AppMsg::Diff { task, text } => {
                self.diff_task = Some(task);
                self.diff_text = Some(text);
                vec![]
            }
            AppMsg::Disconnected(reason) => {
                self.connected = false;
                self.status = format!("daemon disconnected: {reason}; reconnecting…");
                vec![]
            }
            AppMsg::Note(note) => {
                self.status = note;
                vec![]
            }
        }
    }

    // ------------------------------------------------------------------
    // Queries
    // ------------------------------------------------------------------

    pub fn visible_tasks(&self) -> Vec<&TaskView> {
        self.tasks
            .iter()
            .filter(|v| match &self.project_filter {
                Some(name) => self
                    .projects
                    .iter()
                    .find(|p| &p.name == name)
                    .map(|p| p.id == v.task.project_id)
                    .unwrap_or(true),
                None => true,
            })
            .collect()
    }

    pub fn selected_task(&self) -> Option<&TaskView> {
        self.visible_tasks().get(self.task_index).copied()
    }

    pub fn selected_agent(&self) -> Option<&AgentInfo> {
        self.agents.get(self.agent_index)
    }

    pub fn active_project(&self) -> Option<String> {
        if let Some(name) = &self.project_filter {
            return Some(name.clone());
        }
        if let Some(task) = self.selected_task() {
            if let Some(project) = self.projects.iter().find(|p| p.id == task.task.project_id) {
                return Some(project.name.clone());
            }
        }
        if self.projects.len() == 1 {
            return Some(self.projects[0].name.clone());
        }
        None
    }

    pub fn running_agents(&self) -> usize {
        self.agents
            .iter()
            .filter(|a| a.state == forge_core::AgentState::Running)
            .count()
    }

    pub fn input_active(&self) -> bool {
        self.screen == Screen::NewTask
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_tasks().len();
        if len == 0 {
            self.task_index = 0;
        } else if self.task_index >= len {
            self.task_index = len - 1;
        }
        if !self.agents.is_empty() && self.agent_index >= self.agents.len() {
            self.agent_index = self.agents.len() - 1;
        }
    }

    // ------------------------------------------------------------------
    // Input
    // ------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        // 1. Modal cancellation confirmation.
        if let Some((task_id, display)) = self.confirm_cancel.clone() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.confirm_cancel = None;
                    return vec![Effect::CancelTask {
                        task: task_id,
                        display,
                    }];
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.confirm_cancel = None;
                    self.status = "cancellation dismissed".into();
                }
                _ => {}
            }
            return vec![];
        }

        // 2. Text input mode swallows every printable key.
        if self.screen == Screen::NewTask {
            return self.handle_input_key(key);
        }

        let mut effects = vec![];
        match self.screen {
            Screen::Main => self.handle_main_key(key, &mut effects),
            Screen::Help => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => {
                    self.screen = Screen::Main;
                }
                _ => {}
            },
            Screen::Detail => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Main,
                KeyCode::Char('l') => {
                    if let Some(task) = &self.detail {
                        effects.push(Effect::FetchLogs {
                            task: task.task.id,
                            lines: 400,
                        });
                        self.screen = Screen::Logs;
                        self.screen_scroll = 0;
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(task) = &self.detail {
                        effects.push(Effect::FetchDiff(task.task.id));
                        self.screen = Screen::Diff;
                        self.screen_scroll = 0;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.screen_scroll = self.screen_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.screen_scroll = self.screen_scroll.saturating_sub(1)
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    self.screen_scroll = self.screen_scroll.saturating_add(20)
                }
                KeyCode::PageUp => self.screen_scroll = self.screen_scroll.saturating_sub(20),
                KeyCode::Char('r') => {
                    if let Some(task) = &self.detail {
                        effects.push(Effect::FetchDetail(task.task.id));
                    }
                }
                _ => {}
            },
            Screen::Logs => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Main,
                KeyCode::Char('l') => {
                    if let Some(task) = self.selected_task() {
                        effects.push(Effect::FetchLogs {
                            task: task.task.id,
                            lines: 400,
                        });
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(task) = self.selected_task() {
                        effects.push(Effect::FetchDiff(task.task.id));
                        self.screen = Screen::Diff;
                        self.screen_scroll = 0;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.screen_scroll = self.screen_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.screen_scroll = self.screen_scroll.saturating_sub(1)
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    self.screen_scroll = self.screen_scroll.saturating_add(20)
                }
                KeyCode::PageUp => self.screen_scroll = self.screen_scroll.saturating_sub(20),
                KeyCode::Char('g') => self.screen_scroll = 0,
                KeyCode::Char('G') => self.screen_scroll = u16::MAX,
                _ => {}
            },
            Screen::Diff => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Main,
                KeyCode::Char('d') => {
                    if let Some(task) = self.selected_task() {
                        effects.push(Effect::FetchDiff(task.task.id));
                    }
                }
                KeyCode::Char('r') => {
                    if let Some(task) = self.selected_task() {
                        effects.push(Effect::FetchDiff(task.task.id));
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.screen_scroll = self.screen_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.screen_scroll = self.screen_scroll.saturating_sub(1)
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    self.screen_scroll = self.screen_scroll.saturating_add(20)
                }
                KeyCode::PageUp => self.screen_scroll = self.screen_scroll.saturating_sub(20),
                KeyCode::Char('g') => self.screen_scroll = 0,
                KeyCode::Char('G') => self.screen_scroll = u16::MAX,
                _ => {}
            },
            Screen::NewTask => unreachable!("handled above"),
        }
        effects
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let mut effects = vec![];
        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::Main;
                self.input.clear();
                self.status = "new task cancelled".into();
            }
            KeyCode::Enter => {
                let title = self.input.trim().to_string();
                if title.is_empty() {
                    self.status = "task title must not be empty".into();
                    return effects;
                }
                match self.active_project() {
                    Some(project) => {
                        effects.push(Effect::SubmitTask { project, title });
                        self.screen = Screen::Main;
                        self.input.clear();
                    }
                    None => {
                        self.status =
                            "cannot determine the project; pass --project to `forge tui`".into();
                    }
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.push(c);
            }
            _ => {}
        }
        effects
    }

    fn handle_main_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
                effects.push(Effect::Quit);
            }
            KeyCode::Char('?') => self.screen = Screen::Help,
            KeyCode::Char('n') => {
                self.screen = Screen::NewTask;
                self.input.clear();
            }
            KeyCode::Char('a') => self.focus = Focus::Agents,
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Tasks => Focus::Output,
                    Focus::Agents => Focus::Tasks,
                    Focus::Output => Focus::Agents,
                }
            }
            KeyCode::Char('r') => effects.push(Effect::Refresh),
            KeyCode::Char('j') | KeyCode::Down => match self.focus {
                Focus::Tasks => {
                    let len = self.visible_tasks().len();
                    if len > 0 && self.task_index + 1 < len {
                        self.task_index += 1;
                    }
                }
                Focus::Agents => {
                    if !self.agents.is_empty() && self.agent_index + 1 < self.agents.len() {
                        self.agent_index += 1;
                    }
                }
                Focus::Output => self.screen_scroll = self.screen_scroll.saturating_add(1),
            },
            KeyCode::Char('k') | KeyCode::Up => match self.focus {
                Focus::Tasks => {
                    self.task_index = self.task_index.saturating_sub(1);
                }
                Focus::Agents => {
                    self.agent_index = self.agent_index.saturating_sub(1);
                }
                Focus::Output => self.screen_scroll = self.screen_scroll.saturating_sub(1),
            },
            KeyCode::Char('g') => {
                if matches!(self.focus, Focus::Tasks) {
                    self.task_index = 0;
                } else if matches!(self.focus, Focus::Agents) {
                    self.agent_index = 0;
                } else {
                    self.screen_scroll = 0;
                }
            }
            KeyCode::Char('G') => {
                if matches!(self.focus, Focus::Tasks) {
                    self.task_index = self.visible_tasks().len().saturating_sub(1);
                } else if matches!(self.focus, Focus::Agents) {
                    self.agent_index = self.agents.len().saturating_sub(1);
                } else {
                    self.screen_scroll = u16::MAX;
                }
            }
            KeyCode::Enter => {
                if matches!(self.focus, Focus::Tasks) {
                    if let Some(task) = self.selected_task() {
                        let id = task.task.id;
                        self.detail = None;
                        effects.push(Effect::FetchDetail(id));
                        self.screen = Screen::Detail;
                        self.screen_scroll = 0;
                    }
                }
            }
            KeyCode::Char('l') => {
                if let Some(task) = self.selected_task() {
                    effects.push(Effect::FetchLogs {
                        task: task.task.id,
                        lines: 400,
                    });
                    self.screen = Screen::Logs;
                    self.screen_scroll = 0;
                } else {
                    self.status = "no task selected".into();
                }
            }
            KeyCode::Char('d') => {
                if let Some(task) = self.selected_task() {
                    effects.push(Effect::FetchDiff(task.task.id));
                    self.screen = Screen::Diff;
                    self.screen_scroll = 0;
                } else {
                    self.status = "no task selected".into();
                }
            }
            KeyCode::Char('x') => {
                if let Some(task) = self.selected_task() {
                    if matches!(
                        task.task.state,
                        TaskState::Completed | TaskState::Failed | TaskState::Cancelled
                    ) {
                        self.status = format!(
                            "{} is {} and cannot be cancelled",
                            task.task.display_id(),
                            task.task.state
                        );
                    } else {
                        self.confirm_cancel = Some((task.task.id, task.task.display_id()));
                    }
                } else {
                    self.status = "no task selected".into();
                }
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.screen_scroll = self.screen_scroll.saturating_add(20)
            }
            KeyCode::PageUp => self.screen_scroll = self.screen_scroll.saturating_sub(20),
            _ => {}
        }
    }
    pub fn fixture() -> App {
        use forge_core::model::{AgentKind, ProbeStatus, Severity, Task, TaskId};
        use forge_protocol::DaemonInfo;

        fn task(id: TaskId, seq: i64, state: TaskState, agent: Option<&str>) -> TaskView {
            TaskView {
                task: Task {
                    id,
                    project_id: 1,
                    seq_no: seq,
                    title: format!("task {seq}"),
                    spec: String::new(),
                    state,
                    priority: 0,
                    required_caps: vec![],
                    claims: vec![],
                    preferred_agent: None,
                    agent_id: agent.map(|s| s.to_string()),
                    worktree_id: None,
                    base_commit: None,
                    timeout_secs: None,
                    attempts: 0,
                    created_at: 0,
                    updated_at: 0,
                    started_at: None,
                    finished_at: None,
                    exit_code: None,
                    failure_reason: None,
                    log_path: None,
                    summary: None,
                    plan_id: None,
                    plan_key: None,
                },
                deps: vec![],
            }
        }

        let mut app = App::new(Some("demo".into()));
        app.apply_snapshot(StateSnapshot {
            daemon: DaemonInfo {
                version: "0.1.0".into(),
                protocol: 1,
                instance_id: "test".into(),
                started_at: 0,
                uptime_ms: 1000,
                endpoint: "tcp:127.0.0.1:1".into(),
                max_concurrency: 2,
                running_tasks: 1,
            },
            projects: vec![Project {
                id: 1,
                name: "demo".into(),
                root: "C:/tmp/demo".into(),
                default_branch: Some("main".into()),
                created_at: 0,
            }],
            tasks: vec![
                task(1, 1, TaskState::Running, Some("sim-a")),
                task(2, 2, TaskState::Completed, Some("sim-b")),
                task(3, 3, TaskState::Failed, None),
            ],
            agents: vec![AgentInfo {
                id: "sim-a".into(),
                kind: AgentKind::Generic,
                command: "@self".into(),
                capabilities: vec!["code".into()],
                slots: 1,
                enabled: true,
                state: forge_core::AgentState::Running,
                running: 1,
                version: Some("0.1.0".into()),
                probe: ProbeStatus::Ok { version: None },
                probed_at: None,
            }],
            events: vec![EventRecord {
                id: 1,
                ts: 0,
                project_id: Some(1),
                task_id: Some(1),
                agent_id: None,
                kind: forge_core::EventKind::TaskCreated,
                severity: Severity::Info,
                message: "T-001 created".into(),
                data: None,
            }],
        });
        app
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn navigation_moves_selection() {
        let mut app = App::fixture();
        assert_eq!(app.selected_task().unwrap().task.id, 1);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected_task().unwrap().task.id, 2);
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected_task().unwrap().task.id, 1);
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.selected_task().unwrap().task.id, 3);
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.selected_task().unwrap().task.id, 1);
    }

    #[test]
    fn tab_cycles_focus() {
        let mut app = App::fixture();
        assert_eq!(app.focus, Focus::Tasks);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Agents);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Output);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Tasks);
    }

    #[test]
    fn enter_opens_detail_and_requests_it() {
        let mut app = App::fixture();
        let effects = app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Detail);
        assert_eq!(effects, vec![Effect::FetchDetail(1)]);
        let effects = app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Main);
        assert!(effects.is_empty());
    }

    #[test]
    fn l_and_d_request_logs_and_diff() {
        let mut app = App::fixture();
        let effects = app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(app.screen, Screen::Logs);
        assert_eq!(
            effects,
            vec![Effect::FetchLogs {
                task: 1,
                lines: 400
            }]
        );
        app.handle_key(key(KeyCode::Esc));
        let effects = app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.screen, Screen::Diff);
        assert_eq!(effects, vec![Effect::FetchDiff(1)]);
    }

    #[test]
    fn x_requires_confirmation_and_terminal_tasks_are_rejected() {
        let mut app = App::fixture();
        // T-001 (running) -> confirmation dialog.
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.confirm_cancel.is_some());
        // 'n' dismisses.
        app.handle_key(key(KeyCode::Char('n')));
        assert!(app.confirm_cancel.is_none());
        // 'y' cancels.
        app.handle_key(key(KeyCode::Char('x')));
        let effects = app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            effects,
            vec![Effect::CancelTask {
                task: 1,
                display: "T-001".into()
            }]
        );
        // Terminal task -> status note, no dialog.
        app.task_index = 1; // T-002 completed
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.confirm_cancel.is_none());
        assert!(app.status.contains("cannot be cancelled"));
    }

    #[test]
    fn input_mode_blocks_shortcuts() {
        let mut app = App::fixture();
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.screen, Screen::NewTask);
        // 'q' must be typed, not treated as quit.
        app.handle_key(key(KeyCode::Char('q')));
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.input, "qx");
        assert!(!app.should_quit);
        assert!(app.confirm_cancel.is_none());
        // Backspace edits.
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "q");
        // Esc leaves input mode.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Main);
    }

    #[test]
    fn submit_task_effect_uses_active_project() {
        let mut app = App::fixture();
        app.handle_key(key(KeyCode::Char('n')));
        for c in "add caching".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let effects = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::SubmitTask {
                project: "demo".into(),
                title: "add caching".into()
            }]
        );
        assert_eq!(app.screen, Screen::Main);
    }

    #[test]
    fn empty_input_is_rejected() {
        let mut app = App::fixture();
        app.handle_key(key(KeyCode::Char('n')));
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(effects.is_empty());
        assert_eq!(app.screen, Screen::NewTask);
        assert!(app.status.contains("must not be empty"));
    }

    #[test]
    fn q_quits() {
        let mut app = App::fixture();
        let effects = app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
        assert_eq!(effects, vec![Effect::Quit]);
    }

    #[test]
    fn task_changed_notification_updates_list() {
        let mut app = App::fixture();
        let mut updated = app.tasks[0].clone();
        updated.task.state = TaskState::Completed;
        let effects = app.apply_notification(Notification::TaskChanged(Box::new(updated)));
        assert!(effects.is_empty());
        assert_eq!(app.tasks[0].task.state, TaskState::Completed);
    }

    #[test]
    fn resync_requests_refresh() {
        let mut app = App::fixture();
        let effects = app.apply_notification(Notification::Resync);
        assert_eq!(effects, vec![Effect::Refresh]);
    }

    #[test]
    fn help_screen_toggles() {
        let mut app = App::fixture();
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.screen, Screen::Help);
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.screen, Screen::Main);
        app.handle_key(key(KeyCode::Char('?')));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Main);
    }
}
