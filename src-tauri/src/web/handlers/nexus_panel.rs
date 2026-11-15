//! `POST /api/nexus_panel` — body `{}`. The NEXUS command-center data, or `null`.
//! See `commands::nexus_panel`.

use axum::Json;

use crate::app_error::AppCommandError;
use crate::commands::nexus_panel::nexus_panel_core;

pub async fn nexus_panel() -> Result<Json<Option<serde_json::Value>>, AppCommandError> {
    Ok(Json(nexus_panel_core().await?))
}
