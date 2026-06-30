mod app;
mod config;
mod fuzzy;
mod git;

use anyhow::Result;
use clap::{ArgAction, Parser};
use std::panic;

/// A terminal-based Git diff viewer with fuzzy search
#[derive(Parser)]
#[command(version, about)]
#[command(help_template = "\
{name} {version}
{about}

{usage-heading} {usage}

{all-args}

KEYBINDINGS:
    File List:
        j/Down    Move to next file
        k/Up      Move to previous file
        l/Enter   View diff of selected file
        e         Open file in editor
        c         Copy file path to clipboard
        /         Start search mode
        r         Reload
        q         Quit

    Diff View:
        j/Down    Scroll down
        k/Up      Scroll up
        d/PgDn    Scroll down 20 lines
        u/PgUp    Scroll up 20 lines
        g/Home    Go to top
        G/End     Go to bottom
        n/N       Next / previous file
        e         Open file in editor
        c         Copy file path to clipboard
        r         Reload
        h/Esc     Return to file list
        q         Quit
")]
struct Cli {
    /// Revset to diff (passed to `jj diff -r`)
    #[arg(short = 'r', long = "revisions", value_name = "REVSET")]
    revisions: Option<String>,

    /// Show changes from this revision (passed to `jj diff --from`)
    #[arg(short = 'f', long, value_name = "REV")]
    from: Option<String>,

    /// Show changes to this revision (passed to `jj diff --to`)
    #[arg(short = 't', long, value_name = "REV")]
    to: Option<String>,

    /// Ignore whitespace entirely when comparing lines (passed to `jj diff -w`)
    #[arg(short = 'w', long = "ignore-all-space")]
    ignore_all_space: bool,

    /// Ignore changes in amount of whitespace (passed to `jj diff -b`)
    #[arg(short = 'b', long = "ignore-space-change", conflicts_with = "ignore_all_space")]
    ignore_space_change: bool,

    /// Number of context lines to show (passed to `jj diff --context`)
    #[arg(long, value_name = "N")]
    context: Option<usize>,

    /// Also show help (alias for -h)
    #[arg(short = 'H', long = "Help", hide = true, action = ArgAction::Help)]
    help_alias: (),
}

impl Cli {
    /// jj selection args shared by the file-list summary and per-file diff calls.
    /// Empty (no flags) means the working copy, like a bare `jj diff`.
    fn selection(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(r) = &self.revisions {
            args.push("-r".to_string());
            args.push(r.clone());
        }
        if let Some(f) = &self.from {
            args.push("--from".to_string());
            args.push(f.clone());
        }
        if let Some(t) = &self.to {
            args.push("--to".to_string());
            args.push(t.clone());
        }
        if self.ignore_all_space {
            args.push("--ignore-all-space".to_string());
        }
        if self.ignore_space_change {
            args.push("--ignore-space-change".to_string());
        }
        if let Some(n) = self.context {
            args.push("--context".to_string());
            args.push(n.to_string());
        }
        args
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // パニックハンドラーを設定して、パニック時にターミナルを復元する
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        ratatui::restore();
        original_hook(panic_info);
    }));

    let app = app::App::new(cli.selection())?;

    let terminal = ratatui::init();
    let result = app.run(terminal);
    ratatui::restore();

    result
}
