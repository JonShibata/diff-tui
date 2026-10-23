use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use ansi_to_tui::IntoText as _;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal;
use ratatui::{
    layout::{Constraint, Layout, Position},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
    DefaultTerminal, Frame,
};
use regex::{Regex, RegexBuilder};

use crate::config::Config;
use crate::fuzzy::FuzzyMatcher;
use crate::git::{ChangedFile, FileStatus, LineStat, Repository, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    FileList,
    DiffView,
}

/// A regex search over the rendered diffs of every file in the list.
struct ContentSearch {
    regex: Regex,
    /// (list position, rendered row) of every matching row, in list then row order.
    matches: Vec<(usize, usize)>,
    /// Index into `matches` of the match last jumped to.
    current: Option<usize>,
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
    pub diff_lines: Vec<Line<'static>>,
    pub diff_scroll: u16,
    pub wrap: bool,
    pub selected_file: Option<String>,
    pub config: Config,
    pub needs_redraw: bool,
    repository: Repository,
    source: Source,
    /// New-file line number for each rendered row of the open diff, `None` for
    /// rows that aren't a hunk body line (file/hunk headers, separator rules).
    line_numbers: Vec<Option<u32>>,
    /// Transient status line (e.g. "Copied: …"), cleared on the next keypress.
    status_message: Option<String>,
    /// Text typed into the `/` prompt; `Some` while the prompt is open.
    search_prompt: Option<String>,
    /// Regex submitted from the prompt, run after the next draw so "Searching…" shows first.
    pending_search: Option<Regex>,
    content_search: Option<ContentSearch>,
    /// Rendered diffs by path, filled by a search so jumps show exactly what was searched.
    diff_cache: HashMap<String, Vec<Line<'static>>>,
    /// Whether the file list shows per-file line counts (`s` toggles).
    show_stats: bool,
    /// Line counts by path, loaded on first `s` and dropped on reload.
    line_stats: Option<HashMap<String, LineStat>>,
}

/// Widest the `+`/`-` bar in the file list grows, matching jj's stat graph scale.
const STAT_BAR_WIDTH: usize = 40;
/// Columns before a file list path: two borders, the `> ` highlight, and the status letter.
const LIST_ROW_OVERHEAD: usize = 6;
/// Narrowest the path column shrinks to make room for the stat bar.
const MIN_PATH_WIDTH: usize = 20;
/// Spaces between the path, counts, and bar columns.
const STAT_GAP: usize = 2;

impl App {
    pub fn new(source: Source) -> Result<Self> {
        let config = Config::load();
        let repository = Repository::open(source.clone())?;
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
            source,
            line_numbers: Vec::new(),
            status_message: None,
            search_prompt: None,
            pending_search: None,
            content_search: None,
            diff_cache: HashMap::new(),
            show_stats: false,
            line_stats: None,
        })
    }

    pub fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        while self.running {
            if self.needs_redraw {
                terminal.clear()?;
                self.needs_redraw = false;
            }
            terminal.draw(|frame| self.draw(frame))?;
            if let Some(regex) = self.pending_search.take() {
                self.start_search(regex);
                continue;
            }
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
            let search_block = Block::default().title(" Filter ").borders(Borders::ALL);
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

        let mut match_counts = vec![0usize; self.filtered_indices.len()];
        if let Some(search) = &self.content_search {
            for &(pos, _) in &search.matches {
                if let Some(count) = match_counts.get_mut(pos) {
                    *count += 1;
                }
            }
        }

        let stats = self.line_stats.as_ref().filter(|_| self.show_stats);
        let visible_files = || {
            self.filtered_indices
                .iter()
                .filter_map(|&idx| self.files.get(idx))
        };
        let file_stat = |file: &ChangedFile| {
            stats
                .and_then(|s| s.get(&file.path))
                .copied()
                .unwrap_or_default()
        };
        let max_changed = visible_files()
            .map(|f| file_stat(f).added + file_stat(f).removed)
            .max()
            .unwrap_or(0);
        let added_width = visible_files()
            .map(|f| file_stat(f).added.to_string().len())
            .max()
            .unwrap_or(1);
        let removed_width = visible_files()
            .map(|f| file_stat(f).removed.to_string().len())
            .max()
            .unwrap_or(1);
        // Counts always fit; the path column gives way next (down to a floor), then the bar.
        let row_width = (list_area.width as usize).saturating_sub(LIST_ROW_OVERHEAD);
        let fixed_width = STAT_GAP + added_width + removed_width + 3 + STAT_GAP;
        let bar_wanted = max_changed.min(STAT_BAR_WIDTH);
        let longest_path = visible_files()
            .map(|f| f.path.chars().count())
            .max()
            .unwrap_or(0);
        let path_width = row_width
            .saturating_sub(fixed_width + bar_wanted)
            .max(MIN_PATH_WIDTH)
            .min(longest_path);
        let bar_width = row_width
            .saturating_sub(path_width + fixed_width)
            .min(bar_wanted);

        // Build list items from filtered indices
        let items: Vec<ListItem> = visible_files()
            .zip(&match_counts)
            .map(|(file, &count)| {
                let status_char = match file.status {
                    FileStatus::Modified => ("M", Color::Yellow),
                    FileStatus::Added => ("A", Color::Green),
                    FileStatus::Deleted => ("D", Color::Red),
                    FileStatus::Renamed => ("R", Color::Cyan),
                    FileStatus::Untracked => ("?", Color::Gray),
                };
                let mut spans = vec![Span::styled(
                    format!("{} ", status_char.0),
                    Style::default().fg(status_char.1),
                )];
                if stats.is_some() {
                    let path = truncate_start(&file.path, path_width);
                    let pad = path_width - path.chars().count();
                    spans.push(Span::raw(path));
                    spans.push(Span::raw(" ".repeat(pad + STAT_GAP)));
                    spans.extend(stat_spans(
                        file_stat(file),
                        added_width,
                        removed_width,
                        max_changed,
                        bar_width,
                    ));
                } else {
                    spans.push(Span::raw(file.path.clone()));
                }
                if count > 0 {
                    spans.push(Span::styled(
                        format!(" ({count})"),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();

        let totals = stats.map_or(String::new(), |s| {
            let added: usize = s.values().map(|st| st.added).sum();
            let removed: usize = s.values().map(|st| st.removed).sum();
            format!(" +{added} -{removed}")
        });
        let title = format!(
            " Changed Files ({}/{}){totals} ",
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
            " Type to filter  Enter select  Esc cancel".to_string()
        } else if let Some(search) = &self.content_search {
            format!(
                " n/N {}  Esc clear  l view  / search  f filter  q quit",
                search_summary(search)
            )
        } else {
            " l view  e edit  c copy  s stats  / search  f filter  r reload  q quit".to_string()
        };
        self.draw_bottom_line(frame, help_area, help_text);
    }

    /// Bottom line: the open `/` prompt, else a transient status message, else `help_text`.
    fn draw_bottom_line(&self, frame: &mut Frame, area: ratatui::layout::Rect, help_text: String) {
        if let Some(input) = &self.search_prompt {
            frame.render_widget(Paragraph::new(format!("/{input}")), area);
            frame.set_cursor_position(Position::new(
                area.x + 1 + input.chars().count() as u16,
                area.y,
            ));
            return;
        }
        let (help_str, help_color) = match &self.status_message {
            Some(msg) => (msg.clone(), Color::Green),
            None => (help_text, Color::DarkGray),
        };
        let help = Paragraph::new(help_str).style(Style::default().fg(help_color));
        frame.render_widget(help, area);
    }

    fn draw_diff_view(&mut self, frame: &mut Frame) {
        let area = frame.area();

        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

        let title = format!(" {} ", self.selected_file.as_deref().unwrap_or("Diff"));

        let visible_height = chunks[0].height.saturating_sub(2) as usize;
        let list_pos = self.list_state.selected();
        let current_match = self
            .content_search
            .as_ref()
            .and_then(|s| s.current.and_then(|i| s.matches.get(i)).copied());
        let visible_lines: Vec<Line> = self
            .diff_lines
            .iter()
            .enumerate()
            .skip(self.diff_scroll as usize)
            .take(visible_height)
            .map(|(row, line)| match &self.content_search {
                Some(search) => {
                    let style = if list_pos.map(|pos| (pos, row)) == current_match {
                        current_match_style()
                    } else {
                        match_style()
                    };
                    highlight_matches(line, &search.regex, style)
                }
                None => line.clone(),
            })
            .collect();

        let mut diff = Paragraph::new(visible_lines)
            .block(Block::default().title(title).borders(Borders::ALL));
        if self.wrap {
            // `trim: false` keeps leading whitespace so wrapped code stays aligned.
            diff = diff.wrap(Wrap { trim: false });
        }

        frame.render_widget(diff, chunks[0]);

        let total_lines = self.diff_lines.len();
        if total_lines > visible_height {
            let mut scrollbar_state = ScrollbarState::new(total_lines)
                .position(self.diff_scroll as usize)
                .viewport_content_length(visible_height);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None);
            frame.render_stateful_widget(
                scrollbar,
                chunks[0].inner(ratatui::layout::Margin {
                    horizontal: 0,
                    vertical: 1,
                }),
                &mut scrollbar_state,
            );
        }
        let current_line = self.diff_scroll as usize + 1;
        let position =
            scroll_position_label(self.diff_scroll as usize, visible_height, total_lines);
        // Kept within 80 columns; a search drops `r` and `h` to make room for its summary.
        let nav_help = match &self.content_search {
            Some(search) => format!("n/N {}  Esc clear  e edit  c copy", search_summary(search)),
            None => "/ search  e edit  c copy".to_string(),
        };
        let more_help = if self.content_search.is_some() {
            ""
        } else {
            "  r reload  h back"
        };
        let help_text = format!(
            " {nav_help}  w wrap:{}{more_help}  {}/{} {}",
            if self.wrap { "on" } else { "off" },
            current_line.min(total_lines),
            total_lines,
            position
        );
        self.draw_bottom_line(frame, chunks[1], help_text);
    }

    fn handle_events(&mut self) -> Result<()> {
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                return Ok(());
            }

            // Clear any transient status (e.g. "Copied: …") on the next keypress.
            self.status_message = None;
            if self.search_prompt.is_some() {
                self.handle_search_prompt_keys(key.code);
                return Ok(());
            }
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
                KeyCode::Char('n') if self.content_search.is_some() => self.jump_to_match(true),
                KeyCode::Char('N') if self.content_search.is_some() => self.jump_to_match(false),
                KeyCode::Esc => self.clear_search(),
                KeyCode::Char('j') | KeyCode::Char('n') | KeyCode::Down => self.select_next(),
                KeyCode::Char('k') | KeyCode::Char('N') | KeyCode::Up => self.select_previous(),
                KeyCode::Char('/') => self.search_prompt = Some(String::new()),
                KeyCode::Char('f') => {
                    self.search_mode = true;
                }
                KeyCode::Char('e') => self.open_selected_in_editor(),
                KeyCode::Char('c') => self.copy_current_path(),
                KeyCode::Char('s') => self.toggle_stats(),
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
            KeyCode::Esc if self.content_search.is_some() => self.clear_search(),
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
            KeyCode::Char('/') => self.search_prompt = Some(String::new()),
            KeyCode::Char('n') if self.content_search.is_some() => self.jump_to_match(true),
            KeyCode::Char('N') if self.content_search.is_some() => self.jump_to_match(false),
            KeyCode::Char('n') => self.show_adjacent_file(true),
            KeyCode::Char('N') => self.show_adjacent_file(false),
            _ => {}
        }
    }

    fn handle_search_prompt_keys(&mut self, code: KeyCode) {
        let Some(input) = self.search_prompt.as_mut() else {
            return;
        };
        match code {
            KeyCode::Esc => self.search_prompt = None,
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) => input.push(c),
            KeyCode::Enter => {
                let input = self.search_prompt.take().unwrap_or_default();
                if input.is_empty() {
                    return;
                }
                match smart_case_regex(&input) {
                    Ok(regex) => {
                        self.status_message = Some(format!("Searching all files for /{input}/…"));
                        self.pending_search = Some(regex);
                    }
                    Err(e) => {
                        // regex errors span several lines; the last one names the problem.
                        let msg = e.to_string();
                        let reason = msg.lines().last().unwrap_or_default().to_string();
                        self.status_message = Some(format!("Invalid regex: {reason}"));
                    }
                }
            }
            _ => {}
        }
    }

    /// Run a freshly submitted search and jump to the first match at or after the view.
    fn start_search(&mut self, regex: Regex) {
        self.status_message = None;
        let search = self.search_files(regex);
        if search.matches.is_empty() {
            self.status_message = Some(format!("Pattern not found: {}", search.regex));
            self.clear_search();
            return;
        }
        self.content_search = Some(search);
        let pos = self.list_state.selected().unwrap_or(0);
        let row = match self.screen {
            Screen::DiffView => (self.diff_scroll as usize).checked_sub(1),
            Screen::FileList => None,
        };
        self.jump_from((pos, row), true);
    }

    /// Render every listed file's diff (in parallel, reusing the cache) and collect regex matches.
    fn search_files(&mut self, regex: Regex) -> ContentSearch {
        self.render_uncached_diffs();
        let matches = self
            .filtered_indices
            .iter()
            .enumerate()
            .filter_map(|(pos, &idx)| Some((pos, self.diff_cache.get(&self.files.get(idx)?.path)?)))
            .flat_map(|(pos, lines)| {
                lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| regex.is_match(&line_text(line)))
                    .map(move |(row, _)| (pos, row))
            })
            .collect();
        ContentSearch {
            regex,
            matches,
            current: None,
        }
    }

    fn render_uncached_diffs(&mut self) {
        let paths: Vec<&str> = self
            .filtered_indices
            .iter()
            .filter_map(|&idx| self.files.get(idx))
            .map(|f| f.path.as_str())
            .filter(|path| !self.diff_cache.contains_key(*path))
            .collect();
        if paths.is_empty() {
            return;
        }
        let width = diff_width();
        let config = &self.config.diff;
        // The file list already snapshotted; skipping the snapshot lets jj run concurrently.
        let source = self.source.without_snapshot();
        let next = AtomicUsize::new(0);
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(paths.len());
        let rendered: Vec<(String, Vec<Line<'static>>)> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| {
                        let mut out = Vec::new();
                        while let Some(path) = paths.get(next.fetch_add(1, Ordering::Relaxed)) {
                            out.push((
                                path.to_string(),
                                render_diff(path, width, config, &source),
                            ));
                        }
                        out
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|w| w.join().unwrap_or_default())
                .collect()
        });
        self.diff_cache.extend(rendered);
    }

    /// Jump to the next (or previous) match after the current view position, wrapping around.
    fn jump_to_match(&mut self, forward: bool) {
        let pos = self.list_state.selected().unwrap_or(0);
        let row = match self.screen {
            Screen::DiffView => Some(self.diff_scroll as usize),
            Screen::FileList => None,
        };
        self.jump_from((pos, row), forward);
    }

    fn jump_from(&mut self, cursor: (usize, Option<usize>), forward: bool) {
        let Some(search) = self.content_search.as_mut() else {
            return;
        };
        let Some((idx, wrapped)) = find_match(&search.matches, cursor, forward) else {
            self.status_message = Some(format!("Pattern not found: {}", search.regex));
            return;
        };
        search.current = Some(idx);
        let (pos, row) = search.matches[idx];
        if wrapped {
            self.status_message = Some(if forward {
                "Search hit BOTTOM, continuing at TOP".to_string()
            } else {
                "Search hit TOP, continuing at BOTTOM".to_string()
            });
        }
        if self.screen != Screen::DiffView || self.list_state.selected() != Some(pos) {
            self.list_state.select(Some(pos));
            if !self.load_diff_for_selected() {
                return;
            }
            self.screen = Screen::DiffView;
        }
        self.diff_scroll = row as u16;
    }

    fn clear_search(&mut self) {
        self.content_search = None;
        self.diff_cache.clear();
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
        // Matches are keyed by list position, so re-run the search over the new list.
        if let Some(search) = self.content_search.take() {
            self.content_search = Some(self.search_files(search.regex));
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
                    self.diff_lines = match self.diff_cache.get(&file.path) {
                        Some(lines) => lines.clone(),
                        None => render_diff(
                            &file.path,
                            diff_width(),
                            &self.config.diff,
                            &self.source,
                        ),
                    };

                    // Map each rendered hunk-body row to its new-file line number, so
                    // `e` can open the editor at whatever line is at the top of the
                    // current scroll position, not just the hunk's first change. Each
                    // hunk's body starts right after its rendered separator rule.
                    let hunk_targets = crate::git::hunk_line_targets(&file.path, &self.source);
                    let mut targets = hunk_targets.into_iter();
                    let mut current_hunk: Option<std::vec::IntoIter<u32>> = None;
                    self.line_numbers = self
                        .diff_lines
                        .iter()
                        .map(|line| {
                            if is_rule_row(line) {
                                current_hunk = targets.next().map(|v| v.into_iter());
                                return None;
                            }
                            let target = current_hunk.as_mut().and_then(|iter| iter.next());
                            if target.is_none() {
                                current_hunk = None;
                            }
                            target
                        })
                        .collect();

                    self.diff_scroll = 0;
                    return true;
                }
            }
        }
        false
    }

    fn toggle_stats(&mut self) {
        self.show_stats = !self.show_stats;
        if self.show_stats && self.line_stats.is_none() {
            match self.repository.get_line_stats() {
                Ok(stats) => self.line_stats = Some(stats),
                Err(err) => {
                    self.show_stats = false;
                    self.status_message = Some(format!("Stats failed: {err}"));
                }
            }
        }
    }

    /// Re-derive the file list and current diff from jj (after an edit or `r`).
    fn refresh(&mut self) {
        let prev_path = self.selected_file.clone();
        let prev_scroll = self.diff_scroll;
        // Drop renders from before the reload; update_filter re-renders them for an active search.
        self.diff_cache.clear();
        self.line_stats = None;
        if self.show_stats {
            self.line_stats = self.repository.get_line_stats().ok();
        }

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

    /// File line to open the editor at: the new-file line currently at the top of
    /// the diff view's scroll position, so the editor lands on what's on screen
    /// even deep inside a large hunk. When that row is a header/rule decoration
    /// row rather than a body line, falls forward to the next body row, then
    /// backward. None when not viewing a diff or no hunks were detected.
    fn current_hunk_line(&self) -> Option<u32> {
        if self.screen != Screen::DiffView || self.line_numbers.is_empty() {
            return None;
        }
        let scroll = (self.diff_scroll as usize).min(self.line_numbers.len() - 1);
        self.line_numbers[scroll..]
            .iter()
            .find_map(|&n| n)
            .or_else(|| self.line_numbers[..scroll].iter().rev().find_map(|&n| n))
    }

    fn open_in_editor(&mut self) {
        if self.source.is_patch() {
            self.status_message = Some("Editing is unavailable when viewing a patch".to_string());
            return;
        }
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

/// Label for how far scrolled into the diff the view is, matching `less`'s
/// Top/Bot/All/percentage convention.
fn scroll_position_label(scroll: usize, visible_height: usize, total_lines: usize) -> String {
    if total_lines <= visible_height {
        return "All".to_string();
    }
    if scroll == 0 {
        return "Top".to_string();
    }
    let max_scroll = total_lines - visible_height;
    if scroll >= max_scroll {
        return "Bot".to_string();
    }
    format!("{}%", scroll * 100 / max_scroll)
}

/// Diff tool output width: the terminal width minus the diff block's border.
fn diff_width() -> u16 {
    terminal::size()
        .map(|(w, _)| w.saturating_sub(2))
        .unwrap_or(80)
}

/// Run the configured diff tool for one file and parse its ANSI output into styled lines.
fn render_diff(
    path: &str,
    width: u16,
    config: &crate::config::DiffConfig,
    source: &Source,
) -> Vec<Line<'static>> {
    let content = crate::git::get_diff(path, width, config, source);
    match content.as_slice().into_text() {
        Ok(text) => text
            .lines
            .into_iter()
            .map(|line| {
                Line::from(
                    line.spans
                        .into_iter()
                        .map(|span| Span::styled(span.content.to_string(), span.style))
                        .collect::<Vec<_>>(),
                )
            })
            .collect(),
        // Fallback: plain text without ANSI parsing
        Err(_) => String::from_utf8_lossy(&content)
            .lines()
            .map(|s| Line::raw(s.to_string()))
            .collect(),
    }
}

/// Compile `pattern` case-insensitively unless it has an uppercase letter outside an escape.
/// `+N -M` counts and a `+++--` bar scaled so the biggest change fills `bar_width`.
fn stat_spans(
    stat: LineStat,
    added_width: usize,
    removed_width: usize,
    max_changed: usize,
    bar_width: usize,
) -> Vec<Span<'static>> {
    let green = Style::default().fg(Color::Green);
    let red = Style::default().fg(Color::Red);
    if stat.binary {
        return vec![Span::styled("bin", Style::default().fg(Color::DarkGray))];
    }
    let (plus, minus) = stat_bar(stat, max_changed, bar_width);
    vec![
        Span::styled(
            format!("{:>w$}", format!("+{}", stat.added), w = added_width + 1),
            green,
        ),
        Span::raw(" "),
        Span::styled(
            format!(
                "{:>w$}",
                format!("-{}", stat.removed),
                w = removed_width + 1
            ),
            red,
        ),
        Span::raw(" ".repeat(STAT_GAP)),
        Span::styled("+".repeat(plus), green),
        Span::styled("-".repeat(minus), red),
    ]
}

/// Bar lengths for one file; any nonzero side gets at least one cell.
fn stat_bar(stat: LineStat, max_changed: usize, bar_width: usize) -> (usize, usize) {
    if bar_width == 0 || max_changed == 0 {
        return (0, 0);
    }
    let scale = |n: usize| match n {
        0 => 0,
        n => (n * bar_width).div_ceil(max_changed.max(bar_width)).max(1),
    };
    (scale(stat.added), scale(stat.removed))
}

/// `path` cut to `width` columns from the left, keeping the filename end.
fn truncate_start(path: &str, width: usize) -> String {
    let len = path.chars().count();
    if len <= width {
        return path.to_string();
    }
    let tail: String = path.chars().skip(len + 1 - width.max(1)).collect();
    format!("…{tail}")
}

fn smart_case_regex(pattern: &str) -> Result<Regex, regex::Error> {
    let mut escaped = false;
    let has_upper = pattern.chars().any(|c| {
        let upper = !escaped && c.is_uppercase();
        escaped = !escaped && c == '\\';
        upper
    });
    RegexBuilder::new(pattern)
        .case_insensitive(!has_upper)
        .build()
}

fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn match_style() -> Style {
    Style::default().fg(Color::Black).bg(Color::Yellow)
}

fn current_match_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::LightRed)
        .add_modifier(Modifier::BOLD)
}

/// "i/N in F files" for the help line.
fn search_summary(search: &ContentSearch) -> String {
    let mut files: Vec<usize> = search.matches.iter().map(|&(pos, _)| pos).collect();
    files.dedup();
    let current = search
        .current
        .map_or("-".to_string(), |i| (i + 1).to_string());
    format!(
        "{current}/{} in {} files",
        search.matches.len(),
        files.len()
    )
}

/// Index of the first match after `cursor` (or last before it when `!forward`), wrapping
/// around the ends; the bool is true when it wrapped. A `None` row sorts before every row.
fn find_match(
    matches: &[(usize, usize)],
    cursor: (usize, Option<usize>),
    forward: bool,
) -> Option<(usize, bool)> {
    if matches.is_empty() {
        return None;
    }
    let key = |&(pos, row): &(usize, usize)| (pos, Some(row));
    if forward {
        Some(match matches.iter().position(|m| key(m) > cursor) {
            Some(idx) => (idx, false),
            None => (0, true),
        })
    } else {
        Some(match matches.iter().rposition(|m| key(m) < cursor) {
            Some(idx) => (idx, false),
            None => (matches.len() - 1, true),
        })
    }
}

/// Copy of `line` with every non-empty `regex` match patched with `style`, splitting spans.
fn highlight_matches(line: &Line<'static>, regex: &Regex, style: Style) -> Line<'static> {
    let text = line_text(line);
    let ranges: Vec<(usize, usize)> = regex
        .find_iter(&text)
        .filter(|m| !m.is_empty())
        .map(|m| (m.start(), m.end()))
        .collect();
    if ranges.is_empty() {
        return line.clone();
    }
    let mut spans = Vec::new();
    let mut start = 0;
    for span in &line.spans {
        let end = start + span.content.len();
        let mut cut = start;
        for &(match_start, match_end) in &ranges {
            let (s, e) = (match_start.max(start), match_end.min(end));
            if s >= e {
                continue;
            }
            if s > cut {
                spans.push(Span::styled(text[cut..s].to_string(), span.style));
            }
            spans.push(Span::styled(
                text[s..e].to_string(),
                span.style.patch(style),
            ));
            cut = e;
        }
        if end > cut {
            spans.push(Span::styled(text[cut..end].to_string(), span.style));
        }
        start = end;
    }
    Line {
        spans,
        ..line.clone()
    }
}

/// True if a rendered row is a delta hunk-separator rule (a run of `─`),
/// used to locate hunk boundaries in the rendered diff.
fn is_rule_row(line: &Line) -> bool {
    let text = line_text(line);
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

#[cfg(test)]
mod tests {
    use super::{
        find_match, highlight_matches, line_text, smart_case_regex, stat_bar, truncate_start,
    };
    use crate::git::LineStat;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};
    use regex::Regex;

    const MATCHES: [(usize, usize); 3] = [(0, 5), (0, 9), (2, 1)];

    #[test]
    fn forward_finds_next_match_across_files() {
        assert_eq!(find_match(&MATCHES, (0, Some(5)), true), Some((1, false)));
        assert_eq!(find_match(&MATCHES, (0, Some(9)), true), Some((2, false)));
        // A file-list cursor (no row) includes the file's own first match.
        assert_eq!(find_match(&MATCHES, (2, None), true), Some((2, false)));
    }

    #[test]
    fn forward_wraps_past_the_last_match() {
        assert_eq!(find_match(&MATCHES, (2, Some(1)), true), Some((0, true)));
    }

    #[test]
    fn backward_finds_previous_and_wraps() {
        assert_eq!(find_match(&MATCHES, (2, Some(1)), false), Some((1, false)));
        assert_eq!(find_match(&MATCHES, (2, None), false), Some((1, false)));
        assert_eq!(find_match(&MATCHES, (0, Some(5)), false), Some((2, true)));
    }

    #[test]
    fn no_matches_finds_nothing() {
        assert_eq!(find_match(&[], (0, None), true), None);
    }

    #[test]
    fn lowercase_pattern_ignores_case() {
        assert!(smart_case_regex("foo").unwrap().is_match("FOO"));
        // Escapes like `\S` are not uppercase literals.
        assert!(smart_case_regex(r"foo\S").unwrap().is_match("FOOx"));
    }

    #[test]
    fn uppercase_pattern_matches_case() {
        assert!(!smart_case_regex("Foo").unwrap().is_match("foo"));
        assert!(smart_case_regex("Foo").unwrap().is_match("Foo"));
        // An escaped backslash does not hide the uppercase letter after it.
        assert!(!smart_case_regex(r"\\Foo").unwrap().is_match(r"\foo"));
    }

    #[test]
    fn stat_bar_is_unscaled_when_small_and_scaled_when_large() {
        let stat = |added, removed| LineStat {
            added,
            removed,
            binary: false,
        };
        assert_eq!(stat_bar(stat(3, 2), 10, 40), (3, 2));
        assert_eq!(stat_bar(stat(400, 0), 400, 40), (40, 0));
        assert_eq!(stat_bar(stat(400, 0), 400, 10), (10, 0));
        // A tiny change next to a huge one still gets one cell per side.
        assert_eq!(stat_bar(stat(1, 1), 1000, 40), (1, 1));
        assert_eq!(stat_bar(stat(5, 5), 10, 0), (0, 0));
    }

    #[test]
    fn truncate_start_keeps_the_filename_end() {
        assert_eq!(truncate_start("src/app.rs", 20), "src/app.rs");
        assert_eq!(truncate_start("src/git/repository.rs", 12), "…pository.rs");
    }

    #[test]
    fn highlight_splits_spans_and_keeps_text() {
        let red = Style::default().fg(Color::Red);
        let line = Line::from(vec![Span::styled("foo b", red), Span::raw("ar baz")]);
        let hl = Style::default().bg(Color::Yellow);
        let out = highlight_matches(&line, &Regex::new("bar").unwrap(), hl);
        assert_eq!(line_text(&out), "foo bar baz");
        let styled: Vec<(&str, Style)> = out
            .spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style))
            .collect();
        assert_eq!(
            styled,
            vec![
                ("foo ", red),
                ("b", red.patch(hl)),
                ("ar", hl),
                (" baz", Style::default()),
            ]
        );
    }
}
