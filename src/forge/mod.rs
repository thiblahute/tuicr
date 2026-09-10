//! Remote forge integration.
//!
//! This module is intentionally transport-focused for the first integration
//! slice. UI and review submission code should depend on the trait shape here
//! instead of shelling out to forge-specific tools directly.
#![allow(dead_code)]

pub mod azure;
pub mod bitbucket;
pub mod canonical;
pub mod context;
pub mod github;
pub mod gitlab;
pub mod pr_open;
pub mod remote_comments;
pub mod selector;
pub mod submit;
pub mod traits;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use git2::Repository;

use crate::process::{run_command_output, run_command_output_with_env};

use crate::forge::azure::az::parse_azure_remote_url;
use crate::forge::bitbucket::bkt::parse_bitbucket_remote_url;
use crate::forge::github::gh::parse_github_remote_url;
use crate::forge::gitlab::glab::parse_gitlab_remote_url;
use crate::forge::traits::ForgeRepository;

/// Try to detect a GitHub forge repository for the local checkout at `repo_root`.
///
/// Looks at the `origin` remote first, then falls back to any remote whose URL
/// parses as a GitHub host. Returns `None` when no GitHub remote is configured.
pub fn detect_github_repository(repo_root: &Path) -> Option<ForgeRepository> {
    let repo = Repository::discover(repo_root).ok()?;
    if let Ok(remote) = repo.find_remote("origin")
        && let Some(url) = remote.url()
        && let Some(parsed) = parse_github_remote_url(url)
    {
        return Some(parsed);
    }
    let remotes = repo.remotes().ok()?;
    for name in remotes.iter().flatten() {
        if let Ok(remote) = repo.find_remote(name)
            && let Some(url) = remote.url()
            && let Some(parsed) = parse_github_remote_url(url)
        {
            return Some(parsed);
        }
    }
    None
}

/// Try to detect a GitLab forge repository for the local checkout at `repo_root`.
///
/// Looks at the `origin` remote first, then falls back to any remote whose URL
/// parses as a GitLab host. Returns `None` when no GitLab remote is configured.
pub fn detect_gitlab_repository(repo_root: &Path) -> Option<ForgeRepository> {
    let repo = Repository::discover(repo_root).ok()?;
    if let Ok(remote) = repo.find_remote("origin")
        && let Some(url) = remote.url()
        && let Some(parsed) = parse_gitlab_remote_url(url)
    {
        return Some(parsed);
    }
    let remotes = repo.remotes().ok()?;
    for name in remotes.iter().flatten() {
        if let Ok(remote) = repo.find_remote(name)
            && let Some(url) = remote.url()
            && let Some(parsed) = parse_gitlab_remote_url(url)
        {
            return Some(parsed);
        }
    }
    None
}

/// `repo_root`'s remote URLs, `origin` first, then every other remote.
fn remote_urls(repo_root: &Path) -> Vec<String> {
    let Ok(repo) = Repository::discover(repo_root) else {
        return Vec::new();
    };
    let mut all_urls: Vec<String> = Vec::new();

    if let Ok(remote) = repo.find_remote("origin")
        && let Some(url) = remote.url()
    {
        all_urls.push(url.to_string());
    }
    if let Ok(remotes) = repo.remotes() {
        for name in remotes.iter().flatten() {
            if let Ok(remote) = repo.find_remote(name)
                && let Some(url) = remote.url()
            {
                all_urls.push(url.to_string());
            }
        }
    }
    all_urls
}

/// Try to detect an Azure DevOps forge repository for the local checkout at
/// `repo_root`. Looks at `origin` first, then any remote whose URL parses as an
/// Azure DevOps host. Returns `None` when no Azure remote is configured.
pub fn detect_azure_repository(repo_root: &Path) -> Option<ForgeRepository> {
    remote_urls(repo_root)
        .iter()
        .find_map(|url| parse_azure_remote_url(url))
}

/// Parse `url` as a forge remote repository.
///
/// Order matters. Bitbucket and GitLab both gate on the hostname, so trying
/// them first won't claim GitHub Enterprise remotes. Azure next — its parser
/// filters to `dev.azure.com` / `*.visualstudio.com` hosts. GitHub must stay
/// last because its parser accepts *any* host (covers github.com and GHE hosts
/// whose hostname does not literally contain "github") — it would otherwise
/// swallow every Bitbucket, self-hosted GitLab, and Azure remote.
pub fn parse_any_remote_url(url: &str) -> Option<ForgeRepository> {
    parse_bitbucket_remote_url(url)
        .or_else(|| parse_gitlab_remote_url(url))
        .or_else(|| parse_azure_remote_url(url))
        .or_else(|| parse_github_remote_url(url))
}

/// Parse `url` using only the forge parsers that decide from the URL alone —
/// no subprocess, no `~/.ssh/config` read — and without the GitHub catch-all.
///
/// For hot paths such as [`crate::slug::resolve_owner_repo`], which runs on
/// every session save, and only where an unrecognized remote has a usable
/// fallback. Prefer [`parse_any_remote_url`] everywhere else: it recognizes
/// more remotes, at the cost of a `glab config get host` spawn and up to three
/// `~/.ssh/config` reads per call.
///
/// GitHub is excluded on purpose. Its parser accepts any host and reads the
/// *first* two path segments, where `slug.rs`'s generic fallback reads the last
/// two; letting it claim unknown hosts would turn
/// `code.example.com/git/owner/repo` into `git/owner`. Bitbucket is left out
/// because a workspace is always one segment, so the fallback already agrees
/// with it.
pub fn parse_any_remote_url_by_hostname(url: &str) -> Option<ForgeRepository> {
    parse_azure_remote_url(url)
}

/// Detect the forge repository for the local checkout at `repo_root`.
/// Returns `None` when no remote can be parsed.
pub fn detect_forge_repository(repo_root: &Path) -> Option<ForgeRepository> {
    remote_urls(repo_root)
        .iter()
        .find_map(|url| parse_any_remote_url(url))
}

/// Name of the remote in `root` that serves `repository`, `origin` preferred
/// when several match. `None` outside a repo or when no remote points there.
fn remote_name_for_repo(root: &Path, repository: &ForgeRepository) -> Option<String> {
    let repo = Repository::discover(root).ok()?;
    let matches = |name: &str| {
        repo.find_remote(name)
            .ok()
            .and_then(|remote| remote.url().map(str::to_string))
            .and_then(|url| parse_any_remote_url(&url))
            .is_some_and(|parsed| &parsed == repository)
    };
    if matches("origin") {
        return Some("origin".to_string());
    }
    repo.remotes()
        .ok()?
        .iter()
        .flatten()
        .find(|name| matches(name))
        .map(str::to_string)
}

/// True when `rev` names a commit that is already readable in `root`.
fn commit_present(root: &Path, rev: &str) -> bool {
    let spec = format!("{rev}^{{commit}}");
    run_command_output(
        "git",
        Some(root),
        ["cat-file", "-e", spec.as_str()]
            .iter()
            .map(|arg| OsStr::new(*arg)),
    )
    .is_ok()
}

/// Fetch a pull request's head ref into `root` so its commits are readable
/// locally, and return whether they are there afterwards.
///
/// Nothing happens when `head_sha` is already present, which is the common
/// case for your own branches and for any PR reviewed twice. The ref is
/// written under `refs/tuicr/<host>/<owner>/<repo>/<number>` — a namespace of
/// tuicr's own, so it neither collides with the user's branches nor shows up
/// among their remotes — because objects with nothing pointing at them are
/// what `git gc` exists to remove.
///
/// Every failure is silent and non-fatal: the caller's next read simply goes
/// to the forge, exactly as it did before this ran.
pub fn fetch_pr_commits_into_checkout(
    root: &Path,
    repository: &ForgeRepository,
    number: u64,
    head_sha: &str,
    remote_ref: &str,
) -> bool {
    if commit_present(root, head_sha) {
        return true;
    }
    let Some(remote) = remote_name_for_repo(root, repository) else {
        return false;
    };
    let local_ref = format!(
        "refs/tuicr/{}/{}/{}/{number}",
        repository.host, repository.owner, repository.name
    );
    let refspec = format!("+{remote_ref}:{local_ref}");
    let args = [
        "fetch",
        "--no-tags",
        "--quiet",
        remote.as_str(),
        refspec.as_str(),
    ];
    // A private remote with no credential helper would otherwise stop to ask
    // for a password, and under the TUI there is nowhere to ask: the prompt
    // hangs behind the screen. An SSH passphrase or an unknown host key can
    // still block, since silencing those means overriding the user's own SSH
    // config.
    if run_command_output_with_env(
        "git",
        Some(root),
        args.iter().map(|arg| OsStr::new(*arg)),
        &[("GIT_TERMINAL_PROMPT", "0")],
    )
    .is_err()
    {
        return false;
    }
    commit_present(root, head_sha)
}

/// `root`'s local checkout, but only when one of its remotes — not
/// necessarily `origin` — matches `target_repo`.
pub fn local_checkout_for_repo(root: &Path, target_repo: &ForgeRepository) -> Option<PathBuf> {
    remote_urls(root)
        .iter()
        .any(|url| parse_any_remote_url(url).as_ref() == Some(target_repo))
        .then(|| root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_name_prefers_origin_over_another_matching_remote() {
        let dir = init_repo_with_origin("https://github.com/agavra/tuicr");
        let repo = Repository::open(dir.path()).expect("open repo");
        repo.remote("mirror", "https://github.com/agavra/tuicr")
            .expect("add mirror");

        let target = ForgeRepository::github("github.com", "agavra", "tuicr");
        assert_eq!(
            remote_name_for_repo(dir.path(), &target).as_deref(),
            Some("origin")
        );
    }

    #[test]
    fn remote_name_falls_back_to_the_remote_that_matches() {
        // The fork workflow: `origin` is your fork, the PR lives upstream.
        let dir = init_repo_with_origin("https://github.com/contributor/tuicr");
        let repo = Repository::open(dir.path()).expect("open repo");
        repo.remote("upstream", "https://github.com/agavra/tuicr")
            .expect("add upstream");

        let target = ForgeRepository::github("github.com", "agavra", "tuicr");
        assert_eq!(
            remote_name_for_repo(dir.path(), &target).as_deref(),
            Some("upstream")
        );
    }

    #[test]
    fn remote_name_is_none_when_nothing_points_at_the_repo() {
        let dir = init_repo_with_origin("https://github.com/someone/other");
        let target = ForgeRepository::github("github.com", "agavra", "tuicr");
        assert_eq!(remote_name_for_repo(dir.path(), &target), None);
    }

    #[test]
    fn fetching_is_skipped_when_the_commit_is_already_present() {
        // given a repo holding a real commit
        let dir = init_repo_with_origin("https://github.com/agavra/tuicr");
        let head = commit_a_file(dir.path());
        let target = ForgeRepository::github("github.com", "agavra", "tuicr");

        // when — the remote URL is unreachable, so any attempt to fetch would
        // fail; reporting success proves no fetch was attempted.
        let ok =
            fetch_pr_commits_into_checkout(dir.path(), &target, 12, &head, "refs/pull/12/head");

        // then
        assert!(ok);
    }

    #[test]
    fn fetching_gives_up_when_no_remote_serves_the_repo() {
        let dir = init_repo_with_origin("https://github.com/someone/other");
        let target = ForgeRepository::github("github.com", "agavra", "tuicr");
        assert!(!fetch_pr_commits_into_checkout(
            dir.path(),
            &target,
            12,
            "0000000000000000000000000000000000000000",
            "refs/pull/12/head",
        ));
    }

    /// Commit one file and return the new HEAD sha.
    fn commit_a_file(root: &Path) -> String {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(root)
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .expect("run git");
            assert!(out.status.success(), "git {args:?}: {:?}", out);
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        std::fs::write(root.join("README"), "hi\n").unwrap();
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test User"]);
        run(&["add", "README"]);
        run(&["commit", "-q", "-m", "init"]);
        run(&["rev-parse", "HEAD"])
    }

    fn init_repo_with_origin(url: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path()).expect("init repo");
        repo.remote("origin", url).expect("add origin");
        dir
    }

    #[test]
    fn detects_github_repository_from_origin() {
        let dir = init_repo_with_origin("https://github.com/agavra/tuicr");
        assert_eq!(
            detect_forge_repository(dir.path()),
            Some(ForgeRepository::github("github.com", "agavra", "tuicr"))
        );
    }

    #[test]
    fn hostname_only_parser_claims_azure_but_leaves_other_hosts_alone() {
        assert_eq!(
            parse_any_remote_url_by_hostname("https://dev.azure.com/myorg/myproject/_git/myrepo"),
            Some(ForgeRepository::azure(
                "dev.azure.com",
                "myorg/myproject",
                "myrepo"
            ))
        );
        // The GitHub catch-all is excluded, so anything it would have claimed
        // falls through to the caller's own rule. Were it in the chain, its
        // first-two-segments reading would turn the last URL into `git/owner`.
        assert_eq!(
            parse_any_remote_url_by_hostname("https://github.com/agavra/tuicr"),
            None
        );
        assert_eq!(
            parse_any_remote_url_by_hostname("https://code.example.com/git/owner/repo"),
            None
        );
    }

    #[test]
    fn local_checkout_matches_when_origin_equals_target() {
        let dir = init_repo_with_origin("https://github.com/agavra/tuicr");
        let target = ForgeRepository::github("github.com", "agavra", "tuicr");
        assert_eq!(
            local_checkout_for_repo(dir.path(), &target),
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn local_checkout_rejects_mismatched_repo() {
        let dir = init_repo_with_origin("https://github.com/contributor/tuicr");
        let target = ForgeRepository::github("github.com", "agavra", "tuicr");
        assert_eq!(local_checkout_for_repo(dir.path(), &target), None);
    }

    #[test]
    fn local_checkout_returns_none_outside_a_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = ForgeRepository::github("github.com", "agavra", "tuicr");
        assert_eq!(local_checkout_for_repo(dir.path(), &target), None);
    }

    #[test]
    fn local_checkout_matches_upstream_remote_in_fork_workflow() {
        let dir = init_repo_with_origin("https://github.com/contributor/tuicr");
        let repo = Repository::open(dir.path()).expect("open repo");
        repo.remote("upstream", "https://github.com/agavra/tuicr")
            .expect("add upstream");

        let target = ForgeRepository::github("github.com", "agavra", "tuicr");
        assert_eq!(
            local_checkout_for_repo(dir.path(), &target),
            Some(dir.path().to_path_buf())
        );
    }
}
