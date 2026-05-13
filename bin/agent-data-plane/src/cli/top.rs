//! `adp top` -- live supervision tree visualization.
//!
//! Periodically polls the running data plane's `/supervision/tree` endpoint and renders the resulting tree (or flat
//! table) to the terminal. Designed for interactive debugging in the spirit of `top` / `htop`, but with the
//! supervision tree structure surfaced.

use std::{
    io::{self, Write as _},
    time::{Duration, SystemTime},
};

use argh::FromArgs;
use crossterm::{
    cursor,
    event::{poll, read, Event, KeyCode, KeyEvent, KeyModifiers},
    queue,
    style::{Attribute, Print, SetAttribute},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use saluki_config::GenericConfiguration;
use saluki_core::runtime::introspection::{ProcessSnapshot, SupervisionTree};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;
use tracing::error;

use crate::cli::utils::DataPlaneAPIClient;

/// Live view of the supervision tree.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "top")]
pub struct TopCommand {
    /// refresh interval in milliseconds (default 1000)
    #[argh(option, short = 'r', long = "refresh-ms", default = "1000")]
    refresh_ms: u64,

    /// emit a single snapshot and exit (skips the interactive TUI)
    #[argh(switch, long = "once")]
    once: bool,

    /// initial view: "tree" or "table" (default "tree")
    #[argh(option, short = 'v', long = "view", default = "ViewMode::Tree")]
    view: ViewMode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ViewMode {
    #[default]
    Tree,
    Table,
}

impl argh::FromArgValue for ViewMode {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        match value.to_lowercase().as_str() {
            "tree" => Ok(Self::Tree),
            "table" => Ok(Self::Table),
            other => Err(format!("invalid view mode '{}': expected 'tree' or 'table'", other)),
        }
    }
}

#[derive(Deserialize)]
struct TreeResponse {
    processes: Vec<ProcessSnapshot>,
}

/// Entrypoint for `adp top`.
pub async fn handle_top_command(bootstrap_config: &GenericConfiguration, cmd: TopCommand) {
    let mut api_client = match DataPlaneAPIClient::unprivileged_from_config(bootstrap_config) {
        Ok(client) => client,
        Err(e) => {
            error!("Failed to create data plane API client: {:#}", e);
            std::process::exit(1);
        }
    };

    let result = if cmd.once {
        emit_once(&mut api_client, cmd.view).await
    } else {
        run_tui(&mut api_client, cmd.refresh_ms, cmd.view).await
    };

    if let Err(e) = result {
        error!("`adp top` failed: {:#}", e);
        std::process::exit(1);
    }
}

async fn emit_once(client: &mut DataPlaneAPIClient, view: ViewMode) -> Result<(), GenericError> {
    let tree = fetch_tree(client).await?;
    let mut out = String::new();
    let now = SystemTime::now();
    match view {
        ViewMode::Tree => tree
            .render_tree(&mut out, now)
            .map_err(|e| generic_error!("failed to render tree: {}", e))?,
        ViewMode::Table => tree
            .render_table(&mut out, now)
            .map_err(|e| generic_error!("failed to render table: {}", e))?,
    }
    print!("{}", out);
    Ok(())
}

async fn run_tui(client: &mut DataPlaneAPIClient, refresh_ms: u64, initial_view: ViewMode) -> Result<(), GenericError> {
    let guard = TerminalGuard::enter().error_context("Failed to enter interactive terminal mode.")?;

    let refresh = Duration::from_millis(refresh_ms.max(50));
    let mut view = initial_view;

    loop {
        // Fetch and render, capturing any failure into the rendered output instead of bailing out: the TUI should
        // remain interactive even if the running ADP is unreachable for a moment.
        let render = match fetch_tree(client).await {
            Ok(tree) => RenderResult::Ok(tree),
            Err(e) => RenderResult::Err(format!("{:#}", e)),
        };

        if let Err(e) = paint(&render, view) {
            // If we can't paint, bail out -- the terminal is in a bad state.
            drop(guard);
            return Err(generic_error!("failed to paint TUI: {}", e));
        }

        // Wait for the next event: either user input or the refresh timer expires.
        match wait_for_input_or_tick(refresh)? {
            Some(action) => match action {
                Action::Quit => break,
                Action::ToggleView => {
                    view = match view {
                        ViewMode::Tree => ViewMode::Table,
                        ViewMode::Table => ViewMode::Tree,
                    };
                }
                Action::Refresh => {
                    // Just fall through to the next loop iteration which re-fetches.
                }
            },
            None => {
                // Timer expired with no input -- continue to refresh.
            }
        }
    }

    drop(guard);
    Ok(())
}

enum Action {
    Quit,
    ToggleView,
    Refresh,
}

enum RenderResult {
    Ok(SupervisionTree),
    Err(String),
}

fn paint(state: &RenderResult, view: ViewMode) -> io::Result<()> {
    let mut stdout = io::stdout();

    queue!(stdout, cursor::MoveTo(0, 0), Clear(ClearType::All))?;
    queue!(
        stdout,
        SetAttribute(Attribute::Bold),
        Print(format!(
            "adp top -- supervision tree -- view: {} -- press [t] to toggle, [r] to refresh, [q] to quit\r\n",
            match view {
                ViewMode::Tree => "tree",
                ViewMode::Table => "table",
            }
        )),
        SetAttribute(Attribute::Reset),
        Print("\r\n"),
    )?;

    match state {
        RenderResult::Ok(tree) => {
            let mut out = String::new();
            let now = SystemTime::now();
            let rendered = match view {
                ViewMode::Tree => tree.render_tree(&mut out, now),
                ViewMode::Table => tree.render_table(&mut out, now),
            };
            if rendered.is_err() {
                queue!(stdout, Print("(failed to render tree)\r\n"))?;
            } else {
                // Convert newlines to CRLF for raw-mode display, where bare `\n` doesn't return the cursor to column 0.
                for line in out.lines() {
                    queue!(stdout, Print(line), Print("\r\n"))?;
                }
            }
        }
        RenderResult::Err(msg) => {
            queue!(stdout, Print(format!("(error fetching tree: {})\r\n", msg)))?;
        }
    }

    stdout.flush()?;
    Ok(())
}

fn wait_for_input_or_tick(timeout: Duration) -> io::Result<Option<Action>> {
    if !poll(timeout)? {
        return Ok(None);
    }

    match read()? {
        Event::Key(KeyEvent { code, modifiers, .. }) => match (code, modifiers) {
            (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => Ok(Some(Action::Quit)),
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => Ok(Some(Action::Quit)),
            (KeyCode::Char('t'), _) => Ok(Some(Action::ToggleView)),
            (KeyCode::Char('r'), _) => Ok(Some(Action::Refresh)),
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

async fn fetch_tree(client: &mut DataPlaneAPIClient) -> Result<SupervisionTree, GenericError> {
    let body = client.supervision_tree().await?;
    let response: TreeResponse =
        serde_json::from_str(&body).error_context("Failed to deserialize supervision tree response.")?;
    Ok(SupervisionTree::from_snapshots(response.processes))
}

/// RAII guard that enters raw-mode + alternate-screen on creation and unwinds on drop.
struct TerminalGuard {
    alt_screen_entered: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;

        let mut stdout = io::stdout();
        if let Err(e) = queue!(stdout, EnterAlternateScreen, cursor::Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(e);
        }
        stdout.flush()?;

        Ok(Self {
            alt_screen_entered: true,
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        if self.alt_screen_entered {
            let _ = queue!(stdout, cursor::Show, LeaveAlternateScreen);
            let _ = stdout.flush();
        }
        let _ = terminal::disable_raw_mode();
    }
}
