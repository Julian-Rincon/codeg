use std::sync::Arc;

use axum::{extract::Extension, Json};

use crate::app_error::AppCommandError;
use crate::app_state::AppState;
use crate::commands::usage_meter::usage_meter_core;
use crate::models::usage_meter::UsageMeter;

/// `POST /api/usage_meter` — body `{}`. The live Claude subscription quota
/// (cached 60 s in-process) plus today's token spend per agent/model and the
/// 7-day total. See `commands::usage_meter` for the full computation.
pub async fn usage_meter(
    Extension(state): Extension<Arc<AppState>>,
) -> Result<Json<UsageMeter>, AppCommandError> {
    let meter = usage_meter_core(&state.db.conn).await?;
    Ok(Json(meter))
}