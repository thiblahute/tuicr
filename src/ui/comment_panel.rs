use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::App;
use crate::forge::traits::ForgeKind;
use crate::model::LineRange;
use crate::theme::Theme;
use crate::ui::styles;

/// Content prefix used on every comment-body line. 4 pad chars + `│` + 2 spaces.
/// The 4-char left pad lets the bar painter draw `│` up through diff lines
/// without colliding with the col-6 `▌` add/del prefix.
const BORDER_PREFIX: &str = "    │  ";
const BORDER_PREFIX_WIDTH: usize = 7;

/// Split `text` into segments whose display width each fits within `content_area`.
/// Returns a single-element vec when the text already fits. The returned slices
/// borrow from `text`, so the caller must keep `text` alive while iterating.
pub(crate) fn wrap_segments(text: &str, content_area: usize) -> Vec<&str> {
    if content_area == 0 || text.width() <= content_area {
        return vec![text];
    }
    let mut segments = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        let mut take_bytes = 0usize;
        let mut taken_width = 0usize;
        for c in remaining.chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if taken_width + cw > content_area {
                break;
            }
            taken_width += cw;
            take_bytes += c.len_utf8();
        }
        // Single character wider than content_area — emit it anyway so we don't loop forever
        if take_bytes == 0 {
            take_bytes = remaining.chars().next().map_or(0, |c| c.len_utf8());
        }
        let (seg, rest) = remaining.split_at(take_bytes);
        segments.push(seg);
        remaining = rest;
    }
    segments
}

/// Emit spans covering `line_text[start..end)` using the per-line markdown
/// highlight `runs` (concatenation of run text equals `line_text`). Falls back
/// to a single unstyled span when highlighting is unavailable. Offsets are byte
/// positions; callers pass char-boundary offsets (segment/cursor boundaries).
fn highlighted_window_spans(
    runs: Option<&[(Style, String)]>,
    line_text: &str,
    start: usize,
    end: usize,
) -> Vec<Span<'static>> {
    if start >= end {
        return Vec::new();
    }
    let Some(runs) = runs else {
        return vec![Span::raw(line_text[start..end].to_string())];
    };
    let mut out = Vec::new();
    let mut run_start = 0usize;
    for (style, text) in runs {
        let run_end = run_start + text.len();
        let lo = start.max(run_start);
        let hi = end.min(run_end);
        if lo < hi {
            out.push(Span::styled(
                text[lo - run_start..hi - run_start].to_string(),
                *style,
            ));
        }
        run_start = run_end;
        if run_start >= end {
            break;
        }
    }
    out
}

/// Information about where the cursor should be positioned within comment input
#[derive(Debug, Clone)]
pub struct CommentCursorInfo {
    /// Which line within the formatted output contains the cursor (0-indexed, relative to content start)
    /// This is the line index within the Vec<Line> returned by format_comment_input_lines,
    /// where 0 = header line, 1+ = content lines, last = footer line.
    /// The cursor is only on content lines (1 to n-2 inclusive for n total lines).
    pub line_offset: usize,
    /// Column offset (display width) from start of line where cursor should be
    pub column: u16,
}

#[derive(Debug, Clone)]
pub struct CommentTypePresentation {
    pub label: String,
    pub color: Color,
}

/// Format a comment input as multiple lines with a box border for inline editing.
/// This mimics the normal comment display but shows it's being edited.
///
/// Returns a tuple of (lines, cursor_info) where cursor_info contains the position
/// of the cursor within the formatted output for IME positioning.
#[allow(clippy::too_many_arguments)]
pub fn format_comment_input_lines(
    theme: &Theme,
    comment_type: CommentTypePresentation,
    buffer: &str,
    cursor_pos: usize,
    line_range: Option<LineRange>,
    is_editing: bool,
    width: usize,
    vim_mode: Option<(&str, bool)>,
    supports_keyboard_enhancement: bool,
    reply_to: Option<&str>,
) -> (Vec<Line<'static>>, CommentCursorInfo) {
    let type_style = styles::comment_type_style(theme, comment_type.color);
    let border_style = styles::comment_border_style(theme, comment_type.color);
    let cursor_style = Style::default()
        .fg(theme.cursor_color)
        .add_modifier(Modifier::UNDERLINED);

    let action = if reply_to.is_some() {
        "Reply to"
    } else if is_editing {
        "Edit"
    } else {
        "Add"
    };
    let line_info = match line_range {
        Some(range) if range.is_single() => format!("L{} ", range.start),
        Some(range) => format!("L{}-L{} ", range.start, range.end),
        None => String::new(),
    };

    let newline_hint = if supports_keyboard_enhancement {
        "Shift-Enter"
    } else {
        "Alt-Enter"
    };

    // "    │  " is the per-line content prefix; everything past that is content.
    // Subtract two extra: one so ratatui never wraps an exact-fit line, and
    // one so the terminal cursor at end-of-segment stays clear of the border.
    let content_area = width.saturating_sub(BORDER_PREFIX_WIDTH + 2);

    let mut result = Vec::new();
    let mut cursor_line_offset: usize = 1;
    let mut cursor_column: u16 = BORDER_PREFIX_WIDTH as u16;

    // Top-left corner becomes `├` when a line range is present — the bar
    // painter then draws `│` going up through the range and a `╭` at the
    // topmost covered line, so the tee reads as the bar joining the box.
    let top_corner = if line_range.is_some() { '├' } else { '╭' };
    let top_prefix = format!("    {top_corner}── ");

    // Top border with type label and hints. In vim mode the hints describe the
    // modal bindings and a `[MODE]` tag is shown after the type label.
    // Replies have no type, so their hint drops the Tab type-cycling part.
    let hint = match (vim_mode, reply_to) {
        (Some(_), _) => {
            "(i:insert  Alt-Enter:save  Esc:normal  PgUp/PgDn:scroll  :w save  :q discard)"
                .to_string()
        }
        (None, Some(_)) => {
            format!("(Enter:send {newline_hint}:newline PgUp/PgDn:scroll Esc:cancel)")
        }
        (None, None) => {
            format!(
                "(Tab/S-Tab:type Enter:save {newline_hint}:newline PgUp/PgDn:scroll Esc:cancel)"
            )
        }
    };
    let mut header_spans = vec![
        Span::styled(top_prefix, border_style),
        Span::styled(format!("{action} "), styles::dim_style(theme)),
    ];
    if let Some(author) = reply_to {
        header_spans.push(Span::styled(format!("@{author} "), type_style));
    }
    // `None` has an empty label — show no `[TYPE]` badge while composing.
    if !comment_type.label.is_empty() {
        header_spans.push(Span::styled(
            format!("[{}] ", comment_type.label),
            type_style,
        ));
    }
    if let Some((mode, warn)) = vim_mode {
        // The cancel-confirm hint is painted red to flag the destructive action.
        let mode_style = if warn {
            Style::default()
                .fg(theme.comment_issue)
                .add_modifier(Modifier::BOLD)
        } else {
            type_style
        };
        header_spans.push(Span::styled(format!("[{mode}] "), mode_style));
    }
    header_spans.push(Span::styled(line_info, styles::dim_style(theme)));
    header_spans.push(Span::styled(hint, styles::dim_style(theme)));
    result.push(Line::from(header_spans));

    // Content lines with cursor
    if buffer.is_empty() {
        // Show placeholder with cursor at start
        result.push(Line::from(vec![
            Span::styled(BORDER_PREFIX, border_style),
            Span::styled(" ", cursor_style),
            Span::styled("Type your comment...", styles::dim_style(theme)),
        ]));
        // cursor_line_offset is already 1 (first content line)
        // cursor_column is already BORDER_PREFIX_WIDTH (cursor at start of content)
    } else {
        let buffer_lines: Vec<&str> = buffer.split('\n').collect();
        // Markdown-highlight the in-progress text; colors come from the active
        // syntect theme (same engine/theme as diff code highlighting).
        let highlighted = theme.syntax_highlighter().highlight_markdown_body(buffer);
        let mut byte_offset = 0;
        // Tracks how many visual lines have been pushed so far (not counting the header).
        let mut total_visual_lines: usize = 0;

        for (line_idx, text) in buffer_lines.iter().enumerate() {
            let line_start = byte_offset;
            let line_end = byte_offset + text.len();
            let is_last_logical = line_idx + 1 == buffer_lines.len();

            // Check if cursor is on this line
            let cursor_on_this_line = cursor_pos >= line_start
                && (cursor_pos <= line_end || (is_last_logical && cursor_pos == buffer.len()));

            // Pre-wrap this logical line into segments so ratatui never wraps it.
            // Short lines come back as a single-element vec.
            let segments = wrap_segments(text, content_area);
            let mut seg_byte_start = 0usize;

            for (seg_idx, seg) in segments.iter().enumerate() {
                let seg_start = line_start + seg_byte_start;
                let seg_end = seg_start + seg.len();
                let is_last_seg = seg_idx + 1 == segments.len();
                let cursor_in_seg = cursor_on_this_line
                    && cursor_pos >= seg_start
                    && (cursor_pos < seg_end || is_last_seg);

                let mut line_spans = vec![Span::styled(BORDER_PREFIX, border_style)];
                let line_runs = highlighted.get(line_idx).and_then(|o| o.as_deref());
                let seg_end_in_line = seg_byte_start + seg.len();

                if cursor_in_seg {
                    let cursor_pos_in_seg = (cursor_pos - seg_start).min(seg.len());
                    let (before, after) = seg.split_at(cursor_pos_in_seg);
                    let cursor_in_line = seg_byte_start + cursor_pos_in_seg;

                    // Track cursor position for IME
                    cursor_line_offset = 1 + total_visual_lines;
                    cursor_column = BORDER_PREFIX_WIDTH as u16 + before.width() as u16;

                    // before-cursor text, highlighted
                    line_spans.extend(highlighted_window_spans(
                        line_runs,
                        text,
                        seg_byte_start,
                        cursor_in_line,
                    ));
                    // Cursor char gets the cursor style; the rest stays highlighted.
                    // No trailing cell at end-of-segment — the terminal cursor
                    // (set_cursor_position) handles that position.
                    if let Some(cursor_char) = after.chars().next() {
                        line_spans.push(Span::styled(cursor_char.to_string(), cursor_style));
                        line_spans.extend(highlighted_window_spans(
                            line_runs,
                            text,
                            cursor_in_line + cursor_char.len_utf8(),
                            seg_end_in_line,
                        ));
                    }
                } else {
                    line_spans.extend(highlighted_window_spans(
                        line_runs,
                        text,
                        seg_byte_start,
                        seg_end_in_line,
                    ));
                }

                result.push(Line::from(line_spans));
                total_visual_lines += 1;
                seg_byte_start += seg.len();
            }

            // Account for newline character (except for last line)
            byte_offset = line_end + 1;
        }
    }

    // Bottom border — "    ╰" = 5 chars, fill to width
    result.push(Line::from(vec![Span::styled(
        "    ╰".to_string() + &"─".repeat(width.saturating_sub(5)),
        border_style,
    )]));

    let cursor_info = CommentCursorInfo {
        line_offset: cursor_line_offset,
        column: cursor_column,
    };

    (result, cursor_info)
}

/// Format an entire remote (read-only) forge review thread as one fused
/// box so it reads as a single discussion unit. Root comment opens the
/// box; replies appear as `├─ ↳ @author ──` separator headers within the
/// same box; the bottom rule appears once at the end.
///
/// Visually distinct from local drafts: the `[forge @author]` badge on
/// the root header, and a muted palette throughout for resolved/outdated
/// threads.
pub fn format_remote_thread_lines(
    theme: &Theme,
    thread: &crate::forge::remote_comments::RemoteReviewThread,
    muted: bool,
    forge_kind: Option<ForgeKind>,
) -> Vec<Line<'static>> {
    let (badge_fg, border_fg, body_fg) = if muted {
        (theme.fg_dim, theme.fg_dim, theme.fg_dim)
    } else {
        (
            theme.diff_hunk_header,
            theme.diff_hunk_header,
            theme.fg_secondary,
        )
    };

    let badge_style = Style::default().fg(badge_fg).add_modifier(Modifier::BOLD);
    let reply_badge_style = Style::default().fg(badge_fg);
    let border_style = Style::default().fg(border_fg);
    let body_style = Style::default().fg(body_fg);

    let line_info = match thread.line.map(LineRange::single) {
        Some(range) if range.is_single() => format!("L{} ", range.start),
        Some(range) => format!("L{}-L{} ", range.start, range.end),
        None => String::new(),
    };

    // Remote review threads always anchor on a specific line/range, so the
    // top corner is a tee — the bar painter draws the rest going up.
    let mut result = Vec::new();
    let mut iter = thread.comments.iter().peekable();
    let mut is_first = true;
    while let Some(comment) = iter.next() {
        let author = comment.author.as_deref().unwrap_or("unknown");
        if is_first {
            let mut badge_text = format!("[{} @{author}", forge_badge_label(forge_kind));
            if thread.is_resolved {
                badge_text.push_str(" resolved");
            } else if thread.is_outdated {
                badge_text.push_str(" outdated");
            }
            badge_text.push_str("] ");
            result.push(Line::from(vec![
                Span::styled("    ├── ".to_string(), border_style),
                Span::styled(badge_text, badge_style),
                Span::styled(line_info.clone(), styles::dim_style(theme)),
                Span::styled("─".repeat(20), border_style),
            ]));
        } else {
            result.push(Line::from(vec![
                Span::styled("    ├── ".to_string(), border_style),
                Span::styled(format!("↳ @{author} "), reply_badge_style),
                Span::styled("─".repeat(28), border_style),
            ]));
        }

        for line in comment.body.split('\n') {
            result.push(Line::from(vec![
                Span::styled("    │  ".to_string(), border_style),
                Span::styled(line.to_string(), body_style),
            ]));
        }

        is_first = false;
        let _ = iter.peek();
    }

    result.push(Line::from(vec![Span::styled(
        "    ╰".to_string() + &"─".repeat(39),
        border_style,
    )]));

    result
}

/// Format a remote review summary (the body of a `PullRequestReview`) as a
/// box with a `[forge @author <state>]` header. Renders at review scope —
/// no line anchor — so the top corner is `╭`, not the line-anchored `├`.
pub fn format_remote_review_summary_lines(
    theme: &Theme,
    summary: &crate::forge::remote_comments::RemoteReviewSummary,
    forge_kind: Option<ForgeKind>,
) -> Vec<Line<'static>> {
    let badge_fg = theme.diff_hunk_header;
    let border_fg = theme.diff_hunk_header;
    let body_fg = theme.fg_secondary;

    let badge_style = Style::default().fg(badge_fg).add_modifier(Modifier::BOLD);
    let border_style = Style::default().fg(border_fg);
    let body_style = Style::default().fg(body_fg);

    let author = summary.author.as_deref().unwrap_or("unknown");
    let mut badge_text = format!("[{} @{author}", forge_badge_label(forge_kind));
    if let Some(state_label) = summary.state.badge_label() {
        badge_text.push(' ');
        badge_text.push_str(state_label);
    }
    badge_text.push_str("] ");

    let mut result = Vec::new();
    result.push(Line::from(vec![
        Span::styled("    ╭── ".to_string(), border_style),
        Span::styled(badge_text, badge_style),
        Span::styled("─".repeat(28), border_style),
    ]));

    for line in summary.body.split('\n') {
        result.push(Line::from(vec![
            Span::styled("    │  ".to_string(), border_style),
            Span::styled(line.to_string(), body_style),
        ]));
    }

    result.push(Line::from(vec![Span::styled(
        "    ╰".to_string() + &"─".repeat(39),
        border_style,
    )]));

    result
}

fn forge_badge_label(kind: Option<ForgeKind>) -> &'static str {
    match kind {
        Some(ForgeKind::GitHub) => "github",
        Some(ForgeKind::GitLab) => "gitlab",
        Some(ForgeKind::Bitbucket) => "bitbucket",
        Some(ForgeKind::AzureDevOps) => "azure",
        None => "forge",
    }
}

/// Which body line each wrapped body row renders: `wrap_segments` row
/// counts flattened, in the order `markdown_body_lines` emits them. Row
/// accounting and row-to-line resolution both read this, so they cannot
/// drift.
pub(crate) fn body_row_lines(body: &str, content_area: usize) -> Vec<usize> {
    let mut rows = Vec::new();
    for (i, line) in body.split('\n').enumerate() {
        rows.extend(std::iter::repeat_n(
            i,
            wrap_segments(line, content_area).len(),
        ));
    }
    rows
}

/// Render `content` as markdown-highlighted, pre-wrapped lines. Colors come
/// from the active syntect theme.
pub(crate) fn markdown_body_lines(
    theme: &Theme,
    content: &str,
    content_area: usize,
) -> Vec<Line<'static>> {
    let lines: Vec<&str> = content.split('\n').collect();
    // Highlight the body as a whole so multi-line constructs (e.g. fenced code)
    // carry state across lines.
    let highlighted = theme.syntax_highlighter().highlight_markdown_body(content);

    let mut out = Vec::new();
    for (idx, text) in lines.iter().enumerate() {
        let runs = highlighted.get(idx).and_then(|o| o.as_deref());
        let mut seg_start = 0usize;
        for seg in wrap_segments(text, content_area) {
            let seg_end = seg_start + seg.len();
            out.push(Line::from(highlighted_window_spans(
                runs, text, seg_start, seg_end,
            )));
            seg_start = seg_end;
        }
    }
    out
}

/// How a local comment box announces itself in its top border.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CommentBadge<'a> {
    /// The reader's own comment: no author badge, type-colored border.
    #[default]
    Own,
    /// Someone else's comment: `[@name]` badge and an author-colored border.
    Author(&'a str),
    /// A reply in a local thread: the `↳ @name` header remote threads use, so
    /// a thread reads the same whether it lives here or on the forge.
    Reply(&'a str),
}

impl<'a> CommentBadge<'a> {
    /// Badge for a stored comment, as seen by `username`. Replies keep the
    /// reply header even when the reader wrote them — it is what marks the box
    /// as a continuation rather than a second comment on the same line.
    pub fn for_comment(comment: &'a crate::model::Comment, username: &str) -> Self {
        if comment.is_reply() {
            Self::Reply(&comment.author)
        } else if comment.author != username {
            Self::Author(&comment.author)
        } else {
            Self::Own
        }
    }

    /// Badge for a box whose author is known but which is not part of a local
    /// thread (remote PR conversation comments).
    pub fn from_author(author: Option<&'a str>) -> Self {
        match author {
            Some(name) => Self::Author(name),
            None => Self::Own,
        }
    }

    fn author(self) -> Option<&'a str> {
        match self {
            Self::Own => None,
            Self::Author(name) | Self::Reply(name) => Some(name),
        }
    }

    fn is_reply(self) -> bool {
        matches!(self, Self::Reply(_))
    }
}

/// The code a detached comment was written against: the line itself and the
/// lines that surrounded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RememberedCode {
    pub before: Vec<String>,
    pub line: String,
    pub after: Vec<String>,
}

impl RememberedCode {
    /// Rows this block occupies. The row model and the renderer both go
    /// through here, so a box's height cannot come out different in the two.
    pub fn rows(&self) -> usize {
        1 + self.before.len() + self.after.len()
    }
}

/// The code a comment carries when its anchor is gone, or `None` when it
/// carries none.
///
/// A thread is one conversation about one piece of code, and every message in
/// it is marked outdated together. Printing the code above each reply repeats
/// it as many times as the thread is long and buries the reading the reader
/// came for, so only the root shows it — the replies sit directly underneath.
///
/// The renderer and the row model both ask here, or a box's height comes out
/// different in the two and every row below it drifts.
pub fn remembered_code(comment: &crate::model::Comment) -> Option<RememberedCode> {
    if !comment.outdated || comment.is_reply() {
        return None;
    }
    comment.line_context.as_ref().map(|context| RememberedCode {
        before: context.before.clone(),
        line: context.content.clone(),
        after: context.after.clone(),
    })
}

/// How a comment box should present right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadDisplay {
    /// A normal box.
    Open,
    /// Settled, but shown in full because resolved threads are expanded.
    Resolved,
    /// The code this comment was written against is gone. Shown in full — the
    /// text is still the reader's own words — and carrying the line it used to
    /// live on, since it no longer renders there.
    Outdated {
        was_line: Option<u32>,
        /// The code as it read when the comment was written: the commented
        /// line and its neighbours. Detached from the diff, the comment would
        /// otherwise be a remark about code the reader can no longer see, and
        /// one line on its own rarely says what it was about.
        was_text: Option<RememberedCode>,
    },
    /// Settled and hidden: one marker row carrying the reply count and the
    /// leader key that expands it.
    Collapsed { replies: usize, expand_key: char },
}

impl ThreadDisplay {
    fn is_resolved(&self) -> bool {
        !matches!(self, Self::Open)
    }

    fn is_outdated(&self) -> bool {
        matches!(self, Self::Outdated { .. })
    }

    fn was_line(&self) -> Option<u32> {
        match self {
            Self::Outdated { was_line, .. } => *was_line,
            _ => None,
        }
    }

    fn was_text(&self) -> Option<&RememberedCode> {
        match self {
            Self::Outdated { was_text, .. } => was_text.as_ref(),
            _ => None,
        }
    }
}

/// The single row a settled thread collapses to: `├─ ▸ [n replies] resolved:
/// <first line>`. Dim, so it reads as background, and short enough that the
/// diff stays legible while the record stays visible.
fn collapsed_thread_line(
    theme: &Theme,
    content: &str,
    reply_count: usize,
    expand_key: char,
    width: usize,
) -> Line<'static> {
    let dim = styles::dim_style(theme);
    let summary = content.lines().next().unwrap_or_default();
    let replies = match reply_count {
        0 => String::new(),
        1 => " (1 reply)".to_string(),
        n => format!(" ({n} replies)"),
    };
    let head = format!("    ├─ ▸ resolved{replies}: ");
    // The way out belongs on the row: a collapsed line that does not say how
    // to uncollapse it is a dead end.
    let hint = format!("  (⏎ or {expand_key}R to show)");
    // Leave room for the hint and the ellipsis so neither is pushed off.
    let room = width.saturating_sub(head.width() + hint.width() + 1);
    let summary: String = if summary.width() > room {
        let mut cut = String::new();
        for ch in summary.chars() {
            if cut.width() + 1 > room {
                break;
            }
            cut.push(ch);
        }
        format!("{cut}\u{2026}")
    } else {
        summary.to_string()
    };
    Line::from(vec![
        Span::styled(head, dim),
        Span::styled(summary, dim),
        Span::styled(hint, dim),
    ])
}

/// Format a comment as multiple lines with a box border (themed version).
///
/// `badge` sets the top-row badge and tints the box border: `Own` keeps the
/// neutral `[TYPE]` badge and theme border, `Author` reads `[TYPE @name]` and
/// tints the border to the author, and `Reply` adds the `↳ @name` header that
/// a local thread shares with remote forge threads.
pub fn format_comment_lines(
    theme: &Theme,
    comment_type: CommentTypePresentation,
    content: &str,
    line_range: Option<LineRange>,
    width: usize,
    badge: CommentBadge<'_>,
    display: ThreadDisplay,
) -> Vec<Line<'static>> {
    // A collapsed thread is exactly one row; `comment_display_lines_collapsed`
    // promises the same, which is what keeps navigation aligned with the page.
    if let ThreadDisplay::Collapsed {
        replies,
        expand_key,
    } = display
    {
        return vec![collapsed_thread_line(
            theme, content, replies, expand_key, width,
        )];
    }
    let resolved = display.is_resolved();
    // A settled thread keeps its shape — same rows, same anchor — but drops to
    // the dim palette so it reads as background, the way a resolved remote
    // thread does.
    let type_style = if resolved {
        styles::dim_style(theme)
    } else {
        styles::comment_type_style(theme, comment_type.color)
    };
    let author = badge.author();
    let border_style = if resolved {
        styles::dim_style(theme)
    } else {
        match author {
            Some(name) => Style::default()
                .fg(styles::author_color_for(name))
                .add_modifier(ratatui::style::Modifier::BOLD),
            None => styles::comment_border_style(theme, comment_type.color),
        }
    };

    // `None` comments have an empty label: drop the `[TYPE]` badge, keeping the
    // author tag when present so per-author coloring still reads. A reply shows
    // the `↳ @name` header instead — it has no type of its own.
    let badge_text = match (author, comment_type.label.is_empty()) {
        (Some(name), _) if badge.is_reply() => format!("↳ @{name} "),
        (Some(name), true) => format!("[@{name}] "),
        (Some(name), false) => format!("[{} @{name}] ", comment_type.label),
        (None, true) => String::new(),
        (None, false) => format!("[{}] ", comment_type.label),
    };
    // The root announces the state once; its replies are already dimmed.
    let state = match (display.is_outdated(), display.was_line()) {
        // Name the line it used to be on: detached from the diff, that is the
        // only thing that places it.
        (true, Some(line)) => format!("outdated · was L{line}"),
        (true, None) => "outdated".to_string(),
        (false, _) => "resolved".to_string(),
    };
    let badge_text = match (resolved, badge.is_reply()) {
        (false, _) | (true, true) => badge_text,
        (true, false) if badge_text.is_empty() => format!("[{state}] "),
        (true, false) => badge_text.replacen("] ", &format!(" {state}] "), 1),
    };
    let badge_width = badge_text.width();

    let line_info = match line_range {
        // A reply repeats no anchor: its root's header already showed it.
        _ if badge.is_reply() => String::new(),
        Some(range) if range.is_single() => format!("L{} ", range.start),
        Some(range) => format!("L{}-L{} ", range.start, range.end),
        None => String::new(),
    };

    // "    │  " is the per-line content prefix; everything past that is content.
    // Subtract two extra: one so ratatui never wraps an exact-fit line, and
    // one so the terminal cursor at end-of-segment stays clear of the border.
    let content_area = width.saturating_sub(BORDER_PREFIX_WIDTH + 2);

    let mut result = Vec::new();

    // A reply hangs under the comment it answers, so it tees even at review
    // scope; a root only tees when a bar connects it to its diff line.
    let top_corner = if line_range.is_some() || badge.is_reply() {
        '├'
    } else {
        '╭'
    };
    let top_prefix = format!("    {top_corner}── ");

    // Top border — fill dynamically so total line = width.
    // top_prefix = 8 cols, then the badge (width depends on author), then
    // optional line_info, then `─` fill out to `width`.
    let top_fill = width.saturating_sub(8 + badge_width + line_info.width());
    result.push(Line::from(vec![
        Span::styled(top_prefix, border_style),
        Span::styled(badge_text, type_style),
        Span::styled(line_info, styles::dim_style(theme)),
        Span::styled("─".repeat(top_fill), border_style),
    ]));

    // The code the comment was written about, when that code is gone. Without
    // it a detached comment is a remark with nothing to remark on.
    if let Some(code) = display.was_text() {
        let dim = styles::dim_style(theme);
        // The commented line is marked; its neighbours are there to make it
        // recognisable, not to be read as the subject.
        // Strip the indentation the block shares: in a side pane, deeply
        // indented code would otherwise be all leading spaces and an ellipsis.
        // Relative indentation inside the block is kept, so the shape reads.
        let shared_indent = code
            .before
            .iter()
            .chain(std::iter::once(&code.line))
            .chain(code.after.iter())
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.len() - line.trim_start().len())
            .min()
            .unwrap_or(0);
        let rows = code
            .before
            .iter()
            .map(|line| ("  ", line))
            .chain(std::iter::once((" >", &code.line)))
            .chain(code.after.iter().map(|line| ("  ", line)));
        for (marker, text) in rows {
            let text = &text[shared_indent.min(text.len())..];
            let prefix = format!("{BORDER_PREFIX}{marker} ");
            let room = width.saturating_sub(prefix.width() + 1);
            let mut shown = String::new();
            for ch in text.trim_end().chars() {
                if shown.width() + 1 > room {
                    shown.push('\u{2026}');
                    break;
                }
                shown.push(ch);
            }
            result.push(Line::from(vec![
                Span::styled(prefix, border_style),
                Span::styled(shown, dim),
            ]));
        }
    }

    // Content lines — markdown-highlighted, pre-wrapped at content_area.
    let mut body_lines = markdown_body_lines(theme, content, content_area);
    for line in &mut body_lines {
        line.spans
            .insert(0, Span::styled(BORDER_PREFIX, border_style));
    }
    result.extend(body_lines);

    // Bottom border — "    ╰" = 5 chars, fill to width
    result.push(Line::from(vec![Span::styled(
        "    ╰".to_string() + &"─".repeat(width.saturating_sub(5)),
        border_style,
    )]));

    result
}

pub fn render_confirm_dialog(frame: &mut Frame, app: &App, message: &str) {
    let theme = &app.theme;
    let area = centered_rect(50, 20, frame.area());

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Confirm ")
        .borders(Borders::ALL)
        .style(styles::popup_style(theme))
        .border_style(styles::border_style(theme, true));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(""),
        Line::from(Span::raw(message)),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [Y]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("es    "),
            Span::styled("[N]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("o"),
        ]),
    ];

    let paragraph = Paragraph::new(lines)
        .style(styles::popup_style(theme))
        .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(paragraph, inner);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([Constraint::Percentage(percent_y)]).flex(Flex::Center);
    let horizontal = Layout::horizontal([Constraint::Percentage(percent_x)]).flex(Flex::Center);
    let [area] = vertical.areas(area);
    let [area] = horizontal.areas(area);
    area
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use ratatui::style::Color;

    fn test_theme() -> Theme {
        Theme::default()
    }

    /// The renderer replaces off-screen comment boxes with exactly
    /// `App::comment_display_lines` blank rows, and the annotation builder sizes
    /// every comment the same way. If that count ever drifted from what
    /// `format_comment_lines` actually emits, the document would desync — the
    /// cursor would land on the wrong row and culled boxes would leave the wrong
    /// number of gaps. Pin the two together.
    #[test]
    fn comment_display_lines_matches_rendered_box_height() {
        let theme = test_theme();
        let bodies = [
            "",
            "single line",
            "first\nsecond\nthird",
            "trailing newline\n",
            "\n\nleading blanks",
            &"x".repeat(300),
            &"日本語のテキストです ".repeat(20),
            "`code` **bold** and a very long tail that will need to wrap at least once or twice",
        ];
        // Viewport widths, including degenerate ones narrower than the box chrome.
        for viewport_width in [9usize, 12, 40, 80, 120] {
            for body in bodies {
                let comment = crate::model::Comment::new(
                    body.to_string(),
                    crate::model::CommentType::from_id("note"),
                    None,
                );
                let rendered = format_comment_lines(
                    &theme,
                    CommentTypePresentation {
                        label: "NOTE".to_string(),
                        color: Color::Blue,
                    },
                    &comment.content,
                    None,
                    // What every call site passes: the viewport minus the
                    // cursor-indicator column.
                    viewport_width.saturating_sub(1),
                    CommentBadge::Own,
                    ThreadDisplay::Open,
                );
                assert_eq!(
                    App::comment_display_lines(&comment, viewport_width),
                    rendered.len(),
                    "width={viewport_width} body={body:?}"
                );
            }
        }
    }

    // -- wrap_segments tests --

    #[test]
    fn wrap_segments_returns_single_segment_when_text_fits() {
        // given
        let text = "hello";

        // when
        let segments = wrap_segments(text, 80);

        // then
        assert_eq!(segments, vec!["hello"]);
    }

    #[test]
    fn wrap_segments_returns_single_segment_for_empty_text() {
        // given
        let text = "";

        // when
        let segments = wrap_segments(text, 80);

        // then
        assert_eq!(segments, vec![""]);
    }

    #[test]
    fn wrap_segments_returns_text_unchanged_when_content_area_is_zero() {
        // given
        let text = "anything";

        // when
        let segments = wrap_segments(text, 0);

        // then
        assert_eq!(segments, vec!["anything"]);
    }

    #[test]
    fn wrap_segments_splits_long_ascii_at_content_area() {
        // given
        let text = "hello world";

        // when - content_area=5 means each segment is at most 5 display cols
        let segments = wrap_segments(text, 5);

        // then
        assert_eq!(segments, vec!["hello", " worl", "d"]);
    }

    #[test]
    fn wrap_segments_respects_cjk_display_width() {
        // given - each CJK char is 2 display cols, total = 8 cols, 12 bytes
        let text = "中文测试";

        // when - content_area=4 fits exactly 2 CJK chars per segment
        let segments = wrap_segments(text, 4);

        // then
        assert_eq!(segments, vec!["中文", "测试"]);
    }

    #[test]
    fn wrap_segments_handles_mixed_ascii_and_cjk() {
        // given - 'a'(1) + '中'(2) + 'b'(1) + '文'(2) = 6 display cols
        let text = "a中b文";

        // when - content_area=3 fits "a中" (1+2=3), then "b文" (1+2=3)
        let segments = wrap_segments(text, 3);

        // then
        assert_eq!(segments, vec!["a中", "b文"]);
    }

    #[test]
    fn wrap_segments_emits_oversized_char_to_avoid_infinite_loop() {
        // given - '中' is 2 cols wide but content_area only allows 1
        let text = "中a";

        // when
        let segments = wrap_segments(text, 1);

        // then - '中' is emitted even though it exceeds content_area
        assert_eq!(segments, vec!["中", "a"]);
    }

    #[test]
    fn wrap_segments_handles_exact_width_boundary() {
        // given - text exactly fills content_area
        let text = "12345";

        // when
        let segments = wrap_segments(text, 5);

        // then - single segment, no spurious empty trailing segment
        assert_eq!(segments, vec!["12345"]);
    }

    // -- format_comment_input_lines tests --

    #[test]
    fn should_return_cursor_at_start_for_empty_buffer() {
        // given
        let theme = test_theme();

        // when
        let (lines, cursor_info) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            "",
            0,
            None,
            false,
            80,
            None,
            true,
            None,
        );

        // then
        assert_eq!(lines.len(), 3); // header + content + footer
        assert_eq!(cursor_info.line_offset, 1); // cursor on first content line
        assert_eq!(cursor_info.column, 7); // "     │ " = 7 chars
    }

    #[test]
    fn should_label_reply_input_with_thread_author() {
        // given
        let theme = test_theme();

        // when — reply mode: typeless presentation + reply_to author
        let (lines, _) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: String::new(),
                color: Color::Blue,
            },
            "",
            0,
            Some(LineRange::single(42)),
            false,
            80,
            None,
            true,
            Some("alice"),
        );

        // then — the header reads "Reply to @alice" with no type hint
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header.contains("Reply to @alice"), "got: {header:?}");
        assert!(!header.contains("Tab/S-Tab"), "got: {header:?}");
        assert!(header.contains("Enter:send"), "got: {header:?}");
    }

    #[test]
    fn should_return_cursor_position_for_ascii_text() {
        // given
        let theme = test_theme();
        let buffer = "hello";
        let cursor_pos = 3; // cursor after "hel"

        // when
        let (_, cursor_info) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            buffer,
            cursor_pos,
            None,
            false,
            80,
            None,
            true,
            None,
        );

        // then
        assert_eq!(cursor_info.line_offset, 1); // first content line
        assert_eq!(cursor_info.column, 7 + 3); // border + "hel"
    }

    #[test]
    fn should_return_cursor_position_for_multibyte_text() {
        // given
        let theme = test_theme();
        let buffer = "안녕"; // 2 multibyte chars, 6 bytes, 4 display columns
        let cursor_pos = 3; // cursor after first multibyte char (after "안")

        // when
        let (_, cursor_info) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            buffer,
            cursor_pos,
            None,
            false,
            80,
            None,
            true,
            None,
        );

        // then
        assert_eq!(cursor_info.line_offset, 1);
        // "안" has display width 2, so cursor column = border(7) + 2 = 9
        assert_eq!(cursor_info.column, 7 + 2);
    }

    #[test]
    fn should_return_cursor_position_at_end_of_text() {
        // given
        let theme = test_theme();
        let buffer = "test";
        let cursor_pos = 4; // cursor at end

        // when
        let (_, cursor_info) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            buffer,
            cursor_pos,
            None,
            false,
            80,
            None,
            true,
            None,
        );

        // then
        assert_eq!(cursor_info.line_offset, 1);
        assert_eq!(cursor_info.column, 7 + 4); // border + "test"
    }

    #[test]
    fn should_return_cursor_position_on_second_line() {
        // given
        let theme = test_theme();
        let buffer = "line1\nline2";
        let cursor_pos = 8; // cursor after "li" in "line2"

        // when
        let (lines, cursor_info) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            buffer,
            cursor_pos,
            None,
            false,
            80,
            None,
            true,
            None,
        );

        // then
        assert_eq!(lines.len(), 4); // header + 2 content lines + footer
        assert_eq!(cursor_info.line_offset, 2); // second content line (0=header, 1=line1, 2=line2)
        assert_eq!(cursor_info.column, 7 + 2); // border + "li"
    }

    #[test]
    fn should_return_cursor_position_for_mixed_content() {
        // given
        let theme = test_theme();
        let buffer = "a좋b"; // 1 + 3 + 1 = 5 bytes, 1 + 2 + 1 = 4 display columns
        let cursor_pos = 4; // cursor after "a좋" (1 + 3 bytes)

        // when
        let (_, cursor_info) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            buffer,
            cursor_pos,
            None,
            false,
            80,
            None,
            true,
            None,
        );

        // then
        assert_eq!(cursor_info.line_offset, 1);
        // "a" = 1 display width, "좋" = 2 display width, total = 3
        assert_eq!(cursor_info.column, 7 + 3);
    }

    #[test]
    fn should_show_shift_enter_hint_when_keyboard_enhancement_supported() {
        let theme = test_theme();
        let (lines, _) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            "",
            0,
            None,
            false,
            80,
            None,
            true,
            None,
        );
        let header = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(header.contains("Shift-Enter:newline"));
        assert!(!header.contains("Alt-Enter:newline"));
    }

    #[test]
    fn should_show_alt_enter_hint_when_keyboard_enhancement_not_supported() {
        let theme = test_theme();
        let (lines, _) = format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            "",
            0,
            None,
            false,
            80,
            None,
            false,
            None,
        );
        let header = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(header.contains("Alt-Enter:newline"));
        assert!(!header.contains("Shift-Enter:newline"));
    }

    // -- markdown highlighting tests --

    /// Reconstruct the buffer text from the rendered content lines: drop the
    /// header (first) and footer (last) lines, skip each line's BORDER_PREFIX
    /// span, concatenate the rest, and join visual lines with '\n'. With a wide
    /// width (no wrapping) each logical line is one visual line, so this must
    /// equal the original buffer regardless of how content is split into spans.
    fn reconstruct(lines: &[Line<'static>]) -> String {
        lines[1..lines.len() - 1]
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .skip(1) // BORDER_PREFIX
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_md(buffer: &str, cursor: usize) -> Vec<Line<'static>> {
        let theme = test_theme();
        format_comment_input_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            buffer,
            cursor,
            None,
            false,
            80,
            None,
            true,
            None,
        )
        .0
    }

    #[test]
    fn markdown_render_preserves_all_text() {
        let buffer = "# Title\n**bold** and `code`\n- item";
        // cursor in the middle of the bold span
        let lines = render_md(buffer, 11);
        assert_eq!(reconstruct(&lines), buffer);
    }

    #[test]
    fn markdown_render_preserves_multibyte_text() {
        let buffer = "# 世界\n**bold** 좋아";
        for cursor in [0, buffer.find('世').unwrap(), buffer.len()] {
            let lines = render_md(buffer, cursor);
            assert_eq!(reconstruct(&lines), buffer, "cursor={cursor}");
        }
    }

    #[test]
    fn displayed_comment_is_markdown_highlighted_and_preserves_text() {
        let theme = test_theme();
        let content = "# Heading\n**bold** and `code`\n- item";
        let lines = format_comment_lines(
            &theme,
            CommentTypePresentation {
                label: "NOTE".to_string(),
                color: Color::Blue,
            },
            content,
            None,
            80,
            CommentBadge::Own,
            ThreadDisplay::Open,
        );
        // Header + footer wrap the body; reconstruct must round-trip the text.
        assert_eq!(reconstruct(&lines), content);
        // The inline-code line should be split into multiple styled runs.
        let code_line = &lines[2]; // header, heading, **this**
        assert!(
            code_line.spans.len() - 1 > 1,
            "expected displayed markdown to be highlighted into multiple spans"
        );
    }

    #[test]
    fn markdown_highlighting_splits_line_into_runs() {
        // An inline-code line should yield multiple styled content spans (proof
        // the markdown grammar resolved and coloring is applied), not one raw
        // span. Cursor at end so no cursor cell splits the line artificially.
        let buffer = "plain `code` plain";
        let lines = render_md(buffer, buffer.len());
        let content_spans = lines[1].spans.len() - 1; // minus BORDER_PREFIX
        assert!(
            content_spans > 1,
            "expected markdown highlighting to split the line, got {content_spans} span(s)"
        );
    }

    #[test]
    fn remote_thread_badge_uses_gitlab_for_gitlab_comments() {
        let thread = crate::forge::remote_comments::RemoteReviewThread {
            id: "thread".to_string(),
            path: "src/lib.rs".to_string(),
            line: Some(1),
            side: crate::forge::remote_comments::RemoteCommentSide::Right,
            is_resolved: false,
            is_outdated: false,
            comments: vec![crate::forge::remote_comments::RemoteReviewComment {
                id: "comment".to_string(),
                author: Some("alice".to_string()),
                body: "body".to_string(),
                created_at: None,
                in_reply_to: None,
                database_id: None,
                url: String::new(),
            }],
        };

        let lines =
            format_remote_thread_lines(&test_theme(), &thread, false, Some(ForgeKind::GitLab));
        let header = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(header.contains("[gitlab @alice]"));
        assert!(!header.contains("[github @alice]"));
    }

    #[test]
    fn remote_summary_badge_uses_github_for_github_comments() {
        let summary = crate::forge::remote_comments::RemoteReviewSummary {
            id: "summary".to_string(),
            author: Some("alice".to_string()),
            body: "body".to_string(),
            state: crate::forge::remote_comments::RemoteReviewState::Commented,
            created_at: None,
            url: String::new(),
        };

        let lines =
            format_remote_review_summary_lines(&test_theme(), &summary, Some(ForgeKind::GitHub));
        let header = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(header.contains("[github @alice]"));
    }
}
