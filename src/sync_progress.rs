use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(500);
const MIN_REDRAW: Duration = Duration::from_millis(100);

pub(crate) struct SyncProgress {
    total: usize,
    index: usize,
    line: Option<Arc<Mutex<Line>>>,
    stop: Option<Sender<()>>,
    ticker: Option<JoinHandle<()>>,
}

struct Line {
    transient: Option<(String, Instant)>,
    last_width: usize,
    last_draw: Instant,
}

impl SyncProgress {
    pub(crate) fn disabled() -> Self {
        Self { total: 0, index: 0, line: None, stop: None, ticker: None }
    }

    pub(crate) fn for_terminal(total: usize) -> Self {
        if total == 0 || !io::stderr().is_terminal() {
            return Self { total, index: 0, line: None, stop: None, ticker: None };
        }
        let line = Arc::new(Mutex::new(Line {
            transient: None,
            last_width: 0,
            last_draw: Instant::now(),
        }));
        let (stop, wake) = channel::<()>();
        let ticker = {
            let line = Arc::clone(&line);
            std::thread::spawn(move || {
                while wake.recv_timeout(TICK) == Err(RecvTimeoutError::Timeout) {
                    if let Ok(mut line) = line.lock() {
                        line.redraw_transient();
                    }
                }
            })
        };
        Self { total, index: 0, line: Some(line), stop: Some(stop), ticker: Some(ticker) }
    }

    pub(crate) fn begin_source(&mut self, label: &str) {
        self.index += 1;
        let text = format!("[{}/{}] {label}: scanning", self.index, self.total);
        self.with_line(|line| line.set_transient(text, true));
    }

    pub(crate) fn indexing(&mut self, label: &str, done: usize, total: usize) {
        let text = format!("[{}/{}] {label}: indexing {done}/{total}", self.index, self.total);
        self.with_line(|line| line.set_transient(text, false));
    }

    pub(crate) fn end_source(&mut self, label: &str, found: usize, touched: u32, elapsed_ms: u128) {
        if found == 0 && touched == 0 {
            self.with_line(Line::clear);
            return;
        }
        let text = format!(
            "[{}/{}] {label}: {found} sessions read, {touched} indexed, {}",
            self.index,
            self.total,
            format_elapsed(elapsed_ms)
        );
        if self.line.is_some() {
            self.with_line(|line| line.print_permanent(&text));
        } else if self.total > 0 {
            eprintln!("{text}");
        }
    }

    pub(crate) fn finish(&mut self) {
        self.with_line(Line::clear);
        drop(self.stop.take());
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.join();
        }
    }

    fn with_line(&self, apply: impl FnOnce(&mut Line)) {
        if let Some(line) = &self.line
            && let Ok(mut line) = line.lock()
        {
            apply(&mut line);
        }
    }
}

impl Drop for SyncProgress {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Line {
    fn set_transient(&mut self, text: String, restart_clock: bool) {
        let force = restart_clock || self.transient.is_none();
        let since = match (&self.transient, restart_clock) {
            (Some((_, since)), false) => *since,
            _ => Instant::now(),
        };
        self.transient = Some((text, since));
        if force || self.last_draw.elapsed() >= MIN_REDRAW {
            self.redraw_transient();
        }
    }

    fn redraw_transient(&mut self) {
        let Some((text, since)) = &self.transient else {
            return;
        };
        let elapsed = since.elapsed().as_secs();
        let rendered = if elapsed >= 2 { format!("{text} ({elapsed}s)") } else { text.clone() };
        self.overwrite(&rendered);
    }

    fn print_permanent(&mut self, text: &str) {
        self.clear();
        eprintln!("{text}");
        let _ = io::stderr().flush();
    }

    fn overwrite(&mut self, text: &str) {
        let width = text.chars().count();
        let padding = " ".repeat(self.last_width.saturating_sub(width));
        eprint!("\r{text}{padding}");
        let _ = io::stderr().flush();
        self.last_width = width.max(self.last_width);
        self.last_draw = Instant::now();
    }

    fn clear(&mut self) {
        self.transient = None;
        if self.last_width == 0 {
            return;
        }
        eprint!("\r{}\r", " ".repeat(self.last_width));
        let _ = io::stderr().flush();
        self.last_width = 0;
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: f64 = (1u64 << 20) as f64;
    let bytes = bytes as f64;
    if bytes >= GIB { format!("{:.1} GiB", bytes / GIB) } else { format!("{:.0} MiB", bytes / MIB) }
}

pub(crate) fn format_elapsed(elapsed_ms: u128) -> String {
    if elapsed_ms < 1000 {
        format!("{elapsed_ms}ms")
    } else if elapsed_ms < 60_000 {
        format!("{:.1}s", elapsed_ms as f64 / 1000.0)
    } else {
        let secs = elapsed_ms / 1000;
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}
