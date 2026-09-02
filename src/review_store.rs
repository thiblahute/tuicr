use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::error::{Result, TuicrError};
use crate::model::{Comment, CommentType, LineRange, LineSide, ReviewSession};
use crate::persistence::manifest::{ManifestEntry, ManifestKind};
use crate::persistence::storage;

/// File-backed access to persisted tuicr review sessions.
#[derive(Debug, Clone, Default)]
pub struct ReviewStore {
    reviews_dir: Option<PathBuf>,
}

impl ReviewStore {
    /// Use tuicr's platform data directory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Use an explicit reviews directory. This is primarily useful for
    /// wrappers, tests, and tools that want isolated session storage.
    pub fn with_reviews_dir(reviews_dir: impl Into<PathBuf>) -> Self {
        Self {
            reviews_dir: Some(reviews_dir.into()),
        }
    }

    /// List persisted sessions for a repo selector — a checkout path or a
    /// forge coordinate like `owner/repo`. A checkout path matches its own
    /// local sessions and, via its `origin` remote, any PR sessions for the
    /// same repo; a coordinate matches local and PR sessions by `owner/repo`.
    pub fn list_sessions_for_repo(
        &self,
        selector: impl AsRef<Path>,
    ) -> Result<Vec<SessionSummary>> {
        let reviews_dir = self.reviews_dir()?;
        let entries = storage::list_sessions_for_selector_in_dir(&reviews_dir, selector.as_ref())?;
        let active_paths = storage::active_session_paths_in_dir(&reviews_dir)?;
        Ok(entries
            .into_iter()
            .map(|(slug, entry)| summary_from_entry(&reviews_dir, &active_paths, slug, entry))
            .collect())
    }

    /// List every persisted session, local and PR, newest first. Backs
    /// `tuicr review list --all` for when the caller does not know the repo.
    pub fn list_all_sessions(&self) -> Result<Vec<SessionSummary>> {
        let reviews_dir = self.reviews_dir()?;
        let entries = storage::list_all_sessions_in_dir(&reviews_dir)?;
        let active_paths = storage::active_session_paths_in_dir(&reviews_dir)?;
        Ok(entries
            .into_iter()
            .map(|(slug, entry)| summary_from_entry(&reviews_dir, &active_paths, slug, entry))
            .collect())
    }

    /// Resolve a PR session to its [`SessionRef`] from a PR slug
    /// (`gh:owner/repo/pr/<n>`). Returns `None` when no PR session is
    /// persisted for that slug.
    pub fn resolve_pr_session(&self, slug: &str) -> Result<Option<SessionRef>> {
        let reviews_dir = self.reviews_dir()?;
        Ok(storage::pr_session_path_in_dir(&reviews_dir, slug)?.map(SessionRef::from_path))
    }

    /// Load a persisted review session.
    pub fn get_review(&self, session_ref: &SessionRef) -> Result<ReviewSession> {
        storage::load_session(session_ref.path())
    }

    /// Add a local draft comment to a persisted session and save it.
    pub fn add_comment(
        &self,
        session_ref: &SessionRef,
        request: AddCommentRequest,
    ) -> Result<Comment> {
        let reviews_dir = self.reviews_dir()?;
        let (_session, comment) =
            storage::update_session_in_dir(session_ref.path(), &reviews_dir, |session| {
                add_comment_to_session(session, request)
            })?;
        Ok(comment)
    }

    /// Reply to an existing comment in a persisted session.
    pub fn reply_to_comment(
        &self,
        session_ref: &SessionRef,
        request: ReplyRequest,
    ) -> Result<Comment> {
        let reviews_dir = self.reviews_dir()?;
        let (_session, comment) =
            storage::update_session_in_dir(session_ref.path(), &reviews_dir, |session| {
                reply_to_comment_in_session(session, request)
            })?;
        Ok(comment)
    }

    /// Resolve (or reopen) the thread a comment belongs to in a persisted
    /// session. Returns the thread's root.
    pub fn set_thread_resolved(
        &self,
        session_ref: &SessionRef,
        comment_id: &str,
        resolved: bool,
    ) -> Result<Comment> {
        let reviews_dir = self.reviews_dir()?;
        let (_session, comment) =
            storage::update_session_in_dir(session_ref.path(), &reviews_dir, |session| {
                set_thread_resolved(session, comment_id, resolved)
            })?;
        Ok(comment)
    }

    /// Read-modify-write a session under the store lock, so two writers
    /// touching different parts of the same review do not overwrite each
    /// other. Whole-session `save_review` cannot promise that.
    pub fn update_session(
        &self,
        session_ref: &SessionRef,
        update: impl FnOnce(&mut ReviewSession) -> Result<()>,
    ) -> Result<ReviewSession> {
        let reviews_dir = self.reviews_dir()?;
        let (session, ()) =
            storage::update_session_in_dir(session_ref.path(), &reviews_dir, update)?;
        Ok(session)
    }

    /// Save a session through this store's storage root.
    pub fn save_review(&self, session: &ReviewSession) -> Result<SessionRef> {
        let reviews_dir = self.reviews_dir()?;
        storage::save_session_in_dir(session, &reviews_dir).map(SessionRef::from_path)
    }

    fn reviews_dir(&self) -> Result<PathBuf> {
        match &self.reviews_dir {
            Some(path) => Ok(path.clone()),
            None => storage::get_reviews_dir(),
        }
    }
}

/// Build a [`SessionSummary`] from a manifest entry, resolving its absolute
/// path and active state. Shared by the per-repo and `--all` listings.
fn summary_from_entry(
    reviews_dir: &Path,
    active_paths: &std::collections::HashSet<PathBuf>,
    slug: String,
    entry: ManifestEntry,
) -> SessionSummary {
    let path = reviews_dir.join(entry.path);
    let active = active_paths.contains(&storage::normalize_path_for_comparison(&path));
    let kind = match entry.kind {
        ManifestKind::Local => SessionKind::Local,
        ManifestKind::Pr { .. } => SessionKind::Pr,
    };
    SessionSummary {
        session_ref: SessionRef::from_path(path),
        slug,
        kind,
        updated_at: entry.updated_at,
        comment_count: entry.display.comment_count,
        reviewed_count: entry.display.reviewed_count,
        file_count: entry.display.file_count,
        anchor: entry.display.anchor,
        active,
    }
}

/// Opaque reference to a persisted review session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionRef {
    path: PathBuf,
}

impl SessionRef {
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Whether a persisted session tracks a local checkout or a forge PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Local,
    Pr,
}

impl SessionKind {
    pub fn id(self) -> &'static str {
        match self {
            SessionKind::Local => "local",
            SessionKind::Pr => "pr",
        }
    }
}

/// Lightweight metadata for a persisted session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_ref: SessionRef,
    pub slug: String,
    pub kind: SessionKind,
    pub updated_at: DateTime<Utc>,
    pub comment_count: usize,
    pub reviewed_count: usize,
    pub file_count: usize,
    pub anchor: String,
    pub active: bool,
}

/// Request to add a local draft comment to a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddCommentRequest {
    pub target: CommentTarget,
    pub content: String,
    pub comment_type: CommentType,
    /// Author to stamp on the resulting comment. Caller is responsible for
    /// picking a sensible default (`Comment::DEFAULT_AUTHOR`) when none is
    /// supplied.
    pub author: String,
    /// The commented line's text and both line numbers, when the target is a
    /// line or range. A later reload finds the line again by content when an
    /// amend moves it; without it, coordinates drift silently onto whatever
    /// took the line's place. `None` for file- and review-level comments.
    pub line_context: Option<crate::model::LineContext>,
    /// Commit SHA to stamp on the comment when it was created while the
    /// inline commit selector showed exactly one commit. `None` for
    /// review-level comments and full-range selections. Library callers
    /// (the `review add` CLI) leave this `None`.
    pub commit_id: Option<String>,
}

/// Request to reply to an existing local comment, forming a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyRequest {
    /// Id of the comment being replied to. A reply to a reply is flattened
    /// onto that reply's root, so threads stay one level deep.
    pub parent_id: String,
    pub content: String,
    /// Author to stamp on the reply. Agents pass their own name so the box
    /// renders with an author badge.
    pub author: String,
    /// Whether this reply reopens a settled thread. A reply written in the
    /// TUI is someone answering a thread they can see — reopening is the
    /// point. One landing through the CLI was composed against the thread as
    /// it stood earlier: when the reader settled it meanwhile, their resolve
    /// is the later word, and an agent's "done" confirmation quietly joins
    /// the settled record instead of undoing it.
    pub reopen: bool,
}

impl AddCommentRequest {
    /// A comment with no anchor beyond its target — what every caller outside
    /// the diff view can supply. Only the TUI knows the text of the line being
    /// commented on, or which commit the selector is showing; the CLI has no
    /// diff loaded and cannot know either, so it should not have to say so
    /// field by field.
    pub fn new(
        target: CommentTarget,
        content: String,
        comment_type: CommentType,
        author: String,
    ) -> Self {
        Self {
            target,
            content,
            comment_type,
            author,
            line_context: None,
            commit_id: None,
        }
    }
}

/// Where a new local draft comment should be attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentTarget {
    Review,
    File {
        path: PathBuf,
    },
    Line {
        path: PathBuf,
        line: u32,
        side: LineSide,
    },
    LineRange {
        path: PathBuf,
        range: LineRange,
        side: LineSide,
    },
}

/// Add a local draft comment to an in-memory session.
///
/// This is the shared primitive used by the TUI and by [`ReviewStore`].
pub fn add_comment_to_session(
    session: &mut ReviewSession,
    request: AddCommentRequest,
) -> Result<Comment> {
    let content = request.content.trim().to_string();
    if content.is_empty() {
        return Err(TuicrError::InvalidInput(
            "comment cannot be empty".to_string(),
        ));
    }

    let author = request.author;
    let commit_id = request.commit_id;
    let comment = match request.target {
        CommentTarget::Review => {
            let comment = Comment::new(content, request.comment_type, None).with_author(author);
            session.review_comments.push(comment.clone());
            comment
        }
        CommentTarget::File { path } => {
            let review = file_review_mut(session, &path)?;
            let mut comment = Comment::new(content, request.comment_type, None).with_author(author);
            if let Some(sha) = &commit_id {
                comment = comment.with_commit_id(sha.clone());
            }
            review.add_file_comment(comment.clone());
            comment
        }
        CommentTarget::Line { path, line, side } => {
            let review = file_review_mut(session, &path)?;
            let mut comment =
                Comment::new(content, request.comment_type, Some(side)).with_author(author);
            comment.line_context = request.line_context;
            if let Some(sha) = &commit_id {
                comment = comment.with_commit_id(sha.clone());
            }
            review.add_line_comment(line, comment.clone());
            comment
        }
        CommentTarget::LineRange { path, range, side } => {
            let review = file_review_mut(session, &path)?;
            let mut comment =
                Comment::new_with_range(content, request.comment_type, Some(side), range)
                    .with_author(author);
            comment.line_context = request.line_context;
            if let Some(sha) = &commit_id {
                comment = comment.with_commit_id(sha.clone());
            }
            review.add_line_comment(range.end, comment.clone());
            comment
        }
    };

    session.updated_at = Utc::now();
    Ok(comment)
}

/// Add a reply to the local comment `parent_id` names, in the same session.
///
/// The reply is stored as a sibling of its root — same vec, so it inherits the
/// root's anchor (file, line, side, review scope) for free and rides the same
/// persistence and external-merge paths. It is placed directly after the last
/// comment already in the thread so the thread stays contiguous, and carries
/// no comment type: a reply's body posts as written.
///
/// This is the shared primitive used by the TUI and by [`ReviewStore`].
pub fn reply_to_comment_in_session(
    session: &mut ReviewSession,
    request: ReplyRequest,
) -> Result<Comment> {
    let content = request.content.trim().to_string();
    if content.is_empty() {
        return Err(TuicrError::InvalidInput(
            "reply cannot be empty".to_string(),
        ));
    }

    let parent_id = request.parent_id;
    let mut buckets: Vec<&mut Vec<Comment>> = vec![&mut session.review_comments];
    for review in session.files.values_mut() {
        buckets.push(&mut review.file_comments);
        for comments in review.line_comments.values_mut() {
            buckets.push(comments);
        }
    }

    for bucket in buckets {
        let Some(parent) = bucket.iter().find(|c| c.id == parent_id) else {
            continue;
        };
        // Flatten: replying to a reply attaches to the thread's root.
        let root_id = parent
            .in_reply_to
            .clone()
            .unwrap_or_else(|| parent.id.clone());
        let root = bucket
            .iter()
            .find(|c| c.id == root_id)
            .unwrap_or(parent)
            .clone();
        let reply = Comment {
            id: uuid::Uuid::new_v4().to_string(),
            content,
            comment_type: CommentType::None,
            created_at: Utc::now(),
            line_context: root.line_context.clone(),
            side: root.side,
            line_range: root.line_range,
            author: request.author,
            lifecycle_state: Default::default(),
            remote_review_id: None,
            remote_comment_id: None,
            commit_id: root.commit_id.clone(),
            // A reply inherits its root's anchor: a thread is one conversation
            // about one place, and splitting it across scopes would split it
            // across files in the store.
            anchor: root.anchor.clone(),
            in_reply_to: Some(root_id.clone()),
            // A reply that does not reopen joins the thread in its current
            // state — a lone unresolved member of a settled thread would
            // render as an orphan box under the collapsed marker.
            resolved: if request.reopen { false } else { root.resolved },
            outdated: root.outdated,
        };
        // After the last comment already in this thread, so replies read in
        // posted order and the thread never interleaves with its neighbours.
        let insert_at = bucket
            .iter()
            .rposition(|c| c.id == root_id || c.in_reply_to.as_deref() == Some(root_id.as_str()))
            .map(|idx| idx + 1)
            .unwrap_or(bucket.len());
        // A reply from the TUI reopens a settled thread: there is something
        // new to read, written by someone looking at it.
        if request.reopen {
            for comment in bucket.iter_mut() {
                if comment.id == root_id || comment.in_reply_to.as_deref() == Some(root_id.as_str())
                {
                    comment.resolved = false;
                }
            }
        }
        bucket.insert(insert_at, reply.clone());
        session.updated_at = Utc::now();
        return Ok(reply);
    }

    Err(TuicrError::InvalidInput(format!(
        "session has no comment with id {parent_id}"
    )))
}

/// Mark the thread `comment_id` belongs to as resolved (or reopen it), and
/// return its root.
///
/// Naming any member of the thread works — the root, or a reply — because a
/// thread is settled as a unit. The flag is written to every member so
/// renderers can mute a box from the comment in hand.
/// Record that an agent has picked this review up, or — with `done` — that it
/// has stopped working on it.
///
/// Stopping also announces itself: an agent that answered a question without
/// touching the code has a result to report, and a spinner that simply vanishes
/// reads as an agent that died.
pub fn set_agent_working(
    session: &mut ReviewSession,
    activity: crate::model::review::AgentActivity,
    done: bool,
) {
    session.updated_at = activity.at;
    if done {
        session.agent_working = None;
        session.agent_update = Some(crate::model::review::AgentUpdate {
            at: activity.at,
            message: activity.message,
        });
        return;
    }
    session.agent_working = Some(activity);
}

pub fn set_thread_resolved(
    session: &mut ReviewSession,
    comment_id: &str,
    resolved: bool,
) -> Result<Comment> {
    let mut buckets: Vec<&mut Vec<Comment>> = vec![&mut session.review_comments];
    for review in session.files.values_mut() {
        buckets.push(&mut review.file_comments);
        for comments in review.line_comments.values_mut() {
            buckets.push(comments);
        }
    }

    for bucket in buckets {
        let Some(target) = bucket.iter().find(|c| c.id == comment_id) else {
            continue;
        };
        let root_id = target
            .in_reply_to
            .clone()
            .unwrap_or_else(|| target.id.clone());
        for comment in bucket.iter_mut() {
            if comment.id == root_id || comment.in_reply_to.as_deref() == Some(root_id.as_str()) {
                comment.resolved = resolved;
            }
        }
        session.updated_at = Utc::now();
        let root = bucket
            .iter()
            .find(|c| c.id == root_id)
            .cloned()
            .ok_or_else(|| TuicrError::InvalidInput(format!("thread root {root_id} is missing")))?;
        return Ok(root);
    }

    Err(TuicrError::InvalidInput(format!(
        "session has no comment with id {comment_id}"
    )))
}

fn file_review_mut<'a>(
    session: &'a mut ReviewSession,
    path: &Path,
) -> Result<&'a mut crate::model::review::FileReview> {
    session.get_file_mut(&path.to_path_buf()).ok_or_else(|| {
        TuicrError::InvalidInput(format!("session does not contain file {}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FileStatus, SessionDiffSource};

    fn test_session(repo_path: PathBuf) -> ReviewSession {
        let mut session = ReviewSession::new(
            repo_path,
            "abc1234".to_string(),
            Some("main".to_string()),
            SessionDiffSource::WorkingTree,
        );
        session.add_file(PathBuf::from("src/main.rs"), FileStatus::Modified, 0);
        session
    }

    #[test]
    fn should_add_review_level_comment_to_session() {
        let mut session = test_session(PathBuf::from("/repo"));

        let comment = add_comment_to_session(
            &mut session,
            AddCommentRequest::new(
                CommentTarget::Review,
                "looks good".to_string(),
                CommentType::from_id("praise"),
                crate::model::comment::DEFAULT_AUTHOR.to_string(),
            ),
        )
        .unwrap();

        assert_eq!(session.review_comments, vec![comment]);
    }

    #[test]
    fn should_add_file_comment_to_session() {
        let mut session = test_session(PathBuf::from("/repo"));

        let comment = add_comment_to_session(
            &mut session,
            AddCommentRequest {
                target: CommentTarget::File {
                    path: PathBuf::from("src/main.rs"),
                },
                content: "file note".to_string(),
                comment_type: CommentType::from_id("note"),
                author: crate::model::comment::DEFAULT_AUTHOR.to_string(),
                line_context: None,
                commit_id: None,
            },
        )
        .unwrap();

        let review = session.files.get(&PathBuf::from("src/main.rs")).unwrap();
        assert_eq!(review.file_comments, vec![comment]);
    }

    #[test]
    fn should_add_line_range_comment_by_range_end() {
        let mut session = test_session(PathBuf::from("/repo"));
        let range = LineRange::new(10, 12);

        let comment = add_comment_to_session(
            &mut session,
            AddCommentRequest {
                target: CommentTarget::LineRange {
                    path: PathBuf::from("src/main.rs"),
                    range,
                    side: LineSide::New,
                },
                content: "range note".to_string(),
                comment_type: CommentType::from_id("suggestion"),
                author: crate::model::comment::DEFAULT_AUTHOR.to_string(),
                line_context: None,
                commit_id: None,
            },
        )
        .unwrap();

        let review = session.files.get(&PathBuf::from("src/main.rs")).unwrap();
        assert_eq!(review.line_comments.get(&12), Some(&vec![comment]));
    }

    #[test]
    fn should_reject_unknown_file() {
        let mut session = test_session(PathBuf::from("/repo"));

        let err = add_comment_to_session(
            &mut session,
            AddCommentRequest {
                target: CommentTarget::File {
                    path: PathBuf::from("missing.rs"),
                },
                content: "note".to_string(),
                comment_type: CommentType::from_id("note"),
                author: crate::model::comment::DEFAULT_AUTHOR.to_string(),
                line_context: None,
                commit_id: None,
            },
        )
        .unwrap_err();

        assert!(matches!(err, TuicrError::InvalidInput(_)));
    }

    #[test]
    fn should_list_and_update_sessions_through_store() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let reviews_dir = temp.path().join("reviews");
        let store = ReviewStore::with_reviews_dir(reviews_dir.clone());
        let session = test_session(repo.clone());
        let session_ref = store.save_review(&session).unwrap();

        let listed = store.list_sessions_for_repo(&repo).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_ref, session_ref);
        assert_eq!(listed[0].file_count, 1);
        assert_eq!(listed[0].comment_count, 0);
        assert!(!listed[0].active);

        crate::persistence::storage::mark_session_active_in_dir(
            &session,
            session_ref.path(),
            &reviews_dir,
        )
        .unwrap();
        let listed = store.list_sessions_for_repo(&repo).unwrap();
        assert!(listed[0].active);

        store
            .add_comment(
                &session_ref,
                AddCommentRequest {
                    target: CommentTarget::Line {
                        path: PathBuf::from("src/main.rs"),
                        line: 7,
                        side: LineSide::New,
                    },
                    content: "line note".to_string(),
                    comment_type: CommentType::from_id("note"),
                    author: crate::model::comment::DEFAULT_AUTHOR.to_string(),
                    line_context: None,
                    commit_id: None,
                },
            )
            .unwrap();

        let loaded = store.get_review(&session_ref).unwrap();
        let review = loaded.files.get(&PathBuf::from("src/main.rs")).unwrap();
        assert_eq!(review.line_comments.get(&7).unwrap().len(), 1);

        let listed = store.list_sessions_for_repo(&repo).unwrap();
        assert_eq!(listed[0].comment_count, 1);
    }

    fn line_comment(session: &mut ReviewSession, content: &str, author: &str) -> Comment {
        add_comment_to_session(
            session,
            AddCommentRequest {
                target: CommentTarget::Line {
                    path: PathBuf::from("src/main.rs"),
                    line: 42,
                    side: LineSide::New,
                },
                content: content.to_string(),
                comment_type: CommentType::from_id("issue"),
                author: author.to_string(),
                line_context: None,
                commit_id: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn should_store_a_reply_beside_its_root_comment() {
        let mut session = test_session(PathBuf::from("/repo"));
        let root = line_comment(&mut session, "handle the empty case", "user");

        let reply = reply_to_comment_in_session(
            &mut session,
            ReplyRequest {
                parent_id: root.id.clone(),
                content: "fixed in abc1234".to_string(),
                author: "Claude".to_string(),
                reopen: true,
            },
        )
        .unwrap();

        assert_eq!(reply.in_reply_to.as_deref(), Some(root.id.as_str()));
        assert_eq!(reply.author, "Claude");
        // A reply posts verbatim: no type prefix or badge.
        assert!(reply.comment_type.is_none());
        // The anchor is inherited, so the reply renders in the root's thread.
        assert_eq!(reply.side, root.side);
        assert_eq!(reply.line_range, root.line_range);

        let review = session.files.get(&PathBuf::from("src/main.rs")).unwrap();
        let thread = review.line_comments.get(&42).unwrap();
        assert_eq!(thread.len(), 2);
        assert_eq!(thread[0].id, root.id);
        assert_eq!(thread[1].id, reply.id);
    }

    #[test]
    fn should_flatten_a_reply_to_a_reply_onto_the_root() {
        let mut session = test_session(PathBuf::from("/repo"));
        let root = line_comment(&mut session, "handle the empty case", "user");
        let first = reply_to_comment_in_session(
            &mut session,
            ReplyRequest {
                parent_id: root.id.clone(),
                content: "fixed in abc1234".to_string(),
                author: "Claude".to_string(),
                reopen: true,
            },
        )
        .unwrap();

        let second = reply_to_comment_in_session(
            &mut session,
            ReplyRequest {
                parent_id: first.id.clone(),
                content: "thanks".to_string(),
                author: "user".to_string(),
                reopen: true,
            },
        )
        .unwrap();

        assert_eq!(second.in_reply_to.as_deref(), Some(root.id.as_str()));
        let review = session.files.get(&PathBuf::from("src/main.rs")).unwrap();
        let thread = review.line_comments.get(&42).unwrap();
        assert_eq!(
            thread.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec![root.id.as_str(), first.id.as_str(), second.id.as_str()]
        );
    }

    #[test]
    fn should_keep_a_thread_contiguous_when_a_later_comment_shares_the_line() {
        let mut session = test_session(PathBuf::from("/repo"));
        let root = line_comment(&mut session, "first", "user");
        let other = line_comment(&mut session, "unrelated", "user");

        let reply = reply_to_comment_in_session(
            &mut session,
            ReplyRequest {
                parent_id: root.id.clone(),
                content: "done".to_string(),
                author: "Claude".to_string(),
                reopen: true,
            },
        )
        .unwrap();

        let review = session.files.get(&PathBuf::from("src/main.rs")).unwrap();
        let thread = review.line_comments.get(&42).unwrap();
        assert_eq!(
            thread.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec![root.id.as_str(), reply.id.as_str(), other.id.as_str()]
        );
    }

    #[test]
    fn should_reply_to_a_review_level_comment() {
        let mut session = test_session(PathBuf::from("/repo"));
        let root = add_comment_to_session(
            &mut session,
            AddCommentRequest::new(
                CommentTarget::Review,
                "overall looks good".to_string(),
                CommentType::None,
                "user".to_string(),
            ),
        )
        .unwrap();

        let reply = reply_to_comment_in_session(
            &mut session,
            ReplyRequest {
                parent_id: root.id.clone(),
                content: "noted".to_string(),
                author: "Claude".to_string(),
                reopen: true,
            },
        )
        .unwrap();

        assert_eq!(session.review_comments.len(), 2);
        assert_eq!(session.review_comments[1].id, reply.id);
    }

    #[test]
    fn should_reject_a_reply_to_an_unknown_comment() {
        let mut session = test_session(PathBuf::from("/repo"));

        let err = reply_to_comment_in_session(
            &mut session,
            ReplyRequest {
                parent_id: "does-not-exist".to_string(),
                content: "hello".to_string(),
                author: "Claude".to_string(),
                reopen: true,
            },
        )
        .unwrap_err();

        assert!(matches!(err, TuicrError::InvalidInput(_)), "{err:?}");
    }

    #[test]
    fn should_reject_an_empty_reply() {
        let mut session = test_session(PathBuf::from("/repo"));
        let root = line_comment(&mut session, "handle the empty case", "user");

        let err = reply_to_comment_in_session(
            &mut session,
            ReplyRequest {
                parent_id: root.id,
                content: "   ".to_string(),
                author: "Claude".to_string(),
                reopen: true,
            },
        )
        .unwrap_err();

        assert!(matches!(err, TuicrError::InvalidInput(_)), "{err:?}");
    }

    #[test]
    fn should_resolve_a_thread_from_any_of_its_comments() {
        let mut session = test_session(PathBuf::from("/repo"));
        let root = line_comment(&mut session, "handle the empty case", "user");
        let reply = reply_to_comment_in_session(
            &mut session,
            ReplyRequest {
                parent_id: root.id.clone(),
                content: "fixed in abc1234".to_string(),
                author: "Claude".to_string(),
                reopen: true,
            },
        )
        .unwrap();

        // Naming the reply settles the whole thread, not just that message.
        let returned = set_thread_resolved(&mut session, &reply.id, true).unwrap();
        assert_eq!(returned.id, root.id);

        let thread = session.files[&PathBuf::from("src/main.rs")].line_comments[&42].clone();
        assert!(thread.iter().all(|c| c.resolved), "whole thread resolves");

        set_thread_resolved(&mut session, &root.id, false).unwrap();
        let thread = session.files[&PathBuf::from("src/main.rs")].line_comments[&42].clone();
        assert!(thread.iter().all(|c| !c.resolved), "whole thread reopens");
    }

    #[test]
    fn should_leave_a_neighbouring_thread_alone_when_resolving() {
        let mut session = test_session(PathBuf::from("/repo"));
        let first = line_comment(&mut session, "first", "user");
        let second = line_comment(&mut session, "second", "user");

        set_thread_resolved(&mut session, &first.id, true).unwrap();

        let thread = session.files[&PathBuf::from("src/main.rs")].line_comments[&42].clone();
        let by_id = |id: &str| thread.iter().find(|c| c.id == id).unwrap().resolved;
        assert!(by_id(&first.id));
        assert!(
            !by_id(&second.id),
            "the other comment on this line is untouched"
        );
    }

    #[test]
    fn should_reopen_a_resolved_thread_when_a_reply_arrives() {
        let mut session = test_session(PathBuf::from("/repo"));
        let root = line_comment(&mut session, "handle the empty case", "user");
        set_thread_resolved(&mut session, &root.id, true).unwrap();

        reply_to_comment_in_session(
            &mut session,
            ReplyRequest {
                parent_id: root.id.clone(),
                content: "actually, one more thing".to_string(),
                author: "user".to_string(),
                reopen: true,
            },
        )
        .unwrap();

        // A new message means the thread is not settled after all.
        let thread = session.files[&PathBuf::from("src/main.rs")].line_comments[&42].clone();
        assert!(
            thread.iter().all(|c| !c.resolved),
            "reply reopens the thread"
        );
    }

    #[test]
    fn should_reject_resolving_an_unknown_comment() {
        let mut session = test_session(PathBuf::from("/repo"));
        let err = set_thread_resolved(&mut session, "does-not-exist", true).unwrap_err();
        assert!(matches!(err, TuicrError::InvalidInput(_)), "{err:?}");
    }
}
