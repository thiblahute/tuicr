use crate::app::*;

const SOME_HASH: u64 = 0xabc;

fn test_session() -> ReviewSession {
    let mut session = ReviewSession::new(
        PathBuf::from("/repo"),
        "abc1234".to_string(),
        Some("main".to_string()),
        SessionDiffSource::WorkingTree,
    );
    session.add_file(
        PathBuf::from("src/main.rs"),
        FileStatus::Modified,
        SOME_HASH,
    );
    session
}

fn comment(id: &str, content: &str) -> Comment {
    let mut comment = Comment::new(content.to_string(), CommentType::from_id("note"), None);
    comment.id = id.to_string();
    comment
}

fn push_file_comment(session: &mut ReviewSession, id: &str, content: &str) {
    session
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .file_comments
        .push(comment(id, content));
}

fn file_comment_ids(session: &ReviewSession) -> Vec<String> {
    session
        .files
        .get(&PathBuf::from("src/main.rs"))
        .unwrap()
        .file_comments
        .iter()
        .map(|comment| comment.id.clone())
        .collect()
}

#[test]
fn should_merge_external_comment_without_losing_local_comment() {
    let base = test_session();
    let mut current = base.clone();
    let mut latest = base.clone();

    push_file_comment(&mut current, "local", "from tui");
    push_file_comment(&mut latest, "external", "from cli");

    let changed = App::merge_external_session_changes(&mut current, &base, &latest);

    assert_eq!(changed, 1);
    assert_eq!(file_comment_ids(&current), vec!["local", "external"]);
}

#[test]
fn should_not_resurrect_locally_deleted_comment_when_disk_is_unchanged() {
    let mut base = test_session();
    push_file_comment(&mut base, "deleted", "old");
    let mut current = base.clone();
    current
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .file_comments
        .clear();
    let latest = base.clone();

    let changed = App::merge_external_session_changes(&mut current, &base, &latest);

    assert_eq!(changed, 0);
    assert!(file_comment_ids(&current).is_empty());
}

#[test]
fn should_apply_external_edit_when_comment_is_unchanged_locally() {
    let mut base = test_session();
    push_file_comment(&mut base, "same", "old");
    let mut current = base.clone();
    let mut latest = base.clone();
    latest
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .file_comments[0]
        .content = "new".to_string();

    let changed = App::merge_external_session_changes(&mut current, &base, &latest);

    assert_eq!(changed, 1);
    assert_eq!(
        current
            .files
            .get(&PathBuf::from("src/main.rs"))
            .unwrap()
            .file_comments[0]
            .content,
        "new"
    );
}

#[test]
fn should_merge_an_external_reply_into_an_open_session() {
    // The agent's `tuicr review reply` writes to the session file; the review
    // watch tick merges it in without the reviewer doing anything.
    let mut base = test_session();
    push_file_comment(&mut base, "root", "handle the empty case");
    let mut current = base.clone();
    let mut latest = base.clone();

    let mut reply = comment("reply", "fixed in def4567");
    reply.author = "Claude".to_string();
    reply.in_reply_to = Some("root".to_string());
    latest
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .file_comments
        .push(reply);

    let changed = App::merge_external_session_changes(&mut current, &base, &latest);

    assert_eq!(changed, 1);
    assert_eq!(file_comment_ids(&current), vec!["root", "reply"]);
    let merged = &current
        .files
        .get(&PathBuf::from("src/main.rs"))
        .unwrap()
        .file_comments[1];
    assert_eq!(merged.in_reply_to.as_deref(), Some("root"));
    assert_eq!(merged.author, "Claude");
}

#[test]
fn should_not_bring_a_moved_comment_back_under_its_old_path() {
    // A comment that was re-anchored onto another file is still in the review;
    // the copy on disk is the one it left behind.
    let mut base = test_session();
    base.add_file(
        PathBuf::from("old/path.rs"),
        FileStatus::Modified,
        SOME_HASH,
    );
    base.get_file_mut(&PathBuf::from("old/path.rs"))
        .unwrap()
        .file_comments
        .push(comment("moved", "still worth saying"));
    let latest = base.clone();

    // The TUI moved it and dropped the emptied file.
    let mut current = base.clone();
    current.files.remove(&PathBuf::from("old/path.rs"));
    push_file_comment(&mut current, "moved", "still worth saying");

    let changed = App::merge_external_session_changes(&mut current, &base, &latest);

    assert_eq!(changed, 0, "nothing came back");
    assert_eq!(file_comment_ids(&current), vec!["moved"], "one copy of it");
    assert!(
        !current.files.contains_key(&PathBuf::from("old/path.rs")),
        "and the file it left is not resurrected"
    );
}

#[test]
fn should_still_take_a_file_added_by_another_writer() {
    let base = test_session();
    let mut current = base.clone();
    let mut latest = base.clone();
    latest.add_file(PathBuf::from("src/new.rs"), FileStatus::Added, SOME_HASH);
    latest
        .get_file_mut(&PathBuf::from("src/new.rs"))
        .unwrap()
        .file_comments
        .push(comment("external", "from cli"));
    push_file_comment(&mut current, "local", "from tui");

    let changed = App::merge_external_session_changes(&mut current, &base, &latest);

    assert_eq!(changed, 1);
    assert_eq!(
        current.files[&PathBuf::from("src/new.rs")].file_comments[0].id,
        "external"
    );
}
