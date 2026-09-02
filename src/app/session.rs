use super::*;

/// How long an agent's "I am working on it" keeps animating. Past this nobody
/// has renewed the claim, and a spinner would go on promising progress that may
/// have died with the process that promised it.
pub const AGENT_WORKING_LIVE_SECS: i64 = 10 * 60;

/// What an agent last said about the review, ready to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentWorkingStatus {
    /// Still inside the window where the claim is worth animating.
    pub live: bool,
    pub elapsed_secs: i64,
    pub message: Option<String>,
    pub agent: Option<String>,
}

impl App {
    /// Slug for the currently active session, derived from the session's
    /// embedded fields. Returns `None` if derivation fails (e.g., a local
    /// session pointing at a non-existent path).
    ///
    /// Rendered on every frame by the status bar, so the repo's `origin`
    /// coordinate (the only I/O-bearing input: repo open + config parse) is
    /// resolved once and cached; the rest is rebuilt from the live session
    /// fields so commit re-scoping is still reflected.
    pub fn session_slug(&self) -> Option<String> {
        if self.session.pr_session_key.is_some() {
            return crate::persistence::storage::slug_for_session(&self.session)
                .ok()
                .map(|s| s.to_string());
        }
        let owner_repo = self
            .cached_owner_repo
            .get_or_init(|| crate::slug::resolve_owner_repo(&self.session.repo_path).ok())
            .clone()?;
        crate::persistence::storage::slug_for_session_with_owner_repo(&self.session, owner_repo)
            .ok()
            .map(|s| s.to_string())
    }

    pub fn set_review_watch_interval_ms(&mut self, interval_ms: u64) {
        if interval_ms == 0 {
            self.review_watch_interval = None;
        } else {
            let interval = Duration::from_millis(interval_ms);
            self.review_watch_interval = Some(interval);
            self.next_review_watch_at = Instant::now() + interval;
        }
    }

    pub(in crate::app) fn reset_persisted_session_tracking(&mut self) {
        self.session_path = crate::persistence::storage::session_path(&self.session).ok();
        self.session_file_state = self
            .session_path
            .as_deref()
            .filter(|path| path.exists())
            .and_then(|path| SessionFileState::from_path(path).ok());
        self.persisted_session_snapshot = self.session.clone();
        if let Err(e) = self.ensure_ephemeral_session_file() {
            self.set_warning(format!("Failed to initialize review session file: {e}"));
        }
    }

    fn mark_session_saved(&mut self, path: PathBuf, saved: ReviewSession) {
        self.session = saved.clone();
        self.persisted_session_snapshot = saved;
        self.session_path = Some(path.clone());
        self.session_file_state = SessionFileState::from_path(&path).ok();
        self.dirty = false;
    }

    pub fn ensure_ephemeral_session_file(&mut self) -> Result<Option<PathBuf>> {
        let path = match self.session_path.clone() {
            Some(path) => path,
            None => {
                let path = crate::persistence::storage::session_path(&self.session)?;
                self.session_path = Some(path.clone());
                path
            }
        };

        if path.exists() {
            if self.session.pr_session_key.is_some() {
                crate::persistence::storage::reindex_session(&self.session)?;
            }
            if self.ephemeral_session_paths.contains(&path) {
                let saved_path = self.save_current_session_merging_external()?;
                return Ok(Some(saved_path));
            }
            self.session_file_state = SessionFileState::from_path(&path).ok();
            self.mark_current_session_active_at(&path);
            return Ok(None);
        }

        let saved_path = self.save_current_session_merging_external()?;
        self.ephemeral_session_paths.insert(saved_path.clone());
        Ok(Some(saved_path))
    }

    pub fn cleanup_empty_ephemeral_sessions(&mut self) -> Result<usize> {
        let mut deleted = 0;
        for path in self.ephemeral_session_paths.clone() {
            if crate::persistence::storage::delete_session_if_empty(&path)? {
                deleted += 1;
                if self.session_path.as_ref() == Some(&path) {
                    self.session_file_state = None;
                }
            }
            self.ephemeral_session_paths.remove(&path);
        }
        Ok(deleted)
    }

    pub fn clear_active_session_marker(&mut self) -> Result<()> {
        crate::persistence::storage::clear_active_session_for_pid()
    }

    /// Discard this session's persisted state and quit.
    ///
    /// Used when the only unsaved change is reviewed-file markers (no
    /// comments): rather than forcing `:q!`, we drop the persisted session so
    /// reopening starts clean, then quit. Reviewed markers are persisted
    /// eagerly, so the on-disk file is removed too.
    pub fn discard_session_and_quit(&mut self) {
        let path = self
            .session_path
            .clone()
            .or_else(|| crate::persistence::storage::session_path(&self.session).ok());
        if let Some(path) = path {
            let _ = crate::persistence::storage::delete_session(&path);
            self.ephemeral_session_paths.remove(&path);
            if self.session_path.as_ref() == Some(&path) {
                self.session_file_state = None;
            }
        }
        self.dirty = false;
        self.should_quit = true;
    }

    pub fn save_current_session_merging_external(&mut self) -> Result<PathBuf> {
        let identity = self.session.clone();
        let current = self.session.clone();
        let base = self.persisted_session_snapshot.clone();
        let (path, saved, _changed) =
            crate::persistence::storage::save_session_by_identity(&identity, |persisted| {
                let mut merged = current.clone();
                if let Some(latest) = persisted.as_ref() {
                    Self::merge_external_session_changes(&mut merged, &base, latest);
                }
                merged.updated_at = Utc::now();
                Ok((merged, ()))
            })?;
        self.mark_session_saved(path.clone(), saved);
        self.mark_current_session_active_at(&path);
        self.rebuild_annotations();
        Ok(path)
    }

    /// Re-resolve this review's revision expression and adopt whatever the
    /// branch holds now, carrying the session — comments and all — across the
    /// move. Returns the number of commits now under review.
    ///
    /// `commit_range` stores resolved SHAs, so a review whose commits were
    /// amended away reloads into an identical diff: the old objects still
    /// exist, they are simply no longer the branch. Re-running the expression
    /// is the only way back. The session file is keyed by the range, so
    /// adopting a new one also has to move the file, or the comments would be
    /// left behind under the old key.
    pub fn resolve_revset_to_current_commits(&mut self) -> Result<usize> {
        let Some(revset) = self.session.revset.clone() else {
            return Err(TuicrError::InvalidInput(
                "this review was opened from the commit selector, so there is no \
                 revision expression to re-run — reopen it to pick up new commits"
                    .to_string(),
            ));
        };

        let range = self.vcs.resolve_revision_range(&revset)?;
        let commits = range.commit_ids.to_vec();
        if commits == self.session.commit_range.clone().unwrap_or_default() {
            return Ok(commits.len());
        }

        // Save under the old key first: if anything below fails, the comments
        // are still on disk where they were.
        let previous_path = self.save_current_session_merging_external()?;

        self.session.commit_range = Some(commits.clone());
        Self::clear_stale_commit_scopes(&mut self.session, &commits);
        self.diff_source = DiffSource::CommitRange(commits.clone());
        // `commit_ids` resolves oldest-first; `review_commits` stores
        // newest-first (display mirrors it for `commit_order = ascending`),
        // so reverse here as every load path does — or the pane comes back
        // from a reload upside down.
        self.review_commits = self
            .vcs
            .get_commits_info(&commits)
            .unwrap_or_default()
            .into_iter()
            .rev()
            .collect();
        self.commit_selection_range = None;

        // Load the new diff and re-anchor *before* saving. Adopting a range
        // without it writes the comments back at coordinates that belonged to
        // the old commits: a comment then sits on whatever code took its line,
        // looking untouched. Leaving this to a later reload made it depend on
        // the caller, and the one caller that mattered ran it too late.
        let reanchored = self.reload_diff_files();

        let new_path = self.save_current_session_merging_external()?;
        if new_path != previous_path {
            // The review moved to a new key; drop the copy left under the old
            // one so `review list` shows this review once, at its new range.
            crate::persistence::storage::delete_session(&previous_path)?;
        }
        // Report a diff failure only after the move is complete, so a broken
        // fetch cannot leave the review split across two keys.
        reanchored?;
        Ok(commits.len())
    }

    fn mark_current_session_active_at(&mut self, path: &Path) {
        if let Err(e) = crate::persistence::storage::mark_session_active(&self.session, path) {
            self.set_warning(format!("Failed to mark active review session: {e}"));
        }
    }

    /// Returns `true` if visible state changed (external comments merged or a
    /// warning was raised) so the main loop can schedule a redraw without an
    /// input event.
    pub fn poll_persisted_session_changes(&mut self) -> bool {
        let Some(interval) = self.review_watch_interval else {
            return false;
        };
        let now = Instant::now();
        if now < self.next_review_watch_at {
            return false;
        }
        self.next_review_watch_at = now + interval;

        // Do not mutate the session while the user is composing or editing a
        // comment. The next poll after the editor closes will merge changes.
        if self.input_mode == InputMode::Comment {
            return false;
        }

        let merged = match self.reload_persisted_session_if_changed(false) {
            Ok(0) => false,
            Ok(_) => true,
            Err(err) => {
                self.set_warning(format!("Review reload failed: {err}"));
                true
            }
        };
        self.announce_pending_agent_update() || merged
    }

    /// Surface an agent's "I changed the code" announcement, once, when the
    /// reviewer is not mid-comment. It names `:reload` because the diff on
    /// screen is stale until they do — the whole point of the announcement.
    #[cfg(test)]
    pub(in crate::app) fn poll_agent_update_for_test(&mut self) -> bool {
        self.announce_pending_agent_update()
    }

    fn announce_pending_agent_update(&mut self) -> bool {
        if self.input_mode == InputMode::Comment {
            return false;
        }
        let Some(update) = self.pending_agent_update.take() else {
            return false;
        };
        let message = match update.message.as_deref() {
            Some(text) => format!("Agent: {text} · :reload to see it"),
            None => "Agent changed the code under review · :reload to see it".to_string(),
        };
        self.set_warning(message);
        true
    }

    /// What an agent last said it was doing on this review, if anything.
    ///
    /// The claim comes from a file another process writes, so it can outlive
    /// the agent that made it. Callers get the age with it and say so rather
    /// than implying the work is still running.
    pub fn agent_working_status(&self) -> Option<AgentWorkingStatus> {
        let activity = self.session.agent_working.as_ref()?;
        let elapsed = chrono::Utc::now()
            .signed_duration_since(activity.at)
            .num_seconds()
            .max(0);
        Some(AgentWorkingStatus {
            live: elapsed < AGENT_WORKING_LIVE_SECS,
            elapsed_secs: elapsed,
            message: activity.message.clone(),
            agent: activity.agent.clone(),
        })
    }

    /// True while the agent indicator is still animating, so the main loop
    /// knows to keep redrawing. It stops once the claim goes quiet: an idle
    /// redraw rebuilds every line of the diff, and this field can linger.
    pub fn agent_spinner_running(&self) -> bool {
        self.agent_working_status()
            .is_some_and(|status| status.live)
    }

    /// True while any forge background fetch (PR list/open/reload/threads/
    /// submit) is in flight. Used by the main loop to keep redrawing so
    /// spinners animate and results land without waiting for input.
    pub fn has_pending_pr_work(&self) -> bool {
        self.pr_load_rx.is_some()
            || self.pr_open_rx.is_some()
            || self.pr_reload_rx.is_some()
            || self.pr_range_reload_rx.is_some()
            || self.pr_threads_rx.is_some()
            || self.pr_submit_rx.is_some()
            || self.pr_reply_rx.is_some()
    }

    pub fn reload_persisted_session_if_changed(&mut self, force: bool) -> Result<usize> {
        let path = match self.session_path.clone() {
            Some(path) => path,
            None => match crate::persistence::storage::session_path(&self.session) {
                Ok(path) => {
                    self.session_path = Some(path.clone());
                    path
                }
                Err(_) => return Ok(0),
            },
        };

        if !path.exists() {
            self.session_file_state = None;
            return Ok(0);
        }

        let state = SessionFileState::from_path(&path)?;
        if !force && self.session_file_state == Some(state) {
            return Ok(0);
        }

        let latest = crate::persistence::storage::load_session(&path)?;
        // An agent announcing that it changed the code is not a comment merge:
        // it needs saying out loud, because the diff on screen is now stale and
        // nothing else would tell the reader.
        if latest.agent_update.is_some() && latest.agent_update != self.session.agent_update {
            self.session.agent_update = latest.agent_update.clone();
            self.pending_agent_update = latest.agent_update.clone();
        }
        // An agent saying it is working, or has stopped, only ever comes from
        // outside: the file is the whole truth for this field, including when
        // it goes back to `None`. Taken before the comment merge so the next
        // save carries what the file says rather than the state this process
        // started with.
        self.session.agent_working = latest.agent_working.clone();
        let before_count = Self::comment_count(&self.session);
        let changed = Self::merge_external_session_changes(
            &mut self.session,
            &self.persisted_session_snapshot,
            &latest,
        );
        self.persisted_session_snapshot = latest;
        self.session_file_state = Some(state);
        if changed > 0 {
            self.rebuild_annotations();
        }
        let after_count = Self::comment_count(&self.session);
        Ok(after_count.saturating_sub(before_count))
    }

    pub(in crate::app) fn merge_external_session_changes(
        current: &mut ReviewSession,
        base: &ReviewSession,
        latest: &ReviewSession,
    ) -> usize {
        let mut changed = 0;

        // A comment that moved to another file is still here, just not where
        // the file on disk has it. Taking that file back wholesale would bring
        // a second copy of every comment in it, which is how a re-anchored
        // thread came back doubled under the path it had left.
        let held: std::collections::HashSet<String> =
            Self::collect_stored_comments(current).into_keys().collect();

        for (path, latest_review) in &latest.files {
            if !current.files.contains_key(path) {
                let mut adopted = latest_review.clone();
                adopted.file_comments.retain(|c| !held.contains(&c.id));
                adopted.line_comments.retain(|_, comments| {
                    comments.retain(|c| !held.contains(&c.id));
                    !comments.is_empty()
                });
                // What is left may be nothing at all: a file whose comments all
                // moved away is a husk, and re-adding it every save would undo
                // the tidying that moved them.
                if adopted.comment_count() == 0
                    && !adopted.reviewed
                    && adopted.reviewed_hunks.is_empty()
                {
                    continue;
                }
                changed += adopted.comment_count();
                current.files.insert(path.clone(), adopted);
                continue;
            }

            let base_reviewed = base.files.get(path).map(|review| review.reviewed);
            if let Some(current_review) = current.files.get_mut(path)
                && Some(current_review.reviewed) == base_reviewed
                && current_review.reviewed != latest_review.reviewed
            {
                current_review.reviewed = latest_review.reviewed;
                changed += 1;
            }
        }

        let base_comments = Self::collect_stored_comments(base);
        let current_comments = Self::collect_stored_comments(current);
        let latest_comments = Self::collect_stored_comments(latest);

        for (id, latest_comment) in &latest_comments {
            match base_comments.get(id) {
                None => {
                    if !current_comments.contains_key(id) {
                        Self::upsert_stored_comment(current, latest_comment.clone());
                        changed += 1;
                    }
                }
                Some(base_comment) if latest_comment != base_comment => {
                    match current_comments.get(id) {
                        Some(current_comment) if current_comment == base_comment => {
                            Self::upsert_stored_comment(current, latest_comment.clone());
                            changed += 1;
                        }
                        None => {
                            // Local deletion wins over an external edit.
                        }
                        Some(_) => {
                            // Local edit wins over an external edit of the same comment.
                        }
                    }
                }
                Some(_) => {}
            }
        }

        for (id, base_comment) in &base_comments {
            if !latest_comments.contains_key(id)
                && current_comments
                    .get(id)
                    .is_some_and(|current_comment| current_comment == base_comment)
                && Self::remove_stored_comment(current, id)
            {
                changed += 1;
            }
        }

        changed
    }

    fn comment_count(session: &ReviewSession) -> usize {
        session.review_comments.len()
            + session
                .files
                .values()
                .map(|review| review.comment_count())
                .sum::<usize>()
    }

    fn collect_stored_comments(session: &ReviewSession) -> HashMap<String, StoredComment> {
        let mut comments = HashMap::new();
        for comment in &session.review_comments {
            comments.insert(
                comment.id.clone(),
                StoredComment {
                    location: StoredCommentLocation::Review,
                    comment: comment.clone(),
                },
            );
        }

        for (path, review) in &session.files {
            for comment in &review.file_comments {
                comments.insert(
                    comment.id.clone(),
                    StoredComment {
                        location: StoredCommentLocation::File { path: path.clone() },
                        comment: comment.clone(),
                    },
                );
            }

            for (line, line_comments) in &review.line_comments {
                for comment in line_comments {
                    comments.insert(
                        comment.id.clone(),
                        StoredComment {
                            location: StoredCommentLocation::Line {
                                path: path.clone(),
                                line: *line,
                            },
                            comment: comment.clone(),
                        },
                    );
                }
            }
        }

        comments
    }

    fn upsert_stored_comment(session: &mut ReviewSession, stored: StoredComment) {
        Self::remove_stored_comment(session, &stored.comment.id);
        match stored.location {
            StoredCommentLocation::Review => {
                session.review_comments.push(stored.comment);
            }
            StoredCommentLocation::File { path } => {
                if let Some(review) = session.files.get_mut(&path) {
                    review.file_comments.push(stored.comment);
                }
            }
            StoredCommentLocation::Line { path, line } => {
                if let Some(review) = session.files.get_mut(&path) {
                    review
                        .line_comments
                        .entry(line)
                        .or_default()
                        .push(stored.comment);
                }
            }
        }
    }

    fn remove_stored_comment(session: &mut ReviewSession, id: &str) -> bool {
        if let Some(index) = session
            .review_comments
            .iter()
            .position(|comment| comment.id == id)
        {
            session.review_comments.remove(index);
            return true;
        }

        for review in session.files.values_mut() {
            if let Some(index) = review
                .file_comments
                .iter()
                .position(|comment| comment.id == id)
            {
                review.file_comments.remove(index);
                return true;
            }

            let mut emptied_line = None;
            for (line, comments) in &mut review.line_comments {
                if let Some(index) = comments.iter().position(|comment| comment.id == id) {
                    comments.remove(index);
                    if comments.is_empty() {
                        emptied_line = Some(*line);
                    }
                    break;
                }
            }
            if let Some(line) = emptied_line {
                review.line_comments.remove(&line);
                return true;
            }
        }

        false
    }

    /// Load or create a session for a commit range (used by revisions and commit selection).
    /// Drop commit scoping that a rewrite invalidated.
    ///
    /// A comment made while one commit was selected records that SHA, and
    /// `comment_visible` hides comments whose commit is not in the current
    /// selection. Amend the branch and every one of those SHAs is gone, so the
    /// comments vanish from the diff — not deleted, not outdated, just filtered
    /// out with nothing said. The commit it named no longer exists; the comment
    /// is about the code, so it becomes unscoped rather than invisible.
    ///
    /// Returns how many were unscoped.
    pub(in crate::app) fn clear_stale_commit_scopes(
        session: &mut ReviewSession,
        commits: &[String],
    ) -> usize {
        let live: std::collections::HashSet<&str> = commits.iter().map(String::as_str).collect();
        let mut cleared = 0;
        let mut unscope = |comment: &mut Comment| {
            if let Some(id) = comment.commit_id.as_deref()
                && !live.contains(id)
            {
                // Keep it as provenance before dropping it as scoping: it is
                // the only handle on the code this comment was written about
                // once the commit leaves the review.
                if let Some(context) = comment.line_context.as_mut()
                    && context.commit.is_none()
                {
                    context.commit = Some(id.to_string());
                }
                comment.commit_id = None;
                cleared += 1;
            }
        };
        for comment in &mut session.review_comments {
            unscope(comment);
        }
        for review in session.files.values_mut() {
            for comment in &mut review.file_comments {
                unscope(comment);
            }
            for comments in review.line_comments.values_mut() {
                for comment in comments {
                    unscope(comment);
                }
            }
        }
        cleared
    }

    /// Take over a review opened with the same expression whose commits have
    /// since been rewritten: re-key it to the current range and drop the copy
    /// under the old one.
    ///
    /// The new file is written before the old one is removed, so a failure
    /// anywhere leaves the comments on disk under the key they already had.
    fn adopt_session_for_revset(
        vcs_info: &VcsInfo,
        commit_ids: &[String],
        revset: &str,
    ) -> Option<ReviewSession> {
        let (previous_path, mut session) =
            crate::persistence::storage::find_local_session_by_revset(&vcs_info.root_path, revset)
                .ok()
                .flatten()?;
        if session.commit_range.as_deref() == Some(commit_ids) {
            return Some(session);
        }
        session.commit_range = Some(commit_ids.to_vec());
        session.base_commit = commit_ids.last()?.clone();
        Self::clear_stale_commit_scopes(&mut session, commit_ids);
        let saved = crate::persistence::storage::save_session(&session).ok()?;
        if saved != previous_path {
            let _ = crate::persistence::storage::delete_session(&previous_path);
        }
        Some(session)
    }

    pub(in crate::app) fn load_or_create_commit_range_session(
        vcs_info: &VcsInfo,
        commit_ids: &[String],
        revset: Option<&str>,
    ) -> ReviewSession {
        let newest_commit_id = commit_ids.last().unwrap().clone();
        let loaded = load_latest_session_for_context(
            &vcs_info.root_path,
            vcs_info.branch_name.as_deref(),
            &newest_commit_id,
            SessionDiffSource::CommitRange,
            Some(commit_ids),
        )
        .ok()
        .and_then(|found| found.map(|(_path, session)| session))
        // Nothing under this exact range: the commits may have been rewritten
        // since. The expression is the durable name for a review, so try that
        // before starting an empty one and stranding the comments.
        .or_else(|| {
            revset.and_then(|revset| Self::adopt_session_for_revset(vcs_info, commit_ids, revset))
        });

        let mut session = loaded.unwrap_or_else(|| {
            let mut s = ReviewSession::new(
                vcs_info.root_path.clone(),
                newest_commit_id,
                vcs_info.branch_name.clone(),
                SessionDiffSource::CommitRange,
            );
            s.commit_range = Some(commit_ids.to_vec());
            s
        });

        if session.commit_range.is_none() {
            session.commit_range = Some(commit_ids.to_vec());
            session.updated_at = chrono::Utc::now();
        }
        session
    }

    pub(in crate::app) fn load_or_create_staged_unstaged_and_commits_session(
        vcs_info: &VcsInfo,
        commit_ids: &[String],
    ) -> ReviewSession {
        let newest_commit_id = commit_ids.last().unwrap().clone();
        let loaded = load_latest_session_for_context(
            &vcs_info.root_path,
            vcs_info.branch_name.as_deref(),
            &newest_commit_id,
            SessionDiffSource::StagedUnstagedAndCommits,
            Some(commit_ids),
        )
        .ok()
        .and_then(|found| found.map(|(_path, session)| session));

        let mut session = loaded.unwrap_or_else(|| {
            let mut s = ReviewSession::new(
                vcs_info.root_path.clone(),
                newest_commit_id,
                vcs_info.branch_name.clone(),
                SessionDiffSource::StagedUnstagedAndCommits,
            );
            s.commit_range = Some(commit_ids.to_vec());
            s
        });

        if session.commit_range.is_none() {
            session.commit_range = Some(commit_ids.to_vec());
            session.updated_at = chrono::Utc::now();
        }
        session
    }

    pub(in crate::app) fn load_or_create_session(
        vcs_info: &VcsInfo,
        diff_source: SessionDiffSource,
    ) -> ReviewSession {
        let new_session = || {
            ReviewSession::new(
                vcs_info.root_path.clone(),
                vcs_info.head_commit.clone(),
                vcs_info.branch_name.clone(),
                diff_source,
            )
        };

        let Ok(found) = load_latest_session_for_context(
            &vcs_info.root_path,
            vcs_info.branch_name.as_deref(),
            &vcs_info.head_commit,
            diff_source,
            None,
        ) else {
            return new_session();
        };

        let Some((_path, mut session)) = found else {
            return new_session();
        };

        let mut updated = false;
        if session.branch_name.is_none() && vcs_info.branch_name.is_some() {
            session.branch_name = vcs_info.branch_name.clone();
            updated = true;
        }

        if vcs_info.branch_name.is_some() && session.base_commit != vcs_info.head_commit {
            session.base_commit = vcs_info.head_commit.clone();
            updated = true;
        }

        if updated {
            session.updated_at = chrono::Utc::now();
        }

        session
    }

    /// Materialize a PR session from an already-opened PR. Reattaches the
    /// most recent persisted session for the same head SHA when present so
    /// reviewed markers and local comments survive a reopen.
    fn load_pr_session_for_opened(
        opened: &crate::forge::pr_open::OpenedPullRequest,
    ) -> Result<Option<ReviewSession>> {
        let key = opened.key.clone();
        let mut persisted = match crate::persistence::load_pr_session(&key)? {
            Some((_path, persisted)) if persisted.pr_session_key.as_ref() == Some(&key) => {
                persisted
            }
            _ => {
                let path = crate::persistence::storage::session_path(&opened.session)?;
                if !path.exists() {
                    return Ok(None);
                }
                let persisted = crate::persistence::storage::load_session(&path).map_err(|e| {
                    TuicrError::CorruptedSession(format!(
                        "failed to load PR session {}: {e}",
                        path.display()
                    ))
                })?;
                if persisted.pr_session_key.as_ref() != Some(&key) {
                    return Ok(None);
                }
                persisted
            }
        };

        // Re-register diff files against the loaded session so any new files
        // in the PR appear with content_hash tracking, and any deleted files
        // simply stop appearing in the file list.
        // Strict subset sessions are reloaded through a full PR diff first, so
        // pruning here would discard hunk keys hidden by the active selector.
        let preserve_hunks = Self::is_strict_commit_selection(
            persisted.commit_selection_range,
            opened.commits.len(),
        );
        Self::register_diff_files(&mut persisted, &opened.diff_files, preserve_hunks);
        Ok(Some(ReviewSession {
            pr_session_key: Some(key),
            diff_source: SessionDiffSource::PullRequest,
            updated_at: chrono::Utc::now(),
            ..persisted
        }))
    }

    pub(in crate::app) fn opened_pr_with_persisted_session(
        opened: crate::forge::pr_open::OpenedPullRequest,
    ) -> Result<crate::forge::pr_open::OpenedPullRequest> {
        match Self::load_pr_session_for_opened(&opened)? {
            Some(session) => Ok(crate::forge::pr_open::OpenedPullRequest { session, ..opened }),
            None => Ok(opened),
        }
    }

    pub(in crate::app) fn opened_pr_with_new_head_session(
        &mut self,
        opened: crate::forge::pr_open::OpenedPullRequest,
    ) -> Result<crate::forge::pr_open::OpenedPullRequest> {
        self.save_current_session_merging_external()?;
        let previous_session = self.session.clone();
        let session = match Self::load_pr_session_for_opened(&opened)? {
            Some(session) => session,
            None => Self::reviewed_state_carried_forward(
                &previous_session,
                opened.session.clone(),
                &opened.diff_files,
            ),
        };
        Ok(crate::forge::pr_open::OpenedPullRequest { session, ..opened })
    }

    pub(in crate::app) fn reviewed_state_carried_forward(
        previous: &ReviewSession,
        next: ReviewSession,
        diff_files: &[DiffFile],
    ) -> ReviewSession {
        let file_by_path: HashMap<_, _> = diff_files
            .iter()
            .map(|file| (file.display_path().clone(), file))
            .collect();
        let files = next
            .files
            .into_iter()
            .map(|(path, review)| {
                Self::file_review_carried_forward(path, review, previous, &file_by_path)
            })
            .collect();
        let review_comments = previous
            .review_comments
            .iter()
            .filter(|comment| !comment.is_locked())
            .cloned()
            .collect();

        ReviewSession {
            files,
            review_comments,
            ..next
        }
    }

    fn file_review_carried_forward(
        path: PathBuf,
        review: FileReview,
        previous: &ReviewSession,
        file_by_path: &HashMap<PathBuf, &DiffFile>,
    ) -> (PathBuf, FileReview) {
        let Some(file) = file_by_path.get(&path) else {
            return (path, review);
        };
        let Some(previous_review) = previous.files.get(&path) else {
            return (path, review);
        };

        let unchanged_file = previous_review.content_hash == Some(file.content_hash);
        let valid_hunks: HashSet<_> = file.hunk_review_keys().into_iter().collect();
        let reviewed_hunks = previous_review
            .reviewed_hunks
            .iter()
            .filter(|key| valid_hunks.contains(*key))
            .cloned()
            .collect();
        let (file_comments, line_comments) = if unchanged_file {
            (
                previous_review
                    .file_comments
                    .iter()
                    .filter(|comment| !comment.is_locked())
                    .cloned()
                    .collect(),
                Self::line_draft_comments_carried_forward(previous_review),
            )
        } else {
            (review.file_comments, review.line_comments)
        };

        (
            path,
            FileReview {
                reviewed: unchanged_file && previous_review.reviewed,
                reviewed_hunks,
                file_comments,
                line_comments,
                ..review
            },
        )
    }

    fn line_draft_comments_carried_forward(
        previous_review: &FileReview,
    ) -> HashMap<u32, Vec<Comment>> {
        previous_review
            .line_comments
            .iter()
            .filter_map(|(line, comments)| {
                let drafts: Vec<_> = comments
                    .iter()
                    .filter(|comment| !comment.is_locked())
                    .cloned()
                    .collect();
                (!drafts.is_empty()).then_some((*line, drafts))
            })
            .collect()
    }
}

impl App {
    /// The comment store for this review's repository, and the key it lives
    /// under. `None` when the repo has no coordinate to key on.
    pub(in crate::app) fn comment_store(
        &self,
    ) -> Option<crate::persistence::comment_store::CommentStore> {
        let reviews_dir = crate::persistence::storage::get_reviews_dir().ok()?;
        let key = match self.session.pr_session_key.as_ref() {
            Some(pr) => format!("{}/{}", pr.repository.owner, pr.repository.name),
            None => {
                let (owner, repo) = self
                    .cached_owner_repo
                    .get_or_init(|| crate::slug::resolve_owner_repo(&self.session.repo_path).ok())
                    .clone()?;
                match owner {
                    Some(owner) => format!("{owner}/{repo}"),
                    None => repo,
                }
            }
        };
        Some(crate::persistence::comment_store::CommentStore::new(
            reviews_dir,
            &key,
        ))
    }

    /// The commits this review has in view, with their summaries — what the
    /// store is asked about.
    pub(in crate::app) fn review_scopes(&self) -> Vec<(crate::model::CommentScope, String)> {
        use crate::model::review::SessionDiffSource as Source;
        let source = self.session.diff_source;
        let has_commits = matches!(
            source,
            Source::CommitRange
                | Source::WorkingTreeAndCommits
                | Source::StagedUnstagedAndCommits
                | Source::PullRequest
        );
        // A review of uncommitted work asks about its checkout, and one that
        // shows both asks about both — deciding by "are there commits" instead
        // made working-tree comments unreachable the moment a review also had
        // commits in it.
        let has_working_tree = matches!(
            source,
            Source::WorkingTree
                | Source::Staged
                | Source::Unstaged
                | Source::StagedAndUnstaged
                | Source::WorkingTreeAndCommits
                | Source::StagedUnstagedAndCommits
        );

        let mut scopes = Vec::new();
        if has_commits {
            scopes.extend(self.review_commits.iter().map(|commit| {
                (
                    crate::model::CommentScope::commit(commit.id.clone()),
                    commit.summary.clone(),
                )
            }));
        }
        if has_working_tree {
            scopes.push((
                crate::model::CommentScope::working_tree(
                    crate::persistence::comment_store::checkout_key(&self.session.repo_path),
                ),
                String::new(),
            ));
        }
        scopes
    }

    /// Fill the in-memory review from the comment store: the comments on this
    /// review's commits, on earlier versions of them, and whatever a summary
    /// claims.
    ///
    /// A store with nothing for this repository leaves the session untouched,
    /// so a review opened before the migration keeps showing what it always
    /// showed. Migration stays something the reader asks for.
    pub(in crate::app) fn hydrate_comments_from_store(&mut self) {
        let Some(store) = self.comment_store() else {
            return;
        };
        // Before a repository is migrated the store is not consulted at all,
        // and writes keep going to the session. After it, the store answers
        // for both. There is no state in between to get wrong.
        if !store.in_use() {
            return;
        }
        self.comments_in_store = true;
        let live = self.review_scopes();
        let shas: Vec<String> = live
            .iter()
            .filter_map(|(s, _)| s.sha().map(String::from))
            .collect();
        let predecessors = self
            .vcs
            .predecessors(&shas)
            .unwrap_or_default()
            .into_iter()
            .collect();

        let resolved = match crate::persistence::comment_store::resolve_for_review(
            &store,
            &live,
            &predecessors,
        ) {
            Ok(resolved) => resolved,
            Err(_) => return,
        };
        // Drop only the session's copies of what the store just handed back.
        // Clearing the buckets wholesale would take comments the store does
        // not have with them — anything written since the migration — and the
        // next save would write that loss to disk without a word, since the
        // merge compares against the session as it was before hydration and
        // sees nothing missing.
        self.merge_stored_comments(resolved);
    }

    #[cfg(test)]
    pub(in crate::app) fn merge_stored_comments_for_test(
        &mut self,
        resolved: crate::persistence::comment_store::ResolvedComments,
    ) {
        self.merge_stored_comments(resolved);
    }

    /// Replace the session's copies of what the store returned, and place the
    /// stored versions where the renderers look.
    fn merge_stored_comments(
        &mut self,
        resolved: crate::persistence::comment_store::ResolvedComments,
    ) {
        let stored: std::collections::HashSet<String> =
            resolved.comments.iter().map(|c| c.id.clone()).collect();
        self.session
            .review_comments
            .retain(|c| !stored.contains(&c.id));
        for review in self.session.files.values_mut() {
            review.file_comments.retain(|c| !stored.contains(&c.id));
            review.line_comments.retain(|_, comments| {
                comments.retain(|c| !stored.contains(&c.id));
                !comments.is_empty()
            });
        }
        self.comments_from_earlier = resolved.from_earlier.clone();
        for mut comment in resolved.comments {
            // A carried comment is shown under the commit its own became, so
            // narrowing the review to that commit still shows it. Its anchor
            // is untouched: what it was written on is a fact, and only the
            // copy in memory says where that commit went.
            if let Some(under) = resolved.shown_under.get(&comment.id) {
                comment.commit_id = Some(under.clone());
            }
            self.place_stored_comment(comment);
        }
    }

    #[cfg(test)]
    pub(in crate::app) fn place_stored_comment_for_test(&mut self, comment: crate::model::Comment) {
        self.place_stored_comment(comment);
    }

    /// Put a stored comment where the renderers look for it, from its anchor.
    fn place_stored_comment(&mut self, comment: crate::model::Comment) {
        let Some(anchor) = comment.anchor.clone() else {
            self.session.review_comments.push(comment);
            return;
        };
        let Some(path) = anchor.path else {
            self.session.review_comments.push(comment);
            return;
        };
        let review = self.session.files.entry(path.clone()).or_insert_with(|| {
            crate::model::review::FileReview::new(path, crate::model::FileStatus::Modified, 0)
        });
        match anchor.line {
            Some(line) => review.line_comments.entry(line).or_default().push(comment),
            None => review.file_comments.push(comment),
        }
    }
}

impl App {
    /// The commit a comment written now belongs to: the one under review when
    /// the selector is narrowed to one, the head of the range otherwise, and
    /// the checkout when nothing is committed yet.
    pub(in crate::app) fn scope_for_new_comment(&self) -> crate::model::CommentScope {
        if let Some(sha) = self.commit_id_for_new_comment() {
            return crate::model::CommentScope::commit(sha);
        }
        match self.review_commits.last() {
            Some(head) => crate::model::CommentScope::commit(head.id.clone()),
            None => crate::model::CommentScope::working_tree(
                crate::persistence::comment_store::checkout_key(&self.session.repo_path),
            ),
        }
    }

    /// The message of the commit a new comment is filed under, so the store
    /// can find the thread again after that commit is rewritten.
    fn message_for_new_comment(&self, scope: &crate::model::CommentScope) -> Option<String> {
        let sha = scope.sha()?;
        self.review_commits
            .iter()
            .find(|commit| commit.id == sha)
            .map(|commit| match commit.body.as_ref() {
                Some(body) => format!("{}\n\n{body}", commit.summary),
                None => commit.summary.clone(),
            })
    }

    /// Write a newly created comment to the store, stamping the anchor that
    /// says what it was written against.
    pub(in crate::app) fn store_comment(&mut self, id: &str) {
        if !self.comments_in_store {
            return;
        }
        let Some(store) = self.comment_store() else {
            return;
        };
        let Some((scope, comment)) = self.anchored_copy(id) else {
            return;
        };
        let message = self.message_for_new_comment(&scope);
        if let Err(e) = store.add(&scope, message.as_deref(), comment) {
            self.set_warning(format!("Could not save the comment: {e}"));
        }
    }

    /// Apply the in-memory state of `id` to the stored copy.
    pub(in crate::app) fn store_comment_update(&mut self, id: &str) {
        if !self.comments_in_store {
            return;
        }
        let Some(store) = self.comment_store() else {
            return;
        };
        let Some((_, updated)) = self.anchored_copy(id) else {
            return;
        };
        let applied = store.update_comment(id, |stored| {
            stored.content = updated.content.clone();
            stored.comment_type = updated.comment_type.clone();
            stored.resolved = updated.resolved;
            stored.outdated = updated.outdated;
        });
        if let Err(e) = applied {
            self.set_warning(format!("Could not update the comment: {e}"));
        }
    }

    /// Apply the in-memory state of every comment in `id`'s thread.
    pub(in crate::app) fn store_thread_update(&mut self, id: &str) {
        if !self.comments_in_store {
            return;
        }
        let root = self
            .session
            .find_comment(id)
            .map(|c| c.in_reply_to.clone().unwrap_or_else(|| c.id.clone()));
        let Some(root) = root else { return };
        let members: Vec<String> = self
            .all_comments()
            .filter(|c| c.id == root || c.in_reply_to.as_deref() == Some(root.as_str()))
            .map(|c| c.id.clone())
            .collect();
        for member in members {
            self.store_comment_update(&member);
        }
    }

    /// Remove a thread from the store.
    pub(in crate::app) fn store_thread_delete(&mut self, id: &str) {
        if !self.comments_in_store {
            return;
        }
        let Some(store) = self.comment_store() else {
            return;
        };
        if let Err(e) = store.delete_thread(id) {
            self.set_warning(format!("Could not delete the comment: {e}"));
        }
    }

    fn all_comments(&self) -> impl Iterator<Item = &crate::model::Comment> {
        self.session
            .review_comments
            .iter()
            .chain(self.session.files.values().flat_map(|review| {
                review
                    .file_comments
                    .iter()
                    .chain(review.line_comments.values().flatten())
            }))
    }

    /// The in-memory comment with `id`, carrying an anchor: the one it already
    /// has, or one built from where it sits now.
    fn anchored_copy(
        &self,
        id: &str,
    ) -> Option<(crate::model::CommentScope, crate::model::Comment)> {
        let scope = self.scope_for_new_comment();
        if let Some(comment) = self.session.review_comments.iter().find(|c| c.id == id) {
            let mut comment = comment.clone();
            let anchor = comment
                .anchor
                .clone()
                .unwrap_or_else(|| crate::model::CommentAnchor::review(scope.clone()));
            let scope = anchor.scope.clone();
            comment.anchor = Some(anchor);
            return Some((scope, comment));
        }
        for (path, review) in &self.session.files {
            if let Some(comment) = review.file_comments.iter().find(|c| c.id == id) {
                let mut comment = comment.clone();
                let anchor = comment.anchor.clone().unwrap_or_else(|| {
                    crate::model::CommentAnchor::file(scope.clone(), path.clone())
                });
                let scope = anchor.scope.clone();
                comment.anchor = Some(anchor);
                return Some((scope, comment));
            }
            for (line, comments) in &review.line_comments {
                if let Some(comment) = comments.iter().find(|c| c.id == id) {
                    let mut comment = comment.clone();
                    let anchor = comment.anchor.clone().unwrap_or_else(|| {
                        crate::model::CommentAnchor::line(
                            scope.clone(),
                            path.clone(),
                            *line,
                            comment.side.unwrap_or(crate::model::LineSide::New),
                        )
                    });
                    let scope = anchor.scope.clone();
                    comment.anchor = Some(anchor);
                    return Some((scope, comment));
                }
            }
        }
        None
    }
}
