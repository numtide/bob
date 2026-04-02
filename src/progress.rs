//! Terminal progress display for build output.
//!
//! Renders a cargo-style progress line that updates in-place:
//!   [42/170] Building serde v1.0.219 (3 active)
//! Completed crates scroll above the progress line.

use std::io::Write;
use std::sync::Mutex;

/// ANSI color codes.
const GREEN: &str = "\x1b[32m";
const BOLD_GREEN: &str = "\x1b[1;32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[1;31m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

pub struct Progress {
    inner: Mutex<ProgressInner>,
}

struct ProgressInner {
    total: usize,
    completed: usize,
    cached_total: usize,
    active: Vec<String>,
    is_tty: bool,
    /// Width of the last progress line (for clearing).
    last_line_len: usize,
}

impl Progress {
    pub fn new(to_build: usize, cached: usize) -> Self {
        let is_tty = unsafe { libc::isatty(2) != 0 };
        Self {
            inner: Mutex::new(ProgressInner {
                total: to_build + cached,
                completed: cached,
                cached_total: cached,
                active: Vec::new(),
                is_tty,
                last_line_len: 0,
            }),
        }
    }

    /// A crate started building.
    pub fn start(&self, name: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.active.push(name.to_string());
        inner.render_progress();
    }

    /// A crate finished building successfully.
    pub fn finish(&self, name: &str, duration: std::time::Duration) {
        let mut inner = self.inner.lock().unwrap();
        inner.active.retain(|n| n != name);
        inner.completed += 1;
        inner.clear_line();

        let secs = duration.as_secs_f64();
        let stderr = std::io::stderr();
        let mut err = stderr.lock();
        let _ = writeln!(
            err,
            "  {BOLD_GREEN}Built{RESET} {name} ({secs:.1}s)"
        );
        inner.render_progress();
    }

    /// A crate build failed.
    pub fn fail(&self, name: &str, stdout: &str, stderr_text: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.active.retain(|n| n != name);
        inner.completed += 1;
        inner.clear_line();

        let err = std::io::stderr();
        let mut err = err.lock();
        let _ = writeln!(err, "  {RED}FAILED{RESET} {name}");

        // Show last few lines of output for context
        let combined = if !stderr_text.is_empty() { stderr_text } else { stdout };
        let lines: Vec<&str> = combined.lines().collect();
        let show = &lines[lines.len().saturating_sub(10)..];
        for line in show {
            let _ = writeln!(err, "    {DIM}│{RESET} {line}");
        }

        inner.render_progress();
    }

    /// Print the final summary line.
    pub fn summary(&self, built: usize, cached: usize, failed: usize, duration: std::time::Duration) {
        let mut inner = self.inner.lock().unwrap();
        inner.clear_line();

        let secs = duration.as_secs_f64();
        let stderr = std::io::stderr();
        let mut err = stderr.lock();

        if failed > 0 {
            let _ = writeln!(
                err,
                "\n{BOLD}  {GREEN}{built} built{RESET}{BOLD}, {cached} cached, {RED}{failed} failed{RESET}{BOLD} in {secs:.1}s{RESET}"
            );
        } else {
            let _ = writeln!(
                err,
                "\n{BOLD}  {GREEN}{built} built{RESET}{BOLD}, {cached} cached in {secs:.1}s{RESET}"
            );
        }
    }
}

impl ProgressInner {
    fn clear_line(&mut self) {
        if !self.is_tty { return; }
        let stderr = std::io::stderr();
        let mut err = stderr.lock();
        // Move to start of line and clear
        let _ = write!(err, "\r{}\r", " ".repeat(self.last_line_len.min(200)));
        self.last_line_len = 0;
    }

    fn render_progress(&mut self) {
        if !self.is_tty {
            return;
        }

        let stderr = std::io::stderr();
        let mut err = stderr.lock();

        let line = if self.active.is_empty() {
            format!(
                "  {DIM}[{completed}/{total}]{RESET} waiting...",
                completed = self.completed,
                total = self.total,
            )
        } else {
            let current = &self.active[self.active.len() - 1];
            let n_active = self.active.len();
            if n_active == 1 {
                format!(
                    "  {DIM}[{completed}/{total}]{RESET} {YELLOW}Building{RESET} {current}",
                    completed = self.completed,
                    total = self.total,
                )
            } else {
                format!(
                    "  {DIM}[{completed}/{total}]{RESET} {YELLOW}Building{RESET} {current} {DIM}(+{more} more){RESET}",
                    completed = self.completed,
                    total = self.total,
                    more = n_active - 1,
                )
            }
        };

        // Track visible length (without ANSI escapes) for clearing
        let visible_len = strip_ansi_len(&line);
        self.last_line_len = visible_len;

        let _ = write!(err, "\r{line}");
        let _ = err.flush();
    }
}

/// Count visible characters (strip ANSI escape sequences).
fn strip_ansi_len(s: &str) -> usize {
    let mut len = 0;
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c == 'm' {
                in_escape = false;
            }
        } else {
            len += 1;
        }
    }
    len
}
