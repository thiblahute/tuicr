//! Local comment threads: replies stored beside the comment they answer, and
//! deleted with it.

use crate::app::*;
use crate::model::FileStatus;
use crate::review_store::{ReplyRequest, reply_to_comment_in_session};
use crate::vcs::traits::VcsType;

struct DummyVcs {
    info: VcsInfo,
}

impl VcsBackend for DummyVcs {
    fn info(&self) -> &VcsInfo {
        &self.info
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
    let vcs_info = VcsInfo {
        root_path: PathBuf::from("/repo"),
        head_commit: "head".to_string(),
        branch_name: Some("main".to_string()),
        vcs_type: VcsType::Git,
    };
    App::build(
        Box::new(DummyVcs {
            info: vcs_info.clone(),
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
