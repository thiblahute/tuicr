//! Tests for replying to remote review threads: reply-mode entry from the
//! cursor, the save-comment dispatch guards, and the poll failure path that
//! restores the editor. The forge POST itself is covered by backend tests.
use crate::app::*;
use crate::forge::remote_comments::{RemoteCommentSide, RemoteReviewComment, RemoteReviewThread};
use crate::forge::traits::{ForgeRepository, PrSessionKey};
use crate::model::diff_types::{DiffHunk, DiffLine, FileStatus, LineOrigin};
use crate::vcs::traits::{VcsChangeStatus, VcsType};

struct DummyVcs {
    info: VcsInfo,
}

impl VcsBackend for DummyVcs {
    fn info(&self) -> &VcsInfo {
        &self.info
    }
    fn get_working_tree_diff(&self, _h: &SyntaxHighlighter) -> Result<Vec<DiffFile>> {
        Err(TuicrError::NoChanges)
    }
    fn fetch_context_lines(
        &self,
        _p: &Path,
        _s: FileStatus,
        _ref_commit: Option<&str>,
        _start: u32,
        _end: u32,
    ) -> Result<Vec<DiffLine>> {
        Ok(Vec::new())
    }
    fn get_change_status(&self) -> Result<VcsChangeStatus> {
        Ok(VcsChangeStatus {
            staged: false,
            unstaged: false,
        })
    }
    fn file_line_count(&self, _p: &Path, _s: FileStatus, _ref_commit: Option<&str>) -> Result<u32> {
        Ok(0)
    }
}

fn make_pr_app(file_path: &str) -> App {
    let vcs_info = VcsInfo {
        root_path: PathBuf::from("/tmp/repo"),
        head_commit: "abcdef0123".to_string(),
        branch_name: Some("feat".to_string()),
        vcs_type: VcsType::File,
    };
    let session = ReviewSession::new(
        vcs_info.root_path.clone(),
        vcs_info.head_commit.clone(),
        vcs_info.branch_name.clone(),
        SessionDiffSource::PullRequest,
    );
    let diff_file = DiffFile {
        old_path: Some(PathBuf::from(file_path)),
        new_path: Some(PathBuf::from(file_path)),
        status: FileStatus::Modified,
        hunks: vec![DiffHunk {
            header: "@@".to_string(),
            old_start: 1,
            old_count: 0,
            new_start: 1,
            new_count: 0,
            lines: vec![
                DiffLine {
                    origin: LineOrigin::Context,
                    content: "a".to_string(),
                    old_lineno: Some(10),
                    new_lineno: Some(10),
                    highlighted_spans: None,
                },
                DiffLine {
                    origin: LineOrigin::Addition,
                    content: "b".to_string(),
                    old_lineno: None,
                    new_lineno: Some(11),
                    highlighted_spans: None,
                },
            ],
        }],
        is_binary: false,
        is_too_large: false,
        is_commit_message: false,
        content_hash: 0,
    };
    let pr_source = PullRequestDiffSource {
        key: PrSessionKey::new(
            ForgeRepository::github("github.com", "agavra", "tuicr"),
            125,
            "abcdef0123".to_string(),
        ),
        base_sha: "0000".to_string(),
        title: "test pr".to_string(),
        url: "https://github.com/agavra/tuicr/pull/125".to_string(),
        head_ref_name: "feat".to_string(),
        base_ref_name: "main".to_string(),
        state: "OPEN".to_string(),
        closed: false,
        merged: false,
    };
    App::build(
        Box::new(DummyVcs {
            info: vcs_info.clone(),
        }),
        vcs_info,
        Theme::dark(),
        None,
        false,
        vec![diff_file],
        session,
        DiffSource::PullRequest(Box::new(pr_source)),
        InputMode::Normal,
        Vec::new(),
        None,
        None,
    )
    .expect("build app")
}

fn inline_thread(id: &str, line: u32) -> RemoteReviewThread {
    RemoteReviewThread {
        id: id.to_string(),
        path: "src/lib.rs".to_string(),
        line: Some(line),
        side: RemoteCommentSide::Right,
        is_resolved: false,
        is_outdated: false,
        comments: vec![RemoteReviewComment {
            id: format!("{id}-root"),
            author: Some("alice".to_string()),
            body: "Can this be simplified?".to_string(),
            created_at: None,
            in_reply_to: None,
            database_id: Some(42),
            url: "https://example.com/1".to_string(),
        }],
    }
}

#[test]
fn should_yank_the_thread_comment_under_the_cursor() {
    // given — a thread whose root has a two-line body plus one reply:
    // row 0 header, rows 1-2 root body, row 3 separator, row 4 reply body,
    // row 5 closing rule.
    let mut app = make_pr_app("src/lib.rs");
    let mut thread = inline_thread("T1", 11);
    thread.comments[0].body = "Root line one\nRoot line two".to_string();
    thread.comments.push(RemoteReviewComment {
        id: "T1-reply".to_string(),
        author: Some("bob".to_string()),
        body: "Reply body".to_string(),
        created_at: None,
        in_reply_to: Some("T1-root".to_string()),
        database_id: None,
        url: "https://example.com/2".to_string(),
    });
    app.forge_review_threads = vec![thread];
    app.rebuild_annotations();
    let first_row = app
        .line_annotations
        .iter()
        .position(|a| matches!(a, AnnotatedLine::RemoteThreadLine { .. }))
        .expect("thread rows should be annotated");

    // when/then — rows resolve to the comment whose box they render, and
    // the closing rule sticks to the last one
    for offset in 0..=2 {
        app.diff_state.cursor_line = first_row + offset;
        assert_eq!(
            app.remote_comment_content_at_cursor().as_deref(),
            Some("Root line one\nRoot line two"),
            "row offset {offset}"
        );
    }
    for offset in 3..=5 {
        app.diff_state.cursor_line = first_row + offset;
        assert_eq!(
            app.remote_comment_content_at_cursor().as_deref(),
            Some("Reply body"),
            "row offset {offset}"
        );
    }

    // and — a diff line yields nothing
    let diff_row = app
        .line_annotations
        .iter()
        .position(|a| matches!(a, AnnotatedLine::DiffLine { .. }))
        .expect("expected a diff line annotation");
    app.diff_state.cursor_line = diff_row;
    assert_eq!(app.remote_comment_content_at_cursor(), None);
}

#[test]
fn should_yank_a_remote_review_summary_body() {
    // given — one review summary rendered in the review-scope area
    let mut app = make_pr_app("src/lib.rs");
    app.forge_review_summaries = vec![crate::forge::remote_comments::RemoteReviewSummary {
        id: "R1".to_string(),
        author: Some("alice".to_string()),
        body: "Looks good overall".to_string(),
        state: crate::forge::remote_comments::RemoteReviewState::Approved,
        created_at: None,
        url: "https://example.com/r1".to_string(),
    }];
    app.rebuild_annotations();
    let row = app
        .line_annotations
        .iter()
        .position(|a| matches!(a, AnnotatedLine::RemoteReviewSummaryLine { .. }))
        .expect("summary rows should be annotated");
    app.diff_state.cursor_line = row;

    // when/then
    assert_eq!(
        app.remote_comment_content_at_cursor().as_deref(),
        Some("Looks good overall")
    );
}

#[test]
fn should_enter_reply_mode_from_thread_row_under_cursor() {
    // given — a PR app with one inline thread on the added line
    let mut app = make_pr_app("src/lib.rs");
    app.forge_review_threads = vec![inline_thread("T1", 11)];
    app.rebuild_annotations();
    let thread_row = app
        .line_annotations
        .iter()
        .position(|a| matches!(a, AnnotatedLine::RemoteThreadLine { .. }))
        .expect("thread rows should be annotated");
    app.diff_state.cursor_line = thread_row;

    // when
    assert_eq!(app.remote_thread_at_cursor(), Some(0));
    app.enter_reply_mode(0);

    // then — comment editor opens as a reply anchored at the thread's line
    assert_eq!(app.input_mode, InputMode::Comment);
    assert_eq!(app.comment_reply_target, Some(0));
    assert_eq!(app.comment_line, Some((11, LineSide::New)));
    assert!(!app.comment_is_review_level);
    assert!(app.comment_type.is_none());
    assert_eq!(app.comment_reply_author().as_deref(), Some("alice"));
}

#[test]
fn should_anchor_reply_at_review_scope_for_unanchored_thread() {
    // given — a review-level thread (no line anchor)
    let mut app = make_pr_app("src/lib.rs");
    let mut thread = inline_thread("T1", 11);
    thread.line = None;
    app.forge_review_threads = vec![thread];
    // when
    app.enter_reply_mode(0);
    // then
    assert_eq!(app.input_mode, InputMode::Comment);
    assert!(app.comment_is_review_level);
    assert_eq!(app.comment_line, None);
    assert_eq!(app.comment_reply_target, Some(0));
}

#[test]
fn should_clear_reply_target_when_editor_closes() {
    // given
    let mut app = make_pr_app("src/lib.rs");
    app.forge_review_threads = vec![inline_thread("T1", 11)];
    app.enter_reply_mode(0);
    // when
    app.exit_comment_mode();
    // then
    assert_eq!(app.comment_reply_target, None);
    assert_eq!(app.input_mode, InputMode::Normal);
}

#[test]
fn should_keep_editor_open_when_reply_thread_is_gone() {
    // given — reply mode entered, then the threads list was replaced (e.g.
    // a refetch landed) and the target vanished
    let mut app = make_pr_app("src/lib.rs");
    app.forge_review_threads = vec![inline_thread("T1", 11)];
    app.enter_reply_mode(0);
    app.comment_buffer = "important reply text".to_string();
    app.forge_review_threads.clear();

    // when
    app.save_comment();

    // then — nothing saved locally, editor still open with the text
    assert_eq!(app.input_mode, InputMode::Comment);
    assert_eq!(app.comment_buffer, "important reply text");
    assert!(
        app.session
            .files
            .values()
            .all(|r| r.line_comments.is_empty() && r.file_comments.is_empty())
    );
    assert!(app.session.review_comments.is_empty());
}

#[test]
fn should_refuse_second_reply_while_one_is_in_flight() {
    // given — a reply already in flight
    let mut app = make_pr_app("src/lib.rs");
    app.forge_review_threads = vec![inline_thread("T1", 11)];
    app.pr_reply_state = Some(ReplyInFlightState {
        repository: ForgeRepository::github("github.com", "agavra", "tuicr"),
        pr_number: 125,
        thread_id: "T1".to_string(),
        thread_author: Some("alice".to_string()),
        body: "first".to_string(),
        started_at: Instant::now(),
    });
    app.enter_reply_mode(0);
    app.comment_buffer = "second reply".to_string();

    // when
    app.save_comment();

    // then — refused, editor stays open, no local comment
    assert_eq!(app.input_mode, InputMode::Comment);
    assert_eq!(app.comment_buffer, "second reply");
    assert!(app.session.review_comments.is_empty());
}

#[test]
fn should_restore_editor_with_body_when_reply_fails() {
    // given — an in-flight reply whose background call failed
    let mut app = make_pr_app("src/lib.rs");
    app.forge_review_threads = vec![inline_thread("T1", 11)];
    let repository = ForgeRepository::github("github.com", "agavra", "tuicr");
    app.pr_reply_state = Some(ReplyInFlightState {
        repository: repository.clone(),
        pr_number: 125,
        thread_id: "T1".to_string(),
        thread_author: Some("alice".to_string()),
        body: "my reply text".to_string(),
        started_at: Instant::now(),
    });
    let (tx, rx) = std::sync::mpsc::channel();
    app.pr_reply_rx = Some(rx);
    tx.send(PrReplyEvent::Done {
        repository,
        pr_number: 125,
        result: Err("HTTP 502".to_string()),
    })
    .unwrap();

    // when
    app.poll_pr_reply_events();

    // then — sticky error + editor restored with the typed body
    assert_eq!(app.input_mode, InputMode::Comment);
    assert_eq!(app.comment_reply_target, Some(0));
    assert_eq!(app.comment_buffer, "my reply text");
    assert!(app.pr_reply_state.is_none());
    assert!(app.pr_reply_rx.is_none());
}

#[test]
fn should_not_reopen_editor_when_user_is_composing_something_else() {
    // given — the failure lands while the user is already typing a new comment
    let mut app = make_pr_app("src/lib.rs");
    app.forge_review_threads = vec![inline_thread("T1", 11)];
    let repository = ForgeRepository::github("github.com", "agavra", "tuicr");
    app.pr_reply_state = Some(ReplyInFlightState {
        repository: repository.clone(),
        pr_number: 125,
        thread_id: "T1".to_string(),
        thread_author: None,
        body: "lost reply".to_string(),
        started_at: Instant::now(),
    });
    let (tx, rx) = std::sync::mpsc::channel();
    app.pr_reply_rx = Some(rx);
    tx.send(PrReplyEvent::Done {
        repository,
        pr_number: 125,
        result: Err("boom".to_string()),
    })
    .unwrap();
    app.enter_comment_mode(false, Some((11, LineSide::New)));
    app.comment_buffer = "unrelated draft".to_string();

    // when
    app.poll_pr_reply_events();

    // then — the in-progress draft is untouched
    assert_eq!(app.input_mode, InputMode::Comment);
    assert_eq!(app.comment_reply_target, None);
    assert_eq!(app.comment_buffer, "unrelated draft");
}
