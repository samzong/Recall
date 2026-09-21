use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing_subscriber::fmt::MakeWriter;

const TICK: Duration = Duration::from_millis(500);
const MIN_REDRAW: Duration = Duration::from_millis(100);

static ACTIVE_LINE: Mutex<Option<Arc<Mutex<Line>>>> = Mutex::new(None);

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
        if total == 0 { Self::disabled() } else { Self::rendering(total) }
    }

    pub(crate) fn for_phases() -> Self {
        Self::rendering(0)
    }

    fn rendering(total: usize) -> Self {
        if !io::stderr().is_terminal() {
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
        if let Ok(mut active) = ACTIVE_LINE.lock() {
            *active = Some(Arc::clone(&line));
        }
        Self { total, index: 0, line: Some(line), stop: Some(stop), ticker: Some(ticker) }
    }

    pub(crate) fn phase(&mut self, text: &str) {
        let text = text.to_string();
        self.with_line(|line| line.set_transient(text, true));
    }

    pub(crate) fn detail(&mut self, text: String) {
        self.with_line(|line| line.set_transient(text, false));
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

    pub(crate) fn end_source(
        &mut self,
        label: &str,
        found: usize,
        touched: u32,
        busy: u32,
        elapsed_ms: u128,
    ) {
        if found == 0 && touched == 0 && busy == 0 {
            self.with_line(Line::clear);
            return;
        }
        let busy = if busy == 0 { String::new() } else { format!(", {busy} busy") };
        let text = format!(
            "[{}/{}] {label}: {found} sessions read, {touched} indexed{busy}, {}",
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
        if let Some(line) = &self.line
            && let Ok(mut active) = ACTIVE_LINE.lock()
            && active.as_ref().is_some_and(|current| Arc::ptr_eq(current, line))
        {
            *active = None;
        }
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
        self.clear_render();
    }

    fn clear_render(&mut self) {
        if self.last_width == 0 {
            return;
        }
        eprint!("\r{}\r", " ".repeat(self.last_width));
        let _ = io::stderr().flush();
        self.last_width = 0;
    }
}

pub(crate) struct ProgressAwareStderr;

impl<'a> MakeWriter<'a> for ProgressAwareStderr {
    type Writer = LogRecord;

    fn make_writer(&'a self) -> Self::Writer {
        LogRecord { buf: Vec::new() }
    }
}

pub(crate) struct LogRecord {
    buf: Vec<u8>,
}

impl Write for LogRecord {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LogRecord {
    fn drop(&mut self) {
        let line = ACTIVE_LINE.lock().ok().and_then(|active| active.clone());
        let held = line.as_ref().and_then(|line| line.try_lock().ok());
        match held {
            Some(mut line) => {
                line.clear_render();
                write_stderr(&self.buf);
                line.redraw_transient();
            }
            None => write_stderr(&self.buf),
        }
    }
}

fn write_stderr(buf: &[u8]) {
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_all(buf);
    let _ = stderr.flush();
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: f64 = (1u64 << 20) as f64;
    const KIB: f64 = (1u64 << 10) as f64;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.0} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.0} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
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
