use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};
use ratatui::{
    DefaultTerminal, Frame, TerminalOptions, Viewport,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use crate::args::{ModelsCommand, ProvidersCommand};
use crate::catalog;
use crate::config::{AuthMode, Paths};
use crate::launch::{self, EnvLookup};
use crate::provider::Provider;

const PAGE_SIZE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialSource {
    Stored,
    Environment,
}

#[derive(Debug, Clone)]
struct ProviderState {
    provider: Provider,
    credential: Option<CredentialSource>,
    environment_active: bool,
    default: bool,
}

impl ProviderState {
    fn configured(&self) -> bool {
        self.credential.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Login,
    Logout,
    Use,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Provider,
    ApiKey,
    ConfirmLogout,
}

enum Outcome {
    Login { id: String, key: String },
    Logout { index: usize },
    Use { index: usize },
}

struct App {
    action: Action,
    step: Step,
    providers: Vec<ProviderState>,
    query: String,
    cursor: usize,
    selected: Option<usize>,
    api_key: String,
    validation_error: bool,
    direct: bool,
    exit: bool,
    outcome: Option<Outcome>,
}

impl App {
    fn new(action: Action, providers: Vec<ProviderState>) -> Self {
        let mut providers = providers;
        sort_provider_states(&mut providers);
        let mut app = Self {
            action,
            step: Step::Provider,
            providers,
            query: String::new(),
            cursor: 0,
            selected: None,
            api_key: String::new(),
            validation_error: false,
            direct: false,
            exit: false,
            outcome: None,
        };
        if action != Action::Login
            && let Some(position) =
                app.filtered_providers().iter().position(|index| app.providers[*index].default)
        {
            app.cursor = position;
        }
        app
    }

    fn login_for_provider(providers: Vec<ProviderState>, selected: usize) -> Self {
        let mut app = Self::new(Action::Login, providers);
        app.selected = Some(selected);
        app.step = Step::ApiKey;
        app.direct = true;
        app
    }

    fn filtered_providers(&self) -> Vec<usize> {
        let candidates = self
            .providers
            .iter()
            .enumerate()
            .filter(|(_, provider)| self.action == Action::Login || provider.configured());
        if self.query.is_empty() {
            return candidates.map(|(index, _)| index).collect();
        }
        let matcher = SkimMatcherV2::default().ignore_case();
        let mut matches = candidates
            .filter_map(|(index, state)| {
                let provider = &state.provider;
                let candidate = format!("{} {} {}", provider.name, provider.id, provider.endpoint);
                matcher.fuzzy_match(&candidate, &self.query).map(|score| (index, score))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        matches.into_iter().map(|(index, _)| index).collect()
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Paste(value) => self.handle_paste(&value),
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.exit = true;
            return;
        }
        match self.step {
            Step::Provider => self.handle_provider_key(key),
            Step::ApiKey => self.handle_api_key(key),
            Step::ConfirmLogout => self.handle_logout_confirmation(key),
        }
    }

    fn handle_provider_key(&mut self, key: KeyEvent) {
        let count = self.filtered_providers().len();
        match key.code {
            KeyCode::Up if count > 0 => {
                self.cursor = self.cursor.checked_sub(1).unwrap_or(count - 1);
            }
            KeyCode::Down if count > 0 => self.cursor = (self.cursor + 1) % count,
            KeyCode::Enter if count > 0 => {
                self.selected = self.filtered_providers().get(self.cursor).copied();
                self.api_key.clear();
                self.validation_error = false;
                match self.action {
                    Action::Login => self.step = Step::ApiKey,
                    Action::Logout => self.step = Step::ConfirmLogout,
                    Action::Use => {
                        self.outcome =
                            Some(Outcome::Use { index: self.selected.expect("selected provider") });
                        self.exit = true;
                    }
                }
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.cursor = 0;
            }
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.cursor = 0;
            }
            KeyCode::Esc => self.exit = true,
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.query.push(character);
                self.cursor = 0;
            }
            _ => {}
        }
    }

    fn handle_api_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter if self.api_key.trim().is_empty() => self.validation_error = true,
            KeyCode::Enter => {
                let index = self.selected.expect("selected provider");
                self.outcome = Some(Outcome::Login {
                    id: self.providers[index].provider.id.clone(),
                    key: self.api_key.trim().to_string(),
                });
                self.exit = true;
            }
            KeyCode::Backspace => {
                self.api_key.pop();
                self.validation_error = false;
            }
            KeyCode::Esc => {
                self.api_key.clear();
                self.validation_error = false;
                if self.direct {
                    self.exit = true;
                } else {
                    self.step = Step::Provider;
                }
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.api_key.push(character);
                self.validation_error = false;
            }
            _ => {}
        }
    }

    fn handle_logout_confirmation(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y' | 'Y') => {
                self.outcome =
                    Some(Outcome::Logout { index: self.selected.expect("selected provider") });
                self.exit = true;
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => self.step = Step::Provider,
            _ => {}
        }
    }

    fn handle_paste(&mut self, value: &str) {
        let value = value.chars().filter(|character| !character.is_control()).collect::<String>();
        match self.step {
            Step::Provider => {
                self.query.push_str(&value);
                self.cursor = 0;
            }
            Step::ApiKey => {
                self.api_key.push_str(&value);
                self.validation_error = false;
            }
            Step::ConfirmLogout => {}
        }
    }
}

#[derive(Clone, Copy)]
struct Palette {
    accent: Color,
    primary: Color,
    muted: Color,
    default: Color,
    configured: Color,
    error: Color,
}

impl Palette {
    fn current(env: &EnvLookup) -> Self {
        if env.get("NO_COLOR").is_some() {
            return Self {
                accent: Color::Reset,
                primary: Color::Reset,
                muted: Color::Reset,
                default: Color::Reset,
                configured: Color::Reset,
                error: Color::Reset,
            };
        }
        Self {
            accent: Color::Rgb(116, 199, 236),
            primary: Color::Rgb(220, 223, 228),
            muted: Color::Rgb(111, 115, 122),
            default: Color::Rgb(166, 218, 149),
            configured: Color::Rgb(137, 180, 250),
            error: Color::Rgb(237, 135, 150),
        }
    }
}

pub(crate) fn run(command: ProvidersCommand, paths: &Paths, env: &EnvLookup) -> Result<()> {
    match command {
        ProvidersCommand::Help => {
            print!("{}", help());
            Ok(())
        }
        ProvidersCommand::List => list(paths, env),
        ProvidersCommand::Login { provider } => login(paths, env, provider.as_deref()),
        ProvidersCommand::Logout { provider } => logout(paths, env, provider.as_deref()),
        ProvidersCommand::Use { provider } => use_provider(paths, env, provider.as_deref()),
        ProvidersCommand::Models(command) => models(command, paths, env),
    }
}

pub(crate) fn help() -> &'static str {
    concat!(
        "rx providers — manage AI providers\n\n",
        "Usage:\n",
        "  rx providers list\n",
        "  rx providers login [provider]\n",
        "  rx providers logout [provider]\n",
        "  rx providers use [provider]\n",
        "  rx providers models update [provider]\n\n",
    )
}

pub(crate) fn models_help() -> &'static str {
    concat!(
        "rx providers models — update provider model catalogs\n\n",
        "Usage:\n",
        "  rx providers models update [provider]\n\n",
    )
}

fn models(command: ModelsCommand, paths: &Paths, env: &EnvLookup) -> Result<()> {
    match command {
        ModelsCommand::Help => {
            print!("{}", models_help());
            Ok(())
        }
        ModelsCommand::Update { provider } => update_models(paths, env, provider.as_deref()),
    }
}

fn update_models(paths: &Paths, env: &EnvLookup, requested: Option<&str>) -> Result<()> {
    let mut ids = Vec::new();
    if let Some(id) = requested {
        ids.push(id.to_string());
    } else {
        for state in provider_states(paths, env)? {
            if state.configured() {
                ids.push(state.provider.id.clone());
            }
        }
        if ids.is_empty() {
            println!("No providers configured. Run: rx providers login");
            return Ok(());
        }
    }
    for id in ids {
        let Some(target) = launch::configured_provider(Some(&id), paths, env)? else {
            bail!("no API key for provider '{id}'; run: rx providers login {id}");
        };
        let count =
            catalog::update_models(paths, &target.provider_id, &target.base_url, &target.key)?;
        println!("{}: {count} models", target.provider_id);
    }
    Ok(())
}

fn list(paths: &Paths, env: &EnvLookup) -> Result<()> {
    let states = provider_states(paths, env)?;
    let configured = states.iter().filter(|provider| provider.configured()).collect::<Vec<_>>();
    if configured.is_empty() {
        println!("No providers configured. Run: rx providers login");
        return Ok(());
    }
    print!("{}", render_list(&configured));
    Ok(())
}

fn login(paths: &Paths, env: &EnvLookup, requested: Option<&str>) -> Result<()> {
    let states = provider_states(paths, env)?;
    let selected = requested.map(|id| provider_index(&states, id, false)).transpose()?;
    let outcome = run_ui(Action::Login, states, env, selected)?;
    let Some(Outcome::Login { id, key }) = outcome else {
        return Ok(());
    };
    crate::config::login(paths, &id, key)?;
    let provider =
        crate::provider::resolve(&id, crate::config::load_or_default(paths)?.provider.get(&id))?;
    println!("* {} configured and set as default\n  {}", provider.name, provider.endpoint);
    Ok(())
}

fn logout(paths: &Paths, env: &EnvLookup, requested: Option<&str>) -> Result<()> {
    let states = provider_states(paths, env)?;
    if let Some(id) = requested {
        let index = provider_index(&states, id, true)?;
        return logout_provider(paths, &states[index]);
    }
    if !states.iter().any(ProviderState::configured) {
        println!("No providers configured.");
        return Ok(());
    }
    let snapshot = states.clone();
    let outcome = run_ui(Action::Logout, states, env, None)?;
    let Some(Outcome::Logout { index }) = outcome else {
        return Ok(());
    };
    logout_provider(paths, &snapshot[index])
}

fn logout_provider(paths: &Paths, state: &ProviderState) -> Result<()> {
    let removed = crate::config::logout(paths, &state.provider.id)?;
    let environment_active = state.environment_active;
    if removed {
        println!("Removed stored API key for {}.", state.provider.name);
    }
    if environment_active {
        println!(
            "{} is still available through ${}. Run this in your shell to remove it:\n  unset {}",
            state.provider.name, state.provider.env, state.provider.env
        );
    } else if removed {
        println!("{} logged out.", state.provider.name);
    }
    Ok(())
}

fn use_provider(paths: &Paths, env: &EnvLookup, requested: Option<&str>) -> Result<()> {
    let states = provider_states(paths, env)?;
    if let Some(id) = requested {
        let index = provider_index(&states, id, true)?;
        return set_default_provider(paths, &states[index]);
    }
    if !states.iter().any(ProviderState::configured) {
        println!("No providers configured. Run: rx providers login");
        return Ok(());
    }
    let snapshot = states.clone();
    let outcome = run_ui(Action::Use, states, env, None)?;
    let Some(Outcome::Use { index }) = outcome else {
        return Ok(());
    };
    set_default_provider(paths, &snapshot[index])
}

fn set_default_provider(paths: &Paths, state: &ProviderState) -> Result<()> {
    crate::config::set_default(paths, &state.provider.id)?;
    println!("* {} set as default\n  {}", state.provider.name, state.provider.endpoint);
    Ok(())
}

fn provider_index(states: &[ProviderState], id: &str, configured: bool) -> Result<usize> {
    let index = states
        .iter()
        .position(|state| state.provider.id == id)
        .ok_or_else(|| anyhow::anyhow!("unknown provider: {id}"))?;
    if configured && !states[index].configured() {
        bail!("provider '{id}' is not configured; run: rx providers login {id}");
    }
    Ok(index)
}

fn provider_states(paths: &Paths, env: &EnvLookup) -> Result<Vec<ProviderState>> {
    let config = crate::config::load_or_default(paths)?;
    let stored = crate::config::stored_providers(paths)?;
    let mut states = crate::provider::available(&config)?
        .into_iter()
        .map(|provider| {
            let entry = config.provider.get(&provider.id);
            let environment_active = env.get(&provider.env).is_some()
                && (crate::provider::find(&provider.id).is_some()
                    || entry.is_some_and(|entry| entry.auth == AuthMode::Env));
            let credential = if stored.contains(&provider.id) {
                Some(CredentialSource::Stored)
            } else if environment_active {
                Some(CredentialSource::Environment)
            } else {
                None
            };
            let default =
                config.default_provider.as_deref().unwrap_or("openrouter") == provider.id.as_str();
            ProviderState { provider, credential, environment_active, default }
        })
        .collect::<Vec<_>>();
    sort_provider_states(&mut states);
    Ok(states)
}

fn sort_provider_states(states: &mut [ProviderState]) {
    states.sort_by(|left, right| {
        pin_rank(&left.provider.id)
            .cmp(&pin_rank(&right.provider.id))
            .then_with(|| right.configured().cmp(&left.configured()))
            .then_with(|| {
                left.provider
                    .name
                    .to_ascii_lowercase()
                    .cmp(&right.provider.name.to_ascii_lowercase())
            })
            .then_with(|| left.provider.id.cmp(&right.provider.id))
    });
}

fn pin_rank(id: &str) -> u8 {
    match id {
        "openrouter" => 0,
        "tokener" => 1,
        _ => 2,
    }
}

fn render_list(providers: &[&ProviderState]) -> String {
    let name_width = providers
        .iter()
        .map(|state| state.provider.name.chars().count())
        .max()
        .unwrap_or(7)
        .max("PROVIDER".len());
    let mut output = format!("  {:<name_width$}  API ENDPOINT\n", "PROVIDER");
    for state in providers {
        let marker = if state.default { '*' } else { '•' };
        output.push_str(&format!(
            "{marker} {:<name_width$}  {}\n",
            state.provider.name, state.provider.endpoint
        ));
    }
    output
}

fn run_ui(
    action: Action,
    providers: Vec<ProviderState>,
    env: &EnvLookup,
    selected: Option<usize>,
) -> Result<Option<Outcome>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        let command = match action {
            Action::Login => "login",
            Action::Logout => "logout",
            Action::Use => "use",
        };
        bail!("rx providers {command} requires an interactive terminal");
    }
    let palette = Palette::current(env);
    let mut terminal =
        match ratatui::try_init_with_options(TerminalOptions { viewport: Viewport::Inline(13) }) {
            Ok(terminal) => terminal,
            Err(error) => {
                ratatui::restore();
                return Err(error).context("failed to initialize provider picker");
            }
        };
    let result = run_provider_ui(&mut terminal, action, providers, selected, palette);
    let cleanup = clear_provider_ui(&mut terminal);
    ratatui::restore();
    match result {
        Ok(outcome) => {
            cleanup.context("failed to clear provider picker")?;
            Ok(outcome)
        }
        Err(error) => Err(error),
    }
}

fn clear_provider_ui<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
) -> io::Result<()> {
    terminal.clear()
}

fn run_provider_ui(
    terminal: &mut DefaultTerminal,
    action: Action,
    providers: Vec<ProviderState>,
    selected: Option<usize>,
    palette: Palette,
) -> Result<Option<Outcome>> {
    let mut app = match selected {
        Some(index) => App::login_for_provider(providers, index),
        None => App::new(action, providers),
    };
    while !app.exit {
        terminal.draw(|frame| render(frame, &app, palette))?;
        if event::poll(Duration::from_millis(100))? {
            app.handle_event(event::read()?);
        }
    }
    Ok(app.outcome)
}

fn render(frame: &mut Frame, app: &App, palette: Palette) {
    let area = frame.area();
    match app.step {
        Step::Provider => render_provider_picker(frame, area, app, palette),
        Step::ApiKey => render_api_key(frame, area, app, palette),
        Step::ConfirmLogout => render_logout_confirmation(frame, area, app, palette),
    }
}

fn render_provider_picker(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let filtered = app.filtered_providers();
    let mut lines = Vec::with_capacity(13);
    let query = if app.query.is_empty() {
        Span::styled(
            if app.action == Action::Login {
                "Search providers"
            } else {
                "Search configured providers"
            },
            Style::default().fg(palette.muted),
        )
    } else {
        Span::styled(app.query.as_str(), Style::default().fg(palette.primary))
    };
    lines.push(Line::from(vec![Span::styled("> ", Style::default().fg(palette.accent)), query]));
    lines.push(Line::default());
    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No matching providers",
            Style::default().fg(palette.muted),
        )));
        for _ in 1..PAGE_SIZE {
            lines.push(Line::default());
        }
    } else {
        let page_start = (app.cursor / PAGE_SIZE) * PAGE_SIZE;
        for (visible_index, provider_index) in
            filtered.iter().skip(page_start).take(PAGE_SIZE).enumerate()
        {
            let selected = page_start + visible_index == app.cursor;
            lines.push(provider_line(
                &app.providers[*provider_index],
                selected,
                area.width,
                palette,
            ));
        }
        for _ in filtered.len().saturating_sub(page_start).min(PAGE_SIZE)..PAGE_SIZE {
            lines.push(Line::default());
        }
    }
    lines.push(Line::from(Span::styled(
        if filtered.is_empty() {
            "(0/0)".to_string()
        } else {
            format!("({}/{})", app.cursor + 1, filtered.len())
        },
        Style::default().fg(palette.muted),
    )));
    lines.push(Line::from(vec![
        Span::styled("*", Style::default().fg(palette.default)),
        Span::styled(" default  ", Style::default().fg(palette.muted)),
        Span::styled("•", Style::default().fg(palette.configured)),
        Span::styled(" configured", Style::default().fg(palette.muted)),
    ]));
    lines.push(Line::from(Span::styled(
        "type to search  ↑↓ move  enter select  esc clear/cancel",
        Style::default().fg(palette.muted),
    )));
    frame.render_widget(Paragraph::new(lines), area);
    let cursor_x = area.x + 2 + app.query.chars().count() as u16;
    frame.set_cursor_position(Position::new(cursor_x.min(area.right().saturating_sub(1)), area.y));
}

fn provider_line(
    state: &ProviderState,
    selected: bool,
    width: u16,
    palette: Palette,
) -> Line<'static> {
    const NAME_WIDTH: usize = 20;
    const MARKER_WIDTH: u16 = 2;
    let name_style = if selected {
        Style::default().fg(palette.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.primary)
    };
    let (marker, marker_style) = if state.default {
        ("* ", Style::default().fg(palette.default))
    } else if state.configured() {
        ("• ", Style::default().fg(palette.configured))
    } else {
        ("  ", Style::default().fg(palette.muted))
    };
    let endpoint_width = usize::from(width.saturating_sub(2 + MARKER_WIDTH + NAME_WIDTH as u16));
    let endpoint = truncate(&state.provider.endpoint, endpoint_width);
    Line::from(vec![
        Span::styled(if selected { "→ " } else { "  " }, Style::default().fg(palette.accent)),
        Span::styled(marker, marker_style),
        Span::styled(state.provider.name.clone(), name_style),
        Span::raw(" ".repeat(NAME_WIDTH.saturating_sub(state.provider.name.chars().count()))),
        Span::styled(endpoint, Style::default().fg(palette.muted)),
    ])
}

fn render_api_key(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let provider = &app.providers[app.selected.expect("selected provider")].provider;
    let bullets = "•".repeat(app.api_key.chars().count());
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Provider  ", Style::default().fg(palette.muted)),
            Span::styled(
                provider.name.clone(),
                Style::default().fg(palette.accent).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            format!("  {}", provider.endpoint),
            Style::default().fg(palette.muted),
        )),
        Line::default(),
        Line::from(vec![
            Span::styled("> API key  ", Style::default().fg(palette.accent)),
            Span::styled(bullets, Style::default().fg(palette.primary)),
        ]),
        Line::from(Span::styled(
            format!("  Credential: {}", provider.env),
            Style::default().fg(palette.muted),
        )),
    ];
    if app.validation_error {
        lines.push(Line::from(Span::styled(
            "  API key is required",
            Style::default().fg(palette.error),
        )));
    } else {
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        if app.direct {
            "enter save  esc cancel  ctrl+c cancel"
        } else {
            "enter save  esc back  ctrl+c cancel"
        },
        Style::default().fg(palette.muted),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    let cursor_x = area.x + 11 + app.api_key.chars().count() as u16;
    frame.set_cursor_position(Position::new(
        cursor_x.min(area.right().saturating_sub(1)),
        area.y + 3,
    ));
}

fn render_logout_confirmation(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let state = &app.providers[app.selected.expect("selected provider")];
    let detail = match state.credential {
        Some(CredentialSource::Stored) => "Removes the locally stored API key.",
        Some(CredentialSource::Environment) => {
            "The key comes from your environment; rx will show the unset command."
        }
        None => "No local credential is configured.",
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("Log out of ", Style::default().fg(palette.error)),
            Span::styled(
                state.provider.name.clone(),
                Style::default().fg(palette.primary).add_modifier(Modifier::BOLD),
            ),
            Span::styled("?", Style::default().fg(palette.error)),
        ]),
        Line::from(Span::styled(
            format!("  {}", state.provider.endpoint),
            Style::default().fg(palette.muted),
        )),
        Line::default(),
        Line::from(Span::styled(format!("  {detail}"), Style::default().fg(palette.primary))),
        Line::default(),
        Line::from(vec![
            Span::styled("y", Style::default().fg(palette.error).add_modifier(Modifier::BOLD)),
            Span::styled(" confirm  ", Style::default().fg(palette.muted)),
            Span::styled("n/esc", Style::default().fg(palette.accent)),
            Span::styled(" back", Style::default().fg(palette.muted)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn truncate(value: &str, width: usize) -> String {
    match width {
        0 => String::new(),
        1 => "…".to_string(),
        _ if value.chars().count() > width => {
            let mut truncated = value.chars().take(width - 1).collect::<String>();
            truncated.push('…');
            truncated
        }
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::provider::Setup;

    fn state(id: &str, name: &str, configured: bool, default: bool) -> ProviderState {
        ProviderState {
            provider: Provider {
                id: id.to_string(),
                name: name.to_string(),
                endpoint: format!("https://{id}.test/v1"),
                anthropic_base: None,
                default_context: None,
                env: format!("{}_API_KEY", id.to_ascii_uppercase()),
                setup: Setup::Generated,
                default_model: None,
                claude_default_model: None,
            },
            credential: configured.then_some(CredentialSource::Stored),
            environment_active: false,
            default,
        }
    }

    #[test]
    fn picker_pins_openrouter_and_tokener_then_configured_then_alpha() {
        let app = App::new(
            Action::Login,
            vec![
                state("zenmux", "Zenmux", true, false),
                state("tokener", "Tokener", false, false),
                state("abacus", "Abacus", false, false),
                state("openrouter", "OpenRouter", false, false),
                state("deepseek", "DeepSeek", true, false),
            ],
        );
        let names = app
            .filtered_providers()
            .into_iter()
            .map(|index| app.providers[index].provider.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["OpenRouter", "Tokener", "DeepSeek", "Zenmux", "Abacus"]);
    }

    #[test]
    fn picker_searches_hidden_provider_id() {
        let mut app = App::new(
            Action::Login,
            vec![
                state("openrouter", "OpenRouter", false, false),
                state("acme-edge", "Acme", false, false),
            ],
        );
        app.query = "edge".to_string();
        assert_eq!(app.filtered_providers(), vec![1]);
    }

    #[test]
    fn logout_picker_only_contains_configured_providers() {
        let app = App::new(
            Action::Logout,
            vec![
                state("openrouter", "OpenRouter", true, true),
                state("tokener", "Tokener", false, false),
            ],
        );
        assert_eq!(app.filtered_providers(), vec![0]);
    }

    #[test]
    fn use_picker_selects_default_without_authentication_step() {
        let mut app = App::new(
            Action::Use,
            vec![
                state("openrouter", "OpenRouter", true, false),
                state("tokener", "Tokener", true, true),
                state("unused", "Unused", false, false),
            ],
        );
        assert_eq!(app.filtered_providers(), vec![0, 1]);
        assert_eq!(app.cursor, 1);

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.exit);
        assert_eq!(app.step, Step::Provider);
        assert!(matches!(app.outcome, Some(Outcome::Use { index: 1 })));
    }

    #[test]
    fn direct_login_starts_at_the_api_key_step() {
        let mut app = App::login_for_provider(
            vec![
                state("openrouter", "OpenRouter", false, false),
                state("tokener-dev", "Tokener Dev", false, false),
            ],
            1,
        );

        assert_eq!(app.selected, Some(1));
        assert_eq!(app.step, Step::ApiKey);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(app.exit);
        assert_eq!(app.step, Step::ApiKey);
    }

    #[test]
    fn clearing_inline_ui_removes_rendered_content_and_resets_cursor() {
        let backend = TestBackend::new(40, 3);
        let mut terminal =
            Terminal::with_options(backend, TerminalOptions { viewport: Viewport::Inline(3) })
                .unwrap();
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("stale command suffix"), frame.area()))
            .unwrap();

        clear_provider_ui(&mut terminal).unwrap();

        assert!(
            terminal.backend().buffer().content.iter().skip(1).all(|cell| cell.symbol() == " ")
        );
        assert_eq!(terminal.get_cursor_position().unwrap(), Position::ORIGIN);
    }

    #[test]
    fn api_key_is_masked_in_the_rendered_terminal() {
        let mut app =
            App::new(Action::Login, vec![state("openrouter", "OpenRouter", false, false)]);
        app.step = Step::ApiKey;
        app.selected = Some(0);
        app.api_key = "sk-secret".to_string();
        let palette = Palette::current(&EnvLookup::isolated(Default::default()));
        let backend = TestBackend::new(80, 13);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &app, palette)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains("sk-secret"));
        assert!(rendered.contains("•••••••••"));
    }

    #[test]
    fn list_uses_one_mutually_exclusive_status_marker() {
        let default = state("openrouter", "OpenRouter", true, true);
        let configured = state("tokener", "Tokener", true, false);
        let output = render_list(&[&default, &configured]);
        assert!(output.contains("* OpenRouter"));
        assert!(output.contains("• Tokener"));
    }
}
