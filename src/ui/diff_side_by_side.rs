use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    App, DiffSource, ExpandDirection, FocusedPanel, GAP_EXPAND_BATCH, GapId, InputMode,
};
use crate::model::{DiffLine, FileStatus, LineOrigin, LineRange, LineSide};
use crate::theme::Theme;
use crate::ui::comment_panel;
use crate::ui::diff_view::{
    CommentBoxRow, apply_horizontal_scroll, comment_box_row, comment_type_presentation,
    cursor_indicator, cursor_indicator_spaced, diff_stat_title, hunk_header_text_and_style,
    paint_cursor_line_highlight, paint_visual_selection_overlay, populate_row_to_annotation,
    render_expander_line, render_hidden_lines, scroll_comment_input_into_view, skip_comment_box,
};
use crate::ui::styles;
use crate::ui::text_utils::{
    apply_search_highlight_pairs, apply_search_highlight_spans, apply_search_highlight_text,
    truncate_or_pad, truncate_or_pad_pairs_by_chars, truncate_or_pad_spans, wrap_spans,
};
use crate::vcs::git::calculate_gap;

#[derive(Clone, Default)]
struct SbsRowMeta {
    left_content: Vec<Span<'static>>,
    right_content: Vec<Span<'static>>,
    left_prefix: Vec<Span<'static>>,
    right_prefix: Vec<Span<'static>>,
    left_pad_style: Style,
    right_pad_style: Style,
}

fn content_spans_for_diff_line(
    theme: &Theme,
    dl: &DiffLine,
    origin: LineOrigin,
    search: Option<(&str, Style)>,
) -> Vec<Span<'static>> {
    let base = match origin {
        LineOrigin::Context => styles::diff_context_style(theme),
        LineOrigin::Addition => styles::diff_add_style(theme),
        LineOrigin::Deletion => styles::diff_del_style(theme),
    };
    let spans: Vec<Span<'static>> = if let Some(ref h) = dl.highlighted_spans {
        h.iter().map(|(s, t)| Span::styled(t.clone(), *s)).collect()
    } else {
        vec![Span::styled(dl.content.clone(), base)]
    };
    match search {
        Some((needle, hl)) => apply_search_highlight_spans(spans, needle, hl),
        None => spans,
    }
}

fn searched_cell_spans(
    pairs: &[(Style, String)],
    width: usize,
    pad_style: Style,
    search: Option<(&str, Style)>,
) -> Vec<Span<'static>> {
    if let Some((needle, hl)) = search
        && let Some(highlighted) = apply_search_highlight_pairs(pairs, needle, hl)
    {
        return truncate_or_pad_spans(&highlighted, width, pad_style);
    }
    truncate_or_pad_spans(pairs, width, pad_style)
}

fn plain_cell_spans(
    content: &str,
    style: Style,
    width: usize,
    search: Option<(&str, Style)>,
) -> Vec<Span<'static>> {
    if let Some((needle, hl)) = search
        && let Some(highlighted) = apply_search_highlight_text(content, style, needle, hl)
    {
        return truncate_or_pad_pairs_by_chars(&highlighted, width, style);
    }
    vec![Span::styled(truncate_or_pad(content, width), style)]
}

fn column_pad_style(theme: &Theme, dl: &DiffLine, origin: LineOrigin) -> Style {
    match origin {
        LineOrigin::Context => styles::diff_context_style(theme),
        LineOrigin::Addition => {
            if dl.highlighted_spans.is_some() {
                Style::default().fg(theme.diff_add).bg(theme.syntax_add_bg)
            } else {
                styles::diff_add_style(theme)
            }
        }
        LineOrigin::Deletion => {
            if dl.highlighted_spans.is_some() {
                Style::default().fg(theme.diff_del).bg(theme.syntax_del_bg)
            } else {
                styles::diff_del_style(theme)
            }
        }
    }
}

fn pad_spans_to_width(
    mut spans: Vec<Span<'static>>,
    width: usize,
    pad_style: Style,
) -> Vec<Span<'static>> {
    let cur: usize = spans.iter().map(|s| s.content.width()).sum();
    if cur < width {
        spans.push(Span::styled(" ".repeat(width - cur), pad_style));
    }
    spans
}

fn pan_spans(
    spans: &[Span<'static>],
    scroll_x: usize,
    width: usize,
    pad_style: Style,
) -> Vec<Span<'static>> {
    let mut skipped = 0;
    let mut visible_width = 0;
    let mut visible = Vec::new();
    for span in spans {
        let mut text = String::new();
        for ch in span.content.chars() {
            let ch_width = ch.width().unwrap_or(if ch == '\t' { 1 } else { 0 });
            if skipped < scroll_x {
                skipped += ch_width;
            } else if visible_width + ch_width <= width {
                text.push(ch);
                visible_width += ch_width;
            } else {
                if !text.is_empty() {
                    visible.push(Span::styled(text, span.style));
                }
                return pad_spans_to_width(visible, width, pad_style);
            }
        }
        if !text.is_empty() {
            visible.push(Span::styled(text, span.style));
        }
        if visible_width == width {
            break;
        }
    }
    pad_spans_to_width(visible, width, pad_style)
}

fn pan_sbs_row(meta: &SbsRowMeta, scroll_x: usize, content_width: usize) -> Line<'static> {
    let mut spans = meta.left_prefix.clone();
    spans.extend(pan_spans(
        &meta.left_content,
        scroll_x,
        content_width,
        meta.left_pad_style,
    ));
    spans.extend(meta.right_prefix.clone());
    spans.extend(pan_spans(
        &meta.right_content,
        scroll_x,
        content_width,
        meta.right_pad_style,
    ));
    Line::from(spans)
}

struct SideSpec {
    lineno: Option<u32>,
    marker: &'static str,
    marker_style: Style,
}

fn sbs_row_prefixes(
    theme: &Theme,
    indicator: &'static str,
    left: SideSpec,
    right: SideSpec,
    lw: usize,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let dim = styles::dim_style(theme);
    let old_num = left
        .lineno
        .map(|n| format!("{n:>lw$}"))
        .unwrap_or_else(|| " ".repeat(lw));
    let new_num = right
        .lineno
        .map(|n| format!("{n:>lw$}"))
        .unwrap_or_else(|| " ".repeat(lw));

    let left_prefix = vec![
        Span::styled(indicator, styles::current_line_indicator_style(theme)),
        Span::styled(format!("{old_num} "), dim),
        Span::styled(left.marker.to_string(), left.marker_style),
    ];
    let right_prefix = vec![
        Span::styled(" │ ", dim),
        Span::styled(format!("{new_num} "), dim),
        Span::styled(right.marker.to_string(), right.marker_style),
    ];
    (left_prefix, right_prefix)
}

/// Move the `▶` caret onto the active side for the cursor row. The per-line
/// builders always place the caret in the far-left slot (old side). When the
/// cursor's effective side is `New`, blank that slot and redraw the caret in
/// the divider's trailing space, just left of the new line number, so it
/// points at the side a comment would attach to. Run as a post-render overlay
/// so it's independent of the wrapped/unwrapped row-build paths.
fn paint_sbs_active_side_caret(
    frame: &mut Frame,
    inner: Rect,
    app: &App,
    lw: usize,
    content_width: usize,
    row_heights: &[usize],
) {
    // Only the New side needs moving; Old keeps the left-slot caret.
    if !matches!(app.get_line_at_cursor(), Some((_, LineSide::New))) {
        return;
    }
    // The commit-message entry renders full-width in the right column with no
    // divider, so keep its caret in the far-left slot where the row builder
    // placed it.
    if app
        .diff_files
        .get(app.diff_state.current_file_idx)
        .is_some_and(|f| f.is_commit_message)
    {
        return;
    }
    let scroll_offset = app.diff_state.scroll_offset;
    let cursor_line = app.diff_state.cursor_line;
    if cursor_line < scroll_offset {
        return;
    }
    let logical_offset = cursor_line - scroll_offset;
    if logical_offset >= app.diff_state.visible_line_count.max(1) {
        return;
    }
    // First visual row of the cursor's logical line (wrap-aware).
    let visual_row: u16 = if app.diff_state.wrap_lines {
        (0..logical_offset)
            .map(|i| row_heights.get(i).copied().unwrap_or(1) as u16)
            .sum()
    } else {
        logical_offset as u16
    };
    if visual_row >= inner.height {
        return;
    }
    let y = inner.y + visual_row;
    let style = styles::current_line_indicator_style(&app.theme);

    // Blank the far-left caret slot.
    frame.buffer_mut()[(inner.x, y)].set_char(' ');

    // Caret goes in the divider's trailing space: after the left gutter
    // (`sbs_left_gutter`) and the left content column, `" │ "` occupies three
    // cells and the third (index +2) is the space just left of the new lineno.
    let caret_x = inner.x + crate::app::sbs_left_gutter(lw) + content_width as u16 + 2;
    if caret_x < inner.x + inner.width {
        let cell = &mut frame.buffer_mut()[(caret_x, y)];
        cell.set_char('▶');
        if let Some(fg) = style.fg {
            cell.set_fg(fg);
        }
    }
}

/// A side comment box narrower than this (inner content columns) is not worth
/// splitting; fall back to a full-width box.
const MIN_SIDE_BOX_WIDTH: usize = 24;

/// The leading indent (`BORDER_PREFIX`'s 4 spaces) stripped from side-box rows
/// so the box aligns with the pane's line-number column.
const SIDE_BOX_INDENT_STRIP: &str = "    ";
const SIDE_BOX_INDENT_WIDTH: usize = 4;

/// Column geometry for a side-scoped comment box (all relative to `inner.x`):
/// `left_pad` spaces are inserted after the cursor indicator to shift the box
/// under `side`'s pane, `format_width` is handed to `format_comment_*` so its
/// content wraps to the pane, and `right_col` is where the box's own right
/// border is drawn. Returns `None` (use a full-width box) when the pane is too
/// narrow to be worth splitting.
pub(crate) fn sbs_side_box_geometry(
    side: LineSide,
    lw: usize,
    content_width: usize,
    panel_width: usize,
) -> Option<(u16, usize, u16)> {
    let left_gutter = crate::app::sbs_left_gutter(lw) as usize;
    // Column of the divider's leading space (` │ ` starts here).
    let left_region_end = left_gutter + content_width;
    let (left_pad, right_col) = match side {
        // Left pane: no offset, right border just before the divider.
        LineSide::Old => (0usize, left_region_end.saturating_sub(1)),
        // Right pane: pad past the left pane and the ` │ ` divider; right
        // border at the viewport edge.
        LineSide::New => (left_region_end + 2, panel_width.saturating_sub(1)),
    };
    let box_left = 1 + left_pad; // the indicator occupies column 0
    // One less than the box span so the filled top/bottom rule stops one cell
    // short and leaves room for the corner glyph we stamp at `right_col`.
    let format_width = right_col.checked_sub(box_left)?;
    if format_width.saturating_sub(9) < MIN_SIDE_BOX_WIDTH {
        return None;
    }
    Some((left_pad as u16, format_width, right_col as u16))
}

/// Push a side-scoped comment box: shift it under `side`'s pane, draw its own
/// right border (so the shared full-width overlay skips it), and record its
/// rows in `side_box_rows`. Returns the next `line_idx`.
fn push_side_comment_box<'a>(
    ctx: &SideBySideContext,
    lines: &mut Vec<Line<'a>>,
    box_lines: Vec<Line<'a>>,
    left_pad: u16,
    right_col: u16,
    mut line_idx: usize,
) -> usize {
    for line in box_lines {
        let kind = comment_box_row(&line);
        let border_fg = line.spans.first().and_then(|s| s.style.fg);
        let mut spans = line.spans;
        // Drop the 4-space `BORDER_PREFIX` indent (it reserves room for the
        // connector bar, which side boxes don't draw) so the box border aligns
        // with the pane's line-number column instead of sitting further right.
        if let Some(first) = spans.first_mut()
            && let Some(rest) = first.content.strip_prefix(SIDE_BOX_INDENT_STRIP)
        {
            first.content = rest.to_string().into();
        }
        if left_pad > 0 {
            spans.insert(
                0,
                Span::styled(" ".repeat(left_pad as usize), Style::default()),
            );
        }
        let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
        spans.insert(
            0,
            Span::styled(indicator, styles::current_line_indicator_style(ctx.theme)),
        );

        // Draw the box's own right border at `right_col`.
        if let Some(kind) = kind {
            let (fill_ch, glyph) = match kind {
                CommentBoxRow::Top => ('─', '╮'),
                CommentBoxRow::Divider => ('─', '┤'),
                CommentBoxRow::Bottom => ('─', '╯'),
                CommentBoxRow::Middle => (' ', '│'),
            };
            let style = Style::default().fg(border_fg.unwrap_or(ctx.theme.fg_primary));
            let cur_w: usize = spans.iter().map(|s| s.content.width()).sum();
            let right = right_col as usize;
            if cur_w <= right {
                let fill = right - cur_w;
                if fill > 0 {
                    spans.push(Span::styled(fill_ch.to_string().repeat(fill), style));
                }
                spans.push(Span::styled(glyph.to_string(), style));
            }
        }

        lines.push(Line::from(spans));
        ctx.side_box_rows.borrow_mut().insert(line_idx);
        line_idx += 1;
    }
    line_idx
}

/// Push comment-box rows, sizing them to the active side's pane when
/// `side_geom` is `Some`, or full-width (with the connector bar) otherwise.
#[allow(clippy::too_many_arguments)]
fn push_comment_box_lines<'a>(
    ctx: &SideBySideContext,
    lines: &mut Vec<Line<'a>>,
    box_lines: Vec<Line<'a>>,
    side_geom: Option<(u16, usize, u16)>,
    is_commit_message: bool,
    box_top_row: usize,
    line_range: Option<LineRange>,
    line_idx: usize,
) -> usize {
    if let Some((left_pad, _, right_col)) = side_geom {
        // Self-contained side box; skip the connector bar (it would sit under
        // the wrong pane).
        push_side_comment_box(ctx, lines, box_lines, left_pad, right_col, line_idx)
    } else {
        let mut line_idx = line_idx;
        for mut line in box_lines {
            // The commit message renders full-width near the left edge, so the
            // box hangs flush-left too: drop the 4-space indent that otherwise
            // reserves room for the connector bar (which we skip below, exactly
            // as side boxes do).
            if is_commit_message
                && let Some(first) = line.spans.first_mut()
                && let Some(rest) = first.content.strip_prefix(SIDE_BOX_INDENT_STRIP)
            {
                first.content = rest.to_string().into();
            }
            let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
            line.spans.insert(
                0,
                Span::styled(indicator, styles::current_line_indicator_style(ctx.theme)),
            );
            lines.push(line);
            line_idx += 1;
        }
        // A full-width box sitting directly under the commit-message line needs
        // no connector bar — and the bar's fixed gutter column would land on the
        // prose and hide a character.
        if !is_commit_message {
            crate::ui::diff_view::push_comment_bar(
                &mut ctx.comment_bars.borrow_mut(),
                box_top_row,
                line_range,
            );
        }
        line_idx
    }
}

/// Continuation-row prefixes shared by every wrapped line: blank in place of
/// the line numbers (same width, so columns stay aligned) with the center
/// divider preserved.
fn sbs_blank_prefixes(theme: &Theme, lw: usize) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let dim = styles::dim_style(theme);
    let left = vec![Span::styled(" ".repeat(lw + 3), Style::default())];
    let right = vec![
        Span::styled(" │ ", dim),
        Span::styled(" ".repeat(lw + 2), Style::default()),
    ];
    (left, right)
}

/// Cursor info for the inline comment input box in side-by-side view:
/// (cursor_logical_line, cursor_column, box_start_line, box_end_line)
type SideBySideCursorInfo = (usize, u16, usize, usize, usize);

/// Context for rendering side-by-side diff lines
struct SideBySideContext<'a> {
    app: &'a App,
    theme: &'a Theme,
    content_width: usize,
    panel_width: usize,
    current_line_idx: usize,
    lineno_width: usize,
    // Comment input state for inline editing
    comment_input_mode: bool,
    comment_line: Option<(u32, LineSide)>,
    comment_type: crate::model::CommentType,
    comment_buffer: &'a str,
    comment_cursor: usize,
    comment_line_range: Option<LineRange>,
    editing_comment_id: Option<&'a str>,
    current_file_idx: usize,
    // RefCell so deeply-nested rendering helpers can push without each
    // intermediate function needing a `&mut Vec` parameter threaded through.
    comment_bars: std::cell::RefCell<Vec<crate::ui::diff_view::CommentBarAnchor>>,
    sbs_meta: std::cell::RefCell<std::collections::HashMap<usize, SbsRowMeta>>,
    // Logical rows of side-scoped comment boxes: these draw their own right
    // border (offset under one pane), so the shared right-border overlay must
    // skip them.
    side_box_rows: std::cell::RefCell<std::collections::HashSet<usize>>,
    // Only fully build spans for diff lines whose `line_idx` falls in this
    // half-open range; off-screen rows push `Line::default()` placeholders.
    visible_start: usize,
    visible_end: usize,
    search_style: Style,
}

impl SideBySideContext<'_> {
    fn is_visible(&self, line_idx: usize) -> bool {
        line_idx >= self.visible_start && line_idx < self.visible_end
    }

    /// Same question for a multi-row comment box: does any of it land on screen?
    fn box_visible(&self, top: usize, rows: usize) -> bool {
        crate::ui::diff_view::comment_box_visible(top, rows, (self.visible_start, self.visible_end))
    }

    fn search_for(&self, line_idx: usize) -> Option<(&str, Style)> {
        let needle = self.app.search_paint_at(line_idx)?;
        Some((needle, self.search_style))
    }

    fn display_lineno(&self, source_line: Option<u32>, line_idx: usize) -> Option<u32> {
        source_line.map(|line| {
            if self.app.relative_line_numbers {
                line_idx.abs_diff(self.current_line_idx) as u32
            } else {
                line
            }
        })
    }
}

pub(super) fn render_side_by_side_diff(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focused_panel == FocusedPanel::Diff;

    // When the diff is the only pane, drop the frame entirely — including its
    // title row. The file name and stats are folded into the top header line
    // instead (see status_bar::render_header) so there's a single header line.
    let sole = app.is_diff_sole_pane();
    let mut block = Block::default()
        .borders(if sole { Borders::NONE } else { Borders::ALL })
        .style(styles::panel_style(&app.theme))
        .border_style(styles::border_style(&app.theme, focused));
    if !sole {
        block = block
            .title(crate::ui::diff_view::diff_title(app, area.width))
            .title_top(diff_stat_title(app).right_aligned());
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Update viewport height for scroll calculations
    app.diff_state.viewport_height = inner.height as usize;
    app.diff_inner_area = Some(inner);

    // Reset comment input annotation offset (will be set if a comment input box is rendered)
    app.comment_input_annotation_offset = None;

    let lw = app.lineno_width();
    let available_width = inner.width.saturating_sub(crate::app::sbs_overhead(lw)) as usize;
    let content_width = available_width / 2;

    // Determine if we're in line comment mode (not file-level)
    let comment_input_mode = app.input_mode == InputMode::Comment
        && !app.comment_is_file_level
        && !app.comment_is_review_level;

    let (visible_start, visible_end) = crate::ui::diff_view::diff_visible_range(app, inner);

    let ctx = SideBySideContext {
        app,
        theme: &app.theme,
        content_width,
        panel_width: inner.width as usize,
        current_line_idx: app.diff_state.cursor_line,
        lineno_width: lw,
        comment_input_mode,
        comment_line: app.comment_line,
        comment_type: app.comment_type.clone(),
        comment_buffer: &app.comment_buffer,
        comment_cursor: app.comment_cursor,
        comment_line_range: app.comment_line_range.map(|(r, _)| r),
        editing_comment_id: app.editing_comment_id.as_deref(),
        current_file_idx: app.diff_state.current_file_idx,
        comment_bars: std::cell::RefCell::new(Vec::new()),
        sbs_meta: std::cell::RefCell::new(std::collections::HashMap::new()),
        side_box_rows: std::cell::RefCell::new(std::collections::HashSet::new()),
        visible_start,
        visible_end,
        search_style: styles::search_match_style(&app.theme),
    };

    // Build all diff lines for side-by-side view
    let mut lines: Vec<Line> = Vec::new();
    let mut line_idx: usize = 0;

    // Track cursor position for IME when in Comment mode
    let mut comment_cursor_logical_line: Option<usize> = None;
    let mut comment_cursor_column: u16 = 0;
    // Track the full extent of the comment input box so we can auto-scroll
    // the viewport to keep it visible while the user types.
    let mut comment_input_box_range: Option<(usize, usize)> = None;
    let mut annotation_offset: Option<(usize, usize, usize)> = None;

    let is_review_comment_mode =
        app.input_mode == InputMode::Comment && app.comment_is_review_level;

    crate::ui::pr_info_panel::append_pr_info_section(
        app,
        &mut lines,
        &mut line_idx,
        ctx.current_line_idx,
    );

    // The `═══ Review Comments ═══` label is redundant in single-file
    // view -- see the matching guard in `src/ui/diff_unified.rs`.
    if app.show_review_comments_header() {
        let general_indicator = cursor_indicator_spaced(line_idx, ctx.current_line_idx);
        lines.push(Line::from(vec![
            Span::styled(
                general_indicator,
                styles::current_line_indicator_style(&app.theme),
            ),
            Span::styled(
                crate::ui::diff_view::REVIEW_COMMENTS_HEADER_PREFIX,
                styles::file_header_style(&app.theme),
            ),
            Span::styled(
                crate::ui::diff_view::HEADER_RULE,
                styles::file_header_style(&app.theme),
            ),
        ]));
        line_idx += 1;
    }

    for summary in &app.forge_review_summaries {
        let summary_lines = comment_panel::format_remote_review_summary_lines(
            &app.theme,
            summary,
            app.forge_kind(),
        );
        for mut summary_line in summary_lines {
            let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
            summary_line.spans.insert(
                0,
                Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
            );
            lines.push(summary_line);
            line_idx += 1;
        }
    }

    // A review-level reply's editor opens in the slot the reply will be
    // stored in — under the thread it answers, not after every thread.
    let review_reply_slot = app.review_comment_reply_slot();
    let mut review_reply_input_drawn = false;

    for (comment_idx, comment) in app.session.review_comments.iter().enumerate() {
        // The row model skips what is not visible — a settled thread's replies
        // above all — so drawing it here would put the diff one box lower than
        // every row index says it is, and the cursor would act on a different
        // comment than the one under it.
        if !app.comment_visible(comment) {
            continue;
        }
        let is_being_edited =
            app.editing_comment_id.as_ref() == Some(&comment.id) && is_review_comment_mode;

        if is_being_edited {
            let (input_lines, cursor_info) = comment_panel::format_comment_input_lines(
                &app.theme,
                comment_type_presentation(app, &app.comment_type),
                &app.comment_buffer,
                app.comment_cursor,
                None,
                true,
                ctx.panel_width.saturating_sub(1),
                app.comment_vim_mode_label()
                    .as_ref()
                    .map(|(t, w)| (t.as_str(), *w)),
                app.supports_keyboard_enhancement,
                app.comment_reply_author().as_deref(),
            );
            comment_cursor_logical_line = Some(line_idx + cursor_info.line_offset);
            comment_cursor_column = 1 + cursor_info.column;
            comment_input_box_range =
                Some((line_idx, line_idx + input_lines.len().saturating_sub(1)));
            let annotations_replaced = ctx
                .app
                .comment_rows(comment, inner.width.saturating_sub(1) as usize);
            annotation_offset = Some((line_idx, input_lines.len(), annotations_replaced));

            for mut input_line in input_lines {
                let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
                input_line.spans.insert(
                    0,
                    Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                );
                lines.push(input_line);
                line_idx += 1;
            }
        } else {
            let rows = ctx
                .app
                .comment_rows(comment, ctx.panel_width.saturating_sub(1));
            if !ctx.box_visible(line_idx, rows) {
                skip_comment_box(&mut lines, &mut line_idx, rows);
            } else {
                let comment_lines = comment_panel::format_comment_lines(
                    &app.theme,
                    comment_type_presentation(app, &comment.comment_type),
                    &comment.content,
                    None,
                    ctx.panel_width.saturating_sub(1),
                    comment_panel::CommentBadge::for_comment(comment, &app.username),
                    ctx.app.thread_display(comment),
                );
                for mut comment_line in comment_lines {
                    let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
                    comment_line.spans.insert(
                        0,
                        Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                    );
                    lines.push(comment_line);
                    line_idx += 1;
                }
            }
        }

        if is_review_comment_mode
            && app.editing_comment_id.is_none()
            && review_reply_slot == Some(comment_idx)
        {
            let drawn = crate::ui::diff_view::push_comment_input(
                app,
                &mut lines,
                &mut line_idx,
                ctx.panel_width.saturating_sub(1),
                ctx.current_line_idx,
                false,
            );
            comment_cursor_logical_line = Some(drawn.cursor_line);
            comment_cursor_column = drawn.cursor_column;
            comment_input_box_range = Some(drawn.box_range);
            annotation_offset = Some((drawn.box_range.0, drawn.rows, 0));
            review_reply_input_drawn = true;
        }
    }

    // Render remote review-level threads (general MR notes, line: None).
    {
        use crate::forge::remote_comments::{PrCommentsVisibility, RemoteCommentSide};
        let _ = RemoteCommentSide::Right; // ensure import is used
        let visibility = app.session.remote_comments_visibility;
        if !matches!(visibility, PrCommentsVisibility::Hide) {
            for thread in &app.forge_review_threads {
                if thread.line.is_some() {
                    continue; // inline threads are rendered in-diff
                }
                let Some(muted) = visibility.render_decision(thread) else {
                    continue;
                };
                let thread_lines = comment_panel::format_remote_thread_lines(
                    &app.theme,
                    thread,
                    muted,
                    app.forge_kind(),
                );
                for mut comment_line in thread_lines {
                    let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
                    comment_line.spans.insert(
                        0,
                        Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                    );
                    lines.push(comment_line);
                    line_idx += 1;
                }
            }
        }
    }

    // A fresh review-level comment, or a reply whose thread is not on screen.
    if is_review_comment_mode && app.editing_comment_id.is_none() && !review_reply_input_drawn {
        let drawn = crate::ui::diff_view::push_comment_input(
            app,
            &mut lines,
            &mut line_idx,
            ctx.panel_width.saturating_sub(1),
            ctx.current_line_idx,
            false,
        );
        comment_cursor_logical_line = Some(drawn.cursor_line);
        comment_cursor_column = drawn.cursor_column;
        comment_input_box_range = Some(drawn.box_range);
        annotation_offset = Some((drawn.box_range.0, drawn.rows, 0));
    }

    crate::ui::pr_info_panel::append_issue_comments_section(
        app,
        &mut lines,
        &mut line_idx,
        ctx.current_line_idx,
        ctx.panel_width.saturating_sub(1),
        (ctx.visible_start, ctx.visible_end),
    );

    for (file_idx, file) in app.diff_files.iter().enumerate() {
        // Single-file view: hide everything except the cursor's file. See
        // src/ui/diff_unified.rs for the matching guard.
        if app.is_single_file_view && file_idx != app.diff_state.current_file_idx {
            continue;
        }
        // See the matching filter guard in src/ui/diff_unified.rs.
        if !app.file_passes_filter(file) {
            continue;
        }
        let path = file.display_path();
        let is_reviewed = app.session.is_file_reviewed(path);

        if !app.is_single_file_view {
            let indicator = cursor_indicator_spaced(line_idx, ctx.current_line_idx);
            let header_text = crate::ui::diff_view::file_header_prefix_text(app, file);
            lines.push(Line::from(vec![
                Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                Span::styled(header_text, styles::file_header_style(&app.theme)),
                Span::styled(
                    crate::ui::diff_view::HEADER_RULE,
                    styles::file_header_style(&app.theme),
                ),
            ]));
            line_idx += 1;
        }

        // Reviewed files normally collapse in continuous view. A summary jump
        // may reveal one target body without changing its reviewed marker.
        if app.should_collapse_file(file_idx) {
            continue;
        }
        if is_reviewed && app.is_single_file_view {
            let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
            lines.push(Line::from(vec![
                Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                Span::styled(
                    crate::ui::diff_view::REVIEWED_BANNER_TEXT,
                    Style::default()
                        .fg(app.theme.fg_secondary)
                        .add_modifier(Modifier::DIM),
                ),
            ]));
            line_idx += 1;
        }

        // Check if we're editing/adding a file-level comment for this file
        let is_file_comment_mode = app.input_mode == InputMode::Comment
            && app.comment_is_file_level
            && file_idx == app.diff_state.current_file_idx;

        // Show file-level comments
        let reply_slot = app.file_comment_reply_slot(path);
        let mut reply_input_drawn = false;
        if let Some(review) = app.session.files.get(path) {
            for (comment_idx, comment) in review.file_comments.iter().enumerate() {
                if !app.comment_visible(comment) {
                    continue;
                }
                // Skip rendering this comment if it's being edited
                let is_being_edited =
                    app.editing_comment_id.as_ref() == Some(&comment.id) && is_file_comment_mode;

                if is_being_edited {
                    // Render the inline input instead
                    let (input_lines, cursor_info) = comment_panel::format_comment_input_lines(
                        &app.theme,
                        comment_type_presentation(app, &app.comment_type),
                        &app.comment_buffer,
                        app.comment_cursor,
                        None,
                        true,
                        ctx.panel_width.saturating_sub(1),
                        app.comment_vim_mode_label()
                            .as_ref()
                            .map(|(t, w)| (t.as_str(), *w)),
                        app.supports_keyboard_enhancement,
                        app.comment_reply_author().as_deref(),
                    );
                    comment_cursor_logical_line = Some(line_idx + cursor_info.line_offset);
                    comment_cursor_column = 1 + cursor_info.column;
                    comment_input_box_range =
                        Some((line_idx, line_idx + input_lines.len().saturating_sub(1)));
                    let annotations_replaced = ctx
                        .app
                        .comment_rows(comment, inner.width.saturating_sub(1) as usize);
                    annotation_offset = Some((line_idx, input_lines.len(), annotations_replaced));

                    for mut input_line in input_lines {
                        let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
                        input_line.spans.insert(
                            0,
                            Span::styled(
                                indicator,
                                styles::current_line_indicator_style(&app.theme),
                            ),
                        );
                        lines.push(input_line);
                        line_idx += 1;
                    }
                } else {
                    let rows = ctx
                        .app
                        .comment_rows(comment, ctx.panel_width.saturating_sub(1));
                    if !ctx.box_visible(line_idx, rows) {
                        skip_comment_box(&mut lines, &mut line_idx, rows);
                        continue;
                    }
                    let comment_lines = comment_panel::format_comment_lines(
                        &app.theme,
                        comment_type_presentation(app, &comment.comment_type),
                        &comment.content,
                        None,
                        ctx.panel_width.saturating_sub(1),
                        comment_panel::CommentBadge::for_comment(comment, &app.username),
                        ctx.app.thread_display(comment),
                    );
                    for mut comment_line in comment_lines {
                        let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
                        comment_line.spans.insert(
                            0,
                            Span::styled(
                                indicator,
                                styles::current_line_indicator_style(&app.theme),
                            ),
                        );
                        lines.push(comment_line);
                        line_idx += 1;
                    }
                }

                // A reply belongs under the thread it answers, not at the
                // bottom of every thread in the file.
                if is_file_comment_mode && reply_slot == Some(comment_idx) {
                    let drawn = crate::ui::diff_view::push_comment_input(
                        app,
                        &mut lines,
                        &mut line_idx,
                        ctx.panel_width.saturating_sub(1),
                        ctx.current_line_idx,
                        false,
                    );
                    comment_cursor_logical_line = Some(drawn.cursor_line);
                    comment_cursor_column = drawn.cursor_column;
                    comment_input_box_range = Some(drawn.box_range);
                    annotation_offset = Some((drawn.box_range.0, drawn.rows, 0));
                    reply_input_drawn = true;
                }
            }
        }

        // Render inline input for a new file-level comment, or a reply whose
        // thread is not on screen.
        if is_file_comment_mode && app.editing_comment_id.is_none() && !reply_input_drawn {
            let (input_lines, cursor_info) = comment_panel::format_comment_input_lines(
                &app.theme,
                comment_type_presentation(app, &app.comment_type),
                &app.comment_buffer,
                app.comment_cursor,
                None,
                false,
                ctx.panel_width.saturating_sub(1),
                app.comment_vim_mode_label()
                    .as_ref()
                    .map(|(t, w)| (t.as_str(), *w)),
                app.supports_keyboard_enhancement,
                app.comment_reply_author().as_deref(),
            );
            comment_cursor_logical_line = Some(line_idx + cursor_info.line_offset);
            comment_cursor_column = 1 + cursor_info.column;
            comment_input_box_range =
                Some((line_idx, line_idx + input_lines.len().saturating_sub(1)));
            annotation_offset = Some((line_idx, input_lines.len(), 0));

            for mut input_line in input_lines {
                let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
                input_line.spans.insert(
                    0,
                    Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                );
                lines.push(input_line);
                line_idx += 1;
            }
        }

        if file.is_too_large || file.is_binary || file.hunks.is_empty() {
            let indicator = cursor_indicator_spaced(line_idx, ctx.current_line_idx);
            lines.push(Line::from(vec![
                Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                Span::styled(
                    crate::ui::diff_view::binary_or_empty_label(file),
                    styles::dim_style(&app.theme),
                ),
            ]));
            line_idx += 1;
        } else {
            let line_comments = app
                .session
                .files
                .get(path)
                .map(|r| &r.line_comments)
                .unwrap_or(&crate::ui::diff_view::EMPTY_LINE_COMMENTS);

            for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
                // Calculate and render gap before this hunk
                let prev_hunk = if hunk_idx > 0 {
                    file.hunks.get(hunk_idx - 1)
                } else {
                    None
                };
                let gap = calculate_gap(
                    prev_hunk.map(|h| (&h.new_start, &h.new_count)),
                    hunk.new_start,
                );

                let gap_id = GapId { file_idx, hunk_idx };

                if gap > 0 && app.should_render_gap_before_hunk(file_idx, hunk_idx) {
                    let top_lines = app.expanded_top.get(&gap_id);
                    let bot_lines = app.expanded_bottom.get(&gap_id);
                    let top_len = top_lines.map_or(0, |v| v.len());
                    let bot_len = bot_lines.map_or(0, |v| v.len());
                    let remaining = (gap as usize).saturating_sub(top_len + bot_len);
                    let is_top_of_file = hunk_idx == 0;

                    // Render top expanded lines
                    if let Some(top) = top_lines {
                        for expanded_line in top {
                            if !ctx.is_visible(line_idx) {
                                lines.push(Line::default());
                                line_idx += 1;
                                continue;
                            }
                            render_sbs_expanded_context_line(
                                &mut lines,
                                &mut line_idx,
                                expanded_line,
                                &ctx,
                            );
                        }
                    }

                    // Render expanders / hidden lines
                    if remaining > 0 {
                        if is_top_of_file {
                            if remaining > GAP_EXPAND_BATCH {
                                render_hidden_lines(
                                    &mut lines,
                                    &mut line_idx,
                                    ctx.current_line_idx,
                                    remaining,
                                    &app.theme,
                                );
                            }
                            render_expander_line(
                                &mut lines,
                                &mut line_idx,
                                ctx.current_line_idx,
                                ExpandDirection::Up,
                                remaining,
                                &app.theme,
                            );
                        } else if remaining >= GAP_EXPAND_BATCH {
                            render_expander_line(
                                &mut lines,
                                &mut line_idx,
                                ctx.current_line_idx,
                                ExpandDirection::Down,
                                remaining,
                                &app.theme,
                            );
                            render_hidden_lines(
                                &mut lines,
                                &mut line_idx,
                                ctx.current_line_idx,
                                remaining,
                                &app.theme,
                            );
                            render_expander_line(
                                &mut lines,
                                &mut line_idx,
                                ctx.current_line_idx,
                                ExpandDirection::Up,
                                remaining,
                                &app.theme,
                            );
                        } else {
                            render_expander_line(
                                &mut lines,
                                &mut line_idx,
                                ctx.current_line_idx,
                                ExpandDirection::Both,
                                remaining,
                                &app.theme,
                            );
                        }
                    }

                    // Render bottom expanded lines
                    if let Some(bot) = bot_lines {
                        for expanded_line in bot {
                            if !ctx.is_visible(line_idx) {
                                lines.push(Line::default());
                                line_idx += 1;
                                continue;
                            }
                            render_sbs_expanded_context_line(
                                &mut lines,
                                &mut line_idx,
                                expanded_line,
                                &ctx,
                            );
                        }
                    }
                }

                // Hunk header
                let is_hunk_reviewed = app.is_hunk_reviewed(file_idx, hunk_idx);
                let (hunk_header_text, hunk_header_style) =
                    hunk_header_text_and_style(&app.theme, hunk, is_hunk_reviewed);
                let indicator = cursor_indicator_spaced(line_idx, ctx.current_line_idx);
                lines.push(Line::from(vec![
                    Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                    Span::styled(hunk_header_text, hunk_header_style),
                ]));
                line_idx += 1;
                if app.should_collapse_hunk(file_idx, hunk_idx) {
                    continue;
                }

                // Process diff lines in side-by-side format
                let (new_line_idx, cursor_info) = render_hunk_lines_side_by_side(
                    &hunk.lines,
                    line_comments,
                    &ctx,
                    file_idx,
                    line_idx,
                    &mut lines,
                );
                line_idx = new_line_idx;
                if let Some((line, col, box_start, box_end, annotations_replaced)) = cursor_info {
                    comment_cursor_logical_line = Some(line);
                    comment_cursor_column = col;
                    comment_input_box_range = Some((box_start, box_end));
                    let box_len = box_end - box_start + 1;
                    annotation_offset = Some((box_start, box_len, annotations_replaced));
                }
            }
        }

        // End-of-file gap (after all hunks, not for deleted files)
        if file.status != FileStatus::Deleted
            && matches!(
                app.diff_source,
                DiffSource::WorkingTree
                    | DiffSource::Unstaged
                    | DiffSource::StagedAndUnstaged
                    | DiffSource::WorkingTreeFrom(_)
                    | DiffSource::RevisionDiff { .. }
                    | DiffSource::StagedUnstagedAndCommits(_)
                    | DiffSource::CommitRange(_)
                    | DiffSource::PullRequest(_)
            )
            && let Some(last_hunk) = file.hunks.last()
        {
            let eof_start = last_hunk.new_start + last_hunk.new_count;
            if let Some(&total) = app.file_line_count_cache.get(&file_idx)
                && eof_start <= total
            {
                let gap = (total - eof_start + 1) as usize;
                let eof_gap_id = GapId {
                    file_idx,
                    hunk_idx: file.hunks.len(),
                };
                let top_lines = app.expanded_top.get(&eof_gap_id);
                let bot_lines = app.expanded_bottom.get(&eof_gap_id);
                let top_len = top_lines.map_or(0, |v| v.len());
                let bot_len = bot_lines.map_or(0, |v| v.len());
                let remaining = gap.saturating_sub(top_len + bot_len);

                // Render top expanded lines (↓ direction)
                if let Some(top) = top_lines {
                    for expanded_line in top {
                        render_sbs_expanded_context_line(
                            &mut lines,
                            &mut line_idx,
                            expanded_line,
                            &ctx,
                        );
                    }
                }

                // Expander / hidden lines
                if remaining > 0 {
                    render_expander_line(
                        &mut lines,
                        &mut line_idx,
                        ctx.current_line_idx,
                        ExpandDirection::Down,
                        remaining,
                        &app.theme,
                    );
                    if remaining > GAP_EXPAND_BATCH {
                        render_hidden_lines(
                            &mut lines,
                            &mut line_idx,
                            ctx.current_line_idx,
                            remaining,
                            &app.theme,
                        );
                    }
                }

                // Render bottom expanded lines
                if let Some(bot) = bot_lines {
                    for expanded_line in bot {
                        render_sbs_expanded_context_line(
                            &mut lines,
                            &mut line_idx,
                            expanded_line,
                            &ctx,
                        );
                    }
                }
            }
        }

        // Spacing between files
        let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
        lines.push(Line::from(Span::styled(
            indicator,
            styles::current_line_indicator_style(&app.theme),
        )));
        line_idx += 1;
    }

    let comment_bars = {
        let mut bars = ctx.comment_bars.borrow_mut();
        std::mem::take(&mut *bars)
    };
    let sbs_meta = {
        let mut m = ctx.sbs_meta.borrow_mut();
        std::mem::take(&mut *m)
    };
    let side_box_rows = {
        let mut s = ctx.side_box_rows.borrow_mut();
        std::mem::take(&mut *s)
    };
    drop(ctx);
    app.comment_input_annotation_offset = annotation_offset;

    // Auto-scroll so the comment input box stays visible while the user types.
    // ...unless the reader deliberately scrolled away to look at something
    // else while composing. Dragging them back every frame is what made the
    // editor feel like a trap.
    if !app.comment_scroll_detached {
        scroll_comment_input_into_view(
            &mut app.diff_state.scroll_offset,
            comment_input_box_range,
            comment_cursor_logical_line,
            inner.height as usize,
            lines.len(),
        );
    }

    let visible_lines_unscrolled: Vec<Line> = lines
        .into_iter()
        .skip(app.diff_state.scroll_offset)
        .take(inner.height as usize)
        .collect();

    // Calculate the width of each line for max_content_width and visible line count
    let line_widths: Vec<usize> = visible_lines_unscrolled
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.width())
                .sum::<usize>()
        })
        .collect();

    let max_content_width = sbs_meta
        .values()
        .flat_map(|meta| [&meta.left_content, &meta.right_content])
        .map(|spans| spans.iter().map(|span| span.content.width()).sum::<usize>())
        .max()
        .unwrap_or(0);

    app.sync_viewport_width(inner.width as usize);
    app.diff_state.max_content_width = max_content_width;

    let scroll_offset = app.diff_state.scroll_offset;
    let wrap = app.diff_state.wrap_lines;
    let viewport_width = inner.width as usize;
    let visible_lines_unscrolled_for_overlay = visible_lines_unscrolled.clone();
    // Single pass: wrap each logical line once, producing both the visual
    // rows to render and the per-line height used by every row-mapping
    // consumer below, so the two can't disagree.
    let (row_heights, wrapped_lines): (Vec<usize>, Option<Vec<Line>>) = if wrap && content_width > 0
    {
        let mut heights = Vec::with_capacity(visible_lines_unscrolled_for_overlay.len());
        let mut out: Vec<Line> = Vec::new();
        let (left_prefix_blank, right_prefix_blank) = sbs_blank_prefixes(&app.theme, lw);
        for (i, line) in visible_lines_unscrolled_for_overlay.iter().enumerate() {
            let logical_idx = scroll_offset + i;
            match sbs_meta.get(&logical_idx) {
                Some(m) => {
                    let left_rows = if m.left_content.is_empty() {
                        vec![Vec::new()]
                    } else {
                        wrap_spans(&m.left_content, content_width)
                    };
                    let right_rows = if m.right_content.is_empty() {
                        vec![Vec::new()]
                    } else {
                        wrap_spans(&m.right_content, content_width)
                    };
                    let n = left_rows.len().max(right_rows.len()).max(1);
                    heights.push(n);
                    let empty_row: Vec<Span> = Vec::new();
                    for k in 0..n {
                        let left_content_row = left_rows.get(k).unwrap_or(&empty_row).clone();
                        let right_content_row = right_rows.get(k).unwrap_or(&empty_row).clone();
                        let left_padded =
                            pad_spans_to_width(left_content_row, content_width, m.left_pad_style);
                        let right_padded =
                            pad_spans_to_width(right_content_row, content_width, m.right_pad_style);
                        let (left_prefix, right_prefix) = if k == 0 {
                            (m.left_prefix.clone(), m.right_prefix.clone())
                        } else {
                            (left_prefix_blank.clone(), right_prefix_blank.clone())
                        };
                        let mut spans = left_prefix;
                        spans.extend(left_padded);
                        spans.extend(right_prefix);
                        spans.extend(right_padded);
                        out.push(Line::from(spans));
                    }
                }
                None => {
                    let rows = wrap_spans(&line.spans, viewport_width);
                    heights.push(rows.len());
                    out.extend(rows.into_iter().map(Line::from));
                }
            }
        }
        (heights, Some(out))
    } else {
        (vec![1; visible_lines_unscrolled_for_overlay.len()], None)
    };
    app.diff_state.visible_line_count = populate_row_to_annotation(
        &mut app.diff_row_to_annotation,
        &row_heights,
        viewport_width,
        inner.height as usize,
        wrap,
        scroll_offset,
    );

    let max_scroll_x = max_content_width.saturating_sub(content_width);
    if app.diff_state.scroll_x > max_scroll_x {
        app.diff_state.scroll_x = max_scroll_x;
    }
    if app.diff_state.wrap_lines {
        app.diff_state.scroll_x = 0;
    }

    let scroll_x = app.diff_state.scroll_x;
    let visible_lines: Vec<Line> = match wrapped_lines {
        Some(out) => out,
        None => visible_lines_unscrolled
            .into_iter()
            .enumerate()
            .map(|(i, line)| {
                // Side-scoped and inline-input boxes strip the indent
                // `comment_box_row` keys on, so ask the row model too: a
                // panned comment box drags the text cursor off its glyph.
                let row = scroll_offset + i;
                let own_box = side_box_rows.contains(&row)
                    || comment_input_box_range.is_some_and(|(s, e)| row >= s && row <= e);
                if scroll_x == 0 || own_box || comment_box_row(&line).is_some() {
                    line
                } else {
                    sbs_meta
                        .get(&(scroll_offset + i))
                        .map(|meta| pan_sbs_row(meta, scroll_x, content_width))
                        .unwrap_or_else(|| apply_horizontal_scroll(line, scroll_x))
                }
            })
            .collect(),
    };

    let overlay_ctx = crate::ui::diff_view::DiffOverlayPaint {
        inner,
        visible_lines_unscrolled: &visible_lines_unscrolled_for_overlay,
        line_widths: &line_widths,
        row_heights: &row_heights,
        wrap_lines: app.diff_state.wrap_lines,
        viewport_width: inner.width as usize,
        scroll_x,
        scroll_offset: app.diff_state.scroll_offset,
        theme: &app.theme,
        comment_bars: &comment_bars,
        fixed_gutters: true,
        self_bordered_rows: &side_box_rows,
    };

    // Section-marker row tint (hunk headers + expand/hidden stubs).
    crate::ui::diff_view::paint_section_highlight(frame, &overlay_ctx);

    let diff = Paragraph::new(visible_lines).style(styles::panel_style(&app.theme));
    frame.render_widget(diff, inner);

    paint_cursor_line_highlight(
        frame,
        inner,
        &visible_lines_unscrolled_for_overlay,
        &row_heights,
        app,
    );

    // Move the `▶` caret onto the active side for the cursor row.
    paint_sbs_active_side_caret(frame, inner, app, lw, content_width, &row_heights);

    // Painted last so the cell overlay wins over cursor-line bg on overlap.
    if let Some(sel) = app.visual_selection {
        paint_visual_selection_overlay(frame, inner, app, sel, &app.theme);
    }

    crate::ui::diff_view::paint_diff_cursor(frame, inner, app);

    // File-section header rules extended to the full viewport width.
    crate::ui::diff_view::paint_file_header_fill(frame, &overlay_ctx);

    // Comment-box overlays painted last so the box + bar always win on their
    // single cells.
    crate::ui::diff_view::paint_comment_box_bar(frame, &overlay_ctx);
    crate::ui::diff_view::paint_comment_box_right_border(frame, &overlay_ctx);

    // Calculate screen position for comment cursor if in Comment mode
    if let Some(cursor_logical_line) = comment_cursor_logical_line {
        let scroll_offset = app.diff_state.scroll_offset;
        let visible_lines_count = app.diff_state.visible_line_count.max(1);

        // Check if the cursor line is visible (after scrolling)
        if cursor_logical_line >= scroll_offset
            && cursor_logical_line < scroll_offset + visible_lines_count
        {
            // Calculate screen row - need to account for wrapping
            let logical_offset = cursor_logical_line - scroll_offset;

            let mut visual_row: u16 = 0;
            let viewport_width = inner.width as usize;

            if app.diff_state.wrap_lines && viewport_width > 0 {
                for i in 0..logical_offset {
                    visual_row += row_heights.get(i).copied().unwrap_or(1) as u16;
                }
            } else {
                visual_row = logical_offset as u16;
            }

            let screen_col = inner.x + comment_cursor_column;
            let screen_row_abs = inner.y + visual_row;

            app.comment_cursor_screen_pos = Some((screen_col, screen_row_abs));
        }
    }
}

/// Render a single expanded context line in side-by-side mode
fn render_sbs_expanded_context_line(
    lines: &mut Vec<Line<'_>>,
    line_idx: &mut usize,
    expanded_line: &crate::model::DiffLine,
    ctx: &SideBySideContext,
) {
    let theme = ctx.theme;
    let lw = ctx.lineno_width;
    let content_width = ctx.content_width;
    let indicator = cursor_indicator(*line_idx, ctx.current_line_idx);
    let old_line_num = ctx
        .display_lineno(expanded_line.old_lineno, *line_idx)
        .map(|n| format!("{n:>lw$} "))
        .unwrap_or_else(|| " ".repeat(lw + 1));
    let new_line_num = ctx
        .display_lineno(expanded_line.new_lineno, *line_idx)
        .map(|n| format!("{n:>lw$} "))
        .unwrap_or_else(|| " ".repeat(lw + 1));
    let ec_style = styles::expanded_context_style(theme);
    let content_cell = plain_cell_spans(
        &expanded_line.content,
        ec_style,
        content_width,
        ctx.search_for(*line_idx),
    );
    let mut line_spans = vec![
        Span::styled(indicator, styles::current_line_indicator_style(theme)),
        Span::styled(old_line_num.clone(), ec_style),
        Span::styled(" ", ec_style),
    ];
    line_spans.extend(content_cell.clone());
    line_spans.extend([
        Span::styled(" │ ", styles::dim_style(theme)),
        Span::styled(new_line_num.clone(), ec_style),
        Span::styled(" ", ec_style),
    ]);
    line_spans.extend(content_cell);
    lines.push(Line::from(line_spans));

    let dim = styles::dim_style(theme);
    let left_prefix = vec![
        Span::styled(indicator, styles::current_line_indicator_style(theme)),
        Span::styled(old_line_num, ec_style),
        Span::styled(" ", ec_style),
    ];
    let right_prefix = vec![
        Span::styled(" │ ", dim),
        Span::styled(new_line_num, ec_style),
        Span::styled(" ", ec_style),
    ];
    let mut content = vec![Span::styled(expanded_line.content.clone(), ec_style)];
    if let Some((needle, hl)) = ctx.search_for(*line_idx) {
        content = apply_search_highlight_spans(content, needle, hl);
    }
    ctx.sbs_meta.borrow_mut().insert(
        *line_idx,
        SbsRowMeta {
            left_content: content.clone(),
            right_content: content,
            left_prefix,
            right_prefix,
            left_pad_style: ec_style,
            right_pad_style: ec_style,
        },
    );
    *line_idx += 1;
}

/// Process and render all diff lines in a hunk for side-by-side view
/// Returns (new_line_idx, optional cursor info for inline comment input)
fn render_hunk_lines_side_by_side(
    hunk_lines: &[crate::model::DiffLine],
    line_comments: &std::collections::HashMap<u32, Vec<crate::model::Comment>>,
    ctx: &SideBySideContext,
    file_idx: usize,
    mut line_idx: usize,
    lines: &mut Vec<Line>,
) -> (usize, Option<SideBySideCursorInfo>) {
    let mut i = 0;
    let mut cursor_info_out: Option<SideBySideCursorInfo> = None;

    // A commit message is a synthetic "added" file; its lines are Context so
    // the unified view renders them neutrally. In side-by-side that would
    // duplicate the message across both columns, so render it right-side only
    // as an addition instead.
    let is_commit_msg = ctx
        .app
        .diff_files
        .get(file_idx)
        .is_some_and(|f| f.is_commit_message);

    while i < hunk_lines.len() {
        let diff_line = &hunk_lines[i];

        match diff_line.origin {
            LineOrigin::Context if is_commit_msg => {
                let (new_line_idx, cursor_info) = render_commit_message_line_side_by_side(
                    diff_line,
                    line_comments,
                    ctx,
                    file_idx,
                    line_idx,
                    lines,
                );
                line_idx = new_line_idx;
                if cursor_info.is_some() {
                    cursor_info_out = cursor_info;
                }
                i += 1;
            }
            LineOrigin::Context => {
                let (new_line_idx, cursor_info) = render_context_line_side_by_side(
                    diff_line,
                    line_comments,
                    ctx,
                    file_idx,
                    line_idx,
                    lines,
                );
                line_idx = new_line_idx;
                if cursor_info.is_some() {
                    cursor_info_out = cursor_info;
                }
                i += 1;
            }
            LineOrigin::Deletion => {
                let (new_line_idx, lines_processed, cursor_info) =
                    render_deletion_addition_pair_side_by_side(
                        hunk_lines,
                        i,
                        line_comments,
                        ctx,
                        file_idx,
                        line_idx,
                        lines,
                    );
                line_idx = new_line_idx;
                if cursor_info.is_some() {
                    cursor_info_out = cursor_info;
                }
                i = lines_processed;
            }
            LineOrigin::Addition => {
                let (new_line_idx, cursor_info) = render_standalone_addition_side_by_side(
                    diff_line,
                    line_comments,
                    ctx,
                    file_idx,
                    line_idx,
                    lines,
                );
                line_idx = new_line_idx;
                if cursor_info.is_some() {
                    cursor_info_out = cursor_info;
                }
                i += 1;
            }
        }
    }
    (line_idx, cursor_info_out)
}

/// Render a context line (appears on both sides)
/// Returns (new_line_idx, optional cursor info for inline comment input)
fn render_context_line_side_by_side(
    diff_line: &crate::model::DiffLine,
    line_comments: &std::collections::HashMap<u32, Vec<crate::model::Comment>>,
    ctx: &SideBySideContext,
    file_idx: usize,
    mut line_idx: usize,
    lines: &mut Vec<Line>,
) -> (usize, Option<SideBySideCursorInfo>) {
    if ctx.is_visible(line_idx) {
        let w = ctx.lineno_width;
        let old_line_num = ctx
            .display_lineno(diff_line.old_lineno, line_idx)
            .map(|n| format!("{n:>w$}"))
            .unwrap_or_else(|| " ".repeat(w));
        let new_line_num = ctx
            .display_lineno(diff_line.new_lineno, line_idx)
            .map(|n| format!("{n:>w$}"))
            .unwrap_or_else(|| " ".repeat(w));

        let indicator = cursor_indicator(line_idx, ctx.current_line_idx);

        let mut spans = vec![
            Span::styled(indicator, styles::current_line_indicator_style(ctx.theme)),
            Span::styled(format!("{old_line_num} "), styles::dim_style(ctx.theme)),
            Span::styled(" ".to_string(), styles::diff_context_style(ctx.theme)),
        ];

        let search = ctx.search_for(line_idx);
        let content_cell = if let Some(ref highlighted) = diff_line.highlighted_spans {
            searched_cell_spans(
                highlighted,
                ctx.content_width,
                styles::diff_context_style(ctx.theme),
                search,
            )
        } else {
            plain_cell_spans(
                &diff_line.content,
                styles::diff_context_style(ctx.theme),
                ctx.content_width,
                search,
            )
        };

        // Left side content - use syntax highlighting if available
        spans.extend(content_cell.clone());

        // Separator
        spans.push(Span::styled(" │ ", styles::dim_style(ctx.theme)));
        spans.push(Span::styled(
            format!("{new_line_num} "),
            styles::dim_style(ctx.theme),
        ));
        spans.push(Span::styled(
            " ".to_string(),
            styles::diff_context_style(ctx.theme),
        ));

        // Right side content - use same highlighting
        spans.extend(content_cell);

        lines.push(Line::from(spans));

        let content =
            content_spans_for_diff_line(ctx.theme, diff_line, LineOrigin::Context, search);
        let ctx_style = styles::diff_context_style(ctx.theme);
        let (lp, rp) = sbs_row_prefixes(
            ctx.theme,
            indicator,
            SideSpec {
                lineno: ctx.display_lineno(diff_line.old_lineno, line_idx),
                marker: " ",
                marker_style: ctx_style,
            },
            SideSpec {
                lineno: ctx.display_lineno(diff_line.new_lineno, line_idx),
                marker: " ",
                marker_style: ctx_style,
            },
            w,
        );
        ctx.sbs_meta.borrow_mut().insert(
            line_idx,
            SbsRowMeta {
                left_content: content.clone(),
                right_content: content,
                left_prefix: lp,
                right_prefix: rp,
                left_pad_style: ctx_style,
                right_pad_style: ctx_style,
            },
        );
    } else {
        lines.push(Line::default());
    }
    line_idx += 1;

    // Add comments if any. A context line exists on both sides, so a comment
    // can be attached to either — render the old (left) side first, then new.
    let mut cursor_info_out: Option<SideBySideCursorInfo> = None;
    if let Some(old_ln) = diff_line.old_lineno {
        let (new_line_idx, cursor_info) = add_comments_to_line(
            old_ln,
            line_comments,
            LineSide::Old,
            ctx,
            file_idx,
            line_idx,
            lines,
        );
        line_idx = new_line_idx;
        if cursor_info.is_some() {
            cursor_info_out = cursor_info;
        }
        if let Some(file) = ctx.app.diff_files.get(file_idx) {
            line_idx = add_remote_threads_to_line(
                old_ln,
                LineSide::Old,
                ctx,
                file.display_path(),
                line_idx,
                lines,
            );
        }
    }
    if let Some(new_ln) = diff_line.new_lineno {
        let (new_line_idx, cursor_info) = add_comments_to_line(
            new_ln,
            line_comments,
            LineSide::New,
            ctx,
            file_idx,
            line_idx,
            lines,
        );
        line_idx = new_line_idx;
        if cursor_info.is_some() {
            cursor_info_out = cursor_info;
        }
        if let Some(file) = ctx.app.diff_files.get(file_idx) {
            line_idx = add_remote_threads_to_line(
                new_ln,
                LineSide::New,
                ctx,
                file.display_path(),
                line_idx,
                lines,
            );
        }
    }

    (line_idx, cursor_info_out)
}

/// Render paired deletions and additions side-by-side
/// Returns (line_idx, skip_count, optional cursor info for inline comment input)
fn render_deletion_addition_pair_side_by_side(
    hunk_lines: &[crate::model::DiffLine],
    start_idx: usize,
    line_comments: &std::collections::HashMap<u32, Vec<crate::model::Comment>>,
    ctx: &SideBySideContext,
    file_idx: usize,
    mut line_idx: usize,
    lines: &mut Vec<Line>,
) -> (usize, usize, Option<SideBySideCursorInfo>) {
    // Find the range of consecutive deletions
    let mut del_end = start_idx + 1;
    while del_end < hunk_lines.len() && hunk_lines[del_end].origin == LineOrigin::Deletion {
        del_end += 1;
    }

    // Find the range of consecutive additions following the deletions
    let add_start = del_end;
    let mut add_end = add_start;
    while add_end < hunk_lines.len() && hunk_lines[add_end].origin == LineOrigin::Addition {
        add_end += 1;
    }

    let del_count = del_end - start_idx;
    let add_count = add_end - add_start;
    let max_lines = del_count.max(add_count);
    let mut cursor_info_out: Option<SideBySideCursorInfo> = None;

    // Render each pair of deletion/addition
    for offset in 0..max_lines {
        let del_opt = (offset < del_count).then(|| &hunk_lines[start_idx + offset]);
        let add_opt = (offset < add_count).then(|| &hunk_lines[add_start + offset]);
        if ctx.is_visible(line_idx) {
            let indicator = cursor_indicator(line_idx, ctx.current_line_idx);

            let mut spans = vec![Span::styled(
                indicator,
                styles::current_line_indicator_style(ctx.theme),
            )];

            // Left side (deletion)
            if let Some(del_line) = del_opt {
                add_deletion_spans(
                    ctx.theme,
                    &mut spans,
                    del_line,
                    ctx.content_width,
                    ctx.lineno_width,
                    ctx.display_lineno(del_line.old_lineno, line_idx),
                    ctx.search_for(line_idx),
                );
            } else {
                add_empty_column_spans(&mut spans, ctx.content_width, ctx.lineno_width);
            }

            spans.push(Span::styled(" │ ", styles::dim_style(ctx.theme)));

            // Right side (addition)
            if let Some(add_line) = add_opt {
                add_addition_spans(
                    ctx.theme,
                    &mut spans,
                    add_line,
                    ctx.content_width,
                    ctx.lineno_width,
                    ctx.display_lineno(add_line.new_lineno, line_idx),
                    ctx.search_for(line_idx),
                );
            } else {
                add_empty_column_spans(&mut spans, ctx.content_width, ctx.lineno_width);
            }

            lines.push(Line::from(spans));

            let w = ctx.lineno_width;
            let (left_content, left_pad, left_marker, left_lineno, left_marker_style) =
                match del_opt {
                    Some(dl) => (
                        content_spans_for_diff_line(
                            ctx.theme,
                            dl,
                            LineOrigin::Deletion,
                            ctx.search_for(line_idx),
                        ),
                        column_pad_style(ctx.theme, dl, LineOrigin::Deletion),
                        "▌",
                        ctx.display_lineno(dl.old_lineno, line_idx),
                        styles::diff_del_style(ctx.theme),
                    ),
                    None => (Vec::new(), Style::default(), " ", None, Style::default()),
                };
            let (right_content, right_pad, right_marker, right_lineno, right_marker_style) =
                match add_opt {
                    Some(al) => (
                        content_spans_for_diff_line(
                            ctx.theme,
                            al,
                            LineOrigin::Addition,
                            ctx.search_for(line_idx),
                        ),
                        column_pad_style(ctx.theme, al, LineOrigin::Addition),
                        "▌",
                        ctx.display_lineno(al.new_lineno, line_idx),
                        styles::diff_add_style(ctx.theme),
                    ),
                    None => (Vec::new(), Style::default(), " ", None, Style::default()),
                };
            let (lp, rp) = sbs_row_prefixes(
                ctx.theme,
                indicator,
                SideSpec {
                    lineno: left_lineno,
                    marker: left_marker,
                    marker_style: left_marker_style,
                },
                SideSpec {
                    lineno: right_lineno,
                    marker: right_marker,
                    marker_style: right_marker_style,
                },
                w,
            );
            ctx.sbs_meta.borrow_mut().insert(
                line_idx,
                SbsRowMeta {
                    left_content,
                    right_content,
                    left_prefix: lp,
                    right_prefix: rp,
                    left_pad_style: left_pad,
                    right_pad_style: right_pad,
                },
            );
        } else {
            lines.push(Line::default());
        }
        line_idx += 1;

        // Add comments for deletion
        if let Some(del_line) = del_opt
            && let Some(old_ln) = del_line.old_lineno
        {
            let (new_line_idx, cursor_info) = add_comments_to_line(
                old_ln,
                line_comments,
                LineSide::Old,
                ctx,
                file_idx,
                line_idx,
                lines,
            );
            line_idx = new_line_idx;
            if cursor_info.is_some() {
                cursor_info_out = cursor_info;
            }
            if let Some(file) = ctx.app.diff_files.get(file_idx) {
                line_idx = add_remote_threads_to_line(
                    old_ln,
                    LineSide::Old,
                    ctx,
                    file.display_path(),
                    line_idx,
                    lines,
                );
            }
        }

        // Add comments for addition
        if let Some(add_line) = add_opt
            && let Some(new_ln) = add_line.new_lineno
        {
            let (new_line_idx, cursor_info) = add_comments_to_line(
                new_ln,
                line_comments,
                LineSide::New,
                ctx,
                file_idx,
                line_idx,
                lines,
            );
            line_idx = new_line_idx;
            if cursor_info.is_some() {
                cursor_info_out = cursor_info;
            }
            if let Some(file) = ctx.app.diff_files.get(file_idx) {
                line_idx = add_remote_threads_to_line(
                    new_ln,
                    LineSide::New,
                    ctx,
                    file.display_path(),
                    line_idx,
                    lines,
                );
            }
        }
    }

    (line_idx, add_end, cursor_info_out)
}

/// Render a standalone addition (no matching deletion)
/// Returns (new_line_idx, optional cursor info for inline comment input)
fn render_standalone_addition_side_by_side(
    diff_line: &crate::model::DiffLine,
    line_comments: &std::collections::HashMap<u32, Vec<crate::model::Comment>>,
    ctx: &SideBySideContext,
    file_idx: usize,
    mut line_idx: usize,
    lines: &mut Vec<Line>,
) -> (usize, Option<SideBySideCursorInfo>) {
    if ctx.is_visible(line_idx) {
        let indicator = cursor_indicator(line_idx, ctx.current_line_idx);

        let mut spans = vec![Span::styled(
            indicator,
            styles::current_line_indicator_style(ctx.theme),
        )];
        add_empty_column_spans(&mut spans, ctx.content_width, ctx.lineno_width);
        spans.push(Span::styled(" │ ", styles::dim_style(ctx.theme)));
        add_addition_spans(
            ctx.theme,
            &mut spans,
            diff_line,
            ctx.content_width,
            ctx.lineno_width,
            ctx.display_lineno(diff_line.new_lineno, line_idx),
            ctx.search_for(line_idx),
        );

        lines.push(Line::from(spans));

        let w = ctx.lineno_width;
        let right_content = content_spans_for_diff_line(
            ctx.theme,
            diff_line,
            LineOrigin::Addition,
            ctx.search_for(line_idx),
        );
        let right_pad = column_pad_style(ctx.theme, diff_line, LineOrigin::Addition);
        let (lp, rp) = sbs_row_prefixes(
            ctx.theme,
            indicator,
            SideSpec {
                lineno: None,
                marker: " ",
                marker_style: Style::default(),
            },
            SideSpec {
                lineno: ctx.display_lineno(diff_line.new_lineno, line_idx),
                marker: "▌",
                marker_style: styles::diff_add_style(ctx.theme),
            },
            w,
        );
        ctx.sbs_meta.borrow_mut().insert(
            line_idx,
            SbsRowMeta {
                left_content: Vec::new(),
                right_content,
                left_prefix: lp,
                right_prefix: rp,
                left_pad_style: Style::default(),
                right_pad_style: right_pad,
            },
        );
    } else {
        lines.push(Line::default());
    }
    line_idx += 1;

    // Add comments if any
    let mut cursor_info_out: Option<SideBySideCursorInfo> = None;
    if let Some(new_ln) = diff_line.new_lineno {
        let (new_line_idx, cursor_info) = add_comments_to_line(
            new_ln,
            line_comments,
            LineSide::New,
            ctx,
            file_idx,
            line_idx,
            lines,
        );
        line_idx = new_line_idx;
        cursor_info_out = cursor_info;
        if let Some(file) = ctx.app.diff_files.get(file_idx) {
            line_idx = add_remote_threads_to_line(
                new_ln,
                LineSide::New,
                ctx,
                file.display_path(),
                line_idx,
                lines,
            );
        }
    }

    (line_idx, cursor_info_out)
}

/// Render a commit-message line in side-by-side mode. The commit message is a
/// synthetic "added" file, but visually it is prose, not code: delta renders it
/// as a full-width block, not confined to a diff column. So we emit a single
/// full-width, left-aligned, neutrally-styled line with no column split, diff
/// coloring, or per-column line numbers. It is deliberately NOT inserted into
/// `sbs_meta`, so the wrap path falls through to the full-width wrapping branch.
fn render_commit_message_line_side_by_side(
    diff_line: &crate::model::DiffLine,
    line_comments: &std::collections::HashMap<u32, Vec<crate::model::Comment>>,
    ctx: &SideBySideContext,
    file_idx: usize,
    mut line_idx: usize,
    lines: &mut Vec<Line>,
) -> (usize, Option<SideBySideCursorInfo>) {
    let ctx_style = styles::diff_context_style(ctx.theme);

    if ctx.is_visible(line_idx) {
        let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
        let mut spans = vec![Span::styled(
            indicator,
            styles::current_line_indicator_style(ctx.theme),
        )];
        // Git-style two-space indent, then the message text at full width.
        spans.push(Span::styled("  ".to_string(), ctx_style));
        spans.push(Span::styled(diff_line.content.clone(), ctx_style));

        lines.push(Line::from(spans));
    } else {
        lines.push(Line::default());
    }
    line_idx += 1;

    let mut cursor_info_out: Option<SideBySideCursorInfo> = None;
    if let Some(new_ln) = diff_line.new_lineno {
        let (new_line_idx, cursor_info) = add_comments_to_line(
            new_ln,
            line_comments,
            LineSide::New,
            ctx,
            file_idx,
            line_idx,
            lines,
        );
        line_idx = new_line_idx;
        cursor_info_out = cursor_info;
        if let Some(file) = ctx.app.diff_files.get(file_idx) {
            line_idx = add_remote_threads_to_line(
                new_ln,
                LineSide::New,
                ctx,
                file.display_path(),
                line_idx,
                lines,
            );
        }
    }

    (line_idx, cursor_info_out)
}

/// Add deletion line spans to the spans vector
fn add_deletion_spans(
    theme: &Theme,
    spans: &mut Vec<Span>,
    diff_line: &crate::model::DiffLine,
    content_width: usize,
    lw: usize,
    display_lineno: Option<u32>,
    search: Option<(&str, Style)>,
) {
    let line_num = display_lineno
        .map(|n| format!("{n:>lw$}"))
        .unwrap_or_else(|| " ".repeat(lw));

    spans.push(Span::styled(
        format!("{line_num} "),
        styles::dim_style(theme),
    ));
    spans.push(Span::styled("▌".to_string(), styles::diff_del_style(theme)));

    // Use syntax highlighting if available
    if let Some(ref highlighted) = diff_line.highlighted_spans {
        let syntax_pad_style = Style::default().fg(theme.diff_del).bg(theme.syntax_del_bg);
        let content_spans =
            searched_cell_spans(highlighted, content_width, syntax_pad_style, search);
        spans.extend(content_spans);
    } else {
        spans.extend(plain_cell_spans(
            &diff_line.content,
            styles::diff_del_style(theme),
            content_width,
            search,
        ));
    }
}

/// Add addition line spans to the spans vector
fn add_addition_spans(
    theme: &Theme,
    spans: &mut Vec<Span>,
    diff_line: &crate::model::DiffLine,
    content_width: usize,
    lw: usize,
    display_lineno: Option<u32>,
    search: Option<(&str, Style)>,
) {
    let line_num = display_lineno
        .map(|n| format!("{n:>lw$}"))
        .unwrap_or_else(|| " ".repeat(lw));

    spans.push(Span::styled(
        format!("{line_num} "),
        styles::dim_style(theme),
    ));
    spans.push(Span::styled("▌".to_string(), styles::diff_add_style(theme)));

    // Use syntax highlighting if available
    if let Some(ref highlighted) = diff_line.highlighted_spans {
        let syntax_pad_style = Style::default().fg(theme.diff_add).bg(theme.syntax_add_bg);
        let content_spans =
            searched_cell_spans(highlighted, content_width, syntax_pad_style, search);
        spans.extend(content_spans);
    } else {
        spans.extend(plain_cell_spans(
            &diff_line.content,
            styles::diff_add_style(theme),
            content_width,
            search,
        ));
    }
}

/// Add empty column spans (for when one side has no content)
fn add_empty_column_spans(spans: &mut Vec<Span>, content_width: usize, lw: usize) {
    // line_num(lw) + space(1) + prefix(1) + content
    spans.push(Span::styled(
        " ".repeat(lw + 1 + 1 + content_width),
        Style::default(),
    ));
}

/// Add comments for a specific line.
/// Returns (new_line_idx, optional cursor info for inline comment input)
/// Render remote review threads anchored at this `(file, line, side)`
/// position into the side-by-side rendering. Mirrors the unified-view
/// helper but uses the side-by-side cursor indicator path.
fn add_remote_threads_to_line(
    line_num: u32,
    side: LineSide,
    ctx: &SideBySideContext,
    file_path: &std::path::Path,
    mut line_idx: usize,
    lines: &mut Vec<Line>,
) -> usize {
    use crate::forge::remote_comments::{PrCommentsVisibility, RemoteCommentSide};
    let visibility = ctx.app.session.remote_comments_visibility;
    if matches!(visibility, PrCommentsVisibility::Hide) {
        return line_idx;
    }
    let target_path = file_path.to_string_lossy();
    for thread in &ctx.app.forge_review_threads {
        let Some(muted) = visibility.render_decision(thread) else {
            continue;
        };
        if thread.path != *target_path {
            continue;
        }
        let Some(thread_line) = thread.line else {
            continue;
        };
        if thread_line != line_num {
            continue;
        }
        let matches_side = matches!(
            (thread.side, side),
            (RemoteCommentSide::Right, LineSide::New) | (RemoteCommentSide::Left, LineSide::Old)
        );
        if !matches_side {
            continue;
        }
        let thread_lines = comment_panel::format_remote_thread_lines(
            ctx.theme,
            thread,
            muted,
            ctx.app.forge_kind(),
        );
        let box_top_row = line_idx;
        for mut comment_line in thread_lines {
            let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
            comment_line.spans.insert(
                0,
                Span::styled(indicator, styles::current_line_indicator_style(ctx.theme)),
            );
            lines.push(comment_line);
            line_idx += 1;
        }
        crate::ui::diff_view::push_comment_bar(
            &mut ctx.comment_bars.borrow_mut(),
            box_top_row,
            Some(crate::model::LineRange::single(thread_line)),
        );
    }
    line_idx
}

/// Push the inline comment-input box for `line_num`, sized like the line's
/// comment boxes, returning the next `line_idx` and the input's cursor info.
/// Called from the reply slot inside the comment loop and from the
/// end-of-line fallback, so the editor renders the same either way.
#[allow(clippy::too_many_arguments)]
fn push_line_comment_input<'a>(
    ctx: &SideBySideContext,
    line_num: u32,
    side_geom: Option<(u16, usize, u16)>,
    is_commit_message: bool,
    box_width: usize,
    left_pad: u16,
    indent_strip: usize,
    line_idx: usize,
    lines: &mut Vec<Line<'a>>,
) -> (usize, SideBySideCursorInfo) {
    let line_range = ctx
        .comment_line_range
        .or_else(|| Some(LineRange::single(line_num)));
    let (input_lines, cursor_info) = comment_panel::format_comment_input_lines(
        ctx.theme,
        comment_type_presentation(ctx.app, &ctx.comment_type),
        ctx.comment_buffer,
        ctx.comment_cursor,
        line_range,
        false,
        box_width,
        ctx.app
            .comment_vim_mode_label()
            .as_ref()
            .map(|(t, w)| (t.as_str(), *w)),
        ctx.app.supports_keyboard_enhancement,
        ctx.app.comment_reply_author().as_deref(),
    );
    let box_top_row = line_idx;
    let box_end = line_idx + input_lines.len().saturating_sub(1);
    let cursor = (
        line_idx + cursor_info.line_offset,
        (1 + left_pad as usize + cursor_info.column as usize - indent_strip) as u16,
        line_idx,
        box_end,
        0,
    );
    let next_line_idx = push_comment_box_lines(
        ctx,
        lines,
        input_lines,
        side_geom,
        is_commit_message,
        box_top_row,
        (!ctx.app.composing_reply()).then_some(line_range).flatten(),
        line_idx,
    );
    (next_line_idx, cursor)
}

fn add_comments_to_line(
    line_num: u32,
    line_comments: &std::collections::HashMap<u32, Vec<crate::model::Comment>>,
    side: LineSide,
    ctx: &SideBySideContext,
    file_idx: usize,
    mut line_idx: usize,
    lines: &mut Vec<Line>,
) -> (usize, Option<SideBySideCursorInfo>) {
    // Check if we're adding/editing a comment on this line and side
    let is_line_comment_mode = ctx.comment_input_mode
        && file_idx == ctx.current_file_idx
        && ctx.comment_line == Some((line_num, side));
    let mut cursor_info_out: Option<SideBySideCursorInfo> = None;

    // Size the comment box to the active side's pane (full width when the pane
    // is too narrow to split). `left_pad` shifts the box under that pane. The
    // commit-message entry renders full-width with no divider, so its comments
    // stay full-width (on the left) rather than being sized to a side.
    let is_commit_message = ctx
        .app
        .diff_files
        .get(file_idx)
        .is_some_and(|f| f.is_commit_message);
    let side_geom = if is_commit_message {
        None
    } else {
        sbs_side_box_geometry(side, ctx.lineno_width, ctx.content_width, ctx.panel_width)
    };
    let box_width = side_geom
        .map(|(_, w, _)| w)
        .unwrap_or_else(|| ctx.panel_width.saturating_sub(1));
    let left_pad: u16 = side_geom.map(|(p, _, _)| p).unwrap_or(0);
    // Side boxes — and the flush-left commit-message box — strip the 4-space
    // indent, so the text cursor shifts left too. `cursor_info.column` already
    // includes the 7-col border prefix, so the full sum stays positive.
    let indent_strip = if side_geom.is_some() || is_commit_message {
        SIDE_BOX_INDENT_WIDTH
    } else {
        0
    };
    let cursor_col = |col: u16| (1 + left_pad as usize + col as usize - indent_strip) as u16;

    // A reply belongs under the thread it answers, not at the bottom of
    // every thread on the line — the line-scope mirror of the file-level
    // reply slot. `comment_idx` matches the annotation model's absolute
    // index into this line's stored comments.
    let reply_slot = if is_line_comment_mode {
        ctx.app.diff_files.get(file_idx).and_then(|file| {
            ctx.app
                .line_comment_reply_slot(file.display_path(), line_num)
        })
    } else {
        None
    };
    let mut reply_input_drawn = false;

    if let Some(comments) = line_comments.get(&line_num) {
        for (comment_idx, comment) in comments.iter().enumerate() {
            let comment_side = comment.side.unwrap_or(LineSide::New);
            if ((side == LineSide::Old && comment_side == LineSide::Old)
                || (side == LineSide::New && comment_side != LineSide::Old))
                && ctx.app.comment_visible(comment)
            {
                // Check if this comment is being edited
                let is_being_edited =
                    is_line_comment_mode && ctx.editing_comment_id == Some(comment.id.as_str());

                if is_being_edited {
                    // Render inline input instead
                    let line_range = ctx
                        .comment_line_range
                        .or_else(|| Some(LineRange::single(line_num)));
                    let (input_lines, cursor_info) = comment_panel::format_comment_input_lines(
                        ctx.theme,
                        comment_type_presentation(ctx.app, &ctx.comment_type),
                        ctx.comment_buffer,
                        ctx.comment_cursor,
                        line_range,
                        true,
                        box_width,
                        ctx.app
                            .comment_vim_mode_label()
                            .as_ref()
                            .map(|(t, w)| (t.as_str(), *w)),
                        ctx.app.supports_keyboard_enhancement,
                        ctx.app.comment_reply_author().as_deref(),
                    );
                    let box_top_row = line_idx;
                    let box_end = line_idx + input_lines.len().saturating_sub(1);
                    // Annotation rows the original comment box occupied — sized
                    // to the same box width the non-editing render (and the
                    // annotation builder) uses, or the offset mapping drifts.
                    let annotations_replaced = App::comment_display_lines_for_box(
                        comment,
                        box_width,
                        ctx.app.thread_collapsed(comment),
                    );
                    cursor_info_out = Some((
                        line_idx + cursor_info.line_offset,
                        cursor_col(cursor_info.column),
                        line_idx,
                        box_end,
                        annotations_replaced,
                    ));
                    line_idx = push_comment_box_lines(
                        ctx,
                        lines,
                        input_lines,
                        side_geom,
                        is_commit_message,
                        box_top_row,
                        crate::ui::diff_view::comment_bar_range(comment, line_range),
                        line_idx,
                    );
                } else {
                    let line_range = comment
                        .line_range
                        .or_else(|| Some(LineRange::single(line_num)));
                    let box_top_row = line_idx;
                    // `box_width` is what this box is formatted at just below —
                    // a side pane's box, not the whole panel. Counting rows at
                    // any other width makes the annotation model disagree with
                    // the page, and every row beneath it addresses the wrong
                    // line.
                    let rows = ctx.app.comment_rows(comment, box_width);
                    // The bar is recorded either way: it is painted above the
                    // box, so it can be on screen while the box itself is not.
                    // Side boxes and commit-message boxes draw no bar, so the
                    // culled path must skip it exactly as the visible one does.
                    if !ctx.box_visible(line_idx, rows) {
                        skip_comment_box(lines, &mut line_idx, rows);
                        if side_geom.is_none() && !is_commit_message {
                            crate::ui::diff_view::push_comment_bar(
                                &mut ctx.comment_bars.borrow_mut(),
                                box_top_row,
                                crate::ui::diff_view::comment_bar_range(comment, line_range),
                            );
                        }
                    } else {
                        let comment_lines = comment_panel::format_comment_lines(
                            ctx.theme,
                            comment_type_presentation(ctx.app, &comment.comment_type),
                            &comment.content,
                            line_range,
                            box_width,
                            comment_panel::CommentBadge::for_comment(comment, &ctx.app.username),
                            ctx.app.thread_display(comment),
                        );
                        line_idx = push_comment_box_lines(
                            ctx,
                            lines,
                            comment_lines,
                            side_geom,
                            is_commit_message,
                            box_top_row,
                            crate::ui::diff_view::comment_bar_range(comment, line_range),
                            line_idx,
                        );
                    }
                }

                // A reply's editor opens in the slot the reply will be
                // stored in — under the thread it answers.
                if is_line_comment_mode
                    && ctx.editing_comment_id.is_none()
                    && reply_slot == Some(comment_idx)
                {
                    let (new_line_idx, cursor_info) = push_line_comment_input(
                        ctx,
                        line_num,
                        side_geom,
                        is_commit_message,
                        box_width,
                        left_pad,
                        indent_strip,
                        line_idx,
                        lines,
                    );
                    line_idx = new_line_idx;
                    cursor_info_out = Some(cursor_info);
                    reply_input_drawn = true;
                }
            }
        }
    }

    // Render inline input for a new line comment, or a reply whose thread
    // is not on this line's rows.
    if is_line_comment_mode && ctx.editing_comment_id.is_none() && !reply_input_drawn {
        let (new_line_idx, cursor_info) = push_line_comment_input(
            ctx,
            line_num,
            side_geom,
            is_commit_message,
            box_width,
            left_pad,
            indent_strip,
            line_idx,
            lines,
        );
        line_idx = new_line_idx;
        cursor_info_out = Some(cursor_info);
    }

    (line_idx, cursor_info_out)
}

#[cfg(test)]
mod remote_comments_side_by_side_snapshot_tests {
    //! Render-snapshot tests for inline remote review threads in the
    //! side-by-side diff view. Confirms the badge appears at least once
    //! when a thread is active and is hidden under `:comments hide`.
    use crate::app::{App, DiffSource, DiffViewMode, InputMode, PullRequestDiffSource};
    use crate::error::Result as TuicrResult;
    use crate::error::TuicrError;
    use crate::forge::remote_comments::{
        PrCommentsVisibility, RemoteCommentSide, RemoteReviewComment, RemoteReviewThread,
    };
    use crate::forge::traits::{ForgeRepository, PrSessionKey};
    use crate::model::{
        DiffFile, DiffHunk, DiffLine, FileStatus, LineOrigin, LineSide, ReviewSession,
        SessionDiffSource,
    };
    use crate::syntax::SyntaxHighlighter;
    use crate::theme::Theme;
    use crate::ui::render;
    use crate::vcs::traits::{VcsBackend, VcsChangeStatus, VcsInfo, VcsType};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use std::path::{Path, PathBuf};

    struct SnapshotVcs {
        info: VcsInfo,
    }

    impl VcsBackend for SnapshotVcs {
        fn info(&self) -> &VcsInfo {
            &self.info
        }
        fn get_working_tree_diff(
            &self,
            _highlighter: &SyntaxHighlighter,
        ) -> TuicrResult<Vec<DiffFile>> {
            Err(TuicrError::NoChanges)
        }
        fn fetch_context_lines(
            &self,
            _file_path: &Path,
            _file_status: FileStatus,
            _ref_commit: Option<&str>,
            _start_line: u32,
            _end_line: u32,
        ) -> TuicrResult<Vec<DiffLine>> {
            Ok(Vec::new())
        }
        fn get_change_status(&self) -> TuicrResult<VcsChangeStatus> {
            Ok(VcsChangeStatus {
                staged: false,
                unstaged: false,
            })
        }
        fn file_line_count(
            &self,
            _file_path: &Path,
            _file_status: FileStatus,
            _ref_commit: Option<&str>,
        ) -> TuicrResult<u32> {
            Ok(0)
        }
    }

    fn repo() -> ForgeRepository {
        ForgeRepository::github("github.com", "agavra", "tuicr")
    }

    fn sample_diff_file() -> DiffFile {
        let lines = vec![
            DiffLine {
                origin: LineOrigin::Context,
                content: "first".to_string(),
                old_lineno: Some(1),
                new_lineno: Some(1),
                highlighted_spans: None,
            },
            DiffLine {
                origin: LineOrigin::Addition,
                content: "second".to_string(),
                old_lineno: None,
                new_lineno: Some(2),
                highlighted_spans: None,
            },
        ];
        let hunk = DiffHunk {
            header: "@@ -1,1 +1,2 @@".to_string(),
            lines,
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 2,
        };
        let hunks = vec![hunk];
        let content_hash = DiffFile::compute_content_hash(&hunks);
        DiffFile {
            old_path: Some(PathBuf::from("src/lib.rs")),
            new_path: Some(PathBuf::from("src/lib.rs")),
            status: FileStatus::Modified,
            hunks,
            is_binary: false,
            is_too_large: false,
            is_commit_message: false,
            content_hash,
        }
    }

    fn thread() -> RemoteReviewThread {
        RemoteReviewThread {
            id: "T".to_string(),
            path: "src/lib.rs".to_string(),
            line: Some(2),
            side: RemoteCommentSide::Right,
            is_resolved: false,
            is_outdated: false,
            comments: vec![RemoteReviewComment {
                id: "C".to_string(),
                author: Some("alice".to_string()),
                body: "sbs hello".to_string(),
                created_at: None,
                in_reply_to: None,
                database_id: None,
                url: "https://example.com".to_string(),
            }],
        }
    }

    fn make_pr_app() -> App {
        make_pr_app_with(vec![sample_diff_file()])
    }

    fn make_pr_app_with(diff_files: Vec<DiffFile>) -> App {
        let pr = PullRequestDiffSource {
            key: PrSessionKey::new(repo(), 125, "headsha".to_string()),
            base_sha: "basesha".to_string(),
            title: "test pr".to_string(),
            url: "https://example.com".to_string(),
            head_ref_name: "feat".to_string(),
            base_ref_name: "main".to_string(),
            state: "OPEN".to_string(),
            closed: false,
            merged: false,
        };
        let vcs_info = VcsInfo {
            root_path: PathBuf::from("forge:github.com/agavra/tuicr"),
            head_commit: "headsha".to_string(),
            branch_name: Some("feat".to_string()),
            vcs_type: VcsType::File,
        };
        let mut session = ReviewSession::new(
            vcs_info.root_path.clone(),
            "headsha".to_string(),
            Some("feat".to_string()),
            SessionDiffSource::PullRequest,
        );
        session.pr_session_key = Some(pr.key.clone());
        let mut app = App::build(
            Box::new(SnapshotVcs {
                info: vcs_info.clone(),
            }),
            vcs_info,
            Theme::dark(),
            None,
            false,
            diff_files,
            session,
            DiffSource::PullRequest(Box::new(pr)),
            InputMode::Normal,
            Vec::new(),
            None,
            None,
        )
        .expect("build app");
        app.diff_view_mode = DiffViewMode::SideBySide;
        app
    }

    /// Side-by-side mirror: the editor is drawn from its own copy of the
    /// placement wiring, and this family of bug has been side-by-side-only
    /// before.
    #[test]
    fn should_draw_a_reply_editor_under_the_thread_it_answers() {
        use crate::model::{Comment, CommentType};
        let mut app = make_pr_app();
        let path = app.diff_files[0].display_path().clone();
        let mut roots = Vec::new();
        for n in 0..3 {
            let root = Comment::new(format!("question {n}"), CommentType::from_id("issue"), None);
            roots.push(root.id.clone());
            app.session
                .get_file_mut(&path)
                .unwrap()
                .file_comments
                .push(root);
        }
        app.rebuild_annotations();

        app.input_mode = InputMode::Comment;
        app.comment_is_file_level = true;
        app.local_reply_target = Some(roots[0].clone());
        app.comment_buffer = "answering the first".to_string();
        let text = body_text(&draw(&mut app));

        let editor = text.find("answering the first").expect("editor on screen");
        let second = text.find("question 1").expect("second thread on screen");
        assert!(
            editor < second,
            "the editor opened below later threads:\n{text}"
        );
    }

    fn draw(app: &mut App) -> Buffer {
        let backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, app))
            .expect("draw frame");
        terminal.backend().buffer().clone()
    }

    fn body_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Side-by-side mirror of the unified culling test: the skip/emit wiring
    /// here goes through `ctx.box_visible` and a by-value `line_idx`, so it
    /// needs its own coverage.
    #[test]
    fn should_cull_comment_boxes_outside_the_viewport() {
        use crate::app::AnnotatedLine;
        use crate::model::{Comment, CommentType};

        const NEEDLE: &str = "far-below-the-fold";

        let lines: Vec<DiffLine> = (1..=120)
            .map(|n| DiffLine {
                origin: LineOrigin::Addition,
                content: format!("line {n}"),
                old_lineno: None,
                new_lineno: Some(n),
                highlighted_spans: None,
            })
            .collect();
        let hunks = vec![DiffHunk {
            header: "@@ -0,0 +1,120 @@".to_string(),
            lines,
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 120,
        }];
        let content_hash = DiffFile::compute_content_hash(&hunks);
        let path = PathBuf::from("src/lib.rs");
        let file = DiffFile {
            old_path: Some(path.clone()),
            new_path: Some(path.clone()),
            status: FileStatus::Modified,
            hunks,
            is_binary: false,
            is_too_large: false,
            is_commit_message: false,
            content_hash,
        };

        let mut app = make_pr_app_with(vec![file]);
        app.session
            .get_file_mut(&path)
            .expect("file registered in session")
            .add_line_comment(
                100,
                Comment::new(NEEDLE.to_string(), CommentType::from_id("note"), None),
            );
        app.rebuild_annotations();

        let body = body_text(&draw(&mut app));
        assert!(
            !body.contains(NEEDLE),
            "off-screen comment should not be visible:\n{body}"
        );

        let comment_row = app
            .line_annotations
            .iter()
            .position(|a| matches!(a, AnnotatedLine::LineComment { .. }))
            .expect("comment annotated in the document");
        app.diff_state.scroll_offset = comment_row;
        app.diff_state.cursor_line = comment_row;

        let body = body_text(&draw(&mut app));
        assert!(
            body.contains(NEEDLE),
            "comment scrolled into view should render at its annotated row:\n{body}"
        );
    }

    #[test]
    fn should_render_remote_comment_inline_in_side_by_side_diff() {
        // given
        let mut app = make_pr_app();
        app.forge_review_threads = vec![thread()];
        app.rebuild_annotations();
        // when
        let buffer = draw(&mut app);
        // then
        let body = body_text(&buffer);
        assert!(
            body.contains("[github @alice]"),
            "expected badge in side-by-side render:\n{body}"
        );
    }

    #[test]
    fn cursor_stays_on_comment_after_a_wrapping_side_box_comment() {
        // A side-box comment wraps at the pane width, so a line long enough
        // to fit unwrapped at full width still takes extra rows in the box.
        // Annotations must size boxes identically or every row below drifts:
        // the cursor placed on the following comment's annotation would draw
        // on a different rendered row, and edit/delete report "no comment at
        // cursor" (regression: editing a comment placed after another
        // author's long comment).
        use crate::model::{Comment, CommentType};

        let mut app = make_pr_app();
        let path = PathBuf::from("src/lib.rs");
        let review = app.session.get_file_mut(&path).expect("file registered");
        review.add_line_comment(
            2,
            Comment::new(
                "x".repeat(100),
                CommentType::from_id("note"),
                Some(LineSide::New),
            ),
        );
        review.add_line_comment(
            2,
            Comment::new(
                "SHORTNOTE".to_string(),
                CommentType::from_id("note"),
                Some(LineSide::New),
            ),
        );
        // First draw establishes the real viewport width and rebuilds
        // annotations at it (sync_viewport_width).
        draw(&mut app);

        let short_annotation = app
            .line_annotations
            .iter()
            .position(|a| {
                matches!(
                    a,
                    crate::app::AnnotatedLine::LineComment { comment_idx: 1, .. }
                )
            })
            .expect("short comment annotated");
        // Row 0 is the box's top border; +1 is the content row.
        app.diff_state.cursor_line = short_annotation + 1;

        let buffer = draw(&mut app);
        let row_text = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        let short_row = (0..buffer.area.height)
            .find(|&y| row_text(y).contains("SHORTNOTE"))
            .expect("short comment rendered");
        assert!(
            row_text(short_row).contains('\u{25b6}'.to_string().as_str()),
            "cursor on the short comment's annotation must render on its box; row: {:?}",
            row_text(short_row)
        );
    }

    #[test]
    fn should_hide_remote_comments_under_comments_hide_in_side_by_side() {
        // given
        let mut app = make_pr_app();
        app.forge_review_threads = vec![thread()];
        app.set_remote_comments_visibility(PrCommentsVisibility::Hide);
        // when
        let buffer = draw(&mut app);
        // then
        let body = body_text(&buffer);
        assert!(
            !body.contains("[github @alice"),
            "remote comment leaked under Hide:\n{body}"
        );
    }

    fn diff_file_with_pair(left: &str, right: &str) -> DiffFile {
        let lines = vec![
            DiffLine {
                origin: LineOrigin::Deletion,
                content: left.to_string(),
                old_lineno: Some(1),
                new_lineno: None,
                highlighted_spans: None,
            },
            DiffLine {
                origin: LineOrigin::Addition,
                content: right.to_string(),
                old_lineno: None,
                new_lineno: Some(1),
                highlighted_spans: None,
            },
        ];
        let hunks = vec![DiffHunk {
            header: "@@ -1,1 +1,1 @@".to_string(),
            lines,
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
        }];
        let content_hash = DiffFile::compute_content_hash(&hunks);
        DiffFile {
            old_path: Some(PathBuf::from("src/lib.rs")),
            new_path: Some(PathBuf::from("src/lib.rs")),
            status: FileStatus::Modified,
            hunks,
            is_binary: false,
            is_too_large: false,
            is_commit_message: false,
            content_hash,
        }
    }

    fn diff_file_with_standalone_deletion(left: &str) -> DiffFile {
        let lines = vec![DiffLine {
            origin: LineOrigin::Deletion,
            content: left.to_string(),
            old_lineno: Some(1),
            new_lineno: None,
            highlighted_spans: None,
        }];
        let hunks = vec![DiffHunk {
            header: "@@ -1,1 +0,0 @@".to_string(),
            lines,
            old_start: 1,
            old_count: 1,
            new_start: 0,
            new_count: 0,
        }];
        let content_hash = DiffFile::compute_content_hash(&hunks);
        DiffFile {
            old_path: Some(PathBuf::from("src/lib.rs")),
            new_path: Some(PathBuf::from("src/lib.rs")),
            status: FileStatus::Modified,
            hunks,
            is_binary: false,
            is_too_large: false,
            is_commit_message: false,
            content_hash,
        }
    }

    fn draw_sbs(app: &mut App, w: u16, h: u16) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::render_side_by_side_diff(frame, app, Rect::new(0, 0, w, h)))
            .expect("draw sbs");
        terminal.backend().buffer().clone()
    }

    fn char_at(buf: &Buffer, x: u16, y: u16) -> String {
        buf[(x, y)].symbol().to_string()
    }

    #[test]
    fn should_wrap_long_line_in_side_by_side_view_when_wrap_enabled() {
        let long_left = "L".repeat(200);
        let mut app = make_pr_app();
        app.diff_files = vec![diff_file_with_pair(&long_left, "short")];
        app.set_diff_wrap(true);
        app.rebuild_annotations();

        let buf = draw_sbs(&mut app, 160, 20);

        let mut rows_with_l = 0u16;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| char_at(&buf, x, y)).collect();
            if row.contains("LLLLLLLLLL") {
                rows_with_l += 1;
            }
        }
        assert!(
            rows_with_l >= 2,
            "expected long left content to span >=2 visual rows, got {rows_with_l}"
        );
    }

    #[test]
    fn should_not_wrap_when_wrap_disabled_in_side_by_side() {
        let long_left = "L".repeat(200);
        let mut app = make_pr_app();
        app.diff_files = vec![diff_file_with_pair(&long_left, "short")];
        app.set_diff_wrap(false);
        app.rebuild_annotations();

        let buf = draw_sbs(&mut app, 160, 20);

        let rows_with_l: u16 = (0..buf.area.height)
            .filter(|&y| {
                (0..buf.area.width)
                    .map(|x| char_at(&buf, x, y))
                    .collect::<String>()
                    .contains("LLLLLLLLLL")
            })
            .count() as u16;
        assert_eq!(
            rows_with_l, 1,
            "wrap-off should produce exactly one row of L, got {rows_with_l}"
        );
    }

    fn add_side_comment(app: &mut App, line: u32, side: LineSide, text: &str) {
        app.session.add_diff_file(&app.diff_files[0]);
        let pb = PathBuf::from("src/lib.rs");
        app.session
            .get_file_mut(&pb)
            .expect("file in session")
            .add_line_comment(
                line,
                crate::model::Comment::new(
                    text.to_string(),
                    crate::model::CommentType::from_id("note"),
                    Some(side),
                ),
            );
    }

    // Geometry mirrors the divider tests: bordered block (inner.x = 1), single
    // digit line numbers (lw = 1), inner width 158 for a 160-wide buffer.
    fn sbs_divider_col() -> usize {
        let lw = 1usize;
        let inner_w = 158usize;
        let content_width = (inner_w - crate::app::sbs_overhead(lw) as usize) / 2;
        1 + crate::app::sbs_left_gutter(lw) as usize + content_width + 1
    }

    fn diff_file_with_context(text: &str) -> DiffFile {
        let lines = vec![DiffLine {
            origin: LineOrigin::Context,
            content: text.to_string(),
            old_lineno: Some(1),
            new_lineno: Some(1),
            highlighted_spans: None,
        }];
        let hunks = vec![DiffHunk {
            header: "@@ -1,1 +1,1 @@".to_string(),
            lines,
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
        }];
        let content_hash = DiffFile::compute_content_hash(&hunks);
        DiffFile {
            old_path: Some(PathBuf::from("src/lib.rs")),
            new_path: Some(PathBuf::from("src/lib.rs")),
            status: FileStatus::Modified,
            hunks,
            is_binary: false,
            is_too_large: false,
            is_commit_message: false,
            content_hash,
        }
    }

    #[test]
    fn can_comment_on_old_side_of_context_line() {
        // Regression: an old-side comment on an unchanged (context) line
        // rendered no box (only the new side was handled), leaving comment mode
        // stuck. It must now render the input box.
        let mut app = make_pr_app();
        app.diff_files = vec![diff_file_with_context("unchanged line")];
        app.rebuild_annotations();
        app.cursor_side = LineSide::Old;
        app.enter_comment_mode(false, Some((1, LineSide::Old)));
        app.comment_buffer = "OLDCONTEXTNOTE".to_string();
        app.comment_cursor = app.comment_buffer.len();

        let buf = draw_sbs(&mut app, 160, 20);

        let rendered = (0..buf.area.height).any(|y| {
            (0..buf.area.width)
                .map(|x| char_at(&buf, x, y))
                .collect::<String>()
                .contains("OLDCONTEXTNOTE")
        });
        assert!(
            rendered,
            "old-side context comment input box did not render"
        );
    }

    #[test]
    fn new_side_comment_box_stays_in_right_pane() {
        let mut app = make_pr_app();
        app.diff_files = vec![diff_file_with_pair("old text", "new text")];
        app.cursor_side = LineSide::New;
        add_side_comment(&mut app, 1, LineSide::New, "RIGHTSIDECOMMENT");
        app.rebuild_annotations();

        let buf = draw_sbs(&mut app, 160, 20);
        let divider = sbs_divider_col();

        let mut found = false;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| char_at(&buf, x, y)).collect();
            if !row.contains("RIGHTSIDECOMMENT") {
                continue;
            }
            found = true;
            let left: String = (1..divider).map(|x| char_at(&buf, x as u16, y)).collect();
            assert!(
                left.trim().is_empty(),
                "new-side comment must not bleed into the left pane on row {y}: {left:?}"
            );
        }
        assert!(found, "new-side comment text was not rendered");
    }

    #[test]
    fn old_side_comment_box_stays_in_left_pane() {
        let mut app = make_pr_app();
        app.diff_files = vec![diff_file_with_pair("old text", "new text")];
        app.cursor_side = LineSide::Old;
        add_side_comment(&mut app, 1, LineSide::Old, "LEFTSIDECOMMENT");
        app.rebuild_annotations();

        let buf = draw_sbs(&mut app, 160, 20);
        let divider = sbs_divider_col();

        let mut found = false;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| char_at(&buf, x, y)).collect();
            if !row.contains("LEFTSIDECOMMENT") {
                continue;
            }
            found = true;
            // Exclude the panel's right frame border (last column).
            let right: String = (divider..(buf.area.width as usize - 1))
                .map(|x| char_at(&buf, x as u16, y))
                .collect();
            assert!(
                right.trim().is_empty(),
                "old-side comment must not bleed into the right pane on row {y}: {right:?}"
            );
        }
        assert!(found, "old-side comment text was not rendered");
    }

    #[test]
    fn should_pan_both_columns_when_wrap_is_disabled() {
        let mut app = make_pr_app();
        app.diff_files = vec![diff_file_with_pair(
            &format!("0000LEFT{}", "L".repeat(100)),
            &format!("0000RIGHT{}", "R".repeat(100)),
        )];
        app.set_diff_wrap(false);
        app.rebuild_annotations();

        let _ = draw_sbs(&mut app, 80, 20);
        app.scroll_right(4);
        let buf = draw_sbs(&mut app, 80, 20);

        assert_eq!(app.diff_state.scroll_x, 4);
        let body = body_text(&buf);
        assert!(body.contains("LEFT"), "left column did not pan:\n{body}");
        assert!(body.contains("RIGHT"), "right column did not pan:\n{body}");
        assert!(
            !body.contains("0000LEFT"),
            "left column stayed put:\n{body}"
        );
        assert!(
            !body.contains("0000RIGHT"),
            "right column stayed put:\n{body}"
        );

        let row = (0..buf.area.height)
            .find(|&y| {
                (0..buf.area.width)
                    .map(|x| char_at(&buf, x, y))
                    .collect::<String>()
                    .contains("LEFT")
            })
            .expect("panned diff row");
        let lw = app.lineno_width();
        let content_width = (78 - crate::app::sbs_overhead(lw) as usize) / 2;
        let left_start = 1 + crate::app::sbs_left_gutter(lw);
        let divider = left_start + content_width as u16 + 1;
        let right_start = left_start + content_width as u16 + lw as u16 + 5;
        assert_eq!(char_at(&buf, left_start, row), "L");
        assert_eq!(char_at(&buf, divider, row), "│");
        assert_eq!(char_at(&buf, right_start, row), "R");

        app.enter_comment_mode(false, Some((1, LineSide::New)));
        app.comment_buffer = "COMMENT".to_string();
        app.comment_cursor = app.comment_buffer.len();
        let buf = draw_sbs(&mut app, 80, 20);
        let (cursor_x, cursor_y) = app.comment_cursor_screen_pos.expect("comment cursor");
        assert_eq!(app.diff_state.scroll_x, 4);
        assert!(body_text(&buf).contains("COMMENT"));
        assert_eq!(char_at(&buf, cursor_x - 1, cursor_y), "T");
    }

    #[test]
    fn should_align_divider_on_wrapped_rows_in_side_by_side() {
        let long_left = "L".repeat(200);
        let mut app = make_pr_app();
        app.diff_files = vec![diff_file_with_pair(&long_left, "short")];
        app.set_diff_wrap(true);
        app.rebuild_annotations();

        let lw = 1usize;
        let inner_w = 158usize;
        let content_width = (inner_w - crate::app::sbs_overhead(lw) as usize) / 2;
        let divider_x_inner = crate::app::sbs_left_gutter(lw) as usize + content_width;
        let divider_glyph_x = 1 + divider_x_inner + 1;

        let buf = draw_sbs(&mut app, 160, 20);

        let mut rows_with_l: Vec<u16> = Vec::new();
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| char_at(&buf, x, y)).collect();
            if row.contains("LLLLLLLLLL") {
                rows_with_l.push(y);
            }
        }
        assert!(
            rows_with_l.len() >= 2,
            "expected ≥2 wrapped rows, got {}",
            rows_with_l.len()
        );
        for y in &rows_with_l {
            let glyph = char_at(&buf, divider_glyph_x as u16, *y);
            assert_eq!(
                glyph, "│",
                "expected │ at col {divider_glyph_x} on row {y}, got {glyph:?}"
            );
        }
    }

    #[test]
    fn should_pad_shorter_column_on_wrapped_rows_in_side_by_side() {
        let long_left = "L".repeat(200);
        let mut app = make_pr_app();
        app.diff_files = vec![diff_file_with_standalone_deletion(&long_left)];
        app.set_diff_wrap(true);
        app.rebuild_annotations();

        let buf = draw_sbs(&mut app, 160, 20);

        let lw = 1usize;
        let inner_w = 158usize;
        let content_width = (inner_w - crate::app::sbs_overhead(lw) as usize) / 2;
        let divider_glyph_x = 1 + crate::app::sbs_left_gutter(lw) as usize + content_width + 1;
        let right_content_start = divider_glyph_x + 2 + lw + 1 + 1;
        let right_content_end = right_content_start + content_width;

        let mut checked = 0;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| char_at(&buf, x, y)).collect();
            if !row.contains("LLLLLLLLLL") {
                continue;
            }
            checked += 1;
            let right: String = (right_content_start..right_content_end)
                .map(|x| char_at(&buf, x as u16, y))
                .collect();
            assert!(
                right.trim().is_empty(),
                "right column should be blank on wrapped L row {y}, got {right:?}"
            );
        }
        assert!(
            checked >= 2,
            "expected ≥2 wrapped rows to check, got {checked}"
        );
    }

    fn commit_message_file(message: &str) -> DiffFile {
        let lines: Vec<DiffLine> = message
            .lines()
            .enumerate()
            .map(|(i, line)| DiffLine {
                origin: LineOrigin::Context,
                content: line.to_string(),
                old_lineno: None,
                new_lineno: Some(i as u32 + 1),
                highlighted_spans: None,
            })
            .collect();
        let new_count = lines.len() as u32;
        let hunks = vec![DiffHunk {
            header: String::new(),
            lines,
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count,
        }];
        let content_hash = DiffFile::compute_content_hash(&hunks);
        DiffFile {
            old_path: None,
            new_path: Some(PathBuf::from("Commit Message (abc1234)")),
            status: FileStatus::Added,
            hunks,
            is_binary: false,
            is_too_large: false,
            is_commit_message: true,
            content_hash,
        }
    }

    #[test]
    fn should_render_commit_message_full_width_in_side_by_side() {
        let mut app = make_pr_app();
        app.diff_files = vec![commit_message_file("COMMITMSG summary line")];
        app.rebuild_annotations();

        let buf = draw_sbs(&mut app, 160, 20);

        let mut checked = 0;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| char_at(&buf, x, y)).collect();
            let Some(col) = row.find("COMMITMSG") else {
                continue;
            };
            checked += 1;
            // Full-width prose: rendered near the left edge (small indent), not
            // pushed into the right diff column, and with no column divider.
            assert!(
                col < 8,
                "commit message should start near the left edge, got col {col} on row {y}: {row:?}"
            );
            assert!(
                !row.contains(" │ "),
                "commit message row should not have a column divider on row {y}: {row:?}"
            );
        }
        assert_eq!(
            checked, 1,
            "expected the commit message body to render exactly once, got {checked}"
        );
    }

    fn add_commit_message_comment(app: &mut App, line: u32, text: &str) {
        app.session.add_diff_file(&app.diff_files[0]);
        let pb = app.diff_files[0].display_path().clone();
        app.session
            .get_file_mut(&pb)
            .expect("file in session")
            .add_line_comment(
                line,
                crate::model::Comment::new(
                    text.to_string(),
                    crate::model::CommentType::from_id("note"),
                    Some(LineSide::New),
                ),
            );
    }

    #[test]
    fn comment_bar_does_not_cover_commit_message_text_in_side_by_side() {
        // Regression: the connector bar is painted at a fixed gutter column
        // (inner.x + 5). For the full-width commit message, whose prose renders
        // near the left edge, that column landed on the 3rd character of the
        // message and hid it. The commit-message comment box is flush-left with
        // no connector bar, so the message text stays intact.
        let mut app = make_pr_app();
        app.diff_files = vec![commit_message_file("COMMITMSG summary line")];
        add_commit_message_comment(&mut app, 1, "NOTEONMSG");
        app.rebuild_annotations();

        let buf = draw_sbs(&mut app, 160, 20);

        let msg_intact = (0..buf.area.height).any(|y| {
            (0..buf.area.width)
                .map(|x| char_at(&buf, x, y))
                .collect::<String>()
                .contains("COMMITMSG summary line")
        });
        assert!(
            msg_intact,
            "commit message text must stay intact (not overwritten by a connector bar)"
        );

        let box_rendered = (0..buf.area.height).any(|y| {
            (0..buf.area.width)
                .map(|x| char_at(&buf, x, y))
                .collect::<String>()
                .contains("NOTEONMSG")
        });
        assert!(box_rendered, "the commit-message comment box should render");
    }

    /// Regression: the block character cursor took the New pane's geometry on
    /// commit-message rows — their side is always New — and painted in the
    /// right pane while the message renders full-width on the left.
    #[test]
    fn block_cursor_stays_on_full_width_commit_message_row() {
        use ratatui::style::Modifier;
        let mut app = make_pr_app_with(vec![commit_message_file("COMMITMSG summary line")]);
        app.rebuild_annotations();
        let target = app
            .line_annotations
            .iter()
            .position(|a| matches!(a, crate::app::AnnotatedLine::SideBySideLine { .. }))
            .expect("message line annotation");
        app.move_cursor_to_annotation(target);

        let buf = draw_sbs(&mut app, 160, 20);
        let (y, msg_col) = (0..buf.area.height)
            .find_map(|y| {
                let row: String = (0..buf.area.width).map(|x| char_at(&buf, x, y)).collect();
                // Cell column, not byte offset: the border/caret glyphs are
                // multibyte.
                row.find("COMMITMSG")
                    .map(|byte| (y, row[..byte].chars().count() as u16))
            })
            .expect("message row on screen");
        let cursor_cells: Vec<u16> = (0..buf.area.width)
            .filter(|&x| {
                buf[(x, y)]
                    .style()
                    .add_modifier
                    .contains(Modifier::REVERSED)
            })
            .collect();
        assert_eq!(
            cursor_cells,
            vec![msg_col],
            "block cursor must sit on the message's first character, not in the right pane"
        );
    }

    /// The wrap continuation rows of the full-width commit message start at
    /// the left edge — no pane, no indent — so the block cursor's cell count
    /// must not scan them from the first row's indent.
    #[test]
    fn block_cursor_lands_on_wrapped_commit_message_continuation_row() {
        use ratatui::style::Modifier;
        let long = format!("HEAD{}TAIL", "x".repeat(200));
        let mut app = make_pr_app_with(vec![commit_message_file(&long)]);
        app.set_diff_wrap(true);
        app.rebuild_annotations();
        let target = app
            .line_annotations
            .iter()
            .position(|a| matches!(a, crate::app::AnnotatedLine::SideBySideLine { .. }))
            .expect("message line annotation");
        app.move_cursor_to_annotation(target);
        let inner_x = 1u16; // frame border
        let inner_w = 158u16;
        // First visual row holds inner_w minus the indicator + two-space
        // indent; put the cursor 15 chars into the continuation row.
        let first_row_chars = (inner_w - 3) as usize;
        app.diff_state.cursor_col = first_row_chars + 15;

        let buf = draw_sbs(&mut app, 160, 20);
        let mut cursor_cells = Vec::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)]
                    .style()
                    .add_modifier
                    .contains(Modifier::REVERSED)
                {
                    cursor_cells.push((x, y));
                }
            }
        }
        let first_row = (0..buf.area.height)
            .find(|&y| {
                (0..buf.area.width)
                    .map(|x| char_at(&buf, x, y))
                    .collect::<String>()
                    .contains("HEAD")
            })
            .expect("wrapped message on screen");
        assert_eq!(
            cursor_cells,
            vec![(inner_x + 15, first_row + 1)],
            "block cursor must land on the continuation row's own cell"
        );
    }

    /// Same geometry, taken by the visual-selection overlay: `v` on the
    /// commit message must highlight the prose on the left, not the New pane.
    #[test]
    fn visual_selection_stays_on_full_width_commit_message_row() {
        let mut app = make_pr_app_with(vec![commit_message_file("COMMITMSG summary line")]);
        app.rebuild_annotations();
        let target = app
            .line_annotations
            .iter()
            .position(|a| matches!(a, crate::app::AnnotatedLine::SideBySideLine { .. }))
            .expect("message line annotation");
        app.move_cursor_to_annotation(target);
        app.enter_visual_char_mode_at_cursor();

        let sel_bg = crate::ui::styles::visual_selection_style(&app.theme).bg;
        let buf = draw_sbs(&mut app, 160, 20);
        let y = (0..buf.area.height)
            .find(|&y| {
                (0..buf.area.width)
                    .map(|x| char_at(&buf, x, y))
                    .collect::<String>()
                    .contains("COMMITMSG")
            })
            .expect("message row on screen");
        let selected: Vec<u16> = (0..buf.area.width)
            .filter(|&x| buf[(x, y)].style().bg == sel_bg)
            .collect();
        let divider = sbs_divider_col() as u16;
        assert!(
            !selected.is_empty(),
            "the visual selection should be visible on the message row"
        );
        assert!(
            selected.iter().all(|&x| x < divider),
            "selection must stay on the full-width prose, got cells at {selected:?}"
        );
    }

    /// Root + one agent reply on `line`, tagged so tests can find the rows.
    /// Returns the reply's id.
    fn add_line_thread(app: &mut App, line: u32, tag: &str) -> String {
        use crate::model::{Comment, CommentType};
        let path = app.diff_files[0].display_path().clone();
        let mut root = Comment::new(
            format!("{tag} root"),
            CommentType::from_id("issue"),
            Some(LineSide::New),
        );
        root.line_range = Some(crate::model::LineRange::single(line));
        let mut reply = Comment::new(
            format!("{tag} agent reply"),
            CommentType::None,
            Some(LineSide::New),
        );
        reply.in_reply_to = Some(root.id.clone());
        reply.author = "Claude".to_string();
        let reply_id = reply.id.clone();
        let review = app.session.get_file_mut(&path).expect("file in session");
        review.add_line_comment(line, root);
        review.add_line_comment(line, reply);
        reply_id
    }

    /// With several threads piled on one line — what re-anchoring leaves
    /// behind — a reply to the first thread must not open its editor under
    /// the last one.
    #[test]
    fn line_reply_editor_opens_under_the_thread_it_answers() {
        let mut app = make_pr_app();
        app.session.add_diff_file(&app.diff_files[0]);
        let a_reply = add_line_thread(&mut app, 2, "THREAD-A");
        add_line_thread(&mut app, 2, "THREAD-B");
        app.rebuild_annotations();

        // `c` from thread A's last row: reply to the agent's message.
        let target = app
            .line_annotations
            .iter()
            .position(|a| {
                matches!(
                    a,
                    crate::app::AnnotatedLine::LineComment { comment_idx: 1, .. }
                )
            })
            .expect("thread A reply row");
        app.move_cursor_to_annotation(target);
        app.enter_local_reply_mode();
        assert_eq!(app.local_reply_target.as_deref(), Some(a_reply.as_str()));
        app.comment_buffer = "ANSWERINGTHREADA".to_string();

        let text = body_text(&draw(&mut app));
        let a_reply_at = text
            .find("THREAD-A agent reply")
            .expect("thread A on screen");
        let editor_at = text.find("ANSWERINGTHREADA").expect("editor on screen");
        let b_root_at = text.find("THREAD-B root").expect("thread B on screen");
        assert!(
            a_reply_at < editor_at && editor_at < b_root_at,
            "the reply editor must open inside thread A, not under thread B:\n{text}"
        );
    }

    /// The editor opens in the slot the reply is stored in: after saving,
    /// the reply renders exactly where the editor was.
    #[test]
    fn saved_line_reply_lands_where_its_editor_was() {
        let mut app = make_pr_app();
        app.session.add_diff_file(&app.diff_files[0]);
        add_line_thread(&mut app, 2, "THREAD-A");
        add_line_thread(&mut app, 2, "THREAD-B");
        app.rebuild_annotations();
        let target = app
            .line_annotations
            .iter()
            .position(|a| {
                matches!(
                    a,
                    crate::app::AnnotatedLine::LineComment { comment_idx: 1, .. }
                )
            })
            .expect("thread A reply row");
        app.move_cursor_to_annotation(target);
        app.enter_local_reply_mode();
        app.comment_buffer = "ANSWERINGTHREADA".to_string();
        app.save_comment();

        let text = body_text(&draw(&mut app));
        let a_reply_at = text
            .find("THREAD-A agent reply")
            .expect("thread A on screen");
        let saved_at = text
            .find("ANSWERINGTHREADA")
            .expect("saved reply on screen");
        let b_root_at = text.find("THREAD-B root").expect("thread B on screen");
        assert!(
            a_reply_at < saved_at && saved_at < b_root_at,
            "the saved reply must land in thread A, where its editor was:\n{text}"
        );
    }

    /// When the answered thread's slot row is hidden (settled thread, replies
    /// collapsed away), the editor still draws — once, at the end of the
    /// line's comments, like the file-level fallback.
    #[test]
    fn line_reply_editor_falls_back_when_thread_slot_is_hidden() {
        let mut app = make_pr_app();
        app.session.add_diff_file(&app.diff_files[0]);
        let a_reply = add_line_thread(&mut app, 2, "THREAD-A");
        add_line_thread(&mut app, 2, "THREAD-B");
        {
            let path = app.diff_files[0].display_path().clone();
            let review = app.session.get_file_mut(&path).expect("file");
            for c in review.line_comments.get_mut(&2).expect("comments") {
                if c.id == a_reply || c.in_reply_to.is_none() && c.content.starts_with("THREAD-A") {
                    c.resolved = true;
                }
            }
        }
        app.show_resolved_threads = false;
        app.rebuild_annotations();

        app.input_mode = InputMode::Comment;
        app.comment_is_file_level = false;
        app.comment_line = Some((2, LineSide::New));
        app.local_reply_target = Some(a_reply);
        app.comment_buffer = "FALLBACKREPLY".to_string();

        let text = body_text(&draw(&mut app));
        assert_eq!(
            text.matches("FALLBACKREPLY").count(),
            1,
            "the editor must draw exactly once:\n{text}"
        );
    }

    /// Review-scope mirror: a reply to the first review thread must not open
    /// its editor under the last one.
    #[test]
    fn review_reply_editor_opens_under_the_thread_it_answers() {
        use crate::model::{Comment, CommentType};
        let mut app = make_pr_app();
        let mut ids = Vec::new();
        for tag in ["REVIEW-A", "REVIEW-B"] {
            let root = Comment::new(format!("{tag} root"), CommentType::from_id("issue"), None);
            let mut reply = Comment::new(format!("{tag} agent reply"), CommentType::None, None);
            reply.in_reply_to = Some(root.id.clone());
            reply.author = "Claude".to_string();
            ids.push(reply.id.clone());
            app.session.review_comments.push(root);
            app.session.review_comments.push(reply);
        }
        app.rebuild_annotations();

        app.input_mode = InputMode::Comment;
        app.comment_is_review_level = true;
        app.local_reply_target = Some(ids[0].clone());
        app.comment_buffer = "ANSWERINGREVIEWA".to_string();

        let text = body_text(&draw(&mut app));
        let a_reply_at = text
            .find("REVIEW-A agent reply")
            .expect("thread A on screen");
        let editor_at = text.find("ANSWERINGREVIEWA").expect("editor on screen");
        let b_root_at = text.find("REVIEW-B root").expect("thread B on screen");
        assert!(
            a_reply_at < editor_at && editor_at < b_root_at,
            "the review reply editor must open inside thread A:\n{text}"
        );
    }
}
