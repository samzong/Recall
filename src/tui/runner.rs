use anyhow::Result;

use crate::adapters;
use crate::config::AppConfig;
use crate::db::search::{SearchEngine, TimeRange};
use crate::db::store::Store;
use crate::embedding::EmbeddingProvider;
use crate::semantic;
use crate::sync::run_dashboard_sync_job;

pub fn run(usage_start: Option<(Option<Vec<String>>, Option<TimeRange>)>) -> Result<()> {
    use std::io;
    use std::time::Duration;

    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;

    use crate::tui::app::{App, AppMode};
    use crate::tui::event::{AppEvent, poll_event};
    use crate::tui::ui;

    let usage_mode = usage_start.is_some();
    let store = Store::open()?;
    semantic::ensure_background_worker(true)?;
    let sources = if usage_start.is_some() {
        adapters::dashboard_source_labels()
    } else {
        adapters::source_labels()
    };

    struct TerminalGuard;
    impl Drop for TerminalGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let _ =
                execute!(io::stdout(), crossterm::event::DisableMouseCapture, LeaveAlternateScreen);
        }
    }

    enable_raw_mode()?;
    let guard = TerminalGuard;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::event::EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let engine = SearchEngine::new(&store.conn);
    let mut provider: Option<EmbeddingProvider> = None;
    let mut config = AppConfig::load_or_default();
    config.normalize_sources(&sources);

    let mut app = App::new(&store, sources, config);
    if let Some((source_filter, time_filter)) = usage_start {
        app.source_filter_selection = source_filter.unwrap_or_default();
        if let Some(time_filter) = time_filter {
            app.usage_time_filter = time_filter;
        }
        app.mode = AppMode::Usage;
        app.request_usage_refresh();
    }
    let mut usage_sync_pending = usage_mode;
    let tick_rate = Duration::from_millis(50);

    loop {
        app.poll_share_publish();
        terminal.draw(|f| ui::render(f, &app))?;

        match poll_event(tick_rate)? {
            AppEvent::Key(key) => {
                app.handle_key(key, &store, &engine, &mut provider);
            }
            AppEvent::ScrollUp => app.handle_scroll_up(&store),
            AppEvent::ScrollDown => app.handle_scroll_down(&store),
            AppEvent::Tick => {}
        }

        if app.should_quit {
            break;
        }

        if app.usage_refresh_is_due() {
            if usage_sync_pending {
                usage_sync_pending = false;
                match run_dashboard_sync_job() {
                    Ok(()) => app.refresh_usage(&store),
                    Err(err) => app.fail_usage_refresh(err),
                }
            } else {
                app.refresh_usage(&store);
            }
        }

        app.try_search(&store, &engine, &mut provider);

        if app.should_quit {
            break;
        }
    }

    drop(guard);
    terminal.show_cursor()?;

    if let Some((command, cwd)) = app.exec_on_exit.take() {
        exec_resume(command, cwd)?;
    }

    Ok(())
}

#[cfg(unix)]
fn exec_resume(command: adapters::ResumeCommand, cwd: Option<String>) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let mut cmd = std::process::Command::new(&command.program);
    cmd.args(&command.args);
    if let Some(ref dir) = cwd
        && std::path::Path::new(dir).is_dir()
    {
        cmd.current_dir(dir);
    }
    let err = cmd.exec();
    Err(anyhow::anyhow!("failed to exec {}: {err}", command.program))
}

#[cfg(not(unix))]
fn exec_resume(command: adapters::ResumeCommand, cwd: Option<String>) -> Result<()> {
    let mut cmd = std::process::Command::new(&command.program);
    cmd.args(&command.args);
    if let Some(ref dir) = cwd
        && std::path::Path::new(dir).is_dir()
    {
        cmd.current_dir(dir);
    }
    let status =
        cmd.status().map_err(|e| anyhow::anyhow!("failed to run {}: {e}", command.program))?;
    std::process::exit(status.code().unwrap_or(0));
}
