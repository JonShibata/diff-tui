use std::io::Write;
use std::process::{Command, Stdio};

use crate::config::DiffConfig;

pub fn get_diff(file_path: &str, width: u16, config: &DiffConfig, selection: &[String]) -> Vec<u8> {
    match config.tool.as_str() {
        "auto" => {
            // Try delta first, then fall back to jj's own colored diff
            if let Ok(output) = try_tool("delta", file_path, width, &["--width"], selection) {
                return output;
            }
            try_jj_color_diff(file_path, selection).unwrap_or_else(|_| b"Failed to get diff".to_vec())
        }
        "jj" | "git" => {
            try_jj_color_diff(file_path, selection).unwrap_or_else(|_| b"Failed to get diff".to_vec())
        }
        tool => {
            // Try the specified tool
            if let Ok(output) = try_tool(tool, file_path, width, &config.args, selection) {
                return output;
            }
            // Fallback to jj's own colored diff
            try_jj_color_diff(file_path, selection).unwrap_or_else(|_| b"Failed to get diff".to_vec())
        }
    }
}

/// New-file line of the first *changed* line in each hunk, in order. The `@@ -a,b
/// +c,d @@` header's `c` points at the hunk's first line, which is usually leading
/// context; here we walk the hunk body past that context to the first added (`+`)
/// or removed (`-`) line so the editor jumps to the actual change, not the context.
pub fn hunk_first_change_lines(file_path: &str, selection: &[String]) -> Vec<u32> {
    let raw = get_jj_diff_output(file_path, selection).unwrap_or_default();
    first_change_lines(&String::from_utf8_lossy(&raw))
}

/// Parse the first-changed new-file line of each hunk from raw git-format diff text.
/// Split out from the shell-out above so it can be unit-tested directly.
fn first_change_lines(text: &str) -> Vec<u32> {
    let mut result = Vec::new();
    let mut new_line: u32 = 0;
    let mut in_hunk = false;
    let mut found = false;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            // "@@ -a,b +c,d @@ ..." -> the digits right after '+' are the new start.
            if let Some(c) = rest.split('+').nth(1).and_then(|plus| {
                let digits: String = plus.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse::<u32>().ok()
            }) {
                new_line = c;
                in_hunk = true;
                found = false;
            }
            continue;
        }
        if !in_hunk || found {
            continue;
        }
        match line.chars().next() {
            // First change in the hunk: an addition sits at `new_line`; a deletion
            // jumps to the new-file line that now occupies that position.
            Some('+') | Some('-') => {
                result.push(new_line);
                found = true;
            }
            // Context line: present in the new file, so advance the counter.
            Some(' ') => new_line += 1,
            // "\ No newline at end of file" — not a real line; ignore.
            _ => {}
        }
    }

    result
}

fn try_tool(
    tool_name: &str,
    file_path: &str,
    width: u16,
    extra_args: &[impl AsRef<str>],
    selection: &[String],
) -> Result<Vec<u8>, ()> {
    // Check if the tool is available
    if which::which(tool_name).is_err() {
        return Err(());
    }

    // Get the (uncolored) git-format diff first
    let diff_input = get_jj_diff_output(file_path, selection)?;

    if diff_input.is_empty() {
        return Err(());
    }

    // Build command with arguments
    let mut cmd = Command::new(tool_name);

    // Add width argument for delta
    if tool_name == "delta" {
        cmd.args(["--width", &width.to_string()]);
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
fn get_jj_diff_output(file_path: &str, selection: &[String]) -> Result<Vec<u8>, ()> {
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
fn try_jj_color_diff(file_path: &str, selection: &[String]) -> Result<Vec<u8>, ()> {
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
    use super::{first_change_lines, fileset_literal};

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
    fn skips_leading_context_to_first_addition() {
        // Header says new start is line 10, but the change is 3 context lines in.
        let diff = "\
@@ -10,6 +10,7 @@ fn foo() {
 ctx a
 ctx b
 ctx c
+added line
 ctx d
";
        assert_eq!(first_change_lines(diff), vec![13]);
    }

    #[test]
    fn leading_context_then_deletion() {
        // Two context lines, then a deletion: jumps to the new-file line now at
        // that position (12 = 10 + 2 context lines).
        let diff = "\
@@ -10,4 +10,3 @@
 ctx a
 ctx b
-removed line
 ctx c
";
        assert_eq!(first_change_lines(diff), vec![12]);
    }

    #[test]
    fn change_on_first_line_has_no_context() {
        let diff = "\
@@ -1,3 +1,4 @@
+brand new first line
 ctx a
 ctx b
";
        assert_eq!(first_change_lines(diff), vec![1]);
    }

    #[test]
    fn one_entry_per_hunk_in_order() {
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
        assert_eq!(first_change_lines(diff), vec![12, 42]);
    }

    #[test]
    fn ignores_no_newline_marker() {
        let diff = "\
@@ -1,2 +1,2 @@
 ctx a
-old last
\\ No newline at end of file
+new last
\\ No newline at end of file
";
        // First change is the deletion at new-file line 2 (after one context line).
        assert_eq!(first_change_lines(diff), vec![2]);
    }

    #[test]
    fn empty_input_yields_no_hunks() {
        assert_eq!(first_change_lines(""), Vec::<u32>::new());
    }
}
