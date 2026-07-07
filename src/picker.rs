//! Interactive fuzzy-searchable TUI host picker built on ratatui + nucleo.
//!
//! Modes:
//!   * **Normal** — navigate, `⏎` connect, `e` settings, `q` quit.
//!   * **Search** — type to fuzzy-filter; `Esc` returns to Normal.
//!   * **VaultUnlock** — enter vault passphrase, shown as an in-TUI popup
//!     (triggered when connecting to, editing, or saving settings for a host
//!     that needs a locked vault).
//!   * **Settings** — edit name, address, username, password for a host.

use anyhow::Result;
use std::collections::HashMap;
use std::time::Duration;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Margin};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::model::{Host, HostSource};
use crate::tmux::TmuxStatus;
use crate::util::relative_time;
use crate::vault::{HostSettings, LazyVault, Vault};

const ACCENT: Color = Color::Cyan;
const ACTIVE: Color = Color::Green;
const DIM: Color = Color::DarkGray;
const SEARCH_HL: Color = Color::Yellow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Search,
    VaultUnlock,
    Settings,
}

#[derive(Clone)]
struct SettingsField {
    label: &'static str,
    value: String,
    saved: String,
    secret: bool,
}

struct SettingsData {
    fields: Vec<SettingsField>,
    /// Field values as loaded when this settings screen was opened, used to
    /// detect real changes on exit (rather than a stateful dirty flag, which
    /// stayed true even if an edit was typed and then undone).
    originals: Vec<String>,
    selected: usize,
    editing: bool,
}

/// What to do once the vault unlock popup succeeds.
#[derive(Clone)]
enum VaultUnlockAction {
    /// Re-enter settings mode to reload the password field for `host_idx`.
    ReenterSettings,
    /// Save these settings for `alias`.
    Save(String, HostSettings),
    /// Exit the picker and connect to `alias`.
    Connect(String),
}

struct VaultUnlockData {
    passphrase: String,
    error: Option<String>,
    host_idx: usize,
    action: VaultUnlockAction,
}

struct App {
    hosts: Vec<Host>,
    statuses: HashMap<String, Option<bool>>,
    query: String,
    mode: Mode,
    filtered: Vec<usize>,
    selected: usize,
    list_state: ListState,
    matcher: Matcher,
    viewport: usize,
    settings: Option<SettingsData>,
    vault_unlock: Option<VaultUnlockData>,
    /// Set by `try_connect` after a vault unlock popup succeeds; the run
    /// loop picks this up to exit and connect, since the unlock happens
    /// several calls deep inside the key-handling chain.
    connect_target: Option<String>,
    vault: *mut LazyVault,
}

impl App {
    fn new(hosts: Vec<Host>, vault: &mut LazyVault) -> App {
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
            settings: None,
            vault_unlock: None,
            connect_target: None,
            vault,
        };
        app.recompute();
        app
    }

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

    fn move_by(&mut self, delta: isize) {
        if self.filtered.is_empty() { return; }
        let max = self.filtered.len() as isize - 1;
        let next = (self.selected as isize + delta).clamp(0, max);
        self.selected = next as usize;
    }

    fn to_top(&mut self) { self.selected = 0; }
    fn to_bottom(&mut self) { self.selected = self.filtered.len().saturating_sub(1); }

    fn selected_alias(&self) -> Option<String> {
        let idx = *self.filtered.get(self.selected)?;
        Some(self.hosts[idx].alias.clone())
    }

    /// Enter settings mode for `host_idx`. Loads password from vault only if
    /// it's already unlocked. Never prompts.
    fn enter_settings_for(&mut self, host_idx: usize) {
        let host = &self.hosts[host_idx];
        let vault = unsafe { &mut *self.vault };
        let existing = vault.get_settings(&host.alias).ok().flatten().unwrap_or_default();

        let fields = vec![
            SettingsField {
                label: "Name",
                value: existing.name.clone().unwrap_or_else(|| host.alias.clone()),
                saved: String::new(),
                secret: false,
            },
            SettingsField {
                label: "Address",
                value: existing.address.clone()
                    .or_else(|| host.hostname.clone())
                    .unwrap_or_default(),
                saved: String::new(),
                secret: false,
            },
            SettingsField {
                label: "Username",
                value: existing.username.clone()
                    .or_else(|| host.user.clone())
                    .unwrap_or_default(),
                saved: String::new(),
                secret: false,
            },
            SettingsField {
                label: "Password",
                value: existing.password.unwrap_or_default(),
                saved: String::new(),
                secret: true,
            },
        ];

        let mut fields = fields;
        for f in &mut fields { f.saved = f.value.clone(); }
        let originals = fields.iter().map(|f| f.value.clone()).collect();

        self.settings = Some(SettingsData {
            fields,
            originals,
            selected: 0,
            editing: false,
        });
        self.mode = Mode::Settings;
    }

    /// Connect to the currently selected host, popping up the vault-unlock
    /// prompt first if that host might have vault settings and the vault
    /// isn't unlocked yet — instead of exiting the TUI to a bare terminal
    /// prompt.
    fn try_connect(&mut self) -> Option<KeyOutcome> {
        let Some(alias) = self.selected_alias() else {
            return Some(KeyOutcome::Exit(None));
        };
        let vault = unsafe { &mut *self.vault };
        let needs_unlock = !vault.is_unlocked()
            && vault.might_have_settings(&alias).unwrap_or(true);

        if needs_unlock {
            let host_idx = *self.filtered.get(self.selected).unwrap_or(&0);
            self.vault_unlock = Some(VaultUnlockData {
                passphrase: String::new(),
                error: None,
                host_idx,
                action: VaultUnlockAction::Connect(alias),
            });
            self.mode = Mode::VaultUnlock;
            None
        } else {
            Some(KeyOutcome::Exit(Some(alias)))
        }
    }

    /// Try to unlock with the given passphrase.
    fn try_unlock(&mut self, passphrase: &str) {
        let (host_idx, action) = match self.vault_unlock.as_ref() {
            Some(d) => (d.host_idx, d.action.clone()),
            None => return,
        };

        match Vault::open(passphrase) {
            Ok(vault) => {
                let vault_ptr = self.vault;
                unsafe { (*vault_ptr).inject(vault); }
                self.vault_unlock = None;

                match action {
                    VaultUnlockAction::Save(alias, hs) => {
                        let vault = unsafe { &mut *self.vault };
                        if let Err(e) = vault.set_settings_data(&alias, hs) {
                            self.vault_unlock = Some(VaultUnlockData {
                                passphrase: String::new(),
                                error: Some(format!("{e:#}")),
                                host_idx,
                                action: VaultUnlockAction::ReenterSettings,
                            });
                            return;
                        }
                        self.mode = Mode::Normal;
                    }
                    VaultUnlockAction::Connect(alias) => {
                        self.mode = Mode::Normal;
                        self.connect_target = Some(alias);
                    }
                    VaultUnlockAction::ReenterSettings => {
                        // Re-enter settings mode to load password from vault.
                        self.enter_settings_for(host_idx);
                        // Start editing the password field.
                        if let Some(ref mut s) = self.settings {
                            s.selected = 3;
                            s.editing = true;
                            if s.fields[3].value.is_empty() {
                                // Load password from vault now that it's unlocked.
                                let vault = unsafe { &mut *self.vault };
                                if let Ok(Some(stored)) = vault.get_settings(
                                    &self.hosts[host_idx].alias
                                ) {
                                    if let Some(pw) = &stored.password {
                                        s.fields[3].value.clone_from(pw);
                                        s.fields[3].saved.clone_from(pw);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                if let Some(ref mut d) = self.vault_unlock {
                    d.passphrase.clear();
                    d.error = Some(format!("{e:#}"));
                }
            }
        }
    }

    /// Leave settings mode. If anything actually changed and vault is
    /// unlocked, save directly. If changed and vault is locked, enter
    /// VaultUnlock. If no vault and no password, skip. If no vault and
    /// password set, terminal-prompt creation.
    fn leave_settings(&mut self) {
        let Some(settings) = self.settings.take() else {
            self.mode = Mode::Normal;
            return;
        };
        let Some(&host_idx) = self.filtered.get(self.selected) else {
            self.mode = Mode::Normal;
            return;
        };
        let alias = self.hosts[host_idx].alias.clone();

        let changed = settings.fields.iter().zip(settings.originals.iter())
            .any(|(f, orig)| &f.value != orig);
        if !changed {
            self.mode = Mode::Normal;
            return;
        }

        let hs = HostSettings {
            name: Some(settings.fields[0].value.clone()).filter(|s| !s.is_empty()),
            address: Some(settings.fields[1].value.clone()).filter(|s| !s.is_empty()),
            username: Some(settings.fields[2].value.clone()).filter(|s| !s.is_empty()),
            password: Some(settings.fields[3].value.clone()).filter(|s| !s.is_empty()),
        };

        let vault = unsafe { &mut *self.vault };

        // Vault already unlocked — save directly.
        if unsafe { (*self.vault).is_unlocked() } {
            if let Err(e) = vault.set_settings_data(&alias, hs) {
                // Should never happen since vault is Some, but handle gracefully.
                self.settings = Some(settings);
                self.vault_unlock = Some(VaultUnlockData {
                    passphrase: String::new(),
                    error: Some(format!("{e:#}")),
                    host_idx,
                    action: VaultUnlockAction::ReenterSettings,
                });
                self.mode = Mode::VaultUnlock;
                return;
            }
            self.mode = Mode::Normal;
            return;
        }

        // Vault file exists but not yet unlocked — unlock then save.
        if Vault::exists().unwrap_or(false) {
            self.vault_unlock = Some(VaultUnlockData {
                passphrase: String::new(),
                error: None,
                host_idx,
                action: VaultUnlockAction::Save(alias, hs),
            });
            self.mode = Mode::VaultUnlock;
            return;
        }

        // No vault at all.
        let has_password = hs.password.is_some();
        if !has_password {
            // No password to store — skip save.
            self.mode = Mode::Normal;
            return;
        }

        // Need to create a vault — requires terminal prompt.
        ratatui::restore();
        eprintln!("No vault found. Creating one now.");
        let passphrase = crate::vault::prompt_passphrase("Create vault passphrase: ")
            .unwrap_or_default();
        let confirm = crate::vault::prompt_passphrase("Confirm passphrase: ")
            .unwrap_or_default();
        if passphrase.is_empty() || passphrase != confirm {
            eprintln!("Passphrases don't match or empty. Settings not saved.");
        } else {
            match Vault::init(&passphrase) {
                Ok(v) => {
                    vault.inject(v);
                    if let Err(e) = vault.set_settings_data(&alias, hs) {
                        eprintln!("Error saving: {e:#}");
                    } else {
                        eprintln!("Vault created and settings saved for {alias}");
                    }
                }
                Err(e) => eprintln!("Error: {e:#}"),
            }
        }
        eprintln!("Press Enter to continue...");
        let _ = std::io::stdin().read_line(&mut String::new());
        let _ = ratatui::init();
        self.mode = Mode::Normal;
    }
}

// ---- Main event loop -------------------------------------------------------

pub fn run_picker(
    hosts: Vec<Host>,
    mut rx: UnboundedReceiver<TmuxStatus>,
    vault: &mut LazyVault,
) -> Result<Option<String>> {
    let mut terminal = ratatui::init();
    let mut app = App::new(hosts, vault);

    let result = loop {
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

            match app.mode {
                Mode::VaultUnlock => {
                    handle_vault_unlock_key(&mut app, key.code);
                    if let Some(alias) = app.connect_target.take() {
                        break Some(alias);
                    }
                    continue;
                }
                Mode::Settings => {
                    handle_settings_key(&mut app, key.code);
                    continue;
                }
                _ => {}
            }

            match key.code {
                KeyCode::Enter => {
                    if let Some(KeyOutcome::Exit(alias)) = app.try_connect() {
                        break alias;
                    }
                }
                KeyCode::Char('c' | 'g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break None;
                }
                _ => {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    if let Some(outcome) = handle_key(&mut app, key.code, ctrl) {
                        match outcome { KeyOutcome::Exit(alias) => break alias }
                    }
                }
            }
        }
    };

    ratatui::restore();
    Ok(result)
}

#[derive(Debug, PartialEq)]
enum KeyOutcome {
    Exit(Option<String>),
}

fn handle_key(app: &mut App, code: KeyCode, ctrl: bool) -> Option<KeyOutcome> {
    let half_page = (app.viewport / 2).max(1) as isize;
    match code {
        KeyCode::Up | KeyCode::Char('p') if ctrl => app.move_by(-1),
        KeyCode::Down | KeyCode::Char('n') if ctrl => app.move_by(1),
        KeyCode::PageUp => app.move_by(-(app.viewport as isize)),
        KeyCode::PageDown => app.move_by(app.viewport as isize),
        KeyCode::Char('d') if ctrl => app.move_by(half_page),
        KeyCode::Char('u') if ctrl && app.mode == Mode::Normal => app.move_by(-half_page),
        _ => return handle_mode_key(app, code, ctrl),
    }
    None
}

fn handle_mode_key(app: &mut App, code: KeyCode, _ctrl: bool) -> Option<KeyOutcome> {
    match app.mode {
        Mode::Normal => match code {
            KeyCode::Esc | KeyCode::Char('q') => return Some(KeyOutcome::Exit(None)),
            KeyCode::Char('j') => app.move_by(1),
            KeyCode::Char('k') => app.move_by(-1),
            KeyCode::Char('g') => app.to_top(),
            KeyCode::Char('G') => app.to_bottom(),
            KeyCode::Char('l') => return app.try_connect(),
            KeyCode::Char('/' | 's' | 'i') => app.mode = Mode::Search,
            KeyCode::Char('e') => {
                if let Some(&idx) = app.filtered.get(app.selected) {
                    app.enter_settings_for(idx);
                }
            }
            _ => {}
        },
        Mode::Search => match code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Backspace => { app.query.pop(); app.recompute(); }
            KeyCode::Char('u') if _ctrl => { app.query.clear(); app.recompute(); }
            KeyCode::Char('w') if _ctrl => { trim_last_word(&mut app.query); app.recompute(); }
            KeyCode::Char(c) => { app.query.push(c); app.recompute(); }
            _ => {}
        },
        _ => {}
    }
    None
}

fn handle_vault_unlock_key(app: &mut App, code: KeyCode) {
    let Some(ref mut data) = app.vault_unlock else { return };
    match code {
        KeyCode::Enter => {
            let passphrase = data.passphrase.clone();
            app.try_unlock(&passphrase);
        }
        KeyCode::Esc => {
            app.vault_unlock = None;
            if app.settings.is_some() {
                // Return to settings mode (cancelled unlock from settings).
                app.mode = Mode::Settings;
            } else {
                app.mode = Mode::Normal;
            }
        }
        KeyCode::Backspace => { data.passphrase.pop(); data.error = None; }
        KeyCode::Char(c) => { data.passphrase.push(c); data.error = None; }
        _ => {}
    }
}

fn handle_settings_key(app: &mut App, code: KeyCode) {
    let Some(ref mut settings) = app.settings else { return };

    if settings.editing {
        match code {
            KeyCode::Enter => {
                let idx = settings.selected;
                settings.fields[idx].saved = settings.fields[idx].value.clone();
                settings.editing = false;
            }
            KeyCode::Esc => {
                let idx = settings.selected;
                settings.fields[idx].value = settings.fields[idx].saved.clone();
                settings.editing = false;
            }
            KeyCode::Char(c) => {
                let idx = settings.selected;
                settings.fields[idx].value.push(c);
            }
            KeyCode::Backspace => {
                let idx = settings.selected;
                settings.fields[idx].value.pop();
            }
            _ => {}
        }
    } else {
        match code {
            KeyCode::Esc => app.leave_settings(),
            KeyCode::Char('j') | KeyCode::Down => {
                let n = settings.fields.len();
                settings.selected = (settings.selected + 1).min(n - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                settings.selected = settings.selected.saturating_sub(1);
            }
            KeyCode::Enter => {
                let idx = settings.selected;
                // Pressing Enter on the password field when vault is locked:
                // show vault unlock popup instead of editing.
                if idx == 3 {
                    let host_idx = app.filtered.get(app.selected).copied().unwrap_or(0);
                    let vault = unsafe { &mut *app.vault };
                    if vault.is_locked().unwrap_or(false) {
                        app.vault_unlock = Some(VaultUnlockData {
                            passphrase: String::new(),
                            error: None,
                            host_idx,
                            action: VaultUnlockAction::ReenterSettings,
                        });
                        app.mode = Mode::VaultUnlock;
                        return;
                    }
                }
                settings.editing = true;
                settings.fields[idx].saved = settings.fields[idx].value.clone();
            }
            _ => {}
        }
    }
}

fn trim_last_word(query: &mut String) {
    while query.ends_with(char::is_whitespace) { query.pop(); }
    while query.chars().next_back().is_some_and(|c| !c.is_whitespace()) { query.pop(); }
}

// ---- Rendering -------------------------------------------------------------

fn ui(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(3)])
        .split(frame.area());
    app.viewport = (chunks[0].height.saturating_sub(2)) as usize;

    match app.mode {
        Mode::VaultUnlock => {
            render_list(frame, app, chunks[0]);
            render_vault_unlock_overlay(frame, app, chunks[0]);
            render_input(frame, app, chunks[1]);
        }
        Mode::Settings => {
            render_settings_overlay(frame, app, chunks[0]);
            render_input(frame, app, chunks[1]);
        }
        _ => {
            render_list(frame, app, chunks[0]);
            render_input(frame, app, chunks[1]);
        }
    }
}

fn render_list(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let alias_w = app.hosts.iter()
        .map(|h| h.alias.chars().count()).max().unwrap_or(8).clamp(8, 28);

    let items: Vec<ListItem> = app.filtered.iter()
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
    )).alignment(Alignment::Right);

    let block = Block::default()
        .borders(Borders::ALL).border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(left).title(right);

    if app.filtered.is_empty() {
        let empty = Paragraph::new(Line::from(Span::styled(
            "no matching hosts",
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        ))).alignment(Alignment::Center).block(block);
        frame.render_widget(empty, area);
        return;
    }

    let list = List::new(items).block(block).highlight_style(
        Style::default().bg(ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD),
    ).highlight_symbol("▌ ").highlight_spacing(ratatui::widgets::HighlightSpacing::Always);
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn row(host: &Host, status: Option<&Option<bool>>, alias_w: usize) -> ListItem<'static> {
    let (dot, dot_style, tag) = status_visual(status);
    let mut spans = vec![
        Span::styled(format!("{dot} "), dot_style),
        Span::styled(format!("{:<width$}", host.alias, width = alias_w),
            Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(host.endpoint(), Style::default().fg(ACCENT)),
        Span::raw("  "),
        Span::styled(relative_time(host.state.last_connected), Style::default().fg(DIM)),
    ];
    if host.state.connection_count > 0 {
        spans.push(Span::styled(format!("  ×{}", host.state.connection_count), Style::default().fg(DIM)));
    }
    if !tag.is_empty() {
        spans.push(Span::styled(format!("  {tag}"), Style::default().fg(ACTIVE).add_modifier(Modifier::BOLD)));
    }
    if host.source == HostSource::KnownHosts {
        spans.push(Span::styled(format!("  ({})", host.source.label()), Style::default().fg(DIM)));
    }
    let notes = host.meta.notes.as_deref().or(host.state.notes.as_deref());
    if let Some(notes) = notes {
        spans.push(Span::styled(format!("  — {notes}"), Style::default().fg(DIM).add_modifier(Modifier::ITALIC)));
    }
    ListItem::new(Line::from(spans))
}

fn centered_rect(r: ratatui::layout::Rect, pct_x: u16, pct_y: u16) -> ratatui::layout::Rect {
    let popup_x = r.width * pct_x / 100;
    let popup_y = r.height * pct_y / 100;
    let x = (r.width.saturating_sub(popup_x)) / 2;
    let y = (r.height.saturating_sub(popup_y)) / 2;
    ratatui::layout::Rect::new(x, y, popup_x.min(r.width), popup_y.min(r.height))
}

fn render_vault_unlock_overlay(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let Some(ref data) = app.vault_unlock else { return };
    frame.render_widget(Clear, area);

    let popup = centered_rect(area, 50, 25);
    frame.render_widget(Clear, popup);

    let title = match data.action {
        VaultUnlockAction::Save(..) => " Vault passphrase (save) ",
        VaultUnlockAction::Connect(..) => " Vault passphrase (connect) ",
        VaultUnlockAction::ReenterSettings => " Vault passphrase ",
    };
    let block = Block::default()
        .borders(Borders::ALL).border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(title);

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let pass_display: String = std::iter::repeat('*')
        .take(data.passphrase.chars().count()).collect();
    let cursor = if data.passphrase.is_empty() { "" } else { "█" };

    let input_line = Line::from(vec![
        Span::styled("  Passphrase: ", Style::default().fg(ACCENT)),
        Span::styled(format!("{}{}", pass_display, cursor), Style::default().fg(Color::White)),
    ]);
    frame.render_widget(Paragraph::new(input_line), inner.inner(Margin { vertical: 1, horizontal: 1 }));

    if let Some(ref err) = data.error {
        let err_line = Line::from(Span::styled(format!("  {}", err), Style::default().fg(Color::Red)));
        frame.render_widget(Paragraph::new(err_line), inner.inner(Margin { vertical: 3, horizontal: 1 }));
    }

    let hint = Line::from(Span::styled("  ↵ unlock  esc cancel", Style::default().fg(DIM)));
    frame.render_widget(Paragraph::new(hint), inner.inner(Margin { vertical: 4, horizontal: 1 }));
}

fn render_settings_overlay(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let Some(ref settings) = app.settings else { return };
    let alias = app.selected_alias().unwrap_or_default();

    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL).border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Line::from(vec![
            Span::raw(" Settings: "),
            Span::styled(&alias, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
        ]));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let field_area = Layout::vertical(
        std::iter::repeat(Constraint::Length(1))
            .take(settings.fields.len()).collect::<Vec<_>>(),
    ).split(inner.inner(Margin { vertical: 1, horizontal: 2 }));

    for (i, field) in settings.fields.iter().enumerate() {
        let is_selected = i == settings.selected;
        let is_editing = is_selected && settings.editing;
        let is_password = i == 3;

        let display_val = if field.secret && !is_editing {
            "********".to_string()
        } else {
            field.value.clone()
        };

        let label_style = if is_selected {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };

        let val_style = if is_editing {
            Style::default().fg(Color::White).bg(Color::DarkGray)
        } else if is_selected {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::White)
        };

        let cursor = if is_editing { "█" } else { "" };
        let unlock_hint = if is_selected && is_password && field.value.is_empty() {
            " (↵ to unlock)"
        } else {
            ""
        };

        let line = Line::from(vec![
            Span::styled(format!("  {:<9} ", field.label), label_style),
            Span::styled(format!("{}{}{}", display_val, cursor, unlock_hint), val_style),
        ]);
        frame.render_widget(Paragraph::new(line), field_area[i]);
    }

    let hint = if settings.editing {
        "↵ save  esc cancel"
    } else {
        "j/k move  ↵ edit  esc save & back"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(format!("  {hint}"), Style::default().fg(DIM)))),
        inner.inner(Margin { vertical: (settings.fields.len() as u16 + 2), horizontal: 2 }),
    );
}

fn render_input(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let (badge_label, badge_color, hints) = match app.mode {
        Mode::Normal => (" NORMAL ", ACCENT,
            "j/k move · g/G top/bottom · / search · e settings · ⏎ connect · q quit"),
        Mode::Search => (" SEARCH ", SEARCH_HL,
            "type to filter · esc normal · ↑/↓ move · ⏎ connect"),
        Mode::VaultUnlock => (" VAULT ", ACTIVE,
            "↵ unlock · esc cancel"),
        Mode::Settings => (" SETTINGS ", ACTIVE,
            "j/k move · ↵ edit · esc save & back"),
    };

    let title = Line::from(vec![
        Span::styled(badge_label,
            Style::default().bg(badge_color).fg(Color::Black).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(hints, Style::default().fg(DIM)),
        Span::raw(" "),
    ]);

    let block = Block::default()
        .borders(Borders::ALL).border_type(BorderType::Rounded)
        .border_style(Style::default().fg(badge_color))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut spans = vec![Span::styled("❯ ",
        Style::default().fg(badge_color).add_modifier(Modifier::BOLD))];

    match app.mode {
        Mode::VaultUnlock => {
            spans.push(Span::styled("enter vault passphrase", Style::default().fg(Color::White)));
        }
        Mode::Settings => {
            let alias = app.selected_alias().unwrap_or_default();
            spans.push(Span::styled(format!("editing {alias}"), Style::default().fg(Color::White)));
        }
        _ => {
            if app.query.is_empty() && app.mode == Mode::Normal {
                spans.push(Span::styled("press / to search…",
                    Style::default().fg(DIM).add_modifier(Modifier::ITALIC)));
            } else {
                spans.push(Span::raw(app.query.clone()));
                if app.mode == Mode::Search {
                    spans.push(Span::styled("█", Style::default().fg(badge_color)));
                }
            }
        }
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), inner.inner(Margin::new(1, 0)));
}

fn status_visual(status: Option<&Option<bool>>) -> (&'static str, Style, &'static str) {
    match status {
        None => ("•", Style::default().fg(DIM), ""),
        Some(Some(true)) => ("●", Style::default().fg(ACTIVE).add_modifier(Modifier::BOLD), "tmux"),
        Some(Some(false)) => ("○", Style::default().fg(DIM), ""),
        Some(None) => ("⨯", Style::default().fg(Color::Red), ""),
    }
}

// ---- Tests -----------------------------------------------------------------

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

    fn exit_alias(alias: &str) -> Option<KeyOutcome> {
        Some(KeyOutcome::Exit(Some(alias.to_string())))
    }

    fn exit_none() -> Option<KeyOutcome> {
        Some(KeyOutcome::Exit(None))
    }

    #[test]
    fn vim_navigation_in_normal_mode() {
        let mut vault = LazyVault::new();
        let mut app = App::new(sample_hosts(), &mut vault);
        assert_eq!(app.selected, 0);
        handle_key(&mut app, KeyCode::Char('j'), false);
        assert_eq!(app.selected, 1);
        handle_key(&mut app, KeyCode::Char('j'), false);
        assert_eq!(app.selected, 1);
        handle_key(&mut app, KeyCode::Char('k'), false);
        assert_eq!(app.selected, 0);
        handle_key(&mut app, KeyCode::Char('G'), false);
        assert_eq!(app.selected, 1);
        handle_key(&mut app, KeyCode::Char('g'), false);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn slash_enters_search_and_filters() {
        let mut vault = LazyVault::new();
        let mut app = App::new(sample_hosts(), &mut vault);
        handle_key(&mut app, KeyCode::Char('/'), false);
        assert_eq!(app.mode, Mode::Search);
        for c in "db".chars() {
            handle_key(&mut app, KeyCode::Char(c), false);
        }
        assert_eq!(app.query, "db");
        let filtered_with_query = app.filtered.len();
        handle_key(&mut app, KeyCode::Esc, false);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.filtered.len(), filtered_with_query);
    }

    #[test]
    fn enter_returns_selection_and_q_cancels() {
        let mut vault = LazyVault::new();
        let mut app = App::new(sample_hosts(), &mut vault);
        let chosen = handle_key(&mut app, KeyCode::Char('l'), false);
        assert_eq!(chosen, exit_alias("db"));
        let cancelled = handle_key(&mut app, KeyCode::Char('q'), false);
        assert_eq!(cancelled, exit_none());
    }

    #[test]
    fn e_key_enters_settings_immediately() {
        let mut vault = LazyVault::new();
        let mut app = App::new(sample_hosts(), &mut vault);
        app.selected = 0;
        handle_key(&mut app, KeyCode::Char('e'), false);
        assert_eq!(app.mode, Mode::Settings);
        let s = app.settings.as_ref().unwrap();
        assert_eq!(s.fields[0].value, "db");
        assert_eq!(s.fields[1].value, "db.internal");
        assert_eq!(s.fields[2].value, "");
        assert!(s.fields[3].value.is_empty()); // locked vault => pw blank
    }

    #[test]
    fn esc_during_edit_reverts_uncommitted_typing() {
        let mut vault = LazyVault::new();
        let mut app = App::new(sample_hosts(), &mut vault);
        app.selected = 0;
        handle_key(&mut app, KeyCode::Char('e'), false);
        // Select and start editing the Address field (index 1).
        handle_settings_key(&mut app, KeyCode::Char('j'));
        handle_settings_key(&mut app, KeyCode::Enter);
        assert!(app.settings.as_ref().unwrap().editing);
        for c in "extra".chars() {
            handle_settings_key(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.settings.as_ref().unwrap().fields[1].value, "db.internalextra");
        handle_settings_key(&mut app, KeyCode::Esc);
        assert!(!app.settings.as_ref().unwrap().editing);
        assert_eq!(app.settings.as_ref().unwrap().fields[1].value, "db.internal");
    }

    #[test]
    fn typing_then_undoing_leaves_no_net_change() {
        let mut vault = LazyVault::new();
        let mut app = App::new(sample_hosts(), &mut vault);
        app.selected = 0;
        handle_key(&mut app, KeyCode::Char('e'), false);
        handle_settings_key(&mut app, KeyCode::Enter); // edit Name field
        handle_settings_key(&mut app, KeyCode::Char('x'));
        handle_settings_key(&mut app, KeyCode::Backspace);
        handle_settings_key(&mut app, KeyCode::Enter); // confirm edit
        let settings = app.settings.as_ref().unwrap();
        let changed = settings.fields.iter().zip(settings.originals.iter())
            .any(|(f, orig)| &f.value != orig);
        assert!(!changed, "typing a char and backspacing it should not register as a change");
    }

    #[test]
    fn connecting_to_vaulted_host_pops_up_unlock_instead_of_exiting() {
        use crate::vault::test_support::with_temp_vault;

        with_temp_vault(|| {
            let mut vault = crate::vault::Vault::init("testpass").unwrap();
            let settings = HostSettings {
                password: Some("s3cret".into()),
                ..Default::default()
            };
            vault.set_settings("db", settings).unwrap();

            let mut lazy = LazyVault::new();
            let mut app = App::new(sample_hosts(), &mut lazy);
            let select_alias = |app: &mut App, alias: &str| {
                app.selected = app.filtered.iter()
                    .position(|&i| app.hosts[i].alias == alias)
                    .expect("alias present in filtered list");
            };

            select_alias(&mut app, "db"); // has vault settings

            // Enter/'l' should NOT exit the picker straight away — it should
            // pop up the vault-unlock prompt instead of dropping to a bare
            // terminal password prompt.
            let outcome = app.try_connect();
            assert_eq!(outcome, None);
            assert_eq!(app.mode, Mode::VaultUnlock);
            assert!(app.vault_unlock.is_some());

            // A host with no vault settings should connect immediately with
            // no unlock popup at all.
            select_alias(&mut app, "prod-web"); // no vault entry
            let outcome = app.try_connect();
            assert_eq!(outcome, Some(KeyOutcome::Exit(Some("prod-web".into()))));
            assert_eq!(app.mode, Mode::VaultUnlock, "unrelated in-progress unlock is untouched");

            // Now actually unlock "db"'s pending prompt and confirm the loop
            // gets told to exit and connect.
            app.try_unlock("testpass");
            assert_eq!(app.connect_target.as_deref(), Some("db"));
            assert_eq!(app.mode, Mode::Normal);
        });
    }

    #[test]
    fn renders_without_panicking() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut vault = LazyVault::new();
        let mut app = App::new(sample_hosts(), &mut vault);
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("ssht"));
        assert!(text.contains("NORMAL"));
        assert!(text.contains("prod-web"));
        assert!(text.contains("deploy@10.0.0.1"));
    }
}
