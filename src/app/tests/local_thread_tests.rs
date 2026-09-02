//! Local comment threads: replies stored beside the comment they answer, and
//! deleted with it.

use crate::app::*;
use crate::model::FileStatus;
use crate::review_store::{ReplyRequest, reply_to_comment_in_session};
use crate::vcs::traits::VcsType;

struct DummyVcs {
    info: VcsInfo,
    commits: Vec<crate::vcs::CommitInfo>,
    /// What `resolve_revision_range` answers, oldest-first — the order the
    /// real resolver documents. `None` keeps the trait's unsupported error.
    resolved_range: Option<Vec<String>>,
}

impl VcsBackend for DummyVcs {
    fn info(&self) -> &VcsInfo {
        &self.info
    }
    fn get_commits_info(&self, ids: &[String]) -> Result<Vec<crate::vcs::CommitInfo>> {
        // In the requested order, as the real backend answers.
        Ok(ids
            .iter()
            .filter_map(|id| {
                self.commits
                    .iter()
                    .find(|commit| commit.short_id == *id || commit.id == *id)
                    .cloned()
            })
            .collect())
    }
    fn resolve_revision_range(
        &self,
        _revisions: &str,
    ) -> Result<crate::vcs::traits::ResolvedRevisionRange<'static>> {
        match &self.resolved_range {
            Some(ids) => Ok(
                crate::vcs::traits::ResolvedRevisionRange::from_owned_commit_ids(
                    ids.clone(),
                    crate::vcs::traits::RevisionDiffTarget::CommitList,
                ),
            ),
            None => Err(TuicrError::UnsupportedOperation(
                "no resolved range configured".into(),
            )),
        }
    }
    fn get_commit_range_diff(
        &self,
        _revision_range: &crate::vcs::traits::ResolvedRevisionRange<'_>,
        _highlighter: &SyntaxHighlighter,
    ) -> Result<Vec<DiffFile>> {
        Ok(vec![diff_file("src/main.rs")])
    }
    fn get_working_tree_diff(&self, _highlighter: &SyntaxHighlighter) -> Result<Vec<DiffFile>> {
        Err(TuicrError::NoChanges)
    }
    fn fetch_context_lines(
        &self,
        _file_path: &Path,
        _file_status: FileStatus,
        _ref_commit: Option<&str>,
        _start_line: u32,
        _end_line: u32,
    ) -> Result<Vec<DiffLine>> {
        Ok(Vec::new())
    }
    fn file_line_count(
        &self,
        _file_path: &Path,
        _file_status: FileStatus,
        _ref_commit: Option<&str>,
    ) -> Result<u32> {
        Ok(0)
    }
}

/// `find_comment_at_cursor` resolves a comment's file through `diff_files`,
/// so the app under test needs the reviewed file present there too.
fn diff_file(path: &str) -> DiffFile {
    let hunks = vec![DiffHunk {
        header: "@@ -42,1 +42,1 @@".to_string(),
        lines: vec![DiffLine {
            origin: LineOrigin::Addition,
            content: "let x = 1;".to_string(),
            old_lineno: None,
            new_lineno: Some(42),
            highlighted_spans: None,
        }],
        old_start: 42,
        old_count: 1,
        new_start: 42,
        new_count: 1,
    }];
    let content_hash = DiffFile::compute_content_hash(&hunks);
    DiffFile {
        old_path: None,
        new_path: Some(PathBuf::from(path)),
        status: FileStatus::Modified,
        hunks,
        is_binary: false,
        is_too_large: false,
        is_commit_message: false,
        content_hash,
    }
}

fn app_with_session(session: ReviewSession) -> App {
    app_with_session_and_vcs(session, Vec::new(), None)
}

fn app_with_session_and_vcs(
    session: ReviewSession,
    commits: Vec<crate::vcs::CommitInfo>,
    resolved_range: Option<Vec<String>>,
) -> App {
    let vcs_info = VcsInfo {
        root_path: PathBuf::from("/repo"),
        head_commit: "head".to_string(),
        branch_name: Some("main".to_string()),
        vcs_type: VcsType::Git,
    };
    App::build(
        Box::new(DummyVcs {
            info: vcs_info.clone(),
            commits,
            resolved_range,
        }),
        vcs_info,
        Theme::dark(),
        None,
        false,
        Vec::new(),
        session,
        DiffSource::WorkingTree,
        InputMode::Normal,
        Vec::new(),
        None,
        None,
    )
    .expect("failed to build test app")
}

fn app_for(session: ReviewSession) -> App {
    let mut app = app_with_session(session);
    app.diff_files = vec![diff_file("src/main.rs")];
    app
}

fn session_with_line_comment() -> (ReviewSession, Comment) {
    let mut session = ReviewSession::new(
        PathBuf::from("/repo"),
        "abc1234".to_string(),
        Some("main".to_string()),
        SessionDiffSource::WorkingTree,
    );
    session.add_file(PathBuf::from("src/main.rs"), FileStatus::Modified, 0);
    let root = Comment::new(
        "handle the empty case".to_string(),
        CommentType::from_id("issue"),
        Some(LineSide::New),
    );
    session
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .add_line_comment(42, root.clone());
    (session, root)
}

fn reply(session: &mut ReviewSession, parent: &str, body: &str) -> Comment {
    reply_to_comment_in_session(
        session,
        ReplyRequest {
            parent_id: parent.to_string(),
            content: body.to_string(),
            author: "Claude".to_string(),
            reopen: true,
        },
    )
    .expect("reply should attach")
}

fn line_thread(session: &ReviewSession) -> Vec<Comment> {
    session
        .files
        .get(&PathBuf::from("src/main.rs"))
        .unwrap()
        .line_comments
        .get(&42)
        .cloned()
        .unwrap_or_default()
}

#[test]
fn should_badge_a_reply_as_a_thread_continuation() {
    let (mut session, root) = session_with_line_comment();
    let reply = reply(&mut session, &root.id, "fixed in def4567");

    // The reader wrote the root, so it shows no author badge; the reply always
    // shows the reply header, which is what marks it as a continuation.
    assert_eq!(
        crate::ui::comment_panel::CommentBadge::for_comment(&root, "user"),
        crate::ui::comment_panel::CommentBadge::Own
    );
    assert_eq!(
        crate::ui::comment_panel::CommentBadge::for_comment(&reply, "user"),
        crate::ui::comment_panel::CommentBadge::Reply("Claude")
    );
}

#[test]
fn should_render_a_reply_with_the_thread_header_and_no_repeated_anchor() {
    let (mut session, root) = session_with_line_comment();
    let reply = reply(&mut session, &root.id, "fixed in def4567");
    let theme = Theme::dark();
    let presentation = crate::ui::comment_panel::CommentTypePresentation {
        label: String::new(),
        color: theme.fg_primary,
    };

    let lines = crate::ui::comment_panel::format_comment_lines(
        &theme,
        presentation,
        &reply.content,
        Some(LineRange::single(42)),
        80,
        crate::ui::comment_panel::CommentBadge::for_comment(&reply, "user"),
        crate::ui::comment_panel::ThreadDisplay::Open,
    );

    let header: String = lines[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(header.contains("↳ @Claude"), "{header}");
    // The root's header already carries the anchor; repeating it reads as a
    // second comment rather than a continuation.
    assert!(!header.contains("L42"), "{header}");
    assert!(header.starts_with("    ├"), "{header}");
    // A reply box costs the same rows as any other, so the annotation model
    // and the scroll height model need no special case.
    assert_eq!(
        App::comment_display_lines(&reply, 80),
        lines.len(),
        "reply box rows must match the shared row model"
    );
}

#[test]
fn should_delete_a_thread_when_its_root_is_deleted() {
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed in def4567");
    reply(&mut session, &root.id, "and covered by a test");
    let other = Comment::new(
        "unrelated".to_string(),
        CommentType::from_id("note"),
        Some(LineSide::New),
    );
    session
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .add_line_comment(42, other.clone());
    assert_eq!(line_thread(&session).len(), 4);

    let mut app = app_for(session);
    app.line_annotations = vec![AnnotatedLine::LineComment {
        file_idx: 0,
        line: 42,
        side: LineSide::New,
        comment_idx: 0,
    }];
    app.diff_state.cursor_line = 0;

    assert!(app.delete_comment_at_cursor());

    // Root and both replies go; the unrelated comment on the same line stays.
    let remaining = line_thread(&app.session);
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, other.id);
}

#[test]
fn should_delete_only_the_reply_when_the_cursor_is_on_it() {
    let (mut session, root) = session_with_line_comment();
    let reply = reply(&mut session, &root.id, "fixed in def4567");

    let mut app = app_for(session);
    app.line_annotations = vec![AnnotatedLine::LineComment {
        file_idx: 0,
        line: 42,
        side: LineSide::New,
        comment_idx: 1,
    }];
    app.diff_state.cursor_line = 0;

    assert!(app.delete_comment_at_cursor());

    let remaining = line_thread(&app.session);
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, root.id);
    assert_ne!(remaining[0].id, reply.id);
}

/// Put the cursor on the comment box for `comment_idx` on line 42.
fn cursor_on_line_comment(app: &mut App, comment_idx: usize) {
    app.line_annotations = vec![AnnotatedLine::LineComment {
        file_idx: 0,
        line: 42,
        side: LineSide::New,
        comment_idx,
    }];
    app.diff_state.cursor_line = 0;
}

#[test]
fn should_open_a_reply_editor_with_c_on_a_local_comment() {
    let (session, root) = session_with_line_comment();
    let mut app = app_for(session);
    cursor_on_line_comment(&mut app, 0);

    crate::handler::handle_diff_action(&mut app, crate::input::Action::AddLineComment);

    assert_eq!(app.input_mode, InputMode::Comment);
    assert_eq!(app.local_reply_target.as_deref(), Some(root.id.as_str()));
    // Anchored where the comment it answers is, so the box lands in the thread.
    assert_eq!(app.comment_line, Some((42, LineSide::New)));
    assert!(!app.comment_is_review_level);
    // A reply has no type of its own.
    assert!(app.comment_type.is_none());
    // The editor labels itself with whom it is answering.
    assert_eq!(app.comment_reply_author().as_deref(), Some("user"));
}

#[test]
fn should_store_the_reply_beside_its_root_on_save() {
    let (session, root) = session_with_line_comment();
    let mut app = app_for(session);
    cursor_on_line_comment(&mut app, 0);
    crate::handler::handle_diff_action(&mut app, crate::input::Action::AddLineComment);

    app.comment_buffer = "fixed in def4567".to_string();
    app.save_comment();

    let thread = line_thread(&app.session);
    assert_eq!(thread.len(), 2);
    assert_eq!(thread[1].in_reply_to.as_deref(), Some(root.id.as_str()));
    assert_eq!(thread[1].content, "fixed in def4567");
    assert_eq!(thread[1].author, app.username);
    // The editor closes and forgets its target, like every other save.
    assert_eq!(app.input_mode, InputMode::Normal);
    assert!(app.local_reply_target.is_none());
}

#[test]
fn should_thread_a_reply_to_a_reply_onto_the_root() {
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed in def4567");
    let mut app = app_for(session);
    cursor_on_line_comment(&mut app, 1);
    crate::handler::handle_diff_action(&mut app, crate::input::Action::AddLineComment);

    app.comment_buffer = "thanks".to_string();
    app.save_comment();

    let thread = line_thread(&app.session);
    assert_eq!(thread.len(), 3);
    // Flat threads: the second reply answers the root, not the first reply.
    assert_eq!(thread[2].in_reply_to.as_deref(), Some(root.id.as_str()));
}

#[test]
fn should_still_open_a_fresh_comment_when_the_cursor_is_on_a_diff_line() {
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    app.line_annotations = vec![AnnotatedLine::DiffLine {
        file_idx: 0,
        hunk_idx: 0,
        line_idx: 0,
        old_lineno: None,
        new_lineno: Some(42),
    }];
    app.diff_state.cursor_line = 0;

    crate::handler::handle_diff_action(&mut app, crate::input::Action::AddLineComment);

    assert_eq!(app.input_mode, InputMode::Comment);
    assert!(app.local_reply_target.is_none());
    assert_eq!(app.comment_line, Some((42, LineSide::New)));
}

#[test]
fn should_fold_a_thread_into_one_comment_for_the_forge() {
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed in def4567");
    reply(&mut session, &root.id, "and covered by a test");

    let folded = crate::forge::submit::fold_threads(&line_thread(&session));

    // One review comment, not three: a thread is one conversation.
    assert_eq!(folded.len(), 1);
    assert_eq!(folded[0].id, root.id);
    assert!(folded[0].content.starts_with("handle the empty case"));
    assert!(
        folded[0].content.contains("> **@Claude**:"),
        "{}",
        folded[0].content
    );
    assert!(folded[0].content.contains("> fixed in def4567"));
    assert!(folded[0].content.contains("> and covered by a test"));
}

#[test]
fn should_export_replies_under_the_comment_they_answer() {
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed in def4567");
    let other = Comment::new(
        "unrelated".to_string(),
        CommentType::from_id("note"),
        Some(LineSide::New),
    );
    session
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .add_line_comment(43, other);

    let md = crate::output::markdown::generate_export_content(
        &session,
        &DiffSource::WorkingTree,
        &[],
        &crate::config::ExportConfig::default(),
        &[],
        None,
    )
    .expect("export should render");

    // The reply hangs off item 1 rather than taking a number of its own, so
    // the unrelated comment stays item 2.
    assert!(md.contains("- @Claude - fixed in def4567"), "{md}");
    assert!(md.contains("2. **[NOTE]** `src/main.rs:43`"), "{md}");
    assert!(!md.contains("3. "), "{md}");
}

#[test]
fn should_carry_an_orphaned_reply_out_on_its_own() {
    // The reviewer deleted the root in the TUI while the agent replied to it
    // from the CLI; the merge keeps the reply. It must not vanish from the
    // export or the forge payload just because its root is gone.
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed in def4567");
    session
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .line_comments
        .get_mut(&42)
        .unwrap()
        .retain(|c| c.id != root.id);

    let folded = crate::forge::submit::fold_threads(&line_thread(&session));
    assert_eq!(folded.len(), 1);
    assert_eq!(folded[0].content, "fixed in def4567");

    let md = crate::output::markdown::generate_export_content(
        &session,
        &DiffSource::WorkingTree,
        &[],
        &crate::config::ExportConfig::default(),
        &[],
        None,
    )
    .expect("export should render");
    assert!(
        md.contains("1. `src/main.rs:42` - fixed in def4567"),
        "{md}"
    );
}

#[test]
fn should_lock_replies_with_the_root_they_were_submitted_inside() {
    use crate::forge::submit::SubmitEvent;
    use crate::model::comment::CommentLifecycleState;

    let (mut session, root) = session_with_line_comment();
    let reply = reply(&mut session, &root.id, "fixed in def4567");
    let mut app = app_for(session);

    // The thread went out as one inline comment, under the root's id.
    let in_flight = SubmitInFlightState {
        event: SubmitEvent::Comment,
        mappable: vec![crate::forge::submit::InlineComment {
            path: PathBuf::from("src/main.rs"),
            line: 42,
            side: crate::forge::submit::GhSide::Right,
            counterpart_line: None,
            start_line: None,
            start_side: None,
            range_anchors: None,
            old_path: None,
            body: "handle the empty case".to_string(),
            comment_id: root.id.clone(),
        }],
        summary_comment_ids: Vec::new(),
        review_comment_ids: Vec::new(),
        moved_to_summary_count: 0,
        head_sha_snapshot: "head".to_string(),
        repository: crate::forge::traits::ForgeRepository::github("github.com", "agavra", "tuicr"),
        pr_number: 125,
        started_at: std::time::Instant::now(),
    };
    app.apply_submit_success(
        &in_flight,
        &crate::forge::traits::GhCreateReviewResponse {
            id: 7,
            html_url: "https://github.com/agavra/tuicr/pull/125".to_string(),
            state: "COMMENTED".to_string(),
        },
    );

    let thread = line_thread(&app.session);
    // The reply's text is on the forge inside the root's body, so it locks
    // with it — otherwise pruning the root would strand it.
    assert_eq!(thread[0].lifecycle_state, CommentLifecycleState::Submitted);
    assert_eq!(thread[1].id, reply.id);
    assert_eq!(thread[1].lifecycle_state, CommentLifecycleState::Submitted);
    assert_eq!(thread[1].remote_review_id.as_deref(), Some("7"));
}

#[test]
fn should_resolve_the_thread_under_the_cursor() {
    let (mut session, root) = session_with_line_comment();
    let reply = reply(&mut session, &root.id, "fixed in def4567");
    let mut app = app_for(session);
    // Cursor on the reply, not the root: a thread settles as a unit.
    cursor_on_line_comment(&mut app, 1);

    assert!(app.set_thread_resolved_at_cursor(true));

    let thread = line_thread(&app.session);
    assert!(
        thread.iter().all(|c| c.resolved),
        "root and reply both settle"
    );

    // Resolving rebuilds the annotations, so re-anchor the cursor the way a
    // real frame would before toggling back.
    cursor_on_line_comment(&mut app, 1);
    assert!(app.set_thread_resolved_at_cursor(false));
    let thread = line_thread(&app.session);
    assert!(thread.iter().all(|c| !c.resolved), "and both reopen");
    assert_eq!(thread[1].id, reply.id);
}

#[test]
fn should_report_when_there_is_no_thread_under_the_cursor() {
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    app.line_annotations = vec![AnnotatedLine::DiffLine {
        file_idx: 0,
        hunk_idx: 0,
        line_idx: 0,
        old_lineno: None,
        new_lineno: Some(42),
    }];
    app.diff_state.cursor_line = 0;

    assert!(!app.set_thread_resolved_at_cursor(true));
    assert!(line_thread(&app.session).iter().all(|c| !c.resolved));
}

#[test]
fn should_render_a_settled_thread_in_the_dim_palette() {
    let (mut session, root) = session_with_line_comment();
    crate::review_store::set_thread_resolved(&mut session, &root.id, true).unwrap();
    let settled = line_thread(&session)[0].clone();
    let theme = Theme::dark();
    let presentation = crate::ui::comment_panel::CommentTypePresentation {
        label: "ISSUE".to_string(),
        color: theme.fg_primary,
    };

    let lines = crate::ui::comment_panel::format_comment_lines(
        &theme,
        presentation,
        &settled.content,
        Some(LineRange::single(42)),
        80,
        crate::ui::comment_panel::CommentBadge::for_comment(&settled, "user"),
        crate::ui::comment_panel::ThreadDisplay::Resolved,
    );

    let header: String = lines[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    // The state is announced in the badge, as remote resolved threads do.
    assert!(header.contains("resolved"), "{header}");
    assert!(header.contains("ISSUE"), "{header}");
    // Same row count settled or not: the annotation model must not shift.
    assert_eq!(App::comment_display_lines(&settled, 80), lines.len());
}

#[test]
fn should_mark_a_settled_thread_in_the_export() {
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed in def4567");
    crate::review_store::set_thread_resolved(&mut session, &root.id, true).unwrap();

    let md = crate::output::markdown::generate_export_content(
        &session,
        &DiffSource::WorkingTree,
        &[],
        &crate::config::ExportConfig::default(),
        &[],
        None,
    )
    .expect("export should render");

    // Still exported — dropping feedback silently would be worse — but flagged
    // so a reader knows it needs no action.
    assert!(
        md.contains("1. (resolved) **[ISSUE]** `src/main.rs:42`"),
        "{md}"
    );
    assert!(md.contains("- @Claude - fixed in def4567"), "{md}");
}

#[test]
fn should_toggle_the_thread_at_the_cursor_both_ways() {
    // `<leader>r` is one key for both directions, so the toggle has to read
    // the current state rather than assume it.
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed in def4567");
    let mut app = app_for(session);

    cursor_on_line_comment(&mut app, 0);
    assert!(app.toggle_thread_resolved_at_cursor());
    assert!(line_thread(&app.session).iter().all(|c| c.resolved));

    cursor_on_line_comment(&mut app, 0);
    assert!(app.toggle_thread_resolved_at_cursor());
    assert!(line_thread(&app.session).iter().all(|c| !c.resolved));
}

#[test]
fn should_toggle_from_a_reply_row_using_the_threads_state() {
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed in def4567");
    crate::review_store::set_thread_resolved(&mut session, &root.id, true).unwrap();
    let mut app = app_for(session);

    // Cursor on the reply of an already-settled thread: one press reopens it.
    cursor_on_line_comment(&mut app, 1);
    assert!(app.toggle_thread_resolved_at_cursor());
    assert!(line_thread(&app.session).iter().all(|c| !c.resolved));
}

#[test]
fn should_collapse_a_settled_thread_to_one_marker_row() {
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed in def4567");
    reply(&mut session, &root.id, "and covered by a test");
    crate::review_store::set_thread_resolved(&mut session, &root.id, true).unwrap();
    let mut app = app_for(session);
    app.diff_state.viewport_width = 80;

    // Hidden by default: the replies leave the diff, the root becomes a marker.
    let thread = line_thread(&app.session);
    assert!(!app.comment_visible(&thread[1]), "replies drop out");
    assert!(
        app.comment_visible(&thread[0]),
        "the root stays as the marker"
    );
    assert_eq!(app.comment_rows(&thread[0], 80), 1, "one marker row");
    assert_eq!(
        app.thread_display(&thread[0]),
        crate::ui::comment_panel::ThreadDisplay::Collapsed {
            replies: 2,
            expand_key: app.leader_key,
        }
    );

    // Expanded: the whole thread is back at full height.
    app.set_show_resolved_threads(true);
    let thread = line_thread(&app.session);
    assert!(thread.iter().all(|c| app.comment_visible(c)));
    assert!(app.comment_rows(&thread[0], 80) > 1);
    assert_eq!(
        app.thread_display(&thread[0]),
        crate::ui::comment_panel::ThreadDisplay::Resolved
    );
}

#[test]
fn should_keep_annotations_in_step_with_the_collapsed_height() {
    // The scroll model and the renderer must agree, or the cursor lands on the
    // wrong line after a thread collapses.
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed in def4567");
    let mut app = app_for(session);
    app.diff_state.viewport_width = 80;
    app.rebuild_annotations();

    let rows_when_open = app
        .line_annotations
        .iter()
        .filter(|a| matches!(a, AnnotatedLine::LineComment { .. }))
        .count();

    crate::review_store::set_thread_resolved(&mut app.session, &root.id, true).unwrap();
    app.rebuild_annotations();
    let rows_when_collapsed = app
        .line_annotations
        .iter()
        .filter(|a| matches!(a, AnnotatedLine::LineComment { .. }))
        .count();

    assert_eq!(
        rows_when_collapsed, 1,
        "collapsed thread owns exactly one row"
    );
    assert!(rows_when_collapsed < rows_when_open);

    app.set_show_resolved_threads(true);
    let rows_expanded = app
        .line_annotations
        .iter()
        .filter(|a| matches!(a, AnnotatedLine::LineComment { .. }))
        .count();
    assert_eq!(rows_expanded, rows_when_open, "expanding restores the rows");
}

#[test]
fn should_still_reach_a_collapsed_thread_with_the_cursor() {
    // The marker keeps the thread addressable: <leader>r on it reopens.
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed in def4567");
    crate::review_store::set_thread_resolved(&mut session, &root.id, true).unwrap();
    let mut app = app_for(session);
    cursor_on_line_comment(&mut app, 0);

    assert!(app.toggle_thread_resolved_at_cursor());
    assert!(line_thread(&app.session).iter().all(|c| !c.resolved));
}

#[test]
fn should_tell_the_reader_how_to_expand_a_collapsed_thread() {
    // A collapsed row that does not say how to uncollapse it is a dead end —
    // this is the bug the review caught.
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed in def4567");
    crate::review_store::set_thread_resolved(&mut session, &root.id, true).unwrap();
    let app = app_for(session);
    let settled = line_thread(&app.session)[0].clone();

    let lines = crate::ui::comment_panel::format_comment_lines(
        &app.theme,
        crate::ui::comment_panel::CommentTypePresentation {
            label: "ISSUE".to_string(),
            color: app.theme.fg_primary,
        },
        &settled.content,
        Some(LineRange::single(42)),
        80,
        crate::ui::comment_panel::CommentBadge::for_comment(&settled, "user"),
        app.thread_display(&settled),
    );

    assert_eq!(lines.len(), 1, "collapsed to one row");
    let row: String = lines[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(row.contains("resolved (1 reply)"), "{row}");
    assert!(row.contains("to show"), "the way out is on the row: {row}");
    assert!(
        row.contains(app.leader_key),
        "hint names the real leader: {row}"
    );
}

#[test]
fn should_expand_settled_threads_with_enter_on_the_marker() {
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed in def4567");
    crate::review_store::set_thread_resolved(&mut session, &root.id, true).unwrap();
    let mut app = app_for(session);
    cursor_on_line_comment(&mut app, 0);

    assert!(app.toggle_collapsed_thread_at_cursor());
    // Only this thread opens; the review-wide setting is untouched.
    assert!(!app.show_resolved_threads);
    assert!(!app.thread_collapsed(&line_thread(&app.session)[0]));
    // Expanding must not unresolve: the thread is still settled, just visible.
    assert!(line_thread(&app.session).iter().all(|c| c.resolved));
}

#[test]
fn should_collapse_again_with_enter_inside_an_expanded_thread() {
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed in def4567");
    crate::review_store::set_thread_resolved(&mut session, &root.id, true).unwrap();
    let mut app = app_for(session);
    app.diff_state.viewport_width = 80;
    app.set_show_resolved_threads(true);
    app.rebuild_annotations();

    // Enter from a reply row folds the thread back to its marker.
    cursor_on_line_comment(&mut app, 1);
    assert!(app.toggle_collapsed_thread_at_cursor());
    assert!(app.thread_collapsed(&line_thread(&app.session)[0]));
    assert!(line_thread(&app.session).iter().all(|c| c.resolved));
    // And the cursor is left on the marker, not stranded past the shrunken
    // document or on some unrelated row.
    assert!(app.diff_state.cursor_line <= app.max_cursor_line());
    assert!(matches!(
        app.line_annotations.get(app.diff_state.cursor_line),
        Some(AnnotatedLine::LineComment { comment_idx: 0, .. })
    ));
}

#[test]
fn should_leave_enter_alone_when_the_cursor_is_not_on_a_marker() {
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    cursor_on_line_comment(&mut app, 0);

    // An open thread's box is not a marker, so Enter falls through to the
    // gap/expander handling it has always done.
    assert!(!app.toggle_collapsed_thread_at_cursor());
    assert!(!app.show_resolved_threads);
    assert!(app.thread_display_overrides.is_empty());
}

#[test]
fn should_expand_only_the_thread_under_the_cursor() {
    // Opening one settled thread must not open the others: their rows would
    // appear above the reader and shove the page.
    let mut session = session_with_line_comment().0;
    let first = line_thread(&session)[0].clone();
    let other = Comment::new(
        "second thread".to_string(),
        CommentType::from_id("note"),
        Some(LineSide::New),
    );
    session
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .add_line_comment(42, other.clone());
    crate::review_store::set_thread_resolved(&mut session, &first.id, true).unwrap();
    crate::review_store::set_thread_resolved(&mut session, &other.id, true).unwrap();

    let mut app = app_for(session);
    app.diff_state.viewport_width = 80;
    cursor_on_line_comment(&mut app, 0);
    assert!(app.toggle_collapsed_thread_at_cursor());

    let thread = line_thread(&app.session);
    assert!(!app.thread_collapsed(&thread[0]), "the one acted on opens");
    assert!(
        app.thread_collapsed(&thread[1]),
        "its neighbour stays folded"
    );
}

#[test]
fn should_keep_the_thread_where_it_was_on_screen_when_expanding() {
    // The bug this fixes: expanding scrolled the page and lost the reader.
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "a reply");
    reply(&mut session, &root.id, "another reply");
    crate::review_store::set_thread_resolved(&mut session, &root.id, true).unwrap();
    let mut app = app_for(session);
    app.diff_state.viewport_width = 80;
    app.rebuild_annotations();

    let marker_line = app
        .line_annotations
        .iter()
        .position(|a| matches!(a, AnnotatedLine::LineComment { comment_idx: 0, .. }))
        .expect("marker row");
    app.diff_state.cursor_line = marker_line;
    app.diff_state.scroll_offset = marker_line.saturating_sub(3);
    let screen_row_before = app.diff_state.cursor_line - app.diff_state.scroll_offset;

    assert!(app.toggle_collapsed_thread_at_cursor());

    let screen_row_after = app.diff_state.cursor_line - app.diff_state.scroll_offset;
    assert_eq!(
        screen_row_after, screen_row_before,
        "the thread stays on the same screen row"
    );
    // And the cursor is still on that thread, not on whatever row moved into
    // its old index.
    assert!(matches!(
        app.line_annotations.get(app.diff_state.cursor_line),
        Some(AnnotatedLine::LineComment { comment_idx: 0, .. })
    ));
}

#[test]
fn should_not_offer_hidden_replies_when_jumping_between_comments() {
    // `m` walks the comment navigator; a settled thread's replies are not on
    // screen, so stopping on them would jump the cursor to a row that is not
    // there.
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "a reply");
    crate::review_store::set_thread_resolved(&mut session, &root.id, true).unwrap();
    let mut app = app_for(session);
    app.diff_state.viewport_width = 80;
    app.rebuild_annotations();

    let items = app.build_comment_navigator_items();
    for item in &items {
        assert!(
            item.target_annotation < app.line_annotations.len(),
            "navigator points at a row that exists"
        );
    }
}

#[test]
fn should_stamp_the_session_when_handing_the_review_to_an_agent() {
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    assert!(app.session.agent_request.is_none());

    app.submit_to_agent();

    // The stamp is what a waiting `review watch` keys off.
    assert!(app.session.agent_request.is_some());
    // Handing off is not forge submit: nothing locks, so the conversation can
    // carry on in the thread.
    assert!(line_thread(&app.session).iter().all(|c| !c.is_locked()));

    let first = app.session.agent_request;
    app.submit_to_agent();
    assert_ne!(
        app.session.agent_request, first,
        "a second handoff moves the stamp, so a fresh watch fires again"
    );
}

#[test]
fn should_skip_settled_threads_when_iterating_comments() {
    // `m` walks the navigator items; a settled thread is not work, so it drops
    // out of the rotation while it is folded.
    let mut session = session_with_line_comment().0;
    let open = line_thread(&session)[0].clone();
    let settled = Comment::new(
        "already handled".to_string(),
        CommentType::from_id("note"),
        Some(LineSide::New),
    );
    session
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .add_line_comment(42, settled.clone());
    crate::review_store::set_thread_resolved(&mut session, &settled.id, true).unwrap();

    let mut app = app_for(session);
    app.diff_state.viewport_width = 80;
    app.rebuild_annotations();

    let ids: Vec<String> = app
        .build_comment_navigator_items()
        .iter()
        .filter_map(|item| {
            app.line_annotations
                .get(item.target_annotation)
                .and_then(|a| app.annotation_comment_id_for_test(a))
                .map(str::to_string)
        })
        .collect();
    assert!(ids.contains(&open.id), "the open thread is still reachable");
    assert!(!ids.contains(&settled.id), "the settled one is skipped");

    // Showing settled threads puts them back: what is on screen is what `m`
    // visits.
    app.set_show_resolved_threads(true);
    let ids: Vec<String> = app
        .build_comment_navigator_items()
        .iter()
        .filter_map(|item| {
            app.line_annotations
                .get(item.target_annotation)
                .and_then(|a| app.annotation_comment_id_for_test(a))
                .map(str::to_string)
        })
        .collect();
    assert!(
        ids.contains(&settled.id),
        "expanded threads are visitable again"
    );
}

#[test]
fn should_keep_a_reviewed_file_visible_while_a_thread_on_it_is_open() {
    // Ticking a file off does not settle the conversations on it: hiding it
    // would take the reader's own unanswered comments off the screen, and
    // `m` only reaches what is on screen.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    let path = PathBuf::from("src/main.rs");
    app.session.get_file_mut(&path).unwrap().reviewed = true;
    app.set_show_reviewed(false);

    assert!(app.file_has_open_threads(&path));
    assert!(
        app.file_passes_filter(&app.diff_files[0]),
        "a reviewed file with an open thread stays on screen"
    );
}

#[test]
fn should_hide_a_reviewed_file_once_its_threads_are_settled() {
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "fixed");
    crate::review_store::set_thread_resolved(&mut session, &root.id, true).unwrap();
    let mut app = app_for(session);
    let path = PathBuf::from("src/main.rs");
    app.session.get_file_mut(&path).unwrap().reviewed = true;
    app.set_show_reviewed(false);

    assert!(!app.file_has_open_threads(&path));
    assert!(
        !app.file_passes_filter(&app.diff_files[0]),
        "reviewed and nothing open — now it is really done"
    );
}

#[test]
fn should_not_count_a_reply_as_an_open_thread_on_its_own() {
    // A reply belongs to its root's thread; resolving the thread settles both.
    let (mut session, root) = session_with_line_comment();
    reply(&mut session, &root.id, "answered");
    crate::review_store::set_thread_resolved(&mut session, &root.id, true).unwrap();
    let app = app_for(session);

    assert!(!app.file_has_open_threads(&PathBuf::from("src/main.rs")));
}

#[test]
fn should_count_the_same_rows_the_renderer_emits_at_any_box_width() {
    // The bug this pins: `comment_rows` takes the width a box is *formatted*
    // at. Feed it the panel width instead — as the side-by-side renderer did —
    // and a narrow side box wraps to more rows than the annotation model
    // believes, so every row below it addresses the wrong line and commenting
    // on L24 lands on L20.
    let (session, _root) = session_with_line_comment();
    let app = app_for(session);
    let comment = Comment::new(
        "a comment long enough that the box width decides how many rows it \
         takes, which is the whole point of this test"
            .to_string(),
        CommentType::from_id("issue"),
        Some(LineSide::New),
    );
    let presentation = crate::ui::comment_panel::CommentTypePresentation {
        label: "ISSUE".to_string(),
        color: app.theme.fg_primary,
    };

    // A detached comment renders one extra row — the line it was written
    // about — so the model has to count that too, at every width.
    let mut detached = comment.clone();
    detached.outdated = true;
    detached.line_context = Some(crate::model::LineContext {
        new_line: Some(133),
        old_line: None,
        content: "            line_context: None,".to_string(),
        before: vec![
            "            author,".to_string(),
            "            comment_type,".to_string(),
        ],
        after: vec!["            commit_id: None,".to_string()],
        commit: None,
    });
    for box_width in [28usize, 40, 55, 80, 120] {
        let rendered = crate::ui::comment_panel::format_comment_lines(
            &app.theme,
            presentation.clone(),
            &detached.content,
            None,
            box_width,
            crate::ui::comment_panel::CommentBadge::Own,
            app.thread_display(&detached),
        );
        assert_eq!(
            app.comment_rows(&detached, box_width),
            rendered.len(),
            "detached comment: model disagrees with the renderer at width {box_width}"
        );
        // The commented line is marked, its neighbours are shown around it:
        // one line alone rarely says what a remark was about.
        let block: Vec<String> = rendered[1..5]
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        // Narrow boxes truncate, so match on the start of each line.
        assert!(block[0].contains("author,"), "{block:?}");
        assert!(block[1].contains("comment_type,"), "{block:?}");
        assert!(
            block[2].contains('>') && block[2].contains("line_context"),
            "the commented line is the marked one: {block:?}"
        );
        assert!(block[3].contains("commit_id"), "{block:?}");
        // The block's shared indentation is stripped, or a side pane shows
        // nothing but leading spaces and an ellipsis.
        assert!(
            !block[0].contains("            author"),
            "shared indent should be stripped: {block:?}"
        );
    }

    // Narrow (a side pane) through wide (full width): the model must agree
    // with the renderer at every width, not just the one it was written for.
    for box_width in [28usize, 40, 55, 80, 120] {
        let rendered = crate::ui::comment_panel::format_comment_lines(
            &app.theme,
            presentation.clone(),
            &comment.content,
            Some(LineRange::single(42)),
            box_width,
            crate::ui::comment_panel::CommentBadge::Own,
            crate::ui::comment_panel::ThreadDisplay::Open,
        );
        assert_eq!(
            app.comment_rows(&comment, box_width),
            rendered.len(),
            "row model disagrees with the renderer at box width {box_width}"
        );
    }
}

#[test]
fn should_refuse_to_re_resolve_a_selector_opened_review() {
    // Nothing was typed to re-run, so there is no honest way to guess what the
    // reader meant. Say so rather than silently reloading the same commits.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    app.session.revset = None;

    let err = app.resolve_revset_to_current_commits().unwrap_err();
    let TuicrError::InvalidInput(message) = &err else {
        panic!("expected InvalidInput, got {err:?}");
    };
    assert!(message.contains("commit selector"), "{message}");
    assert!(message.contains("reopen"), "{message}");
}

#[test]
fn should_record_the_revset_so_a_reload_can_re_run_it() {
    // The regression this whole change is about: a session that only knows its
    // resolved SHAs cannot find the branch again after an amend.
    let (mut session, _root) = session_with_line_comment();
    session.revset = Some("main..HEAD".to_string());
    session.commit_range = Some(vec!["deadbeef".to_string()]);
    let app = app_for(session);

    assert_eq!(app.session.revset.as_deref(), Some("main..HEAD"));
    // And it survives a round trip through storage, or a restart loses it.
    let json = serde_json::to_string(&app.session).unwrap();
    let restored: ReviewSession = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.revset.as_deref(), Some("main..HEAD"));
}

#[test]
fn should_tell_the_reviewer_when_an_agent_changed_the_code() {
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    app.pending_agent_update = Some(crate::model::review::AgentUpdate {
        at: chrono::Utc::now(),
        message: Some("rebased onto main, dropped the duplicate".to_string()),
    });

    assert!(app.poll_agent_update_for_test());

    let shown = app.message.clone().expect("a message was shown");
    assert!(
        shown.content.contains("rebased onto main"),
        "{:?}",
        shown.content
    );
    // Naming the command matters: the diff on screen is stale until they run it.
    assert!(shown.content.contains(":reload"), "{:?}", shown.content);
    // Announced once, not on every poll.
    assert!(!app.poll_agent_update_for_test());
}

#[test]
fn should_hold_the_announcement_while_a_comment_is_open() {
    // Interrupting someone mid-comment to say the branch moved would be worse
    // than telling them a second later.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    app.input_mode = InputMode::Comment;
    app.pending_agent_update = Some(crate::model::review::AgentUpdate {
        at: chrono::Utc::now(),
        message: None,
    });

    assert!(!app.poll_agent_update_for_test());
    assert!(app.pending_agent_update.is_some(), "kept for later");

    app.input_mode = InputMode::Normal;
    assert!(app.poll_agent_update_for_test());
}

/// A cumulative diff after fixup commits rewrote the commented lines: the
/// original content is gone, so re-anchoring strands the comments at file
/// level as outdated.
fn diff_file_with_line(path: &str, content: &str) -> DiffFile {
    let hunks = vec![DiffHunk {
        header: "@@ -42,1 +42,1 @@".to_string(),
        lines: vec![DiffLine {
            origin: LineOrigin::Addition,
            content: content.to_string(),
            old_lineno: None,
            new_lineno: Some(42),
            highlighted_spans: None,
        }],
        old_start: 42,
        old_count: 1,
        new_start: 42,
        new_count: 1,
    }];
    let content_hash = DiffFile::compute_content_hash(&hunks);
    DiffFile {
        old_path: None,
        new_path: Some(PathBuf::from(path)),
        status: FileStatus::Modified,
        hunks,
        is_binary: false,
        is_too_large: false,
        is_commit_message: false,
        content_hash,
    }
}

/// App mid-review of `aaa` (initial) + `bbb` (its fixup): the reviewer's
/// comment was written while looking at `aaa`, whose line the fixup rewrote.
fn app_with_detached_initial_commit_comment() -> App {
    let (mut session, root) = session_with_line_comment();
    {
        let comment = session
            .get_file_mut(&PathBuf::from("src/main.rs"))
            .unwrap()
            .line_comments
            .get_mut(&42)
            .unwrap()
            .iter_mut()
            .find(|c| c.id == root.id)
            .unwrap();
        comment.line_context = Some(crate::model::LineContext {
            new_line: Some(42),
            old_line: None,
            content: "let x = 1;".to_string(),
            before: Vec::new(),
            after: Vec::new(),
            commit: Some("aaa".to_string()),
        });
    }
    session.commit_range = Some(vec!["aaa".to_string(), "bbb".to_string()]);
    let mut app = app_with_session(session);
    app.diff_source = DiffSource::CommitRange(vec!["aaa".to_string(), "bbb".to_string()]);
    // Newest-first, as the pane stores them.
    app.review_commits = vec![
        commit_info("bbb", "fixup! the initial commit"),
        commit_info("aaa", "the initial commit"),
    ];
    app.show_commit_selector = true;

    // Reload lands the post-fixup cumulative diff: line 42 no longer reads
    // as it did when the comment was written.
    app.apply_diff_files(vec![diff_file_with_line("src/main.rs", "let x = fixed();")]);
    app
}

#[test]
fn should_hide_a_detached_comment_when_only_other_commits_are_selected() {
    let mut app = app_with_detached_initial_commit_comment();

    // The re-anchor pass stranded the comment at file level, outdated.
    let review = app
        .session
        .files
        .get(&PathBuf::from("src/main.rs"))
        .unwrap();
    assert!(
        !review.line_comments.contains_key(&42),
        "detached from its line"
    );
    assert_eq!(review.file_comments.len(), 1);
    assert!(review.file_comments[0].outdated);
    assert!(review.file_comments[0].commit_id.is_none());

    // Narrow the review to the fixup commit (data index 0, newest-first):
    // the comment was written on `aaa`, so `bbb`'s view must not show it.
    app.commit_selection_range = Some((0, 0));
    app.rebuild_annotations();
    let shown = app
        .line_annotations
        .iter()
        .any(|a| matches!(a, AnnotatedLine::FileComment { .. }));
    assert!(
        !shown,
        "a comment written on the initial commit must not appear (as outdated) \
         in the fixup commit's view"
    );

    // The full selection keeps it visible: detached, but never dropped.
    app.commit_selection_range = Some((0, 1));
    app.rebuild_annotations();
    let shown = app
        .line_annotations
        .iter()
        .any(|a| matches!(a, AnnotatedLine::FileComment { .. }));
    assert!(
        shown,
        "the full-range view still shows the detached comment"
    );

    // And with no selection at all it stays visible too.
    app.commit_selection_range = None;
    app.rebuild_annotations();
    let shown = app
        .line_annotations
        .iter()
        .any(|a| matches!(a, AnnotatedLine::FileComment { .. }));
    assert!(shown, "no selector: the detached comment stays visible");
}

#[test]
fn should_hide_a_detached_thread_whole_not_just_its_root() {
    // The agent replied before writing the fixup; the reply clones the
    // root's context, so the whole thread detaches — and hides — together.
    let (mut session, root) = session_with_line_comment();
    {
        let comment = session
            .get_file_mut(&PathBuf::from("src/main.rs"))
            .unwrap()
            .line_comments
            .get_mut(&42)
            .unwrap()
            .iter_mut()
            .find(|c| c.id == root.id)
            .unwrap();
        comment.line_context = Some(crate::model::LineContext {
            new_line: Some(42),
            old_line: None,
            content: "let x = 1;".to_string(),
            before: Vec::new(),
            after: Vec::new(),
            commit: Some("aaa".to_string()),
        });
    }
    reply(&mut session, &root.id, "fixed in the next commit");
    session.commit_range = Some(vec!["aaa".to_string(), "bbb".to_string()]);
    let mut app = app_with_session(session);
    app.diff_source = DiffSource::CommitRange(vec!["aaa".to_string(), "bbb".to_string()]);
    app.review_commits = vec![
        commit_info("bbb", "fixup! the initial commit"),
        commit_info("aaa", "the initial commit"),
    ];
    app.show_commit_selector = true;
    app.apply_diff_files(vec![diff_file_with_line("src/main.rs", "let x = fixed();")]);

    let review = app
        .session
        .files
        .get(&PathBuf::from("src/main.rs"))
        .unwrap();
    assert_eq!(
        review.file_comments.len(),
        2,
        "root and reply both stranded"
    );

    app.commit_selection_range = Some((0, 0));
    app.rebuild_annotations();
    let shown = app
        .line_annotations
        .iter()
        .any(|a| matches!(a, AnnotatedLine::FileComment { .. }));
    assert!(
        !shown,
        "neither the root nor its reply belongs in the fixup view"
    );
}

#[test]
fn should_reattach_a_detached_comment_when_its_commit_is_reviewed_alone() {
    let mut app = app_with_detached_initial_commit_comment();

    // Narrowing to the initial commit refetches its own diff, where the
    // commented line still reads exactly as it did.
    app.commit_selection_range = Some((1, 1));
    app.apply_diff_files(vec![diff_file_with_line("src/main.rs", "let x = 1;")]);

    let review = app
        .session
        .files
        .get(&PathBuf::from("src/main.rs"))
        .unwrap();
    assert!(review.file_comments.is_empty(), "restored off file level");
    let comments = review.line_comments.get(&42).expect("back on its line");
    assert_eq!(comments.len(), 1);
    assert!(
        !comments[0].outdated,
        "no longer outdated on its own commit"
    );
}

#[test]
fn should_stamp_a_new_comment_context_with_the_head_of_the_range() {
    // The range view's new side is the head's tree — the one that still holds
    // the commented line. Stamping the oldest commit pointed context recovery
    // (and the detached-comment commit gate) at a tree from before the line
    // existed.
    let (session, _root) = session_with_line_comment();
    let mut session = session;
    session.commit_range = Some(vec!["aaa".to_string(), "bbb".to_string()]);
    let mut app = app_for(session);
    app.diff_source = DiffSource::CommitRange(vec!["aaa".to_string(), "bbb".to_string()]);

    app.enter_comment_mode(false, Some((42, LineSide::New)));
    app.comment_buffer = "about the head's version of this line".to_string();
    app.save_comment();

    let review = app
        .session
        .files
        .get(&PathBuf::from("src/main.rs"))
        .unwrap();
    let stamped = review
        .line_comments
        .get(&42)
        .unwrap()
        .iter()
        .find(|c| c.content.starts_with("about the head"))
        .expect("comment saved");
    assert_eq!(
        stamped
            .line_context
            .as_ref()
            .and_then(|c| c.commit.as_deref()),
        Some("bbb"),
        "context must name the head of the range, not the oldest commit"
    );
}

fn working(message: &str, agent: &str) -> crate::model::review::AgentActivity {
    crate::model::review::AgentActivity {
        at: chrono::Utc::now(),
        message: Some(message.to_string()),
        agent: Some(agent.to_string()),
    }
}

#[test]
fn should_show_that_an_agent_picked_the_review_up() {
    // Between `:submit agent` and the first reply the screen said nothing, so
    // a handoff into a dead terminal looked exactly like one being worked on.
    let (mut session, _root) = session_with_line_comment();
    crate::review_store::set_agent_working(
        &mut session,
        working("reading your six comments", "Claude Opus 5"),
        false,
    );
    let app = app_for(session);

    let status = app.agent_working_status().expect("the reviewer is told");
    assert!(status.live, "just said, so it animates");
    assert_eq!(status.message.as_deref(), Some("reading your six comments"));
    assert_eq!(status.agent.as_deref(), Some("Claude Opus 5"));
    assert!(app.agent_spinner_running());
}

#[test]
fn should_stop_claiming_progress_once_an_agent_goes_quiet() {
    // The flag is written by another process and outlives it. A spinner that
    // never stops is a promise nobody is keeping.
    let (mut session, _root) = session_with_line_comment();
    let mut stale = working("rebasing", "Claude Opus 5");
    stale.at =
        chrono::Utc::now() - chrono::Duration::seconds(crate::app::AGENT_WORKING_LIVE_SECS + 30);
    crate::review_store::set_agent_working(&mut session, stale, false);
    let app = app_for(session);

    let status = app.agent_working_status().expect("still worth showing");
    assert!(!status.live, "but not as progress");
    assert!(status.elapsed_secs >= crate::app::AGENT_WORKING_LIVE_SECS);
    assert!(
        !app.agent_spinner_running(),
        "and the main loop stops redrawing for it"
    );
}

#[test]
fn should_say_so_when_an_agent_stops_without_changing_code() {
    // Answering a question is a result. Clearing the flag silently would look
    // like the agent died on the way.
    let (mut session, _root) = session_with_line_comment();
    crate::review_store::set_agent_working(&mut session, working("reading", "Claude"), false);
    crate::review_store::set_agent_working(
        &mut session,
        working("answered in the thread, no code change", "Claude"),
        true,
    );

    assert!(session.agent_working.is_none(), "no longer working");
    assert_eq!(
        session.agent_update.unwrap().message.as_deref(),
        Some("answered in the thread, no code change"),
        "and the reviewer hears why"
    );
}

#[test]
fn should_drop_a_stale_claim_when_the_review_is_handed_over_again() {
    let (mut session, _root) = session_with_line_comment();
    crate::review_store::set_agent_working(&mut session, working("reading", "Claude"), false);
    let mut app = app_for(session);

    app.submit_to_agent();

    assert!(
        app.session.agent_working.is_none(),
        "a new handoff is not the old agent's work"
    );
    assert!(app.session.agent_request.is_some());
}

#[test]
fn should_anchor_a_comment_by_content_not_only_by_line_number() {
    // Coordinates alone drift: after an amend inserts a line above, line 42 is
    // different code. The stored text is what lets a reload find it again.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    app.line_annotations = vec![AnnotatedLine::DiffLine {
        file_idx: 0,
        hunk_idx: 0,
        line_idx: 0,
        old_lineno: None,
        new_lineno: Some(42),
    }];
    app.diff_state.cursor_line = 0;
    app.diff_state.current_file_idx = 0;

    crate::handler::handle_diff_action(&mut app, crate::input::Action::AddLineComment);
    app.comment_buffer = "this needs a guard".to_string();
    app.save_comment();

    let saved = line_thread(&app.session)
        .into_iter()
        .find(|c| c.content == "this needs a guard")
        .expect("comment saved");
    let context = saved.line_context.expect("line context recorded");
    assert_eq!(context.content, "let x = 1;", "the line's text is stored");
    assert_eq!(context.new_line, Some(42));
}

/// A file whose line 42 says `text`, with `pad` context lines before it — used
/// to simulate an amend that shifts the commented line.
fn file_with_line_at(path: &str, text: &str, at: u32) -> DiffFile {
    let lines: Vec<DiffLine> = (1..=at)
        .map(|n| DiffLine {
            origin: LineOrigin::Addition,
            content: if n == at {
                text.to_string()
            } else {
                format!("filler {n}")
            },
            old_lineno: None,
            new_lineno: Some(n),
            highlighted_spans: None,
        })
        .collect();
    let hunks = vec![DiffHunk {
        header: format!("@@ -1,{at} +1,{at} @@"),
        lines,
        old_start: 1,
        old_count: at,
        new_start: 1,
        new_count: at,
    }];
    let content_hash = DiffFile::compute_content_hash(&hunks);
    DiffFile {
        old_path: None,
        new_path: Some(PathBuf::from(path)),
        status: FileStatus::Modified,
        hunks,
        is_binary: false,
        is_too_large: false,
        is_commit_message: false,
        content_hash,
    }
}

#[test]
fn should_follow_a_comment_when_an_amend_moves_its_line() {
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    // The comment was made on `let x = 1;` at line 42.
    let path = PathBuf::from("src/main.rs");
    let comment = app.session.files[&path].line_comments[&42][0].clone();
    let mut anchored = comment.clone();
    anchored.line_context = Some(crate::model::LineContext {
        new_line: Some(42),
        old_line: None,
        content: "let x = 1;".to_string(),
        before: Vec::new(),
        after: Vec::new(),
        commit: None,
    });
    app.session
        .get_file_mut(&path)
        .unwrap()
        .line_comments
        .insert(42, vec![anchored]);

    // An amend inserted three lines above it: the same code now sits at 45.
    app.diff_files = vec![file_with_line_at("src/main.rs", "let x = 1;", 45)];
    app.reanchor_comments();

    let review = &app.session.files[&path];
    assert!(!review.line_comments.contains_key(&42), "left its old line");
    let moved = &review.line_comments[&45][0];
    assert_eq!(moved.id, comment.id, "same comment, new line");
    assert!(!moved.outdated);
}

#[test]
fn should_mark_a_comment_outdated_when_its_code_is_gone() {
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    let path = PathBuf::from("src/main.rs");
    let mut anchored = app.session.files[&path].line_comments[&42][0].clone();
    anchored.line_context = Some(crate::model::LineContext {
        new_line: Some(42),
        old_line: None,
        content: "let x = 1;".to_string(),
        before: Vec::new(),
        after: Vec::new(),
        commit: None,
    });
    app.session
        .get_file_mut(&path)
        .unwrap()
        .line_comments
        .insert(42, vec![anchored]);

    // The amend replaced that line's code.
    app.diff_files = vec![file_with_line_at("src/main.rs", "something else", 42)];
    app.reanchor_comments();

    // Detached from line 42: the number survives the amend, the code does not,
    // and a comment pinned to whatever now occupies it reads as misplaced.
    let review = &app.session.files[&path];
    assert!(review.line_comments.is_empty());
    let stranded = &review.file_comments[0];
    assert!(stranded.outdated, "marked, not dropped");
    assert_eq!(stranded.content, "handle the empty case");
    assert_eq!(
        stranded.line_context.as_ref().unwrap().new_line,
        Some(42),
        "and it still says where it used to live"
    );
}

#[test]
fn should_leave_comments_alone_in_a_file_absent_from_the_diff() {
    // A narrowed commit selection hides files; that is not the code dying.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    app.diff_files = vec![file_with_line_at("src/other.rs", "unrelated", 3)];

    app.reanchor_comments();

    let review = &app.session.files[&PathBuf::from("src/main.rs")];
    assert!(review.line_comments[&42].iter().all(|c| !c.outdated));
}

#[test]
fn should_not_call_a_pre_anchor_comment_outdated() {
    // Comments written before content anchoring have nothing to match on.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    app.diff_files = vec![file_with_line_at("src/main.rs", "totally different", 42)];

    app.reanchor_comments();

    let kept = &app.session.files[&PathBuf::from("src/main.rs")].line_comments[&42][0];
    assert!(!kept.outdated, "no anchor recorded — keep it where it is");
}

#[test]
fn should_move_a_stranded_comment_to_the_file_so_it_still_shows() {
    // The amend removed the hunk entirely: line 42 has no row left, so a
    // comment sitting there would render nowhere and vanish from `m`.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    let path = PathBuf::from("src/main.rs");
    let mut anchored = app.session.files[&path].line_comments[&42][0].clone();
    anchored.line_context = Some(crate::model::LineContext {
        new_line: Some(42),
        old_line: None,
        content: "let x = 1;".to_string(),
        before: Vec::new(),
        after: Vec::new(),
        commit: None,
    });
    app.session
        .get_file_mut(&path)
        .unwrap()
        .line_comments
        .insert(42, vec![anchored.clone()]);

    app.diff_files = vec![file_with_line_at("src/main.rs", "unrelated", 3)];
    app.reanchor_comments();

    let review = &app.session.files[&path];
    assert!(review.line_comments.is_empty(), "no longer on a dead line");
    let stranded = &review.file_comments[0];
    assert_eq!(stranded.id, anchored.id);
    assert!(stranded.outdated);
    // It still carries where it came from, so the reader can place it.
    assert_eq!(stranded.line_context.as_ref().unwrap().new_line, Some(42));
}

#[test]
fn should_send_a_stranded_comment_home_when_its_code_comes_back() {
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    let path = PathBuf::from("src/main.rs");
    let mut stranded = app.session.files[&path].line_comments[&42][0].clone();
    stranded.outdated = true;
    stranded.line_context = Some(crate::model::LineContext {
        new_line: Some(42),
        old_line: None,
        content: "let x = 1;".to_string(),
        before: Vec::new(),
        after: Vec::new(),
        commit: None,
    });
    {
        let review = app.session.get_file_mut(&path).unwrap();
        review.line_comments.clear();
        review.file_comments = vec![stranded.clone()];
    }

    // Undo the amend: the line is back, two lines lower.
    app.diff_files = vec![file_with_line_at("src/main.rs", "let x = 1;", 44)];
    app.reanchor_comments();

    let review = &app.session.files[&path];
    assert!(review.file_comments.is_empty(), "no longer stranded");
    let home = &review.line_comments[&44][0];
    assert_eq!(home.id, stranded.id);
    assert!(!home.outdated, "and no longer outdated");
}

#[test]
fn should_say_outdated_in_the_comment_box() {
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    let path = PathBuf::from("src/main.rs");
    app.session
        .get_file_mut(&path)
        .unwrap()
        .line_comments
        .get_mut(&42)
        .unwrap()[0]
        .outdated = true;
    let comment = app.session.files[&path].line_comments[&42][0].clone();

    let lines = crate::ui::comment_panel::format_comment_lines(
        &app.theme,
        crate::ui::comment_panel::CommentTypePresentation {
            label: "ISSUE".to_string(),
            color: app.theme.fg_primary,
        },
        &comment.content,
        Some(LineRange::single(42)),
        80,
        crate::ui::comment_panel::CommentBadge::for_comment(&comment, "user"),
        app.thread_display(&comment),
    );
    let header: String = lines[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(header.contains("outdated"), "{header}");
    // Not silently mistaken for settled.
    assert!(!header.contains("resolved"), "{header}");
}

#[test]
fn should_never_leave_a_comment_on_code_it_was_not_written_about() {
    // The failure this pins, seen for real: history was rewritten under an
    // open review, the range was adopted, and a comment stayed at its old line
    // number — now unrelated code — reported as perfectly current. Either it
    // follows its text or it says `outdated`; sitting silently on someone
    // else's line is the one outcome that must not happen.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    let path = PathBuf::from("src/main.rs");
    let mut anchored = app.session.files[&path].line_comments[&42][0].clone();
    anchored.line_context = Some(crate::model::LineContext {
        new_line: Some(42),
        old_line: None,
        content: "let x = 1;".to_string(),
        before: Vec::new(),
        after: Vec::new(),
        commit: None,
    });
    app.session
        .get_file_mut(&path)
        .unwrap()
        .line_comments
        .insert(42, vec![anchored]);

    // The amend replaced that line's code; line 42 still exists, saying
    // something else entirely.
    app.diff_files = vec![file_with_line_at("src/main.rs", "let y = 2;", 42)];
    app.reanchor_comments();

    let review = &app.session.files[&path];
    let survivors: Vec<_> = review
        .line_comments
        .values()
        .flatten()
        .chain(review.file_comments.iter())
        .collect();
    assert_eq!(survivors.len(), 1, "the comment is never dropped");
    let comment = survivors[0];
    let still_at_42 = review
        .line_comments
        .get(&42)
        .is_some_and(|cs| cs.iter().any(|c| c.id == comment.id));
    assert!(
        comment.outdated && !still_at_42,
        "line 42 is not what it was written about any more, so the comment is \
         marked outdated and detached from it"
    );
}

#[test]
fn should_find_a_review_again_after_its_commits_were_rewritten() {
    // Sessions are keyed by their resolved commits, so an amend leaves the
    // review unreachable by that key. Reopening with the same expression has
    // to find it, or the comments are stranded and the reader starts empty —
    // which is what happened in a real review.
    use crate::persistence::storage;

    let dir = tempfile::tempdir().unwrap();
    storage::set_test_reviews_dir(Some(dir.path().to_path_buf()));

    let (mut session, _root) = session_with_line_comment();
    session.revset = Some("main..HEAD".to_string());
    session.commit_range = Some(vec!["oldsha1".to_string()]);
    session.repo_path = std::env::current_dir().unwrap();
    storage::save_session(&session).expect("saved under the old range");

    let found = storage::find_local_session_by_revset(&session.repo_path, "main..HEAD")
        .expect("lookup ran")
        .expect("the review is found by what it was opened with");
    assert_eq!(found.1.id, session.id);
    assert_eq!(
        found
            .1
            .files
            .values()
            .map(|f| f.comment_count())
            .sum::<usize>(),
        1,
        "and it still carries its comments"
    );

    // A different expression must not adopt someone else's review.
    let other = storage::find_local_session_by_revset(&session.repo_path, "other..HEAD").unwrap();
    assert!(other.is_none());

    // Reopening after an amend leaves an empty session for the new range, and
    // it is always the most recent. Adopting *that* would leave the review it
    // was meant to rescue behind, so work beats recency.
    let mut empty = ReviewSession::new(
        session.repo_path.clone(),
        "newsha".to_string(),
        Some("main".to_string()),
        SessionDiffSource::CommitRange,
    );
    empty.revset = Some("main..HEAD".to_string());
    empty.commit_range = Some(vec!["newsha".to_string()]);
    storage::save_session(&empty).expect("saved the empty one, newer");

    let found = storage::find_local_session_by_revset(&session.repo_path, "main..HEAD")
        .unwrap()
        .expect("still finds one");
    assert_eq!(found.1.id, session.id, "the review with comments wins");
}

#[test]
fn should_not_let_a_rewrite_hide_comments_behind_a_dead_commit() {
    // The failure this pins, seen for real: after an amend the review was
    // adopted, the comments were re-anchored and marked outdated — and the
    // pane showed nothing at all. Each comment recorded the commit it was made
    // against, `comment_visible` hides comments outside the current selection,
    // and every one of those SHAs had been rewritten away.
    let (mut session, root) = session_with_line_comment();
    session
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .line_comments
        .get_mut(&42)
        .unwrap()[0]
        .commit_id = Some("deadbeef".to_string());
    let _ = root;

    let cleared = App::clear_stale_commit_scopes(&mut session, &["c0ffee".to_string()]);
    assert_eq!(cleared, 1);

    let comment = &session.files[&PathBuf::from("src/main.rs")].line_comments[&42][0];
    assert!(
        comment.commit_id.is_none(),
        "the commit it named is gone, so the comment stops being scoped to it"
    );
    assert!(
        App::comment_visible_with(comment, None),
        "and it is visible again rather than silently filtered out"
    );
}

#[test]
fn should_keep_commit_scoping_that_is_still_live() {
    let (mut session, _root) = session_with_line_comment();
    session
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .line_comments
        .get_mut(&42)
        .unwrap()[0]
        .commit_id = Some("c0ffee".to_string());

    let cleared = App::clear_stale_commit_scopes(&mut session, &["c0ffee".to_string()]);

    assert_eq!(cleared, 0, "a commit still under review keeps its scoping");
    let comment = &session.files[&PathBuf::from("src/main.rs")].line_comments[&42][0];
    assert_eq!(comment.commit_id.as_deref(), Some("c0ffee"));
}

fn commit_info(short_id: &str, summary: &str) -> crate::vcs::CommitInfo {
    crate::vcs::CommitInfo {
        id: short_id.to_string(),
        short_id: short_id.to_string(),
        branch_name: None,
        summary: summary.to_string(),
        body: None,
        author: "tester".to_string(),
        time: chrono::Utc::now(),
    }
}

fn commit_message_file(short_id: &str, lines: &[&str]) -> DiffFile {
    let hunk_lines: Vec<DiffLine> = lines
        .iter()
        .enumerate()
        .map(|(i, text)| DiffLine {
            origin: LineOrigin::Context,
            content: (*text).to_string(),
            old_lineno: None,
            new_lineno: Some(i as u32 + 1),
            highlighted_spans: None,
        })
        .collect();
    let hunks = vec![DiffHunk {
        header: String::new(),
        lines: hunk_lines,
        old_start: 0,
        old_count: 0,
        new_start: 1,
        new_count: lines.len() as u32,
    }];
    let content_hash = DiffFile::compute_content_hash(&hunks);
    DiffFile {
        old_path: None,
        new_path: Some(PathBuf::from(format!("Commit Message ({short_id})"))),
        status: FileStatus::Added,
        hunks,
        is_binary: false,
        is_too_large: false,
        is_commit_message: true,
        content_hash,
    }
}

/// Put a comment on line `line` of the message of commit `short_id`, the way
/// the app stores one: under a path that embeds the short id.
fn comment_on_message(app: &mut App, short_id: &str, line: u32, text: &str, body: &str) -> Comment {
    let path = PathBuf::from(format!("Commit Message ({short_id})"));
    let mut comment = Comment::new(
        body.to_string(),
        CommentType::from_id("issue"),
        Some(LineSide::New),
    );
    comment.line_context = Some(crate::model::LineContext {
        new_line: Some(line),
        old_line: None,
        content: text.to_string(),
        before: Vec::new(),
        after: Vec::new(),
        commit: None,
    });
    app.session.add_file(path.clone(), FileStatus::Added, 0);
    app.session
        .get_file_mut(&path)
        .unwrap()
        .add_line_comment(line, comment.clone());
    comment
}

#[test]
fn should_carry_a_commit_message_comment_across_an_amend() {
    // The pseudo path embeds the commit's short id, so an amend left the thread
    // under a path the diff no longer had: unreachable, and not even outdated.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    let written = comment_on_message(&mut app, "c82a8fa", 2, "why it changed", "explain this");

    // The amend kept that line, and moved it down by one.
    app.diff_files = vec![commit_message_file(
        "2f2f7ae",
        &["summary", "", "why it changed"],
    )];
    app.review_commits = vec![commit_info("2f2f7ae", "summary")];
    let changed = app.reanchor_comments();

    assert!(changed > 0, "and the move is worth persisting");
    let new = PathBuf::from("Commit Message (2f2f7ae)");
    let moved = &app.session.files[&new].line_comments[&3][0];
    assert_eq!(
        moved.id, written.id,
        "same comment, message under review now"
    );
    assert!(!moved.outdated, "its line is still there");
    assert!(
        !app.session
            .files
            .contains_key(&PathBuf::from("Commit Message (c82a8fa)")),
        "the spent husk goes"
    );
}

#[test]
fn should_keep_a_commit_message_comment_when_the_amend_rewrote_its_line() {
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    let written = comment_on_message(&mut app, "c82a8fa", 2, "why it changed", "explain this");

    // This time the amend rewrote the line the comment was made on.
    app.diff_files = vec![commit_message_file("2f2f7ae", &["summary", "", "reworded"])];
    app.review_commits = vec![commit_info("2f2f7ae", "summary")];
    app.reanchor_comments();

    // Outdated at file level, as any comment whose code is gone — but on the
    // message that is on screen, so `m` still reaches it.
    let review = &app.session.files[&PathBuf::from("Commit Message (2f2f7ae)")];
    assert!(review.line_comments.is_empty());
    let stranded = &review.file_comments[0];
    assert_eq!(stranded.id, written.id);
    assert!(stranded.outdated, "marked, not dropped");
    assert_eq!(
        stranded.line_context.as_ref().unwrap().new_line,
        Some(2),
        "and it still says where it used to live"
    );
}

#[test]
fn should_show_the_lost_code_once_for_the_whole_thread() {
    // Every message in a detached thread is marked outdated together, so the
    // block of code they are about was printed above each one — the same six
    // lines, as many times as the thread was long.
    let (mut session, root) = session_with_line_comment();
    let answer = reply(&mut session, &root.id, "done");
    let mut app = app_for(session);
    let path = PathBuf::from("src/main.rs");
    for comment in app
        .session
        .get_file_mut(&path)
        .unwrap()
        .line_comments
        .get_mut(&42)
        .unwrap()
    {
        comment.outdated = true;
        comment.line_context = Some(crate::model::LineContext {
            new_line: Some(42),
            old_line: None,
            content: "let x = 1;".to_string(),
            before: vec!["fn main() {".to_string()],
            after: vec!["}".to_string()],
            commit: None,
        });
    }
    let thread = line_thread(&app.session);
    let (root, answer) = (
        thread.iter().find(|c| c.id == root.id).unwrap(),
        thread.iter().find(|c| c.id == answer.id).unwrap(),
    );

    use crate::ui::comment_panel::ThreadDisplay;
    assert!(
        matches!(
            app.thread_display(root),
            ThreadDisplay::Outdated {
                was_text: Some(_),
                ..
            }
        ),
        "the root says what the thread was about"
    );
    assert!(
        matches!(
            app.thread_display(answer),
            ThreadDisplay::Outdated { was_text: None, .. }
        ),
        "the reply below it does not say it again"
    );
    // And the row model agrees, or every row under the box drifts.
    assert_eq!(
        App::comment_display_lines_for_box(root, 60, false)
            - App::comment_display_lines_for_box(answer, 60, false),
        3,
        "three rows of remembered code, on the root alone"
    );
}

#[test]
fn should_finish_a_migration_that_already_half_landed() {
    // The comments were carried over once and the old copies came back from
    // disk. Carrying them again must clear the husk, not double what is there.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    let written = comment_on_message(&mut app, "c82a8fa", 3, "why it changed", "explain this");
    let amended = commit_message_file("2f2f7ae", &["summary", "", "why it changed"]);
    app.session.add_diff_file(&amended);
    app.session
        .get_file_mut(&PathBuf::from("Commit Message (2f2f7ae)"))
        .unwrap()
        .add_line_comment(3, written.clone());
    app.diff_files = vec![amended];
    app.review_commits = vec![commit_info("2f2f7ae", "summary")];

    app.reanchor_comments();

    let review = &app.session.files[&PathBuf::from("Commit Message (2f2f7ae)")];
    assert_eq!(review.comment_count(), 1, "one comment, not two");
    assert!(
        !app.session
            .files
            .contains_key(&PathBuf::from("Commit Message (c82a8fa)")),
        "and the copy it came from is gone"
    );
}

#[test]
fn should_place_a_carried_comment_when_the_message_arrives() {
    // Startup builds the app, anchors comments, and only then works out which
    // commit is under review and puts its message on screen. A migration that
    // ran only during anchoring would never see the message at all — which is
    // exactly how this bug survived its first fix.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    let written = comment_on_message(&mut app, "c82a8fa", 3, "why it changed", "explain this");
    app.diff_files = Vec::new();
    app.review_commits = vec![commit_info("2f2f7ae", "diff: look around while composing")];
    app.review_commits[0].body = Some("why it changed".to_string());

    app.insert_commit_message_if_single();

    let moved = &app.session.files[&PathBuf::from("Commit Message (2f2f7ae)")].line_comments[&3][0];
    assert_eq!(moved.id, written.id, "carried onto the message on screen");
    assert!(!moved.outdated, "and back on the line it was written about");
}

#[test]
fn should_not_file_a_comment_under_a_commit_it_was_never_about() {
    // `HEAD^..HEAD` still means "the last commit" once a commit lands on top,
    // so the message that left the review did not necessarily leave by amend.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    comment_on_message(&mut app, "c82a8fa", 2, "why it changed", "explain this");
    let old = PathBuf::from("Commit Message (c82a8fa)");
    // The rewritten object is still readable, and it says what it was about.
    app.vcs = Box::new(DummyVcs {
        info: app.vcs_info.clone(),
        commits: vec![commit_info("c82a8fa", "the commit this comment is about")],
        resolved_range: None,
    });

    app.diff_files = vec![commit_message_file(
        "af9e922",
        &["something else", "", "unrelated"],
    )];
    app.review_commits = vec![commit_info("af9e922", "something else")];
    app.reanchor_comments();

    assert_eq!(
        app.session.files[&old].line_comments[&2][0].content, "explain this",
        "left where it was: out of this view beats filed under the wrong commit"
    );
    assert!(
        app.session
            .files
            .get(&PathBuf::from("Commit Message (af9e922)"))
            .is_none_or(|review| review.comment_count() == 0)
    );
}

#[test]
fn should_not_drag_another_commits_message_comments_along() {
    // Narrowing a multi-commit review to one commit puts that commit's message
    // on screen; the other commits are still under review and keep their own.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    let written = comment_on_message(&mut app, "aaaaaaa", 2, "why it changed", "explain this");
    app.review_commits = vec![
        commit_info("aaaaaaa", "first"),
        commit_info("bbbbbbb", "second"),
    ];

    // The reader selected the second commit, so its message is the one shown.
    app.diff_files = vec![commit_message_file("bbbbbbb", &["second", "", "unrelated"])];
    app.reanchor_comments();

    let review = &app.session.files[&PathBuf::from("Commit Message (aaaaaaa)")];
    assert_eq!(
        review.line_comments[&2][0].id, written.id,
        "left on the commit it was written about"
    );
    assert!(!review.line_comments[&2][0].outdated);
}

#[test]
fn should_leave_a_reviewed_husk_standing() {
    // Dropping review state to tidy up has cost this branch data before.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    comment_on_message(&mut app, "c82a8fa", 2, "why it changed", "explain this");
    let old = PathBuf::from("Commit Message (c82a8fa)");
    app.session.get_file_mut(&old).unwrap().reviewed = true;

    let amended = commit_message_file("2f2f7ae", &["summary", "", "why it changed"]);
    app.session.add_diff_file(&amended);
    app.diff_files = vec![amended];
    app.review_commits = vec![commit_info("2f2f7ae", "summary")];
    app.reanchor_comments();

    assert!(app.session.files.contains_key(&old), "husk kept");
    assert!(
        app.session.files[&old].line_comments.is_empty(),
        "but emptied of the comments that moved"
    );
}

#[test]
fn should_let_the_reader_look_around_while_composing() {
    // Writing a comment usually means checking another part of the file. Until
    // now the only way was to cancel the comment and start again.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    app.diff_state.viewport_height = 20;
    app.input_mode = InputMode::Comment;
    app.comment_buffer = "half a thought".to_string();
    app.comment_cursor = app.comment_buffer.len();
    let before = app.diff_state.scroll_offset;

    crate::handler::handle_comment_action(&mut app, crate::input::Action::PageDown);

    assert_ne!(app.diff_state.scroll_offset, before, "the diff moved");
    assert!(
        app.comment_scroll_detached,
        "and the renderer must stop dragging the editor back into view"
    );
    // The comment itself is untouched: this is looking around, not leaving.
    assert_eq!(app.comment_buffer, "half a thought");
    assert_eq!(app.input_mode, InputMode::Comment);
}

#[test]
fn should_bring_the_editor_back_when_the_reader_types_again() {
    // Typing into a box that is off screen is worse than losing the place you
    // scrolled to.
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    app.input_mode = InputMode::Comment;
    app.comment_scroll_detached = true;

    crate::handler::handle_comment_action(&mut app, crate::input::Action::InsertChar('x'));

    assert!(!app.comment_scroll_detached);
    assert_eq!(app.comment_buffer, "x");
}

#[test]
fn should_start_every_editor_attached() {
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    app.comment_scroll_detached = true;
    app.exit_comment_mode();
    assert!(!app.comment_scroll_detached, "closing resets it");

    app.comment_scroll_detached = true;
    app.enter_comment_mode(false, Some((42, LineSide::New)));
    assert!(
        !app.comment_scroll_detached,
        "a new comment starts with the editor in view"
    );
}

#[test]
fn should_scroll_with_the_wheel_while_composing() {
    // Reaching for the wheel is the first thing anyone does to look at other
    // code; it was being dropped entirely while the editor was open.
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let (session, _root) = session_with_line_comment();
    let mut app = app_for(session);
    app.diff_state.viewport_height = 20;
    app.diff_state.scroll_offset = 10;
    app.diff_area = Some(ratatui::layout::Rect::new(0, 0, 80, 20));
    app.input_mode = InputMode::Comment;
    app.comment_buffer = "half a thought".to_string();
    let _ = MouseButton::Left;

    crate::handler::handle_mouse_event(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        },
    );

    assert!(app.diff_state.scroll_offset < 10, "the view moved");
    assert!(app.comment_scroll_detached, "and stays where it was put");
    // Scrolling is looking, not leaving: the comment survives untouched.
    assert_eq!(app.comment_buffer, "half a thought");
    assert_eq!(app.input_mode, InputMode::Comment);
}

#[test]
fn should_leave_a_session_alone_when_the_store_is_empty() {
    // Until the reader migrates, a review keeps showing exactly what it showed
    // before: an empty store must not blank a session that holds comments.
    let (session, root) = session_with_line_comment();
    let mut app = app_for(session);

    app.hydrate_comments_from_store();

    let thread = line_thread(&app.session);
    assert_eq!(thread.len(), 1, "the session's own comment survives");
    assert_eq!(thread[0].id, root.id);
    assert!(app.comments_from_earlier.is_empty());
}
