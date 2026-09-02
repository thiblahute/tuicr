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
const FORMAT_VERSION: u32 = 1;

/// One commit's comments, as stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeFile {
    pub version: u32,
    pub scope: CommentScope,
    /// The commit's summary when the file was written. The only thing linking
    /// a rewritten commit to the one that replaced it, short of patch-id.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub comments: Vec<Comment>,
}

impl ScopeFile {
    fn new(scope: CommentScope, summary: Option<String>) -> Self {
        Self {
            version: FORMAT_VERSION,
            scope,
            summary,
            comments: Vec::new(),
        }
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
            .join(sanitize_repo_key(repo_key));
        Self { reviews_dir, root }
    }

    pub fn root(&self) -> &Path {
        &self.root
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
    pub fn add(&self, scope: &CommentScope, summary: Option<&str>, comment: Comment) -> Result<()> {
        self.update_scope(scope, summary, |file| {
            file.comments.push(comment);
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
                    summary: file.summary,
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
    /// keeps. A summary claimed by more than one dead file is ambiguous and
    /// left out — showing a thread against the wrong commit is worse than not
    /// showing it in place.
    pub fn orphans_by_summary(
        &self,
        live: &[CommentScope],
    ) -> Result<BTreeMap<String, OrphanScope>> {
        let index = self.index()?;
        let mut by_summary: BTreeMap<String, Vec<IndexRow>> = BTreeMap::new();
        for row in index.rows {
            if row.count == 0 || live.contains(&row.scope) {
                continue;
            }
            let Some(summary) = row.summary.clone() else {
                continue;
            };
            by_summary.entry(summary).or_default().push(row);
        }
        Ok(by_summary
            .into_iter()
            .filter(|(_, rows)| rows.len() == 1)
            .map(|(summary, mut rows)| (summary, OrphanScope(rows.remove(0).scope)))
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
            if file.summary.is_none() && summary.is_some() {
                file.summary = summary.map(str::to_string);
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
            summary: file.summary.clone(),
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

/// A scope holding comments whose commit is no longer under review. Named so
/// a caller cannot pass one where a live scope belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanScope(pub CommentScope);

/// `owner/repo` is a path in disguise; flatten it so the store stays one
/// directory deep.
fn sanitize_repo_key(key: &str) -> String {
    key.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => c,
            _ => '-',
        })
        .collect()
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

        let live = vec![CommentScope::commit("cccc3333")];
        let orphans = store.orphans_by_summary(&live).unwrap();

        assert_eq!(
            orphans.get("diff: look around"),
            Some(&OrphanScope(old)),
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
            .orphans_by_summary(&[CommentScope::commit("cccc3333")])
            .unwrap();

        assert!(
            orphans.is_empty(),
            "ambiguous is left out: a thread under the wrong commit is worse than one out of place"
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
