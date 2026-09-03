//! Non-interactive review session commands.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cli::{LineSideArg, ReviewCommand};
use crate::config;
use crate::error::{Result, TuicrError};
use crate::model::comment::{self, CommentLifecycleState};
use crate::model::{Comment, CommentType, LineRange, LineSide, ReviewSession};
use crate::review_store::{
    AddCommentRequest, CommentTarget, ReplyRequest, ReviewStore, SessionRef, SessionSummary,
};
use crate::slug::Slug;

pub fn run(command: ReviewCommand) -> Result<()> {
    let mut stdout = io::stdout();
    run_with_writer(command, &mut stdout)
}

fn run_with_writer(command: ReviewCommand, out: &mut impl Write) -> Result<()> {
    match command {
        ReviewCommand::List { repo, all } => list_sessions(&repo, all, out),
        ReviewCommand::Add {
            session,
            input,
            repo,
            comment_type,
            file,
            line,
            end_line,
            side,
            username,
            content,
        } => add_comment(
            &session,
            &repo,
            AddCommentOptions {
                input,
                comment_type,
                file,
                line,
                end_line,
                side,
                username,
                content,
            },
            out,
        ),
        ReviewCommand::Reply {
            session,
            comment_id,
            input,
            repo,
            username,
            content,
        } => reply_to_comment(
            &session,
            &repo,
            ReplyOptions {
                comment_id,
                input,
                username,
                content,
            },
            out,
        ),
        ReviewCommand::Resolve {
            session,
            comment_id,
            unresolve,
            repo,
        } => set_resolved(&session, &repo, &comment_id, !unresolve, out),
        ReviewCommand::Watch {
            session,
            any,
            unanswered,
            username,
            timeout_secs,
            repo,
        } => watch_session(
            &session,
            &repo,
            WatchMode::new(any, unanswered, username),
            timeout_secs,
            out,
        ),
        ReviewCommand::Migrate {
            dry_run,
            reviews_dir,
        } => migrate_to_store(dry_run, reviews_dir, out),
        ReviewCommand::Working {
            session,
            message,
            username,
            done,
            repo,
        } => announce_working(&session, &repo, message, username, done, out),
        ReviewCommand::Update {
            session,
            message,
            repo,
        } => announce_update(&session, &repo, message, out),
        ReviewCommand::Comments { session, repo } => show_comments(&session, &repo, out),
    }
}

fn list_sessions(repo: &Path, all: bool, out: &mut impl Write) -> Result<()> {
    let store = ReviewStore::new();
    let summaries = if all {
        store.list_all_sessions()?
    } else {
        store.list_sessions_for_repo(repo)?
    };
    let output: Vec<_> = summaries
        .into_iter()
        .map(SessionSummaryOutput::from)
        .collect();
    serde_json::to_writer_pretty(&mut *out, &output)?;
    writeln!(out)?;
    Ok(())
}

struct AddCommentOptions {
    input: Option<String>,
    comment_type: String,
    file: Option<PathBuf>,
    line: Option<u32>,
    end_line: Option<u32>,
    side: LineSideArg,
    username: Option<String>,
    content: Option<String>,
}

fn add_comment(
    session: &str,
    repo: &Path,
    options: AddCommentOptions,
    out: &mut impl Write,
) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    let request_parts = build_add_request_parts(options)?;
    let target = request_parts.target;
    let comment_type = CommentType::from_id(&request_parts.comment_type);
    // One config read serves both the type check and the author fallback.
    let config = config::load_config()
        .ok()
        .and_then(|outcome| outcome.config);
    if let Some(warning) = unknown_comment_type_warning(&comment_type, config.as_ref()) {
        eprintln!("{warning}");
    }
    let author = resolve_cli_author(request_parts.username, config.as_ref());
    let request =
        AddCommentRequest::new(target.clone(), request_parts.content, comment_type, author);
    let session_data = store.get_review(&session_ref)?;
    let comment = match store_for(&session_data) {
        Some(cstore) => store_add_comment(&cstore, &session_data, request)?,
        None => store.add_comment(&session_ref, request)?,
    };
    let output = CommentOutput::from_target(&target, &comment);
    serde_json::to_writer_pretty(&mut *out, &output)?;
    writeln!(out)?;
    Ok(())
}

struct ReplyOptions {
    comment_id: Option<String>,
    input: Option<String>,
    username: Option<String>,
    content: Option<String>,
}

/// Payload accepted by `tuicr review reply --input`.
#[derive(Debug, Deserialize)]
struct ReplyPayload {
    /// Id of the comment being replied to. `comment_id` and `in_reply_to` are
    /// both accepted so the payload can be built straight from a
    /// `review comments` entry either way.
    #[serde(alias = "in_reply_to")]
    comment_id: Option<String>,
    content: Option<String>,
    username: Option<String>,
}

/// Flags and JSON payload resolved into the parts of a reply. Flags win over
/// JSON fields, matching `review add`.
#[derive(Debug)]
struct ReplyParts {
    parent_id: String,
    content: String,
    username: Option<String>,
}

fn build_reply_parts(options: ReplyOptions) -> Result<ReplyParts> {
    let ReplyOptions {
        mut comment_id,
        input,
        mut username,
        mut content,
    } = options;

    if let Some(input) = input {
        let raw = read_json_input(&input)?;
        let payload: ReplyPayload = serde_json::from_str(&raw)
            .map_err(|err| TuicrError::InvalidInput(format!("invalid reply JSON: {err}")))?;
        comment_id = comment_id.or(payload.comment_id);
        content = content.or(payload.content);
        username = username.or(payload.username);
    }

    let parent_id = comment_id
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            TuicrError::InvalidInput("reply needs --comment-id (or a JSON comment_id)".to_string())
        })?;
    let content = content
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .ok_or_else(|| TuicrError::InvalidInput("reply cannot be empty".to_string()))?;

    Ok(ReplyParts {
        parent_id,
        content,
        username,
    })
}

/// Resolve `--comment-id` against the session, accepting an unambiguous
/// prefix the way git accepts short SHAs — copying a full UUID out of
/// `review comments` to name a comment is needless friction.
fn resolve_comment_id(session: &ReviewSession, wanted: &str) -> Result<String> {
    let ids: Vec<&str> = session
        .review_comments
        .iter()
        .chain(session.files.values().flat_map(|review| {
            review
                .file_comments
                .iter()
                .chain(review.line_comments.values().flatten())
        }))
        .map(|comment| comment.id.as_str())
        .collect();

    // An exact id wins outright: never let one comment's id being a prefix of
    // another's turn a precise request into an ambiguity error.
    if ids.contains(&wanted) {
        return Ok(wanted.to_string());
    }

    let matches: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|id| id.starts_with(wanted))
        .collect();
    match matches.as_slice() {
        [id] => Ok((*id).to_string()),
        [] => Err(TuicrError::InvalidInput(format!(
            "session has no comment with id {wanted}"
        ))),
        several => Err(TuicrError::InvalidInput(format!(
            "comment id {wanted} is ambiguous — matches {}",
            several.join(", ")
        ))),
    }
}

fn reply_to_comment(
    session: &str,
    repo: &Path,
    options: ReplyOptions,
    out: &mut impl Write,
) -> Result<()> {
    let ReplyParts {
        parent_id,
        content,
        username,
    } = build_reply_parts(options)?;

    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    let session_data = store.get_review(&session_ref)?;
    // Same config read the comment path does: the author fallback lives there.
    let config = config::load_config()
        .ok()
        .and_then(|outcome| outcome.config);
    let author = resolve_cli_author(username, config.as_ref());
    let reply = match store_for(&session_data) {
        Some(cstore) => {
            let parent_id = resolve_stored_comment_id(&cstore, &session_data, &parent_id)?;
            store_reply(&cstore, &session_data, &parent_id, content, author)?
        }
        None => {
            let parent_id = resolve_comment_id(&session_data, &parent_id)?;
            store.reply_to_comment(
                &session_ref,
                ReplyRequest {
                    parent_id,
                    content,
                    author,
                    reopen: false,
                },
            )?
        }
    };

    // Report the reply with its thread's anchor, so callers see where it
    // landed without re-reading the whole session.
    let session_data = store.get_review(&session_ref)?;
    let output = collect_comments(&session_data)
        .into_iter()
        .find(|c| c.id == reply.id)
        .ok_or_else(|| {
            TuicrError::InvalidInput("reply was not found in the saved session".to_string())
        })?;
    serde_json::to_writer_pretty(&mut *out, &output)?;
    writeln!(out)?;
    Ok(())
}

struct AddRequestParts {
    target: CommentTarget,
    comment_type: String,
    content: String,
    username: Option<String>,
}

/// Resolve the author for a CLI-authored comment.
///
/// Priority: explicit `--username` / JSON `username` ► config `username` ►
/// `Comment::DEFAULT_AUTHOR`. Trims whitespace so `--username " "` doesn't
/// produce an awkward all-whitespace badge.
fn resolve_cli_author(explicit: Option<String>, config: Option<&config::AppConfig>) -> String {
    if let Some(name) = explicit.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return name.to_string();
    }
    if let Some(name) = config
        .and_then(|cfg| cfg.username.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return name.to_string();
    }
    comment::DEFAULT_AUTHOR.to_string()
}

/// Warn when `--type` names a type that no `comment_types` entry defines.
///
/// `CommentType::from_id` turns any non-empty id into `Custom`, so a typo is
/// indistinguishable from a configured type: it is stored, exported as
/// `**[TYPO]**`, and shown with no badge in the TUI. Warning rather than
/// rejecting keeps the CLI usable when `comment_types` is unset — the default,
/// under which every id but `none` is technically undefined — and keeps
/// scripted callers from breaking on a config change they don't control.
fn unknown_comment_type_warning(
    comment_type: &CommentType,
    config: Option<&config::AppConfig>,
) -> Option<String> {
    if comment_type.is_none() {
        return None;
    }
    let configured = config.and_then(|cfg| cfg.comment_types.as_deref())?;
    if configured
        .iter()
        .any(|definition| definition.id == comment_type.id())
    {
        return None;
    }
    let known: Vec<&str> = configured
        .iter()
        .map(|definition| definition.id.as_str())
        .collect();
    Some(format!(
        "Warning: comment type '{}' is not configured; known types: {}",
        comment_type.id(),
        known.join(", ")
    ))
}

fn build_add_request_parts(options: AddCommentOptions) -> Result<AddRequestParts> {
    let mut comment_type = options.comment_type;
    let mut content = options.content;
    let mut file = options.file;
    let mut line = options.line;
    let mut end_line = options.end_line;
    let mut side = options.side;
    let mut username = options.username;
    let mut target = None;

    if let Some(input) = options.input {
        let payload = parse_add_payload(&read_json_input(&input)?)?;
        if let Some(payload_comment_type) = payload.comment_type {
            comment_type = payload_comment_type;
        }
        if payload.content.is_some() {
            content = payload.content;
        }
        if payload.username.is_some() {
            username = payload.username;
        }
        if let Some(payload_target) = payload.target {
            target = Some(payload_target.into_comment_target()?);
        } else {
            if let Some(payload_file) = payload.file {
                file = Some(payload_file);
            }
            if payload.line.is_some() || payload.start_line.is_some() {
                line = payload.line.or(payload.start_line);
            }
            if let Some(payload_end_line) = payload.end_line {
                end_line = Some(payload_end_line);
            }
            if let Some(payload_side) = payload.side {
                side = parse_line_side(&payload_side)?;
            }
        }
    }

    let content = content.ok_or_else(|| {
        TuicrError::InvalidInput(
            "comment text is required either as COMMENT or JSON field `content`".to_string(),
        )
    })?;
    let target = match target {
        Some(target) => target,
        None => build_comment_target(file, line, end_line, side)?,
    };

    Ok(AddRequestParts {
        target,
        comment_type,
        content,
        username,
    })
}

fn read_json_input(input: &str) -> Result<String> {
    if input == "-" {
        let mut contents = String::new();
        io::stdin().read_to_string(&mut contents)?;
        return Ok(contents);
    }
    if let Some(path) = input.strip_prefix('@') {
        return fs::read_to_string(path).map_err(TuicrError::Io);
    }
    Ok(input.to_string())
}

fn parse_add_payload(input: &str) -> Result<AddCommentPayload> {
    serde_json::from_str(input)
        .map_err(|err| TuicrError::InvalidInput(format!("invalid JSON review payload: {err}")))
}

#[derive(Debug, Deserialize)]
struct AddCommentPayload {
    #[serde(default, alias = "type")]
    comment_type: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    target: Option<JsonCommentTarget>,
    #[serde(default)]
    file: Option<PathBuf>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
    #[serde(default)]
    side: Option<String>,
    #[serde(default, alias = "author")]
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JsonCommentTarget {
    #[serde(default, rename = "type", alias = "kind")]
    target_type: Option<String>,
    #[serde(default)]
    file: Option<PathBuf>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
    #[serde(default)]
    side: Option<String>,
}

impl JsonCommentTarget {
    fn into_comment_target(self) -> Result<CommentTarget> {
        let side = match self.side {
            Some(side) => parse_line_side(&side)?,
            None => LineSideArg::New,
        };
        let inferred_type = if self.file.is_none() {
            "review"
        } else if self.line.is_some() || self.start_line.is_some() {
            if self.end_line.is_some() {
                "line_range"
            } else {
                "line"
            }
        } else {
            "file"
        };
        let target_type = self
            .target_type
            .unwrap_or_else(|| inferred_type.to_string())
            .replace('-', "_")
            .to_ascii_lowercase();

        match target_type.as_str() {
            "review" => Ok(CommentTarget::Review),
            "file" => Ok(CommentTarget::File {
                path: required_file(self.file, "target.file")?,
            }),
            "line" => Ok(CommentTarget::Line {
                path: required_file(self.file, "target.file")?,
                line: required_line(self.line.or(self.start_line), "target.line")?,
                side: line_side_arg_to_model(side),
            }),
            "line_range" | "range" => Ok(CommentTarget::LineRange {
                path: required_file(self.file, "target.file")?,
                range: LineRange::new(
                    required_line(self.line.or(self.start_line), "target.start_line")?,
                    required_line(self.end_line, "target.end_line")?,
                ),
                side: line_side_arg_to_model(side),
            }),
            other => Err(TuicrError::InvalidInput(format!(
                "unknown JSON target type '{other}'"
            ))),
        }
    }
}

fn required_file(path: Option<PathBuf>, name: &str) -> Result<PathBuf> {
    path.ok_or_else(|| TuicrError::InvalidInput(format!("{name} is required")))
}

fn required_line(line: Option<u32>, name: &str) -> Result<u32> {
    let line = line.ok_or_else(|| TuicrError::InvalidInput(format!("{name} is required")))?;
    validate_line(line, name)?;
    Ok(line)
}

fn parse_line_side(side: &str) -> Result<LineSideArg> {
    match side.to_ascii_lowercase().as_str() {
        "old" => Ok(LineSideArg::Old),
        "new" => Ok(LineSideArg::New),
        other => Err(TuicrError::InvalidInput(format!(
            "unknown side '{other}', expected 'old' or 'new'"
        ))),
    }
}

fn line_side_arg_to_model(side: LineSideArg) -> LineSide {
    match side {
        LineSideArg::Old => LineSide::Old,
        LineSideArg::New => LineSide::New,
    }
}

fn set_resolved(
    session: &str,
    repo: &Path,
    comment_id: &str,
    resolved: bool,
    out: &mut impl Write,
) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    let session_data = store.get_review(&session_ref)?;
    let root = match store_for(&session_data) {
        Some(cstore) => {
            let comment_id = resolve_stored_comment_id(&cstore, &session_data, comment_id)?;
            store_set_thread_resolved(&cstore, &comment_id, resolved)?
        }
        None => {
            let comment_id = resolve_comment_id(&session_data, comment_id)?;
            store.set_thread_resolved(&session_ref, &comment_id, resolved)?
        }
    };

    // Report the thread's root as `comments` would show it, so the caller sees
    // which thread moved and its new state.
    let session_data = store.get_review(&session_ref)?;
    let output = collect_comments(&session_data)
        .into_iter()
        .find(|c| c.id == root.id)
        .ok_or_else(|| {
            TuicrError::InvalidInput("thread root was not found in the saved session".to_string())
        })?;
    serde_json::to_writer_pretty(&mut *out, &output)?;
    writeln!(out)?;
    Ok(())
}

/// How long between reads of the session file while watching. Short enough to
/// feel immediate, long enough to be free.
const WATCH_POLL: std::time::Duration = std::time::Duration::from_millis(400);

/// What ended a watch, so the caller can tell "here is your work" from
/// "nothing happened".
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum WatchOutcome {
    /// The reviewer ran `:submit agent`.
    Handoff,
    /// `--unanswered`: threads are waiting on the caller.
    Unanswered,
    /// `--any`: the session changed.
    Changed,
    /// Nothing happened before `--timeout`.
    Timeout,
    /// The session file went away — the review was discarded.
    Gone,
}

#[derive(Debug, Serialize)]
struct WatchOutput {
    outcome: WatchOutcome,
    /// The session that woke, resolved — what was passed may have been a path
    /// or a prefix, and the next call (`review working`) wants the real name.
    session: String,
    comments: Vec<CommentOutput>,
}

/// What a watch is waiting for.
enum WatchMode {
    /// The explicit handoff, and nothing else.
    Handoff,
    /// Any edit at all.
    Any,
    /// A thread waiting on this agent — or the handoff, which still means
    /// "everything, now" and must not be swallowed by a quiet inbox.
    Unanswered(String),
}

impl WatchMode {
    fn new(any: bool, unanswered: bool, username: Option<String>) -> Self {
        match (unanswered, username) {
            (true, Some(name)) => Self::Unanswered(name),
            _ if any => Self::Any,
            _ => Self::Handoff,
        }
    }
}

/// Block until the reviewer hands the review over (or the session changes,
/// with `--any`), then print its comments.
///
/// The point of waiting on an explicit handoff rather than on any edit is that
/// a review is written in pieces: waking on every saved comment would have the
/// agent answering half a thought.
fn watch_session(
    session: &str,
    repo: &Path,
    mode: WatchMode,
    timeout_secs: u64,
    out: &mut impl Write,
) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    let baseline = store.get_review(&session_ref)?;
    let baseline_request = baseline.agent_request;
    let baseline_updated = baseline.updated_at;

    // Work that is already waiting is work: a watch started after the reviewer
    // wrote something should not sleep through it.
    if let WatchMode::Unanswered(agent) = &mode
        && !unanswered_threads(&baseline, agent).is_empty()
    {
        return report_watch(
            session,
            WatchOutcome::Unanswered,
            Some(baseline),
            &mode,
            out,
        );
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let (outcome, session_data) = loop {
        if std::time::Instant::now() >= deadline {
            break (WatchOutcome::Timeout, store.get_review(&session_ref).ok());
        }
        std::thread::sleep(WATCH_POLL);

        // A session that disappears mid-watch is not an error: the reviewer
        // discarded it, and the caller should stop waiting rather than fail.
        let Ok(current) = store.get_review(&session_ref) else {
            if session_ref.path().exists() {
                continue;
            }
            break (WatchOutcome::Gone, None);
        };

        // The handoff outranks everything, in every mode. It still means "I am
        // done, go" — and an inbox that happens to be answered must not
        // swallow it.
        if current.agent_request != baseline_request {
            break (WatchOutcome::Handoff, Some(current));
        }
        match &mode {
            WatchMode::Any if current.updated_at != baseline_updated => {
                break (WatchOutcome::Changed, Some(current));
            }
            WatchMode::Unanswered(agent) if !unanswered_threads(&current, agent).is_empty() => {
                break (WatchOutcome::Unanswered, Some(current));
            }
            _ => {}
        }
    };

    report_watch(session, outcome, session_data, &mode, out)
}

/// Print what the watch woke on. An `unanswered` wake carries only the threads
/// waiting on the agent; every other outcome carries the whole review, because
/// a handoff means "all of it".
fn report_watch(
    session: &str,
    outcome: WatchOutcome,
    session_data: Option<ReviewSession>,
    mode: &WatchMode,
    out: &mut impl Write,
) -> Result<()> {
    let comments = match (&outcome, mode, session_data.as_ref()) {
        (WatchOutcome::Unanswered, WatchMode::Unanswered(agent), Some(data)) => {
            unanswered_threads(data, agent)
        }
        (_, _, Some(data)) => collect_comments(data),
        (_, _, None) => Vec::new(),
    };
    serde_json::to_writer_pretty(
        &mut *out,
        &WatchOutput {
            outcome,
            session: session.to_string(),
            comments,
        },
    )?;
    writeln!(out)?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct UpdateOutput {
    session: String,
    announced_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Serialize)]
struct WorkingOutput {
    session: String,
    working: bool,
    at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
}

/// Say an agent has the review and is working on it — or, with `--done`, that
/// it has stopped.
///
/// `:submit agent` hands the work over and `review update` hands it back; in
/// between, the reviewer had a still screen that could not tell an agent
/// thinking from no agent listening. This is the sign of life.
///
/// Read-modify-write under the store lock, so a reviewer commenting in the TUI
/// at the same moment — the normal case right after a handoff — does not lose
/// their comment to this one field.
fn announce_working(
    session: &str,
    repo: &Path,
    message: Option<String>,
    username: Option<String>,
    done: bool,
    out: &mut impl Write,
) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    let message = message
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let config = config::load_config()
        .ok()
        .and_then(|outcome| outcome.config);
    let agent = (!done).then(|| resolve_cli_author(username, config.as_ref()));
    let activity = crate::model::review::AgentActivity {
        at: chrono::Utc::now(),
        message: message.clone(),
        agent: agent.clone(),
    };
    let stamped = activity.clone();
    store.update_session(&session_ref, move |session_data| {
        crate::review_store::set_agent_working(session_data, stamped, done);
        Ok(())
    })?;

    let output = WorkingOutput {
        session: session.to_string(),
        working: !done,
        at: activity.at.to_rfc3339(),
        message,
        agent,
    };
    serde_json::to_writer_pretty(&mut *out, &output)?;
    writeln!(out)?;
    Ok(())
}

/// Stamp the session so the reviewer's open TUI can say the branch moved.
///
/// The counterpart of `:submit agent`: that is the reviewer handing work over,
/// this is the agent handing it back. Without it the reviewer discovers a
/// rewritten branch by reloading and seeing nothing change, which reads as a
/// broken reload rather than a moved branch.
fn announce_update(
    session: &str,
    repo: &Path,
    message: Option<String>,
    out: &mut impl Write,
) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    let message = message
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let announced = crate::model::review::AgentUpdate {
        at: chrono::Utc::now(),
        message: message.clone(),
    };
    let stamped = announced.clone();
    // Under the store lock: the reviewer is usually still reading and
    // commenting while this lands, and a whole-session write would drop
    // whatever they saved since it was read.
    store.update_session(&session_ref, move |session_data| {
        session_data.agent_update = Some(stamped.clone());
        // Handing the work back ends the working state, whether or not the
        // agent remembered to say so.
        session_data.agent_working = None;
        session_data.updated_at = stamped.at;
        Ok(())
    })?;

    let output = UpdateOutput {
        session: session.to_string(),
        announced_at: announced.at.to_rfc3339(),
        message,
    };
    serde_json::to_writer_pretty(&mut *out, &output)?;
    writeln!(out)?;
    Ok(())
}

fn show_comments(session: &str, repo: &Path, out: &mut impl Write) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    let session = store.get_review(&session_ref)?;
    let comments = collect_comments(&session);
    serde_json::to_writer_pretty(&mut *out, &comments)?;
    writeln!(out)?;
    Ok(())
}

fn resolve_session_ref(store: &ReviewStore, repo: &Path, session: &str) -> Result<SessionRef> {
    let direct_path = PathBuf::from(session);
    if direct_path.exists() || direct_path.is_absolute() || session.ends_with(".json") {
        return Ok(SessionRef::from_path(direct_path));
    }

    // PR sessions are keyed by forge coordinates, not a local checkout, so they
    // resolve from the manifest by slug rather than the per-repo listing.
    if matches!(session.parse::<Slug>(), Ok(Slug::Pr(_))) {
        return match store.resolve_pr_session(session)? {
            Some(session_ref) => Ok(session_ref),
            None => Err(TuicrError::InvalidInput(format!(
                "no PR session found for '{session}'. Run `tuicr review list --all` to see available sessions."
            ))),
        };
    }

    let matches: Vec<_> = store
        .list_sessions_for_repo(repo)?
        .into_iter()
        .filter(|summary| summary.slug == session)
        .collect();
    match matches.as_slice() {
        [summary] => Ok(summary.session_ref.clone()),
        [] => Err(TuicrError::InvalidInput(format!(
            "session '{session}' was not found for repo {}. Run `tuicr review list --repo {}` to see available sessions.",
            repo.display(),
            repo.display()
        ))),
        _ => Err(TuicrError::InvalidInput(format!(
            "session '{session}' is ambiguous for repo {}",
            repo.display()
        ))),
    }
}

fn build_comment_target(
    file: Option<PathBuf>,
    line: Option<u32>,
    end_line: Option<u32>,
    side: LineSideArg,
) -> Result<CommentTarget> {
    let side = match side {
        LineSideArg::Old => LineSide::Old,
        LineSideArg::New => LineSide::New,
    };

    match (file, line, end_line) {
        (None, None, None) => Ok(CommentTarget::Review),
        (Some(path), None, None) => Ok(CommentTarget::File { path }),
        (Some(path), Some(line), None) => {
            validate_line(line, "--line")?;
            Ok(CommentTarget::Line { path, line, side })
        }
        (Some(path), Some(start), Some(end)) => {
            validate_line(start, "--line")?;
            validate_line(end, "--end-line")?;
            Ok(CommentTarget::LineRange {
                path,
                range: LineRange::new(start, end),
                side,
            })
        }
        (None, Some(_), _) => Err(TuicrError::InvalidInput(
            "--line requires --target-file for review comments".to_string(),
        )),
        (None, None, Some(_)) => Err(TuicrError::InvalidInput(
            "--end-line requires --line and --target-file".to_string(),
        )),
        (Some(_), None, Some(_)) => Err(TuicrError::InvalidInput(
            "--end-line requires --line".to_string(),
        )),
    }
}

fn validate_line(line: u32, name: &str) -> Result<()> {
    if line == 0 {
        return Err(TuicrError::InvalidInput(format!(
            "{name} must be greater than zero"
        )));
    }
    Ok(())
}

/// The threads waiting on `agent`: not settled, and the last thing said in them
/// was not said by the agent.
///
/// This is what "answer comments as they come in" runs on, and it is computed
/// from the review itself rather than from a mark the watcher keeps. An agent
/// that dies mid-round leaves its outstanding threads outstanding, so the next
/// watch hands them straight back; and an agent cannot wake itself, because its
/// own reply is what makes a thread answered.
///
/// The whole thread comes back, not just the message that needs answering — a
/// reply written without the conversation above it usually says the wrong
/// thing. A comment edited after it was answered does not come back: the last
/// word in it is still the agent's.
/// Resolve a comment id, or an unambiguous prefix of one, against what this
/// review shows — which includes comments carried in from commits it has
/// rewritten, and which the session file does not have.
fn resolve_stored_comment_id(
    cstore: &crate::persistence::comment_store::CommentStore,
    session: &ReviewSession,
    id_or_prefix: &str,
) -> Result<String> {
    let shown = comments_of(session);
    let exact = shown.iter().find(|c| c.id == id_or_prefix);
    if let Some(comment) = exact {
        return Ok(comment.id.clone());
    }
    let mut matches = shown.iter().filter(|c| c.id.starts_with(id_or_prefix));
    match (matches.next(), matches.next()) {
        (Some(comment), None) => Ok(comment.id.clone()),
        (Some(_), Some(_)) => Err(TuicrError::InvalidInput(format!(
            "comment id `{id_or_prefix}` is ambiguous"
        ))),
        _ => match cstore.find(id_or_prefix)? {
            Some(comment) => Ok(comment.id),
            None => Err(TuicrError::InvalidInput(format!(
                "no comment with id `{id_or_prefix}`"
            ))),
        },
    }
}

/// The commit a comment written now belongs to: the head of the range under
/// review, or the checkout when nothing is committed.
fn new_comment_scope(session: &ReviewSession) -> (crate::model::CommentScope, Option<String>) {
    let scopes = scopes_for(session);
    match scopes.iter().rev().find(|(scope, _)| scope.sha().is_some()) {
        Some((scope, summary)) => (
            scope.clone(),
            Some(summary.clone()).filter(|s| !s.is_empty()),
        ),
        None => (
            crate::model::CommentScope::working_tree(
                crate::persistence::comment_store::checkout_key(&session.repo_path),
            ),
            None,
        ),
    }
}

/// Write a comment straight to the comment store, the way the TUI does.
fn store_add_comment(
    cstore: &crate::persistence::comment_store::CommentStore,
    session: &ReviewSession,
    request: AddCommentRequest,
) -> Result<Comment> {
    let (scope, summary) = new_comment_scope(session);
    let mut comment = Comment::new(
        request.content.trim().to_string(),
        request.comment_type,
        match &request.target {
            CommentTarget::Line { side, .. } => Some(*side),
            CommentTarget::LineRange { side, .. } => Some(*side),
            _ => None,
        },
    );
    if comment.content.is_empty() {
        return Err(TuicrError::InvalidInput(
            "comment cannot be empty".to_string(),
        ));
    }
    comment.author = request.author;
    comment.commit_id = scope.sha().map(str::to_string);
    comment.line_context = request.line_context;
    comment.anchor = Some(match &request.target {
        CommentTarget::Review => crate::model::CommentAnchor::review(scope.clone()),
        CommentTarget::File { path } => {
            crate::model::CommentAnchor::file(scope.clone(), path.clone())
        }
        CommentTarget::Line { path, line, side } => {
            crate::model::CommentAnchor::line(scope.clone(), path.clone(), *line, *side)
        }
        CommentTarget::LineRange { path, range, side } => {
            comment.line_range = Some(*range);
            crate::model::CommentAnchor::line(scope.clone(), path.clone(), range.end, *side)
        }
    });
    cstore.add(&scope, summary.as_deref(), comment.clone())?;
    Ok(comment)
}

/// Reply in the store: the reply joins its root's thread, inherits its anchor
/// so the conversation stays in one file, and reopens it.
#[cfg(test)]
pub(crate) fn store_reply_for_test(
    cstore: &crate::persistence::comment_store::CommentStore,
    session: &ReviewSession,
    parent_id: &str,
    content: String,
    author: String,
) -> Result<Comment> {
    store_reply(cstore, session, parent_id, content, author)
}

fn store_reply(
    cstore: &crate::persistence::comment_store::CommentStore,
    session: &ReviewSession,
    parent_id: &str,
    content: String,
    author: String,
) -> Result<Comment> {
    let content = content.trim().to_string();
    if content.is_empty() {
        return Err(TuicrError::InvalidInput(
            "reply cannot be empty".to_string(),
        ));
    }
    let parent = cstore
        .find(parent_id)?
        .ok_or_else(|| TuicrError::InvalidInput(format!("no comment with id `{parent_id}`")))?;
    let root_id = parent
        .in_reply_to
        .clone()
        .unwrap_or_else(|| parent.id.clone());
    let root = cstore.find(&root_id)?.unwrap_or(parent);

    let mut reply = Comment::new(content, CommentType::None, root.side);
    reply.author = author;
    reply.in_reply_to = Some(root_id.clone());
    reply.anchor = root.anchor.clone();
    reply.commit_id = root.commit_id.clone();
    reply.line_context = root.line_context.clone();
    reply.line_range = root.line_range;
    reply.outdated = root.outdated;
    // A CLI reply was composed against the thread as the agent read it
    // earlier. When the reader settled the thread meanwhile, their resolve is
    // the later word: the reply joins the settled record instead of undoing
    // it — reopening here is how a review came back from :reload with every
    // settled thread standing open again. The TUI's own reply path still
    // reopens: there, someone is looking at the thread as they answer it.
    reply.resolved = root.resolved;

    let scope = root
        .anchor
        .as_ref()
        .map(|anchor| anchor.scope.clone())
        .unwrap_or_else(|| new_comment_scope(session).0);
    cstore.add(&scope, None, reply.clone())?;
    Ok(reply)
}

/// Settle or reopen a whole thread in the store.
fn store_set_thread_resolved(
    cstore: &crate::persistence::comment_store::CommentStore,
    id: &str,
    resolved: bool,
) -> Result<Comment> {
    let target = cstore
        .find(id)?
        .ok_or_else(|| TuicrError::InvalidInput(format!("no comment with id `{id}`")))?;
    let root_id = target
        .in_reply_to
        .clone()
        .unwrap_or_else(|| target.id.clone());
    let scope = target
        .anchor
        .as_ref()
        .map(|anchor| anchor.scope.clone())
        .ok_or_else(|| TuicrError::InvalidInput("comment has no anchor".to_string()))?;

    for comment in cstore.comments_for(std::slice::from_ref(&scope))? {
        if comment.id == root_id || comment.in_reply_to.as_deref() == Some(root_id.as_str()) {
            cstore.update_comment(&comment.id, |stored| stored.resolved = resolved)?;
        }
    }
    cstore
        .find(&root_id)?
        .ok_or_else(|| TuicrError::InvalidInput("thread root vanished".to_string()))
}

#[derive(Debug, Serialize)]
struct MigrateOutput {
    applied: bool,
    sessions: usize,
    comments: usize,
    stamped_from_range_head: usize,
    working_tree: usize,
    with_message: usize,
    repos: usize,
    skipped: Vec<String>,
    diverged: Vec<String>,
}

/// Copy comments out of the session files and into the per-commit store, then
/// switch the repositories over.
///
/// It verifies before it switches: every session is re-read and compared with
/// what the store now holds, and a single divergence leaves the repositories
/// exactly as they were. The session files keep their comments either way, so
/// undoing this is deleting a directory.
fn migrate_to_store(
    dry_run: bool,
    reviews_dir: Option<PathBuf>,
    out: &mut impl Write,
) -> Result<()> {
    let dir = match reviews_dir {
        Some(dir) => dir,
        None => crate::persistence::storage::get_reviews_dir()?,
    };
    let report = crate::persistence::migrate_comments::migrate_comments(
        &dir,
        !dry_run,
        &crate::persistence::migrate_comments::GitMessages::default(),
    )?;

    let mut diverged = Vec::new();
    let mut repos = 0;
    if !dry_run {
        diverged = crate::persistence::migrate_comments::verify_migration(&dir)?
            .into_iter()
            .map(|d| format!("{}: {} — {}", d.slug, d.comment_id, d.reason))
            .collect();
        if diverged.is_empty() {
            // Only now do reviews start reading and writing the store.
            let comments_root = dir.join("comments");
            if let Ok(entries) = std::fs::read_dir(&comments_root) {
                for entry in entries.flatten().filter(|e| e.path().is_dir()) {
                    let key = entry.file_name().to_string_lossy().to_string();
                    crate::persistence::comment_store::CommentStore::new(&dir, &key).take_over()?;
                    repos += 1;
                }
            }
        }
    }

    let output = MigrateOutput {
        applied: !dry_run && diverged.is_empty(),
        sessions: report.sessions,
        comments: report.comments,
        stamped_from_range_head: report.stamped_from_range_head,
        working_tree: report.working_tree,
        with_message: report.with_message,
        repos,
        skipped: report
            .skipped
            .iter()
            .map(|s| format!("{}: {}", s.slug, s.reason))
            .collect(),
        diverged,
    };
    serde_json::to_writer_pretty(&mut *out, &output)?;
    writeln!(out)?;
    Ok(())
}

/// The comment store for a session's repository, when that repository is on
/// the store.
///
/// The CLI and the TUI have to agree about where comments live, or an agent
/// writes where the reader is not looking.
fn store_for(session: &ReviewSession) -> Option<crate::persistence::comment_store::CommentStore> {
    let reviews_dir = crate::persistence::storage::get_reviews_dir().ok()?;
    let key = match session.pr_session_key.as_ref() {
        Some(pr) => format!("{}/{}", pr.repository.owner, pr.repository.name),
        None => {
            let (owner, repo) = crate::slug::resolve_owner_repo(&session.repo_path).ok()?;
            match owner {
                Some(owner) => format!("{owner}/{repo}"),
                None => repo,
            }
        }
    };
    let store = crate::persistence::comment_store::CommentStore::new(reviews_dir, &key);
    store.in_use().then_some(store)
}

/// The commits a session has in view, with their summaries — the summary is
/// what finds a thread again once its commit has been rewritten, so it is
/// worth a `git log` per commit here.
fn scopes_for(session: &ReviewSession) -> Vec<(crate::model::CommentScope, String)> {
    use crate::model::review::SessionDiffSource as Source;
    let mut scopes = Vec::new();
    if let Some(key) = session.pr_session_key.as_ref() {
        scopes.push((
            crate::model::CommentScope::commit(key.head_sha.clone()),
            String::new(),
        ));
    }
    if let Some(range) = session.commit_range.as_ref() {
        for sha in range {
            let summary = std::process::Command::new("git")
                .arg("-C")
                .arg(&session.repo_path)
                .args(["log", "-1", "--format=%s", sha])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .map(|text| text.trim().to_string())
                .unwrap_or_default();
            scopes.push((crate::model::CommentScope::commit(sha.clone()), summary));
        }
    }
    if matches!(
        session.diff_source,
        Source::WorkingTree
            | Source::Staged
            | Source::Unstaged
            | Source::StagedAndUnstaged
            | Source::WorkingTreeAndCommits
            | Source::StagedUnstagedAndCommits
    ) {
        scopes.push((
            crate::model::CommentScope::working_tree(
                crate::persistence::comment_store::checkout_key(&session.repo_path),
            ),
            String::new(),
        ));
    }
    scopes
}

/// Every comment this session shows: from the store when its repository is on
/// the store, from the session itself otherwise.
fn comments_of(session: &ReviewSession) -> Vec<crate::model::Comment> {
    let Some(store) = store_for(session) else {
        return session_comments(session);
    };
    let scopes = scopes_for(session);
    let shas: Vec<String> = scopes
        .iter()
        .filter_map(|(scope, _)| scope.sha().map(String::from))
        .collect();
    let predecessors = crate::vcs::git::lineage::predecessors(&session.repo_path, &shas)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let repo_path = session.repo_path.clone();
    let still_exists = move |sha: &str| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{sha}^{{commit}}"),
            ])
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    };
    match crate::persistence::comment_store::resolve_for_review(
        &store,
        &scopes,
        &predecessors,
        &still_exists,
    ) {
        Ok(resolved) => resolved.comments,
        Err(_) => session_comments(session),
    }
}

/// The comments the session file holds, in the order the review shows them.
fn session_comments(session: &ReviewSession) -> Vec<crate::model::Comment> {
    let mut out = session.review_comments.clone();
    let mut files: Vec<_> = session.files.iter().collect();
    files.sort_by_key(|(path, _)| path.as_os_str().to_os_string());
    for (_, review) in files {
        out.extend(review.file_comments.iter().cloned());
        let mut lines: Vec<_> = review.line_comments.keys().copied().collect();
        lines.sort_unstable();
        for line in lines {
            out.extend(review.line_comments[&line].iter().cloned());
        }
    }
    out
}

fn unanswered_threads(session: &ReviewSession, agent: &str) -> Vec<CommentOutput> {
    let comments = collect_comments(session);
    // Storage keeps a thread's messages together and in posted order, so the
    // last one in a group is the last word in that thread.
    let mut threads: Vec<(String, Vec<CommentOutput>)> = Vec::new();
    for comment in comments {
        let root = comment
            .in_reply_to
            .clone()
            .unwrap_or_else(|| comment.id.clone());
        match threads.iter_mut().find(|(id, _)| *id == root) {
            Some((_, thread)) => thread.push(comment),
            None => threads.push((root, vec![comment])),
        }
    }

    threads
        .into_iter()
        .filter(|(_, thread)| match thread.last() {
            Some(last) => !last.resolved && last.author != agent,
            None => false,
        })
        .flat_map(|(_, thread)| thread)
        .collect()
}

/// A stored comment's location, read off the anchor that says what it was
/// written against.
fn output_from_anchor(comment: &crate::model::Comment) -> CommentOutput {
    let Some(anchor) = comment.anchor.as_ref() else {
        return CommentOutput::from_parts("review".to_string(), None, None, None, None, comment);
    };
    let path = anchor
        .path
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
    let location = match (&path, anchor.line) {
        (Some(path), Some(line)) => format!("{path}:{line}"),
        (Some(path), None) => path.clone(),
        (None, _) => "review".to_string(),
    };
    CommentOutput::from_parts(
        location,
        path,
        anchor.line,
        anchor.line,
        anchor.line.map(|_| anchor.side),
        comment,
    )
}

fn collect_comments(session: &ReviewSession) -> Vec<CommentOutput> {
    // Once a repository is on the store, that is where the review's comments
    // are — including the ones carried in from commits it has since rewritten,
    // which the session has never heard of.
    if store_for(session).is_some() {
        return comments_of(session)
            .iter()
            .map(output_from_anchor)
            .collect();
    }
    let mut comments = Vec::new();
    for comment in &session.review_comments {
        comments.push(CommentOutput::from_parts(
            "review".to_string(),
            None,
            None,
            None,
            None,
            comment,
        ));
    }

    let mut files: Vec<_> = session.files.iter().collect();
    files.sort_by_key(|(path, _)| path.as_os_str().to_os_string());
    for (path, review) in files {
        let path_display = path.to_string_lossy().to_string();
        for comment in &review.file_comments {
            comments.push(CommentOutput::from_parts(
                path_display.clone(),
                Some(path_display.clone()),
                None,
                None,
                None,
                comment,
            ));
        }

        let mut line_comments: Vec<_> = review.line_comments.iter().collect();
        line_comments.sort_by_key(|(line, _)| *line);
        for (line, line_comments) in line_comments {
            for comment in line_comments {
                let (start_line, end_line) = comment
                    .line_range
                    .map(|range| (range.start, range.end))
                    .unwrap_or((*line, *line));
                let location = line_location(&path_display, start_line, end_line, comment.side);
                comments.push(CommentOutput::from_parts(
                    location,
                    Some(path_display.clone()),
                    Some(start_line),
                    Some(end_line),
                    comment.side,
                    comment,
                ));
            }
        }
    }

    comments
}

fn line_location(path: &str, start_line: u32, end_line: u32, side: Option<LineSide>) -> String {
    let line = if start_line == end_line {
        start_line.to_string()
    } else {
        format!("{start_line}-{end_line}")
    };
    match side {
        Some(LineSide::Old) => format!("{path}:{line} [old]"),
        _ => format!("{path}:{line}"),
    }
}

fn target_location(target: &CommentTarget) -> String {
    match target {
        CommentTarget::Review => "review".to_string(),
        CommentTarget::File { path } => path.display().to_string(),
        CommentTarget::Line { path, line, side } => {
            line_location(&path.to_string_lossy(), *line, *line, Some(*side))
        }
        CommentTarget::LineRange { path, range, side } => {
            line_location(&path.to_string_lossy(), range.start, range.end, Some(*side))
        }
    }
}

fn side_id(side: Option<LineSide>) -> Option<&'static str> {
    match side {
        Some(LineSide::Old) => Some("old"),
        Some(LineSide::New) => Some("new"),
        None => None,
    }
}

fn lifecycle_id(state: CommentLifecycleState) -> &'static str {
    match state {
        CommentLifecycleState::LocalDraft => "local_draft",
        CommentLifecycleState::PushedDraft => "pushed_draft",
        CommentLifecycleState::Submitted => "submitted",
    }
}

#[derive(Debug, Serialize)]
struct SessionSummaryOutput {
    slug: String,
    kind: &'static str,
    path: String,
    updated_at: String,
    comment_count: usize,
    reviewed_count: usize,
    file_count: usize,
    anchor: String,
    active: bool,
}

impl From<SessionSummary> for SessionSummaryOutput {
    fn from(summary: SessionSummary) -> Self {
        Self {
            slug: summary.slug,
            kind: summary.kind.id(),
            path: summary.session_ref.path().display().to_string(),
            updated_at: summary.updated_at.to_rfc3339(),
            comment_count: summary.comment_count,
            reviewed_count: summary.reviewed_count,
            file_count: summary.file_count,
            anchor: summary.anchor,
            active: summary.active,
        }
    }
}

#[derive(Debug, Serialize)]
struct CommentOutput {
    id: String,
    location: String,
    path: Option<String>,
    start_line: Option<u32>,
    end_line: Option<u32>,
    side: Option<&'static str>,
    comment_type: String,
    lifecycle_state: &'static str,
    created_at: String,
    /// Who wrote the comment. Agents pass `--username`; the user's own
    /// comments carry the config `username` or `"user"`. Callers use this to
    /// tell their own replies from the comments they still have to answer.
    author: String,
    /// Set on replies: the id of the comment this one answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    in_reply_to: Option<String>,
    /// Whether this comment's thread is settled. Callers answering a review
    /// skip resolved threads.
    resolved: bool,
    /// True when the code this comment was written against is gone — an amend
    /// or rebase moved past it. The comment still stands; its anchor does not.
    outdated: bool,
    content: String,
}

impl CommentOutput {
    fn from_target(target: &CommentTarget, comment: &Comment) -> Self {
        let (path, start_line, end_line, side) = match target {
            CommentTarget::Review => (None, None, None, None),
            CommentTarget::File { path } => (Some(path.display().to_string()), None, None, None),
            CommentTarget::Line { path, line, side } => (
                Some(path.display().to_string()),
                Some(*line),
                Some(*line),
                Some(*side),
            ),
            CommentTarget::LineRange { path, range, side } => (
                Some(path.display().to_string()),
                Some(range.start),
                Some(range.end),
                Some(*side),
            ),
        };
        Self::from_parts(
            target_location(target),
            path,
            start_line,
            end_line,
            side,
            comment,
        )
    }

    fn from_parts(
        location: String,
        path: Option<String>,
        start_line: Option<u32>,
        end_line: Option<u32>,
        side: Option<LineSide>,
        comment: &Comment,
    ) -> Self {
        Self {
            id: comment.id.clone(),
            location,
            path,
            start_line,
            end_line,
            side: side_id(side),
            comment_type: comment.comment_type.id().to_string(),
            lifecycle_state: lifecycle_id(comment.lifecycle_state),
            created_at: comment.created_at.to_rfc3339(),
            author: comment.author.clone(),
            in_reply_to: comment.in_reply_to.clone(),
            resolved: comment.resolved,
            outdated: comment.outdated,
            content: comment.content.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
    fn should_build_review_comment_target_by_default() {
        let target = build_comment_target(None, None, None, LineSideArg::New).unwrap();
        assert!(matches!(target, CommentTarget::Review));
    }

    #[test]
    fn should_build_line_range_comment_target() {
        let target = build_comment_target(
            Some(PathBuf::from("src/main.rs")),
            Some(12),
            Some(10),
            LineSideArg::Old,
        )
        .unwrap();

        assert!(matches!(
            target,
            CommentTarget::LineRange {
                range: LineRange { start: 10, end: 12 },
                side: LineSide::Old,
                ..
            }
        ));
    }

    #[test]
    fn should_reject_zero_line() {
        let err = build_comment_target(
            Some(PathBuf::from("src/main.rs")),
            Some(0),
            None,
            LineSideArg::New,
        )
        .unwrap_err();
        assert!(matches!(err, TuicrError::InvalidInput(_)));
    }

    #[test]
    fn should_build_add_request_from_flat_json_payload() {
        let parts = build_add_request_parts(AddCommentOptions {
            input: Some(
                r#"{"file":"src/main.rs","line":42,"side":"old","type":"issue","content":"fix it"}"#
                    .to_string(),
            ),
            comment_type: "note".to_string(),
            file: None,
            line: None,
            end_line: None,
            side: LineSideArg::New,
            username: None,
            content: None,
        })
        .unwrap();

        assert_eq!(parts.comment_type, "issue");
        assert_eq!(parts.content, "fix it");
        assert!(matches!(
            parts.target,
            CommentTarget::Line {
                path,
                line: 42,
                side: LineSide::Old,
            } if path.as_path() == Path::new("src/main.rs")
        ));
    }

    #[test]
    fn should_build_add_request_from_nested_json_payload() {
        let parts = build_add_request_parts(AddCommentOptions {
            input: Some(
                r#"{"comment_type":"suggestion","content":"collapse this","target":{"type":"line_range","file":"src/main.rs","start_line":5,"end_line":7}}"#
                    .to_string(),
            ),
            comment_type: "note".to_string(),
            file: None,
            line: None,
            end_line: None,
            side: LineSideArg::New,
            username: None,
            content: None,
        })
        .unwrap();

        assert_eq!(parts.comment_type, "suggestion");
        assert!(matches!(
            parts.target,
            CommentTarget::LineRange {
                range: LineRange { start: 5, end: 7 },
                side: LineSide::New,
                ..
            }
        ));
    }

    fn save_pr_session(store: &ReviewStore) -> SessionRef {
        use crate::forge::traits::{ForgeRepository, PrSessionKey};

        let key = PrSessionKey::new(
            ForgeRepository::github("github.com", "slatedb", "slatedb"),
            1745,
            "43e3566924690c06a45b2177b4dd2df59a0f09c6".to_string(),
        );
        let mut session = ReviewSession::new(
            PathBuf::from("forge:github.com/slatedb/slatedb"),
            key.head_sha.clone(),
            Some("reviews".to_string()),
            SessionDiffSource::PullRequest,
        );
        session.pr_session_key = Some(key);
        store.save_review(&session).unwrap()
    }

    #[test]
    fn should_find_pr_session_by_repo_coordinate() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        let session_ref = save_pr_session(&store);

        // A bare repo coordinate surfaces the PR session and emits its slug.
        let listed = store
            .list_sessions_for_repo(Path::new("slatedb/slatedb"))
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].slug, "gh:slatedb/slatedb/pr/1745");
        assert_eq!(listed[0].kind, crate::review_store::SessionKind::Pr);

        // The emitted slug resolves the same way regardless of --repo.
        let resolved =
            resolve_session_ref(&store, Path::new("slatedb/slatedb"), &listed[0].slug).unwrap();
        assert_eq!(resolved, session_ref);
    }

    #[test]
    fn should_match_pr_session_via_forge_repo_path_coordinate() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        save_pr_session(&store);

        // The `forge:host/owner/repo` form (as stored on disk) also resolves.
        let listed = store
            .list_sessions_for_repo(Path::new("forge:github.com/slatedb/slatedb"))
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].slug, "gh:slatedb/slatedb/pr/1745");
    }

    #[test]
    fn should_not_match_pr_session_for_unrelated_repo() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        save_pr_session(&store);

        assert!(
            store
                .list_sessions_for_repo(Path::new("other/project"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn should_list_pr_session_in_list_all() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        save_pr_session(&store);

        let all = store.list_all_sessions().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].slug, "gh:slatedb/slatedb/pr/1745");
    }

    #[test]
    fn should_resolve_pr_session_by_slug_without_repo() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        let session_ref = save_pr_session(&store);

        // PR slugs are self-contained: `--repo` is irrelevant.
        let resolved =
            resolve_session_ref(&store, Path::new("."), "gh:slatedb/slatedb/pr/1745").unwrap();
        assert_eq!(resolved, session_ref);
    }

    #[test]
    fn should_error_for_unknown_pr_slug() {
        let temp = tempdir().unwrap();
        let reviews = temp.path().join("reviews");
        let store = ReviewStore::with_reviews_dir(&reviews);
        let err = resolve_session_ref(&store, Path::new("."), "gh:nope/nope/pr/9999").unwrap_err();
        assert!(matches!(err, TuicrError::InvalidInput(_)));
    }

    const AGENT: &str = "Claude Opus 5";

    fn session_with_thread() -> (ReviewSession, String) {
        let mut session = test_session(PathBuf::from("/repo"));
        let comment = crate::review_store::add_comment_to_session(
            &mut session,
            AddCommentRequest::new(
                CommentTarget::Line {
                    path: PathBuf::from("src/main.rs"),
                    line: 42,
                    side: LineSide::New,
                },
                "why this?".to_string(),
                CommentType::from_id("issue"),
                "thiblahute".to_string(),
            ),
        )
        .unwrap();
        (session, comment.id)
    }

    fn answer(session: &mut ReviewSession, parent: &str, body: &str, author: &str) {
        crate::review_store::reply_to_comment_in_session(
            session,
            ReplyRequest {
                parent_id: parent.to_string(),
                content: body.to_string(),
                author: author.to_string(),
                reopen: false,
            },
        )
        .unwrap();
    }

    /// A session whose repository is on the comment store, with the reviews
    /// dir pointed at `dir` so the CLI and the assertions agree.
    fn session_on_store(dir: &std::path::Path, repo: &std::path::Path) -> ReviewSession {
        crate::persistence::storage::set_test_reviews_dir(Some(dir.to_path_buf()));
        let mut session = ReviewSession::new(
            repo.to_path_buf(),
            "headsha".to_string(),
            Some("main".to_string()),
            crate::model::review::SessionDiffSource::CommitRange,
        );
        session.commit_range = Some(vec!["aaaa1111".to_string()]);
        let key = repo.file_name().unwrap().to_string_lossy().to_string();
        crate::persistence::comment_store::CommentStore::new(dir, &key)
            .take_over()
            .unwrap();
        session
    }

    #[test]
    fn should_add_a_cli_comment_to_the_store() {
        // An agent writing into a migrated repo must write where the reader
        // reads, or the comment is invisible and erased on the next open.
        let dir = tempdir().unwrap();
        let repo = dir.path().join("checkout");
        std::fs::create_dir_all(&repo).unwrap();
        let session = session_on_store(dir.path(), &repo);
        let cstore = store_for(&session).expect("the repo is on the store");

        let written = store_add_comment(
            &cstore,
            &session,
            AddCommentRequest::new(
                CommentTarget::Line {
                    path: PathBuf::from("src/main.rs"),
                    line: 42,
                    side: LineSide::New,
                },
                "from an agent".to_string(),
                CommentType::from_id("issue"),
                "Claude".to_string(),
            ),
        )
        .unwrap();

        let held = cstore
            .comments_for(&[crate::model::CommentScope::commit("aaaa1111")])
            .unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].id, written.id);
        assert_eq!(held[0].anchor.as_ref().unwrap().line, Some(42));
        assert_eq!(held[0].author, "Claude");
    }

    #[test]
    fn should_reply_into_the_stored_thread_without_reopening_it() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("checkout");
        std::fs::create_dir_all(&repo).unwrap();
        let session = session_on_store(dir.path(), &repo);
        let cstore = store_for(&session).unwrap();
        let root = store_add_comment(
            &cstore,
            &session,
            AddCommentRequest::new(
                CommentTarget::File {
                    path: PathBuf::from("src/main.rs"),
                },
                "why this?".to_string(),
                CommentType::from_id("issue"),
                "user".to_string(),
            ),
        )
        .unwrap();
        store_set_thread_resolved(&cstore, &root.id, true).unwrap();

        let reply = store_reply(
            &cstore,
            &session,
            &root.id,
            "because of the empty case".to_string(),
            "Claude".to_string(),
        )
        .unwrap();

        let held = cstore
            .comments_for(&[crate::model::CommentScope::commit("aaaa1111")])
            .unwrap();
        assert_eq!(held.len(), 2, "the thread lives in one file");
        assert_eq!(reply.in_reply_to.as_deref(), Some(root.id.as_str()));
        // The reader settled the thread before this reply landed: their
        // resolve is the later word, and the reply joins the settled record.
        // Reopening here is how a review came back from :reload with every
        // settled thread standing open again.
        assert!(
            held.iter().all(|c| c.resolved),
            "a CLI reply must not reopen a thread the reader settled"
        );
    }

    #[test]
    fn should_leave_an_open_thread_open_when_a_cli_reply_lands() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("checkout");
        std::fs::create_dir_all(&repo).unwrap();
        let session = session_on_store(dir.path(), &repo);
        let cstore = store_for(&session).unwrap();
        let root = store_add_comment(
            &cstore,
            &session,
            AddCommentRequest::new(
                CommentTarget::File {
                    path: PathBuf::from("src/main.rs"),
                },
                "why this?".to_string(),
                CommentType::from_id("issue"),
                "user".to_string(),
            ),
        )
        .unwrap();

        let reply = store_reply(
            &cstore,
            &session,
            &root.id,
            "because of the empty case".to_string(),
            "Claude".to_string(),
        )
        .unwrap();
        assert!(!reply.resolved);

        let held = cstore
            .comments_for(&[crate::model::CommentScope::commit("aaaa1111")])
            .unwrap();
        assert!(
            held.iter().all(|c| !c.resolved),
            "still an open conversation"
        );
    }

    #[test]
    fn should_resolve_a_stored_thread_through_the_cli() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("checkout");
        std::fs::create_dir_all(&repo).unwrap();
        let session = session_on_store(dir.path(), &repo);
        let cstore = store_for(&session).unwrap();
        let root = store_add_comment(
            &cstore,
            &session,
            AddCommentRequest::new(
                CommentTarget::Review,
                "about the whole change".to_string(),
                CommentType::from_id("note"),
                "user".to_string(),
            ),
        )
        .unwrap();
        store_reply(
            &cstore,
            &session,
            &root.id,
            "noted".to_string(),
            "Claude".to_string(),
        )
        .unwrap();

        store_set_thread_resolved(&cstore, &root.id, true).unwrap();

        let held = cstore
            .comments_for(&[crate::model::CommentScope::commit("aaaa1111")])
            .unwrap();
        assert_eq!(held.len(), 2);
        assert!(held.iter().all(|c| c.resolved), "the whole thread settles");
    }

    #[test]
    fn should_read_a_migrated_review_through_the_store() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("checkout");
        std::fs::create_dir_all(&repo).unwrap();
        let session = session_on_store(dir.path(), &repo);
        let cstore = store_for(&session).unwrap();
        store_add_comment(
            &cstore,
            &session,
            AddCommentRequest::new(
                CommentTarget::Line {
                    path: PathBuf::from("src/main.rs"),
                    line: 7,
                    side: LineSide::New,
                },
                "look here".to_string(),
                CommentType::from_id("issue"),
                "user".to_string(),
            ),
        )
        .unwrap();

        let shown = collect_comments(&session);

        assert_eq!(shown.len(), 1, "review comments reads the store");
        assert_eq!(shown[0].location, "src/main.rs:7");
        assert_eq!(shown[0].start_line, Some(7));
    }

    #[test]
    fn should_hand_back_a_thread_nobody_has_answered() {
        let (session, _root) = session_with_thread();
        let waiting = unanswered_threads(&session, AGENT);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].content, "why this?");
    }

    #[test]
    fn should_not_let_an_agent_wake_itself() {
        // The agent's own reply is what marks a thread answered. Without that
        // an incremental watch would fire on every reply it wrote.
        let (mut session, root) = session_with_thread();
        answer(&mut session, &root, "fixed, amended into the commit", AGENT);

        assert!(unanswered_threads(&session, AGENT).is_empty());
    }

    #[test]
    fn should_hand_back_a_thread_the_reviewer_came_back_to() {
        let (mut session, root) = session_with_thread();
        answer(&mut session, &root, "fixed", AGENT);
        answer(
            &mut session,
            &root,
            "not quite — see the second case",
            "thiblahute",
        );

        let waiting = unanswered_threads(&session, AGENT);
        assert_eq!(waiting.len(), 3, "the whole conversation comes back");
        assert_eq!(waiting[2].content, "not quite — see the second case");
    }

    #[test]
    fn should_leave_a_settled_thread_alone() {
        let (mut session, root) = session_with_thread();
        crate::review_store::set_thread_resolved(&mut session, &root, true).unwrap();

        assert!(
            unanswered_threads(&session, AGENT).is_empty(),
            "resolving is the other way to finish with a thread"
        );
    }

    #[test]
    fn should_hand_back_only_the_threads_still_waiting() {
        let (mut session, first) = session_with_thread();
        answer(&mut session, &first, "done", AGENT);
        crate::review_store::add_comment_to_session(
            &mut session,
            AddCommentRequest::new(
                CommentTarget::File {
                    path: PathBuf::from("src/main.rs"),
                },
                "and this file needs a test".to_string(),
                CommentType::from_id("issue"),
                "thiblahute".to_string(),
            ),
        )
        .unwrap();

        let waiting = unanswered_threads(&session, AGENT);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].content, "and this file needs a test");
    }

    #[test]
    fn should_list_add_and_show_comments() {
        let temp = tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let reviews = temp.path().join("reviews");
        let store = ReviewStore::with_reviews_dir(&reviews);
        let session = test_session(repo.clone());
        let session_ref = store.save_review(&session).unwrap();

        let mut out = Vec::new();
        let sessions = store.list_sessions_for_repo(&repo).unwrap();
        assert_eq!(sessions.len(), 1);
        let slug = sessions[0].slug.clone();

        let resolved = resolve_session_ref(&store, &repo, &slug).unwrap();
        assert_eq!(resolved, session_ref);

        let comment = store
            .add_comment(
                &resolved,
                AddCommentRequest {
                    target: CommentTarget::Line {
                        path: PathBuf::from("src/main.rs"),
                        line: 42,
                        side: LineSide::New,
                    },
                    content: "check this".to_string(),
                    comment_type: CommentType::from_id("issue"),
                    author: crate::model::comment::DEFAULT_AUTHOR.to_string(),
                    line_context: None,
                    commit_id: None,
                },
            )
            .unwrap();

        let loaded = store.get_review(&session_ref).unwrap();
        let comments = collect_comments(&loaded);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, comment.id);
        assert_eq!(comments[0].location, "src/main.rs:42");
        assert_eq!(comments[0].comment_type, "issue");

        show_comments(&session_ref.path().display().to_string(), &repo, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value[0]["comment_type"], "issue");
        assert_eq!(value[0]["location"], "src/main.rs:42");
        assert_eq!(value[0]["content"], "check this");
    }

    fn config_with_types(ids: &[&str]) -> config::AppConfig {
        config::AppConfig {
            comment_types: Some(
                ids.iter()
                    .map(|id| config::CommentTypeConfig {
                        id: (*id).to_string(),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn should_warn_when_type_is_not_configured() {
        // given a config declaring note and issue
        let config = config_with_types(&["note", "issue"]);

        // when a typo slips through
        let warning = unknown_comment_type_warning(&CommentType::from_id("isue"), Some(&config));

        // then the id and the valid ids are both named
        let warning = warning.expect("an unconfigured type should warn");
        assert!(warning.contains("'isue'"), "got {warning}");
        assert!(warning.contains("note, issue"), "got {warning}");
    }

    #[test]
    fn should_not_warn_for_a_configured_type() {
        let config = config_with_types(&["note", "issue"]);
        assert!(
            unknown_comment_type_warning(&CommentType::from_id("issue"), Some(&config)).is_none()
        );
    }

    #[test]
    fn should_not_warn_when_comment_types_are_unconfigured() {
        // `comment_types` is unset by default, which leaves every id but
        // `none` undefined. Warning there would fire on every typed comment
        // the agent skill documents, so an absent list means no opinion.
        assert!(unknown_comment_type_warning(&CommentType::from_id("issue"), None).is_none());
        assert!(
            unknown_comment_type_warning(
                &CommentType::from_id("issue"),
                Some(&config::AppConfig::default())
            )
            .is_none()
        );
    }

    #[test]
    fn should_not_warn_for_the_untyped_default() {
        // `--type none` is always valid and never carries a badge.
        let config = config_with_types(&["issue"]);
        assert!(
            unknown_comment_type_warning(&CommentType::from_id("none"), Some(&config)).is_none()
        );
    }

    #[test]
    fn should_still_store_a_comment_whose_type_is_unconfigured() {
        // given a session and a type no config declares
        let dir = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(dir.path());
        let session = test_session(PathBuf::from("/tmp/repo"));
        let session_ref = store.save_review(&session).unwrap();

        // when
        let comment = store
            .add_comment(
                &session_ref,
                AddCommentRequest {
                    target: CommentTarget::File {
                        path: PathBuf::from("src/main.rs"),
                    },
                    content: "body".to_string(),
                    comment_type: CommentType::from_id("isue"),
                    author: "Codex".to_string(),
                    commit_id: None,
                    line_context: None,
                },
            )
            .expect("an unconfigured type must not block the write");

        // then the warning is advisory only — the comment is still stored
        assert_eq!(comment.comment_type.id(), "isue");
    }

    #[test]
    fn should_build_reply_parts_from_flags() {
        let parts = build_reply_parts(ReplyOptions {
            comment_id: Some("  abc-123  ".to_string()),
            input: None,
            username: Some("Claude".to_string()),
            content: Some("  fixed in def4567  ".to_string()),
        })
        .unwrap();

        assert_eq!(parts.parent_id, "abc-123");
        assert_eq!(parts.content, "fixed in def4567");
        assert_eq!(parts.username.as_deref(), Some("Claude"));
    }

    #[test]
    fn should_build_reply_parts_from_json_using_the_in_reply_to_alias() {
        let parts = build_reply_parts(ReplyOptions {
            comment_id: None,
            input: Some(
                r#"{"in_reply_to":"abc-123","content":"done","username":"Claude"}"#.to_string(),
            ),
            username: None,
            content: None,
        })
        .unwrap();

        assert_eq!(parts.parent_id, "abc-123");
        assert_eq!(parts.content, "done");
        assert_eq!(parts.username.as_deref(), Some("Claude"));
    }

    #[test]
    fn should_reject_a_reply_without_a_parent() {
        let err = build_reply_parts(ReplyOptions {
            comment_id: None,
            input: None,
            username: None,
            content: Some("done".to_string()),
        })
        .unwrap_err();
        assert!(matches!(err, TuicrError::InvalidInput(_)), "{err:?}");
    }

    #[test]
    fn should_report_author_and_reply_link_in_comment_output() {
        let temp = tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let reviews = temp.path().join("reviews");
        let store = ReviewStore::with_reviews_dir(&reviews);
        let session_ref = store.save_review(&test_session(repo.clone())).unwrap();

        let root = store
            .add_comment(
                &session_ref,
                AddCommentRequest {
                    target: CommentTarget::Line {
                        path: PathBuf::from("src/main.rs"),
                        line: 42,
                        side: LineSide::New,
                    },
                    content: "check this".to_string(),
                    comment_type: CommentType::from_id("issue"),
                    author: comment::DEFAULT_AUTHOR.to_string(),
                    line_context: None,
                    commit_id: None,
                },
            )
            .unwrap();
        let reply = store
            .reply_to_comment(
                &session_ref,
                ReplyRequest {
                    parent_id: root.id.clone(),
                    content: "fixed in def4567".to_string(),
                    author: "Claude".to_string(),
                    reopen: true,
                },
            )
            .unwrap();

        let mut out = Vec::new();
        show_comments(&session_ref.path().display().to_string(), &repo, &mut out).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();

        assert_eq!(value[0]["id"], root.id);
        assert_eq!(value[0]["author"], comment::DEFAULT_AUTHOR);
        // A root carries no in_reply_to, so the field is omitted entirely.
        assert!(value[0].get("in_reply_to").is_none());

        assert_eq!(value[1]["id"], reply.id);
        assert_eq!(value[1]["author"], "Claude");
        assert_eq!(value[1]["in_reply_to"], root.id);
        // The reply is reported at the root's anchor, not as a stray comment.
        assert_eq!(value[1]["location"], "src/main.rs:42");
    }

    #[test]
    fn should_resolve_a_comment_id_from_an_unambiguous_prefix() {
        let mut session = test_session(PathBuf::from("/repo"));
        let mut first = Comment::new("a".to_string(), CommentType::None, None);
        first.id = "abc12345-0000-0000-0000-000000000000".to_string();
        let mut second = Comment::new("b".to_string(), CommentType::None, None);
        second.id = "def67890-0000-0000-0000-000000000000".to_string();
        session.review_comments.push(first);
        session
            .get_file_mut(&PathBuf::from("src/main.rs"))
            .unwrap()
            .add_line_comment(7, second);

        assert_eq!(
            resolve_comment_id(&session, "abc1").unwrap(),
            "abc12345-0000-0000-0000-000000000000"
        );
        // A line comment is reachable by prefix too, not just review scope.
        assert_eq!(
            resolve_comment_id(&session, "def").unwrap(),
            "def67890-0000-0000-0000-000000000000"
        );
        // A full id still resolves to itself.
        assert_eq!(
            resolve_comment_id(&session, "abc12345-0000-0000-0000-000000000000").unwrap(),
            "abc12345-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn should_reject_an_ambiguous_comment_id_prefix() {
        let mut session = test_session(PathBuf::from("/repo"));
        for suffix in ["1111", "2222"] {
            let mut comment = Comment::new("x".to_string(), CommentType::None, None);
            comment.id = format!("abc12345-0000-0000-0000-00000000{suffix}");
            session.review_comments.push(comment);
        }

        let err = resolve_comment_id(&session, "abc").unwrap_err();
        let TuicrError::InvalidInput(message) = &err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        // The message has to name the candidates, or the caller cannot pick.
        assert!(message.contains("ambiguous"), "{message}");
        assert!(message.contains("00001111"), "{message}");
        assert!(message.contains("00002222"), "{message}");
    }

    #[test]
    fn should_reject_a_comment_id_that_matches_nothing() {
        let session = test_session(PathBuf::from("/repo"));
        let err = resolve_comment_id(&session, "nope").unwrap_err();
        assert!(matches!(err, TuicrError::InvalidInput(_)), "{err:?}");
    }
}
