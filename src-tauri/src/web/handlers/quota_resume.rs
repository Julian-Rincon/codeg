//! HTTP handlers for the quota-resume endpoints. Thin wrappers around the
//! `commands::quota_resume` `_core` functions — see that module for the
//! scheduling rules and the background loop.

use std::sync::Arc;

use axum::{extract::Extension, Json};
use serde::Deserialize;

use crate::app_error::AppCommandError;
use crate::app_state::AppState;
use crate::commands::quota_resume::{
    cancel_core, list_core, schedule_core, QuotaResumeCanceled, QuotaResumeScheduled,
    QuotaResumeView,
};
use crate::models::AgentType;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaResumeScheduleParams {
    pub connection_id: String,
    pub conversation_id: i32,
    pub agent_type: AgentType,
}

#[derive(Deserialize)]
pub struct QuotaResumeCancelParams {
    pub id: String,
}

/// `POST /api/quota_resume_schedule` — body `{"connectionId":"…",
/// "conversationId":42,"agentType":"claude_code"}`. Programs the automatic
/// "continue by itself" resume for when the quota comes back; re-scheduling the
/// same conversation replaces the previous pending one.
pub async fn quota_resume_schedule(
    Extension(state): Extension<Arc<AppState>>,
    Json(params): Json<QuotaResumeScheduleParams>,
) -> Result<Json<QuotaResumeScheduled>, AppCommandError> {
    let scheduled = schedule_core(
        &state.db.conn,
        params.connection_id,
        params.conversation_id,
        params.agent_type,
    )
    .await?;
    Ok(Json(scheduled))
}

/// `POST /api/quota_resume_list` — body `{}`. Every schedule, pending first and
/// soonest first, so the UI can render a countdown without re-sorting.
pub async fn quota_resume_list(
    Extension(state): Extension<Arc<AppState>>,
) -> Result<Json<Vec<QuotaResumeView>>, AppCommandError> {
    Ok(Json(list_core(&state.db.conn).await?))
}

/// `POST /api/quota_resume_cancel` — body `{"id":"qr-…"}`. Always answers
/// `{"ok":true}`: an unknown or already-finished id cannot resume anyway.
pub async fn quota_resume_cancel(
    Extension(state): Extension<Arc<AppState>>,
    Json(params): Json<QuotaResumeCancelParams>,
) -> Result<Json<QuotaResumeCanceled>, AppCommandError> {
    Ok(Json(cancel_core(&state.db.conn, &params.id).await?))
}
