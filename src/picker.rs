//! Interactive fuzzy-searchable TUI host picker built on ratatui + nucleo.

use anyhow::Result;
use std::collections::HashMap;
use std::time::Duration;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::model::{Host, HostSource};
use crate::tmux::TmuxStatus;
use crate::util::relative_time;

/// Picker application state.
struct App {
    hosts: Vec<Host>,
    /// alias -> active session? (`None` = unreachable). Absent = still probing.
    statuses: HashMap<String, Option<bool>>,
    query: String,
    /// Indices into `hosts`, in display order.
    filtered: Vec<usize>,
    /// Index into `filtered` of the highlighted row.
    selected: usize,
    list_state: ListState,
    matcher: Matcher,
}

impl App {
    fn new(hosts: Vec<Host>) -> App {
        let mut app = App {
            hosts,
            statuses: HashMap::new(),
            query: String::new(),
            filtered: Vec::new(),
            selected: 0,
            list_state: ListState::default(),
            matcher: Matcher::new(NucleoConfig::DEFAULT),
        };
        app.recompute();
        app
    }

    /// Recompute the filtered/sorted index list from the current query.
    fn recompute(&mut self) {
        if self.query.is_empty() {
            let mut idx: Vec<usize> = (0..self.hosts.len()).collect();
            idx.sort_by(|&a, &b| {
                let la = self.hosts[a].state.last_connected;
                let lb = self.hosts[b].state.last_connected;
                lb.cmp(&la)
                    .then_with(|| self.hosts[a].alias.cmp(&self.hosts[b].alias))
            });
            self.filtered = idx;
        } else {
            let pattern =
                Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
            let mut buf = Vec::new();
            let mut scored: Vec<(usize, u32)> = Vec::new();
            for (i, h) in self.hosts.iter().enumerate() {
                let hay = h.haystack();
                let utf = Utf32Str::new(&hay, &mut buf);
                if let Some(score) = pattern.score(utf, &mut self.matcher) {
                    scored.push((i, score));
                }
            }
            scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| {
                self.hosts[a.0].alias.cmp(&self.hosts[b.0].alias)
            }));
            self.filtered = scored.into_iter().map(|(i, _)| i).collect();
        }

        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn move_down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
    }

    /// Alias of the currently highlighted host, if any.
    fn selected_alias(&self) -> Option<String> {
        let idx = *self.filtered.get(self.selected)?;
        Some(self.hosts[idx].alias.clone())
    }
}

/// Run the picker. Returns `Some(alias)` if the user chose a host, `None` if
/// they cancelled. Consumes background status updates from `rx`.
pub fn run_picker(
    hosts: Vec<Host>,
    mut rx: UnboundedReceiver<TmuxStatus>,
) -> Result<Option<String>> {
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, App::new(hosts), &mut rx);
    ratatui::restore();
    result
}

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    mut app: App,
    rx: &mut UnboundedReceiver<TmuxStatus>,
) -> Result<Option<String>> {
    loop {
        // Drain any completed status probes.
        while let Ok(status) = rx.try_recv() {
            app.statuses.insert(status.alias, status.active);
        }

        app.list_state.select(if app.filtered.is_empty() {
            None
        } else {
            Some(app.selected)
        });
        terminal.draw(|frame| ui(frame, &mut app))?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::Char('c') if ctrl => return Ok(None),
                KeyCode::Char('g') if ctrl => return Ok(None),
                KeyCode::Enter => return Ok(app.selected_alias()),
                KeyCode::Up => app.move_up(),
                KeyCode::Down => app.move_down(),
                KeyCode::Char('p') if ctrl => app.move_up(),
                KeyCode::Char('n') if ctrl => app.move_down(),
                KeyCode::Backspace => {
                    app.query.pop();
                    app.recompute();
                }
                KeyCode::Char('u') if ctrl => {
                    app.query.clear();
                    app.recompute();
                }
                KeyCode::Char(c) => {
                    app.query.push(c);
                    app.recompute();
                }
                _ => {}
            }
        }
    }
}

fn ui(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(3)])
        .split(frame.area());

    // Compute alias column width for alignment.
    let alias_w = app
        .hosts
        .iter()
        .map(|h| h.alias.chars().count())
        .max()
        .unwrap_or(8)
        .clamp(8, 28);

    let items: Vec<ListItem> = app
        .filtered
        .iter()
        .map(|&i| {
            let host = &app.hosts[i];
            let (dot, dot_style, tag) = status_visual(app.statuses.get(&host.alias));

            let mut spans = vec![
                Span::styled(format!("{dot} "), dot_style),
                Span::styled(
                    format!("{:<width$}", host.alias, width = alias_w),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(host.endpoint(), Style::default().fg(Color::Cyan)),
            ];
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                relative_time(host.state.last_connected),
                Style::default().fg(Color::DarkGray),
            ));
            if host.state.connection_count > 0 {
                spans.push(Span::styled(
                    format!("  ×{}", host.state.connection_count),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if !tag.is_empty() {
                spans.push(Span::styled(
                    format!("  [{tag}]"),
                    Style::default().fg(Color::Green),
                ));
            }
            if host.source == HostSource::KnownHosts {
                spans.push(Span::styled(
                    format!("  ({})", host.source.label()),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            // Notes from TOML config take precedence over DB-stored notes.
            let notes = host.meta.notes.as_deref().or(host.state.notes.as_deref());
            if let Some(notes) = notes {
                spans.push(Span::styled(
                    format!("  — {notes}"),
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let title = format!(
        " ssht — {} host(s)  (↑/↓ move · Enter connect · Esc quit) ",
        app.hosts.len()
    );
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, chunks[0], &mut app.list_state);

    let prompt = Line::from(vec![
        Span::styled("❯ ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw(app.query.as_str()),
        Span::styled("▏", Style::default().fg(Color::Green)),
    ]);
    let input = Paragraph::new(prompt).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" search ({} matches) ", app.filtered.len())),
    );
    frame.render_widget(input, chunks[1]);
}

/// Map a probe status to (symbol, style, tag).
fn status_visual(status: Option<&Option<bool>>) -> (&'static str, Style, &'static str) {
    match status {
        None => ("…", Style::default().fg(Color::DarkGray), ""), // still probing
        Some(Some(true)) => (
            "●",
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            "tmux",
        ),
        Some(Some(false)) => ("○", Style::default().fg(Color::DarkGray), ""),
        Some(None) => ("⨯", Style::default().fg(Color::Red), ""), // unreachable
    }
}
