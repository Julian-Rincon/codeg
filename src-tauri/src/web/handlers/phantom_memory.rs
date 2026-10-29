//! HTTP handlers for Phantom's memory editor (`CLAUDE.md` files). Thin
//! wrappers around the `commands::phantom_memory` `_core` functions — see that
//! module for which files are reachable and how paths are validated.

use std::sync::Arc;

use axum::{extract::Extension, Json};
use serde::Deserialize;

use crate::app_error::AppCommandError;
use crate::app_state::AppState;
use crate::commands::phantom_memory::{
    phantom_memory_append_core, phantom_memory_list_core, phantom_memory_read_core,
    phantom_memory_write_core, MemoryAppendResult, MemoryFileContent, MemoryFileInfo, MemoryScope,
    MemoryWriteResult,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhantomMemoryListParams {
    pub folder_path: String,
}

/// `POST /api/phantom_memory_list` — body `{"folderPath":"/abs/folder"}`.
pub async fn phantom_memory_list(
    Extension(state): Extension<Arc<AppState>>,
    Json(params): Json<PhantomMemoryListParams>,
) -> Result<Json<Vec<MemoryFileInfo>>, AppCommandError> {
    Ok(Json(
        phantom_memory_list_core(&state.db.conn, &params.folder_path).await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhantomMemoryReadParams {
    pub folder_path: String,
    pub scope: MemoryScope,
}

/// `POST /api/phantom_memory_read` — body `{"folderPath":…,"scope":"project"}`.
pub async fn phantom_memory_read(
    Extension(state): Extension<Arc<AppState>>,
    Json(params): Json<PhantomMemoryReadParams>,
) -> Result<Json<MemoryFileContent>, AppCommandError> {
    Ok(Json(
        phantom_memory_read_core(&state.db.conn, &params.folder_path, params.scope).await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhantomMemoryWriteParams {
    pub folder_path: String,
    pub scope: MemoryScope,
    pub content: String,
    #[serde(default)]
    pub expected_mtime_ms: Option<i64>,
}

/// `POST /api/phantom_memory_write` — body `{"folderPath":…,"scope":…,
/// "content":"…","expectedMtimeMs":1730000000000|null}`.
pub async fn phantom_memory_write(
    Extension(state): Extension<Arc<AppState>>,
    Json(params): Json<PhantomMemoryWriteParams>,
) -> Result<Json<MemoryWriteResult>, AppCommandError> {
    Ok(Json(
        phantom_memory_write_core(
            &state.db.conn,
            &params.folder_path,
            params.scope,
            &params.content,
            params.expected_mtime_ms,
        )
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhantomMemoryAppendParams {
    pub folder_path: String,
    pub scope: MemoryScope,
    pub text: String,
}

/// `POST /api/phantom_memory_append` — body `{"folderPath":…,"scope":"user",
/// "text":"prefer pnpm"}`; appends `- prefer pnpm` as its own line.
pub async fn phantom_memory_append(
    Extension(state): Extension<Arc<AppState>>,
    Json(params): Json<PhantomMemoryAppendParams>,
) -> Result<Json<MemoryAppendResult>, AppCommandError> {
    Ok(Json(
        phantom_memory_append_core(
            &state.db.conn,
            &params.folder_path,
            params.scope,
            &params.text,
        )
        .await?,
    ))
}
