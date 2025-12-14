//! Terminal UI for interactive test execution.
//!
//! Provides a `cargo build`-style interface with a status bar at the bottom
//! and log output above that persists after the program exits.

use std::{
    io::{self, Write},
    sync::Arc,
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

use crate::reporter::TestResult;

/// Events sent from test runners to the TUI.
#[derive(Clone, Debug)]
pub enum TuiEvent {
    /// The test run is starting.
    RunStarted { total_tests: usize },
    /// A test has started running.
    TestStarted { name: String },
    /// A test has completed (passed or failed).
    TestCompleted { result: TestResult },
    /// All tests have finished.
    AllDone,
}

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
    /// Create a new TUI with the given total test count.
    pub fn new(total_tests: usize) -> io::Result<Self> {
        // Enable raw mode for input handling, but don't use alternate screen.
        terminal::enable_raw_mode()?;

        // Hide cursor during operation.
        let mut stdout = io::stdout();
        stdout.execute(cursor::Hide)?;

        let mut tui = Self {
            active_tests: Vec::new(),
            lines_printed: 0,
            total_tests,
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

    /// Create an event sender for test runners to use.
    pub fn create_event_channel() -> (mpsc::UnboundedSender<TuiEvent>, mpsc::UnboundedReceiver<TuiEvent>) {
        mpsc::unbounded_channel()
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

    /// Process a TUI event.
    pub fn handle_event(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::RunStarted { total_tests } => {
                self.add_line(format!("Running {} test(s)...", total_tests));
            }
            TuiEvent::TestStarted { name } => {
                self.add_line(format!("Starting test '{}'...", name));
                self.active_tests.push(name);
            }
            TuiEvent::TestCompleted { result } => {
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
            TuiEvent::AllDone => {
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
    pub fn render(&mut self) -> io::Result<()> {
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

        // Build status line.
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

        // Print status bar without newline.
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
    pub fn poll_input(&self) -> io::Result<bool> {
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
    pub fn shutdown(&mut self) -> io::Result<()> {
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

/// Run tests with the TUI.
pub async fn run_with_tui(
    test_cases: Vec<crate::config::TestCase>, parallelism: usize, fail_fast: bool, log_dir: Option<std::path::PathBuf>,
) -> io::Result<Vec<TestResult>> {
    let total = test_cases.len();
    let mut tui = Tui::new(total)?;

    // Log the log directory location if enabled.
    if let Some(ref dir) = log_dir {
        tui.add_line(format!("Container logs will be written to '{}'.", dir.display()));
    }

    let (tx, mut rx) = Tui::create_event_channel();

    // Create a cancellation token to signal tests to stop.
    let cancel_token = CancellationToken::new();

    // Spawn the test runner task.
    let runner_handle = tokio::spawn(run_tests_with_events(
        test_cases,
        parallelism,
        fail_fast,
        tx,
        cancel_token.clone(),
        log_dir,
    ));

    let mut results = Vec::new();
    let mut done = false;
    let mut cancelled = false;

    // Main event loop.
    while !done {
        // Check for user input (quit).
        if tui.poll_input()? {
            if !cancelled {
                cancelled = true;
                cancel_token.cancel();
                tui.add_line("Cancelling...");
            }
        }

        // Process any pending events.
        while let Ok(event) = rx.try_recv() {
            if let TuiEvent::TestCompleted { ref result } = event {
                results.push(result.clone());
            }
            if matches!(event, TuiEvent::AllDone) {
                done = true;
            }
            tui.handle_event(event);
        }

        // Render.
        tui.render()?;

        // Small delay to avoid busy-waiting.
        tokio::time::sleep(Duration::from_millis(16)).await;
    }

    // Wait for runner to finish.
    let _ = runner_handle.await;

    // Shut down TUI and restore terminal.
    tui.shutdown()?;

    Ok(results)
}

/// Run tests and send events to the TUI.
async fn run_tests_with_events(
    test_cases: Vec<crate::config::TestCase>, parallelism: usize, fail_fast: bool, tx: mpsc::UnboundedSender<TuiEvent>,
    cancel_token: CancellationToken, log_dir: Option<std::path::PathBuf>,
) -> Vec<TestResult> {
    use futures::stream::{self, StreamExt as _};
    use tokio::sync::Semaphore;

    use crate::runner::TestRunner;

    // Emit run started event.
    let _ = tx.send(TuiEvent::RunStarted {
        total_tests: test_cases.len(),
    });

    let semaphore = Arc::new(Semaphore::new(parallelism.max(1)));
    let tx = Arc::new(tx);
    let log_dir = Arc::new(log_dir);

    if fail_fast {
        // Sequential execution with fail-fast.
        let mut results = Vec::new();

        for test_case in test_cases {
            // Check for cancellation before starting each test.
            if cancel_token.is_cancelled() {
                break;
            }

            let _permit = semaphore.acquire().await.unwrap();
            let name = test_case.name.clone();

            let _ = tx.send(TuiEvent::TestStarted { name });

            let mut runner = TestRunner::new(test_case);
            if let Some(ref dir) = *log_dir {
                runner = runner.with_log_dir(dir.clone());
            }
            let result = runner.run().await;

            let failed = !result.passed;
            let _ = tx.send(TuiEvent::TestCompleted { result: result.clone() });
            results.push(result);

            if failed {
                break;
            }
        }

        let _ = tx.send(TuiEvent::AllDone);
        results
    } else {
        // Parallel execution.
        let cancel = cancel_token.clone();
        let results: Vec<TestResult> = stream::iter(test_cases)
            .take_while(|_| {
                let cancelled = cancel.is_cancelled();
                async move { !cancelled }
            })
            .map(|test_case| {
                let semaphore = semaphore.clone();
                let tx = tx.clone();
                let cancel = cancel_token.clone();
                let log_dir = log_dir.clone();

                async move {
                    // Check for cancellation before acquiring permit.
                    if cancel.is_cancelled() {
                        return None;
                    }

                    let _permit = semaphore.acquire().await.unwrap();

                    // Check again after acquiring permit.
                    if cancel.is_cancelled() {
                        return None;
                    }

                    let name = test_case.name.clone();
                    let _ = tx.send(TuiEvent::TestStarted { name });

                    let mut runner = TestRunner::new(test_case);
                    if let Some(ref dir) = *log_dir {
                        runner = runner.with_log_dir(dir.clone());
                    }
                    let result = runner.run().await;

                    let _ = tx.send(TuiEvent::TestCompleted { result: result.clone() });

                    Some(result)
                }
            })
            .buffer_unordered(parallelism.max(1))
            .filter_map(|r| async { r })
            .collect()
            .await;

        let _ = tx.send(TuiEvent::AllDone);
        results
    }
}
