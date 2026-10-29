use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use sea_orm::{EntityTrait, QueryOrder};

use super::*;
use crate::db::test_helpers::{fresh_in_memory_db, seed_conversation, seed_folder};
use crate::db::AppDatabase;
use crate::models::agent::AgentType;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn read(root: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(root.join(rel)).ok()
}

/// A repo with one commit: `a.txt`, `src/lib.rs`, and a `.gitignore` that
/// ignores `*.log` and `target/`.
fn repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "user.email", "test@example.com"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    write(&root, ".gitignore", "*.log\ntarget/\n");
    write(&root, "a.txt", "one\n");
    write(&root, "src/lib.rs", "fn main() {}\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    (dir, root)
}

async fn db_with_conversation(root: &Path) -> (AppDatabase, i32) {
    let db = fresh_in_memory_db().await;
    let folder = seed_folder(&db, &root.to_string_lossy()).await;
    let cid = seed_conversation(&db, folder, AgentType::ClaudeCode).await;
    (db, cid)
}

fn index_bytes(root: &Path) -> Vec<u8> {
    std::fs::read(root.join(".git/index")).unwrap()
}

async fn rows(db: &AppDatabase) -> Vec<CheckpointRow> {
    Checkpoint::find()
        .order_by_asc(phantom_checkpoint::Column::Seq)
        .all(&db.conn)
        .await
        .unwrap()
}

#[tokio::test]
async fn snapshot_leaves_index_head_branches_and_stash_untouched() {
    let (_dir, root) = repo();
    let (db, cid) = db_with_conversation(&root).await;

    // A stash entry, a staged change, an unstaged change, an untracked file
    // and an ignored one.
    write(&root, "a.txt", "stashed\n");
    git(&root, &["stash", "push", "-q", "-m", "keep-me"]);
    write(&root, "a.txt", "staged\n");
    git(&root, &["add", "a.txt"]);
    write(&root, "src/lib.rs", "fn main() { changed() }\n");
    write(&root, "notes/new.md", "untracked\n");
    write(&root, "debug.log", "ignored\n");

    let head_before = git(&root, &["rev-parse", "HEAD"]);
    let branches_before = git(&root, &["branch", "--list"]);
    let stash_before = git(&root, &["stash", "list"]);
    let index_before = index_bytes(&root);

    let row = snapshot_before_prompt(
        &db.conn,
        cid,
        &root.join("src"),
        Some("msg-1"),
        Some("hola".into()),
        SNAPSHOT_BUDGET,
    )
    .await
    .expect("checkpoint recorded");

    assert_eq!(index_bytes(&root), index_before, "user index rewritten");
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&root, &["branch", "--list"]), branches_before);
    assert_eq!(git(&root, &["stash", "list"]), stash_before);
    assert_eq!(git(&root, &["symbolic-ref", "HEAD"]), "refs/heads/main");

    assert_eq!(row.kind, KIND_PROMPT);
    assert_eq!(row.seq, 1);
    assert_eq!(row.message_id.as_deref(), Some("msg-1"));
    assert_eq!(row.repo_root, root.to_string_lossy());
    let commit = row.commit_sha.clone().unwrap();
    let ref_target = git(&root, &["rev-parse", &ref_name(cid, 1)]);
    assert_eq!(ref_target, commit);
    assert_eq!(
        git(&root, &["rev-parse", &format!("{commit}^")]),
        head_before
    );

    // The snapshot is the working tree, not the index: staged + unstaged +
    // untracked content, and no ignored file.
    assert_eq!(git(&root, &["show", &format!("{commit}:a.txt")]), "staged");
    assert_eq!(
        git(&root, &["show", &format!("{commit}:src/lib.rs")]),
        "fn main() { changed() }"
    );
    assert_eq!(
        git(&root, &["show", &format!("{commit}:notes/new.md")]),
        "untracked"
    );
    let files = git(&root, &["ls-tree", "-r", "--name-only", &commit]);
    assert!(!files.contains("debug.log"));
    // a.txt, src/lib.rs and notes/new.md differ from HEAD.
    assert_eq!(row.changed_files, 3);

    // The working files themselves are untouched.
    assert_eq!(read(&root, "a.txt").as_deref(), Some("staged\n"));
    assert_eq!(read(&root, "debug.log").as_deref(), Some("ignored\n"));
}

#[tokio::test]
async fn identical_tree_reuses_the_previous_commit() {
    let (_dir, root) = repo();
    let (db, cid) = db_with_conversation(&root).await;

    let first = take_checkpoint(&db.conn, cid, &root, KIND_PROMPT, None, None)
        .await
        .unwrap();
    let second = take_checkpoint(&db.conn, cid, &root, KIND_PROMPT, None, None)
        .await
        .unwrap();
    assert!(!first.deduped);
    assert!(second.deduped);
    assert_eq!(second.commit_sha, first.commit_sha);
    assert_eq!(second.changed_files, 0);
    // Each row still holds its own ref.
    assert_eq!(
        git(&root, &["rev-parse", &ref_name(cid, 2)]),
        first.commit_sha.clone().unwrap()
    );

    write(&root, "a.txt", "two\n");
    let third = take_checkpoint(&db.conn, cid, &root, KIND_PROMPT, None, None)
        .await
        .unwrap();
    assert!(!third.deduped);
    assert_ne!(third.commit_sha, first.commit_sha);
    assert_eq!(third.changed_files, 1);
}

#[tokio::test]
async fn restore_takes_a_safety_checkpoint_and_matches_the_checkpoint() {
    let (_dir, root) = repo();
    let (db, cid) = db_with_conversation(&root).await;
    write(&root, "build.log", "ignored before\n");
    let checkpoint = take_checkpoint(&db.conn, cid, &root, KIND_PROMPT, None, None)
        .await
        .unwrap();

    // What an agent's turn might do afterwards.
    write(&root, "a.txt", "agent edit\n");
    std::fs::remove_file(root.join("src/lib.rs")).unwrap();
    write(&root, "feature/new/mod.rs", "pub fn x() {}\n");
    write(&root, "target/out.bin", "ignored build output\n");
    write(&root, "build.log", "ignored, rewritten\n");

    let head_before = git(&root, &["rev-parse", "HEAD"]);
    let index_before = index_bytes(&root);

    let result = restore(&db.conn, cid, checkpoint.id, false).await.unwrap();

    assert_eq!(read(&root, "a.txt").as_deref(), Some("one\n"));
    assert_eq!(read(&root, "src/lib.rs").as_deref(), Some("fn main() {}\n"));
    assert!(!root.join("feature/new/mod.rs").exists(), "new file kept");
    assert!(!root.join("feature").exists(), "emptied dirs kept");
    // Ignored files are never touched, whatever happened to them.
    assert_eq!(
        read(&root, "target/out.bin").as_deref(),
        Some("ignored build output\n")
    );
    assert_eq!(
        read(&root, "build.log").as_deref(),
        Some("ignored, rewritten\n")
    );
    // Index and HEAD as they were.
    assert_eq!(index_bytes(&root), index_before);
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(result.deleted, 1);
    assert_eq!(result.written, 2);

    // The safety checkpoint holds the pre-restore tree, so it undoes.
    let all = rows(&db).await;
    let safety = all
        .iter()
        .find(|r| r.id == result.safety_checkpoint_id)
        .unwrap();
    assert_eq!(safety.kind, KIND_SAFETY);
    let safety_commit = safety.commit_sha.clone().unwrap();
    assert_eq!(
        git(&root, &["show", &format!("{safety_commit}:a.txt")]),
        "agent edit"
    );

    let undo = restore(&db.conn, cid, safety.id, false).await.unwrap();
    assert_eq!(read(&root, "a.txt").as_deref(), Some("agent edit\n"));
    assert!(!root.join("src/lib.rs").exists());
    assert_eq!(
        read(&root, "feature/new/mod.rs").as_deref(),
        Some("pub fn x() {}\n")
    );
    assert_ne!(undo.safety_checkpoint_id, result.safety_checkpoint_id);
    assert_eq!(index_bytes(&root), index_before);
}

#[tokio::test]
async fn restore_is_refused_while_a_turn_runs() {
    let (_dir, root) = repo();
    let (db, cid) = db_with_conversation(&root).await;
    let checkpoint = take_checkpoint(&db.conn, cid, &root, KIND_PROMPT, None, None)
        .await
        .unwrap();
    write(&root, "a.txt", "agent edit\n");

    let err = restore(&db.conn, cid, checkpoint.id, true)
        .await
        .unwrap_err();
    assert!(matches!(err.code, AppErrorCode::TurnInProgress));
    assert_eq!(read(&root, "a.txt").as_deref(), Some("agent edit\n"));
    assert_eq!(
        rows(&db).await.len(),
        1,
        "no safety checkpoint when refused"
    );
}

#[tokio::test]
async fn restore_rejects_another_conversations_checkpoint() {
    let (_dir, root) = repo();
    let (db, cid) = db_with_conversation(&root).await;
    let checkpoint = take_checkpoint(&db.conn, cid, &root, KIND_PROMPT, None, None)
        .await
        .unwrap();
    let err = restore(&db.conn, cid + 1, checkpoint.id, false)
        .await
        .unwrap_err();
    assert!(matches!(err.code, AppErrorCode::NotFound));
}

#[tokio::test]
async fn non_git_folder_records_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (db, cid) = db_with_conversation(dir.path()).await;
    let row = snapshot_before_prompt(&db.conn, cid, dir.path(), None, None, SNAPSHOT_BUDGET).await;
    assert!(row.is_none());
    assert!(rows(&db).await.is_empty());

    let preview = preview_for_turn(&db.conn, cid, None, Some(Utc::now()), None, false)
        .await
        .unwrap();
    assert_eq!(preview.status, PreviewStatus::NotGit);
}

#[tokio::test]
async fn an_exhausted_budget_records_the_gap() {
    let (_dir, root) = repo();
    let (db, cid) = db_with_conversation(&root).await;
    let row = snapshot_before_prompt(&db.conn, cid, &root, None, None, Duration::from_nanos(1))
        .await
        .expect("skip recorded");
    assert_eq!(row.skipped_reason.as_deref(), Some("timeout"));
    assert!(row.commit_sha.is_none());
    assert!(git(&root, &["for-each-ref", "refs/phantom/"]).is_empty());

    let preview = preview_for_turn(
        &db.conn,
        cid,
        None,
        Some(row.created_at + chrono::Duration::seconds(5)),
        None,
        false,
    )
    .await
    .unwrap();
    assert_eq!(preview.status, PreviewStatus::Skipped);
}

#[tokio::test]
async fn preview_lists_what_a_restore_would_undo() {
    let (_dir, root) = repo();
    let (db, cid) = db_with_conversation(&root).await;
    let row = snapshot_before_prompt(&db.conn, cid, &root, Some("m1"), None, SNAPSHOT_BUDGET)
        .await
        .unwrap();
    write(&root, "a.txt", "one\ntwo\nthree\n");
    write(&root, "new.rs", "x\n");
    std::fs::remove_file(root.join("src/lib.rs")).unwrap();

    let preview = preview_for_turn(&db.conn, cid, Some("m1"), None, None, false)
        .await
        .unwrap();
    assert_eq!(preview.status, PreviewStatus::Ready);
    assert_eq!(preview.checkpoint.unwrap().id, row.id);
    let by_path = |p: &str| preview.files.iter().find(|f| f.path == p).unwrap().clone();
    assert_eq!(by_path("a.txt").status, "modified");
    assert_eq!(by_path("a.txt").additions, Some(2));
    assert_eq!(by_path("new.rs").status, "added");
    assert_eq!(by_path("src/lib.rs").status, "deleted");
    assert_eq!(preview.files.len(), 3);
}

#[tokio::test]
async fn prune_keeps_the_newest_rows_and_drops_old_refs() {
    let (_dir, root) = repo();
    let (db, cid) = db_with_conversation(&root).await;
    for i in 0..5 {
        write(&root, "a.txt", &format!("v{i}\n"));
        take_checkpoint(&db.conn, cid, &root, KIND_PROMPT, None, None)
            .await
            .unwrap();
    }
    let removed = prune(&db.conn, cid, 2).await.unwrap();
    assert_eq!(removed, 3);
    let left: Vec<i32> = rows(&db).await.iter().map(|r| r.seq).collect();
    assert_eq!(left, vec![4, 5]);
    let refs = git(
        &root,
        &["for-each-ref", "--format=%(refname)", "refs/phantom/"],
    );
    assert_eq!(
        refs.lines().collect::<Vec<_>>(),
        vec![ref_name(cid, 4), ref_name(cid, 5)]
    );
}

#[tokio::test]
async fn deleting_a_conversation_drops_its_refs_and_rows() {
    let (_dir, root) = repo();
    let (db, cid) = db_with_conversation(&root).await;
    take_checkpoint(&db.conn, cid, &root, KIND_PROMPT, None, None)
        .await
        .unwrap();
    write(&root, "a.txt", "two\n");
    take_checkpoint(&db.conn, cid, &root, KIND_PROMPT, None, None)
        .await
        .unwrap();
    cleanup_conversation(&db.conn, cid).await;
    assert!(rows(&db).await.is_empty());
    assert!(git(&root, &["for-each-ref", "refs/phantom/"]).is_empty());
}

// ─── resolve_for_turn ──────────────────────────────────────────────────────

fn row(
    id: i32,
    seq: i32,
    secs: i64,
    message_id: Option<&str>,
    preview: Option<&str>,
) -> CheckpointRow {
    CheckpointRow {
        id,
        conversation_id: 1,
        seq,
        kind: KIND_PROMPT.into(),
        message_id: message_id.map(Into::into),
        prompt_preview: preview.map(Into::into),
        repo_root: "/repo".into(),
        ref_name: Some(ref_name(1, seq)),
        commit_sha: Some(format!("c{seq}")),
        tree_sha: Some(format!("t{seq}")),
        changed_files: 0,
        deduped: false,
        skipped_reason: None,
        created_at: Utc.timestamp_opt(1_000_000 + secs, 0).unwrap(),
    }
}

fn at(secs: i64) -> Option<chrono::DateTime<Utc>> {
    Some(Utc.timestamp_opt(1_000_000 + secs, 0).unwrap())
}

#[test]
fn resolve_prefers_the_exact_message_id() {
    let rows = vec![
        row(1, 1, 0, Some("a"), Some("first")),
        row(2, 2, 60, Some("b"), Some("second")),
    ];
    let hit = resolve_for_turn(&rows, Some("a"), at(120), Some("second")).unwrap();
    assert_eq!(hit.id, 1);
}

#[test]
fn resolve_takes_the_newest_checkpoint_before_the_message() {
    let rows = vec![
        row(1, 1, 0, None, Some("arregla el login")),
        row(2, 2, 60, None, Some("ahora los tests")),
        row(3, 3, 120, None, Some("y el README")),
    ];
    let hit = resolve_for_turn(&rows, Some("turn-3"), at(61), Some("Ahora los tests")).unwrap();
    assert_eq!(hit.id, 2);
    // Within the clock slack.
    let hit = resolve_for_turn(&rows, None, at(119), Some("y el README")).unwrap();
    assert_eq!(hit.id, 3);
    // Before any checkpoint.
    assert!(resolve_for_turn(&rows, None, at(-5), Some("arregla el login")).is_none());
}

#[test]
fn resolve_refuses_a_message_whose_text_matches_no_checkpoint() {
    let rows = vec![
        row(1, 1, 0, None, Some("arregla el login")),
        row(2, 2, 60, None, Some("ahora los tests")),
    ];
    // Typed straight into the agent's CLI after checkpoint 2: no match.
    assert!(resolve_for_turn(&rows, None, at(90), Some("algo totalmente distinto")).is_none());
    // A later skipped-over row whose text does match is found.
    let hit = resolve_for_turn(&rows, None, at(90), Some("arregla el login por favor")).unwrap();
    assert_eq!(hit.id, 1);
}

#[test]
fn resolve_tolerates_formatting_around_the_prompt() {
    let rows = vec![row(1, 1, 0, None, Some("Revisa  `src/main.rs`,\nporfa"))];
    let hit = resolve_for_turn(
        &rows,
        None,
        at(3),
        Some("[Image #1] revisa src/main.rs porfa"),
    )
    .unwrap();
    assert_eq!(hit.id, 1);
}

#[test]
fn resolve_ignores_safety_checkpoints() {
    let mut safety = row(2, 2, 30, None, None);
    safety.kind = KIND_SAFETY.into();
    let rows = vec![row(1, 1, 0, None, Some("hola")), safety];
    let hit = resolve_for_turn(&rows, None, at(40), Some("hola")).unwrap();
    assert_eq!(hit.id, 1);
}
