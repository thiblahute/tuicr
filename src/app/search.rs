use super::*;
use crate::ui::text_utils::{contains_fold, fold_for_search};
use std::borrow::Cow;

fn find_search_match(
    total_lines: usize,
    start_idx: usize,
    forward: bool,
    include_current: bool,
    pattern: &str,
    mut line_text: impl FnMut(usize) -> Option<String>,
) -> Option<usize> {
    if total_lines == 0 {
        return None;
    }

    let normalized_pattern = pattern.to_lowercase();
    let mut matches = |line_idx| {
        line_text(line_idx).is_some_and(|text| text.to_lowercase().contains(&normalized_pattern))
    };
    let start_idx = start_idx.min(total_lines - 1);
    if forward {
        let first = if include_current {
            start_idx
        } else {
            start_idx.saturating_add(1)
        };
        (first..total_lines).find(|&line_idx| matches(line_idx))
    } else {
        let first = if include_current {
            Some(start_idx)
        } else {
            start_idx.checked_sub(1)
        };
        first.and_then(|line_idx| (0..=line_idx).rev().find(|&line_idx| matches(line_idx)))
    }
}

/// True for characters that form an identifier-like word — the unit `w`/`b`
/// step over and `*`/`#` search for.
pub(crate) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `(start, end)` char ranges of the identifier-like words in `text`,
/// left to right.
pub(in crate::app) fn word_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = None;
    let mut idx = 0;
    for c in text.chars() {
        if is_word_char(c) {
            start.get_or_insert(idx);
        } else if let Some(s) = start.take() {
            spans.push((s, idx));
        }
        idx += 1;
    }
    if let Some(s) = start {
        spans.push((s, idx));
    }
    spans
}

fn span_text(text: &str, span: (usize, usize)) -> String {
    text.chars().skip(span.0).take(span.1 - span.0).collect()
}

/// The word the cursor column sits on, or failing that the next word after it
/// (vim's `*` rule), or failing that the last word on the line (a sticky
/// column carried over from a longer line clamps like an editor cursor).
pub(in crate::app) fn word_at_or_after(text: &str, col: usize) -> Option<(usize, usize, String)> {
    let spans = word_spans(text);
    let &(start, end) = spans.iter().find(|&&(_, end)| col < end).or(spans.last())?;
    Some((start, end, span_text(text, (start, end))))
}

/// Char index of the first case-insensitive occurrence of `pattern` in `text`.
/// Folds each char to its first lowercase form, mirroring the render-side
/// matcher, so indices stay aligned with the original text.
fn find_char_ci(text: &str, pattern: &str) -> Option<usize> {
    let lower1 = |c: char| c.to_lowercase().next().unwrap_or(c);
    let hay: Vec<char> = text.chars().map(lower1).collect();
    let pat: Vec<char> = pattern.chars().map(lower1).collect();
    if pat.is_empty() || hay.len() < pat.len() {
        return None;
    }
    (0..=hay.len() - pat.len()).find(|&i| hay[i..i + pat.len()] == pat[..])
}

impl HelpState {
    fn search(&mut self, pattern: &str, forward: bool, include_current: bool) -> bool {
        let start_idx = self.current_match_line.unwrap_or(self.scroll_offset);
        let Some(line) = find_search_match(
            self.searchable_lines.len(),
            start_idx,
            forward,
            include_current,
            pattern,
            |line_idx| self.searchable_lines.get(line_idx).cloned(),
        ) else {
            return false;
        };

        self.current_match_line = Some(line);
        let max_offset = self
            .searchable_lines
            .len()
            .saturating_sub(self.viewport_height);
        self.scroll_offset = line
            .saturating_sub(self.viewport_height / 2)
            .min(max_offset);
        true
    }
}

impl App {
    pub fn search_in_help_from_scroll(&mut self) -> bool {
        let pattern = self.search_buffer.clone();
        if pattern.trim().is_empty() {
            self.set_message("Search pattern is empty");
            return false;
        }

        self.help_state.last_search_pattern = Some(pattern.clone());
        self.help_state.current_match_line = None;
        if self.help_state.search(&pattern, true, true) {
            true
        } else {
            self.set_message(format!("No help matches for \"{pattern}\""));
            false
        }
    }

    pub fn search_next_in_help(&mut self) -> bool {
        let Some(pattern) = self.help_state.last_search_pattern.clone() else {
            self.set_message("No previous help search");
            return false;
        };
        if self.help_state.search(&pattern, true, false) {
            true
        } else {
            self.set_message(format!("No further help matches for \"{pattern}\""));
            false
        }
    }

    pub fn search_prev_in_help(&mut self) -> bool {
        let Some(pattern) = self.help_state.last_search_pattern.clone() else {
            self.set_message("No previous help search");
            return false;
        };
        if self.help_state.search(&pattern, false, false) {
            true
        } else {
            self.set_message(format!("No earlier help matches for \"{pattern}\""));
            false
        }
    }

    pub fn search_in_diff_from_cursor(&mut self) -> bool {
        let pattern = self.search_buffer.clone();
        if pattern.trim().is_empty() {
            self.set_message("Search pattern is empty");
            return false;
        }

        self.search_needle_lower = Some(fold_for_search(&pattern));
        self.last_search_pattern = Some(pattern);
        self.recompute_search_matches();
        if self.line_annotations.is_empty() {
            self.set_message("No diff content to search");
            return false;
        }
        self.cycle_search_match(true, true)
    }

    pub fn search_next_in_diff(&mut self) -> bool {
        if self.last_search_pattern.is_none() {
            self.set_message("No previous search");
            return false;
        }
        self.cycle_search_match(true, false)
    }

    pub fn search_prev_in_diff(&mut self) -> bool {
        if self.last_search_pattern.is_none() {
            self.set_message("No previous search");
            return false;
        }
        self.cycle_search_match(false, false)
    }

    /// The word under the cursor on the current line, as `(start, end, word)`
    /// char range within the cursor side's content.
    fn word_under_cursor(&self) -> Option<(usize, usize, String)> {
        let content = self.content_for_side(self.diff_state.cursor_line, self.cursor_side)?;
        word_at_or_after(content, self.diff_state.cursor_col)
    }

    /// `h` / `l`: move the cursor `n` characters left / right, clamped to the
    /// current line.
    pub fn move_cursor_char(&mut self, forward: bool, n: usize) {
        let Some(content) = self.content_for_side(self.diff_state.cursor_line, self.cursor_side)
        else {
            return;
        };
        let len = content.chars().count();
        if len == 0 {
            self.diff_state.cursor_col = 0;
            return;
        }
        let col = self.diff_state.cursor_col.min(len - 1);
        self.diff_state.cursor_col = if forward {
            (col + n).min(len - 1)
        } else {
            col.saturating_sub(n)
        };
    }

    /// `w` / `b`: move the cursor to the next / previous word start within
    /// the current side's text, crossing onto other lines when the current
    /// one runs out.
    pub fn move_word_cursor(&mut self, forward: bool) {
        let side = self.cursor_side;
        let cur = self.diff_state.cursor_line;
        if let Some(content) = self.content_for_side(cur, side) {
            let len = content.chars().count();
            let col = self.diff_state.cursor_col.min(len.saturating_sub(1));
            let spans = word_spans(content);
            let next = if forward {
                spans.iter().find(|&&(start, _)| start > col)
            } else {
                spans.iter().rev().find(|&&(start, _)| start < col)
            };
            if let Some(&(start, _)) = next {
                self.diff_state.cursor_col = start;
                return;
            }
        }

        // Ran out of words on this line: continue on the next line that has
        // any, like an editor's `w`/`b` crossing line boundaries.
        let candidates: Box<dyn Iterator<Item = usize>> = if forward {
            Box::new(cur + 1..self.total_lines())
        } else {
            Box::new((0..cur).rev())
        };
        for line_idx in candidates {
            let Some(content) = self.content_for_side(line_idx, side) else {
                continue;
            };
            let spans = word_spans(content);
            let span = if forward { spans.first() } else { spans.last() };
            if let Some(&(start, _)) = span {
                self.diff_state.cursor_col = start;
                self.diff_state.cursor_line = line_idx;
                self.ensure_cursor_visible();
                self.update_current_file_from_cursor();
                return;
            }
        }
        self.set_message("No more words");
    }

    /// `*` / `#`: search forward / backward for the word under the cursor.
    /// Feeds the regular search state so `n`/`N` and the match highlighting
    /// continue from it.
    pub fn search_word_under_cursor(&mut self, forward: bool) -> bool {
        let Some((start, _, word)) = self.word_under_cursor() else {
            self.set_message("No word under cursor");
            return false;
        };
        self.diff_state.cursor_col = start;
        self.search_needle_lower = Some(fold_for_search(&word));
        self.last_search_pattern = Some(word);
        self.recompute_search_matches();
        self.cycle_search_match(forward, false)
    }

    /// Where the block cursor sits for rendering: the side whose pane holds
    /// it and the clamped char index within that side's content. `None` on
    /// lines without diff content (headers, comments, spacing).
    pub fn diff_cursor_target(&self) -> Option<(LineSide, usize)> {
        let side = self.cursor_side;
        let content = self.content_for_side(self.diff_state.cursor_line, side)?;
        let len = content.chars().count();
        if len == 0 {
            return None;
        }
        Some((side, self.diff_state.cursor_col.min(len - 1)))
    }

    fn cycle_search_match(&mut self, forward: bool, include_current: bool) -> bool {
        if self.search_matches_stale {
            self.recompute_search_matches();
        }
        if self.search_matches.is_empty() {
            let pattern = self.last_search_pattern.as_deref().unwrap_or_default();
            self.set_message(format!("No matches for \"{pattern}\""));
            self.search_highlight_visible = false;
            return false;
        }

        self.search_highlight_visible = true;
        let cursor = self.diff_state.cursor_line;
        let match_idx = if forward {
            let idx = self.search_matches.partition_point(|&line| {
                if include_current {
                    line < cursor
                } else {
                    line <= cursor
                }
            });
            if idx == self.search_matches.len() {
                self.set_message("search hit BOTTOM, continuing at TOP");
                0
            } else {
                idx
            }
        } else {
            let idx = self.search_matches.partition_point(|&line| {
                if include_current {
                    line <= cursor
                } else {
                    line < cursor
                }
            });
            if idx == 0 {
                self.set_message("search hit TOP, continuing at BOTTOM");
                self.search_matches.len() - 1
            } else {
                idx - 1
            }
        };
        self.move_cursor_to_search_match(match_idx)
    }

    fn move_cursor_to_search_match(&mut self, match_idx: usize) -> bool {
        let Some(&line_idx) = self.search_matches.get(match_idx) else {
            return false;
        };
        self.diff_state.cursor_line = line_idx;
        // Land the cursor on the match like an editor. Try the current
        // side's pane first; in side-by-side the match may sit in the other
        // pane, so fall over to it (and move the caret side along).
        if let Some(pattern) = self.last_search_pattern.clone() {
            let other = match self.cursor_side {
                LineSide::Old => LineSide::New,
                LineSide::New => LineSide::Old,
            };
            for side in [self.cursor_side, other] {
                if let Some(col) = self
                    .content_for_side(line_idx, side)
                    .and_then(|content| find_char_ci(content, &pattern))
                {
                    self.diff_state.cursor_col = col;
                    if self.is_side_by_side() {
                        self.set_cursor_side(side);
                    }
                    break;
                }
            }
        }
        self.ensure_cursor_visible();
        self.center_cursor();
        self.update_current_file_from_cursor();
        true
    }

    pub(crate) fn refresh_search_matches(&mut self) {
        if self.search_highlight_visible {
            self.recompute_search_matches();
        } else {
            self.search_matches_stale = true;
        }
    }

    fn recompute_search_matches(&mut self) {
        self.search_matches_stale = false;
        let Some(needle) = self.search_needle_lower.as_deref() else {
            self.search_matches.clear();
            return;
        };
        let mut matches = Vec::new();
        let mut pr_info_lines = None;
        let mut last_thread_match: Option<(usize, bool)> = None;
        for line_idx in 0..self.line_annotations.len() {
            let matched = match self.line_annotations.get(line_idx) {
                Some(AnnotatedLine::RemoteThreadLine { thread_idx }) => match last_thread_match {
                    Some((last_idx, last_matched)) if last_idx == *thread_idx => last_matched,
                    _ => {
                        let matched = self.thread_matches_search(*thread_idx, needle);
                        last_thread_match = Some((*thread_idx, matched));
                        matched
                    }
                },
                _ => self
                    .line_text_for_search(line_idx, &mut pr_info_lines)
                    .is_some_and(|text| contains_fold(&text, needle)),
            };
            if matched {
                matches.push(line_idx);
            }
        }
        debug_assert!(matches.is_sorted());
        self.search_matches = matches;
    }

    fn thread_matches_search(&self, thread_idx: usize, needle: &str) -> bool {
        let Some(thread) = self.forge_review_threads.get(thread_idx) else {
            return false;
        };
        contains_fold(&format!("github {}", thread.path), needle)
            || thread
                .comments
                .iter()
                .any(|comment| contains_fold(&comment.body, needle))
    }

    pub fn clear_search_highlight(&mut self) {
        self.search_highlight_visible = false;
    }

    pub fn search_match_position(&self) -> Option<(usize, usize)> {
        if !self.search_highlight_visible || self.search_matches.is_empty() {
            return None;
        }
        let current = self
            .search_matches
            .partition_point(|&line| line <= self.diff_state.cursor_line)
            .max(1);
        Some((current, self.search_matches.len()))
    }

    pub fn active_search_needle(&self) -> Option<&str> {
        if !self.search_highlight_enabled
            || !self.search_highlight_visible
            || self.input_mode == InputMode::Comment
        {
            return None;
        }
        self.search_needle_lower.as_deref()
    }

    pub(crate) fn search_paint_at(&self, line_idx: usize) -> Option<&str> {
        let needle = self.active_search_needle()?;
        self.search_matches.binary_search(&line_idx).ok()?;
        Some(needle)
    }

    fn pr_info_search_lines(&self) -> Vec<String> {
        let Some(info) = self.pr_info_for_render() else {
            return Vec::new();
        };
        crate::ui::pr_info_panel::build_pr_info_lines(
            info,
            crate::ui::pr_info_panel::pr_info_content_width(self.diff_state.viewport_width),
            &self.theme,
        )
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect()
    }

    fn line_text_for_search<'a>(
        &'a self,
        line_idx: usize,
        pr_info_lines: &'a mut Option<Vec<String>>,
    ) -> Option<Cow<'a, str>> {
        match self.line_annotations.get(line_idx)? {
            AnnotatedLine::PrInfoLine { line_idx } => pr_info_lines
                .get_or_insert_with(|| self.pr_info_search_lines())
                .get(*line_idx)
                .map(|line| Cow::Borrowed(line.as_str())),
            AnnotatedLine::IssueCommentsHeader => {
                let info = self.pr_info.as_ref()?;
                Some(Cow::Owned(format!("PR #{} Comments", info.details.number)))
            }
            AnnotatedLine::IssueComment { comment_idx } => {
                let info = self.pr_info.as_ref()?;
                let comment = info.issue_comments.get(*comment_idx)?;
                Some(Cow::Borrowed(comment.body.as_str()))
            }
            AnnotatedLine::ReviewCommentsHeader => Some(Cow::Borrowed("Review comments")),
            AnnotatedLine::ReviewComment { comment_idx } => {
                let comment = self.session.review_comments.get(*comment_idx)?;
                Some(Cow::Borrowed(comment.content.as_str()))
            }
            AnnotatedLine::RemoteReviewSummaryLine { summary_idx } => {
                let summary = self.forge_review_summaries.get(*summary_idx)?;
                let author = summary.author.as_deref().unwrap_or("unknown");
                Some(Cow::Owned(format!("github @{author} {}", summary.body)))
            }
            AnnotatedLine::FileHeader { file_idx } => {
                let file = self.diff_files.get(*file_idx)?;
                Some(Cow::Owned(format!(
                    "{} [{}]",
                    file.display_path().display(),
                    file.status.as_char()
                )))
            }
            AnnotatedLine::FileComment {
                file_idx,
                comment_idx,
            } => {
                let path = self.diff_files.get(*file_idx)?.display_path();
                let review = self.session.files.get(path)?;
                let comment = review.file_comments.get(*comment_idx)?;
                Some(Cow::Borrowed(comment.content.as_str()))
            }
            AnnotatedLine::LineComment {
                file_idx,
                line,
                comment_idx,
                ..
            } => {
                let path = self.diff_files.get(*file_idx)?.display_path();
                let review = self.session.files.get(path)?;
                let comments = review.line_comments.get(line)?;
                let comment = comments.get(*comment_idx)?;
                Some(Cow::Borrowed(comment.content.as_str()))
            }
            // Expander and hidden-lines rows are collapsed-region chrome, not
            // content; their placeholder text ("... expand ...", "... N lines
            // hidden ...") must not be searchable, and the collapsed content
            // behind them isn't shown, so there's nothing to match there.
            AnnotatedLine::Expander { .. } | AnnotatedLine::HiddenLines { .. } => None,
            AnnotatedLine::ExpandedContext {
                gap_id,
                line_idx: context_idx,
            } => {
                let content = self.get_expanded_line(gap_id, *context_idx)?;
                Some(Cow::Borrowed(content.content.as_str()))
            }
            AnnotatedLine::HunkHeader { file_idx, hunk_idx } => {
                let file = self.diff_files.get(*file_idx)?;
                let hunk = file.hunks.get(*hunk_idx)?;
                Some(Cow::Borrowed(hunk.header.as_str()))
            }
            AnnotatedLine::DiffLine {
                file_idx,
                hunk_idx,
                line_idx: diff_idx,
                ..
            } => {
                let file = self.diff_files.get(*file_idx)?;
                let hunk = file.hunks.get(*hunk_idx)?;
                let line = hunk.lines.get(*diff_idx)?;
                Some(Cow::Borrowed(line.content.as_str()))
            }
            AnnotatedLine::BinaryOrEmpty { file_idx } => {
                let file = self.diff_files.get(*file_idx)?;
                if file.is_too_large {
                    Some(Cow::Borrowed("(file too large to display)"))
                } else if file.is_binary {
                    Some(Cow::Borrowed("(binary file)"))
                } else {
                    Some(Cow::Borrowed("(no changes)"))
                }
            }
            AnnotatedLine::SideBySideLine {
                file_idx,
                hunk_idx,
                del_line_idx,
                add_line_idx,
                ..
            } => {
                let file = self.diff_files.get(*file_idx)?;
                let hunk = file.hunks.get(*hunk_idx)?;

                let del_content = del_line_idx
                    .and_then(|idx| hunk.lines.get(idx))
                    .map(|l| l.content.as_str())
                    .unwrap_or("");
                let add_content = add_line_idx
                    .and_then(|idx| hunk.lines.get(idx))
                    .map(|l| l.content.as_str())
                    .unwrap_or("");
                Some(Cow::Owned(format!("{} {}", del_content, add_content)))
            }
            AnnotatedLine::RemoteThreadLine { .. }
            | AnnotatedLine::Spacing
            | AnnotatedLine::ReviewedBanner { .. } => None,
        }
    }
}

#[cfg(test)]
mod word_tests {
    use super::{find_char_ci, word_at_or_after, word_spans};

    #[test]
    fn should_split_identifier_like_words() {
        assert_eq!(
            word_spans("let foo_bar = baz(42);"),
            vec![(0, 3), (4, 11), (14, 17), (18, 20)]
        );
        assert_eq!(word_spans("  \t !!"), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn should_pick_word_at_cursor_or_next_one_like_vim_star() {
        let text = "let foo = bar;";
        // On a word: that word.
        assert_eq!(word_at_or_after(text, 5), Some((4, 7, "foo".to_string())));
        // On the space between words: the next word.
        assert_eq!(word_at_or_after(text, 3), Some((4, 7, "foo".to_string())));
        // Past every word (sticky column from a longer line): clamp to last.
        assert_eq!(
            word_at_or_after(text, 50),
            Some((10, 13, "bar".to_string()))
        );
        assert_eq!(word_at_or_after("  ", 0), None);
    }

    #[test]
    fn should_find_case_insensitive_char_index() {
        assert_eq!(find_char_ci("let Foo = 1", "foo"), Some(4));
        assert_eq!(find_char_ci("let Foo = 1", "missing"), None);
    }
}

#[cfg(test)]
mod tests {
    use super::HelpState;

    fn help_state() -> HelpState {
        HelpState {
            viewport_height: 5,
            searchable_lines: vec![
                "Navigation".to_string(),
                "Scroll down/up".to_string(),
                "Review actions".to_string(),
                "Add line comment".to_string(),
                "Commands".to_string(),
                "Reload comments".to_string(),
                "Toggle this help".to_string(),
            ],
            ..HelpState::default()
        }
    }

    #[test]
    fn should_find_help_text_case_insensitively_and_center_it_in_the_viewport() {
        let mut state = help_state();

        assert!(state.search("COMMENT", true, true));
        assert_eq!(state.current_match_line, Some(3));
        assert_eq!(state.scroll_offset, 1);
    }

    #[test]
    fn should_move_to_next_and_previous_help_matches() {
        let mut state = help_state();
        assert!(state.search("comment", true, true));

        assert!(state.search("comment", true, false));
        assert_eq!(state.current_match_line, Some(5));

        assert!(state.search("comment", false, false));
        assert_eq!(state.current_match_line, Some(3));
    }

    #[test]
    fn should_pan_help_right_only_until_the_widest_line_ends() {
        let mut state = help_state();
        state.viewport_width = 20;
        state.max_line_width = 30;

        state.scroll_right(4);
        assert_eq!(state.horizontal_offset, 4);

        state.scroll_right(40);
        assert_eq!(state.horizontal_offset, 10);
    }

    #[test]
    fn should_not_pan_help_when_every_line_fits() {
        let mut state = help_state();
        state.viewport_width = 40;
        state.max_line_width = 30;

        state.scroll_right(4);

        assert_eq!(state.horizontal_offset, 0);
    }

    #[test]
    fn should_pan_help_back_to_the_left_edge() {
        let mut state = help_state();
        state.viewport_width = 20;
        state.max_line_width = 30;
        state.horizontal_offset = 8;

        state.scroll_left(4);
        assert_eq!(state.horizontal_offset, 4);

        state.scroll_left(40);
        assert_eq!(state.horizontal_offset, 0);
    }

    #[test]
    fn should_keep_the_current_help_position_when_no_match_exists() {
        let mut state = help_state();
        state.scroll_offset = 2;

        assert!(!state.search("missing", true, true));
        assert_eq!(state.current_match_line, None);
        assert_eq!(state.scroll_offset, 2);
    }
}
