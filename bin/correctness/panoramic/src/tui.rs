//! Terminal UI for interactive test execution.
//!
//! Provides a `cargo build`-style interface with a status bar at the bottom
//! and log output above that persists after the program exits.

use std::{
    io::{self, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

use chrono::Local;
use colored::Colorize as _;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    terminal, ExecutableCommand as _, QueueableCommand as _,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::events::TestEvent;

/// Terminal UI state and rendering.
pub struct Tui {
    /// Names of currently running tests.
    active_tests: Vec<String>,
    /// Number of log lines printed (for tracking).
    lines_printed: usize,
    /// Total number of tests to run.
    total_tests: usize,
    /// Number of tests completed so far.
    completed_tests: usize,
    /// Number of passed tests.
    passed_tests: usize,
    /// Number of failed tests.
    failed_tests: usize,
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
    /// Optional colored prefix (e.g., "PASS", "FAIL").
    prefix: Option<(String, LineColor)>,
    /// Main text content (always white).
    text: String,
}

#[derive(Clone, Copy)]
enum LineColor {
    Green,
    Red,
}

impl Tui {
    /// Create a new TUI.
    fn new() -> io::Result<Self> {
        // Enable raw mode for input handling, but don't use alternate screen.
        terminal::enable_raw_mode()?;

        // Hide cursor during operation.
        let mut stdout = io::stdout();
        stdout.execute(cursor::Hide)?;

        let mut tui = Self {
            active_tests: Vec::new(),
            lines_printed: 0,
            total_tests: 0,
            completed_tests: 0,
            passed_tests: 0,
            failed_tests: 0,
            started: Instant::now(),
            shutdown: false,
            pending_lines: Vec::new(),
        };

        // Add initial startup message.
        tui.add_line("Panoramic starting...");

        Ok(tui)
    }

    /// Add a log line to be printed.
    fn add_line(&mut self, text: impl Into<String>) {
        self.pending_lines.push(FormattedLine {
            timestamp: Local::now().format("%H:%M:%S").to_string(),
            prefix: None,
            text: text.into(),
        });
    }

    /// Add a log line with a colored prefix.
    fn add_line_with_prefix(&mut self, prefix: impl Into<String>, color: LineColor, text: impl Into<String>) {
        self.pending_lines.push(FormattedLine {
            timestamp: Local::now().format("%H:%M:%S").to_string(),
            prefix: Some((prefix.into(), color)),
            text: text.into(),
        });
    }

    /// Process a test event.
    fn handle_event(&mut self, event: TestEvent) {
        match event {
            TestEvent::RunStarted { total_tests } => {
                self.total_tests = total_tests;
                self.add_line(format!("Running {} test(s)...", total_tests));
            }
            TestEvent::TestStarted { name } => {
                self.add_line(format!("Starting test '{}'...", name));
                self.active_tests.push(name);
            }
            TestEvent::TestCompleted { result } => {
                // Remove from active tests.
                self.active_tests.retain(|n| n != &result.name);
                self.completed_tests += 1;

                let total_assertions = result.assertion_results.len();
                let passed_assertions = result.assertion_results.iter().filter(|a| a.passed).count();

                if result.passed {
                    self.passed_tests += 1;
                    self.add_line_with_prefix(
                        "PASS",
                        LineColor::Green,
                        format!(
                            "{} ({} assertions, {:.2?})",
                            result.name, total_assertions, result.duration
                        ),
                    );
                } else {
                    self.failed_tests += 1;
                    self.add_line_with_prefix(
                        "FAIL",
                        LineColor::Red,
                        format!(
                            "{} ({}/{} assertions passed, {:.2?})",
                            result.name, passed_assertions, total_assertions, result.duration
                        ),
                    );

                    // Add error details if present.
                    if let Some(ref error) = result.error {
                        self.add_line(format!("  Error: {}", error));
                    }

                    // Add failed assertion details.
                    for assertion in &result.assertion_results {
                        if !assertion.passed {
                            self.add_line(format!("  - {} ({:.2?})", assertion.name, assertion.duration));
                            self.add_line(format!("    {}", assertion.message));
                        }
                    }
                }
            }
            TestEvent::AllDone => {
                // Add summary line.
                let elapsed = self.started.elapsed();
                let (status, color) = if self.failed_tests == 0 {
                    ("PASSED", LineColor::Green)
                } else {
                    ("FAILED", LineColor::Red)
                };
                self.add_line(String::new());
                self.add_line_with_prefix(
                    status,
                    color,
                    format!(
                        "{} passed, {} failed, {} total ({:.2?})",
                        self.passed_tests, self.failed_tests, self.total_tests, elapsed
                    ),
                );
            }
        }
    }

    /// Render the current state to the terminal.
    fn render(&mut self) -> io::Result<()> {
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
        self.draw_status_bar(&mut stdout)?;

        stdout.flush()?;
        Ok(())
    }

    /// Draw the status bar.
    fn draw_status_bar(&self, stdout: &mut io::Stdout) -> io::Result<()> {
        let elapsed = self.started.elapsed();

        let progress = format!("[{}/{}]", self.completed_tests, self.total_tests);

        let active = if self.active_tests.is_empty() {
            String::new()
        } else if self.active_tests.len() <= 3 {
            format!(" Running: {}", self.active_tests.join(", "))
        } else {
            format!(
                " Running: {}, ... (+{} more)",
                self.active_tests[..3].join(", "),
                self.active_tests.len() - 3
            )
        };

        let elapsed_str = format!(" ({:.1}s)", elapsed.as_secs_f64());

        write!(
            stdout,
            "{}{}{}",
            progress.cyan().bold(),
            active.yellow(),
            elapsed_str.dimmed()
        )?;

        Ok(())
    }

    /// Check for user input (non-blocking).
    fn poll_input(&self) -> io::Result<bool> {
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // Allow Ctrl+C or 'q' to quit.
                if key.code == KeyCode::Char('c') && key.modifiers.contains(event::KeyModifiers::CONTROL) {
                    return Ok(true);
                }
                if key.code == KeyCode::Char('q') {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Shut down the TUI and restore the terminal.
    fn shutdown(&mut self) -> io::Result<()> {
        if self.shutdown {
            return Ok(());
        }

        self.shutdown = true;

        let mut stdout = io::stdout();

        // Clear the status bar line.
        stdout.queue(cursor::MoveToColumn(0))?;
        stdout.queue(terminal::Clear(terminal::ClearType::CurrentLine))?;

        // Show cursor again.
        stdout.execute(cursor::Show)?;

        // Disable raw mode.
        terminal::disable_raw_mode()?;

        stdout.flush()?;
        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Run the TUI event consumer.
///
/// This function consumes test events from the channel and renders them to the terminal.
/// It returns when `AllDone` is received or the user cancels via Ctrl+C or 'q'.
pub async fn run_tui_consumer(
    mut rx: mpsc::UnboundedReceiver<TestEvent>, cancel_token: CancellationToken, log_dir: Option<PathBuf>,
) -> io::Result<()> {
    let mut tui = Tui::new()?;

    // Log the log directory location if enabled.
    if let Some(ref dir) = log_dir {
        tui.add_line(format!("Container logs will be written to '{}'.", dir.display()));
    }

    let mut done = false;
    let mut cancelled = false;

    while !done {
        if tui.poll_input()? && !cancelled {
            cancelled = true;
            cancel_token.cancel();
            tui.add_line("Cancelling...");
        }

        while let Ok(event) = rx.try_recv() {
            if matches!(event, TestEvent::AllDone) {
                done = true;
            }
            tui.handle_event(event);
        }

        tui.render()?;

        tokio::time::sleep(Duration::from_millis(16)).await;
    }

    tui.shutdown()?;

    Ok(())
}
