use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::process::Command;

use super::Source;

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

/// Lines added and removed in one file, as `jj diff --stat` counts them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LineStat {
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
}

#[derive(Debug, Clone)]
pub struct ChangedFile {
    pub path: String,
    pub status: FileStatus,
}

/// A source of changed files: jj revset args (e.g. `["--from", "x", "--to", "y"]`)
/// shared with the per-file diff calls (empty means the working copy), or a patch.
pub struct Repository {
    source: Source,
}

impl Repository {
    pub fn open(source: Source) -> Result<Self> {
        if source.is_patch() {
            return Ok(Self { source });
        }
        let ok = Command::new("jj")
            .arg("root")
            .output()
            .context("Failed to run jj. Is jj installed and on PATH?")?
            .status
            .success();
        if !ok {
            bail!("Not a jj repository. Please run this command inside a jj repository.");
        }
        Ok(Self { source })
    }

    pub fn get_changed_files(&self) -> Result<Vec<ChangedFile>> {
        let selection = match &self.source {
            Source::Jj(args) => args,
            Source::Patch(patch) => return Ok(patch.files()),
        };
        let output = Command::new("jj")
            .arg("diff")
            .args(selection)
            .arg("--summary")
            .output()
            .context("Failed to run jj diff --summary")?;

        let summary =
            String::from_utf8(output.stdout).context("jj diff --summary returned invalid UTF-8")?;

        let mut files: Vec<ChangedFile> = summary.lines().filter_map(parse_summary_line).collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    /// Per-file line counts for the whole selection, keyed by the path the file list shows.
    pub fn get_line_stats(&self) -> Result<HashMap<String, LineStat>> {
        let selection = match &self.source {
            Source::Jj(args) => args,
            Source::Patch(patch) => return Ok(parse_git_stats(patch.text())),
        };
        let output = Command::new("jj")
            .args(["--color=never", "diff"])
            .args(selection)
            .arg("--git")
            .output()
            .context("Failed to run jj diff --git")?;
        Ok(parse_git_stats(&String::from_utf8_lossy(&output.stdout)))
    }
}

/// Count hunk `+`/`-` lines per file in git-format diff text.
fn parse_git_stats(text: &str) -> HashMap<String, LineStat> {
    let mut stats = HashMap::new();
    let mut current: Option<(String, LineStat)> = None;
    let mut in_hunk = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            stats.extend(current.take());
            let path = rest.rsplit_once(" b/").map_or(rest, |(_, new)| new);
            current = Some((path.to_string(), LineStat::default()));
            in_hunk = false;
            continue;
        }
        let Some((path, stat)) = current.as_mut() else {
            continue;
        };
        if line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk {
            match line.as_bytes().first() {
                Some(b'+') => stat.added += 1,
                Some(b'-') => stat.removed += 1,
                _ => {}
            }
        } else if let Some(new) = line.strip_prefix("rename to ") {
            *path = new.to_string();
        } else if let Some(new) = line.strip_prefix("+++ b/") {
            *path = new.to_string();
        } else if line.starts_with("Binary files ") {
            stat.binary = true;
        }
    }
    stats.extend(current);
    stats
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

#[cfg(test)]
mod tests {
    use super::{parse_git_stats, LineStat};

    fn stat(added: usize, removed: usize) -> LineStat {
        LineStat {
            added,
            removed,
            binary: false,
        }
    }

    #[test]
    fn counts_hunk_lines_per_file() {
        let text = "\
diff --git a/a.rs b/a.rs
index 1..2 100644
--- a/a.rs
+++ b/a.rs
@@ -1,3 +1,3 @@
 keep
-old
+new
+++ added line that looks like a header
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-x
-y
";
        let stats = parse_git_stats(text);
        assert_eq!(stats["a.rs"], stat(2, 1));
        assert_eq!(stats["gone.txt"], stat(0, 2));
    }

    #[test]
    fn rename_is_keyed_by_new_path() {
        let text = "\
diff --git a/old.rs b/new.rs
rename from old.rs
rename to new.rs
--- a/old.rs
+++ b/new.rs
@@ -1 +1 @@
-a
+b
";
        assert_eq!(parse_git_stats(text)["new.rs"], stat(1, 1));
    }

    #[test]
    fn binary_file_is_flagged() {
        let text = "\
diff --git a/img.png b/img.png
index 1..2 100644
Binary files a/img.png and b/img.png differ
";
        assert!(parse_git_stats(text)["img.png"].binary);
    }

    #[test]
    fn path_with_spaces_comes_from_the_header() {
        let text = "\
diff --git a/my dir/b/x.txt b/my dir/b/x.txt
new file mode 100644
--- /dev/null
+++ b/my dir/b/x.txt
@@ -0,0 +1 @@
+hi
";
        assert_eq!(parse_git_stats(text)["my dir/b/x.txt"], stat(1, 0));
    }
}
