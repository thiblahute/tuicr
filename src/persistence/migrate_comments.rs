//! Copying comments out of session files and into the comment store.
//!
//! Additive on purpose: a session keeps its comments, and the store gets a
//! copy. Rolling back is deleting a directory, and a review that is open while
//! this runs is not disturbed by it.
//!
//! Nothing here opens a repository. The slug the manifest is keyed by already
//! carries the `owner/repo` coordinate, so a repo that has since moved or been
//! deleted migrates exactly like one that is still there.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::error::Result;
use crate::model::{Comment, CommentAnchor, CommentScope, LineSide, ReviewSession};
use crate::persistence::comment_store::{CommentStore, checkout_key};
use crate::persistence::manifest::load_manifest;
use crate::persistence::storage::load_session;
use crate::slug::Slug;

/// Where the migration gets a commit's message from.
///
/// The session files hold no commit message, so it has to come from the repo.
/// It is asked for through this rather than read here directly: the summary is
/// what recovers a thread after an amend, and a store that cannot answer must
/// degrade to "no summary", not to a git dependency in the persistence layer.
pub trait MessageSource {
    fn message(&self, repo: &Path, sha: &str) -> Option<String>;
}

/// For tests and for callers that would rather not touch a repository.
pub struct NoMessages;

impl MessageSource for NoMessages {
    fn message(&self, _repo: &Path, _sha: &str) -> Option<String> {
        None
    }
}

/// Best effort through `git`, asked once per commit.
///
/// A repo that has moved, a commit that was garbage collected, or a backend
/// that is not git all yield `None` — the comment still migrates, it just
/// cannot be found again by summary if its commit is later rewritten.
#[derive(Default)]
pub struct GitMessages {
    seen: RefCell<HashMap<(PathBuf, String), Option<String>>>,
}

impl MessageSource for GitMessages {
    fn message(&self, repo: &Path, sha: &str) -> Option<String> {
        let key = (repo.to_path_buf(), sha.to_string());
        if let Some(hit) = self.seen.borrow().get(&key) {
            return hit.clone();
        }
        let found = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["log", "-1", "--format=%B", sha])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|text| text.trim_end().to_string())
            .filter(|text| !text.is_empty());
        self.seen.borrow_mut().insert(key, found.clone());
        found
    }
}

#[derive(Debug, Default)]
pub struct MigrationReport {
    pub sessions: usize,
    pub comments: usize,
    /// Comments that named no commit and took their session's range head.
    pub stamped_from_range_head: usize,
    /// Comments from a review of uncommitted work.
    pub working_tree: usize,
    /// Commit files that got a message, and so can be found again after an
    /// amend rewrites their commit.
    pub with_message: usize,
    pub skipped: Vec<Skipped>,
}

#[derive(Debug)]
pub struct Skipped {
    pub slug: String,
    pub reason: String,
}

/// A comment the store does not hold as the session holds it.
#[derive(Debug)]
pub struct Divergence {
    pub slug: String,
    pub comment_id: String,
    pub reason: String,
}

/// Copy the whole reviews directory aside before anything is moved, once.
///
/// The migration is additive, but the session copies are refreshed every time
/// a review saves, so "delete the store to undo" recovers comments only as of
/// the last save. A snapshot taken before the first migration is what makes
/// undoing exact.
pub fn ensure_backup(reviews_dir: &Path) -> Result<Option<PathBuf>> {
    let Some(parent) = reviews_dir.parent() else {
        return Ok(None);
    };
    let stem = reviews_dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "reviews".to_string());
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&format!("{stem}.bak-")) {
                return Ok(None);
            }
        }
    }
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let backup = parent.join(format!("{stem}.bak-{stamp}"));
    copy_tree(reviews_dir, &backup)?;
    Ok(Some(backup))
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Check that every comment a session file holds is in the store, comparing
/// ids and the fields a reader would notice.
///
/// Deliberately independent of the migration: it derives nothing, shares no
/// scoping logic, and simply walks both sides. `verify_migration` asks whether
/// the store holds what the migration meant to write, which cannot catch the
/// migration meaning the wrong thing — this can.
pub fn comments_not_in_store(
    reviews_dir: &Path,
    wanted: impl Fn(&str) -> bool,
) -> Result<Vec<String>> {
    let mut in_sessions: BTreeMap<String, (String, String, bool)> = BTreeMap::new();
    let sessions = reviews_dir.join("sessions");
    if let Ok(entries) = std::fs::read_dir(&sessions) {
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(session) = load_session(&entry.path()) else {
                continue;
            };
            // Only the repositories being asked about. A machine-wide check
            // fails on the first repository that has not moved yet, which for
            // a per-repository migration means the switch is never thrown.
            match repo_key_for_session(reviews_dir, &entry.path(), &session) {
                Some(key) if wanted(&key) => {}
                _ => continue,
            }
            let mut note = |comment: &Comment| {
                in_sessions.insert(
                    comment.id.clone(),
                    (
                        comment.content.clone(),
                        comment.author.clone(),
                        comment.resolved,
                    ),
                );
            };
            for comment in &session.review_comments {
                note(comment);
            }
            for review in session.files.values() {
                for comment in &review.file_comments {
                    note(comment);
                }
                for comments in review.line_comments.values() {
                    for comment in comments {
                        note(comment);
                    }
                }
            }
        }
    }

    let mut in_store: BTreeMap<String, (String, String, bool)> = BTreeMap::new();
    collect_stored(&reviews_dir.join("comments"), &mut in_store);

    let mut problems = Vec::new();
    for (id, expected) in &in_sessions {
        match in_store.get(id) {
            None => problems.push(format!("{id}: missing from the store")),
            Some(actual) if actual != expected => {
                problems.push(format!("{id}: changed on the way into the store"))
            }
            Some(_) => {}
        }
    }
    Ok(problems)
}

fn collect_stored(dir: &Path, out: &mut BTreeMap<String, (String, String, bool)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_stored(&path, out);
        } else if path.file_name().and_then(|n| n.to_str()) != Some("index.json")
            && path.extension().and_then(|e| e.to_str()) == Some("json")
            && let Ok(bytes) = std::fs::read(&path)
            && let Ok(file) =
                serde_json::from_slice::<crate::persistence::comment_store::ScopeFile>(&bytes)
        {
            for comment in file.comments {
                out.insert(
                    comment.id.clone(),
                    (comment.content, comment.author, comment.resolved),
                );
            }
        }
    }
}

/// Copy every session's comments into the store beneath `reviews_dir`.
///
/// With `apply` false nothing is written: the report says what would happen.
pub fn migrate_comments(
    reviews_dir: &Path,
    apply: bool,
    messages: &dyn MessageSource,
) -> Result<MigrationReport> {
    migrate_repos(reviews_dir, apply, messages, |_| true)
}

/// Migrate only the repositories `wanted` accepts, so a review can move its
/// own and leave every other repository on this machine alone.
pub fn migrate_repos(
    reviews_dir: &Path,
    apply: bool,
    messages: &dyn MessageSource,
    wanted: impl Fn(&str) -> bool,
) -> Result<MigrationReport> {
    let mut report = MigrationReport::default();

    for (path, session) in sessions_on_disk(reviews_dir, &mut report) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let repo_key = match repo_key_for_session(reviews_dir, &path, &session) {
            Some(key) => key,
            None => {
                report.skipped.push(Skipped {
                    slug: name,
                    reason: "no repo coordinate".to_string(),
                });
                continue;
            }
        };
        if !wanted(&repo_key) {
            continue;
        }
        report.sessions += 1;

        let checkout = session.repo_path.clone();
        let anchored = anchored_comments(&session, &checkout);

        let mut by_scope: BTreeMap<String, (CommentScope, Vec<Comment>)> = BTreeMap::new();
        for (scope, comment, from_range_head) in anchored {
            if from_range_head {
                report.stamped_from_range_head += 1;
            }
            if matches!(scope, CommentScope::WorkingTree { .. }) {
                report.working_tree += 1;
            }
            report.comments += 1;
            by_scope
                .entry(scope.file_name())
                .or_insert_with(|| (scope, Vec::new()))
                .1
                .push(comment);
        }

        let store = CommentStore::new(reviews_dir, &repo_key);
        for (_, (scope, comments)) in by_scope {
            // The message is what finds these comments again once their commit
            // is rewritten, so it is fetched now: after the rewrite the sha in
            // hand no longer resolves to anything.
            let message = match scope.sha() {
                Some(sha) => messages.message(&session.repo_path, sha),
                None => None,
            };
            if message.is_some() {
                report.with_message += 1;
            }
            if apply {
                store.add_many(&scope, message.as_deref(), comments)?;
            }
        }
    }

    Ok(report)
}

/// Check the store holds every comment the sessions hold, unchanged.
///
/// The store may hold *more* for a given session — that is the point of the
/// change, since a review now sees comments written from other ranges — so
/// this only asks that nothing is missing and nothing was altered on the way.
pub fn verify_migration(reviews_dir: &Path) -> Result<Vec<Divergence>> {
    let mut divergences = Vec::new();
    let mut ignored = MigrationReport::default();

    for (path, session) in sessions_on_disk(reviews_dir, &mut ignored) {
        let slug = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some(repo_key) = repo_key_for_session(reviews_dir, &path, &session) else {
            divergences.push(Divergence {
                slug,
                comment_id: String::new(),
                reason: "no repo coordinate, so nothing was migrated".to_string(),
            });
            continue;
        };
        let checkout = session.repo_path.clone();

        let store = CommentStore::new(reviews_dir, &repo_key);
        let anchored = anchored_comments(&session, &checkout);
        let scopes: Vec<CommentScope> = {
            let mut seen: Vec<CommentScope> = Vec::new();
            for (scope, _, _) in &anchored {
                if !seen.contains(scope) {
                    seen.push(scope.clone());
                }
            }
            seen
        };
        let stored = store.comments_for(&scopes)?;

        for (_, expected, _) in anchored {
            match stored.iter().find(|c| c.id == expected.id) {
                None => divergences.push(Divergence {
                    slug: slug.clone(),
                    comment_id: expected.id.clone(),
                    reason: "missing from the store".to_string(),
                }),
                Some(actual) => {
                    if let Some(reason) = differs(&expected, actual) {
                        divergences.push(Divergence {
                            slug: slug.clone(),
                            comment_id: expected.id.clone(),
                            reason,
                        });
                    }
                }
            }
        }
    }

    Ok(divergences)
}

/// What a reader would notice if the migration changed it.
fn differs(expected: &Comment, actual: &Comment) -> Option<String> {
    if expected.content != actual.content {
        return Some("content changed".to_string());
    }
    if expected.author != actual.author {
        return Some("author changed".to_string());
    }
    if expected.in_reply_to != actual.in_reply_to {
        return Some("thread parent changed".to_string());
    }
    if expected.resolved != actual.resolved {
        return Some("resolved changed".to_string());
    }
    if expected.outdated != actual.outdated {
        return Some("outdated changed".to_string());
    }
    if expected.comment_type != actual.comment_type {
        return Some("type changed".to_string());
    }
    None
}

/// Every comment in a session, with the scope it belongs to and whether that
/// scope had to be taken from the session's range head.
fn anchored_comments(
    session: &ReviewSession,
    checkout: &Path,
) -> Vec<(CommentScope, Comment, bool)> {
    // A pull-request review never fills `commit_range`, but it knows its head
    // sha — and a PR review asks the store about commits, never about a
    // checkout. Falling through to the working tree files every PR comment
    // where no PR reader will ever look for it.
    let range_head = session
        .commit_range
        .as_ref()
        .and_then(|range| range.last())
        .cloned()
        .or_else(|| {
            session
                .pr_session_key
                .as_ref()
                .map(|key| key.head_sha.clone())
        });
    let mut out = Vec::new();

    let mut push = |comment: &Comment, path: Option<PathBuf>, line: Option<u32>| {
        let (scope, from_range_head) = match (&comment.commit_id, &range_head) {
            (Some(sha), _) => (CommentScope::commit(sha.clone()), false),
            (None, Some(head)) => (CommentScope::commit(head.clone()), true),
            (None, None) => (CommentScope::working_tree(checkout_key(checkout)), false),
        };
        let mut anchored = comment.clone();
        if anchored.anchor.is_none() {
            anchored.anchor = Some(CommentAnchor {
                scope: scope.clone(),
                path,
                line,
                side: comment.side.unwrap_or(LineSide::New),
            });
        }
        out.push((scope, anchored, from_range_head));
    };

    for comment in &session.review_comments {
        push(comment, None, None);
    }
    let mut paths: Vec<&PathBuf> = session.files.keys().collect();
    paths.sort();
    for path in paths {
        let review = &session.files[path];
        for comment in &review.file_comments {
            push(comment, Some(path.clone()), None);
        }
        let mut lines: Vec<&u32> = review.line_comments.keys().collect();
        lines.sort();
        for line in lines {
            for comment in &review.line_comments[line] {
                push(comment, Some(path.clone()), Some(*line));
            }
        }
    }

    out
}

/// Every session file in the store, whether or not the manifest knows about
/// it. The manifest is a lookup index and can fall behind — a session it has
/// forgotten is precisely the one that would be lost without a sound.
fn sessions_on_disk(
    reviews_dir: &Path,
    report: &mut MigrationReport,
) -> Vec<(PathBuf, ReviewSession)> {
    let dir = reviews_dir.join("sessions");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match load_session(&path) {
            Ok(session) => out.push((path, session)),
            Err(err) => report.skipped.push(Skipped {
                slug: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                reason: format!("{err}"),
            }),
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The store directory this session's comments belong in.
///
/// The manifest's slug answers it without opening anything, so it is asked
/// first. A session the manifest has forgotten falls back to what the session
/// itself knows: the forge coordinate for a PR review, the repo for a local
/// one.
fn repo_key_for_session(
    reviews_dir: &Path,
    path: &Path,
    session: &ReviewSession,
) -> Option<String> {
    if let Ok(manifest) = load_manifest(reviews_dir) {
        let relative = path.strip_prefix(reviews_dir).unwrap_or(path);
        for (slug, entry) in manifest.iter() {
            if entry.path == relative
                && let Some(key) = repo_key_for(slug)
            {
                return Some(key);
            }
        }
    }
    if let Some(pr) = session.pr_session_key.as_ref() {
        return Some(format!("{}/{}", pr.repository.owner, pr.repository.name));
    }
    let (owner, repo) = crate::slug::resolve_owner_repo(&session.repo_path).ok()?;
    Some(match owner {
        Some(owner) => format!("{owner}/{repo}"),
        None => repo,
    })
}

/// The store directory a slug's comments belong in: its `owner/repo`
/// coordinate, which the slug already carries, so no repository is opened.
fn repo_key_for(slug: &str) -> Option<String> {
    match Slug::from_str(slug).ok()? {
        Slug::Local(local) => Some(match local.owner {
            Some(owner) => format!("{owner}/{}", local.repo),
            None => local.repo,
        }),
        Slug::Pr(pr) => Some(format!("{}/{}", pr.owner, pr.repo)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::review::SessionDiffSource;
    use crate::model::{CommentType, FileStatus, LineSide, ReviewSession};
    use crate::persistence::storage::save_session_in_dir;
    use tempfile::tempdir;

    fn session_with(commits: Option<Vec<String>>) -> ReviewSession {
        let mut session = ReviewSession::new(
            PathBuf::from("/repo/checkout"),
            "headsha".to_string(),
            Some("main".to_string()),
            if commits.is_some() {
                SessionDiffSource::CommitRange
            } else {
                SessionDiffSource::WorkingTree
            },
        );
        session.commit_range = commits;
        session.add_file(PathBuf::from("src/main.rs"), FileStatus::Modified, 0);
        session
    }

    fn line_comment(session: &mut ReviewSession, body: &str, commit: Option<&str>) -> String {
        let mut comment = Comment::new(
            body.to_string(),
            CommentType::from_id("issue"),
            Some(LineSide::New),
        );
        comment.commit_id = commit.map(str::to_string);
        let id = comment.id.clone();
        session
            .get_file_mut(&PathBuf::from("src/main.rs"))
            .unwrap()
            .add_line_comment(42, comment);
        id
    }

    #[test]
    fn should_file_a_comment_under_the_commit_it_names() {
        let dir = tempdir().unwrap();
        let mut session = session_with(Some(vec!["aaaa".into(), "bbbb".into()]));
        let id = line_comment(&mut session, "why this?", Some("aaaa"));
        save_session_in_dir(&session, dir.path()).unwrap();

        let report = migrate_comments(dir.path(), true, &NoMessages).unwrap();

        assert_eq!(report.comments, 1);
        assert_eq!(report.stamped_from_range_head, 0);
        let store = CommentStore::new(dir.path(), "checkout");
        let held = store.comments_for(&[CommentScope::commit("aaaa")]).unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].id, id);
        assert_eq!(
            held[0].anchor.as_ref().unwrap().line,
            Some(42),
            "and it remembers where it was written"
        );
    }

    #[test]
    fn should_give_a_range_comment_the_head_of_its_range() {
        // A comment on a cumulative diff names no commit. The head of the
        // range is what the reader was looking at.
        let dir = tempdir().unwrap();
        let mut session = session_with(Some(vec!["aaaa".into(), "bbbb".into()]));
        line_comment(&mut session, "about all of it", None);
        save_session_in_dir(&session, dir.path()).unwrap();

        let report = migrate_comments(dir.path(), true, &NoMessages).unwrap();

        assert_eq!(report.stamped_from_range_head, 1);
        let store = CommentStore::new(dir.path(), "checkout");
        assert_eq!(
            store
                .comments_for(&[CommentScope::commit("bbbb")])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn should_keep_uncommitted_comments_with_their_checkout() {
        let dir = tempdir().unwrap();
        let mut session = session_with(None);
        line_comment(&mut session, "not committed yet", None);
        save_session_in_dir(&session, dir.path()).unwrap();

        let report = migrate_comments(dir.path(), true, &NoMessages).unwrap();

        assert_eq!(report.working_tree, 1);
        assert!(verify_migration(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn should_leave_the_sessions_alone() {
        // Additive: rolling back is deleting a directory.
        let dir = tempdir().unwrap();
        let mut session = session_with(Some(vec!["aaaa".into()]));
        line_comment(&mut session, "still here?", Some("aaaa"));
        let path = save_session_in_dir(&session, dir.path()).unwrap();
        let before = std::fs::read(&path).unwrap();

        migrate_comments(dir.path(), true, &NoMessages).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), before, "session untouched");
    }

    #[test]
    fn should_not_double_a_session_migrated_twice() {
        let dir = tempdir().unwrap();
        let mut session = session_with(Some(vec!["aaaa".into()]));
        line_comment(&mut session, "once", Some("aaaa"));
        save_session_in_dir(&session, dir.path()).unwrap();

        migrate_comments(dir.path(), true, &NoMessages).unwrap();
        migrate_comments(dir.path(), true, &NoMessages).unwrap();

        let store = CommentStore::new(dir.path(), "checkout");
        assert_eq!(
            store
                .comments_for(&[CommentScope::commit("aaaa")])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn should_write_nothing_without_apply() {
        let dir = tempdir().unwrap();
        let mut session = session_with(Some(vec!["aaaa".into()]));
        line_comment(&mut session, "dry", Some("aaaa"));
        save_session_in_dir(&session, dir.path()).unwrap();

        let report = migrate_comments(dir.path(), false, &NoMessages).unwrap();

        assert_eq!(report.comments, 1, "counted");
        assert!(!dir.path().join("comments").exists(), "and nothing written");
    }

    /// Runs against a *copy* of a real reviews directory when
    /// `TUICR_MIGRATION_FIXTURE` points at one. Never against the live store,
    /// and the fixture is never committed: those sessions hold private review
    /// text.
    #[test]
    fn should_migrate_a_real_store_without_losing_a_comment() {
        let Ok(fixture) = std::env::var("TUICR_MIGRATION_FIXTURE") else {
            return;
        };
        let dir = PathBuf::from(fixture);
        let report = migrate_comments(&dir, true, &GitMessages::default()).unwrap();
        let divergences = verify_migration(&dir).unwrap();

        println!(
            "sessions={} comments={} range-head={} working-tree={} with-message={} skipped={}",
            report.sessions,
            report.comments,
            report.stamped_from_range_head,
            report.working_tree,
            report.with_message,
            report.skipped.len()
        );
        for skipped in &report.skipped {
            println!("  skipped {} — {}", skipped.slug, skipped.reason);
        }
        for divergence in &divergences {
            println!(
                "  DIVERGES {} {} — {}",
                divergence.slug, divergence.comment_id, divergence.reason
            );
        }
        assert!(divergences.is_empty(), "{} diverged", divergences.len());
    }
}

#[cfg(test)]
mod safety_tests {
    use super::*;
    use crate::model::review::SessionDiffSource;
    use crate::model::{CommentType, FileStatus, LineSide, ReviewSession};
    use crate::persistence::storage::save_session_in_dir;
    use tempfile::tempdir;

    fn a_session(dir: &Path) -> String {
        let mut session = ReviewSession::new(
            PathBuf::from("/repo/checkout"),
            "headsha".to_string(),
            Some("main".to_string()),
            SessionDiffSource::CommitRange,
        );
        session.commit_range = Some(vec!["aaaa".to_string()]);
        session.add_file(PathBuf::from("src/main.rs"), FileStatus::Modified, 0);
        let mut comment = Comment::new(
            "look here".to_string(),
            CommentType::from_id("issue"),
            Some(LineSide::New),
        );
        comment.commit_id = Some("aaaa".to_string());
        let id = comment.id.clone();
        session
            .get_file_mut(&PathBuf::from("src/main.rs"))
            .unwrap()
            .add_line_comment(42, comment);
        save_session_in_dir(&session, dir).unwrap();
        id
    }

    #[test]
    fn should_snapshot_the_reviews_directory_once() {
        let temp = tempdir().unwrap();
        let dir = temp.path().join("reviews");
        a_session(&dir);

        let first = ensure_backup(&dir).unwrap().expect("a snapshot is taken");
        assert!(first.join("sessions").is_dir(), "and it holds the sessions");
        assert!(
            ensure_backup(&dir).unwrap().is_none(),
            "but only the first time, so a later migration cannot overwrite it"
        );
    }

    #[test]
    fn should_report_a_comment_that_did_not_reach_the_store() {
        let temp = tempdir().unwrap();
        let dir = temp.path().join("reviews");
        let id = a_session(&dir);

        assert_eq!(
            comments_not_in_store(&dir, |_| true).unwrap(),
            vec![format!("{id}: missing from the store")],
            "before migrating, the session's comment is not in the store"
        );

        migrate_comments(&dir, true, &NoMessages).unwrap();

        assert!(
            comments_not_in_store(&dir, |_| true).unwrap().is_empty(),
            "and afterwards every one of them is"
        );
    }

    #[test]
    fn should_migrate_one_repository_and_leave_the_others() {
        let temp = tempdir().unwrap();
        let dir = temp.path().join("reviews");
        a_session(&dir);

        let report = migrate_repos(&dir, true, &NoMessages, |key| key == "somewhere-else").unwrap();

        assert_eq!(report.comments, 0, "nothing from another repository moved");
        assert!(!comments_not_in_store(&dir, |_| true).unwrap().is_empty());
    }
}

#[cfg(test)]
mod scoped_verification_tests {
    use super::*;
    use crate::model::review::SessionDiffSource;
    use crate::model::{CommentType, FileStatus, LineSide, ReviewSession};
    use crate::persistence::storage::save_session_in_dir;
    use tempfile::tempdir;

    fn session_for(dir: &Path, repo: &str) {
        let mut session = ReviewSession::new(
            PathBuf::from(format!("/repos/{repo}")),
            "head".to_string(),
            Some("main".to_string()),
            SessionDiffSource::CommitRange,
        );
        session.commit_range = Some(vec!["aaaa".to_string()]);
        session.add_file(PathBuf::from("src/main.rs"), FileStatus::Modified, 0);
        let mut comment = Comment::new(
            format!("about {repo}"),
            CommentType::from_id("issue"),
            Some(LineSide::New),
        );
        comment.commit_id = Some("aaaa".to_string());
        session
            .get_file_mut(&PathBuf::from("src/main.rs"))
            .unwrap()
            .add_line_comment(1, comment);
        save_session_in_dir(&session, dir).unwrap();
    }

    #[test]
    fn should_check_only_the_repository_that_was_migrated() {
        // A machine-wide check fails on the first repository that has not
        // moved, so a per-repository migration would never be accepted — on a
        // machine with fifteen repositories, never at all.
        let temp = tempdir().unwrap();
        let dir = temp.path().join("reviews");
        session_for(&dir, "one");
        session_for(&dir, "two");

        migrate_repos(&dir, true, &NoMessages, |key| key == "one").unwrap();

        assert!(
            comments_not_in_store(&dir, |key| key == "one")
                .unwrap()
                .is_empty(),
            "the repository that moved checks out"
        );
        assert!(
            !comments_not_in_store(&dir, |_| true).unwrap().is_empty(),
            "while the machine as a whole does not, which is why the scope matters"
        );
    }
}
