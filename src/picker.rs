//! Interactive fuzzy-searchable TUI host picker built on ratatui + nucleo.
//!
//! The picker is modal, in the spirit of vim / lazygit / k9s:
//!   * **Normal** mode — navigate with `j`/`k` (or arrows), `g`/`G` for
//!     top/bottom, `Ctrl-d`/`Ctrl-u` to page, `/` or `s` to start searching.
//!   * **Search** mode — type to fuzzy-filter; `Esc` returns to Normal while
//!     keeping the filter. Arrows / `Ctrl-n`/`Ctrl-p` still move the selection.

use anyhow::Result;
use std::collections::HashMap;
use std::time::Duration;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Margin};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::model::{Host, HostSource};
use crate::tmux::TmuxStatus;
use crate::util::relative_time;

// ---- Theme -----------------------------------------------------------------

const ACCENT: Color = Color::Cyan;
const ACTIVE: Color = Color::Green;
const DIM: Color = Color::DarkGray;
const SEARCH_HL: Color = Color::Yellow;

/// Input mode of the picker.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Search,
}

/// Picker application state.
struct App {
    hosts: Vec<Host>,
    /// alias -> active session? (`None` = unreachable). Absent = still probing.
    statuses: HashMap<String, Option<bool>>,
    query: String,
    mode: Mode,
    /// Indices into `hosts`, in display order.
    filtered: Vec<usize>,
    /// Index into `filtered` of the highlighted row.
    selected: usize,
    list_state: ListState,
    matcher: Matcher,
    /// Number of visible list rows, updated each render (for paging).
    viewport: usize,
}

impl App {
    fn new(hosts: Vec<Host>) -> App {
        let mut app = App {
            hosts,
            statuses: HashMap::new(),
            query: String::new(),
            mode: Mode::Normal,
            filtered: Vec::new(),
            selected: 0,
            list_state: ListState::default(),
            matcher: Matcher::new(NucleoConfig::DEFAULT),
            viewport: 10,
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
            scored.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then_with(|| self.hosts[a.0].alias.cmp(&self.hosts[b.0].alias))
            });
            self.filtered = scored.into_iter().map(|(i, _)| i).collect();
        }

        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    /// Move the selection by `delta` rows, clamped to the list bounds.
    fn move_by(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let max = self.filtered.len() as isize - 1;
        let next = (self.selected as isize + delta).clamp(0, max);
        self.selected = next as usize;
    }

    fn to_top(&mut self) {
        self.selected = 0;
    }

    fn to_bottom(&mut self) {
        self.selected = self.filtered.len().saturating_sub(1);
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
            if let Some(outcome) = handle_key(&mut app, key.code, key.modifiers) {
                return Ok(outcome);
            }
        }
    }
}

/// Process a key press. Returns `Some(result)` when the picker should exit
/// (`Some(Some(alias))` = chosen, `Some(None)` = cancelled), or `None` to keep
/// running.
fn handle_key(
    app: &mut App,
    code: KeyCode,
    mods: KeyModifiers,
) -> Option<Option<String>> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let half_page = (app.viewport / 2).max(1) as isize;

    // Keys common to both modes.
    match code {
        KeyCode::Enter => return Some(app.selected_alias()),
        KeyCode::Char('c' | 'g') if ctrl => return Some(None),
        KeyCode::Up => app.move_by(-1),
        KeyCode::Down => app.move_by(1),
        KeyCode::Char('p') if ctrl => app.move_by(-1),
        KeyCode::Char('n') if ctrl => app.move_by(1),
        KeyCode::PageUp => app.move_by(-(app.viewport as isize)),
        KeyCode::PageDown => app.move_by(app.viewport as isize),
        KeyCode::Char('d') if ctrl => app.move_by(half_page),
        KeyCode::Char('u') if ctrl && app.mode == Mode::Normal => app.move_by(-half_page),
        _ => return handle_mode_key(app, code, ctrl),
    }
    None
}

/// Mode-specific keys (everything not handled by the common bindings).
fn handle_mode_key(app: &mut App, code: KeyCode, ctrl: bool) -> Option<Option<String>> {
    match app.mode {
        Mode::Normal => match code {
            KeyCode::Esc | KeyCode::Char('q') => return Some(None),
            KeyCode::Char('j') => app.move_by(1),
            KeyCode::Char('k') => app.move_by(-1),
            KeyCode::Char('g') => app.to_top(),
            KeyCode::Char('G') => app.to_bottom(),
            KeyCode::Char('l') => return Some(app.selected_alias()),
            KeyCode::Char('/' | 's' | 'i') => app.mode = Mode::Search,
            _ => {}
        },
        Mode::Search => match code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Backspace => {
                app.query.pop();
                app.recompute();
            }
            KeyCode::Char('u') if ctrl => {
                app.query.clear();
                app.recompute();
            }
            KeyCode::Char('w') if ctrl => {
                trim_last_word(&mut app.query);
                app.recompute();
            }
            KeyCode::Char(c) => {
                app.query.push(c);
                app.recompute();
            }
            _ => {}
        },
    }
    None
}

/// Delete the trailing word (and trailing whitespace) from the query.
fn trim_last_word(query: &mut String) {
    while query.ends_with(char::is_whitespace) {
        query.pop();
    }
    while query.chars().next_back().is_some_and(|c| !c.is_whitespace()) {
        query.pop();
    }
}

// ---- Rendering -------------------------------------------------------------

fn ui(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(3)])
        .split(frame.area());

    // Track the visible row count for paging (minus borders).
    app.viewport = (chunks[0].height.saturating_sub(2)) as usize;

    render_list(frame, app, chunks[0]);
    render_input(frame, app, chunks[1]);
}

fn render_list(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    // Column width for aligning aliases.
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
        .map(|&i| row(&app.hosts[i], app.statuses.get(&app.hosts[i].alias), alias_w))
        .collect();

    let left = Line::from(vec![
        Span::raw(" "),
        Span::styled("ssht", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled("  SSH + tmux ", Style::default().fg(DIM)),
    ]);
    let right = Line::from(Span::styled(
        format!(" {}/{} hosts ", app.filtered.len(), app.hosts.len()),
        Style::default().fg(DIM),
    ))
    .alignment(Alignment::Right);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(left)
        .title(right);

    if app.filtered.is_empty() {
        let empty = Paragraph::new(Line::from(Span::styled(
            "no matching hosts",
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        )))
        .alignment(Alignment::Center)
        .block(block);
        frame.render_widget(empty, area);
        return;
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(ACCENT)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌ ")
        .highlight_spacing(ratatui::widgets::HighlightSpacing::Always);
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

/// Build one list row for a host.
fn row(host: &Host, status: Option<&Option<bool>>, alias_w: usize) -> ListItem<'static> {
    let (dot, dot_style, tag) = status_visual(status);

    let mut spans = vec![
        Span::styled(format!("{dot} "), dot_style),
        Span::styled(
            format!("{:<width$}", host.alias, width = alias_w),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(host.endpoint(), Style::default().fg(ACCENT)),
        Span::raw("  "),
        Span::styled(
            relative_time(host.state.last_connected),
            Style::default().fg(DIM),
        ),
    ];

    if host.state.connection_count > 0 {
        spans.push(Span::styled(
            format!("  ×{}", host.state.connection_count),
            Style::default().fg(DIM),
        ));
    }
    if !tag.is_empty() {
        spans.push(Span::styled(
            format!("  {tag}"),
            Style::default().fg(ACTIVE).add_modifier(Modifier::BOLD),
        ));
    }
    if host.source == HostSource::KnownHosts {
        spans.push(Span::styled(
            format!("  ({})", host.source.label()),
            Style::default().fg(DIM),
        ));
    }
    // Notes from TOML config take precedence over DB-stored notes.
    let notes = host.meta.notes.as_deref().or(host.state.notes.as_deref());
    if let Some(notes) = notes {
        spans.push(Span::styled(
            format!("  — {notes}"),
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        ));
    }

    ListItem::new(Line::from(spans))
}

fn render_input(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let (badge_label, badge_color, hints) = match app.mode {
        Mode::Normal => (
            " NORMAL ",
            ACCENT,
            "j/k move · g/G top/bottom · / search · ⏎ connect · q quit",
        ),
        Mode::Search => (
            " SEARCH ",
            SEARCH_HL,
            "type to filter · esc normal · ↑/↓ move · ⏎ connect",
        ),
    };

    let title = Line::from(vec![
        Span::styled(
            badge_label,
            Style::default()
                .bg(badge_color)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(hints, Style::default().fg(DIM)),
        Span::raw(" "),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(badge_color))
        .title(title);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Prompt line: a colored chevron, the query, and (in search mode) a cursor.
    let mut spans = vec![Span::styled(
        "❯ ",
        Style::default().fg(badge_color).add_modifier(Modifier::BOLD),
    )];
    if app.query.is_empty() && app.mode == Mode::Normal {
        spans.push(Span::styled(
            "press / to search…",
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        ));
    } else {
        spans.push(Span::raw(app.query.clone()));
        if app.mode == Mode::Search {
            spans.push(Span::styled("█", Style::default().fg(badge_color)));
        }
    }

    let prompt = Paragraph::new(Line::from(spans));
    frame.render_widget(prompt, inner.inner(Margin::new(1, 0)));
}

/// Map a probe status to (symbol, style, tag).
fn status_visual(status: Option<&Option<bool>>) -> (&'static str, Style, &'static str) {
    match status {
        None => ("•", Style::default().fg(DIM), ""), // still probing
        Some(Some(true)) => (
            "●",
            Style::default().fg(ACTIVE).add_modifier(Modifier::BOLD),
            "tmux",
        ),
        Some(Some(false)) => ("○", Style::default().fg(DIM), ""),
        Some(None) => ("⨯", Style::default().fg(Color::Red), ""), // unreachable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HostMeta;
    use crate::state::HostState;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn sample_hosts() -> Vec<Host> {
        vec![
            Host {
                alias: "prod-web".into(),
                hostname: Some("10.0.0.1".into()),
                user: Some("deploy".into()),
                port: None,
                source: HostSource::SshConfig,
                meta: HostMeta { notes: Some("primary".into()), ..Default::default() },
                state: HostState { connection_count: 3, ..Default::default() },
            },
            Host {
                alias: "db".into(),
                hostname: Some("db.internal".into()),
                user: None,
                port: None,
                source: HostSource::KnownHosts,
                meta: HostMeta::default(),
                state: HostState::default(),
            },
        ]
    }

    #[test]
    fn trims_last_word() {
        let mut q = String::from("prod web");
        trim_last_word(&mut q);
        assert_eq!(q, "prod ");
        trim_last_word(&mut q);
        assert_eq!(q, "");
    }

    #[test]
    fn vim_navigation_in_normal_mode() {
        let mut app = App::new(sample_hosts());
        assert_eq!(app.selected, 0);
        handle_key(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.selected, 1);
        handle_key(&mut app, KeyCode::Char('j'), KeyModifiers::NONE); // clamp at end
        assert_eq!(app.selected, 1);
        handle_key(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.selected, 0);
        handle_key(&mut app, KeyCode::Char('G'), KeyModifiers::NONE);
        assert_eq!(app.selected, 1);
        handle_key(&mut app, KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn slash_enters_search_and_filters() {
        let mut app = App::new(sample_hosts());
        assert_eq!(app.mode as u8, Mode::Normal as u8);
        handle_key(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
        assert_eq!(app.mode as u8, Mode::Search as u8);
        // In search mode, 'j' is a literal character, not navigation.
        for c in "db".chars() {
            handle_key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.query, "db"); // 'j'/'d'/'b' typed literally, not navigation
        // Exact "db" ranks the "db" host at the top.
        assert_eq!(app.selected_alias().as_deref(), Some("db"));
        let filtered_with_query = app.filtered.len();
        // Esc returns to normal but keeps the filter.
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.mode as u8, Mode::Normal as u8);
        assert_eq!(app.filtered.len(), filtered_with_query);
    }

    #[test]
    fn enter_returns_selection_and_q_cancels() {
        let mut app = App::new(sample_hosts());
        let chosen = handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(chosen, Some(Some("db".to_string()))); // db sorts first (recency tie -> alphabetical)
        let cancelled = handle_key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(cancelled, Some(None));
    }

    #[test]
    fn renders_without_panicking() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::new(sample_hosts());
        terminal.draw(|f| ui(f, &mut app)).expect("draw");

        let buf = terminal.backend().buffer();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("ssht"));
        assert!(text.contains("NORMAL"));
        assert!(text.contains("prod-web"));
        assert!(text.contains("deploy@10.0.0.1"));
    }
}
