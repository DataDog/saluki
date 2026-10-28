//! Terminal UI with a bottom status bar and persistent log output above it.
//!
//! The consume loop forwards events, polls interrupt keys, and renders the run report.

use std::{
    io::{self, Write},
    path::Path,
    time::{Duration, Instant},
};

use chrono::Local;
use colored::Colorize as _;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal, ExecutableCommand as _, QueueableCommand as _,
};

use crate::events::TestEvent;
use crate::machine_output::RunReport;
use crate::record::StepStatus;
use crate::reporter::{run_counts, run_status_label, run_unfinished_lines, TestResult, RUN_PASSED_LABEL};

/// Terminal UI state and rendering.
pub struct Tui {
    /// Number of log lines printed (for tracking).
    lines_printed: usize,
    /// When the test run started.
    started: Instant,
    /// Whether the TUI has been shut down.
    shutdown: bool,
    /// Lines pending to be printed.
    pending_lines: Vec<FormattedLine>,
}

/// A formatted line ready to print.
struct FormattedLine {
    timestamp: String,
    /// Optional colored prefix.
    ///
    /// Useful for adding typical status/outcome prefixes, such as "PASS" colored in green, or "FAIL" colored in red.
    prefix: Option<(String, LineColor)>,
    /// Main text content (always white).
    text: String,
}

impl FormattedLine {
    fn new(text: impl Into<String>) -> Self {
        Self {
            timestamp: Local::now().format("%H:%M:%S").to_string(),
            prefix: None,
            text: text.into(),
        }
    }

    fn with_prefix(prefix: impl Into<String>, color: LineColor, text: impl Into<String>) -> Self {
        Self {
            prefix: Some((prefix.into(), color)),
            ..Self::new(text)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LineColor {
    Green,
    Red,
}

impl Tui {
    /// Enables raw mode, hides the cursor, and queues startup messages.
    pub fn new(log_dir: &Path) -> io::Result<Self> {
        // Enable raw mode for input handling, but don't use alternate screen.
        terminal::enable_raw_mode()?;

        let mut tui = Self {
            lines_printed: 0,
            started: Instant::now(),
            shutdown: false,
            pending_lines: Vec::new(),
        };

        // Construct `tui` before fallible output so its destructor restores the terminal on error.
        io::stdout().execute(cursor::Hide)?;

        tui.add_line("Panoramic starting...");
        tui.add_line(format!("Container logs will be written to '{}'.", log_dir.display()));

        Ok(tui)
    }

    /// Add a log line to be printed.
    fn add_line(&mut self, text: impl Into<String>) {
        self.pending_lines.push(FormattedLine::new(text));
    }

    /// Queues the lines for one runner event.
    pub fn show_event(&mut self, event: &TestEvent) {
        match event {
            TestEvent::RunStarted { total_tests } => self.add_line(format!("Running {} test(s)...", total_tests)),
            TestEvent::TestStarted { name } => self.add_line(format!("Starting test '{}'...", name)),
            TestEvent::TestCompleted { result, log_dir } => {
                self.pending_lines.extend(completed_test_lines(result, log_dir));
            }
            TestEvent::StatusLine { message } => self.add_line(message.clone()),
        }
    }

    /// Queues a line from Panoramic itself, such as an interrupt notice.
    pub fn show_notice(&mut self, notice: &str) {
        self.add_line(notice);
    }

    /// Reads the pending key presses without blocking, returning the first interrupt key's name.
    ///
    /// Keys after an interrupt key stay pending for the next call.
    pub fn poll_interrupt_key(&mut self) -> io::Result<Option<&'static str>> {
        while event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()? {
                if let Some(source) = interrupt_key(&key) {
                    return Ok(Some(source));
                }
            }
        }
        Ok(None)
    }

    /// Prints the pending lines and redraws the status bar from `report`.
    pub fn render(&mut self, report: &RunReport) -> io::Result<()> {
        let mut stdout = io::stdout();

        // Always move to column 0 and clear the current line first.
        // This clears the status bar from the previous frame.
        stdout.execute(cursor::MoveToColumn(0))?;
        stdout.execute(terminal::Clear(terminal::ClearType::CurrentLine))?;

        // Print any pending log lines.
        // In raw mode, \n doesn't return to column 0, so we use \r\n.
        for line in self.pending_lines.drain(..) {
            let formatted = match line.prefix {
                Some((prefix, color)) => {
                    let colored_prefix = match color {
                        LineColor::Green => prefix.green().bold().to_string(),
                        LineColor::Red => prefix.red().bold().to_string(),
                    };
                    format!("{} {}", colored_prefix, line.text.white())
                }
                None => line.text.white().to_string(),
            };
            write!(stdout, "{}  {}\r\n", line.timestamp.dimmed(), formatted)?;
            self.lines_printed += 1;
        }

        // Draw the status bar.
        self.draw_status_bar(&mut stdout, report)?;

        stdout.flush()?;
        Ok(())
    }

    /// Draw the status bar.
    fn draw_status_bar(&self, stdout: &mut io::Stdout, report: &RunReport) -> io::Result<()> {
        let elapsed = self.started.elapsed();

        let selected = report.tests.len() + report.running.len() + report.not_run.len();
        let progress = format!("[{}/{}]", report.tests.len(), selected);

        let running = &report.running;
        let active = if running.is_empty() {
            String::new()
        } else if running.len() <= 3 {
            format!(" Running: {}", running.join(", "))
        } else {
            format!(
                " Running: {}, ... (+{} more)",
                running[..3].join(", "),
                running.len() - 3
            )
        };

        let elapsed_str = format!(" ({:.1}s)", elapsed.as_secs_f64());
        let status = truncate_status_bar(&progress, &active, &elapsed_str);

        write!(
            stdout,
            "{}{}{}",
            status.progress.cyan().bold(),
            status.active.yellow(),
            status.elapsed.dimmed()
        )?;

        Ok(())
    }

    /// Prints the run summary and restores the terminal.
    pub fn finish(mut self, report: &RunReport) -> io::Result<()> {
        let status = run_status_label(report);
        let color = if status == RUN_PASSED_LABEL {
            LineColor::Green
        } else {
            LineColor::Red
        };
        self.add_line(String::new());
        self.pending_lines
            .push(FormattedLine::with_prefix(status, color, run_counts(report)));
        for line in run_unfinished_lines(report) {
            self.add_line(line);
        }

        self.render(report)?;
        self.shutdown()
    }

    /// Shut down the TUI and restore the terminal.
    fn shutdown(&mut self) -> io::Result<()> {
        if self.shutdown {
            return Ok(());
        }

        self.shutdown = true;

        let mut stdout = io::stdout();
        let redrawn = stdout
            .queue(cursor::MoveToColumn(0))
            .and_then(|stdout| stdout.queue(terminal::Clear(terminal::ClearType::CurrentLine)))
            .and_then(|stdout| stdout.execute(cursor::Show))
            .and_then(|stdout| stdout.flush());

        // Attempt to restore terminal input even if stdout fails.
        let restored = terminal::disable_raw_mode();

        redrawn.and(restored)
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Recognizes Ctrl-C and `q` key presses as interrupts.
///
/// Raw mode delivers Ctrl-C as a key event. Ignoring releases and repeats avoids counting one press twice.
fn interrupt_key(key: &KeyEvent) -> Option<&'static str> {
    if key.kind != KeyEventKind::Press {
        return None;
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some("Ctrl-C key"),
        KeyCode::Char('q') => Some("q key"),
        _ => None,
    }
}

/// The lines the TUI prints for a completed test.
fn completed_test_lines(result: &TestResult, log_dir: &Path) -> Vec<FormattedLine> {
    let total_assertions = result.assertion_results.len();

    if result.outcome.is_passed() {
        let mut lines = vec![FormattedLine::with_prefix(
            result.label(),
            LineColor::Green,
            format!(
                "{} ({} assertions, {:.2?})",
                result.name, total_assertions, result.duration
            ),
        )];
        lines.extend(
            result
                .cause_lines()
                .into_iter()
                .map(|line| FormattedLine::new(format!("  {}", line))),
        );
        return lines;
    }

    let passed_assertions = result
        .assertion_results
        .iter()
        .filter(|a| a.status == StepStatus::Passed)
        .count();
    let mut lines = vec![FormattedLine::with_prefix(
        result.label(),
        LineColor::Red,
        format!(
            "{} ({}/{} assertions passed, {:.2?})",
            result.name, passed_assertions, total_assertions, result.duration
        ),
    )];

    lines.extend(
        result
            .cause_lines()
            .into_iter()
            .map(|line| FormattedLine::new(format!("  {}", line))),
    );

    for assertion in &result.assertion_results {
        if !matches!(assertion.status, StepStatus::Passed | StepStatus::NotRun) {
            lines.push(FormattedLine::new(format!(
                "  - {} ({:.2?})",
                assertion.name, assertion.duration
            )));
            for line in assertion.message.lines() {
                lines.push(FormattedLine::new(format!("    {}", line)));
            }
        }
    }

    if !result.phase_timings.is_empty() {
        lines.push(FormattedLine::new("  Phase timings:"));
        for phase in &result.phase_timings {
            lines.push(FormattedLine::new(format!(
                "    {} ({:.2?})",
                phase.phase, phase.duration
            )));
        }
    }

    lines.push(FormattedLine::new(format!("  Logs: {}", log_dir.display())));
    lines
}

struct StatusBar {
    progress: String,
    active: String,
    elapsed: String,
}

fn truncate_status_bar(progress: &str, active: &str, elapsed: &str) -> StatusBar {
    let Ok((columns, _)) = terminal::size() else {
        return StatusBar {
            progress: progress.to_string(),
            active: active.to_string(),
            elapsed: elapsed.to_string(),
        };
    };

    let max_width = usize::from(columns);
    let fixed_width = progress.chars().count() + elapsed.chars().count();
    let active = if fixed_width >= max_width {
        String::new()
    } else {
        truncate_to_width(active, max_width - fixed_width)
    };

    StatusBar {
        progress: progress.to_string(),
        active,
        elapsed: elapsed.to_string(),
    }
}

fn truncate_to_width(value: &str, max_width: usize) -> String {
    let width = value.chars().count();
    if width <= max_width {
        return value.to_string();
    }

    if max_width == 0 {
        return String::new();
    }

    if max_width == 1 {
        return "…".to_string();
    }

    let mut truncated = value.chars().take(max_width - 1).collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEventState;

    use super::*;
    use crate::reporter::test_support::{each_outcome, NOT_RUN_KIND, PASS_ISSUE, TIMEOUT_CAUSE};

    #[test]
    fn tui_labels_every_outcome_and_prints_a_timeout_cause() {
        for (result, label) in each_outcome() {
            let lines = completed_test_lines(&result, Path::new("/logs"));
            let color = if label == "PASS" {
                LineColor::Green
            } else {
                LineColor::Red
            };
            assert_eq!(lines[0].prefix, Some((label.to_string(), color)));
            let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
            match label {
                "PASS" => assert_eq!(texts[1..], [format!("  {}", PASS_ISSUE)]),
                "FAIL" => assert!(
                    texts.iter().all(|text| !text.contains(NOT_RUN_KIND)),
                    "a step that never ran should not be listed: {texts:?}"
                ),
                "TIMEOUT" => assert_eq!(texts[1], format!("  {}", TIMEOUT_CAUSE)),
                _ => {}
            }
        }
    }

    #[test]
    fn only_a_press_of_ctrl_c_or_q_is_an_interrupt_key() {
        let key = |code, modifiers, kind| KeyEvent {
            code,
            modifiers,
            kind,
            state: KeyEventState::NONE,
        };
        let cases = [
            (
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press,
                Some("Ctrl-C key"),
            ),
            (
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KeyEventKind::Press,
                Some("q key"),
            ),
            (KeyCode::Char('c'), KeyModifiers::CONTROL, KeyEventKind::Release, None),
            (KeyCode::Char('c'), KeyModifiers::CONTROL, KeyEventKind::Repeat, None),
            (KeyCode::Char('q'), KeyModifiers::NONE, KeyEventKind::Release, None),
            (KeyCode::Char('c'), KeyModifiers::NONE, KeyEventKind::Press, None),
            (KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press, None),
        ];

        for (code, modifiers, kind, expected) in cases {
            assert_eq!(
                interrupt_key(&key(code, modifiers, kind)),
                expected,
                "{:?} {:?} {:?}",
                code,
                modifiers,
                kind
            );
        }
    }
}
