use super::*;

impl App {
    pub fn copy_visual_selection(&mut self) -> Result<usize> {
        let Some(sel) = self.visual_selection else {
            return Ok(0);
        };
        let (start, end) = sel.ordered();
        let side = sel.anchor.side;
        let mut out = String::new();
        let mut emitted = 0usize;
        for idx in start.annotation_idx..=end.annotation_idx {
            let snippet = if let Some(content) = self.content_for_side(idx, side) {
                let total = content.chars().count();
                let (lo, hi) = sel.char_range(idx, total);
                char_slice(content, lo, Some(hi)).to_string()
            } else if let Some(text) = self.atomic_text_for_annotation(idx) {
                text
            } else {
                continue;
            };
            if emitted > 0 {
                out.push('\n');
            }
            out.push_str(&snippet);
            emitted += 1;
        }
        if out.is_empty() {
            return Ok(0);
        }
        let count = out.chars().count();
        crate::output::copy_text_to_clipboard(&out)
            .map_err(|e| TuicrError::Clipboard(format!("{e}")))?;
        Ok(count)
    }

    pub fn enter_visual_mode_at_cursor(&mut self) {
        let idx = self.diff_state.cursor_line;
        let side = self
            .get_line_at_cursor()
            .map(|(_, s)| s)
            .unwrap_or(LineSide::New);
        let len = self.annotation_content_len(idx, side);
        let anchor = SelPoint {
            annotation_idx: idx,
            char_offset: 0,
            side,
        };
        let head = SelPoint {
            annotation_idx: idx,
            char_offset: len,
            side,
        };
        self.input_mode = InputMode::VisualSelect;
        self.visual_selection = Some(VisualSelection {
            anchor,
            head,
            kind: VisualKind::Line,
        });
    }

    /// `v`: start a character-wise visual selection on the character under
    /// the cursor (`V` selects whole lines). Falls back to the line-wise
    /// selection on lines without diff content.
    pub fn enter_visual_char_mode_at_cursor(&mut self) {
        let Some((side, col)) = self.diff_cursor_target() else {
            self.enter_visual_mode_at_cursor();
            return;
        };
        let idx = self.diff_state.cursor_line;
        self.input_mode = InputMode::VisualSelect;
        self.visual_selection = Some(VisualSelection {
            anchor: SelPoint {
                annotation_idx: idx,
                char_offset: col,
                side,
            },
            head: SelPoint {
                annotation_idx: idx,
                char_offset: col + 1,
                side,
            },
            kind: VisualKind::Char,
        });
    }

    /// `h` / `l` in visual mode: move the selection head `n` characters,
    /// clamped to the head's line. Shrinks when the head moves back toward
    /// the anchor — `ordered()` keeps the range well-formed either way.
    pub fn extend_visual_by_char(&mut self, forward: bool, n: usize) {
        let Some(sel) = self.visual_selection else {
            return;
        };
        let len = self.annotation_content_len(sel.head.annotation_idx, sel.anchor.side);
        let off = sel.head.char_offset.min(len);
        let new_off = if forward {
            (off + n).min(len)
        } else {
            off.saturating_sub(n)
        };
        if let Some(sel) = &mut self.visual_selection {
            sel.head.char_offset = new_off;
        }
        self.diff_state.cursor_col = new_off.min(len.saturating_sub(1));
    }

    /// `w` / `b` in visual mode: move the selection head to the next /
    /// previous word start, crossing lines within the anchor's side, exactly
    /// like the normal-mode cursor motions.
    pub fn extend_visual_by_word(&mut self, forward: bool) {
        let Some(sel) = self.visual_selection else {
            return;
        };
        let side = sel.anchor.side;
        let head = sel.head;
        let candidates: Box<dyn Iterator<Item = usize>> = if forward {
            Box::new(head.annotation_idx..self.total_lines())
        } else {
            Box::new((0..=head.annotation_idx).rev())
        };
        for idx in candidates {
            let Some(content) = self.content_for_side(idx, side) else {
                continue;
            };
            let spans = super::search::word_spans(content);
            let on_head_line = idx == head.annotation_idx;
            // Past the head's line, any word qualifies (first / last one).
            let char_offset = match (forward, on_head_line) {
                (true, true) => spans
                    .iter()
                    .find(|&&(start, _)| start > head.char_offset)
                    .map(|&(start, _)| start),
                (true, false) => spans.first().map(|&(start, _)| start),
                (false, true) => spans
                    .iter()
                    .rev()
                    .find(|&&(start, _)| start < head.char_offset)
                    .map(|&(start, _)| start),
                (false, false) => spans.last().map(|&(start, _)| start),
            };
            if let Some(char_offset) = char_offset {
                if let Some(sel) = &mut self.visual_selection {
                    sel.head = SelPoint {
                        annotation_idx: idx,
                        char_offset,
                        side,
                    };
                }
                self.diff_state.cursor_line = idx;
                self.diff_state.cursor_col = char_offset;
                self.ensure_cursor_visible();
                self.update_current_file_from_cursor();
                return;
            }
        }
    }

    pub fn exit_visual_mode(&mut self) {
        self.input_mode = InputMode::Normal;
        self.visual_selection = None;
    }

    pub fn get_visual_selection(&self) -> Option<&VisualSelection> {
        if self.input_mode != InputMode::VisualSelect {
            return None;
        }
        self.visual_selection.as_ref()
    }

    pub fn annotation_content_len(&self, idx: usize, side: LineSide) -> usize {
        self.content_for_side(idx, side)
            .map(|s| s.chars().count())
            .unwrap_or(0)
    }

    pub fn extend_visual_to_cursor(&mut self) {
        let Some(sel) = self.visual_selection else {
            return;
        };
        let cursor_idx = self.diff_state.cursor_line;
        let side = sel.anchor.side;

        // Char-wise (`v`): the head follows the cursor line keeping its
        // column, like an editor cursor moving vertically.
        if sel.kind == VisualKind::Char {
            let len = self.annotation_content_len(cursor_idx, side);
            if let Some(sel) = &mut self.visual_selection {
                sel.head = SelPoint {
                    annotation_idx: cursor_idx,
                    char_offset: sel.head.char_offset.min(len),
                    side,
                };
            }
            return;
        }

        let anchor_idx = sel.anchor.annotation_idx;
        let anchor_len = self.annotation_content_len(anchor_idx, side);
        let cursor_len = self.annotation_content_len(cursor_idx, side);
        let (anchor_char, head_char) = if cursor_idx >= anchor_idx {
            (0, cursor_len)
        } else {
            (anchor_len, 0)
        };
        self.visual_selection = Some(VisualSelection {
            anchor: SelPoint {
                annotation_idx: anchor_idx,
                char_offset: anchor_char,
                side,
            },
            head: SelPoint {
                annotation_idx: cursor_idx,
                char_offset: head_char,
                side,
            },
            kind: VisualKind::Line,
        });
    }

    pub fn visual_selection_line_range(&self) -> Option<(LineRange, LineSide)> {
        let sel = self.get_visual_selection()?;
        let (start, end) = sel.ordered();
        let start_line = self.annotation_line_for_side(start.annotation_idx, start.side);
        let end_line = self.annotation_line_for_side(end.annotation_idx, end.side);
        let start_ln = start_line?;
        let end_ln = end_line?;
        Some((LineRange::new(start_ln, end_ln), start.side))
    }

    fn annotation_line_for_side(&self, idx: usize, side: LineSide) -> Option<u32> {
        match self.line_annotations.get(idx)? {
            AnnotatedLine::DiffLine {
                old_lineno,
                new_lineno,
                ..
            }
            | AnnotatedLine::SideBySideLine {
                old_lineno,
                new_lineno,
                ..
            } => match side {
                LineSide::New => *new_lineno,
                LineSide::Old => *old_lineno,
            },
            _ => None,
        }
    }

    pub fn enter_comment_from_visual(&mut self) {
        if let Some((range, side)) = self.visual_selection_line_range() {
            self.comment_line_range = Some((range, side));
            self.comment_line = Some((range.end, side));
            self.input_mode = InputMode::Comment;
            if self.diff_view_mode != DiffViewMode::SideBySide {
                self.diff_state.scroll_x = 0;
            }
            self.comment_buffer.clear();
            self.comment_cursor = 0;
            self.comment_type = self.default_comment_type();
            self.comment_is_review_level = false;
            self.comment_is_file_level = false;
            self.visual_selection = None;
        } else {
            self.set_warning("Invalid visual selection");
            self.exit_visual_mode();
        }
    }
}
