//! Comments, stored beside the commits they were written on.
//!
//! A comment is a fact about a commit, not about the range that happened to be
//! on screen when it was written. Filing it under the range is what made a
//! review lose its own comments the moment a commit was added to the branch:
//! the range's endpoints moved, so the session slug moved, so the comments
//! were in another file.
//!
//! Here one file holds the comments for one commit, named by its sha. A review
//! reads the files for the commits it has in view, whatever range it was
//! opened with. `index.json` beside them is a cache — one row per file — so
//! resolving a review is one small read rather than opening every file in the
//! repo; it can always be rebuilt from the files themselves, so it can never
//! be the thing that loses a comment.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, TuicrError};
use crate::model::{Comment, CommentScope};
use crate::persistence::storage::{with_reviews_dir_lock, write_atomic};

const COMMENTS_DIRNAME: &str = "comments";
const INDEX_FILENAME: &str = "index.json";
const MIGRATED_FILENAME: &str = "migrated";
const FORMAT_VERSION: u32 = 1;

/// One commit's comments, as stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeFile {
    pub version: u32,
    pub scope: CommentScope,
    /// The commit's message when the file was written.
    ///
    /// Its first line is the only thing linking a rewritten commit to the one
    /// that replaced it, short of patch-id, so a file without one cannot be
    /// recovered after an amend. The rest of it is what a comment on the
    /// message was written against, kept so a reader can still see it once the
    /// commit is gone.
    #[serde(default, alias = "summary")]
    pub message: Option<String>,
    #[serde(default)]
    pub comments: Vec<Comment>,
}

impl ScopeFile {
    fn new(scope: CommentScope, message: Option<String>) -> Self {
        Self {
            version: FORMAT_VERSION,
            scope,
            message,
            comments: Vec::new(),
        }
    }

    /// The first line of the message — what a rewrite keeps, and what the
    /// index is looked up by.
    pub fn summary(&self) -> Option<&str> {
        self.message.as_deref().and_then(|m| m.lines().next())
    }
}

/// One row per stored file. A cache: rebuilt by reading the directory, never
/// read as truth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexRow {
    pub file: String,
    pub scope: CommentScope,
    #[serde(default)]
    pub summary: Option<String>,
    pub count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub rows: Vec<IndexRow>,
}

/// The comments of one repository.
pub struct CommentStore {
    reviews_dir: PathBuf,
    root: PathBuf,
}

impl CommentStore {
    /// `repo_key` is the repo's `owner/repo` coordinate, so every worktree of
    /// a repo reads the same comments. Slashes become `-`: one flat directory
    /// per repo, no nesting to walk.
    pub fn new(reviews_dir: impl Into<PathBuf>, repo_key: &str) -> Self {
        let reviews_dir = reviews_dir.into();
        let root = reviews_dir
            .join(COMMENTS_DIRNAME)
            .join(sanitized_repo_key(repo_key));
        Self { reviews_dir, root }
    }

    /// A store for a directory that already exists under `comments/`.
    ///
    /// The name is taken as-is. Feeding a directory name back through `new`
    /// sanitizes an already-sanitized key and lands on a different directory —
    /// a shadow beside the real one, which is where the migration marker went
    /// while every repository stayed unswitched.
    pub fn at_dir(reviews_dir: impl Into<PathBuf>, dir_name: &str) -> Self {
        let reviews_dir = reviews_dir.into();
        let root = reviews_dir.join(COMMENTS_DIRNAME).join(dir_name);
        Self { reviews_dir, root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// True once this repository's comments live here.
    ///
    /// An explicit switch rather than "did a read return anything": with it,
    /// a review either uses the store for both reads and writes or does not
    /// touch it at all. Inferring the answer from whether a read came back
    /// non-empty leaves a half-migrated state in which reads come from one
    /// place and writes go to another, which is how a comment gets written to
    /// the session and erased by the next read.
    pub fn in_use(&self) -> bool {
        self.root.join(MIGRATED_FILENAME).is_file()
    }

    /// Mark this repository as migrated, so reviews start using the store.
    ///
    /// A marker of its own, not the index: every write rebuilds the index, so
    /// testing that file would flip the switch on the first write rather than
    /// when the migration was checked and accepted.
    pub fn take_over(&self) -> Result<()> {
        self.rebuild_index()?;
        write_atomic(&self.root.join(MIGRATED_FILENAME), b"1\n")
    }

    /// Remove a comment and every reply to it, wherever it is stored.
    /// Returns how many went.
    pub fn delete_thread(&self, id: &str) -> Result<usize> {
        let Some(scope) = self.scope_of(id)? else {
            return Ok(0);
        };
        let mut removed = 0;
        self.update_scope(&scope, None, |file| {
            let root = file
                .comments
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.in_reply_to.clone().unwrap_or_else(|| c.id.clone()));
            let Some(root) = root else {
                return Ok(());
            };
            let before = file.comments.len();
            file.comments
                .retain(|c| c.id != root && c.in_reply_to.as_deref() != Some(root.as_str()));
            removed = before - file.comments.len();
            Ok(())
        })?;
        Ok(removed)
    }

    /// Every comment written against any of `scopes`, in stored order.
    ///
    /// Scopes with no file are simply absent — a commit nobody has commented
    /// on is the common case, not an error.
    pub fn comments_for(&self, scopes: &[CommentScope]) -> Result<Vec<Comment>> {
        let mut out = Vec::new();
        for scope in scopes {
            if let Some(file) = self.read_scope(scope)? {
                out.extend(file.comments);
            }
        }
        Ok(out)
    }

    /// Store a comment against its own anchor's scope.
    pub fn add(&self, scope: &CommentScope, message: Option<&str>, comment: Comment) -> Result<()> {
        self.add_many(scope, message, vec![comment])
    }

    /// Store several comments against one scope in a single locked write.
    ///
    /// A comment already there, by id, is left as it stands: importing the
    /// same session twice must not double its comments.
    pub fn add_many(
        &self,
        scope: &CommentScope,
        message: Option<&str>,
        comments: Vec<Comment>,
    ) -> Result<()> {
        self.update_scope(scope, message, |file| {
            for comment in comments {
                if file.comments.iter().any(|c| c.id == comment.id) {
                    continue;
                }
                file.comments.push(comment);
            }
            Ok(())
        })
    }

    /// Apply `edit` to the comment with `id`, wherever it is stored.
    pub fn update_comment(&self, id: &str, edit: impl FnOnce(&mut Comment)) -> Result<bool> {
        let Some(scope) = self.scope_of(id)? else {
            return Ok(false);
        };
        let mut found = false;
        self.update_scope(&scope, None, |file| {
            if let Some(comment) = file.comments.iter_mut().find(|c| c.id == id) {
                edit(comment);
                found = true;
            }
            Ok(())
        })?;
        Ok(found)
    }

    /// The comment with `id`, or the one whose id starts with it when that is
    /// unambiguous — the same courtesy git extends to short shas.
    pub fn find(&self, id_or_prefix: &str) -> Result<Option<Comment>> {
        let mut hit = None;
        for scope in self.scopes()? {
            let Some(file) = self.read_scope(&scope)? else {
                continue;
            };
            for comment in file.comments {
                if comment.id == id_or_prefix {
                    return Ok(Some(comment));
                }
                if comment.id.starts_with(id_or_prefix) {
                    if hit.is_some() {
                        return Err(TuicrError::InvalidInput(format!(
                            "comment id `{id_or_prefix}` is ambiguous"
                        )));
                    }
                    hit = Some(comment);
                }
            }
        }
        Ok(hit)
    }

    /// The index, rebuilt from the files if it is missing or unreadable. A
    /// stale index is a cache miss, never a lost comment.
    pub fn index(&self) -> Result<Index> {
        let path = self.root.join(INDEX_FILENAME);
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Index>(&bytes) {
                Ok(index) if index.version == FORMAT_VERSION => Ok(index),
                _ => self.rebuild_index(),
            },
            Err(_) => self.rebuild_index(),
        }
    }

    /// Read every stored file and write the index from what is actually there.
    pub fn rebuild_index(&self) -> Result<Index> {
        let mut rows = Vec::new();
        for scope in self.scopes()? {
            if let Some(file) = self.read_scope(&scope)? {
                rows.push(IndexRow {
                    file: scope.file_name(),
                    scope,
                    summary: file.summary().map(str::to_string),
                    count: file.comments.len(),
                });
            }
        }
        rows.sort_by(|a, b| a.file.cmp(&b.file));
        let index = Index {
            version: FORMAT_VERSION,
            rows,
        };
        self.write_index(&index)?;
        Ok(index)
    }

    /// Commits that hold comments but are not among `live`, keyed by summary.
    ///
    /// This is how a rewritten commit's comments are found again: an amend or
    /// rebase leaves the old sha behind, and the summary is what an amend
    /// keeps.
    ///
    /// `live` is the review's own commits with their summaries, so the
    /// question asked is "which commit *in this review* was this file written
    /// against", not "is this summary unique in the repo". The difference is
    /// not academic: a commit amended twice leaves two dead files sharing one
    /// summary, and on a real store that is the common case — both are earlier
    /// generations of the same commit and both belong to it.
    ///
    /// What is refused is the other direction: a dead file whose summary
    /// matches more than one commit in view, where attaching it would mean
    /// picking one. A thread shown against the wrong commit is worse than one
    /// shown out of place.
    pub fn orphans_for(
        &self,
        live: &[(CommentScope, String)],
        still_exists: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<(CommentScope, Vec<OrphanScope>)>> {
        let index = self.index()?;
        let here: Vec<&CommentScope> = live.iter().map(|(scope, _)| scope).collect();

        let mut claimed: BTreeMap<String, Vec<OrphanScope>> = BTreeMap::new();
        for row in index.rows {
            if row.count == 0 || here.contains(&&row.scope) {
                continue;
            }
            let Some(summary) = row.summary.clone() else {
                continue;
            };
            if live.iter().filter(|(_, s)| *s == summary).count() != 1 {
                continue;
            }
            // "Not in this review" is not "dead". A commit still reachable
            // from a ref belongs to another branch, and claiming its comments
            // shows them under the wrong commit — and deleting that thread
            // then deletes the other branch's conversation.
            //
            // Asked last on purpose: it costs a git call per commit, and only
            // a row whose summary matches something in view could be claimed
            // at all. Asking every row first spent most of a second, on a
            // repository with a thousand refs, answering about rows that were
            // never candidates.
            if row.scope.sha().is_some_and(still_exists) {
                continue;
            }
            claimed
                .entry(summary)
                .or_default()
                .push(OrphanScope(row.scope));
        }

        Ok(live
            .iter()
            .filter_map(|(scope, summary)| {
                claimed
                    .get(summary)
                    .map(|orphans| (scope.clone(), orphans.clone()))
            })
            .collect())
    }

    fn scopes(&self) -> Result<Vec<CommentScope>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(_) => return Ok(Vec::new()),
        };
        let mut scopes = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == INDEX_FILENAME || !name.ends_with(".json") {
                continue;
            }
            if let Some(file) = self.read_file(&entry.path())? {
                scopes.push(file.scope);
            }
        }
        scopes.sort_by_key(|scope: &CommentScope| scope.file_name());
        Ok(scopes)
    }

    fn scope_of(&self, id: &str) -> Result<Option<CommentScope>> {
        for scope in self.scopes()? {
            if let Some(file) = self.read_scope(&scope)?
                && file.comments.iter().any(|c| c.id == id)
            {
                return Ok(Some(scope));
            }
        }
        Ok(None)
    }

    fn read_scope(&self, scope: &CommentScope) -> Result<Option<ScopeFile>> {
        self.read_file(&self.root.join(scope.file_name()))
    }

    fn read_file(&self, path: &Path) -> Result<Option<ScopeFile>> {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(|e| {
                TuicrError::CorruptedSession(format!("{}: {e}", path.display()))
            })?)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(TuicrError::Io(err)),
        }
    }

    /// Read-modify-write one scope's file under the reviews-dir lock, so a
    /// reviewer commenting in the TUI and an agent replying from the CLI
    /// cannot overwrite each other. One small file, not the whole store.
    fn update_scope(
        &self,
        scope: &CommentScope,
        summary: Option<&str>,
        edit: impl FnOnce(&mut ScopeFile) -> Result<()>,
    ) -> Result<()> {
        let root = self.root.clone();
        let path = root.join(scope.file_name());
        with_reviews_dir_lock(&self.reviews_dir.clone(), || {
            let mut file = self
                .read_file(&path)?
                .unwrap_or_else(|| ScopeFile::new(scope.clone(), summary.map(str::to_string)));
            if file.message.is_none() && summary.is_some() {
                file.message = summary.map(str::to_string);
            }
            edit(&mut file)?;
            fs::create_dir_all(&root)?;
            write_atomic(&path, serde_json::to_string_pretty(&file)?.as_bytes())?;
            self.reindex_unlocked(&file, scope)
        })
    }

    fn reindex_unlocked(&self, file: &ScopeFile, scope: &CommentScope) -> Result<()> {
        let mut index = match fs::read(self.root.join(INDEX_FILENAME)) {
            Ok(bytes) => serde_json::from_slice::<Index>(&bytes).unwrap_or_default(),
            Err(_) => Index::default(),
        };
        index.version = FORMAT_VERSION;
        let row = IndexRow {
            file: scope.file_name(),
            scope: scope.clone(),
            summary: file.summary().map(str::to_string),
            count: file.comments.len(),
        };
        match index.rows.iter_mut().find(|r| r.file == row.file) {
            Some(existing) => *existing = row,
            None => index.rows.push(row),
        }
        index.rows.sort_by(|a, b| a.file.cmp(&b.file));
        self.write_index(&index)
    }

    fn write_index(&self, index: &Index) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        write_atomic(
            &self.root.join(INDEX_FILENAME),
            serde_json::to_string_pretty(index)?.as_bytes(),
        )
    }
}

/// What a review shows: comments on its own commits, comments on earlier
/// versions of them, and comments a dead file's summary claims.
#[derive(Debug, Default)]
pub struct ResolvedComments {
    pub comments: Vec<Comment>,
    /// Ids of the comments that came from a commit no longer in the review —
    /// an earlier version of one that is. The view says so rather than
    /// pretending they were written on what is on screen.
    pub from_earlier: std::collections::HashSet<String>,
    /// The commit in view each carried comment is shown under. Its anchor
    /// still names the commit it was written on, which is a fact and does not
    /// change; this says where that commit went.
    pub shown_under: std::collections::HashMap<String, String>,
}

/// Gather a review's comments.
///
/// `live` is the review's commits with their summaries; `predecessors` maps
/// each of them to the shas it was built from, which the VCS answers and this
/// only consumes — the store has no business knowing what a reflog is.
///
/// Lineage runs first because it knows what happened; summary matching then
/// covers what it cannot reach, which is mostly a series rebuilt after a
/// `reset --hard`, where git recorded no relation at all.
pub fn resolve_for_review(
    store: &CommentStore,
    live: &[(CommentScope, String)],
    predecessors: &BTreeMap<String, Vec<String>>,
    still_exists: &dyn Fn(&str) -> bool,
) -> Result<ResolvedComments> {
    let mut resolved = ResolvedComments::default();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_scopes: Vec<CommentScope> = Vec::new();

    let take = |store: &CommentStore,
                scope: &CommentScope,
                under: Option<&CommentScope>,
                resolved: &mut ResolvedComments,
                seen_ids: &mut std::collections::HashSet<String>|
     -> Result<()> {
        for comment in store.comments_for(std::slice::from_ref(scope))? {
            if !seen_ids.insert(comment.id.clone()) {
                continue;
            }
            if let Some(under) = under {
                resolved.from_earlier.insert(comment.id.clone());
                if let Some(sha) = under.sha() {
                    resolved
                        .shown_under
                        .insert(comment.id.clone(), sha.to_string());
                }
            }
            resolved.comments.push(comment);
        }
        Ok(())
    };

    for (scope, _) in live {
        take(store, scope, None, &mut resolved, &mut seen_ids)?;
        seen_scopes.push(scope.clone());
    }

    for (scope, _) in live {
        let Some(sha) = scope.sha() else { continue };
        for old in predecessors.get(sha).into_iter().flatten() {
            let old_scope = CommentScope::commit(old.clone());
            if seen_scopes.contains(&old_scope) {
                continue;
            }
            take(store, &old_scope, Some(scope), &mut resolved, &mut seen_ids)?;
            seen_scopes.push(old_scope);
        }
    }

    for (live_scope, orphans) in store.orphans_for(live, still_exists)? {
        for OrphanScope(scope) in orphans {
            if seen_scopes.contains(&scope) {
                continue;
            }
            take(
                store,
                &scope,
                Some(&live_scope),
                &mut resolved,
                &mut seen_ids,
            )?;
            seen_scopes.push(scope);
        }
    }

    Ok(resolved)
}

/// A scope holding comments whose commit is no longer under review. Named so
/// a caller cannot pass one where a live scope belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanScope(pub CommentScope);

/// A stable short name for a checkout, for the file that holds its
/// uncommitted-work comments.
///
/// FNV-1a rather than the standard hasher, whose output is explicitly not
/// stable across releases — this ends up in a filename that has to keep
/// meaning the same thing after an upgrade.
pub fn checkout_key(path: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// `owner/repo` is a path in disguise; flatten it so the store stays one
/// directory deep — without letting two different repositories flatten onto
/// each other.
///
/// Replacing every unsafe character with `-` is not injective: `foo/bar-baz`
/// and `foo-bar/baz` land on the same name, as do the same coordinate on two
/// forges and two unrelated local checkouts that share a directory name. Two
/// repositories sharing a store means one's comments surface in the other, so
/// the readable name carries a short digest of the key it came from.
pub fn sanitized_repo_key(key: &str) -> String {
    let readable: String = key
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => c,
            _ => '-',
        })
        .collect();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{readable}-{:08x}", hash as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CommentAnchor, CommentType, LineSide};
    use tempfile::tempdir;

    fn store(dir: &Path) -> CommentStore {
        CommentStore::new(dir, "agavra/tuicr")
    }

    fn comment(body: &str, scope: CommentScope, line: u32) -> Comment {
        let mut c = Comment::new(
            body.to_string(),
            CommentType::from_id("issue"),
            Some(LineSide::New),
        );
        c.anchor = Some(CommentAnchor::line(
            scope,
            "src/main.rs",
            line,
            LineSide::New,
        ));
        c
    }

    #[test]
    fn should_not_be_in_use_until_a_repository_is_migrated() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        assert!(
            !store.in_use(),
            "an untouched repo keeps using its sessions"
        );

        store.take_over().unwrap();

        assert!(store.in_use(), "and switches over once, deliberately");
    }

    #[test]
    fn should_count_as_in_use_even_with_nothing_stored_yet() {
        // The first comment written after migrating must go to the store, not
        // to the session, so "in use" cannot mean "holds something".
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        store.take_over().unwrap();

        assert!(store.in_use());
        assert!(
            store
                .comments_for(&[CommentScope::commit("aaaa1111")])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn should_delete_a_thread_and_its_replies() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let scope = CommentScope::commit("aaaa1111");
        let root = comment("why this?", scope.clone(), 42);
        let root_id = root.id.clone();
        let mut answer = comment("because", scope.clone(), 42);
        answer.in_reply_to = Some(root_id.clone());
        let other = comment("unrelated", scope.clone(), 7);
        let other_id = other.id.clone();
        store
            .add_many(&scope, None, vec![root, answer, other])
            .unwrap();

        let removed = store.delete_thread(&root_id).unwrap();

        assert_eq!(removed, 2, "the root and its reply");
        let left = store.comments_for(&[scope]).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, other_id, "the other thread is untouched");
    }

    #[test]
    fn should_delete_a_thread_from_any_of_its_replies() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let scope = CommentScope::commit("aaaa1111");
        let root = comment("why this?", scope.clone(), 42);
        let root_id = root.id.clone();
        let mut answer = comment("because", scope.clone(), 42);
        answer.in_reply_to = Some(root_id.clone());
        let answer_id = answer.id.clone();
        store.add_many(&scope, None, vec![root, answer]).unwrap();

        assert_eq!(store.delete_thread(&answer_id).unwrap(), 2);
        assert!(store.comments_for(&[scope]).unwrap().is_empty());
    }

    #[test]
    fn should_hand_back_the_comments_of_every_commit_in_view() {
        // The whole point: a review that has both commits in view sees both
        // commits' comments, whatever range it was opened with.
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let older = CommentScope::commit("aaaa1111");
        let newer = CommentScope::commit("bbbb2222");
        store
            .add(
                &older,
                Some("first commit"),
                comment("why this?", older.clone(), 42),
            )
            .unwrap();
        store
            .add(
                &newer,
                Some("second commit"),
                comment("and this?", newer.clone(), 7),
            )
            .unwrap();

        let both = store.comments_for(&[older.clone(), newer.clone()]).unwrap();
        assert_eq!(both.len(), 2);

        let narrowed = store.comments_for(&[newer]).unwrap();
        assert_eq!(narrowed.len(), 1, "a narrower range sees only its own");
        assert_eq!(narrowed[0].content, "and this?");
    }

    #[test]
    fn should_keep_uncommitted_comments_to_their_own_checkout() {
        // Two worktrees of one repo have different uncommitted state, so one
        // must not show the other's notes.
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let here = CommentScope::working_tree("checkout-a");
        let there = CommentScope::working_tree("checkout-b");
        store
            .add(&here, None, comment("half-finished", here.clone(), 3))
            .unwrap();

        assert_eq!(store.comments_for(&[here]).unwrap().len(), 1);
        assert!(store.comments_for(&[there]).unwrap().is_empty());
    }

    #[test]
    fn should_rebuild_an_index_that_is_missing_or_unreadable() {
        // The index is a cache. Losing it must cost a directory read, never a
        // comment.
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let scope = CommentScope::commit("aaaa1111");
        store
            .add(
                &scope,
                Some("a commit"),
                comment("look here", scope.clone(), 42),
            )
            .unwrap();

        fs::write(store.root().join(INDEX_FILENAME), b"{ not json").unwrap();
        let index = store.index().unwrap();

        assert_eq!(index.rows.len(), 1);
        assert_eq!(index.rows[0].count, 1);
        assert_eq!(index.rows[0].summary.as_deref(), Some("a commit"));
        assert_eq!(store.comments_for(&[scope]).unwrap().len(), 1);
    }

    #[test]
    fn should_show_comments_written_on_an_earlier_version_of_a_commit() {
        // The point of the whole design: amend a commit and the comments on it
        // are still the comments on it.
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let old = CommentScope::commit("aaaa1111");
        let now = CommentScope::commit("cccc3333");
        store
            .add(
                &old,
                Some("a change"),
                comment("why this?", old.clone(), 42),
            )
            .unwrap();
        store
            .add(&now, Some("a change"), comment("and this?", now.clone(), 7))
            .unwrap();

        let live = vec![(now.clone(), "a change".to_string())];
        let predecessors = BTreeMap::from([("cccc3333".to_string(), vec!["aaaa1111".to_string()])]);
        let resolved = resolve_for_review(&store, &live, &predecessors, &|_| false).unwrap();

        assert_eq!(resolved.comments.len(), 2);
        let carried = resolved
            .comments
            .iter()
            .find(|c| c.content == "why this?")
            .unwrap();
        assert!(
            resolved.from_earlier.contains(&carried.id),
            "and it says the comment was written on an earlier version"
        );
        let current = resolved
            .comments
            .iter()
            .find(|c| c.content == "and this?")
            .unwrap();
        assert!(!resolved.from_earlier.contains(&current.id));
    }

    #[test]
    fn should_fall_back_to_the_summary_when_lineage_knows_nothing() {
        // A series rebuilt after a reset --hard: git recorded no relation, so
        // only the message connects them.
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let old = CommentScope::commit("aaaa1111");
        let now = CommentScope::commit("cccc3333");
        store
            .add(
                &old,
                Some("a change"),
                comment("still relevant", old.clone(), 42),
            )
            .unwrap();

        let live = vec![(now, "a change".to_string())];
        let resolved = resolve_for_review(&store, &live, &BTreeMap::new(), &|_| false).unwrap();

        assert_eq!(resolved.comments.len(), 1);
        assert_eq!(resolved.comments[0].content, "still relevant");
        assert!(resolved.from_earlier.contains(&resolved.comments[0].id));
    }

    #[test]
    fn should_not_hand_back_a_comment_twice() {
        // Lineage and the summary both reach the same dead commit.
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let old = CommentScope::commit("aaaa1111");
        let now = CommentScope::commit("cccc3333");
        store
            .add(&old, Some("a change"), comment("once", old.clone(), 42))
            .unwrap();

        let live = vec![(now, "a change".to_string())];
        let predecessors = BTreeMap::from([("cccc3333".to_string(), vec!["aaaa1111".to_string()])]);
        let resolved = resolve_for_review(&store, &live, &predecessors, &|_| false).unwrap();

        assert_eq!(resolved.comments.len(), 1);
    }

    #[test]
    fn should_gather_every_generation_a_fixup_was_folded_through() {
        // What the reflog hands back for one commit: several amends and the
        // fixup! that was squashed in. All of their comments belong to it.
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let now = CommentScope::commit("cccc3333");
        for (sha, body) in [
            ("aaaa1111", "first pass"),
            ("bbbb2222", "second pass"),
            ("dddd4444", "on the fixup"),
        ] {
            let scope = CommentScope::commit(sha);
            store
                .add(&scope, Some("a change"), comment(body, scope.clone(), 1))
                .unwrap();
        }

        let live = vec![(now, "a change".to_string())];
        let predecessors = BTreeMap::from([(
            "cccc3333".to_string(),
            vec![
                "aaaa1111".to_string(),
                "bbbb2222".to_string(),
                "dddd4444".to_string(),
            ],
        )]);
        let resolved = resolve_for_review(&store, &live, &predecessors, &|_| false).unwrap();

        assert_eq!(resolved.comments.len(), 3);
        assert_eq!(resolved.from_earlier.len(), 3);
    }

    #[test]
    fn should_leave_another_review_alone() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let elsewhere = CommentScope::commit("aaaa1111");
        store
            .add(
                &elsewhere,
                Some("someone else's change"),
                comment("x", elsewhere.clone(), 1),
            )
            .unwrap();

        let live = vec![(CommentScope::commit("cccc3333"), "a change".to_string())];
        let resolved = resolve_for_review(&store, &live, &BTreeMap::new(), &|_| false).unwrap();

        assert!(resolved.comments.is_empty());
    }

    #[test]
    fn should_bring_back_every_earlier_generation_of_a_commit() {
        // Amended twice, so two dead files carry the same summary. Both are
        // earlier versions of the commit in view and both hold its comments —
        // on a real store this is the common case, not the odd one.
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        for sha in ["aaaa1111", "bbbb2222"] {
            let scope = CommentScope::commit(sha);
            store
                .add(
                    &scope,
                    Some("diff: look around"),
                    comment("why?", scope.clone(), 8),
                )
                .unwrap();
        }

        let live = vec![(
            CommentScope::commit("cccc3333"),
            "diff: look around".to_string(),
        )];
        let found = store.orphans_for(&live, &|_| false).unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, CommentScope::commit("cccc3333"));
        assert_eq!(found[0].1.len(), 2, "both generations come back");
    }

    #[test]
    fn should_not_claim_a_commit_that_is_alive_on_another_branch() {
        // "Not in this review" is not "dead": the commit is on another branch,
        // and claiming its thread here shows it under the wrong commit — and
        // deleting it then deletes the other branch's conversation.
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let elsewhere = CommentScope::commit("aaaa1111");
        store
            .add(
                &elsewhere,
                Some("a change"),
                comment("theirs", elsewhere.clone(), 8),
            )
            .unwrap();

        let live = vec![(CommentScope::commit("cccc3333"), "a change".to_string())];
        let claimed = store.orphans_for(&live, &|sha| sha == "aaaa1111").unwrap();
        assert!(claimed.is_empty(), "a live commit keeps its comments");

        let dead = store.orphans_for(&live, &|_| false).unwrap();
        assert_eq!(dead.len(), 1, "a dead one is still recovered");
    }

    #[test]
    fn should_refuse_when_a_dead_file_could_belong_to_two_commits_in_view() {
        // Two commits in the review share a summary, so attaching the dead
        // file to either would be a guess.
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let dead = CommentScope::commit("aaaa1111");
        store
            .add(&dead, Some("fixup"), comment("x", dead.clone(), 1))
            .unwrap();

        let live = vec![
            (CommentScope::commit("bbbb2222"), "fixup".to_string()),
            (CommentScope::commit("cccc3333"), "fixup".to_string()),
        ];

        assert!(store.orphans_for(&live, &|_| false).unwrap().is_empty());
    }

    #[test]
    fn should_find_the_commit_a_rewrite_left_behind() {
        // An amend leaves the old sha holding the comments. The summary is
        // what survives the rewrite, so it is what finds them again.
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let old = CommentScope::commit("aaaa1111");
        store
            .add(
                &old,
                Some("diff: look around"),
                comment("why?", old.clone(), 8),
            )
            .unwrap();

        let live = vec![(
            CommentScope::commit("cccc3333"),
            "diff: look around".to_string(),
        )];
        let orphans = store.orphans_for(&live, &|_| false).unwrap();

        assert_eq!(
            orphans,
            vec![(CommentScope::commit("cccc3333"), vec![OrphanScope(old)])],
            "the dead file is offered for the commit that replaced it"
        );
    }

    #[test]
    fn should_refuse_to_guess_between_two_commits_with_one_summary() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        for sha in ["aaaa1111", "bbbb2222"] {
            let scope = CommentScope::commit(sha);
            store
                .add(&scope, Some("fixup"), comment("x", scope.clone(), 1))
                .unwrap();
        }

        let orphans = store
            .orphans_for(
                &[(
                    CommentScope::commit("cccc3333"),
                    "something else".to_string(),
                )],
                &|_| false,
            )
            .unwrap();

        assert!(
            orphans.is_empty(),
            "a summary no commit in view carries belongs to another review"
        );
    }

    #[test]
    fn should_resolve_a_comment_id_from_an_unambiguous_prefix() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let scope = CommentScope::commit("aaaa1111");
        let written = comment("check this", scope.clone(), 42);
        let id = written.id.clone();
        store.add(&scope, None, written).unwrap();

        let found = store.find(&id[..8]).unwrap().expect("found by prefix");
        assert_eq!(found.id, id);
        assert!(store.find("no-such-comment").unwrap().is_none());
    }

    #[test]
    fn should_edit_a_comment_wherever_it_is_stored() {
        let dir = tempdir().unwrap();
        let store = store(dir.path());
        let scope = CommentScope::commit("aaaa1111");
        let written = comment("check this", scope.clone(), 42);
        let id = written.id.clone();
        store.add(&scope, None, written).unwrap();

        assert!(store.update_comment(&id, |c| c.resolved = true).unwrap());

        let after = store.find(&id).unwrap().unwrap();
        assert!(after.resolved);
        assert!(
            !store
                .update_comment("missing", |c| c.resolved = true)
                .unwrap()
        );
    }
}

#[cfg(test)]
mod carried_visibility_tests {
    use super::*;
    use crate::model::{CommentAnchor, CommentType, LineSide};
    use tempfile::tempdir;

    #[test]
    fn should_show_a_carried_comment_under_the_commit_its_own_became() {
        // Narrowing the review to one commit filters by commit id, so a
        // comment carried from an earlier version has to answer with the
        // commit that is in view — or it vanishes exactly when the reader
        // looks straight at it.
        let dir = tempdir().unwrap();
        let store = CommentStore::new(dir.path(), "agavra/tuicr");
        let old = CommentScope::commit("aaaa1111");
        let now = CommentScope::commit("cccc3333");
        let mut comment = Comment::new(
            "written on the fixup".to_string(),
            CommentType::from_id("issue"),
            Some(LineSide::New),
        );
        comment.anchor = Some(CommentAnchor::file(old.clone(), "src/main.rs"));
        store.add(&old, Some("a change"), comment).unwrap();

        let live = vec![(now, "a change".to_string())];
        let predecessors = BTreeMap::from([("cccc3333".to_string(), vec!["aaaa1111".to_string()])]);
        let resolved = resolve_for_review(&store, &live, &predecessors, &|_| false).unwrap();

        let id = &resolved.comments[0].id;
        assert_eq!(
            resolved.shown_under.get(id).map(String::as_str),
            Some("cccc3333"),
            "shown under the commit in view"
        );
        assert_eq!(
            resolved.comments[0].anchor.as_ref().unwrap().scope.sha(),
            Some("aaaa1111"),
            "while the anchor still says where it was written"
        );
    }

    #[test]
    fn should_attribute_a_summary_match_to_the_commit_that_claimed_it() {
        let dir = tempdir().unwrap();
        let store = CommentStore::new(dir.path(), "agavra/tuicr");
        let old = CommentScope::commit("aaaa1111");
        let mut comment = Comment::new(
            "rebuilt series".to_string(),
            CommentType::from_id("issue"),
            None,
        );
        comment.anchor = Some(CommentAnchor::file(old.clone(), "src/main.rs"));
        store.add(&old, Some("a change"), comment).unwrap();

        let live = vec![(CommentScope::commit("dddd4444"), "a change".to_string())];
        let resolved = resolve_for_review(&store, &live, &BTreeMap::new(), &|_| false).unwrap();

        let id = &resolved.comments[0].id;
        assert_eq!(
            resolved.shown_under.get(id).map(String::as_str),
            Some("dddd4444")
        );
    }
}

#[cfg(test)]
mod switch_tests {
    use super::*;
    use crate::model::CommentType;
    use tempfile::tempdir;

    #[test]
    fn should_not_switch_a_repository_over_just_because_something_was_written() {
        // Every write rebuilds the index, so a switch that tested for the
        // index would be thrown by the first write rather than by a migration
        // that was checked and accepted.
        let dir = tempdir().unwrap();
        let store = CommentStore::new(dir.path(), "agavra/tuicr");
        let scope = CommentScope::commit("aaaa1111");
        store
            .add(
                &scope,
                None,
                Comment::new("x".to_string(), CommentType::from_id("note"), None),
            )
            .unwrap();

        assert!(
            store.root().join(INDEX_FILENAME).is_file(),
            "the index is there"
        );
        assert!(
            !store.in_use(),
            "but the repository has not been switched over"
        );

        store.take_over().unwrap();
        assert!(store.in_use());
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn should_keep_two_repositories_apart_when_their_names_flatten_alike() {
        // Sharing a store means one repository's comments surface in another.
        assert_ne!(
            sanitized_repo_key("foo/bar-baz"),
            sanitized_repo_key("foo-bar/baz")
        );
        assert_ne!(
            sanitized_repo_key("github.com/acme/app"),
            sanitized_repo_key("gitlab.com/acme/app")
        );
        assert_eq!(
            sanitized_repo_key("agavra/tuicr"),
            sanitized_repo_key("agavra/tuicr"),
            "and the same repository always lands in the same place"
        );
        assert!(
            sanitized_repo_key("agavra/tuicr").starts_with("agavra-tuicr-"),
            "still readable in a directory listing"
        );
    }
}

#[cfg(test)]
mod dir_name_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn should_reach_the_same_store_from_a_key_or_from_its_directory_name() {
        // Enumerating `comments/` and passing the names back as keys sanitizes
        // an already-sanitized name: a shadow directory beside the real one,
        // which is where the migration marker landed while every repository
        // stayed on its session files.
        let dir = tempdir().unwrap();
        let from_key = CommentStore::new(dir.path(), "agavra/tuicr");
        let name = from_key
            .root()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let from_dir = CommentStore::at_dir(dir.path(), &name);

        assert_eq!(from_key.root(), from_dir.root());
        from_dir.take_over().unwrap();
        assert!(from_key.in_use(), "the marker lands on the real store");
    }
}
