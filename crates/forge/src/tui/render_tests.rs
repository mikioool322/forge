//! Rendering tests with a ratatui `TestBackend` at multiple terminal sizes,
//! including degraded layouts, empty states, long logs and dialogs.

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use super::app::{App, Screen};
use super::ui;

fn render_to_text(app: &App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| ui::render(frame, app)).expect("draw");
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn overview_renders_at_80x24() {
    let app = App::fixture();
    let text = render_to_text(&app, 80, 24);
    assert!(text.contains("FORGE"), "{text}");
    assert!(text.contains("project: demo"));
    assert!(text.contains("TASKS"));
    assert!(text.contains("T-001"));
    assert!(text.contains("RUNNING"));
    assert!(text.contains("EVENTS"));
    assert!(text.contains("n:new"));
    // 80 columns is too narrow for the agent pane: it degrades gracefully.
    assert!(!text.contains("AGENTS"));
}

#[test]
fn overview_renders_at_100x30_with_agents() {
    let app = App::fixture();
    let text = render_to_text(&app, 100, 30);
    assert!(text.contains("AGENTS"), "{text}");
    assert!(text.contains("sim-a"));
    assert!(text.contains("TASKS"));
    assert!(text.contains("OUTPUT"));
}

#[test]
fn overview_renders_at_140x40() {
    let app = App::fixture();
    let text = render_to_text(&app, 140, 40);
    assert!(text.contains("FORGE"));
    assert!(text.contains("AGENTS"));
    assert!(text.contains("sim-a"));
    assert!(text.contains("OUTPUT"));
    assert!(text.contains("T-002"));
}

#[test]
fn tiny_terminal_degrades_without_panicking() {
    let app = App::fixture();
    let text = render_to_text(&app, 60, 10);
    assert!(text.contains("TASKS"), "{text}");
    // No events/output panes at this height, but the key bar stays visible.
    assert!(!text.contains("EVENTS"));
    assert!(text.contains("q:detach"));
    let _ = render_to_text(&app, 40, 6);
    let _ = render_to_text(&app, 20, 4);
}

#[test]
fn empty_state_has_guidance() {
    let app = App::new(None);
    let text = render_to_text(&app, 100, 30);
    assert!(text.contains("no project registered"), "{text}");
    assert!(text.contains("OFFLINE"));
}

#[test]
fn disconnected_banner_is_visible() {
    let mut app = App::fixture();
    app.connected = false;
    app.status = "daemon disconnected: refused; reconnecting…".into();
    let text = render_to_text(&app, 100, 30);
    assert!(text.contains("OFFLINE"), "{text}");
    assert!(text.contains("reconnecting"));
}

#[test]
fn help_screen_lists_keybindings() {
    let mut app = App::fixture();
    app.screen = Screen::Help;
    let text = render_to_text(&app, 100, 30);
    assert!(text.contains("HELP"));
    assert!(text.contains("keyboard reference"));
    assert!(text.contains("detach the TUI"));
}

#[test]
fn long_logs_render_the_tail_and_scroll() {
    let mut app = App::fixture();
    app.screen = Screen::Logs;
    app.log_task = Some(1);
    let long: String = (0..5000).map(|i| format!("line {i}\n")).collect();
    app.log_text = Some(long);
    let text = render_to_text(&app, 100, 30);
    assert!(text.contains("LOG T-001"));
    // Lines beyond the viewport must not be rendered (no giant buffer).
    assert!(!text.contains("line 4999"));
    app.screen_scroll = 4000;
    let scrolled = render_to_text(&app, 100, 30);
    assert!(scrolled.contains("line 40"), "{scrolled}");
}

#[test]
fn detail_screen_renders_task_fields() {
    let mut app = App::fixture();
    app.screen = Screen::Detail;
    let task = app.tasks[0].task.clone();
    app.detail = Some(forge_protocol::TaskDetail {
        task: task.clone(),
        deps: vec![],
        worktree: None,
        artifacts: vec![forge_core::TaskArtifact {
            id: 1,
            task_id: task.id,
            kind: "git_diff_stat".into(),
            path: None,
            value: Some("1 file changed".into()),
            created_at: 0,
        }],
        sessions: vec![],
        events: vec![],
    });
    let text = render_to_text(&app, 100, 30);
    assert!(text.contains("T-001"));
    assert!(text.contains("git_diff_stat"));
    assert!(text.contains("1 file changed"));
    assert!(text.contains("[l] log"));
}

#[test]
fn diff_screen_renders() {
    let mut app = App::fixture();
    app.screen = Screen::Diff;
    app.diff_task = Some(1);
    app.diff_text =
        Some("# branch forge/demo/t-001\n\nalpha.txt | 2 ++\n 1 file changed\n+generated\n".into());
    let text = render_to_text(&app, 100, 30);
    assert!(text.contains("DIFF T-001"));
    assert!(text.contains("alpha.txt"));
}

#[test]
fn new_task_dialog_shows_input() {
    let mut app = App::fixture();
    app.screen = Screen::NewTask;
    app.input = "add caching layer".into();
    let text = render_to_text(&app, 100, 30);
    assert!(text.contains("NEW TASK"));
    assert!(text.contains("add caching layer"));
    assert!(text.contains("enter: submit"));
}

#[test]
fn confirm_dialog_shows_selected_task() {
    let mut app = App::fixture();
    app.confirm_cancel = Some((1, "T-001".into()));
    let text = render_to_text(&app, 100, 30);
    assert!(text.contains("CONFIRM"));
    assert!(text.contains("Cancel T-001?"));
    assert!(text.contains("y: cancel task"));
}

#[test]
fn output_pane_shows_selected_task_output() {
    let mut app = App::fixture();
    app.output_task = Some(1);
    app.output_text = "21:00:01 reading pricing module\n21:00:02 editing pricing service".into();
    let text = render_to_text(&app, 120, 35);
    assert!(text.contains("OUTPUT: T-001 / sim-a"), "{text}");
    assert!(text.contains("editing pricing service"));
}
