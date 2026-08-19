use ansi_to_tui::IntoText as _;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal;
use ratatui::{
    layout::{Constraint, Layout, Position},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    DefaultTerminal, Frame,
};

use crate::config::Config;
use crate::fuzzy::FuzzyMatcher;
use crate::git::{ChangedFile, FileStatus, Repository};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    FileList,
    DiffView,
}

pub struct App {
    pub running: bool,
    pub screen: Screen,
    pub files: Vec<ChangedFile>,
    pub file_paths: Vec<String>,
    pub filtered_indices: Vec<usize>,
    pub list_state: ListState,
    pub search_mode: bool,
    pub search_query: String,
    pub fuzzy_matcher: FuzzyMatcher,
    pub diff_content: Vec<u8>,
    pub diff_lines: Vec<Line<'static>>,
    pub diff_scroll: u16,
    pub wrap: bool,
    pub selected_file: Option<String>,
    pub config: Config,
    pub needs_redraw: bool,
    repository: Repository,
    selection: Vec<String>,
    /// (rendered row index of a hunk start, hunk's new-file line) for the open diff.
    hunk_markers: Vec<(usize, u32)>,
    /// Transient status line (e.g. "Copied: …"), cleared on the next keypress.
    status_message: Option<String>,
}

impl App {
    pub fn new(selection: Vec<String>) -> Result<Self> {
        let config = Config::load();
        let repository = Repository::open(selection.clone())?;
        let files = repository.get_changed_files()?;
        let file_paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        let filtered_indices: Vec<usize> = (0..files.len()).collect();

        let mut list_state = ListState::default();
        if !files.is_empty() {
            list_state.select(Some(0));
        }

        Ok(Self {
            running: true,
            screen: Screen::FileList,
            files,
            file_paths,
            filtered_indices,
            list_state,
            search_mode: false,
            search_query: String::new(),
            fuzzy_matcher: FuzzyMatcher::new(),
            diff_content: Vec::new(),
            diff_lines: Vec::new(),
            diff_scroll: 0,
            // Wrap on by default so long lines are shown in full; `w` toggles it
            // off for one-row-per-line. Delta already preserves the whole line
            // (see `--max-line-length 0`), so nothing is truncated either way
            // except by the screen edge when wrap is off.
            wrap: true,
            selected_file: None,
            config,
            needs_redraw: false,
            repository,
            selection,
            hunk_markers: Vec::new(),
            status_message: None,
        })
    }

    pub fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        while self.running {
            if self.needs_redraw {
                terminal.clear()?;
                self.needs_redraw = false;
            }
            terminal.draw(|frame| self.draw(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        match self.screen {
            Screen::FileList => self.draw_file_list(frame),
            Screen::DiffView => self.draw_diff_view(frame),
        }
    }

    fn draw_file_list(&mut self, frame: &mut Frame) {
        let area = frame.area();

        let constraints = if self.search_mode {
            vec![
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(1),
            ]
        } else {
            vec![Constraint::Min(1), Constraint::Length(1)]
        };

        let chunks = Layout::vertical(constraints).split(area);

        let (list_area, help_area) = if self.search_mode {
            // Draw search input
            let search_block = Block::default().title(" Search ").borders(Borders::ALL);
            let search_input = Paragraph::new(self.search_query.as_str()).block(search_block);
            frame.render_widget(search_input, chunks[0]);

            // Set cursor position
            frame.set_cursor_position(Position::new(
                chunks[0].x + self.search_query.len() as u16 + 1,
                chunks[0].y + 1,
            ));

            (chunks[1], chunks[2])
        } else {
            (chunks[0], chunks[1])
        };

        // Build list items from filtered indices
        let items: Vec<ListItem> = self
            .filtered_indices
            .iter()
            .filter_map(|&idx| self.files.get(idx))
            .map(|file| {
                let status_char = match file.status {
                    FileStatus::Modified => ("M", Color::Yellow),
                    FileStatus::Added => ("A", Color::Green),
                    FileStatus::Deleted => ("D", Color::Red),
                    FileStatus::Renamed => ("R", Color::Cyan),
                    FileStatus::Untracked => ("?", Color::Gray),
                };
                let line = Line::from(vec![
                    Span::styled(
                        format!("{} ", status_char.0),
                        Style::default().fg(status_char.1),
                    ),
                    Span::raw(&file.path),
                ]);
                ListItem::new(line)
            })
            .collect();

        let title = format!(
            " Changed Files ({}/{}) ",
            self.filtered_indices.len(),
            self.files.len()
        );
        let list = List::new(items)
            .block(Block::default().title(title).borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::REVERSED)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, list_area, &mut self.list_state);

        let help_text = if self.search_mode {
            " Type to search | Enter: select | Esc: cancel "
        } else {
            " j/k: move | l/Enter: view diff | e: edit | c: copy path | /: search | r: reload | q: quit"
        };
        let (help_str, help_color) = match &self.status_message {
            Some(msg) => (msg.clone(), Color::Green),
            None => (help_text.to_string(), Color::DarkGray),
        };
        let help = Paragraph::new(help_str).style(Style::default().fg(help_color));
        frame.render_widget(help, help_area);
    }

    fn draw_diff_view(&mut self, frame: &mut Frame) {
        let area = frame.area();

        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

        let title = format!(" {} ", self.selected_file.as_deref().unwrap_or("Diff"));

        let visible_height = chunks[0].height.saturating_sub(2) as usize;
        let visible_lines: Vec<Line> = self
            .diff_lines
            .iter()
            .skip(self.diff_scroll as usize)
            .take(visible_height)
            .cloned()
            .collect();

        let mut diff =
            Paragraph::new(visible_lines).block(Block::default().title(title).borders(Borders::ALL));
        if self.wrap {
            // `trim: false` keeps leading whitespace so wrapped code stays aligned.
            diff = diff.wrap(Wrap { trim: false });
        }

        frame.render_widget(diff, chunks[0]);

        let total_lines = self.diff_lines.len();
        let current_line = self.diff_scroll as usize + 1;
        let (help_str, help_color) = match &self.status_message {
            Some(msg) => (msg.clone(), Color::Green),
            None => (
                format!(
                    " j/k: scroll | n/N: next/prev file | e: edit | c: copy path | w: wrap ({}) | r: reload | h/Esc: back | q: quit | Line {}/{} ",
                    if self.wrap { "on" } else { "off" },
                    current_line.min(total_lines),
                    total_lines
                ),
                Color::DarkGray,
            ),
        };
        let help = Paragraph::new(help_str).style(Style::default().fg(help_color));
        frame.render_widget(help, chunks[1]);
    }

    fn handle_events(&mut self) -> Result<()> {
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                return Ok(());
            }

            // Clear any transient status (e.g. "Copied: …") on the next keypress.
            self.status_message = None;
            match self.screen {
                Screen::FileList => self.handle_file_list_keys(key.code),
                Screen::DiffView => self.handle_diff_view_keys(key.code),
            }
        }
        Ok(())
    }

    fn handle_file_list_keys(&mut self, code: KeyCode) {
        if self.search_mode {
            match code {
                KeyCode::Esc => {
                    self.search_mode = false;
                    self.search_query.clear();
                    self.update_filter();
                }
                KeyCode::Enter => {
                    self.search_mode = false;
                    if !self.filtered_indices.is_empty() {
                        self.open_diff();
                    }
                }
                KeyCode::Backspace => {
                    self.search_query.pop();
                    self.update_filter();
                }
                KeyCode::Char(c) => {
                    self.search_query.push(c);
                    self.update_filter();
                }
                KeyCode::Down => self.select_next(),
                KeyCode::Up => self.select_previous(),
                _ => {}
            }
        } else {
            match code {
                KeyCode::Char('q') => self.running = false,
                KeyCode::Char('j') | KeyCode::Char('n') | KeyCode::Down => self.select_next(),
                KeyCode::Char('k') | KeyCode::Char('N') | KeyCode::Up => self.select_previous(),
                KeyCode::Char('/') => {
                    self.search_mode = true;
                }
                KeyCode::Char('e') => self.open_selected_in_editor(),
                KeyCode::Char('c') => self.copy_current_path(),
                KeyCode::Char('r') => self.refresh(),
                KeyCode::Enter | KeyCode::Char('l') => self.open_diff(),
                _ => {}
            }
        }
    }

    fn handle_diff_view_keys(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') => {
                self.running = false;
            }
            KeyCode::Esc | KeyCode::Char('h') => {
                self.screen = Screen::FileList;
                self.diff_scroll = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let max_scroll = self.diff_lines.len().saturating_sub(1);
                self.diff_scroll = (self.diff_scroll + 1).min(max_scroll as u16);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.diff_scroll = self.diff_scroll.saturating_sub(1);
            }
            KeyCode::Char('d') | KeyCode::PageDown => {
                let max_scroll = self.diff_lines.len().saturating_sub(1);
                self.diff_scroll = (self.diff_scroll + 20).min(max_scroll as u16);
            }
            KeyCode::Char('u') | KeyCode::PageUp => {
                self.diff_scroll = self.diff_scroll.saturating_sub(20);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.diff_scroll = 0;
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.diff_scroll = self.diff_lines.len().saturating_sub(1) as u16;
            }
            KeyCode::Char('e') => {
                self.open_in_editor();
            }
            KeyCode::Char('r') => {
                self.refresh();
            }
            KeyCode::Char('c') => self.copy_current_path(),
            KeyCode::Char('w') => self.wrap = !self.wrap,
            KeyCode::Char('n') => self.show_adjacent_file(true),
            KeyCode::Char('N') => self.show_adjacent_file(false),
            _ => {}
        }
    }

    /// From the diff view, move to the next/previous file in the list and load
    /// its diff in place (staying in the diff view). At a boundary (last file
    /// with `n`, first file with `N`) the selection can't advance, so return to
    /// the file list with that file still highlighted instead of reloading it.
    fn show_adjacent_file(&mut self, forward: bool) {
        let current = self.list_state.selected();
        if forward {
            self.select_next();
        } else {
            self.select_previous();
        }
        if self.list_state.selected() == current {
            self.screen = Screen::FileList;
            return;
        }
        self.load_diff_for_selected();
    }

    /// Copy the path of the highlighted file (file list) or the open file
    /// (diff view) to the system clipboard, with a brief status confirmation.
    fn copy_current_path(&mut self) {
        let path = match self.screen {
            Screen::DiffView => self.selected_file.clone(),
            Screen::FileList => self
                .list_state
                .selected()
                .and_then(|i| self.filtered_indices.get(i).copied())
                .and_then(|idx| self.files.get(idx))
                .map(|f| f.path.clone()),
        };
        let Some(path) = path else {
            return;
        };
        self.status_message = Some(if copy_to_clipboard(&path) {
            format!("Copied: {path}")
        } else {
            "Copy failed: no wl-copy/xclip/xsel found".to_string()
        });
    }

    fn select_next(&mut self) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => (i + 1).min(self.filtered_indices.len() - 1),
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn select_previous(&mut self) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn update_filter(&mut self) {
        self.filtered_indices = self
            .fuzzy_matcher
            .filter(&self.file_paths, &self.search_query);
        // Reset selection to first item if there are results
        if !self.filtered_indices.is_empty() {
            self.list_state.select(Some(0));
        } else {
            self.list_state.select(None);
        }
    }

    fn open_diff(&mut self) {
        if self.load_diff_for_selected() {
            self.screen = Screen::DiffView;
        }
    }

    /// Load the diff for the currently-selected list item into `diff_lines` and
    /// hunk markers, resetting scroll. Does not change the active screen, so it
    /// serves both opening a diff and switching files within the diff view.
    /// Returns true if a file's diff was loaded.
    fn load_diff_for_selected(&mut self) -> bool {
        if let Some(list_idx) = self.list_state.selected() {
            if let Some(&file_idx) = self.filtered_indices.get(list_idx) {
                if let Some(file) = self.files.get(file_idx) {
                    self.selected_file = Some(file.path.clone());
                    // Get terminal width (subtract 2 for border)
                    let width = terminal::size()
                        .map(|(w, _)| w.saturating_sub(2))
                        .unwrap_or(80);
                    self.diff_content =
                        crate::git::get_diff(&file.path, width, &self.config.diff, &self.selection);

                    // Parse ANSI escape sequences into styled lines
                    self.diff_lines = match self.diff_content.as_slice().into_text() {
                        Ok(text) => text
                            .lines
                            .into_iter()
                            .map(|line| {
                                Line::from(
                                    line.spans
                                        .into_iter()
                                        .map(|span| {
                                            Span::styled(span.content.to_string(), span.style)
                                        })
                                        .collect::<Vec<_>>(),
                                )
                            })
                            .collect(),
                        Err(_) => {
                            // Fallback: plain text without ANSI parsing
                            String::from_utf8_lossy(&self.diff_content)
                                .lines()
                                .map(|s| Line::raw(s.to_string()))
                                .collect()
                        }
                    };

                    // Pair each rendered hunk-separator rule row with the new-file
                    // line of the hunk's first actual change (from the raw diff), in
                    // order. Used by `e` to open the editor at the change in the hunk
                    // currently in view.
                    let starts = crate::git::hunk_first_change_lines(&file.path, &self.selection);
                    self.hunk_markers = self
                        .diff_lines
                        .iter()
                        .enumerate()
                        .filter(|(_, line)| is_rule_row(line))
                        .map(|(i, _)| i)
                        .zip(starts)
                        .map(|(row, line_no)| (row.saturating_sub(1), line_no))
                        .collect();

                    self.diff_scroll = 0;
                    return true;
                }
            }
        }
        false
    }

    /// Re-derive the file list and current diff from jj (after an edit or `r`).
    fn refresh(&mut self) {
        let prev_path = self.selected_file.clone();
        let prev_scroll = self.diff_scroll;

        if let Ok(files) = self.repository.get_changed_files() {
            self.file_paths = files.iter().map(|f| f.path.clone()).collect();
            self.files = files;
            self.update_filter();
        }

        // Restore the list selection to the same path if it still exists.
        if let Some(path) = &prev_path {
            if let Some(pos) = self.filtered_indices.iter().position(|&idx| {
                self.files.get(idx).map(|f| f.path.as_str()) == Some(path.as_str())
            }) {
                self.list_state.select(Some(pos));
            }
        }

        // If a diff is open, regenerate it; drop back to the list if the file is gone.
        if self.screen == Screen::DiffView {
            let still_present = prev_path
                .as_ref()
                .map(|p| self.files.iter().any(|f| &f.path == p))
                .unwrap_or(false);
            if still_present {
                // load_diff_for_selected resets scroll to 0; restore the prior
                // position (clamped) so a reload keeps the user where they were.
                self.load_diff_for_selected();
                let max_scroll = self.diff_lines.len().saturating_sub(1) as u16;
                self.diff_scroll = prev_scroll.min(max_scroll);
            } else {
                self.screen = Screen::FileList;
                self.diff_scroll = 0;
            }
        }

        self.needs_redraw = true;
    }

    fn open_selected_in_editor(&mut self) {
        if let Some(list_idx) = self.list_state.selected() {
            if let Some(&file_idx) = self.filtered_indices.get(list_idx) {
                if let Some(file) = self.files.get(file_idx) {
                    self.selected_file = Some(file.path.clone());
                    self.open_in_editor();
                }
            }
        }
    }

    /// File line to open the editor at: the first changed line of the hunk at the
    /// top of the diff view. None when not viewing a diff or no hunks were detected.
    fn current_hunk_line(&self) -> Option<u32> {
        if self.screen != Screen::DiffView || self.hunk_markers.is_empty() {
            return None;
        }
        let scroll = self.diff_scroll as usize;
        let mut line = self.hunk_markers[0].1; // default to the first hunk
        for &(row, line_no) in &self.hunk_markers {
            if row <= scroll {
                line = line_no;
            } else {
                break;
            }
        }
        Some(line)
    }

    fn open_in_editor(&mut self) {
        let line = self.current_hunk_line();
        let Some(file_path) = self.selected_file.clone() else {
            return;
        };
        let command = self.config.editor.get_command();
        let editor_args = self.config.editor.args.clone();

        // Temporarily exit TUI mode
        let _ = terminal::disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), terminal::LeaveAlternateScreen);

        // Build and run the editor command. `+N` opens at a line for hx/vim/
        // nano/emacs; harmless to omit when there's no hunk line.
        let mut cmd = std::process::Command::new(&command);
        cmd.args(&editor_args);
        if let Some(n) = line {
            cmd.arg(format!("+{n}"));
        }
        cmd.arg(&file_path);
        let _ = cmd.status();

        // Restore TUI mode
        let _ = terminal::enable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            terminal::EnterAlternateScreen,
            terminal::Clear(terminal::ClearType::All)
        );

        // Re-derive list + diff so edits are reflected immediately.
        self.refresh();
    }
}

/// True if a rendered row is a delta hunk-separator rule (a run of `─`),
/// used to locate hunk boundaries in the rendered diff.
fn is_rule_row(line: &Line) -> bool {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    text.chars().filter(|&c| c == '─').count() >= 10
        && text.chars().all(|c| c == '─' || c.is_whitespace())
}

/// Copy text to the system clipboard via the first available CLI tool.
/// Mirrors the codebase's shell-out style (jj/delta/$EDITOR); no extra crate.
fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    // Candidates cover Wayland (wl-copy) and X11 (xclip/xsel).
    let candidates: [(&str, &[&str]); 3] = [
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (cmd, args) in candidates {
        if which::which(cmd).is_err() {
            continue;
        }
        let Ok(mut child) = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        if child.wait().map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    false
}
