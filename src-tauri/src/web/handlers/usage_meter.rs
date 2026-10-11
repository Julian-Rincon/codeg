use std::sync::Arc;

use axum::{extract::Extension, Json};
use serde::Deserialize;

use crate::app_error::AppCommandError;
use crate::app_state::AppState;
use crate::commands::usage_meter::{router_profile_set_core, usage_meter_core};
use crate::models::usage_meter::{RouterProfileSet, UsageMeter};

#[derive(Deserialize)]
pub struct RouterProfileSetParams {
    pub profile: String,
}

/// `POST /api/usage_meter` — body `{}`. The live Claude subscription quota
/// (cached 60 s in-process) plus today's token spend per agent/model, the
/// 7-day total and the delegation router's profile. See
/// `commands::usage_meter` for the full computation.
pub async fn usage_meter(
    Extension(state): Extension<Arc<AppState>>,
) -> Result<Json<UsageMeter>, AppCommandError> {
    let meter = usage_meter_core(&state.db.conn).await?;
    Ok(Json(meter))
}

/// `POST /api/router_profile_set` — body `{"profile":"calidad"|"equilibrado"}`.
/// Validates against the router's own `Profile::parse` before writing
/// `phantom.router.profile`, so an unreadable value can never be stored.
pub async fn router_profile_set(
    Extension(state): Extension<Arc<AppState>>,
    Json(params): Json<RouterProfileSetParams>,
) -> Result<Json<RouterProfileSet>, AppCommandError> {
    let set = router_profile_set_core(&state.db.conn, &params.profile).await?;
    Ok(Json(set))
}