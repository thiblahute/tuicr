//! CLI argument parsing, backed by `clap`.
//!
//! The struct [`Cli`] is the clap-derived parser; [`CliArgs`] is the simple
//! POJO the rest of the binary consumes. Conversion lives in `From<Cli>`.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::theme::{AppearanceArg, ThemeArg};

/// CLI arguments consumed by the rest of the binary.
#[derive(Debug, Clone, Default)]
pub struct CliArgs {
    pub theme: Option<String>,
    pub appearance: Option<AppearanceArg>,
    /// Output to stdout instead of clipboard when exporting.
    pub output_to_stdout: bool,
    /// Skip checking for updates on startup.
    pub no_update_check: bool,
    /// Commit/revision range to review.
    pub revisions: Option<String>,
    /// Skip commit selector and review uncommitted changes directly.
    pub working_tree: bool,
    /// Diff the working tree against this base revision (like `git diff BASE`).
    pub working_tree_base: Option<String>,
    /// Review only staged changes (like `git diff --staged`).
    pub staged: bool,
    /// Exclude untracked files from the working-tree diff (like `git diff`).
    pub no_untracked: bool,
    /// Include untracked files in the working-tree diff (overrides config).
    pub untracked: bool,
    /// Exclude staged changes from the working-tree diff (like `git diff`).
    pub no_staged: bool,
    /// Include staged changes in the working-tree diff (overrides config).
    pub with_staged: bool,
    /// Filter diff to a specific file or directory path.
    pub path_filter: Option<String>,
    /// Open a single file or directory for annotation (no VCS required).
    pub file_path: Option<String>,
    /// Whole-repo annotation mode.
    pub all_files: bool,
    /// Direct PR target from `tuicr pr <target>`.
    pub pr_target: Option<String>,
    /// Override the GitHub repo used for PR operations.
    pub repo_url: Option<String>,
    /// Non-interactive review session operation.
    pub review_command: Option<ReviewCommand>,
    /// Update the installed tuicr binary and exit.
    pub update_command: bool,
    /// Exact version requested by `tuicr update`, if any.
    pub update_version: Option<semver::Version>,
}

#[derive(Parser, Debug)]
#[command(
    name = "tuicr",
    version,
    about = "A code review TUI with vim keybindings. Export to GitHub or clipboard.",
    after_help = "Press ? in the application for keybinding help.",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(flatten)]
    tui_options: TuiOptions,

    /// Base revision to diff the working tree against, like `git diff BASE`.
    /// Requires -w. An explicit range (`A..B`) shows the flat diff between
    /// its endpoints instead, like `git diff A..B`.
    #[arg(
        value_name = "BASE",
        requires = "working_tree",
        conflicts_with = "revisions"
    )]
    working_tree_base: Option<String>,

    #[command(subcommand)]
    command: Option<Subcmd>,
}

/// Options that launch or configure the interactive TUI.
#[derive(Args, Debug, Clone, Default)]
struct TuiOptions {
    /// Commit range / revset to review (syntax depends on VCS backend).
    #[arg(
        short = 'r',
        long = "revisions",
        value_name = "REVSET",
        allow_hyphen_values = true
    )]
    revisions: Option<String>,

    /// Color theme to use. Bundled themes resolve first; local themes are
    /// loaded from the config `themes/` directory.
    #[arg(long, value_name = "THEME", value_parser = non_empty_theme_name)]
    theme: Option<String>,

    /// Appearance mode (light/dark/system); used when no explicit theme is set.
    #[arg(long, value_name = "MODE", value_parser = parse_appearance_arg)]
    appearance: Option<AppearanceArg>,

    /// Filter diff to a specific file or directory.
    #[arg(
        short = 'p',
        long = "path",
        value_name = "PATH",
        value_parser = non_empty_path,
        conflicts_with_all = ["file_path", "all_files"],
    )]
    path_filter: Option<String>,

    /// Include uncommitted changes (skip commit selector when used alone;
    /// combine with commits when used with -r).
    #[arg(
        short = 'w',
        long = "working-tree",
        action = ArgAction::SetTrue,
        conflicts_with_all = ["file_path", "all_files"],
    )]
    working_tree: bool,

    /// Review only staged changes (`git diff --staged`), skipping the commit
    /// selector.
    #[arg(
        long = "staged",
        action = ArgAction::SetTrue,
        conflicts_with_all = ["file_path", "all_files", "working_tree", "revisions"],
    )]
    staged: bool,

    /// Exclude untracked files from the working-tree diff, reviewing only
    /// tracked uncommitted changes (like `git diff HEAD`). Pair with `-w`.
    #[arg(
        long = "no-untracked",
        action = ArgAction::SetTrue,
        conflicts_with_all = ["file_path", "all_files"],
    )]
    no_untracked: bool,

    /// Include untracked files in the working-tree diff. Overrides the
    /// `show_untracked` config for this run.
    #[arg(
        long = "untracked",
        action = ArgAction::SetTrue,
        conflicts_with_all = ["file_path", "all_files", "no_untracked"],
    )]
    untracked: bool,

    /// Exclude staged changes from the working-tree diff, reviewing only
    /// unstaged changes (like `git diff`). Pair with `-w`.
    #[arg(
        long = "no-staged",
        action = ArgAction::SetTrue,
        conflicts_with_all = ["file_path", "all_files", "staged"],
    )]
    no_staged: bool,

    /// Include staged changes in the working-tree diff. Overrides the
    /// `show_staged` config for this run.
    #[arg(
        long = "with-staged",
        action = ArgAction::SetTrue,
        conflicts_with_all = ["file_path", "all_files", "staged", "no_staged"],
    )]
    with_staged: bool,

    /// Open a file or directory for annotation (no VCS required).
    #[arg(
        long = "file",
        value_name = "PATH",
        value_parser = non_empty_path,
        conflicts_with_all = ["path_filter", "revisions", "working_tree", "all_files"],
    )]
    file_path: Option<String>,

    /// Review every tracked file in the cwd's git repo.
    #[arg(
        short = 'A',
        long = "all-files",
        action = ArgAction::SetTrue,
        conflicts_with_all = ["path_filter", "revisions", "working_tree", "file_path"],
    )]
    all_files: bool,

    /// Output to stdout instead of clipboard when exporting.
    #[arg(long = "stdout", action = ArgAction::SetTrue)]
    stdout: bool,

    /// Skip checking for updates on startup.
    #[arg(long = "no-update-check", action = ArgAction::SetTrue)]
    no_update_check: bool,

    /// Override the forge repo for PR operations. Accepts GitHub, GitLab, or
    /// Azure DevOps URLs (HTTPS, SCP-style SSH, or ssh:// forms).
    #[arg(
        long = "repo-url",
        value_name = "URL",
        value_parser = parse_repo_url
    )]
    repo_url: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Subcmd {
    /// Open the interactive TUI.
    Tui(TuiCommand),
    /// Review a GitHub pull request or GitLab merge request.
    #[command(visible_alias = "mr")]
    Pr(PrCommand),
    /// Inspect or update persisted review sessions.
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
    /// Update the installed tuicr binary.
    Update {
        /// Install a specific SemVer release, including an older known-good version.
        #[arg(value_name = "VERSION")]
        version: Option<semver::Version>,
    },
}

/// Explicit `tuicr tui` entrypoint. With no nested command, opens the local
/// target selector / local diff TUI. `tuicr tui pr <target>` opens PR mode.
#[derive(Args, Debug, Clone, Default)]
struct TuiCommand {
    #[command(flatten)]
    options: TuiOptions,

    /// Base revision to diff the working tree against, like `git diff BASE`.
    /// Requires -w. An explicit range (`A..B`) shows the flat diff between
    /// its endpoints instead, like `git diff A..B`.
    #[arg(
        value_name = "BASE",
        requires = "working_tree",
        conflicts_with = "revisions"
    )]
    working_tree_base: Option<String>,

    #[command(subcommand)]
    command: Option<TuiSubcmd>,
}

#[derive(Subcommand, Debug, Clone)]
enum TuiSubcmd {
    /// Review a GitHub pull request or GitLab merge request in the TUI.
    #[command(visible_alias = "mr")]
    Pr(PrCommand),
}

#[derive(Args, Debug, Clone, Default)]
struct PrCommand {
    /// PR target: <number>, <owner/repo#N>, or a PR URL.
    target: String,

    #[command(flatten)]
    options: TuiOptions,
}

/// Non-interactive review session commands.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum ReviewCommand {
    /// List persisted review sessions for a checkout or forge repo.
    List {
        /// Repo selector: a checkout path, or a forge coordinate like
        /// `owner/repo`, `host/owner/repo`, or a repo/PR URL. A path also
        /// surfaces PR sessions for that checkout's origin repo.
        #[arg(long, value_name = "PATH|OWNER/REPO", default_value = ".")]
        repo: PathBuf,

        /// List every persisted session (local and PR), ignoring --repo.
        #[arg(long)]
        all: bool,
    },

    /// Add a local draft comment to a persisted session.
    Add {
        /// Session slug from `tuicr review list` (local or PR), or path to a
        /// session JSON file.
        #[arg(long, value_name = "SESSION")]
        session: String,

        /// JSON payload. Use literal JSON, @path/to/file.json, or - for stdin.
        #[arg(long, value_name = "JSON|@FILE|-")]
        input: Option<String>,

        /// Repo selector used to resolve a local session slug (path or
        /// `owner/repo`). PR slugs and JSON paths resolve without it.
        #[arg(long, value_name = "PATH|OWNER/REPO", default_value = ".")]
        repo: PathBuf,

        /// Comment classification. Defaults to `none` (no type, no `[TYPE]`
        /// prefix); pass a type configured via `comment_types` to classify.
        /// An id absent from a configured `comment_types` warns on stderr but
        /// is still stored.
        #[arg(long = "type", value_name = "TYPE", default_value = "none", value_parser = non_empty_comment_type)]
        comment_type: String,

        /// File path for a file, line, or range comment. Omit for a review comment.
        #[arg(long = "target-file", value_name = "PATH")]
        file: Option<PathBuf>,

        /// Line number for a line or range comment. Requires --target-file.
        #[arg(long, value_name = "LINE", requires = "file")]
        line: Option<u32>,

        /// End line for a range comment. Requires --line.
        #[arg(long = "end-line", value_name = "LINE", requires = "line")]
        end_line: Option<u32>,

        /// Diff side for line and range comments.
        #[arg(long, value_enum, default_value_t = LineSideArg::New)]
        side: LineSideArg,

        /// Author stamped on the new comment. Pass an explicit value when
        /// invoking from an agent (e.g. `--username "Claude Opus 4.7"`) so
        /// human and agent comments are visually distinguished in the TUI.
        /// Falls back to the config `username` setting, then to `"user"`.
        #[arg(long, value_name = "NAME")]
        username: Option<String>,

        /// Comment text.
        #[arg(
            value_name = "COMMENT",
            required_unless_present = "input",
            value_parser = non_empty_comment_text,
            allow_hyphen_values = true
        )]
        content: Option<String>,
    },

    /// Reply to an existing comment in a persisted session, forming a thread.
    Reply {
        /// Session slug from `tuicr review list` (local or PR), or path to a
        /// session JSON file.
        #[arg(long, value_name = "SESSION")]
        session: String,

        /// Id of the comment to reply to, from `tuicr review comments`.
        /// Replying to a reply attaches to that thread's root comment.
        #[arg(long = "comment-id", value_name = "ID")]
        comment_id: Option<String>,

        /// JSON payload. Use literal JSON, @path/to/file.json, or - for stdin.
        #[arg(long, value_name = "JSON|@FILE|-")]
        input: Option<String>,

        /// Repo selector used to resolve a local session slug (path or
        /// `owner/repo`). PR slugs and JSON paths resolve without it.
        #[arg(long, value_name = "PATH|OWNER/REPO", default_value = ".")]
        repo: PathBuf,

        /// Author stamped on the reply. Pass an explicit value when invoking
        /// from an agent (e.g. `--username "Claude Opus 4.7"`) so the reply is
        /// visually distinguished from the user's own comments — and so the
        /// agent can tell which comments it has already answered.
        #[arg(long, value_name = "NAME")]
        username: Option<String>,

        /// Reply text.
        #[arg(
            value_name = "REPLY",
            required_unless_present = "input",
            value_parser = non_empty_comment_text,
            allow_hyphen_values = true
        )]
        content: Option<String>,
    },

    /// Mark a comment's thread resolved, or reopen it with --unresolve.
    Resolve {
        /// Session slug from `tuicr review list` (local or PR), or path to a
        /// session JSON file.
        #[arg(long, value_name = "SESSION")]
        session: String,

        /// Id of any comment in the thread, from `tuicr review comments`. An
        /// unambiguous prefix works, like a short SHA in git.
        #[arg(long = "comment-id", value_name = "ID")]
        comment_id: String,

        /// Reopen the thread instead of resolving it.
        #[arg(long, action = ArgAction::SetTrue)]
        unresolve: bool,

        /// Repo selector used to resolve a local session slug (path or
        /// `owner/repo`). PR slugs and JSON paths resolve without it.
        #[arg(long, value_name = "PATH|OWNER/REPO", default_value = ".")]
        repo: PathBuf,
    },

    /// Block until the reviewer hands the review over, then print its
    /// comments. Intended to be run in the background by an agent.
    Watch {
        /// Session slug from `tuicr review list` (local or PR), or path to a
        /// session JSON file.
        #[arg(long, value_name = "SESSION")]
        session: String,

        /// Return on any change to the session instead of waiting for an
        /// explicit `:submit agent`. Every saved comment wakes the caller.
        #[arg(long, action = ArgAction::SetTrue)]
        any: bool,

        /// Give up after this many seconds so a forgotten watcher does not
        /// outlive the review.
        #[arg(long = "timeout", value_name = "SECONDS", default_value_t = 1800)]
        timeout_secs: u64,

        /// Repo selector used to resolve a local session slug (path or
        /// `owner/repo`). PR slugs and JSON paths resolve without it.
        #[arg(long, value_name = "PATH|OWNER/REPO", default_value = ".")]
        repo: PathBuf,
    },

    /// Tell a review an agent has picked it up and is working on it, so the
    /// reviewer sees the handoff land instead of waiting at a still screen.
    Working {
        /// Session slug from `tuicr review list` (local or PR), or path to a
        /// session JSON file.
        #[arg(long, value_name = "SESSION")]
        session: String,

        /// What you are doing, in your own words — shown to the reviewer
        /// verbatim, so "reading your six comments" beats a bare spinner.
        #[arg(long, value_name = "TEXT")]
        message: Option<String>,

        /// Which agent is working. Falls back to the config `username`.
        #[arg(long, value_name = "NAME")]
        username: Option<String>,

        /// Stop: you are no longer working on this review. Pass `--message`
        /// with it to say why — a reply with no code change is a result, and
        /// a spinner that just disappears looks like an agent that died.
        #[arg(long)]
        done: bool,

        /// Repo selector used to resolve a local session slug (path or
        /// `owner/repo`). PR slugs and JSON paths resolve without it.
        #[arg(long, value_name = "PATH|OWNER/REPO", default_value = ".")]
        repo: PathBuf,
    },

    /// Tell a review that the code under it changed, so the reviewer is
    /// prompted to reload instead of finding out by reloading into nothing.
    Update {
        /// Session slug from `tuicr review list` (local or PR), or path to a
        /// session JSON file.
        #[arg(long, value_name = "SESSION")]
        session: String,

        /// What changed, in your own words — shown to the reviewer verbatim.
        #[arg(long, value_name = "TEXT")]
        message: Option<String>,

        /// Repo selector used to resolve a local session slug (path or
        /// `owner/repo`). PR slugs and JSON paths resolve without it.
        #[arg(long, value_name = "PATH|OWNER/REPO", default_value = ".")]
        repo: PathBuf,
    },

    /// Print comments stored in a persisted session.
    #[command(alias = "get")]
    Comments {
        /// Session slug from `tuicr review list` (local or PR), or path to a
        /// session JSON file.
        #[arg(long, value_name = "SESSION")]
        session: String,

        /// Repo selector used to resolve a local session slug (path or
        /// `owner/repo`). PR slugs and JSON paths resolve without it.
        #[arg(long, value_name = "PATH|OWNER/REPO", default_value = ".")]
        repo: PathBuf,
    },
}

/// Diff side accepted by `tuicr review add --side`.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineSideArg {
    Old,
    #[default]
    New,
}

impl From<Cli> for CliArgs {
    fn from(cli: Cli) -> Self {
        let (options, working_tree_base, pr_target, review_command, update_version, update_command) =
            match cli.command {
                Some(Subcmd::Tui(command)) => match command.command {
                    Some(TuiSubcmd::Pr(pr)) => (
                        cli.tui_options.merge(command.options).merge(pr.options),
                        None,
                        Some(pr.target),
                        None,
                        None,
                        false,
                    ),
                    None => (
                        cli.tui_options.merge(command.options),
                        command.working_tree_base.or(cli.working_tree_base),
                        None,
                        None,
                        None,
                        false,
                    ),
                },
                Some(Subcmd::Pr(pr)) => (
                    cli.tui_options.merge(pr.options),
                    None,
                    Some(pr.target),
                    None,
                    None,
                    false,
                ),
                Some(Subcmd::Review { command }) => (
                    TuiOptions::default(),
                    None,
                    None,
                    Some(command),
                    None,
                    false,
                ),
                Some(Subcmd::Update { version }) => {
                    (TuiOptions::default(), None, None, None, version, true)
                }
                None => (
                    cli.tui_options,
                    cli.working_tree_base,
                    None,
                    None,
                    None,
                    false,
                ),
            };
        Self {
            theme: options.theme,
            appearance: options.appearance,
            output_to_stdout: options.stdout,
            no_update_check: options.no_update_check,
            revisions: options.revisions,
            working_tree: options.working_tree,
            working_tree_base,
            staged: options.staged,
            no_untracked: options.no_untracked,
            untracked: options.untracked,
            no_staged: options.no_staged,
            with_staged: options.with_staged,
            path_filter: options.path_filter,
            file_path: options.file_path,
            all_files: options.all_files,
            pr_target,
            repo_url: options.repo_url,
            review_command,
            update_command,
            update_version,
        }
    }
}

impl TuiOptions {
    fn has_any_explicit_value(&self) -> bool {
        self.theme.is_some()
            || self.appearance.is_some()
            || self.stdout
            || self.no_update_check
            || self.revisions.is_some()
            || self.working_tree
            || self.staged
            || self.no_untracked
            || self.untracked
            || self.no_staged
            || self.with_staged
            || self.path_filter.is_some()
            || self.file_path.is_some()
            || self.all_files
            || self.repo_url.is_some()
    }

    fn merge(self, later: TuiOptions) -> Self {
        Self {
            theme: later.theme.or(self.theme),
            appearance: later.appearance.or(self.appearance),
            stdout: self.stdout || later.stdout,
            no_update_check: self.no_update_check || later.no_update_check,
            revisions: later.revisions.or(self.revisions),
            working_tree: self.working_tree || later.working_tree,
            staged: self.staged || later.staged,
            no_untracked: self.no_untracked || later.no_untracked,
            untracked: self.untracked || later.untracked,
            no_staged: self.no_staged || later.no_staged,
            with_staged: self.with_staged || later.with_staged,
            path_filter: later.path_filter.or(self.path_filter),
            file_path: later.file_path.or(self.file_path),
            all_files: self.all_files || later.all_files,
            repo_url: later.repo_url.or(self.repo_url),
        }
    }
}

impl Cli {
    fn try_into_args(self) -> std::result::Result<CliArgs, clap::Error> {
        if self.base_combined_with_pr() {
            return Err(clap::Error::raw(
                clap::error::ErrorKind::ArgumentConflict,
                "a BASE revision cannot be used with a pull request review",
            ));
        }
        match (
            self.tui_options.has_any_explicit_value() || self.working_tree_base.is_some(),
            self.non_tui_command_name(),
        ) {
            (true, Some(command_name)) => Err(clap::Error::raw(
                clap::error::ErrorKind::ArgumentConflict,
                format!(
                    "TUI options cannot be used with `tuicr {command_name}`; run `tuicr {command_name} --help` for command options"
                ),
            )),
            _ => Ok(self.into()),
        }
    }

    fn non_tui_command_name(&self) -> Option<&'static str> {
        match self.command {
            Some(Subcmd::Review { .. }) => Some("review"),
            Some(Subcmd::Update { .. }) => Some("update"),
            _ => None,
        }
    }

    fn base_combined_with_pr(&self) -> bool {
        let tui_base_with_pr = matches!(
            &self.command,
            Some(Subcmd::Tui(command))
                if command.working_tree_base.is_some() && command.command.is_some()
        );
        let top_base_with_pr = self.working_tree_base.is_some()
            && matches!(
                &self.command,
                Some(Subcmd::Pr(_))
                    | Some(Subcmd::Tui(TuiCommand {
                        command: Some(_),
                        ..
                    }))
            );
        tui_base_with_pr || top_base_with_pr
    }
}

fn parse_appearance_arg(s: &str) -> Result<AppearanceArg, String> {
    AppearanceArg::parse_name(s).ok_or_else(|| {
        let valid = AppearanceArg::valid_values_display();
        format!("Unknown appearance '{s}'. Valid options: {valid}")
    })
}

fn non_empty_theme_name(s: &str) -> Result<String, String> {
    if s.is_empty() {
        let valid = ThemeArg::valid_values_display();
        Err(format!("--theme requires a value ({valid})"))
    } else {
        Ok(s.to_string())
    }
}

/// Reject `--repo-url` values that don't parse as a supported forge remote URL
/// (GitHub, GitLab, Bitbucket, or Azure DevOps) so the failure is surfaced at
/// startup rather than when the PR tab is opened.
fn parse_repo_url(s: &str) -> Result<String, String> {
    if crate::forge::parse_any_remote_url(s).is_some() {
        Ok(s.to_string())
    } else {
        Err(format!(
            "--repo-url value '{s}' is not a recognized GitHub, GitLab, Bitbucket, or Azure \
             DevOps URL. Expected forms like: https://github.com/owner/repo, \
             git@gitlab.com:owner/repo, https://bitbucket.org/workspace/repo, or \
             https://dev.azure.com/org/project/_git/repo"
        ))
    }
}

fn non_empty_path(s: &str) -> Result<String, String> {
    if s.is_empty() {
        Err("a file or directory path is required".to_string())
    } else {
        Ok(s.to_string())
    }
}

fn non_empty_comment_type(s: &str) -> Result<String, String> {
    if s.is_empty() {
        Err("a comment type is required".to_string())
    } else {
        Ok(s.to_string())
    }
}

fn non_empty_comment_text(s: &str) -> Result<String, String> {
    if s.trim().is_empty() {
        Err("comment text cannot be empty".to_string())
    } else {
        Ok(s.to_string())
    }
}

/// Parse CLI arguments from `std::env::args`. On `--help`/`--version`/parse
/// errors, clap prints to stdout/stderr and exits the process.
pub fn parse_cli_args() -> CliArgs {
    match Cli::parse().try_into_args() {
        Ok(args) => args,
        Err(err) => err.exit(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    fn parse_for_test(args: &[&str]) -> Result<CliArgs, clap::Error> {
        Cli::try_parse_from(args).and_then(Cli::try_into_args)
    }

    #[test]
    fn should_parse_theme_when_provided() {
        let parsed = parse_for_test(&["tuicr", "--theme", "light"]).expect("parse should succeed");
        assert_eq!(parsed.theme, Some("light".to_string()));
    }

    #[test]
    fn should_parse_catppuccin_themes() {
        let parsed = parse_for_test(&["tuicr", "--theme", "catppuccin-mocha"])
            .expect("parse should succeed");
        assert_eq!(parsed.theme, Some("catppuccin-mocha".to_string()));

        let parsed =
            parse_for_test(&["tuicr", "--theme=catppuccin-latte"]).expect("parse should succeed");
        assert_eq!(parsed.theme, Some("catppuccin-latte".to_string()));
    }

    #[test]
    fn should_parse_ayu_light_theme() {
        let parsed =
            parse_for_test(&["tuicr", "--theme", "ayu-light"]).expect("parse should succeed");
        assert_eq!(parsed.theme, Some("ayu-light".to_string()));
    }

    #[test]
    fn should_parse_onedark_theme() {
        let parsed =
            parse_for_test(&["tuicr", "--theme", "onedark"]).expect("parse should succeed");
        assert_eq!(parsed.theme, Some("onedark".to_string()));
    }

    #[test]
    fn should_parse_gruvbox_themes() {
        let parsed =
            parse_for_test(&["tuicr", "--theme", "gruvbox-dark"]).expect("parse should succeed");
        assert_eq!(parsed.theme, Some("gruvbox-dark".to_string()));

        let parsed =
            parse_for_test(&["tuicr", "--theme=gruvbox-light"]).expect("parse should succeed");
        assert_eq!(parsed.theme, Some("gruvbox-light".to_string()));
    }

    #[test]
    fn should_parse_everforest_themes() {
        let parsed =
            parse_for_test(&["tuicr", "--theme", "everforest-dark"]).expect("parse should succeed");
        assert_eq!(parsed.theme, Some("everforest-dark".to_string()));

        let parsed =
            parse_for_test(&["tuicr", "--theme=everforest-light"]).expect("parse should succeed");
        assert_eq!(parsed.theme, Some("everforest-light".to_string()));
    }

    #[test]
    fn should_leave_theme_none_when_not_provided() {
        let parsed = parse_for_test(&["tuicr"]).expect("parse should succeed");
        assert_eq!(parsed.theme, None);
    }

    #[test]
    fn should_parse_working_tree_short_flag() {
        let parsed = parse_for_test(&["tuicr", "-w"]).expect("parse should succeed");
        assert!(parsed.working_tree);
    }

    #[test]
    fn should_parse_working_tree_base() {
        let parsed = parse_for_test(&["tuicr", "-w", "main"]).expect("parse should succeed");
        assert!(parsed.working_tree);
        assert_eq!(parsed.working_tree_base, Some("main".to_string()));
    }

    #[test]
    fn should_parse_working_tree_base_via_tui_subcommand() {
        let parsed = parse_for_test(&["tuicr", "tui", "-w", "main"]).expect("parse should succeed");
        assert!(parsed.working_tree);
        assert_eq!(parsed.working_tree_base, Some("main".to_string()));
    }

    #[test]
    fn should_reject_base_without_working_tree() {
        let err = parse_for_test(&["tuicr", "main"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn should_reject_base_combined_with_revisions() {
        let err = parse_for_test(&["tuicr", "-w", "-r", "main..", "main"])
            .expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn should_reject_base_combined_with_pr() {
        let err =
            parse_for_test(&["tuicr", "-w", "main", "pr", "123"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn should_parse_working_tree_long_flag() {
        let parsed = parse_for_test(&["tuicr", "--working-tree"]).expect("parse should succeed");
        assert!(parsed.working_tree);
    }

    #[test]
    fn should_default_working_tree_to_false() {
        let parsed = parse_for_test(&["tuicr"]).expect("parse should succeed");
        assert!(!parsed.working_tree);
    }

    #[test]
    fn should_parse_staged_flag() {
        let parsed = parse_for_test(&["tuicr", "--staged"]).expect("parse should succeed");
        assert!(parsed.staged);
    }

    #[test]
    fn should_reject_staged_combined_with_working_tree() {
        let err = parse_for_test(&["tuicr", "--staged", "-w"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn should_reject_staged_combined_with_revisions() {
        let err = parse_for_test(&["tuicr", "--staged", "-r", "HEAD~1.."])
            .expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn should_parse_no_untracked_with_working_tree() {
        let parsed =
            parse_for_test(&["tuicr", "-w", "--no-untracked"]).expect("parse should succeed");
        assert!(parsed.working_tree);
        assert!(parsed.no_untracked);
    }

    #[test]
    fn should_default_no_untracked_to_false() {
        let parsed = parse_for_test(&["tuicr", "-w"]).expect("parse should succeed");
        assert!(!parsed.no_untracked);
    }

    #[test]
    fn should_parse_no_staged_with_working_tree() {
        let parsed = parse_for_test(&["tuicr", "-w", "--no-staged"]).expect("parse should succeed");
        assert!(parsed.working_tree);
        assert!(parsed.no_staged);
    }

    #[test]
    fn should_reject_no_staged_combined_with_staged_mode() {
        let err =
            parse_for_test(&["tuicr", "--staged", "--no-staged"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn should_reject_with_staged_combined_with_no_staged() {
        let err = parse_for_test(&["tuicr", "-w", "--with-staged", "--no-staged"])
            .expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn should_parse_untracked_override_flag() {
        let parsed = parse_for_test(&["tuicr", "-w", "--untracked"]).expect("parse should succeed");
        assert!(parsed.untracked);
        assert!(!parsed.no_untracked);
    }

    #[test]
    fn should_reject_untracked_combined_with_no_untracked() {
        let err = parse_for_test(&["tuicr", "-w", "--untracked", "--no-untracked"])
            .expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn should_parse_working_tree_with_revisions() {
        let parsed =
            parse_for_test(&["tuicr", "-w", "-r", "HEAD~3..HEAD"]).expect("parse should succeed");
        assert!(parsed.working_tree);
        assert_eq!(parsed.revisions, Some("HEAD~3..HEAD".to_string()));
    }

    #[test]
    fn should_allow_custom_theme_name_in_separate_arg() {
        let parsed = parse_for_test(&["tuicr", "--theme", "tuicr-teal"])
            .expect("custom theme parse should succeed");
        assert_eq!(parsed.theme, Some("tuicr-teal".to_string()));
    }

    #[test]
    fn should_allow_custom_theme_name_in_equals_arg() {
        let parsed = parse_for_test(&["tuicr", "--theme=tuicr-teal"])
            .expect("custom theme parse should succeed");
        assert_eq!(parsed.theme, Some("tuicr-teal".to_string()));
    }

    #[test]
    fn should_error_when_theme_value_missing() {
        let err = parse_for_test(&["tuicr", "--theme"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::InvalidValue);
    }

    #[test]
    fn should_parse_appearance_when_provided() {
        let parsed =
            parse_for_test(&["tuicr", "--appearance", "system"]).expect("parse should succeed");
        assert_eq!(parsed.appearance, Some(AppearanceArg::System));
    }

    #[test]
    fn should_error_for_invalid_appearance() {
        let err =
            parse_for_test(&["tuicr", "--appearance", "nope"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
        assert!(err.to_string().contains("Unknown appearance 'nope'"));
    }

    #[test]
    fn should_parse_path_short_flag() {
        let parsed = parse_for_test(&["tuicr", "-p", "src/main.rs"]).expect("parse should succeed");
        assert_eq!(parsed.path_filter, Some("src/main.rs".to_string()));
    }

    #[test]
    fn should_parse_path_long_flag() {
        let parsed = parse_for_test(&["tuicr", "--path", "src/"]).expect("parse should succeed");
        assert_eq!(parsed.path_filter, Some("src/".to_string()));
    }

    #[test]
    fn should_parse_path_equals_syntax() {
        let parsed = parse_for_test(&["tuicr", "--path=plans/current-plan.md"])
            .expect("parse should succeed");
        assert_eq!(
            parsed.path_filter,
            Some("plans/current-plan.md".to_string())
        );
    }

    #[test]
    fn should_error_when_path_value_missing() {
        let err = parse_for_test(&["tuicr", "--path"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::InvalidValue);
    }

    #[test]
    fn should_error_when_path_equals_empty() {
        let err = parse_for_test(&["tuicr", "--path="]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn should_default_path_filter_to_none() {
        let parsed = parse_for_test(&["tuicr"]).expect("parse should succeed");
        assert_eq!(parsed.path_filter, None);
    }

    #[test]
    fn should_parse_path_with_working_tree() {
        let parsed =
            parse_for_test(&["tuicr", "-p", "file.md", "-w"]).expect("parse should succeed");
        assert_eq!(parsed.path_filter, Some("file.md".to_string()));
        assert!(parsed.working_tree);
    }

    #[test]
    fn should_parse_path_with_revisions() {
        let parsed = parse_for_test(&["tuicr", "--path", "src/", "-r", "HEAD~3.."])
            .expect("parse should succeed");
        assert_eq!(parsed.path_filter, Some("src/".to_string()));
        assert_eq!(parsed.revisions, Some("HEAD~3..".to_string()));
    }

    #[test]
    fn should_reject_file_combined_with_path() {
        let err = parse_for_test(&["tuicr", "--file", "f.md", "--path", "src/"])
            .expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn should_reject_file_combined_with_revisions() {
        let err = parse_for_test(&["tuicr", "--file", "f.md", "-r", "HEAD~1.."])
            .expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn should_reject_file_combined_with_working_tree() {
        let err =
            parse_for_test(&["tuicr", "--file", "f.md", "-w"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn should_reject_all_files_combined_with_path() {
        let err =
            parse_for_test(&["tuicr", "-A", "--path", "src/"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn should_reject_all_files_combined_with_file() {
        let err =
            parse_for_test(&["tuicr", "-A", "--file", "f.md"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn should_parse_all_files_short_flag() {
        let parsed = parse_for_test(&["tuicr", "-A"]).expect("parse should succeed");
        assert!(parsed.all_files);
    }

    #[test]
    fn should_parse_all_files_long_flag() {
        let parsed = parse_for_test(&["tuicr", "--all-files"]).expect("parse should succeed");
        assert!(parsed.all_files);
    }

    #[test]
    fn should_parse_stdout_flag() {
        let parsed = parse_for_test(&["tuicr", "--stdout"]).expect("parse should succeed");
        assert!(parsed.output_to_stdout);
    }

    #[test]
    fn should_parse_no_update_check_flag() {
        let parsed = parse_for_test(&["tuicr", "--no-update-check"]).expect("parse should succeed");
        assert!(parsed.no_update_check);
    }

    #[test]
    fn should_parse_update_command_without_tui_options() {
        let parsed = parse_for_test(&["tuicr", "update"]).expect("parse should succeed");
        assert!(parsed.update_command);
        assert_eq!(parsed.update_version, None);
        assert_eq!(parsed.review_command, None);
    }

    #[test]
    fn should_parse_specific_update_version() {
        let parsed = parse_for_test(&["tuicr", "update", "0.18.0"]).expect("parse should succeed");
        assert!(parsed.update_command);
        assert_eq!(
            parsed.update_version.as_ref().map(ToString::to_string),
            Some("0.18.0".to_string())
        );
    }

    #[test]
    fn should_reject_invalid_update_version() {
        let err = parse_for_test(&["tuicr", "update", "latest"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn should_reject_tui_options_with_update_command() {
        let err =
            parse_for_test(&["tuicr", "--theme", "dark", "update"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn should_parse_pr_target_as_bare_number() {
        let parsed = parse_for_test(&["tuicr", "pr", "125"]).expect("parse should succeed");
        assert_eq!(parsed.pr_target, Some("125".to_string()));
    }

    #[test]
    fn should_parse_mr_alias_like_pr() {
        let parsed = parse_for_test(&["tuicr", "mr", "125"]).expect("parse should succeed");
        assert_eq!(parsed.pr_target, Some("125".to_string()));
    }

    #[test]
    fn should_parse_tui_mr_alias_like_pr() {
        let parsed = parse_for_test(&["tuicr", "tui", "mr", "125"]).expect("parse should succeed");
        assert_eq!(parsed.pr_target, Some("125".to_string()));
    }

    #[test]
    fn should_parse_pr_target_as_owner_repo_hash() {
        let parsed =
            parse_for_test(&["tuicr", "pr", "agavra/tuicr#125"]).expect("parse should succeed");
        assert_eq!(parsed.pr_target, Some("agavra/tuicr#125".to_string()));
    }

    #[test]
    fn should_parse_pr_target_as_full_url() {
        let parsed = parse_for_test(&["tuicr", "pr", "https://github.com/agavra/tuicr/pull/125"])
            .expect("parse should succeed");
        assert_eq!(
            parsed.pr_target,
            Some("https://github.com/agavra/tuicr/pull/125".to_string()),
        );
    }

    #[test]
    fn should_error_when_pr_target_is_missing() {
        let err = parse_for_test(&["tuicr", "pr"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn should_combine_pr_target_with_theme_flag() {
        // Legacy `tuicr pr` still accepts TUI flags on the subcommand.
        let parsed = parse_for_test(&["tuicr", "pr", "125", "--theme", "dark"])
            .expect("parse should succeed");
        assert_eq!(parsed.pr_target, Some("125".to_string()));
        assert_eq!(parsed.theme, Some("dark".to_string()));
    }

    #[test]
    fn should_allow_root_tui_options_before_legacy_pr_subcommand() {
        let parsed = parse_for_test(&["tuicr", "--theme", "dark", "pr", "125"])
            .expect("parse should succeed");
        assert_eq!(parsed.pr_target, Some("125".to_string()));
        assert_eq!(parsed.theme, Some("dark".to_string()));
    }

    #[test]
    fn should_parse_explicit_tui_command() {
        let parsed = parse_for_test(&["tuicr", "tui", "-w", "--theme", "dark"])
            .expect("parse should succeed");
        assert!(parsed.working_tree);
        assert_eq!(parsed.theme, Some("dark".to_string()));
        assert_eq!(parsed.pr_target, None);
        assert_eq!(parsed.review_command, None);
    }

    #[test]
    fn should_parse_explicit_tui_pr_command() {
        let parsed = parse_for_test(&["tuicr", "tui", "pr", "125", "--theme", "dark"])
            .expect("parse should succeed");
        assert_eq!(parsed.pr_target, Some("125".to_string()));
        assert_eq!(parsed.theme, Some("dark".to_string()));
    }

    #[test]
    fn should_reject_root_tui_options_before_subcommands() {
        let err = parse_for_test(&["tuicr", "--theme", "dark", "review", "list"])
            .expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn should_leave_pr_target_none_when_no_pr_subcommand() {
        let parsed = parse_for_test(&["tuicr"]).expect("parse should succeed");
        assert_eq!(parsed.pr_target, None);
    }

    #[test]
    fn should_parse_repo_url_https() {
        let parsed = parse_for_test(&["tuicr", "--repo-url", "https://github.com/slatedb/slatedb"])
            .expect("parse should succeed");
        assert_eq!(
            parsed.repo_url,
            Some("https://github.com/slatedb/slatedb".to_string())
        );
    }

    #[test]
    fn should_parse_repo_url_equals_form() {
        let parsed = parse_for_test(&["tuicr", "--repo-url=git@github.com:slatedb/slatedb.git"])
            .expect("parse should succeed");
        assert_eq!(
            parsed.repo_url,
            Some("git@github.com:slatedb/slatedb.git".to_string())
        );
    }

    #[test]
    fn should_parse_repo_url_ssh_scheme() {
        let parsed = parse_for_test(&[
            "tuicr",
            "--repo-url",
            "ssh://git@github.com/slatedb/slatedb.git",
        ])
        .expect("parse should succeed");
        assert_eq!(
            parsed.repo_url,
            Some("ssh://git@github.com/slatedb/slatedb.git".to_string())
        );
    }

    #[test]
    fn should_error_when_repo_url_value_missing() {
        let err = parse_for_test(&["tuicr", "--repo-url"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::InvalidValue);
    }

    #[test]
    fn should_error_when_repo_url_unparseable() {
        let err =
            parse_for_test(&["tuicr", "--repo-url", "not-a-url"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
        assert!(
            err.to_string()
                .contains("not a recognized GitHub, GitLab, Bitbucket, or Azure"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn should_accept_repo_url_for_every_supported_forge() {
        // `--repo-url` used to validate against GitHub only, which silently
        // rejected GitLab, Bitbucket, and Azure DevOps remotes.
        for url in [
            "https://github.com/slatedb/slatedb.git",
            "https://gitlab.com/owner/repo.git",
            "https://bitbucket.org/example-workspace/repo.git",
            "git@bitbucket.org:example-workspace/repo.git",
            "https://dev.azure.com/org/project/_git/repo",
        ] {
            let parsed = parse_for_test(&["tuicr", "--repo-url", url])
                .unwrap_or_else(|err| panic!("{url} should parse: {err}"));
            assert_eq!(parsed.repo_url, Some(url.to_string()));
        }
    }

    #[test]
    fn should_error_when_repo_url_equals_empty() {
        let err = parse_for_test(&["tuicr", "--repo-url="]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn should_leave_repo_url_none_when_not_provided() {
        let parsed = parse_for_test(&["tuicr"]).expect("parse should succeed");
        assert_eq!(parsed.repo_url, None);
    }

    #[test]
    fn should_parse_review_list_command() {
        let parsed = parse_for_test(&["tuicr", "review", "list", "--repo", "/tmp/repo"])
            .expect("parse should succeed");
        assert_eq!(
            parsed.review_command,
            Some(ReviewCommand::List {
                repo: PathBuf::from("/tmp/repo"),
                all: false,
            })
        );
    }

    #[test]
    fn should_parse_review_list_all_flag() {
        let parsed =
            parse_for_test(&["tuicr", "review", "list", "--all"]).expect("parse should succeed");
        assert_eq!(
            parsed.review_command,
            Some(ReviewCommand::List {
                repo: PathBuf::from("."),
                all: true,
            })
        );
    }

    #[test]
    fn should_parse_review_list_by_coordinate() {
        let parsed = parse_for_test(&["tuicr", "review", "list", "--repo", "slatedb/slatedb"])
            .expect("parse should succeed");
        assert_eq!(
            parsed.review_command,
            Some(ReviewCommand::List {
                repo: PathBuf::from("slatedb/slatedb"),
                all: false,
            })
        );
    }

    #[test]
    fn should_reject_review_json_flag_because_output_is_always_json() {
        let err =
            parse_for_test(&["tuicr", "review", "list", "--json"]).expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn should_parse_review_add_line_comment() {
        let parsed = parse_for_test(&[
            "tuicr",
            "review",
            "add",
            "--session",
            "agavra/tuicr@main/worktree",
            "--target-file",
            "src/main.rs",
            "--line",
            "42",
            "--type",
            "issue",
            "--side",
            "old",
            "Handle the empty case",
        ])
        .expect("parse should succeed");

        assert_eq!(
            parsed.review_command,
            Some(ReviewCommand::Add {
                session: "agavra/tuicr@main/worktree".to_string(),
                input: None,
                repo: PathBuf::from("."),
                comment_type: "issue".to_string(),
                file: Some(PathBuf::from("src/main.rs")),
                line: Some(42),
                end_line: None,
                side: LineSideArg::Old,
                username: None,
                content: Some("Handle the empty case".to_string()),
            })
        );
    }

    #[test]
    fn should_parse_review_add_json_input() {
        let parsed = parse_for_test(&[
            "tuicr",
            "review",
            "add",
            "--session",
            "agavra/tuicr@main/worktree",
            "--input",
            r#"{"file":"src/main.rs","line":42,"side":"old","content":"note"}"#,
        ])
        .expect("parse should succeed");

        assert_eq!(
            parsed.review_command,
            Some(ReviewCommand::Add {
                session: "agavra/tuicr@main/worktree".to_string(),
                input: Some(
                    r#"{"file":"src/main.rs","line":42,"side":"old","content":"note"}"#.to_string()
                ),
                repo: PathBuf::from("."),
                comment_type: "none".to_string(),
                file: None,
                line: None,
                end_line: None,
                side: LineSideArg::New,
                username: None,
                content: None,
            })
        );
    }

    #[test]
    fn should_parse_review_comments_command() {
        let parsed = parse_for_test(&["tuicr", "review", "comments", "--session", "session.json"])
            .expect("parse should succeed");
        assert_eq!(
            parsed.review_command,
            Some(ReviewCommand::Comments {
                session: "session.json".to_string(),
                repo: PathBuf::from("."),
            })
        );
    }

    #[test]
    fn should_parse_review_comments_get_alias() {
        let parsed = parse_for_test(&["tuicr", "review", "get", "--session", "session.json"])
            .expect("parse should succeed");
        assert_eq!(
            parsed.review_command,
            Some(ReviewCommand::Comments {
                session: "session.json".to_string(),
                repo: PathBuf::from("."),
            })
        );
    }

    #[test]
    fn should_require_file_for_review_add_line() {
        let err = parse_for_test(&[
            "tuicr",
            "review",
            "add",
            "--session",
            "session",
            "--line",
            "42",
            "note",
        ])
        .expect_err("parse should fail");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }
}
