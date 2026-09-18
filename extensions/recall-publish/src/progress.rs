use std::io::{IsTerminal, Write, stderr};

pub struct Progress {
    tty: bool,
    quiet: bool,
    open: std::cell::Cell<bool>,
}

impl Progress {
    pub fn new(quiet: bool) -> Self {
        Self { tty: stderr().is_terminal(), quiet, open: std::cell::Cell::new(false) }
    }

    pub fn silent() -> Self {
        Self { tty: false, quiet: true, open: std::cell::Cell::new(false) }
    }

    pub fn step(&self, message: &str) {
        if self.quiet {
            return;
        }
        self.clear();
        let _ = writeln!(stderr(), "{message}");
    }

    pub fn tick(&self, message: &str) {
        if self.quiet {
            return;
        }
        if !self.tty {
            return;
        }
        let _ = write!(stderr(), "\r\x1b[K{message}");
        let _ = stderr().flush();
        self.open.set(true);
    }

    fn clear(&self) {
        if self.open.replace(false) {
            let _ = write!(stderr(), "\r\x1b[K");
            let _ = stderr().flush();
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if self.open.get() {
            let _ = writeln!(stderr());
        }
    }
}
