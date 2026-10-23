use std::sync::Arc;

use super::repository::{ChangedFile, FileStatus};

/// Where diffs come from: the local jj repo, or a git-format patch read up front.
#[derive(Clone)]
pub enum Source {
    /// jj revset/flag args shared by every `jj diff` call; empty means the working copy.
    Jj(Vec<String>),
    Patch(Arc<Patch>),
}

impl Source {
    /// A copy that skips jj's working-copy snapshot, so parallel `jj diff` calls do not contend.
    pub fn without_snapshot(&self) -> Source {
        match self {
            Source::Jj(args) => {
                let mut args = args.clone();
                args.push("--ignore-working-copy".to_string());
                Source::Jj(args)
            }
            Source::Patch(patch) => Source::Patch(Arc::clone(patch)),
        }
    }

    pub fn is_patch(&self) -> bool {
        matches!(self, Source::Patch(_))
    }
}

/// A git-format diff split into one section per file.
pub struct Patch {
    text: String,
    sections: Vec<Section>,
}

struct Section {
    path: String,
    status: FileStatus,
    start: usize,
    end: usize,
}

impl Patch {
    pub fn parse(text: String) -> Self {
        let mut sections: Vec<Section> = Vec::new();
        let mut offset = 0;
        let mut in_hunk = false;
        for line in text.split_inclusive('\n') {
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if let Some(rest) = trimmed.strip_prefix("diff --git ") {
                if let Some(last) = sections.last_mut() {
                    last.end = offset;
                }
                let path = rest.rsplit_once(" b/").map_or(rest, |(_, new)| new);
                sections.push(Section {
                    path: path.to_string(),
                    status: FileStatus::Modified,
                    start: offset,
                    end: text.len(),
                });
                in_hunk = false;
            } else if let Some(section) = sections.last_mut() {
                if trimmed.starts_with("@@") {
                    in_hunk = true;
                } else if !in_hunk {
                    if trimmed.starts_with("new file mode") {
                        section.status = FileStatus::Added;
                    } else if trimmed.starts_with("deleted file mode") {
                        section.status = FileStatus::Deleted;
                    } else if let Some(new) = trimmed.strip_prefix("rename to ") {
                        section.status = FileStatus::Renamed;
                        section.path = new.to_string();
                    } else if let Some(new) = trimmed.strip_prefix("+++ b/") {
                        section.path = new.to_string();
                    }
                }
            }
            offset += line.len();
        }
        Patch { text, sections }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn files(&self) -> Vec<ChangedFile> {
        let mut files: Vec<ChangedFile> = self
            .sections
            .iter()
            .map(|s| ChangedFile {
                path: s.path.clone(),
                status: s.status,
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files
    }

    /// The raw diff text for one file, empty when the path is not in the patch.
    pub fn file_diff(&self, path: &str) -> &str {
        self.sections
            .iter()
            .find(|s| s.path == path)
            .map_or("", |s| &self.text[s.start..s.end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "\
diff --git a/a.rs b/a.rs
index 1..2 100644
--- a/a.rs
+++ b/a.rs
@@ -1 +1 @@
-old
+new
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hi
diff --git a/old.rs b/moved.rs
rename from old.rs
rename to moved.rs
";

    #[test]
    fn files_carry_status_and_sorted_paths() {
        let files = Patch::parse(PATCH.to_string()).files();
        let got: Vec<_> = files.iter().map(|f| (f.path.as_str(), f.status)).collect();
        assert_eq!(
            got,
            vec![
                ("a.rs", FileStatus::Modified),
                ("moved.rs", FileStatus::Renamed),
                ("new.txt", FileStatus::Added),
            ]
        );
    }

    #[test]
    fn file_diff_is_only_that_files_section() {
        let patch = Patch::parse(PATCH.to_string());
        let diff = patch.file_diff("a.rs");
        assert!(diff.starts_with("diff --git a/a.rs"));
        assert!(diff.contains("+new"));
        assert!(!diff.contains("new.txt"));
        assert_eq!(patch.file_diff("missing"), "");
    }

    #[test]
    fn hunk_line_starting_with_header_text_is_not_a_header() {
        let patch = Patch::parse(
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+++ b/fake\n".to_string(),
        );
        assert_eq!(patch.files()[0].path, "x");
    }
}
