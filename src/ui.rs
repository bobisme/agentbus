//! Native, system-wide roster over agentbus's query model.
//!
//! The daemon still renders nothing. This is a short-lived reader, like
//! `sessions` and `wait`, with one crucial default: only process-verified
//! sessions are shown. Transcript history and stale hook reports remain
//! available behind `--all` / `a`, but cannot masquerade as running agents.

use crate::query::{self, Locations, Presence, Session};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Terminal;
use serde_json::Value;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal, Stdout};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const REFRESH: Duration = Duration::from_millis(500);

pub fn run(args: &[String], locations: &Locations) -> i32 {
    let show_all = args.iter().any(|arg| arg == "--all");
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("agentbus ui [--all]\n\n  --all  include unverified and historical records");
        return 0;
    }

    let mut app = App::new(locations.clone(), show_all);
    app.refresh();
    if let Err(e) = run_terminal(&mut app) {
        eprintln!("agentbus ui: {e}");
        return 1;
    }
    0
}

struct UiTerminal {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl UiTerminal {
    fn enter() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stdin and stdout must both be terminals",
            ));
        }
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(e) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                Err(e)
            }
        }
    }
}

impl Drop for UiTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn run_terminal(app: &mut App) -> io::Result<()> {
    let stopped = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&stopped))?;
    flag::register(SIGTERM, Arc::clone(&stopped))?;
    let mut ui = UiTerminal::enter()?;
    let mut next_refresh = Instant::now();
    loop {
        if Instant::now() >= next_refresh {
            app.refresh();
            next_refresh = Instant::now() + REFRESH;
        }
        ui.terminal.draw(|frame| draw(frame, app))?;
        if stopped.load(AtomicOrdering::Relaxed) {
            return Ok(());
        }

        let wait = next_refresh.saturating_duration_since(Instant::now());
        if !event::poll(wait)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.code == KeyCode::Char('c')
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            return Ok(());
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Down | KeyCode::Char('j') => app.select_next(),
            KeyCode::Up | KeyCode::Char('k') => app.select_previous(),
            KeyCode::Char('a') => {
                app.show_all = !app.show_all;
                app.rebuild_visible();
            }
            KeyCode::Char('c') => app.compact = !app.compact,
            KeyCode::Char('r') => app.refresh(),
            _ => {}
        }
    }
}

struct App {
    locations: Locations,
    all: Vec<Session>,
    visible: Vec<usize>,
    selected: usize,
    selected_id: String,
    show_all: bool,
    compact: bool,
    error: String,
}

impl App {
    fn new(locations: Locations, show_all: bool) -> Self {
        Self {
            locations,
            all: Vec::new(),
            visible: Vec::new(),
            selected: 0,
            selected_id: String::new(),
            show_all,
            compact: false,
            error: String::new(),
        }
    }

    fn refresh(&mut self) {
        let Some(all) = query::all(&self.locations) else {
            self.error = format!(
                "cannot read snapshot at {}; retrying",
                self.locations.snapshot.display()
            );
            return;
        };
        self.error.clear();
        self.all = all;
        self.all.sort_by(compare_sessions);
        self.rebuild_visible();
    }

    fn rebuild_visible(&mut self) {
        self.visible = visible_indices(&self.all, self.show_all);

        if !self.selected_id.is_empty() {
            if let Some(i) = self
                .visible
                .iter()
                .position(|&row| self.all[row].id == self.selected_id)
            {
                self.selected = i;
            }
        }
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
        self.remember_selection();
    }

    fn select_next(&mut self) {
        if self.selected + 1 < self.visible.len() {
            self.selected += 1;
            self.remember_selection();
        }
    }

    fn select_previous(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.remember_selection();
        }
    }

    fn remember_selection(&mut self) {
        self.selected_id = self
            .visible
            .get(self.selected)
            .map(|&i| self.all[i].id.clone())
            .unwrap_or_default();
    }

    fn selected(&self) -> Option<&Session> {
        self.visible
            .get(self.selected)
            .and_then(|&i| self.all.get(i))
    }
}

/// Select one canonical row per verified process.
///
/// A hook-only identity can coexist with the real transcript-backed session
/// for the same exact pid. Both are useful raw records, but rendering both says
/// two agents are running when only one process exists. Unverified records have
/// no exact identity and therefore are never guessed together.
fn visible_indices(sessions: &[Session], show_all: bool) -> Vec<usize> {
    let mut by_process: BTreeMap<(u64, u64), usize> = BTreeMap::new();
    for (i, session) in sessions.iter().enumerate() {
        if session.presence() != Presence::Verified {
            continue;
        }
        by_process
            .entry((session.pid, session.starttime))
            .and_modify(|current| {
                if richness(session) > richness(&sessions[*current]) {
                    *current = i;
                }
            })
            .or_insert(i);
    }
    let canonical: BTreeSet<usize> = by_process.into_values().collect();
    sessions
        .iter()
        .enumerate()
        .filter(|(i, session)| {
            canonical.contains(i) || (show_all && session.presence() == Presence::Unverified)
        })
        .map(|(i, _)| i)
        .collect()
}

fn richness(session: &Session) -> (bool, bool, bool, bool, bool) {
    (
        session.text("source") != "hook",
        !session.text("state").is_empty(),
        !session.text("last_activity").is_empty(),
        !session.text("label").is_empty() || !session.text("title").is_empty(),
        !session.transcript.is_empty(),
    )
}

fn compare_sessions(a: &Session, b: &Session) -> Ordering {
    presence_rank(a)
        .cmp(&presence_rank(b))
        .then_with(|| location(a).cmp(&location(b)))
        .then_with(|| a.text("cwd").cmp(b.text("cwd")))
        .then_with(|| a.id.cmp(&b.id))
}

fn presence_rank(s: &Session) -> u8 {
    match s.presence() {
        Presence::Verified => 0,
        Presence::Unverified => 1,
    }
}

fn draw(frame: &mut ratatui::Frame, app: &App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(6),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let verified_rows = visible_indices(&app.all, false);
    let verified = verified_rows.len();
    let hidden = app
        .all
        .iter()
        .filter(|s| s.presence() == Presence::Unverified)
        .count();
    let count_state = |state: &str| {
        verified_rows
            .iter()
            .filter(|&&i| app.all[i].text("state") == state)
            .count()
    };
    let mut header_spans = vec![
        Span::styled(" AGENTBUS ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("{} blocked", count_state("blocked")),
            Style::default().fg(Color::LightRed),
        ),
        Span::raw("   "),
        Span::styled(
            format!("{} working", count_state("working")),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("   "),
        Span::styled(
            format!("{} idle", count_state("idle")),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(format!("   {verified} verified · {hidden} unverified")),
    ];
    if !app.error.is_empty() {
        header_spans.push(Span::styled(
            format!("   {}", clean(&app.error)),
            Style::default().fg(Color::LightRed),
        ));
    }
    let header = Line::from(header_spans);
    frame.render_widget(
        Paragraph::new(header).block(Block::default().borders(Borders::BOTTOM)),
        areas[0],
    );

    let items: Vec<ListItem> = if app.visible.is_empty() {
        vec![ListItem::new(Line::styled(
            if app.show_all {
                "no agent records"
            } else {
                "no verified agents running — press a to show unverified history"
            },
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.visible
            .iter()
            .map(|&i| session_item(&app.all[i], app.compact))
            .collect()
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_symbol("▌")
        .highlight_style(Style::default().bg(Color::Rgb(35, 42, 52)));
    let mut state = ListState::default();
    if !app.visible.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, areas[1], &mut state);

    frame.render_widget(detail(app.selected()), areas[2]);
    let mode = if app.show_all { "all" } else { "verified" };
    let density = if app.compact { "compact" } else { "normal" };
    frame.render_widget(
        Paragraph::new(format!(
            " j/k move · a {mode} · c {density} · r refresh · q quit"
        ))
        .style(Style::default().fg(Color::DarkGray)),
        areas[3],
    );
}

fn session_item(s: &Session, compact: bool) -> ListItem<'static> {
    let state = s.text("state");
    let state_label = clean(nonempty(state, "unknown"));
    let (glyph, color) = state_style(state);
    let source_raw = nonempty(s.text("source"), "agent");
    let source = clean(source_raw);
    let label = [s.text("label"), s.text("title"), s.text("cwd")]
        .into_iter()
        .find(|v| !v.is_empty())
        .unwrap_or("untitled");
    let label = clean(label);
    let presence = if s.presence() == Presence::Verified {
        String::new()
    } else {
        "  unverified".to_string()
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!(" {glyph} "), Style::default().fg(color)),
        Span::styled(
            format!("{source:<9}"),
            Style::default().fg(source_color(source_raw)),
        ),
        Span::styled(format!("{state_label:<8}"), Style::default().fg(color)),
        Span::raw(label),
        Span::styled(presence, Style::default().fg(Color::DarkGray)),
    ])];

    if !compact {
        let stats = session_stats(s);
        if !stats.is_empty() {
            lines.push(Line::styled(
                format!("     {stats}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }

    let subs = s
        .state
        .get("subagents")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for (i, sub) in subs.iter().enumerate() {
        let done = sub.get("state").and_then(Value::as_str) == Some("done");
        let branch = if i + 1 == subs.len() { "└" } else { "├" };
        let agent_type = clean(value_text(sub, "agent_type", "subagent"));
        let description = if done {
            value_text(sub, "result", value_text(sub, "description", "finished"))
        } else {
            value_text(sub, "description", "working")
        };
        let description = clean(description);
        lines.push(Line::from(vec![
            Span::styled(
                format!("     {branch} "),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                if done { "✓ " } else { "● " },
                Style::default().fg(if done { Color::Cyan } else { Color::Yellow }),
            ),
            Span::styled(agent_type, Style::default().fg(Color::LightBlue)),
            Span::raw(format!("  {description}")),
        ]));
    }
    ListItem::new(lines)
}

fn detail(selected: Option<&Session>) -> Paragraph<'static> {
    let Some(s) = selected else {
        return Paragraph::new("").block(Block::default().borders(Borders::TOP));
    };
    let loc = clean(&location(s));
    let model = clean(&model_effort(s.text("model"), s.text("effort")));
    let lines = vec![
        Line::from(vec![
            Span::styled(" cwd  ", Style::default().fg(Color::DarkGray)),
            Span::raw(clean(nonempty(s.text("cwd"), "—"))),
        ]),
        Line::from(vec![
            Span::styled(" at   ", Style::default().fg(Color::DarkGray)),
            Span::raw(if loc.is_empty() { "—".into() } else { loc }),
            Span::styled("    pid  ", Style::default().fg(Color::DarkGray)),
            Span::raw(if s.pid > 0 {
                s.pid.to_string()
            } else {
                "—".into()
            }),
            Span::styled("    model  ", Style::default().fg(Color::DarkGray)),
            Span::raw(if model.is_empty() { "—" } else { &model }.to_string()),
        ]),
        Line::from(vec![
            Span::styled(" id   ", Style::default().fg(Color::DarkGray)),
            Span::raw(clean(&s.id)),
            Span::styled("    presence  ", Style::default().fg(Color::DarkGray)),
            Span::raw(s.presence().label()),
        ]),
    ];
    Paragraph::new(lines)
        .block(Block::default().borders(Borders::TOP).title(" selected "))
        .wrap(Wrap { trim: true })
}

fn session_stats(s: &Session) -> String {
    let mut parts = Vec::new();
    let since = s.number("state_since");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if since > 0 && now >= since {
        parts.push(human_secs(now - since));
    }
    let tools = s.number("tools");
    if tools > 0 {
        parts.push(format!("{tools} tool{}", if tools == 1 { "" } else { "s" }));
    }
    if s.text("state") == "working" && !s.text("last_tool").is_empty() {
        parts.push(clean(s.text("last_tool")));
    }
    let context = s.pointer_number("/tokens/context");
    if context > 0 {
        parts.push(format!("{} ctx", human_count(context)));
    }
    let model = model_effort(s.text("model"), s.text("effort"));
    if !model.is_empty() {
        parts.push(clean(&model));
    }
    let loc = location(s);
    if !loc.is_empty() {
        parts.push(clean(&loc));
    }
    parts.join(" · ")
}

fn location(s: &Session) -> String {
    let mux = s.pointer_text("/location/mux");
    let session = s.pointer_text("/location/session");
    let pane = s.pointer_text("/location/pane");
    if pane.is_empty() {
        String::new()
    } else {
        format!("{mux}/{session}/{pane}")
    }
}

fn state_style(state: &str) -> (&'static str, Color) {
    match state {
        "blocked" => ("◆", Color::LightRed),
        "working" => ("●", Color::Yellow),
        "idle" => ("○", Color::DarkGray),
        _ => ("·", Color::DarkGray),
    }
}

fn source_color(source: &str) -> Color {
    match source {
        "claude" => Color::LightYellow,
        "codex" => Color::LightGreen,
        "agy" => Color::LightMagenta,
        "opencode" => Color::LightBlue,
        _ => Color::Gray,
    }
}

fn model_effort(model: &str, effort: &str) -> String {
    let model = model
        .trim_start_matches("claude-")
        .trim_end_matches("-latest");
    match (model.is_empty(), effort.is_empty()) {
        (true, _) => String::new(),
        (false, true) => model.to_string(),
        (false, false) => format!("{model} {effort}"),
    }
}

fn human_secs(n: u64) -> String {
    match n {
        0..=59 => format!("{n}s"),
        60..=3599 => format!("{}m{:02}s", n / 60, n % 60),
        _ => format!("{}h{:02}m", n / 3600, (n % 3600) / 60),
    }
}

fn human_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

fn nonempty<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() {
        fallback
    } else {
        value
    }
}

/// Snapshot text can contain model-authored terminal controls. A status reader
/// must display those as data, never execute them in the user's terminal.
fn clean(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else if c.is_control() {
                '�'
            } else {
                c
            }
        })
        .collect()
}

fn value_text<'a>(value: &'a Value, key: &str, fallback: &'a str) -> &'a str {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use serde_json::json;
    use std::path::PathBuf;

    fn session(id: &str, pid: u64, location: &str) -> Session {
        Session {
            id: id.into(),
            pid,
            starttime: if pid > 0 { 7 } else { 0 },
            transcript: String::new(),
            state: json!({
                "state": "working",
                "cwd": "/tmp/work",
                "location": {"mux": "zellij", "session": "main", "pane": location},
            }),
        }
    }

    #[test]
    fn verified_sessions_sort_before_unverified_history() {
        let mut sessions = [session("old", 0, ""), session("live", 9, "2")];
        sessions.sort_by(compare_sessions);
        assert_eq!(sessions[0].id, "live");
    }

    #[test]
    fn location_is_empty_without_a_pane() {
        assert_eq!(location(&session("old", 0, "")), "");
        assert_eq!(location(&session("live", 9, "2")), "zellij/main/2");
    }

    #[test]
    fn duplicate_live_process_prefers_the_transcript_backed_session() {
        let thin = Session {
            id: "hook-row".into(),
            pid: 9,
            starttime: 7,
            transcript: String::new(),
            state: json!({"source": "hook", "state": "", "location": {}}),
        };
        let rich = Session {
            id: "real-row".into(),
            pid: 9,
            starttime: 7,
            transcript: "/tmp/transcript.jsonl".into(),
            state: json!({"source": "codex", "state": "working", "location": {}}),
        };
        let sessions = [thin, rich];
        let visible = visible_indices(&sessions, false);
        assert_eq!(visible, vec![1]);
    }

    #[test]
    fn strips_terminal_controls_from_model_text() {
        assert_eq!(clean("hello\n\u{1b}[31m"), "hello �[31m");
    }

    #[test]
    fn narrow_terminal_render_does_not_panic() {
        let mut app = App::new(
            Locations {
                snapshot: PathBuf::new(),
                log: PathBuf::new(),
                register: PathBuf::new(),
                completions: PathBuf::new(),
            },
            false,
        );
        app.all = vec![session("live", 9, "2")];
        app.rebuild_visible();
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
    }
}
