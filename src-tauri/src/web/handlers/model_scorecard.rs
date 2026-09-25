use std::sync::Arc;

use axum::{extract::Extension, Json};

use crate::app_error::AppCommandError;
use crate::app_state::AppState;
use crate::commands::model_scorecard::model_scorecard_core;
use crate::models::model_scorecard::ModelScorecard;

/// `POST /api/model_scorecard` — body `{}`. Measured per-model quality/speed
/// numbers folded from the user's own `token_usage_turn` history, joined
/// against the live model catalog and the bundled models.dev spec snapshot.
/// See `commands::model_scorecard` for the full computation.
pub async fn model_scorecard(
    Extension(state): Extension<Arc<AppState>>,
) -> Result<Json<ModelScorecard>, AppCommandError> {
    let card = model_scorecard_core(&state.db.conn).await?;
    Ok(Json(card))
}
