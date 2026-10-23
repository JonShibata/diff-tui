use std::io::Write;
use std::process::{Command, Stdio};

use super::Source;
use crate::config::DiffConfig;

pub fn get_diff(file_path: &str, width: u16, config: &DiffConfig, source: &Source) -> Vec<u8> {
    match config.tool.as_str() {
        "auto" => {
            // Try delta first, then fall back to jj's own colored diff
            if let Ok(output) = try_tool("delta", file_path, width, &["--width"], source) {
                return output;
            }
            try_jj_color_diff(file_path, source).unwrap_or_else(|_| b"Failed to get diff".to_vec())
        }
        "jj" | "git" => {
            try_jj_color_diff(file_path, source).unwrap_or_else(|_| b"Failed to get diff".to_vec())
        }
        tool => {
            // Try the specified tool
            if let Ok(output) = try_tool(tool, file_path, width, &config.args, source) {
                return output;
            }
            // Fallback to jj's own colored diff
            try_jj_color_diff(file_path, source).unwrap_or_else(|_| b"Failed to get diff".to_vec())
        }
    }
}

/// For each hunk, the new-file line number that each body line (context,
/// addition, or deletion, in raw order) maps to. Lets the editor jump to
/// whatever line is at the top of the diff view, not just the hunk's first
/// changed line: context and addition lines get their own new-file line
/// number, and a deletion gets the line number of whatever now occupies its
/// position (the following context/addition line, or one past the hunk's end).
pub fn hunk_line_targets(file_path: &str, source: &Source) -> Vec<Vec<u32>> {
    let raw = get_jj_diff_output(file_path, source).unwrap_or_default();
    parse_hunk_line_targets(&String::from_utf8_lossy(&raw))
}

/// Parse per-hunk line targets from raw git-format diff text. Split out from
/// the shell-out above so it can be unit-tested directly.
fn parse_hunk_line_targets(text: &str) -> Vec<Vec<u32>> {
    let mut hunks = Vec::new();
    let mut current = Vec::new();
    let mut new_line: u32 = 0;
    let mut in_hunk = false;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            if in_hunk {
                hunks.push(std::mem::take(&mut current));
            }
            // "@@ -a,b +c,d @@ ..." -> the digits right after '+' are the new start.
            if let Some(c) = rest.split('+').nth(1).and_then(|plus| {
                let digits: String = plus.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse::<u32>().ok()
            }) {
                new_line = c;
                in_hunk = true;
            } else {
                in_hunk = false;
            }
            continue;
        }
        if !in_hunk {
            continue;
        }
        match line.chars().next() {
            // Present in the new file, so this line owns the current counter
            // value and advances it.
            Some('+') | Some(' ') => {
                current.push(new_line);
                new_line += 1;
            }
            // Removed: not in the new file, so it takes the position that
            // whatever follows will occupy, without advancing the counter.
            Some('-') => current.push(new_line),
            // "\ No newline at end of file" still renders as its own row, so
            // give it a target too, keeping row alignment with the rendered diff.
            Some('\\') => current.push(new_line),
            _ => {}
        }
    }
    if in_hunk {
        hunks.push(current);
    }

    hunks
}

fn try_tool(
    tool_name: &str,
    file_path: &str,
    width: u16,
    extra_args: &[impl AsRef<str>],
    source: &Source,
) -> Result<Vec<u8>, ()> {
    // Check if the tool is available
    if which::which(tool_name).is_err() {
        return Err(());
    }

    // Get the (uncolored) git-format diff first
    let diff_input = get_jj_diff_output(file_path, source)?;

    if diff_input.is_empty() {
        return Err(());
    }

    // Build command with arguments
    let mut cmd = Command::new(tool_name);

    // Add width argument for delta. `--max-line-length 0` disables delta's
    // default 512-char truncation so the full line reaches the TUI, which then
    // soft-wraps it; without it, long lines are cut off with a `→` marker before
    // we ever see them. Delta can be slow on very long (e.g. minified) lines
    // with truncation off, an acceptable trade for not losing content.
    if tool_name == "delta" {
        cmd.args(["--width", &width.to_string(), "--max-line-length", "0"]);
    }

    // Add extra arguments from config
    for arg in extra_args {
        let arg_str = arg.as_ref();
        // Skip --width for delta as we already added it
        if tool_name == "delta" && arg_str == "--width" {
            continue;
        }
        cmd.arg(arg_str);
    }

    // Pipe diff through the tool. Feed stdin from a separate thread while the
    // main thread drains stdout, or a large diff deadlocks: once the tool's
    // output pipe fills (~64KB) it stops reading, and a single-threaded
    // write_all of the whole diff then blocks forever on a full stdin pipe.
    let mut process = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| ())?;

    let mut stdin = process.stdin.take().ok_or(())?;
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&diff_input);
        // `stdin` drops here, closing the pipe so the tool sees EOF.
    });

    let output = process.wait_with_output().map_err(|_| ())?;
    let _ = writer.join();
    Ok(output.stdout)
}

/// jj parses path arguments after `--` as *fileset expressions*, not literal
/// paths: characters like `$`, spaces, and glob metacharacters have syntactic
/// meaning, so a path such as `.../$id/index.tsx` is a fileset parse error and
/// jj prints nothing to stdout — the file shows in the list (from `--summary`,
/// which takes no path) but the diff view comes up empty. Wrap the literal path
/// in a double-quoted fileset string so every character is taken verbatim,
/// escaping `\` and `"` for the string literal itself. A bare quoted string is a
/// cwd-relative prefix pattern, matching the semantics the bare path already had.
fn fileset_literal(file_path: &str) -> String {
    let escaped = file_path.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Uncolored git-format diff for one file (jj disables color when piped).
fn get_jj_diff_output(file_path: &str, source: &Source) -> Result<Vec<u8>, ()> {
    let selection = match source {
        Source::Jj(args) => args,
        Source::Patch(patch) => return Ok(patch.file_diff(file_path).as_bytes().to_vec()),
    };
    let output = Command::new("jj")
        .arg("diff")
        .args(selection)
        .arg("--git")
        .arg("--")
        .arg(fileset_literal(file_path))
        .output()
        .map_err(|_| ())?;

    Ok(output.stdout)
}

/// Fallback when no external diff tool is available: jj's own colored git-format diff.
fn try_jj_color_diff(file_path: &str, source: &Source) -> Result<Vec<u8>, ()> {
    let selection = match source {
        Source::Jj(args) => args,
        Source::Patch(patch) => return Ok(patch.file_diff(file_path).as_bytes().to_vec()),
    };
    let output = Command::new("jj")
        .args(["--color=always", "diff"])
        .args(selection)
        .arg("--git")
        .arg("--")
        .arg(fileset_literal(file_path))
        .output()
        .map_err(|_| ())?;

    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::{fileset_literal, parse_hunk_line_targets};

    #[test]
    fn fileset_literal_quotes_dollar_paths() {
        // `$` is a fileset metacharacter; quoting makes it literal.
        assert_eq!(
            fileset_literal("web/routes/$id/index.tsx"),
            "\"web/routes/$id/index.tsx\""
        );
    }

    #[test]
    fn fileset_literal_escapes_backslash_and_quote() {
        // Escape for the string literal itself so exotic names still round-trip.
        assert_eq!(fileset_literal(r#"a\b"c"#), r#""a\\b\"c""#);
    }

    #[test]
    fn context_and_addition_lines_get_consecutive_targets() {
        let diff = "\
@@ -10,6 +10,7 @@ fn foo() {
 ctx a
 ctx b
 ctx c
+added line
 ctx d
";
        assert_eq!(parse_hunk_line_targets(diff), vec![vec![10, 11, 12, 13, 14]]);
    }

    #[test]
    fn deletion_takes_the_position_of_whatever_follows() {
        // The deletion doesn't advance the counter, so it shares the next
        // context line's new-file number (12 = 10 + 2 context lines).
        let diff = "\
@@ -10,4 +10,3 @@
 ctx a
 ctx b
-removed line
 ctx c
";
        assert_eq!(parse_hunk_line_targets(diff), vec![vec![10, 11, 12, 12]]);
    }

    #[test]
    fn change_on_first_line_has_no_context() {
        let diff = "\
@@ -1,3 +1,4 @@
+brand new first line
 ctx a
 ctx b
";
        assert_eq!(parse_hunk_line_targets(diff), vec![vec![1, 2, 3]]);
    }

    #[test]
    fn one_vector_per_hunk_in_order() {
        let diff = "\
@@ -10,5 +10,6 @@
 ctx a
 ctx b
+add in hunk one
 ctx c
@@ -40,4 +41,5 @@
 ctx d
+add in hunk two
 ctx e
";
        assert_eq!(
            parse_hunk_line_targets(diff),
            vec![vec![10, 11, 12, 13], vec![41, 42, 43]]
        );
    }

    #[test]
    fn no_newline_marker_still_gets_a_target() {
        // Keeps row alignment with the rendered diff, which shows this as its own row.
        let diff = "\
@@ -1,2 +1,2 @@
 ctx a
-old last
\\ No newline at end of file
+new last
\\ No newline at end of file
";
        assert_eq!(parse_hunk_line_targets(diff), vec![vec![1, 2, 2, 2, 3]]);
    }

    #[test]
    fn empty_input_yields_no_hunks() {
        assert_eq!(parse_hunk_line_targets(""), Vec::<Vec<u32>>::new());
    }
}
