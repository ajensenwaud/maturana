//! Console TUI for chatting with a running Maturana agent
//! (`maturana agent chat <id>`). This is the local "console TUI" surface
//! declared by `channels.tui`. It's a full-screen terminal app — scrollable
//! conversation history, multiline input, slash-command autocomplete, a
//! thinking spinner, and a status header — modeled on Hermes-style agent TUIs.
//!
//! Each turn enqueues the message into `sessiond` and waits for the agent's
//! reply on a background thread (the same round-trip `agent run --wait` uses),
//! so the UI stays responsive while the guest worker answers.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use maturana_core::spec::{AgentSpec, HarnessRuntime};
use maturana_core::state::MaturanaHome;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

#[derive(Clone, Copy, PartialEq)]
enum Role {
    User,
    Agent,
    System,
}

struct ChatMsg {
    role: Role,
    text: String,
}

// Slash commands are sourced from `channels::console_command_catalog()` so the
// console TUI stays at parity with the Telegram command menu.

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

struct App {
    home: MaturanaHome,
    /// All selectable agents (sorted); `current` indexes into this.
    agents: Vec<String>,
    current: usize,
    agent_id: String,
    session_id: String,
    harness: String,
    timeout_seconds: u64,
    messages: Vec<ChatMsg>,
    input: String,
    /// Lines scrolled UP from the bottom; 0 = pinned to newest.
    scrollback: u16,
    awaiting: bool,
    spinner: usize,
    waited: Option<Instant>,
    reply_rx: Option<mpsc::Receiver<Result<String, String>>>,
    show_slash: bool,
    slash_sel: usize,
    /// When Some, the agent-selector HUD is open; the value is the highlighted row.
    selector: Option<usize>,
    quit: bool,
}

fn harness_of(home: &MaturanaHome, agent_id: &str) -> String {
    AgentSpec::from_maturana_markdown(&home.agent_dir(agent_id).join("MATURANA.md"))
        .map(|spec| match spec.runtime.harness {
            HarnessRuntime::Codex => "codex",
            HarnessRuntime::ClaudeCode => "claude-code",
            HarnessRuntime::Opencode => "opencode",
        })
        .unwrap_or("unknown")
        .to_string()
}

impl App {
    fn new(
        home: &MaturanaHome,
        agents: Vec<String>,
        current: usize,
        open_selector: bool,
        timeout_seconds: u64,
    ) -> Self {
        let mut app = Self {
            home: MaturanaHome::new(home.root().to_path_buf()),
            agents,
            current,
            agent_id: String::new(),
            session_id: String::new(),
            harness: String::new(),
            timeout_seconds,
            messages: Vec::new(),
            input: String::new(),
            scrollback: 0,
            awaiting: false,
            spinner: 0,
            waited: None,
            reply_rx: None,
            show_slash: false,
            slash_sel: 0,
            selector: if open_selector { Some(current) } else { None },
            quit: false,
        };
        app.load_current();
        app.messages.push(ChatMsg {
            role: Role::System,
            text: format!(
                "Connected to agent '{}' ({} harness). Type a message and press Enter. \
                 /help for commands · Ctrl+P agents · Ctrl+←/→ switch · Esc or Ctrl+C quit.",
                app.agent_id, app.harness
            ),
        });
        app
    }

    /// Load the agent at `current` into agent_id/session_id/harness.
    fn load_current(&mut self) {
        let agent_id = self.agents.get(self.current).cloned().unwrap_or_default();
        self.harness = harness_of(&self.home, &agent_id);
        // Match the agent's guest worker session so commands (status, /session,
        // feedback) target the same queue the turns use.
        self.session_id = crate::infer_agent_session_id(&self.home, &agent_id)
            .unwrap_or_else(|_| "telegram-main".to_string());
        self.agent_id = agent_id;
    }

    /// Switch the active agent: abandon any in-flight reply, reload identity, and
    /// reset the conversation view.
    fn switch_to(&mut self, index: usize) {
        if self.agents.is_empty() {
            return;
        }
        self.current = index.min(self.agents.len() - 1);
        self.finish_wait();
        self.load_current();
        self.messages.clear();
        self.input.clear();
        self.show_slash = false;
        self.scrollback = 0;
        self.messages.push(ChatMsg {
            role: Role::System,
            text: format!(
                "▸ agent '{}' ({}/{}, {} harness). Ctrl+P to switch.",
                self.agent_id,
                self.current + 1,
                self.agents.len(),
                self.harness
            ),
        });
    }

    /// Cycle to the previous/next agent (wrapping).
    fn cycle(&mut self, delta: i32) {
        let n = self.agents.len();
        if n < 2 {
            return;
        }
        let next = (self.current as i32 + delta).rem_euclid(n as i32) as usize;
        self.switch_to(next);
    }

    fn slash_matches(&self) -> Vec<(&'static str, &'static str)> {
        let q = self.input.trim();
        crate::channels::console_command_catalog()
            .into_iter()
            .filter(|(name, _)| name.starts_with(q))
            .collect()
    }

    /// Interrupt the in-flight turn: drop the receiver so the orphaned worker
    /// thread's eventual result is discarded, and stop waiting locally. Used for
    /// both a bare interrupt (Esc / Ctrl+X) and interrupt-and-redirect (a new
    /// message sent while a reply is pending).
    fn interrupt(&mut self, redirecting: bool) {
        if !self.awaiting {
            return;
        }
        self.reply_rx = None;
        self.awaiting = false;
        self.waited = None;
        self.messages.push(ChatMsg {
            role: Role::System,
            text: if redirecting {
                "↪ interrupted the previous turn — redirecting".to_string()
            } else {
                "✕ interrupted (the previous turn was abandoned)".to_string()
            },
        });
    }

    fn submit(&mut self) {
        let text = self.input.trim_end().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        self.show_slash = false;
        self.scrollback = 0;

        if text.starts_with('/') {
            // Slash commands run through the shared channel command pipeline
            // (parity with Telegram) without disturbing an in-flight turn.
            self.handle_slash(&text);
            return;
        }
        self.send_turn(text);
    }

    /// Send `text` to the agent as a normal turn, interrupting any in-flight
    /// reply (interrupt-and-redirect).
    fn send_turn(&mut self, text: String) {
        if self.awaiting {
            self.interrupt(true);
        }
        self.messages.push(ChatMsg {
            role: Role::User,
            text: text.clone(),
        });
        // Round-trip on a background thread so the UI keeps animating.
        let (tx, rx) = mpsc::channel();
        let home = MaturanaHome::new(self.home.root().to_path_buf());
        let agent_id = self.agent_id.clone();
        let timeout = self.timeout_seconds;
        thread::spawn(move || {
            let result = crate::agent_chat_turn(&home, &agent_id, &text, timeout)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        self.reply_rx = Some(rx);
        self.awaiting = true;
        self.waited = Some(Instant::now());
    }

    fn handle_slash(&mut self, cmd: &str) {
        use crate::channels::ConsoleCommand;
        // The shared dispatcher reuses the exact handlers the Telegram channel
        // uses, so the console TUI exposes the same command set.
        match crate::channels::run_console_command(&self.home, &self.agent_id, &self.session_id, cmd)
        {
            ConsoleCommand::Reply(text) => self.messages.push(ChatMsg {
                role: Role::System,
                text,
            }),
            ConsoleCommand::Prompt(text) => self.send_turn(text),
            ConsoleCommand::Clear => self.messages.clear(),
            ConsoleCommand::NewSession => {
                self.messages.clear();
                self.messages.push(ChatMsg {
                    role: Role::System,
                    text: "New session started.".to_string(),
                });
            }
            ConsoleCommand::Quit => self.quit = true,
        }
    }

    fn poll_reply(&mut self) {
        if let Some(rx) = &self.reply_rx {
            match rx.try_recv() {
                Ok(Ok(text)) => {
                    self.messages.push(ChatMsg {
                        role: Role::Agent,
                        text,
                    });
                    self.finish_wait();
                }
                Ok(Err(err)) => {
                    self.messages.push(ChatMsg {
                        role: Role::System,
                        text: format!("⚠ no reply: {err}"),
                    });
                    self.finish_wait();
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.messages.push(ChatMsg {
                        role: Role::System,
                        text: "⚠ reply worker stopped unexpectedly".to_string(),
                    });
                    self.finish_wait();
                }
            }
        }
    }

    fn finish_wait(&mut self) {
        self.reply_rx = None;
        self.awaiting = false;
        self.waited = None;
        self.scrollback = 0;
    }
}

/// `maturana agent chat <id>` — jump straight into the TUI for one agent.
pub fn run_chat(home: &MaturanaHome, agent_id: &str, timeout_seconds: u64) -> Result<()> {
    run_tui(home, Some(agent_id), timeout_seconds)
}

/// `maturana tui [agent]` — open the console TUI. With no agent the agent-selector
/// HUD opens first; otherwise it starts in that agent. Ctrl+P reopens the selector
/// and Ctrl+←/→ cycle between agents.
pub fn run_tui(home: &MaturanaHome, agent_id: Option<&str>, timeout_seconds: u64) -> Result<()> {
    let mut agents = crate::discover_agent_ids(home)?;
    if let Some(id) = agent_id {
        if !agents.iter().any(|a| a == id) {
            // Allow targeting an agent whose dir exists but wasn't discovered
            // (e.g. just created); put it first.
            agents.insert(0, id.to_string());
        }
    }
    if agents.is_empty() {
        anyhow::bail!(
            "no agents found under {}. Create one from Codex (the maturana-agent-create skill), \
             then run `maturana tui`.",
            home.agents_dir().display()
        );
    }
    let current = agent_id
        .and_then(|id| agents.iter().position(|a| a == id))
        .unwrap_or(0);
    // No agent on the command line → open the selector first.
    let open_selector = agent_id.is_none();
    let mut app = App::new(home, agents, current, open_selector, timeout_seconds);

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        app.poll_reply();
        terminal.draw(|f| draw(f, app))?;
        if app.quit {
            return Ok(());
        }
        // Short poll so the spinner animates and replies surface promptly.
        if event::poll(Duration::from_millis(120))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                // While the agent-selector HUD is open it captures navigation.
                if let Some(sel) = app.selector {
                    match key.code {
                        KeyCode::Char('c') if ctrl => app.quit = true,
                        KeyCode::Up => app.selector = Some(sel.saturating_sub(1)),
                        KeyCode::Down => {
                            app.selector = Some((sel + 1).min(app.agents.len().saturating_sub(1)));
                        }
                        KeyCode::Enter => {
                            app.selector = None;
                            app.switch_to(sel);
                        }
                        KeyCode::Esc => app.selector = None,
                        KeyCode::Char('p') if ctrl => app.selector = None,
                        _ => {}
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Char('c') if ctrl => app.quit = true,
                    KeyCode::Char('x') if ctrl => app.interrupt(false),
                    KeyCode::Char('p') if ctrl => app.selector = Some(app.current),
                    KeyCode::Left if ctrl => app.cycle(-1),
                    KeyCode::Right if ctrl => app.cycle(1),
                    KeyCode::Esc => {
                        if app.show_slash {
                            app.show_slash = false;
                        } else if app.awaiting {
                            // Esc interrupts an in-flight turn first; press again
                            // (when idle) to quit.
                            app.interrupt(false);
                        } else {
                            app.quit = true;
                        }
                    }
                    KeyCode::Char('j') if ctrl => app.input.push('\n'),
                    KeyCode::Enter if alt => app.input.push('\n'),
                    KeyCode::Enter => {
                        if app.show_slash {
                            let matches = app.slash_matches();
                            if let Some((name, _)) = matches.get(app.slash_sel) {
                                app.input = name.to_string();
                                app.show_slash = false;
                            }
                        }
                        app.submit();
                    }
                    KeyCode::Tab if app.show_slash => {
                        let matches = app.slash_matches();
                        if let Some((name, _)) = matches.get(app.slash_sel) {
                            app.input = format!("{name} ");
                            app.show_slash = false;
                        }
                    }
                    KeyCode::Up if app.show_slash => {
                        app.slash_sel = app.slash_sel.saturating_sub(1);
                    }
                    KeyCode::Down if app.show_slash => {
                        let n = app.slash_matches().len().saturating_sub(1);
                        app.slash_sel = (app.slash_sel + 1).min(n);
                    }
                    KeyCode::PageUp => app.scrollback = app.scrollback.saturating_add(5),
                    KeyCode::PageDown => app.scrollback = app.scrollback.saturating_sub(5),
                    KeyCode::Backspace => {
                        app.input.pop();
                        app.show_slash = app.input.starts_with('/');
                        app.slash_sel = 0;
                    }
                    KeyCode::Char(c) => {
                        app.input.push(c);
                        if app.input.starts_with('/') && !app.input.contains(' ') {
                            app.show_slash = true;
                            app.slash_sel = 0;
                        } else {
                            app.show_slash = false;
                        }
                    }
                    _ => {}
                }
            }
        } else if app.awaiting {
            app.spinner = (app.spinner + 1) % SPINNER.len();
        }
    }
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(3),    // transcript
            Constraint::Length(5), // input
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    draw_header(f, chunks[0], app);
    draw_transcript(f, chunks[1], app);
    draw_input(f, chunks[2], app);
    draw_footer(f, chunks[3], app);

    if app.show_slash {
        draw_slash_popup(f, chunks[2], app);
    }
    if let Some(sel) = app.selector {
        draw_agent_selector(f, f.area(), app, sel);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let status = if app.awaiting {
        let secs = app.waited.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        Span::styled(
            format!(" {} thinking… {}s ", SPINNER[app.spinner], secs),
            Style::default().fg(Color::Yellow),
        )
    } else {
        Span::styled(" ● ready ", Style::default().fg(Color::Green))
    };
    let line = Line::from(vec![
        Span::styled(
            format!(
                " maturana · {} ({}/{}) ",
                app.agent_id,
                app.current + 1,
                app.agents.len()
            ),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  {}  ", app.harness)),
        status,
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_transcript(f: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    for msg in &app.messages {
        let (label, color) = match msg.role {
            Role::User => ("you", Color::Cyan),
            Role::Agent => (app.agent_id.as_str(), Color::Green),
            Role::System => ("·", Color::DarkGray),
        };
        let mut first = true;
        for raw in msg.text.split('\n') {
            if first {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{label}: "),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(raw.to_string()),
                ]));
                first = false;
            } else {
                lines.push(Line::from(Span::raw(format!("    {raw}"))));
            }
        }
        lines.push(Line::from(""));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" conversation ")
        .border_style(Style::default().fg(Color::DarkGray));
    let inner_w = area.width.saturating_sub(2).max(1);
    let inner_h = area.height.saturating_sub(2).max(1);
    // Estimate wrapped rows (char-count based) so the view pins to the newest
    // message; `Paragraph::line_count` is private in ratatui 0.29.
    let total: u16 = lines
        .iter()
        .map(|l| {
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            ((w + inner_w as usize - 1) / inner_w as usize).max(1) as u16
        })
        .sum::<u16>()
        .max(1);
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    // Pin to bottom, then apply any manual scrollback.
    let max_off = total.saturating_sub(inner_h);
    let offset = max_off.saturating_sub(app.scrollback.min(max_off));
    f.render_widget(para.scroll((offset, 0)), area);
}

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let title = if app.awaiting {
        " message (waiting for reply…) "
    } else {
        " message "
    };
    let shown = format!("{}\u{2588}", app.input); // trailing block cursor
    let para = Paragraph::new(shown)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn draw_footer(f: &mut Frame, area: Rect, _app: &App) {
    let hint = Line::from(vec![Span::styled(
        " Enter send · Alt+Enter newline · / commands · Ctrl+P agents · \
         Ctrl+←/→ switch · PgUp/PgDn scroll · Esc/Ctrl+X interrupt · Ctrl+C quit ",
        Style::default().fg(Color::DarkGray),
    )]);
    f.render_widget(Paragraph::new(hint), area);
}

/// Centered agent-selector HUD: lists every agent, marks the active one (●),
/// highlights the cursor row. Opened at startup (no agent arg) and via Ctrl+P.
fn draw_agent_selector(f: &mut Frame, full: Rect, app: &App, sel: usize) {
    let rows = app.agents.len() as u16;
    let height = (rows + 2).min(full.height.saturating_sub(2)).max(3);
    let width = 52.min(full.width.saturating_sub(4)).max(24);
    let area = Rect {
        x: full.x + full.width.saturating_sub(width) / 2,
        y: full.y + full.height.saturating_sub(height) / 2,
        width,
        height,
    };
    let items: Vec<ListItem> = app
        .agents
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let marker = if i == app.current { "● " } else { "  " };
            let harness = harness_of(&app.home, id);
            let style = if i == sel {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {marker}{id} "),
                    style.add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {harness}"), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" select agent · ↑↓ Enter · Esc cancel ")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(Clear, area);
    f.render_widget(list, area);
}

fn draw_slash_popup(f: &mut Frame, input_area: Rect, app: &App) {
    let matches = app.slash_matches();
    if matches.is_empty() {
        return;
    }
    // The menu floats ABOVE the input, so size it to the space available there
    // (rows from the top of the screen down to the input), not the input's own
    // 5-row height — otherwise only ~3 of the ~20 commands showed (just the
    // first entries /help /clear /quit), making it look like the real commands
    // were missing. +2 for the border; show the whole list when it fits.
    let space_above = input_area.y.max(3);
    let height = (matches.len() as u16 + 2).min(space_above);
    let area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width: input_area.width.min(48),
        height,
    };
    let items: Vec<ListItem> = matches
        .iter()
        .map(|(name, desc)| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {name} "),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {desc}"), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    // ListState drives highlight + auto-scroll, so the selected command is always
    // in view even when the list is taller than the popup (short terminals).
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(app.slash_sel.min(matches.len().saturating_sub(1))));
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" commands ")
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, &mut state);
}
