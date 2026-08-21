use rio_backend::config::hints::Hint;
use rio_backend::crosswords::grid::Dimensions;
use rio_backend::crosswords::pos::{Column, Line, Pos};
use rio_backend::event::EventListener;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Extract the visible text of `line` together with a byte-offset → grid
/// column mapping. Spacer cells (the trailing half of a wide glyph, and
/// the `LeadingSpacer` placed at the soft-wrap boundary) are skipped:
/// they carry a placeholder `' '` that is not a separate visible
/// character and would otherwise desynchronize the byte-to-column map.
///
/// `byte_to_col[i]` is the grid column of the cell whose codepoint's
/// UTF-8 encoding contains byte `i` of the returned string. Trailing
/// whitespace is left intact so the mapping stays aligned across the
/// full row.
///
/// Used by the regex hint pipeline to convert onig's byte offsets
/// (which would otherwise mis-locate the click target when emoji or
/// CJK glyphs precede a URL) back into grid columns.
pub(crate) fn extract_line_text_with_cols<T: EventListener>(
    term: &rio_backend::crosswords::Crosswords<T>,
    line: Line,
) -> (String, Vec<Column>) {
    let grid = &term.grid;
    let mut text = String::with_capacity(grid.columns());
    let mut byte_to_col = Vec::with_capacity(grid.columns());

    for col in (0..grid.columns()).map(Column) {
        let pos = Pos::new(line, col);
        let cell = &grid[pos];
        if cell.is_spacer() || cell.is_leading_spacer() {
            continue;
        }

        for c in grid.cell_text(pos).map(|c| if c == '\0' { ' ' } else { c }) {
            text.push(c);
            byte_to_col.extend(std::iter::repeat_n(col, c.len_utf8()));
        }
    }

    (text, byte_to_col)
}

pub(crate) fn regex_match<T: EventListener>(
    term: &rio_backend::crosswords::Crosswords<T>,
    line: Line,
    line_text: &str,
    byte_to_col: &[Column],
    start: usize,
    end: usize,
    hint: Rc<Hint>,
) -> Option<HintMatch> {
    if start == end || end > byte_to_col.len() {
        return None;
    }

    let mut text = line_text[start..end].to_string();
    if hint.post_processing {
        text = post_process_hyperlink_uri(&text);
    }
    if text.is_empty() {
        return None;
    }

    let start_col = byte_to_col[start];
    let mut end_col = byte_to_col[start + text.len() - 1];
    if term.grid[line][end_col].is_wide() {
        end_col += 1;
    }

    Some(HintMatch {
        text,
        start: Pos::new(line, start_col),
        end: Pos::new(line, end_col),
        hint,
    })
}

/// State for hint selection mode
pub struct HintState {
    /// Currently active hint configuration
    active_hint: Option<Rc<Hint>>,

    /// Visible matches for the current hint
    matches: Vec<HintMatch>,

    /// Labels for each match (as Vec<char>)
    labels: Vec<Vec<char>>,

    /// Keys pressed so far for hint selection
    keys: Vec<char>,

    /// Alphabet for generating labels
    alphabet: String,
}

/// A match found by a hint
#[derive(Debug, Clone)]
pub struct HintMatch {
    /// The text that was matched
    pub text: String,

    /// Start position of the match
    pub start: Pos,

    /// End position of the match
    pub end: Pos,

    /// The hint configuration that created this match
    pub hint: Rc<Hint>,
}

impl HintState {
    pub fn new(alphabet: String) -> Self {
        Self {
            active_hint: None,
            matches: Vec::new(),
            labels: Vec::new(),
            keys: Vec::new(),
            alphabet,
        }
    }

    /// Check if hint mode is active
    pub fn is_active(&self) -> bool {
        self.active_hint.is_some()
    }

    /// Start hint mode with the given hint configuration
    pub fn start(&mut self, hint: Rc<Hint>) {
        self.active_hint = Some(hint);
        self.keys.clear();
        // matches and labels will be updated by update_matches
    }

    /// Stop hint mode
    pub fn stop(&mut self) {
        self.active_hint = None;
        self.matches.clear();
        self.labels.clear();
        self.keys.clear();
    }

    /// Update visible matches for the current hint
    pub fn update_matches<T: EventListener>(
        &mut self,
        term: &rio_backend::crosswords::Crosswords<T>,
    ) {
        self.rebuild_matches(term);
        if self.matches.is_empty() {
            self.stop();
        }
    }

    /// Refresh matches without leaving hint mode when none are visible.
    pub fn refresh_matches<T: EventListener>(
        &mut self,
        term: &rio_backend::crosswords::Crosswords<T>,
    ) {
        self.keys.clear();
        self.rebuild_matches(term);
    }

    fn rebuild_matches<T: EventListener>(
        &mut self,
        term: &rio_backend::crosswords::Crosswords<T>,
    ) {
        self.matches.clear();

        let hint = match &self.active_hint {
            Some(hint) => hint.clone(),
            None => {
                return;
            }
        };

        // Find OSC 8 hyperlinks if enabled
        if hint.hyperlinks {
            self.find_hyperlink_matches(term, hint.clone());
        }

        // Insert hyperlinks first so they win deduplication when a regex match
        // starts on the same cell.
        if let Some(regex_pattern) = &hint.regex {
            if let Ok(regex) = onig::Regex::new(regex_pattern) {
                self.find_regex_matches(term, &regex, hint.clone());
            }
        }

        if self.matches.is_empty() {
            self.labels.clear();
            return;
        }

        // Sort and dedup matches
        self.matches.sort_by_key(|m| (m.start.row, m.start.col));
        self.matches.dedup_by_key(|m| m.start);

        // Generate labels for matches
        self.generate_labels();
    }

    /// Handle keyboard input during hint selection
    pub fn keyboard_input<T: EventListener>(
        &mut self,
        term: &rio_backend::crosswords::Crosswords<T>,
        c: char,
    ) -> Option<(HintMatch, bool)> {
        match c {
            // Use backspace to remove the last character pressed
            '\x08' | '\x1f' => {
                self.keys.pop();
                // Only update matches after backspace to regenerate visible labels
                self.update_matches(term);
                return None;
            }
            // Cancel hint highlighting on ESC/Ctrl+c
            '\x1b' | '\x03' => {
                self.stop();
                return None;
            }
            _ => (),
        }

        let persist = self.active_hint.as_ref()?.persist;

        // Find the last label starting with the input character
        let (index, remaining_len) = self
            .visible_labels()
            .rev()
            .find(|(_, remaining)| {
                remaining
                    .first()
                    .is_some_and(|label| key_matches(*label, c))
            })
            .map(|(index, remaining)| (index, remaining.len()))?;

        // Check if this completes the label (only one character remaining)
        if remaining_len == 1 {
            let hint_match = self.matches.get(index)?.clone();
            let paste = self.labels[index]
                .iter()
                .zip(self.keys.iter().copied().chain(std::iter::once(c)))
                .filter(|(label, _)| label.is_lowercase())
                .all(|(label, input)| is_uppercase_variant(*label, input))
                && self.labels[index].iter().any(|label| label.is_lowercase());

            // Exit hint mode unless it requires explicit dismissal
            if persist {
                self.keys.clear();
            } else {
                self.stop();
            }

            Some((hint_match, paste))
        } else {
            // Store character to preserve the selection
            self.keys.push(c);
            None
        }
    }

    /// Get current matches
    pub fn matches(&self) -> &[HintMatch] {
        &self.matches
    }

    /// Get visible labels (filtered by current input)
    pub fn visible_labels(&self) -> impl DoubleEndedIterator<Item = (usize, &[char])> {
        let keys_len = self.keys.len();
        self.labels
            .iter()
            .enumerate()
            .filter_map(move |(i, label)| {
                if label.len() >= keys_len
                    && label
                        .iter()
                        .zip(&self.keys)
                        .all(|(label, input)| key_matches(*label, *input))
                {
                    Some((i, &label[keys_len..]))
                } else {
                    None
                }
            })
    }

    fn find_regex_matches<T: EventListener>(
        &mut self,
        term: &rio_backend::crosswords::Crosswords<T>,
        regex: &onig::Regex,
        hint: Rc<Hint>,
    ) {
        // Get the visible area of the terminal
        let grid = &term.grid;
        let display_offset = grid.display_offset();
        let visible_lines = grid.screen_lines();

        // Scan each visible line for matches
        for line_idx in 0..visible_lines {
            let line = Line(line_idx as i32 - display_offset as i32);
            // Extract text plus a byte→grid-column mapping so regex byte
            // offsets translate back to the right cells when the line
            // contains wide glyphs or multibyte codepoints.
            let (line_text, byte_to_col) = extract_line_text_with_cols(term, line);

            // Find all matches in this line. Onig yields (byte_start, byte_end);
            for (start, end) in regex.find_iter(&line_text) {
                if let Some(hint_match) = regex_match(
                    term,
                    line,
                    &line_text,
                    &byte_to_col,
                    start,
                    end,
                    hint.clone(),
                ) {
                    self.matches.push(hint_match);
                }
            }
        }
    }

    fn find_hyperlink_matches<T: EventListener>(
        &mut self,
        term: &rio_backend::crosswords::Crosswords<T>,
        hint: Rc<Hint>,
    ) {
        // Walk the visible region looking for OSC 8 hyperlink spans.
        //
        // After the cell repack, hyperlinks live in the per-grid
        // `extras_table`. Each cell carries an `extras_id: u16`; cells
        // in the same hyperlink span share that id. We compare ids
        // (cheap u16 compare) to find the start and end of each span,
        // then look up the URI once via `Crosswords::cell_hyperlink`.
        let grid = &term.grid;
        let display_offset = grid.display_offset();
        let visible_lines = grid.screen_lines();

        for line_idx in 0..visible_lines {
            let line = Line(line_idx as i32 - display_offset as i32);
            let mut col = 0usize;
            let cols = grid.columns();
            while col < cols {
                let id = match term.cell_hyperlink_id(line, Column(col)) {
                    Some(id) => id,
                    None => {
                        col += 1;
                        continue;
                    }
                };

                // Found the start of a hyperlink span. Walk forward
                // until the extras_id changes.
                let start_col = col;
                let mut end_col = col;
                while end_col < cols
                    && term.cell_hyperlink_id(line, Column(end_col)) == Some(id)
                {
                    end_col += 1;
                }

                // Look up the URI once for the whole span.
                if let Some(hyperlink) = term.cell_hyperlink(line, Column(start_col)) {
                    let mut uri = hyperlink.uri().to_string();
                    if hint.post_processing {
                        uri = post_process_hyperlink_uri(&uri);
                    }
                    if uri.is_empty() {
                        col = end_col;
                        continue;
                    }
                    self.matches.push(HintMatch {
                        text: uri,
                        start: Pos::new(line, Column(start_col)),
                        end: Pos::new(line, Column(end_col - 1)),
                        hint: hint.clone(),
                    });
                }

                col = end_col;
            }
        }
    }

    fn generate_labels(&mut self) {
        use rio_backend::config::hints::{HintAction, HintInternalAction::*};

        let action = &self
            .active_hint
            .as_ref()
            .expect("label generation requires an active hint")
            .action;
        let share_by_text = match action {
            HintAction::Action {
                action: Copy | Paste | Open,
            }
            | HintAction::Command { .. } => true,
            HintAction::Action {
                action: Select | MoveViModeCursor,
            } => false,
        };
        let mut index_by_text = HashMap::new();
        let label_indices: Vec<_> = self
            .matches
            .iter()
            .enumerate()
            .map(|(index, hint_match)| {
                if share_by_text {
                    let next_index = index_by_text.len();
                    *index_by_text
                        .entry(hint_match.text.as_str())
                        .or_insert(next_index)
                } else {
                    index
                }
            })
            .collect();
        let label_count = label_indices.iter().max().map_or(0, |index| *index + 1);
        let Some(labels) = hint_labels(&self.alphabet, label_count) else {
            tracing::error!("hint alphabet must contain enough unique characters");
            self.stop();
            return;
        };

        self.labels = label_indices
            .into_iter()
            .map(|index| labels[index].clone())
            .collect();
    }
}

fn is_uppercase_variant(label: char, input: char) -> bool {
    let mut lowercase = input.to_lowercase();
    input.is_uppercase() && lowercase.next() == Some(label) && lowercase.next().is_none()
}

fn key_matches(label: char, input: char) -> bool {
    label == input || is_uppercase_variant(label, input)
}

fn hint_labels(alphabet: &str, count: usize) -> Option<Vec<Vec<char>>> {
    let alphabet_len = alphabet.chars().count();
    let mut seen = HashSet::new();
    let alphabet: Vec<char> = alphabet
        .chars()
        .filter(|c| seen.insert(c.to_lowercase().collect::<String>()))
        .collect();

    if alphabet.len() != alphabet_len {
        return None;
    }

    if alphabet.is_empty() || (alphabet.len() == 1 && count > 1) {
        return None;
    }

    let mut labels: VecDeque<Vec<char>> = alphabet.iter().map(|c| vec![*c]).collect();
    while labels.len() < count {
        let parent = labels.pop_front()?;
        for c in &alphabet {
            let mut child = parent.clone();
            child.push(*c);
            labels.push_back(child);
        }
    }

    Some(labels.into_iter().take(count).collect())
}

/// URI scheme prefixes that should never be resolved as file paths.
/// Matches the scheme branch of `DEFAULT_URL_REGEX`.
const URI_SCHEMES: &[&str] = &[
    "ipfs:",
    "ipns:",
    "magnet:",
    "mailto:",
    "gemini://",
    "gopher://",
    "https://",
    "http://",
    "news:",
    "file:",
    "git://",
    "ssh:",
    "ssh://",
    "ftp://",
    "tel:",
];

/// If `text` looks like a local filesystem path, resolve it against `cwd` and
/// return the absolute path when it exists on disk. Returns `None` for
/// URL-scheme strings, paths that don't exist, or anything we can't resolve
/// (e.g. relative path with no known `cwd`). On `None`, the caller should
/// fall back to the raw text and let the OS opener handle it.
///
/// Modelled on ghostty's `resolvePathForOpening` (`src/Surface.zig:2045`).
/// core only joins relative paths against the OSC 7 cwd; tilde
/// expansion lives in the macOS apprt's Swift `openURL`
/// (`.App.swift:715`, via `NSString.standardizingPath`), so `~/x`
/// works on macOS but isn't expanded on Linux/BSD where `xdg-open` gets the
/// literal `~`. Rio doesn't have a per-platform apprt layer, so we do the
/// expansion here to get consistent cross-platform behaviour:
///
/// 1. `~/x` and `~` expand via `dirs::home_dir()`.
/// 2. `$VAR/x` expands via `std::env::var` (ghostty doesn't do this on any
///    platform).
/// 3. Strings starting with a known URI scheme are rejected up front so the
///    OS opener routes them as URLs (saves one filesystem syscall vs
///    ghostty's "join cwd + stat → fail" path).
/// 4. Absolute paths are existence-checked too. short-circuits
///    absolute paths to `None` (caller passes raw); user-visible behaviour
///    is the same since the raw and resolved strings match.
pub fn resolve_path_for_opening(text: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    // Scheme URLs are not paths — let the OS opener route them.
    if URI_SCHEMES.iter().any(|s| text.starts_with(s)) {
        return None;
    }

    // Expand a recognized path prefix. Anything falling through is treated as
    // a bare relative path (e.g. `src/main.rs`).
    let expanded: PathBuf = if let Some(rest) = text.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else if text == "~" {
        dirs::home_dir()?
    } else if let Some(rest) = text.strip_prefix('$') {
        let (var_name, tail) = rest.split_once('/').unwrap_or((rest, ""));
        if var_name.is_empty() {
            return None;
        }
        let value = std::env::var(var_name).ok()?;
        let base = PathBuf::from(value);
        if tail.is_empty() {
            base
        } else {
            base.join(tail)
        }
    } else {
        PathBuf::from(text)
    };

    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd?.join(expanded)
    };

    if absolute.exists() {
        Some(absolute)
    } else {
        None
    }
}

/// Apply post-processing to hyperlink URIs (same as in screen/mod.rs)
pub(crate) fn post_process_hyperlink_uri(uri: &str) -> String {
    let mut end = uri.len();
    let mut open_parents = 0;
    let mut open_brackets = 0;

    for (index, c) in uri.char_indices() {
        match c {
            '(' => open_parents += 1,
            '[' => open_brackets += 1,
            ')' => {
                if open_parents == 0 {
                    end = index;
                    break;
                }
                open_parents -= 1;
            }
            ']' => {
                if open_brackets == 0 {
                    end = index;
                    break;
                }
                open_brackets -= 1;
            }
            _ => (),
        }
    }

    uri[..end]
        .trim_end_matches(['.', ',', ':', ';', '?', '!', '(', '[', '\''])
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_backend::config::hints::{HintAction, HintInternalAction};

    fn hint() -> Rc<Hint> {
        Rc::new(Hint {
            regex: Some("test".to_string()),
            hyperlinks: false,
            post_processing: true,
            persist: false,
            action: HintAction::Action {
                action: HintInternalAction::Copy,
            },
            mouse: Default::default(),
            binding: None,
        })
    }

    fn hint_match(text: &str, col: usize, hint: &Rc<Hint>) -> HintMatch {
        HintMatch {
            text: text.to_string(),
            start: Pos::new(Line(0), Column(col)),
            end: Pos::new(
                Line(0),
                Column(col + text.chars().count().saturating_sub(1)),
            ),
            hint: hint.clone(),
        }
    }

    #[test]
    fn test_label_generator() {
        assert_eq!(
            hint_labels("abc", 3),
            Some(vec![vec!['a'], vec!['b'], vec!['c']])
        );

        let labels = hint_labels("abc", 7).unwrap();
        assert_eq!(labels.len(), 7);
        assert_eq!(labels.iter().collect::<HashSet<_>>().len(), labels.len());
        assert!(labels.iter().all(|label| labels
            .iter()
            .all(|other| label == other || !other.starts_with(label))));
        assert_eq!(hint_labels("aaa", 2), None);
        assert_eq!(hint_labels("aA", 2), None);
    }

    #[test]
    fn test_visible_labels() {
        let mut state = HintState::new("abc".to_string());
        state.labels = vec![vec!['a', 'a'], vec!['a', 'b'], vec!['b', 'a']];

        state.keys = vec!['a'];
        assert_eq!(
            state.visible_labels().collect::<Vec<_>>(),
            vec![(0, &['a'][..]), (1, &['b'][..])]
        );
    }

    #[test]
    fn extraction_renders_empty_cells_as_spaces() {
        let terminal = mock_term_with_line("");
        let (text, columns) = extract_line_text_with_cols(&terminal, Line(0));

        assert_eq!(text, " ".repeat(terminal.grid.columns()));
        assert_eq!(columns.len(), terminal.grid.columns());
    }

    #[test]
    fn uppercase_label_selects_copy_and_paste_variant() {
        let hint = hint();
        let terminal = mock_term_with_line("test");
        let mut state = HintState::new("ab".to_string());
        state.active_hint = Some(hint.clone());
        state.matches = vec![hint_match("test", 0, &hint)];
        state.labels = vec![vec!['a', 'b']];

        assert!(state.keyboard_input(&terminal, 'A').is_none());
        let (selected, paste) = state.keyboard_input(&terminal, 'B').unwrap();

        assert_eq!(selected.text, "test");
        assert!(paste);
    }

    #[test]
    fn lowercase_or_mixed_case_label_only_selects() {
        let hint = hint();
        let terminal = mock_term_with_line("test");

        for input in [['a', 'b'], ['A', 'b']] {
            let mut state = HintState::new("ab".to_string());
            state.active_hint = Some(hint.clone());
            state.matches = vec![hint_match("test", 0, &hint)];
            state.labels = vec![vec!['a', 'b']];

            assert!(state.keyboard_input(&terminal, input[0]).is_none());
            let (_, paste) = state.keyboard_input(&terminal, input[1]).unwrap();
            assert!(!paste);
        }
    }

    #[test]
    fn persistent_hint_remains_active_after_selection() {
        let mut config = (*hint()).clone();
        config.persist = true;
        let hint = Rc::new(config);
        let terminal = mock_term_with_line("test");
        let mut state = HintState::new("ab".to_string());
        state.active_hint = Some(hint.clone());
        state.matches = vec![hint_match("test", 0, &hint)];
        state.labels = vec![vec!['a']];

        assert!(state.keyboard_input(&terminal, 'a').is_some());
        assert!(state.is_active());
        assert!(state.keys.is_empty());
    }

    #[test]
    fn scrolling_keeps_hint_mode_active_until_matches_reappear() {
        let mut term = mock_term_with_line("xxxx");
        term.resize(CrosswordsSize::new(4, 2));
        for (column, character) in "test".chars().enumerate() {
            term.grid[Line(0)][Column(column)].set_c(character);
        }
        term.grid.scroll_up(&(Line(0)..Line(2)), 1);

        let mut state = HintState::new("ab".to_string());
        state.start(hint());

        state.refresh_matches(&term);
        assert!(state.is_active());
        assert!(state.matches().is_empty());

        term.scroll_display(rio_backend::crosswords::grid::Scroll::Delta(1));
        state.refresh_matches(&term);
        assert!(state.is_active());
        assert_eq!(state.matches().len(), 1);
        assert_eq!(state.visible_labels().count(), 1);
    }

    #[test]
    fn repeated_matches_share_labels_unless_position_sensitive() {
        use rio_backend::config::hints::HintCommand;
        use HintAction::{Action, Command};
        use HintInternalAction::*;

        let labels_for = |action| {
            let mut config = (*hint()).clone();
            config.action = action;
            let hint = Rc::new(config);
            let mut state = HintState::new("abc".to_string());
            state.start(hint.clone());
            state.matches = ["foo", "bar", "foo"]
                .into_iter()
                .enumerate()
                .map(|(col, text)| hint_match(text, col, &hint))
                .collect();

            state.generate_labels();

            state.labels.into_iter().flatten().collect::<String>()
        };

        for action in [Copy, Paste, Open] {
            assert_eq!(labels_for(Action { action }), "aba");
        }
        assert_eq!(
            labels_for(Command {
                command: HintCommand::Simple("open".into()),
            }),
            "aba"
        );
        for action in [Select, MoveViModeCursor] {
            assert_eq!(labels_for(Action { action }), "abc");
        }
    }

    #[test]
    fn test_resolve_path_skips_scheme_urls() {
        assert!(resolve_path_for_opening("https://example.com", None).is_none());
        assert!(resolve_path_for_opening("mailto:a@b.c", None).is_none());
        assert!(resolve_path_for_opening("file:///tmp", None).is_none());
        assert!(resolve_path_for_opening("ssh://host/path", None).is_none());
    }

    #[test]
    fn test_resolve_path_returns_none_when_nonexistent() {
        let cwd = std::env::temp_dir();
        assert!(resolve_path_for_opening(
            "rio-definitely-does-not-exist-xyz",
            Some(&cwd)
        )
        .is_none());
        assert!(resolve_path_for_opening(
            "./rio-definitely-does-not-exist-xyz",
            Some(&cwd)
        )
        .is_none());
    }

    #[test]
    fn test_resolve_path_absolute_existing_file() {
        let tmp = std::env::temp_dir();
        let file = tmp.join("rio-test-resolve-abs.txt");
        std::fs::write(&file, "hi").unwrap();

        let resolved = resolve_path_for_opening(&file.to_string_lossy(), None).unwrap();
        // PathBuf::exists() follows symlinks; on macOS /tmp is a symlink to
        // /private/tmp, so compare existence rather than exact paths.
        assert!(resolved.exists());

        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn test_resolve_path_relative_joined_with_cwd() {
        let tmp = std::env::temp_dir();
        let subdir = tmp.join("rio-test-resolve-dir");
        std::fs::create_dir_all(&subdir).unwrap();
        let file = subdir.join("child.txt");
        std::fs::write(&file, "hi").unwrap();

        let resolved = resolve_path_for_opening("child.txt", Some(&subdir)).unwrap();
        assert!(resolved.exists());
        assert!(resolved.ends_with("child.txt"));

        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&subdir);
    }

    #[test]
    fn test_resolve_path_dot_relative_joined_with_cwd() {
        let tmp = std::env::temp_dir();
        let subdir = tmp.join("rio-test-resolve-dot-dir");
        std::fs::create_dir_all(&subdir).unwrap();
        let file = subdir.join("dot-child.txt");
        std::fs::write(&file, "hi").unwrap();

        let resolved =
            resolve_path_for_opening("./dot-child.txt", Some(&subdir)).unwrap();
        assert!(resolved.exists());

        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&subdir);
    }

    #[test]
    fn test_resolve_path_requires_cwd_for_relative() {
        // With no cwd and a relative path, we can't resolve; return None.
        assert!(resolve_path_for_opening("foo/bar.txt", None).is_none());
    }

    // -----------------------------------------------------------------
    // Regression tests for issue #1619.
    //
    // The bug: regex hint matching mapped onig's byte offsets directly
    // to grid columns, so any wide (display-width-2) glyph or multibyte
    // codepoint *before* a URL on the same line shifted the underline
    // and click hit-box right by (byte_count - cell_count) cells. On a
    // line like "😀 https://example.com" the underline started a few
    // cells inside the URL, and clicking the visible URL landed in the
    // gap and did nothing.
    //
    // These tests build small terminal grids, run `find_regex_matches`
    // against the default URL regex, and assert that the resulting
    // (start_col, end_col) are the *grid columns* of the visible URL's
    // first and last cells.
    // -----------------------------------------------------------------
    use rio_backend::ansi::CursorShape;
    use rio_backend::config::hints::DEFAULT_URL_REGEX;
    use rio_backend::crosswords::square::Wide;
    use rio_backend::crosswords::Crosswords;
    use rio_backend::crosswords::CrosswordsSize;
    use rio_backend::event::{VoidListener, WindowId};
    use unicode_width::UnicodeWidthChar;

    /// Build a tiny `Crosswords` whose first row contains `content`.
    /// Mirrors `rio_backend::crosswords::search::tests::mock_term` —
    /// duplicated here because that helper is `pub(crate)` to its own
    /// crate. Wide glyphs occupy two cells (`Wide` + `Spacer`) just
    /// like a real PTY write would produce.
    fn mock_term_with_line(content: &str) -> Crosswords<VoidListener> {
        let num_cols: usize = content.chars().map(|c| c.width().unwrap_or(1)).sum();
        // Always leave at least a couple of trailing cells so callers
        // can append more text in a future iteration without resizing.
        let num_cols = num_cols.max(1) + 4;
        let size = CrosswordsSize::new(num_cols, 1);
        let window_id = WindowId::from(0);
        let mut term = Crosswords::new(
            size,
            CursorShape::Block,
            VoidListener {},
            window_id,
            0,
            10_000,
        );

        let line = Line(0);
        let mut col = 0usize;
        for c in content.chars() {
            term.grid[line][Column(col)].set_c(c);
            let width = c.width().unwrap_or(1);
            if width == 2 {
                term.grid[line][Column(col)].set_wide(Wide::Wide);
                term.grid[line][Column(col + 1)].set_c(' ');
                term.grid[line][Column(col + 1)].set_wide(Wide::Spacer);
            }
            col += width.max(1);
        }
        term
    }

    /// Run the URL regex against the first line of `term` and return
    /// every (start_col, end_col, text) match in column order.
    fn url_matches(term: &Crosswords<VoidListener>) -> Vec<(usize, usize, String)> {
        let regex = onig::Regex::new(DEFAULT_URL_REGEX).expect("default regex compiles");
        let hint = Rc::new(Hint {
            regex: Some(DEFAULT_URL_REGEX.to_string()),
            hyperlinks: false,
            post_processing: true,
            persist: false,
            action: HintAction::Action {
                action: HintInternalAction::Copy,
            },
            mouse: Default::default(),
            binding: None,
        });
        let mut state = HintState::new("abc".to_string());
        state.find_regex_matches(term, &regex, hint);
        let mut out: Vec<(usize, usize, String)> = state
            .matches
            .into_iter()
            .map(|m| (m.start.col.0, m.end.col.0, m.text))
            .collect();
        out.sort_by_key(|(s, _, _)| *s);
        out
    }

    // The visible-cell column of every byte in `content`'s display
    // layout, used to derive expected start/end columns from a literal
    // substring. A simple model: walk chars, assign each char's bytes
    // to the cell it starts in.
    fn col_of_substr(content: &str, needle: &str) -> (usize, usize) {
        let byte_start = content.find(needle).expect("needle in content");
        let mut col = 0usize;
        let mut byte = 0usize;
        let mut start_col = None;
        let mut end_col = 0usize;
        let needle_end = byte_start + needle.len();
        for c in content.chars() {
            let len = c.len_utf8();
            if start_col.is_none() && byte >= byte_start {
                start_col = Some(col);
            }
            if byte < needle_end {
                let width = c.width().unwrap_or(1).max(1);
                end_col = col + width - 1;
            }
            byte += len;
            col += c.width().unwrap_or(1).max(1);
        }
        (start_col.expect("needle found"), end_col)
    }

    fn assert_single_url(content: &str, expected_url: &str) {
        let term = mock_term_with_line(content);
        let matches = url_matches(&term);
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one URL match for line {:?}, got {:?}",
            content,
            matches
        );
        let (start_col, end_col, text) = &matches[0];
        let (expected_start, expected_end) = col_of_substr(content, expected_url);
        assert_eq!(
            text, expected_url,
            "matched text mismatch for line {:?}",
            content
        );
        assert_eq!(
            *start_col, expected_start,
            "start column mismatch for line {:?}: got {}, expected {}",
            content, start_col, expected_start
        );
        assert_eq!(
            *end_col, expected_end,
            "end column mismatch for line {:?}: got {}, expected {}",
            content, end_col, expected_end
        );
    }

    #[test]
    fn issue_1619_url_hitboxes_follow_visible_columns() {
        for (content, url) in [
            ("https://example.com", "https://example.com"),
            ("ab https://example.com/ascii", "https://example.com/ascii"),
            ("😀 https://example.com/emoji", "https://example.com/emoji"),
            ("世界 https://example.com/cjk", "https://example.com/cjk"),
            ("⏺ https://linear.app/ENG-993", "https://linear.app/ENG-993"),
            (
                "🎉🎉🎉 https://example.com/party",
                "https://example.com/party",
            ),
            ("a😀b⏺c https://example.com/mix", "https://example.com/mix"),
            ("(😀 https://example.com/end)", "https://example.com/end"),
            ("see https://example.com/page.", "https://example.com/page"),
        ] {
            assert_single_url(content, url);
        }
    }

    #[test]
    fn test_resolve_path_expands_env_var() {
        let tmp = std::env::temp_dir();
        // Safety: setting an env var inside a process-local test. This is
        // unsafe in Rust 2024; rio-backend uses an earlier edition so it's
        // permitted here. If rio moves to 2024 this test needs adjustment.
        unsafe {
            std::env::set_var("RIO_TEST_PATH_VAR", tmp.to_string_lossy().to_string());
        }

        let file = tmp.join("rio-test-env-var.txt");
        std::fs::write(&file, "hi").unwrap();

        let resolved =
            resolve_path_for_opening("$RIO_TEST_PATH_VAR/rio-test-env-var.txt", None)
                .unwrap();
        assert!(resolved.exists());

        let _ = std::fs::remove_file(&file);
        unsafe {
            std::env::remove_var("RIO_TEST_PATH_VAR");
        }
    }
}
