//! Checkpoints / rewind — Phantom's agent-agnostic take on Claude Code's
//! `/rewind`. No ACP adapter exposes one, so Phantom owns it:
//!
//! * **Snapshot.** Just before a prompt is forwarded to any agent whose
//!   working dir is inside a git repo, [`before_prompt`] stores the working
//!   tree as a commit under `refs/phantom/checkpoints/<conversation>/<seq>`
//!   (see [`git::snapshot_tree`]: the user's index, HEAD, branches and stash
//!   are never touched) and records it in `phantom_checkpoint`. A tree equal
//!   to the previous checkpoint's reuses its commit. The whole thing runs
//!   under [`SNAPSHOT_BUDGET`]; a repo too big for it is skipped (and the gap
//!   recorded) rather than holding the prompt back.
//! * **Restore.** [`restore`] first snapshots the tree it is about to
//!   overwrite (a `safety` checkpoint — restoring THAT is "undo"), then makes
//!   the working tree match the chosen checkpoint, again without touching the
//!   index or HEAD.
//! * **Housekeeping.** The last [`KEEP_PER_CONVERSATION`] rows per
//!   conversation are kept, older refs deleted; deleting a conversation
//!   deletes all of its refs.
//!
//! Nothing here talks to a model.

pub mod git;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
};
use serde::Serialize;
use tokio::sync::RwLock;

use crate::acp::manager::ConnectionManager;
use crate::acp::session_state::SessionState;
use crate::acp::types::PromptInputBlock;
use crate::app_error::{AppCommandError, AppErrorCode};
use crate::db::entities::phantom_checkpoint::{self, Entity as Checkpoint, Model as CheckpointRow};

/// Longest a snapshot may hold a prompt back. Local git on a normal repo
/// takes well under 100 ms; past this the prompt goes out without one.
pub const SNAPSHOT_BUDGET: Duration = Duration::from_secs(2);

/// Rows (and refs) kept per conversation; older ones are pruned.
pub const KEEP_PER_CONVERSATION: u64 = 100;

/// Upper bound for the housekeeping git calls (prune, delete cleanup), which
/// never sit on a prompt's path.
const HOUSEKEEPING_BUDGET: Duration = Duration::from_secs(10);

/// Files listed in a restore preview; the rest are counted, not listed.
const PREVIEW_FILE_LIMIT: usize = 200;

pub const KIND_PROMPT: &str = "prompt";
pub const KIND_SAFETY: &str = "safety";

const REF_ROOT: &str = "refs/phantom/checkpoints";

/// The database the prompt hook writes to. The hook sits on the shared
/// prompt path, which has no database handle of its own (the chat channel's
/// plain `send_prompt` doesn't carry one), so `db::init_database` — the one
/// place every runtime opens its database — installs it here.
static DB: OnceLock<DatabaseConnection> = OnceLock::new();

pub fn install_db(conn: DatabaseConnection) {
    let _ = DB.set(conn);
}

/// Serializes restores: two overlapping restores of one repo would each
/// snapshot a tree the other is rewriting.
static RESTORE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn ref_name(conversation_id: i32, seq: i32) -> String {
    format!("{REF_ROOT}/{conversation_id}/{seq}")
}

fn conversation_ref_prefix(conversation_id: i32) -> String {
    format!("{REF_ROOT}/{conversation_id}/")
}

/// The prompt's text blocks joined and trimmed, capped — enough to tell
/// which transcript message a row belongs to.
pub fn prompt_preview(blocks: &[PromptInputBlock]) -> Option<String> {
    let joined = blocks
        .iter()
        .filter_map(|b| match b {
            PromptInputBlock::Text { text } => Some(text.trim()),
            _ => None,
        })
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.chars().take(400).collect())
}

// ─── Prompt hook ───────────────────────────────────────────────────────────

/// Called on the shared prompt path just before the prompt is handed to the
/// agent. Snapshots the conversation's working tree when it is a git repo;
/// a no-op for unlinked connections, non-git folders, or a prompt about to be
/// rejected because a turn is already running. Never fails the send.
pub async fn before_prompt(
    state: &Arc<RwLock<SessionState>>,
    message_id: Option<&str>,
    blocks: &[PromptInputBlock],
) {
    let Some(conn) = DB.get() else { return };
    let (conversation_id, working_dir, in_flight) = {
        let s = state.read().await;
        (s.conversation_id, s.working_dir.clone(), s.turn_in_flight)
    };
    let (Some(conversation_id), Some(working_dir), false) =
        (conversation_id, working_dir, in_flight)
    else {
        return;
    };
    snapshot_before_prompt(
        conn,
        conversation_id,
        &working_dir,
        message_id,
        prompt_preview(blocks),
        SNAPSHOT_BUDGET,
    )
    .await;
}

/// Take the prompt checkpoint for `conversation_id` within `budget`.
/// Returns the recorded row: a real checkpoint, or a skipped one marking the
/// gap. `None` when `working_dir` is not in a git repo (nothing recorded).
pub async fn snapshot_before_prompt(
    conn: &DatabaseConnection,
    conversation_id: i32,
    working_dir: &Path,
    message_id: Option<&str>,
    preview: Option<String>,
    budget: Duration,
) -> Option<CheckpointRow> {
    let started = Instant::now();
    let root = match tokio::time::timeout(budget, git::repo_root(working_dir)).await {
        Ok(Some(root)) => root,
        Ok(None) => return None,
        Err(_) => {
            tracing::warn!(
                conversation_id,
                dir = %working_dir.display(),
                "[checkpoint] git did not answer in time; prompt sent without a checkpoint"
            );
            return record_skipped(
                conn,
                conversation_id,
                working_dir,
                message_id,
                preview,
                "timeout",
            )
            .await;
        }
    };
    let remaining = budget.saturating_sub(started.elapsed());
    let taken = tokio::time::timeout(
        remaining,
        take_checkpoint(
            conn,
            conversation_id,
            &root,
            KIND_PROMPT,
            message_id,
            preview.clone(),
        ),
    )
    .await;
    match taken {
        Ok(Ok(row)) => {
            tracing::debug!(
                conversation_id,
                seq = row.seq,
                deduped = row.deduped,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "[checkpoint] snapshot taken"
            );
            spawn_prune(conn.clone(), conversation_id);
            Some(row)
        }
        Ok(Err(e)) => {
            tracing::warn!(conversation_id, error = %e, "[checkpoint] snapshot failed; skipped");
            record_skipped(
                conn,
                conversation_id,
                &root,
                message_id,
                preview,
                &format!("error: {}", e.message),
            )
            .await
        }
        Err(_) => {
            tracing::warn!(
                conversation_id,
                repo = %root.display(),
                budget_ms = budget.as_millis() as u64,
                "[checkpoint] snapshot exceeded its budget; prompt sent without a checkpoint"
            );
            record_skipped(conn, conversation_id, &root, message_id, preview, "timeout").await
        }
    }
}

async fn record_skipped(
    conn: &DatabaseConnection,
    conversation_id: i32,
    root: &Path,
    message_id: Option<&str>,
    preview: Option<String>,
    reason: &str,
) -> Option<CheckpointRow> {
    let seq = match next_seq(conn, conversation_id).await {
        Ok(seq) => seq,
        Err(e) => {
            tracing::warn!(conversation_id, error = %e.message, "[checkpoint] could not record skip");
            return None;
        }
    };
    let row = phantom_checkpoint::ActiveModel {
        conversation_id: Set(conversation_id),
        seq: Set(seq),
        kind: Set(KIND_PROMPT.to_string()),
        message_id: Set(message_id.map(str::to_string)),
        prompt_preview: Set(preview),
        repo_root: Set(root.to_string_lossy().into_owned()),
        ref_name: Set(None),
        commit_sha: Set(None),
        tree_sha: Set(None),
        changed_files: Set(0),
        deduped: Set(false),
        skipped_reason: Set(Some(reason.to_string())),
        created_at: Set(Utc::now()),
        ..Default::default()
    };
    match row.insert(conn).await {
        Ok(row) => Some(row),
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "[checkpoint] could not record skip");
            None
        }
    }
}

fn db_err(e: sea_orm::DbErr) -> AppCommandError {
    AppCommandError::database_error(e.to_string())
}

fn git_err(e: git::GitError) -> AppCommandError {
    AppCommandError::external_command("git failed", e.to_string())
}

async fn next_seq(conn: &DatabaseConnection, conversation_id: i32) -> Result<i32, AppCommandError> {
    let last = Checkpoint::find()
        .filter(phantom_checkpoint::Column::ConversationId.eq(conversation_id))
        .order_by_desc(phantom_checkpoint::Column::Seq)
        .one(conn)
        .await
        .map_err(db_err)?;
    Ok(last.map(|r| r.seq + 1).unwrap_or(1))
}

/// Snapshot `root` now and record it as a `kind` checkpoint. Reuses the
/// previous checkpoint's commit when the tree is unchanged; every row still
/// gets its own ref, so pruning one row can never orphan another's commit.
pub async fn take_checkpoint(
    conn: &DatabaseConnection,
    conversation_id: i32,
    root: &Path,
    kind: &str,
    message_id: Option<&str>,
    preview: Option<String>,
) -> Result<CheckpointRow, AppCommandError> {
    let tree = git::snapshot_tree(root).await.map_err(git_err)?;
    record_checkpoint(
        conn,
        conversation_id,
        root,
        kind,
        &tree,
        message_id,
        preview,
    )
    .await
}

async fn record_checkpoint(
    conn: &DatabaseConnection,
    conversation_id: i32,
    root: &Path,
    kind: &str,
    tree: &str,
    message_id: Option<&str>,
    preview: Option<String>,
) -> Result<CheckpointRow, AppCommandError> {
    let root_str = root.to_string_lossy().into_owned();
    let previous = Checkpoint::find()
        .filter(phantom_checkpoint::Column::ConversationId.eq(conversation_id))
        .filter(phantom_checkpoint::Column::RepoRoot.eq(root_str.clone()))
        .filter(phantom_checkpoint::Column::CommitSha.is_not_null())
        .order_by_desc(phantom_checkpoint::Column::Seq)
        .one(conn)
        .await
        .map_err(db_err)?;

    let seq = next_seq(conn, conversation_id).await?;
    let name = ref_name(conversation_id, seq);

    let reused = previous
        .as_ref()
        .filter(|p| p.tree_sha.as_deref() == Some(tree))
        .and_then(|p| p.commit_sha.clone());
    let (commit, deduped, changed) = match reused {
        Some(commit) => (commit, true, 0),
        None => {
            let head = git::head_commit(root).await;
            let message = format!("phantom checkpoint {conversation_id}/{seq} ({kind})");
            let commit = git::commit_tree(root, tree, head.as_deref(), &message)
                .await
                .map_err(git_err)?;
            let base = match (&previous, &head) {
                (Some(p), _) => p.tree_sha.clone(),
                (None, Some(h)) => Some(h.clone()),
                (None, None) => None,
            };
            let changed = match base {
                Some(base) => git::count_changed(root, &base, tree).await,
                None => git::count_files(root, tree).await,
            }
            .unwrap_or(0);
            (commit, false, changed)
        }
    };
    git::update_ref(root, &name, &commit)
        .await
        .map_err(git_err)?;

    let row = phantom_checkpoint::ActiveModel {
        conversation_id: Set(conversation_id),
        seq: Set(seq),
        kind: Set(kind.to_string()),
        message_id: Set(message_id.map(str::to_string)),
        prompt_preview: Set(preview),
        repo_root: Set(root_str),
        ref_name: Set(Some(name)),
        commit_sha: Set(Some(commit)),
        tree_sha: Set(Some(tree.to_string())),
        changed_files: Set(i32::try_from(changed).unwrap_or(i32::MAX)),
        deduped: Set(deduped),
        skipped_reason: Set(None),
        created_at: Set(Utc::now()),
        ..Default::default()
    };
    row.insert(conn).await.map_err(db_err)
}

// ─── Housekeeping ──────────────────────────────────────────────────────────

fn spawn_prune(conn: DatabaseConnection, conversation_id: i32) {
    tokio::spawn(async move {
        if let Err(e) = prune(&conn, conversation_id, KEEP_PER_CONVERSATION).await {
            tracing::warn!(conversation_id, error = %e.message, "[checkpoint] prune failed");
        }
    });
}

/// Keep the newest `keep` rows of a conversation; delete the rest and their
/// refs (their commits then become unreachable and `git gc` reclaims them).
pub async fn prune(
    conn: &DatabaseConnection,
    conversation_id: i32,
    keep: u64,
) -> Result<usize, AppCommandError> {
    // `OFFSET` without `LIMIT` is not valid SQLite, so take the newest rows
    // and split the tail in memory (a conversation never holds many more than
    // `keep` rows anyway).
    let mut newest = Checkpoint::find()
        .filter(phantom_checkpoint::Column::ConversationId.eq(conversation_id))
        .order_by_desc(phantom_checkpoint::Column::Seq)
        .all(conn)
        .await
        .map_err(db_err)?;
    let keep = usize::try_from(keep).unwrap_or(usize::MAX);
    if newest.len() <= keep {
        return Ok(0);
    }
    let stale = newest.split_off(keep);
    delete_rows_and_refs(conn, &stale).await?;
    Ok(stale.len())
}

/// Drop every checkpoint of a deleted conversation: all refs under its
/// prefix in every repo it snapshotted, then the rows. Best effort.
pub async fn cleanup_conversation(conn: &DatabaseConnection, conversation_id: i32) {
    let rows = match Checkpoint::find()
        .filter(phantom_checkpoint::Column::ConversationId.eq(conversation_id))
        .all(conn)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "[checkpoint] cleanup could not read rows");
            return;
        }
    };
    if rows.is_empty() {
        return;
    }
    let mut roots: Vec<&str> = rows.iter().map(|r| r.repo_root.as_str()).collect();
    roots.sort_unstable();
    roots.dedup();
    let prefix = conversation_ref_prefix(conversation_id);
    for root in roots {
        let root = Path::new(root);
        if !root.is_dir() {
            continue;
        }
        let deleted = tokio::time::timeout(HOUSEKEEPING_BUDGET, async {
            let refs = git::list_refs(root, &prefix).await?;
            git::delete_refs(root, &refs).await
        })
        .await;
        if !matches!(deleted, Ok(Ok(()))) {
            tracing::warn!(
                conversation_id,
                repo = %root.display(),
                "[checkpoint] could not delete checkpoint refs"
            );
        }
    }
    if let Err(e) = Checkpoint::delete_many()
        .filter(phantom_checkpoint::Column::ConversationId.eq(conversation_id))
        .exec(conn)
        .await
    {
        tracing::warn!(conversation_id, error = %e, "[checkpoint] cleanup could not delete rows");
    }
}

async fn delete_rows_and_refs(
    conn: &DatabaseConnection,
    rows: &[CheckpointRow],
) -> Result<(), AppCommandError> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut by_root: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
    for row in rows {
        if let Some(name) = &row.ref_name {
            by_root
                .entry(row.repo_root.as_str())
                .or_default()
                .push(name.clone());
        }
    }
    for (root, names) in by_root {
        let root = Path::new(root);
        if !root.is_dir() {
            continue;
        }
        match tokio::time::timeout(HOUSEKEEPING_BUDGET, git::delete_refs(root, &names)).await {
            Ok(Ok(())) => {}
            _ => {
                tracing::warn!(repo = %root.display(), "[checkpoint] could not delete pruned refs")
            }
        }
    }
    let ids: Vec<i32> = rows.iter().map(|r| r.id).collect();
    Checkpoint::delete_many()
        .filter(phantom_checkpoint::Column::Id.is_in(ids))
        .exec(conn)
        .await
        .map_err(db_err)?;
    Ok(())
}

// ─── Matching a transcript message to its checkpoint ───────────────────────

/// Clock slack between the backend stamping a checkpoint and the agent
/// stamping the user message it wrote right after.
const TURN_CLOCK_SLACK_SECS: i64 = 1;

/// How far back a mismatching nearest row may be passed over looking for one
/// whose prompt text matches.
const TURN_TEXT_SEARCH_WINDOW_SECS: i64 = 600;

/// Lower-case alphanumerics only, so formatting the agent adds around a
/// prompt (attachment markers, quoting, whitespace) doesn't defeat the match.
fn text_key(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether a row's prompt and a transcript message's text plausibly are the
/// same message. Unknown on either side counts as a match.
fn text_matches(preview: Option<&str>, turn_text: Option<&str>) -> bool {
    let (Some(a), Some(b)) = (preview, turn_text) else {
        return true;
    };
    let (a, b) = (text_key(a), text_key(b));
    if a.is_empty() || b.is_empty() {
        return true;
    }
    let k = 32.min(a.chars().count()).min(b.chars().count());
    let head = |s: &str| s.chars().take(k).collect::<String>();
    b.contains(&head(&a)) || a.contains(&head(&b))
}

/// The prompt checkpoint taken right before the user message described by
/// `message_id` / `turn_at` / `turn_text`, among one conversation's rows.
///
/// An exact message id wins (a message sent in this session). Otherwise the
/// checkpoint is the newest one stamped no later than the message: each
/// prompt's checkpoint lands before the agent records the prompt, and the
/// next one only after its turn ends. The prompt text must agree, so a
/// message that never went through Phantom (typed straight into the agent's
/// CLI) resolves to nothing rather than to an older, wrong tree.
pub fn resolve_for_turn<'a>(
    rows: &'a [CheckpointRow],
    message_id: Option<&str>,
    turn_at: Option<DateTime<Utc>>,
    turn_text: Option<&str>,
) -> Option<&'a CheckpointRow> {
    let mut prompts: Vec<&CheckpointRow> = rows.iter().filter(|r| r.kind == KIND_PROMPT).collect();
    prompts.sort_by_key(|r| r.seq);

    if let Some(id) = message_id.filter(|id| !id.is_empty()) {
        if let Some(row) = prompts.iter().find(|r| r.message_id.as_deref() == Some(id)) {
            return Some(row);
        }
    }

    let at = turn_at?;
    let limit = at + chrono::Duration::seconds(TURN_CLOCK_SLACK_SECS);
    let candidates: Vec<&CheckpointRow> = prompts
        .into_iter()
        .filter(|r| r.created_at <= limit)
        .collect();
    let nearest = candidates.last()?;
    if text_matches(nearest.prompt_preview.as_deref(), turn_text) {
        return Some(nearest);
    }
    let floor = at - chrono::Duration::seconds(TURN_TEXT_SEARCH_WINDOW_SECS);
    candidates
        .iter()
        .rev()
        .skip(1)
        .take_while(|r| r.created_at >= floor)
        .find(|r| {
            r.prompt_preview.is_some() && text_matches(r.prompt_preview.as_deref(), turn_text)
        })
        .copied()
}

// ─── Preview / restore (the dialog's two calls) ────────────────────────────

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreviewStatus {
    /// A checkpoint exists and can be restored.
    Ready,
    /// The conversation's folder is not a git repo: conversation-only rewind.
    NotGit,
    /// No checkpoint was taken before this message (sent before Phantom
    /// recorded checkpoints, or outside Phantom, or already pruned).
    NoCheckpoint,
    /// The snapshot before this message was skipped (timeout / git error).
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointInfo {
    pub id: i32,
    pub seq: i32,
    pub kind: String,
    pub created_at: DateTime<Utc>,
    pub changed_files: i32,
    pub skipped_reason: Option<String>,
}

impl From<&CheckpointRow> for CheckpointInfo {
    fn from(r: &CheckpointRow) -> Self {
        Self {
            id: r.id,
            seq: r.seq,
            kind: r.kind.clone(),
            created_at: r.created_at,
            changed_files: r.changed_files,
            skipped_reason: r.skipped_reason.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChangedFile {
    pub path: String,
    /// Relative to the checkpoint: `added` (created since — a restore deletes
    /// it), `deleted` (removed since — a restore brings it back) or
    /// `modified`.
    pub status: String,
    pub additions: Option<u32>,
    pub deletions: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointPreview {
    pub status: PreviewStatus,
    pub checkpoint: Option<CheckpointInfo>,
    /// What changed between the checkpoint and the working tree right now,
    /// i.e. what a code restore would undo.
    pub files: Vec<ChangedFile>,
    /// Files beyond `files` that were left out of the list.
    pub more_files: usize,
    /// A turn is running in this conversation or this repo: restore refused.
    pub busy: bool,
    /// The repo the checkpoint belongs to (for the caller's busy check).
    #[serde(skip)]
    pub repo_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    /// The checkpoint of the tree as it was just before the restore —
    /// restoring it undoes this restore.
    pub safety_checkpoint_id: i32,
    pub written: usize,
    pub deleted: usize,
}

/// Whether a turn is running in `conversation_id`, or in any conversation
/// whose working dir lies inside `repo_root` — either would race a restore.
pub async fn turn_running(
    manager: &ConnectionManager,
    conversation_id: i32,
    repo_root: Option<&Path>,
) -> bool {
    let connections = manager.connections.lock().await;
    for conn in connections.values() {
        let s = conn.state.read().await;
        if !s.turn_in_flight {
            continue;
        }
        if s.conversation_id == Some(conversation_id) {
            return true;
        }
        if let (Some(root), Some(wd)) = (repo_root, s.working_dir.as_ref()) {
            if wd.starts_with(root) {
                return true;
            }
        }
    }
    false
}

async fn conversation_rows(
    conn: &DatabaseConnection,
    conversation_id: i32,
) -> Result<Vec<CheckpointRow>, AppCommandError> {
    Checkpoint::find()
        .filter(phantom_checkpoint::Column::ConversationId.eq(conversation_id))
        .order_by_asc(phantom_checkpoint::Column::Seq)
        .all(conn)
        .await
        .map_err(db_err)
}

/// The conversation's folder path, when it has one on disk.
async fn conversation_dir(conn: &DatabaseConnection, conversation_id: i32) -> Option<PathBuf> {
    let conv = crate::db::service::conversation_service::get_by_id(conn, conversation_id)
        .await
        .ok()?;
    let folder = crate::db::service::folder_service::get_folder_by_id(conn, conv.folder_id)
        .await
        .ok()??;
    Some(PathBuf::from(folder.path))
}

/// What "Rebobinar aquí" on one user message would do to the code.
pub async fn preview_for_turn(
    conn: &DatabaseConnection,
    conversation_id: i32,
    message_id: Option<&str>,
    turn_at: Option<DateTime<Utc>>,
    turn_text: Option<&str>,
    busy: bool,
) -> Result<CheckpointPreview, AppCommandError> {
    let rows = conversation_rows(conn, conversation_id).await?;
    let empty = |status: PreviewStatus, checkpoint: Option<CheckpointInfo>| CheckpointPreview {
        status,
        checkpoint,
        files: Vec::new(),
        more_files: 0,
        busy,
        repo_root: None,
    };

    let Some(row) = resolve_for_turn(&rows, message_id, turn_at, turn_text) else {
        let in_git = match rows.last() {
            Some(r) => git::repo_root(Path::new(&r.repo_root)).await.is_some(),
            None => match conversation_dir(conn, conversation_id).await {
                Some(dir) => git::repo_root(&dir).await.is_some(),
                None => false,
            },
        };
        let status = if in_git {
            PreviewStatus::NoCheckpoint
        } else {
            PreviewStatus::NotGit
        };
        return Ok(empty(status, None));
    };
    let Some(tree) = row.tree_sha.clone() else {
        return Ok(empty(PreviewStatus::Skipped, Some(row.into())));
    };
    let root = PathBuf::from(&row.repo_root);
    if git::repo_root(&root).await.is_none() {
        return Ok(empty(PreviewStatus::NotGit, Some(row.into())));
    }

    let current = git::snapshot_tree(&root).await.map_err(git_err)?;
    let entries = git::diff_entries(&root, &tree, &current)
        .await
        .map_err(git_err)?;
    let stats = git::diff_numstat(&root, &tree, &current)
        .await
        .map_err(git_err)?;
    let more_files = entries.len().saturating_sub(PREVIEW_FILE_LIMIT);
    let files = entries
        .into_iter()
        .take(PREVIEW_FILE_LIMIT)
        .map(|e| {
            let (additions, deletions) = stats.get(&e.path).copied().unwrap_or((None, None));
            ChangedFile {
                status: match e.status {
                    'A' => "added",
                    'D' => "deleted",
                    _ => "modified",
                }
                .to_string(),
                path: e.path,
                additions,
                deletions,
            }
        })
        .collect();
    Ok(CheckpointPreview {
        status: PreviewStatus::Ready,
        checkpoint: Some(row.into()),
        files,
        more_files,
        busy,
        repo_root: Some(root),
    })
}

/// Make the conversation's repo match checkpoint `checkpoint_id`, after
/// taking a safety checkpoint of the current tree. Refused (`TurnInProgress`)
/// when `busy`.
pub async fn restore(
    conn: &DatabaseConnection,
    conversation_id: i32,
    checkpoint_id: i32,
    busy: bool,
) -> Result<RestoreResult, AppCommandError> {
    if busy {
        return Err(AppCommandError::new(
            AppErrorCode::TurnInProgress,
            "a turn is running in this conversation or repository",
        ));
    }
    let row = Checkpoint::find_by_id(checkpoint_id)
        .one(conn)
        .await
        .map_err(db_err)?
        .filter(|r| r.conversation_id == conversation_id)
        .ok_or_else(|| AppCommandError::not_found("checkpoint not found"))?;
    let commit = row
        .commit_sha
        .clone()
        .ok_or_else(|| AppCommandError::invalid_input("this checkpoint was skipped"))?;
    let root = PathBuf::from(&row.repo_root);
    if git::repo_root(&root).await.is_none() {
        return Err(AppCommandError::not_a_git_repository(format!(
            "{} is no longer a git repository",
            root.display()
        )));
    }

    let _guard = RESTORE_LOCK.lock().await;
    let target_tree = git::tree_of(&root, &commit).await.map_err(git_err)?;
    let current_tree = git::snapshot_tree(&root).await.map_err(git_err)?;
    let safety = record_checkpoint(
        conn,
        conversation_id,
        &root,
        KIND_SAFETY,
        &current_tree,
        None,
        None,
    )
    .await?;
    let outcome = git::restore_worktree(&root, &target_tree, &current_tree)
        .await
        .map_err(git_err)?;
    tracing::info!(
        conversation_id,
        checkpoint = row.seq,
        safety = safety.seq,
        written = outcome.written,
        deleted = outcome.deleted,
        "[checkpoint] working tree restored"
    );
    spawn_prune(conn.clone(), conversation_id);
    Ok(RestoreResult {
        safety_checkpoint_id: safety.id,
        written: outcome.written,
        deleted: outcome.deleted,
    })
}

#[cfg(test)]
mod tests;
