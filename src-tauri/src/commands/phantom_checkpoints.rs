//! "Rebobinar aquí": the two calls behind the rewind dialog. Thin shells over
//! `crate::phantom::checkpoints`, shared by the Tauri commands and the web
//! handlers (`web::handlers::phantom`).

use chrono::{DateTime, Utc};

use crate::acp::manager::ConnectionManager;
use crate::app_error::AppCommandError;
use crate::db::AppDatabase;
use crate::phantom::checkpoints::{self, CheckpointPreview, RestoreResult};

/// What restoring the code to just before one user message would change.
pub async fn phantom_checkpoint_preview_core(
    db: &AppDatabase,
    manager: &ConnectionManager,
    conversation_id: i32,
    message_id: Option<String>,
    turn_timestamp: Option<DateTime<Utc>>,
    turn_text: Option<String>,
) -> Result<CheckpointPreview, AppCommandError> {
    let mut preview = checkpoints::preview_for_turn(
        &db.conn,
        conversation_id,
        message_id.as_deref(),
        turn_timestamp,
        turn_text.as_deref(),
        false,
    )
    .await?;
    preview.busy =
        checkpoints::turn_running(manager, conversation_id, preview.repo_root.as_deref()).await;
    Ok(preview)
}

/// Restore the working tree to `checkpoint_id` (a prompt checkpoint, or the
/// safety checkpoint a previous restore returned — "undo"). Refused while a
/// turn runs in this conversation or anywhere in the same repo.
pub async fn phantom_checkpoint_restore_core(
    db: &AppDatabase,
    manager: &ConnectionManager,
    conversation_id: i32,
    checkpoint_id: i32,
) -> Result<RestoreResult, AppCommandError> {
    use sea_orm::EntityTrait;
    let root = crate::db::entities::phantom_checkpoint::Entity::find_by_id(checkpoint_id)
        .one(&db.conn)
        .await
        .ok()
        .flatten()
        .map(|r| std::path::PathBuf::from(r.repo_root));
    let busy = checkpoints::turn_running(manager, conversation_id, root.as_deref()).await;
    checkpoints::restore(&db.conn, conversation_id, checkpoint_id, busy).await
}

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn phantom_checkpoint_preview(
    conversation_id: i32,
    message_id: Option<String>,
    turn_timestamp: Option<DateTime<Utc>>,
    turn_text: Option<String>,
    manager: tauri::State<'_, ConnectionManager>,
    db: tauri::State<'_, AppDatabase>,
) -> Result<CheckpointPreview, AppCommandError> {
    phantom_checkpoint_preview_core(
        &db,
        &manager,
        conversation_id,
        message_id,
        turn_timestamp,
        turn_text,
    )
    .await
}

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn phantom_checkpoint_restore(
    conversation_id: i32,
    checkpoint_id: i32,
    manager: tauri::State<'_, ConnectionManager>,
    db: tauri::State<'_, AppDatabase>,
) -> Result<RestoreResult, AppCommandError> {
    phantom_checkpoint_restore_core(&db, &manager, conversation_id, checkpoint_id).await
}
