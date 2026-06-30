use anyhow::{bail, Context, Result};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    // jj reports new files as Added, so this is never produced; kept for the UI match.
    #[allow(dead_code)]
    Untracked,
}

#[derive(Debug, Clone)]
pub struct ChangedFile {
    pub path: String,
    pub status: FileStatus,
}

/// A jj-backed source of changed files. `selection` holds the jj revset args
/// (e.g. `["--from", "x", "--to", "y"]`) shared with the per-file diff calls;
/// empty means the working copy.
pub struct Repository {
    selection: Vec<String>,
}

impl Repository {
    pub fn open(selection: Vec<String>) -> Result<Self> {
        let ok = Command::new("jj")
            .arg("root")
            .output()
            .context("Failed to run jj. Is jj installed and on PATH?")?
            .status
            .success();
        if !ok {
            bail!("Not a jj repository. Please run this command inside a jj repository.");
        }
        Ok(Self { selection })
    }

    pub fn get_changed_files(&self) -> Result<Vec<ChangedFile>> {
        let output = Command::new("jj")
            .arg("diff")
            .args(&self.selection)
            .arg("--summary")
            .output()
            .context("Failed to run jj diff --summary")?;

        let summary =
            String::from_utf8(output.stdout).context("jj diff --summary returned invalid UTF-8")?;

        let mut files: Vec<ChangedFile> = summary.lines().filter_map(parse_summary_line).collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }
}

/// Parse one `jj diff --summary` line, e.g. `M path/to/file` or `R old => new`.
fn parse_summary_line(line: &str) -> Option<ChangedFile> {
    let (marker, rest) = line.split_once(' ')?;
    let status = match marker {
        "A" => FileStatus::Added,
        "D" => FileStatus::Deleted,
        "R" => FileStatus::Renamed,
        "M" | "C" => FileStatus::Modified,
        _ => return None,
    };
    // Renames render as "old => new"; show (and later diff) the new path.
    let path = rest.rsplit(" => ").next().unwrap_or(rest).to_string();
    if path.is_empty() {
        return None;
    }
    Some(ChangedFile { path, status })
}
