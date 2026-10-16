//! TUI rendering.
//!
//! All rendering is a pure function of [`App`], so the whole UI can be
//! exercised with a ratatui `TestBackend` at any terminal size.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use forge_core::model::{EventRecord, Severity, TaskState};

use super::app::{App, Focus, Screen};
use crate::util::{format_ago, format_hms};

pub const KEYBAR_FULL: &str =
    "n:new  enter:inspect  tab:focus  d:diff  l:logs  x:cancel  r:refresh  ?:help  q:detach";
pub const KEYBAR_MED: &str = "n:new  enter:inspect  d:diff  l:logs  x:cancel  ?:help  q:detach";
pub const KEYBAR_MIN: &str = "n:new  d:diff  l:logs  x:cancel  ?:help  q:detach";
pub const KEYBAR_TINY: &str = "?:help  q:detach";

/// Key bar adapted to the available width; `q:detach` is kept as long as
/// possible because detaching is the most important key to discover.
pub fn keybar_for(width: u16) -> String {
    let width = width as usize;
    for candidate in [KEYBAR_FULL, KEYBAR_MED, KEYBAR_MIN, KEYBAR_TINY] {
        if candidate.chars().count() <= width {
            return candidate.to_string();
        }
    }
    KEYBAR_TINY.chars().take(width).collect()
}

pub fn render(f: &mut Frame, app: &App) {
    match app.screen {
        Screen::Main => render_main(f, app),
        Screen::Help => render_help(f, app),
        Screen::Detail => render_detail(f, app),
        Screen::Logs => render_logs(f, app),
        Screen::Diff => render_diff(f, app),
        Screen::NewTask => {
            render_main(f, app);
            render_new_task_dialog(f, app);
        }
    }
    if app.confirm_cancel.is_some() {
        render_confirm_dialog(f, app);
    }
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

fn render_main(f: &mut Frame, app: &App) {
    let area = f.area();
    let h = area.height;
    let events_h: u16 = if h >= 20 {
        5
    } else if h >= 18 {
        4
    } else {
        0
    };
    let output_h: u16 = if h >= 28 {
        9
    } else if h >= 22 {
        7
    } else if h >= 15 {
        4
    } else {
        0
    };
    let top_h = h.saturating_sub(1 + output_h + events_h + 1).max(3);

    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(top_h),
        Constraint::Length(output_h),
        Constraint::Length(events_h),
        Constraint::Length(1),
    ])
    .split(area);

    render_header(f, chunks[0], app);

    // Tasks and agents side by side when wide enough.
    if area.width >= 90 {
        let split = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(chunks[1]);
        render_tasks(f, split[0], app);
        render_agents(f, split[1], app);
    } else {
        render_tasks(f, chunks[1], app);
    }

    if output_h > 0 {
        render_output(f, chunks[2], app);
    }
    if events_h > 0 {
        render_events(f, chunks[3], app);
    }
    render_status(f, chunks[4], app);
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let project = match &app.project_filter {
        Some(name) => name.clone(),
        None => {
            if app.projects.len() == 1 {
                app.projects[0].name.clone()
            } else {
                format!("{} projects", app.projects.len())
            }
        }
    };
    let branch = app
        .project_filter
        .as_ref()
        .and_then(|name| app.projects.iter().find(|p| &p.name == name))
        .or_else(|| {
            if app.projects.len() == 1 {
                app.projects.first()
            } else {
                None
            }
        })
        .and_then(|p| p.default_branch.clone())
        .unwrap_or_else(|| "-".into());
    let agents = format!("{}/{}", app.running_agents(), app.agents.len());
    let connection = if app.connected {
        Span::styled("connected", Style::default().fg(Color::Green))
    } else {
        Span::styled(
            "OFFLINE",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    };
    let line = Line::from(vec![
        Span::styled("FORGE ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" project: "),
        Span::styled(project, Style::default().fg(Color::Cyan)),
        Span::raw("  branch: "),
        Span::raw(branch),
        Span::raw("  agents: "),
        Span::raw(agents),
        Span::raw("  "),
        connection,
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_tasks(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Tasks && app.screen == Screen::Main;
    let title = if focused {
        " TASKS [focus] "
    } else {
        " TASKS "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let visible = app.visible_tasks();
    if visible.is_empty() {
        let hint = if app.projects.is_empty() {
            "no project registered — run `forge init` in your repository"
        } else {
            "no tasks — press n to create one, or `forge run <plan.toml>`"
        };
        f.render_widget(
            Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let height = inner.height as usize;
    let (start, end) = window(app.task_index, visible.len(), height);
    let now = forge_core::model::now_ms();
    let mut lines: Vec<Line> = Vec::with_capacity(end - start);
    for (row, view) in visible[start..end].iter().enumerate() {
        let index = start + row;
        lines.push(task_line(view, index == app.task_index, focused, now));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn task_line(
    view: &forge_protocol::TaskView,
    selected: bool,
    focused: bool,
    now: i64,
) -> Line<'static> {
    let task = &view.task;
    let marker = if selected {
        if focused {
            "> "
        } else {
            "· "
        }
    } else {
        "  "
    };
    let title_width = 40usize;
    let time = if let Some(started) = task.started_at {
        format_ago(started, task.finished_at.unwrap_or(now))
    } else {
        format_ago(task.created_at, now)
    };
    let mut spans = vec![
        Span::styled(
            marker.to_string(),
            if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        ),
        Span::styled(
            format!("{:<6}", task.display_id()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<9} ", short_state(task.state)),
            state_style(task.state),
        ),
        Span::raw(format!("{:<9} ", task.agent_id.as_deref().unwrap_or("-"))),
        Span::styled(format!("{time:>5} "), Style::default().fg(Color::DarkGray)),
        Span::raw(truncate(&task.title, title_width)),
    ];
    if selected {
        spans.push(Span::styled("".to_string(), Style::default()));
    }
    Line::from(spans)
}

fn render_agents(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Agents && app.screen == Screen::Main;
    let title = if focused {
        " AGENTS [focus] "
    } else {
        " AGENTS "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.agents.is_empty() {
        f.render_widget(
            Paragraph::new("no agents configured (add [[agents]] to config.toml)")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }
    let height = inner.height as usize;
    let (start, end) = window(app.agent_index, app.agents.len(), height);
    let mut lines = vec![];
    for (row, agent) in app.agents[start..end].iter().enumerate() {
        let index = start + row;
        let selected = index == app.agent_index;
        let marker = if selected {
            if focused {
                "> "
            } else {
                "· "
            }
        } else {
            "  "
        };
        lines.push(Line::from(vec![
            Span::styled(marker.to_string(), Style::default()),
            Span::styled(
                format!("{:<12}", agent.id),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<9} ", format!("{}/{}", agent.running, agent.slots)),
                Style::default(),
            ),
            Span::styled(agent.state.as_str().to_string(), agent_style(agent.state)),
        ]));
    }
    if end < app.agents.len() {
        lines.push(Line::from(Span::styled(
            format!("  … {} more", app.agents.len() - end),
            Style::default().fg(Color::DarkGray),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_output(f: &mut Frame, area: Rect, app: &App) {
    let (title, task_label) = match app.selected_task() {
        Some(view) => (
            format!(
                " OUTPUT: {} / {} ",
                view.task.display_id(),
                view.task.agent_id.as_deref().unwrap_or("-")
            ),
            Some(view.task.id),
        ),
        None => (" OUTPUT ".to_string(), None),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if task_label.is_none() {
        f.render_widget(
            Paragraph::new("select a task to follow its output")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }
    if app.output_task == task_label && !app.output_text.is_empty() {
        let height = inner.height as usize;
        let lines: Vec<&str> = app.output_text.lines().collect();
        let drop = lines.len().saturating_sub(height);
        let text: Vec<Line> = lines[drop..]
            .iter()
            .map(|l| Line::from(Span::raw(l.to_string())))
            .collect();
        f.render_widget(Paragraph::new(Text::from(text)), inner);
    } else {
        f.render_widget(
            Paragraph::new("(waiting for output…)").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
    }
}

fn render_events(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" EVENTS [{}] ", app.events.len()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.events.is_empty() {
        f.render_widget(
            Paragraph::new("no events yet").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }
    let height = inner.height as usize;
    let skip = app.events.len().saturating_sub(height);
    let lines: Vec<Line> = app
        .events
        .iter()
        .skip(skip)
        .map(|event| event_line(event, app))
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn event_line(event: &EventRecord, app: &App) -> Line<'static> {
    let task_label = event
        .task_id
        .and_then(|id| {
            app.tasks
                .iter()
                .find(|v| v.task.id == id)
                .map(|v| v.task.display_id())
        })
        .unwrap_or_else(|| "     ".into());
    let style = match event.severity {
        Severity::Info => Style::default(),
        Severity::Warn => Style::default().fg(Color::Yellow),
        Severity::Error => Style::default().fg(Color::Red),
    };
    Line::from(vec![
        Span::styled(
            format!("{} ", format_hms(event.ts)),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("{task_label:<6} "),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(event.message.clone(), style),
    ])
}

fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    if app.status.is_empty() {
        let bar = keybar_for(area.width);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                bar,
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }
    // Status on the right (always visible), key bar on the left.
    let status = truncate(&app.status, width.saturating_sub(12).max(8));
    let key_space = width.saturating_sub(status.chars().count() + 2);
    let bar = keybar_for(key_space as u16);
    let gap = width.saturating_sub(bar.chars().count() + status.chars().count());
    let line = Line::from(vec![
        Span::styled(bar, Style::default().fg(Color::DarkGray)),
        Span::raw(" ".repeat(gap)),
        Span::styled(status, Style::default().fg(Color::Yellow)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

// ---------------------------------------------------------------------------
// Full-screen views
// ---------------------------------------------------------------------------

fn render_screen_frame<'a>(title: &'a str) -> Block<'a> {
    Block::default().borders(Borders::ALL).title(title)
}

fn render_help(f: &mut Frame, app: &App) {
    let block = render_screen_frame(" HELP ");
    let inner = block.inner(f.area());
    f.render_widget(block, f.area());
    let lines = vec![
        Line::from(Span::styled(
            "Forge terminal UI — keyboard reference",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  j / k, ↑ / ↓     move selection in the focused pane"),
        Line::from("  tab / shift-tab  cycle focus: tasks → agents → output"),
        Line::from("  g / G            jump to top / bottom"),
        Line::from("  enter            inspect the selected task"),
        Line::from("  n                create a task in the active project"),
        Line::from("  a                focus the agents pane"),
        Line::from("  l                open the log of the selected task"),
        Line::from("  d                open the git diff of the selected task"),
        Line::from("  x                cancel the selected task (asks for confirmation)"),
        Line::from("  r                refresh the state from the daemon"),
        Line::from("  ?                toggle this help"),
        Line::from("  q                detach the TUI (the daemon keeps working)"),
        Line::from(""),
        Line::from("  log/diff screens: j/k scroll, space/pgdn page, g/G top/bottom, esc back"),
        Line::from(""),
        Line::from(format!(
            "  task states: {}",
            TaskState::ALL
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("  ")
        )),
        Line::from(""),
        Line::from(Span::styled(
            "All data shown here comes from the running daemon.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(lines), inner);
    let _ = app;
}

fn render_detail(f: &mut Frame, app: &App) {
    let title = match &app.detail {
        Some(detail) => format!(" {} — {} ", detail.task.display_id(), detail.task.title),
        None => " TASK ".to_string(),
    };
    let block = render_screen_frame(title.as_str());
    let inner = block.inner(f.area());
    f.render_widget(block, f.area());

    let Some(detail) = &app.detail else {
        f.render_widget(
            Paragraph::new("loading…").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };
    let task = &detail.task;
    let mut lines = vec![
        kv("id", &task.display_id()),
        kv("state", &format!("{}", task.state)),
        kv("agent", task.agent_id.as_deref().unwrap_or("-")),
        kv("deps", &format!("{:?}", detail.deps)),
        kv("created", &format_hms(task.created_at)),
        kv("attempts", &task.attempts.to_string()),
        kv("exit code", &format!("{:?}", task.exit_code)),
        kv("failure", task.failure_reason.as_deref().unwrap_or("-")),
        kv("summary", task.summary.as_deref().unwrap_or("-")),
        kv(
            "worktree",
            &detail
                .worktree
                .as_ref()
                .map(|w| format!("{} ({})", w.path.display(), w.branch))
                .unwrap_or_else(|| "-".into()),
        ),
        kv("log", task.log_path.as_deref().unwrap_or("-")),
        Line::from(""),
        Line::from(Span::styled(
            "artifacts:",
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];
    for artifact in &detail.artifacts {
        let value = artifact.value.as_deref().unwrap_or("-");
        lines.push(Line::from(format!(
            "  {:<16} {}",
            artifact.kind,
            truncate(&value.replace('\n', " | "), 90)
        )));
    }
    if detail.artifacts.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (none yet)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "sessions:",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for session in &detail.sessions {
        lines.push(Line::from(format!(
            "  #{} {} pid={:?} exit={:?}",
            session.id,
            session.state.as_str(),
            session.pid,
            session.exit_code
        )));
    }
    if detail.sessions.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (none yet)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "recent events:",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for event in detail.events.iter().rev().take(20).rev() {
        lines.push(event_line(event, app));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[l] log   [d] diff   [r] refresh   [esc] back",
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.screen_scroll, 0)),
        inner,
    );
}

fn render_logs(f: &mut Frame, app: &App) {
    let title = match app.log_task.and_then(|id| {
        app.tasks
            .iter()
            .find(|v| v.task.id == id)
            .map(|v| v.task.display_id())
    }) {
        Some(id) => format!(" LOG {id} "),
        None => " LOG ".to_string(),
    };
    let mut block = render_screen_frame(title.as_str());
    if app.log_truncated {
        block = block.title_bottom(Span::styled(
            " tail only ",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let inner = block.inner(f.area());
    f.render_widget(block, f.area());
    match &app.log_text {
        Some(text) if !text.is_empty() => {
            f.render_widget(
                Paragraph::new(text.clone())
                    .wrap(Wrap { trim: false })
                    .scroll((app.screen_scroll, 0)),
                inner,
            );
        }
        Some(_) => {
            f.render_widget(
                Paragraph::new("(log is empty)").style(Style::default().fg(Color::DarkGray)),
                inner,
            );
        }
        None => {
            f.render_widget(
                Paragraph::new("loading…").style(Style::default().fg(Color::DarkGray)),
                inner,
            );
        }
    }
}

fn render_diff(f: &mut Frame, app: &App) {
    let title = match app.diff_task.and_then(|id| {
        app.tasks
            .iter()
            .find(|v| v.task.id == id)
            .map(|v| v.task.display_id())
    }) {
        Some(id) => format!(" DIFF {id} "),
        None => " DIFF ".to_string(),
    };
    let block = render_screen_frame(title.as_str());
    let inner = block.inner(f.area());
    f.render_widget(block, f.area());
    match &app.diff_text {
        Some(text) if !text.trim().is_empty() => {
            f.render_widget(
                Paragraph::new(text.clone())
                    .wrap(Wrap { trim: false })
                    .scroll((app.screen_scroll, 0)),
                inner,
            );
        }
        Some(_) => {
            f.render_widget(
                Paragraph::new("(no changes in this worktree)")
                    .style(Style::default().fg(Color::DarkGray)),
                inner,
            );
        }
        None => {
            f.render_widget(
                Paragraph::new("loading…").style(Style::default().fg(Color::DarkGray)),
                inner,
            );
        }
    }
}

fn render_new_task_dialog(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 70, 5);
    f.render_widget(Clear, area);
    let block = Block::default().borders(Borders::ALL).title(" NEW TASK ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let project = app.active_project().unwrap_or_else(|| "?".into());
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!("project: {project}  "),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled("task title:", Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Green)),
            Span::raw(app.input.clone()),
            Span::styled("█", Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(Span::styled(
            "enter: submit   esc: cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_confirm_dialog(f: &mut Frame, app: &App) {
    let Some((_, display)) = &app.confirm_cancel else {
        return;
    };
    let area = centered(f.area(), 70, 5);
    f.render_widget(Clear, area);
    let block = Block::default().borders(Borders::ALL).title(" CONFIRM ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let lines = vec![
        Line::from(vec![
            Span::styled("Cancel ", Style::default()),
            Span::styled(
                display.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("?", Style::default()),
        ]),
        Line::from(Span::styled(
            "the agent process is terminated; the worktree is retained",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "y: cancel task    n/esc: dismiss",
            Style::default().fg(Color::Yellow),
        )),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Visible window of a list keeping `selected` in view.
pub fn window(selected: usize, len: usize, height: usize) -> (usize, usize) {
    if len == 0 || height == 0 {
        return (0, 0);
    }
    let height = height.min(len);
    let start = if selected < height {
        0
    } else {
        (selected + 1).saturating_sub(height)
    };
    let start = start.min(len - height);
    (start, start + height)
}

pub fn short_state(state: TaskState) -> &'static str {
    match state {
        TaskState::Created => "PENDING",
        TaskState::Ready => "QUEUED",
        TaskState::Leased => "LEASED",
        TaskState::Running => "RUNNING",
        TaskState::ReviewPending => "REVIEW",
        TaskState::Blocked => "BLOCKED",
        TaskState::Completed => "DONE",
        TaskState::Failed => "FAILED",
        TaskState::Cancelled => "CANCELLED",
        TaskState::Interrupted => "INTERRUPTED",
    }
}

pub fn state_style(state: TaskState) -> Style {
    let color = match state {
        TaskState::Created => Color::DarkGray,
        TaskState::Ready => Color::Yellow,
        TaskState::Leased => Color::Cyan,
        TaskState::Running => Color::Green,
        TaskState::ReviewPending => Color::Cyan,
        TaskState::Blocked => Color::Magenta,
        TaskState::Completed => Color::Blue,
        TaskState::Failed => Color::Red,
        TaskState::Cancelled => Color::DarkGray,
        TaskState::Interrupted => Color::Red,
    };
    Style::default().fg(color)
}

fn agent_style(state: forge_core::AgentState) -> Style {
    let color = match state {
        forge_core::AgentState::Idle => Color::DarkGray,
        forge_core::AgentState::Running => Color::Green,
        forge_core::AgentState::Offline => Color::Red,
        forge_core::AgentState::Disabled => Color::DarkGray,
    };
    Style::default().fg(color)
}

fn kv(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<12}"), Style::default().fg(Color::DarkGray)),
        Span::raw(value.to_string()),
    ])
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn centered(area: Rect, width_pct: u16, height: u16) -> Rect {
    let width = (area.width * width_pct / 100).max(24).min(area.width);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let height = height.min(area.height);
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_keeps_selection_visible() {
        assert_eq!(window(0, 10, 5), (0, 5));
        assert_eq!(window(4, 10, 5), (0, 5));
        assert_eq!(window(5, 10, 5), (1, 6));
        assert_eq!(window(9, 10, 5), (5, 10));
        assert_eq!(window(0, 3, 5), (0, 3));
        assert_eq!(window(0, 0, 5), (0, 0));
        assert_eq!(window(2, 10, 0), (0, 0));
    }

    #[test]
    fn centered_rect_stays_inside() {
        let area = Rect::new(0, 0, 80, 24);
        let r = centered(area, 70, 5);
        assert!(r.width <= 80 && r.x + r.width <= 80);
        assert!(r.y + r.height <= 24);
        let tiny = Rect::new(0, 0, 20, 4);
        let r = centered(tiny, 70, 5);
        assert!(r.width <= 20 && r.height <= 4);
    }

    #[test]
    fn truncate_respects_width() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a very long title", 8).chars().count(), 8);
    }
}
