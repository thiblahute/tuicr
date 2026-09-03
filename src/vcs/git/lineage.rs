//! Earlier versions of a commit, read out of the reflog.
//!
//! A commit that is amended, rebased, or has a `fixup!` squashed into it comes
//! out with a new sha, and anything filed under the old one — review comments,
//! here — is stranded on a sha nothing points at. Git wrote down every one of
//! those rewrites as it happened; this reads them back.
//!
//! The reflog *files* are parsed rather than `git reflog` output: each line
//! already carries both the old and the new sha, so nothing has to be inferred
//! by pairing adjacent lines. Every file is read — `logs/HEAD`, the branch
//! logs, and one HEAD log per worktree — because a rewrite performed in
//! another worktree of the same repository is still a rewrite of these
//! commits.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::error::Result;

/// One reflog line.
struct Entry {
    old: String,
    new: String,
    message: String,
}

/// What a reflog entry did, keyed on the parenthesized verb rather than on the
/// literal `rebase (` prefix — `rebase -i (start)` and `pull --rebase (pick)`
/// are the same operations under other names.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Op {
    Amend,
    Start,
    Finish,
    Abort,
    Applied,
    /// `reword`: the subject the entry carries is the NEW one, so the slot
    /// holding the old subject can never match by name.
    Reword,
    Squashy,
    /// Moves HEAD without rewriting: `checkout:`, `reset:`.
    Move,
    Other,
}

const NULL_SHA: &str = "0000000000000000000000000000000000000000";

fn classify(message: &str) -> Op {
    if message.starts_with("commit (amend)") {
        return Op::Amend;
    }
    let verb = message
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(verb, _)| verb);
    match verb {
        Some("amend") => Op::Amend,
        Some("start") => Op::Start,
        Some("finish") => Op::Finish,
        Some("abort") => Op::Abort,
        Some("pick" | "continue" | "edit") => Op::Applied,
        Some("reword") => Op::Reword,
        Some("fixup" | "squash") => Op::Squashy,
        _ if message.starts_with("checkout:") || message.starts_with("reset:") => Op::Move,
        _ => Op::Other,
    }
}

/// Which of `of` are still reachable from a branch or tag.
///
/// `rev-parse` is the wrong question: a commit a rebase left behind resolves
/// for as long as its reflog entry lives, which is months. `for-each-ref
/// --contains` asks the one that matters — is anything still pointing at it.
pub fn reachable_from_refs(repo: &Path, of: &[String]) -> Result<HashSet<String>> {
    let mut live = HashSet::new();
    for sha in of {
        match git(
            repo,
            &[
                "for-each-ref",
                "--count=1",
                "--format=%(refname)",
                "--contains",
                sha,
            ],
        ) {
            // A commit git cannot answer about counts as live: declining to
            // claim its comments is the recoverable mistake, taking another
            // branch's thread is not.
            Err(_) => {
                live.insert(sha.clone());
            }
            Ok(refs) if !refs.trim().is_empty() => {
                live.insert(sha.clone());
            }
            Ok(_) => {}
        }
    }
    Ok(live)
}

/// The commits `of` were built from: every amend, rebase and squash that led
/// to them, transitively. Keyed by the sha asked about.
pub(crate) fn predecessors(repo: &Path, of: &[String]) -> Result<HashMap<String, Vec<String>>> {
    predecessors_cached(repo, of, cache_path_for(repo).as_deref())
}

/// Where this repository's edge map is kept: in tuicr's own data directory,
/// named after the repository, so nothing is written inside `.git` and a stale
/// file costs a rebuild rather than a wrong answer.
fn cache_path_for(repo: &Path) -> Option<PathBuf> {
    let reviews = crate::persistence::storage::get_reviews_dir().ok()?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in repo.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(
        reviews
            .join("cache")
            .join(format!("lineage-{hash:016x}.json")),
    )
}

/// As `predecessors`, reusing a cached edge map when `cache` names a file and
/// the reflogs have not moved since it was written.
///
/// The map does not depend on which commits are asked about, only on the
/// reflogs — so it survives between openings of a review, and rebuilding it
/// costs seconds on a repository with a few thousand entries.
pub(crate) fn predecessors_cached(
    repo: &Path,
    of: &[String],
    cache: Option<&Path>,
) -> Result<HashMap<String, Vec<String>>> {
    let common = common_dir(repo)?;
    let stamp = reflog_stamp(&common);

    if let Some(path) = cache
        && let Some(backward) = read_cache(path, &stamp)
    {
        return Ok(of
            .iter()
            .map(|sha| (sha.clone(), walk_back(sha, &backward)))
            .collect());
    }

    let backward = backward_map(repo, &common);
    if let Some(path) = cache {
        write_cache(path, &stamp, &backward);
    }
    Ok(of
        .iter()
        .map(|sha| (sha.clone(), walk_back(sha, &backward)))
        .collect())
}

/// Every rewrite the reflogs record, as edges from a commit to the one that
/// replaced it.
fn backward_map(repo: &Path, common: &Path) -> HashMap<String, Vec<String>> {
    let mut backward: HashMap<String, Vec<String>> = HashMap::new();

    // The same commit is asked about from several windows, and a subject
    // costs a process to fetch, so they are remembered for the whole run.
    let mut subjects = HashMap::new();
    let logs: Vec<(PathBuf, bool)> = reflog_files(common);
    let read: Vec<(Vec<Entry>, bool)> = logs
        .iter()
        .map(|(path, is_head_log)| (read_entries(path.as_path()), *is_head_log))
        .collect();

    // One `git log` for every subject the walk will want, instead of one
    // process per reflog entry. On a repository with a few thousand entries
    // that was several hundred spawns and most of the running time.
    let wanted: Vec<String> = read
        .iter()
        .flat_map(|(entries, _)| entries.iter())
        .filter(|entry| entry.old != entry.new)
        .map(|entry| entry.new.clone())
        .collect();
    prefetch_subjects(repo, &wanted, &mut subjects);

    for (entries, is_head_log) in &read {
        collect_amends(entries, &mut backward);
        if *is_head_log {
            collect_rebases(repo, entries, &mut backward, &mut subjects);
        }
    }

    backward
}

/// What the reflogs looked like: every file's size and modification time.
/// Cheap to compute and changes whenever git writes a rewrite.
fn reflog_stamp(common: &Path) -> String {
    let mut parts: Vec<String> = reflog_files(common)
        .iter()
        .filter_map(|(path, _)| {
            let meta = std::fs::metadata(path).ok()?;
            let modified = meta
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?;
            Some(format!(
                "{}:{}:{}",
                path.display(),
                meta.len(),
                modified.as_nanos()
            ))
        })
        .collect();
    parts.sort();
    parts.join("\n")
}

fn read_cache(path: &Path, stamp: &str) -> Option<HashMap<String, Vec<String>>> {
    let bytes = std::fs::read(path).ok()?;
    let cached: CachedLineage = serde_json::from_slice(&bytes).ok()?;
    (cached.stamp == stamp).then_some(cached.backward)
}

fn write_cache(path: &Path, stamp: &str, backward: &HashMap<String, Vec<String>>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let cached = CachedLineage {
        stamp: stamp.to_string(),
        backward: backward.clone(),
    };
    if let Ok(bytes) = serde_json::to_vec(&cached) {
        let _ = crate::persistence::storage::write_atomic(path, &bytes);
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedLineage {
    stamp: String,
    backward: HashMap<String, Vec<String>>,
}

/// Predecessors of `start`, breadth-first. Cycles are real — a branch reset
/// back to an old sha and amended again makes one — so the visited set is not
/// optional.
fn walk_back(start: &str, backward: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::from([start]);
    let mut queue: VecDeque<&str> = VecDeque::from([start]);
    let mut found = Vec::new();
    while let Some(sha) = queue.pop_front() {
        let Some(olds) = backward.get(sha) else {
            continue;
        };
        for old in olds {
            if seen.insert(old) {
                found.push(old.clone());
                queue.push_back(old);
            }
        }
    }
    found
}

fn add_edge(backward: &mut HashMap<String, Vec<String>>, old: &str, new: &str) {
    if old == new || old == NULL_SHA || new == NULL_SHA {
        return;
    }
    let olds = backward.entry(new.to_string()).or_default();
    if !olds.iter().any(|existing| existing == old) {
        olds.push(old.to_string());
    }
}

/// `commit (amend)` states both sides outright.
fn collect_amends(entries: &[Entry], backward: &mut HashMap<String, Vec<String>>) {
    for entry in entries {
        if classify(&entry.message) == Op::Amend {
            add_edge(backward, &entry.old, &entry.new);
        }
    }
}

/// Walk each `rebase (start)`…`(finish)` window, pairing the commits that went
/// in against the commits that came out.
fn collect_rebases(
    repo: &Path,
    entries: &[Entry],
    backward: &mut HashMap<String, Vec<String>>,
    subjects: &mut HashMap<String, String>,
) {
    let mut i = 0;
    while i < entries.len() {
        if classify(&entries[i].message) != Op::Start {
            i += 1;
            continue;
        }
        // The onto is the start entry's own new sha. The message may name a
        // ref (`checkout HEAD~2`), and resolving that today gives an unrelated
        // commit.
        let onto = entries[i].new.clone();
        let pre_tip = entries[i].old.clone();
        let mut slots = Slots::new(replayed(repo, &onto, &pre_tip, subjects));
        let mut pending: Vec<(String, String)> = Vec::new();
        let mut last_produced: Option<String> = None;

        let mut j = i + 1;
        while j < entries.len() {
            let entry = &entries[j];
            match classify(&entry.message) {
                Op::Finish => {
                    j += 1;
                    break;
                }
                // Everything the window produced was thrown away.
                Op::Abort => {
                    pending.clear();
                    j += 1;
                    break;
                }
                // An unterminated window: a crashed or abandoned rebase. The
                // entry that ends it belongs to whatever comes next.
                Op::Start | Op::Move => break,
                Op::Applied if entry.old != entry.new => {
                    let subject = reflog_subject(&entry.message);
                    if let Some(slot) =
                        slots.take_applied(&entry.new, subject.as_deref(), repo, subjects)
                    {
                        pending.push((slot, entry.new.clone()));
                    }
                    last_produced = Some(entry.new.clone());
                }
                // A reword carries the NEW subject, so the slot with the old
                // one never matches by name — and the summary fallback cannot
                // help either, since the summary is the thing that changed.
                // Two shapes: after a fast-forward the entry's own old sha IS
                // the original, so the slot matches by sha; without one, git
                // writes two entries and the second's old sha is the first's
                // product, so the edge chains through the intermediate.
                Op::Reword if entry.old != entry.new => {
                    let subject = reflog_subject(&entry.message);
                    if let Some(slot) =
                        slots.take_applied(&entry.new, subject.as_deref(), repo, subjects)
                    {
                        pending.push((slot, entry.new.clone()));
                    } else if let Some(slot) = slots.take_sha(&entry.old) {
                        pending.push((slot, entry.new.clone()));
                    } else if last_produced.as_deref() == Some(entry.old.as_str()) {
                        pending.push((entry.old.clone(), entry.new.clone()));
                    }
                    last_produced = Some(entry.new.clone());
                }
                // A fixup supersedes what the window built so far, so the
                // commit it lands on *and* the fixup itself both become the
                // combined sha.
                Op::Squashy if entry.old != entry.new => {
                    if let Some(previous) = last_produced.take() {
                        add_edge(backward, &previous, &entry.new);
                    }
                    if let Some(slot) =
                        slots.take_squashed(reflog_subject(&entry.message).as_deref())
                    {
                        pending.push((slot, entry.new.clone()));
                    }
                    add_edge(backward, &entry.old, &entry.new);
                    last_produced = Some(entry.new.clone());
                }
                _ => {}
            }
            j += 1;
        }

        for (old, new) in pending {
            add_edge(backward, &old, new.as_str());
        }
        i = j.max(i + 1);
    }
}

/// The commits a rebase set out to replay, oldest first, with their subjects.
fn replayed(
    repo: &Path,
    onto: &str,
    pre_tip: &str,
    subjects: &mut HashMap<String, String>,
) -> Vec<(String, String)> {
    let range = format!("{onto}..{pre_tip}");
    let out = git(repo, &["log", "--reverse", "--format=%H%x1f%s", &range]).unwrap_or_default();
    let replayed: Vec<(String, String)> = out
        .lines()
        .filter_map(|line| line.split_once('\u{1f}'))
        .map(|(sha, subject)| (sha.to_string(), subject.to_string()))
        .collect();
    // This call already answered for every commit it listed.
    for (sha, subject) in &replayed {
        subjects.insert(sha.clone(), subject.clone());
    }
    replayed
}

/// Fetch many subjects at once, in chunks git will accept on a command line.
///
/// `--ignore-missing` because the reflog names commits that have since been
/// pruned, and one of those would otherwise fail the whole batch.
fn prefetch_subjects(repo: &Path, shas: &[String], subjects: &mut HashMap<String, String>) {
    const CHUNK: usize = 256;
    let mut distinct: Vec<&String> = shas.iter().collect();
    distinct.sort();
    distinct.dedup();

    for chunk in distinct.chunks(CHUNK) {
        let mut args: Vec<&str> = vec!["log", "--no-walk", "--ignore-missing", "--format=%H%x1f%s"];
        args.extend(chunk.iter().map(|sha| sha.as_str()));
        let Ok(out) = git(repo, &args) else { continue };
        for (sha, subject) in out.lines().filter_map(|line| line.split_once('\u{1f}')) {
            subjects.insert(sha.to_string(), subject.to_string());
        }
    }
}

/// A commit's subject, fetched once per run.
fn subject_of(repo: &Path, sha: &str, subjects: &mut HashMap<String, String>) -> Option<String> {
    if let Some(known) = subjects.get(sha) {
        return Some(known.clone());
    }
    let fetched = git(repo, &["log", "-1", "--format=%s", sha])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    subjects.insert(sha.to_string(), fetched.clone());
    Some(fetched)
}

/// The rebase todo list, consumed as the window replays it.
struct Slots {
    items: Vec<(String, String)>,
    used: Vec<bool>,
    cursor: usize,
}

impl Slots {
    fn new(items: Vec<(String, String)>) -> Self {
        let used = vec![false; items.len()];
        Self {
            items,
            used,
            cursor: 0,
        }
    }

    /// Consume the slot this produced commit came from, and move the cursor
    /// past it. Slots skipped over stay available: under `--autosquash` those
    /// are the `fixup!`s, replayed later than they were committed.
    fn take_applied(
        &mut self,
        produced: &str,
        subject: Option<&str>,
        repo: &Path,
        subjects: &mut HashMap<String, String>,
    ) -> Option<String> {
        if let Some(k) = self.scan(|item| item.0 == produced) {
            return Some(self.use_forward(k));
        }
        let produced_subject = subject_of(repo, produced, subjects);
        for key in [subject.map(str::to_string), produced_subject]
            .into_iter()
            .flatten()
        {
            if key.starts_with('#') {
                continue;
            }
            if let Some(k) = self.scan(|item| item.1 == key) {
                return Some(self.use_forward(k));
            }
        }
        None
    }

    /// Consume the `fixup!` slot. The entry's own message names the commit
    /// being fixed up, not the fixup, so that is what it is matched against —
    /// and the cursor does not move, because the fixup sat further down the
    /// todo list than the pick that consumed it.
    fn take_squashed(&mut self, target: Option<&str>) -> Option<String> {
        let target = target.filter(|t| !t.starts_with('#'));
        if let Some(target) = target
            && let Some(k) = self.find_any(|item| {
                strip_squash_prefix(&item.1).is_some_and(|stripped| stripped == target)
            })
        {
            return Some(self.use_anywhere(k));
        }
        if let Some(k) = self.find_any(|item| strip_squash_prefix(&item.1).is_some()) {
            return Some(self.use_anywhere(k));
        }
        self.scan(|_| true).map(|k| self.use_forward(k))
    }

    /// Consume the slot whose ORIGINAL sha is `sha` — the fast-forwarded
    /// commit a reword entry names as its old side.
    fn take_sha(&mut self, sha: &str) -> Option<String> {
        self.scan(|item| item.0 == sha).map(|k| self.use_forward(k))
    }

    fn scan(&self, matches: impl Fn(&(String, String)) -> bool) -> Option<usize> {
        (self.cursor..self.items.len()).find(|&k| !self.used[k] && matches(&self.items[k]))
    }

    fn find_any(&self, matches: impl Fn(&(String, String)) -> bool) -> Option<usize> {
        (0..self.items.len()).find(|&k| !self.used[k] && matches(&self.items[k]))
    }

    fn use_forward(&mut self, k: usize) -> String {
        self.used[k] = true;
        self.cursor = self.cursor.max(k + 1);
        self.items[k].0.clone()
    }

    fn use_anywhere(&mut self, k: usize) -> String {
        self.used[k] = true;
        self.items[k].0.clone()
    }
}

fn strip_squash_prefix(subject: &str) -> Option<&str> {
    ["fixup! ", "squash! ", "amend! "]
        .iter()
        .find_map(|prefix| subject.strip_prefix(prefix))
}

/// The part of a reflog message after the operation, which for a replay is the
/// commit's subject.
fn reflog_subject(message: &str) -> Option<String> {
    message
        .split_once(": ")
        .map(|(_, rest)| rest.trim().to_string())
        .filter(|rest| !rest.is_empty())
}

/// Every reflog in the repository, with whether it is a HEAD log — the
/// start/pick/finish sequence of a rebase is only coherent within one of
/// those.
fn reflog_files(common: &Path) -> Vec<(PathBuf, bool)> {
    let mut files = Vec::new();
    let head = common.join("logs").join("HEAD");
    if head.is_file() {
        files.push((head, true));
    }
    collect_files(&common.join("logs").join("refs"), &mut files, false);
    if let Ok(worktrees) = std::fs::read_dir(common.join("worktrees")) {
        for worktree in worktrees.flatten() {
            let head = worktree.path().join("logs").join("HEAD");
            if head.is_file() {
                files.push((head, true));
            }
        }
    }
    files.sort();
    files
}

fn collect_files(dir: &Path, out: &mut Vec<(PathBuf, bool)>, is_head_log: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out, is_head_log);
        } else if path.is_file() {
            out.push((path, is_head_log));
        }
    }
}

/// `<old> <new> <who> <ts> <tz>\t<message>`, in the order they happened.
fn read_entries(path: &Path) -> Vec<Entry> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let (head, message) = line.split_once('\t')?;
            let mut parts = head.split(' ');
            let old = parts.next()?;
            let new = parts.next()?;
            if old.len() != 40 || new.len() != 40 {
                return None;
            }
            Some(Entry {
                old: old.to_string(),
                new: new.to_string(),
                message: message.to_string(),
            })
        })
        .collect()
}

fn common_dir(repo: &Path) -> Result<PathBuf> {
    let out = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(PathBuf::from(out.trim()))
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(crate::error::TuicrError::Io)?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository with `subjects` committed in order, and a reflog we write
    /// ourselves — the point is to exercise the window walker against reflog
    /// text, with real commits behind it so `git log` can answer.
    struct Fixture {
        _dir: tempfile::TempDir,
        path: PathBuf,
        shas: Vec<String>,
    }

    impl Fixture {
        fn new(subjects: &[&str]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().to_path_buf();
            let run = |args: &[&str]| {
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&path)
                    .args(args)
                    .output()
                    .unwrap();
            };
            run(&["init", "-q", "-b", "main"]);
            run(&["config", "user.email", "t@example.com"]);
            run(&["config", "user.name", "T"]);
            run(&["config", "commit.gpgsign", "false"]);
            let mut shas = Vec::new();
            for (i, subject) in subjects.iter().enumerate() {
                std::fs::write(path.join(format!("f{i}")), format!("{i}")).unwrap();
                run(&["add", "-A"]);
                run(&["commit", "-q", "-m", subject]);
                shas.push(
                    git(&path, &["rev-parse", "HEAD"])
                        .unwrap()
                        .trim()
                        .to_string(),
                );
            }
            Self {
                _dir: dir,
                path,
                shas,
            }
        }

        /// A commit on top of `parent`, off to the side. A rewritten commit is
        /// not an ancestor of the commit that replaced it, so the shapes here
        /// have to be built deliberately rather than in a line.
        fn commit_on(&mut self, parent: &str, subject: &str) -> String {
            let run = |args: &[&str]| {
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&self.path)
                    .args(args)
                    .output()
                    .unwrap();
            };
            run(&["checkout", "-q", "--detach", parent]);
            let name = format!("f{}", self.shas.len());
            std::fs::write(self.path.join(&name), subject).unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-q", "-m", subject]);
            let sha = git(&self.path, &["rev-parse", "HEAD"])
                .unwrap()
                .trim()
                .to_string();
            self.shas.push(sha.clone());
            sha
        }

        /// Replace the HEAD reflog with exactly these `(old, new, message)`
        /// lines, in order.
        fn set_reflog(&self, lines: &[(&str, &str, &str)]) {
            let logs = self.path.join(".git").join("logs");
            std::fs::create_dir_all(&logs).unwrap();
            let body: String = lines
                .iter()
                .map(|(old, new, msg)| {
                    format!("{old} {new} T <t@example.com> 1770900354 -0300\t{msg}\n")
                })
                .collect();
            std::fs::write(logs.join("HEAD"), body).unwrap();
        }

        fn predecessors_of(&self, sha: &str) -> Vec<String> {
            predecessors(&self.path, std::slice::from_ref(&sha.to_string()))
                .unwrap()
                .remove(sha)
                .unwrap_or_default()
        }
    }

    #[test]
    fn should_classify_on_the_verb_not_the_prefix() {
        assert_eq!(classify("commit (amend): tidy"), Op::Amend);
        assert_eq!(classify("rebase (start): checkout HEAD~2"), Op::Start);
        assert_eq!(classify("rebase -i (start): checkout main"), Op::Start);
        assert_eq!(classify("pull --rebase (pick): a change"), Op::Applied);
        assert_eq!(classify("rebase (fixup): a change"), Op::Squashy);
        assert_eq!(
            classify("rebase (finish): returning to refs/heads/x"),
            Op::Finish
        );
        assert_eq!(classify("checkout: moving from a to b"), Op::Move);
        assert_eq!(classify("commit: new work"), Op::Other);
    }

    #[test]
    fn should_read_both_shas_off_a_reflog_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("HEAD");
        std::fs::write(
            &path,
            format!(
                "{a} {b} Someone <s@x> 1770900354 -0300\tcommit (amend): tidy\n",
                a = "a".repeat(40),
                b = "b".repeat(40)
            ),
        )
        .unwrap();

        let entries = read_entries(&path);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].old, "a".repeat(40));
        assert_eq!(entries[0].new, "b".repeat(40));
        assert_eq!(entries[0].message, "commit (amend): tidy");
    }

    #[test]
    fn should_compose_a_chain_of_amends() {
        let mut backward = HashMap::new();
        let (a, b, c) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
        add_edge(&mut backward, &a, &b);
        add_edge(&mut backward, &b, &c);

        let found = walk_back(&c, &backward);

        assert_eq!(found, vec![b, a], "nearest first, all the way back");
    }

    #[test]
    fn should_survive_a_cycle() {
        // Reset back to an old sha, then amend again: the graph has a loop.
        let mut backward = HashMap::new();
        let (a, b) = ("a".repeat(40), "b".repeat(40));
        add_edge(&mut backward, &a, &b);
        add_edge(&mut backward, &b, &a);

        assert_eq!(walk_back(&b, &backward), vec![a]);
    }

    #[test]
    fn should_ignore_the_null_sha_of_a_branch_being_created() {
        let mut backward = HashMap::new();
        add_edge(&mut backward, NULL_SHA, &"b".repeat(40));
        assert!(backward.is_empty());
    }

    #[test]
    fn should_leave_a_skipped_slot_available_for_a_later_fixup() {
        // Under --autosquash the fixup! is committed last but replayed early,
        // so a slot the cursor moved past must still be takeable.
        let mut slots = Slots::new(vec![
            ("1".repeat(40), "a change".to_string()),
            ("2".repeat(40), "fixup! a change".to_string()),
            ("3".repeat(40), "another change".to_string()),
        ]);
        let repo = Path::new("/nonexistent");

        let mut subjects = HashMap::new();
        let picked = slots.take_applied(&"9".repeat(40), Some("a change"), repo, &mut subjects);
        assert_eq!(picked, Some("1".repeat(40)));
        let fixed = slots.take_squashed(Some("a change"));
        assert_eq!(
            fixed,
            Some("2".repeat(40)),
            "the fixup! slot, found behind the cursor"
        );
        let next = slots.take_applied(&"8".repeat(40), Some("another change"), repo, &mut subjects);
        assert_eq!(next, Some("3".repeat(40)), "and the cursor did not drift");
    }

    #[test]
    fn should_pair_a_rebase_window_in_order() {
        // base, then two commits replayed onto it.
        let fx = Fixture::new(&["base", "first", "second", "first'", "second'"]);
        let (base, old1, old2, new1, new2) = (
            &fx.shas[0],
            &fx.shas[1],
            &fx.shas[2],
            &fx.shas[3],
            &fx.shas[4],
        );
        fx.set_reflog(&[
            (old2, base, "rebase (start): checkout main~2"),
            (base, new1, "rebase (pick): first"),
            (new1, new2, "rebase (pick): second"),
            (new2, new2, "rebase (finish): returning to refs/heads/main"),
        ]);

        assert_eq!(fx.predecessors_of(new1), vec![old1.clone()]);
        assert_eq!(fx.predecessors_of(new2), vec![old2.clone()]);
    }

    #[test]
    fn should_take_the_onto_from_the_start_entry_not_its_message() {
        // The message names `main~2`, which resolves today to something else
        // entirely. Using it would pair the wrong commits.
        let fx = Fixture::new(&["base", "first", "later", "first'"]);
        let (base, old1, new1) = (&fx.shas[0], &fx.shas[1], &fx.shas[3]);
        fx.set_reflog(&[
            (old1, base, "rebase (start): checkout main~2"),
            (base, new1, "rebase (pick): first"),
            (new1, new1, "rebase (finish): returning to refs/heads/main"),
        ]);

        assert_eq!(fx.predecessors_of(new1), vec![old1.clone()]);
    }

    #[test]
    fn should_fold_a_fixup_into_the_commit_it_lands_on() {
        // pick A, then fixup F: both the original and the fixup! become the
        // combined commit. The fixup entry rewrites HEAD in place, so its old
        // and new differ.
        let mut fx = Fixture::new(&["base"]);
        let base = fx.shas[0].clone();
        let original = fx.commit_on(&base, "a change");
        let fixup = fx.commit_on(&original, "fixup! a change");
        let picked = fx.commit_on(&base, "a change");
        let combined = fx.commit_on(&base, "a change");
        fx.set_reflog(&[
            (&fixup, &base, "rebase (start): checkout main~2"),
            (&base, &picked, "rebase (pick): a change"),
            (&picked, &combined, "rebase (fixup): a change"),
            (
                &combined,
                &combined,
                "rebase (finish): returning to refs/heads/main",
            ),
        ]);

        let found = fx.predecessors_of(&combined);
        assert!(
            found.contains(&original),
            "the commit that was fixed up: {found:?}"
        );
        assert!(found.contains(&fixup), "and the fixup! itself: {found:?}");
    }

    #[test]
    fn should_discard_a_window_that_was_aborted() {
        let fx = Fixture::new(&["base", "first", "first'"]);
        let (base, old1, new1) = (&fx.shas[0], &fx.shas[1], &fx.shas[2]);
        fx.set_reflog(&[
            (old1, base, "rebase (start): checkout main~1"),
            (base, new1, "rebase (pick): first"),
            (new1, old1, "rebase (abort): returning to refs/heads/main"),
        ]);

        assert!(
            fx.predecessors_of(new1).is_empty(),
            "what an aborted rebase produced was thrown away"
        );
    }

    #[test]
    fn should_end_an_unterminated_window_at_the_next_move() {
        // A crashed rebase: no finish, then a checkout. The checkout is not
        // part of the window and must not consume a slot.
        let fx = Fixture::new(&["base", "first", "first'", "elsewhere"]);
        let (base, old1, new1, other) = (&fx.shas[0], &fx.shas[1], &fx.shas[2], &fx.shas[3]);
        fx.set_reflog(&[
            (old1, base, "rebase (start): checkout main~1"),
            (base, new1, "rebase (pick): first"),
            (new1, other, "checkout: moving from main to other"),
        ]);

        assert_eq!(fx.predecessors_of(new1), vec![old1.clone()]);
        assert!(fx.predecessors_of(other).is_empty());
    }

    #[test]
    fn should_carry_a_chain_across_an_amend_and_a_rebase() {
        // v1 amended into v2, then v2 rebased into v3. Each version sits on
        // the base rather than on its predecessor.
        let mut fx = Fixture::new(&["base"]);
        let base = fx.shas[0].clone();
        let v1 = fx.commit_on(&base, "first");
        let v2 = fx.commit_on(&base, "first");
        let v3 = fx.commit_on(&base, "first");
        fx.set_reflog(&[
            (&v1, &v2, "commit (amend): first"),
            (&v2, &base, "rebase (start): checkout main~1"),
            (&base, &v3, "rebase (pick): first"),
            (&v3, &v3, "rebase (finish): returning to refs/heads/main"),
        ]);

        let found = fx.predecessors_of(&v3);
        assert!(
            found.contains(&v2) && found.contains(&v1),
            "both hops: {found:?}"
        );
    }

    #[test]
    fn should_follow_a_reword_after_a_fast_forward() {
        // The reword entry's old side IS the original commit, fast-forwarded;
        // the subject can never match, because it is what changed.
        let mut fx = Fixture::new(&["base"]);
        let base = fx.shas[0].clone();
        let v1 = fx.commit_on(&base, "first");
        let v2 = fx.commit_on(&base, "first, reworded");
        fx.set_reflog(&[
            (&v1, &base, "rebase (start): checkout main~1"),
            (&v1, &v2, "rebase (reword): first, reworded"),
            (&v2, &v2, "rebase (finish): returning to refs/heads/main"),
        ]);

        assert_eq!(fx.predecessors_of(&v2), vec![v1.clone()]);
    }

    #[test]
    fn should_follow_a_reword_through_its_intermediate() {
        // Without a fast-forward git writes two entries: a replay keyed by the
        // OLD subject, then a reword whose old side is that replay's product.
        let mut fx = Fixture::new(&["base"]);
        let base = fx.shas[0].clone();
        let v1 = fx.commit_on(&base, "first");
        let mid = fx.commit_on(&base, "first");
        let v2 = fx.commit_on(&base, "first, reworded");
        fx.set_reflog(&[
            (&v1, &base, "rebase (start): checkout main~1"),
            (&base, &mid, "rebase (reword): first"),
            (&mid, &v2, "rebase (reword): first, reworded"),
            (&v2, &v2, "rebase (finish): returning to refs/heads/main"),
        ]);

        let found = fx.predecessors_of(&v2);
        assert!(
            found.contains(&v1),
            "the original is reachable through the intermediate: {found:?}"
        );
    }

    /// Against a real repository when `TUICR_LINEAGE_REPO` names one.
    #[test]
    fn should_chain_a_real_rewrite_history() {
        let Ok(repo) = std::env::var("TUICR_LINEAGE_REPO") else {
            return;
        };
        let repo = PathBuf::from(repo);
        let probe = std::env::var("TUICR_LINEAGE_COMMIT").unwrap_or_else(|_| "HEAD".to_string());
        let head = git(&repo, &["rev-parse", &probe])
            .unwrap()
            .trim()
            .to_string();
        let started = std::time::Instant::now();
        let found = predecessors(&repo, std::slice::from_ref(&head)).unwrap();
        let elapsed = started.elapsed();

        let chain = &found[&head];
        println!(
            "predecessors of {}: {} in {:?}",
            &head[..8],
            chain.len(),
            elapsed
        );
        for sha in chain.iter().take(10) {
            let subject = git(&repo, &["log", "-1", "--format=%s", sha]).unwrap();
            println!("   {} {}", &sha[..8], subject.trim());
        }
    }

    #[test]
    fn should_call_a_rebased_away_commit_dead_even_though_it_still_resolves() {
        // git keeps a rewritten commit resolvable for as long as its reflog
        // entry lives. Asking "does this object exist" says yes for months and
        // leaves a review's comments stranded on the version it replaced.
        let mut fx = Fixture::new(&["base"]);
        let base = fx.shas[0].clone();
        let left_behind = fx.commit_on(&base, "first draft");
        // The branch never moved to it: exactly the state a rebase leaves the
        // version it replaced in.
        let live = reachable_from_refs(&fx.path, &[base.clone(), left_behind.clone()]).unwrap();

        assert!(live.contains(&base), "the branch tip is live");
        assert!(
            !live.contains(&left_behind),
            "nothing points at the replaced version, though it still resolves"
        );
        assert!(
            git(&fx.path, &["rev-parse", "--verify", &left_behind]).is_ok(),
            "and this is exactly why resolving is the wrong question"
        );
    }

    #[test]
    fn should_reuse_the_edge_map_until_the_reflog_moves() {
        let mut fx = Fixture::new(&["base"]);
        let base = fx.shas[0].clone();
        let v1 = fx.commit_on(&base, "a change");
        let v2 = fx.commit_on(&base, "a change, fixed");
        fx.set_reflog(&[(&v1, &v2, "commit (amend): a change, fixed")]);
        let cache = fx.path.join("lineage.json");

        let first = predecessors_cached(&fx.path, std::slice::from_ref(&v2), Some(&cache)).unwrap();
        assert_eq!(first[&v2], vec![v1.clone()]);
        assert!(cache.is_file(), "the map is written for next time");

        // Doctor the cached edges, leaving the stamp alone: the doctored
        // answer coming back is what proves the reflogs were not read again.
        let mut stored: CachedLineage =
            serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
        stored
            .backward
            .insert(v2.clone(), vec!["doctored".to_string()]);
        std::fs::write(&cache, serde_json::to_vec(&stored).unwrap()).unwrap();

        let served =
            predecessors_cached(&fx.path, std::slice::from_ref(&v2), Some(&cache)).unwrap();
        assert_eq!(
            served[&v2],
            vec!["doctored".to_string()],
            "served from the cache without rebuilding"
        );

        // A reflog that moved throws it away — a rewrite changes the answer,
        // which is the whole reason the map exists.
        let v3 = fx.commit_on(&base, "a change, fixed again");
        fx.set_reflog(&[
            (&v1, &v2, "commit (amend): a change, fixed"),
            (&v2, &v3, "commit (amend): a change, fixed again"),
        ]);
        let fresh = predecessors_cached(&fx.path, std::slice::from_ref(&v3), Some(&cache)).unwrap();
        assert!(
            fresh[&v3].contains(&v2),
            "rebuilt after the reflog moved: {:?}",
            fresh[&v3]
        );
    }
}
