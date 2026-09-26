//! HTTP handlers for Phantom's model-quota-failover endpoints. Both are thin
//! wrappers around the `commands::phantom_successor` / `commands::phantom_handoff`
//! `_core` functions — see those modules for the actual logic.

use std::sync::Arc;

use axum::{extract::Extension, Json};
use serde::Deserialize;

use crate::app_error::AppCommandError;
use crate::app_state::AppState;
use crate::commands::phantom_handoff::phantom_handoff_core;
use crate::commands::phantom_successor::phantom_successor_core;
use crate::models::agent::AgentType;
use crate::models::phantom::{PhantomHandoffResponse, PhantomSuccessorResponse};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhantomSuccessorParams {
    pub agent_type: AgentType,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<i32>,
}

/// `POST /api/phantom_successor` — body `{"agentType":"claude_code",
/// "model":"claude-sonnet-5"|null,"conversationId":66|null}`. Answers "what
/// should Phantom hand this conversation off to?" from real measured data;
/// see `commands::phantom_successor` for the selection rules.
pub async fn phantom_successor(
    Extension(state): Extension<Arc<AppState>>,
    Json(params): Json<PhantomSuccessorParams>,
) -> Result<Json<PhantomSuccessorResponse>, AppCommandError> {
    let resp = phantom_successor_core(
        &state.db.conn,
        params.agent_type,
        params.model.as_deref(),
        params.conversation_id,
    )
    .await?;
    Ok(Json(resp))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhantomHandoffParams {
    pub conversation_id: i32,
    pub target_agent_type: AgentType,
    #[serde(default)]
    pub target_model: Option<String>,
}

/// `POST /api/phantom_handoff` — body `{"conversationId":66,
/// "targetAgentType":"open_code","targetModel":"opencode/…"|null}`. Creates a
/// new conversation for the target agent in the source conversation's folder
/// and sends it one prompt carrying the full handoff context. See
/// `commands::phantom_handoff` for what that prompt contains.
pub async fn phantom_handoff(
    Extension(state): Extension<Arc<AppState>>,
    Json(params): Json<PhantomHandoffParams>,
) -> Result<Json<PhantomHandoffResponse>, AppCommandError> {
    let resp = phantom_handoff_core(
        &state.db,
        &state.connection_manager,
        &state.emitter,
        &state.data_dir,
        params.conversation_id,
        params.target_agent_type,
        params.target_model,
    )
    .await?;
    Ok(Json(resp))
}
